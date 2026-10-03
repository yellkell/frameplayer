//! Database schema and forward-only migrations.
//!
//! The schema version lives in `PRAGMA user_version`. Migration `i` (zero
//! based) in [`MIGRATIONS`] upgrades version `i` to `i + 1`. Each migration
//! runs in its own transaction together with the version bump, so a crash
//! leaves the database at a consistent version. Never edit a migration that
//! has shipped; append a new one instead.

use rusqlite::Connection;

use crate::error::{Error, Result};

/// Version 1: the initial schema.
const V1: &str = r#"
CREATE TABLE media (
    id                INTEGER PRIMARY KEY,
    location          TEXT    NOT NULL UNIQUE,
    source_id         TEXT    NOT NULL,
    title             TEXT    NOT NULL,
    size              INTEGER,
    mtime             INTEGER,
    duration          REAL,
    width             INTEGER,
    height            INTEGER,
    video_codec       TEXT,
    -- fp_core::VideoFormat as JSON, and fp_core::format::Evidence.
    detected_format   TEXT    NOT NULL,
    detected_evidence TEXT    NOT NULL,
    -- User override (fp_core::VideoFormat JSON), NULL when none.
    user_format       TEXT,
    -- Effective format, denormalised for filtering (user wins over detected).
    projection_kind   TEXT    NOT NULL,
    stereo            TEXT    NOT NULL,
    view_settings     TEXT,
    keyframes         TEXT    NOT NULL DEFAULT '{"frames":[]}',
    rating            INTEGER NOT NULL DEFAULT 0 CHECK (rating BETWEEN 0 AND 5),
    favorite          INTEGER NOT NULL DEFAULT 0,
    play_count        INTEGER NOT NULL DEFAULT 0,
    last_played       INTEGER,
    resume_position   REAL    NOT NULL DEFAULT 0,
    added_at          INTEGER NOT NULL,
    missing           INTEGER NOT NULL DEFAULT 0,
    thumbnail         TEXT,
    preview_strip     TEXT,
    thumbnail_url     TEXT,
    probed            INTEGER NOT NULL DEFAULT 0,
    probe_error       TEXT,
    scripts           TEXT    NOT NULL DEFAULT '[]',
    subtitles         TEXT    NOT NULL DEFAULT '[]'
);
CREATE INDEX media_source      ON media(source_id);
CREATE INDEX media_added       ON media(added_at);
CREATE INDEX media_last_played ON media(last_played);
CREATE INDEX media_pending     ON media(probed, missing);

CREATE TABLE tags (
    id   INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE COLLATE NOCASE
);
CREATE TABLE media_tags (
    media_id INTEGER NOT NULL REFERENCES media(id) ON DELETE CASCADE,
    tag_id   INTEGER NOT NULL REFERENCES tags(id)  ON DELETE CASCADE,
    PRIMARY KEY (media_id, tag_id)
) WITHOUT ROWID;
CREATE INDEX media_tags_tag ON media_tags(tag_id);

CREATE TABLE markers (
    id          INTEGER PRIMARY KEY,
    media_id    INTEGER NOT NULL REFERENCES media(id) ON DELETE CASCADE,
    time        REAL    NOT NULL,
    name        TEXT    NOT NULL,
    from_source INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX markers_media ON markers(media_id, time);

CREATE TABLE playlists (
    id          INTEGER PRIMARY KEY,
    name        TEXT    NOT NULL,
    smart_query TEXT,
    created_at  INTEGER NOT NULL
);
CREATE TABLE playlist_items (
    playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
    position    INTEGER NOT NULL,
    media_id    INTEGER NOT NULL REFERENCES media(id) ON DELETE CASCADE,
    PRIMARY KEY (playlist_id, position)
) WITHOUT ROWID;
CREATE INDEX playlist_items_media ON playlist_items(media_id);

CREATE TABLE history (
    id         INTEGER PRIMARY KEY,
    media_id   INTEGER NOT NULL REFERENCES media(id) ON DELETE CASCADE,
    started_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    position   REAL    NOT NULL DEFAULT 0,
    completed  INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX history_media   ON history(media_id);
CREATE INDEX history_started ON history(started_at);
"#;

/// All migrations, oldest first. The schema version equals the number of
/// migrations applied.
pub(crate) const MIGRATIONS: &[&str] = &[V1];

/// Schema version this build writes.
pub const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

/// Sets connection pragmas: WAL journal, foreign keys, busy timeout.
pub(crate) fn configure(conn: &Connection) -> Result<()> {
    // `journal_mode` returns a row; in-memory databases answer "memory".
    let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA synchronous = NORMAL;
         PRAGMA busy_timeout = 5000;",
    )?;
    Ok(())
}

/// Reads `PRAGMA user_version`.
pub(crate) fn version(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
}

/// Brings the database up to `migrations.len()`, refusing databases written
/// by a newer build.
pub(crate) fn migrate(conn: &mut Connection, migrations: &[&str]) -> Result<()> {
    let supported = migrations.len() as i64;
    let found = version(conn)?;
    if found > supported {
        return Err(Error::SchemaTooNew { found, supported });
    }
    for (index, sql) in migrations.iter().enumerate().skip(found as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        // PRAGMA does not accept bound parameters; the value is an integer we
        // computed, so formatting it in is safe.
        tx.execute_batch(&format!("PRAGMA user_version = {}", index + 1))?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table_names(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    #[test]
    fn fresh_database_reaches_latest_version() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn, MIGRATIONS).unwrap();
        assert_eq!(version(&conn).unwrap(), SCHEMA_VERSION);
        let tables = table_names(&conn);
        for t in [
            "history",
            "markers",
            "media",
            "media_tags",
            "playlist_items",
            "playlists",
            "tags",
        ] {
            assert!(tables.iter().any(|x| x == t), "missing table {t}");
        }
        // Running again is a no-op.
        migrate(&mut conn, MIGRATIONS).unwrap();
        assert_eq!(version(&conn).unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn forward_migration_keeps_data() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn, MIGRATIONS).unwrap();
        conn.execute(
            "INSERT INTO media (location, source_id, title, detected_format, detected_evidence,
                                projection_kind, stereo, added_at)
             VALUES ('/a.mp4', 'local', 'a', '{}', 'default', 'flat', 'mono', 0)",
            [],
        )
        .unwrap();
        let v2 = "ALTER TABLE media ADD COLUMN note TEXT NOT NULL DEFAULT 'none';";
        let mut next: Vec<&str> = MIGRATIONS.to_vec();
        next.push(v2);
        migrate(&mut conn, &next).unwrap();
        assert_eq!(version(&conn).unwrap(), SCHEMA_VERSION + 1);
        let (title, note): (String, String) = conn
            .query_row("SELECT title, note FROM media", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((title.as_str(), note.as_str()), ("a", "none"));
    }

    #[test]
    fn failed_migration_rolls_back() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn, MIGRATIONS).unwrap();
        let mut next: Vec<&str> = MIGRATIONS.to_vec();
        next.push("CREATE TABLE extra (x); THIS IS NOT SQL;");
        assert!(migrate(&mut conn, &next).is_err());
        assert_eq!(version(&conn).unwrap(), SCHEMA_VERSION);
        assert!(!table_names(&conn).iter().any(|t| t == "extra"));
    }

    #[test]
    fn newer_database_is_refused() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA user_version = 999").unwrap();
        match migrate(&mut conn, MIGRATIONS) {
            Err(Error::SchemaTooNew { found, supported }) => {
                assert_eq!((found, supported), (999, SCHEMA_VERSION));
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
