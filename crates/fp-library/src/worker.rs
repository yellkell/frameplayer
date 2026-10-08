//! Background metadata and thumbnail extraction.
//!
//! The library does not decode video itself. The app implements
//! [`MediaProber`] (with FFmpeg); the [`MetadataWorker`] takes videos that
//! lack metadata or a thumbnail from the database, asks the prober, refines
//! the detected format with container metadata and the frame size, and
//! writes `<id>.jpg` thumbnails and `<id>_strip.jpg` scrub-preview sheets to
//! a cache folder.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use fp_core::format::{ContainerHints, resolve};
use fp_core::{StereoLayout, VideoFormat};

use crate::error::{Error, Result};
use crate::image::{RgbaImage, tile};
use crate::library::{Library, StoredProbe};
use crate::record::{MediaId, MediaRecord, PreviewStrip};

/// What a prober learned about a video's container and stream.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProbeInfo {
    /// Seconds; `None` for live streams or when unknown.
    pub duration: Option<f64>,
    /// Coded frame width in pixels (whole frame, both eyes).
    pub width: u32,
    /// Coded frame height in pixels.
    pub height: u32,
    /// Codec name (`"hevc"`, `"av1"`, `"h264"`...).
    pub video_codec: Option<String>,
    /// Spherical / stereo metadata from the container.
    pub hints: ContainerHints,
}

/// Which part of the frame a thumbnail shows, so stereo videos give a
/// single-eye picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ThumbnailCrop {
    /// Left half (side-by-side stereo).
    LeftEye,
    /// Top half (top/bottom stereo).
    TopEye,
    /// The whole frame (mono).
    Full,
}

impl ThumbnailCrop {
    /// The crop for a format: SBS → left half, TB → top half, mono → full.
    pub fn for_format(format: &VideoFormat) -> ThumbnailCrop {
        match format.stereo {
            StereoLayout::SideBySide => ThumbnailCrop::LeftEye,
            StereoLayout::TopBottom => ThumbnailCrop::TopEye,
            StereoLayout::Mono => ThumbnailCrop::Full,
        }
    }
}

/// Reads metadata and frames from media. Implemented by the app on top of
/// FFmpeg; the library only calls it from the worker thread.
pub trait MediaProber: Send + Sync {
    /// Container and stream information for `location` (path or URL).
    fn probe(&self, location: &str) -> Result<ProbeInfo>;

    /// One frame near `at_seconds`, cropped as asked, scaled to fit within
    /// `max_width` × `max_height` keeping its aspect ratio. Larger results
    /// are scaled down by the worker.
    fn thumbnail(
        &self,
        location: &str,
        at_seconds: f64,
        max_width: u32,
        max_height: u32,
        crop: ThumbnailCrop,
    ) -> Result<RgbaImage>;
}

/// Tuning for the metadata worker.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkerOptions {
    /// Where `<id>.jpg` and `<id>_strip.jpg` go. Created when needed.
    pub cache_dir: PathBuf,
    /// Thumbnail bounding box.
    pub thumbnail_max_width: u32,
    /// Thumbnail bounding box.
    pub thumbnail_max_height: u32,
    /// Where to take the thumbnail, as a fraction of the duration (the
    /// first frame when the duration is unknown).
    pub thumbnail_position: f64,
    /// Frames in the scrub preview sheet; 0 disables it.
    pub strip_frames: u32,
    /// Bounding box of one preview frame.
    pub strip_frame_max_width: u32,
    /// Bounding box of one preview frame.
    pub strip_frame_max_height: u32,
    /// Frames per row in the sheet; `None` puts them all in one row
    /// (wrapped if the row would exceed the JPEG size limit).
    pub strip_columns: Option<u32>,
    /// JPEG quality, 1–100.
    pub jpeg_quality: u8,
    /// Rows fetched from the database at a time.
    pub batch_size: usize,
    /// Also look for new work this often without being woken; `None` waits
    /// for [`MetadataWorker::wake`].
    pub poll_interval: Option<Duration>,
}

impl WorkerOptions {
    /// Defaults with a specific cache folder.
    pub fn with_cache_dir(cache_dir: impl Into<PathBuf>) -> WorkerOptions {
        WorkerOptions {
            cache_dir: cache_dir.into(),
            thumbnail_max_width: 640,
            thumbnail_max_height: 640,
            thumbnail_position: 0.2,
            strip_frames: 20,
            strip_frame_max_width: 160,
            strip_frame_max_height: 160,
            strip_columns: None,
            jpeg_quality: 82,
            batch_size: 16,
            poll_interval: None,
        }
    }
}

impl Default for WorkerOptions {
    /// Cache in `$XDG_CACHE_HOME/frameplayer/thumbnails`.
    fn default() -> Self {
        WorkerOptions::with_cache_dir(fp_core::dirs::cache_dir().join("thumbnails"))
    }
}

/// Progress reported by the worker.
#[derive(Clone, Debug, PartialEq)]
pub enum WorkerEvent {
    /// A pass over pending videos began.
    Started {
        /// Videos waiting.
        pending: usize,
    },
    /// A video was probed. `warning` explains a missing thumbnail or
    /// preview sheet.
    Processed {
        /// The video.
        id: MediaId,
        /// Videos handled in this pass, this one included.
        done: usize,
        /// `done` plus what is still waiting.
        total: usize,
        /// A thumbnail was written.
        thumbnail: bool,
        /// A preview sheet was written.
        preview_strip: bool,
        /// Non-fatal problem.
        warning: Option<String>,
    },
    /// Probing failed; the error is stored on the row and the video is not
    /// retried until [`Library::retry_failed_probes`].
    Failed {
        /// The video.
        id: MediaId,
        /// Videos handled in this pass, this one included.
        done: usize,
        /// `done` plus what is still waiting.
        total: usize,
        /// What went wrong.
        error: String,
    },
    /// Nothing left to do for now.
    Idle {
        /// Videos probed successfully in this pass.
        processed: usize,
        /// Videos that failed in this pass.
        failed: usize,
    },
    /// A database or cache-folder error interrupted a pass.
    Error(String),
    /// The worker thread exited.
    Stopped,
}

/// Counts from one [`process_pending`] pass.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorkerStats {
    /// Videos probed successfully.
    pub processed: usize,
    /// Videos whose probe failed.
    pub failed: usize,
}

/// Writes `data` to `path` through a temporary file, so readers never see a
/// half-written JPEG.
fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("jpg.tmp");
    std::fs::write(&tmp, data).map_err(|e| Error::io(&tmp, e))?;
    std::fs::rename(&tmp, path).map_err(|e| Error::io(path, e))
}

fn thumbnail_path(dir: &Path, id: MediaId) -> PathBuf {
    dir.join(format!("{id}.jpg"))
}

fn strip_path(dir: &Path, id: MediaId) -> PathBuf {
    dir.join(format!("{id}_strip.jpg"))
}

fn make_strip(
    prober: &dyn MediaProber,
    options: &WorkerOptions,
    rec: &MediaRecord,
    duration: f64,
    crop: ThumbnailCrop,
) -> Result<PreviewStrip> {
    let n = options.strip_frames;
    let interval = duration / n as f64;
    let (mw, mh) = (
        options.strip_frame_max_width.max(1),
        options.strip_frame_max_height.max(1),
    );
    let mut frames = Vec::with_capacity(n as usize);
    for i in 0..n {
        let t = (i as f64 + 0.5) * interval;
        let img = prober.thumbnail(&rec.location, t, mw, mh, crop)?;
        frames.push(img.fit_within(mw, mh)?);
    }
    let widest = frames.iter().map(|f| f.width).max().unwrap_or(1).max(1);
    let max_columns = (u16::MAX as u32 / widest).max(1);
    let columns = options
        .strip_columns
        .unwrap_or(n)
        .clamp(1, n)
        .min(max_columns);
    let (sheet, cell_width, cell_height) = tile(&frames, columns)?;
    let path = strip_path(&options.cache_dir, rec.id);
    write_atomic(&path, &sheet.to_jpeg(options.jpeg_quality)?)?;
    Ok(PreviewStrip {
        path,
        frames: n,
        columns,
        cell_width,
        cell_height,
        interval,
    })
}

/// Probes one video and stores the result. `Err` means the probe itself
/// failed; thumbnail problems come back as a warning.
fn process_one(
    library: &Library,
    prober: &dyn MediaProber,
    options: &WorkerOptions,
    rec: &MediaRecord,
) -> Result<(bool, bool, Option<String>)> {
    let info = prober.probe(&rec.location)?;
    // Detect from the file name only, so a URL's query string cannot add
    // format tokens.
    let resolved = resolve(None, info.hints, rec.file_name(), info.width, info.height);
    // Evidence orders strongest first: keep a stronger earlier detection
    // (e.g. a format declared by a DeoVR feed) over a weaker new guess.
    let detected = if resolved.evidence <= rec.detected.evidence {
        resolved
    } else {
        rec.detected
    };
    let effective = rec.user_format.unwrap_or(detected.format);
    let crop = ThumbnailCrop::for_format(&effective);
    let duration = info
        .duration
        .filter(|d| d.is_finite() && *d > 0.0)
        .or(rec.duration);

    let mut warnings: Vec<String> = Vec::new();
    let at = duration.map_or(0.0, |d| d * options.thumbnail_position.clamp(0.0, 1.0));
    let (tw, th) = (
        options.thumbnail_max_width.max(1),
        options.thumbnail_max_height.max(1),
    );
    let thumbnail = prober
        .thumbnail(&rec.location, at, tw, th, crop)
        .and_then(|img| {
            let jpg = img.fit_within(tw, th)?.to_jpeg(options.jpeg_quality)?;
            let path = thumbnail_path(&options.cache_dir, rec.id);
            write_atomic(&path, &jpg)?;
            Ok(path)
        });
    let (thumbnail, thumb_error) = match thumbnail {
        Ok(p) => (Some(p), None),
        Err(e) => {
            let msg = format!("thumbnail: {e}");
            warnings.push(msg.clone());
            (None, Some(msg))
        }
    };

    let preview_strip = match duration {
        Some(d) if options.strip_frames > 0 && thumbnail.is_some() => {
            match make_strip(prober, options, rec, d, crop) {
                Ok(s) => Some(s),
                Err(e) => {
                    warnings.push(format!("preview strip: {e}"));
                    None
                }
            }
        }
        _ => None,
    };

    let positive = |v: u32| (v > 0).then_some(v);
    library.store_probe(
        rec.id,
        &StoredProbe {
            duration,
            width: positive(info.width),
            height: positive(info.height),
            video_codec: info.video_codec.clone(),
            detected,
            thumbnail: thumbnail.clone(),
            preview_strip: preview_strip.clone(),
            error: thumb_error,
        },
    )?;
    let warning = (!warnings.is_empty()).then(|| warnings.join("; "));
    Ok((thumbnail.is_some(), preview_strip.is_some(), warning))
}

/// Processes every pending video once, on the calling thread, until none is
/// left or `stop` becomes true. [`MetadataWorker`] runs this in the
/// background; call it directly for synchronous use (tools, tests).
pub fn process_pending(
    library: &Library,
    prober: &dyn MediaProber,
    options: &WorkerOptions,
    stop: &AtomicBool,
    on_event: &mut dyn FnMut(WorkerEvent),
) -> Result<WorkerStats> {
    let mut stats = WorkerStats::default();
    let mut total = library.pending_metadata_count()?;
    on_event(WorkerEvent::Started { pending: total });
    if total > 0 {
        std::fs::create_dir_all(&options.cache_dir)
            .map_err(|e| Error::io(&options.cache_dir, e))?;
    }
    let mut done = 0;
    'outer: while !stop.load(Ordering::Relaxed) {
        let batch = library.pending_metadata(options.batch_size.max(1))?;
        if batch.is_empty() {
            break;
        }
        for rec in &batch {
            if stop.load(Ordering::Relaxed) {
                break 'outer;
            }
            done += 1;
            total = total.max(done);
            match process_one(library, prober, options, rec) {
                Ok((thumbnail, preview_strip, warning)) => {
                    stats.processed += 1;
                    on_event(WorkerEvent::Processed {
                        id: rec.id,
                        done,
                        total,
                        thumbnail,
                        preview_strip,
                        warning,
                    });
                }
                Err(e) => {
                    let error = e.to_string();
                    library.store_probe_error(rec.id, &error)?;
                    stats.failed += 1;
                    on_event(WorkerEvent::Failed {
                        id: rec.id,
                        done,
                        total,
                        error,
                    });
                }
            }
        }
        total = total.max(done + library.pending_metadata_count()?);
    }
    on_event(WorkerEvent::Idle {
        processed: stats.processed,
        failed: stats.failed,
    });
    Ok(stats)
}

/// A background thread that keeps metadata and thumbnails up to date.
///
/// It makes a pass when spawned, then sleeps until [`wake`](Self::wake)
/// (call it after a scan or upsert) or the poll interval. Dropping it (or
/// calling [`stop`](Self::stop)) stops it after the current video and joins
/// the thread.
pub struct MetadataWorker {
    control: Option<mpsc::Sender<()>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for MetadataWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetadataWorker")
            .field("running", &self.is_running())
            .finish()
    }
}

impl MetadataWorker {
    /// Starts the worker. Events arrive on the returned receiver; dropping
    /// the receiver is fine (events are then discarded).
    pub fn spawn(
        library: Library,
        prober: Arc<dyn MediaProber>,
        options: WorkerOptions,
    ) -> Result<(MetadataWorker, mpsc::Receiver<WorkerEvent>)> {
        let (event_tx, event_rx) = mpsc::channel();
        let (control_tx, control_rx) = mpsc::channel::<()>();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("fp-metadata".into())
            .spawn(move || {
                let send = |e: WorkerEvent| {
                    // A dropped receiver only means nobody listens.
                    let _ = event_tx.send(e);
                };
                loop {
                    let mut forward = |e: WorkerEvent| send(e);
                    if let Err(e) =
                        process_pending(&library, &*prober, &options, &thread_stop, &mut forward)
                    {
                        send(WorkerEvent::Error(e.to_string()));
                    }
                    if thread_stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let woke = match options.poll_interval {
                        Some(every) => match control_rx.recv_timeout(every) {
                            Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => true,
                            Err(mpsc::RecvTimeoutError::Disconnected) => false,
                        },
                        None => control_rx.recv().is_ok(),
                    };
                    if !woke || thread_stop.load(Ordering::Relaxed) {
                        break;
                    }
                    // Coalesce wake-ups that arrived meanwhile.
                    while control_rx.try_recv().is_ok() {}
                }
                send(WorkerEvent::Stopped);
            })
            .map_err(Error::Spawn)?;
        Ok((
            MetadataWorker {
                control: Some(control_tx),
                stop,
                handle: Some(handle),
            },
            event_rx,
        ))
    }

    /// Asks the worker to look for new pending videos.
    pub fn wake(&self) {
        if let Some(tx) = &self.control {
            // Fails only when the thread has exited, which `is_running` shows.
            let _ = tx.send(());
        }
    }

    /// Whether the thread is still alive.
    pub fn is_running(&self) -> bool {
        self.handle.as_ref().is_some_and(|h| !h.is_finished())
    }

    /// Stops after the current video and waits for the thread.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Disconnecting the control channel wakes a sleeping thread.
        self.control.take();
        if let Some(h) = self.handle.take() {
            // A panic in the worker has already been reported by the panic
            // hook; there is nothing more to do with it here.
            let _ = h.join();
        }
    }
}

impl Drop for MetadataWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::jpeg_size;
    use crate::library::MediaUpsert;
    use fp_core::Projection;
    use fp_core::format::{DetectedFormat, Evidence};
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;

    /// A prober that knows a fixed set of files and paints solid frames of
    /// the right (cropped, fitted) size.
    #[derive(Default)]
    struct FakeProber {
        infos: HashMap<String, ProbeInfo>,
        broken_thumbnails: HashSet<String>,
        calls: Mutex<Vec<(String, f64, ThumbnailCrop)>>,
    }

    impl FakeProber {
        fn with(mut self, loc: &str, w: u32, h: u32, dur: f64, hints: ContainerHints) -> Self {
            self.infos.insert(
                loc.into(),
                ProbeInfo {
                    duration: Some(dur),
                    width: w,
                    height: h,
                    video_codec: Some("hevc".into()),
                    hints,
                },
            );
            self
        }

        fn crops_for(&self, loc: &str) -> Vec<ThumbnailCrop> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c.0 == loc)
                .map(|c| c.2)
                .collect()
        }
    }

    impl MediaProber for FakeProber {
        fn probe(&self, location: &str) -> Result<ProbeInfo> {
            self.infos
                .get(location)
                .cloned()
                .ok_or_else(|| Error::Probe(format!("cannot open {location}")))
        }

        fn thumbnail(
            &self,
            location: &str,
            at: f64,
            max_w: u32,
            max_h: u32,
            crop: ThumbnailCrop,
        ) -> Result<RgbaImage> {
            self.calls.lock().unwrap().push((location.into(), at, crop));
            if self.broken_thumbnails.contains(location) {
                return Err(Error::Probe("decoder error".into()));
            }
            let info = self.probe(location)?;
            let (w, h) = match crop {
                ThumbnailCrop::LeftEye => (info.width / 2, info.height),
                ThumbnailCrop::TopEye => (info.width, info.height / 2),
                ThumbnailCrop::Full => (info.width, info.height),
            };
            let (w, h) = crate::image::fit_size(w, h, max_w, max_h);
            Ok(RgbaImage::filled(w, h, [200, 100, 50, 255]))
        }
    }

    fn opts(dir: &Path) -> WorkerOptions {
        WorkerOptions {
            strip_frames: 4,
            strip_frame_max_width: 64,
            strip_frame_max_height: 64,
            batch_size: 2,
            ..WorkerOptions::with_cache_dir(dir)
        }
    }

    fn run(
        lib: &Library,
        prober: &FakeProber,
        o: &WorkerOptions,
    ) -> (WorkerStats, Vec<WorkerEvent>) {
        let mut events = Vec::new();
        let stats = process_pending(lib, prober, o, &AtomicBool::new(false), &mut |e| {
            events.push(e)
        })
        .unwrap();
        (stats, events)
    }

    #[test]
    fn crop_follows_stereo_layout() {
        let f = |s| VideoFormat::new(Projection::EQUIRECT_180, s);
        assert_eq!(
            ThumbnailCrop::for_format(&f(StereoLayout::SideBySide)),
            ThumbnailCrop::LeftEye
        );
        assert_eq!(
            ThumbnailCrop::for_format(&f(StereoLayout::TopBottom)),
            ThumbnailCrop::TopEye
        );
        assert_eq!(
            ThumbnailCrop::for_format(&f(StereoLayout::Mono)),
            ThumbnailCrop::Full
        );
    }

    #[test]
    fn processes_pending_media() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        let lib = Library::open_in_memory().unwrap();
        let sbs = lib
            .upsert(&MediaUpsert::new("/v/a_LR.mp4", "local"))
            .unwrap()
            .id;
        let meta = lib
            .upsert(&MediaUpsert::new("/v/b.mp4", "local"))
            .unwrap()
            .id;
        let aspect = lib
            .upsert(&MediaUpsert::new("/v/c.mp4", "local"))
            .unwrap()
            .id;
        let broken = lib
            .upsert(&MediaUpsert::new("/v/broken.mp4", "local"))
            .unwrap()
            .id;
        let prober = FakeProber::default()
            .with("/v/a_LR.mp4", 3840, 1080, 100.0, ContainerHints::default())
            .with(
                "/v/b.mp4",
                4096,
                2048,
                60.0,
                ContainerHints {
                    projection: Some(Projection::EQUIRECT_360),
                    stereo: Some(StereoLayout::Mono),
                },
            )
            .with("/v/c.mp4", 5760, 2880, 30.0, ContainerHints::default());
        let o = opts(&cache);
        assert_eq!(lib.pending_metadata_count().unwrap(), 4);

        let (stats, events) = run(&lib, &prober, &o);
        assert_eq!(
            stats,
            WorkerStats {
                processed: 3,
                failed: 1
            }
        );
        assert_eq!(events.first(), Some(&WorkerEvent::Started { pending: 4 }));
        assert_eq!(
            events.last(),
            Some(&WorkerEvent::Idle {
                processed: 3,
                failed: 1
            })
        );
        let dones: Vec<usize> = events
            .iter()
            .filter_map(|e| match e {
                WorkerEvent::Processed { done, total, .. }
                | WorkerEvent::Failed { done, total, .. } => {
                    assert_eq!(*total, 4);
                    Some(*done)
                }
                _ => None,
            })
            .collect();
        assert_eq!(dones, vec![1, 2, 3, 4]);
        assert!(events.iter().any(|e| matches!(
            e,
            WorkerEvent::Failed { id, .. } if *id == broken
        )));

        // SBS from the name: left-eye crop, square-ish thumbnail.
        let a = lib.get(sbs).unwrap().unwrap();
        assert!(a.probed && a.probe_error.is_none());
        assert_eq!(
            (a.width, a.height, a.duration),
            (Some(3840), Some(1080), Some(100.0))
        );
        assert_eq!(a.video_codec.as_deref(), Some("hevc"));
        assert_eq!(a.detected.evidence, Evidence::FileName);
        assert!(
            prober
                .crops_for("/v/a_LR.mp4")
                .iter()
                .all(|c| *c == ThumbnailCrop::LeftEye)
        );
        let thumb = a.thumbnail.clone().unwrap();
        assert_eq!(thumb, cache.join(format!("{sbs}.jpg")));
        let jpg = std::fs::read(&thumb).unwrap();
        assert_eq!(jpeg_size(&jpg), Some((640, 360)));
        let strip = a.preview_strip.clone().unwrap();
        assert_eq!(strip.path, cache.join(format!("{sbs}_strip.jpg")));
        assert_eq!((strip.frames, strip.columns), (4, 4));
        assert_eq!((strip.cell_width, strip.cell_height), (64, 36));
        assert_eq!(strip.interval, 25.0);
        let sheet = std::fs::read(&strip.path).unwrap();
        assert_eq!(jpeg_size(&sheet), Some((256, 36)));
        // Thumbnail at 20 %, strip frames at the middle of each quarter.
        let times: Vec<f64> = prober
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.0 == "/v/a_LR.mp4")
            .map(|c| c.1)
            .collect();
        assert_eq!(times, vec![20.0, 12.5, 37.5, 62.5, 87.5]);

        // Container metadata wins over the name.
        let b = lib.get(meta).unwrap().unwrap();
        assert_eq!(b.detected.evidence, Evidence::Metadata);
        assert_eq!(b.detected.format.projection, Projection::EQUIRECT_360);
        assert!(
            prober
                .crops_for("/v/b.mp4")
                .iter()
                .all(|c| *c == ThumbnailCrop::Full)
        );

        // Aspect ratio: 2:1 at 5760 wide is SBS 180, so the filter finds it,
        // along with the unreadable file, which plays as the 180 SBS fallback.
        let c = lib.get(aspect).unwrap().unwrap();
        assert_eq!(c.detected.evidence, Evidence::AspectRatio);
        let found = lib
            .search(&crate::Query {
                projections: vec![crate::ProjectionKind::Equirect180],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            found.iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![broken, aspect]
        );

        // The failure is recorded and not retried...
        let x = lib.get(broken).unwrap().unwrap();
        assert!(x.probe_error.unwrap().contains("cannot open"));
        assert_eq!(lib.pending_metadata_count().unwrap(), 0);
        let (stats, _) = run(&lib, &prober, &o);
        assert_eq!(stats, WorkerStats::default());
        // ...until asked.
        assert_eq!(lib.retry_failed_probes().unwrap(), 1);
        assert_eq!(lib.pending_metadata_count().unwrap(), 1);
    }

    #[test]
    fn keeps_stronger_detection_and_honours_user_format() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open_in_memory().unwrap();
        let mut u = MediaUpsert::new("http://srv/feed1.mp4", "deovr");
        u.detected = DetectedFormat {
            format: VideoFormat::new(Projection::fisheye(200.0), StereoLayout::SideBySide),
            evidence: Evidence::Metadata,
        };
        let declared = lib.upsert(&u).unwrap().id;
        let user = lib
            .upsert(&MediaUpsert::new("/v/plain.mp4", "local"))
            .unwrap()
            .id;
        lib.set_user_format(
            user,
            Some(VideoFormat::new(
                Projection::EQUIRECT_180,
                StereoLayout::TopBottom,
            )),
        )
        .unwrap();
        let prober = FakeProber::default()
            .with(
                "http://srv/feed1.mp4",
                1920,
                1080,
                10.0,
                ContainerHints::default(),
            )
            .with("/v/plain.mp4", 4000, 4000, 10.0, ContainerHints::default());
        run(&lib, &prober, &opts(tmp.path()));

        assert_eq!(lib.get(declared).unwrap().unwrap().detected, u.detected);
        assert!(
            prober
                .crops_for("http://srv/feed1.mp4")
                .iter()
                .all(|c| *c == ThumbnailCrop::LeftEye)
        );
        assert!(
            prober
                .crops_for("/v/plain.mp4")
                .iter()
                .all(|c| *c == ThumbnailCrop::TopEye)
        );
        let r = lib.get(user).unwrap().unwrap();
        assert_eq!(r.effective_format().evidence, Evidence::User);
        assert_eq!(r.detected.evidence, Evidence::AspectRatio);
    }

    #[test]
    fn thumbnail_failure_keeps_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open_in_memory().unwrap();
        let id = lib
            .upsert(&MediaUpsert::new("/v/x.mp4", "local"))
            .unwrap()
            .id;
        let mut prober =
            FakeProber::default().with("/v/x.mp4", 1920, 1080, 10.0, ContainerHints::default());
        prober.broken_thumbnails.insert("/v/x.mp4".into());
        let (stats, events) = run(&lib, &prober, &opts(tmp.path()));
        assert_eq!(stats.processed, 1);
        assert!(events.iter().any(|e| matches!(
            e,
            WorkerEvent::Processed { thumbnail: false, preview_strip: false, warning: Some(w), .. }
                if w.contains("thumbnail")
        )));
        let r = lib.get(id).unwrap().unwrap();
        assert_eq!(r.duration, Some(10.0));
        assert!(r.thumbnail.is_none());
        assert!(r.probe_error.is_some());
        assert_eq!(lib.pending_metadata_count().unwrap(), 0);
    }

    #[test]
    fn rescan_of_changed_file_requeues_it() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open_in_memory().unwrap();
        let mut u = MediaUpsert::new("/v/x.mp4", "local");
        u.size = Some(1);
        lib.upsert(&u).unwrap();
        let prober =
            FakeProber::default().with("/v/x.mp4", 1920, 1080, 10.0, ContainerHints::default());
        run(&lib, &prober, &opts(tmp.path()));
        assert_eq!(lib.pending_metadata_count().unwrap(), 0);
        u.size = Some(2);
        lib.upsert(&u).unwrap();
        assert_eq!(lib.pending_metadata_count().unwrap(), 1);
    }

    fn wait_for_idle(rx: &mpsc::Receiver<WorkerEvent>) -> Vec<WorkerEvent> {
        let mut seen = Vec::new();
        loop {
            let e = rx
                .recv_timeout(Duration::from_secs(10))
                .expect("worker went quiet");
            let idle = matches!(e, WorkerEvent::Idle { .. });
            seen.push(e);
            if idle {
                return seen;
            }
        }
    }

    #[test]
    fn background_worker_runs_wakes_and_stops() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open_in_memory().unwrap();
        let first = lib
            .upsert(&MediaUpsert::new("/v/1.mp4", "local"))
            .unwrap()
            .id;
        let prober = Arc::new(
            FakeProber::default()
                .with("/v/1.mp4", 1920, 1080, 10.0, ContainerHints::default())
                .with(
                    "/v/2_180_LR.mp4",
                    4000,
                    2000,
                    10.0,
                    ContainerHints::default(),
                ),
        );
        let (worker, rx) =
            MetadataWorker::spawn(lib.clone(), prober.clone(), opts(tmp.path())).unwrap();
        let events = wait_for_idle(&rx);
        assert!(events.iter().any(|e| matches!(
            e,
            WorkerEvent::Processed { id, thumbnail: true, preview_strip: true, .. } if *id == first
        )));
        assert!(worker.is_running());

        let second = lib
            .upsert(&MediaUpsert::new("/v/2_180_LR.mp4", "local"))
            .unwrap()
            .id;
        worker.wake();
        let events = wait_for_idle(&rx);
        assert!(events.iter().any(|e| matches!(
            e,
            WorkerEvent::Processed { id, .. } if *id == second
        )));
        assert!(lib.get(second).unwrap().unwrap().thumbnail.is_some());

        worker.stop();
        let rest: Vec<WorkerEvent> = rx.iter().collect();
        assert_eq!(rest.last(), Some(&WorkerEvent::Stopped));
    }

    #[test]
    fn worker_reports_cache_errors_and_drop_stops_it() {
        let tmp = tempfile::tempdir().unwrap();
        // A file where the cache folder should be.
        let blocker = tmp.path().join("cache");
        std::fs::write(&blocker, b"x").unwrap();
        let lib = Library::open_in_memory().unwrap();
        lib.upsert(&MediaUpsert::new("/v/1.mp4", "local")).unwrap();
        let prober = Arc::new(FakeProber::default());
        let (worker, rx) = MetadataWorker::spawn(lib, prober, opts(&blocker)).unwrap();
        let mut got_error = false;
        while let Ok(e) = rx.recv_timeout(Duration::from_secs(10)) {
            if matches!(e, WorkerEvent::Error(_)) {
                got_error = true;
                break;
            }
        }
        assert!(got_error);
        drop(worker);
        assert_eq!(rx.iter().last(), Some(WorkerEvent::Stopped));
    }
}
