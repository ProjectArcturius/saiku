//! DSH 内核契约：把「已安装的内核长什么样」变成可比较的数据。
//!
//! 为什么要有这一层：Saiku 与 DSH 的耦合有四层（安装 / 传输 / 事件词汇 / patch 行），
//! 其中 patch 行最脆 —— 它按 `id` 定位 DSH 的内部配置行，而那些 id 是 DSH 最不承诺的
//! 部分。DSH 每升一个小版本，行的集合就可能变：2026-11-13 实测 `0.2.1-alpha.2` →
//! `0.2.2-rc.2` 的 sdk profile 从 98 行变 115 行（97 共有、1 删除、18 新增）。
//!
//! 所以升级不能靠「读 changelog 猜」，要靠**同一份 dump 比两次**：
//! `node <bin.js> --profile sdk --dump-config` 是内核自己给出的、机器可读的装配结果，
//! 里面有行 id、包名与 disabled 状态。把它存成快照，下次升级只差一份清单。
//!
//! 这里刻意不引 YAML 依赖：dump 的形状稳定到可以逐行认（`- id:` / `  name:` /
//! `  disabled:`），而多一个解析器就多一个会漂的东西。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// 一行配置。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    /// 包名（`name` 的第一段；`@scope/pkg/tools` 归到 `@scope/pkg`）。
    #[serde(default)]
    pub package: Option<String>,
    /// 只在 `disabled: true` 时为 true；表达式（`!!js …`）记 `None`，不猜。
    #[serde(default)]
    pub disabled: Option<bool>,
}

/// 一次 dump 的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contract {
    pub schema_version: u32,
    pub version: String,
    pub profile: String,
    /// 行 id → 行信息。
    pub rows: BTreeMap<String, Row>,
}

impl Contract {
    pub fn ids(&self) -> BTreeSet<&str> {
        self.rows.keys().map(String::as_str).collect()
    }

    pub fn packages(&self) -> BTreeSet<&str> {
        self.rows.values().filter_map(|r| r.package.as_deref()).collect()
    }
}

/// 两份契约之间的差异。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Diff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub newly_disabled: Vec<String>,
    pub newly_enabled: Vec<String>,
    pub packages_added: Vec<String>,
    pub packages_removed: Vec<String>,
}

impl Diff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.removed.is_empty()
            && self.newly_disabled.is_empty()
            && self.newly_enabled.is_empty()
            && self.packages_added.is_empty()
            && self.packages_removed.is_empty()
    }

    /// 只列会影响 Saiku 的差异：删行、包增删、开关翻转。新增行通常是能力变多，
    /// 值得知道但不必拦升级 —— 除非它是交互型工具（由 `[`REQUIRED_ROWS`]` 之外的单测管）。
    pub fn summary(&self) -> String {
        let mut out = Vec::new();
        if !self.removed.is_empty() {
            out.push(format!("删除 {} 行：{}", self.removed.len(), self.removed.join(", ")));
        }
        if !self.added.is_empty() {
            out.push(format!("新增 {} 行：{}", self.added.len(), self.added.join(", ")));
        }
        if !self.packages_removed.is_empty() {
            out.push(format!("移除包：{}", self.packages_removed.join(", ")));
        }
        if !self.packages_added.is_empty() {
            out.push(format!("新增包：{}", self.packages_added.join(", ")));
        }
        if !self.newly_disabled.is_empty() {
            out.push(format!("新禁用：{}", self.newly_disabled.join(", ")));
        }
        if !self.newly_enabled.is_empty() {
            out.push(format!("新启用：{}", self.newly_enabled.join(", ")));
        }
        if out.is_empty() {
            "行集合一致".to_string()
        } else {
            out.join("；")
        }
    }
}

/// 比较两份契约（`old` 是先前的快照，`new` 是现在的内核）。
pub fn diff(old: &Contract, new: &Contract) -> Diff {
    let old_ids = old.ids();
    let new_ids = new.ids();
    let mut d = Diff {
        added: new_ids.difference(&old_ids).map(|s| s.to_string()).collect(),
        removed: old_ids.difference(&new_ids).map(|s| s.to_string()).collect(),
        packages_added: new
            .packages()
            .difference(&old.packages())
            .map(|s| s.to_string())
            .collect(),
        packages_removed: old
            .packages()
            .difference(&new.packages())
            .map(|s| s.to_string())
            .collect(),
        ..Default::default()
    };
    for (id, now) in &new.rows {
        let Some(before) = old.rows.get(id) else { continue };
        match (before.disabled, now.disabled) {
            (Some(false), Some(true)) => d.newly_disabled.push(id.clone()),
            (Some(true), Some(false)) => d.newly_enabled.push(id.clone()),
            _ => {}
        }
    }
    d
}

/// 解析 `--dump-config` 的文本。
///
/// 形状（2026-11-13 用 `0.2.1-alpha.2` 与 `0.2.2-rc.2` 两个版本核对过）：
///
/// ```text
/// # == @deepseek-ai/dsh-base
/// - id: tool-goal
///   name: '@deepseek-ai/dsh-tool-goal'
///   disabled: true
/// ```
///
/// 只认顶层 `- id:` 开始的行；缩进行属于当前行。`!!js` 表达式只记「有 disabled 字段」，
/// 不评估 —— 猜一个布尔值比不知道更糟。
pub fn parse_dump(text: &str, version: &str, profile: &str) -> Contract {
    let mut rows = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        let trimmed = line.trim_end();
        if let Some(rest) = trimmed.strip_prefix("- id: ") {
            let id = rest.trim().trim_matches(['\'', '"']).to_string();
            if !id.is_empty() {
                rows.insert(id.clone(), Row::default());
                current = Some(id);
            }
            continue;
        }
        let Some(id) = &current else { continue };
        let Some(row) = rows.get_mut(id) else { continue };
        let body = trimmed.trim_start();
        if let Some(rest) = body.strip_prefix("name: ") {
            row.package = package_of(rest.trim().trim_matches(['\'', '"']));
        } else if let Some(rest) = body.strip_prefix("disabled: ") {
            let value = rest.trim();
            row.disabled = match value {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            };
        }
    }
    Contract { schema_version: 1, version: version.to_string(), profile: profile.to_string(), rows }
}

/// `@deepseek-ai/dsh-tool-goal` → `@deepseek-ai/dsh-tool-goal`；
/// `@deepseek-ai/dsh-plugin-manager/tools` → `@deepseek-ai/dsh-plugin-manager`。
fn package_of(name: &str) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    if let Some(rest) = name.strip_prefix('@') {
        let mut parts = rest.split('/');
        let scope = parts.next()?;
        let pkg = parts.next()?;
        return Some(format!("@{scope}/{pkg}"));
    }
    Some(name.split('/').next().unwrap_or(name).to_string())
}

/// 跑一次 dump。`entry` 是 `lib/bin.js`，`profile` 一般是 `sdk`。
pub fn dump(entry: &Path, profile: &str, version: &str) -> Result<Contract, String> {
    if !entry.is_file() {
        return Err(format!("找不到 DSH 入口 {}", entry.display()));
    }
    let mut child = std::process::Command::new("node")
        .arg(entry)
        .args(["--profile", profile, "--dump-config"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("运行 node 失败（需要 Node.js 在 PATH 上）：{e}"))?;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(None) => {
                let _ = child.kill();
                return Err("--dump-config 超时（60 秒）".to_string());
            }
            Err(e) => return Err(format!("等待 dump 失败：{e}")),
        }
    }
    let out = child.wait_with_output().map_err(|e| format!("读取 dump 输出失败：{e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "--dump-config 退出 {}；stderr：{}",
            out.status,
            err.lines().take(5).collect::<Vec<_>>().join(" | ")
        ));
    }
    Ok(parse_dump(&String::from_utf8_lossy(&out.stdout), version, profile))
}

/// Saiku 的 patch 会点名的行。**新增一行 patch 就要同步这里**，单测会逼你同步。
///
/// 这些 id 是 DSH 的内部配置行，不是公开 API —— 所以每次升级都要重新核对一遍，
/// 而这份清单就是核对的对象。
pub const REQUIRED_ROWS: &[&str] = &[
    // 执行层限制
    "sandbox-policy",
    "approval",
    "permission",
    // Saiku 自己管 Plan 状态
    "plan-mode",
    // shell / 网络 / 检索
    "tool-bash",
    "tool-pwsh",
    "tool-jobs",
    "tool-workflow",
    "ptc-runtime",
    "workflow-ptc",
    "tool-web",
    "session-query-sqlite",
    // 数据外发与模板
    "session-log-deepseek",
    "plugin-package-inventory-deepseek",
    "system-prompt",
    // 路由
    "llm-deepseek",
    "llm-pi-ai",
];

/// 版本比较：只比数字段，忽略预发布后缀（`0.2.2-rc.2` 视作 `0.2.2`）。
///
/// 用途只有一个 —— 判断某个**新增行**在不在：`tool-ask-user` 是 0.2.2 才有的，
/// 在 0.2.1 上写它会让内核打印 `patch: entry not found`，属于自己制造的噪声。
pub fn version_at_least(version: &str, minimum: &str) -> bool {
    fn parts(raw: &str) -> Vec<u64> {
        raw.split('-')
            .next()
            .unwrap_or("")
            .split('.')
            .map(|p| p.trim().parse::<u64>().unwrap_or(0))
            .collect()
    }
    let (a, b) = (parts(version), parts(minimum));
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        if x != y {
            return x > y;
        }
    }
    true
}

/// 快照路径：`<仓库>/contracts/dsh-<version>.json`。运行时不可知仓库位置，
/// 所以由调用方给目录。
pub fn contract_path(dir: &Path, version: &str) -> PathBuf {
    dir.join(format!("dsh-{version}.json"))
}

pub fn load(path: &Path) -> Option<Contract> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn save(path: &Path, contract: &Contract) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("建目录失败：{e}"))?;
    }
    let text = serde_json::to_string_pretty(contract).map_err(|e| e.to_string())?;
    std::fs::write(path, text + "\n").map_err(|e| format!("写 {} 失败：{e}", path.display()))
}


    #[test]
    fn versions_compare_on_numbers_and_ignore_prerelease() {
        assert!(version_at_least("0.2.2-rc.2", "0.2.2"));
        assert!(version_at_least("0.2.2", "0.2.1-alpha.2"));
        assert!(!version_at_least("0.2.1-alpha.2", "0.2.2"));
        assert!(version_at_least("1.0.0", "0.2.9"));
    }
#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
# == @deepseek-ai/dsh-base
- id: tool-plugin-manager
  name: '@deepseek-ai/dsh-plugin-manager/tools'
  disabled: true
- id: timer
  name: '@deepseek-ai/cordis-plugin-timer'
- id: llm
  name: '@deepseek-ai/dsh-llm'
  disabled: !!js '!ctx.get(''profileContext'')'
- id: tool-goal
  name: '@deepseek-ai/dsh-tool-goal'
"#;

    #[test]
    fn parse_keeps_ids_packages_and_only_literal_disabled() {
        let c = parse_dump(SAMPLE, "0.2.1", "sdk");
        assert_eq!(c.rows.len(), 4);
        assert_eq!(c.rows["tool-plugin-manager"].package.as_deref(), Some("@deepseek-ai/dsh-plugin-manager"));
        assert_eq!(c.rows["tool-plugin-manager"].disabled, Some(true));
        assert_eq!(c.rows["timer"].disabled, None, "没有 disabled 行 = 未声明");
        assert_eq!(c.rows["llm"].disabled, None, "!!js 表达式不猜");
        assert_eq!(c.rows["llm"].package.as_deref(), Some("@deepseek-ai/dsh-llm"));
    }

    #[test]
    fn diff_names_removed_rows_and_packages() {
        let old = parse_dump(SAMPLE, "0.2.1", "sdk");
        let new = parse_dump(
            r#"
- id: timer
  name: '@deepseek-ai/cordis-plugin-timer'
- id: llm
  name: '@deepseek-ai/dsh-llm'
- id: tool-ask-user
  name: '@deepseek-ai/dsh-tool-ask-user'
"#,
            "0.2.2",
            "sdk",
        );
        let d = diff(&old, &new);
        assert_eq!(d.removed, vec!["tool-goal", "tool-plugin-manager"]);
        assert!(d.added.contains(&"tool-ask-user".to_string()));
        assert!(d.summary().contains("删除 2 行"));
        assert!(d.packages_removed.contains(&"@deepseek-ai/dsh-plugin-manager".to_string()));
    }

    #[test]
    fn disabled_flip_is_visible() {
        let old = parse_dump("- id: tool-web\n  disabled: true\n", "a", "sdk");
        let new = parse_dump("- id: tool-web\n  disabled: false\n", "b", "sdk");
        let d = diff(&old, &new);
        assert_eq!(d.newly_enabled, vec!["tool-web"]);
    }
}
