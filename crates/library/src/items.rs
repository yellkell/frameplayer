//! Item rows: upsert, lookup, probe results, detection, view overrides,
//! thumbnails and scripts.

use crate::db::{now, Library};
use crate::error::{LibraryError, Result};
use crate::funscript::ScriptRef;
use crate::indexer::SpriteInfo;
use crate::model::*;
use fp_core::{ColorTransfer, MediaInfo, MediaTime, Projection, StereoMode, ViewSettings};
use rusqlite::{params, OptionalExtension, Row};

pub(crate) const ITEM_SELECT: &str = "SELECT i.id, i.source_id, i.uri, i.path, i.title, i.size, i.mtime, i.content_hash, i.duration_us,
    i.width, i.height, i.codec, i.fps, i.hdr, i.projection_json, i.projection_kind, i.stereo, i.swap_eyes, i.detect_source,
    i.thumbnail_path, i.sprite_path, i.remote_thumbnail, i.added_at, i.probed,
    r.rating, f.item_id IS NOT NULL, rp.position_us,
    (SELECT MAX(w.watched_at) FROM watch_history w WHERE w.item_id = i.id),
    (SELECT COUNT(*) FROM watch_history w WHERE w.item_id = i.id),
    (SELECT GROUP_CONCAT(t.name, char(31)) FROM item_tags it JOIN tags t ON t.id = it.tag_id WHERE it.item_id = i.id),
    EXISTS (SELECT 1 FROM scripts s WHERE s.item_id = i.id)
  FROM items i
  LEFT JOIN ratings r ON r.item_id = i.id
  LEFT JOIN favourites f ON f.item_id = i.id
  LEFT JOIN resume_points rp ON rp.item_id = i.id";

pub(crate) fn item_from_row(r: &Row<'_>) -> rusqlite::Result<Item> {
    let projection_json: Option<String> = r.get(14)?;
    let kind: Option<String> = r.get(15)?;
    let stereo: Option<String> = r.get(16)?;
    let codec: Option<String> = r.get(11)?;
    let tags: Option<String> = r.get(29)?;
    let mut tags: Vec<String> = tags
        .map(|t| t.split('\u{1f}').map(str::to_string).collect())
        .unwrap_or_default();
    tags.sort_by_key(|t| t.to_lowercase());
    let detect: String = r.get(18)?;
    Ok(Item {
        id: r.get(0)?,
        source_id: r.get(1)?,
        uri: r.get(2)?,
        path: r.get(3)?,
        title: r.get(4)?,
        size: r.get::<_, Option<i64>>(5)?.map(|v| v as u64),
        mtime: r.get(6)?,
        content_hash: r.get(7)?,
        duration: r.get::<_, Option<i64>>(8)?.map(MediaTime),
        width: r.get(9)?,
        height: r.get(10)?,
        codec: codec.as_deref().and_then(parse_codec),
        fps: r.get(12)?,
        hdr: r.get(13)?,
        projection: projection_json.and_then(|j| serde_json::from_str(&j).ok()),
        projection_kind: kind
            .and_then(|k| serde_json::from_value(serde_json::Value::String(k)).ok()),
        stereo: stereo.as_deref().and_then(parse_stereo),
        swap_eyes: r.get(17)?,
        detect_source: DetectSource::parse(&detect),
        thumbnail_path: r.get(19)?,
        sprite_path: r.get(20)?,
        remote_thumbnail: r.get(21)?,
        added_at: r.get(22)?,
        probed: r.get(23)?,
        rating: r.get::<_, Option<f64>>(24)?.map(|v| v as f32),
        favourite: r.get(25)?,
        resume: r.get::<_, Option<i64>>(26)?.map(MediaTime),
        last_watched: r.get(27)?,
        watch_count: r.get::<_, i64>(28)? as u32,
        tags,
        has_script: r.get(30)?,
    })
}

/// Minimal identity columns used by the indexer.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ItemKey {
    pub id: i64,
    pub uri: String,
    pub size: Option<u64>,
    pub mtime: Option<i64>,
    pub content_hash: Option<String>,
}

impl Library {
    /// Insert or update (by URI) an item. Returns its id. Filename-based
    /// projection detection is applied unless something better is known.
    pub fn upsert_item(&self, it: &NewItem) -> Result<i64> {
        let t = now();
        let id: i64 = self.conn().query_row(
            "INSERT INTO items (source_id, uri, path, title, size, mtime, content_hash, duration_us, remote_thumbnail, added_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)
             ON CONFLICT(uri) DO UPDATE SET source_id=excluded.source_id, path=excluded.path, title=excluded.title,
                size=excluded.size, mtime=excluded.mtime, content_hash=COALESCE(excluded.content_hash, items.content_hash),
                duration_us=COALESCE(items.duration_us, excluded.duration_us),
                remote_thumbnail=COALESCE(excluded.remote_thumbnail, items.remote_thumbnail), updated_at=excluded.updated_at
             RETURNING id",
            params![it.source_id, it.uri, it.path, it.title, it.size.map(|s| s as i64), it.mtime, it.content_hash, it.duration.map(|d| d.0), it.remote_thumbnail, t],
            |r| r.get(0),
        )?;
        self.apply_filename_detection(id, &it.path)?;
        Ok(id)
    }

    fn apply_filename_detection(&self, id: i64, path: &str) -> Result<()> {
        let d = fp_core::detect::from_filename(path);
        match (d.projection, d.stereo) {
            (None, None) => {
                // Make sure "default" items still get a kind for filtering.
                self.conn().execute("UPDATE items SET projection_kind = COALESCE(projection_kind, 'flat'), stereo = COALESCE(stereo, 'mono') WHERE id = ?1", [id])?;
                Ok(())
            }
            (p, s) => self.set_detection(
                id,
                &p.unwrap_or_default(),
                s.unwrap_or_default(),
                d.swap_eyes,
                DetectSource::Filename,
            ),
        }
    }

    /// Record a detected projection/stereo if `source` ranks at least as
    /// high as what's stored (user > container > feed > filename).
    pub fn set_detection(
        &self,
        id: i64,
        projection: &Projection,
        stereo: StereoMode,
        swap_eyes: bool,
        source: DetectSource,
    ) -> Result<()> {
        let conn = self.conn();
        let cur: Option<String> = conn
            .query_row("SELECT detect_source FROM items WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .optional()?;
        let cur = cur.ok_or_else(|| LibraryError::NotFound(format!("item {id}")))?;
        if DetectSource::parse(&cur) > source {
            return Ok(());
        }
        conn.execute(
            "UPDATE items SET projection_json=?2, projection_kind=?3, stereo=?4, swap_eyes=?5, detect_source=?6 WHERE id=?1",
            params![id, serde_json::to_string(projection)?, ProjectionKind::of(projection).as_str(), stereo_str(stereo), swap_eyes, source.as_str()],
        )?;
        Ok(())
    }

    /// Point an existing item at a new location (file moved/renamed),
    /// keeping its id and therefore all user data.
    pub fn move_item(&self, id: i64, to: &NewItem) -> Result<()> {
        self.conn().execute(
            "UPDATE items SET source_id=?2, uri=?3, path=?4, title=?5, size=?6, mtime=?7, content_hash=COALESCE(?8, content_hash), updated_at=?9 WHERE id=?1",
            params![id, to.source_id, to.uri, to.path, to.title, to.size.map(|s| s as i64), to.mtime, to.content_hash, now()],
        )?;
        self.apply_filename_detection(id, &to.path)
    }

    pub fn delete_item(&self, id: i64) -> Result<bool> {
        Ok(self
            .conn()
            .execute("DELETE FROM items WHERE id = ?1", [id])?
            > 0)
    }

    pub fn item(&self, id: i64) -> Result<Option<Item>> {
        let conn = self.conn();
        let sql = format!("{ITEM_SELECT} WHERE i.id = ?1");
        Ok(conn.query_row(&sql, [id], item_from_row).optional()?)
    }

    pub fn item_by_uri(&self, uri: &str) -> Result<Option<Item>> {
        let conn = self.conn();
        let sql = format!("{ITEM_SELECT} WHERE i.uri = ?1");
        Ok(conn.query_row(&sql, [uri], item_from_row).optional()?)
    }

    pub fn items_by_hash(&self, hash: &str) -> Result<Vec<Item>> {
        self.select_items("WHERE i.content_hash = ?1", [hash])
    }

    /// All items, optionally restricted to one source.
    pub fn items(&self, source_id: Option<&str>) -> Result<Vec<Item>> {
        match source_id {
            Some(s) => self.select_items(
                "WHERE i.source_id = ?1 ORDER BY i.title COLLATE NOCASE",
                [s],
            ),
            None => self.select_items("ORDER BY i.title COLLATE NOCASE", []),
        }
    }

    pub(crate) fn select_items<P: rusqlite::Params>(&self, tail: &str, p: P) -> Result<Vec<Item>> {
        let conn = self.conn();
        let mut st = conn.prepare(&format!("{ITEM_SELECT} {tail}"))?;
        let rows = st.query_map(p, item_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn item_count(&self) -> Result<u64> {
        Ok(self
            .conn()
            .query_row("SELECT COUNT(*) FROM items", [], |r| r.get::<_, i64>(0))? as u64)
    }

    pub(crate) fn item_keys(&self, source_id: &str) -> Result<Vec<ItemKey>> {
        let conn = self.conn();
        let mut st = conn
            .prepare("SELECT id, uri, size, mtime, content_hash FROM items WHERE source_id = ?1")?;
        let rows = st.query_map([source_id], |r| {
            Ok(ItemKey {
                id: r.get(0)?,
                uri: r.get(1)?,
                size: r.get::<_, Option<i64>>(2)?.map(|v| v as u64),
                mtime: r.get(3)?,
                content_hash: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Store probe results; container-signalled projection beats filename
    /// detection.
    pub fn apply_media_info(&self, id: i64, info: &MediaInfo) -> Result<()> {
        let v = info.primary_video();
        self.conn().execute(
            "UPDATE items SET duration_us=COALESCE(?2, duration_us), width=COALESCE(?3, width), height=COALESCE(?4, height),
                codec=COALESCE(?5, codec), fps=COALESCE(?6, fps), hdr=?7, probed=1, updated_at=?8 WHERE id=?1",
            params![
                id,
                info.duration.map(|d| d.0),
                v.map(|v| v.width),
                v.map(|v| v.height),
                v.map(|v| codec_str(v.codec)),
                v.map(|v| v.fps).filter(|f| *f > 0.0),
                v.is_some_and(|v| v.transfer != ColorTransfer::Sdr),
                now()
            ],
        )?;
        if let Some(v) = v {
            if v.signalled_projection.is_some() || v.signalled_stereo.is_some() {
                let item = self
                    .item(id)?
                    .ok_or_else(|| LibraryError::NotFound(format!("item {id}")))?;
                let fname = fp_core::detect::from_filename(&item.path);
                let (p, s, swap) = fp_core::detect::merge(
                    None,
                    (v.signalled_projection.clone(), v.signalled_stereo),
                    &fname,
                );
                self.set_detection(id, &p, s, swap, DetectSource::Container)?;
            }
        }
        Ok(())
    }

    /// Apply a DeoVR per-video document: projection, tags (categories and
    /// actors), funscripts, duration and thumbnail.
    pub fn apply_deovr_video(&self, id: i64, v: &fp_sources::deovr::DeoVrVideo) -> Result<()> {
        if v.screen_type.is_some() || v.stereo_mode.is_some() || v.is3d.is_some() {
            let (p, s) = v.projection();
            self.set_detection(id, &p, s, false, DetectSource::Feed)?;
        }
        if let Some(len) = v.video_length.filter(|l| *l > 0.0) {
            self.conn().execute(
                "UPDATE items SET duration_us = ?2 WHERE id = ?1",
                params![id, MediaTime::from_secs_f64(len).0],
            )?;
        }
        if let Some(t) = v.thumbnail() {
            self.conn().execute(
                "UPDATE items SET remote_thumbnail = ?2 WHERE id = ?1",
                params![id, t],
            )?;
        }
        for tag in v.tags().into_iter().chain(v.actors()) {
            self.add_tag(id, &tag)?;
        }
        let scripts: Vec<ScriptRef> = v
            .scripts()
            .iter()
            .enumerate()
            .map(|(i, s)| ScriptRef {
                axis: crate::funscript::axis_from_name(&s.title).unwrap_or_else(|| {
                    if i == 0 {
                        "main".into()
                    } else {
                        format!("alt{i}")
                    }
                }),
                uri: s.url.clone(),
            })
            .collect();
        if !scripts.is_empty() {
            self.set_scripts(id, &scripts)?;
        }
        Ok(())
    }

    // ---- view overrides -------------------------------------------------

    /// Store a view override keyed by content hash and/or path.
    pub fn set_override(
        &self,
        content_hash: Option<&str>,
        path: Option<&str>,
        vs: &ViewSettings,
    ) -> Result<()> {
        if content_hash.is_none() && path.is_none() {
            return Err(LibraryError::Invalid(
                "override needs a content hash or a path".into(),
            ));
        }
        self.conn().execute(
            "INSERT INTO view_overrides (content_hash, path, settings_json, updated_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(content_hash, path) DO UPDATE SET settings_json=excluded.settings_json, updated_at=excluded.updated_at",
            params![content_hash.unwrap_or(""), path.unwrap_or(""), serde_json::to_string(vs)?, now()],
        )?;
        Ok(())
    }

    /// Look up an override: exact (hash, path), then same content anywhere
    /// (file moved), then same path (file replaced/re-encoded).
    pub fn get_override(
        &self,
        content_hash: Option<&str>,
        path: Option<&str>,
    ) -> Result<Option<ViewSettings>> {
        let conn = self.conn();
        let h = content_hash.unwrap_or("");
        let p = path.unwrap_or("");
        let q = |sql: &str, a: &[&dyn rusqlite::ToSql]| -> Result<Option<String>> {
            Ok(conn.query_row(sql, a, |r| r.get(0)).optional()?)
        };
        let mut found = q(
            "SELECT settings_json FROM view_overrides WHERE content_hash = ?1 AND path = ?2",
            &[&h, &p],
        )?;
        if found.is_none() && !h.is_empty() {
            found = q("SELECT settings_json FROM view_overrides WHERE content_hash = ?1 ORDER BY updated_at DESC LIMIT 1", &[&h])?;
        }
        if found.is_none() && !p.is_empty() {
            found = q("SELECT settings_json FROM view_overrides WHERE path = ?1 ORDER BY updated_at DESC LIMIT 1", &[&p])?;
        }
        Ok(match found {
            Some(j) => Some(serde_json::from_str(&j)?),
            None => None,
        })
    }

    /// All overrides as `(content_hash, path, settings)` (export).
    pub fn all_overrides(&self) -> Result<Vec<(String, String, ViewSettings)>> {
        let conn = self.conn();
        let mut st = conn.prepare(
            "SELECT content_hash, path, settings_json FROM view_overrides ORDER BY updated_at",
        )?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (h, p, j) = row?;
            out.push((h, p, serde_json::from_str(&j)?));
        }
        Ok(out)
    }

    /// Persist the user's view settings for an item (the "per-file override
    /// that persists" from the outline) and mark its detection as user-set.
    pub fn set_item_view_settings(&self, id: i64, vs: &ViewSettings) -> Result<()> {
        let item = self
            .item(id)?
            .ok_or_else(|| LibraryError::NotFound(format!("item {id}")))?;
        self.set_override(item.content_hash.as_deref(), Some(&item.uri), vs)?;
        self.set_detection(
            id,
            &vs.projection,
            vs.stereo,
            vs.swap_eyes,
            DetectSource::User,
        )
    }

    /// Forget a user override and fall back to detection.
    pub fn clear_item_view_settings(&self, id: i64) -> Result<()> {
        let item = self
            .item(id)?
            .ok_or_else(|| LibraryError::NotFound(format!("item {id}")))?;
        {
            let conn = self.conn();
            if let Some(h) = &item.content_hash {
                conn.execute("DELETE FROM view_overrides WHERE content_hash = ?1", [h])?;
            }
            conn.execute("DELETE FROM view_overrides WHERE path = ?1", [&item.uri])?;
            conn.execute("UPDATE items SET detect_source = 'default', projection_json = NULL, projection_kind = NULL, stereo = NULL, swap_eyes = 0 WHERE id = ?1", [id])?;
        }
        self.apply_filename_detection(id, &item.path)
    }

    /// Settings to play an item with: user override, else detection, else
    /// flat mono.
    pub fn view_settings(&self, id: i64) -> Result<ViewSettings> {
        let item = self
            .item(id)?
            .ok_or_else(|| LibraryError::NotFound(format!("item {id}")))?;
        if let Some(vs) = self.get_override(item.content_hash.as_deref(), Some(&item.uri))? {
            return Ok(vs);
        }
        Ok(ViewSettings {
            projection: item.projection.unwrap_or_default(),
            stereo: item.stereo.unwrap_or_default(),
            swap_eyes: item.swap_eyes,
            ..Default::default()
        })
    }

    // ---- thumbnails ------------------------------------------------------

    pub fn set_thumbnails(
        &self,
        id: i64,
        thumbnail: Option<&str>,
        sprite: Option<&str>,
        sprite_info: Option<&SpriteInfo>,
    ) -> Result<()> {
        let sj = sprite_info.map(serde_json::to_string).transpose()?;
        self.conn().execute(
            "UPDATE items SET thumbnail_path = COALESCE(?2, thumbnail_path), sprite_path = COALESCE(?3, sprite_path), sprite_json = COALESCE(?4, sprite_json) WHERE id = ?1",
            params![id, thumbnail, sprite, sj],
        )?;
        Ok(())
    }

    pub fn sprite_info(&self, id: i64) -> Result<Option<SpriteInfo>> {
        let j: Option<String> = self
            .conn()
            .query_row("SELECT sprite_json FROM items WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .optional()?
            .flatten();
        Ok(j.map(|j| serde_json::from_str(&j)).transpose()?)
    }

    // ---- scripts -----------------------------------------------------------

    /// Replace an item's funscripts.
    pub fn set_scripts(&self, id: i64, scripts: &[ScriptRef]) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM scripts WHERE item_id = ?1", [id])?;
        for s in scripts {
            tx.execute(
                "INSERT OR REPLACE INTO scripts (item_id, axis, uri) VALUES (?1, ?2, ?3)",
                params![id, s.axis, s.uri],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Funscripts for an item, main script first.
    pub fn scripts(&self, id: i64) -> Result<Vec<ScriptRef>> {
        let conn = self.conn();
        let mut st = conn.prepare(
            "SELECT axis, uri FROM scripts WHERE item_id = ?1 ORDER BY axis <> 'main', axis",
        )?;
        let rows = st.query_map([id], |r| {
            Ok(ScriptRef {
                axis: r.get(0)?,
                uri: r.get(1)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use fp_core::{Codec, FisheyeLens, VideoTrackInfo};

    pub(crate) fn new_item(uri: &str) -> NewItem {
        let path = uri.rsplit('/').next().unwrap().to_string();
        NewItem {
            source_id: None,
            uri: uri.into(),
            title: path.clone(),
            path,
            size: Some(100),
            mtime: Some(1),
            ..Default::default()
        }
    }

    #[test]
    fn upsert_detects_from_filename() {
        let lib = Library::open_in_memory().unwrap();
        let id = lib
            .upsert_item(&new_item("file:///v/Scene_180_LR.mp4"))
            .unwrap();
        let it = lib.item(id).unwrap().unwrap();
        assert_eq!(it.projection, Some(Projection::EQUIRECT_180));
        assert_eq!(it.projection_kind, Some(ProjectionKind::Equirect180));
        assert_eq!(it.stereo, Some(StereoMode::Sbs));
        assert_eq!(it.detect_source, DetectSource::Filename);
        // Upsert again keeps the id.
        assert_eq!(
            lib.upsert_item(&new_item("file:///v/Scene_180_LR.mp4"))
                .unwrap(),
            id
        );
        let plain = lib.upsert_item(&new_item("file:///v/holiday.mp4")).unwrap();
        assert_eq!(
            lib.item(plain).unwrap().unwrap().projection_kind,
            Some(ProjectionKind::Flat)
        );
        assert_eq!(lib.item_count().unwrap(), 2);
    }

    #[test]
    fn media_info_and_container_precedence() {
        let lib = Library::open_in_memory().unwrap();
        let id = lib
            .upsert_item(&new_item("file:///v/clip_180_LR.mp4"))
            .unwrap();
        let info = MediaInfo {
            duration: Some(MediaTime::from_millis(90_000)),
            video: vec![VideoTrackInfo {
                index: 0,
                codec: Codec::Hevc,
                width: 8192,
                height: 4096,
                fps: 59.94,
                bit_depth: 10,
                transfer: ColorTransfer::Pq,
                signalled_projection: Some(Projection::EQUIRECT_360),
                signalled_stereo: None,
            }],
            ..Default::default()
        };
        lib.apply_media_info(id, &info).unwrap();
        let it = lib.item(id).unwrap().unwrap();
        assert_eq!(it.duration, Some(MediaTime::from_millis(90_000)));
        assert_eq!(
            (it.width, it.height, it.codec, it.hdr, it.probed),
            (Some(8192), Some(4096), Some(Codec::Hevc), true, true)
        );
        assert_eq!(it.resolution_label().as_deref(), Some("8K"));
        assert_eq!(it.projection, Some(Projection::EQUIRECT_360));
        assert_eq!(it.stereo, Some(StereoMode::Sbs));
        assert_eq!(it.detect_source, DetectSource::Container);
        // Filename detection on a rescan doesn't downgrade it.
        lib.upsert_item(&new_item("file:///v/clip_180_LR.mp4"))
            .unwrap();
        assert_eq!(
            lib.item(id).unwrap().unwrap().projection,
            Some(Projection::EQUIRECT_360)
        );
    }

    #[test]
    fn overrides_by_hash_and_path() {
        let lib = Library::open_in_memory().unwrap();
        let mut ni = new_item("file:///v/a.mp4");
        ni.content_hash = Some("h1".into());
        let id = lib.upsert_item(&ni).unwrap();
        let vs = ViewSettings {
            projection: Projection::fisheye(FisheyeLens::Mkx200),
            stereo: StereoMode::Sbs,
            ..Default::default()
        };
        lib.set_item_view_settings(id, &vs).unwrap();
        assert_eq!(lib.view_settings(id).unwrap(), vs);
        assert_eq!(
            lib.item(id).unwrap().unwrap().detect_source,
            DetectSource::User
        );
        // Moved file with the same content keeps the override.
        assert_eq!(
            lib.get_override(Some("h1"), Some("file:///elsewhere/b.mp4"))
                .unwrap(),
            Some(vs.clone())
        );
        // Same path, different content still matches by path.
        assert_eq!(
            lib.get_override(Some("h2"), Some("file:///v/a.mp4"))
                .unwrap(),
            Some(vs.clone())
        );
        assert_eq!(
            lib.get_override(Some("h3"), Some("file:///x.mp4")).unwrap(),
            None
        );
        assert!(lib.set_override(None, None, &vs).is_err());
        assert_eq!(lib.all_overrides().unwrap().len(), 1);
        lib.clear_item_view_settings(id).unwrap();
        assert_eq!(lib.view_settings(id).unwrap(), ViewSettings::default());
        assert_eq!(
            lib.item(id).unwrap().unwrap().detect_source,
            DetectSource::Default
        );
    }

    #[test]
    fn scripts_and_thumbnails() {
        let lib = Library::open_in_memory().unwrap();
        let id = lib.upsert_item(&new_item("file:///v/a.mp4")).unwrap();
        lib.set_scripts(
            id,
            &[
                ScriptRef {
                    axis: "twist".into(),
                    uri: "file:///v/a.twist.funscript".into(),
                },
                ScriptRef {
                    axis: "main".into(),
                    uri: "file:///v/a.funscript".into(),
                },
            ],
        )
        .unwrap();
        let s = lib.scripts(id).unwrap();
        assert_eq!(s[0].axis, "main");
        assert!(lib.item(id).unwrap().unwrap().has_script);
        let info = SpriteInfo {
            columns: 10,
            rows: 10,
            tile_width: 160,
            tile_height: 90,
            interval_secs: 6.0,
        };
        lib.set_thumbnails(id, Some("/c/a.jpg"), Some("/c/a_s.jpg"), Some(&info))
            .unwrap();
        lib.set_thumbnails(id, None, None, None).unwrap();
        let it = lib.item(id).unwrap().unwrap();
        assert_eq!(it.thumbnail_path.as_deref(), Some("/c/a.jpg"));
        assert_eq!(lib.sprite_info(id).unwrap(), Some(info));
        assert!(lib.delete_item(id).unwrap());
        assert!(lib.scripts(id).unwrap().is_empty());
    }

    #[test]
    fn deovr_video_applied() {
        let lib = Library::open_in_memory().unwrap();
        let id = lib
            .upsert_item(&new_item("deovr+http://x/deovr/1"))
            .unwrap();
        let v: fp_sources::deovr::DeoVrVideo = serde_json::from_str(
            r#"{"title":"S","screenType":"sphere","stereoMode":"tb","videoLength":120,"thumbnailUrl":"http://x/t.jpg",
                "categories":[{"tag":{"name":"Outdoor"}}],"actors":[{"name":"Jane"}],
                "fleshlight":[{"title":"S.funscript","url":"http://x/s1"},{"title":"S.roll.funscript","url":"http://x/s2"}]}"#,
        )
        .unwrap();
        lib.apply_deovr_video(id, &v).unwrap();
        let it = lib.item(id).unwrap().unwrap();
        assert_eq!(it.projection, Some(Projection::EQUIRECT_360));
        assert_eq!(it.stereo, Some(StereoMode::Ou));
        assert_eq!(it.detect_source, DetectSource::Feed);
        assert_eq!(it.duration, Some(MediaTime::from_secs_f64(120.0)));
        assert_eq!(it.tags, vec!["Jane", "Outdoor"]);
        assert_eq!(it.remote_thumbnail.as_deref(), Some("http://x/t.jpg"));
        let s = lib.scripts(id).unwrap();
        assert_eq!(
            s.iter().map(|s| s.axis.as_str()).collect::<Vec<_>>(),
            ["main", "roll"]
        );
    }
}
