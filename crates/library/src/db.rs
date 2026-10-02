//! Connection management and schema migrations.

use crate::error::Result;
use fp_sources::{SourceConfig, SourceKind};
use parking_lot::{Mutex, MutexGuard};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};

/// Ordered migrations; `PRAGMA user_version` records how many ran.
const MIGRATIONS: &[&str] = &[
    // v1: initial schema.
    r#"
    CREATE TABLE sources (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        kind TEXT NOT NULL,
        uri TEXT NOT NULL,
        pinned_host_key TEXT,
        last_scan INTEGER
    );
    CREATE TABLE items (
        id INTEGER PRIMARY KEY,
        source_id TEXT REFERENCES sources(id) ON DELETE CASCADE,
        uri TEXT NOT NULL UNIQUE,
        path TEXT NOT NULL,
        title TEXT NOT NULL,
        size INTEGER,
        mtime INTEGER,
        content_hash TEXT,
        duration_us INTEGER,
        width INTEGER,
        height INTEGER,
        codec TEXT,
        fps REAL,
        hdr INTEGER NOT NULL DEFAULT 0,
        projection_json TEXT,
        projection_kind TEXT,
        stereo TEXT,
        swap_eyes INTEGER NOT NULL DEFAULT 0,
        detect_source TEXT NOT NULL DEFAULT 'default',
        thumbnail_path TEXT,
        sprite_path TEXT,
        sprite_json TEXT,
        remote_thumbnail TEXT,
        probed INTEGER NOT NULL DEFAULT 0,
        added_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE INDEX items_source ON items(source_id);
    CREATE INDEX items_hash ON items(content_hash);
    CREATE TABLE view_overrides (
        content_hash TEXT NOT NULL DEFAULT '',
        path TEXT NOT NULL DEFAULT '',
        settings_json TEXT NOT NULL,
        updated_at INTEGER NOT NULL,
        PRIMARY KEY (content_hash, path)
    );
    CREATE TABLE tags (
        id INTEGER PRIMARY KEY,
        name TEXT NOT NULL UNIQUE COLLATE NOCASE
    );
    CREATE TABLE item_tags (
        item_id INTEGER NOT NULL REFERENCES items(id) ON DELETE CASCADE,
        tag_id INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
        PRIMARY KEY (item_id, tag_id)
    );
    CREATE TABLE ratings (
        item_id INTEGER PRIMARY KEY REFERENCES items(id) ON DELETE CASCADE,
        rating REAL NOT NULL CHECK (rating >= 0 AND rating <= 5)
    );
    CREATE TABLE favourites (
        item_id INTEGER PRIMARY KEY REFERENCES items(id) ON DELETE CASCADE,
        added_at INTEGER NOT NULL
    );
    CREATE TABLE resume_points (
        item_id INTEGER PRIMARY KEY REFERENCES items(id) ON DELETE CASCADE,
        position_us INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE TABLE watch_history (
        id INTEGER PRIMARY KEY,
        item_id INTEGER NOT NULL REFERENCES items(id) ON DELETE CASCADE,
        watched_at INTEGER NOT NULL,
        position_us INTEGER,
        completed INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX watch_history_item ON watch_history(item_id, watched_at);
    CREATE TABLE bookmarks (
        id INTEGER PRIMARY KEY,
        item_id INTEGER NOT NULL REFERENCES items(id) ON DELETE CASCADE,
        position_us INTEGER NOT NULL,
        name TEXT NOT NULL,
        created_at INTEGER NOT NULL
    );
    CREATE INDEX bookmarks_item ON bookmarks(item_id, position_us);
    CREATE TABLE playlists (
        id INTEGER PRIMARY KEY,
        name TEXT NOT NULL,
        smart_query_json TEXT,
        created_at INTEGER NOT NULL
    );
    CREATE TABLE playlist_items (
        playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
        item_id INTEGER NOT NULL REFERENCES items(id) ON DELETE CASCADE,
        position INTEGER NOT NULL,
        PRIMARY KEY (playlist_id, item_id)
    );
    CREATE TABLE scripts (
        item_id INTEGER NOT NULL REFERENCES items(id) ON DELETE CASCADE,
        axis TEXT NOT NULL,
        uri TEXT NOT NULL,
        PRIMARY KEY (item_id, axis)
    );
    "#,
];

pub const SCHEMA_VERSION: u32 = MIGRATIONS.len() as u32;

/// `$XDG_DATA_HOME/frameplayer/library.sqlite`.
pub fn default_db_path() -> PathBuf {
    fp_sources::credentials::data_dir().join("library.sqlite")
}

pub(crate) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The library database. All methods are synchronous and short; the
/// connection sits behind a mutex so a `Library` can be shared via `Arc`
/// between the UI thread and async indexing tasks.
pub struct Library {
    conn: Mutex<Connection>,
    path: Option<PathBuf>,
}

impl Library {
    /// Open (creating and migrating) a database file.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        Self::init(conn, Some(path.to_path_buf()))
    }

    pub fn open_default() -> Result<Self> {
        Self::open(default_db_path())
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?, None)
    }

    fn init(conn: Connection, path: Option<PathBuf>) -> Result<Self> {
        // journal_mode returns a row; WAL is ignored (stays "memory") for
        // in-memory databases.
        let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        conn.execute_batch(
            "PRAGMA foreign_keys=ON; PRAGMA synchronous=NORMAL; PRAGMA busy_timeout=5000;",
        )?;
        let lib = Library {
            conn: Mutex::new(conn),
            path,
        };
        lib.migrate()?;
        Ok(lib)
    }

    fn migrate(&self) -> Result<()> {
        let mut conn = self.conn.lock();
        let version: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version as usize > MIGRATIONS.len() {
            return Err(crate::LibraryError::Invalid(format!(
                "database schema v{version} is newer than this build (v{SCHEMA_VERSION})"
            )));
        }
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(version as usize) {
            let tx = conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", (i + 1) as u32)?;
            tx.commit()?;
            tracing::info!("library schema migrated to v{}", i + 1);
        }
        Ok(())
    }

    pub(crate) fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock()
    }

    /// Database file path (`None` for in-memory).
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn schema_version(&self) -> Result<u32> {
        Ok(self
            .conn()
            .query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }

    // ---- sources -------------------------------------------------------

    pub fn upsert_source(&self, cfg: &SourceConfig) -> Result<()> {
        self.conn().execute(
            "INSERT INTO sources (id, name, kind, uri, pinned_host_key) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET name=excluded.name, kind=excluded.kind, uri=excluded.uri, pinned_host_key=excluded.pinned_host_key",
            params![cfg.id, cfg.name, cfg.kind.as_str(), cfg.uri, cfg.pinned_host_key],
        )?;
        Ok(())
    }

    pub fn sources(&self) -> Result<Vec<SourceConfig>> {
        let conn = self.conn();
        let mut st = conn.prepare(
            "SELECT id, name, kind, uri, pinned_host_key FROM sources ORDER BY name COLLATE NOCASE",
        )?;
        let rows = st.query_map([], |r| {
            let kind: String = r.get(2)?;
            Ok(SourceConfig {
                id: r.get(0)?,
                name: r.get(1)?,
                kind: SourceKind::parse(&kind).unwrap_or(SourceKind::Local),
                uri: r.get(3)?,
                pinned_host_key: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn source(&self, id: &str) -> Result<Option<SourceConfig>> {
        Ok(self.sources()?.into_iter().find(|s| s.id == id))
    }

    /// Remove a source and (via cascade) its items and their user data.
    pub fn remove_source(&self, id: &str) -> Result<bool> {
        Ok(self
            .conn()
            .execute("DELETE FROM sources WHERE id = ?1", [id])?
            > 0)
    }

    pub fn set_source_scanned(&self, id: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE sources SET last_scan = ?2 WHERE id = ?1",
            params![id, now()],
        )?;
        Ok(())
    }

    pub fn source_last_scan(&self, id: &str) -> Result<Option<i64>> {
        Ok(self
            .conn()
            .query_row("SELECT last_scan FROM sources WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .optional()?
            .flatten())
    }

    /// Write a consistent snapshot of the database to `dest` (used by export).
    pub fn backup_to(&self, dest: &Path) -> Result<()> {
        if dest.exists() {
            std::fs::remove_file(dest)?;
        }
        self.conn()
            .execute("VACUUM INTO ?1", [dest.to_string_lossy()])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_run_once() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("lib.sqlite");
        let lib = Library::open(&p).unwrap();
        assert_eq!(lib.schema_version().unwrap(), SCHEMA_VERSION);
        let mode: String = lib
            .conn()
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        drop(lib);
        let lib = Library::open(&p).unwrap();
        assert_eq!(lib.schema_version().unwrap(), SCHEMA_VERSION);
        lib.conn().pragma_update(None, "user_version", 99).unwrap();
        drop(lib);
        assert!(Library::open(&p).is_err());
    }

    #[test]
    fn sources_crud() {
        let lib = Library::open_in_memory().unwrap();
        let mut s = SourceConfig {
            id: "nas".into(),
            name: "NAS".into(),
            kind: SourceKind::Smb,
            uri: "smb://nas/v".into(),
            pinned_host_key: None,
        };
        lib.upsert_source(&s).unwrap();
        s.name = "My NAS".into();
        lib.upsert_source(&s).unwrap();
        assert_eq!(lib.sources().unwrap(), vec![s.clone()]);
        assert_eq!(lib.source_last_scan("nas").unwrap(), None);
        lib.set_source_scanned("nas").unwrap();
        assert!(lib.source_last_scan("nas").unwrap().is_some());
        assert!(lib.remove_source("nas").unwrap());
        assert!(lib.source("nas").unwrap().is_none());
    }
}
