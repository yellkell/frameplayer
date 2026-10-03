//! Library search: structured filters in SQL, forgiving free-text matching
//! and ranking in Rust.

use std::cmp::Ordering;

use rusqlite::Connection;
use rusqlite::types::Value;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::library::{Library, get_many, normalize_tags};
use crate::record::{MediaId, MediaRecord, ProjectionKind};

/// Sort key for [`Query`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sort {
    /// Best text match first; by title when there is no text.
    #[default]
    Relevance,
    /// Title, case-insensitive with numbers compared by value ("Ep 2" before
    /// "Ep 10").
    Title,
    /// Time first indexed.
    Added,
    /// Last playback (never played sorts last in either direction).
    LastPlayed,
    /// Duration (unknown sorts last in either direction).
    Duration,
    /// Star rating.
    Rating,
    /// Times watched.
    PlayCount,
    /// Shuffled, stable for a given [`Query::random_seed`] so paging works.
    Random,
}

impl Sort {
    /// Direction used when [`Query::direction`] is `None`: ascending for
    /// title, duration and random, descending otherwise.
    pub fn default_direction(self) -> SortDirection {
        match self {
            Sort::Title | Sort::Duration | Sort::Random => SortDirection::Ascending,
            _ => SortDirection::Descending,
        }
    }
}

/// Sort direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortDirection {
    /// Smallest first.
    Ascending,
    /// Largest first.
    Descending,
}

/// Stereo layout filter on the effective format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StereoFilter {
    /// 2D only.
    Mono,
    /// Any 3D layout.
    Stereo,
    /// Side by side only.
    SideBySide,
    /// Top/bottom only.
    TopBottom,
}

/// A library search. Every field narrows the result; the default matches
/// every present video. Serialisable so smart playlists can store it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Query {
    /// Free text. Split into words on anything that is not a letter or
    /// digit; every word must appear (case-insensitively) in the title, the
    /// location or a tag.
    pub text: String,
    /// Tags that must all be present (case-insensitive).
    pub tags: Vec<String>,
    /// Only favourites.
    pub favorites_only: bool,
    /// Minimum star rating; 0 accepts unrated videos.
    pub min_rating: u8,
    /// Accepted projection families of the effective format; empty accepts
    /// all.
    pub projections: Vec<ProjectionKind>,
    /// Stereo layout of the effective format.
    pub stereo: Option<StereoFilter>,
    /// Minimum duration in seconds (unknown durations are excluded).
    pub min_duration: Option<f64>,
    /// Maximum duration in seconds (unknown durations are excluded).
    pub max_duration: Option<f64>,
    /// Accepted source ids; empty accepts all.
    pub sources: Vec<String>,
    /// Also return videos the last scan could not find.
    pub include_missing: bool,
    /// Sort key.
    pub sort: Sort,
    /// Sort direction; `None` uses [`Sort::default_direction`].
    pub direction: Option<SortDirection>,
    /// Seed for [`Sort::Random`].
    pub random_seed: u64,
    /// Maximum number of results.
    pub limit: Option<usize>,
    /// Results to skip (paging).
    pub offset: usize,
}

impl Query {
    /// A query for free text, everything else default.
    pub fn text(text: impl Into<String>) -> Query {
        Query {
            text: text.into(),
            ..Query::default()
        }
    }
}

/// The light-weight row used for filtering and ranking.
struct Candidate {
    id: i64,
    title: String,
    added_at: i64,
    last_played: Option<i64>,
    duration: Option<f64>,
    rating: i64,
    play_count: i64,
    score: f64,
}

/// SQL `WHERE` clause (with leading `WHERE`, or empty) and its parameters.
fn filter_sql(q: &Query) -> (String, Vec<Value>) {
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<Value> = Vec::new();
    if !q.include_missing {
        clauses.push("m.missing = 0".into());
    }
    if q.favorites_only {
        clauses.push("m.favorite = 1".into());
    }
    if q.min_rating > 0 {
        clauses.push("m.rating >= ?".into());
        params.push(Value::Integer(q.min_rating as i64));
    }
    if !q.projections.is_empty() {
        clauses.push(format!(
            "m.projection_kind IN ({})",
            vec!["?"; q.projections.len()].join(",")
        ));
        params.extend(
            q.projections
                .iter()
                .map(|p| Value::Text(p.as_str().to_string())),
        );
    }
    match q.stereo {
        None => {}
        Some(StereoFilter::Mono) => clauses.push("m.stereo = 'mono'".into()),
        Some(StereoFilter::Stereo) => clauses.push("m.stereo <> 'mono'".into()),
        Some(StereoFilter::SideBySide) => clauses.push("m.stereo = 'side_by_side'".into()),
        Some(StereoFilter::TopBottom) => clauses.push("m.stereo = 'top_bottom'".into()),
    }
    if let Some(min) = q.min_duration.filter(|v| v.is_finite()) {
        clauses.push("m.duration >= ?".into());
        params.push(Value::Real(min));
    }
    if let Some(max) = q.max_duration.filter(|v| v.is_finite()) {
        clauses.push("m.duration <= ?".into());
        params.push(Value::Real(max));
    }
    if !q.sources.is_empty() {
        clauses.push(format!(
            "m.source_id IN ({})",
            vec!["?"; q.sources.len()].join(",")
        ));
        params.extend(q.sources.iter().map(|s| Value::Text(s.clone())));
    }
    for tag in normalize_tags(&q.tags) {
        // tags.name is declared COLLATE NOCASE, so `=` ignores case.
        clauses.push(
            "EXISTS (SELECT 1 FROM media_tags mt JOIN tags t ON t.id = mt.tag_id
                     WHERE mt.media_id = m.id AND t.name = ?)"
                .into(),
        );
        params.push(Value::Text(tag));
    }
    let sql = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };
    (sql, params)
}

/// Lower-cased words of `s`, split on anything not alphanumeric.
fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// One searchable field of a candidate.
struct Field {
    lower: String,
    words: Vec<String>,
    compact: String,
    weight: f64,
}

impl Field {
    fn new(text: &str, weight: f64) -> Field {
        let lower = text.to_lowercase();
        Field {
            words: words(&lower),
            compact: lower.chars().filter(|c| c.is_alphanumeric()).collect(),
            lower,
            weight,
        }
    }

    /// How well `term` matches: whole word 1.0, word prefix 0.75, anywhere
    /// 0.4, anywhere once separators are ignored 0.3, else 0.
    fn strength(&self, term: &str) -> f64 {
        if self.words.iter().any(|w| w == term) {
            1.0
        } else if self.words.iter().any(|w| w.starts_with(term)) {
            0.75
        } else if self.lower.contains(term) {
            0.4
        } else if self.compact.contains(term) {
            0.3
        } else {
            0.0
        }
    }
}

const TITLE_WEIGHT: f64 = 3.0;
const TAG_WEIGHT: f64 = 2.0;
const LOCATION_WEIGHT: f64 = 1.0;

/// Relevance of a video for the query words, `None` when some word does not
/// match anywhere. Each word scores its best field match (title weighs most,
/// then tags, then the location); a title that starts with the whole query
/// gets a small bonus and an exact title a bigger one.
pub(crate) fn score(
    terms: &[String],
    query_lower: &str,
    title: &str,
    tags: &str,
    location: &str,
) -> Option<f64> {
    let fields = [
        Field::new(title, TITLE_WEIGHT),
        Field::new(tags, TAG_WEIGHT),
        Field::new(location, LOCATION_WEIGHT),
    ];
    let mut total = 0.0;
    for term in terms {
        let best = fields
            .iter()
            .map(|f| f.weight * f.strength(term))
            .fold(0.0, f64::max);
        if best <= 0.0 {
            return None;
        }
        total += best;
    }
    let title_lower = &fields[0].lower;
    if !query_lower.is_empty() {
        if title_lower == query_lower {
            total += 1.0;
        } else if title_lower.starts_with(query_lower) {
            total += 0.5;
        }
    }
    Some(total)
}

fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// Case-insensitive comparison where runs of digits compare by value.
pub(crate) fn natural_cmp(a: &str, b: &str) -> Ordering {
    let a = a.to_lowercase();
    let b = b.to_lowercase();
    let mut ai = a.chars().peekable();
    let mut bi = b.chars().peekable();
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let mut na = String::new();
                while let Some(c) = ai.peek().copied().filter(char::is_ascii_digit) {
                    na.push(c);
                    ai.next();
                }
                let mut nb = String::new();
                while let Some(c) = bi.peek().copied().filter(char::is_ascii_digit) {
                    nb.push(c);
                    bi.next();
                }
                let ta = na.trim_start_matches('0');
                let tb = nb.trim_start_matches('0');
                let ord = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (Some(x), Some(y)) => {
                if x != y {
                    return x.cmp(&y);
                }
                ai.next();
                bi.next();
            }
        }
    }
}

/// Compares optional keys with `None` last whatever the direction.
fn cmp_opt<T: PartialOrd>(a: Option<T>, b: Option<T>, dir: SortDirection) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => {
            let o = x.partial_cmp(&y).unwrap_or(Ordering::Equal);
            apply(o, dir)
        }
    }
}

fn apply(o: Ordering, dir: SortDirection) -> Ordering {
    match dir {
        SortDirection::Ascending => o,
        SortDirection::Descending => o.reverse(),
    }
}

fn compare(a: &Candidate, b: &Candidate, q: &Query) -> Ordering {
    let dir = q.direction.unwrap_or(q.sort.default_direction());
    let primary = match q.sort {
        Sort::Relevance => apply(a.score.total_cmp(&b.score), dir),
        Sort::Title => apply(natural_cmp(&a.title, &b.title), dir),
        Sort::Added => apply(a.added_at.cmp(&b.added_at), dir),
        Sort::LastPlayed => cmp_opt(a.last_played, b.last_played, dir),
        Sort::Duration => cmp_opt(a.duration, b.duration, dir),
        Sort::Rating => apply(a.rating.cmp(&b.rating), dir),
        Sort::PlayCount => apply(a.play_count.cmp(&b.play_count), dir),
        Sort::Random => {
            let h = |id: i64| splitmix64(q.random_seed ^ id as u64);
            apply(h(a.id).cmp(&h(b.id)), dir)
        }
    };
    primary
        .then_with(|| b.score.total_cmp(&a.score))
        .then_with(|| natural_cmp(&a.title, &b.title))
        .then_with(|| a.id.cmp(&b.id))
}

/// Filtered and sorted candidates (before offset/limit).
fn ranked(conn: &Connection, q: &Query) -> Result<Vec<Candidate>> {
    let terms = words(&q.text);
    let query_lower = q.text.trim().to_lowercase();
    let (where_sql, params) = filter_sql(q);
    let sql = format!(
        "SELECT m.id, m.title, m.location,
                COALESCE((SELECT group_concat(t.name, ' ') FROM media_tags mt
                          JOIN tags t ON t.id = mt.tag_id WHERE mt.media_id = m.id), ''),
                m.added_at, m.last_played, m.duration, m.rating, m.play_count
         FROM media m {where_sql}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(rusqlite::params_from_iter(params))?;
    let mut out = Vec::new();
    while let Some(r) = rows.next()? {
        let title: String = r.get(1)?;
        let score = if terms.is_empty() {
            0.0
        } else {
            let location: String = r.get(2)?;
            let tags: String = r.get(3)?;
            match score(&terms, &query_lower, &title, &tags, &location) {
                Some(s) => s,
                None => continue,
            }
        };
        out.push(Candidate {
            id: r.get(0)?,
            title,
            added_at: r.get(4)?,
            last_played: r.get(5)?,
            duration: r.get(6)?,
            rating: r.get(7)?,
            play_count: r.get(8)?,
            score,
        });
    }
    out.sort_by(|a, b| compare(a, b, q));
    Ok(out)
}

pub(crate) fn search_conn(conn: &Connection, q: &Query) -> Result<Vec<MediaRecord>> {
    let ids: Vec<MediaId> = ranked(conn, q)?
        .into_iter()
        .skip(q.offset)
        .take(q.limit.unwrap_or(usize::MAX))
        .map(|c| MediaId(c.id))
        .collect();
    get_many(conn, &ids)
}

pub(crate) fn count_conn(conn: &Connection, q: &Query) -> Result<usize> {
    if words(&q.text).is_empty() {
        let (where_sql, params) = filter_sql(q);
        let n: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM media m {where_sql}"),
            rusqlite::params_from_iter(params),
            |r| r.get(0),
        )?;
        Ok(n.max(0) as usize)
    } else {
        Ok(ranked(conn, q)?.len())
    }
}

impl Library {
    /// Runs a query: filters, free-text match, ranking, sorting, paging.
    pub fn search(&self, query: &Query) -> Result<Vec<MediaRecord>> {
        self.with(|c| search_conn(c, query))
    }

    /// Number of results `query` has, ignoring `limit` and `offset`.
    pub fn count(&self, query: &Query) -> Result<usize> {
        self.with(|c| count_conn(c, query))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::MediaUpsert;
    use fp_core::{Projection, StereoLayout, VideoFormat};

    fn add(lib: &Library, loc: &str, title: &str) -> MediaId {
        let mut u = MediaUpsert::new(loc, "local");
        u.title = title.into();
        lib.upsert(&u).unwrap().id
    }

    fn titles(lib: &Library, q: &Query) -> Vec<String> {
        lib.search(q)
            .unwrap()
            .into_iter()
            .map(|r| r.title)
            .collect()
    }

    #[test]
    fn natural_ordering() {
        assert_eq!(natural_cmp("Ep 2", "ep 10"), Ordering::Less);
        assert_eq!(natural_cmp("a010", "a10"), Ordering::Equal);
        assert_eq!(natural_cmp("B", "a"), Ordering::Greater);
        assert_eq!(natural_cmp("x", "x1"), Ordering::Less);
    }

    #[test]
    fn scoring_prefers_whole_words_and_prefixes() {
        let t = |s: &str| words(s);
        let whole = score(&t("beach"), "beach", "Sunset Beach", "", "/v/a.mp4").unwrap();
        let prefix = score(&t("beach"), "beach", "Beachside Party", "", "/v/b.mp4").unwrap();
        let sub = score(&t("beach"), "beach", "Offbeach Hotel", "", "/v/c.mp4").unwrap();
        assert!(whole > prefix && prefix > sub, "{whole} {prefix} {sub}");
        assert!(score(&t("beach"), "beach", "Forest", "", "/v/d.mp4").is_none());
        // Separators are forgiven.
        assert!(score(&t("scene180"), "scene180", "Scene_180_LR", "", "/x").is_some());
    }

    #[test]
    fn free_text_ranking() {
        let lib = Library::open_in_memory().unwrap();
        add(&lib, "/v/1.mp4", "Offbeach Hotel");
        add(&lib, "/v/2.mp4", "Beachside Party");
        add(&lib, "/v/3.mp4", "Sunset Beach");
        let tagged = add(&lib, "/v/4.mp4", "Movie");
        lib.set_tags(tagged, &["Beach"]).unwrap();
        add(&lib, "/videos/beach/clip.mp4", "clip");
        add(&lib, "/v/6.mp4", "Forest");
        assert_eq!(
            titles(&lib, &Query::text("BEACH")),
            vec![
                "Sunset Beach",
                "Beachside Party",
                "Movie",
                "Offbeach Hotel",
                "clip"
            ]
        );
        assert_eq!(lib.count(&Query::text("beach")).unwrap(), 5);
        // All words must match, in any order and field.
        assert_eq!(
            titles(&lib, &Query::text("beach sunset")),
            vec!["Sunset Beach"]
        );
        assert_eq!(titles(&lib, &Query::text("movie beach")), vec!["Movie"]);
        assert!(titles(&lib, &Query::text("beach zebra")).is_empty());
        // Punctuation-only text is no text.
        assert_eq!(lib.count(&Query::text(" -- ")).unwrap(), 6);
        // Paging over ranked results.
        let q = Query {
            limit: Some(2),
            offset: 1,
            ..Query::text("beach")
        };
        assert_eq!(titles(&lib, &q), vec!["Beachside Party", "Movie"]);
        // An explicit sort overrides relevance.
        let q = Query {
            sort: Sort::Title,
            ..Query::text("beach")
        };
        assert_eq!(titles(&lib, &q)[0], "Beachside Party");
    }

    #[test]
    fn structured_filters() {
        let lib = Library::open_in_memory().unwrap();
        let mut u = MediaUpsert::new("/v/a_180_LR.mp4", "local");
        u.duration = Some(600.0);
        let a = lib.upsert(&u).unwrap().id;
        let mut u = MediaUpsert::new("/v/b_360.mp4", "local");
        u.duration = Some(60.0);
        let b = lib.upsert(&u).unwrap().id;
        let mut u = MediaUpsert::new("smb://nas/c_MKX200.mp4", "nas");
        u.duration = Some(1800.0);
        let c = lib.upsert(&u).unwrap().id;
        let d = lib
            .upsert(&MediaUpsert::new("/v/d_EAC.webm", "local"))
            .unwrap()
            .id;
        let e = lib
            .upsert(&MediaUpsert::new("/v/movie_SBS.mkv", "local"))
            .unwrap()
            .id;

        lib.set_favorite(a, true).unwrap();
        lib.set_rating(a, 5).unwrap();
        lib.set_rating(c, 3).unwrap();
        lib.set_tags(a, &["pov", "outdoor"]).unwrap();
        lib.set_tags(c, &["POV"]).unwrap();

        let ids = |q: Query| -> Vec<MediaId> {
            let mut v: Vec<MediaId> = lib.search(&q).unwrap().iter().map(|r| r.id).collect();
            v.sort();
            assert_eq!(lib.count(&q).unwrap(), v.len());
            v
        };
        let proj = |p: Vec<ProjectionKind>| Query {
            projections: p,
            ..Default::default()
        };
        assert_eq!(ids(proj(vec![ProjectionKind::Equirect180])), vec![a]);
        assert_eq!(ids(proj(vec![ProjectionKind::Equirect360])), vec![b]);
        assert_eq!(ids(proj(vec![ProjectionKind::Fisheye])), vec![c]);
        assert_eq!(ids(proj(vec![ProjectionKind::Eac])), vec![d]);
        assert_eq!(ids(proj(vec![ProjectionKind::Flat])), vec![e]);
        assert_eq!(
            ids(proj(vec![
                ProjectionKind::Equirect180,
                ProjectionKind::Fisheye
            ])),
            vec![a, c]
        );
        let st = |s| Query {
            stereo: Some(s),
            ..Default::default()
        };
        assert_eq!(ids(st(StereoFilter::Mono)), vec![b, d]);
        assert_eq!(ids(st(StereoFilter::Stereo)), vec![a, c, e]);
        assert_eq!(ids(st(StereoFilter::SideBySide)), vec![a, c, e]);
        assert!(ids(st(StereoFilter::TopBottom)).is_empty());

        assert_eq!(
            ids(Query {
                favorites_only: true,
                ..Default::default()
            }),
            vec![a]
        );
        assert_eq!(
            ids(Query {
                min_rating: 3,
                ..Default::default()
            }),
            vec![a, c]
        );
        assert_eq!(
            ids(Query {
                tags: vec!["Pov".into()],
                ..Default::default()
            }),
            vec![a, c]
        );
        assert_eq!(
            ids(Query {
                tags: vec!["pov".into(), "OUTDOOR".into()],
                ..Default::default()
            }),
            vec![a]
        );
        assert_eq!(
            ids(Query {
                min_duration: Some(100.0),
                max_duration: Some(1000.0),
                ..Default::default()
            }),
            vec![a]
        );
        assert_eq!(
            ids(Query {
                sources: vec!["nas".into()],
                ..Default::default()
            }),
            vec![c]
        );

        // The user's override drives the format filters.
        lib.set_user_format(
            e,
            Some(VideoFormat::new(
                Projection::EQUIRECT_360,
                StereoLayout::TopBottom,
            )),
        )
        .unwrap();
        assert_eq!(ids(st(StereoFilter::TopBottom)), vec![e]);

        // Missing rows are hidden unless asked for.
        lib.with(|conn| {
            conn.execute("UPDATE media SET missing = 1 WHERE id = ?1", [b.0])?;
            Ok(())
        })
        .unwrap();
        assert_eq!(ids(Query::default()).len(), 4);
        assert_eq!(
            ids(Query {
                include_missing: true,
                ..Default::default()
            })
            .len(),
            5
        );
    }

    #[test]
    fn sorting() {
        let lib = Library::open_in_memory().unwrap();
        let mk = |loc: &str, title: &str, dur: Option<f64>| {
            let mut u = MediaUpsert::new(loc, "local");
            u.title = title.into();
            u.duration = dur;
            lib.upsert(&u).unwrap().id
        };
        let a = mk("/a.mp4", "Ep 10", Some(30.0));
        let b = mk("/b.mp4", "ep 2", None);
        let c = mk("/c.mp4", "Alpha", Some(90.0));
        lib.set_rating(a, 2).unwrap();
        lib.set_rating(c, 4).unwrap();
        let s = lib.start_playback(b).unwrap();
        lib.update_playback(s, 100.0, true).unwrap();

        let order = |sort: Sort, dir: Option<SortDirection>| -> Vec<MediaId> {
            lib.search(&Query {
                sort,
                direction: dir,
                ..Default::default()
            })
            .unwrap()
            .iter()
            .map(|r| r.id)
            .collect()
        };
        assert_eq!(order(Sort::Title, None), vec![c, b, a]);
        assert_eq!(order(Sort::Relevance, None), vec![c, b, a]);
        assert_eq!(
            order(Sort::Title, Some(SortDirection::Descending)),
            vec![a, b, c]
        );
        assert_eq!(order(Sort::Rating, None), vec![c, a, b]);
        assert_eq!(order(Sort::PlayCount, None)[0], b);
        // Unknown duration last in both directions.
        assert_eq!(order(Sort::Duration, None), vec![a, c, b]);
        assert_eq!(
            order(Sort::Duration, Some(SortDirection::Descending)),
            vec![c, a, b]
        );
        assert_eq!(order(Sort::LastPlayed, None)[0], b);
        assert_eq!(
            order(Sort::LastPlayed, Some(SortDirection::Ascending))[0],
            b,
            "played before never-played"
        );
        // Added: same second, so ties fall back to title.
        assert_eq!(order(Sort::Added, None).len(), 3);
        // Random is a stable permutation for a seed.
        let r1 = lib
            .search(&Query {
                sort: Sort::Random,
                random_seed: 7,
                ..Default::default()
            })
            .unwrap();
        let r2 = lib
            .search(&Query {
                sort: Sort::Random,
                random_seed: 7,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(r1, r2);
        let mut ids: Vec<MediaId> = r1.iter().map(|r| r.id).collect();
        ids.sort();
        assert_eq!(ids, vec![a, b, c]);
    }

    #[test]
    fn query_serialises() {
        let q = Query {
            text: "beach".into(),
            projections: vec![ProjectionKind::Equirect180],
            stereo: Some(StereoFilter::Stereo),
            sort: Sort::Rating,
            ..Default::default()
        };
        let j = serde_json::to_string(&q).unwrap();
        assert_eq!(serde_json::from_str::<Query>(&j).unwrap(), q);
        // Missing fields take defaults, so stored queries survive new fields.
        let partial: Query = serde_json::from_str(r#"{"text":"x"}"#).unwrap();
        assert_eq!(partial, Query::text("x"));
    }
}
