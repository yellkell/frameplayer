//! Font loading, glyph atlas and light-weight text layout.
//!
//! Faces are rasterized on demand with `fontdue` into a single R8 coverage
//! atlas (shelf packed). Layout is "shaping-light": one glyph per `char`,
//! pair kerning, greedy line wrapping at spaces (and between CJK ideographs),
//! ellipsis truncation and per-line alignment. Missing glyphs fall back
//! through additional faces (the CJK slot) before using the primary face's
//! `.notdef`.

use crate::geom::{Rect, Vec2};
use fp_core::draw::AtlasImage;
use std::collections::HashMap;
use std::ops::Range;

/// Noto Sans Regular, bundled via the `ttf-noto-sans` crate (Apache-2.0
/// licensed Noto release).
pub static DEFAULT_FONT: &[u8] = ttf_noto_sans::REGULAR;

#[derive(Debug, thiserror::Error)]
pub enum FontError {
    #[error("failed to parse font: {0}")]
    Parse(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Align {
    #[default]
    Left,
    Center,
    Right,
}

/// How to lay out a run of text.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextParams {
    /// Font size in pixels (rounded to whole pixels for caching).
    pub size: f32,
    /// Box width used for wrapping, truncation and alignment.
    pub max_width: Option<f32>,
    pub wrap: bool,
    pub max_lines: Option<usize>,
    /// Replace overflowing text with "…" (single-line, or the last allowed line).
    pub ellipsis: bool,
    pub align: Align,
    /// Multiplier on the font's natural line height.
    pub line_spacing: f32,
}

impl TextParams {
    pub fn new(size: f32) -> TextParams {
        TextParams {
            size,
            max_width: None,
            wrap: false,
            max_lines: None,
            ellipsis: false,
            align: Align::Left,
            line_spacing: 1.0,
        }
    }
    pub fn width(mut self, w: f32) -> Self {
        self.max_width = Some(w);
        self
    }
    pub fn wrap(mut self) -> Self {
        self.wrap = true;
        self
    }
    pub fn ellipsis(mut self) -> Self {
        self.ellipsis = true;
        self
    }
    pub fn lines(mut self, n: usize) -> Self {
        self.max_lines = Some(n);
        self
    }
    pub fn align(mut self, a: Align) -> Self {
        self.align = a;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LineMetrics {
    pub ascent: f32,
    /// Positive distance below the baseline.
    pub descent: f32,
    pub line_height: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PositionedGlyph {
    pub face: u16,
    pub glyph: u16,
    /// Pen position on the baseline, relative to the layout's top-left.
    pub pos: Vec2,
    pub px: u16,
    /// Byte offset of the source char (`usize::MAX` for inserted ellipsis glyphs).
    pub byte: usize,
    pub advance: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TextLine {
    pub glyphs: Range<usize>,
    /// Source bytes covered by this line.
    pub bytes: Range<usize>,
    pub width: f32,
    pub x: f32,
    pub baseline: f32,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TextLayout {
    pub glyphs: Vec<PositionedGlyph>,
    pub lines: Vec<TextLine>,
    pub size: Vec2,
    /// Text was cut (ellipsis applied or lines dropped).
    pub truncated: bool,
    pub line_height: f32,
}

impl TextLayout {
    /// X position of the caret before byte `byte` on a single-line layout.
    pub fn caret_x(&self, byte: usize) -> f32 {
        let Some(line) = self.lines.first() else {
            return 0.0;
        };
        let mut x = line.x;
        for g in &self.glyphs[line.glyphs.clone()] {
            if g.byte >= byte {
                return g.pos.x;
            }
            x = g.pos.x + g.advance;
        }
        x
    }

    /// Byte offset of the caret position nearest to `x` (single-line).
    pub fn hit_byte(&self, x: f32, text_len: usize) -> usize {
        let Some(line) = self.lines.first() else {
            return 0;
        };
        for g in &self.glyphs[line.glyphs.clone()] {
            if g.byte != usize::MAX && x < g.pos.x + g.advance * 0.5 {
                return g.byte;
            }
        }
        text_len
    }
}

/// Atlas entry for one rasterized glyph.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GlyphEntry {
    /// Normalized `[u0, v0, u1, v1]`.
    pub uv: [f32; 4],
    /// Offset from the baseline pen position to the bitmap's top-left.
    pub offset: Vec2,
    pub size: Vec2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct GlyphKey {
    face: u16,
    glyph: u16,
    px: u16,
}

/// Shelf-packed R8 atlas. When it fills up, further glyphs are dropped for the
/// rest of the frame and the atlas is cleared at the next [`Fonts::begin_frame`].
#[derive(Debug)]
pub struct GlyphAtlas {
    image: AtlasImage,
    cursor_x: u32,
    shelf_y: u32,
    shelf_h: u32,
    full: bool,
}

impl GlyphAtlas {
    pub fn new(size: u32) -> GlyphAtlas {
        GlyphAtlas {
            image: AtlasImage {
                version: 1,
                width: size,
                height: size,
                pixels: vec![0; (size * size) as usize],
            },
            cursor_x: 0,
            shelf_y: 0,
            shelf_h: 0,
            full: false,
        }
    }

    /// Reserves a `w`×`h` cell (plus 1 px gutter) and returns its origin.
    pub fn allocate(&mut self, w: u32, h: u32) -> Option<(u32, u32)> {
        let (aw, ah) = (self.image.width, self.image.height);
        let (cw, ch) = (w + 1, h + 1);
        if cw > aw {
            return None;
        }
        if self.cursor_x + cw > aw {
            self.shelf_y += self.shelf_h;
            self.cursor_x = 0;
            self.shelf_h = 0;
        }
        if self.shelf_y + ch > ah {
            self.full = true;
            return None;
        }
        let pos = (self.cursor_x, self.shelf_y);
        self.cursor_x += cw;
        self.shelf_h = self.shelf_h.max(ch);
        Some(pos)
    }

    fn blit(&mut self, x: u32, y: u32, w: u32, h: u32, src: &[u8]) {
        let aw = self.image.width as usize;
        for row in 0..h as usize {
            let d = (y as usize + row) * aw + x as usize;
            self.image.pixels[d..d + w as usize]
                .copy_from_slice(&src[row * w as usize..(row + 1) * w as usize]);
        }
        self.image.version += 1;
    }

    pub fn clear(&mut self) {
        self.image.pixels.fill(0);
        self.image.version += 1;
        self.cursor_x = 0;
        self.shelf_y = 0;
        self.shelf_h = 0;
        self.full = false;
    }

    pub fn image(&self) -> &AtlasImage {
        &self.image
    }

    pub fn is_full(&self) -> bool {
        self.full
    }
}

/// Font faces plus glyph cache.
pub struct Fonts {
    faces: Vec<fontdue::Font>,
    atlas: GlyphAtlas,
    cache: HashMap<GlyphKey, Option<GlyphEntry>>,
    resolve_cache: HashMap<char, (u16, u16)>,
}

impl std::fmt::Debug for Fonts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fonts")
            .field("faces", &self.faces.len())
            .field("cached_glyphs", &self.cache.len())
            .finish()
    }
}

const DEFAULT_ATLAS_SIZE: u32 = 2048;

fn parse(bytes: &[u8]) -> Result<fontdue::Font, FontError> {
    fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default()).map_err(FontError::Parse)
}

impl Fonts {
    /// Primary face from TTF/OTF bytes.
    pub fn new(primary: &[u8]) -> Result<Fonts, FontError> {
        Ok(Fonts::from_face(parse(primary)?, DEFAULT_ATLAS_SIZE))
    }

    /// Embedded Noto Sans as the primary face.
    pub fn with_default_font() -> Fonts {
        Fonts::from_face(
            parse(DEFAULT_FONT).expect("embedded font parses"),
            DEFAULT_ATLAS_SIZE,
        )
    }

    fn from_face(face: fontdue::Font, atlas: u32) -> Fonts {
        Fonts {
            faces: vec![face],
            atlas: GlyphAtlas::new(atlas),
            cache: HashMap::new(),
            resolve_cache: HashMap::new(),
        }
    }

    /// Use an atlas of `size`² pixels (clears cached glyphs).
    pub fn set_atlas_size(&mut self, size: u32) {
        self.atlas = GlyphAtlas::new(size.max(64));
        self.cache.clear();
    }

    /// Adds a fallback face (e.g. a CJK font) consulted for chars the
    /// primary face lacks. Faces are tried in insertion order.
    pub fn add_fallback(&mut self, bytes: &[u8]) -> Result<(), FontError> {
        self.faces.push(parse(bytes)?);
        self.resolve_cache.clear();
        Ok(())
    }

    pub fn face_count(&self) -> usize {
        self.faces.len()
    }

    /// Clears the atlas if it overflowed last frame.
    pub fn begin_frame(&mut self) {
        if self.atlas.is_full() {
            self.atlas.clear();
            self.cache.clear();
        }
    }

    pub fn atlas(&self) -> &AtlasImage {
        self.atlas.image()
    }

    pub fn line_metrics(&self, size: f32) -> LineMetrics {
        let px = size.round().max(1.0);
        match self.faces[0].horizontal_line_metrics(px) {
            Some(m) => LineMetrics {
                ascent: m.ascent,
                descent: -m.descent,
                line_height: m.new_line_size,
            },
            None => LineMetrics {
                ascent: px * 0.8,
                descent: px * 0.2,
                line_height: px * 1.2,
            },
        }
    }

    /// `(face, glyph index)` for `ch`, walking the fallback chain.
    pub fn resolve(&mut self, ch: char) -> (u16, u16) {
        if let Some(&r) = self.resolve_cache.get(&ch) {
            return r;
        }
        let r = self
            .faces
            .iter()
            .enumerate()
            .find_map(|(i, f)| {
                let g = f.lookup_glyph_index(ch);
                (g != 0).then_some((i as u16, g))
            })
            .unwrap_or((0, 0));
        self.resolve_cache.insert(ch, r);
        r
    }

    pub fn has_glyph(&mut self, ch: char) -> bool {
        let (f, g) = self.resolve(ch);
        g != 0 || (f == 0 && ch.is_whitespace())
    }

    fn advance(&self, face: u16, glyph: u16, px: f32) -> f32 {
        self.faces[face as usize]
            .metrics_indexed(glyph, px)
            .advance_width
    }

    fn kern(&self, face: u16, a: u16, b: u16, px: f32) -> f32 {
        self.faces[face as usize]
            .horizontal_kern_indexed(a, b, px)
            .unwrap_or(0.0)
    }

    /// Width of a single line of text (no wrapping).
    pub fn measure(&mut self, text: &str, size: f32) -> f32 {
        self.layout(text, TextParams::new(size)).size.x
    }

    /// Rasterizes (if needed) and returns the atlas entry for a glyph.
    /// `None` for empty glyphs (spaces) or when the atlas is full.
    pub fn glyph(&mut self, face: u16, glyph: u16, px: u16) -> Option<GlyphEntry> {
        let key = GlyphKey { face, glyph, px };
        if let Some(e) = self.cache.get(&key) {
            return *e;
        }
        let (m, bitmap) = self.faces[face as usize].rasterize_indexed(glyph, px as f32);
        let entry = if m.width == 0 || m.height == 0 {
            Some(None)
        } else {
            self.atlas
                .allocate(m.width as u32, m.height as u32)
                .map(|(x, y)| {
                    self.atlas
                        .blit(x, y, m.width as u32, m.height as u32, &bitmap);
                    let (aw, ah) = (
                        self.atlas.image.width as f32,
                        self.atlas.image.height as f32,
                    );
                    Some(GlyphEntry {
                        uv: [
                            x as f32 / aw,
                            y as f32 / ah,
                            (x + m.width as u32) as f32 / aw,
                            (y + m.height as u32) as f32 / ah,
                        ],
                        offset: Vec2::new(m.xmin as f32, -(m.ymin as f32 + m.height as f32)),
                        size: Vec2::new(m.width as f32, m.height as f32),
                    })
                })
        };
        match entry {
            Some(e) => {
                self.cache.insert(key, e);
                e
            }
            // Atlas full: don't cache, retry after the reset.
            None => None,
        }
    }

    /// Lays out `text`; see [`TextParams`].
    pub fn layout(&mut self, text: &str, p: TextParams) -> TextLayout {
        let px = p.size.round().max(1.0);
        let pxu = px as u16;
        let lm = self.line_metrics(px);
        let line_h = lm.line_height * p.line_spacing;
        let max_w = p.max_width.unwrap_or(f32::INFINITY).max(0.0);

        // Shape every char once.
        let mut items: Vec<Item> = Vec::with_capacity(text.len());
        for (byte, ch) in text.char_indices() {
            let (face, glyph) = if ch == '\n' { (0, 0) } else { self.resolve(ch) };
            let advance = if ch == '\n' {
                0.0
            } else {
                self.advance(face, glyph, px)
            };
            let kern = match items.last() {
                Some(prev) if prev.face == face && prev.ch != '\n' => {
                    self.kern(face, prev.glyph, glyph, px)
                }
                _ => 0.0,
            };
            items.push(Item {
                ch,
                byte,
                face,
                glyph,
                advance,
                kern,
            });
        }

        let mut lines = break_lines(&items, if p.wrap { max_w } else { f32::INFINITY });
        let mut truncated = false;
        if let Some(n) = p.max_lines {
            if lines.len() > n {
                lines.truncate(n.max(1));
                truncated = true;
                if let Some(last) = lines.last_mut() {
                    last.force_ellipsis = p.ellipsis;
                }
            }
        }

        let ell = self.ellipsis_glyphs(px);
        let ell_w: f32 = ell.iter().map(|g| g.2).sum();

        let mut out = TextLayout {
            line_height: line_h,
            ..Default::default()
        };
        let mut widest: f32 = 0.0;
        for (li, l) in lines.iter().enumerate() {
            let mut end = l.end;
            let mut width = line_width(&items[l.start..end]);
            let mut add_ellipsis = l.force_ellipsis;
            if p.ellipsis && (width > max_w || add_ellipsis) {
                add_ellipsis = true;
                while end > l.start && line_width(&items[l.start..end]) + ell_w > max_w {
                    end -= 1;
                }
                // Don't leave a dangling space before the ellipsis.
                while end > l.start && items[end - 1].ch.is_whitespace() {
                    end -= 1;
                }
                width = line_width(&items[l.start..end]) + ell_w;
                truncated = true;
            }
            let baseline = li as f32 * line_h + lm.ascent;
            let g0 = out.glyphs.len();
            let mut x = 0.0;
            for (i, it) in items[l.start..end].iter().enumerate() {
                if it.ch == '\n' {
                    continue;
                }
                if i > 0 {
                    x += it.kern;
                }
                out.glyphs.push(PositionedGlyph {
                    face: it.face,
                    glyph: it.glyph,
                    pos: Vec2::new(x, baseline),
                    px: pxu,
                    byte: it.byte,
                    advance: it.advance,
                });
                x += it.advance;
            }
            if add_ellipsis {
                for &(face, glyph, adv) in &ell {
                    out.glyphs.push(PositionedGlyph {
                        face,
                        glyph,
                        pos: Vec2::new(x, baseline),
                        px: pxu,
                        byte: usize::MAX,
                        advance: adv,
                    });
                    x += adv;
                }
            }
            widest = widest.max(width);
            let bytes = items.get(l.start).map(|i| i.byte).unwrap_or(text.len())
                ..items.get(l.end).map(|i| i.byte).unwrap_or(text.len());
            out.lines.push(TextLine {
                glyphs: g0..out.glyphs.len(),
                bytes,
                width,
                x: 0.0,
                baseline,
            });
        }
        if out.lines.is_empty() {
            out.lines.push(TextLine {
                glyphs: 0..0,
                bytes: 0..0,
                width: 0.0,
                x: 0.0,
                baseline: lm.ascent,
            });
        }

        let box_w = p.max_width.unwrap_or(widest);
        for line in &mut out.lines {
            let dx = match p.align {
                Align::Left => 0.0,
                Align::Center => ((box_w - line.width) * 0.5).max(0.0),
                Align::Right => (box_w - line.width).max(0.0),
            };
            if dx != 0.0 {
                line.x = dx;
                for g in &mut out.glyphs[line.glyphs.clone()] {
                    g.pos.x += dx;
                }
            }
        }
        out.size = Vec2::new(widest, out.lines.len() as f32 * line_h);
        out.truncated = truncated;
        out
    }

    fn ellipsis_glyphs(&mut self, px: f32) -> Vec<(u16, u16, f32)> {
        let (f, g) = self.resolve('…');
        if g != 0 {
            return vec![(f, g, self.advance(f, g, px))];
        }
        let (f, g) = self.resolve('.');
        let a = self.advance(f, g, px);
        vec![(f, g, a); 3]
    }
}

#[derive(Debug, Clone, Copy)]
struct Item {
    ch: char,
    byte: usize,
    face: u16,
    glyph: u16,
    advance: f32,
    /// Kerning against the previous item.
    kern: f32,
}

#[derive(Debug, Clone, Copy)]
struct LineSpan {
    start: usize,
    end: usize,
    force_ellipsis: bool,
}

/// CJK ideographs, kana and hangul may break between any two characters.
fn is_cjk(ch: char) -> bool {
    matches!(ch as u32, 0x2E80..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF | 0x20000..=0x2FFFF)
}

/// Width of a run, excluding trailing whitespace and the first item's kerning.
fn line_width(items: &[Item]) -> f32 {
    let mut end = items.len();
    while end > 0 && (items[end - 1].ch.is_whitespace()) {
        end -= 1;
    }
    items[..end]
        .iter()
        .enumerate()
        .map(|(i, it)| it.advance + if i > 0 { it.kern } else { 0.0 })
        .sum()
}

/// Full advance of a run including whitespace (first item's kerning ignored).
fn run_width(items: &[Item]) -> f32 {
    items
        .iter()
        .enumerate()
        .map(|(i, it)| it.advance + if i > 0 { it.kern } else { 0.0 })
        .sum()
}

/// Greedy line breaking. Lines are item ranges; the '\n' item ends its line.
fn break_lines(items: &[Item], max_w: f32) -> Vec<LineSpan> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut x = 0.0f32;
    // Index where the next line would start if we broke at the last opportunity.
    let mut last_break: Option<usize> = None;
    for i in 0..items.len() {
        let it = items[i];
        if it.ch == '\n' {
            lines.push(LineSpan {
                start,
                end: i + 1,
                force_ellipsis: false,
            });
            start = i + 1;
            x = 0.0;
            last_break = None;
            continue;
        }
        if !it.ch.is_whitespace() && i > start && x + it.advance + it.kern > max_w {
            let brk = match last_break {
                Some(b) if b > start && b <= i => b,
                _ => i,
            };
            lines.push(LineSpan {
                start,
                end: brk,
                force_ellipsis: false,
            });
            start = brk;
            while start < i && items[start].ch.is_whitespace() {
                start += 1;
            }
            last_break = None;
            x = run_width(&items[start..i]);
        }
        x += it.advance + if i > start { it.kern } else { 0.0 };
        if it.ch.is_whitespace() || is_cjk(it.ch) || items.get(i + 1).is_some_and(|n| is_cjk(n.ch))
        {
            last_break = Some(i + 1);
        }
    }
    if start < items.len() || lines.is_empty() || items.last().is_some_and(|l| l.ch == '\n') {
        lines.push(LineSpan {
            start,
            end: items.len(),
            force_ellipsis: false,
        });
    }
    lines
}

/// Top-left of a block of `size` aligned inside `rect` (vertically centred).
pub fn align_in_rect(rect: Rect, size: Vec2, align: Align) -> Vec2 {
    let x = match align {
        Align::Left => rect.x,
        Align::Center => rect.x + (rect.w - size.x) * 0.5,
        Align::Right => rect.right() - size.x,
    };
    Vec2::new(x, rect.y + (rect.h - size.y) * 0.5)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fonts() -> Fonts {
        Fonts::with_default_font()
    }

    #[test]
    fn measure_scales_with_size() {
        let mut f = fonts();
        let a = f.measure("Hello world", 20.0);
        let b = f.measure("Hello world", 40.0);
        assert!(a > 50.0 && a < 200.0, "{a}");
        assert!((b / a - 2.0).abs() < 0.1);
        assert_eq!(f.measure("", 20.0), 0.0);
    }

    #[test]
    fn kerning_applies() {
        let mut f = fonts();
        let px = 40.0;
        let (_, a) = f.resolve('A');
        let (_, v) = f.resolve('V');
        let k = f.kern(0, a, v, px);
        let sum = f.advance(0, a, px) + f.advance(0, v, px);
        let w = f.measure("AV", px);
        assert!((w - (sum + k)).abs() < 0.01);
    }

    #[test]
    fn wraps_at_spaces() {
        let mut f = fonts();
        let text = "The quick brown fox jumps over the lazy dog";
        let full = f.measure(text, 20.0);
        let l = f.layout(text, TextParams::new(20.0).width(full / 2.5).wrap());
        assert!(l.lines.len() >= 3, "{}", l.lines.len());
        for line in &l.lines {
            assert!(line.width <= full / 2.5 + 0.01);
            // Lines start at word boundaries.
            let s = &text[line.bytes.clone()];
            assert!(!s.starts_with(' '), "{s:?}");
        }
        assert!((l.size.y - l.lines.len() as f32 * l.line_height).abs() < 0.01);
    }

    #[test]
    fn long_word_breaks_mid_word() {
        let mut f = fonts();
        let l = f.layout(
            "Supercalifragilisticexpialidocious",
            TextParams::new(20.0).width(60.0).wrap(),
        );
        assert!(l.lines.len() > 2);
        assert!(l.lines.iter().all(|ln| ln.width <= 60.01));
        assert_eq!(l.glyphs.len(), 34);
    }

    #[test]
    fn newlines_create_lines() {
        let mut f = fonts();
        let l = f.layout("a\nb\n\nc", TextParams::new(20.0));
        assert_eq!(l.lines.len(), 4);
        assert_eq!(l.glyphs.len(), 3);
    }

    #[test]
    fn ellipsis_truncates() {
        let mut f = fonts();
        let text = "A very long title that will not fit";
        let l = f.layout(text, TextParams::new(20.0).width(120.0).ellipsis());
        assert!(l.truncated);
        assert_eq!(l.lines.len(), 1);
        assert!(l.size.x <= 120.0 + 0.01);
        assert_eq!(l.glyphs.last().unwrap().byte, usize::MAX);
        let short = f.layout("Hi", TextParams::new(20.0).width(120.0).ellipsis());
        assert!(!short.truncated);
    }

    #[test]
    fn max_lines_with_ellipsis() {
        let mut f = fonts();
        let text = "one two three four five six seven eight nine ten eleven twelve";
        let l = f.layout(
            text,
            TextParams::new(20.0)
                .width(100.0)
                .wrap()
                .lines(2)
                .ellipsis(),
        );
        assert_eq!(l.lines.len(), 2);
        assert!(l.truncated);
        assert_eq!(l.glyphs.last().unwrap().byte, usize::MAX);
    }

    #[test]
    fn alignment_offsets() {
        let mut f = fonts();
        let w = f.measure("abc", 20.0);
        let c = f.layout(
            "abc",
            TextParams::new(20.0).width(200.0).align(Align::Center),
        );
        assert!((c.lines[0].x - (200.0 - w) / 2.0).abs() < 0.01);
        let r = f.layout(
            "abc",
            TextParams::new(20.0).width(200.0).align(Align::Right),
        );
        assert!((r.lines[0].x - (200.0 - w)).abs() < 0.01);
    }

    #[test]
    fn cjk_breaks_and_fallback_slot() {
        let mut f = fonts();
        // Noto Sans has no CJK; resolves to .notdef in the primary face.
        assert_eq!(f.resolve('日'), (0, 0));
        assert!(!f.has_glyph('日'));
        let text = "日本語のテキストを折り返す";
        let l = f.layout(text, TextParams::new(20.0).width(60.0).wrap());
        assert!(l.lines.len() > 1);
        assert_eq!(l.glyphs.len(), text.chars().count());
        // A fallback face is consulted for glyphs the primary lacks.
        f.add_fallback(DEFAULT_FONT).unwrap();
        assert_eq!(f.face_count(), 2);
        assert_eq!(f.resolve('A').0, 0);
    }

    #[test]
    fn caret_and_hit_testing() {
        let mut f = fonts();
        let l = f.layout("abcd", TextParams::new(20.0));
        assert_eq!(l.caret_x(0), 0.0);
        let x2 = l.caret_x(2);
        assert!(x2 > 0.0 && x2 < l.size.x);
        assert_eq!(l.caret_x(4), l.size.x);
        assert_eq!(l.hit_byte(x2 + 0.1, 4), 2);
        assert_eq!(l.hit_byte(-5.0, 4), 0);
        assert_eq!(l.hit_byte(1000.0, 4), 4);
    }

    #[test]
    fn atlas_rasterizes_and_resets() {
        let mut f = fonts();
        f.set_atlas_size(64);
        let v0 = f.atlas().version;
        let (face, g) = f.resolve('W');
        let e = f.glyph(face, g, 24).expect("fits");
        assert!(f.atlas().version > v0);
        assert!(e.uv[2] > e.uv[0] && e.uv[3] > e.uv[1]);
        assert!(f.atlas().pixels.iter().any(|&p| p > 0));
        // Space has no bitmap.
        let (sf, sg) = f.resolve(' ');
        assert!(f.glyph(sf, sg, 24).is_none());
        // Fill the tiny atlas until it overflows, then reset on next frame.
        let mut overflowed = false;
        for ch in "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz".chars() {
            let (face, g) = f.resolve(ch);
            if f.glyph(face, g, 40).is_none() {
                overflowed = true;
                break;
            }
        }
        assert!(overflowed);
        f.begin_frame();
        assert!(f.glyph(face, g, 24).is_some());
    }
}
