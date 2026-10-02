//! The per-panel immediate-mode context.
//!
//! One [`Ui`] per UI panel (library, player controls, keyboard…). Each frame:
//! [`Ui::begin_frame`] with the frame's [`FrameInput`], call widget/screen
//! functions, then [`Ui::end_frame`] for the [`UiOutput`] draw list.
//!
//! Interaction follows the classic hot/active model: a press over a widget
//! makes it *active* (captured by that pointer) until release; a click is a
//! release while still over the widget. Popups/modals register areas that
//! block pointer input to lower [`Layer`]s from the following frame on.
//! Focus navigation (thumbstick / D-pad) moves between focusable widgets
//! registered in the previous frame, picking the nearest in the pressed
//! direction.

use crate::geom::{Rect, Vec2};
use crate::input::{
    FrameInput, GazeDimmer, NavDir, NavInput, PointerInput, PointerSource, TextEvent,
};
use crate::layout::{Dir, Layout, ScrollState, FILL};
use crate::painter::{Layer, Painter};
use crate::text::{Align, Fonts, TextLayout, TextParams};
use crate::theme::{Color, Theme};
use crate::widgets::toast::Toasts;
use fp_core::draw::{AtlasImage, DrawList};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

/// Stable widget identity derived from the ID stack plus a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Id(pub u64);

impl Id {
    pub fn with(self, key: impl Hash) -> Id {
        let mut h = DefaultHasher::new();
        self.0.hash(&mut h);
        key.hash(&mut h);
        Id(h.finish())
    }
}

/// What a widget responds to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Sense {
    pub click: bool,
    /// Keeps capture while dragging and suppresses drag-to-scroll of parents.
    pub drag: bool,
    /// Reachable by thumbstick/D-pad navigation; A activates.
    pub focusable: bool,
    /// When focused, Left/Right go to the widget instead of moving focus.
    pub wants_horizontal: bool,
}

impl Sense {
    pub const HOVER: Sense = Sense {
        click: false,
        drag: false,
        focusable: false,
        wants_horizontal: false,
    };
    pub const CLICK: Sense = Sense {
        click: true,
        drag: false,
        focusable: true,
        wants_horizontal: false,
    };
    pub const DRAG: Sense = Sense {
        click: true,
        drag: true,
        focusable: true,
        wants_horizontal: true,
    };
}

/// Result of [`Ui::interact`].
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Response {
    pub id: Id,
    pub rect: Rect,
    /// A pointer is over the widget (and nothing else holds capture).
    pub hovered: bool,
    /// Hovered with the trigger touched / pinch starting: show "about to click".
    pub armed: bool,
    /// Held down by the capturing pointer.
    pub pressed: bool,
    /// Became active this frame.
    pub press_started: bool,
    pub clicked: bool,
    pub dragging: bool,
    /// Position of the capturing (or hovering) pointer.
    pub pointer: Option<Vec2>,
    pub focused: bool,
    /// Focus should be drawn (navigation mode rather than pointer mode).
    pub focus_visible: bool,
    /// Widget value changed this frame (set by widgets).
    pub changed: bool,
}

impl Response {
    /// Whether to draw the hover highlight.
    pub fn highlighted(&self) -> bool {
        self.hovered || (self.focused && self.focus_visible)
    }
}

/// Haptic/audio cues for the app to play on the matching controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feedback {
    /// Pointer entered a new interactive widget.
    HoverTick(PointerSource),
    /// A click landed.
    ClickTick(PointerSource),
}

/// Everything the renderer and app need after a frame.
#[derive(Debug, Clone, Default)]
pub struct UiOutput {
    pub draw_list: DrawList,
    /// Bumped whenever atlas pixels change; re-upload [`Ui::atlas`] then.
    pub atlas_version: u64,
    /// Gaze-dimming opacity already baked into vertex colours.
    pub opacity: f32,
    /// Some pointer is over the panel: don't route its trigger to playback.
    pub wants_pointer: bool,
    /// A text field has keyboard focus: route hardware/remote typing here.
    pub wants_text: bool,
    pub feedback: Vec<Feedback>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PointerState {
    pub input: PointerInput,
    pub prev_down: bool,
    pub prev_pos: Option<Vec2>,
    pub press_origin: Option<Vec2>,
    /// Scroll area that turned this pointer's press into a drag-scroll.
    pub capture: Option<Id>,
    pub hover: Option<Id>,
    pub prev_hover: Option<Id>,
    /// Innermost scroll area under the pointer (this / previous frame).
    pub scroll_target: Option<Id>,
    pub prev_scroll_target: Option<Id>,
}

impl PointerState {
    pub fn just_pressed(&self) -> bool {
        self.input.pressed && !self.prev_down
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Active {
    pub id: Id,
    pub pointer: usize,
    pub claims_drag: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct FocusItem {
    pub id: Id,
    pub rect: Rect,
    pub layer: Layer,
    pub wants_horizontal: bool,
    /// Enclosing scroll area and the item's content-space vertical extent.
    pub scroll: Option<(Id, f32, f32)>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ScrollCtx {
    pub id: Id,
    /// Screen y of content y = 0.
    pub origin_y: f32,
}

/// A widget recorded for automation/tests (see [`Ui::set_record_widgets`]).
#[derive(Debug, Clone, PartialEq)]
pub struct WidgetInfo {
    pub id: Id,
    /// Visible (clipped) rect.
    pub rect: Rect,
    pub label: String,
    pub layer: Layer,
}

/// Result of [`Ui::virtual_list`] / [`Ui::virtual_grid`].
#[derive(Debug, Clone, PartialEq)]
pub struct VirtualResponse {
    /// Scroll state key (see [`Ui::scroll_state`]).
    pub id: Id,
    /// Item indices built this frame (visible plus overscan).
    pub visible: std::ops::Range<usize>,
    pub viewport: Rect,
}

/// Caret state for a text field (text itself is owned by the caller).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TextEditState {
    /// Byte offset, always on a char boundary.
    pub caret: usize,
    pub scroll_x: f32,
}

pub struct Ui {
    pub theme: Theme,
    pub fonts: Fonts,
    pub(crate) size: Vec2,
    pub(crate) ppm: f32,
    pub(crate) painter: Painter,
    pub(crate) pointers: Vec<PointerState>,
    pub(crate) nav: NavInput,
    pub(crate) dt: f32,
    pub(crate) time: f64,
    pub(crate) id_stack: Vec<Id>,
    pub(crate) active: Option<Active>,
    active_seen: bool,
    pub(crate) focus: Option<Id>,
    pub(crate) focus_visible: bool,
    focus_seen: bool,
    pub(crate) focus_items: Vec<FocusItem>,
    prev_focus_items: Vec<FocusItem>,
    pub(crate) text_focus: Option<Id>,
    text_focus_seen: bool,
    pub(crate) text_queue: Vec<TextEvent>,
    pub(crate) layouts: Vec<Layout>,
    pub(crate) areas: Vec<(Layer, Rect)>,
    prev_areas: Vec<(Layer, Rect)>,
    pub(crate) scroll_states: HashMap<Id, ScrollState>,
    pub(crate) scroll_stack: Vec<ScrollCtx>,
    pub(crate) edit_states: HashMap<Id, TextEditState>,
    pub(crate) open_popup: Option<Id>,
    /// Small per-widget float memory (animations, measured heights).
    pub(crate) memory: HashMap<Id, f32>,
    pub(crate) qr_cache: HashMap<String, Option<crate::widgets::qr::QrMatrix>>,
    pub toasts: Toasts,
    pub dimmer: GazeDimmer,
    pub(crate) interacted: bool,
    pub(crate) feedback: Vec<Feedback>,
    gaze_on_panel: Option<bool>,
    in_frame: bool,
    record_widgets: bool,
    recording: Vec<WidgetInfo>,
    widgets: Vec<WidgetInfo>,
}

impl std::fmt::Debug for Ui {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ui")
            .field("size", &self.size)
            .field("ppm", &self.ppm)
            .field("focus", &self.focus)
            .finish_non_exhaustive()
    }
}

/// Assumed viewing distance for theme sizing.
pub const DEFAULT_VIEW_DISTANCE_M: f32 = 1.5;

impl Ui {
    /// A panel of `size_px` pixels at `pixels_per_meter` density, with the
    /// embedded font and a dark theme sized for ~1.5 m.
    pub fn new(size_px: Vec2, pixels_per_meter: f32) -> Ui {
        Ui::with_fonts(Fonts::with_default_font(), size_px, pixels_per_meter)
    }

    pub fn with_fonts(fonts: Fonts, size_px: Vec2, pixels_per_meter: f32) -> Ui {
        Ui {
            theme: Theme::for_panel(pixels_per_meter, DEFAULT_VIEW_DISTANCE_M),
            fonts,
            size: size_px,
            ppm: pixels_per_meter,
            painter: Painter::new(size_px),
            pointers: Vec::new(),
            nav: NavInput::default(),
            dt: 0.0,
            time: 0.0,
            id_stack: vec![Id(0x5eed)],
            active: None,
            active_seen: false,
            focus: None,
            focus_visible: false,
            focus_seen: false,
            focus_items: Vec::new(),
            prev_focus_items: Vec::new(),
            text_focus: None,
            text_focus_seen: false,
            text_queue: Vec::new(),
            layouts: Vec::new(),
            areas: Vec::new(),
            prev_areas: Vec::new(),
            scroll_states: HashMap::new(),
            scroll_stack: Vec::new(),
            edit_states: HashMap::new(),
            open_popup: None,
            memory: HashMap::new(),
            qr_cache: HashMap::new(),
            toasts: Toasts::default(),
            dimmer: GazeDimmer::default(),
            interacted: false,
            feedback: Vec::new(),
            gaze_on_panel: None,
            in_frame: false,
            record_widgets: false,
            recording: Vec::new(),
            widgets: Vec::new(),
        }
    }

    pub fn size(&self) -> Vec2 {
        self.size
    }

    pub fn pixels_per_meter(&self) -> f32 {
        self.ppm
    }

    /// Panel size in metres (for sizing the OpenXR layer).
    pub fn size_m(&self) -> Vec2 {
        self.size / self.ppm
    }

    pub fn set_size(&mut self, size_px: Vec2) {
        self.size = size_px;
    }

    pub fn screen_rect(&self) -> Rect {
        Rect::new(0.0, 0.0, self.size.x, self.size.y)
    }

    /// Seconds since the first frame.
    pub fn time(&self) -> f64 {
        self.time
    }

    pub fn dt(&self) -> f32 {
        self.dt
    }

    pub fn atlas(&self) -> &AtlasImage {
        self.fonts.atlas()
    }

    // ----- frame ------------------------------------------------------------

    pub fn begin_frame(&mut self, input: FrameInput) {
        debug_assert!(!self.in_frame, "begin_frame called twice");
        self.in_frame = true;
        self.fonts.begin_frame();
        self.painter.reset(self.size);
        self.dt = input.dt.clamp(0.0, 0.25);
        self.time += self.dt as f64;
        self.gaze_on_panel = input.gaze_on_panel;
        self.interacted = false;
        self.feedback.clear();

        // Match pointers by source so press edges survive reordering.
        let mut next = Vec::with_capacity(input.pointers.len());
        for p in input.pointers {
            let prev = self
                .pointers
                .iter()
                .find(|s| s.input.source == p.source)
                .copied();
            let mut st = PointerState {
                input: p,
                prev_down: prev.is_some_and(|s| s.input.pressed),
                prev_pos: prev.and_then(|s| s.input.pos),
                press_origin: prev.and_then(|s| s.press_origin),
                capture: prev.and_then(|s| s.capture),
                hover: None,
                prev_hover: prev.and_then(|s| s.hover),
                scroll_target: None,
                prev_scroll_target: prev.and_then(|s| s.scroll_target),
            };
            if st.just_pressed() {
                st.press_origin = p.pos;
                st.capture = None;
                self.focus_visible = false;
                self.interacted = true;
            }
            if !p.pressed {
                st.press_origin = None;
                st.capture = None;
            }
            if p.scroll.length_squared() > 0.01 {
                self.interacted = true;
            }
            next.push(st);
        }
        // An active widget whose pointer vanished loses capture.
        if let Some(a) = self.active {
            let src = self.pointers.get(a.pointer).map(|p| p.input.source);
            match next.iter().position(|p| Some(p.input.source) == src) {
                Some(i) => self.active = Some(Active { pointer: i, ..a }),
                None => self.active = None,
            }
        }
        self.pointers = next;

        self.nav = input.nav;
        if self.nav.dir.is_some() || self.nav.activate || self.nav.back {
            self.interacted = true;
        }
        self.text_queue.extend(input.text);
        self.prev_focus_items = std::mem::take(&mut self.focus_items);
        self.prev_areas = std::mem::take(&mut self.areas);
        self.resolve_nav();

        self.id_stack.truncate(1);
        self.layouts.clear();
        self.layouts.push(Layout::new(
            self.screen_rect(),
            Dir::Vertical,
            self.theme.spacing,
        ));
        self.scroll_stack.clear();
        self.active_seen = false;
        self.focus_seen = false;
        self.text_focus_seen = false;
    }

    pub fn end_frame(&mut self) -> UiOutput {
        debug_assert!(self.in_frame, "end_frame without begin_frame");
        self.in_frame = false;
        self.draw_toasts();

        if !self.active_seen {
            self.active = None;
        }
        if !self.focus_seen {
            self.focus = None;
        }
        if !self.text_focus_seen {
            self.text_focus = None;
        }
        if self.text_focus.is_none() {
            self.text_queue.clear();
        }
        // Hover-enter ticks for controller haptics.
        for p in &self.pointers {
            if p.hover.is_some() && p.hover != p.prev_hover && p.input.source.has_haptics() {
                self.feedback.push(Feedback::HoverTick(p.input.source));
            }
        }
        let screen = self.screen_rect();
        let wants_pointer = self
            .pointers
            .iter()
            .any(|p| p.input.pos.is_some_and(|q| screen.contains(q)));
        let opacity =
            self.dimmer
                .update(self.dt, self.gaze_on_panel, wants_pointer, self.interacted);
        self.widgets = std::mem::take(&mut self.recording);
        let mut draw_list = DrawList::default();
        self.painter.finish_into(&mut draw_list, opacity);
        UiOutput {
            draw_list,
            atlas_version: self.fonts.atlas().version,
            opacity,
            wants_pointer,
            wants_text: self.text_focus.is_some(),
            feedback: std::mem::take(&mut self.feedback),
        }
    }

    // ----- automation ------------------------------------------------------

    /// Records labelled interactive widgets each frame, for tests, automation
    /// and a future accessibility/remote-inspection API.
    pub fn set_record_widgets(&mut self, on: bool) {
        self.record_widgets = on;
    }

    /// Widgets recorded during the last completed frame.
    pub fn widgets(&self) -> &[WidgetInfo] {
        &self.widgets
    }

    /// First widget from the last frame whose label equals `label`.
    pub fn find_widget(&self, label: &str) -> Option<&WidgetInfo> {
        self.widgets.iter().find(|w| w.label == label)
    }

    pub(crate) fn record(&mut self, id: Id, rect: Rect, label: impl FnOnce() -> String) {
        if self.record_widgets {
            let rect = rect.intersect(&self.painter.clip());
            if !rect.is_empty() {
                let layer = self.painter.layer();
                self.recording.push(WidgetInfo {
                    id,
                    rect,
                    label: label(),
                    layer,
                });
            }
        }
    }

    // ----- ids ----------------------------------------------------------------

    pub fn make_id(&self, key: impl Hash) -> Id {
        self.id_stack.last().copied().unwrap_or_default().with(key)
    }

    pub fn push_id(&mut self, key: impl Hash) {
        let id = self.make_id(key);
        self.id_stack.push(id);
    }

    pub fn pop_id(&mut self) {
        if self.id_stack.len() > 1 {
            self.id_stack.pop();
        }
    }

    /// Runs `f` with `key` pushed on the ID stack.
    pub fn scope<R>(&mut self, key: impl Hash, f: impl FnOnce(&mut Ui) -> R) -> R {
        self.push_id(key);
        let r = f(self);
        self.pop_id();
        r
    }

    // ----- input state ----------------------------------------------------------

    /// The B button was pressed and nothing consumed it yet.
    pub fn back_pressed(&self) -> bool {
        self.nav.back
    }

    /// Consumes the B press (returns whether there was one).
    pub fn take_back(&mut self) -> bool {
        std::mem::take(&mut self.nav.back)
    }

    pub fn focused(&self) -> Option<Id> {
        self.focus
    }

    pub fn set_focus(&mut self, id: Option<Id>) {
        self.focus = id;
    }

    pub fn text_focus(&self) -> Option<Id> {
        self.text_focus
    }

    pub fn set_text_focus(&mut self, id: Option<Id>) {
        self.text_focus = id;
        self.text_focus_seen = id.is_some();
    }

    /// Queues text events for the focused text field (e.g. from a companion
    /// web remote). Dropped at frame end if no field has focus.
    pub fn send_text(&mut self, ev: TextEvent) {
        self.text_queue.push(ev);
    }

    pub fn is_popup_open(&self, id: Id) -> bool {
        self.open_popup == Some(id)
    }

    /// Any pointer pressed this frame, with its position.
    pub(crate) fn any_just_pressed(&self) -> Option<Vec2> {
        self.pointers
            .iter()
            .find(|p| p.just_pressed())
            .and_then(|p| p.input.pos)
    }

    pub(crate) fn blocked(&self, pos: Vec2, layer: Layer) -> bool {
        self.prev_areas
            .iter()
            .any(|(l, r)| *l > layer && r.contains(pos))
    }

    /// Registers an overlay area that blocks lower layers from next frame.
    pub(crate) fn add_area(&mut self, layer: Layer, rect: Rect) {
        self.areas.push((layer, rect));
    }

    /// Core interaction: hover, capture, click, drag and focus for `rect`.
    pub fn interact(&mut self, rect: Rect, id: Id, sense: Sense) -> Response {
        let layer = self.painter.layer();
        let hit_rect = rect.intersect(&self.painter.clip());
        let mut r = Response {
            id,
            rect,
            ..Default::default()
        };
        let interactive = sense.click || sense.drag;

        for pi in 0..self.pointers.len() {
            let p = self.pointers[pi];
            let Some(pos) = p.input.pos else { continue };
            if p.capture.is_some() || !hit_rect.contains(pos) || self.blocked(pos, layer) {
                continue;
            }
            let free = self.active.is_none_or_id(id);
            if free {
                r.hovered = true;
                r.pointer = Some(pos);
                r.armed |= p.input.touch_hint || p.input.pressed;
                if interactive {
                    self.pointers[pi].hover = Some(id);
                }
            }
            if interactive && p.just_pressed() && self.active.is_none() {
                self.active = Some(Active {
                    id,
                    pointer: pi,
                    claims_drag: sense.drag,
                });
                r.press_started = true;
                if sense.focusable {
                    self.focus = Some(id);
                }
            }
        }

        if let Some(a) = self.active.filter(|a| a.id == id) {
            self.active_seen = true;
            let p = self.pointers[a.pointer];
            r.pointer = p.input.pos.or(r.pointer);
            if p.input.pressed {
                r.pressed = true;
                r.dragging = sense.drag;
            } else {
                let over = p
                    .input
                    .pos
                    .is_some_and(|q| hit_rect.contains(q) && !self.blocked(q, layer));
                if over && sense.click {
                    r.clicked = true;
                    self.feedback.push(Feedback::ClickTick(p.input.source));
                }
                self.active = None;
            }
        }

        if sense.focusable {
            let scroll = self
                .scroll_stack
                .last()
                .map(|c| (c.id, rect.y - c.origin_y, rect.bottom() - c.origin_y));
            self.focus_items.push(FocusItem {
                id,
                rect,
                layer,
                wants_horizontal: sense.wants_horizontal,
                scroll,
            });
            if self.focus == Some(id) {
                self.focus_seen = true;
                r.focused = true;
                r.focus_visible = self.focus_visible;
                if self.nav.activate && sense.click {
                    self.nav.activate = false;
                    r.clicked = true;
                }
            }
        }
        r
    }

    /// Horizontal navigation step delivered to the focused widget that asked
    /// for it (`Sense::wants_horizontal`): -1, 0 or +1. Consumes it.
    pub fn take_nav_horizontal(&mut self, resp: &Response) -> i32 {
        if !resp.focused {
            return 0;
        }
        match self.nav.dir {
            Some(NavDir::Left) => {
                self.nav.dir = None;
                -1
            }
            Some(NavDir::Right) => {
                self.nav.dir = None;
                1
            }
            _ => 0,
        }
    }

    fn resolve_nav(&mut self) {
        let Some(dir) = self.nav.dir else { return };
        let Some(top) = self.prev_focus_items.iter().map(|i| i.layer).max() else {
            return;
        };
        let items: Vec<FocusItem> = self
            .prev_focus_items
            .iter()
            .filter(|i| i.layer == top)
            .copied()
            .collect();
        self.focus_visible = true;
        let current = self
            .focus
            .and_then(|f| items.iter().find(|i| i.id == f))
            .copied();
        let target = match current {
            None => items
                .iter()
                .min_by(|a, b| {
                    (a.rect.y, a.rect.x)
                        .partial_cmp(&(b.rect.y, b.rect.x))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .copied(),
            Some(c) => {
                if c.wants_horizontal && matches!(dir, NavDir::Left | NavDir::Right) {
                    return;
                }
                let cands: Vec<(Id, Rect)> = items.iter().map(|i| (i.id, i.rect)).collect();
                nav_target(c.rect, dir, &cands)
                    .and_then(|id| items.iter().find(|i| i.id == id))
                    .copied()
            }
        };
        if let Some(t) = target {
            self.focus = Some(t.id);
            self.nav.dir = None;
            if let Some((sid, top, bottom)) = t.scroll {
                let pad = self.theme.spacing;
                if let Some(st) = self.scroll_states.get_mut(&sid) {
                    st.scroll_to_visible(top - pad, bottom + pad);
                }
            }
        }
    }

    // ----- layout -------------------------------------------------------------

    pub(crate) fn layout_mut(&mut self) -> &mut Layout {
        if self.layouts.is_empty() {
            let screen = self.screen_rect();
            self.layouts
                .push(Layout::new(screen, Dir::Vertical, self.theme.spacing));
        }
        self.layouts.last_mut().expect("layout stack")
    }

    /// Remaining space in the current layout.
    pub fn available(&mut self) -> Rect {
        self.layout_mut().available()
    }

    /// Places a widget of `size` ([`FILL`] allowed) in the current layout.
    pub fn allocate(&mut self, size: Vec2) -> Rect {
        self.layout_mut().allocate(size)
    }

    /// Full-width (vertical layout) or full-height (horizontal) item.
    pub fn allocate_row(&mut self, extent: f32) -> Rect {
        match self.layout_mut().dir {
            Dir::Vertical => self.allocate(Vec2::new(FILL, extent)),
            Dir::Horizontal => self.allocate(Vec2::new(extent, FILL)),
        }
    }

    pub fn add_space(&mut self, px: f32) {
        self.layout_mut().add_space(px);
    }

    /// Runs `f` in a layout covering `rect` without advancing the parent.
    /// Returns `f`'s result and the content size it used.
    pub fn region<R>(&mut self, rect: Rect, dir: Dir, f: impl FnOnce(&mut Ui) -> R) -> (R, Vec2) {
        self.layouts
            .push(Layout::new(rect, dir, self.theme.spacing));
        let r = f(self);
        let used = self
            .layouts
            .pop()
            .map(|l| l.content_size())
            .unwrap_or_default();
        (r, used)
    }

    fn child<R>(&mut self, dir: Dir, max_h: Option<f32>, f: impl FnOnce(&mut Ui) -> R) -> R {
        let avail = self.available();
        let rect = match max_h {
            Some(h) => Rect::new(avail.x, avail.y, avail.w, avail.h.min(h)),
            None => avail,
        };
        let (r, used) = self.region(rect, dir, f);
        let parent_dir = self.layout_mut().dir;
        match parent_dir {
            Dir::Vertical => self.allocate(Vec2::new(avail.w, used.y)),
            Dir::Horizontal => self.allocate(Vec2::new(used.x, used.y)),
        };
        r
    }

    /// Lays children left-to-right. [`FILL`]-height children get one
    /// `widget_height`; the row grows to its tallest child.
    pub fn horizontal<R>(&mut self, f: impl FnOnce(&mut Ui) -> R) -> R {
        let h = self.theme.widget_height;
        self.child(Dir::Horizontal, Some(h), f)
    }

    /// A row of fixed height.
    pub fn row<R>(&mut self, height: f32, f: impl FnOnce(&mut Ui) -> R) -> R {
        let rect = self.allocate_row(height);
        self.region(rect, Dir::Horizontal, f).0
    }

    pub fn vertical<R>(&mut self, f: impl FnOnce(&mut Ui) -> R) -> R {
        self.child(Dir::Vertical, None, f)
    }

    /// `n` equal-width columns; parent advances by the tallest.
    pub fn columns(&mut self, n: usize, mut f: impl FnMut(&mut Ui, usize)) {
        let avail = self.available();
        let spacing = self.theme.spacing;
        let mut max_h: f32 = 0.0;
        for (i, (x, w)) in crate::layout::split_even(avail.w, n, spacing)
            .into_iter()
            .enumerate()
        {
            let rect = Rect::new(avail.x + x, avail.y, w, avail.h);
            let ((), used) = self.region(rect, Dir::Vertical, |ui| f(ui, i));
            max_h = max_h.max(used.y);
        }
        self.allocate(Vec2::new(avail.w, max_h));
    }

    /// Children inset by `pad` on every side.
    pub fn padded<R>(&mut self, pad: f32, f: impl FnOnce(&mut Ui) -> R) -> R {
        let avail = self.available();
        let inner = avail.shrink(pad);
        let (r, used) = self.region(inner, Dir::Vertical, f);
        self.allocate(Vec2::new(avail.w, used.y + 2.0 * pad));
        r
    }

    // ----- painting helpers ---------------------------------------------------

    pub fn painter(&mut self) -> &mut Painter {
        &mut self.painter
    }

    pub fn layout_text(&mut self, text: &str, params: TextParams) -> TextLayout {
        self.fonts.layout(text, params)
    }

    /// Draws text aligned within `rect` (vertically centred, single line with ellipsis).
    pub fn draw_text_in(&mut self, rect: Rect, text: &str, size: f32, color: Color, align: Align) {
        let layout = self.fonts.layout(
            text,
            TextParams::new(size).width(rect.w).ellipsis().align(align),
        );
        let pos = Vec2::new(rect.x, rect.y + (rect.h - layout.size.y) * 0.5);
        self.painter.text(&mut self.fonts, pos, &layout, color);
    }

    pub fn draw_text_at(&mut self, pos: Vec2, layout: &TextLayout, color: Color) {
        self.painter.text(&mut self.fonts, pos, layout, color);
    }

    /// Focus ring around `rect` if `resp` is keyboard-focused.
    pub(crate) fn focus_ring(&mut self, resp: &Response, radius: f32) {
        if resp.focused && resp.focus_visible {
            let w = self.theme.focus_ring_width;
            self.painter.rect_stroke(
                resp.rect.expand(w + 1.0),
                radius + w,
                w,
                self.theme.focus_ring,
            );
        }
    }

    /// Background colour for a surface-style widget in its current state.
    pub(crate) fn surface_color(&self, resp: &Response) -> Color {
        if resp.pressed {
            self.theme.surface_active
        } else if resp.armed {
            self.theme
                .surface_hover
                .lerp(self.theme.surface_active, 0.5)
        } else if resp.highlighted() {
            self.theme.surface_hover
        } else {
            self.theme.surface
        }
    }

    /// Animated 0..1 value approaching `target`, remembered per id.
    pub fn animate(&mut self, id: Id, target: f32, rate: f32) -> f32 {
        let dt = self.dt;
        let v = self.memory.entry(id).or_insert(target);
        *v += (target - *v) * (1.0 - (-rate * dt).exp());
        if (*v - target).abs() < 1e-3 {
            *v = target;
        }
        *v
    }

    // ----- scrolling ------------------------------------------------------------

    pub fn scroll_state(&self, id: Id) -> Option<&ScrollState> {
        self.scroll_states.get(&id)
    }

    pub fn scroll_state_mut(&mut self, id: Id) -> &mut ScrollState {
        self.scroll_states.entry(id).or_default()
    }

    /// Handles drag-to-scroll, thumbstick and momentum for `viewport`.
    pub(crate) fn scroll_input(&mut self, id: Id, viewport: Rect, content: f32) -> ScrollState {
        let layer = self.painter.layer();
        let clip = self.painter.clip().intersect(&viewport);
        let threshold = self.theme.drag_threshold;
        let dt = self.dt;
        let mut st = self.scroll_states.get(&id).copied().unwrap_or_default();
        st.set_extent(content, viewport.h);
        let mut dragged = false;
        for pi in 0..self.pointers.len() {
            let p = self.pointers[pi];
            let Some(pos) = p.input.pos else {
                if p.capture == Some(id) && !p.input.pressed {
                    st.release();
                }
                continue;
            };
            let inside = clip.contains(pos) && !self.blocked(pos, layer);
            if inside {
                self.pointers[pi].scroll_target = Some(id);
                // Touching a flinging list stops it (so the press lands where aimed).
                if p.just_pressed() {
                    st.velocity = 0.0;
                }
            }
            let targeted = p.prev_scroll_target.is_none() || p.prev_scroll_target == Some(id);
            if p.capture == Some(id) {
                if p.input.pressed {
                    st.drag(pos.y - p.prev_pos.unwrap_or(pos).y, dt);
                    dragged = true;
                }
                continue;
            }
            if p.capture.is_none() && p.input.pressed && targeted {
                if let Some(origin) = p.press_origin.filter(|o| clip.contains(*o)) {
                    let d = pos - origin;
                    let active_ok = match self.active {
                        None => true,
                        Some(a) => a.pointer == pi && !a.claims_drag,
                    };
                    if d.y.abs() > threshold
                        && d.y.abs() > d.x.abs()
                        && active_ok
                        && st.max_offset() > 0.0
                    {
                        self.pointers[pi].capture = Some(id);
                        if self.active.is_some_and(|a| a.pointer == pi) {
                            self.active = None;
                        }
                        st.velocity = 0.0;
                        st.drag(d.y, dt);
                        dragged = true;
                    }
                }
            }
            if inside && targeted && p.input.scroll.y.abs() > 0.0 {
                st.stick(p.input.scroll.y, dt);
            }
        }
        if !dragged {
            st.release();
            st.step(dt);
        }
        self.scroll_states.insert(id, st);
        st
    }

    pub(crate) fn draw_scrollbar(&mut self, viewport: Rect, st: &ScrollState) {
        let w = self.theme.scrollbar_width;
        if let Some((pos, len)) = st.thumb(viewport.h, w * 4.0) {
            let track = Rect::new(viewport.right() - w, viewport.y, w, viewport.h);
            self.painter
                .rect_rounded(track, w * 0.5, self.theme.surface.alpha(0.6));
            self.painter.rect_rounded(
                Rect::new(track.x, track.y + pos, w, len),
                w * 0.5,
                self.theme.text_dim,
            );
        }
    }

    /// A vertically scrolling region `height` tall ([`FILL`] for the rest).
    /// Content is laid out top-down; drag, thumbstick and momentum supported.
    pub fn scroll_area<R>(
        &mut self,
        key: impl Hash,
        height: f32,
        f: impl FnOnce(&mut Ui) -> R,
    ) -> R {
        let id = self.make_id(("scroll", key));
        let viewport = self.allocate(Vec2::new(FILL, height));
        let prev_content = self
            .scroll_states
            .get(&id)
            .map(|s| s.content)
            .unwrap_or(0.0);
        let st = self.scroll_input(id, viewport, prev_content);
        let bar = self.theme.scrollbar_width + self.theme.spacing * 0.5;
        let content_rect = Rect::new(
            viewport.x,
            viewport.y - st.offset,
            viewport.w - bar,
            f32::MAX / 4.0,
        );
        self.painter.push_clip(viewport);
        self.scroll_stack.push(ScrollCtx {
            id,
            origin_y: content_rect.y,
        });
        self.push_id(id.0);
        let (r, used) = self.region(content_rect, Dir::Vertical, f);
        self.pop_id();
        self.scroll_stack.pop();
        self.painter.pop_clip();
        let st = self.scroll_states.entry(id).or_default();
        st.set_extent(used.y, viewport.h);
        let st = *st;
        self.draw_scrollbar(viewport, &st);
        r
    }

    /// Virtualized uniform list: only rows in (or next to) the viewport are
    /// built. `row` gets the index and its rect.
    pub fn virtual_list(
        &mut self,
        key: impl Hash,
        count: usize,
        row_h: f32,
        height: f32,
        mut row: impl FnMut(&mut Ui, usize, Rect),
    ) -> VirtualResponse {
        let id = self.make_id(("vlist", key));
        let viewport = self.allocate(Vec2::new(FILL, height));
        let spacing = self.theme.spacing;
        let st = self.scroll_input(
            id,
            viewport,
            crate::layout::list_height(count, row_h, spacing),
        );
        let bar = self.theme.scrollbar_width + spacing * 0.5;
        let origin_y = viewport.y - st.offset;
        self.painter.push_clip(viewport);
        self.scroll_stack.push(ScrollCtx { id, origin_y });
        let visible = crate::layout::visible_range(st.offset, viewport.h, row_h, spacing, count, 1);
        for i in visible.clone() {
            let rect = Rect::new(
                viewport.x,
                origin_y + i as f32 * (row_h + spacing),
                viewport.w - bar,
                row_h,
            );
            self.push_id(("row", i));
            self.region(rect, Dir::Horizontal, |ui| row(ui, i, rect));
            self.pop_id();
        }
        self.scroll_stack.pop();
        self.painter.pop_clip();
        self.draw_scrollbar(viewport, &st);
        VirtualResponse {
            id,
            visible,
            viewport,
        }
    }

    /// Virtualized grid of tiles at least `min_tile_w` wide with height
    /// `tile_h_for(tile_w)`.
    pub fn virtual_grid(
        &mut self,
        key: impl Hash,
        count: usize,
        min_tile_w: f32,
        tile_h_for: impl Fn(f32) -> f32,
        height: f32,
        mut tile: impl FnMut(&mut Ui, usize, Rect),
    ) -> VirtualResponse {
        let id = self.make_id(("vgrid", key));
        let viewport = self.allocate(Vec2::new(FILL, height));
        let spacing = self.theme.spacing;
        let bar = self.theme.scrollbar_width + spacing * 0.5;
        let (cols, tile_w) = crate::layout::grid_columns(viewport.w - bar, min_tile_w, spacing);
        let tile_h = tile_h_for(tile_w);
        let rows = count.div_ceil(cols);
        let st = self.scroll_input(
            id,
            viewport,
            crate::layout::list_height(rows, tile_h, spacing),
        );
        let origin_y = viewport.y - st.offset;
        self.painter.push_clip(viewport);
        self.scroll_stack.push(ScrollCtx { id, origin_y });
        let visible = crate::layout::grid_visible_range(
            st.offset, viewport.h, cols, tile_h, spacing, count, 1,
        );
        for i in visible.clone() {
            let (r, c) = (i / cols, i % cols);
            let rect = Rect::new(
                viewport.x + c as f32 * (tile_w + spacing),
                origin_y + r as f32 * (tile_h + spacing),
                tile_w,
                tile_h,
            );
            self.push_id(("tile", i));
            self.region(rect, Dir::Vertical, |ui| tile(ui, i, rect));
            self.pop_id();
        }
        self.scroll_stack.pop();
        self.painter.pop_clip();
        self.draw_scrollbar(viewport, &st);
        VirtualResponse {
            id,
            visible,
            viewport,
        }
    }
}

trait ActiveExt {
    fn is_none_or_id(&self, id: Id) -> bool;
}

impl ActiveExt for Option<Active> {
    fn is_none_or_id(&self, id: Id) -> bool {
        match self {
            None => true,
            Some(a) => a.id == id,
        }
    }
}

/// Picks the best focus target from `from` in direction `dir`: the nearest
/// candidate ahead, penalizing perpendicular offset.
pub fn nav_target(from: Rect, dir: NavDir, candidates: &[(Id, Rect)]) -> Option<Id> {
    let fc = from.center();
    let mut best: Option<(f32, Id)> = None;
    for &(id, r) in candidates {
        if r == from {
            continue;
        }
        let c = r.center();
        let (along, gap_perp) = match dir {
            NavDir::Down => (c.y - fc.y, axis_gap(r.x, r.right(), from.x, from.right())),
            NavDir::Up => (fc.y - c.y, axis_gap(r.x, r.right(), from.x, from.right())),
            NavDir::Right => (c.x - fc.x, axis_gap(r.y, r.bottom(), from.y, from.bottom())),
            NavDir::Left => (fc.x - c.x, axis_gap(r.y, r.bottom(), from.y, from.bottom())),
        };
        if along <= 1.0 {
            continue;
        }
        // Distance between centres on the perpendicular axis breaks ties among overlapping items.
        let perp_centre = match dir {
            NavDir::Up | NavDir::Down => (c.x - fc.x).abs(),
            NavDir::Left | NavDir::Right => (c.y - fc.y).abs(),
        };
        let score = along + gap_perp * 4.0 + perp_centre * 0.25;
        if best.is_none_or_lt(score) {
            best = Some((score, id));
        }
    }
    best.map(|(_, id)| id)
}

trait BestExt {
    fn is_none_or_lt(&self, score: f32) -> bool;
}

impl BestExt for Option<(f32, Id)> {
    fn is_none_or_lt(&self, score: f32) -> bool {
        match self {
            None => true,
            Some((s, _)) => score < *s,
        }
    }
}

/// Gap between intervals `[a0,a1]` and `[b0,b1]` (0 when overlapping).
fn axis_gap(a0: f32, a1: f32, b0: f32, b1: f32) -> f32 {
    (b0 - a1).max(a0 - b1).max(0.0)
}
