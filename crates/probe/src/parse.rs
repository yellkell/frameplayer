//! Pure parsers and classifiers (unit-tested without hardware).

use serde::Serialize;
use std::cmp::Ordering;
use std::collections::BTreeMap;

/// `/etc/os-release` (shell-style `KEY=value`, optional quotes/escapes).
pub fn parse_os_release(s: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in s.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let k = k.trim();
        if k.is_empty() || !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        out.insert(k.to_string(), unquote(v.trim()));
    }
    out
}

fn unquote(v: &str) -> String {
    let q = v.chars().next();
    if matches!(q, Some('"') | Some('\'')) && v.len() >= 2 && v.ends_with(q.unwrap()) {
        let inner = &v[1..v.len() - 1];
        if q == Some('\'') {
            return inner.to_string();
        }
        let mut out = String::new();
        let mut it = inner.chars();
        while let Some(c) = it.next() {
            if c == '\\' {
                if let Some(n) = it.next() {
                    out.push(n);
                }
            } else {
                out.push(c);
            }
        }
        return out;
    }
    v.to_string()
}

/// `/proc/meminfo` → key → kB.
pub fn parse_meminfo(s: &str) -> BTreeMap<String, u64> {
    s.lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            let n = v.split_whitespace().next()?.parse().ok()?;
            Some((k.trim().to_string(), n))
        })
        .collect()
}

/// What `/proc/cpuinfo` says about the CPU.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CpuInfo {
    pub processors: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hardware: Option<String>,
    /// Distinct `implementer:part` pairs (ARM), e.g. `0x41:0xd82`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub arm_parts: Vec<String>,
    /// Feature flags of the first processor (ARM `Features` / x86 `flags`),
    /// limited to ones relevant for media code.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
}

const INTERESTING_FEATURES: &[&str] = &[
    "asimd", "fphp", "asimdhp", "asimddp", "sve", "sve2", "i8mm", "bf16", "lrcpc", "avx2",
    "avx512f",
];

pub fn parse_cpuinfo(s: &str) -> CpuInfo {
    let mut c = CpuInfo::default();
    let mut implementer = String::new();
    let mut seen_features = false;
    for line in s.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        match k {
            "processor" => c.processors += 1,
            "model name" if c.model_name.is_none() => c.model_name = Some(v.into()),
            "Hardware" if c.hardware.is_none() => c.hardware = Some(v.into()),
            "CPU implementer" => implementer = v.into(),
            "CPU part" => {
                let p = format!("{implementer}:{v}");
                if !c.arm_parts.contains(&p) {
                    c.arm_parts.push(p);
                }
            }
            "Features" | "flags" if !seen_features => {
                seen_features = true;
                c.features = v
                    .split_whitespace()
                    .filter(|f| INTERESTING_FEATURES.contains(f))
                    .map(String::from)
                    .collect();
            }
            _ => {}
        }
    }
    c
}

/// One `/etc/group` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupEntry {
    pub name: String,
    pub gid: u32,
    pub members: Vec<String>,
}

pub fn parse_group_file(s: &str) -> Vec<GroupEntry> {
    s.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            if f.len() < 3 {
                return None;
            }
            Some(GroupEntry {
                name: f[0].to_string(),
                gid: f[2].parse().ok()?,
                members: f
                    .get(3)
                    .map(|m| {
                        m.split(',')
                            .filter(|x| !x.is_empty())
                            .map(String::from)
                            .collect()
                    })
                    .unwrap_or_default(),
            })
        })
        .collect()
}

/// Login name for `uid` from `/etc/passwd` content.
pub fn passwd_name(s: &str, uid: u32) -> Option<String> {
    s.lines().find_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        (f.len() >= 3 && f[2].parse::<u32>().ok() == Some(uid)).then(|| f[0].to_string())
    })
}

/// `ls -l` style mode string (`crw-rw----`).
pub fn mode_string(mode: u32) -> String {
    let kind = match mode & libc::S_IFMT {
        libc::S_IFCHR => 'c',
        libc::S_IFBLK => 'b',
        libc::S_IFDIR => 'd',
        libc::S_IFLNK => 'l',
        libc::S_IFSOCK => 's',
        libc::S_IFIFO => 'p',
        _ => '-',
    };
    let mut s = String::with_capacity(10);
    s.push(kind);
    for shift in [6u32, 3, 0] {
        let b = (mode >> shift) & 7;
        s.push(if b & 4 != 0 { 'r' } else { '-' });
        s.push(if b & 2 != 0 { 'w' } else { '-' });
        s.push(if b & 1 != 0 { 'x' } else { '-' });
    }
    s
}

/// Compare strings with embedded numbers numerically (`video2 < video10`).
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut ai, mut bi) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let mut na = String::new();
                while let Some(c) = ai.peek().copied().filter(char::is_ascii_digit) {
                    na.push(c);
                    ai.next();
                }
                let mut nb = String::new();
                while let Some(c) = bi.peek().copied().filter(char::is_ascii_digit) {
                    nb.push(c);
                    bi.next();
                }
                let o = na
                    .trim_start_matches('0')
                    .len()
                    .cmp(&nb.trim_start_matches('0').len())
                    .then_with(|| na.trim_start_matches('0').cmp(nb.trim_start_matches('0')));
                if o != Ordering::Equal {
                    return o;
                }
            }
            (Some(x), Some(y)) => {
                if x != y {
                    return x.cmp(&y);
                }
                ai.next();
                bi.next();
            }
        }
    }
}

/// Path of the first mapped file in `/proc/self/maps` whose name starts
/// with `base` (e.g. `libvulkan`).
pub fn mapped_library(maps: &str, base: &str) -> Option<String> {
    maps.lines().find_map(|l| {
        let path = l.split_whitespace().nth(5)?;
        let file = path.rsplit('/').next()?;
        (file.starts_with(base) && file[base.len()..].starts_with(".so")).then(|| path.to_string())
    })
}

/// Highest `<prefix><version>` string found in binary data (e.g. the
/// newest `GLIBCXX_3.4.N` a libstdc++ provides).
pub fn max_symbol_version(data: &[u8], prefix: &str) -> Option<String> {
    let p = prefix.as_bytes();
    let mut best: Option<String> = None;
    let mut i = 0;
    while i + p.len() <= data.len() {
        if &data[i..i + p.len()] == p {
            let mut j = i + p.len();
            while j < data.len() && (data[j].is_ascii_digit() || data[j] == b'.') {
                j += 1;
            }
            let v = String::from_utf8_lossy(&data[i + p.len()..j])
                .trim_end_matches('.')
                .to_string();
            if !v.is_empty()
                && best
                    .as_deref()
                    .is_none_or(|b| version_cmp(&v, b) == Ordering::Greater)
            {
                best = Some(v);
            }
            i = j;
        } else {
            i += 1;
        }
    }
    best.map(|v| format!("{prefix}{v}"))
}

/// Dotted numeric version comparison (`2.10 > 2.9`).
pub fn version_cmp(a: &str, b: &str) -> Ordering {
    let pa: Vec<u64> = a.split('.').map(|x| x.parse().unwrap_or(0)).collect();
    let pb: Vec<u64> = b.split('.').map(|x| x.parse().unwrap_or(0)).collect();
    for i in 0..pa.len().max(pb.len()) {
        let o = pa.get(i).unwrap_or(&0).cmp(pb.get(i).unwrap_or(&0));
        if o != Ordering::Equal {
            return o;
        }
    }
    Ordering::Equal
}

/// Environment variables relevant to SteamVR/XR/Steam, privacy-filtered:
/// values only for an allowlist of non-sensitive variables, names only for
/// the rest of the relevant ones, and a count of everything else.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct EnvReport {
    pub values: BTreeMap<String, String>,
    pub names_only: Vec<String>,
    pub other_count: usize,
}

/// Variables whose value is published (paths get `~` for the home dir).
const ENV_VALUE_ALLOW: &[&str] = &[
    "XR_RUNTIME_JSON",
    "XR_API_LAYER_PATH",
    "VK_ICD_FILENAMES",
    "VK_DRIVER_FILES",
    "XDG_SESSION_TYPE",
    "XDG_CURRENT_DESKTOP",
    "WAYLAND_DISPLAY",
    "DISPLAY",
    "SDL_VIDEODRIVER",
    "SteamDeck",
    "SteamOS",
    "STEAM_RUNTIME",
    "ENABLE_GAMESCOPE_WSI",
    "container",
];

/// Variables whose value is published only when it is a plain integer.
const ENV_INT_ALLOW: &[&str] = &[
    "SteamAppId",
    "SteamGameId",
    "SteamOverlayGameId",
    "STEAM_COMPAT_APP_ID",
];

const ENV_RELEVANT_PREFIXES: &[&str] = &[
    "XR_",
    "OPENXR",
    "VR_",
    "STEAM",
    "Steam",
    "PRESSURE_VESSEL",
    "VK_",
    "MESA",
    "SDL_",
    "XDG_",
    "WAYLAND",
    "DISPLAY",
    "LD_",
    "PIPEWIRE",
    "PULSE",
    "SSH_",
    "DBUS",
    "GAMESCOPE",
    "ENABLE_",
];

pub fn classify_env(vars: &[(String, String)], home: Option<&str>) -> EnvReport {
    let mut r = EnvReport::default();
    for (k, v) in vars {
        let relevant = ENV_RELEVANT_PREFIXES.iter().any(|p| k.starts_with(p)) || k == "container";
        if !relevant {
            r.other_count += 1;
            continue;
        }
        let looks_secret = ["TOKEN", "SECRET", "PASS", "KEY", "AUTH", "COOKIE", "CRED"]
            .iter()
            .any(|w| k.to_ascii_uppercase().contains(w));
        let publish = !looks_secret
            && (ENV_VALUE_ALLOW.contains(&k.as_str())
                || k.starts_with("PRESSURE_VESSEL_")
                || (ENV_INT_ALLOW.contains(&k.as_str()) && v.parse::<u64>().is_ok()));
        if publish {
            let v = crate::redact::tilde(v, home);
            let v = if v.chars().count() > 200 {
                format!("{}…", v.chars().take(200).collect::<String>())
            } else {
                v
            };
            r.values.insert(k.clone(), v);
        } else {
            r.names_only.push(k.clone());
        }
    }
    r.names_only.sort();
    r
}

/// Kind of a network interface from its name (no addresses are reported).
pub fn interface_kind(name: &str) -> &'static str {
    let n = name;
    if n == "lo" {
        "loopback"
    } else if n.starts_with("wl") || n.starts_with("wlan") {
        "wifi"
    } else if n.starts_with("en") || n.starts_with("eth") {
        "ethernet"
    } else if n.starts_with("usb") || n.starts_with("rndis") {
        "usb"
    } else if n.starts_with("tun")
        || n.starts_with("tap")
        || n.starts_with("wg")
        || n.starts_with("tailscale")
    {
        "vpn"
    } else if n.starts_with("docker")
        || n.starts_with("veth")
        || n.starts_with("br")
        || n.starts_with("virbr")
        || n.starts_with("podman")
    {
        "virtual"
    } else if n.starts_with("p2p") {
        "wifi-direct"
    } else {
        "other"
    }
}

/// IPv4 address class without revealing it: private LAN, link-local, …
pub fn ipv4_class(o: [u8; 4]) -> &'static str {
    match o {
        [127, ..] => "loopback",
        [10, ..] => "lan",
        [172, b, ..] if (16..=31).contains(&b) => "lan",
        [192, 168, ..] => "lan",
        [169, 254, ..] => "link-local",
        [100, b, ..] if (64..=127).contains(&b) => "cgnat",
        _ => "public",
    }
}

/// Value of `"key"  "value"` in a Valve KeyValues (`.acf`/`.vdf`) file.
pub fn acf_value(s: &str, key: &str) -> Option<String> {
    let want = format!("\"{key}\"");
    s.lines().find_map(|l| {
        let l = l.trim();
        let rest = l.strip_prefix(&want)?.trim();
        Some(rest.trim_matches('"').to_string())
    })
}

/// Mount-table summary safe to publish: counts by filesystem type and the
/// mount points of a few well-known locations (labels / user names in
/// removable-media paths are replaced).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct MountSummary {
    pub total: usize,
    pub by_fs_type: BTreeMap<String, usize>,
    pub key_mounts: Vec<String>,
    pub removable: Vec<String>,
}

pub fn summarize_mounts(all: &[fp_sources::local::MountInfo]) -> MountSummary {
    let mut s = MountSummary {
        total: all.len(),
        ..Default::default()
    };
    for m in all {
        *s.by_fs_type.entry(m.fs_type.clone()).or_default() += 1;
        let p = m.mount_point.to_string_lossy();
        if [
            "/", "/home", "/var", "/usr", "/etc", "/tmp", "/opt", "/boot", "/efi",
        ]
        .contains(&p.as_ref())
        {
            s.key_mounts.push(format!("{p} ({})", m.fs_type));
        }
    }
    for m in fp_sources::local::removable_mounts(all) {
        s.removable.push(format!(
            "{} ({}, {})",
            anonymize_media_path(&m.mount_point.to_string_lossy()),
            m.fs_type,
            device_kind(&m.device)
        ));
    }
    s
}

/// `/run/media/alice/MY CARD` → `/run/media/<user>/<label>`.
pub fn anonymize_media_path(p: &str) -> String {
    let parts: Vec<&str> = p.split('/').collect();
    match parts.as_slice() {
        ["", "run", "media", _user, _label, rest @ ..] => {
            let mut s = "/run/media/<user>/<label>".to_string();
            for r in rest {
                s.push('/');
                s.push_str(r);
            }
            s
        }
        ["", "run", "media", _label] => "/run/media/<label>".into(),
        ["", "media", _user, _label, ..] => "/media/<user>/<label>".into(),
        ["", "media", _label] => "/media/<label>".into(),
        ["", "mnt", _label, ..] => "/mnt/<label>".into(),
        _ => p.to_string(),
    }
}

/// `mmcblk0p1` → `microSD/eMMC`, `sda1` → `scsi/usb`, …
pub fn device_kind(dev: &str) -> &'static str {
    let n = dev.rsplit('/').next().unwrap_or(dev);
    if n.starts_with("mmcblk") {
        "mmc (microSD)"
    } else if n.starts_with("sd") {
        "sd (USB/SCSI)"
    } else if n.starts_with("nvme") {
        "nvme"
    } else if n.starts_with("dm-") || dev.starts_with("/dev/mapper") {
        "device-mapper"
    } else {
        "other"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_release() {
        let s = "# comment\nNAME=\"SteamOS\"\nID=steamos\nVERSION_ID=3.7.13\nPRETTY_NAME='Steam OS'\nBUILD_ID=\"20250101.1\"\nX=\"a \\\"q\\\" b\"\nbad line\n=nokey\n";
        let m = parse_os_release(s);
        assert_eq!(m["NAME"], "SteamOS");
        assert_eq!(m["ID"], "steamos");
        assert_eq!(m["VERSION_ID"], "3.7.13");
        assert_eq!(m["PRETTY_NAME"], "Steam OS");
        assert_eq!(m["BUILD_ID"], "20250101.1");
        assert_eq!(m["X"], "a \"q\" b");
        assert_eq!(m.len(), 6);
    }

    #[test]
    fn meminfo() {
        let m = parse_meminfo(
            "MemTotal:       16000000 kB\nMemAvailable:    8000000 kB\nHugePages_Total: 0\n",
        );
        assert_eq!(m["MemTotal"], 16_000_000);
        assert_eq!(m["MemAvailable"], 8_000_000);
        assert_eq!(m["HugePages_Total"], 0);
    }

    #[test]
    fn cpuinfo_arm_and_x86() {
        let arm = "processor\t: 0\nBogoMIPS\t: 38.40\nFeatures\t: fp asimd fphp asimdhp asimddp sve2 i8mm\nCPU implementer\t: 0x41\nCPU part\t: 0xd80\n\nprocessor\t: 1\nCPU implementer\t: 0x41\nCPU part\t: 0xd82\n\nprocessor\t: 2\nCPU implementer\t: 0x41\nCPU part\t: 0xd82\nHardware\t: Qualcomm SM8650\n";
        let c = parse_cpuinfo(arm);
        assert_eq!(c.processors, 3);
        assert_eq!(c.arm_parts, ["0x41:0xd80", "0x41:0xd82"]);
        assert_eq!(c.hardware.as_deref(), Some("Qualcomm SM8650"));
        assert_eq!(
            c.features,
            ["asimd", "fphp", "asimdhp", "asimddp", "sve2", "i8mm"]
        );
        let x86 = "processor : 0\nmodel name : AMD Custom APU 0405\nflags : fpu sse avx2\nprocessor : 1\nmodel name : AMD Custom APU 0405\n";
        let c = parse_cpuinfo(x86);
        assert_eq!(c.processors, 2);
        assert_eq!(c.model_name.as_deref(), Some("AMD Custom APU 0405"));
        assert_eq!(c.features, ["avx2"]);
    }

    #[test]
    fn group_and_passwd() {
        let g = parse_group_file("root:x:0:\nvideo:x:44:deck,alice\nbad\nrender:x:107:\n");
        assert_eq!(g.len(), 3);
        assert_eq!(g[1].name, "video");
        assert_eq!(g[1].gid, 44);
        assert_eq!(g[1].members, ["deck", "alice"]);
        assert!(g[2].members.is_empty());
        let p = "root:x:0:0::/root:/bin/bash\ndeck:x:1000:1000::/home/deck:/bin/bash\n";
        assert_eq!(passwd_name(p, 1000).as_deref(), Some("deck"));
        assert_eq!(passwd_name(p, 5), None);
    }

    #[test]
    fn modes() {
        assert_eq!(mode_string(libc::S_IFCHR | 0o660), "crw-rw----");
        assert_eq!(mode_string(libc::S_IFDIR | 0o755), "drwxr-xr-x");
        assert_eq!(mode_string(0o644 | libc::S_IFREG), "-rw-r--r--");
    }

    #[test]
    fn natural_sorting() {
        let mut v = vec!["/dev/video10", "/dev/video2", "/dev/video0", "/dev/media1"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(
            v,
            ["/dev/media1", "/dev/video0", "/dev/video2", "/dev/video10"]
        );
        assert_eq!(natural_cmp("a01", "a1"), Ordering::Equal);
    }

    #[test]
    fn maps_lookup() {
        let maps = "7f00-7f01 r--p 00000000 08:01 1 /usr/lib/libvulkan.so.1.3.280\n7f02-7f03 r--p 00000000 08:01 2 /usr/lib/libvulkan_radeon.so\n";
        assert_eq!(
            mapped_library(maps, "libvulkan").as_deref(),
            Some("/usr/lib/libvulkan.so.1.3.280")
        );
        assert_eq!(mapped_library(maps, "libopenxr_loader"), None);
    }

    #[test]
    fn symbol_versions() {
        let data =
            b"\0GLIBCXX_3.4.9\0GLIBCXX_3.4.33\0GLIBCXX_3.4.30\0CXXABI_1.3.15\0GLIBCXX_DEBUG\0";
        assert_eq!(
            max_symbol_version(data, "GLIBCXX_3.4.").as_deref(),
            Some("GLIBCXX_3.4.33")
        );
        assert_eq!(max_symbol_version(data, "GLIBC_2."), None);
        assert_eq!(version_cmp("2.10", "2.9"), Ordering::Greater);
        assert_eq!(version_cmp("2.36", "2.36.0"), Ordering::Equal);
    }

    #[test]
    fn env_privacy() {
        let vars: Vec<(String, String)> = [
            ("XR_RUNTIME_JSON", "/home/alice/.steam/steamvr.json"),
            ("SteamAppId", "123"),
            ("SteamGameId", "not-a-number"),
            ("STEAM_TOKEN_XYZ", "abc"),
            ("PRESSURE_VESSEL_RUNTIME", "scout"),
            ("SSH_CONNECTION", "10.0.0.2 5555 10.0.0.3 22"),
            ("HOME", "/home/alice"),
            ("PATH", "/usr/bin"),
        ]
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
        let r = classify_env(&vars, Some("/home/alice"));
        assert_eq!(r.values["XR_RUNTIME_JSON"], "~/.steam/steamvr.json");
        assert_eq!(r.values["SteamAppId"], "123");
        assert_eq!(r.values["PRESSURE_VESSEL_RUNTIME"], "scout");
        assert!(!r.values.contains_key("SteamGameId"));
        assert!(!r.values.contains_key("STEAM_TOKEN_XYZ"));
        assert!(!r.values.contains_key("SSH_CONNECTION"));
        assert_eq!(
            r.names_only,
            ["SSH_CONNECTION", "STEAM_TOKEN_XYZ", "SteamGameId"]
        );
        assert_eq!(r.other_count, 2);
    }

    #[test]
    fn interfaces_and_addresses() {
        assert_eq!(interface_kind("wlan0"), "wifi");
        assert_eq!(interface_kind("wlp1s0"), "wifi");
        assert_eq!(interface_kind("enp3s0"), "ethernet");
        assert_eq!(interface_kind("lo"), "loopback");
        assert_eq!(interface_kind("docker0"), "virtual");
        assert_eq!(interface_kind("usb0"), "usb");
        assert_eq!(ipv4_class([192, 168, 1, 2]), "lan");
        assert_eq!(ipv4_class([172, 20, 0, 1]), "lan");
        assert_eq!(ipv4_class([172, 40, 0, 1]), "public");
        assert_eq!(ipv4_class([169, 254, 3, 3]), "link-local");
        assert_eq!(ipv4_class([127, 0, 0, 1]), "loopback");
    }

    #[test]
    fn acf() {
        let s = "\"AppState\"\n{\n\t\"appid\"\t\t\"250820\"\n\t\"buildid\"\t\t\"19876543\"\n}\n";
        assert_eq!(acf_value(s, "buildid").as_deref(), Some("19876543"));
        assert_eq!(acf_value(s, "nope"), None);
    }

    #[test]
    fn mounts_reuse_fp_sources_parser() {
        let mi = "22 1 259:2 / / rw,relatime - btrfs /dev/nvme0n1p2 rw\n\
                  30 22 179:1 / /run/media/alice/My\\040Card rw - exfat /dev/mmcblk0p1 rw\n\
                  31 22 8:1 / /run/media/deck/USB rw - vfat /dev/sda1 rw\n\
                  40 22 0:5 / /proc rw - proc proc rw\n";
        let all = fp_sources::local::parse_mountinfo(mi);
        let s = summarize_mounts(&all);
        assert_eq!(s.total, 4);
        assert_eq!(s.by_fs_type["exfat"], 1);
        assert_eq!(s.key_mounts, ["/ (btrfs)"]);
        assert_eq!(
            s.removable,
            [
                "/run/media/<user>/<label> (exfat, mmc (microSD))",
                "/run/media/<user>/<label> (vfat, sd (USB/SCSI))"
            ]
        );
        assert!(!format!("{s:?}").contains("alice"));
        assert_eq!(anonymize_media_path("/media/CARD"), "/media/<label>");
        assert_eq!(device_kind("/dev/nvme0n1p1"), "nvme");
    }
}
