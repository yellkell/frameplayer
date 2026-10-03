//! Thumbnail textures: decoded on worker threads, uploaded into one egui
//! context, kept in a small LRU.

use crate::services::Opener;
use crossbeam_channel::{Receiver, Sender, unbounded};
use egui::{ColorImage, TextureHandle, TextureOptions};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

const MAX_TEXTURES: usize = 240;
const MAX_WIDTH: u32 = 480;

pub struct Thumbs {
    textures: HashMap<String, TextureHandle>,
    order: VecDeque<String>,
    pending: HashSet<String>,
    failed: HashSet<String>,
    req: Sender<String>,
    done: Receiver<(String, Option<ColorImage>)>,
}

/// Decodes an image file into an egui image no wider than `max_width`.
pub fn decode(bytes: &[u8], max_width: u32) -> Option<ColorImage> {
    let img = image::load_from_memory(bytes).ok()?;
    let img = if img.width() > max_width {
        let h = (img.height() as u64 * max_width as u64 / img.width() as u64).max(1) as u32;
        img.resize_exact(max_width, h, image::imageops::FilterType::Triangle)
    } else {
        img
    };
    let rgba = img.to_rgba8();
    Some(ColorImage::from_rgba_unmultiplied(
        [rgba.width() as usize, rgba.height() as usize],
        rgba.as_raw(),
    ))
}

impl Thumbs {
    /// `key`s are local paths or URLs.
    pub fn new(opener: Arc<Opener>) -> Thumbs {
        let (req, rx) = unbounded::<String>();
        let (tx, done) = unbounded();
        for i in 0..3 {
            let rx = rx.clone();
            let tx = tx.clone();
            let opener = opener.clone();
            let _ = std::thread::Builder::new()
                .name(format!("fp-thumbs-{i}"))
                .spawn(move || {
                    for key in rx.iter() {
                        let img = opener
                            .fetch(&key, 16 << 20)
                            .ok()
                            .and_then(|b| decode(&b, MAX_WIDTH));
                        if tx.send((key, img)).is_err() {
                            break;
                        }
                    }
                });
        }
        Thumbs {
            textures: HashMap::new(),
            order: VecDeque::new(),
            pending: HashSet::new(),
            failed: HashSet::new(),
            req,
            done,
        }
    }

    /// Uploads finished decodes; true when something new is available.
    pub fn poll(&mut self, ctx: &egui::Context) -> bool {
        let mut any = false;
        for (key, img) in self.done.try_iter() {
            self.pending.remove(&key);
            match img {
                Some(img) => {
                    let tex = ctx.load_texture(&key, img, TextureOptions::LINEAR);
                    self.textures.insert(key.clone(), tex);
                    self.order.push_back(key);
                    any = true;
                }
                None => {
                    self.failed.insert(key);
                }
            }
        }
        while self.order.len() > MAX_TEXTURES {
            if let Some(old) = self.order.pop_front() {
                self.textures.remove(&old);
            }
        }
        any
    }

    /// The texture for `key`, requesting it when missing.
    pub fn get(&mut self, key: &str) -> Option<TextureHandle> {
        if let Some(t) = self.textures.get(key) {
            // Refresh LRU position.
            if let Some(pos) = self.order.iter().position(|k| k == key)
                && pos + 32 < self.order.len()
            {
                let k = self.order.remove(pos).unwrap_or_default();
                self.order.push_back(k);
            }
            return Some(t.clone());
        }
        if !self.pending.contains(key) && !self.failed.contains(key) {
            self.pending.insert(key.to_string());
            let _ = self.req.send(key.to_string());
        }
        None
    }

    /// Forget failures (e.g. after the library re-thumbnailed).
    pub fn retry_failed(&mut self) {
        self.failed.clear();
    }
}

/// Paints `tex` covering `rect` (cropping to keep its aspect).
pub fn paint_cover(painter: &egui::Painter, rect: egui::Rect, tex: &TextureHandle) {
    painter.image(tex.id(), rect, cover_uv(rect, tex), egui::Color32::WHITE);
}

/// [`paint_cover`] with rounded corners.
pub fn paint_cover_rounded(
    painter: &egui::Painter,
    rect: egui::Rect,
    tex: &TextureHandle,
    radius: u8,
) {
    painter.add(
        egui::epaint::RectShape::filled(rect, radius, egui::Color32::WHITE)
            .with_texture(tex.id(), cover_uv(rect, tex)),
    );
}

/// The part of the texture that fills `rect`, cropped to its aspect.
fn cover_uv(rect: egui::Rect, tex: &TextureHandle) -> egui::Rect {
    let [w, h] = tex.size();
    let r_aspect = rect.width() / rect.height().max(1.0);
    let t_aspect = w as f32 / (h as f32).max(1.0);
    let uv = if t_aspect > r_aspect {
        let f = r_aspect / t_aspect;
        egui::Rect::from_min_max(
            egui::pos2(0.5 - f / 2.0, 0.0),
            egui::pos2(0.5 + f / 2.0, 1.0),
        )
    } else {
        let f = t_aspect / r_aspect;
        egui::Rect::from_min_max(
            egui::pos2(0.0, 0.5 - f / 2.0),
            egui::pos2(1.0, 0.5 + f / 2.0),
        )
    };
    uv
}
