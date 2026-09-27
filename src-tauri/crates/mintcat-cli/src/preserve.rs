//! Protection for hand-placed UE4SS content.
//!
//! The GUI's Save always runs `uninstall(gamePath, isDeleteUe4ss = true)` before installing
//! (src/tasks/ModInstallTask.ts step 8), and `uninstall_ue4ss` does
//! `remove_dir_all(Binaries/Win64/ue4ss)` + removes `Binaries/Win64/mods`. Anything not
//! produced by MintCat (hand-installed JS mods, their config/log files, .bak files) is lost.
//!
//! The CLI therefore copies `ue4ss/` and `mods/` to a backup dir first, lets the engine run
//! exactly like the GUI, then restores what the engine did not recreate:
//!   * `ue4ss/mods/<dir>` not owned by any MintCat mod ("foreign")  -> restored entirely
//!   * `ue4ss/mods/<dir>` owned by a mod that was just reinstalled -> only missing files
//!     outside `js/` and not `*.dll` are restored (runtime config/logs), code comes from the mod
//!   * `ue4ss/mods/<dir>` owned by a mod that is now disabled/removed -> not restored (GUI semantics)
//!   * `ue4ss/UE4SS-settings.ini` -> old file restored, then MintCat's required keys re-applied
//!   * other files -> restored only if missing; engine-managed files are never overwritten.

use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Serialize)]
pub struct RestoreReport {
    pub backup_dir: Option<PathBuf>,
    pub foreign_mod_dirs: Vec<String>,
    pub restored_files: Vec<String>,
    pub dropped_mod_dirs: Vec<String>,
    pub settings_merged: bool,
}

fn copy_tree(src: &Path, dst: &Path) -> Result<u64> {
    let mut n = 0;
    if src.is_dir() {
        fs::create_dir_all(dst)?;
        for e in fs::read_dir(src)? {
            let e = e?;
            n += copy_tree(&e.path(), &dst.join(e.file_name()))?;
        }
    } else if src.is_file() {
        if let Some(p) = dst.parent() {
            fs::create_dir_all(p)?;
        }
        fs::copy(src, dst).with_context(|| format!("copy {:?} -> {:?}", src, dst))?;
        n += 1;
    }
    Ok(n)
}

fn walk_files(root: &Path, base: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(rd) = fs::read_dir(root) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk_files(&p, base, out);
            } else if let Ok(rel) = p.strip_prefix(base) {
                out.push(rel.to_path_buf());
            }
        }
    }
}

/// Snapshot `ue4ss/` and legacy `mods/` under `binaries`. Returns None if there is nothing.
pub fn snapshot(binaries: &Path, backup_root: &Path, label: &str) -> Result<Option<PathBuf>> {
    let targets = ["ue4ss", "mods"];
    if !targets.iter().any(|t| binaries.join(t).exists()) {
        return Ok(None);
    }
    let dir = backup_root.join(label);
    fs::create_dir_all(&dir)?;
    for t in targets {
        let src = binaries.join(t);
        if src.exists() {
            copy_tree(&src, &dir.join(t))?;
        }
    }
    fs::write(dir.join("SOURCE.txt"), format!("{}\n", binaries.display()))?;
    Ok(Some(dir))
}

/// Keep only the newest `keep` snapshot dirs for a game prefix.
pub fn prune(backup_root: &Path, prefix: &str, keep: usize) {
    let Ok(rd) = fs::read_dir(backup_root) else { return };
    let mut dirs: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(prefix))
        })
        .collect();
    dirs.sort();
    while dirs.len() > keep {
        let d = dirs.remove(0);
        let _ = fs::remove_dir_all(d);
    }
}

pub struct RestorePlan<'a> {
    pub binaries: &'a Path,
    pub backup: &'a Path,
    /// sanitized `ue4ss/mods/<dir>` names owned by any mod in MintCat's DB (+ UE4SSL runtime dirs)
    pub managed_dirs: &'a HashSet<String>,
    pub ue4ss_enabled: bool,
    /// required UE4SS-settings.ini entries the engine writes for this game (RC only)
    pub required_ini: &'a [(&'a str, &'a [(&'a str, &'a str)])],
}

const ENGINE_FILES: &[&str] = &["ue4ss/UE4SSL.dll", "ue4ss/UE4SS.dll", "ue4ss/config/mods.json"];

pub fn restore(plan: &RestorePlan) -> Result<RestoreReport> {
    let mut rep = RestoreReport {
        backup_dir: Some(plan.backup.to_path_buf()),
        ..Default::default()
    };
    let mut files = Vec::new();
    walk_files(plan.backup, plan.backup, &mut files);
    files.sort();
    let mut seen_dirs = HashSet::new();

    for rel in files {
        let rel_s = rel.to_string_lossy().replace('\\', "/");
        if rel_s == "SOURCE.txt" {
            continue;
        }
        let target = plan.binaries.join(&rel);
        let src = plan.backup.join(&rel);

        if rel_s == "ue4ss/UE4SS-settings.ini" {
            let old = fs::read_to_string(&src).unwrap_or_default();
            let merged = if plan.ue4ss_enabled && !plan.required_ini.is_empty() {
                ensure_ini_entries(&old, plan.required_ini).unwrap_or(old)
            } else {
                old
            };
            let current = fs::read_to_string(&target).ok();
            if current.as_deref() != Some(merged.as_str()) {
                if let Some(p) = target.parent() {
                    fs::create_dir_all(p)?;
                }
                fs::write(&target, merged)?;
                rep.settings_merged = true;
                rep.restored_files.push(rel_s.clone());
            }
            continue;
        }
        if ENGINE_FILES.contains(&rel_s.as_str()) {
            continue;
        }

        let parts: Vec<&str> = rel_s.split('/').collect();
        if parts.len() >= 3 && parts[0] == "ue4ss" && parts[1] == "mods" {
            let dir = parts[2];
            let inner = &parts[3..];
            let managed = plan.managed_dirs.contains(dir);
            let reinstalled = plan.binaries.join("ue4ss/mods").join(dir).exists();
            if managed && !reinstalled {
                if seen_dirs.insert(dir.to_string()) {
                    rep.dropped_mod_dirs.push(dir.to_string());
                }
                continue;
            }
            if managed {
                let is_code = inner.first() == Some(&"js")
                    || rel_s.to_ascii_lowercase().ends_with(".dll");
                if is_code {
                    continue;
                }
            } else if seen_dirs.insert(dir.to_string()) {
                rep.foreign_mod_dirs.push(dir.to_string());
            }
        }

        if !target.exists() {
            if let Some(p) = target.parent() {
                fs::create_dir_all(p)?;
            }
            fs::copy(&src, &target).with_context(|| format!("restore {:?}", target))?;
            rep.restored_files.push(rel_s);
        }
    }
    Ok(rep)
}

// ---- port of ensure_ini_entries from mintcat-integrator-core/src/common/ue4ss.rs ----

fn parse_section(line: &str) -> Option<&str> {
    let t = line.trim();
    (t.len() >= 2 && t.starts_with('[') && t.ends_with(']')).then(|| t[1..t.len() - 1].trim())
}
fn parse_kv(line: &str) -> Option<(&str, &str)> {
    let t = line.trim();
    if t.starts_with(';') || t.starts_with('#') {
        return None;
    }
    t.split_once('=').map(|(k, v)| (k.trim(), v.trim()))
}

pub fn ensure_ini_entries(content: &str, sections: &[(&str, &[(&str, &str)])]) -> Option<String> {
    let mut lines: Vec<String> = content.lines().map(str::to_string).collect();
    let mut changed = false;
    for (section, entries) in sections {
        let start = lines
            .iter()
            .position(|l| parse_section(l).is_some_and(|s| s.eq_ignore_ascii_case(section)));
        let Some(start) = start else {
            if lines.last().is_some_and(|l| !l.trim().is_empty()) {
                lines.push(String::new());
            }
            lines.push(format!("[{section}]"));
            lines.extend(entries.iter().map(|(k, v)| format!("{k} = {v}")));
            changed = true;
            continue;
        };
        let end = lines
            .iter()
            .enumerate()
            .skip(start + 1)
            .find_map(|(i, l)| parse_section(l).map(|_| i))
            .unwrap_or(lines.len());
        let mut missing = Vec::new();
        for (key, value) in entries.iter() {
            let desired = format!("{key} = {value}");
            let idx = lines[start + 1..end]
                .iter()
                .position(|l| parse_kv(l).is_some_and(|(k, _)| k.eq_ignore_ascii_case(key)));
            match idx {
                Some(i) => {
                    let line = &mut lines[start + 1 + i];
                    if parse_kv(line).map(|(_, v)| v) != Some(*value) {
                        *line = desired;
                        changed = true;
                    }
                }
                None => missing.push(desired),
            }
        }
        if !missing.is_empty() {
            let mut at = end;
            while at > start + 1 && lines[at - 1].trim().is_empty() {
                at -= 1;
            }
            lines.splice(at..at, missing);
            changed = true;
        }
    }
    if !changed {
        return None;
    }
    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    Some(out)
}

pub const RC_REQUIRED_INI: &[(&str, &[(&str, &str)])] = &[
    ("EngineVersionOverride", &[("MajorVersion", "5"), ("MinorVersion", "6")]),
    (
        "Hooks",
        &[("HookProcessInternal", "1"), ("HookProcessLocalScriptFunction", "1")],
    ),
];
