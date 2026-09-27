//! Port of src/services/InternalAssetService.ts `ensureInternalAssets`:
//! keeps UE4SSL.zip / DRG.zip / RC.zip in the cache dir in sync with MintCat's
//! update.json, tracked by `assets_manifest.json` (same file the GUI uses).

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::report::Reporter;

const MANIFEST_HZ: &str = "https://yuri-oss-hz.oss-cn-hangzhou.aliyuncs.com/update.json";
const MANIFEST_SG: &str = "https://yuri-oss-sg.oss-ap-southeast-1.aliyuncs.com/update.json";
const MANIFEST_FILE: &str = "assets_manifest.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AssetManifest {
    pub ue4ssl: String,
    pub drg: String,
    pub rc: String,
    pub channel: String,
}

impl Default for AssetManifest {
    fn default() -> Self {
        Self {
            ue4ssl: "0".into(),
            drg: "0".into(),
            rc: "0".into(),
            channel: "stable".into(),
        }
    }
}

impl AssetManifest {
    fn get(&self, key: &str) -> &str {
        match key {
            "ue4ssl" => &self.ue4ssl,
            "drg" => &self.drg,
            _ => &self.rc,
        }
    }
    fn set(&mut self, key: &str, v: String) {
        match key {
            "ue4ssl" => self.ue4ssl = v,
            "drg" => self.drg = v,
            _ => self.rc = v,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct AssetPaths {
    pub ue4ss_zip: Option<PathBuf>,
    pub drg_zip: Option<PathBuf>,
    pub rc_zip: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AssetAction {
    pub asset: String,
    pub path: PathBuf,
    pub local_version: String,
    pub latest_version: Option<String>,
    pub action: String, // "up_to_date" | "download" | "would_download"
}

pub struct AssetOptions<'a> {
    pub cache_dir: &'a Path,
    pub channel: &'a str,
    pub language: &'a str,
    pub game: &'a str, // "drg" | "rc"
    pub include_ue4ss: bool,
    pub dry_run: bool,
    pub offline: bool,
}

fn read_manifest(cache_dir: &Path) -> (AssetManifest, bool) {
    let path = cache_dir.join(MANIFEST_FILE);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return (AssetManifest::default(), false);
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return (AssetManifest::default(), false);
    };
    let s = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .unwrap_or("0")
            .to_string()
    };
    let channel = v
        .get("channel")
        .and_then(|x| x.as_str())
        .filter(|c| ["stable", "beta", "alpha"].contains(c))
        .unwrap_or("stable")
        .to_string();
    (
        AssetManifest {
            ue4ssl: s("ue4ssl"),
            drg: s("drg"),
            rc: s("rc"),
            channel,
        },
        true,
    )
}

fn write_manifest(cache_dir: &Path, m: &AssetManifest) -> Result<()> {
    // Key order matches the GUI's JSON.stringify({...current, ...updates}).
    let text = serde_json::to_string(m)?;
    std::fs::write(cache_dir.join(MANIFEST_FILE), text)?;
    Ok(())
}

pub fn is_valid_zip(path: &Path) -> bool {
    std::fs::File::open(path)
        .ok()
        .map(|f| zip::ZipArchive::new(f).is_ok())
        .unwrap_or(false)
}

/// Same comparison as release.ts compareVersion.
fn compare_version(a: &str, b: &str) -> i64 {
    let parse = |s: &str| -> Vec<i64> {
        s.split(|c| c == '.' || c == '-')
            .map(|p| {
                let digits: String = p.chars().take_while(|c| c.is_ascii_digit()).collect();
                digits.parse::<i64>().unwrap_or(0)
            })
            .collect()
    };
    let (x, y) = (parse(a), parse(b));
    let len = x.len().max(y.len()).max(3);
    for i in 0..len {
        let d = x.get(i).copied().unwrap_or(0) - y.get(i).copied().unwrap_or(0);
        if d != 0 {
            return d;
        }
    }
    0
}

pub fn manifest_url(language: &str) -> &'static str {
    if language.starts_with("zh") {
        MANIFEST_HZ
    } else {
        MANIFEST_SG
    }
}

fn http() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(600))
        .build()?)
}

fn fetch_update_manifest(url: &str) -> Result<Vec<Value>> {
    let resp = http()?
        .get(url)
        .header("Accept", "application/json")
        .send()
        .with_context(|| format!("failed to fetch {url}"))?;
    if !resp.status().is_success() {
        bail!("update manifest request failed: {}", resp.status());
    }
    match resp.json::<Value>()? {
        Value::Array(items) => Ok(items),
        _ => bail!("update manifest array required"),
    }
}

fn download_verified(url: &str, dest: &Path, md5_hex: &str) -> Result<()> {
    let mut last = String::new();
    for attempt in 1..=3 {
        let result = (|| -> Result<()> {
            let resp = http()?.get(url).send()?;
            if !resp.status().is_success() {
                bail!("HTTP {}", resp.status());
            }
            let bytes = resp.bytes()?;
            let got = format!("{:x}", md5::compute(&bytes));
            if !got.eq_ignore_ascii_case(md5_hex.trim()) {
                bail!("md5 mismatch: expected {md5_hex}, got {got}");
            }
            let tmp = dest.with_extension("zip.part");
            std::fs::write(&tmp, &bytes)?;
            std::fs::rename(&tmp, dest)?;
            if !is_valid_zip(dest) {
                bail!("downloaded file is not a valid zip");
            }
            Ok(())
        })();
        match result {
            Ok(()) => return Ok(()),
            Err(e) => {
                last = format!("{e:#}");
                let _ = std::fs::remove_file(dest);
                if attempt < 3 {
                    std::thread::sleep(Duration::from_millis(1200 * attempt));
                }
            }
        }
    }
    bail!("failed to download {url} after 3 attempts: {last}")
}

fn resolve_url(item: &Value, manifest_url: &str) -> Option<String> {
    let raw = ["downloadUrl", "url", "path"]
        .iter()
        .find_map(|k| item.get(*k).and_then(|v| v.as_str()))
        .filter(|s| !s.trim().is_empty())?;
    if raw.starts_with("http://") || raw.starts_with("https://") {
        return Some(raw.to_string());
    }
    let base = &manifest_url[..manifest_url.rfind('/').map(|i| i + 1).unwrap_or(0)];
    Some(format!("{base}{}", raw.trim_start_matches('/')))
}

pub fn ensure_internal_assets(opts: &AssetOptions, rep: &Reporter) -> Result<(AssetPaths, Vec<AssetAction>)> {
    let cache = opts.cache_dir;
    std::fs::create_dir_all(cache)?;
    let ue4ss_zip = cache.join("UE4SSL.zip");
    let second_key = if opts.game == "rc" { "rc" } else { "drg" };
    let second_zip = cache.join(if opts.game == "rc" { "RC.zip" } else { "DRG.zip" });

    let (mut manifest, valid) = read_manifest(cache);
    let mut force = !valid;
    if valid && manifest.channel != opts.channel {
        // GUI: channel switch -> delete all three zips and reset manifest
        if !opts.dry_run {
            for n in ["UE4SSL.zip", "DRG.zip", "RC.zip"] {
                let _ = std::fs::remove_file(cache.join(n));
            }
            manifest = AssetManifest {
                channel: opts.channel.to_string(),
                ..Default::default()
            };
            write_manifest(cache, &manifest)?;
        }
        force = true;
    }

    let ue4ss_valid = !opts.include_ue4ss || (!force && is_valid_zip(&ue4ss_zip));
    let second_valid = !force && is_valid_zip(&second_zip);
    if !opts.dry_run {
        let mut changed = false;
        if opts.include_ue4ss && !force && !ue4ss_valid && manifest.ue4ssl != "0" {
            if ue4ss_zip.exists() {
                let _ = std::fs::remove_file(&ue4ss_zip);
            }
            manifest.ue4ssl = "0".into();
            changed = true;
        }
        if !force && !second_valid && manifest.get(second_key) != "0" {
            if second_zip.exists() {
                let _ = std::fs::remove_file(&second_zip);
            }
            manifest.set(second_key, "0".into());
            changed = true;
        }
        if changed {
            write_manifest(cache, &manifest)?;
        }
    }

    let mut wanted: Vec<(&str, PathBuf, String, bool)> = Vec::new(); // key, path, localVersion, valid
    if opts.include_ue4ss {
        let v = if force || !ue4ss_valid { "0".to_string() } else { manifest.ue4ssl.clone() };
        wanted.push(("ue4ssl", ue4ss_zip.clone(), v, ue4ss_valid));
    }
    let v = if force || !second_valid { "0".to_string() } else { manifest.get(second_key).to_string() };
    wanted.push((second_key, second_zip.clone(), v, second_valid));

    let mut actions = Vec::new();
    if opts.offline {
        for (key, path, local, ok) in &wanted {
            if !ok {
                bail!("{} missing or invalid at {:?} and --offline was given", key, path);
            }
            actions.push(AssetAction {
                asset: key.to_string(),
                path: path.clone(),
                local_version: local.clone(),
                latest_version: None,
                action: "up_to_date".into(),
            });
        }
    } else {
        let url = manifest_url(opts.language);
        rep.info(format!("checking internal assets against {url}"));
        let items = fetch_update_manifest(url).context("error.checkInternalAssetsUpdates")?;
        for (key, path, local, ok) in &wanted {
            let row = items.iter().find(|row| {
                let ch = row.get("channel").and_then(|v| v.as_str()).unwrap_or("stable").to_lowercase();
                let name = row.get("name").and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
                let ty = row.get("type").and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
                ch == opts.channel && (name == *key || ty == *key)
            });
            let latest = row.and_then(|r| r.get("latestVersion")).and_then(|v| v.as_str()).map(String::from);
            let md5 = row.and_then(|r| r.get("md5")).and_then(|v| v.as_str()).map(String::from);
            let has_update = latest.as_deref().map(|l| compare_version(l, local) > 0).unwrap_or(false);
            let need = force || !ok || (has_update && latest.is_some() && md5.is_some());
            let mut action = AssetAction {
                asset: key.to_string(),
                path: path.clone(),
                local_version: local.clone(),
                latest_version: latest.clone(),
                action: "up_to_date".into(),
            };
            if need {
                let (Some(latest), Some(md5), Some(dl)) = (latest.clone(), md5, row.and_then(|r| resolve_url(r, url))) else {
                    bail!("mod.missingVersionOrMd5: {key}");
                };
                if opts.dry_run {
                    action.action = "would_download".into();
                } else {
                    rep.info(format!("downloading {key} {latest} -> {:?}", path));
                    download_verified(&dl, path, &md5)?;
                    let (mut m, _) = read_manifest(cache);
                    m.set(key, latest);
                    m.channel = opts.channel.to_string();
                    write_manifest(cache, &m)?;
                    action.action = "download".into();
                }
            }
            actions.push(action);
        }
    }

    if !opts.dry_run {
        if opts.include_ue4ss && !is_valid_zip(&ue4ss_zip) {
            bail!("internalAssets.missingAfterCheck: UE4SSL.zip");
        }
        if !is_valid_zip(&second_zip) {
            bail!("internalAssets.missingAfterCheck: {:?}", second_zip.file_name().unwrap());
        }
    }

    let ue4ss = opts.include_ue4ss.then(|| ue4ss_zip.clone());
    let paths = if opts.game == "rc" {
        AssetPaths { ue4ss_zip: ue4ss, drg_zip: None, rc_zip: Some(second_zip) }
    } else {
        AssetPaths { ue4ss_zip: ue4ss, drg_zip: Some(second_zip), rc_zip: None }
    };
    Ok((paths, actions))
}
