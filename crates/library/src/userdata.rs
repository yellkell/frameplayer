//! Per-item user data: ratings, favourites, resume points, watch history,
//! bookmarks and tags.

use crate::db::{now, Library};
use crate::error::{LibraryError, Result};
use crate::model::{Bookmark, HistoryEntry};
use fp_core::MediaTime;
use rusqlite::{params, OptionalExtension};

/// Positions this close to the end count as "finished" and clear resume.
const FINISHED_MARGIN: MediaTime = MediaTime(30_000_000);
/// Positions this close to the start aren't worth resuming.
const MIN_RESUME: MediaTime = MediaTime(10_000_000);

impl Library {
    /// Set a 0–5 rating (half stars allowed); `None` clears it.
    pub fn set_rating(&self, id: i64, rating: Option<f32>) -> Result<()> {
        match rating {
            Some(r) if !(0.0..=5.0).contains(&r) => {
                Err(LibraryError::Invalid(format!("rating {r} outside 0..=5")))
            }
            Some(r) => {
                self.conn().execute("INSERT INTO ratings (item_id, rating) VALUES (?1, ?2) ON CONFLICT(item_id) DO UPDATE SET rating = excluded.rating", params![id, r as f64])?;
                Ok(())
            }
            None => {
                self.conn()
                    .execute("DELETE FROM ratings WHERE item_id = ?1", [id])?;
                Ok(())
            }
        }
    }

    pub fn set_favourite(&self, id: i64, favourite: bool) -> Result<()> {
        if favourite {
            self.conn().execute(
                "INSERT OR IGNORE INTO favourites (item_id, added_at) VALUES (?1, ?2)",
                params![id, now()],
            )?;
        } else {
            self.conn()
                .execute("DELETE FROM favourites WHERE item_id = ?1", [id])?;
        }
        Ok(())
    }

    /// Save where playback stopped. Positions near the start are ignored
    /// and positions near the end clear the resume point (the video was
    /// finished), so "Continue watching" stays meaningful.
    pub fn set_resume(&self, id: i64, position: MediaTime) -> Result<()> {
        let duration: Option<i64> = self
            .conn()
            .query_row("SELECT duration_us FROM items WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .optional()?
            .flatten();
        let near_end = duration.is_some_and(|d| position.0 >= d - FINISHED_MARGIN.0);
        if position < MIN_RESUME || near_end {
            return self.clear_resume(id);
        }
        self.conn().execute(
            "INSERT INTO resume_points (item_id, position_us, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(item_id) DO UPDATE SET position_us = excluded.position_us, updated_at = excluded.updated_at",
            params![id, position.0, now()],
        )?;
        Ok(())
    }

    pub fn clear_resume(&self, id: i64) -> Result<()> {
        self.conn()
            .execute("DELETE FROM resume_points WHERE item_id = ?1", [id])?;
        Ok(())
    }

    pub fn resume_point(&self, id: i64) -> Result<Option<MediaTime>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT position_us FROM resume_points WHERE item_id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?
            .map(MediaTime))
    }

    /// Items with a resume point, most recently watched first.
    pub fn continue_watching(&self, limit: usize) -> Result<Vec<crate::Item>> {
        self.select_items(
            "WHERE rp.position_us IS NOT NULL ORDER BY rp.updated_at DESC LIMIT ?1",
            [limit as i64],
        )
    }

    /// Append to watch history (call when playback stops).
    pub fn record_watch(
        &self,
        id: i64,
        position: Option<MediaTime>,
        completed: bool,
    ) -> Result<()> {
        self.record_watch_at(id, position, completed, now())
    }

    pub(crate) fn record_watch_at(
        &self,
        id: i64,
        position: Option<MediaTime>,
        completed: bool,
        at: i64,
    ) -> Result<()> {
        self.conn().execute("INSERT INTO watch_history (item_id, watched_at, position_us, completed) VALUES (?1, ?2, ?3, ?4)", params![id, at, position.map(|p| p.0), completed])?;
        Ok(())
    }

    /// Most recent history entries across the library.
    pub fn history(&self, limit: usize) -> Result<Vec<HistoryEntry>> {
        let conn = self.conn();
        let mut st = conn.prepare("SELECT item_id, watched_at, position_us, completed FROM watch_history ORDER BY watched_at DESC, id DESC LIMIT ?1")?;
        let rows = st.query_map([limit as i64], |r| {
            Ok(HistoryEntry {
                item_id: r.get(0)?,
                watched_at: r.get(1)?,
                position: r.get::<_, Option<i64>>(2)?.map(MediaTime),
                completed: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn clear_history(&self) -> Result<()> {
        self.conn().execute("DELETE FROM watch_history", [])?;
        Ok(())
    }

    // ---- bookmarks -----------------------------------------------------

    pub fn add_bookmark(&self, id: i64, position: MediaTime, name: &str) -> Result<i64> {
        let conn = self.conn();
        conn.execute("INSERT INTO bookmarks (item_id, position_us, name, created_at) VALUES (?1, ?2, ?3, ?4)", params![id, position.0, name, now()])?;
        Ok(conn.last_insert_rowid())
    }

    /// Bookmarks for an item, in timeline order.
    pub fn bookmarks(&self, id: i64) -> Result<Vec<Bookmark>> {
        let conn = self.conn();
        let mut st = conn.prepare("SELECT id, item_id, position_us, name, created_at FROM bookmarks WHERE item_id = ?1 ORDER BY position_us")?;
        let rows = st.query_map([id], |r| {
            Ok(Bookmark {
                id: r.get(0)?,
                item_id: r.get(1)?,
                position: MediaTime(r.get(2)?),
                name: r.get(3)?,
                created_at: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn rename_bookmark(&self, bookmark_id: i64, name: &str) -> Result<bool> {
        Ok(self.conn().execute(
            "UPDATE bookmarks SET name = ?2 WHERE id = ?1",
            params![bookmark_id, name],
        )? > 0)
    }

    pub fn delete_bookmark(&self, bookmark_id: i64) -> Result<bool> {
        Ok(self
            .conn()
            .execute("DELETE FROM bookmarks WHERE id = ?1", [bookmark_id])?
            > 0)
    }

    /// The next bookmark strictly after `after` (for "jump to next").
    pub fn next_bookmark(&self, id: i64, after: MediaTime) -> Result<Option<Bookmark>> {
        Ok(self.bookmarks(id)?.into_iter().find(|b| b.position > after))
    }

    // ---- tags ----------------------------------------------------------

    pub fn add_tag(&self, id: i64, tag: &str) -> Result<()> {
        let tag = tag.trim();
        if tag.is_empty() {
            return Err(LibraryError::Invalid("empty tag".into()));
        }
        let conn = self.conn();
        conn.execute("INSERT OR IGNORE INTO tags (name) VALUES (?1)", [tag])?;
        conn.execute("INSERT OR IGNORE INTO item_tags (item_id, tag_id) SELECT ?1, id FROM tags WHERE name = ?2", params![id, tag])?;
        Ok(())
    }

    pub fn remove_tag(&self, id: i64, tag: &str) -> Result<()> {
        let conn = self.conn();
        conn.execute("DELETE FROM item_tags WHERE item_id = ?1 AND tag_id = (SELECT id FROM tags WHERE name = ?2)", params![id, tag])?;
        conn.execute(
            "DELETE FROM tags WHERE id NOT IN (SELECT tag_id FROM item_tags)",
            [],
        )?;
        Ok(())
    }

    /// All tags with their item counts, alphabetical.
    pub fn all_tags(&self) -> Result<Vec<(String, u32)>> {
        let conn = self.conn();
        let mut st = conn.prepare("SELECT t.name, COUNT(it.item_id) FROM tags t LEFT JOIN item_tags it ON it.tag_id = t.id GROUP BY t.id ORDER BY t.name COLLATE NOCASE")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u32)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

#[cfg(test)]
mod tests {
    use crate::items::tests::new_item;
    use crate::Library;
    use fp_core::MediaTime;

    #[test]
    fn ratings_favourites() {
        let lib = Library::open_in_memory().unwrap();
        let id = lib.upsert_item(&new_item("file:///a.mp4")).unwrap();
        lib.set_rating(id, Some(4.5)).unwrap();
        lib.set_favourite(id, true).unwrap();
        lib.set_favourite(id, true).unwrap();
        let it = lib.item(id).unwrap().unwrap();
        assert_eq!((it.rating, it.favourite), (Some(4.5), true));
        assert!(lib.set_rating(id, Some(6.0)).is_err());
        lib.set_rating(id, None).unwrap();
        lib.set_favourite(id, false).unwrap();
        let it = lib.item(id).unwrap().unwrap();
        assert_eq!((it.rating, it.favourite), (None, false));
    }

    #[test]
    fn resume_rules() {
        let lib = Library::open_in_memory().unwrap();
        let mut ni = new_item("file:///a.mp4");
        ni.duration = Some(MediaTime::from_millis(600_000));
        let id = lib.upsert_item(&ni).unwrap();
        lib.set_resume(id, MediaTime::from_millis(120_000)).unwrap();
        assert_eq!(
            lib.resume_point(id).unwrap(),
            Some(MediaTime::from_millis(120_000))
        );
        assert_eq!(lib.continue_watching(10).unwrap().len(), 1);
        lib.set_resume(id, MediaTime::from_millis(590_000)).unwrap();
        assert_eq!(lib.resume_point(id).unwrap(), None);
        lib.set_resume(id, MediaTime::from_millis(5_000)).unwrap();
        assert_eq!(lib.resume_point(id).unwrap(), None);
    }

    #[test]
    fn history_and_bookmarks() {
        let lib = Library::open_in_memory().unwrap();
        let id = lib.upsert_item(&new_item("file:///a.mp4")).unwrap();
        lib.record_watch_at(id, Some(MediaTime::from_millis(1000)), false, 100)
            .unwrap();
        lib.record_watch_at(id, None, true, 200).unwrap();
        let h = lib.history(10).unwrap();
        assert_eq!(h.len(), 2);
        assert!(h[0].completed && h[0].watched_at == 200);
        let it = lib.item(id).unwrap().unwrap();
        assert_eq!((it.watch_count, it.last_watched), (2, Some(200)));

        let b2 = lib
            .add_bookmark(id, MediaTime::from_millis(60_000), "Second")
            .unwrap();
        let b1 = lib
            .add_bookmark(id, MediaTime::from_millis(10_000), "First")
            .unwrap();
        let bm = lib.bookmarks(id).unwrap();
        assert_eq!(bm.iter().map(|b| b.id).collect::<Vec<_>>(), [b1, b2]);
        assert_eq!(
            lib.next_bookmark(id, MediaTime::from_millis(10_000))
                .unwrap()
                .unwrap()
                .id,
            b2
        );
        assert!(lib.rename_bookmark(b1, "Intro").unwrap());
        assert_eq!(lib.bookmarks(id).unwrap()[0].name, "Intro");
        assert!(lib.delete_bookmark(b2).unwrap());
        assert_eq!(lib.bookmarks(id).unwrap().len(), 1);
        lib.clear_history().unwrap();
        assert!(lib.history(10).unwrap().is_empty());
    }

    #[test]
    fn tags() {
        let lib = Library::open_in_memory().unwrap();
        let a = lib.upsert_item(&new_item("file:///a.mp4")).unwrap();
        let b = lib.upsert_item(&new_item("file:///b.mp4")).unwrap();
        lib.add_tag(a, "Outdoor").unwrap();
        lib.add_tag(b, "outdoor").unwrap();
        lib.add_tag(a, "POV").unwrap();
        assert!(lib.add_tag(a, "  ").is_err());
        assert_eq!(
            lib.all_tags().unwrap(),
            vec![("Outdoor".to_string(), 2), ("POV".to_string(), 1)]
        );
        assert_eq!(lib.item(a).unwrap().unwrap().tags, vec!["Outdoor", "POV"]);
        lib.remove_tag(a, "POV").unwrap();
        assert_eq!(lib.all_tags().unwrap().len(), 1);
    }
}
