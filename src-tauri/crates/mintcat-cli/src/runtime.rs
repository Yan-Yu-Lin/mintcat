//! Loads the same integrator runtime the GUI uses (`plugins/state.json` ->
//! `plugins/versions/<ver>/libmintcat_integrator.so` under the Rust app-data dir) and calls
//! its C ABI exactly like src-tauri/src/integrator/runtime.rs does.
//!
//! Note: the runtime's Oodle loader looks for `liboo2corelinux64.so.9` beside
//! `current_exe()`, i.e. beside the *CLI* binary (symlinks are resolved by the kernel).

use anyhow::{bail, Context, Result};
use libloading::Library;
use mintcat_integrator_api::{
    CheckForeignPaksRequest, CheckInstalledRequest, FindGamePakRequest, InstallEvent,
    InstallRequest, PathRequest, UninstallModsRequest,
};
use serde::{de::DeserializeOwned, Serialize};
use std::ffi::{c_char, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::ptr;

const ABI_VERSION: u32 = 1;

type ProgressCallback = unsafe extern "C" fn(event_json: *const c_char, user_data: *mut c_void);
type AbiVersionFn = unsafe extern "C" fn() -> u32;
type InstallFn = unsafe extern "C" fn(
    *const c_char,
    Option<ProgressCallback>,
    *mut c_void,
    *mut *mut c_char,
    *mut *mut c_char,
) -> i32;
type CommandFn = unsafe extern "C" fn(*const c_char, *mut *mut c_char, *mut *mut c_char) -> i32;
type FreeStringFn = unsafe extern "C" fn(*mut c_char);

pub struct Runtime {
    #[allow(dead_code)]
    pub path: PathBuf,
    lib: Library,
}

pub fn runtime_file_name() -> &'static str {
    "libmintcat_integrator.so"
}

/// Mirrors `active_downloaded_library_path` (no bundled-resource fallback: the Linux build
/// has no bundled runtime).
pub fn locate(data_dir: &Path, override_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = override_path {
        if p.is_file() {
            return Ok(p.to_path_buf());
        }
        bail!("integrator runtime not found at {:?}", p);
    }
    let state_path = data_dir.join("plugins").join("state.json");
    let state: serde_json::Value = std::fs::read_to_string(&state_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .with_context(|| format!("integrator runtime is not available (no {:?})", state_path))?;
    let version = state
        .get("activeVersion")
        .and_then(|v| v.as_str())
        .filter(|v| !v.is_empty() && *v != "bundled" && !v.contains('/') && !v.contains(".."))
        .context("integrator runtime is not available (no activeVersion)")?;
    let path = data_dir
        .join("plugins")
        .join("versions")
        .join(version)
        .join(runtime_file_name());
    if !path.is_file() {
        bail!("integrator runtime is not available: {:?} missing", path);
    }
    Ok(path)
}

struct Bridge<'a> {
    on_event: &'a mut dyn FnMut(InstallEvent),
}

unsafe extern "C" fn progress_callback(event_json: *const c_char, user_data: *mut c_void) {
    if event_json.is_null() || user_data.is_null() {
        return;
    }
    let bridge = &mut *(user_data as *mut Bridge<'_>);
    if let Some(ev) = CStr::from_ptr(event_json)
        .to_str()
        .ok()
        .and_then(|s| serde_json::from_str::<InstallEvent>(s).ok())
    {
        (bridge.on_event)(ev);
    }
}

impl Runtime {
    pub fn load(path: &Path) -> Result<Self> {
        let lib = unsafe { Library::new(path) }
            .with_context(|| format!("failed to load integrator runtime: {:?}", path))?;
        let abi = unsafe {
            *lib.get::<AbiVersionFn>(b"mintcat_integrator_abi_version")
                .context("missing mintcat_integrator_abi_version")?
        };
        let v = unsafe { abi() };
        if v != ABI_VERSION {
            bail!("integrator ABI mismatch (runtime {v}, expected {ABI_VERSION})");
        }
        Ok(Self {
            path: path.to_path_buf(),
            lib,
        })
    }

    fn free_fn(&self) -> Result<FreeStringFn> {
        Ok(unsafe {
            *self
                .lib
                .get::<FreeStringFn>(b"mintcat_integrator_free_string")
                .context("missing mintcat_integrator_free_string")?
        })
    }

    unsafe fn take(ptr: *mut c_char, free: FreeStringFn) -> Option<String> {
        if ptr.is_null() {
            return None;
        }
        let s = CStr::from_ptr(ptr).to_string_lossy().into_owned();
        free(ptr);
        Some(s)
    }

    fn command<Req: Serialize, Resp: DeserializeOwned>(&self, symbol: &[u8], req: &Req) -> Result<Resp> {
        let name = String::from_utf8_lossy(symbol).into_owned();
        let f = unsafe {
            *self
                .lib
                .get::<CommandFn>(symbol)
                .with_context(|| format!("missing {name}"))?
        };
        let free = self.free_fn()?;
        let req = CString::new(serde_json::to_string(req)?).context("request contains nul")?;
        let mut resp: *mut c_char = ptr::null_mut();
        let mut err: *mut c_char = ptr::null_mut();
        let code = unsafe { f(req.as_ptr(), &mut resp, &mut err) };
        let err = unsafe { Self::take(err, free) };
        let resp = unsafe { Self::take(resp, free) };
        if code != 0 {
            bail!(err.unwrap_or_else(|| format!("{name} failed with code {code}")));
        }
        let resp = resp.with_context(|| format!("{name} returned no response"))?;
        serde_json::from_str(&resp).with_context(|| format!("bad {name} response"))
    }

    pub fn install(&self, request: &InstallRequest, on_event: &mut dyn FnMut(InstallEvent)) -> Result<()> {
        let f = unsafe {
            *self
                .lib
                .get::<InstallFn>(b"mintcat_integrator_install")
                .context("missing mintcat_integrator_install")?
        };
        let free = self.free_fn()?;
        let req = CString::new(serde_json::to_string(request)?).context("request contains nul")?;
        let mut bridge = Bridge { on_event };
        let mut resp: *mut c_char = ptr::null_mut();
        let mut err: *mut c_char = ptr::null_mut();
        let code = unsafe {
            f(
                req.as_ptr(),
                Some(progress_callback),
                &mut bridge as *mut Bridge<'_> as *mut c_void,
                &mut resp,
                &mut err,
            )
        };
        let err = unsafe { Self::take(err, free) };
        let _ = unsafe { Self::take(resp, free) };
        if code != 0 {
            bail!(err.unwrap_or_else(|| format!("integrator failed with code {code}")));
        }
        Ok(())
    }

    pub fn uninstall(&self, game_path: &str, is_delete_ue4ss: bool) -> Result<bool> {
        self.command(
            b"mintcat_integrator_uninstall_mods",
            &UninstallModsRequest { game_path: game_path.into(), is_delete_ue4ss },
        )
    }

    pub fn check_installed(&self, game_path: &str) -> Result<String> {
        self.command(
            b"mintcat_integrator_check_installed",
            &CheckInstalledRequest { game_path: game_path.into(), install_time: 0 },
        )
    }

    pub fn foreign_paks(&self, game_path: &str) -> Result<Vec<String>> {
        self.command(
            b"mintcat_integrator_check_foreign_paks",
            &CheckForeignPaksRequest { game_path: game_path.into() },
        )
    }

    pub fn find_game_pak(&self, game: &str) -> Result<String> {
        self.command(
            b"mintcat_integrator_find_game_pak",
            &FindGamePakRequest { game_name: Some(game.into()) },
        )
    }

    /// `is_valid_unpacked_mod` (GUI calls it for every cache path; true for mod *directories*).
    pub fn is_valid_unpacked_mod(&self, path: &str) -> bool {
        if !Path::new(path).is_dir() {
            return false; // GUI: stat().isDirectory check first
        }
        self.command::<_, bool>(
            b"mintcat_integrator_is_valid_unpacked_mod",
            &PathRequest { path: path.into() },
        )
        .unwrap_or(false)
    }
}
