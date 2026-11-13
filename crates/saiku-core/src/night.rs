//! 夜班的**只读视图**：从控制器的落盘状态里算出「昨晚怎么样」。
//!
//! 写者是 `saiku night`（`saiku-cli/src/night.rs`），这里只读，供 TUI 与将来的 GUI 用。
//! 为什么放在 core：前端不该知道 `state.json` 的字段名，换一版控制器只改这一处；
//! 而且「心跳过期 = 控制器已停」这条判据只能有一个出处，写两份迟早对不上。
//!
//! 判据全部来自文件：`state.json`（每件任务）、`heartbeat.json`（控制器还活着吗）。
//! 没有心跳就说「从未心跳」，不拿 mtime 假装。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// 心跳超过这么久没更新，控制器就算停了（与控制器自己的口径一致）。
pub const RUNNER_STALE_MS: u64 = 60_000;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Counts {
    pub done: usize,
    pub running: usize,
    pub pending: usize,
    pub failed: usize,
    pub stopped: usize,
}

impl Counts {
    pub fn total(&self) -> usize {
        self.done + self.running + self.pending + self.failed + self.stopped
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub task: String,
    pub code: Option<String>,
    pub message: Option<String>,
}

/// 一次夜班的只读报告。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub dir: PathBuf,
    pub run_id: String,
    /// `state.json` 的最后修改时间（毫秒）。列表排序与「多久以前」用它。
    pub updated_at: u64,
    pub counts: Counts,
    /// 控制器最后一次心跳距今；`None` = 从来没写过心跳。
    pub heartbeat_age_ms: Option<u64>,
    /// 心跳在 `RUNNER_STALE_MS` 以内。
    pub alive: bool,
    /// 失败的任务（带 code），按 id 排序。
    pub failures: Vec<Failure>,
    /// 正在跑的任务 id。
    pub running: Vec<String>,
    /// 还在排队、且依赖未满足/名额未到的任务数。
    pub pending: Vec<String>,
    /// 全部任务 id → 状态（用量汇总与将来别的视图都要按 id 找运行目录）。
    pub tasks: BTreeMap<String, String>,
}

impl Report {
    pub fn counts_of_tasks(&self) -> &BTreeMap<String, String> {
        &self.tasks
    }

    /// 一行结论。TUI 的页脚与 `saiku night status` 说的是同一句话。
    pub fn one_line(&self) -> String {
        let alive = match self.heartbeat_age_ms {
            None => "从未心跳".to_string(),
            Some(age) if self.alive => format!("心跳 {}s", age / 1000),
            Some(age) => format!("控制器已停 {}s", age / 1000),
        };
        format!(
            "night {}: {} done · {} running · {} pending · {} failed · {}",
            self.run_id, self.counts.done, self.counts.running, self.counts.pending, self.counts.failed, alive
        )
    }
}

#[derive(Debug, Deserialize)]
struct RawState {
    run_id: String,
    tasks: BTreeMap<String, RawTask>,
}

#[derive(Debug, Deserialize)]
struct RawTask {
    state: String,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawBeat {
    at: u64,
}

/// 整批夜班的用量合计（provider 实报的 token，不做估算）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageTotal {
    pub prompt: u64,
    pub cache_read: u64,
    pub completion: u64,
    /// 报告过用量的运行数。
    pub runs: usize,
}

impl UsageTotal {
    pub fn is_zero(&self) -> bool {
        self.runs == 0
    }
}

/// 从每件任务的状态目录里取 `usage` 累加。
///
/// 用 `Value` 而不是 cli 的 `StatusFile`：这一层只关心三个字段，不想把「状态文件长什么样」
/// 也变成 core 的契约（那属于 worker，不属于前端）。
pub fn usage(state_root: &Path, report: &Report) -> UsageTotal {
    let mut total = UsageTotal::default();
    for task in report.counts_of_tasks().keys() {
        let path = state_root.join("runs").join(task).join("status.json");
        let Ok(raw) = std::fs::read_to_string(&path) else { continue };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else { continue };
        let Some(usage) = value.get("usage") else { continue };
        let field = |name: &str| usage.get(name).and_then(|v| v.as_u64()).unwrap_or(0);
        total.prompt += field("prompt_tokens");
        total.cache_read += field("cache_read_tokens");
        total.completion += field("completion_tokens");
        total.runs += 1;
    }
    total
}

/// 读一个控制器目录。`state.json` 不在就返回 `None`（目录可能是半天前留下的空壳）。
pub fn read_dir(dir: &Path, now: u64) -> Option<Report> {
    let raw = std::fs::read_to_string(dir.join("state.json")).ok()?;
    let state: RawState = serde_json::from_str(&raw).ok()?;
    let beat: Option<RawBeat> = std::fs::read_to_string(dir.join("heartbeat.json"))
        .ok()
        .and_then(|b| serde_json::from_str(&b).ok());
    let heartbeat_age_ms = beat.as_ref().map(|b| now.saturating_sub(b.at));
    let alive = heartbeat_age_ms.is_some_and(|age| age < RUNNER_STALE_MS);

    let mut counts = Counts::default();
    let mut failures = Vec::new();
    let mut running = Vec::new();
    let mut pending = Vec::new();
    let mut tasks = BTreeMap::new();
    for (id, task) in &state.tasks {
        tasks.insert(id.clone(), task.state.clone());
        match task.state.as_str() {
            "done" => counts.done += 1,
            "running" => {
                counts.running += 1;
                running.push(id.clone());
            }
            "pending" => {
                counts.pending += 1;
                pending.push(id.clone());
            }
            "failed" => {
                counts.failed += 1;
                failures.push(Failure {
                    task: id.clone(),
                    code: task.code.clone(),
                    message: task.message.clone(),
                });
            }
            "stopped" => counts.stopped += 1,
            other => {
                // 未来的控制器可能加状态；不认识就计入 pending，别让它悄悄消失。
                let _ = other;
                counts.pending += 1;
                pending.push(id.clone());
            }
        }
    }
    let updated_at = std::fs::metadata(dir.join("state.json"))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    Some(Report {
        dir: dir.to_path_buf(),
        run_id: state.run_id,
        updated_at,
        counts,
        heartbeat_age_ms,
        alive,
        failures,
        running,
        pending,
        tasks,
    })
}

/// 最近几次夜班（新的在前，最多 `limit` 次）。`/night list` 与将来 GUI 的历史页用。
pub fn history(state_root: &Path, now: u64, limit: usize) -> Vec<Report> {
    let root = state_root.join("night");
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&root)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default();
    dirs.sort();
    let mut reports: Vec<Report> = dirs.iter().filter_map(|dir| read_dir(dir, now)).collect();
    reports.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(a.run_id.cmp(&b.run_id)));
    reports.truncate(limit.max(1));
    reports
}

/// 状态根下**最新**的一次夜班（按 `state.json` 的 mtime；没有就返回 `None`）。
pub fn latest(state_root: &Path, now: u64) -> Option<Report> {
    let root = state_root.join("night");
    let mut best: Option<(u64, PathBuf)> = None;
    for entry in std::fs::read_dir(&root).ok()?.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let state = path.join("state.json");
        let stamp = std::fs::metadata(&state)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64);
        let Some(stamp) = stamp else { continue };
        if best.as_ref().is_none_or(|(s, _)| stamp > *s) {
            best = Some((stamp, path));
        }
    }
    read_dir(&best?.1, now)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, state: &str, beat: Option<u64>) {
        std::fs::write(dir.join("state.json"), state).unwrap();
        if let Some(at) = beat {
            std::fs::write(dir.join("heartbeat.json"), format!(r#"{{"at":{at}}}"#)).unwrap();
        }
    }

    #[test]
    fn a_finished_run_reports_counts_and_no_failures() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            r#"{"run_id":"n1","tasks":{"a":{"state":"done"},"b":{"state":"done"}}}"#,
            Some(1_000),
        );
        let report = read_dir(dir.path(), 5_000).unwrap();
        assert_eq!(report.counts.done, 2);
        assert!(report.alive, "心跳 4 秒前，控制器应算活着");
        assert!(report.failures.is_empty());
        assert!(report.one_line().contains("2 done"));
    }

    #[test]
    fn a_dead_controller_is_named_as_such() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), r#"{"run_id":"n2","tasks":{}}"#, Some(1_000));
        let report = read_dir(dir.path(), 1_000 + RUNNER_STALE_MS + 1).unwrap();
        assert!(!report.alive);
        assert!(report.one_line().contains("控制器已停"));
    }

    #[test]
    fn missing_heartbeat_is_never_alive() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), r#"{"run_id":"n3","tasks":{}}"#, None);
        let report = read_dir(dir.path(), 9_999).unwrap();
        assert!(!report.alive);
        assert!(report.one_line().contains("从未心跳"));
    }

    #[test]
    fn failures_keep_the_code_so_the_next_morning_has_a_reason() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            r#"{"run_id":"n4","tasks":{"w1":{"state":"failed","code":"MODEL_ROUTE","message":"bad model"},"w2":{"state":"running"}}}"#,
            Some(0),
        );
        let report = read_dir(dir.path(), 1).unwrap();
        assert_eq!(report.failures[0].task, "w1");
        assert_eq!(report.failures[0].code.as_deref(), Some("MODEL_ROUTE"));
        assert_eq!(report.running, vec!["w2".to_string()]);
    }
    #[test]
    fn history_is_newest_first_and_bounded() {
        use std::time::{Duration, SystemTime};
        let root = tempfile::tempdir().unwrap();
        for (id, minutes) in [("a", 90u64), ("b", 5u64), ("c", 30u64)] {
            let dir = root.path().join("night").join(id);
            std::fs::create_dir_all(&dir).unwrap();
            let state = dir.join("state.json");
            std::fs::write(&state, format!(r#"{{"run_id":"{id}","tasks":{{}}}}"#)).unwrap();
            let stamp = SystemTime::now() - Duration::from_secs(minutes * 60);
            std::fs::File::options().write(true).open(&state).unwrap().set_modified(stamp).unwrap();
        }
        let reports = history(root.path(), 0, 2);
        assert_eq!(reports.len(), 2, "limit 生效");
        assert_eq!(reports[0].run_id, "b");
        assert_eq!(reports[1].run_id, "c");
    }

}
