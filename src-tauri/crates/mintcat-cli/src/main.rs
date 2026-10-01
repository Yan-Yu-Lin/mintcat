//! mintcat-cli: headless front end for MintCat.
//!
//! Reads/writes the same SQLite DB as the GUI and drives the same integrator runtime
//! (libmintcat_integrator.so) with the same inputs as src/tasks/ModInstallTask.ts.

mod assets;
mod db;
mod install;
mod modio;
mod preserve;
mod report;
mod runtime;
mod util;

use anyhow::Context;
use clap::{Parser, Subcommand};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use db::{Db, Game, Profile, ProfileMod};
use install::{Ctx, InstallOptions};
use report::{not_found, refused, CliError, CliResult, Reporter};

#[derive(Parser)]
#[command(name = "mintcat-cli", version, about = "Headless MintCat: manage and install DRG / Rogue Core mods")]
struct Cli {
    /// Machine-readable JSON on stdout (progress still goes to stderr)
    #[arg(long, global = true)]
    json: bool,
    /// Less progress output on stderr
    #[arg(long, short, global = true)]
    quiet: bool,
    /// Operate on this game (drg|rc) instead of the DB's active game. Does not change the active game.
    #[arg(long, short, global = true)]
    game: Option<String>,
    /// MintCat DB path
    #[arg(long, global = true, env = "MINTCAT_DB")]
    db: Option<PathBuf>,
    /// Integrator runtime (.so) override
    #[arg(long, global = true, env = "MINTCAT_INTEGRATOR")]
    runtime: Option<PathBuf>,
    /// Allow DB writes while the MintCat GUI is running (the GUI caches state and may overwrite)
    #[arg(long, global = true)]
    allow_gui_running: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List games, install paths and which one is active
    Games,
    /// Set a game's pak path (FSD-WindowsNoEditor.pak / RogueCore-Windows.pak); "auto" = Steam lookup
    SetGamePath { game: String, path: String },
    /// Make a game the active one (what the GUI's game selector does)
    UseGame { game: String },
    /// List profiles of the game
    Profiles,
    /// Activate a profile of the game
    UseProfile { name: String },
    /// List mods in the active profile (enabled state, folder, source, cache path)
    List {
        /// Only enabled mods
        #[arg(long)]
        enabled: bool,
    },
    /// Enable mods (selector: profile_mod id, mod id "m123", exact name, or substring)
    Enable { selectors: Vec<String> },
    /// Disable mods
    Disable { selectors: Vec<String> },
    /// Add a local .pak / .zip / mod folder (js/main.js, pak/, dll/, Content/), or a mod.io URL
    /// (https://mod.io/g/drg/m/<name>, metadata + dependencies; `install` downloads), to the active profile
    Add {
        path: PathBuf,
        /// Folder name (or id) inside the profile; default = first top-level folder (GUI default)
        #[arg(long)]
        folder: Option<String>,
        /// Create the folder if it does not exist
        #[arg(long)]
        create_folder: bool,
        /// Add disabled
        #[arg(long)]
        disabled: bool,
    },
    /// Integrate the active profile into the game (the GUI's Save), incl. UE4SS + internal assets
    Install {
        #[arg(long)]
        dry_run: bool,
        /// Reinstall even if the manifest hash says nothing changed
        #[arg(long)]
        force: bool,
        /// No network (no mod.io refresh, no asset update check); fails if something must be downloaded
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        allow_foreign_paks: bool,
        #[arg(long)]
        allow_old_mint: bool,
        /// Do NOT restore hand-placed ue4ss/mods content after the uninstall step (pure GUI behaviour)
        #[arg(long)]
        no_preserve: bool,
        /// Install into a test copy: path to <copy>/.../Content/Paks/<game pak>. DB is not modified.
        #[arg(long)]
        target: Option<String>,
    },
    /// Refresh mod.io metadata and download new versions of online mods in the active profile
    Update {
        #[arg(long)]
        dry_run: bool,
        /// Also update disabled mods
        #[arg(long)]
        all: bool,
    },
    /// Show paths, runtime, GUI/game process state, mod.io auth
    Doctor,
}

fn main() {
    let cli = Cli::parse();
    let rep = Reporter { json: cli.json, quiet: cli.quiet };
    let code = match run(&cli, rep) {
        Ok((value, text)) => {
            rep.done(value, text);
            0
        }
        Err(e) => {
            rep.fail(&e);
            e.code()
        }
    };
    std::process::exit(code);
}

fn db_path(cli: &Cli) -> PathBuf {
    cli.db.clone().unwrap_or_else(|| util::default_config_dir().join("mintcat.sqlite"))
}

/// CacheApi.getCacheDir(): settings.cachePath, created if missing (default ~/.cache/com.mint.cat).
fn cache_dir(db: &Db) -> CliResult<PathBuf> {
    let v = db.setting("cachePath")?;
    let dir = if v.trim().is_empty() { util::default_cache_dir() } else { PathBuf::from(v.trim()) };
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create cache dir {:?}", dir))?;
    Ok(dir)
}

fn is_write(cmd: &Cmd) -> bool {
    match cmd {
        Cmd::Games | Cmd::Profiles | Cmd::List { .. } | Cmd::Doctor => false,
        Cmd::Install { dry_run, target, .. } => !*dry_run && target.is_none(),
        Cmd::Update { dry_run, .. } => !*dry_run,
        _ => true,
    }
}

fn guard_gui(cli: &Cli) -> CliResult<()> {
    let pids = util::gui_pids();
    if !pids.is_empty() && !cli.allow_gui_running {
        return Err(refused(format!(
            "MintCat GUI is running (pid {:?}); it keeps profile state in memory and can overwrite CLI changes. \
             Close it first, or pass --allow-gui-running",
            pids
        )));
    }
    Ok(())
}

fn select_game(db: &Db, cli: &Cli) -> CliResult<Game> {
    match &cli.game {
        Some(g) => db.game_by_name(g).map_err(|e| not_found(format!("{e:#}"))),
        None => Ok(db.active_game()?),
    }
}

fn active_profile(db: &Db, game: &Game, write: bool) -> CliResult<Profile> {
    if write {
        return Ok(db.ensure_active_profile(game.id)?);
    }
    db.find_active_profile(game.id)?
        .ok_or_else(|| not_found(format!("game {} has no profile yet (any write command creates 'default')", game.name)))
}

/// HomeService.addModFromModioUrl: metadata only (+ mod.io dependencies); `install` downloads.
fn add_modio_url(
    db: &mut Db,
    _cli: &Cli,
    g: &Game,
    url: &str,
    folder: Option<&str>,
    create_folder: bool,
    disabled: bool,
) -> CliResult<(Value, String)> {
    if !g.name.eq_ignore_ascii_case("drg") {
        return Err(refused("mod.io URLs are only supported for DRG"));
    }
    let name_id = modio::parse_mod_link(url)
        .ok_or_else(|| refused(format!("Invalid Mod Link: {url} (expected https://mod.io/g/drg/m/<name>)")))?;
    let mio = modio::Modio::new(db)?;
    let info = mio
        .mod_by_name_id(&name_id)?
        .ok_or_else(|| not_found(format!("Mod Not Existed: {url}")))?;
    let platform_id = info.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
    let deps = if platform_id > 0 { mio.dependencies(platform_id)? } else { Vec::new() };

    let b = db.backup_once("add")?;
    let p = active_profile(db, g, true)?;
    let folders = db.folders(p.id)?;
    let folder_id = match folder {
        Some(f) => match folders.iter().find(|x| x.name == f || x.id.to_string() == f) {
            Some(x) => Some(x.id),
            None if create_folder => {
                let max = folders.iter().filter(|x| x.parent_folder_id.is_none()).map(|x| x.sort_order).max().unwrap_or(-1);
                Some(db.create_folder(p.id, None, f, max + 1)?)
            }
            None => return Err(not_found(format!("no folder '{f}' in profile {} (use --create-folder)", p.name))),
        },
        None => folders.first().map(|f| f.id),
    };
    let main = mio.add_from_info(db, &info, p.id, folder_id)?;
    if disabled && main.status == "added" {
        db.set_enabled(main.profile_mod_id, false)?;
    }
    let mut dep_out = Vec::new();
    for d in &deps {
        dep_out.push(mio.add_from_info(db, d, p.id, folder_id)?);
    }
    let fname = db.folder_path(&db.folders(p.id)?, folder_id);
    let mut text = format!(
        "{}: {} {} (mod.io id {}) [{}] in {}/{}",
        main.status, main.name, main.version, main.platform_id, main.profile_mod_id, p.name, fname
    );
    for d in &dep_out {
        text.push_str(&format!("\n  dependency {}: {} {} (mod.io id {}) [{}]", d.status, d.name, d.version, d.platform_id, d.profile_mod_id));
    }
    if deps.is_empty() {
        text.push_str("\n  no mod.io dependencies");
    }
    Ok((
        json!({ "status": main.status, "mod": main, "dependencies": dep_out, "folder": fname, "profile": p.name, "db_backup": b,
                "note": "metadata only; run `install` to download and apply" }),
        text,
    ))
}

fn load_runtime(cli: &Cli) -> CliResult<runtime::Runtime> {
    let path = runtime::locate(&util::default_data_dir(), cli.runtime.as_deref())?;
    Ok(runtime::Runtime::load(&path)?)
}

fn mod_json(m: &ProfileMod, folder: &str) -> Value {
    json!({
        "id": m.profile_mod_id,
        "mod_id": m.mod_id,
        "name": m.display_name,
        "name_id": m.name_id,
        "enabled": m.enabled,
        "folder": folder,
        "folder_id": m.folder_id,
        "sort_order": m.sort_order,
        "source": m.source_type,
        "platform_id": m.platform_id,
        "version": m.current_version,
        "cache_path": m.cache_path,
        "file_exists": util::path_exists(&m.cache_path),
        "update_available": m.is_online() && m.online_update_date > m.last_update_date,
        "online_available": m.is_online_available,
    })
}

/// Resolve selectors against profile mods. Priority per selector:
/// numeric profile_mod id, "m<mod_id>", exact display/name_id (case-insensitive), unique substring.
fn resolve<'a>(mods: &'a [ProfileMod], sel: &str) -> CliResult<Vec<&'a ProfileMod>> {
    if let Ok(id) = sel.parse::<i64>() {
        if let Some(m) = mods.iter().find(|m| m.profile_mod_id == id) {
            return Ok(vec![m]);
        }
    }
    if let Some(id) = sel.strip_prefix('m').and_then(|s| s.parse::<i64>().ok()) {
        if let Some(m) = mods.iter().find(|m| m.mod_id == id) {
            return Ok(vec![m]);
        }
    }
    let l = sel.to_lowercase();
    let exact: Vec<&ProfileMod> = mods
        .iter()
        .filter(|m| m.display_name.to_lowercase() == l || m.name_id.to_lowercase() == l)
        .collect();
    if !exact.is_empty() {
        return Ok(exact);
    }
    let sub: Vec<&ProfileMod> = mods
        .iter()
        .filter(|m| m.display_name.to_lowercase().contains(&l) || m.name_id.to_lowercase().contains(&l))
        .collect();
    match sub.len() {
        0 => Err(not_found(format!("no mod in the active profile matches '{sel}'"))),
        1 => Ok(sub),
        _ => Err(not_found(format!(
            "'{sel}' is ambiguous: {}",
            sub.iter().map(|m| format!("{} [{}]", m.display_name, m.profile_mod_id)).collect::<Vec<_>>().join(", ")
        ))),
    }
}

fn run(cli: &Cli, rep: Reporter) -> CliResult<(Value, String)> {
    let dbp = db_path(cli);
    let write = is_write(&cli.cmd);
    // The GUI only ever opens its own DB; a --db copy can be written while it runs.
    let is_gui_db = std::fs::canonicalize(&dbp).ok()
        == std::fs::canonicalize(util::default_config_dir().join("mintcat.sqlite")).ok();
    if write && is_gui_db {
        guard_gui(cli)?;
    }
    let mut db = if write { Db::open_rw(&dbp)? } else { Db::open_readonly(&dbp).or_else(|_| Db::open_rw(&dbp))? };

    match &cli.cmd {
        Cmd::Games => {
            let games = db.games()?;
            let text = games
                .iter()
                .map(|g| format!("{} {:<4} {:<34} {}", if g.is_active { "*" } else { " " }, g.name, g.display_name, if g.install_path.is_empty() { "(no path)" } else { &g.install_path }))
                .collect::<Vec<_>>()
                .join("\n");
            Ok((json!({ "games": games }), text))
        }
        Cmd::SetGamePath { game, path } => {
            let g = db.game_by_name(game).map_err(|e| not_found(format!("{e:#}")))?;
            let path = if path == "auto" {
                let rt = load_runtime(cli)?;
                let p = rt.find_game_pak(&g.name)?;
                if p.is_empty() {
                    return Err(not_found(format!("Steam lookup found no {} install", g.name)));
                }
                p
            } else {
                path.clone()
            };
            let rc = g.name.eq_ignore_ascii_case("rc");
            let ok = if rc { path.ends_with("RogueCore-Windows.pak") } else { path.ends_with("FSD-WindowsNoEditor.pak") || path.ends_with("FSD-WinGDK.pak") };
            if !ok {
                return Err(refused(if rc { "path must end with RogueCore-Windows.pak" } else { "path must end with FSD-WindowsNoEditor.pak or FSD-WinGDK.pak" }));
            }
            if !Path::new(&path).is_file() {
                return Err(not_found(format!("file does not exist: {path}")));
            }
            let b = db.backup_once("set-game-path")?;
            db.set_game_path(g.id, &path)?;
            Ok((json!({ "game": g.name, "install_path": path, "db_backup": b }), format!("{} -> {}", g.name, path)))
        }
        Cmd::UseGame { game } => {
            let g = db.game_by_name(game).map_err(|e| not_found(format!("{e:#}")))?;
            let b = db.backup_once("use-game")?;
            db.set_active_game(g.id)?;
            let p = db.ensure_active_profile(g.id)?;
            Ok((json!({ "active_game": g.name, "active_profile": p.name, "db_backup": b }), format!("active game: {} (profile {})", g.name, p.name)))
        }
        Cmd::Profiles => {
            let g = select_game(&db, cli)?;
            let ps = db.profiles(g.id)?;
            let text = ps.iter().map(|p| format!("{} [{}] {}", if p.is_active { "*" } else { " " }, p.id, p.name)).collect::<Vec<_>>().join("\n");
            Ok((json!({ "game": g.name, "profiles": ps }), if text.is_empty() { format!("{}: no profiles", g.name) } else { text }))
        }
        Cmd::UseProfile { name } => {
            let g = select_game(&db, cli)?;
            let ps = db.profiles(g.id)?;
            let p = ps
                .iter()
                .find(|p| &p.name == name)
                .or_else(|| ps.iter().find(|p| p.name.eq_ignore_ascii_case(name) || p.display_name.eq_ignore_ascii_case(name)))
                .cloned()
                .ok_or_else(|| not_found(format!("no profile '{name}' for game {}", g.name)))?;
            let b = db.backup_once("use-profile")?;
            db.activate_profile(&p)?;
            Ok((json!({ "game": g.name, "active_profile": p.name, "db_backup": b }), format!("{}: active profile = {}", g.name, p.name)))
        }
        Cmd::List { enabled } => {
            let g = select_game(&db, cli)?;
            let p = active_profile(&db, &g, false)?;
            let folders = db.folders(p.id)?;
            let mods = db.profile_mods(p.id)?;
            let mut items = Vec::new();
            let mut lines = vec![format!("game {} / profile {} ({} mods, {} enabled)", g.name, p.name, mods.len(), mods.iter().filter(|m| m.enabled).count())];
            for m in mods.iter().filter(|m| !*enabled || m.enabled) {
                let f = db.folder_path(&folders, m.folder_id);
                lines.push(format!(
                    "{} [{:>4}] {:<44} {:<6} {}{}",
                    if m.enabled { "x" } else { " " },
                    m.profile_mod_id,
                    m.display_name,
                    m.source_type,
                    f,
                    if util::path_exists(&m.cache_path) { "" } else { "  (FILE MISSING)" }
                ));
                items.push(mod_json(m, &f));
            }
            Ok((json!({ "game": g.name, "profile": p.name, "folders": folders, "mods": items }), lines.join("\n")))
        }
        Cmd::Enable { selectors } | Cmd::Disable { selectors } => {
            let on = matches!(cli.cmd, Cmd::Enable { .. });
            if selectors.is_empty() {
                return Err(not_found("no mod selector given"));
            }
            let g = select_game(&db, cli)?;
            let p = active_profile(&db, &g, true)?;
            let mods = db.profile_mods(p.id)?;
            let mut targets = Vec::new();
            for s in selectors {
                for m in resolve(&mods, s)? {
                    if !targets.iter().any(|t: &&ProfileMod| t.profile_mod_id == m.profile_mod_id) {
                        targets.push(m);
                    }
                }
            }
            let b = db.backup_once(if on { "enable" } else { "disable" })?;
            let mut changed = Vec::new();
            for m in &targets {
                db.set_enabled(m.profile_mod_id, on)?;
                changed.push(json!({ "id": m.profile_mod_id, "name": m.display_name, "was": m.enabled, "now": on }));
            }
            let text = targets.iter().map(|m| format!("{} {}", if on { "enabled " } else { "disabled" }, m.display_name)).collect::<Vec<_>>().join("\n");
            Ok((json!({ "changed": changed, "db_backup": b, "note": "run `install` to apply to the game" }), text))
        }
        Cmd::Add { path, folder, create_folder, disabled } => {
            let g = select_game(&db, cli)?;
            let path_s = path.to_string_lossy().into_owned();
            if path_s.starts_with("http://") || path_s.starts_with("https://") {
                return add_modio_url(&mut db, cli, &g, &path_s, folder.as_deref(), *create_folder, *disabled);
            }
            let abs =std::fs::canonicalize(path).map_err(|_| not_found(format!("path does not exist: {}", path.display())))?;
            let abs_s = abs.to_string_lossy().into_owned();
            let rt = load_runtime(cli)?;
            if abs.is_dir() {
                if !rt.is_valid_unpacked_mod(&abs_s) {
                    return Err(refused("folder is not a mod: needs js/main.js, pak/, dll/ or Content/"));
                }
            } else {
                let ext = abs.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
                if ext != "pak" && ext != "zip" {
                    return Err(refused("file must be a .pak or .zip"));
                }
                if ext == "zip" && !assets::is_valid_zip(&abs) {
                    return Err(refused("not a valid zip"));
                }
            }
            let b = db.backup_once("add")?;
            let p = active_profile(&db, &g, true)?;
            let folders = db.folders(p.id)?;
            let folder_id = match folder {
                Some(f) => match folders.iter().find(|x| x.name == *f || x.id.to_string() == *f) {
                    Some(x) => Some(x.id),
                    None if *create_folder => {
                        let max = folders.iter().filter(|x| x.parent_folder_id.is_none()).map(|x| x.sort_order).max().unwrap_or(-1);
                        Some(db.create_folder(p.id, None, f, max + 1)?)
                    }
                    None => return Err(not_found(format!("no folder '{f}' in profile {} (use --create-folder)", p.name))),
                },
                None => folders.first().map(|f| f.id),
            };
            let file_name = abs.file_name().and_then(|n| n.to_str()).unwrap_or("mod").to_string();
            let (_, mtime) = util::size_and_mtime(&abs_s);
            let (mod_id, pm_id, created, added) = db.add_local_mod(p.id, folder_id, &abs_s, &file_name, mtime)?;
            if *disabled && added {
                db.set_enabled(pm_id, false)?;
            }
            let row = db.profile_mods(p.id)?.into_iter().find(|m| m.profile_mod_id == pm_id);
            let fname = db.folder_path(&db.folders(p.id)?, row.as_ref().and_then(|r| r.folder_id));
            let enabled = row.as_ref().map(|r| r.enabled).unwrap_or(!disabled);
            let status = if added { "added" } else { "exists" };
            Ok((
                json!({ "status": status, "mod_id": mod_id, "id": pm_id, "created_mod": created, "path": abs_s, "folder": fname, "enabled": enabled, "profile": p.name, "db_backup": b }),
                format!("{status}: {} [{}] in {}/{}", file_name, pm_id, p.name, fname),
            ))
        }
        Cmd::Install { dry_run, force, offline, allow_foreign_paks, allow_old_mint, no_preserve, target } => {
            let g = select_game(&db, cli)?;
            // validate before anything can write (ensure_active_profile may create a profile)
            let pak = target.clone().unwrap_or_else(|| g.install_path.clone());
            if pak.is_empty() || !Path::new(&pak).is_file() {
                return Err(refused(format!("Game Path Not Found for {}: '{pak}' (use set-game-path)", g.name)));
            }
            let p = if *dry_run || target.is_some() { active_profile(&db, &g, false)? } else { active_profile(&db, &g, true)? };
            let rt = load_runtime(cli)?;
            let cache = cache_dir(&db)?;
            let mut ctx = Ctx { db: &mut db, rt: &rt, rep, cache_dir: cache, data_dir: util::default_data_dir() };
            let opts = InstallOptions {
                dry_run: *dry_run,
                force: *force,
                offline: *offline,
                allow_foreign_paks: *allow_foreign_paks,
                allow_old_mint: *allow_old_mint,
                preserve: !*no_preserve,
                target: target.clone(),
            };
            let v = install::run_install(&mut ctx, &g, &p, &opts)?;
            let text = format!(
                "{}: {} ({} mods, ue4ss {}){}",
                g.name,
                v["action"].as_str().unwrap_or("?"),
                v["mods"].as_array().map(|a| a.len()).unwrap_or(0),
                if v["ue4ss_enabled"].as_bool() == Some(true) { "on" } else { "off" },
                v.get("preserved")
                    .and_then(|p| p.get("foreign_mod_dirs"))
                    .and_then(|f| f.as_array())
                    .filter(|a| !a.is_empty())
                    .map(|a| format!("; preserved foreign ue4ss mods: {}", a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", ")))
                    .unwrap_or_default()
            );
            Ok((v, text))
        }
        Cmd::Update { dry_run, all } => {
            let g = select_game(&db, cli)?;
            let p = active_profile(&db, &g, !*dry_run)?;
            let mods: Vec<ProfileMod> = db.profile_mods(p.id)?.into_iter().filter(|m| m.is_online() && (*all || m.enabled)).collect();
            let client = modio::Modio::new(&db)?;
            client.check_auth()?;
            let ids: Vec<i64> = mods.iter().filter(|m| m.source_type == "Modio" && m.platform_id > 0).map(|m| m.platform_id).collect();
            let infos = client.mods_by_ids(&ids)?;
            let mut entries = Vec::new();
            if *dry_run {
                for m in &mods {
                    let info = infos.iter().find(|i| i.get("id").and_then(|v| v.as_i64()) == Some(m.platform_id));
                    let new_version = info.and_then(|i| i.pointer("/modfile/version")).and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let date = info.and_then(|i| i.get("date_updated")).and_then(|v| v.as_i64()).unwrap_or(0) * 1000;
                    let need = info.is_some() && (date > m.last_update_date || !util::path_exists(&m.cache_path) || m.download_progress != 100);
                    entries.push(modio::UpdateEntry {
                        mod_id: m.mod_id,
                        name: m.display_name.clone(),
                        old_version: m.current_version.clone(),
                        new_version,
                        needs_download: need,
                        action: if info.is_none() { "unavailable".into() } else if need { "would_download".into() } else { "up_to_date".into() },
                        cache_path: m.cache_path.clone(),
                        message: None,
                    });
                }
            } else {
                let b = db.backup_once("update")?;
                if let Some(b) = b {
                    rep.info(format!("DB backup: {}", b.display()));
                }
                let unavailable = modio::refresh_metadata(&client, &db, &mods, false, &rep)?;
                let refreshed: Vec<ProfileMod> = db.profile_mods(p.id)?.into_iter().filter(|x| mods.iter().any(|m| m.profile_mod_id == x.profile_mod_id)).collect();
                let need: Vec<ProfileMod> = refreshed.iter().filter(|m| m.source_type == "Modio" && !unavailable.contains(&m.mod_id) && modio::needs_download(m)).cloned().collect();
                if !need.is_empty() {
                    modio::refresh_metadata(&client, &db, &need, true, &rep)?;
                }
                let cache = cache_dir(&db)?;
                let latest = db.profile_mods(p.id)?;
                let mut failed = 0;
                for m in &refreshed {
                    let old = mods.iter().find(|o| o.profile_mod_id == m.profile_mod_id).unwrap();
                    let mut e = modio::UpdateEntry {
                        mod_id: m.mod_id,
                        name: m.display_name.clone(),
                        old_version: old.current_version.clone(),
                        new_version: m.current_version.clone(),
                        needs_download: false,
                        action: "up_to_date".into(),
                        cache_path: m.cache_path.clone(),
                        message: None,
                    };
                    if unavailable.contains(&m.mod_id) {
                        e.action = "unavailable".into();
                    } else if m.source_type != "Modio" {
                        e.action = "skipped".into();
                        e.message = Some("ModCat source not supported by CLI".into());
                    } else if need.iter().any(|n| n.profile_mod_id == m.profile_mod_id) {
                        e.needs_download = true;
                        let cur = latest.iter().find(|x| x.profile_mod_id == m.profile_mod_id).unwrap_or(m);
                        e.new_version = cur.current_version.clone();
                        rep.info(format!("downloading {} {}", cur.display_name, cur.current_version));
                        match client.download(&db, cur, &cache) {
                            Ok(path) => {
                                e.action = "downloaded".into();
                                e.cache_path = path.to_string_lossy().into_owned();
                            }
                            Err(err) => {
                                failed += 1;
                                e.action = "failed".into();
                                e.message = Some(format!("{err:#}"));
                            }
                        }
                    }
                    entries.push(e);
                }
                if failed > 0 {
                    return Err(CliError::Failed(anyhow::anyhow!("{failed} downloads failed: {}", serde_json::to_string(&entries).unwrap_or_default())));
                }
            }
            let n = entries.iter().filter(|e| e.needs_download).count();
            let text = entries
                .iter()
                .filter(|e| e.action != "up_to_date")
                .map(|e| format!("{:<15} {} {} -> {}", e.action, e.name, e.old_version, e.new_version))
                .collect::<Vec<_>>()
                .join("\n");
            Ok((
                json!({ "game": g.name, "profile": p.name, "dry_run": dry_run, "checked": entries.len(), "updates": n, "mods": entries, "note": "run `install` to apply downloaded updates to the game" }),
                if text.is_empty() { format!("{} online mods checked, all up to date", entries.len()) } else { text },
            ))
        }
        Cmd::Doctor => {
            let data = util::default_data_dir();
            let rt_path = runtime::locate(&data, cli.runtime.as_deref());
            let rt_ok = rt_path.as_ref().ok().map(|p| runtime::Runtime::load(p).map(|_| ()).map_err(|e| format!("{e:#}")));
            let exe = std::env::current_exe().ok();
            let oodle = exe.as_ref().map(|e| e.with_file_name("liboo2corelinux64.so.9"));
            let modio_auth = match modio::Modio::new(&db) {
                Ok(m) => match m.check_auth() {
                    Ok(()) => "ok".to_string(),
                    Err(e) => format!("{e:#}"),
                },
                Err(e) => format!("{e:#}"),
            };
            let v = json!({
                "db": dbp,
                "cache_dir": cache_dir(&db)?,
                "data_dir": data,
                "runtime": rt_path.as_ref().map(|p| p.display().to_string()).map_err(|e| format!("{e:#}")).unwrap_or_else(|e| e),
                "runtime_loads": rt_ok.map(|r| r.err().unwrap_or_else(|| "ok".into())),
                "exe": exe,
                "oodle_beside_exe": oodle.as_ref().map(|o| o.exists()),
                "gui_pids": util::gui_pids(),
                "game_processes": install::running_game_processes(),
                "ue4ss_enabled": install::ue4ss_enabled(&db)?,
                "release_channel": install::release_channel(&db)?,
                "modio_auth": modio_auth,
                "games": db.games()?,
            });
            let text = serde_json::to_string_pretty(&v).context("json")?;
            Ok((v, text))
        }
    }
}
