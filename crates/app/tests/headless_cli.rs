//! Runs the real `frameplayer` binary headless on a generated MP4 with the
//! default (non-mock) media backend: pure-Rust MP4 demux, PCM audio decode,
//! the A/V clock with the null audio output as master, library lookup and
//! all services. The HEVC video track has no decoder in this build (no V4L2
//! device, no software decoder feature), which the player reports as a
//! warning before playing the audio to the end.

use std::process::Command;

fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut v = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
    v.extend_from_slice(kind);
    v.extend_from_slice(payload);
    v
}

fn full(kind: &[u8; 4], version: u8, flags: u32, payload: &[u8]) -> Vec<u8> {
    let mut p = vec![
        version,
        (flags >> 16) as u8,
        (flags >> 8) as u8,
        flags as u8,
    ];
    p.extend_from_slice(payload);
    bx(kind, &p)
}

fn be32(v: u32) -> [u8; 4] {
    v.to_be_bytes()
}

struct Track {
    id: u32,
    handler: [u8; 4],
    entry: Vec<u8>,
    timescale: u32,
    /// (data, duration)
    samples: Vec<(Vec<u8>, u32)>,
}

fn video_entry(w: u16, h: u16) -> Vec<u8> {
    let mut p = vec![0u8; 24];
    p[7] = 1;
    p.extend_from_slice(&w.to_be_bytes());
    p.extend_from_slice(&h.to_be_bytes());
    p.extend_from_slice(&[0, 0x48, 0, 0, 0, 0x48, 0, 0, 0, 0, 0, 0, 0, 1]);
    p.extend_from_slice(&[0u8; 32]);
    p.extend_from_slice(&[0, 0x18, 0xff, 0xff]);
    bx(b"hvc1", &p)
}

fn audio_entry(ch: u16, rate: u32) -> Vec<u8> {
    let mut p = vec![0u8; 8];
    p[7] = 1;
    p.extend_from_slice(&[0u8; 8]);
    p.extend_from_slice(&ch.to_be_bytes());
    p.extend_from_slice(&16u16.to_be_bytes());
    p.extend_from_slice(&[0u8; 4]);
    p.extend_from_slice(&(rate << 16).to_be_bytes());
    bx(b"sowt", &p)
}

fn build(tracks: &[Track]) -> Vec<u8> {
    let ftyp = bx(b"ftyp", b"isom\0\0\x02\0isomiso2mp41");
    let moov_for = |data_start: u64| -> Vec<u8> {
        let mut moov = full(
            b"mvhd",
            0,
            0,
            &[&[0u8; 8][..], &be32(1000), &be32(0), &[0u8; 80]].concat(),
        );
        let mut off = data_start;
        for t in tracks {
            let n = t.samples.len() as u32;
            let mut stts = be32(n).to_vec();
            let mut stsz = be32(0).to_vec();
            stsz.extend_from_slice(&be32(n));
            let mut co64 = be32(n).to_vec();
            for s in &t.samples {
                stts.extend_from_slice(&be32(1));
                stts.extend_from_slice(&be32(s.1));
                stsz.extend_from_slice(&be32(s.0.len() as u32));
                co64.extend_from_slice(&off.to_be_bytes());
                off += s.0.len() as u64;
            }
            let stbl = [
                full(b"stsd", 0, 0, &[&be32(1)[..], &t.entry].concat()),
                full(b"stts", 0, 0, &stts),
                full(
                    b"stsc",
                    0,
                    0,
                    &[be32(1), be32(1), be32(1), be32(1)].concat(),
                ),
                full(b"stsz", 0, 0, &stsz),
                full(b"co64", 0, 0, &co64),
            ]
            .concat();
            let mdhd = full(
                b"mdhd",
                0,
                0,
                &[
                    &[0u8; 8][..],
                    &be32(t.timescale),
                    &be32(0),
                    &[0x55, 0xc4, 0, 0],
                ]
                .concat(),
            );
            let hdlr = full(
                b"hdlr",
                0,
                0,
                &[&[0u8; 4][..], &t.handler, &[0u8; 12], b"h\0"].concat(),
            );
            let mdia = bx(
                b"mdia",
                &[mdhd, hdlr, bx(b"minf", &bx(b"stbl", &stbl))].concat(),
            );
            let tkhd = full(
                b"tkhd",
                0,
                3,
                &[&[0u8; 8][..], &be32(t.id), &[0u8; 68]].concat(),
            );
            moov.extend(bx(b"trak", &[tkhd, mdia].concat()));
        }
        bx(b"moov", &moov)
    };
    let probe = moov_for(0);
    let start = (ftyp.len() + probe.len() + 8) as u64;
    let moov = moov_for(start);
    let mdat: Vec<u8> = tracks
        .iter()
        .flat_map(|t| t.samples.iter().flat_map(|s| s.0.clone()))
        .collect();
    [ftyp, moov, bx(b"mdat", &mdat)].concat()
}

/// 1.5 s: HEVC 3840x1920 "180 SBS" video (no decoder here) + 48 kHz stereo PCM.
fn sample_mp4() -> Vec<u8> {
    let video = Track {
        id: 1,
        handler: *b"vide",
        entry: video_entry(3840, 1920),
        timescale: 30_000,
        samples: (0..45).map(|i| (vec![i as u8; 64], 1001)).collect(),
    };
    let tone: Vec<u8> = (0..4800)
        .flat_map(|i| {
            let v = ((i as f32 * 0.05).sin() * 3000.0) as i16;
            [v.to_le_bytes(), v.to_le_bytes()].concat()
        })
        .collect();
    let audio = Track {
        id: 2,
        handler: *b"soun",
        entry: audio_entry(2, 48_000),
        timescale: 48_000,
        samples: (0..15).map(|_| (tone.clone(), 4800)).collect(),
    };
    build(&[video, audio])
}

#[test]
fn headless_binary_plays_generated_mp4_to_the_end() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("demo_180_LR.mp4");
    std::fs::write(&file, sample_mp4()).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_frameplayer"))
        .args([
            "--headless",
            "--no-scan",
            "--exit-after",
            "30",
            "--log-level",
            "info",
        ])
        .arg("--data-root")
        .arg(dir.path().join("root"))
        .arg("--open")
        .arg(&file)
        .env_remove("RUST_LOG")
        .env_remove("FP_LOG_DIR")
        .env_remove("FP_INSTALL_ROOT")
        .output()
        .expect("run frameplayer");
    let log = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "exit {:?}\n{log}", out.status);
    assert!(log.contains("player state: Playing"), "{log}");
    assert!(log.contains("player state: Ended"), "{log}");
    assert!(log.contains("ended: true"), "{log}");
    assert!(
        log.contains("cannot be decoded"),
        "video track reported: {log}"
    );
    // Config and library were created under --data-root.
    assert!(dir.path().join("root/data/library.sqlite").exists());
    assert!(
        dir.path().join("root/config/config.toml").exists(),
        "default config written"
    );
}

#[test]
fn headless_binary_reports_open_failure() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_frameplayer"))
        .args(["--headless", "--no-scan", "--exit-after", "20"])
        .arg("--data-root")
        .arg(dir.path())
        .arg("--open")
        .arg(dir.path().join("missing.mp4"))
        .env_remove("FP_LOG_DIR")
        .env_remove("FP_INSTALL_ROOT")
        .output()
        .expect("run frameplayer");
    assert!(!out.status.success());
    let log = String::from_utf8_lossy(&out.stderr);
    assert!(log.contains("missing.mp4"), "{log}");
}

#[test]
fn export_and_import_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let zip = dir.path().join("backup.zip");
    // A first (headless, no media) run creates the library.
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_frameplayer"))
            .args(args)
            .arg("--data-root")
            .arg(&root)
            .env_remove("FP_LOG_DIR")
            .env_remove("FP_INSTALL_ROOT")
            .output()
            .expect("run frameplayer")
    };
    let out = run(&["--headless", "--no-scan", "--exit-after", "0.2"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = run(&["--export", zip.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(zip.exists());
    let out = run(&["--import", zip.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(root.join("data/library.sqlite").exists());
}
