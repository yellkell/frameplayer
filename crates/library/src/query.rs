//! Library queries: fuzzy text search, filters and sorting. Smart playlists
//! store an [`ItemQuery`] as JSON and evaluate it on demand.

use crate::db::Library;
use crate::error::Result;
use crate::fuzzy;
use crate::model::{Item, ProjectionKind};
use fp_core::StereoMode;
use rand::seq::SliceRandom;
use rand::SeedableRng;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sort {
    /// Best fuzzy score first when there's search text, else name.
    #[default]
    Relevance,
    Name,
    DateAdded,
    Duration,
    Rating,
    LastWatched,
    Size,
    Random,
}

/// A search / filter specification. All filters combine with AND; unset
/// fields don't filter.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ItemQuery {
    pub text: Option<String>,
    pub source_id: Option<String>,
    /// Any of these projection kinds.
    pub projections: Vec<ProjectionKind>,
    /// Only immersive (non-flat) content.
    pub immersive: Option<bool>,
    pub stereo: Option<StereoMode>,
    /// Minimum frame height in pixels.
    pub min_height: Option<u32>,
    /// Items must carry all of these tags (case-insensitive).
    pub tags: Vec<String>,
    pub favourite: Option<bool>,
    /// Never watched.
    pub unwatched: bool,
    pub has_script: Option<bool>,
    pub min_rating: Option<f32>,
    /// Watched within the last N days.
    pub watched_within_days: Option<u32>,
    /// Added within the last N days.
    pub added_within_days: Option<u32>,
    pub sort: Sort,
    pub descending: bool,
    pub limit: Option<usize>,
    pub offset: usize,
    /// Seed for [`Sort::Random`] so paging through a shuffled list is stable.
    pub seed: Option<u64>,
}

impl ItemQuery {
    pub fn search(text: &str) -> Self {
        ItemQuery {
            text: Some(text.to_string()),
            ..Default::default()
        }
    }

    fn matches(&self, it: &Item, now: i64) -> bool {
        if self
            .source_id
            .as_ref()
            .is_some_and(|s| it.source_id.as_ref() != Some(s))
        {
            return false;
        }
        let kind = it.projection_kind.unwrap_or(ProjectionKind::Flat);
        if !self.projections.is_empty() && !self.projections.contains(&kind) {
            return false;
        }
        if self.immersive.is_some_and(|imm| kind.is_immersive() != imm) {
            return false;
        }
        if self
            .stereo
            .is_some_and(|s| it.stereo.unwrap_or_default() != s)
        {
            return false;
        }
        if self.min_height.is_some_and(|h| it.height.unwrap_or(0) < h) {
            return false;
        }
        if !self
            .tags
            .iter()
            .all(|t| it.tags.iter().any(|it| it.eq_ignore_ascii_case(t)))
        {
            return false;
        }
        if self.favourite.is_some_and(|f| it.favourite != f) {
            return false;
        }
        if self.unwatched && it.watch_count > 0 {
            return false;
        }
        if self.has_script.is_some_and(|s| it.has_script != s) {
            return false;
        }
        if self
            .min_rating
            .is_some_and(|r| it.rating.unwrap_or(0.0) < r)
        {
            return false;
        }
        if let Some(d) = self.watched_within_days {
            if it.last_watched.is_none_or(|w| now - w > d as i64 * 86400) {
                return false;
            }
        }
        if let Some(d) = self.added_within_days {
            if now - it.added_at > d as i64 * 86400 {
                return false;
            }
        }
        true
    }
}

/// A query hit with its relevance score (0 without search text).
#[derive(Debug, Clone, PartialEq)]
pub struct ScoredItem {
    pub item: Item,
    pub score: f32,
}

impl Library {
    /// Run a query. Filtering and fuzzy scoring happen in Rust over the
    /// joined item rows, which is comfortably fast for libraries of tens of
    /// thousands of videos and keeps the scoring logic testable.
    pub fn query(&self, q: &ItemQuery) -> Result<Vec<ScoredItem>> {
        let items = match &q.source_id {
            Some(s) => self.select_items("WHERE i.source_id = ?1", [s])?,
            None => self.select_items("", [])?,
        };
        let now = crate::db::now();
        let text = q.text.as_deref().map(str::trim).filter(|t| !t.is_empty());
        let mut hits: Vec<ScoredItem> = items
            .into_iter()
            .filter(|it| q.matches(it, now))
            .filter_map(|it| {
                let score = match text {
                    Some(t) => {
                        let tags = it.tags.join(" ");
                        fuzzy::score(t, &[(&it.title, 1.5), (&it.path, 1.0), (&tags, 1.2)])?
                    }
                    None => 0.0,
                };
                Some(ScoredItem { item: it, score })
            })
            .collect();

        let by_name = |a: &ScoredItem, b: &ScoredItem| {
            a.item
                .title
                .to_lowercase()
                .cmp(&b.item.title.to_lowercase())
                .then(a.item.id.cmp(&b.item.id))
        };
        match q.sort {
            Sort::Random => {
                hits.sort_by_key(|h| h.item.id);
                let mut rng =
                    rand::rngs::StdRng::seed_from_u64(q.seed.unwrap_or_else(rand::random));
                hits.shuffle(&mut rng);
            }
            sort => {
                hits.sort_by(|a, b| {
                    let ord = match sort {
                        Sort::Relevance if text.is_some() => b.score.total_cmp(&a.score),
                        Sort::Relevance | Sort::Name => by_name(a, b),
                        Sort::DateAdded => a.item.added_at.cmp(&b.item.added_at),
                        Sort::Duration => a.item.duration.cmp(&b.item.duration),
                        Sort::Rating => a
                            .item
                            .rating
                            .unwrap_or(-1.0)
                            .total_cmp(&b.item.rating.unwrap_or(-1.0)),
                        Sort::LastWatched => a.item.last_watched.cmp(&b.item.last_watched),
                        Sort::Size => a.item.size.cmp(&b.item.size),
                        Sort::Random => unreachable!(),
                    };
                    let ord = if q.descending { ord.reverse() } else { ord };
                    ord.then_with(|| by_name(a, b))
                });
            }
        }
        let start = q.offset.min(hits.len());
        let end = q.limit.map_or(hits.len(), |l| (start + l).min(hits.len()));
        Ok(hits.drain(start..end).collect())
    }

    /// Convenience: just the items.
    pub fn query_items(&self, q: &ItemQuery) -> Result<Vec<Item>> {
        Ok(self.query(q)?.into_iter().map(|s| s.item).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::items::tests::new_item;
    use fp_core::{Codec, MediaInfo, MediaTime, VideoTrackInfo};

    fn lib() -> (Library, Vec<i64>) {
        let lib = Library::open_in_memory().unwrap();
        let names = [
            "Beach_Trip_180_LR.mp4",
            "Mountain Hike 360 TB.mkv",
            "Movie.2019.3D.HSBS.mkv",
            "holiday.mp4",
            "scene_MKX200_LR.mp4",
        ];
        let ids: Vec<i64> = names
            .iter()
            .enumerate()
            .map(|(i, n)| {
                let mut ni = new_item(&format!("file:///v/{n}"));
                ni.size = Some((i as u64 + 1) * 1000);
                lib.upsert_item(&ni).unwrap()
            })
            .collect();
        let heights = [2880, 3840, 1080, 720, 4000];
        for (id, h) in ids.iter().zip(heights) {
            let info = MediaInfo {
                duration: Some(MediaTime::from_millis(h as i64 * 100)),
                video: vec![VideoTrackInfo {
                    index: 0,
                    codec: Codec::Hevc,
                    width: h * 2,
                    height: h,
                    fps: 60.0,
                    bit_depth: 8,
                    transfer: Default::default(),
                    signalled_projection: None,
                    signalled_stereo: None,
                }],
                ..Default::default()
            };
            lib.apply_media_info(*id, &info).unwrap();
        }
        (lib, ids)
    }

    fn titles(v: &[ScoredItem]) -> Vec<&str> {
        v.iter().map(|s| s.item.title.as_str()).collect()
    }

    #[test]
    fn filters() {
        let (lib, ids) = lib();
        let q = |q: ItemQuery| lib.query(&q).unwrap();
        assert_eq!(
            q(ItemQuery {
                projections: vec![ProjectionKind::Equirect180],
                ..Default::default()
            })
            .len(),
            1
        );
        assert_eq!(
            q(ItemQuery {
                immersive: Some(true),
                ..Default::default()
            })
            .len(),
            3
        );
        assert_eq!(
            q(ItemQuery {
                immersive: Some(false),
                ..Default::default()
            })
            .len(),
            2
        );
        assert_eq!(
            q(ItemQuery {
                stereo: Some(StereoMode::Ou),
                ..Default::default()
            })
            .len(),
            1
        );
        assert_eq!(
            q(ItemQuery {
                min_height: Some(2880),
                ..Default::default()
            })
            .len(),
            3
        );
        lib.add_tag(ids[0], "Outdoor").unwrap();
        lib.add_tag(ids[1], "outdoor").unwrap();
        lib.add_tag(ids[1], "Nature").unwrap();
        assert_eq!(
            q(ItemQuery {
                tags: vec!["OUTDOOR".into()],
                ..Default::default()
            })
            .len(),
            2
        );
        assert_eq!(
            q(ItemQuery {
                tags: vec!["outdoor".into(), "nature".into()],
                ..Default::default()
            })
            .len(),
            1
        );
        lib.set_favourite(ids[3], true).unwrap();
        assert_eq!(
            titles(&q(ItemQuery {
                favourite: Some(true),
                ..Default::default()
            })),
            ["holiday.mp4"]
        );
        lib.record_watch(ids[0], None, true).unwrap();
        assert_eq!(
            q(ItemQuery {
                unwatched: true,
                ..Default::default()
            })
            .len(),
            4
        );
        assert_eq!(
            q(ItemQuery {
                watched_within_days: Some(1),
                ..Default::default()
            })
            .len(),
            1
        );
        assert_eq!(
            q(ItemQuery {
                added_within_days: Some(1),
                ..Default::default()
            })
            .len(),
            5
        );
        lib.set_scripts(
            ids[4],
            &[crate::ScriptRef {
                axis: "main".into(),
                uri: "x".into(),
            }],
        )
        .unwrap();
        assert_eq!(
            q(ItemQuery {
                has_script: Some(true),
                ..Default::default()
            })
            .len(),
            1
        );
        assert_eq!(
            q(ItemQuery {
                has_script: Some(false),
                ..Default::default()
            })
            .len(),
            4
        );
        lib.set_rating(ids[2], Some(4.0)).unwrap();
        assert_eq!(
            q(ItemQuery {
                min_rating: Some(3.5),
                ..Default::default()
            })
            .len(),
            1
        );
        assert_eq!(
            q(ItemQuery {
                source_id: Some("nope".into()),
                ..Default::default()
            })
            .len(),
            0
        );
    }

    #[test]
    fn search_and_sort() {
        let (lib, ids) = lib();
        let r = lib.query(&ItemQuery::search("beach")).unwrap();
        assert_eq!(titles(&r), ["Beach_Trip_180_LR.mp4"]);
        // Fuzzy subsequence + relevance ordering.
        let r = lib.query(&ItemQuery::search("mkx")).unwrap();
        assert_eq!(r[0].item.title, "scene_MKX200_LR.mp4");
        // Tags are searchable.
        lib.add_tag(ids[3], "Family").unwrap();
        assert_eq!(
            titles(&lib.query(&ItemQuery::search("family")).unwrap()),
            ["holiday.mp4"]
        );
        // Typos.
        assert_eq!(
            titles(&lib.query(&ItemQuery::search("mountian")).unwrap()),
            ["Mountain Hike 360 TB.mkv"]
        );

        let names = |s: Sort, desc: bool| {
            titles(
                &lib.query(&ItemQuery {
                    sort: s,
                    descending: desc,
                    ..Default::default()
                })
                .unwrap(),
            )
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
        };
        assert_eq!(names(Sort::Name, false)[0], "Beach_Trip_180_LR.mp4");
        assert_eq!(names(Sort::Name, true)[0], "scene_MKX200_LR.mp4");
        assert_eq!(names(Sort::Duration, true)[0], "scene_MKX200_LR.mp4");
        assert_eq!(names(Sort::Size, false)[0], "Beach_Trip_180_LR.mp4");
        lib.set_rating(ids[3], Some(5.0)).unwrap();
        assert_eq!(names(Sort::Rating, true)[0], "holiday.mp4");
        lib.record_watch(ids[2], None, false).unwrap();
        assert_eq!(names(Sort::LastWatched, true)[0], "Movie.2019.3D.HSBS.mkv");

        let rq = |seed| ItemQuery {
            sort: Sort::Random,
            seed: Some(seed),
            ..Default::default()
        };
        let a = titles(&lib.query(&rq(7)).unwrap())
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        let b = titles(&lib.query(&rq(7)).unwrap())
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        assert_eq!(a, b);
        assert_eq!(a.len(), 5);

        let page = lib
            .query(&ItemQuery {
                sort: Sort::Name,
                offset: 1,
                limit: Some(2),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].item.title, "holiday.mp4");
        assert!(lib
            .query(&ItemQuery {
                offset: 50,
                ..Default::default()
            })
            .unwrap()
            .is_empty());
    }

    #[test]
    fn query_serde_roundtrip() {
        let q = ItemQuery {
            text: Some("x".into()),
            projections: vec![ProjectionKind::Fisheye],
            stereo: Some(StereoMode::Sbs),
            sort: Sort::Rating,
            ..Default::default()
        };
        let j = serde_json::to_string(&q).unwrap();
        assert_eq!(serde_json::from_str::<ItemQuery>(&j).unwrap(), q);
        // Missing fields default (forward compatible smart playlists).
        assert_eq!(
            serde_json::from_str::<ItemQuery>("{}").unwrap(),
            ItemQuery::default()
        );
    }
}
