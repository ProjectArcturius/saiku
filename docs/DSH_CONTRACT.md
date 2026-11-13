# DSH 契约与升级（`saiku dsh`）

> 2026-11-13 立。代码：`crates/saiku-dsh/src/contract.rs`、`crates/saiku-cli/src/dsh_cmd.rs`；
> 快照：`contracts/dsh-<版本>.json`。

## 为什么要这一层

Saiku 与 DSH 的耦合有四层，脆度不同：

| 层 | 内容 | 变更频率 | 失效形态 | 现有防线 |
|---|---|---|---|---|
| L1 安装 | node 入口、`DSH_HOME`、profile 名 | 低 | 启动即报错 | 版本钉 + 每版本独立目录 |
| L2 传输 | 6 个方法 + 4 个通知 | 低 | 全线不可用 | 握手 + 夹具回归 |
| L3 事件 | `turn/*`、`tool/*`、`goal/change` 字段 | 中 | 状态误判 | 未知事件留档、保守分类 |
| L4 patch | 内部配置行 id（17 行） | **高** | 启动失败或静默降级 | **本页** |

L4 是唯一「按内部 id 定位」的层，也是 DSH 最不承诺的部分。实测漂移：
`0.2.1-alpha.2` → `0.2.2-rc.2` 的 sdk profile 从 98 行变 115 行（97 共有、1 删除、18 新增，
新增里包含 `tool-ask-user` 这类**会改变无人值守行为**的工具）。

## 命令

```bash
saiku dsh list                        # 已安装的版本、哪个是当前钉住的
saiku dsh doctor                      # 体检当前版本：行数、包数、patch 点名的行是否都在
saiku dsh doctor --against-version 0.2.2-rc.2   # 与另一个已安装版本逐行比
saiku dsh doctor --against /usr/lib   # 或直接给一个含 node_modules 的目录
saiku dsh doctor --write-contract     # 写/更新 contracts/dsh-<版本>.json
```

体检做三件事，全部来自内核自己的 `node <bin.js> --profile sdk --dump-config`，不调模型、不烧额度：

1. **必需行**：`REQUIRED_ROWS`（17 个 id）必须都还在。少一个就退出码 1，并指名道姓。
   这份清单与 `render_patch` 由单测绑定（`every_patch_row_is_declared_in_the_contract_list`），
   patch 加了新行而清单没加，测试先红。
2. **与快照比**：行列增删、包增删、`disabled` 翻转。快照查找顺序：
   `--contracts` → 当前目录 `contracts/` → `<数据目录>/saiku/contracts/`。
3. **与另一版本比**：同一台机上通常有两个版本（本机就是 `0.2.1-alpha.2` 与系统 `0.2.2-rc.2`），
   一眼看清「升级会多出什么、少掉什么」。

## 升级节奏

1. **平时钉死，永不自动更新，永不追 `latest`。** 夜班前不改内核。
2. 每周一次刻意升级：`saiku runtime install --version <新版本>`（并存，不覆盖旧目录）。
3. `saiku dsh doctor --version <新版本>` → 缺行就停，先修 `render_patch`。
4. `saiku dsh doctor --against <旧目录>` 读差量表，只看三类：**删掉的行 / 改名的事件字段 /
   新增的交互型工具**（`ask_user` 这类在 headless 里必须禁掉，否则工人会停下来等人回答）。
5. 通过后把 `saiku.json` 的 `runtime.dsh_version` 指过去；旧版本目录保留，回滚就是改回一行。

## 还没做的

- **事件词汇的自动核对**：`--dump-config` 里没有事件名，要把假 provider 会话跑起来断言
  `turn/start`、`turn/end`、`tool/call`、`goal/change` 的字段；目前靠夹具测试与人工看。
- **工具集策略断言**：新增工具默认会进模型的可调用集合，`doctor` 只能报「多了哪个包」，
  还不能判「这个工具在 headless 下会不会阻塞」。
- **`promote` / `rollback` 子命令**：现在靠改配置，没做成一键。
