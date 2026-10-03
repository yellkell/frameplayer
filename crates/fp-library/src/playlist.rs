//! Manual (ordered) and smart (stored query) playlists.

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::library::{Library, ensure_media, get_many};
use crate::query::{Query, count_conn, search_conn};
use crate::record::{MediaId, MediaRecord, PlaylistId, now};

/// A playlist summary.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Playlist {
    /// Row id.
    pub id: PlaylistId,
    /// Display name.
    pub name: String,
    /// For smart playlists, the query evaluated on every read.
    pub smart_query: Option<Query>,
    /// Unix seconds.
    pub created_at: i64,
    /// Entries (manual) or current matches (smart).
    pub item_count: usize,
}

impl Playlist {
    /// Whether the content comes from a stored query.
    pub fn is_smart(&self) -> bool {
        self.smart_query.is_some()
    }
}

fn check_name(name: &str) -> Result<&str> {
    let name = name.trim();
    if name.is_empty() {
        Err(Error::InvalidArgument("playlist name is empty".into()))
    } else {
        Ok(name)
    }
}

fn playlist_conn(conn: &Connection, id: PlaylistId) -> Result<Option<Playlist>> {
    let row: Option<(String, Option<String>, i64)> = conn
        .query_row(
            "SELECT name, smart_query, created_at FROM playlists WHERE id = ?1",
            [id.0],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((name, smart, created_at)) = row else {
        return Ok(None);
    };
    let smart_query: Option<Query> = smart.map(|s| serde_json::from_str(&s)).transpose()?;
    let item_count = match &smart_query {
        Some(q) => {
            let n = count_conn(conn, q)?.saturating_sub(q.offset);
            q.limit.map_or(n, |l| n.min(l))
        }
        None => {
            let n: i64 = conn.query_row(
                "SELECT COUNT(*) FROM playlist_items WHERE playlist_id = ?1",
                [id.0],
                |r| r.get(0),
            )?;
            n.max(0) as usize
        }
    };
    Ok(Some(Playlist {
        id,
        name,
        smart_query,
        created_at,
        item_count,
    }))
}

fn require(conn: &Connection, id: PlaylistId) -> Result<Playlist> {
    playlist_conn(conn, id)?.ok_or(Error::PlaylistNotFound(id))
}

fn require_manual(conn: &Connection, id: PlaylistId) -> Result<()> {
    if require(conn, id)?.is_smart() {
        return Err(Error::InvalidArgument(format!(
            "playlist {id} is smart; its items come from its query"
        )));
    }
    Ok(())
}

pub(crate) fn manual_ids(conn: &Connection, id: PlaylistId) -> Result<Vec<MediaId>> {
    let mut stmt = conn.prepare_cached(
        "SELECT media_id FROM playlist_items WHERE playlist_id = ?1 ORDER BY position",
    )?;
    let rows = stmt.query_map([id.0], |r| Ok(MediaId(r.get(0)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub(crate) fn write_ids(conn: &Connection, id: PlaylistId, items: &[MediaId]) -> Result<()> {
    conn.execute("DELETE FROM playlist_items WHERE playlist_id = ?1", [id.0])?;
    let mut ins = conn.prepare_cached(
        "INSERT INTO playlist_items (playlist_id, position, media_id) VALUES (?1, ?2, ?3)",
    )?;
    for (pos, m) in items.iter().enumerate() {
        ensure_media(conn, *m)?;
        ins.execute(params![id.0, pos as i64, m.0])?;
    }
    Ok(())
}

pub(crate) fn create_conn(
    conn: &Connection,
    name: &str,
    query: Option<&Query>,
) -> Result<PlaylistId> {
    let name = check_name(name)?;
    let smart = query.map(serde_json::to_string).transpose()?;
    conn.execute(
        "INSERT INTO playlists (name, smart_query, created_at) VALUES (?1, ?2, ?3)",
        params![name, smart, now()],
    )?;
    Ok(PlaylistId(conn.last_insert_rowid()))
}

impl Library {
    /// Creates an empty manual playlist.
    pub fn create_playlist(&self, name: &str) -> Result<PlaylistId> {
        self.with(|c| create_conn(c, name, None))
    }

    /// Creates a smart playlist whose content is `query`, evaluated on read.
    pub fn create_smart_playlist(&self, name: &str, query: &Query) -> Result<PlaylistId> {
        self.with(|c| create_conn(c, name, Some(query)))
    }

    /// All playlists, by name.
    pub fn playlists(&self) -> Result<Vec<Playlist>> {
        self.with(|c| {
            let ids: Vec<PlaylistId> = {
                let mut stmt =
                    c.prepare_cached("SELECT id FROM playlists ORDER BY name COLLATE NOCASE, id")?;
                let rows = stmt.query_map([], |r| Ok(PlaylistId(r.get(0)?)))?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            let mut out = Vec::with_capacity(ids.len());
            for id in ids {
                out.extend(playlist_conn(c, id)?);
            }
            Ok(out)
        })
    }

    /// One playlist.
    pub fn playlist(&self, id: PlaylistId) -> Result<Option<Playlist>> {
        self.with(|c| playlist_conn(c, id))
    }

    /// Renames a playlist.
    pub fn rename_playlist(&self, id: PlaylistId, name: &str) -> Result<()> {
        let name = check_name(name)?;
        self.with(|c| {
            let n = c.execute(
                "UPDATE playlists SET name = ?2 WHERE id = ?1",
                params![id.0, name],
            )?;
            if n == 0 {
                return Err(Error::PlaylistNotFound(id));
            }
            Ok(())
        })
    }

    /// Turns a playlist smart (`Some`, dropping manual entries) or manual
    /// (`None`, starting empty), or changes a smart playlist's query.
    pub fn set_smart_query(&self, id: PlaylistId, query: Option<&Query>) -> Result<()> {
        let smart = query.map(serde_json::to_string).transpose()?;
        self.in_tx(|c| {
            require(c, id)?;
            c.execute(
                "UPDATE playlists SET smart_query = ?2 WHERE id = ?1",
                params![id.0, smart],
            )?;
            c.execute("DELETE FROM playlist_items WHERE playlist_id = ?1", [id.0])?;
            Ok(())
        })
    }

    /// Deletes a playlist (the videos stay).
    pub fn delete_playlist(&self, id: PlaylistId) -> Result<()> {
        self.with(|c| {
            if c.execute("DELETE FROM playlists WHERE id = ?1", [id.0])? == 0 {
                return Err(Error::PlaylistNotFound(id));
            }
            Ok(())
        })
    }

    /// The videos of a playlist in order. Smart playlists run their query.
    pub fn playlist_items(&self, id: PlaylistId) -> Result<Vec<MediaRecord>> {
        self.with(|c| match require(c, id)?.smart_query {
            Some(q) => search_conn(c, &q),
            None => get_many(c, &manual_ids(c, id)?),
        })
    }

    /// Appends a video to a manual playlist (duplicates allowed).
    pub fn add_to_playlist(&self, id: PlaylistId, media: MediaId) -> Result<()> {
        self.edit_items(id, |items| {
            items.push(media);
            Ok(())
        })
    }

    /// Inserts a video at `index` (clamped to the end).
    pub fn insert_into_playlist(&self, id: PlaylistId, index: usize, media: MediaId) -> Result<()> {
        self.edit_items(id, |items| {
            items.insert(index.min(items.len()), media);
            Ok(())
        })
    }

    /// Removes the entry at `index`.
    pub fn remove_from_playlist(&self, id: PlaylistId, index: usize) -> Result<()> {
        self.edit_items(id, |items| {
            if index >= items.len() {
                return Err(Error::InvalidArgument(format!(
                    "index {index} outside playlist of {}",
                    items.len()
                )));
            }
            items.remove(index);
            Ok(())
        })
    }

    /// Moves the entry at `from` so it ends up at `to`.
    pub fn move_playlist_item(&self, id: PlaylistId, from: usize, to: usize) -> Result<()> {
        self.edit_items(id, |items| {
            if from >= items.len() || to >= items.len() {
                return Err(Error::InvalidArgument(format!(
                    "move {from} -> {to} outside playlist of {}",
                    items.len()
                )));
            }
            let m = items.remove(from);
            items.insert(to, m);
            Ok(())
        })
    }

    /// Replaces the entries of a manual playlist.
    pub fn set_playlist_items(&self, id: PlaylistId, items: &[MediaId]) -> Result<()> {
        self.edit_items(id, |current| {
            *current = items.to_vec();
            Ok(())
        })
    }

    fn edit_items(
        &self,
        id: PlaylistId,
        f: impl FnOnce(&mut Vec<MediaId>) -> Result<()>,
    ) -> Result<()> {
        self.in_tx(|c| {
            require_manual(c, id)?;
            let mut items = manual_ids(c, id)?;
            f(&mut items)?;
            write_ids(c, id, &items)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::MediaUpsert;
    use crate::query::Sort;

    fn setup() -> (Library, Vec<MediaId>) {
        let lib = Library::open_in_memory().unwrap();
        let ids = ["a", "b", "c", "d"]
            .iter()
            .map(|n| {
                lib.upsert(&MediaUpsert::new(format!("/v/{n}.mp4"), "local"))
                    .unwrap()
                    .id
            })
            .collect();
        (lib, ids)
    }

    fn items(lib: &Library, p: PlaylistId) -> Vec<MediaId> {
        lib.playlist_items(p)
            .unwrap()
            .iter()
            .map(|r| r.id)
            .collect()
    }

    #[test]
    fn manual_playlist_crud_and_reorder() {
        let (lib, m) = setup();
        let p = lib.create_playlist("  Evening ").unwrap();
        assert!(lib.create_playlist("   ").is_err());
        lib.add_to_playlist(p, m[0]).unwrap();
        lib.add_to_playlist(p, m[1]).unwrap();
        lib.add_to_playlist(p, m[2]).unwrap();
        lib.insert_into_playlist(p, 0, m[3]).unwrap();
        assert_eq!(items(&lib, p), vec![m[3], m[0], m[1], m[2]]);
        lib.move_playlist_item(p, 0, 3).unwrap();
        assert_eq!(items(&lib, p), vec![m[0], m[1], m[2], m[3]]);
        lib.move_playlist_item(p, 2, 0).unwrap();
        assert_eq!(items(&lib, p), vec![m[2], m[0], m[1], m[3]]);
        lib.remove_from_playlist(p, 1).unwrap();
        assert_eq!(items(&lib, p), vec![m[2], m[1], m[3]]);
        assert!(lib.remove_from_playlist(p, 9).is_err());
        assert!(lib.move_playlist_item(p, 0, 9).is_err());
        assert!(lib.add_to_playlist(p, MediaId(999)).is_err());
        assert_eq!(
            items(&lib, p),
            vec![m[2], m[1], m[3]],
            "failed edit rolled back"
        );
        lib.set_playlist_items(p, &[m[1], m[1]]).unwrap();
        assert_eq!(items(&lib, p), vec![m[1], m[1]]);

        let pl = lib.playlist(p).unwrap().unwrap();
        assert_eq!((pl.name.as_str(), pl.item_count), ("Evening", 2));
        lib.rename_playlist(p, "Night").unwrap();
        let q = lib.create_playlist("aardvark").unwrap();
        let names: Vec<String> = lib
            .playlists()
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, vec!["aardvark", "Night"]);

        // Removing a video removes its entries.
        lib.remove_media(m[1]).unwrap();
        assert!(items(&lib, p).is_empty());
        lib.delete_playlist(q).unwrap();
        assert!(matches!(
            lib.delete_playlist(q),
            Err(Error::PlaylistNotFound(_))
        ));
        assert!(lib.playlist(q).unwrap().is_none());
    }

    #[test]
    fn smart_playlist_evaluates_query() {
        let (lib, m) = setup();
        lib.set_favorite(m[1], true).unwrap();
        lib.set_favorite(m[3], true).unwrap();
        lib.set_rating(m[3], 5).unwrap();
        let q = Query {
            favorites_only: true,
            sort: Sort::Rating,
            ..Default::default()
        };
        let p = lib.create_smart_playlist("Favs", &q).unwrap();
        assert_eq!(items(&lib, p), vec![m[3], m[1]]);
        let pl = lib.playlist(p).unwrap().unwrap();
        assert!(pl.is_smart());
        assert_eq!(pl.smart_query.as_ref(), Some(&q));
        assert_eq!(pl.item_count, 2);
        // Live: new favourites appear.
        lib.set_favorite(m[0], true).unwrap();
        assert_eq!(items(&lib, p).len(), 3);
        // Limits apply to the count too.
        let limited = Query {
            limit: Some(1),
            ..q.clone()
        };
        lib.set_smart_query(p, Some(&limited)).unwrap();
        assert_eq!(lib.playlist(p).unwrap().unwrap().item_count, 1);
        // Smart playlists cannot be edited by hand.
        assert!(matches!(
            lib.add_to_playlist(p, m[2]),
            Err(Error::InvalidArgument(_))
        ));
        // Turning it manual starts empty and allows edits.
        lib.set_smart_query(p, None).unwrap();
        assert!(items(&lib, p).is_empty());
        lib.add_to_playlist(p, m[2]).unwrap();
        assert_eq!(items(&lib, p), vec![m[2]]);
    }
}
