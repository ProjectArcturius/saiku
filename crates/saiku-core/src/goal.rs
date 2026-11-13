//! 目标模式的**共享文案**：headless 的 `--goal` 与 TUI 的 `/goal` 必须说同一句话。
//!
//! 为什么把提示词放在 core 而不是各写各的：目标模式的全部效果都来自「模型愿不愿意
//! 调 `create_goal`、愿不愿意在每轮结束时保持 active」。两份措辞会让「TUI 里的目标
//! 不生效、headless 里的生效」这种问题变成需要复现的玄学 —— 一句话只有一个出处。
//!
//! 与 DSH 的关系：`create_goal` / `update_goal` 是内核工具，目标本身由 `goal-round-driver`
//! 在 idle 后推进；这里只负责把人的意图写成模型能照做的协议。

/// 目标最多推进几轮（缺省）。与 `GoalSpec::default_max_rounds` 保持一致。
pub const DEFAULT_MAX_ROUNDS: u32 = 24;

/// 把目标写成一段可执行的协议，追加到用户指令后面。
pub fn directive(objective: &str, max_rounds: u32) -> String {
    format!(
        "\n\n---\n\n## 目标模式\n\n这次不是「答完就停」，是「做到完」。按下面四步做：\n\n\
         1. 第一步就调用 `create_goal` 建立会话目标，`max_goal_rounds` 设为 {max_rounds}，objective 用这一行原文：\n   {objective}\n\
         2. 每轮做最小可验证的一步；做完用 `update_goal` 保持 active，不要停在提问上。\n\
         3. 全部达成时 `update_goal` 置 complete，并给出证据（命令输出、文件路径、提交号）。\n\
         4. 真被外部条件卡死时置 blocked 并写清卡点；不要用 complete 蒙混。\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directive_names_the_objective_rounds_and_the_two_ends() {
        let text = directive("把导航页补齐", 12);
        assert!(text.contains("把导航页补齐"));
        assert!(text.contains("`max_goal_rounds` 设为 12"));
        assert!(text.contains("complete"));
        assert!(text.contains("blocked"));
        assert!(text.starts_with("\n\n---\n\n## 目标模式"));
    }
}
