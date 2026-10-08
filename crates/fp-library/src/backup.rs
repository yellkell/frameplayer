//! Export and import of everything the user created (ratings, favourites,
//! tags, markers, format overrides, view settings, keyframes, resume points,
//! play counts and playlists) as one JSON document keyed by location.
//!
//! Probed metadata and thumbnails are not exported: they are rebuilt by the
//! scanner and the metadata worker.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use fp_core::view::Keyframes;
use fp_core::{VideoFormat, ViewSettings};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::library::{
    Library, MediaUpsert, get_by_location_conn, query_records, set_tags_conn, set_user_format_conn,
    set_user_markers_conn, upsert_conn,
};
use crate::playlist::{create_conn, manual_ids, write_ids};
use crate::query::Query;
use crate::record::{MediaId, PlaylistId, now};

/// Value of [`UserDataExport::format`].
pub const EXPORT_FORMAT: &str = "frameplayer-library";
/// Current export document version.
pub const EXPORT_VERSION: u32 = 1;

/// The whole export document.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UserDataExport {
    /// Always [`EXPORT_FORMAT`].
    pub format: String,
    /// Document version, [`EXPORT_VERSION`] when written by this build.
    pub version: u32,
    /// Unix seconds.
    pub exported_at: i64,
    /// User data per video location. Includes every video that has user
    /// data or appears in a playlist.
    pub media: BTreeMap<String, MediaUserData>,
    /// All playlists.
    pub playlists: Vec<PlaylistExport>,
}

/// User data of one video.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaUserData {
    /// Source the video belonged to (used when the row must be recreated).
    pub source_id: String,
    /// Title at export time (used when the row must be recreated).
    pub title: String,
    /// 0–5 stars.
    pub rating: u8,
    /// Favourite flag.
    pub favorite: bool,
    /// Tag names.
    pub tags: Vec<String>,
    /// User bookmarks (source chapters are not exported).
    pub markers: Vec<MarkerExport>,
    /// Format override.
    pub user_format: Option<VideoFormat>,
    /// Picture corrections.
    pub view_settings: Option<ViewSettings>,
    /// Keyframed corrections.
    #[serde(skip_serializing_if = "keyframes_empty")]
    pub keyframes: Keyframes,
    /// Resume point in seconds.
    pub resume_position: f64,
    /// Times watched.
    pub play_count: u32,
    /// Unix seconds of the last playback.
    pub last_played: Option<i64>,
}

fn keyframes_empty(k: &Keyframes) -> bool {
    k.frames.is_empty()
}

impl MediaUserData {
    /// Whether anything differs from a freshly indexed video.
    pub fn is_empty(&self) -> bool {
        self.rating == 0
            && !self.favorite
            && self.tags.is_empty()
            && self.markers.is_empty()
            && self.user_format.is_none()
            && self.view_settings.is_none()
            && self.keyframes.frames.is_empty()
            && self.resume_position <= 0.0
            && self.play_count == 0
            && self.last_played.is_none()
    }
}

/// A user bookmark.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MarkerExport {
    /// Seconds from the start.
    pub time: f64,
    /// Label.
    pub name: String,
}

/// A playlist, with entries as locations.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlaylistExport {
    /// Name; import replaces a playlist with the same name.
    pub name: String,
    /// Query of a smart playlist.
    #[serde(default)]
    pub smart_query: Option<Query>,
    /// Locations of a manual playlist, in order.
    #[serde(default)]
    pub items: Vec<String>,
}

/// What an import changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ImportReport {
    /// Existing videos whose user data was replaced.
    pub media_updated: usize,
    /// Videos not in the library yet, recreated as missing placeholders
    /// that come back when a scan or source refresh finds them.
    pub media_created: usize,
    /// New playlists.
    pub playlists_created: usize,
    /// Playlists replaced because one with the same name existed.
    pub playlists_replaced: usize,
    /// Playlist entries skipped because their location was unknown.
    pub playlist_items_skipped: usize,
}

fn export_conn(conn: &Connection) -> Result<UserDataExport> {
    let records = query_records(conn, "ORDER BY m.id", [])?;
    let mut user_markers: HashMap<i64, Vec<MarkerExport>> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT media_id, time, name FROM markers WHERE from_source = 0
             ORDER BY media_id, time, id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                MarkerExport {
                    time: r.get(1)?,
                    name: r.get(2)?,
                },
            ))
        })?;
        for row in rows {
            let (id, m) = row?;
            user_markers.entry(id).or_default().push(m);
        }
    }

    let mut playlists = Vec::new();
    let mut in_playlists: HashSet<MediaId> = HashSet::new();
    let mut stmt = conn
        .prepare("SELECT id, name, smart_query FROM playlists ORDER BY name COLLATE NOCASE, id")?;
    let rows: Vec<(i64, String, Option<String>)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let location_of: HashMap<MediaId, &str> = records
        .iter()
        .map(|r| (r.id, r.location.as_str()))
        .collect();
    for (id, name, smart) in rows {
        let smart_query = smart.map(|s| serde_json::from_str(&s)).transpose()?;
        let ids = manual_ids(conn, PlaylistId(id))?;
        let items = ids
            .iter()
            .filter_map(|m| location_of.get(m).map(|l| l.to_string()))
            .collect();
        in_playlists.extend(ids);
        playlists.push(PlaylistExport {
            name,
            smart_query,
            items,
        });
    }

    let mut media = BTreeMap::new();
    for r in &records {
        let data = MediaUserData {
            source_id: r.source_id.clone(),
            title: r.title.clone(),
            rating: r.rating,
            favorite: r.favorite,
            tags: r.tags.clone(),
            markers: user_markers.remove(&r.id.0).unwrap_or_default(),
            user_format: r.user_format,
            view_settings: r.view_settings,
            keyframes: r.keyframes.clone(),
            resume_position: r.resume_position,
            play_count: r.play_count,
            last_played: r.last_played,
        };
        if !data.is_empty() || in_playlists.contains(&r.id) {
            media.insert(r.location.clone(), data);
        }
    }
    Ok(UserDataExport {
        format: EXPORT_FORMAT.to_string(),
        version: EXPORT_VERSION,
        exported_at: now(),
        media,
        playlists,
    })
}

fn import_conn(conn: &Connection, doc: &UserDataExport) -> Result<ImportReport> {
    if doc.format != EXPORT_FORMAT {
        return Err(Error::InvalidArgument(format!(
            "not a FramePlayer library export (format {:?})",
            doc.format
        )));
    }
    if doc.version > EXPORT_VERSION {
        return Err(Error::InvalidArgument(format!(
            "export version {} is newer than this build supports ({EXPORT_VERSION})",
            doc.version
        )));
    }
    let mut report = ImportReport::default();
    for (location, data) in &doc.media {
        let id = match get_by_location_conn(conn, location)? {
            Some(r) => {
                report.media_updated += 1;
                r.id
            }
            None => {
                let source = if data.source_id.is_empty() {
                    "imported"
                } else {
                    data.source_id.as_str()
                };
                let mut u = MediaUpsert::new(location.clone(), source);
                if !data.title.trim().is_empty() {
                    u.title = data.title.clone();
                }
                let id = upsert_conn(conn, &u)?.id;
                conn.execute("UPDATE media SET missing = 1 WHERE id = ?1", [id.0])?;
                report.media_created += 1;
                id
            }
        };
        let resume = if data.resume_position.is_finite() {
            data.resume_position.max(0.0)
        } else {
            0.0
        };
        conn.execute(
            "UPDATE media SET rating = ?2, favorite = ?3, view_settings = ?4, keyframes = ?5,
                 resume_position = ?6, play_count = MAX(play_count, ?7),
                 last_played = CASE WHEN ?8 IS NULL THEN last_played
                                    WHEN last_played IS NULL OR last_played < ?8 THEN ?8
                                    ELSE last_played END
             WHERE id = ?1",
            params![
                id.0,
                data.rating.min(5),
                data.favorite,
                data.view_settings
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?,
                serde_json::to_string(&data.keyframes)?,
                resume,
                data.play_count,
                data.last_played,
            ],
        )?;
        set_user_format_conn(conn, id, data.user_format.as_ref())?;
        set_tags_conn(conn, id, &data.tags)?;
        let markers: Vec<(f64, String)> = data
            .markers
            .iter()
            .filter(|m| m.time.is_finite())
            .map(|m| (m.time, m.name.clone()))
            .collect();
        set_user_markers_conn(conn, id, &markers)?;
    }

    for p in &doc.playlists {
        let existing: Option<i64> = conn
            .query_row(
                "SELECT id FROM playlists WHERE name = ?1 ORDER BY id LIMIT 1",
                [p.name.trim()],
                |r| r.get(0),
            )
            .optional()?;
        let id = match existing {
            Some(id) => {
                conn.execute(
                    "UPDATE playlists SET smart_query = ?2 WHERE id = ?1",
                    params![
                        id,
                        p.smart_query
                            .as_ref()
                            .map(serde_json::to_string)
                            .transpose()?
                    ],
                )?;
                report.playlists_replaced += 1;
                PlaylistId(id)
            }
            None => {
                report.playlists_created += 1;
                create_conn(conn, &p.name, p.smart_query.as_ref())?
            }
        };
        let mut ids = Vec::new();
        if p.smart_query.is_none() {
            for loc in &p.items {
                match get_by_location_conn(conn, loc)? {
                    Some(r) => ids.push(r.id),
                    None => report.playlist_items_skipped += 1,
                }
            }
        }
        write_ids(conn, id, &ids)?;
    }
    Ok(report)
}

impl Library {
    /// Collects all user data into an export document.
    pub fn export_user_data(&self) -> Result<UserDataExport> {
        self.with(|c| export_conn(c))
    }

    /// Applies an export document in one transaction. Data for a location
    /// already in the library replaces its user data (play count and last
    /// played keep the larger value); unknown locations become missing
    /// placeholders carrying the data. Playlists replace same-named ones.
    pub fn import_user_data(&self, doc: &UserDataExport) -> Result<ImportReport> {
        self.in_tx(|c| import_conn(c, doc))
    }

    /// [`Library::export_user_data`] as pretty-printed JSON.
    pub fn export_json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(&self.export_user_data()?)?)
    }

    /// [`Library::import_user_data`] from JSON text.
    pub fn import_json(&self, json: &str) -> Result<ImportReport> {
        self.import_user_data(&serde_json::from_str(json)?)
    }

    /// Writes the JSON export to a file (atomically, via a temporary file).
    pub fn export_to_file(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        let json = self.export_json()?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, json).map_err(|e| Error::io(&tmp, e))?;
        std::fs::rename(&tmp, path).map_err(|e| Error::io(path, e))
    }

    /// Imports a JSON export file.
    pub fn import_from_file(&self, path: impl AsRef<Path>) -> Result<ImportReport> {
        let path = path.as_ref();
        let json = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        self.import_json(&json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::Sort;
    use fp_core::{Projection, StereoLayout};

    fn populated() -> Library {
        let lib = Library::open_in_memory().unwrap();
        let mut ids = Vec::new();
        for loc in ["/v/a.mp4", "/v/b_180_LR.mp4", "/v/c.mp4", "/v/plain.mp4"] {
            let mut u = MediaUpsert::new(loc, "local");
            u.duration = Some(100.0);
            ids.push(lib.upsert(&u).unwrap().id);
        }
        let (a, b, c) = (ids[0], ids[1], ids[2]);
        lib.set_rating(a, 5).unwrap();
        lib.set_favorite(a, true).unwrap();
        lib.set_tags(a, &["x", "Y"]).unwrap();
        lib.add_marker(a, 10.0, "here").unwrap();
        lib.set_user_format(
            b,
            Some(VideoFormat::new(
                Projection::fisheye(190.0),
                StereoLayout::SideBySide,
            )),
        )
        .unwrap();
        let vs = ViewSettings {
            ipd_offset: 0.5,
            ..Default::default()
        };
        lib.set_view_settings(b, Some(&vs)).unwrap();
        let mut kf = Keyframes::default();
        kf.insert(1.0, vs);
        kf.insert(9.0, ViewSettings::default());
        lib.set_keyframes(b, &kf).unwrap();
        let s = lib.start_playback(c).unwrap();
        lib.update_playback(s, 42.0, false).unwrap();
        let s = lib.start_playback(a).unwrap();
        lib.update_playback(s, 100.0, true).unwrap();
        // A source chapter must not be exported.
        let mut e = fp_core::Entry::new("r.mp4", "http://h/r.mp4", fp_core::EntryKind::Video);
        e.markers = vec![(1.0, "chapter".into())];
        let r = lib.upsert_entry("feed", &e).unwrap().id;
        let p = lib.create_playlist("Mix").unwrap();
        lib.set_playlist_items(p, &[c, r, a]).unwrap();
        lib.create_smart_playlist(
            "Top",
            &Query {
                min_rating: 4,
                sort: Sort::Rating,
                ..Default::default()
            },
        )
        .unwrap();
        lib
    }

    #[test]
    fn export_contains_only_user_data() {
        let lib = populated();
        let doc = lib.export_user_data().unwrap();
        assert_eq!(doc.format, EXPORT_FORMAT);
        let locs: Vec<&String> = doc.media.keys().collect();
        // plain.mp4 has no user data and is in no playlist.
        assert_eq!(
            locs,
            vec!["/v/a.mp4", "/v/b_180_LR.mp4", "/v/c.mp4", "http://h/r.mp4"]
        );
        let r = &doc.media["http://h/r.mp4"];
        assert!(r.is_empty() && r.markers.is_empty());
        let a = &doc.media["/v/a.mp4"];
        assert_eq!((a.rating, a.favorite, a.play_count), (5, true, 1));
        assert_eq!(
            a.markers,
            vec![MarkerExport {
                time: 10.0,
                name: "here".into()
            }]
        );
        assert_eq!(doc.playlists.len(), 2);
        assert_eq!(
            doc.playlists[0].items,
            vec!["/v/c.mp4", "http://h/r.mp4", "/v/a.mp4"]
        );
        assert!(doc.playlists[1].smart_query.is_some());
    }

    #[test]
    fn round_trip_into_empty_library_and_rescan() {
        let src = populated();
        let json = src.export_json().unwrap();
        let dst = Library::open_in_memory().unwrap();
        let report = dst.import_json(&json).unwrap();
        assert_eq!(
            report,
            ImportReport {
                media_updated: 0,
                media_created: 4,
                playlists_created: 2,
                playlists_replaced: 0,
                playlist_items_skipped: 0,
            }
        );
        // Placeholders are missing until found.
        assert_eq!(dst.count(&Query::default()).unwrap(), 0);
        let mut a_doc = src.export_user_data().unwrap();
        let mut b_doc = dst.export_user_data().unwrap();
        a_doc.exported_at = 0;
        b_doc.exported_at = 0;
        assert_eq!(a_doc, b_doc);

        // A scan finding the file brings it back with its data.
        let mut u = MediaUpsert::new("/v/b_180_LR.mp4", "local");
        u.size = Some(5);
        let out = dst.upsert(&u).unwrap();
        assert!(out.restored);
        let b = dst.get(out.id).unwrap().unwrap();
        assert!(!b.missing);
        assert_eq!(
            b.effective_format().format.projection,
            Projection::fisheye(190.0)
        );
        assert_eq!(b.keyframes.frames.len(), 2);
        assert_eq!(b.view_settings.unwrap().ipd_offset, 0.5);
        let c = dst.get_by_location("/v/c.mp4").unwrap().unwrap();
        assert_eq!(c.resume_position, 42.0);
        assert!(c.last_played.is_some());
        // Playlists point at the right rows.
        let mix = dst
            .playlists()
            .unwrap()
            .into_iter()
            .find(|p| p.name == "Mix")
            .unwrap();
        let locs: Vec<String> = dst
            .playlist_items(mix.id)
            .unwrap()
            .into_iter()
            .map(|r| r.location)
            .collect();
        assert_eq!(locs, vec!["/v/c.mp4", "http://h/r.mp4", "/v/a.mp4"]);
    }

    #[test]
    fn import_over_existing_rows_merges() {
        let src = populated();
        let doc = src.export_user_data().unwrap();
        let dst = Library::open_in_memory().unwrap();
        let a = dst
            .upsert(&MediaUpsert::new("/v/a.mp4", "local"))
            .unwrap()
            .id;
        dst.set_tags(a, &["old"]).unwrap();
        dst.add_marker(a, 1.0, "old marker").unwrap();
        let s = dst.start_playback(a).unwrap();
        dst.update_playback(s, 1.0, true).unwrap();
        dst.update_playback(dst.start_playback(a).unwrap(), 1.0, true)
            .unwrap();
        let old = dst.create_playlist("Mix").unwrap();
        dst.add_to_playlist(old, a).unwrap();

        let report = dst.import_user_data(&doc).unwrap();
        assert_eq!((report.media_updated, report.media_created), (1, 3));
        assert_eq!(
            (report.playlists_replaced, report.playlists_created),
            (1, 1)
        );
        let r = dst.get(a).unwrap().unwrap();
        assert_eq!(r.tags, vec!["x", "Y"]);
        assert_eq!(r.play_count, 2, "larger play count kept");
        assert!(!r.missing);
        let names: Vec<String> = dst
            .markers(a)
            .unwrap()
            .into_iter()
            .map(|m| m.name)
            .collect();
        assert_eq!(names, vec!["here"]);
        assert_eq!(dst.playlist_items(old).unwrap().len(), 3);
        // Importing twice is idempotent.
        let again = dst.import_user_data(&doc).unwrap();
        assert_eq!((again.media_created, again.playlists_created), (0, 0));
    }

    #[test]
    fn rejects_foreign_documents_and_skips_unknown_items() {
        let lib = Library::open_in_memory().unwrap();
        let mut doc = UserDataExport {
            format: "something-else".into(),
            version: 1,
            exported_at: 0,
            media: BTreeMap::new(),
            playlists: vec![PlaylistExport {
                name: "P".into(),
                smart_query: None,
                items: vec!["/nowhere.mp4".into()],
            }],
        };
        assert!(matches!(
            lib.import_user_data(&doc),
            Err(Error::InvalidArgument(_))
        ));
        doc.format = EXPORT_FORMAT.into();
        doc.version = EXPORT_VERSION + 1;
        assert!(lib.import_user_data(&doc).is_err());
        doc.version = EXPORT_VERSION;
        let report = lib.import_user_data(&doc).unwrap();
        assert_eq!(report.playlist_items_skipped, 1);
        assert!(lib.import_json("{not json").is_err());
    }

    #[test]
    fn file_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("backup.json");
        let src = populated();
        src.export_to_file(&path).unwrap();
        let dst = Library::open_in_memory().unwrap();
        assert_eq!(dst.import_from_file(&path).unwrap().media_created, 4);
        assert!(dst.import_from_file(tmp.path().join("nope.json")).is_err());
    }
}
