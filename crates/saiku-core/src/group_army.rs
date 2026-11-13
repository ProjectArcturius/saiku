//! 集团军（Group Army, GA）：同一类模型能力的**账户池**。
//!
//! 一个 GA 对应一家供应商的一个模型族，底下挂多个**互相独立**的账户（每个账户一个 key、
//! 一套限流）。调度时 Saiku 在这里选账户，而不是让工人自己挑 —— 工人只拿到「用哪个 key
//! 跑」，看不到池子里还有谁。
//!
//! 为什么要有这一层（用户 2026-11-13）：
//!
//! - 一个 key 的并发上限（截图里是 10）不等于一台机器的并发上限。三个独立账户就是
//!   三个故障域、三个限流桶；把它们当一个池用，才谈得上「30 并发」。
//! - 供应商挂了要能换：池里换账户是同模型族内的平移，不换能力、不换提示词、不换缓存口径；
//!   换供应商是另一件事，必须显式建新 GA。
//! - 夜班没人看着：账户配额、冷却、租约都要落盘，第二天能回答「昨晚为什么慢」。
//!
//! **容量账本落在状态目录里，不在内存里**：`saiku` 一件任务一个进程，进程之间没有共享
//! 服务，只有文件。所以「此刻有几个工人在这个账户上」只能靠目录里的租约数出来。
//!
//! 布局：
//!
//! ```text
//! <state_root>/capacity/<ga>/<account>/
//!     .lock/                 互斥（`create_dir` 是原子的；没有 flock 依赖）
//!     <pid>-<nanos>/         一个在途租约
//!         meta.json          谁、什么时候、跑哪件
//!         beat               心跳（mtime 就是最后一次活着的证据）
//!     cooldown.json          账户冷却到什么时候、为什么
//! ```
//!
//! 旧租约（心跳过期，或同机 pid 已经不在）在**下一次取号时**回收 —— 没有常驻清理进程。

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{code, SpecError, SpecResult};

/// 心跳超过这么久没更新，租约算失效。
pub const STALE_MS: u64 = 120_000;
/// 取号时最多为一把锁等这么久。
const LOCK_WAIT_MS: u64 = 3_000;
/// 锁本身超过这么久没释放，视为持有者已死。
const LOCK_STALE_MS: u64 = 15_000;

/// 一个 GA 的配置。写在 `saiku.json` 的 `group_army.<id>`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupArmyConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `provider` 块里的 id（供应商地址与协议在那一块，这里不重复）。
    pub provider: String,
    /// 模型 id。GA 内所有账户用同一个模型 —— 换模型就是换 GA。
    pub model: String,
    #[serde(default)]
    pub capabilities: Capabilities,
    /// 账户的缺省限流；单个账户可覆盖。
    #[serde(default)]
    pub limits: Limits,
    /// 账户（相互独立）。写字符串 id 或带覆盖的对象都行。
    pub accounts: Vec<AccountSpec>,
    /// 没有 `--ga` 时是否默认用这一个。
    #[serde(default)]
    pub default: bool,
}

/// 账户：`"inf-1"` 或 `{ "id": "inf-1", "limits": { … } }`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AccountSpec {
    Id(String),
    Full(Account),
}

impl AccountSpec {
    pub fn id(&self) -> &str {
        match self {
            Self::Id(id) => id,
            Self::Full(account) => &account.id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Account {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<Limits>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// 账户限流。字段与供应商控制台上的口径同名，避免「我以为的 TPM 和你以为的不一样」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// 请求/分钟。只做展示与准入判断的辅助，**不在这里做令牌桶**。
    #[serde(default)]
    pub rpm: Option<u32>,
    /// 输入 + 缓存读 + 输出 / 分钟。
    #[serde(default)]
    pub tpm: Option<u64>,
    /// 未命中缓存的输入 / 分钟；`None` 表示供应商不限制。
    #[serde(default)]
    pub uncached_tpm: Option<u64>,
    /// 在途请求上限 —— 这是**唯一真正硬拦**的一项。
    #[serde(default = "default_concurrency")]
    pub concurrency: u32,
}

fn default_concurrency() -> u32 {
    1
}

impl Default for Limits {
    fn default() -> Self {
        Self { rpm: None, tpm: None, uncached_tpm: None, concurrency: default_concurrency() }
    }
}

/// 模型能力。工人只能干自己最擅长的工作，靠的就是这几个数。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    /// 上下文窗口（token）。
    #[serde(default)]
    pub context: Option<u64>,
    /// 单次最大输出（token）。
    #[serde(default)]
    pub max_output: Option<u64>,
    /// 能不能读图。
    #[serde(default)]
    pub vision: bool,
    /// 支持的 effort 档（如 `["none", "high"]`）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effort: Vec<String>,
}

/// 一件任务对模型的要求。来源是 `--need` / 任务书的 `needs`。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Needs {
    pub vision: bool,
    pub context: Option<u64>,
    pub max_output: Option<u64>,
    pub effort: Option<String>,
}

impl Needs {
    /// `vision` / `context=500000` / `output=100000` / `effort=high`。
    pub fn parse(items: &[String]) -> SpecResult<Self> {
        let mut needs = Self::default();
        for raw in items {
            let item = raw.trim();
            if item.is_empty() {
                continue;
            }
            match item.split_once('=') {
                None => match item {
                    "vision" => needs.vision = true,
                    other => {
                        return Err(SpecError::new(
                            code::CONFIG_INVALID,
                            format!("不认识的 need `{other}`；只支持 vision / context=N / output=N / effort=X"),
                        ))
                    }
                },
                Some((key, value)) => {
                    let n: u64 = value.trim().parse().map_err(|_| {
                        SpecError::new(code::CONFIG_INVALID, format!("need `{item}` 的数值不合法"))
                    })?;
                    match key.trim() {
                        "context" => needs.context = Some(n),
                        "output" => needs.max_output = Some(n),
                        other => {
                            return Err(SpecError::new(
                                code::CONFIG_INVALID,
                                format!("不认识的 need `{other}`；只支持 vision / context=N / output=N"),
                            ))
                        }
                    }
                }
            }
        }
        Ok(needs)
    }

    pub fn is_empty(&self) -> bool {
        !self.vision && self.context.is_none() && self.max_output.is_none() && self.effort.is_none()
    }

    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if self.vision {
            parts.push("vision".to_string());
        }
        if let Some(context) = self.context {
            parts.push(format!("context={context}"));
        }
        if let Some(output) = self.max_output {
            parts.push(format!("output={output}"));
        }
        if let Some(effort) = &self.effort {
            parts.push(format!("effort={effort}"));
        }
        parts.join(" ")
    }

    fn check(&self, capabilities: &Capabilities) -> Option<String> {
        if self.vision && !capabilities.vision {
            return Some("要求读图，但这个 GA 的模型不支持多模态".into());
        }
        if let Some(need) = self.context {
            match capabilities.context {
                Some(have) if have >= need => {}
                Some(have) => return Some(format!("上下文不够：要 {need}，只有 {have}")),
                None => return Some("要求上下文下限，但这个 GA 没声明 capabilities.context".into()),
            }
        }
        if let Some(need) = self.max_output {
            match capabilities.max_output {
                Some(have) if have >= need => {}
                Some(have) => return Some(format!("最大输出不够：要 {need}，只有 {have}")),
                None => return Some("要求输出下限，但这个 GA 没声明 capabilities.max_output".into()),
            }
        }
        if let Some(effort) = &self.effort
            && !capabilities.effort.is_empty()
            && !capabilities.effort.iter().any(|e| e == effort)
        {
            return Some(format!("不支持 effort `{effort}`（支持：{}）", capabilities.effort.join(", ")));
        }
        None
    }
}

impl GroupArmyConfig {
    pub fn limits_of(&self, account: &str) -> Limits {
        self.accounts
            .iter()
            .find(|a| a.id() == account)
            .and_then(|a| match a {
                AccountSpec::Full(full) => full.limits,
                AccountSpec::Id(_) => None,
            })
            .unwrap_or(self.limits)
    }
}

/// 一个在途租约。**Drop 时自动释放** —— 正常退出与提前返回都不该漏掉名额。
#[derive(Debug)]
pub struct Lease {
    path: PathBuf,
    pub ga: String,
    pub account: String,
    released: bool,
}

impl Lease {
    pub fn dir(&self) -> &Path {
        &self.path
    }

    /// 刷心跳。工人进程每 30 秒调一次；不刷就会被下一次取号当成死租约回收。
    pub fn beat(&self) -> std::io::Result<()> {
        let beat = self.path.join("beat");
        // 先写内容再改 mtime：两条都成功才算心跳，失败由调用方决定是否算异常。
        let mut f = std::fs::OpenOptions::new().create(true).write(true).truncate(true).open(&beat)?;
        write!(f, "{}", now_ms())?;
        Ok(())
    }

    /// 把租约交给别的进程（分离运行时的监工）：本对象不再负责删除目录。
    pub fn hand_off(mut self) -> PathBuf {
        self.released = true;
        self.path.clone()
    }

    pub fn release(mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
        self.released = true;
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if !self.released {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

/// 一次取号的结果（给调用方看「为什么是这个账户」）。
#[derive(Debug)]
pub struct Acquired {
    pub ga: String,
    pub account: String,
    pub lease: Lease,
    /// 同池其余账户此刻的占用，方便日志里回答「为什么没轮到我」。
    pub snapshot: Vec<AccountState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountState {
    pub id: String,
    pub busy: u32,
    pub limit: u32,
    /// 冷却到什么时候（毫秒时间戳）。
    pub cooldown_until: Option<u64>,
    pub cooldown_reason: Option<String>,
}

impl AccountState {
    pub fn free(&self) -> u32 {
        self.limit.saturating_sub(self.busy)
    }
}

/// 状态目录下的容量账本根。
pub fn capacity_root(state_root: &Path) -> PathBuf {
    state_root.join("capacity")
}

/// 取一个名额。顺序：先筛能力，再筛冷却，最后按「剩余名额多的账户优先」。
///
/// 并发安全靠目录锁：`create_dir` 在 POSIX 与 Windows 上都是原子的，拿不到就是别人拿着。
/// 锁里做三件事 —— 回收死租约、数数、写新租约。
pub fn acquire(
    ga_id: &str,
    ga: &GroupArmyConfig,
    state_root: &Path,
    needs: &Needs,
    run_id: &str,
) -> SpecResult<Acquired> {
    if ga.accounts.is_empty() {
        return Err(SpecError::new(
            code::CONFIG_INVALID,
            format!("GA `{ga_id}` 没有账户；先 `saiku auth set <账户 id>` 再把它写进 group_army.{ga_id}.accounts"),
        ));
    }
    if let Some(why) = needs.check(&ga.capabilities) {
        return Err(SpecError::new(
            code::CAPABILITY_MISS,
            format!("GA `{ga_id}` 不满足这件任务：{why}"),
        ));
    }

    let root = capacity_root(state_root).join(ga_id);
    let now = now_ms();
    let mut states = inspect(ga_id, ga, state_root, now);
    // 剩余名额多的先选；同分按配置顺序（`inspect` 已保持顺序）。
    let mut order: Vec<usize> = (0..states.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(states[i].free()));

    for i in order {
        let account = states[i].id.clone();
        if states[i].cooldown_until.is_some_and(|until| until > now) {
            continue;
        }
        // **即使看起来满了也要进去数一遍**：租约可能是死的（进程被强杀，心跳停了），
        // 不进来就永远回收不了，那个账户从此少一个名额。
        let dir = root.join(&account);
        let Some(_guard) = lock_dir(&dir, now) else { continue };
        // 拿到锁之后重新数：等锁期间别人可能刚占满。
        prune_stale(&dir, now);
        let limit = ga.limits_of(&account).concurrency.max(1);
        let busy = count_leases(&dir) as u32;
        if busy >= limit {
            states[i].busy = busy;
            continue;
        }
        let lease_dir = unique_lease_dir(&dir, now);
        // `create_dir` 而不是 `create_dir_all`：同名目录已经存在时必须失败，
        // 否则两个租约会共用同一个目录、名额被数漏（实测过一次，第三件任务挤进了
        // 「2/2」的账户）。
        match std::fs::create_dir(&lease_dir) {
            Ok(()) => {}
            Err(_) => continue,
        }
        let host = host_name();
        let meta = Meta { run_id: run_id.to_string(), pid: std::process::id(), host, started_at: now };
        let written = std::fs::write(
            lease_dir.join("meta.json"),
            serde_json::to_vec_pretty(&meta).unwrap_or_default(),
        )
        .and_then(|_| std::fs::write(lease_dir.join("beat"), now.to_string()));
        if written.is_err() {
            let _ = std::fs::remove_dir_all(&lease_dir);
            continue;
        }
        let lease = Lease { path: lease_dir, ga: ga_id.to_string(), account: account.clone(), released: false };
        states[i].busy += 1;
        return Ok(Acquired { ga: ga_id.to_string(), account, lease, snapshot: states });
    }

    Err(SpecError::new(code::GA_CAPACITY, describe_full(ga_id, &states, now)))
}

/// 按能力挑一个 GA：`--need` 不满足的排除，满足了就优先 `default`，其次配置顺序。
///
/// 只在「没写 `--ga`、也没配 `dispatch.ga`、但写了 needs」时用 —— 这时用户的意图
/// 明确是「谁合适用谁」，而不是「用我指定的那一个」。
pub fn pick_for(needs: &Needs, armies: &BTreeMap<String, GroupArmyConfig>) -> Option<String> {
    let mut fallback: Option<String> = None;
    for (id, ga) in armies {
        if needs.check(&ga.capabilities).is_some() {
            continue;
        }
        if ga.default {
            return Some(id.clone());
        }
        if fallback.is_none() {
            fallback = Some(id.clone());
        }
    }
    fallback
}

/// 给账户记一次冷却（供应商 429 / 5xx 之后用）。冷却期内不取号。
pub fn cooldown(ga_id: &str, account: &str, state_root: &Path, seconds: u64, reason: &str) -> SpecResult<()> {
    let dir = capacity_root(state_root).join(ga_id).join(account);
    std::fs::create_dir_all(&dir).map_err(|e| {
        SpecError::new(code::CONFIG_INVALID, format!("容量账本不可写：{} —— {e}", dir.display()))
    })?;
    let until = now_ms() + seconds * 1000;
    let body = serde_json::json!({ "until": until, "reason": reason, "at": now_ms() });
    let path = dir.join("cooldown.json");
    let tmp = dir.join("cooldown.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&body).unwrap_or_default())
        .and_then(|_| std::fs::rename(&tmp, &path))
        .map_err(|e| SpecError::new(code::CONFIG_INVALID, format!("写冷却失败：{e}")))
}

/// 读一个 GA 的账户状态（`saiku ga ls` 用，也用于取号前的排序）。
pub fn inspect(ga_id: &str, ga: &GroupArmyConfig, state_root: &Path, now: u64) -> Vec<AccountState> {
    let root = capacity_root(state_root).join(ga_id);
    ga.accounts
        .iter()
        .map(|spec| {
            let id = spec.id().to_string();
            let dir = root.join(&id);
            let cooldown = read_cooldown(&dir, now);
            let busy = count_leases(&dir) as u32;
            AccountState {
                limit: ga.limits_of(&id).concurrency.max(1),
                busy,
                cooldown_until: cooldown.as_ref().map(|(until, _)| *until),
                cooldown_reason: cooldown.map(|(_, reason)| reason),
                id,
            }
        })
        .collect()
}

fn describe_full(ga_id: &str, states: &[AccountState], now: u64) -> String {
    let detail = states
        .iter()
        .map(|s| match s.cooldown_until {
            Some(until) if until > now => {
                format!("{}: 冷却中（{}）", s.id, s.cooldown_reason.as_deref().unwrap_or("未说明"))
            }
            _ => format!("{}: {}/{}", s.id, s.busy, s.limit),
        })
        .collect::<Vec<_>>()
        .join("；");
    format!("GA `{ga_id}` 没有空名额 —— {detail}")
}

// ── 目录原语 ───────────────────────────────────────────────────────────

struct DirGuard {
    path: PathBuf,
}

impl Drop for DirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 目录锁。拿不到等一会儿；持有者明显已死（mtime 过旧）就把锁拆了重试。
fn lock_dir(dir: &Path, now: u64) -> Option<DirGuard> {
    std::fs::create_dir_all(dir).ok()?;
    let lock = dir.join(".lock");
    let deadline = now + LOCK_WAIT_MS;
    loop {
        match std::fs::create_dir(&lock) {
            Ok(()) => {
                let _ = std::fs::write(lock.join("owner"), format!("{}\n{}", std::process::id(), now));
                return Some(DirGuard { path: lock });
            }
            Err(_) => {
                // 过期锁：直接拆。
                if let Ok(meta) = std::fs::metadata(&lock)
                    && let Ok(modified) = meta.modified()
                    && now.saturating_sub(mtime_ms(modified)) > LOCK_STALE_MS
                {
                    let _ = std::fs::remove_dir_all(&lock);
                    continue;
                }
                if now_ms() > deadline {
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }
}

/// 租约目录名：pid + 纳秒 + 进程内序号。同毫秒内多次取号也不会撞名。
fn unique_lease_dir(dir: &Path, now: u64) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    loop {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let candidate = dir.join(format!("{}-{now}-{nanos}-{seq}", std::process::id()));
        if !candidate.exists() {
            return candidate;
        }
    }
}

fn prune_stale(dir: &Path, now: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let beat = path.join("beat");
        let last = std::fs::metadata(&beat)
            .and_then(|m| m.modified())
            .map(mtime_ms)
            .or_else(|_| std::fs::metadata(&path).and_then(|m| m.modified()).map(mtime_ms))
            .unwrap_or(0);
        // 同机且 pid 已不在：立刻回收，不必等心跳过期。
        let pid_dead = meta_pid(&path).is_some_and(|pid| !pid_alive(pid));
        if pid_dead || now.saturating_sub(last) > STALE_MS {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

fn count_leases(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    entries
        .flatten()
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            !name.starts_with('.') && e.path().is_dir()
        })
        .count()
}

fn read_cooldown(dir: &Path, now: u64) -> Option<(u64, String)> {
    let raw = std::fs::read_to_string(dir.join("cooldown.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let until = value.get("until")?.as_u64()?;
    if until <= now {
        return None;
    }
    Some((until, value.get("reason").and_then(|v| v.as_str()).unwrap_or("未说明").to_string()))
}

#[derive(Debug, Serialize, Deserialize)]
struct Meta {
    run_id: String,
    pid: u32,
    host: String,
    started_at: u64,
}

fn meta_pid(lease_dir: &Path) -> Option<u32> {
    let raw = std::fs::read_to_string(lease_dir.join("meta.json")).ok()?;
    let meta: Meta = serde_json::from_str(&raw).ok()?;
    if meta.host != host_name() {
        return None; // 别的机器上的 pid 不能按本机判活
    }
    Some(meta.pid)
}

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(not(unix))]
fn pid_alive(_pid: u32) -> bool {
    // 没有依赖可查进程表时保守：只按心跳过期回收。
    true
}

fn host_name() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "unknown".into())
}

fn mtime_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

pub fn now_ms() -> u64 {
    mtime_ms(SystemTime::now())
}

/// 全部 GA 的配置视图（`model` 写法与账户数）。
pub fn summary(config: &BTreeMap<String, GroupArmyConfig>) -> Vec<(String, String, usize)> {
    config
        .iter()
        .map(|(id, ga)| (id.clone(), format!("{}/{}", ga.provider, ga.model), ga.accounts.len()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ga(accounts: &[&str], concurrency: u32) -> GroupArmyConfig {
        GroupArmyConfig {
            name: None,
            provider: "inferknock".into(),
            model: "deepseek-v4.1-flash".into(),
            capabilities: Capabilities { context: Some(1_000_000), max_output: Some(384_000), vision: true, effort: vec![] },
            limits: Limits { rpm: Some(300), tpm: Some(10_000_000), uncached_tpm: None, concurrency },
            accounts: accounts.iter().map(|id| AccountSpec::Id((*id).into())).collect(),
            default: false,
        }
    }

#[test]
    fn pick_for_prefers_default_and_skips_incapable() {
        let mut armies = BTreeMap::new();
        let mut text_only = ga(&["a"], 1);
        text_only.capabilities.vision = false;
        armies.insert("text".to_string(), text_only);
        let mut vision = ga(&["b"], 1);
        vision.default = true;
        armies.insert("vision".to_string(), vision);
        let need = Needs { vision: true, ..Default::default() };
        assert_eq!(pick_for(&need, &armies).as_deref(), Some("vision"));
        assert_eq!(pick_for(&Needs::default(), &armies).as_deref(), Some("vision"), "缺省仍偏爱 default");
        let mut none = BTreeMap::new();
        let mut only_text = ga(&["a"], 1);
        only_text.capabilities.vision = false;
        none.insert("t".to_string(), only_text);
        assert_eq!(pick_for(&need, &none), None);
    }

    #[test]
    fn account_spec_accepts_both_shapes() {
        let raw = r#"{"provider":"p","model":"m","accounts":["a",{"id":"b","limits":{"concurrency":2}}]}"#;
        let parsed: GroupArmyConfig = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.accounts[0].id(), "a");
        assert_eq!(parsed.limits_of("b").concurrency, 2);
        assert_eq!(parsed.limits_of("a").concurrency, 1);
    }

    #[test]
    fn concurrency_is_the_hard_cap_and_release_returns_the_slot() {
        let dir = tempfile::tempdir().unwrap();
        let config = ga(&["a"], 2);
        let first = acquire("g", &config, dir.path(), &Needs::default(), "r1").unwrap();
        let _second = acquire("g", &config, dir.path(), &Needs::default(), "r2").unwrap();
        let err = acquire("g", &config, dir.path(), &Needs::default(), "r3").unwrap_err();
        assert_eq!(err.code, code::GA_CAPACITY, "{}", err.message);
        first.lease.release();
        assert!(acquire("g", &config, dir.path(), &Needs::default(), "r4").is_ok(), "释放后名额应回来");
    }

    #[test]
    fn stale_lease_is_reclaimed_on_the_next_acquire() {
        let dir = tempfile::tempdir().unwrap();
        let config = ga(&["a"], 1);
        let lease = acquire("g", &config, dir.path(), &Needs::default(), "r1").unwrap();
        // 伪造一条 10 分钟前的心跳。
        let beat = lease.lease.dir().join("beat");
        let old = SystemTime::now() - std::time::Duration::from_secs(600);
        let file = std::fs::File::options().write(true).open(&beat).unwrap();
        file.set_modified(old).unwrap();
        std::mem::forget(lease); // 不释放，模拟进程被杀
        assert!(acquire("g", &config, dir.path(), &Needs::default(), "r2").is_ok(), "死租约应被回收");
    }

    #[test]
    fn capability_gate_rejects_vision_on_a_text_only_ga() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = ga(&["a"], 1);
        config.capabilities.vision = false;
        let need = Needs { vision: true, ..Default::default() };
        let err = acquire("g", &config, dir.path(), &need, "r1").unwrap_err();
        assert_eq!(err.code, code::CAPABILITY_MISS);
    }

    #[test]
    fn needs_parse_known_and_unknown_forms() {
        let needs = Needs::parse(&["vision".into(), "context=500000".into()]).unwrap();
        assert!(needs.vision);
        assert_eq!(needs.context, Some(500_000));
        assert!(Needs::parse(&["gpu=2".into()]).is_err());
    }

    #[test]
    fn cooldown_hides_the_account_until_it_expires() {
        let dir = tempfile::tempdir().unwrap();
        let config = ga(&["a"], 1);
        cooldown("g", "a", dir.path(), 60, "429").unwrap();
        let err = acquire("g", &config, dir.path(), &Needs::default(), "r1").unwrap_err();
        assert_eq!(err.code, code::GA_CAPACITY);
        assert!(err.message.contains("冷却"), "{}", err.message);
    }
}
