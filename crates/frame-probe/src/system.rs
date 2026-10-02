//! Operating system, C runtime, user and environment facts.

use crate::util::{cstr_ptr, read_trimmed};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Serialize, Default)]
pub struct SystemReport {
    pub os_release: BTreeMap<String, String>,
    pub kernel: String,
    pub machine: String,
    pub glibc: Option<String>,
    pub cpu_model: Option<String>,
    pub cpu_count: usize,
    pub mem_total_mib: Option<u64>,
    pub uid: u32,
    pub groups: Vec<String>,
    /// Environment variables that reveal how and where we were launched.
    pub env: BTreeMap<String, String>,
    /// Hints that we run inside a container (Steam Linux Runtime, Flatpak).
    pub container_hints: Vec<String>,
    /// Removable and home storage with free space, for the media library.
    pub storage: Vec<Mount>,
}

#[derive(Serialize)]
pub struct Mount {
    pub mount_point: String,
    pub fs_type: String,
    pub free_gib: Option<f64>,
    pub total_gib: Option<f64>,
}

const ENV_KEYS: &[&str] = &[
    "XDG_SESSION_TYPE",
    "XDG_CURRENT_DESKTOP",
    "XDG_RUNTIME_DIR",
    "XDG_CONFIG_HOME",
    "XDG_CONFIG_DIRS",
    "XDG_DATA_DIRS",
    "WAYLAND_DISPLAY",
    "DISPLAY",
    "HOME",
    "XR_RUNTIME_JSON",
    "VK_ICD_FILENAMES",
    "VK_DRIVER_FILES",
    "SteamAppId",
    "SteamGameId",
    "STEAM_COMPAT_APP_ID",
    "SteamDeck",
    "STEAM_RUNTIME",
    "PRESSURE_VESSEL_RUNTIME",
    "container",
    "LD_LIBRARY_PATH",
    "LANG",
];

pub fn probe() -> SystemReport {
    let mut r = SystemReport::default();

    if let Ok(text) = std::fs::read_to_string("/etc/os-release") {
        r.os_release = parse_os_release(&text);
    }

    unsafe {
        let mut u: libc::utsname = std::mem::zeroed();
        if libc::uname(&mut u) == 0 {
            r.kernel = format!(
                "{} {}",
                crate::util::fixed_cstr(&u.sysname),
                crate::util::fixed_cstr(&u.release)
            );
            r.machine = crate::util::fixed_cstr(&u.machine);
        }
        r.glibc = cstr_ptr(libc::gnu_get_libc_version());
        r.uid = libc::getuid();
    }

    if let Ok(cpuinfo) = std::fs::read_to_string("/proc/cpuinfo") {
        r.cpu_model = cpuinfo
            .lines()
            .find(|l| l.starts_with("model name") || l.starts_with("Hardware"))
            .and_then(|l| l.split_once(':'))
            .map(|(_, v)| v.trim().to_string());
    }
    r.cpu_count = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0);
    r.mem_total_mib = std::fs::read_to_string("/proc/meminfo").ok().and_then(|m| {
        m.lines()
            .find(|l| l.starts_with("MemTotal:"))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|kib| kib.parse::<u64>().ok())
            .map(|kib| kib / 1024)
    });

    r.groups = group_names();

    for key in ENV_KEYS {
        if let Ok(v) = std::env::var(key) {
            r.env.insert((*key).to_string(), v);
        }
    }

    for (path, hint) in [
        (
            "/run/pressure-vessel",
            "Steam Linux Runtime container (pressure-vessel)",
        ),
        ("/.flatpak-info", "Flatpak sandbox"),
        (
            "/run/host/os-release",
            "container with host OS mounted at /run/host",
        ),
    ] {
        if std::path::Path::new(path).exists() {
            r.container_hints.push(hint.to_string());
        }
    }

    r.storage = mounts();
    r
}

pub fn parse_os_release(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|l| l.split_once('='))
        .filter(|(k, _)| !k.trim().is_empty() && !k.starts_with('#'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().trim_matches('"').to_string()))
        .collect()
}

fn group_names() -> Vec<String> {
    let mut gids = vec![0 as libc::gid_t; 256];
    let n = unsafe { libc::getgroups(gids.len() as i32, gids.as_mut_ptr()) };
    if n < 0 {
        return Vec::new();
    }
    gids.truncate(n as usize);
    let table = std::fs::read_to_string("/etc/group").unwrap_or_default();
    gids.iter()
        .map(|gid| {
            table
                .lines()
                .find_map(|l| {
                    let mut f = l.split(':');
                    let name = f.next()?;
                    let id = f.nth(1)?.parse::<u32>().ok()?;
                    (id == *gid).then(|| name.to_string())
                })
                .unwrap_or_else(|| gid.to_string())
        })
        .collect()
}

fn mounts() -> Vec<Mount> {
    let Some(text) = read_trimmed("/proc/mounts") else {
        return Vec::new();
    };
    let home = std::env::var("HOME").unwrap_or_default();
    let mut out = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 3 {
            continue;
        }
        let mp = f[1].replace("\\040", " ");
        let interesting = mp == "/"
            || mp == "/home"
            || (!home.is_empty() && mp == home)
            || mp.starts_with("/run/media/")
            || mp.starts_with("/media/")
            || mp.starts_with("/mnt/");
        if !interesting {
            continue;
        }
        let (free, total) = statvfs_gib(&mp);
        out.push(Mount {
            mount_point: mp,
            fs_type: f[2].to_string(),
            free_gib: free,
            total_gib: total,
        });
    }
    out
}

fn statvfs_gib(path: &str) -> (Option<f64>, Option<f64>) {
    let Ok(c) = std::ffi::CString::new(path) else {
        return (None, None);
    };
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return (None, None);
    }
    let gib = |blocks: u64| (blocks as f64 * s.f_frsize as f64) / (1u64 << 30) as f64;
    (Some(gib(s.f_bavail as u64)), Some(gib(s.f_blocks as u64)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_os_release() {
        let m = parse_os_release("NAME=\"SteamOS\"\nID=steamos\n# c\nVERSION_ID=3.8\n\n");
        assert_eq!(m["NAME"], "SteamOS");
        assert_eq!(m["ID"], "steamos");
        assert_eq!(m["VERSION_ID"], "3.8");
        assert_eq!(m.len(), 3);
    }
}
