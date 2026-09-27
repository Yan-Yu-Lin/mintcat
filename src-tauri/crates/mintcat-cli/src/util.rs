//! Small helpers shared by the CLI: paths, file stats, JS-compatible hashing.

use anyhow::{Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// JS side: `configDir() + com.mint.cat` (see src/storage/db/Client.ts).
pub fn default_config_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"));
    base.join("com.mint.cat")
}

/// Rust side: `app_data_dir()` (plugins/state.json lives here).
pub fn default_data_dir() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/share"));
    base.join("com.mint.cat")
}

pub fn default_cache_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".cache"));
    base.join("com.mint.cat")
}

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Local timestamp string like 20260927-203512 (uses `date` so it honours the local TZ).
pub fn stamp() -> String {
    std::process::Command::new("date")
        .arg("+%Y%m%d-%H%M%S")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| now_secs().to_string())
}

/// Same numbers the GUI gets from tauri-plugin-fs `stat`: size = metadata.len(),
/// mtime = modified() in milliseconds. Missing path -> (0, 0).
pub fn size_and_mtime(path: &str) -> (u64, i64) {
    if path.is_empty() {
        return (0, 0);
    }
    match std::fs::metadata(path) {
        Ok(meta) => {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            (meta.len(), mtime)
        }
        Err(_) => (0, 0),
    }
}

pub fn path_exists(path: &str) -> bool {
    !path.is_empty() && Path::new(path).exists()
}

/// `md5(JSON.stringify(value))` exactly as src/utils/CryptApi.ts computes it.
pub fn js_md5_of_json<T: Serialize>(value: &T) -> Result<String> {
    let json = serde_json::to_string(value).context("failed to serialize manifest")?;
    Ok(gui_md5(&json))
}

/// Bit-exact port of the GUI's hand-written `md5()` (src/utils/CryptApi.ts). It is NOT
/// standard MD5: message words that were never assigned are `undefined` in JS, so
/// `(a + f + K[j] + m[i+g]) | 0` becomes `NaN | 0 = 0` for them; the length's high word
/// is never written; and each state word is printed big-endian. The GUI stores this value
/// in settings `profile_<id>_installHash` / `game_<id>_installedHash`, so the CLI must
/// reproduce it to stay compatible with the GUI's "nothing changed" check.
pub fn gui_md5(input: &str) -> String {
    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
        0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
        0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
        0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
        0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
        0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
        0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
    ];
    const R: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9,
        14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15,
        21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    let bytes = input.as_bytes();
    let len = bytes.len();
    // JS sparse array: None == undefined
    let last = ((len + 8) >> 6) * 16 + 14;
    let mut m: Vec<Option<u32>> = vec![None; last + 1];
    for (i, b) in bytes.iter().enumerate() {
        let w = m[i >> 2].unwrap_or(0);
        m[i >> 2] = Some(w | ((*b as u32) << ((i % 4) * 8)));
    }
    let w = m[len >> 2].unwrap_or(0);
    m[len >> 2] = Some(w | (0x80u32 << ((len % 4) * 8)));
    m[last] = Some((len as u64 * 8) as u32);

    let (mut a, mut b, mut c, mut d) = (0x67452301u32, 0xefcdab89u32, 0x98badcfeu32, 0x10325476u32);
    let mut i = 0;
    while i < m.len() {
        let (aa, bb, cc, dd) = (a, b, c, d);
        for j in 0..64 {
            let (f, g) = if j < 16 {
                ((b & c) | (!b & d), j)
            } else if j < 32 {
                ((d & b) | (!d & c), (5 * j + 1) % 16)
            } else if j < 48 {
                (b ^ c ^ d, (3 * j + 5) % 16)
            } else {
                (c ^ (b | !d), (7 * j) % 16)
            };
            let sum = match m.get(i + g).copied().flatten() {
                Some(word) => a.wrapping_add(f).wrapping_add(K[j]).wrapping_add(word),
                None => 0, // NaN | 0
            };
            let temp = d;
            d = c;
            c = b;
            b = b.wrapping_add(sum.rotate_left(R[j]));
            a = temp;
        }
        a = a.wrapping_add(aa);
        b = b.wrapping_add(bb);
        c = c.wrapping_add(cc);
        d = d.wrapping_add(dd);
        i += 16;
    }
    format!("{a:08x}{b:08x}{c:08x}{d:08x}")
}

/// Mirror of `sanitize_dir_name` in mintcat-integrator-core/src/common/ue4ss.rs,
/// used to predict which `ue4ss/mods/<dir>` a mod installs into.
pub fn sanitize_dir_name(input: &str) -> String {
    let mut s: String = input
        .chars()
        .map(|c| {
            if c.is_control()
                || std::path::is_separator(c)
                || matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|')
            {
                '_'
            } else {
                c
            }
        })
        .collect();
    s = s.trim().trim_matches('.').to_string();
    if s.is_empty() {
        s = "mod".to_string();
    }
    let base = s.split('.').next().unwrap_or(&s);
    let upper = base.to_ascii_uppercase();
    let reserved = matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || upper
            .strip_prefix("COM")
            .and_then(|n| n.parse::<u8>().ok())
            .is_some_and(|n| (1..=9).contains(&n))
        || upper
            .strip_prefix("LPT")
            .and_then(|n| n.parse::<u8>().ok())
            .is_some_and(|n| (1..=9).contains(&n));
    if reserved {
        format!("_{}", s)
    } else {
        s
    }
}

/// CacheApi.sanitizeFileName: strip \ / : * ? " < > |
pub fn sanitize_cache_file_name(name: &str) -> String {
    name.chars()
        .filter(|c| !matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
        .collect()
}

/// Process scan over /proc: returns true when any process cmdline contains one of `needles`.
pub fn process_running(needles: &[&str]) -> Vec<(u32, String)> {
    let mut hits = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return hits;
    };
    let me = std::process::id();
    for entry in entries.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        if pid == me {
            continue;
        }
        let Ok(raw) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let cmd = String::from_utf8_lossy(&raw).replace('\0', " ");
        if needles.iter().any(|n| cmd.contains(n)) {
            hits.push((pid, cmd.trim().to_string()));
        }
    }
    hits
}

/// PIDs whose executable basename is exactly `mintcat` (the GUI), not this CLI.
pub fn gui_pids() -> Vec<u32> {
    let mut pids = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return pids;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        if let Ok(exe) = std::fs::read_link(entry.path().join("exe")) {
            if exe.file_name().and_then(|n| n.to_str()) == Some("mintcat") {
                pids.push(pid);
            }
        }
    }
    pids
}

#[cfg(test)]
mod tests {
    /// Expected values produced by running src/utils/CryptApi.ts `md5()` under node.
    #[test]
    fn gui_md5_matches_frontend() {
        assert_eq!(super::gui_md5(""), "c2ca64284b52ecb0f4401e256bb7959d");
        assert_eq!(super::gui_md5("abc"), "6f6436f4f7ecbf7ca0d9f0f118516869");
        assert_eq!(super::gui_md5("新的默認 profile 😀"), "24955eb9f5bb2d829ea85ef7cd82902e");
        let long = "x".repeat(1000) + "默";
        assert_eq!(super::gui_md5(&long), "741d1e70e6c2f00a01dc0c1b6ea199ea");
    }

    /// Optional extra vectors: GUI_MD5_VECTORS="<hash>\t<input>" lines.
    #[test]
    fn gui_md5_env_vectors() {
        for line in std::env::var("GUI_MD5_VECTORS").unwrap_or_default().lines() {
            let (hash, input) = line.split_once('\t').unwrap();
            assert_eq!(super::gui_md5(input), hash, "input {input:?}");
        }
    }
}
