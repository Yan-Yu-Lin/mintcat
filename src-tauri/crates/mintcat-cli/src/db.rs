//! Direct access to MintCat's SQLite state (`~/.config/com.mint.cat/mintcat.sqlite`).
//!
//! Every write mirrors what the corresponding GUI DAO does (src/storage/dao/*.ts):
//! drizzle `timestamp` columns are unix *seconds*, `mod_status` dates are *milliseconds*,
//! booleans are 0/1, JSON columns are compact JSON text.

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::util::{now_secs, stamp};

pub const ACTIVE_USER_ID: i64 = 1; // UserDAO.getActiveUser() always returns user 1

#[derive(Debug, Clone, Serialize)]
pub struct Game {
    pub id: i64,
    pub name: String,
    pub display_name: String,
    pub install_path: String,
    pub is_active: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Profile {
    pub id: i64,
    pub name: String,
    pub display_name: String,
    pub game_id: i64,
    pub is_active: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Folder {
    pub id: i64,
    pub parent_folder_id: Option<i64>,
    pub name: String,
    pub sort_order: i64,
}

/// One row of `profile_mods` joined with the mod's metadata (CompleteModData equivalent).
#[derive(Debug, Clone, Serialize)]
pub struct ProfileMod {
    pub profile_mod_id: i64,
    pub mod_id: i64,
    pub folder_id: Option<i64>,
    pub sort_order: i64,
    pub enabled: bool,
    pub used_version: String,
    pub platform_id: i64,
    pub name_id: String,
    pub display_name: String,
    pub url: String,
    pub source_type: String,
    pub tags: Vec<String>,
    pub current_version: String,
    pub download_url: String,
    pub cache_path: String,
    pub file_size: i64,
    pub download_progress: i64,
    pub last_update_date: i64,
    pub online_update_date: i64,
    pub is_online_available: bool,
}

impl ProfileMod {
    pub fn is_online(&self) -> bool {
        self.source_type == "Modio" || self.source_type == "modcat"
    }
}

pub struct Db {
    pub conn: Connection,
    pub path: PathBuf,
    backed_up: bool,
}

impl Db {
    pub fn open_readonly(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("failed to open MintCat DB {:?}", path))?;
        conn.busy_timeout(Duration::from_secs(10))?;
        Ok(Self {
            conn,
            path: path.to_path_buf(),
            backed_up: false,
        })
    }

    pub fn open_rw(path: &Path) -> Result<Self> {
        if !path.is_file() {
            bail!("MintCat DB not found: {:?}", path);
        }
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("failed to open MintCat DB {:?}", path))?;
        conn.busy_timeout(Duration::from_secs(10))?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        Ok(Self {
            conn,
            path: path.to_path_buf(),
            backed_up: false,
        })
    }

    /// Consistent snapshot next to the DB (`mintcat.sqlite.cli-<stamp>.bak`) via
    /// `VACUUM INTO`, which includes pending WAL content. Called once before the first write.
    pub fn backup_once(&mut self, label: &str) -> Result<Option<PathBuf>> {
        if self.backed_up {
            return Ok(None);
        }
        let file = self
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("mintcat.sqlite");
        let base = format!("{file}.cli-{label}-{}", stamp());
        let mut backup = self.path.with_file_name(format!("{base}.bak"));
        let mut n = 1;
        while backup.exists() {
            n += 1;
            backup = self.path.with_file_name(format!("{base}-{n}.bak"));
        }
        self.conn
            .execute("VACUUM INTO ?1", params![backup.to_string_lossy()])
            .with_context(|| format!("failed to back up DB to {:?}", backup))?;
        self.backed_up = true;
        Ok(Some(backup))
    }

    // ---------------------------------------------------------------- games

    pub fn games(&self) -> Result<Vec<Game>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, display_name, install_path, is_active FROM games ORDER BY id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Game {
                id: r.get(0)?,
                name: r.get(1)?,
                display_name: r.get(2)?,
                install_path: r.get(3)?,
                is_active: r.get::<_, i64>(4)? != 0,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// GameDAO.getActiveGame(): first is_active row by id.
    pub fn active_game(&self) -> Result<Game> {
        self.games()?
            .into_iter()
            .find(|g| g.is_active)
            .context("no active game in DB")
    }

    pub fn game_by_name(&self, name: &str) -> Result<Game> {
        self.games()?
            .into_iter()
            .find(|g| g.name.eq_ignore_ascii_case(name))
            .with_context(|| format!("unknown game '{name}' (expected drg or rc)"))
    }

    /// DialogGameService.setGameActive
    pub fn set_active_game(&self, game_id: i64) -> Result<()> {
        let now = now_secs();
        for g in self.games()? {
            let should = g.id == game_id;
            if g.is_active != should {
                self.conn.execute(
                    "UPDATE games SET is_active = ?1, updated_at = ?2 WHERE id = ?3",
                    params![should as i64, now, g.id],
                )?;
            }
        }
        Ok(())
    }

    /// DialogGameService.updateGameInstallPath
    pub fn set_game_path(&self, game_id: i64, path: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE games SET install_path = ?1, updated_at = ?2 WHERE id = ?3",
            params![path, now_secs(), game_id],
        )?;
        Ok(())
    }

    // ------------------------------------------------------------- profiles

    /// ProfileDAO.getProfilesByUserAndGame (ordered by last_used_at desc)
    pub fn profiles(&self, game_id: i64) -> Result<Vec<Profile>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, display_name, game_id, is_active FROM profiles
             WHERE user_id = ?1 AND game_id = ?2 ORDER BY last_used_at DESC",
        )?;
        let rows = stmt.query_map(params![ACTIVE_USER_ID, game_id], |r| {
            Ok(Profile {
                id: r.get(0)?,
                name: r.get(1)?,
                display_name: r.get(2)?,
                game_id: r.get(3)?,
                is_active: r.get::<_, i64>(4)? != 0,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// ProfileService.ensureActiveProfile without the create-default branch
    /// (read-only callers must not create rows).
    pub fn find_active_profile(&self, game_id: i64) -> Result<Option<Profile>> {
        let list = self.profiles(game_id)?;
        if let Some(p) = list.iter().find(|p| p.is_active) {
            return Ok(Some(p.clone()));
        }
        Ok(list.into_iter().next())
    }

    /// Full ProfileService.ensureActiveProfile: creates "default" + folder "默认分组" when
    /// the game has no profile, and activates the first profile when none is active.
    pub fn ensure_active_profile(&self, game_id: i64) -> Result<Profile> {
        let list = self.profiles(game_id)?;
        if list.is_empty() {
            self.conn.execute(
                "INSERT INTO profiles (name, display_name, game_id, user_id, is_active)
                 VALUES ('default', 'default', ?1, ?2, 1)",
                params![game_id, ACTIVE_USER_ID],
            )?;
            let id = self.conn.last_insert_rowid();
            self.create_folder(id, None, "默认分组", 0)?;
            return self
                .profiles(game_id)?
                .into_iter()
                .find(|p| p.id == id)
                .context("created profile vanished");
        }
        if let Some(p) = list.iter().find(|p| p.is_active) {
            return Ok(p.clone());
        }
        let first = list[0].clone();
        self.conn.execute(
            "UPDATE profiles SET is_active = 1, updated_at = ?1 WHERE id = ?2",
            params![now_secs(), first.id],
        )?;
        Ok(Profile {
            is_active: true,
            ..first
        })
    }

    /// ProfileDAO.activateProfile
    pub fn activate_profile(&self, profile: &Profile) -> Result<()> {
        let now = now_secs();
        self.conn.execute(
            "UPDATE profiles SET is_active = 0, updated_at = ?1 WHERE user_id = ?2 AND game_id = ?3",
            params![now, ACTIVE_USER_ID, profile.game_id],
        )?;
        self.conn.execute(
            "UPDATE profiles SET is_active = 1, last_used_at = ?1, updated_at = ?1 WHERE id = ?2",
            params![now, profile.id],
        )?;
        Ok(())
    }

    // -------------------------------------------------------------- folders

    pub fn folders(&self, profile_id: i64) -> Result<Vec<Folder>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, parent_folder_id, name, sort_order FROM profile_folders
             WHERE profile_id = ?1 ORDER BY sort_order",
        )?;
        let rows = stmt.query_map(params![profile_id], |r| {
            Ok(Folder {
                id: r.get(0)?,
                parent_folder_id: r.get(1)?,
                name: r.get(2)?,
                sort_order: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn create_folder(
        &self,
        profile_id: i64,
        parent: Option<i64>,
        name: &str,
        sort_order: i64,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO profile_folders (profile_id, parent_folder_id, name, folder_type, sort_order, is_expanded)
             VALUES (?1, ?2, ?3, 'custom', ?4, 1)",
            params![profile_id, parent, name, sort_order],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// "a/b" style path or plain name -> folder id.
    pub fn folder_path(&self, folders: &[Folder], id: Option<i64>) -> String {
        let mut parts = Vec::new();
        let mut cur = id;
        let mut guard = 0;
        while let Some(fid) = cur {
            guard += 1;
            if guard > 64 {
                break;
            }
            match folders.iter().find(|f| f.id == fid) {
                Some(f) => {
                    parts.push(f.name.clone());
                    cur = f.parent_folder_id;
                }
                None => break,
            }
        }
        parts.reverse();
        parts.join("/")
    }

    // ---------------------------------------------------------- profile mods

    /// ProfileDAO.getProfileMods (ORDER BY sort_order) + getCompleteModData for each.
    /// Ties on sort_order fall back to SQLite's natural (rowid) order, same query as the GUI.
    pub fn profile_mods(&self, profile_id: i64) -> Result<Vec<ProfileMod>> {
        let mut stmt = self.conn.prepare(
            "SELECT pm.id, pm.mod_id, pm.parent_folder_id, pm.sort_order, pm.is_enabled, pm.used_version,
                    m.platform_id, m.name_id, m.display_name, m.url, m.source_type, m.tags,
                    COALESCE(v.current_version, '-'),
                    COALESCE(d.download_url, ''), COALESCE(d.cache_path, ''), COALESCE(d.file_size, 0),
                    COALESCE(d.download_progress, 0),
                    COALESCE(s.last_update_date, 0), COALESCE(s.online_update_date, 0),
                    COALESCE(s.is_online_available, 1)
             FROM (SELECT * FROM profile_mods WHERE profile_id = ?1 ORDER BY sort_order) pm
             JOIN mods m ON m.mod_id = pm.mod_id
             LEFT JOIN mod_versions v ON v.mod_id = m.mod_id
             LEFT JOIN mod_downloads d ON d.mod_id = m.mod_id
             LEFT JOIN mod_status s ON s.mod_id = m.mod_id",
        )?;
        let rows = stmt.query_map(params![profile_id], |r| {
            let tags: String = r.get(11)?;
            Ok(ProfileMod {
                profile_mod_id: r.get(0)?,
                mod_id: r.get(1)?,
                folder_id: r.get(2)?,
                sort_order: r.get(3)?,
                enabled: r.get::<_, i64>(4)? != 0,
                used_version: r.get(5)?,
                platform_id: r.get(6)?,
                name_id: r.get(7)?,
                display_name: r.get(8)?,
                url: r.get(9)?,
                source_type: r.get(10)?,
                tags: serde_json::from_str(&tags).unwrap_or_default(),
                current_version: r.get(12)?,
                download_url: r.get(13)?,
                cache_path: r.get(14)?,
                file_size: r.get(15)?,
                download_progress: r.get(16)?,
                last_update_date: r.get(17)?,
                online_update_date: r.get(18)?,
                is_online_available: r.get::<_, i64>(19)? != 0,
            })
        })?;
        let list: Vec<ProfileMod> = rows.collect::<std::result::Result<_, _>>()?;
        // The join may not preserve order. Re-order by the exact statement the GUI's
        // ProfileDAO.getProfileMods runs, so ties on sort_order resolve identically
        // (the order of mods fed to the integrator decides which duplicate file wins).
        let order = self.profile_mod_order(profile_id)?;
        let mut by_id: std::collections::HashMap<i64, ProfileMod> =
            list.into_iter().map(|m| (m.profile_mod_id, m)).collect();
        Ok(order.into_iter().filter_map(|id| by_id.remove(&id)).collect())
    }

    /// Same statement the GUI's ProfileDAO.getProfileMods runs, to capture the exact tie order.
    pub fn profile_mod_order(&self, profile_id: i64) -> Result<Vec<i64>> {
        let mut stmt = self.conn.prepare(
            "SELECT id FROM profile_mods WHERE profile_id = ?1 ORDER BY sort_order",
        )?;
        let rows = stmt.query_map(params![profile_id], |r| r.get::<_, i64>(0))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn set_enabled(&self, profile_mod_id: i64, enabled: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE profile_mods SET is_enabled = ?1, updated_at = ?2 WHERE id = ?3",
            params![enabled as i64, now_secs(), profile_mod_id],
        )?;
        Ok(())
    }

    pub fn mod_id_by_url(&self, url: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row("SELECT mod_id FROM mods WHERE url = ?1", params![url], |r| {
                r.get(0)
            })
            .optional()?)
    }

    /// ModDAO.getModByPlatformId(platformId, sourceType)
    pub fn mod_id_by_platform(&self, platform_id: i64, source_type: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT mod_id FROM mods WHERE platform_id = ?1 AND source_type = ?2 LIMIT 1",
                params![platform_id, source_type],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn profile_mod_id(&self, profile_id: i64, mod_id: i64) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM profile_mods WHERE profile_id = ?1 AND mod_id = ?2",
                params![profile_id, mod_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// HomeService.addModToProfile / ModService.ensureProfileModAssociation (enabled, used_version '')
    pub fn add_profile_mod(&self, profile_id: i64, mod_id: i64, folder_id: Option<i64>) -> Result<i64> {
        let max_sort: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(sort_order), -1) FROM profile_mods WHERE profile_id = ?1",
            params![profile_id],
            |r| r.get(0),
        )?;
        self.conn.execute(
            "INSERT INTO profile_mods (profile_id, mod_id, parent_folder_id, sort_order, is_enabled, used_version)
             VALUES (?1, ?2, ?3, ?4, 1, '')",
            params![profile_id, mod_id, folder_id, max_sort + 1],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// ModService.addModFromPath / HomeService.addModFromPath for a local file or folder.
    /// Returns (mod_id, profile_mod_id, created_mod, added_to_profile).
    pub fn add_local_mod(
        &self,
        profile_id: i64,
        folder_id: Option<i64>,
        path: &str,
        file_name: &str,
        mtime_ms: i64,
    ) -> Result<(i64, i64, bool, bool)> {
        let (mod_id, created) = match self.mod_id_by_url(path)? {
            Some(id) => (id, false),
            None => {
                self.conn.execute(
                    "INSERT INTO mods (platform_id, game_id, name_id, display_name, original_name, url,
                                       source_type, tags, approval_status, depend_mod_id)
                     VALUES (0, 1, ?1, ?1, ?1, ?2, 'Local', '[]', '', 0)",
                    params![file_name, path],
                )?;
                let id = self.conn.last_insert_rowid();
                self.conn.execute(
                    "INSERT INTO mod_versions (mod_id, current_version, available_versions) VALUES (?1, '-', '[]')",
                    params![id],
                )?;
                self.conn.execute(
                    "INSERT INTO mod_downloads (mod_id, download_url, cache_path, file_size, download_progress, download_status)
                     VALUES (?1, '', ?2, 0, 100, 'completed')",
                    params![id, path],
                )?;
                self.conn.execute(
                    "INSERT INTO mod_status (mod_id, last_update_date, online_update_date, is_online_available, is_local_not_found)
                     VALUES (?1, ?2, 0, 0, 0)",
                    params![id, mtime_ms],
                )?;
                (id, true)
            }
        };

        let existing: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM profile_mods WHERE profile_id = ?1 AND mod_id = ?2",
                params![profile_id, mod_id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(pm) = existing {
            return Ok((mod_id, pm, created, false));
        }
        let max_sort: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(sort_order), -1) FROM profile_mods WHERE profile_id = ?1",
            params![profile_id],
            |r| r.get(0),
        )?;
        self.conn.execute(
            "INSERT INTO profile_mods (profile_id, mod_id, parent_folder_id, sort_order, is_enabled, used_version)
             VALUES (?1, ?2, ?3, ?4, 1, '-')",
            params![profile_id, mod_id, folder_id, max_sort + 1],
        )?;
        Ok((mod_id, self.conn.last_insert_rowid(), created, true))
    }

    // -------------------------------------------------------------- mod state

    /// ModUpdateService.checkLocalModModify: store the local file mtime (ms) as last_update_date.
    pub fn set_last_update_date(&self, mod_id: i64, value: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE mod_status SET last_update_date = ?1, updated_at = ?2 WHERE mod_id = ?3",
            params![value, now_secs(), mod_id],
        )?;
        Ok(())
    }

    pub fn set_local_not_found(&self, mod_id: i64, missing: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE mod_status SET is_local_not_found = ?1, updated_at = ?2 WHERE mod_id = ?3 AND is_local_not_found != ?1",
            params![missing as i64, now_secs(), mod_id],
        )?;
        Ok(())
    }

    /// Ensure the three satellite rows exist (GUI upserts create them on demand).
    pub fn ensure_satellite_rows(&self, mod_id: i64) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO mod_versions (mod_id) VALUES (?1)",
            params![mod_id],
        )?;
        self.conn.execute(
            "INSERT OR IGNORE INTO mod_downloads (mod_id) VALUES (?1)",
            params![mod_id],
        )?;
        self.conn.execute(
            "INSERT OR IGNORE INTO mod_status (mod_id) VALUES (?1)",
            params![mod_id],
        )?;
        Ok(())
    }

    // --------------------------------------------------------------- settings

    pub fn setting(&self, name: &str) -> Result<String> {
        Ok(self
            .conn
            .query_row(
                "SELECT value FROM settings WHERE name = ?1",
                params![name],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or_default())
    }

    /// SettingDAO.setValue (insert ... on conflict(name) do update)
    pub fn set_setting(&self, name: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO settings (name, value) VALUES (?1, ?2)
             ON CONFLICT(name) DO UPDATE SET value = excluded.value, updated_at = ?3",
            params![name, value, now_secs()],
        )?;
        Ok(())
    }

    pub fn modio_token(&self) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT oauth FROM oauths WHERE platform = 'mod.io' AND uid = ?1 LIMIT 1",
                params![ACTIVE_USER_ID],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()))
    }
}
