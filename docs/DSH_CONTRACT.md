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

## 升级体检记录：0.2.1-alpha.2 → 0.2.2-rc.2（2026-11-13）

并存安装（不动当前 pin）：

```sh
saiku runtime install --version 0.2.2-rc.2     # → runtime/dsh-0.2.2-rc.2
saiku dsh doctor --version 0.2.2-rc.2          # 115 行 / 113 包，17 个必需行都在
```

与 0.2.1 快照相比：97 行共有、1 行删除（`tool-subagent-list-agents`）、18 行新增。
**真会话体检**（假模型端点，不消耗配额，`SAIKU_TEST_DSH_VERSION=0.2.2-rc.2` 跑
`m3_negative_live`）：17 条里 **15 过 2 败**；两条失败在钉住的 0.2.1 上**同样失败**
（`full_pwsh_runs_inside` 用的是 PowerShell 语法，Linux 上不成立；`git_commit` 用例的环境前提），
所以**不构成 0.2.2 的回归**。

通过的关键项：workspace 内写、只读拒绝写、shell 工具不可见（不是「被拒」）、
`network=false` 摘掉 web/shell、子 agent 无写无 shell、提权按策略拒、会话日志记录 guard 事件。

结论：0.2.2 具备**升级候选**资格。升级还需两步：一次真模型冒烟（用 GA 里最便宜的账户），
以及决定是否同批接 F4 附件与 F5 ask_user 应答。

### 更正：F4 附件与 F5 ask_user **不是版本门槛**（2026-11-13 实测）

此前记成「0.2.1 没有 `file-upload` / `user-questions`，要等 0.2.2」。实测推翻了这个说法：

- 0.2.1 的 web profile 里 `user-questions`、`file-upload` 都已经加载，`tool-ask-user`
  也在工具门控里（嵌套位置，顶层行 diff 看不到）；
- 真内核上 `userQuestions/answer` 回 `session/not-found`、`fileUploads/upload` 回
  `gateway/input-invalid` —— 都是**端点存在**的错，而不是 404。

代价是这条错误认识让交互会话按「0.2.2 才有」门控 ask_user，于是在钉住的 0.2.1 上
把会等人的工具放了出来。已改为两个入口一律摘掉（`blocking_tools: true`），
等 F5 的应答 UI 接上再按能力打开。新增 live 用例
`remote_live_attachment_and_question_endpoints_exist` 专门盯这件事。

结论：F4/F5 的进度只取决于 Saiku 侧的 UI 与事件映射，**不取决于升级**。

### 审批端到端通过，并收窄了「什么时候会弹审批」（2026-11-13）

`remote_live_an_approval_lets_the_tool_run`：真内核 + 脚本化模型，模型用
`write` + `sandbox_permissions: danger-full-access` 请求提权 → 内核发
`approval/request` → Saiku 答 `allowed-once` → 写入**真的**发生在 workspace 外面。

同一轮还测出一个容易误解的事实：**`confirm` 档下 workspace 内的普通写入不弹审批**，
命令直接就跑了（第一版用例就是这么写的，文件出现了却没等到瀑布）。审批只在工具
**要求超出当前沙箱**时出现 —— 这点写进了矩阵，别再把「没弹审批」当成坏。

因此空跑审批的路径也收窄了：想验审批，脚本里必须**显式提权**。

### `$events/result` 只有一个 `args` 层（2026-11-13，端到端发现）

写 F5 的端到端用例时撞到的：`events::answer` 把结果发成
`{"args":{clientId,eventId,outcome}}`，而网关的
`parseRemoteEventResultPayload` 只认 `{clientId,eventId,outcome}` 三个键 ——
`rpc.call` 已经把它放进 `payload.args` 了。多包一层的后果是
`api gateway: invalid Remote event result`，**而且它一直是坏的**：
审批的「允许/拒绝」走的也是同一个函数。

修好后新 live 用例 `remote_live_a_question_is_answered_through_the_waterfall`
（真内核 + 脚本化假模型：模型调 `ask_user_question` → 瀑布 → 我们答 B → 模型收到 B）
通过。审批的应答形状与它同源，但「真跑一次审批」还需要一条脚本化工具的用例，
记在下一轮。

### 命令通道：在两个真内核上都实测过（2026-11-13）

`commands/list` 与 `commands/execute` 不经过模型，所以在**不消耗配额**的前提下就能验形状：

- 新 live 用例 `remote_live_commands_answer_the_kernel_registry`：起真内核 → 建会话 →
  列命令 → 执行 `/compact`（空会话的**结果**由内核决定，这里只验**调用通道**）。
- 0.2.1-alpha.2 与 0.2.2-rc.2 都通过；两者的命令表都含 `compact`。
- 跑法：`SAIKU_DSH_TEST_VERSION=<版本> SAIKU_DSH_RUNTIME_DIR=<目录> cargo test -p saiku-dsh
  --lib remote_live_commands -- --ignored`。

这条实证取代了「按内核源码推断」：TUI 的 `/kernel`、`/` 补全里的内核命令、以及未知
命令的转发，依据的是实测行为而不是我们的理解。

### 这次试验暴露并修掉的 bug

`saiku runtime install --version X` 曾经先看配置里的 `runtime.dir`，于是把 X 装进了
**钉住版本**的目录，直接覆盖了在用的运行时。现在：给了 `--version` 就按版本换目录；
目录里已有**另一个**版本时 `install` 拒绝覆盖，除非显式 `--force`。
live 用例也改成读 `SAIKU_TEST_DSH_VERSION`，同一套断言可以体检任何候选版本。

## 还没做的

- **事件词汇的自动核对**：`--dump-config` 里没有事件名，要把假 provider 会话跑起来断言
  `turn/start`、`turn/end`、`tool/call`、`goal/change` 的字段；目前靠夹具测试与人工看。
- **工具集策略断言**：新增工具默认会进模型的可调用集合，`doctor` 只能报「多了哪个包」，
  还不能判「这个工具在 headless 下会不会阻塞」。
- **`promote` / `rollback` 子命令**：现在靠改配置，没做成一键。
