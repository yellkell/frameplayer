//! Manual and smart playlists.

use crate::db::{now, Library};
use crate::error::{LibraryError, Result};
use crate::model::{Item, Playlist};
use crate::query::ItemQuery;
use rusqlite::{params, OptionalExtension};

impl Library {
    pub fn create_playlist(&self, name: &str) -> Result<i64> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO playlists (name, created_at) VALUES (?1, ?2)",
            params![name, now()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// A playlist whose contents are whatever `query` matches right now.
    pub fn create_smart_playlist(&self, name: &str, query: &ItemQuery) -> Result<i64> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO playlists (name, smart_query_json, created_at) VALUES (?1, ?2, ?3)",
            params![name, serde_json::to_string(query)?, now()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn update_smart_query(&self, id: i64, query: &ItemQuery) -> Result<()> {
        let n = self.conn().execute("UPDATE playlists SET smart_query_json = ?2 WHERE id = ?1 AND smart_query_json IS NOT NULL", params![id, serde_json::to_string(query)?])?;
        if n == 0 {
            return Err(LibraryError::NotFound(format!("smart playlist {id}")));
        }
        Ok(())
    }

    pub fn rename_playlist(&self, id: i64, name: &str) -> Result<bool> {
        Ok(self.conn().execute(
            "UPDATE playlists SET name = ?2 WHERE id = ?1",
            params![id, name],
        )? > 0)
    }

    pub fn delete_playlist(&self, id: i64) -> Result<bool> {
        Ok(self
            .conn()
            .execute("DELETE FROM playlists WHERE id = ?1", [id])?
            > 0)
    }

    pub fn playlists(&self) -> Result<Vec<Playlist>> {
        let conn = self.conn();
        let mut st = conn.prepare("SELECT id, name, smart_query_json, created_at FROM playlists ORDER BY name COLLATE NOCASE")?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, name, q, created_at) = row?;
            out.push(Playlist {
                id,
                name,
                query: q.map(|q| serde_json::from_str(&q)).transpose()?,
                created_at,
            });
        }
        Ok(out)
    }

    pub fn playlist(&self, id: i64) -> Result<Option<Playlist>> {
        Ok(self.playlists()?.into_iter().find(|p| p.id == id))
    }

    fn is_smart(&self, id: i64) -> Result<bool> {
        let q: Option<Option<String>> = self
            .conn()
            .query_row(
                "SELECT smart_query_json FROM playlists WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        match q {
            None => Err(LibraryError::NotFound(format!("playlist {id}"))),
            Some(q) => Ok(q.is_some()),
        }
    }

    /// Append an item to a manual playlist (no-op if already present).
    pub fn add_to_playlist(&self, playlist: i64, item: i64) -> Result<()> {
        if self.is_smart(playlist)? {
            return Err(LibraryError::Invalid(
                "cannot add items to a smart playlist".into(),
            ));
        }
        self.conn().execute(
            "INSERT OR IGNORE INTO playlist_items (playlist_id, item_id, position)
             VALUES (?1, ?2, (SELECT COALESCE(MAX(position), -1) + 1 FROM playlist_items WHERE playlist_id = ?1))",
            params![playlist, item],
        )?;
        Ok(())
    }

    pub fn remove_from_playlist(&self, playlist: i64, item: i64) -> Result<()> {
        self.conn().execute(
            "DELETE FROM playlist_items WHERE playlist_id = ?1 AND item_id = ?2",
            params![playlist, item],
        )?;
        Ok(())
    }

    /// Move an item to `new_index` within a manual playlist.
    pub fn reorder_playlist(&self, playlist: i64, item: i64, new_index: usize) -> Result<()> {
        let mut ids: Vec<i64> = {
            let conn = self.conn();
            let mut st = conn.prepare(
                "SELECT item_id FROM playlist_items WHERE playlist_id = ?1 ORDER BY position",
            )?;
            let rows = st.query_map([playlist], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let Some(pos) = ids.iter().position(|&i| i == item) else {
            return Err(LibraryError::NotFound(format!(
                "item {item} in playlist {playlist}"
            )));
        };
        ids.remove(pos);
        ids.insert(new_index.min(ids.len()), item);
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        for (i, id) in ids.iter().enumerate() {
            tx.execute(
                "UPDATE playlist_items SET position = ?3 WHERE playlist_id = ?1 AND item_id = ?2",
                params![playlist, id, i as i64],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Items of a playlist: stored order for manual playlists, query
    /// results for smart ones.
    pub fn playlist_items(&self, id: i64) -> Result<Vec<Item>> {
        let pl = self
            .playlist(id)?
            .ok_or_else(|| LibraryError::NotFound(format!("playlist {id}")))?;
        match pl.query {
            Some(q) => self.query_items(&q),
            None => self.select_items("JOIN playlist_items pi ON pi.item_id = i.id WHERE pi.playlist_id = ?1 ORDER BY pi.position", [id]),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::items::tests::new_item;
    use crate::query::{ItemQuery, Sort};
    use crate::{Library, ProjectionKind};

    #[test]
    fn manual_playlists() {
        let lib = Library::open_in_memory().unwrap();
        let a = lib.upsert_item(&new_item("file:///a.mp4")).unwrap();
        let b = lib.upsert_item(&new_item("file:///b.mp4")).unwrap();
        let c = lib.upsert_item(&new_item("file:///c.mp4")).unwrap();
        let p = lib.create_playlist("Mix").unwrap();
        for id in [a, b, c, a] {
            lib.add_to_playlist(p, id).unwrap();
        }
        let ids = |lib: &Library| {
            lib.playlist_items(p)
                .unwrap()
                .iter()
                .map(|i| i.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&lib), [a, b, c]);
        lib.reorder_playlist(p, c, 0).unwrap();
        assert_eq!(ids(&lib), [c, a, b]);
        lib.remove_from_playlist(p, a).unwrap();
        assert_eq!(ids(&lib), [c, b]);
        // Deleting an item removes it from playlists.
        lib.delete_item(b).unwrap();
        assert_eq!(ids(&lib), [c]);
        assert!(lib.rename_playlist(p, "Renamed").unwrap());
        assert_eq!(lib.playlists().unwrap()[0].name, "Renamed");
        assert!(lib.delete_playlist(p).unwrap());
        assert!(lib.playlist_items(p).is_err());
    }

    #[test]
    fn smart_playlists() {
        let lib = Library::open_in_memory().unwrap();
        let a = lib.upsert_item(&new_item("file:///a_180_LR.mp4")).unwrap();
        lib.upsert_item(&new_item("file:///b.mp4")).unwrap();
        let q = ItemQuery {
            projections: vec![ProjectionKind::Equirect180],
            sort: Sort::Name,
            ..Default::default()
        };
        let p = lib.create_smart_playlist("VR 180", &q).unwrap();
        assert_eq!(lib.playlist(p).unwrap().unwrap().query, Some(q));
        assert_eq!(
            lib.playlist_items(p)
                .unwrap()
                .iter()
                .map(|i| i.id)
                .collect::<Vec<_>>(),
            [a]
        );
        // Evaluated live.
        let c = lib.upsert_item(&new_item("file:///c_180_TB.mp4")).unwrap();
        assert_eq!(lib.playlist_items(p).unwrap().len(), 2);
        assert!(lib.add_to_playlist(p, c).is_err());
        lib.update_smart_query(
            p,
            &ItemQuery {
                favourite: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(lib.playlist_items(p).unwrap().is_empty());
        let m = lib.create_playlist("manual").unwrap();
        assert!(lib.update_smart_query(m, &ItemQuery::default()).is_err());
    }
}
