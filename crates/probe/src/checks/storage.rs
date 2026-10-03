//! Storage: mount table summary (via fp-sources' mountinfo parser),
//! removable media under `/run/media`, microSD / USB block devices and
//! free space. Answers P14. Volume labels and user names never appear.

use crate::parse;
use crate::report::{CheckOutput, Status};
use crate::runner::CheckContext;
use crate::util::{self, read_trim, Out};
use serde_json::json;
use std::path::Path;

pub fn run(_ctx: &CheckContext) -> CheckOutput {
    let mut o = Out::new();
    let mounts = std::fs::read_to_string("/proc/self/mountinfo")
        .map(|s| fp_sources::local::parse_mountinfo(&s))
        .unwrap_or_default();
    let summary = parse::summarize_mounts(&mounts);
    let removable_count = summary.removable.len();
    o.set("mounts", &summary);

    // What exists under /run/media (structure only).
    let mut run_media = Vec::new();
    for top in util::list_prefixed("/run/media", "") {
        let inner = util::list_prefixed(&top.to_string_lossy(), "");
        let is_mount = |p: &Path| mounts.iter().any(|m| m.mount_point == p);
        if is_mount(&top) {
            run_media.push("/run/media/<label> (mounted directly)".to_string());
        } else {
            run_media.push(format!(
                "/run/media/<user>/ with {} entr{} ({} mounted)",
                inner.len(),
                if inner.len() == 1 { "y" } else { "ies" },
                inner.iter().filter(|p| is_mount(p)).count()
            ));
        }
    }
    o.set("run_media", &run_media);

    // Block devices: microSD (mmc) and USB mass storage.
    let mut blocks = Vec::new();
    let (mut sd_cards, mut usb) = (0, 0);
    for b in util::list_prefixed("/sys/block", "") {
        let name = b
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        if name.starts_with("loop") || name.starts_with("ram") || name.starts_with("zram") {
            continue;
        }
        let removable = read_trim(b.join("removable")).as_deref() == Some("1");
        let link = std::fs::read_link(&b)
            .map(|l| l.to_string_lossy().into_owned())
            .unwrap_or_default();
        let via_usb = link.contains("/usb");
        let mmc_type = read_trim(b.join("device/type"));
        let size_gb = read_trim(b.join("size"))
            .and_then(|s| s.parse::<u64>().ok())
            .map(|sectors| (sectors * 512) as f64 / 1e9);
        let kind = if name.starts_with("mmcblk") {
            match mmc_type.as_deref() {
                Some("SD") => {
                    sd_cards += 1;
                    "microSD"
                }
                Some("MMC") => "eMMC",
                _ => "mmc",
            }
        } else if via_usb {
            usb += 1;
            "usb"
        } else if name.starts_with("nvme") {
            "nvme"
        } else if name.starts_with("sd") {
            "ufs/scsi"
        } else {
            "other"
        };
        blocks.push(json!({
            "name": name,
            "kind": kind,
            "removable": removable,
            "size_gb": size_gb.map(|g| (g * 10.0).round() / 10.0),
        }));
    }
    o.set("block_devices", &blocks);
    o.set("free_space_home_gb", free_gb(&util::home()));

    let any_media = removable_count > 0;
    let (st, msg) = if any_media {
        (
            Status::Pass,
            format!(
                "{removable_count} removable volume(s) mounted: {}",
                summary.removable.join(", ")
            ),
        )
    } else if sd_cards + usb > 0 {
        (
            Status::Fail,
            format!(
                "{sd_cards} microSD / {usb} USB device(s) present but none mounted where fp-sources looks (/run/media, /media, /mnt)"
            ),
        )
    } else {
        (
            Status::Unknown,
            "no microSD card or USB drive inserted; insert one and re-run to learn the mount point"
                .to_string(),
        )
    };
    o.finding("removable_mount_point", st, &["P14"], msg.clone());
    let status = if st == Status::Fail {
        Status::Fail
    } else {
        Status::Pass
    };
    o.finish(status, format!("{} mounts; {msg}", summary.total))
}

fn free_gb(p: &Path) -> Option<f64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).ok()?;
    // SAFETY: zeroed statvfs filled by the call.
    unsafe {
        let mut s: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut s) != 0 {
            return None;
        }
        let gb = s.f_bavail as f64 * s.f_frsize as f64 / 1e9;
        Some((gb * 10.0).round() / 10.0)
    }
}
