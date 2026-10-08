//! Port of src/tasks/ModInstallTask.ts (the GUI's Save / "install mods" task).

use anyhow::{Context, Result};
use mintcat_integrator_api::{InstallEvent, InstallRequest, ModInfo};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::assets::{ensure_internal_assets, AssetAction, AssetOptions, AssetPaths};
use crate::db::{Db, Game, Profile, ProfileMod};
use crate::modio::{needs_download, refresh_metadata, Modio};
use crate::preserve::{self, RestorePlan, RestoreReport};
use crate::report::{refused, CliResult, Reporter};
use crate::runtime::Runtime;
use crate::util::{path_exists, process_running, sanitize_dir_name, size_and_mtime, stamp};

const AUDIO_TAG: &str = "Audio";
const AUDIO_DIRECT_COPY_MIN_SIZE: i64 = 10 * 1024 * 1024;

pub struct Ctx<'a> {
    pub db: &'a mut Db,
    pub rt: &'a Runtime,
    pub rep: Reporter,
    pub cache_dir: PathBuf,
    pub data_dir: PathBuf,
}

pub struct InstallOptions {
    pub dry_run: bool,
    pub force: bool,
    pub offline: bool,
    pub allow_foreign_paks: bool,
    pub allow_old_mint: bool,
    pub preserve: bool,
    /// install into this pak path instead of games.install_path; DB is not written
    pub target: Option<String>,
}

#[derive(Serialize)]
pub struct PlannedMod {
    pub mod_id: i64,
    pub display_name: String,
    pub source_type: String,
    pub name: String,
    pub pak_path: String,
    pub is_unpacked: bool,
    pub is_audio_only: bool,
    pub needs_download: bool,
}

pub fn game_kind_for(game: &Game) -> &'static str {
    if game.name.eq_ignore_ascii_case("rc") {
        "rc"
    } else {
        "drg"
    }
}

pub fn binaries_dir(pak_path: &str) -> Option<PathBuf> {
    // <root>/Content/Paks/<pak> -> <root>/Binaries/Win64 (DRGInstallation / RcInstallation)
    Path::new(pak_path)
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .map(|root| root.join("Binaries").join("Win64"))
}

pub fn ue4ss_enabled(db: &Db) -> Result<bool> {
    Ok(db.setting("ue4ss")? != "Disabled")
}

pub fn release_channel(db: &Db) -> Result<String> {
    let v = db.setting("releaseChannel")?;
    Ok(if ["stable", "beta", "alpha"].contains(&v.as_str()) { v } else { "stable".into() })
}

/// computeInstallManifestHash from ModInstallTask.ts
pub fn manifest_hash(rt: &Runtime, mods: &[ProfileMod], ue4ss: bool, assets: &AssetPaths) -> Result<String> {
    let mut entries = Vec::new();
    for m in mods {
        let (size, mtime) = size_and_mtime(&m.cache_path);
        entries.push(json!({
            "modId": m.mod_id,
            "nameId": m.name_id,
            "cachePath": m.cache_path,
            "fileSize": size,
            "mtime": mtime,
            "isUnpacked": if m.cache_path.is_empty() { false } else { rt.is_valid_unpacked_mod(&m.cache_path) },
        }));
    }
    let p = |o: &Option<PathBuf>| o.as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let ue4ss_zip = if ue4ss { p(&assets.ue4ss_zip) } else { String::new() };
    let stat = |s: &str| {
        let (size, mtime) = size_and_mtime(s);
        (size, mtime)
    };
    let (us, um) = stat(&ue4ss_zip);
    let drg = p(&assets.drg_zip);
    let (ds, dm) = stat(&drg);
    let rc = p(&assets.rc_zip);
    let (rs, rm) = stat(&rc);
    let manifest = json!({
        "mods": entries,
        "ue4ssMode": if ue4ss { "Enabled" } else { "Disabled" },
        "ue4ssZip": { "path": ue4ss_zip, "size": us, "mtime": um },
        "drgZip": { "path": drg, "size": ds, "mtime": dm },
        "rcZip": { "path": rc, "size": rs, "mtime": rm },
    });
    // serde_json keeps insertion order only with preserve_order; build the string by hand
    // in the GUI's key order instead.
    crate::util::js_md5_of_json(&OrderedManifest(&manifest))
}

/// Serializes the manifest object with the exact key order used by the GUI.
struct OrderedManifest<'a>(&'a Value);

impl Serialize for OrderedManifest<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        fn ordered<S: serde::Serializer>(v: &Value, keys: &[&str], s: S) -> std::result::Result<S::Ok, S::Error> {
            let mut m = s.serialize_map(Some(keys.len()))?;
            for k in keys {
                m.serialize_entry(k, &v[*k])?;
            }
            m.end()
        }
        struct Obj<'b>(&'b Value, &'static [&'static str]);
        impl Serialize for Obj<'_> {
            fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
                ordered(self.0, self.1, s)
            }
        }
        struct Mods<'b>(&'b Value);
        impl Serialize for Mods<'_> {
            fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
                use serde::ser::SerializeSeq;
                let arr = self.0.as_array().cloned().unwrap_or_default();
                let mut seq = s.serialize_seq(Some(arr.len()))?;
                for e in &arr {
                    seq.serialize_element(&Obj(e, &["modId", "nameId", "cachePath", "fileSize", "mtime", "isUnpacked"]))?;
                }
                seq.end()
            }
        }
        const ZIP: &[&str] = &["path", "size", "mtime"];
        let v = self.0;
        let mut m = s.serialize_map(Some(5))?;
        m.serialize_entry("mods", &Mods(&v["mods"]))?;
        m.serialize_entry("ue4ssMode", &v["ue4ssMode"])?;
        m.serialize_entry("ue4ssZip", &Obj(&v["ue4ssZip"], ZIP))?;
        m.serialize_entry("drgZip", &Obj(&v["drgZip"], ZIP))?;
        m.serialize_entry("rcZip", &Obj(&v["rcZip"], ZIP))?;
        m.end()
    }
}

/// Every `ue4ss/mods/<dir>` name that some MintCat mod (any profile/game) would install into.
pub fn managed_dir_names(db: &Db) -> Result<HashSet<String>> {
    let mut stmt = db.conn.prepare("SELECT name_id, display_name FROM mods")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let mut set: HashSet<String> = ["UE4SSL.JavaScript", "UE4SSL.JavaScript.Framework"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    for row in rows {
        let (name_id, display) = row?;
        let n = if name_id.is_empty() { display } else { name_id };
        set.insert(sanitize_dir_name(&n));
    }
    Ok(set)
}

/// Running processes of the given game ("drg" | "rc"). Only that game's own exe is matched,
/// so e.g. Rogue Core running does not block a DRG install.
pub fn game_processes(kind: &str) -> Vec<(u32, String)> {
    if kind == "rc" {
        process_running(&["RogueCore-Win64-Shipping.exe"])
    } else {
        process_running(&["FSD-Win64-Shipping.exe", "\\FSD.exe", "/FSD.exe"])
    }
}

/// Where a (Proton) game process runs from: STEAM_COMPAT_INSTALL_PATH, else its cwd.
fn process_install_dir(pid: u32) -> Option<PathBuf> {
    let env = std::fs::read(format!("/proc/{pid}/environ")).ok();
    if let Some(env) = env {
        for var in env.split(|b| *b == 0) {
            if let Some(v) = var.strip_prefix(b"STEAM_COMPAT_INSTALL_PATH=") {
                return Some(PathBuf::from(String::from_utf8_lossy(v).into_owned()));
            }
        }
    }
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

/// Game processes that run from the install containing `pak_path`. A process whose install
/// dir cannot be determined counts as a match (fail safe).
pub fn game_processes_for_pak(kind: &str, pak_path: &str) -> Vec<(u32, String)> {
    let pak = std::fs::canonicalize(pak_path).unwrap_or_else(|_| PathBuf::from(pak_path));
    game_processes(kind)
        .into_iter()
        .filter(|(pid, _)| match process_install_dir(*pid) {
            Some(dir) => {
                let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
                pak.starts_with(&dir)
            }
            None => true,
        })
        .collect()
}

pub fn running_game_processes() -> Vec<(u32, String)> {
    let mut all = game_processes("drg");
    all.extend(game_processes("rc"));
    all
}

pub fn run_install(
    ctx: &mut Ctx,
    game: &Game,
    profile: &Profile,
    opts: &InstallOptions,
) -> CliResult<Value> {
    let rep = ctx.rep;
    let testing = opts.target.is_some();
    let read_only = opts.dry_run || testing;
    let pak_path = opts.target.clone().unwrap_or_else(|| game.install_path.clone());
    let kind = game_kind_for(game);

    // Step 1: validate game environment
    if pak_path.is_empty() || !Path::new(&pak_path).is_file() {
        return Err(refused(format!("Game Path Not Found: '{pak_path}' (use set-game-path)")));
    }
    let valid_name = if kind == "rc" {
        pak_path.ends_with("RogueCore-Windows.pak")
    } else {
        pak_path.ends_with("FSD-WindowsNoEditor.pak") || pak_path.ends_with("FSD-WinGDK.pak")
    };
    if !valid_name {
        return Err(refused(format!("{pak_path} is not a {kind} game pak")));
    }
    let running = game_processes_for_pak(kind, &pak_path);
    let running_desc = running.iter().map(|(p, c)| format!("{p} {c}")).collect::<Vec<_>>().join("; ");
    if !testing && !opts.dry_run && !running.is_empty() {
        return Err(refused(format!("Game Not Closed: {running_desc}")));
    }
    if opts.dry_run && !running.is_empty() {
        rep.warn(format!("game is running ({running_desc}); a real install would be refused"));
    }
    let foreign = ctx.rt.foreign_paks(&pak_path)?;
    if !foreign.is_empty() && !opts.allow_foreign_paks {
        return Err(refused(format!(
            "foreign .pak files in Paks dir: {} (GUI would ask; pass --allow-foreign-paks)",
            foreign.join(", ")
        )));
    }

    // Step 2: enabled mods of the profile
    let mut mods: Vec<ProfileMod> = ctx.db.profile_mods(profile.id)?.into_iter().filter(|m| m.enabled).collect();
    rep.info(format!("profile '{}' (game {}): {} enabled mods", profile.name, game.name, mods.len()));

    // Step 3: online metadata refresh + downloads
    let online: Vec<ProfileMod> = mods.iter().filter(|m| m.is_online()).cloned().collect();
    let mut would_download = HashSet::new();
    if !online.is_empty() && !opts.offline && !read_only {
        let modio = Modio::new(ctx.db)?;
        ctx.db_backed_up_for(&rep)?;
        refresh_metadata(&modio, ctx.db, &online, false, &rep)?;
        mods = reload(ctx.db, profile.id, &mods)?;
        let need: Vec<ProfileMod> = mods.iter().filter(|m| m.source_type == "Modio" && needs_download(m)).cloned().collect();
        if !need.is_empty() {
            // batchDownloadModFiles: refresh again with tags, then download
            refresh_metadata(&modio, ctx.db, &need, true, &rep)?;
            let need = reload(ctx.db, profile.id, &need)?;
            let mut failed = Vec::new();
            for m in &need {
                rep.info(format!("downloading {} {}", m.display_name, m.current_version));
                if let Err(e) = modio.download(ctx.db, m, &ctx.cache_dir) {
                    failed.push(format!("{} ({e:#})", m.display_name));
                }
            }
            if !failed.is_empty() {
                return Err(anyhow::anyhow!("Download Failed: {}", failed.join(", ")).into());
            }
            mods = reload(ctx.db, profile.id, &mods)?;
        }
    } else {
        for m in &online {
            if needs_download(m) {
                would_download.insert(m.mod_id);
            }
        }
        if !would_download.is_empty() && !opts.dry_run {
            return Err(refused(format!(
                "{} online mods need a download but --offline/--target forbids network/DB writes",
                would_download.len()
            )));
        }
    }

    // local mods must exist; record their mtime (checkLocalModModify / checkLocalModCache)
    for m in &mods {
        if m.is_online() {
            if !would_download.contains(&m.mod_id) && !path_exists(&m.cache_path) {
                return Err(anyhow::anyhow!("File Not Found: {}: {}", m.display_name, m.cache_path).into());
            }
            continue;
        }
        if !path_exists(&m.cache_path) {
            if !read_only {
                ctx.db_backed_up_for(&rep)?;
                ctx.db.set_local_not_found(m.mod_id, true)?;
            }
            return Err(anyhow::anyhow!(
                "File Not Found: {}\nmod.localPathMissingReadd: {}",
                m.display_name,
                m.cache_path
            )
            .into());
        }
        if !read_only && m.source_type == "Local" {
            let (_, mtime) = size_and_mtime(&m.cache_path);
            if mtime != m.last_update_date {
                ctx.db_backed_up_for(&rep)?;
                ctx.db.set_last_update_date(m.mod_id, mtime)?;
            }
            ctx.db.set_local_not_found(m.mod_id, false)?;
        }
    }

    // Step 5: installation status
    let ue4ss = ue4ss_enabled(ctx.db)?;
    let install_type = ctx.rt.check_installed(&pak_path)?;
    if install_type == "old_version_mint_installed" && !opts.allow_old_mint {
        return Err(refused("old mods_P.pak (old MintCat install) found; GUI would ask. Pass --allow-old-mint"));
    }

    // Step 6: internal assets
    let channel = release_channel(ctx.db)?;
    let (assets, asset_actions): (AssetPaths, Vec<AssetAction>) = ensure_internal_assets(
        &AssetOptions {
            cache_dir: &ctx.cache_dir,
            channel: &channel,
            game: kind,
            include_ue4ss: ue4ss,
            // asset zips only touch the cache (same as the GUI), so --target may fetch them
            dry_run: opts.dry_run,
            offline: opts.offline,
        },
        &rep,
    )?;

    // Step 7: manifest hash
    let has_unpacked = mods.iter().any(|m| !m.cache_path.is_empty() && ctx.rt.is_valid_unpacked_mod(&m.cache_path));
    let hash = manifest_hash(ctx.rt, &mods, ue4ss, &assets)?;
    let saved = ctx.db.setting(&format!("profile_{}_installHash", profile.id))?;
    let installed = ctx.db.setting(&format!("game_{}_installedHash", game.id))?;
    let up_to_date = !has_unpacked && install_type == "mintcat_installed" && hash == saved && hash == installed;

    let plan: Vec<PlannedMod> = mods
        .iter()
        .map(|m| {
            let unpacked = ctx.rt.is_valid_unpacked_mod(&m.cache_path);
            PlannedMod {
                mod_id: m.mod_id,
                display_name: m.display_name.clone(),
                source_type: m.source_type.clone(),
                name: if m.name_id.is_empty() { m.display_name.clone() } else { m.name_id.clone() },
                pak_path: m.cache_path.clone(),
                is_unpacked: unpacked,
                is_audio_only: !unpacked && m.tags.iter().any(|t| t == AUDIO_TAG) && m.file_size >= AUDIO_DIRECT_COPY_MIN_SIZE,
                needs_download: would_download.contains(&m.mod_id),
            }
        })
        .collect();

    let mut result = json!({
        "game": game.name,
        "profile": profile.name,
        "pak_path": pak_path,
        "target_is_test_copy": testing,
        "ue4ss_enabled": ue4ss,
        "install_type": install_type,
        "manifest_hash": hash,
        "saved_profile_hash": saved,
        "saved_game_hash": installed,
        "has_unpacked_mod": has_unpacked,
        "gui_would_skip": up_to_date,
        "foreign_paks": foreign,
        "assets": asset_actions,
        "mods": plan,
    });

    if opts.dry_run {
        result["dry_run"] = json!(true);
        result["action"] = json!(if up_to_date && !opts.force { "skip_up_to_date" } else { "install" });
        return Ok(result);
    }
    if up_to_date && !opts.force && !testing {
        rep.info("Mod Already Install (manifest hash unchanged); use --force to reinstall");
        result["action"] = json!("skipped_up_to_date");
        return Ok(result);
    }

    // Snapshot hand-placed UE4SS content before the GUI-identical uninstall
    let binaries = binaries_dir(&pak_path).context("cannot derive Binaries/Win64")?;
    let backup_root = ctx.data_dir.join("cli-backups");
    let label = format!("{}-{}", game.name, stamp());
    let snapshot = if opts.preserve { preserve::snapshot(&binaries, &backup_root, &label)? } else { None };

    // Step 8 + 9: uninstall (always deletes ue4ss, like the GUI) then install
    rep.info("uninstalling old mods");
    ctx.rt.uninstall(&pak_path, true)?;

    let install_list: Vec<ModInfo> = plan
        .iter()
        .map(|p| ModInfo {
            modio_id: Some(mods.iter().find(|m| m.mod_id == p.mod_id).map(|m| m.platform_id).unwrap_or(0) as u32),
            name: p.name.clone(),
            pak_path: p.pak_path.clone(),
            is_unpacked: p.is_unpacked,
            is_audio_only: p.is_audio_only,
        })
        .collect();
    let request = InstallRequest::new(
        pak_path.clone(),
        install_list,
        !ue4ss,
        if ue4ss { assets.ue4ss_zip.as_ref().map(|p| p.to_string_lossy().into_owned()) } else { None },
        if kind == "rc" { None } else { assets.drg_zip.as_ref().map(|p| p.to_string_lossy().into_owned()) },
        if kind == "rc" { assets.rc_zip.as_ref().map(|p| p.to_string_lossy().into_owned()) } else { None },
    );
    let mut errors = Vec::new();
    let mut success_ts = None;
    let json_mode = rep.json;
    let quiet = rep.quiet;
    let install_res = ctx.rt.install(&request, &mut |ev| match ev {
        InstallEvent::StatusLog(v) if !quiet => eprintln!("[integrator] {}", v),
        InstallEvent::Percent(p) if !quiet && !json_mode => eprintln!("[integrator] {p:.0}%"),
        InstallEvent::Success(ts) => success_ts = Some(ts),
        InstallEvent::Error(v) => errors.push(v),
        _ => {}
    });

    let restore_report = match (&snapshot, opts.preserve) {
        (Some(dir), true) => {
            let managed = managed_dir_names(ctx.db)?;
            let r = preserve::restore(&RestorePlan {
                binaries: &binaries,
                backup: dir,
                managed_dirs: &managed,
                ue4ss_enabled: ue4ss,
                required_ini: if kind == "rc" { preserve::RC_REQUIRED_INI } else { &[] },
            })?;
            preserve::prune(&backup_root, &format!("{}-", game.name), 5);
            r
        }
        _ => RestoreReport::default(),
    };
    result["preserved"] = serde_json::to_value(&restore_report).map_err(anyhow::Error::from)?;

    if let Err(e) = install_res {
        return Err(anyhow::anyhow!("Installation Failed: {e:#} (events: {errors:?})").into());
    }

    if !testing {
        ctx.db_backed_up_for(&rep)?;
        ctx.db.set_setting(&format!("profile_{}_installHash", profile.id), &hash)?;
        ctx.db.set_setting(&format!("game_{}_installedHash", game.id), &hash)?;
    }
    result["action"] = json!("installed");
    result["mod_pak_timestamp"] = json!(success_ts);
    Ok(result)
}

fn reload(db: &Db, profile_id: i64, subset: &[ProfileMod]) -> Result<Vec<ProfileMod>> {
    let all = db.profile_mods(profile_id)?;
    Ok(subset
        .iter()
        .filter_map(|s| all.iter().find(|a| a.profile_mod_id == s.profile_mod_id).cloned())
        .collect())
}

impl Ctx<'_> {
    /// Back up the DB once before the first write of this run.
    pub fn db_backed_up_for(&mut self, rep: &Reporter) -> Result<bool> {
        if let Some(p) = self.db.backup_once("pre")? {
            rep.info(format!("DB backup: {}", p.display()));
        }
        Ok(true)
    }
}
