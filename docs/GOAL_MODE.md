# 目标模式（`saiku run --goal`）

> 2026-11-13 立。依据：DSH 的 `goal` 域与 `goal-round-driver`（调查证据见本节末）。
> 代码：`saiku-core::config::GoalSpec`、`saiku-dsh::projection::GoalView`、`worker` 的续轮判定。

## 一句话

DSH 的目标**不是**宿主的循环，而是会话内的一等公民：模型自己 `create_goal`，之后每次
`idle` 由 DSH 的 `goal-round-driver` 注入下一轮（`<goal_round>`），直到 `complete` / `blocked`
或轮数用尽。宿主唯一必须做对的事是：**在目标还 active 的时候别把内核收掉。**

旧行为正是反例：监工看到第一个 `turn/end` 就 `shutdown`，目标刚建好就被连内核一起杀掉。

## 用法

```bash
saiku run 任务书.md --goal "把 Settings 的导航页补齐并让 cargo test -p settings 通过" \
  --goal-rounds 24 --accept "cargo test -p settings" --permission full --worktree dsh/nav
```

`--goal` 会往指令末尾追加一段协议（建目标、每轮推进、complete 给证据、blocked 写卡点）。
`--goal-rounds` 同时写进 `create_goal` 的 `max_goal_rounds`。

## 宿主侧的判定

| 投影里看到的 | 监工的动作 |
|---|---|
| `goal/change` → `phase=active`，又收到 `turn/end` | **不收工**，把 `ends_before` 推到当前，继续等下一轮 |
| `phase=complete` | 正常收尾；验收照旧按 `--accept` 判 |
| `phase=blocked` | 记 `GOAL_BLOCKED` + `blockedReason.code`，状态 `blocked`，不重派 |
| 仍 `active` 但到了轮数/预算 | 记 `GOAL_UNFINISHED`，状态 `blocked` |
| 从没出现 `goal/change` | 当作普通任务（模型没建目标），首个 `turn/end` 即收尾 |

轮间的 `idle` 不算结束：目标 active 时 `session/wait` 之后的宽限期从 3 秒放宽到 120 秒，
避免「轮间空隙被误判成没有 terminal」。

## 已知边界（照抄上游语义）

- **`max-tokens` 会 disarm 目标**：某一轮撞到输出上限后，目标要等下一次人类轮次才会恢复。
  夜班应把 `--goal-rounds` 与任务书的输出预算一起给足。
- **轮数用尽后 `resume` 会被拒**：DSH 要求先 `edit` 提高上限（`round-limit` 阻塞）。
  Saiku 目前把这种情况报成 `GOAL_BLOCKED`，不做自动 `edit`。
- **权限**：`create_goal` 要求「直接人类根权限」，而 SDK 把宿主发来的 prompt 标成
  `source.kind=user`，所以 worker 里模型可以直接建目标。这也意味着**任何 SDK 客户端都被当作
  人类**——不要把 SDK 入口暴露给不可信进程。
- **未做真机验证**：本机没有配额授权，目标模式的端到端只在投影层有单测；第一次真跑要留证据
  （事件流里的 `goal/change` 序列 + `status.json` 的 `goal` 字段）。

## 证据

- `REFS/deepseek-harness/packages/goal/goal-round-driver/src/index.ts`（续轮注入）
- `REFS/deepseek-harness/packages/goal/tool-goal/src/authority.ts`（根权限判定）
- `REFS/deepseek-harness/packages/sdk/protocol/src/types.ts`（SDK 只有 6 个方法，无 `goal/*`）
- `docs/tool-catalog.md`（`create_goal` / `get_goal` / `update_goal`）
