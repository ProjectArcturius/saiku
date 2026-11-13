# 夜班控制器（`saiku night`）

> 2026-11-13 立。代码：`crates/saiku-cli/src/night.rs`。定位：**没人看着的时候，让「Saiku 死了」变成一行字。**

## 它解决的是什么

`saiku run` 管一件任务：监工死了，`status.json` 的心跳会过期，读者能判 `dead`。但夜里有三件事
它答不了：

1. 监工死了之后**谁来重派**？
2. 控制器自己死了，谁来发现？
3. 一个账户被限流/额度用尽之后，下一件活该用哪个 key？

夜班控制器就是把这三条接上：一个队列、一组子进程、一份落盘状态。

## 计划（只读）

```json
{
  "run_id": "night-20261113",
  "config": "/home/me/.config/saiku/saiku.json",
  "ga": "inf-ds",
  "max_parallel": 6,
  "retry": { "attempts": 2, "cooldown_seconds": 180 },
  "tasks": [
    { "id": "w1", "book": "/abs/tasks/fill-nav.md", "worktree": "dsh/w1",
      "accept": ["cargo test -p settings"], "by": "claude" },
    { "id": "w2", "book": "/abs/tasks/ime-bug.md", "worktree": "dsh/w2",
      "accept": ["cargo test -p ceyboard"], "depends_on": ["w1"], "needs": ["vision"] }
  ]
}
```

- `max_parallel` 缺省 4。**它不是容量的上限，是审阅能力的上限**：塞满 30 个槽只会让分支互相
  踩文件、让合并变成瓶颈（参工作台关于派工的结论）。
- `depends_on` 全部 `done` 才起跑；`needs` 原样传给 `run --need`，由 GA 的能力门槛判。
- 任务书本身仍然带验收门；`accept` 只是把命令行上的门写进计划，二者会叠加。

## 状态（可变，原子替换）

`<状态根>/night/<run_id>/`：

| 文件 | 谁写 | 含义 |
|---|---|---|
| `plan.json` | 控制器，首次 | 计划副本（留档；改它不会改变本次运行） |
| `state.json` | 控制器，每次转移 | 每件任务的状态、尝试次数、账户、失败码 |
| `heartbeat.json` | 控制器，每 5 秒 | `done/running/pending/failed` 与在跑列表 |
| `night.log` | 控制器，追加 | 人类可读的事件流 |
| `runner.log` | `--detach` 时的重定向 | 控制器自己的 stdout/stderr |
| `stop` | `saiku night stop` | 请求停止 |

## 恢复语义（这是重点）

| 现场 | 控制器重跑同一条命令之后 |
|---|---|
| 任务 `running`，`status.json` 心跳新鲜 | **接管**：不重派，只轮询状态 |
| 任务 `running`，监工已 `dead` | 记一次失败并按重派策略处理（`SUPERVISOR_DIED`） |
| 任务 `pending` | 照常起跑 |
| 控制器自己被杀 | 子进程继续跑（它们是独立进程）；重启后按上两行接管 |
| 状态目录属于别的 `run_id` | 拒绝启动，不猜 |

**判据是文件，不是记忆**：控制器不持有任何只在内存里的队列状态。

## 失败 → 动作

| 分类 | 动作 |
|---|---|
| `MODEL_RATE_LIMITED` / `MODEL_SERVER_ERROR` | 该账户冷却（180 s / ≥300 s），换同池账户重派 |
| `MODEL_QUOTA_EXHAUSTED` / `MODEL_AUTH` | 该账户冷却 12 小时（这一夜不再用），换同池账户重派 |
| `MODEL_CONTEXT_LENGTH` / `MODEL_ROUTE` / 配置、任务书、授权错 | **不重派**，记 `failed` 并写明原因 |
| `blocked`（含 `GOAL_BLOCKED`） | 不重派 —— 需要人重新规划 |
| `timed_out` | 重派一次（换账户），再超时就算失败 |
| 没有 `status.json`，`console.err` 首行是 `GA_CAPACITY: …` | 重派（池子满了不是错误） |
| 没有 `status.json`，首行是 `CONFIG_INVALID:` 等 | 不重派 |

**跨 GA 漂移一律不做**：换供应商等于换模型能力、换缓存口径、换上下文窗口，那是产品决定，
不是一次重试。

## 命令

```bash
saiku night run night.json            # 前台；Ctrl-C 会留下现场，重跑即续
saiku night run night.json --detach   # 分离；返回控制器目录
saiku night status <控制器目录>        # 一行：done/running/pending/failed + 心跳
saiku night stop <控制器目录>          # 取消在跑的，不再接新的
```

`night status` 的退出码：`0` 全部完成 · `1` 有失败 · `10` 仍在跑。心跳超过 60 秒没更新时，
输出里写的是「控制器已停 Ns」——**这一行就是「今晚是不是白跑了」的答案**。

## 它不管什么

- **不管审阅与合并**：`done` 只代表工人自己过了验收门；合并仍要人（或另一个角色）看。
- **不管额度**：没有配额账本，`MODEL_QUOTA_EXHAUSTED` 只冷却账户，不会阻止整夜继续。
- **不管物理电源**：断电保护在 Ether 的 `tools/power_guard` 那一层，不在本仓。
- **不管 push**：控制器不 push，合并与推送仍是显式动作。
