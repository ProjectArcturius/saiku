# 集团军（Group Army, GA）—— 供应商账户池

> 2026-11-13 立。代码：`crates/saiku-core/src/group_army.rs`；命令：`saiku ga ls`；派工：`saiku run --ga <id>`。

一个 GA = **一家供应商的一个模型族 + 若干互相独立的账户**。Saiku 在池子里取号，工人只拿到一把钥匙。

## 为什么不是「一个 key 一个 provider 块」

三个 Inferknock 账户是三个独立注册的账号：各有各的 RPM / TPM / 并发，各有各的余额与故障。
把它们写成三个 provider 块，等于把「同一类能力」复制三份，任务书、缓存前缀、验收口径都会分叉；
写成 `provider` 里三个 `apiKey`，则无法表达「此刻每个账户上有几个工人在跑」。

GA 把两件事分开：

| 层 | 放在哪 | 由谁决定 |
|---|---|---|
| 地址、协议、模型表 | `provider.<id>` | 供应商，一份 |
| 能力（上下文 / 输出 / 多模态 / effort） | `group_army.<id>.capabilities` | 模型族，一份 |
| 账户（key）与限额 | `group_army.<id>.accounts` + `auth.json` | 每个账号，各一份 |

## 配置

```json
{
  "model": "inferknock/deepseek-v4.1-flash",
  "provider": {
    "inferknock": {
      "npm": "@ai-sdk/openai-compatible",
      "options": { "baseURL": "https://…/v1" },
      "models": { "deepseek-v4.1-flash": { "name": "DeepSeek V4.1 Flash", "contextWindow": 1000000 } }
    }
  },
  "group_army": {
    "inf-ds": {
      "name": "Inferknock DeepSeek",
      "provider": "inferknock",
      "model": "deepseek-v4.1-flash",
      "capabilities": { "context": 1000000, "max_output": 384000, "vision": true },
      "limits": { "rpm": 300, "tpm": 10000000, "concurrency": 10 },
      "accounts": ["inf-1", "inf-2", "inf-3"],
      "default": true
    }
  },
  "dispatch": { "ga": "inf-ds", "needs": ["vision"] }
}
```

- `accounts` 里的 id 就是 `auth.json` 的条目：`saiku auth set inf-1`、`inf-2`、`inf-3`。
  **密钥不进 `saiku.json`**，也不进任务状态目录。
- `limits` 是账户缺省；单个账户要更严就写成对象：
  `{"id": "inf-3", "limits": {"concurrency": 4}}`。
- `concurrency` 是**唯一硬拦**的一项。`rpm` / `tpm` 只记录与展示 —— 本地做令牌桶只会
  和供应商的真实限流打架，反而把 429 变成自己制造的排队。
- `capabilities` 决定「谁配干这件活」：`--need vision` / `--need context=500000` 不满足就
  在**派工那一秒**拒绝（`CAPABILITY_MISS`），不是跑到一半才发现读不了图。

## 取号：怎么决定用哪个账户

1. 先看能力：`--need`（或 `dispatch.needs`）不满足 → 直接拒绝。
2. 跳过冷却中的账户（`429` / 5xx 之后由 `group_army::cooldown` 写入）。
3. 按**剩余名额多**的优先，同分按配置顺序。
4. 在目录锁里回收死租约、数数、写新租约 —— 并发安全靠 `create_dir` 的原子性，不引入 flock 依赖。

租约落在状态目录：

```text
<state_root>/capacity/<ga>/<account>/
    <pid>-<纳秒>/meta.json   谁、什么时候、跑哪件
    <pid>-<纳秒>/beat        心跳（mtime 即最后活着的证据）
    cooldown.json            冷却到什么时候、为什么
```

监工每 25 秒刷一次心跳，退出时删掉租约；进程被强杀时心跳停，下一次取号按
**120 秒过期**或**同机 pid 已不在**回收。没有常驻清理进程。

## 命令

```bash
saiku ga ls            # 每个池：能力、账户、此刻占用/上限、冷却
saiku ga ls --json
saiku run 任务书.md --ga inf-ds --need vision --accept "<验收>" --detach
```

`status.json` 会记 `ga` 与 `account`：夜班结束后能回答「昨晚哪个 key 在拖后腿」。

## 与失败分类的关系（`failure.rs`）

| 分类 | 含义 | 池内动作 |
|---|---|---|
| `MODEL_RATE_LIMITED` | 429 / overloaded | 账户冷却，换同池账户重派 |
| `MODEL_QUOTA_EXHAUSTED` | 余额 / 额度用尽 | 该账户本夜不再用；**不换供应商** |
| `MODEL_AUTH` | 401/403 | 该账户停用，报出账户 id |
| `MODEL_SERVER_ERROR` | 5xx | 冷却后换账户 |
| `MODEL_CONTEXT_LENGTH` | 超上下文 | 换更大的池（另一个 GA），不是换账户 |

**换供应商要显式建新 GA**，不允许自动跨 GA 漂移：跨 GA 等于换模型能力、换缓存口径、
换上下文窗口，那是一次产品决定，不是一次重试。

## 还没做的

- 派工进程内的**自动换账户重跑**尚未接线：当前取号在派工那一刻完成，失败后由
  夜班控制器（`saiku night`）按上表重派。等夜班控制器落地后，这条才闭环。
- RPM / TPM 的实际用量还没有从 DSH 事件里汇总到账户维度（`usage` 只到任务）。
