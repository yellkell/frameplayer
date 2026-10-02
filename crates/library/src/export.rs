//! Whole-library export/import as a single zip (OUTLINE §3.5).
//!
//! Layout:
//! ```text
//! manifest.json      format id, version, schema version, export time
//! library.sqlite     consistent snapshot (VACUUM INTO) of the database
//! overrides.json     per-video view overrides, human readable
//! config/<name>      app config files (TOML etc.) handed in by the caller
//! ```
//! The credential key file is never exported; credentials must be
//! re-entered after restoring on another device (see fp-sources'
//! `credentials` module for the rationale).

use crate::db::{Library, SCHEMA_VERSION};
use crate::error::{LibraryError, Result};
use fp_core::ViewSettings;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use zip::write::SimpleFileOptions;

pub const FORMAT: &str = "frameplayer-library";
pub const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub format: String,
    pub version: u32,
    pub schema_version: u32,
    pub exported_at: i64,
    pub app_version: String,
    pub config_files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OverrideRecord {
    pub content_hash: String,
    pub path: String,
    pub settings: ViewSettings,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportReport {
    pub manifest: Manifest,
    pub db_path: PathBuf,
    pub config_files: Vec<PathBuf>,
}

fn check_config_name(name: &str) -> Result<()> {
    let p = Path::new(name);
    if name.is_empty() || p.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(LibraryError::Invalid(format!(
            "bad config file name {name:?}"
        )));
    }
    Ok(())
}

/// Write the library plus `config_files` (`(name in zip, file on disk)`)
/// to `zip_path`. Missing config files are skipped.
pub fn export_library(
    lib: &Library,
    zip_path: &Path,
    config_files: &[(&str, &Path)],
) -> Result<Manifest> {
    let tmp_dir = zip_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&tmp_dir)?;
    let snapshot = tempfile_in(&tmp_dir, "fp-export", ".sqlite");
    lib.backup_to(&snapshot)?;
    let result = (|| {
        let partial = zip_path.with_extension("zip.partial");
        let mut zw = zip::ZipWriter::new(File::create(&partial)?);
        let opts = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .large_file(true);

        let mut included = Vec::new();
        for (name, path) in config_files {
            check_config_name(name)?;
            match std::fs::read(path) {
                Ok(data) => {
                    zw.start_file(format!("config/{name}"), opts)?;
                    zw.write_all(&data)?;
                    included.push(name.to_string());
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }

        let overrides: Vec<OverrideRecord> = lib
            .all_overrides()?
            .into_iter()
            .map(|(content_hash, path, settings)| OverrideRecord {
                content_hash,
                path,
                settings,
            })
            .collect();
        zw.start_file("overrides.json", opts)?;
        zw.write_all(&serde_json::to_vec_pretty(&overrides)?)?;

        zw.start_file("library.sqlite", opts)?;
        std::io::copy(&mut File::open(&snapshot)?, &mut zw)?;

        let manifest = Manifest {
            format: FORMAT.into(),
            version: FORMAT_VERSION,
            schema_version: SCHEMA_VERSION,
            exported_at: crate::db::now(),
            app_version: env!("CARGO_PKG_VERSION").into(),
            config_files: included,
        };
        zw.start_file("manifest.json", opts)?;
        zw.write_all(&serde_json::to_vec_pretty(&manifest)?)?;
        zw.finish()?.sync_all()?;
        std::fs::rename(&partial, zip_path)?;
        Ok(manifest)
    })();
    let _ = std::fs::remove_file(&snapshot);
    result
}

fn tempfile_in(dir: &Path, prefix: &str, suffix: &str) -> PathBuf {
    dir.join(format!(
        "{prefix}-{}-{:016x}{suffix}",
        std::process::id(),
        rand::random::<u64>()
    ))
}

fn open_zip(zip_path: &Path) -> Result<zip::ZipArchive<File>> {
    Ok(zip::ZipArchive::new(File::open(zip_path)?)?)
}

/// Read and validate an export's manifest.
pub fn read_manifest(zip_path: &Path) -> Result<Manifest> {
    let mut z = open_zip(zip_path)?;
    let mut s = String::new();
    z.by_name("manifest.json")
        .map_err(|_| LibraryError::Invalid("not a FramePlayer export (no manifest.json)".into()))?
        .read_to_string(&mut s)?;
    let m: Manifest = serde_json::from_str(&s)?;
    if m.format != FORMAT {
        return Err(LibraryError::Invalid(format!(
            "unknown export format {:?}",
            m.format
        )));
    }
    if m.version > FORMAT_VERSION || m.schema_version > SCHEMA_VERSION {
        return Err(LibraryError::Invalid(format!(
            "export is from a newer FramePlayer (format v{}, schema v{})",
            m.version, m.schema_version
        )));
    }
    Ok(m)
}

/// Restore an export: replaces the database at `db_path` (close any open
/// [`Library`] on it first) and writes config files into `config_dir`.
/// The imported database is opened (and migrated) before it replaces the
/// current one, so a broken archive never clobbers a working library.
pub fn import_library(zip_path: &Path, db_path: &Path, config_dir: &Path) -> Result<ImportReport> {
    let manifest = read_manifest(zip_path)?;
    let mut z = open_zip(zip_path)?;
    let dir = db_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&dir)?;
    let staged = tempfile_in(&dir, "fp-import", ".sqlite");
    {
        let mut entry = z
            .by_name("library.sqlite")
            .map_err(|_| LibraryError::Invalid("export has no library.sqlite".into()))?;
        let mut out = File::create(&staged)?;
        std::io::copy(&mut entry, &mut out)?;
        out.sync_all()?;
    }
    // Validate + migrate the staged copy, then checkpoint it to a single file.
    match Library::open(&staged) {
        Ok(lib) => {
            lib.conn()
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
            drop(lib);
        }
        Err(e) => {
            let _ = std::fs::remove_file(&staged);
            return Err(e);
        }
    }
    for ext in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{ext}", db_path.display()));
        let _ = std::fs::remove_file(format!("{}{ext}", staged.display()));
    }
    std::fs::rename(&staged, db_path)?;

    let mut written = Vec::new();
    for i in 0..z.len() {
        let mut f = z.by_index(i)?;
        let Some(name) = f.enclosed_name() else {
            continue;
        };
        let Ok(rel) = name.strip_prefix("config") else {
            continue;
        };
        if f.is_dir() || rel.as_os_str().is_empty() {
            continue;
        }
        let rel_s = rel.to_string_lossy().into_owned();
        check_config_name(&rel_s)?;
        let dest = config_dir.join(rel);
        if let Some(p) = dest.parent() {
            std::fs::create_dir_all(p)?;
        }
        let mut data = Vec::new();
        f.read_to_end(&mut data)?;
        std::fs::write(&dest, data)?;
        written.push(dest);
    }
    Ok(ImportReport {
        manifest,
        db_path: db_path.to_path_buf(),
        config_files: written,
    })
}

/// Merge just the view overrides from an export into an existing library
/// (newer local entries are overwritten). Returns how many were applied.
pub fn import_overrides(lib: &Library, zip_path: &Path) -> Result<usize> {
    read_manifest(zip_path)?;
    let mut z = open_zip(zip_path)?;
    let mut s = String::new();
    z.by_name("overrides.json")?.read_to_string(&mut s)?;
    let recs: Vec<OverrideRecord> = serde_json::from_str(&s)?;
    for r in &recs {
        let h = (!r.content_hash.is_empty()).then_some(r.content_hash.as_str());
        let p = (!r.path.is_empty()).then_some(r.path.as_str());
        lib.set_override(h, p, &r.settings)?;
    }
    Ok(recs.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::items::tests::new_item;
    use fp_core::{Projection, StereoMode};

    #[test]
    fn export_import_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("data/library.sqlite");
        let cfg_dir = dir.path().join("config");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(cfg_dir.join("config.toml"), "theme = \"dark\"\n").unwrap();
        let lib = Library::open(&db).unwrap();
        let mut ni = new_item("file:///v/a.mp4");
        ni.content_hash = Some("abc".into());
        let id = lib.upsert_item(&ni).unwrap();
        lib.set_favourite(id, true).unwrap();
        lib.add_tag(id, "Keep").unwrap();
        let vs = ViewSettings {
            projection: Projection::EQUIRECT_360,
            stereo: StereoMode::Ou,
            ..Default::default()
        };
        lib.set_item_view_settings(id, &vs).unwrap();

        let zip_path = dir.path().join("backup/export.zip");
        let m = export_library(
            &lib,
            &zip_path,
            &[
                ("config.toml", &cfg_dir.join("config.toml")),
                ("missing.toml", &cfg_dir.join("missing.toml")),
            ],
        )
        .unwrap();
        assert_eq!(m.config_files, vec!["config.toml"]);
        assert_eq!(read_manifest(&zip_path).unwrap(), m);
        // No stray temp files next to the archive.
        assert_eq!(
            std::fs::read_dir(zip_path.parent().unwrap())
                .unwrap()
                .count(),
            1
        );

        // Wreck the live library, then restore.
        lib.delete_item(id).unwrap();
        drop(lib);
        let restore_cfg = dir.path().join("restored-config");
        let rep = import_library(&zip_path, &db, &restore_cfg).unwrap();
        assert_eq!(rep.config_files, vec![restore_cfg.join("config.toml")]);
        assert_eq!(
            std::fs::read_to_string(restore_cfg.join("config.toml")).unwrap(),
            "theme = \"dark\"\n"
        );
        let lib = Library::open(&db).unwrap();
        let it = lib.item_by_uri("file:///v/a.mp4").unwrap().unwrap();
        assert!(it.favourite);
        assert_eq!(it.tags, vec!["Keep"]);
        assert_eq!(lib.view_settings(it.id).unwrap(), vs);

        // Overrides can be merged into another library.
        let other = Library::open_in_memory().unwrap();
        assert_eq!(import_overrides(&other, &zip_path).unwrap(), 1);
        assert_eq!(other.get_override(Some("abc"), None).unwrap(), Some(vs));
    }

    #[test]
    fn rejects_bad_archives() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.zip");
        {
            let mut zw = zip::ZipWriter::new(File::create(&p).unwrap());
            zw.start_file("manifest.json", SimpleFileOptions::default())
                .unwrap();
            zw.write_all(br#"{"format":"frameplayer-library","version":99,"schema_version":1,"exported_at":0,"app_version":"9","config_files":[]}"#).unwrap();
            zw.finish().unwrap();
        }
        assert!(read_manifest(&p).is_err());
        let q = dir.path().join("y.zip");
        {
            let mut zw = zip::ZipWriter::new(File::create(&q).unwrap());
            zw.start_file("manifest.json", SimpleFileOptions::default())
                .unwrap();
            zw.write_all(br#"{"format":"frameplayer-library","version":1,"schema_version":1,"exported_at":0,"app_version":"x","config_files":[]}"#).unwrap();
            zw.start_file("library.sqlite", SimpleFileOptions::default())
                .unwrap();
            zw.write_all(b"this is not sqlite at all, not even close......")
                .unwrap();
            zw.finish().unwrap();
        }
        let db = dir.path().join("lib.sqlite");
        Library::open(&db)
            .unwrap()
            .upsert_item(&new_item("file:///keep.mp4"))
            .unwrap();
        assert!(import_library(&q, &db, dir.path()).is_err());
        // Existing library untouched.
        assert!(Library::open(&db)
            .unwrap()
            .item_by_uri("file:///keep.mp4")
            .unwrap()
            .is_some());
        assert!(check_config_name("../evil").is_err());
        assert!(check_config_name("/abs").is_err());
        assert!(check_config_name("sub/ok.toml").is_ok());
        assert!(read_manifest(&dir.path().join("nope.zip")).is_err());
    }
}
