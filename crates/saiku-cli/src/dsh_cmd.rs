//! `saiku dsh` —— 与内核版本打交道：体检、快照、对照。
//!
//! 这一层只读：它不改运行时、不改配置。判据全部来自内核自己的 `--dump-config`，
//! 这样「DSH 变了什么」有物证，而不是靠 changelog 与记忆。

use std::path::{Path, PathBuf};

use saiku_core::error::{code, SpecError};
use saiku_core::userconfig::{self, UserConfig};
use saiku_dsh::contract;
use saiku_dsh::runtime::RuntimeLayout;

use crate::commands::Flags;

pub fn cmd_dsh(argv: &[String]) -> Result<i32, SpecError> {
    let sub = argv.first().map(String::as_str).unwrap_or("doctor");
    let flags = Flags::parse(
        &argv[1..],
        &["--config", "--version", "--against-version", "--against", "--contracts"],
        &["--json", "--write-contract"],
    )?;
    match sub {
        "doctor" => doctor(&flags),
        "list" => list(&flags),
        other => Err(SpecError::new(
            code::CONFIG_INVALID,
            format!("dsh 不认识 `{other}`（doctor / list）"),
        )),
    }
}

fn doctor(flags: &Flags) -> Result<i32, SpecError> {
    let cfg = UserConfig::locate_and_load(flags.one("--config").map(Path::new)).ok().map(|(c, _)| c);
    let version = flags
        .one("--version")
        .map(str::to_string)
        .or_else(|| cfg.as_ref().map(UserConfig::dsh_version))
        .unwrap_or_else(|| userconfig::PINNED_DSH_VERSION.to_string());
    let layout = layout_for(flags, cfg.as_ref(), &version);
    if let Err(m) = layout.check() {
        eprintln!("saiku: {m}");
        return Ok(1);
    }

    let current = contract::dump(&layout.entry(), "sdk", &version).map_err(|m| {
        SpecError::new(code::RUNTIME_ENTRY_MISSING, m)
    })?;

    // 1) Saiku 的 patch 会点名的行必须还在。
    let missing: Vec<&str> = contract::REQUIRED_ROWS
        .iter()
        .copied()
        .filter(|id| !current.rows.contains_key(*id))
        .collect();

    // 2) 与基线比：行列增删、包增删、开关翻转。
    let baseline_path = baseline_path(flags, &version);
    let baseline = baseline_path.as_deref().and_then(contract::load);
    let diff = baseline.as_ref().map(|old| contract::diff(old, &current));

    // 3) 与另一个已安装版本比（同一台机上通常就有两个）。
    let against = match flags.one("--against-version") {
        Some(other) => {
            let dir = userconfig::default_runtime_dir(other);
            Some((other.to_string(), dir))
        }
        None => flags.one("--against").map(|dir| (dir.to_string(), PathBuf::from(dir))),
    }
    .map(|(label, dir)| {
        let layout = RuntimeLayout::new(&dir, label.clone());
        match contract::dump(&layout.entry(), "sdk", &label) {
            Ok(other) => Ok((label, other)),
            Err(m) => Err(m),
        }
    })
    .transpose()
    .map_err(|m| SpecError::new(code::RUNTIME_ENTRY_MISSING, m))?;

    if flags.has("--write-contract") {
        let dir = contracts_dir(flags);
        let path = contract::contract_path(&dir, &version);
        contract::save(&path, &current).map_err(|m| SpecError::new(code::CONFIG_INVALID, m))?;
        println!("契约快照已写入 {}", path.display());
    }

    if flags.has("--json") {
        let body = serde_json::json!({
            "version": version,
            "rows": current.rows.len(),
            "packages": current.packages().len(),
            "missing_required_rows": missing,
            "baseline": baseline_path.as_ref().map(|p| p.display().to_string()),
            "diff": diff.as_ref().map(|d| serde_json::json!({
                "added": d.added,
                "removed": d.removed,
                "packages_added": d.packages_added,
                "packages_removed": d.packages_removed,
                "newly_disabled": d.newly_disabled,
                "newly_enabled": d.newly_enabled,
            })),
            "against": against.as_ref().map(|(label, c)| {
                let d = contract::diff(&current, c);
                serde_json::json!({
                    "version": label,
                    "rows": c.rows.len(),
                    "diff": { "added": d.added, "removed": d.removed },
                })
            }),
        });
        println!("{}", serde_json::to_string_pretty(&body).unwrap_or_default());
    } else {
        println!("DSH {version}（sdk profile）：{} 行 · {} 个包", current.rows.len(), current.packages().len());
        match &baseline_path {
            Some(path) if baseline.is_some() => {
                match &diff {
                    Some(d) if !d.is_empty() => println!("  与快照 {} 相比：{}", path.display(), d.summary()),
                    _ => println!("  与快照 {} 一致", path.display()),
                }
            }
            Some(path) => println!("  没有快照 {}（用 --write-contract 生成）", path.display()),
            None => {}
        }
        if let Some((label, other)) = &against {
            // 方向：以**当前钉住的版本**为基准，{label} 多出来的算「新增」。
            let d = contract::diff(&current, other);
            println!("  {label}（{} 行）相对当前：{}", other.rows.len(), d.summary());
        }
        if missing.is_empty() {
            println!("  patch 会点名的 {} 行都在", contract::REQUIRED_ROWS.len());
        } else {
            println!("  ✗ patch 点名的行缺失：{}", missing.join(", "));
            println!("    这些行是 DSH 的内部配置，改名/删除都会让补丁落空；先修 `render_patch` 再升级。");
        }
    }

    Ok(if missing.is_empty() { 0 } else { 1 })
}

fn list(flags: &Flags) -> Result<i32, SpecError> {
    let cfg = UserConfig::locate_and_load(flags.one("--config").map(Path::new)).ok().map(|(c, _)| c);
    let root = cfg
        .as_ref()
        .map(|c| c.runtime_dir())
        .and_then(|dir| dir.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| {
            userconfig::default_runtime_dir(userconfig::PINNED_DSH_VERSION)
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default()
        });
    let pinned = cfg.as_ref().map(UserConfig::dsh_version).unwrap_or_else(|| userconfig::PINNED_DSH_VERSION.to_string());
    println!("运行时候选目录：{}", root.display());
    let mut found = false;
    if let Ok(entries) = std::fs::read_dir(&root) {
        let mut dirs: Vec<_> = entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        dirs.sort();
        for dir in dirs {
            let name = dir.file_name().unwrap_or_default().to_string_lossy().to_string();
            if let Some(version) = name.strip_prefix("dsh-") {
                let layout = RuntimeLayout::new(&dir, version);
                let mark = if version == pinned { " ← 当前" } else { "" };
                match layout.check() {
                    Ok(()) => println!("  {version} 可用{mark}"),
                    Err(m) => println!("  {version} 不可用（{m}）{mark}"),
                }
                found = true;
            }
        }
    }
    if !found {
        println!("  还没有安装任何版本；先 `saiku runtime install`");
    }
    Ok(0)
}

fn layout_for(flags: &Flags, cfg: Option<&UserConfig>, version: &str) -> RuntimeLayout {
    let dir = if flags.one("--version").is_some() {
        userconfig::default_runtime_dir(version)
    } else {
        cfg.map(UserConfig::runtime_dir).unwrap_or_else(|| userconfig::default_runtime_dir(version))
    };
    RuntimeLayout::new(dir, version)
}

/// 快照的查找顺序：`--contracts` → 当前目录的 `contracts/` → 用户数据目录。
fn baseline_path(flags: &Flags, version: &str) -> Option<PathBuf> {
    let candidates = [
        flags.one("--contracts").map(PathBuf::from),
        std::env::current_dir().ok().map(|d| d.join("contracts")),
        Some(userconfig::data_dir().join("saiku").join("contracts")),
    ];
    candidates
        .into_iter()
        .flatten()
        .map(|dir| contract::contract_path(&dir, version))
        .find(|p| p.is_file())
}

fn contracts_dir(flags: &Flags) -> PathBuf {
    if let Some(dir) = flags.one("--contracts") {
        return PathBuf::from(dir);
    }
    let cwd = std::env::current_dir().unwrap_or_default().join("contracts");
    if cwd.is_dir() {
        return cwd;
    }
    userconfig::data_dir().join("saiku").join("contracts")
}
