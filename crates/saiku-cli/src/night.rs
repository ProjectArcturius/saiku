//! `saiku night` —— 夜班控制器：一个队列、多个工人、没人看着。
//!
//! 夜班和白班的分界不是「谁在跑」，而是**出事的时候有没有人在看**。所以这一层的职责
//! 只有一个：让「Saiku 死了」这件事在第二天是一行字，而不是一段浪费掉的时间。
//!
//! 三条设计：
//!
//! 1. **状态全在磁盘**：`plan.json`（只读）、`state.json`（可变，原子替换）、`night.log`
//!    （追加）、`heartbeat.json`。控制器自己被杀了，重跑同一条命令就接着干 ——
//!    它不依赖任何内存里的东西。
//! 2. **监工是子进程，不是线程**：子进程死了控制器能立刻看见（`try_wait`）；控制器死了
//!    子进程还活着，重启后按 `status.json` 的健康度**接管**已经在跑的任务，不重派。
//! 3. **重派要花在刀刃上**：只有「再派一次可能就好了」的失败才重派；账户级的失败先把
//!    那个账户冷却掉，让下一次取号落到同池的另一个 key 上；跨 GA 漂移一律不做 ——
//!    那等于换模型能力，是一次产品决定。
//!
//! 队列是拉动的：依赖全部 `done` 才允许起跑，在跑数到 `max_parallel` 为止。

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use saiku_core::error::{code, SpecError, SpecResult};
use saiku_core::group_army;
use saiku_core::userconfig::UserConfig;

use crate::status;

const POLL: Duration = Duration::from_millis(1500);
const HEARTBEAT_EVERY: Duration = Duration::from_secs(5);

// ── 计划 ───────────────────────────────────────────────────────────────

/// 夜班计划（`night.json`）。**只读**：跑起来之后改它没用，改的是 `state.json` 的补派。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    #[serde(default = "schema_version")]
    pub schema_version: u32,
    /// 这次运行的 id；同时是状态目录名。换一次夜班就换一个。
    pub run_id: String,
    /// 启动配置；缺省按 `--config` / `$SAIKU_CONFIG` / 标准位置找。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<PathBuf>,
    /// 状态根；缺省取配置里的 `state_root`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_root: Option<PathBuf>,
    /// 控制器自己的目录；缺省 `<状态根>/night/<run_id>`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub night_dir: Option<PathBuf>,
    /// 队列级的默认 GA；任务可以覆盖。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ga: Option<String>,
    /// 同时在跑的工人上限。缺省 4 —— 不是容量上限，是**审阅能力**的上限。
    #[serde(default = "default_parallel")]
    pub max_parallel: usize,
    #[serde(default)]
    pub retry: Retry,
    pub tasks: Vec<PlanTask>,
}

fn schema_version() -> u32 {
    1
}

fn default_parallel() -> usize {
    4
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Retry {
    /// 每件任务最多跑几次（含第一次）。
    #[serde(default = "default_attempts")]
    pub attempts: u32,
    /// 限流类失败后该账户冷却多久。
    #[serde(default = "default_cooldown")]
    pub cooldown_seconds: u64,
}

fn default_attempts() -> u32 {
    2
}

fn default_cooldown() -> u64 {
    180
}

impl Default for Retry {
    fn default() -> Self {
        Self { attempts: default_attempts(), cooldown_seconds: default_cooldown() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanTask {
    /// 任务 id，同时是状态目录名。整夜唯一。
    pub id: String,
    /// 任务书（Markdown 或 JSON）。
    pub book: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    /// 验收命令，逐条原样传给 `saiku run --accept`。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accept: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub needs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ga: Option<String>,
    /// 任务书里的 `--by`（派工关系的边）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
}

// ── 状态 ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NightState {
    pub schema_version: u32,
    pub run_id: String,
    pub tasks: BTreeMap<String, TaskState>,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskState {
    /// `pending` / `running` / `done` / `failed` / `stopped`
    pub state: String,
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ga: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<u64>,
}

impl TaskState {
    fn pending() -> Self {
        Self {
            state: "pending".into(),
            attempts: 0,
            ga: None,
            account: None,
            code: None,
            message: None,
            pid: None,
            started_at: None,
            ended_at: None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Heartbeat {
    run_id: String,
    at: u64,
    done: usize,
    running: usize,
    pending: usize,
    failed: usize,
    note: String,
}

// ── 命令 ───────────────────────────────────────────────────────────────

pub fn cmd_night(argv: &[String]) -> Result<i32, SpecError> {
    let sub = argv.first().map(String::as_str).unwrap_or("status");
    match sub {
        "run" => cmd_run(&argv[1..]),
        "status" => cmd_status(&argv[1..]),
        "stop" => cmd_stop(&argv[1..]),
        other => Err(SpecError::new(code::CONFIG_INVALID, format!("未知命令 `night {other}`；用 run / status / stop"))),
    }
}

fn cmd_run(argv: &[String]) -> Result<i32, SpecError> {
    let flags = crate::commands::Flags::parse(
        argv,
        &["--config", "--state-dir"],
        &["--detach", "--json"],
    )?;
    let plan_path = flags
        .positional
        .first()
        .ok_or_else(|| SpecError::new(code::CONFIG_INVALID, "用法：saiku night run <night.json>"))?;
    let plan = load_plan(Path::new(plan_path))?;
    let (cfg, cfg_path) = UserConfig::locate_and_load(flags.one("--config").map(Path::new))?;
    let ctx = Context::resolve(
        &plan,
        &cfg,
        Some(&cfg_path),
        flags.one("--state-dir").map(Path::new),
    )?;

    if flags.has("--detach") {
        return detach(&ctx, Path::new(plan_path));
    }
    run_loop(&plan, &ctx)
}

fn load_plan(path: &Path) -> SpecResult<Plan> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| SpecError::new(code::CONFIG_UNREADABLE, format!("{} —— {e}", path.display())))?;
    let plan: Plan = serde_json::from_str(&raw).map_err(|e| {
        SpecError::new(code::CONFIG_INVALID, format!("{} 不是合法的夜班计划 —— {e}", path.display()))
    })?;
    if plan.tasks.is_empty() {
        return Err(SpecError::new(code::CONFIG_INVALID, "夜间计划里没有任务；空队列不能开工"));
    }
    let mut ids = std::collections::BTreeSet::new();
    for task in &plan.tasks {
        if !ids.insert(task.id.clone()) {
            return Err(SpecError::new(code::CONFIG_INVALID, format!("任务 id `{}` 写了两次", task.id)));
        }
    }
    for task in &plan.tasks {
        for dep in &task.depends_on {
            if !ids.contains(dep) {
                return Err(SpecError::new(
                    code::CONFIG_INVALID,
                    format!("任务 `{}` 依赖 `{dep}`，但计划里没有这个任务", task.id),
                ));
            }
        }
    }
    Ok(plan)
}

/// 运行上下文：所有路径都先算成绝对路径，子进程的 cwd 与配置无关。
struct Context {
    night_dir: PathBuf,
    state_root: PathBuf,
    config: PathBuf,
}

impl Context {
    fn resolve(
        plan: &Plan,
        cfg: &UserConfig,
        loaded_config: Option<&Path>,
        override_dir: Option<&Path>,
    ) -> SpecResult<Self> {
        let state_root = plan.state_root.clone().unwrap_or_else(|| cfg.state_root());
        let night_dir = override_dir
            .map(Path::to_path_buf)
            .or_else(|| plan.night_dir.clone())
            .unwrap_or_else(|| state_root.join("night").join(&plan.run_id));
        // 子进程必须拿到**同一份**配置：`--config` 显式给的优先，其次计划里写死的，
        // 最后才是实际加载到的那一份 —— 不能再按环境变量猜一次，那是两份真相。
        let config = plan
            .config
            .clone()
            .or_else(|| loaded_config.map(Path::to_path_buf))
            .unwrap_or_else(|| {
                saiku_core::userconfig::config_dir().join("saiku").join("saiku.json")
            });
        Ok(Self { night_dir, state_root, config })
    }
}

fn detach(ctx: &Context, plan_path: &Path) -> Result<i32, SpecError> {
    std::fs::create_dir_all(&ctx.night_dir).map_err(io(&ctx.night_dir))?;
    let log = ctx.night_dir.join("runner.log");
    let out = std::fs::File::create(&log).map_err(io(&log))?;
    let err = out.try_clone().map_err(io(&log))?;
    let exe = std::env::current_exe()
        .map_err(|e| SpecError::new(code::CONFIG_INVALID, format!("取自身路径失败：{e}")))?;
    let mut cmd = Command::new(exe);
    cmd.args(["night", "run"]).arg(plan_path);
    cmd.arg("--state-dir").arg(&ctx.night_dir);
    cmd.arg("--config").arg(&ctx.config);
    let mut child = saiku_dsh::process::no_window(&mut cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .spawn()
        .map_err(|e| SpecError::new(code::CONFIG_INVALID, format!("分离启动失败：{e}")))?;
    // 握手判据：心跳文件出现 = 控制器接住了；子进程先退出 = 没接住。
    let beat = ctx.night_dir.join("heartbeat.json");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if beat.is_file() {
            println!("{}", ctx.night_dir.display());
            return Ok(0);
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(SpecError::new(
                code::CONFIG_INVALID,
                format!("控制器没接住（退出 {status}）；看 {}", log.display()),
            ));
        }
        if Instant::now() > deadline {
            println!("{}", ctx.night_dir.display());
            return Ok(0);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

// ── 主循环 ─────────────────────────────────────────────────────────────

struct Running {
    id: String,
    child: Option<Child>,
}

fn run_loop(plan: &Plan, ctx: &Context) -> Result<i32, SpecError> {
    std::fs::create_dir_all(&ctx.night_dir).map_err(io(&ctx.night_dir))?;
    copy_plan(plan, ctx)?;
    let mut state = load_state(plan, ctx)?;
    let mut log = Log::open(&ctx.night_dir.join("night.log"))?;
    log.line(&format!("night {} 开始：{} 件任务，并行上限 {}", plan.run_id, plan.tasks.len(), plan.max_parallel));
    // 上一次控制器留下的 running 记录：能接管就接管，接不了就当一次失败重派。
    let mut running: Vec<Running> = Vec::new();
    for task in &plan.tasks {
        if state.tasks.get(&task.id).is_some_and(|s| s.state == "running") {
            log.line(&format!("{} 上次停在 running，按状态文件接管", task.id));
        }
    }

    let mut last_beat = Instant::now() - HEARTBEAT_EVERY;
    loop {
        if stop_requested(ctx) {
            log.line("收到 stop，取消在跑的任务并停止接新任务");
            // 先写取消文件让监工自己收尾（它会 shutdown 内核、写终态）；
            // 20 秒还不退再杀，避免留下一个「已经说停但还在跑」的孤儿。
            for run in &running {
                let cancel = ctx.state_root.join("runs").join(&run.id).join("cancel.json");
                let body = serde_json::json!({
                    "task_id": run.id,
                    "action": "cancel",
                    "reason": "夜班控制器收到 stop",
                });
                let _ = std::fs::write(&cancel, serde_json::to_vec_pretty(&body).unwrap_or_default());
            }
            let deadline = Instant::now() + Duration::from_secs(20);
            while Instant::now() < deadline {
                let all_gone = running.iter_mut().all(|r| match &mut r.child {
                    Some(child) => matches!(child.try_wait(), Ok(Some(_))),
                    None => read_health(&ctx.state_root, &r.id) == "done"
                        || read_health(&ctx.state_root, &r.id) == "dead",
                });
                if all_gone {
                    break;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            for run in &mut running {
                if let Some(child) = &mut run.child {
                    let _ = child.kill();
                }
                if let Some(st) = state.tasks.get_mut(&run.id) {
                    st.state = "stopped".into();
                    st.ended_at = Some(status::now_ms());
                }
            }
            for task in &plan.tasks {
                if let Some(st) = state.tasks.get_mut(&task.id)
                    && st.state == "pending"
                {
                    st.state = "stopped".into();
                    st.ended_at = Some(status::now_ms());
                }
            }
            save_state(&mut state, ctx)?;
            log.line("已停止");
            return Ok(3);
        }

        // 1) 收在跑的：子进程退出 = 监工退出，结论在 status.json 里。
        let mut settled: Vec<String> = Vec::new();
        for run in &mut running {
            let exited = match &mut run.child {
                Some(child) => matches!(child.try_wait(), Ok(Some(_))),
                // 接管的进程没有句柄，只能看状态文件。
                None => {
                    let health = read_health(&ctx.state_root, &run.id);
                    health == "done" || health == "dead"
                }
            };
            if exited {
                settled.push(run.id.clone());
            }
        }
        running.retain(|r| !settled.contains(&r.id));
        for id in settled {
            settle(plan, ctx, &mut state, &mut log, &id)?;
        }

        // 2) 起新的：依赖 done、名额未满。
        let done: Vec<String> = state
            .tasks
            .iter()
            .filter(|(_, s)| s.state == "done")
            .map(|(id, _)| id.clone())
            .collect();
        for task in &plan.tasks {
            if running.len() >= plan.max_parallel {
                break;
            }
            let Some(st) = state.tasks.get(&task.id) else { continue };
            if st.state != "pending" && st.state != "running" {
                continue;
            }
            if task.depends_on.iter().any(|d| !done.contains(d)) {
                continue;
            }
            if st.state == "running" {
                // 接管：状态文件说还活着，就把它算进「在跑」，不重派；已经接管过就跳过。
                if read_health(&ctx.state_root, &task.id) != "dead" {
                    if !running.iter().any(|r| r.id == task.id) {
                        log.line(&format!("{} 接管（监工还活着）", task.id));
                        running.push(Running { id: task.id.clone(), child: None });
                    }
                    continue;
                }
            }
            match spawn(plan, ctx, task) {
                Ok(child) => {
                    let st = state.tasks.get_mut(&task.id).expect("任务状态应已初始化");
                    st.state = "running".into();
                    st.attempts += 1;
                    st.pid = Some(child.id());
                    st.started_at = Some(status::now_ms());
                    st.code = None;
                    st.message = None;
                    log.line(&format!(
                        "{} 起跑（第 {} 次，pid {}）",
                        task.id, st.attempts, child.id()
                    ));
                    running.push(Running { id: task.id.clone(), child: Some(child) });
                }
                Err(e) => {
                    let st = state.tasks.get_mut(&task.id).expect("任务状态应已初始化");
                    st.state = "failed".into();
                    st.code = Some(e.code.clone());
                    st.message = Some(e.message.clone());
                    st.ended_at = Some(status::now_ms());
                    log.line(&format!("{} 起跑失败：{} {}", task.id, e.code, e.message));
                }
            }
            save_state(&mut state, ctx)?;
        }

        // 3) 心跳 + 收尾判断。
        if last_beat.elapsed() >= HEARTBEAT_EVERY {
            write_heartbeat(&state, ctx, &running)?;
            save_state(&mut state, ctx)?;
            last_beat = Instant::now();
        }
        let all_terminal = plan
            .tasks
            .iter()
            .all(|t| state.tasks.get(&t.id).is_some_and(|s| matches!(s.state.as_str(), "done" | "failed" | "stopped")));
        if all_terminal && running.is_empty() {
            let done = count(&state, "done");
            let failed = count(&state, "failed");
            log.line(&format!("整夜结束：done={done} failed={failed}"));
            write_heartbeat(&state, ctx, &running)?;
            save_state(&mut state, ctx)?;
            return Ok(if failed == 0 { 0 } else { 1 });
        }
        std::thread::sleep(POLL);
    }
}

fn spawn(plan: &Plan, ctx: &Context, task: &PlanTask) -> Result<Child, SpecError> {
    let exe = std::env::current_exe()
        .map_err(|e| SpecError::new(code::CONFIG_INVALID, format!("取自身路径失败：{e}")))?;
    let run_dir = ctx.state_root.join("runs").join(&task.id);
    std::fs::create_dir_all(&run_dir).map_err(io(&run_dir))?;
    let stdout = std::fs::File::create(run_dir.join("console.out")).map_err(io(&run_dir))?;
    let stderr = std::fs::File::create(run_dir.join("console.err")).map_err(io(&run_dir))?;
    let mut cmd = Command::new(&exe);
    cmd.arg("run").arg(&task.book);
    cmd.arg("--id").arg(&task.id);
    cmd.arg("--config").arg(&ctx.config);
    cmd.arg("--state-root").arg(&ctx.state_root);
    cmd.arg("--format").arg("jsonl");
    cmd.arg("--permission").arg(task.permission.as_deref().unwrap_or("full"));
    cmd.arg("--rerun"); // 重派时把旧记录改名留档，不删
    if let Some(branch) = &task.worktree {
        cmd.arg("--worktree").arg(branch);
    }
    if let Some(by) = &task.by {
        cmd.arg("--by").arg(by);
    }
    if let Some(ga) = task.ga.as_ref().or(plan.ga.as_ref()) {
        cmd.arg("--ga").arg(ga);
    }
    for need in &task.needs {
        cmd.arg("--need").arg(need);
    }
    for accept in &task.accept {
        cmd.arg("--accept").arg(accept);
    }
    saiku_dsh::process::no_window(&mut cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .map_err(|e| SpecError::new(code::CONFIG_INVALID, format!("启动 `{}` 失败：{e}", task.book.display())))
}

/// 一件任务结束了：读结论，决定「记完成」还是「再派一次」。
fn settle(
    plan: &Plan,
    ctx: &Context,
    state: &mut NightState,
    log: &mut Log,
    id: &str,
) -> SpecResult<()> {
    let task = plan.tasks.iter().find(|t| t.id == id).expect("任务在计划里");
    let status_path = ctx.state_root.join("runs").join(id).join(status::STATUS_FILE);
    let status = status::read(&status_path);
    let st = state.tasks.get_mut(id).expect("任务状态应已初始化");
    let attempt = st.attempts;

    let verdict = match status.as_ref() {
        Some(st) => verdict(Some(st)),
        // 没有状态文件时，`console.err` 的第一行往往就是预检拒绝的原因
        // （`GA_CAPACITY: …` / `CONFIG_INVALID: …`）—— 别把它当成「监工莫名死了」。
        None => verdict_from_console(&ctx.state_root.join("runs").join(id).join("console.err")),
    };
    match verdict {
        Verdict::Done => {
            st.state = "done".into();
            st.code = None;
            st.message = None;
            st.ended_at = Some(status::now_ms());
            if let Some(s) = &status {
                st.account = s.account.clone();
                st.ga = s.ga.clone();
            }
            log.line(&format!("{id} 完成（第 {attempt} 次）"));
        }
        Verdict::Retry { code: why, message, provider_side } => {
            if attempt < plan.retry.attempts.max(1) {
                if provider_side
                    && let (Some(ga), Some(account)) = (st.ga.clone().or(task.ga.clone()).or(plan.ga.clone()), st.account.clone())
                {
                    let seconds = cooldown_for(&why, plan.retry.cooldown_seconds);
                    let _ = group_army::cooldown(&ga, &account, &ctx.state_root, seconds, &why);
                    log.line(&format!("{id} 失败 {why}，账户 {ga}/{account} 冷却 {seconds}s"));
                }
                st.state = "pending".into();
                st.code = Some(why.clone());
                st.message = Some(message.clone());
                st.pid = None;
                log.line(&format!("{id} 重派（第 {attempt}/{attempt_next} 次）：{why} {message}", attempt_next = attempt + 1));
            } else {
                st.state = "failed".into();
                st.code = Some(why.clone());
                st.message = Some(message);
                st.ended_at = Some(status::now_ms());
                log.line(&format!("{id} 失败（已用完 {attempt} 次）：{why}"));
            }
        }
        Verdict::Stop { code: why, message } => {
            st.state = "failed".into();
            st.code = Some(why.clone());
            st.message = Some(message.clone());
            st.ended_at = Some(status::now_ms());
            log.line(&format!("{id} 停止：{why} {message}"));
        }
    }
    save_state(state, ctx)
}

/// 失败归成三类动作。**认不出就不重派** —— 重派的代价是烧配额，看一眼的代价是零。
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Done,
    Retry { code: String, message: String, provider_side: bool },
    Stop { code: String, message: String },
}

fn verdict(status: Option<&status::StatusFile>) -> Verdict {
    let Some(status) = status else {
        // 连状态文件都没有：监工在预检里就死了，或者根本没起来。
        return Verdict::Retry {
            code: "SUPERVISOR_DIED".into(),
            message: "监工没有写出状态（预检失败或进程被杀）".into(),
            provider_side: false,
        };
    };
    match status.state.as_str() {
        "completed" => Verdict::Done,
        "cancelled" => Verdict::Stop { code: "CANCELLED".into(), message: "被取消".into() },
        "blocked" => Verdict::Stop {
            code: status.last_error.as_ref().map(|e| e.code.clone()).unwrap_or_else(|| "BLOCKED".into()),
            message: "任务受阻，需要人重新规划".into(),
        },
        "timed_out" => Verdict::Retry {
            code: "TIMED_OUT".into(),
            message: "超过墙钟预算".into(),
            provider_side: false,
        },
        "failed" => {
            let (why, message, retryable) = match &status.last_error {
                Some(e) => (e.code.clone(), e.message.clone(), e.retryable),
                None => ("FAILED".into(), "没有给出原因".into(), false),
            };
            if retryable {
                Verdict::Retry { code: why.clone(), message, provider_side: provider_side(&why) }
            } else {
                Verdict::Stop { code: why, message }
            }
        }
        _ => {
            // 非终态：到这里说明子进程已经退出但没写终态，等同于监工死了。
            Verdict::Retry {
                code: "SUPERVISOR_DIED".into(),
                message: format!("监工退出但终态停在 `{}`", status.state),
                provider_side: false,
            }
        }
    }
}

/// 没有 `status.json` 时的兜底：从监工的 stderr 里认一个 `CODE: 说明` 前缀。
///
/// 派工在预检里被拒（没有名额、配置错、任务书读不了）时，工人还没建立状态面；
/// 若一律记成 `SUPERVISOR_DIED`，第二天只会看到「莫名死了」，而真实原因是
/// 「三个账户都占满了」或「任务书路径写错」。
fn verdict_from_console(path: &Path) -> Verdict {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    let (head, message) = first.split_once(':').unwrap_or((first, ""));
    let known = !head.is_empty()
        && head.chars().all(|c| c.is_ascii_uppercase() || c == '_')
        && head.contains('_');
    if !known {
        return Verdict::Retry {
            code: "SUPERVISOR_DIED".into(),
            message: if first.is_empty() {
                "监工没有写出状态，也没有留下 stderr".into()
            } else {
                first.to_string()
            },
            provider_side: false,
        };
    }
    match head {
        // 池子满了不是错误：等冷却/空出来再派。
        "GA_CAPACITY" => Verdict::Retry {
            code: head.into(),
            message: message.trim().to_string(),
            provider_side: false,
        },
        // 配置、任务书、授权、运行时缺失 —— 再派几次都一样。
        _ => Verdict::Stop { code: head.into(), message: message.trim().to_string() },
    }
}

fn provider_side(code: &str) -> bool {
    matches!(
        code,
        "MODEL_RATE_LIMITED" | "MODEL_QUOTA_EXHAUSTED" | "MODEL_AUTH" | "MODEL_SERVER_ERROR" | "MODEL_NETWORK"
    )
}

fn cooldown_for(code: &str, base: u64) -> u64 {
    match code {
        // 额度与密钥不是等一会儿就好的：那一夜就别再用这个账户了。
        "MODEL_QUOTA_EXHAUSTED" | "MODEL_AUTH" => 12 * 3600,
        "MODEL_SERVER_ERROR" => base.max(300),
        _ => base,
    }
}

// ── status / stop ──────────────────────────────────────────────────────

fn cmd_status(argv: &[String]) -> Result<i32, SpecError> {
    let flags = crate::commands::Flags::parse(argv, &["--state-dir"], &["--json"])?;
    let dir = flags
        .positional
        .first()
        .map(PathBuf::from)
        .or_else(|| flags.one("--state-dir").map(PathBuf::from))
        .ok_or_else(|| SpecError::new(code::CONFIG_INVALID, "用法：saiku night status <控制器目录>"))?;
    // 结论与判据都来自 `saiku_core::night`：TUI 的 `/night` 与这里读的是同一个函数，
    // 两边各算一份的话，「控制器已停」的阈值迟早会分叉。
    let now = status::now_ms();
    let report = saiku_core::night::read_dir(&dir, now).ok_or_else(|| {
        SpecError::new(code::CONFIG_INVALID, format!("{} 里没有 night 的运行记录", dir.display()))
    })?;
    if flags.has("--json") {
        let state: Option<NightState> = read_json(&dir.join("state.json"));
        let body = serde_json::json!({
            "run_id": report.run_id,
            "tasks": state.map(|s| s.tasks).unwrap_or_default(),
            "counts": {
                "done": report.counts.done,
                "running": report.counts.running,
                "pending": report.counts.pending,
                "failed": report.counts.failed,
                "stopped": report.counts.stopped,
            },
            "heartbeat_age_ms": report.heartbeat_age_ms,
            "runner_alive": report.alive,
        });
        println!("{}", serde_json::to_string_pretty(&body).unwrap_or_default());
    } else {
        println!("{}", report.one_line());
        if let Some(failure) = report.failures.first() {
            println!("  失败：{} {}", failure.task, failure.code.clone().unwrap_or_default());
        } else if let Some(running) = report.running.first() {
            println!("  在跑：{running}");
        }
    }
    Ok(if report.counts.failed > 0 {
        1
    } else if report.counts.pending + report.counts.running > 0 {
        10
    } else {
        0
    })
}

fn cmd_stop(argv: &[String]) -> Result<i32, SpecError> {
    let flags = crate::commands::Flags::parse(argv, &["--state-dir"], &[])?;
    let dir = flags
        .positional
        .first()
        .map(PathBuf::from)
        .or_else(|| flags.one("--state-dir").map(PathBuf::from))
        .ok_or_else(|| SpecError::new(code::CONFIG_INVALID, "用法：saiku night stop <控制器目录>"))?;
    std::fs::create_dir_all(&dir).map_err(io(&dir))?;
    std::fs::write(dir.join("stop"), status::now_ms().to_string()).map_err(io(&dir))?;
    println!("已请求停止：{}", dir.display());
    Ok(0)
}

fn stop_requested(ctx: &Context) -> bool {
    ctx.night_dir.join("stop").exists()
}

// ── 落盘小工具 ─────────────────────────────────────────────────────────

fn load_state(plan: &Plan, ctx: &Context) -> SpecResult<NightState> {
    let path = ctx.night_dir.join("state.json");
    if let Some(mut state) = read_json::<NightState>(&path) {
        if state.run_id != plan.run_id {
            return Err(SpecError::new(
                code::CONFIG_INVALID,
                format!("{} 属于 run_id `{}`，与计划的 `{}` 不一致", path.display(), state.run_id, plan.run_id),
            ));
        }
        for task in &plan.tasks {
            state.tasks.entry(task.id.clone()).or_insert_with(TaskState::pending);
        }
        return Ok(state);
    }
    let mut state = NightState {
        schema_version: 1,
        run_id: plan.run_id.clone(),
        tasks: BTreeMap::new(),
        updated_at: status::now_ms(),
    };
    for task in &plan.tasks {
        state.tasks.insert(task.id.clone(), TaskState::pending());
    }
    save_state(&mut state, ctx)?;
    Ok(state)
}

fn save_state(state: &mut NightState, ctx: &Context) -> SpecResult<()> {
    state.updated_at = status::now_ms();
    write_json(&ctx.night_dir.join("state.json"), state)
}

fn write_heartbeat(state: &NightState, ctx: &Context, running: &[Running]) -> SpecResult<()> {
    let note = running.iter().map(|r| r.id.clone()).collect::<Vec<_>>().join(",");
    let beat = Heartbeat {
        run_id: state.run_id.clone(),
        at: status::now_ms(),
        done: count(state, "done"),
        running: count(state, "running"),
        pending: count(state, "pending"),
        failed: count(state, "failed"),
        note,
    };
    write_json(&ctx.night_dir.join("heartbeat.json"), &beat)
}

fn copy_plan(plan: &Plan, ctx: &Context) -> SpecResult<()> {
    let path = ctx.night_dir.join("plan.json");
    if !path.exists() {
        write_json(&path, plan)?;
    }
    Ok(())
}

fn count(state: &NightState, want: &str) -> usize {
    state.tasks.values().filter(|s| s.state == want).count()
}

fn read_health(state_root: &Path, id: &str) -> String {
    let path = state_root.join("runs").join(id).join(status::STATUS_FILE);
    match status::read(&path) {
        Some(st) => status::health(&st, status::now_ms()).to_string(),
        None => "dead".into(),
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> SpecResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io(parent))?;
    }
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(value).unwrap_or_default();
    std::fs::write(&tmp, text).map_err(io(&tmp))?;
    std::fs::rename(&tmp, path).map_err(io(path))
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn io(path: &Path) -> impl Fn(std::io::Error) -> SpecError + '_ {
    move |e| SpecError::new(code::CONFIG_INVALID, format!("{} —— {e}", path.display()))
}

struct Log {
    file: std::fs::File,
}

impl Log {
    fn open(path: &Path) -> SpecResult<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(io(path))?;
        Ok(Self { file })
    }

    fn line(&mut self, text: &str) {
        let stamp = status::now_ms() / 1000;
        let _ = writeln!(self.file, "[{stamp}] {text}");
        println!("[{stamp}] {text}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status_with(state: &str, code: Option<&str>, retryable: bool) -> status::StatusFile {
        let mut st = sample_status();
        st.state = state.into();
        st.last_error = code.map(|c| status::LastError {
            code: c.into(),
            message: "x".into(),
            retryable,
        });
        st
    }

    fn sample_status() -> status::StatusFile {
        serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "task_id": "t",
            "state": "running",
            "host": "h",
            "pid": 1,
            "workspace": "/w",
            "started_at": 0,
            "heartbeat_at": 0,
            "progress_at": 0,
            "activity": { "kind": "idle", "since": 0 },
            "counters": { "steps": 0, "tool_calls": 0, "tool_errors": 0 },
            "attempt": 1,
            "max_attempts": 3,
            "limits": { "heartbeat_s": 15, "model_idle_s": 600, "tool_stall_s": 1800 }
        }))
        .unwrap()
    }

    #[test]
    fn completed_is_done() {
        assert_eq!(verdict(Some(&status_with("completed", None, false))), Verdict::Done);
    }

    #[test]
    fn retryable_failure_retries_and_names_the_code() {
        match verdict(Some(&status_with("failed", Some("MODEL_RATE_LIMITED"), true))) {
            Verdict::Retry { code, provider_side, .. } => {
                assert_eq!(code, "MODEL_RATE_LIMITED");
                assert!(provider_side);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn non_retryable_failure_stops() {
        assert!(matches!(
            verdict(Some(&status_with("failed", Some("MODEL_ROUTE"), false))),
            Verdict::Stop { .. }
        ));
    }

    #[test]
    fn missing_status_is_a_dead_supervisor_and_is_retryable() {
        assert!(matches!(verdict(None), Verdict::Retry { provider_side: false, .. }));
    }

    #[test]
    fn blocked_needs_a_human() {
        assert!(matches!(verdict(Some(&status_with("blocked", None, false))), Verdict::Stop { .. }));
    }

    #[test]
    fn quota_cooldown_is_longer_than_rate_limit() {
        assert!(cooldown_for("MODEL_QUOTA_EXHAUSTED", 180) > cooldown_for("MODEL_RATE_LIMITED", 180) * 10);
    }

    #[test]
    fn plan_rejects_unknown_dependencies() {
        let raw = r#"{"run_id":"n","tasks":[{"id":"a","book":"/x.md","depends_on":["b"]}]}"#;
        let plan: Plan = serde_json::from_str(raw).unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("p.json"), raw).unwrap();
        let err = load_plan(&dir.path().join("p.json")).unwrap_err();
        assert_eq!(err.code, code::CONFIG_INVALID, "{}", err.message);
        assert!(err.message.contains("依赖"));
        let _ = plan;
    }
}
