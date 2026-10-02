//! Media segments shared by the HLS and DASH parsers, and a sequential
//! fetcher that turns a segment list into a byte stream.

use crate::error::Result;
use crate::http::{check_status, HttpClient};
use bytes::Bytes;

/// A byte sub-range of a resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub offset: u64,
    pub length: u64,
}

impl ByteRange {
    /// Inclusive end offset.
    pub fn end(&self) -> u64 {
        self.offset + self.length.saturating_sub(1)
    }
}

/// HLS `EXT-X-KEY` information carried by a segment.
#[derive(Debug, Clone, PartialEq)]
pub struct EncryptionKey {
    /// "AES-128", "SAMPLE-AES", ...
    pub method: String,
    pub uri: Option<String>,
    pub iv: Option<String>,
}

/// One fetchable piece of a stream.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    /// Absolute URL.
    pub url: String,
    pub range: Option<ByteRange>,
    /// Seconds (0 for init segments).
    pub duration: f64,
    /// HLS discontinuity before this segment.
    pub discontinuity: bool,
    pub key: Option<EncryptionKey>,
}

impl Segment {
    pub fn new(url: impl Into<String>, duration: f64) -> Self {
        Segment {
            url: url.into(),
            range: None,
            duration,
            discontinuity: false,
            key: None,
        }
    }
}

/// Fetches an optional init segment and then media segments in order.
pub struct SegmentStream {
    client: HttpClient,
    init: Option<Segment>,
    segments: Vec<Segment>,
    next: usize,
    init_sent: bool,
}

impl SegmentStream {
    pub fn new(client: HttpClient, init: Option<Segment>, segments: Vec<Segment>) -> Self {
        SegmentStream {
            client,
            init,
            segments,
            next: 0,
            init_sent: false,
        }
    }

    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    pub fn total_duration(&self) -> f64 {
        self.segments.iter().map(|s| s.duration).sum()
    }

    /// Position the stream at the segment containing `secs`; returns that
    /// segment's start time. The init segment is re-sent first.
    pub fn seek_to_time(&mut self, secs: f64) -> f64 {
        let mut t = 0.0;
        self.next = self.segments.len();
        for (i, s) in self.segments.iter().enumerate() {
            if t + s.duration > secs {
                self.next = i;
                break;
            }
            t += s.duration;
        }
        self.init_sent = false;
        t
    }

    /// Index of the next media segment to be fetched.
    pub fn position(&self) -> usize {
        self.next
    }

    /// Next piece of data: init segment (once after construction or seek),
    /// then each media segment. `None` at the end.
    pub async fn next_segment(&mut self) -> Option<Result<Bytes>> {
        if !self.init_sent {
            self.init_sent = true;
            if let Some(init) = self.init.clone() {
                return Some(fetch_segment(&self.client, &init).await);
            }
        }
        let seg = self.segments.get(self.next)?.clone();
        self.next += 1;
        if seg.key.as_ref().is_some_and(|k| k.method != "NONE") {
            return Some(Err(crate::SourceError::Unsupported(format!(
                "encrypted HLS segments ({})",
                seg.key.as_ref().unwrap().method
            ))));
        }
        Some(fetch_segment(&self.client, &seg).await)
    }
}

/// GET one segment, honouring its byte range.
pub async fn fetch_segment(client: &HttpClient, seg: &Segment) -> Result<Bytes> {
    let resp = match seg.range {
        Some(r) => client.get_range(&seg.url, r.offset, Some(r.end())).await?,
        None => {
            client
                .send(reqwest::Method::GET, &seg.url, Default::default(), None)
                .await?
        }
    };
    let partial = resp.status() == reqwest::StatusCode::PARTIAL_CONTENT;
    let body = check_status(resp)?.bytes().await?;
    match seg.range {
        // Server ignored the range and sent the whole resource.
        Some(r) if !partial => {
            let start = (r.offset as usize).min(body.len());
            let end = (start + r.length as usize).min(body.len());
            Ok(body.slice(start..end))
        }
        _ => Ok(body),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seek_picks_containing_segment() {
        let segs = (0..5)
            .map(|i| Segment::new(format!("http://h/{i}.ts"), 4.0))
            .collect();
        let mut s = SegmentStream::new(HttpClient::new(None).unwrap(), None, segs);
        assert_eq!(s.total_duration(), 20.0);
        assert_eq!(s.seek_to_time(9.0), 8.0);
        assert_eq!(s.position(), 2);
        assert_eq!(s.seek_to_time(100.0), 20.0);
        assert_eq!(s.position(), 5);
        assert_eq!(
            ByteRange {
                offset: 10,
                length: 5
            }
            .end(),
            14
        );
    }
}
