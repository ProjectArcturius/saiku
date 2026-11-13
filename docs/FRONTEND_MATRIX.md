# 前端能力矩阵（TUI / 未来 GUI × DSH 内核）

> 2026-11-13 立。用途：决定「下一个接什么」，而不是凭感觉补界面。
> 依据：DSH 0.2.1/0.2.2 的 `--dump-config` 行清单（`saiku dsh doctor --against …`）+
> Saiku 的 `Action` 注册表（`crates/saiku-tui/src/commands.rs`）与 headless 命令面。

## 读法

- **状态**：`有` = 可用且有测试；`半` = 能用但缺关键一环；`无` = 没有入口。
- **降级**：内核没有这个能力时，界面必须怎么说。**没有降级路径的功能不许接** ——
  否则用户看到的是一个假装存在的按钮。
- 阶段：F2 目标 → F3 委托 → F4 输入面 → F5 审批 → F6 路由与成本（顺序可按需调整）。

## 矩阵

| 能力 | 内核（包 / 行） | TUI | headless | 阶段 | 缺什么 / 降级 |
|---|---|---|---|---|---|
| 多轮会话、流式、取消、恢复 | `session/*` | 有 | 有 | — | — |
| 工具视图（diff / 完整输出） | `tool/call`、`tool/result` | 有 | 有（JSONL） | — | — |
| 会话检索 | `session-query-sqlite` | 有 | 无 | — | 无索引时 `/search` 报「本内核未开检索」 |
| 会话时间线 | `session/*` | 有 | 无 | — | — |
| **目标（goal）** | `goal`、`goal-round-driver`、`tool-goal` | **半**（`/goal` 已接、页脚显示轮次） | 有（`--goal`） | **F2** | 缺 `/goal` 的暂停/恢复/收尾展示；内核无目标时 `/goal` 明确报错 |
| 内核斜杠命令（`/compact` 等） | `commands/list`、`commands/execute` | 有（`/` 补全里与自家命令并列、`/kernel` 可浏览；不认识的 `/x` 原样转发，内核拒绝时回报原文） | 无 | F3 | 命令表不复制；补全按会话懒取，换会话时重取 |
| 计划 / 只读模式 | `plan-mode`（Saiku 关掉，自己管） | 有（`/plan`） | 有（`--permission readonly`） | — | — |
| 子 agent | `subagent`、`subagent-fork/spawn`、`client-ui-subagent` | 有（`/subagents` 只读跳转） | 半（内核自建） | F3 | 缺「从 TUI 派一个子 agent」；无该能力时按钮禁掉 |
| Agent team（多角色协作） | `agent-team`、`tool-agent-team`（0.2.2 新增） | 无 | 无 | F3 | 先探测包在不在，再决定是否显示 |
| 技能（skills） | `skill`、`skill-filesystem`、`skill-office` | 无 | 无 | F3 | 无入口，不显示 |
| 文件附件 | `file-upload`、`client-file-upload` | **有**（`/attach <路径>` 上传 → 页脚 `📎 N` → 下一条消息带 `{type:"file",receiptId}`） | 无 | F4 完成（文件） | 上限 20 MB；`/detach` 清空。**图片**还没走：内核支持 `{type:"image",data}` 直传，需要先按 GA 的 vision 能力拒绝不支持的池子 |
| `@` 文件引用 | `file-reference-local` | 有（`@` 补全 + `@"带空格"` 语法与内核 `formatFileMention` 一致） | 无 | F4 | 内核没有该包时退化成纯文本路径（模型只能自己读文件） |
| 交付物 / 展示 | `present`、`deliverable-file-references` | 无 | 无 | F4 | 无入口 |
| Office（docx/xlsx/pptx） | `office-kit`、`tool-office` | 无 | 无 | F4 | 无入口；不要假装能读 |
| 定时任务 | `schedule`、`tool-schedule`、`client-ui-schedule` | 无 | 无（有 `night` 控制器） | F4 | 显示「用 saiku night」 |
| 夜班视图（Saiku 自有） | `saiku night` 的落盘状态 | 有（`/night`：完成/在跑/失败码/心跳/整批 token；`/night list` 看最近 10 次） | 有（`night status`） | — | 控制器从未跑过时明确说「没有记录」 |
| 提问（ask_user） | `tool-ask-user`、`user-questions`、`client-ui-user-questions` | **有**（瀑布 → 弹层 → `$events/result`；Enter 确认、Esc 跳过） | headless **仍禁** | F5 完成 | 端到端已用真内核 + 脚本化模型验过（`remote_live_a_question_is_answered_through_the_waterfall`）。交互面打开，`night`/headless 继续摘掉——那里没有人答。限制：自由输入靠「跳过」降级，多选只取当前项 |
| 审批（confirm） | `approval`、`user-approval`、`permission` | 有（`/approvals`、confirm 档、端到端已验：提权写入 → 允许 → 真的写到 workspace 外） | 无（headless 拒绝 confirm） | F5 完成 | 只有**提权**会弹审批；workspace 内的普通写入不弹（实测）。无审批通道时 confirm 档拒绝启动 |
| 浏览器/网络工具 | `tool-web`、`web-deepseek-api` | 无（内核工具直接可见） | 半（`--permission` 控制） | F6 | — |
| 模型目录与切换 | `model/selection` | 有（模型对话框） | 有（`--model`） | — | — |
| 供应商池（GA） | Saiku 自有 | 有（`/ga` 默认池、`/pools` 容量/冷却/能力标签、`/dispatch` 派出、`/runs` 详情与账户列） | 有（`--ga`、`ga ls`） | F6 | 交互会话不经过 GA；成本只有 token，没有费用换算 |
| 用量 / 成本 | `assistant/message.usage` | 有（`/usage`） | 有（`usage`） | — | 无费用换算（只有 token） |
| 会话压缩 | `command-compact`、`compaction-*` | 无 | 自动 | — | 无手动入口 |
| 遥测 / 会话日志上传 | `session-log-deepseek` | — | 已显式关闭 | — | 开关在 patch 里，不在界面 |
| 远程访问 | `remote-access`（0.2.2） | 无 | 无 | 不接 | 安全边界，不在本轮 |

## 结论（现在最该做的三件）

1. **F2 收尾**：`/goal` 已能发目标、页脚常驻轮次、完成/受阻各有一次性结论。
   只剩「清掉目标」—— 内核没有对应的宿主 RPC，要等 F5 一起看。
2. **F5 的 ask_user**：内核（0.2.1 起）有会等人的工具；夜班与 headless 必须显式摘掉它，TUI 也一样，
   否则工人会停在一个没人回答的问题上。这是**安全/可用性**问题，优先级高于任何新功能。
3. **F4 的剩余项**：`@` 引用已对齐内核语法；下一步是图片/文件附件（需要内核 0.2.2 的
   `fileUploads/upload` 通道已存在（实测），缺 UI；GA 的 vision 门槛已能做拒绝）。

补充（2026-11-13 本轮）：`/dispatch` 让「从 TUI 把一件长活交给池子」成立；
`/night` 让「昨晚怎么样」不必离开 TUI。两者都只是读/转发，不新增内核能力。

## 契约边界（已钉住）

TUI 只依赖 `saiku-core` 的会话契约（`service::Command` / `Event`），**不链接 `saiku-dsh`**，
也不依赖 `saiku-cli`。这不是洁癖：GUI 将来经 IPC 连同一个后端，前端若直接摸 DSH Remote，
换内核或换 GUI 就要改两份。`saiku-tui` 里有一条测试读自己的 `Cargo.toml` 钉住这条边界。

## 不做的

- 不把 DSH web 的每一页都复刻一遍：前端只做「一个人跑长任务 + 看住一队工人」需要的部分。
- 不在 TUI 里造内核没有的能力（例如自定义工具）：那会把「内核升级」变成「前端重写」。
