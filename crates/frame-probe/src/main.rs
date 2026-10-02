//! frame-probe: reports what a Steam Frame offers a native Linux ARM64 VR
//! video player. Run it on the headset; it writes a JSON and a text report.
//!
//! Usage: frame-probe [--no-session] [--out DIR] [--json]
//!   --no-session  skip creating an OpenXR session (no headset display change)
//!   --out DIR     report directory (default: ~/frameplayer-probe)
//!   --json        also print the JSON report to stdout

mod drm;
mod libs;
mod system;
mod util;
mod v4l2;
mod verdict;
mod vk;
mod xr;
mod xr_loader;

use serde::Serialize;
use std::fmt::Write as _;
use std::path::PathBuf;

#[derive(Serialize)]
pub struct Report {
    pub probe_version: &'static str,
    pub generated_unix: u64,
    pub verdicts: Vec<verdict::Verdict>,
    pub system: system::SystemReport,
    pub libs: Vec<libs::LibReport>,
    pub drm: Vec<drm::DrmNode>,
    pub v4l2: v4l2::V4l2Report,
    pub vulkan: vk::VkReport,
    pub xr: xr::XrReport,
}

struct Args {
    session: bool,
    out: PathBuf,
    json: bool,
}

fn parse_args() -> Result<Args, String> {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let mut a = Args {
        session: true,
        out: PathBuf::from(home).join("frameplayer-probe"),
        json: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--no-session" => a.session = false,
            "--json" => a.json = true,
            "--out" => a.out = it.next().ok_or("--out needs a directory")?.into(),
            "-h" | "--help" => {
                println!(
                    "{}",
                    include_str!("main.rs")
                        .lines()
                        .skip(1)
                        .take(6)
                        .map(|l| l.trim_start_matches("//!").trim_start())
                        .collect::<Vec<_>>()
                        .join("\n")
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(a)
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("frame-probe: {e}");
            std::process::exit(2);
        }
    };
    eprintln!(
        "frame-probe {}: probing system, libraries, DRM, V4L2, Vulkan, OpenXR...",
        env!("CARGO_PKG_VERSION")
    );

    let mut report = Report {
        probe_version: env!("CARGO_PKG_VERSION"),
        generated_unix: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        verdicts: Vec::new(),
        system: system::probe(),
        libs: libs::probe(),
        drm: drm::probe(),
        v4l2: v4l2::probe(),
        vulkan: vk::probe(),
        xr: xr::probe(args.session),
    };
    report.verdicts = verdict::evaluate(&report);

    let json = serde_json::to_string_pretty(&report).expect("report serializes");
    let text = render_text(&report);
    print!("{text}");
    if args.json {
        println!("{json}");
    }
    match write_files(&args.out, report.generated_unix, &json, &text) {
        Ok((j, t)) => eprintln!("\nSaved {}\n      {}", j.display(), t.display()),
        Err(e) => eprintln!("\nCould not save report to {}: {e}", args.out.display()),
    }
}

fn write_files(
    dir: &std::path::Path,
    ts: u64,
    json: &str,
    text: &str,
) -> std::io::Result<(PathBuf, PathBuf)> {
    std::fs::create_dir_all(dir)?;
    let j = dir.join(format!("probe-{ts}.json"));
    let t = dir.join(format!("probe-{ts}.txt"));
    std::fs::write(&j, json)?;
    std::fs::write(&t, text)?;
    Ok((j, t))
}

fn render_text(r: &Report) -> String {
    let mut s = String::new();
    let os = r
        .system
        .os_release
        .get("PRETTY_NAME")
        .cloned()
        .unwrap_or_default();
    let _ = writeln!(s, "FramePlayer platform probe {}", r.probe_version);
    let _ = writeln!(
        s,
        "{os} | {} | {} | glibc {}",
        r.system.kernel,
        r.system.machine,
        r.system.glibc.clone().unwrap_or_default()
    );
    let _ = writeln!(
        s,
        "{} | {} CPUs | {} MiB RAM\n",
        r.system.cpu_model.clone().unwrap_or_default(),
        r.system.cpu_count,
        r.system.mem_total_mib.unwrap_or(0)
    );

    let _ = writeln!(s, "== Answers ==");
    for v in &r.verdicts {
        let tag = match v.status {
            verdict::Status::Yes => "YES    ",
            verdict::Status::No => "NO     ",
            verdict::Status::Partial => "PARTIAL",
            verdict::Status::Unknown => "UNKNOWN",
        };
        let _ = writeln!(s, "[{tag}] {}\n          {}", v.question, v.answer);
    }

    let _ = writeln!(s, "\n== OpenXR ==");
    if let Some(m) = &r.xr.manifest {
        let _ = writeln!(
            s,
            "manifest: {} -> {}",
            m.manifest_path, m.resolved_library_path
        );
    }
    if let Some(n) = &r.xr.runtime_name {
        let _ = writeln!(
            s,
            "runtime: {n} {} (API {})",
            r.xr.runtime_version.clone().unwrap_or_default(),
            r.xr.instance_api_version.clone().unwrap_or_default()
        );
    }
    if let Some(sys) = &r.xr.system {
        let _ = writeln!(
            s,
            "system: {} | max swapchain {}x{} | layers {} | blend {}",
            sys.name,
            sys.max_swapchain_width,
            sys.max_swapchain_height,
            sys.max_layer_count,
            sys.blend_modes.join(",")
        );
    }
    if !r.xr.extensions.is_empty() {
        let _ = writeln!(s, "extensions ({}):", r.xr.extensions.len());
        for (n, ver) in &r.xr.extensions {
            let _ = writeln!(s, "  {n} v{ver}");
        }
    }
    for p in &r.xr.interaction_profiles {
        let _ = writeln!(
            s,
            "profile {}: {} accepted, {} rejected",
            p.profile,
            p.accepted.len(),
            p.rejected.len()
        );
        for a in &p.accepted {
            let _ = writeln!(s, "  + {a}");
        }
    }
    if let Some(sess) = &r.xr.session {
        let _ = writeln!(
            s,
            "session GPU: {} / {}",
            sess.vulkan_device.clone().unwrap_or_default(),
            sess.vulkan_driver.clone().unwrap_or_default()
        );
        let _ = writeln!(s, "reference spaces: {}", sess.reference_spaces.join(", "));
        for e in &sess.errors {
            let _ = writeln!(s, "session error: {e}");
        }
    }
    for e in &r.xr.errors {
        let _ = writeln!(s, "error: {e}");
    }

    let _ = writeln!(s, "\n== Vulkan ==");
    for d in &r.vulkan.devices {
        let _ = writeln!(
            s,
            "{} ({}) API {} driver {} {}",
            d.name,
            d.device_type,
            d.api_version,
            d.driver_name.clone().unwrap_or_default(),
            d.driver_info.clone().unwrap_or_default()
        );
        let _ = writeln!(
            s,
            "  notable extensions: {}",
            d.notable_extensions.join(", ")
        );
        for q in &d.queue_families {
            let _ = writeln!(
                s,
                "  queue {}: {} x{}{}",
                q.index,
                q.flags,
                q.count,
                q.video_codecs
                    .as_ref()
                    .map(|c| format!(" video {c}"))
                    .unwrap_or_default()
            );
        }
        for f in &d.format_modifiers {
            let _ = writeln!(
                s,
                "  {}: {} modifiers ({} sampleable)",
                f.format,
                f.modifiers.len(),
                f.modifiers.iter().filter(|m| m.2).count()
            );
        }
    }
    for e in &r.vulkan.errors {
        let _ = writeln!(s, "error: {e}");
    }

    let _ = writeln!(s, "\n== V4L2 / DRM ==");
    for d in &r.v4l2.devices {
        match &d.open_error {
            Some(e) => {
                let _ = writeln!(
                    s,
                    "{} mode {} group {}: {e}",
                    d.node,
                    d.mode,
                    d.owner_group.clone().unwrap_or_default()
                );
            }
            None => {
                let ins: Vec<&str> = d.compressed_in.iter().map(|f| f.fourcc.as_str()).collect();
                let outs: Vec<&str> = d.raw_out.iter().map(|f| f.fourcc.as_str()).collect();
                let _ = writeln!(
                    s,
                    "{} {} [{} / {}]: in {} out {}",
                    d.node,
                    d.kind,
                    d.driver,
                    d.card,
                    ins.join(","),
                    outs.join(",")
                );
            }
        }
    }
    let _ = writeln!(s, "media nodes: {}", r.v4l2.media_nodes.join(", "));
    for n in &r.drm {
        let _ = writeln!(
            s,
            "{}: {} {}",
            n.node,
            n.driver.clone().unwrap_or_default(),
            n.open_error.clone().unwrap_or_default()
        );
    }

    let _ = writeln!(s, "\n== Libraries ==");
    for l in &r.libs {
        let _ = writeln!(
            s,
            "{:<26} {}",
            l.soname,
            if l.found {
                l.path.clone().unwrap_or("found".into())
            } else {
                "-".into()
            }
        );
    }
    let _ = writeln!(s, "\n== Storage ==");
    for m in &r.system.storage {
        let _ = writeln!(
            s,
            "{} ({}) {:.1} of {:.1} GiB free",
            m.mount_point,
            m.fs_type,
            m.free_gib.unwrap_or(0.0),
            m.total_gib.unwrap_or(0.0)
        );
    }
    let _ = writeln!(s, "groups: {}", r.system.groups.join(", "));
    s
}
