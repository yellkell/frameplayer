//! A minimal OpenXR loader: find the active runtime manifest, load the runtime
//! library it names, and negotiate with it directly.
//!
//! This is what the Khronos loader does for the single-runtime, no-API-layer
//! case. Doing it here means no C++ loader to cross-compile or ship, and lets
//! the probe report exactly which manifest and library were used.
//!
//! Search order follows the OpenXR loader specification on Linux:
//! `$XR_RUNTIME_JSON`, then `$XDG_CONFIG_HOME` (default `~/.config`), then each
//! entry of `$XDG_CONFIG_DIRS` (default `/etc/xdg`), then `/etc`; in each,
//! `openxr/1/active_runtime.<arch>.json` before `openxr/1/active_runtime.json`.

use openxr::sys;
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct RuntimeManifest {
    pub manifest_path: String,
    pub canonical_manifest_path: String,
    pub name: Option<String>,
    pub library_path: String,
    pub resolved_library_path: String,
    pub is_steamvr: bool,
    pub file_format_version: Option<String>,
}

/// Environment the search depends on, injectable for tests.
pub struct SearchEnv {
    pub xr_runtime_json: Option<String>,
    pub xdg_config_home: Option<String>,
    pub home: Option<String>,
    pub xdg_config_dirs: Option<String>,
    pub arch: &'static str,
}

impl SearchEnv {
    pub fn from_process() -> Self {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        SearchEnv {
            xr_runtime_json: get("XR_RUNTIME_JSON"),
            xdg_config_home: get("XDG_CONFIG_HOME"),
            home: get("HOME"),
            xdg_config_dirs: get("XDG_CONFIG_DIRS"),
            arch: std::env::consts::ARCH,
        }
    }
}

/// Every candidate manifest path, in loader search order.
pub fn candidate_paths(env: &SearchEnv) -> Vec<PathBuf> {
    if let Some(p) = &env.xr_runtime_json {
        return vec![PathBuf::from(p)];
    }
    let mut dirs: Vec<PathBuf> = Vec::new();
    match (&env.xdg_config_home, &env.home) {
        (Some(c), _) => dirs.push(c.into()),
        (None, Some(h)) => dirs.push(Path::new(h).join(".config")),
        _ => {}
    }
    let config_dirs = env
        .xdg_config_dirs
        .clone()
        .unwrap_or_else(|| "/etc/xdg".into());
    dirs.extend(
        config_dirs
            .split(':')
            .filter(|d| !d.is_empty())
            .map(PathBuf::from),
    );
    dirs.push("/etc".into());

    let arch_name = format!("active_runtime.{}.json", env.arch);
    let mut out = Vec::new();
    for d in dirs {
        out.push(d.join("openxr/1").join(&arch_name));
        out.push(d.join("openxr/1/active_runtime.json"));
    }
    out
}

/// Parses a runtime manifest. `manifest_path` anchors a relative library path
/// at the manifest's real (symlink-resolved) directory, as the loader does.
pub fn parse_manifest(manifest_path: &Path, json: &str) -> Result<RuntimeManifest, String> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("invalid JSON: {e}"))?;
    let rt = v.get("runtime").ok_or("no \"runtime\" object")?;
    let library_path = rt
        .get("library_path")
        .and_then(|l| l.as_str())
        .filter(|l| !l.is_empty())
        .ok_or("no runtime.library_path")?
        .to_string();
    let canonical =
        std::fs::canonicalize(manifest_path).unwrap_or_else(|_| manifest_path.to_path_buf());
    let lib = Path::new(&library_path);
    let resolved = if lib.is_absolute() {
        lib.to_path_buf()
    } else if library_path.contains('/') {
        canonical.parent().unwrap_or(Path::new("/")).join(lib)
    } else {
        // A bare file name goes through the dynamic linker search path.
        lib.to_path_buf()
    };
    let resolved = std::fs::canonicalize(&resolved).unwrap_or(resolved);
    Ok(RuntimeManifest {
        manifest_path: manifest_path.display().to_string(),
        canonical_manifest_path: canonical.display().to_string(),
        name: rt.get("name").and_then(|n| n.as_str()).map(str::to_string),
        library_path,
        resolved_library_path: resolved.display().to_string(),
        is_steamvr: rt
            .get("VALVE_runtime_is_steamvr")
            .and_then(|b| b.as_bool())
            .unwrap_or(false),
        file_format_version: v
            .get("file_format_version")
            .and_then(|f| f.as_str())
            .map(str::to_string),
    })
}

/// Finds and parses the active runtime manifest.
pub fn find_active_runtime(env: &SearchEnv) -> Result<RuntimeManifest, String> {
    let candidates = candidate_paths(env);
    for path in &candidates {
        if let Ok(text) = std::fs::read_to_string(path) {
            return parse_manifest(path, &text).map_err(|e| format!("{}: {e}", path.display()));
        }
    }
    Err(format!(
        "no active OpenXR runtime manifest; searched: {}",
        candidates
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

type NegotiateFn = unsafe extern "system" fn(
    *const sys::NegotiateLoaderInfo,
    *mut sys::NegotiateRuntimeRequest,
) -> sys::Result;

pub struct LoadedRuntime {
    pub entry: openxr::Entry,
    pub runtime_interface_version: u32,
    pub runtime_api_version: String,
}

/// Loads the runtime library and performs loader/runtime negotiation.
///
/// The library stays loaded for the life of the process: runtimes start
/// threads and register atexit handlers that must not outlive their code.
pub fn load_runtime(manifest: &RuntimeManifest) -> Result<LoadedRuntime, String> {
    let lib = unsafe { libloading::Library::new(&manifest.resolved_library_path) }
        .map_err(|e| format!("dlopen {}: {e}", manifest.resolved_library_path))?;
    let negotiate: NegotiateFn = unsafe {
        *lib.get::<NegotiateFn>(b"xrNegotiateLoaderRuntimeInterface\0")
            .map_err(|e| format!("runtime has no xrNegotiateLoaderRuntimeInterface: {e}"))?
    };
    let info = sys::NegotiateLoaderInfo {
        struct_type: sys::LoaderInterfaceStructs::LOADER_INFO,
        struct_version: sys::LOADER_INFO_STRUCT_VERSION as u32,
        struct_size: std::mem::size_of::<sys::NegotiateLoaderInfo>(),
        min_interface_version: 1,
        max_interface_version: sys::CURRENT_LOADER_RUNTIME_VERSION as u32,
        min_api_version: sys::Version::new(1, 0, 0),
        max_api_version: sys::Version::new(1, 0x3ff, 0xfff),
    };
    let mut req = sys::NegotiateRuntimeRequest {
        struct_type: sys::LoaderInterfaceStructs::RUNTIME_REQUEST,
        struct_version: sys::RUNTIME_INFO_STRUCT_VERSION as u32,
        struct_size: std::mem::size_of::<sys::NegotiateRuntimeRequest>(),
        runtime_interface_version: 0,
        runtime_api_version: sys::Version::new(0, 0, 0),
        get_instance_proc_addr: None,
    };
    let r = unsafe { negotiate(&info, &mut req) };
    if r != sys::Result::SUCCESS {
        return Err(format!("xrNegotiateLoaderRuntimeInterface failed: {r:?}"));
    }
    let gipa = req
        .get_instance_proc_addr
        .ok_or("runtime returned no xrGetInstanceProcAddr")?;
    let entry = unsafe { openxr::Entry::from_get_instance_proc_addr(gipa, &()) }
        .map_err(|e| format!("resolving core entry points: {e}"))?;
    std::mem::forget(lib);
    let v = req.runtime_api_version;
    Ok(LoadedRuntime {
        entry,
        runtime_interface_version: req.runtime_interface_version,
        runtime_api_version: format!("{}.{}.{}", v.major(), v.minor(), v.patch()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> SearchEnv {
        SearchEnv {
            xr_runtime_json: None,
            xdg_config_home: None,
            home: Some("/home/deck".into()),
            xdg_config_dirs: None,
            arch: "aarch64",
        }
    }

    #[test]
    fn explicit_env_var_wins() {
        let e = SearchEnv {
            xr_runtime_json: Some("/x/rt.json".into()),
            ..env()
        };
        assert_eq!(candidate_paths(&e), vec![PathBuf::from("/x/rt.json")]);
    }

    #[test]
    fn default_search_order() {
        let got: Vec<String> = candidate_paths(&env())
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        assert_eq!(
            got,
            [
                "/home/deck/.config/openxr/1/active_runtime.aarch64.json",
                "/home/deck/.config/openxr/1/active_runtime.json",
                "/etc/xdg/openxr/1/active_runtime.aarch64.json",
                "/etc/xdg/openxr/1/active_runtime.json",
                "/etc/openxr/1/active_runtime.aarch64.json",
                "/etc/openxr/1/active_runtime.json",
            ]
        );
    }

    #[test]
    fn xdg_dirs_respected() {
        let e = SearchEnv {
            xdg_config_home: Some("/cfg".into()),
            xdg_config_dirs: Some("/a::/b".into()),
            ..env()
        };
        let got: Vec<String> = candidate_paths(&e)
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        assert_eq!(got[0], "/cfg/openxr/1/active_runtime.aarch64.json");
        assert_eq!(got[2], "/a/openxr/1/active_runtime.aarch64.json");
        assert_eq!(got[4], "/b/openxr/1/active_runtime.aarch64.json");
        assert_eq!(got.len(), 8);
    }

    #[test]
    fn parses_steamvr_manifest_with_relative_library() {
        let dir = std::env::temp_dir().join(format!("fp-xr-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("bin/linuxarm64")).unwrap();
        std::fs::write(dir.join("bin/linuxarm64/vrclient.so"), b"").unwrap();
        let manifest = dir.join("steamxr_linux64.json");
        let json = r#"{"file_format_version":"1.0.0","runtime":{"VALVE_runtime_is_steamvr":true,
            "library_path":"bin/linuxarm64/vrclient.so","name":"SteamVR/OpenXR"}}"#;
        std::fs::write(&manifest, json).unwrap();
        let link = dir.join("active_runtime.json");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&manifest, &link).unwrap();

        let m = parse_manifest(&link, json).unwrap();
        assert!(m.is_steamvr);
        assert_eq!(m.name.as_deref(), Some("SteamVR/OpenXR"));
        let real = std::fs::canonicalize(dir.join("bin/linuxarm64/vrclient.so")).unwrap();
        assert_eq!(m.resolved_library_path, real.display().to_string());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejects_bad_manifests() {
        let p = Path::new("/nonexistent/m.json");
        assert!(parse_manifest(p, "nope").is_err());
        assert!(parse_manifest(p, r#"{"runtime":{}}"#).is_err());
        assert!(parse_manifest(p, r#"{"runtime":{"library_path":""}}"#).is_err());
        let bare =
            parse_manifest(p, r#"{"runtime":{"library_path":"libopenxr_monado.so"}}"#).unwrap();
        assert_eq!(bare.resolved_library_path, "libopenxr_monado.so");
        assert!(!bare.is_steamvr);
    }
}
