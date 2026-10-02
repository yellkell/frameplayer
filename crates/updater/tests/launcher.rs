//! Interop test: `dist/frameplayer.sh` and `fp_updater::layout` must agree on
//! the on-disk protocol (adoption, trial counting, rollback).
#![cfg(unix)]

use fp_updater::layout::{BootOutcome, HealthPolicy, InstallLayout};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

const LAUNCHER: &str = include_str!("../../../dist/frameplayer.sh");

/// Lay down what a flat tarball extraction of `ver` produces. The fake
/// binary prints its version and the env the launcher exported.
fn flat_install(root: &Path, ver: &str) {
    let bin = root.join("versions").join(ver).join("bin");
    fs::create_dir_all(&bin).unwrap();
    let exe = bin.join("frameplayer");
    fs::write(
        &exe,
        format!(
            "#!/bin/sh\necho \"ran {ver} counted=$FP_LAUNCHER_COUNTED root=$FP_INSTALL_ROOT\"\n"
        ),
    )
    .unwrap();
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(root.join("frameplayer.sh"), LAUNCHER).unwrap();
    fs::set_permissions(
        root.join("frameplayer.sh"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    fs::write(root.join("RELEASE"), format!("{ver}\n")).unwrap();
}

/// Run the launcher; return what the fake binary logged.
fn launch(root: &Path, data: &Path) -> String {
    let st = Command::new("sh")
        .arg(root.join("frameplayer.sh"))
        .env("XDG_DATA_HOME", data)
        .env("FP_MAX_ATTEMPTS", "2")
        .status()
        .unwrap();
    assert!(st.success());
    let log = fs::read_to_string(data.join("frameplayer/logs/launcher.log")).unwrap();
    log.lines()
        .rev()
        .find(|l| l.starts_with("ran "))
        .unwrap_or("")
        .to_string()
}

#[test]
fn launcher_and_layout_interoperate() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("frameplayer");
    let data = d.path().join("data");
    fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let layout = InstallLayout::new(&root);

    // First flat install: adopted, no trial.
    flat_install(&root, "0.1.0");
    let out = launch(&root, &data);
    assert!(out.starts_with("ran 0.1.0 counted=1"), "{out}");
    assert!(out.ends_with(&format!("root={}", root.display())));
    assert_eq!(layout.current_version().as_deref(), Some("0.1.0"));
    assert!(layout.trial().is_none());

    // Second flat install on top: goes on trial; launcher counts attempts.
    flat_install(&root, "0.2.0");
    assert!(launch(&root, &data).starts_with("ran 0.2.0"));
    assert_eq!(layout.previous_version().as_deref(), Some("0.1.0"));
    assert_eq!(layout.trial().unwrap().attempts, 1);
    // The app, started by the launcher, sees the trial without recounting.
    assert_eq!(
        layout
            .boot_check_with(&HealthPolicy::default(), true)
            .unwrap(),
        BootOutcome::Trial {
            version: "0.2.0".into(),
            attempt: 1
        }
    );
    assert!(launch(&root, &data).starts_with("ran 0.2.0"));
    assert_eq!(layout.trial().unwrap().attempts, 2);

    // Third launch exceeds max (2) without mark_healthy: rollback.
    assert!(launch(&root, &data).starts_with("ran 0.1.0"));
    assert_eq!(layout.current_version().as_deref(), Some("0.1.0"));
    assert!(layout.is_blocked("0.2.0"));
    assert!(layout.trial().is_none());
    // RELEASE still says 0.2.0 but it was already adopted: no re-switch.
    assert!(launch(&root, &data).starts_with("ran 0.1.0"));

    // An in-app update installed by the Rust side, then marked healthy.
    flat_install(&root, "0.3.0");
    fs::write(root.join(".installed-release"), "0.3.0\n").unwrap();
    layout.activate("0.3.0", &HealthPolicy::default()).unwrap();
    assert!(launch(&root, &data).starts_with("ran 0.3.0"));
    layout.mark_healthy().unwrap();
    for _ in 0..4 {
        assert!(launch(&root, &data).starts_with("ran 0.3.0"));
    }
    assert!(layout.trial().is_none());
}
