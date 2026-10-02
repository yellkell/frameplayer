//! MPEG-DASH MPD parsing (ISO/IEC 23009-1) into representations with
//! resolved segment URL lists.
//!
//! Supports SegmentTemplate (with `$Number$`/`$Time$` and SegmentTimeline),
//! SegmentBase (single file with index range) and SegmentList, with
//! template/BaseURL inheritance from MPD → Period → AdaptationSet →
//! Representation. Dynamic (live) MPDs are parsed but only their timeline
//! segments are listed; no availability-time arithmetic.

use crate::error::{Result, SourceError};
use crate::segments::{ByteRange, Segment};
use crate::xml::{self, Element};
use url::Url;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Mpd {
    pub dynamic: bool,
    /// mediaPresentationDuration, seconds.
    pub duration: Option<f64>,
    pub periods: Vec<Period>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Period {
    pub id: Option<String>,
    pub start: f64,
    pub duration: Option<f64>,
    pub adaptation_sets: Vec<AdaptationSet>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContentType {
    Video,
    Audio,
    Text,
    #[default]
    Other,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct AdaptationSet {
    pub id: Option<String>,
    pub content_type: ContentType,
    pub mime_type: Option<String>,
    pub lang: Option<String>,
    pub representations: Vec<Representation>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Representation {
    pub id: String,
    pub bandwidth: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub codecs: Option<String>,
    pub frame_rate: Option<f64>,
    pub mime_type: Option<String>,
    pub init: Option<Segment>,
    pub segments: Vec<Segment>,
    /// SegmentBase@indexRange (sidx box location), when used.
    pub index_range: Option<ByteRange>,
}

impl Mpd {
    /// All video representations across periods (first period for VOD).
    pub fn video_representations(&self) -> impl Iterator<Item = &Representation> {
        self.periods
            .iter()
            .take(1)
            .flat_map(|p| p.adaptation_sets.iter())
            .filter(|a| a.content_type == ContentType::Video)
            .flat_map(|a| a.representations.iter())
    }

    /// Highest-bandwidth video representation with height ≤ `max_height`.
    pub fn best_video(&self, max_height: Option<u32>) -> Option<&Representation> {
        let fits = |r: &&Representation| max_height.is_none_or(|m| r.height.is_none_or(|h| h <= m));
        self.video_representations()
            .filter(fits)
            .max_by_key(|r| r.bandwidth)
            .or_else(|| self.video_representations().min_by_key(|r| r.bandwidth))
    }
}

/// Parse an ISO 8601 duration such as `PT1H2M3.5S` or `P1DT2H` to seconds.
pub fn parse_duration(s: &str) -> Option<f64> {
    let s = s.trim();
    let rest = s.strip_prefix('P')?;
    let (date, time) = rest.split_once('T').unwrap_or((rest, ""));
    let mut total = 0.0;
    let mut num = String::new();
    for c in date.chars() {
        match c {
            '0'..='9' | '.' => num.push(c),
            'Y' => total += num.drain(..).as_str().parse::<f64>().ok()? * 365.0 * 86400.0,
            'M' => total += num.drain(..).as_str().parse::<f64>().ok()? * 30.0 * 86400.0,
            'W' => total += num.drain(..).as_str().parse::<f64>().ok()? * 7.0 * 86400.0,
            'D' => total += num.drain(..).as_str().parse::<f64>().ok()? * 86400.0,
            _ => return None,
        }
    }
    for c in time.chars() {
        match c {
            '0'..='9' | '.' => num.push(c),
            'H' => total += num.drain(..).as_str().parse::<f64>().ok()? * 3600.0,
            'M' => total += num.drain(..).as_str().parse::<f64>().ok()? * 60.0,
            'S' => total += num.drain(..).as_str().parse::<f64>().ok()?,
            _ => return None,
        }
    }
    num.is_empty().then_some(total)
}

fn parse_frame_rate(s: &str) -> Option<f64> {
    match s.split_once('/') {
        Some((n, d)) => {
            let d: f64 = d.parse().ok()?;
            (d != 0.0)
                .then(|| n.parse::<f64>().ok().map(|n| n / d))
                .flatten()
        }
        None => s.parse().ok(),
    }
}

fn parse_range(s: &str) -> Option<ByteRange> {
    let (a, b) = s.split_once('-')?;
    let a: u64 = a.trim().parse().ok()?;
    let b: u64 = b.trim().parse().ok()?;
    (b >= a).then(|| ByteRange {
        offset: a,
        length: b - a + 1,
    })
}

fn join(base: &Url, el: &Element) -> Url {
    match el.child_text("BaseURL") {
        Some(b) if !b.is_empty() => base.join(b).unwrap_or_else(|_| base.clone()),
        _ => base.clone(),
    }
}

/// Merged view of SegmentTemplate/SegmentBase/SegmentList attributes along
/// the inheritance chain (later levels override earlier ones).
#[derive(Clone, Default)]
struct Inherited<'a> {
    template: Vec<&'a Element>,
    base: Vec<&'a Element>,
    list: Vec<&'a Element>,
}

impl<'a> Inherited<'a> {
    fn push(&self, el: &'a Element) -> Inherited<'a> {
        let mut n = self.clone();
        if let Some(t) = el.child("SegmentTemplate") {
            n.template.push(t);
        }
        if let Some(b) = el.child("SegmentBase") {
            n.base.push(b);
        }
        if let Some(l) = el.child("SegmentList") {
            n.list.push(l);
        }
        n
    }
}

fn get_attr<'a>(chain: &[&'a Element], name: &str) -> Option<&'a str> {
    chain.iter().rev().find_map(|e| e.attr(name))
}

fn get_child<'a>(chain: &[&'a Element], name: &str) -> Option<&'a Element> {
    chain.iter().rev().find_map(|e| e.child(name))
}

/// Expand `$RepresentationID$`, `$Number%05d$`, `$Bandwidth$`, `$Time$`, `$$`.
pub fn expand_template(tpl: &str, rep_id: &str, bandwidth: u64, number: u64, time: u64) -> String {
    let mut out = String::with_capacity(tpl.len() + 16);
    let mut parts = tpl.split('$');
    if let Some(first) = parts.next() {
        out.push_str(first);
    }
    let rest: Vec<&str> = parts.collect();
    let mut i = 0;
    while i < rest.len() {
        let ident = rest[i];
        // Identifiers sit at even positions of `rest`; literal text after.
        let (name, fmt) = match ident.split_once('%') {
            Some((n, f)) => (n, Some(f)),
            None => (ident, None),
        };
        let value: Option<String> = match name {
            "" => Some("$".into()),
            "RepresentationID" => Some(rep_id.into()),
            "Number" => Some(format_num(number, fmt)),
            "Bandwidth" => Some(format_num(bandwidth, fmt)),
            "Time" => Some(format_num(time, fmt)),
            _ => None,
        };
        match value {
            Some(v) => out.push_str(&v),
            None => {
                out.push('$');
                out.push_str(ident);
                out.push('$');
            }
        }
        if let Some(lit) = rest.get(i + 1) {
            out.push_str(lit);
        }
        i += 2;
    }
    out
}

fn format_num(v: u64, fmt: Option<&str>) -> String {
    match fmt
        .and_then(|f| f.strip_suffix('d'))
        .and_then(|w| w.trim_start_matches('0').parse::<usize>().ok().or(Some(0)))
    {
        Some(width) if width > 0 => format!("{v:0width$}"),
        _ => v.to_string(),
    }
}

/// Parse an MPD fetched from `mpd_url`.
pub fn parse(text: &str, mpd_url: &str) -> Result<Mpd> {
    let root = xml::parse(text)?;
    if root.name != "MPD" {
        return Err(SourceError::parse("not an MPD document"));
    }
    let base = join(&Url::parse(mpd_url)?, &root);
    let dynamic = root.attr("type") == Some("dynamic");
    let duration = root
        .attr("mediaPresentationDuration")
        .and_then(parse_duration);
    let periods_el: Vec<&Element> = root.children_named("Period").collect();
    let mut periods = Vec::new();
    let mut start_acc = 0.0;
    for (pi, pe) in periods_el.iter().enumerate() {
        let start = pe
            .attr("start")
            .and_then(parse_duration)
            .unwrap_or(start_acc);
        let next_start = periods_el
            .get(pi + 1)
            .and_then(|n| n.attr("start"))
            .and_then(parse_duration);
        let pdur = pe
            .attr("duration")
            .and_then(parse_duration)
            .or_else(|| next_start.map(|n| n - start))
            .or_else(|| duration.map(|d| d - start));
        start_acc = start + pdur.unwrap_or(0.0);
        let pbase = join(&base, pe);
        let pinh = Inherited::default().push(pe);
        let mut sets = Vec::new();
        for ae in pe.children_named("AdaptationSet") {
            let abase = join(&pbase, ae);
            let ainh = pinh.push(ae);
            let mut reps = Vec::new();
            for re in ae.children_named("Representation") {
                let rbase = join(&abase, re);
                let rinh = ainh.push(re);
                let id = re.attr("id").unwrap_or("").to_string();
                let bandwidth = re
                    .attr("bandwidth")
                    .and_then(|b| b.parse().ok())
                    .unwrap_or(0);
                let mut rep = Representation {
                    id: id.clone(),
                    bandwidth,
                    width: re
                        .attr("width")
                        .or(ae.attr("width"))
                        .and_then(|v| v.parse().ok()),
                    height: re
                        .attr("height")
                        .or(ae.attr("height"))
                        .and_then(|v| v.parse().ok()),
                    codecs: re.attr("codecs").or(ae.attr("codecs")).map(str::to_string),
                    frame_rate: re
                        .attr("frameRate")
                        .or(ae.attr("frameRate"))
                        .and_then(parse_frame_rate),
                    mime_type: re
                        .attr("mimeType")
                        .or(ae.attr("mimeType"))
                        .map(str::to_string),
                    ..Default::default()
                };
                build_segments(&mut rep, &rinh, &rbase, pdur)?;
                reps.push(rep);
            }
            let mime = ae
                .attr("mimeType")
                .map(str::to_string)
                .or_else(|| reps.first().and_then(|r| r.mime_type.clone()));
            let ct = ae.attr("contentType").map(str::to_string).or_else(|| {
                mime.as_deref()
                    .and_then(|m| m.split('/').next())
                    .map(str::to_string)
            });
            let content_type = match ct.as_deref() {
                Some("video") => ContentType::Video,
                Some("audio") => ContentType::Audio,
                Some("text") | Some("application") => ContentType::Text,
                _ => ContentType::Other,
            };
            sets.push(AdaptationSet {
                id: ae.attr("id").map(str::to_string),
                content_type,
                mime_type: mime,
                lang: ae.attr("lang").map(str::to_string),
                representations: reps,
            });
        }
        periods.push(Period {
            id: pe.attr("id").map(str::to_string),
            start,
            duration: pdur,
            adaptation_sets: sets,
        });
    }
    Ok(Mpd {
        dynamic,
        duration,
        periods,
    })
}

fn build_segments(
    rep: &mut Representation,
    inh: &Inherited<'_>,
    base: &Url,
    period_dur: Option<f64>,
) -> Result<()> {
    let abs = |u: &str| {
        base.join(u)
            .map(|u| u.to_string())
            .unwrap_or_else(|_| u.to_string())
    };
    if !inh.template.is_empty() {
        let t = &inh.template;
        let timescale: f64 = get_attr(t, "timescale")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1.0);
        let start_number: u64 = get_attr(t, "startNumber")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1);
        let pto: u64 = get_attr(t, "presentationTimeOffset")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        if let Some(init) = get_attr(t, "initialization") {
            rep.init = Some(Segment::new(
                abs(&expand_template(init, &rep.id, rep.bandwidth, 0, 0)),
                0.0,
            ));
        }
        let Some(media) = get_attr(t, "media") else {
            return Ok(());
        };
        if let Some(tl) = get_child(t, "SegmentTimeline") {
            let s_els: Vec<&Element> = tl.children_named("S").collect();
            let mut time: u64 = 0;
            let mut number = start_number;
            let end_time = period_dur.map(|d| pto + (d * timescale) as u64);
            for (i, s) in s_els.iter().enumerate() {
                if let Some(t0) = s.attr("t").and_then(|v| v.parse().ok()) {
                    time = t0;
                }
                let d: u64 = s
                    .attr("d")
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| SourceError::parse("S without @d"))?;
                if d == 0 {
                    continue;
                }
                let r: i64 = s.attr("r").and_then(|v| v.parse().ok()).unwrap_or(0);
                let count = if r >= 0 {
                    r as u64 + 1
                } else {
                    let limit = s_els
                        .get(i + 1)
                        .and_then(|n| n.attr("t"))
                        .and_then(|v| v.parse::<u64>().ok())
                        .or(end_time);
                    match limit {
                        Some(l) if l > time => (l - time).div_ceil(d),
                        _ => 1,
                    }
                };
                for _ in 0..count {
                    rep.segments.push(Segment::new(
                        abs(&expand_template(
                            media,
                            &rep.id,
                            rep.bandwidth,
                            number,
                            time,
                        )),
                        d as f64 / timescale,
                    ));
                    time += d;
                    number += 1;
                }
            }
        } else if let Some(dur) = get_attr(t, "duration").and_then(|v| v.parse::<f64>().ok()) {
            let seg_secs = dur / timescale;
            let total = period_dur.ok_or_else(|| {
                SourceError::Unsupported("live DASH without SegmentTimeline".into())
            })?;
            let count = (total / seg_secs).ceil().max(0.0) as u64;
            for i in 0..count {
                let d = if i + 1 == count {
                    total - seg_secs * i as f64
                } else {
                    seg_secs
                };
                let time = pto + (i as f64 * dur) as u64;
                rep.segments.push(Segment::new(
                    abs(&expand_template(
                        media,
                        &rep.id,
                        rep.bandwidth,
                        start_number + i,
                        time,
                    )),
                    d,
                ));
            }
        }
        return Ok(());
    }
    if !inh.list.is_empty() {
        let l = &inh.list;
        let timescale: f64 = get_attr(l, "timescale")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1.0);
        let dur = get_attr(l, "duration")
            .and_then(|v| v.parse::<f64>().ok())
            .map(|d| d / timescale)
            .unwrap_or(0.0);
        if let Some(init) = get_child(l, "Initialization") {
            let mut s = Segment::new(
                init.attr("sourceURL")
                    .map(abs)
                    .unwrap_or_else(|| base.to_string()),
                0.0,
            );
            s.range = init.attr("range").and_then(parse_range);
            rep.init = Some(s);
        }
        let list_el = l.last().expect("non-empty");
        for su in list_el.children_named("SegmentURL") {
            let mut s = Segment::new(
                su.attr("media")
                    .map(abs)
                    .unwrap_or_else(|| base.to_string()),
                dur,
            );
            s.range = su.attr("mediaRange").and_then(parse_range);
            rep.segments.push(s);
        }
        return Ok(());
    }
    // SegmentBase or bare BaseURL: one segment covering the whole file.
    if !inh.base.is_empty() {
        let b = &inh.base;
        rep.index_range = get_attr(b, "indexRange").and_then(parse_range);
        if let Some(init) = get_child(b, "Initialization") {
            let mut s = Segment::new(
                init.attr("sourceURL")
                    .map(abs)
                    .unwrap_or_else(|| base.to_string()),
                0.0,
            );
            s.range = init.attr("range").and_then(parse_range);
            rep.init = Some(s);
        }
    }
    rep.segments
        .push(Segment::new(base.to_string(), period_dur.unwrap_or(0.0)));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("PT1H2M3.5S"), Some(3723.5));
        assert_eq!(parse_duration("P1DT2H"), Some(93600.0));
        assert_eq!(parse_duration("PT0S"), Some(0.0));
        assert_eq!(parse_duration("1H"), None);
        assert_eq!(
            parse_frame_rate("30000/1001").map(|f| (f * 1000.0).round()),
            Some(29970.0)
        );
    }

    #[test]
    fn template_expansion() {
        assert_eq!(
            expand_template("$RepresentationID$/seg-$Number%05d$.m4s", "v1", 0, 42, 0),
            "v1/seg-00042.m4s"
        );
        assert_eq!(
            expand_template("t$Time$_$Bandwidth$_$$.mp4", "x", 500, 0, 9000),
            "t9000_500_$.mp4"
        );
        assert_eq!(expand_template("plain.mp4", "x", 0, 1, 0), "plain.mp4");
        assert_eq!(
            expand_template("$Unknown$-$Number$", "x", 0, 3, 0),
            "$Unknown$-3"
        );
    }

    const TEMPLATE_MPD: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT10S" minBufferTime="PT2S">
  <BaseURL>media/</BaseURL>
  <Period id="p0">
    <AdaptationSet mimeType="video/mp4" segmentAlignment="true">
      <SegmentTemplate timescale="1000" duration="4000" startNumber="1" media="$RepresentationID$/$Number%03d$.m4s" initialization="$RepresentationID$/init.mp4"/>
      <Representation id="8k" bandwidth="60000000" width="7680" height="3840" codecs="hvc1.2.4.L183" frameRate="60"/>
      <Representation id="4k" bandwidth="20000000" width="3840" height="1920" codecs="hvc1.2.4.L153" frameRate="60"/>
    </AdaptationSet>
    <AdaptationSet contentType="audio" lang="en">
      <Representation id="a" bandwidth="128000" mimeType="audio/mp4" codecs="mp4a.40.2">
        <SegmentTemplate timescale="48000" media="a/$Time$.m4s" initialization="a/init.mp4">
          <SegmentTimeline><S t="0" d="96000" r="2"/><S d="48000"/></SegmentTimeline>
        </SegmentTemplate>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;

    #[test]
    fn segment_template() {
        let mpd = parse(TEMPLATE_MPD, "https://cdn/x/manifest.mpd").unwrap();
        assert!(!mpd.dynamic);
        assert_eq!(mpd.duration, Some(10.0));
        let p = &mpd.periods[0];
        assert_eq!(p.adaptation_sets[0].content_type, ContentType::Video);
        let v = &p.adaptation_sets[0].representations[0];
        assert_eq!(
            (v.width, v.height, v.frame_rate),
            (Some(7680), Some(3840), Some(60.0))
        );
        assert_eq!(
            v.init.as_ref().unwrap().url,
            "https://cdn/x/media/8k/init.mp4"
        );
        assert_eq!(v.segments.len(), 3);
        assert_eq!(v.segments[0].url, "https://cdn/x/media/8k/001.m4s");
        assert_eq!(v.segments[2].url, "https://cdn/x/media/8k/003.m4s");
        assert!((v.segments[2].duration - 2.0).abs() < 1e-9);
        let a = &p.adaptation_sets[1];
        assert_eq!(a.content_type, ContentType::Audio);
        let ar = &a.representations[0];
        let urls: Vec<_> = ar
            .segments
            .iter()
            .map(|s| s.url.rsplit('/').next().unwrap().to_string())
            .collect();
        assert_eq!(urls, ["0.m4s", "96000.m4s", "192000.m4s", "288000.m4s"]);
        assert_eq!(mpd.best_video(Some(2160)).unwrap().id, "4k");
        assert_eq!(mpd.best_video(None).unwrap().id, "8k");
    }

    #[test]
    fn segment_base_and_list() {
        let text = r#"<MPD type="static" mediaPresentationDuration="PT1M">
          <Period>
            <AdaptationSet mimeType="video/mp4">
              <Representation id="1" bandwidth="1000" height="1080">
                <BaseURL>https://other/video_1080.mp4</BaseURL>
                <SegmentBase indexRange="862-1381"><Initialization range="0-861"/></SegmentBase>
              </Representation>
              <Representation id="2" bandwidth="500" height="720">
                <SegmentList timescale="10" duration="100">
                  <Initialization sourceURL="init720.mp4"/>
                  <SegmentURL media="s1.m4s"/><SegmentURL media="all.m4s" mediaRange="100-199"/>
                </SegmentList>
              </Representation>
              <Representation id="3" bandwidth="100"><BaseURL>plain.mp4</BaseURL></Representation>
            </AdaptationSet>
          </Period></MPD>"#;
        let mpd = parse(text, "http://h/d/m.mpd").unwrap();
        let reps = &mpd.periods[0].adaptation_sets[0].representations;
        let r1 = &reps[0];
        assert_eq!(
            r1.index_range,
            Some(ByteRange {
                offset: 862,
                length: 520
            })
        );
        assert_eq!(
            r1.init.as_ref().unwrap().range,
            Some(ByteRange {
                offset: 0,
                length: 862
            })
        );
        assert_eq!(
            r1.init.as_ref().unwrap().url,
            "https://other/video_1080.mp4"
        );
        assert_eq!(r1.segments.len(), 1);
        assert_eq!(r1.segments[0].duration, 60.0);
        let r2 = &reps[1];
        assert_eq!(r2.init.as_ref().unwrap().url, "http://h/d/init720.mp4");
        assert_eq!(r2.segments.len(), 2);
        assert_eq!(
            r2.segments[1].range,
            Some(ByteRange {
                offset: 100,
                length: 100
            })
        );
        assert_eq!(r2.segments[0].duration, 10.0);
        assert_eq!(reps[2].segments[0].url, "http://h/d/plain.mp4");
        assert!(parse("<html/>", "http://h/").is_err());
    }
}
