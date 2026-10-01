//! mod.io metadata refresh + download, ported from src/services/ModUpdateService.ts
//! (refreshModioMetadataBatch / updateModInDatabase / updateModFile) and
//! src/apis/modio/index.ts (getModInfoByIdList / downloadModFile).
//! Uses the OAuth token MintCat already stored in `oauths` (platform 'mod.io').

use anyhow::{bail, Context, Result};
use rusqlite::params;
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::assets::is_valid_zip;
use crate::db::{Db, ProfileMod};
use crate::report::Reporter;
use crate::util::{now_ms, now_secs, sanitize_cache_file_name};

const GAME_ID: u32 = 2475;
const FALLBACK_UID: &str = "13595141";

pub struct Modio {
    client: reqwest::blocking::Client,
    host: String,
    token: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateEntry {
    pub mod_id: i64,
    pub name: String,
    pub old_version: String,
    pub new_version: String,
    pub needs_download: bool,
    pub action: String, // up_to_date | would_download | downloaded | unavailable | skipped
    pub cache_path: String,
    pub message: Option<String>,
}

fn version_like(tag: &str) -> bool {
    // /^v?\d+\.\d+(\.\d+)*$/i
    let t = tag.trim();
    let t = t.strip_prefix('v').or_else(|| t.strip_prefix('V')).unwrap_or(t);
    let parts: Vec<&str> = t.split('.').collect();
    parts.len() >= 2 && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

/// ModMapper.parseTags -> (tags, versions (latest first), approval)
fn parse_tags(raw: &[String]) -> (Vec<String>, Vec<String>, String) {
    let mut tags = Vec::new();
    let mut versions = Vec::new();
    let mut approval = String::new();
    for tag in raw {
        if version_like(tag) {
            versions.push(tag.trim().to_string());
        } else if tag == "Verified" || tag == "Auto-Verified" {
            approval = "Verified".into();
        } else if tag == "Approved" {
            approval = "Approved".into();
        } else if tag == "Sandbox" {
            approval = "Sandbox".into();
        } else {
            tags.push(tag.clone());
        }
    }
    versions.reverse();
    (tags, versions, approval)
}

impl Modio {
    pub fn new(db: &Db) -> Result<Self> {
        let token = db
            .modio_token()?
            .context("no mod.io OAuth token in MintCat DB (log in to mod.io once in the GUI)")?;
        let uid: Option<String> = db
            .conn
            .query_row(
                "SELECT uid FROM oauths WHERE platform = 'mod.io' AND uid = 1 LIMIT 1",
                [],
                |r| r.get::<_, i64>(0),
            )
            .ok()
            .map(|v| v.to_string());
        let uid = uid.filter(|s| !s.is_empty()).unwrap_or_else(|| FALLBACK_UID.into());
        Ok(Self {
            client: reqwest::blocking::Client::builder()
                .connect_timeout(Duration::from_secs(20))
                .timeout(Duration::from_secs(900))
                .build()?,
            host: format!("https://u-{uid}.modapi.io/v1"),
            token,
        })
    }

    fn get(&self, path: &str) -> Result<Value> {
        let url = format!("{}{}", self.host, path);
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&self.token)
            .header("Accept", "application/json")
            .send()
            .with_context(|| format!("mod.io request failed: {url}"))?;
        match resp.status().as_u16() {
            200 => Ok(resp.json()?),
            401 => bail!("mod.io Unauthorized (token expired/revoked: log in again in the GUI)"),
            404 => bail!("mod.io Not Found: {url}"),
            429 => bail!("mod.io Too Many Requests"),
            s => bail!("mod.io Error: {s} ({url})"),
        }
    }

    pub fn check_auth(&self) -> Result<()> {
        self.get("/me").map(|_| ())
    }

    pub fn mods_by_ids(&self, ids: &[i64]) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        for chunk in ids.chunks(20) {
            let list = chunk.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
            let data = self.get(&format!("/games/{GAME_ID}/mods?id-in={list}"))?;
            if let Some(arr) = data.get("data").and_then(|d| d.as_array()) {
                out.extend(arr.iter().cloned());
            }
        }
        Ok(out)
    }

    pub fn mod_tags(&self, platform_id: i64) -> Vec<String> {
        self.get(&format!("/games/{GAME_ID}/mods/{platform_id}/tags"))
            .ok()
            .and_then(|d| d.get("data").and_then(|x| x.as_array()).cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(String::from))
            .collect()
    }

    /// ModUpdateService.updateModInDatabase
    pub fn apply_info(&self, db: &Db, m: &ProfileMod, info: &Value, fetch_tags: bool) -> Result<()> {
        let s = |v: Option<&Value>| v.and_then(|x| x.as_str()).unwrap_or("").to_string();
        let platform_id = info.get("id").and_then(|v| v.as_i64()).unwrap_or(m.platform_id);
        let mut raw_tags: Vec<String> = if fetch_tags {
            self.mod_tags(platform_id)
        } else {
            info.get("tags")
                .and_then(|t| t.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(String::from))
                        .collect()
                })
                .unwrap_or_default()
        };
        if raw_tags.is_empty() && platform_id > 0 {
            raw_tags = self.mod_tags(platform_id);
        }
        let name_id = Some(s(info.get("name_id"))).filter(|x| !x.is_empty()).unwrap_or(m.name_id.clone());
        let url = Some(s(info.get("profile_url"))).filter(|x| !x.is_empty()).unwrap_or(m.url.clone());
        let original = s(info.get("name"));
        let now = now_secs();
        db.conn.execute(
            "UPDATE mods SET platform_id = ?1, name_id = ?2, url = ?3,
                    original_name = CASE WHEN ?4 = '' THEN original_name ELSE ?4 END, updated_at = ?5
             WHERE mod_id = ?6",
            params![platform_id, name_id, url, original, now, m.mod_id],
        )?;
        let (tags, versions, approval) = parse_tags(&raw_tags);
        if !raw_tags.is_empty() {
            db.conn.execute(
                "UPDATE mods SET tags = ?1, approval_status = ?2 WHERE mod_id = ?3",
                params![serde_json::to_string(&tags)?, approval, m.mod_id],
            )?;
        }
        db.ensure_satellite_rows(m.mod_id)?;
        let mf = info.get("modfile");
        let version = mf
            .and_then(|f| f.get("version").and_then(|v| v.as_str()).filter(|v| !v.is_empty()))
            .or_else(|| mf.and_then(|f| f.get("filename").and_then(|v| v.as_str())).filter(|v| !v.is_empty()))
            .unwrap_or("-")
            .to_string();
        if !raw_tags.is_empty() {
            db.conn.execute(
                "UPDATE mod_versions SET current_version = ?1, available_versions = ?2, updated_at = ?3 WHERE mod_id = ?4",
                params![version, serde_json::to_string(&versions)?, now, m.mod_id],
            )?;
        } else {
            db.conn.execute(
                "UPDATE mod_versions SET current_version = ?1, updated_at = ?2 WHERE mod_id = ?3",
                params![version, now, m.mod_id],
            )?;
        }
        let dl = mf
            .and_then(|f| f.pointer("/download/binary_url"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let size = mf.and_then(|f| f.get("filesize")).and_then(|v| v.as_i64()).unwrap_or(0);
        db.conn.execute(
            "UPDATE mod_downloads SET download_url = ?1, file_size = ?2, updated_at = ?3 WHERE mod_id = ?4",
            params![dl, size, now, m.mod_id],
        )?;
        let date = info.get("date_updated").and_then(|v| v.as_i64()).unwrap_or(0);
        let online = if date > 0 { date * 1000 } else { now_ms() };
        db.conn.execute(
            "UPDATE mod_status SET online_update_date = ?1, is_online_available = 1, updated_at = ?2 WHERE mod_id = ?3",
            params![online, now, m.mod_id],
        )?;
        Ok(())
    }

    pub fn mark_unavailable(&self, db: &Db, mod_id: i64) -> Result<()> {
        db.ensure_satellite_rows(mod_id)?;
        db.conn.execute(
            "UPDATE mod_status SET is_online_available = 0, updated_at = ?1 WHERE mod_id = ?2",
            params![now_secs(), mod_id],
        )?;
        Ok(())
    }

    /// ModioApi.downloadModFile + ModUpdateService.updateModFile (DB bookkeeping).
    pub fn download(&self, db: &Db, m: &ProfileMod, cache_dir: &Path) -> Result<PathBuf> {
        let file = sanitize_cache_file_name(&format!("{}-{}.zip", m.name_id, m.current_version));
        let dest = cache_dir.join(file);
        let reuse = std::fs::metadata(&dest)
            .map(|md| md.len() as i64 == m.file_size)
            .unwrap_or(false)
            && is_valid_zip(&dest);
        if !reuse {
            if m.download_url.is_empty() {
                bail!("no download url for {}", m.display_name);
            }
            let resp = self
                .client
                .get(&m.download_url)
                .send()
                .with_context(|| format!("download failed: {}", m.display_name))?;
            if !resp.status().is_success() {
                bail!("download failed for {}: HTTP {}", m.display_name, resp.status());
            }
            let bytes = resp.bytes()?;
            let tmp = dest.with_extension("zip.part");
            std::fs::write(&tmp, &bytes)?;
            std::fs::rename(&tmp, &dest)?;
            if !is_valid_zip(&dest) {
                let _ = std::fs::remove_file(&dest);
                bail!("Downloaded file is corrupted: {}", m.display_name);
            }
        }
        let now = now_secs();
        db.conn.execute(
            "UPDATE mod_downloads SET cache_path = ?1, download_progress = 100, download_status = 'completed', updated_at = ?2 WHERE mod_id = ?3",
            params![dest.to_string_lossy(), now, m.mod_id],
        )?;
        db.conn.execute(
            "UPDATE mod_status SET last_update_date = CASE WHEN online_update_date = 0 THEN ?1 ELSE online_update_date END, updated_at = ?2 WHERE mod_id = ?3",
            params![now_ms(), now, m.mod_id],
        )?;
        Ok(dest)
    }
}

/// ModioApi.parseModLinks: `^https://mod\.io/g/drg/m/([^/#]+)` -> name_id
pub fn parse_mod_link(link: &str) -> Option<String> {
    let rest = link.strip_prefix("https://mod.io/g/drg/m/")?;
    let name: String = rest.chars().take_while(|c| *c != '/' && *c != '#').collect();
    // the GUI regex has no `?` exclusion, but a query string is never part of a name_id
    let name = name.split('?').next().unwrap_or("").to_string();
    if name.is_empty() { None } else { Some(name) }
}

#[derive(Debug, Clone, Serialize)]
pub struct AddedOnline {
    pub status: String, // added | exists
    pub mod_id: i64,
    pub profile_mod_id: i64,
    pub created_mod: bool,
    pub platform_id: i64,
    pub name: String,
    pub version: String,
}

impl Modio {
    /// ModioApi.getModInfoByName
    pub fn mod_by_name_id(&self, name_id: &str) -> Result<Option<Value>> {
        let data = self.get(&format!("/games/{GAME_ID}/mods?name_id={name_id}"))?;
        Ok(data.get("data").and_then(|d| d.as_array()).and_then(|a| a.first().cloned()))
    }

    /// ModioApi.getDependencies
    pub fn dependencies(&self, platform_id: i64) -> Result<Vec<Value>> {
        let data = self.get(&format!("/games/{GAME_ID}/mods/{platform_id}/dependencies"))?;
        Ok(data.get("data").and_then(|d| d.as_array()).cloned().unwrap_or_default())
    }

    /// ModService.addModFromModio (ModMapper.fromModioResponse + ensureProfileModAssociation +
    /// upsert version/download/status). Metadata only; the file is downloaded by `install`.
    pub fn add_from_info(&self, db: &Db, info: &Value, profile_id: i64, folder_id: Option<i64>) -> Result<AddedOnline> {
        let s = |v: Option<&Value>| v.and_then(|x| x.as_str()).unwrap_or("").to_string();
        let platform_id = info.get("id").and_then(|v| v.as_i64()).context("mod.io response has no id")?;
        let raw_tags: Vec<String> = info
            .get("tags")
            .and_then(|t| t.as_array())
            .map(|a| a.iter().filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(String::from)).collect())
            .unwrap_or_default();
        let (tags, versions, approval) = parse_tags(&raw_tags);
        let name = s(info.get("name"));
        let name_id = s(info.get("name_id"));
        let url = s(info.get("profile_url"));
        let mf = info.get("modfile");
        let version = mf
            .and_then(|f| f.get("version").and_then(|v| v.as_str()).filter(|v| !v.is_empty()))
            .or_else(|| mf.and_then(|f| f.get("filename").and_then(|v| v.as_str())).filter(|v| !v.is_empty()))
            .unwrap_or("-")
            .to_string();
        let dl = mf.and_then(|f| f.pointer("/download/binary_url")).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let size = mf.and_then(|f| f.get("filesize")).and_then(|v| v.as_i64()).unwrap_or(0);
        let now = now_secs();
        let now_ms_v = now_ms();

        let mut existing = db.mod_id_by_platform(platform_id, "Modio")?;
        if existing.is_none() && !url.is_empty() {
            existing = db.mod_id_by_url(&url)?;
        }
        let created = existing.is_none();
        if let Some(mod_id) = existing {
            // HomeService.addModFromModioUrl: a known mod is only linked to the profile
            let (pm_id, added) = match db.profile_mod_id(profile_id, mod_id)? {
                Some(pm) => (pm, false),
                None => (db.add_profile_mod(profile_id, mod_id, folder_id)?, true),
            };
            return Ok(AddedOnline {
                status: if added { "added".into() } else { "exists".into() },
                mod_id,
                profile_mod_id: pm_id,
                created_mod: false,
                platform_id,
                name,
                version,
            });
        }
        db.conn.execute(
            "INSERT INTO mods (platform_id, game_id, name_id, display_name, original_name, url,
                               source_type, tags, approval_status, depend_mod_id)
             VALUES (?1, 1, ?2, ?3, ?3, ?4, 'Modio', ?5, ?6, 0)",
            params![platform_id, name_id, name, url, serde_json::to_string(&tags)?, approval],
        )?;
        let mod_id = db.conn.last_insert_rowid();
        // modsDAO.updateMod(...) with the same DTO fields
        db.conn.execute(
            "UPDATE mods SET platform_id = ?1, game_id = 1, name_id = ?2, display_name = ?3, original_name = ?3,
                    url = ?4, source_type = 'Modio', tags = ?5, approval_status = ?6, depend_mod_id = 0, updated_at = ?7
             WHERE mod_id = ?8",
            params![platform_id, name_id, name, url, serde_json::to_string(&tags)?, approval, now, mod_id],
        )?;
        let (pm_id, added) = match db.profile_mod_id(profile_id, mod_id)? {
            Some(pm) => (pm, false),
            None => (db.add_profile_mod(profile_id, mod_id, folder_id)?, true),
        };
        db.ensure_satellite_rows(mod_id)?;
        db.conn.execute(
            "UPDATE mod_versions SET current_version = ?1, available_versions = ?2, updated_at = ?3 WHERE mod_id = ?4",
            params![version, serde_json::to_string(&versions)?, now, mod_id],
        )?;
        db.conn.execute(
            "UPDATE mod_downloads SET download_url = ?1, cache_path = '', file_size = ?2, download_progress = 0,
                    download_status = 'pending', updated_at = ?3 WHERE mod_id = ?4",
            params![dl, size, now, mod_id],
        )?;
        db.conn.execute(
            "UPDATE mod_status SET last_update_date = ?1, online_update_date = ?1, is_online_available = 1,
                    is_local_not_found = 0, updated_at = ?2 WHERE mod_id = ?3",
            params![now_ms_v, now, mod_id],
        )?;
        Ok(AddedOnline {
            status: if added { "added".into() } else { "exists".into() },
            mod_id,
            profile_mod_id: pm_id,
            created_mod: created,
            platform_id,
            name,
            version,
        })
    }
}

/// ModUpdateService.needsOnlineModDownload
pub fn needs_download(m: &ProfileMod) -> bool {
    m.cache_path.is_empty()
        || !Path::new(&m.cache_path).exists()
        || m.online_update_date > m.last_update_date
        || m.download_progress != 100
}

/// Refresh metadata for the given online mods (the GUI's refreshModioMetadataBatch).
/// Returns the list of mod_ids that mod.io no longer returns (marked unavailable).
pub fn refresh_metadata(
    modio: &Modio,
    db: &Db,
    mods: &[ProfileMod],
    fetch_tags: bool,
    rep: &Reporter,
) -> Result<Vec<i64>> {
    let modio_mods: Vec<&ProfileMod> = mods.iter().filter(|m| m.source_type == "Modio").collect();
    if mods.iter().any(|m| m.source_type == "modcat") {
        rep.warn("ModCat mods are not supported by the CLI; they are left as-is");
    }
    let ids: Vec<i64> = modio_mods.iter().filter(|m| m.platform_id > 0).map(|m| m.platform_id).collect();
    rep.info(format!("refreshing mod.io metadata for {} mods", ids.len()));
    let infos = modio.mods_by_ids(&ids)?;
    let mut unavailable = Vec::new();
    for m in modio_mods {
        match infos.iter().find(|i| i.get("id").and_then(|v| v.as_i64()) == Some(m.platform_id)) {
            Some(info) => modio.apply_info(db, m, info, fetch_tags)?,
            None if m.platform_id > 0 => {
                modio.mark_unavailable(db, m.mod_id)?;
                unavailable.push(m.mod_id);
            }
            None => rep.warn(format!("{}: no mod.io id, skipped", m.display_name)),
        }
    }
    Ok(unavailable)
}
