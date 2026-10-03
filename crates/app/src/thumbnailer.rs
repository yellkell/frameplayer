//! Library thumbnailer on top of fp-video's single-frame decoder, plus the
//! image helpers the app uses to show thumbnails in the headset.
//!
//! Per item: probe the container (duration, codec, signalled projection),
//! decode a poster frame (left eye only for stereo), write it as JPEG, then
//! build a preview sprite sheet for timeline scrubbing. Thumbnail decoding
//! needs a software decoder (`dav1d` / `ffmpeg` features of fp-video); in a
//! build without one the probe still succeeds and the item simply has no
//! poster.

use crate::media_input::SourceInput;
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use fp_core::{MediaTime, StereoMode};
use fp_library::{SpriteInfo, ThumbnailJob, ThumbnailOutput, Thumbnailer};
use fp_sources::{BlockingReader, RandomAccess, ReadAheadConfig};
use fp_video::decode::convert::RgbaImage;
use fp_video::{decode_frame_rgba, MediaInput, ThumbnailOptions, VideoError};
use std::path::Path;
use std::sync::Arc;
use tokio::runtime::Handle;

/// JPEG quality for posters and sprite sheets.
pub const JPEG_QUALITY: u8 = 82;
/// Largest thumbnail kept in GPU memory for the library grid.
pub const DISPLAY_MAX_WIDTH: u32 = 512;

/// [`Thumbnailer`] implementation used by the indexer.
pub struct VideoThumbnailer {
    handle: Handle,
}

impl VideoThumbnailer {
    pub fn new(handle: Handle) -> Self {
        VideoThumbnailer { handle }
    }
}

#[async_trait]
impl Thumbnailer for VideoThumbnailer {
    async fn generate(
        &self,
        job: ThumbnailJob,
    ) -> std::result::Result<ThumbnailOutput, fp_library::indexer::ThumbnailError> {
        let ra: Arc<dyn RandomAccess> = Arc::from(job.source.open(&job.uri).await?);
        let handle = self.handle.clone();
        // Decoding blocks (and BlockingReader must not run on a worker).
        let out = tokio::task::spawn_blocking(move || generate_blocking(ra, handle, &job))
            .await
            .map_err(|e| anyhow!("thumbnail task: {e}"))??;
        Ok(out)
    }
}

/// Poster time: 10 % into the video, at most 5 minutes in.
pub fn poster_time(duration: Option<MediaTime>) -> MediaTime {
    match duration {
        Some(d) if d > MediaTime::ZERO => MediaTime((d.0 / 10).min(300_000_000)),
        _ => MediaTime::from_secs_f64(5.0),
    }
}

/// Seconds between sprite tiles so `tiles` cover the whole duration.
pub fn sprite_interval(duration: MediaTime, tiles: u32) -> f64 {
    duration.as_secs_f64() / tiles.max(1) as f64
}

fn generate_blocking(
    ra: Arc<dyn RandomAccess>,
    handle: Handle,
    job: &ThumbnailJob,
) -> Result<ThumbnailOutput> {
    let input = || -> Box<dyn MediaInput> {
        Box::new(SourceInput(BlockingReader::new(
            ra.clone(),
            handle.clone(),
            ReadAheadConfig::default(),
        )))
    };
    let demux = fp_video::open_demuxer(input()).map_err(|e| anyhow!("probe: {e}"))?;
    let info = demux.media_info().clone();
    drop(demux);
    let mut out = ThumbnailOutput {
        media_info: Some(info.clone()),
        ..Default::default()
    };
    let stereo = info
        .primary_video()
        .and_then(|v| v.signalled_stereo)
        .unwrap_or(job.stereo);
    let duration = info.duration.or(job.duration_hint);
    let opts = ThumbnailOptions {
        left_eye_only: (stereo != StereoMode::Mono).then_some(stereo),
        ..Default::default()
    };
    let poster = match decode_frame_rgba(input(), poster_time(duration), &opts) {
        Ok(img) => img,
        Err(VideoError::NoSoftwareDecoder(codec)) => {
            tracing::debug!(
                "no software decoder for {codec}; {} gets no poster",
                job.uri
            );
            return Ok(out);
        }
        Err(e) => return Err(anyhow!("poster: {e}")),
    };
    write_jpeg(&job.thumbnail_path, &poster)?;
    out.thumbnail_written = true;

    if let Some(d) = duration.filter(|d| *d > MediaTime::ZERO) {
        let spec = job.sprite;
        let tiles = spec.columns * spec.rows;
        let tile_w = spec.tile_width.max(16);
        let aspect = poster.width as f32 / poster.height.max(1) as f32;
        let tile_h = ((tile_w as f32 / aspect).round() as u32).max(8);
        let interval = sprite_interval(d, tiles);
        let mut sheet = RgbaImage {
            width: tile_w * spec.columns,
            height: tile_h * spec.rows,
            data: vec![0; (tile_w * spec.columns * tile_h * spec.rows * 4) as usize],
        };
        let tile_opts = ThumbnailOptions {
            max_width: tile_w,
            max_height: tile_h,
            ..opts.clone()
        };
        for i in 0..tiles {
            let t = MediaTime::from_secs_f64(interval * (i as f64 + 0.5));
            match decode_frame_rgba(input(), t, &tile_opts) {
                Ok(img) => blit(
                    &mut sheet,
                    &img.resize_box(tile_w, tile_h),
                    (i % spec.columns) * tile_w,
                    (i / spec.columns) * tile_h,
                ),
                Err(e) => {
                    tracing::debug!("sprite tile {i} of {}: {e}", job.uri);
                    break;
                }
            }
        }
        write_jpeg(&job.sprite_path, &sheet)?;
        out.sprite = Some(SpriteInfo {
            columns: spec.columns,
            rows: spec.rows,
            tile_width: tile_w,
            tile_height: tile_h,
            interval_secs: interval,
        });
    }
    Ok(out)
}

/// Copy `src` into `dst` at `(x, y)` (clipped).
pub fn blit(dst: &mut RgbaImage, src: &RgbaImage, x: u32, y: u32) {
    for row in 0..src.height.min(dst.height.saturating_sub(y)) {
        let w = src.width.min(dst.width.saturating_sub(x)) as usize * 4;
        let s = (row * src.width) as usize * 4;
        let d = (((y + row) * dst.width) + x) as usize * 4;
        dst.data[d..d + w].copy_from_slice(&src.data[s..s + w]);
    }
}

/// Encode RGBA as JPEG bytes.
pub fn encode_jpeg(img: &RgbaImage) -> Result<Vec<u8>> {
    if img.width > u16::MAX as u32 || img.height > u16::MAX as u32 {
        bail!("image too large for JPEG");
    }
    let mut buf = Vec::new();
    jpeg_encoder::Encoder::new(&mut buf, JPEG_QUALITY)
        .encode(
            &img.data,
            img.width as u16,
            img.height as u16,
            jpeg_encoder::ColorType::Rgba,
        )
        .map_err(|e| anyhow!("jpeg: {e}"))?;
    Ok(buf)
}

/// Write a JPEG atomically (temp file + rename).
pub fn write_jpeg(path: &Path, img: &RgbaImage) -> Result<()> {
    let bytes = encode_jpeg(img)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("jpg.tmp");
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Decode a JPEG or PNG into tightly packed RGBA.
pub fn decode_image(bytes: &[u8]) -> Result<RgbaImage> {
    if bytes.starts_with(b"\x89PNG") {
        let mut dec = png::Decoder::new(std::io::Cursor::new(bytes));
        dec.set_transformations(png::Transformations::normalize_to_color8());
        let mut reader = dec.read_info().map_err(|e| anyhow!("png: {e}"))?;
        let mut buf = vec![0; reader.output_buffer_size()];
        let info = reader
            .next_frame(&mut buf)
            .map_err(|e| anyhow!("png: {e}"))?;
        let px = &buf[..info.buffer_size()];
        let data = match info.color_type {
            png::ColorType::Rgba => px.to_vec(),
            png::ColorType::Rgb => px
                .chunks_exact(3)
                .flat_map(|c| [c[0], c[1], c[2], 255])
                .collect(),
            png::ColorType::Grayscale => px.iter().flat_map(|&g| [g, g, g, 255]).collect(),
            png::ColorType::GrayscaleAlpha => px
                .chunks_exact(2)
                .flat_map(|c| [c[0], c[0], c[0], c[1]])
                .collect(),
            png::ColorType::Indexed => bail!("png: unexpanded palette"),
        };
        return Ok(RgbaImage {
            width: info.width,
            height: info.height,
            data,
        });
    }
    let mut dec = jpeg_decoder::Decoder::new(std::io::Cursor::new(bytes));
    let px = dec.decode().map_err(|e| anyhow!("jpeg: {e}"))?;
    let info = dec.info().ok_or_else(|| anyhow!("jpeg: no header"))?;
    let data = match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => px
            .chunks_exact(3)
            .flat_map(|c| [c[0], c[1], c[2], 255])
            .collect(),
        jpeg_decoder::PixelFormat::L8 => px.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        jpeg_decoder::PixelFormat::L16 => px
            .chunks_exact(2)
            .flat_map(|c| [c[0], c[0], c[0], 255])
            .collect(),
        jpeg_decoder::PixelFormat::CMYK32 => px
            .chunks_exact(4)
            .flat_map(|c| {
                let k = 255 - c[3] as u32;
                let f = |v: u8| ((255 - v as u32) * k / 255) as u8;
                [f(c[0]), f(c[1]), f(c[2]), 255]
            })
            .collect(),
    };
    Ok(RgbaImage {
        width: info.width as u32,
        height: info.height as u32,
        data,
    })
}

/// Decode and shrink for display in the library grid.
pub fn display_thumbnail(bytes: &[u8]) -> Result<RgbaImage> {
    let img = decode_image(bytes)?;
    Ok(img.fit_within(DISPLAY_MAX_WIDTH, DISPLAY_MAX_WIDTH))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(w: u32, h: u32) -> RgbaImage {
        let mut data = Vec::new();
        for y in 0..h {
            for x in 0..w {
                data.extend_from_slice(&[(x * 255 / w) as u8, (y * 255 / h) as u8, 128, 255]);
            }
        }
        RgbaImage {
            width: w,
            height: h,
            data,
        }
    }

    #[test]
    fn jpeg_roundtrip() {
        let img = gradient(64, 32);
        let bytes = encode_jpeg(&img).unwrap();
        assert!(bytes.starts_with(&[0xff, 0xd8]));
        let back = decode_image(&bytes).unwrap();
        assert_eq!((back.width, back.height), (64, 32));
        let p = back.pixel(32, 16);
        assert!(
            (p[0] as i32 - 127).abs() < 20 && (p[2] as i32 - 128).abs() < 20,
            "{p:?}"
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t/x.jpg");
        write_jpeg(&path, &img).unwrap();
        assert!(path.exists());
        let small = display_thumbnail(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(small.width, 64, "never upscales");
    }

    #[test]
    fn png_decodes() {
        let mut buf = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut buf, 2, 1);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            let mut w = enc.write_header().unwrap();
            w.write_image_data(&[255, 0, 0, 0, 255, 0]).unwrap();
        }
        let img = decode_image(&buf).unwrap();
        assert_eq!(img.data, vec![255, 0, 0, 255, 0, 255, 0, 255]);
    }

    #[test]
    fn blit_places_tiles() {
        let mut sheet = RgbaImage {
            width: 4,
            height: 2,
            data: vec![0; 32],
        };
        let tile = RgbaImage {
            width: 2,
            height: 2,
            data: vec![9; 16],
        };
        blit(&mut sheet, &tile, 2, 0);
        assert_eq!(sheet.pixel(1, 1), [0; 4]);
        assert_eq!(sheet.pixel(3, 1), [9; 4]);
        blit(&mut sheet, &tile, 3, 1); // clipped, must not panic
    }

    #[test]
    fn poster_and_interval() {
        assert_eq!(
            poster_time(Some(MediaTime::from_secs_f64(100.0))),
            MediaTime::from_secs_f64(10.0)
        );
        assert_eq!(
            poster_time(Some(MediaTime::from_secs_f64(7200.0))),
            MediaTime::from_secs_f64(300.0)
        );
        assert_eq!(poster_time(None), MediaTime::from_secs_f64(5.0));
        assert_eq!(sprite_interval(MediaTime::from_secs_f64(100.0), 100), 1.0);
    }

    #[test]
    fn thumbnailer_probes_without_software_decoder() {
        // A source whose file is not media: the probe fails cleanly.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.mp4"), b"not a video").unwrap();
        let source: Arc<dyn fp_sources::Source> =
            Arc::new(fp_sources::local::LocalSource::new(dir.path()));
        let uri = fp_sources::local::path_to_uri(&dir.path().join("a.mp4"));
        let t = VideoThumbnailer::new(rt.handle().clone());
        let job = ThumbnailJob {
            item_id: 1,
            uri,
            source,
            thumbnail_path: dir.path().join("t.jpg"),
            sprite_path: dir.path().join("s.jpg"),
            sprite: Default::default(),
            duration_hint: None,
            projection: Default::default(),
            stereo: StereoMode::Mono,
        };
        assert!(rt.block_on(t.generate(job)).is_err());
    }
}
