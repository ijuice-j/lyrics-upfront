//! The Focus card: state, drawing, and frame scheduling.
//!
//! The Focus design with whole-line highlighting (Spotify style), matching
//! Spotify Lyrics Overlay's web/style.css card: a tinted rounded card, header (cover, title,
//! artist, clock, close), five lyric slots with the neighbours blurred and
//! faded by distance, the sung line lit whole, a 720ms "rack focus" roll on
//! line change (stack rises, arriving line pulls into focus, departing line
//! softens), a 900ms colour crossfade, and a 2px progress bar.
//!
//! Efficiency comes from two things the web version could not do:
//!  1. Layers. The plate+header and the blurred neighbours only change on a
//!     line/track/theme/size change, so they are drawn once into bitmaps; the
//!     sung and departing lines are pre-rendered sharp and blurred, so the
//!     roll is a crossfade. A frame is a few bitmap copies, clock and bar.
//!  2. Scheduling. A frame is drawn at vsync only while something is moving
//!     (the roll, the colour crossfade). Otherwise the app sleeps until the
//!     next visible change after the last frame drawn: the next line, the
//!     clock ticking over, or the progress bar advancing one physical pixel.

use std::time::{Duration, Instant};

use windows::core::Result;
use windows::Foundation::Numerics::Matrix3x2;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;

use crate::gfx::{bitmap_props, Gpu};
use crate::lyrics::Line;
use crate::palette::{Theme, NEUTRAL};
use crate::text::Text;

const ROLL: Duration = Duration::from_millis(720);
const APPEAR: Duration = Duration::from_millis(560);
const CLOSE_FADE: Duration = Duration::from_millis(160);
const CROSSFADE: Duration = Duration::from_millis(900);
const BREATHE_MS: f32 = 1400.0;

const PAD_X: f32 = 26.0;
const PAD_TOP: f32 = 18.0;
const PAD_BOTTOM: f32 = 20.0;
const HEADER_H: f32 = 24.0;
/// The progress hairline: 2px, 9px below the title row (as in the review page).
const BAR_GAP: f32 = 9.0;
const BAR_H: f32 = 2.0;
/// Width reserved for the "0:45 / 3:26" clock.
const CLOCK_W: f32 = 96.0;

/// Width of the progress hairline in DIPs for a card `w` DIPs wide; the
/// scheduler wakes each time the fill advances one physical pixel of it.
pub fn progress_width(w: f32) -> f32 {
    (w - 2.0 * PAD_X).max(0.0)
}
const RADIUS: f32 = 22.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Idle,
    Loading,
    Ready,
    Unsynced,
    NoLyrics,
    Offline,
}

pub struct Model {
    pub status: Status,
    pub title: String,
    pub artist: String,
    pub lines: Vec<Line>,
    pub duration_ms: i64,
    pub anchor_ms: i64,
    pub anchor_at: Instant,
    pub playing: bool,
    pub theme_from: Theme,
    pub theme_to: Theme,
    pub theme_at: Option<Instant>,
    pub plate_core: f32,
    pub plate_edge: f32,
    pub text_scale: f32,
}

impl Model {
    pub fn new() -> Self {
        Model {
            status: Status::Idle,
            title: String::new(),
            artist: String::new(),
            lines: vec![],
            duration_ms: 0,
            anchor_ms: 0,
            anchor_at: Instant::now(),
            playing: false,
            theme_from: NEUTRAL,
            theme_to: NEUTRAL,
            theme_at: None,
            plate_core: 0.75,
            plate_edge: 0.64,
            text_scale: 1.0,
        }
    }

    pub fn position(&self, now: Instant) -> i64 {
        if !self.playing {
            return self.anchor_ms;
        }
        self.anchor_ms + now.saturating_duration_since(self.anchor_at).as_millis() as i64
    }

    pub fn set_theme(&mut self, theme: Theme, now: Instant) {
        if theme == self.theme_to {
            return;
        }
        self.theme_from = self.theme(now);
        self.theme_to = theme;
        self.theme_at = Some(now);
    }

    pub fn theme(&self, now: Instant) -> Theme {
        let Some(at) = self.theme_at else { return self.theme_to };
        let t = (now.saturating_duration_since(at).as_secs_f32() / CROSSFADE.as_secs_f32()).min(1.0);
        let e = ease_out(t);
        let mix = |a: [f32; 3], b: [f32; 3]| [0, 1, 2].map(|i| a[i] + (b[i] - a[i]) * e);
        Theme { plate: mix(self.theme_from.plate, self.theme_to.plate), accent: mix(self.theme_from.accent, self.theme_to.accent) }
    }

    fn crossfading(&self, now: Instant) -> bool {
        self.theme_at.map_or(false, |at| now.saturating_duration_since(at) < CROSSFADE)
    }

    pub fn find_line_index(&self, ms: i64) -> i32 {
        let lines = &self.lines;
        if lines.is_empty() || ms < lines[0].start_ms {
            return -1;
        }
        (lines.partition_point(|l| l.start_ms <= ms) as i32) - 1
    }

    fn label(&self, index: i32) -> String {
        if index < 0 {
            return String::new();
        }
        match self.lines.get(index as usize) {
            Some(l) if l.instrumental => "• • •".into(),
            Some(l) => l.text.clone(),
            None => String::new(),
        }
    }
}

fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

/// cubic-bezier(0.16, 1, 0.3, 1), the CSS roll curve.
fn roll_ease(x: f32) -> f32 {
    let (x1, y1, x2, y2) = (0.16f32, 1.0f32, 0.3f32, 1.0f32);
    let bez = |t: f32, a: f32, b: f32| 3.0 * (1.0 - t).powi(2) * t * a + 3.0 * (1.0 - t) * t * t * b + t * t * t;
    let mut t = x;
    for _ in 0..8 {
        let xt = bez(t, x1, x2) - x;
        let d = 3.0 * (1.0 - t).powi(2) * x1 + 6.0 * (1.0 - t) * t * (x2 - x1) + 3.0 * t * t * (1.0 - x2);
        if d.abs() < 1e-5 {
            break;
        }
        t = (t - xt / d).clamp(0.0, 1.0);
    }
    bez(t, y1, y2)
}

fn clock(ms: i64) -> String {
    let s = (ms / 1000).max(0);
    format!("{}:{:02}", s / 60, s % 60)
}

fn color(rgb: [f32; 3], a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r: rgb[0], g: rgb[1], b: rgb[2], a }
}

fn white(a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r: 1.0, g: 1.0, b: 1.0, a }
}

fn rect(l: f32, t: f32, r: f32, b: f32) -> D2D_RECT_F {
    D2D_RECT_F { left: l, top: t, right: r, bottom: b }
}

fn v2(x: f32, y: f32) -> D2D_POINT_2F {
    D2D_POINT_2F { x, y }
}

enum Active {
    None,
    /// The sung line, lit whole. Its pixels live in `Focus::active_layer`.
    Line,
    Message { layout: IDWriteTextLayout, alpha: f32, breathe: bool },
}

struct Stack {
    index: i32,
    status: Status,
    active: Active,
    active_origin: D2D_POINT_2F,
}

/// One lyric line pre-rendered twice, sharp and blurred, in a bitmap sized to
/// the line. The roll crossfades between the two, which looks like the CSS
/// blur animation but costs two bitmap copies a frame instead of a blur.
struct LineLayer {
    sharp: ID2D1Bitmap1,
    soft: ID2D1Bitmap1,
    x: f32,
    y: f32,
}

/// Padding around a line layer so the blur has room to spread.
const LAYER_PAD: f32 = 12.0;

/// Roll progress from the CSS keyframes: the stack rises one step, the
/// arriving line "pulls" into focus (blur 3px -> 0, opacity .45 -> 1) and the
/// departing one "pushes" back (blur 0 -> 1.2px, opacity 1 -> .5).
struct Roll {
    dy: f32,
    /// 0 at the start of the roll, 1 at rest.
    x: f32,
}

#[derive(Default)]
pub struct Focus {
    brush: Option<ID2D1SolidColorBrush>,
    grain: Option<ID2D1BitmapBrush1>,
    blur: Option<[ID2D1Effect; 2]>,
    plate: Option<ID2D1Bitmap1>,
    plate_dirty: bool,
    lyrics: Option<ID2D1Bitmap1>,
    /// The sung line (sharp, and blurred 3px for the roll's pull).
    active_layer: Option<LineLayer>,
    /// The line that just finished (sharp, and blurred 1.2px at rest).
    prev_layer: Option<LineLayer>,
    /// Backing bitmaps for the two line layers, allocated once per size and
    /// reused for every line, so a line change allocates nothing.
    active_bmps: Option<[ID2D1Bitmap1; 2]>,
    prev_bmps: Option<[ID2D1Bitmap1; 2]>,
    scratch: Option<[ID2D1Bitmap1; 2]>,
    stack: Option<Stack>,
    stack_dirty: bool,
    clock: Option<(String, IDWriteTextLayout)>,
    clock_format: Option<IDWriteTextFormat>,
    roll_start: Option<Instant>,
    /// Playhead position of the last frame drawn. Scheduling looks for the
    /// next change *after this*, so a wake-up that lands a few ms late (the
    /// system timer is ~15.6ms coarse) draws at once instead of skipping it.
    drawn_ms: Option<i64>,
    pub cover: Option<ID2D1Bitmap1>,
    /// The app's mark, shown in the cover slot while nothing plays.
    logo: Option<ID2D1Bitmap1>,
    hover: bool,
    hover_close: bool,
    /// When the pointer last entered or left the card, and the close
    /// button's opacity at that moment, for its 160ms fade.
    hover_at: Option<Instant>,
    hover_from: f32,
    /// First frame drawn, for the card's fade-in.
    appear_at: Option<Instant>,
}

pub enum Wake {
    /// Something is moving: draw at the next vsync.
    Continuous,
    /// Nothing moves until then.
    At(Instant),
    Never,
}

impl Focus {
    pub fn new() -> Self {
        Focus { plate_dirty: true, stack_dirty: true, ..Default::default() }
    }

    /// Size, DPI, device or text-scale change: rebuild everything.
    pub fn invalidate_all(&mut self) {
        self.plate = None;
        self.lyrics = None;
        self.active_layer = None;
        self.prev_layer = None;
        self.active_bmps = None;
        self.prev_bmps = None;
        self.scratch = None;
        self.clock = None;
        self.clock_format = None;
        self.plate_dirty = true;
        self.stack_dirty = true;
    }

    /// Device lost: drop every GPU resource.
    pub fn reset_device(&mut self) {
        self.invalidate_all();
        self.brush = None;
        self.grain = None;
        self.blur = None;
        self.cover = None;
        self.logo = None;
    }

    pub fn invalidate_plate(&mut self) {
        self.plate_dirty = true;
    }

    pub fn invalidate_stack(&mut self) {
        self.stack_dirty = true;
    }

    /// Update hover state; returns whether anything visible changed.
    pub fn set_hover(&mut self, hover: bool, over_close: bool, now: Instant) -> bool {
        let changed = hover != self.hover || over_close != self.hover_close;
        if hover != self.hover {
            self.hover_from = self.close_alpha(now);
            self.hover_at = Some(now);
            self.hover = hover;
        }
        self.hover_close = over_close;
        changed
    }

    fn close_alpha(&self, now: Instant) -> f32 {
        let target = if self.hover { 1.0 } else { 0.0 };
        match self.hover_at {
            Some(t) => {
                let x = (now.saturating_duration_since(t).as_secs_f32() / CLOSE_FADE.as_secs_f32()).min(1.0);
                self.hover_from + (target - self.hover_from) * ease_out(x)
            }
            None => target,
        }
    }

    pub fn close_rect(w: f32) -> D2D_RECT_F {
        rect(w - 44.0, PAD_TOP, w - 20.0, PAD_TOP + 24.0)
    }

    // ------------------------------------------------------------ schedule --

    pub fn next_wake(&self, m: &Model, now: Instant, bar_px: f32) -> Wake {
        if self.roll_start.map_or(false, |t| now.saturating_duration_since(t) < ROLL) || m.crossfading(now) {
            return Wake::Continuous;
        }
        if self.appear_at.map_or(true, |t| now.saturating_duration_since(t) < APPEAR)
            || self.hover_at.map_or(false, |t| now.saturating_duration_since(t) < CLOSE_FADE)
        {
            return Wake::Continuous;
        }
        if matches!(self.stack.as_ref().map(|s| &s.active), Some(Active::Message { breathe: true, .. })) {
            return Wake::Continuous;
        }
        if !m.playing {
            return Wake::Never;
        }
        let ms = m.position(now) as f64;
        let Some(drawn) = self.drawn_ms else { return Wake::Continuous };
        let drawn = drawn as f64;
        // The first visible change after the last frame drawn.
        let mut next = f64::INFINITY;
        if m.status == Status::Ready {
            // Whole-line highlighting: the lyric only changes when the next
            // line starts, so between lines there is nothing to animate.
            let Some(stack) = &self.stack else { return Wake::Continuous };
            if let Some(l) = m.lines.get((stack.index + 1).max(0) as usize) {
                next = next.min(l.start_ms as f64);
            }
        }
        if m.duration_ms > 0 || m.status == Status::Ready {
            next = next.min((drawn / 1000.0).floor() * 1000.0 + 1000.0); // clock
        }
        if m.duration_ms > 0 && bar_px > 0.0 {
            let per_px = m.duration_ms as f64 / bar_px as f64;
            next = next.min(((drawn / per_px).floor() + 1.0) * per_px);
        }
        if !next.is_finite() {
            return Wake::Never;
        }
        if next <= ms {
            return Wake::Continuous; // already due: draw now
        }
        Wake::At(now + Duration::from_micros(((next - ms) * 1000.0) as u64))
    }

    // ---------------------------------------------------------------- draw --

    pub fn render(&mut self, gpu: &Gpu, text: &Text, m: &Model, now: Instant) -> Result<()> {
        let ctx = &gpu.ctx;
        let (w, h) = gpu.size_dips();
        let ms = m.position(now);
        unsafe {
            if self.brush.is_none() {
                self.brush = Some(ctx.CreateSolidColorBrush(&white(1.0), None)?);
            }
            if self.grain.is_none() {
                self.grain = Some(make_grain(gpu)?);
            }
            if self.blur.is_none() {
                self.blur = Some([ctx.CreateEffect(&CLSID_D2D1GaussianBlur)?, ctx.CreateEffect(&CLSID_D2D1GaussianBlur)?]);
            }
            if m.crossfading(now) {
                self.plate_dirty = true;
            }
            if self.plate_dirty || self.plate.is_none() {
                self.build_plate(gpu, text, m, now, w, h)?;
            }
            let index = if m.status == Status::Ready { m.find_line_index(ms) } else { -1 };
            let rebuild = self.stack_dirty
                || self.stack.as_ref().map_or(true, |s| s.index != index || s.status != m.status);
            if rebuild {
                let previous = self.stack.as_ref().map(|s| (s.index, s.status));
                self.build_stack(gpu, text, m, index, w, h)?;
                if m.status == Status::Ready && matches!(previous, Some((p, Status::Ready)) if p != index) {
                    self.roll_start = Some(now);
                }
            }

            let brush = self.brush.clone().unwrap();
            // Card-in: the card fades in and rises 8px when the app starts
            // (the `card-in` keyframes in web/style.css).
            let appear_at = *self.appear_at.get_or_insert(now);
            let appear = roll_ease((now.saturating_duration_since(appear_at).as_secs_f32() / APPEAR.as_secs_f32()).min(1.0));
            let base_dy = 8.0 * (1.0 - appear);
            ctx.BeginDraw();
            ctx.Clear(Some(&white(0.0)));
            let fading_in = appear < 1.0;
            if fading_in {
                let params = D2D1_LAYER_PARAMETERS1 {
                    contentBounds: rect(-1.0e6, -1.0e6, 1.0e6, 1.0e6),
                    geometricMask: std::mem::ManuallyDrop::new(None),
                    maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
                    maskTransform: Matrix3x2::identity(),
                    opacity: appear,
                    opacityBrush: std::mem::ManuallyDrop::new(None),
                    layerOptions: D2D1_LAYER_OPTIONS1_NONE,
                };
                ctx.PushLayer(&params, None);
            }
            ctx.SetTransform(&Matrix3x2::translation(0.0, base_dy));
            if let Some(p) = &self.plate {
                ctx.DrawBitmap(p, None, 1.0, D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR, None, None);
            }

            // Roll: the whole stack rises one step, the arriving line pulls
            // into focus and the departing one softens (web/style.css keyframes).
            let roll = match self.roll_start {
                Some(t) if now.saturating_duration_since(t) < ROLL => {
                    let x = roll_ease(now.saturating_duration_since(t).as_secs_f32() / ROLL.as_secs_f32());
                    Roll { dy: lyric_size(m, w) * 1.1 * (1.0 - x), x }
                }
                _ => Roll { dy: 0.0, x: 1.0 },
            };
            ctx.SetTransform(&Matrix3x2::translation(0.0, roll.dy + base_dy));
            if let Some(l) = &self.lyrics {
                ctx.DrawBitmap(l, None, 1.0, D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR, None, None);
            }
            // Push: the departing line goes from sharp at full strength to
            // blurred at half, as a crossfade of its two pre-rendered states.
            if let Some(prev) = &self.prev_layer {
                draw_layer(ctx, &prev.sharp, prev.x, prev.y, 1.0 - roll.x);
                draw_layer(ctx, &prev.soft, prev.x, prev.y, 0.5 * roll.x);
            }
            if let Some(stack) = &self.stack {
                let o = stack.active_origin;
                match &stack.active {
                    Active::Line => {
                        // Pull: blurred at .45 into sharp at full strength.
                        if let Some(a) = &self.active_layer {
                            let strength = 0.45 + 0.55 * roll.x;
                            if roll.x < 1.0 {
                                draw_layer(ctx, &a.soft, a.x, a.y, strength * (1.0 - roll.x));
                            }
                            draw_layer(ctx, &a.sharp, a.x, a.y, strength * roll.x);
                        }
                    }
                    Active::Message { layout, alpha, breathe } => {
                        let a = if *breathe {
                            let phase = (instant_ms(now) % BREATHE_MS) / BREATHE_MS;
                            0.35 + 0.65 * (0.5 - 0.5 * (phase * std::f32::consts::TAU).cos())
                        } else {
                            1.0
                        };
                        brush.SetColor(&white(alpha * a));
                        ctx.DrawTextLayout(o, layout, &brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
                    }
                    Active::None => {}
                }
            }
            ctx.SetTransform(&Matrix3x2::translation(0.0, base_dy));

            // Clock, right-aligned before the close button: elapsed / total.
            if m.duration_ms > 0 || m.status == Status::Ready {
                let label = if m.duration_ms > 0 { format!("{} / {}", clock(ms), clock(m.duration_ms)) } else { clock(ms) };
                if self.clock.as_ref().map_or(true, |(s, _)| *s != label) {
                    if self.clock_format.is_none() {
                        let f = text.format(12.5, 400.0)?;
                        f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_TRAILING)?;
                        self.clock_format = Some(f);
                    }
                    let layout = text.layout(&label, self.clock_format.as_ref().unwrap(), CLOCK_W, 24.0, false)?;
                    text.tabular(&layout, label.encode_utf16().count());
                    self.clock = Some((label, layout));
                }
                if let Some((_, layout)) = &self.clock {
                    let mut tm = DWRITE_TEXT_METRICS::default();
                    layout.GetMetrics(&mut tm)?;
                    brush.SetColor(&white(0.5));
                    let right = w - PAD_X - 18.0 - 10.0;
                    ctx.DrawTextLayout(v2(right - CLOCK_W, PAD_TOP + HEADER_H / 2.0 - tm.height / 2.0), layout, &brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
                }
            }

            // Progress: a hairline under the title row, across the content width.
            let (x0, x1) = (PAD_X, w - PAD_X);
            let y = PAD_TOP + HEADER_H + BAR_GAP;
            let track = D2D1_ROUNDED_RECT { rect: rect(x0, y, x1, y + BAR_H), radiusX: BAR_H / 2.0, radiusY: BAR_H / 2.0 };
            brush.SetColor(&white(0.1));
            ctx.FillRoundedRectangle(&track, &brush);
            if m.duration_ms > 0 {
                let ratio = (ms as f32 / m.duration_ms as f32).clamp(0.0, 1.0);
                if ratio > 0.0 {
                    let theme = m.theme(now);
                    brush.SetColor(&color(theme.accent, 1.0));
                    ctx.FillRectangle(&rect(x0, y, x0 + (x1 - x0) * ratio, y + BAR_H), &brush);
                }
            }

            // Close button: fades in over 160ms while the pointer is over the card.
            let close_alpha = self.close_alpha(now);
            if close_alpha > 0.003 {
                let r = Self::close_rect(w);
                let ink = if self.hover_close {
                    brush.SetColor(&D2D1_COLOR_F { r: 1.0, g: 86.0 / 255.0, b: 86.0 / 255.0, a: 0.22 * close_alpha });
                    ctx.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: r, radiusX: 7.0, radiusY: 7.0 }, &brush);
                    D2D1_COLOR_F { r: 1.0, g: 155.0 / 255.0, b: 155.0 / 255.0, a: close_alpha }
                } else {
                    white(0.55 * close_alpha)
                };
                brush.SetColor(&ink);
                let (cx, cy, d) = ((r.left + r.right) / 2.0, (r.top + r.bottom) / 2.0, 2.9);
                ctx.DrawLine(v2(cx - d, cy - d), v2(cx + d, cy + d), &brush, 1.4, None);
                ctx.DrawLine(v2(cx + d, cy - d), v2(cx - d, cy + d), &brush, 1.4, None);
            }
            ctx.SetTransform(&Matrix3x2::identity());
            if fading_in {
                ctx.PopLayer();
            }
            ctx.EndDraw(None, None)?;
            self.drawn_ms = Some(ms);
            Ok(())
        }
    }

    unsafe fn build_plate(&mut self, gpu: &Gpu, text: &Text, m: &Model, now: Instant, w: f32, h: f32) -> Result<()> {
        let ctx = &gpu.ctx;
        if self.plate.is_none() {
            self.plate = Some(gpu.layer_bitmap()?);
        }
        let brush = self.brush.clone().unwrap();
        ctx.SetTarget(self.plate.as_ref().unwrap());
        ctx.BeginDraw();
        ctx.Clear(Some(&white(0.0)));
        ctx.SetTransform(&Matrix3x2::identity());

        let theme = m.theme(now);
        let card = D2D1_ROUNDED_RECT { rect: rect(0.0, 0.0, w, h), radiusX: RADIUS, radiusY: RADIUS };
        // --focus-plate: the album hue pushed 16% further into the plate.
        let fp = [0, 1, 2].map(|i| theme.accent[i] * 0.16 + theme.plate[i] * 0.84);
        let stops = [
            D2D1_GRADIENT_STOP { position: 0.0, color: color(fp, m.plate_core) },
            D2D1_GRADIENT_STOP { position: 0.5, color: color(fp, m.plate_core) },
            D2D1_GRADIENT_STOP { position: 1.0, color: color(fp, m.plate_edge) },
        ];
        let collection = ctx.CreateGradientStopCollection(&stops, D2D1_COLOR_SPACE_SRGB, D2D1_COLOR_SPACE_SRGB, D2D1_BUFFER_PRECISION_8BPC_UNORM, D2D1_EXTEND_MODE_CLAMP, D2D1_COLOR_INTERPOLATION_MODE_PREMULTIPLIED)?;
        let radial = ctx.CreateRadialGradientBrush(
            &D2D1_RADIAL_GRADIENT_BRUSH_PROPERTIES { center: v2(w / 2.0, h / 2.0), gradientOriginOffset: v2(0.0, 0.0), radiusX: w * 1.2, radiusY: h },
            None,
            &collection,
        )?;
        ctx.FillRoundedRectangle(&card, &radial);
        if let Some(g) = &self.grain {
            ctx.FillRoundedRectangle(&card, g);
        }
        // Vignette.
        let vstops = [
            D2D1_GRADIENT_STOP { position: 0.45, color: D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 0.0 } },
            D2D1_GRADIENT_STOP { position: 1.0, color: D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 0.36 } },
        ];
        let vcol = ctx.CreateGradientStopCollection(&vstops, D2D1_COLOR_SPACE_SRGB, D2D1_COLOR_SPACE_SRGB, D2D1_BUFFER_PRECISION_8BPC_UNORM, D2D1_EXTEND_MODE_CLAMP, D2D1_COLOR_INTERPOLATION_MODE_PREMULTIPLIED)?;
        let vignette = ctx.CreateRadialGradientBrush(
            &D2D1_RADIAL_GRADIENT_BRUSH_PROPERTIES { center: v2(w * 0.3, h * 0.45), gradientOriginOffset: v2(0.0, 0.0), radiusX: w * 1.2, radiusY: h * 0.95 },
            None,
            &vcol,
        )?;
        ctx.FillRoundedRectangle(&card, &vignette);
        brush.SetColor(&white(0.07));
        let hair = D2D1_ROUNDED_RECT { rect: rect(0.5, 0.5, w - 0.5, h - 0.5), radiusX: RADIUS - 0.5, radiusY: RADIUS - 0.5 };
        ctx.DrawRoundedRectangle(&hair, &brush, 1.0, None);

        // Header: cover, title, artist.
        let cy = PAD_TOP + HEADER_H / 2.0;
        let cover_r = rect(PAD_X, cy - 11.0, PAD_X + 22.0, cy + 11.0);
        let cover_rr = D2D1_ROUNDED_RECT { rect: cover_r, radiusX: 5.0, radiusY: 5.0 };
        // Nothing playing: the app's mark sits where the cover goes, beside
        // the app's name.
        let show_logo = m.status == Status::Idle && self.cover.is_none();
        if show_logo && self.logo.is_none() {
            self.logo = crate::art::decode(crate::icon_art::largest(), false).and_then(|d| cover_bitmap(gpu, &d.cover).ok());
        }
        if let (true, Some(logo)) = (show_logo, &self.logo) {
            ctx.DrawBitmap(logo, Some(&cover_r), 1.0, D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC, None, None);
        } else {
            let base = [0, 1, 2].map(|i| theme.accent[i] * 0.3 + [0x15 as f32, 0x16 as f32, 0x1b as f32][i] / 255.0 * 0.7);
            brush.SetColor(&color(base, 1.0));
            ctx.FillRoundedRectangle(&cover_rr, &brush);
            if let Some(cover) = &self.cover {
                let bb = ctx.CreateBitmapBrush(
                    cover,
                    Some(&D2D1_BITMAP_BRUSH_PROPERTIES1 {
                        extendModeX: D2D1_EXTEND_MODE_CLAMP,
                        extendModeY: D2D1_EXTEND_MODE_CLAMP,
                        interpolationMode: D2D1_INTERPOLATION_MODE_LINEAR,
                    }),
                    None,
                )?;
                let size = cover.GetSize();
                let s = 22.0 / size.width.max(1.0);
                bb.SetTransform(&(Matrix3x2 { M11: s, M12: 0.0, M21: 0.0, M22: s, M31: cover_r.left, M32: cover_r.top }));
                ctx.FillRoundedRectangle(&cover_rr, &bb);
            }
            brush.SetColor(&white(0.08));
            ctx.DrawRoundedRectangle(&cover_rr, &brush, 1.0, None);
        }

        let title = if m.title.is_empty() && m.status == Status::Idle { "Lyrics UpFront".to_string() } else { m.title.clone() };
        let tf = text.format(12.5, 650.0)?;
        let af = text.format(12.5, 400.0)?;
        let left = PAD_X + 22.0 + 10.0;
        let right = w - PAD_X - 18.0 - 10.0 - CLOCK_W - 10.0; // leave room for the clock
        let avail = (right - left).max(10.0);
        let natural = |s: &str, f: &IDWriteTextFormat| -> Result<f32> {
            let l = text.layout(s, f, 10_000.0, 30.0, false)?;
            let mut tm = DWRITE_TEXT_METRICS::default();
            l.GetMetrics(&mut tm)?;
            Ok(tm.widthIncludingTrailingWhitespace)
        };
        let (tw, aw) = (natural(&title, &tf)?, if m.artist.is_empty() { 0.0 } else { natural(&m.artist, &af)? });
        let gap = if aw > 0.0 { 10.0 } else { 0.0 };
        let scale = if tw + gap + aw > avail { (avail - gap) / (tw + aw).max(1.0) } else { 1.0 };
        let tl = text.layout(&title, &tf, (tw * scale).max(1.0), 30.0, false)?;
        let mut tm = DWRITE_TEXT_METRICS::default();
        tl.GetMetrics(&mut tm)?;
        brush.SetColor(&white(1.0));
        ctx.DrawTextLayout(v2(left, cy - tm.height / 2.0), &tl, &brush, D2D1_DRAW_TEXT_OPTIONS_CLIP);
        if aw > 0.0 {
            let al = text.layout(&m.artist, &af, (aw * scale).max(1.0), 30.0, false)?;
            brush.SetColor(&white(0.5));
            ctx.DrawTextLayout(v2(left + tw * scale + gap, cy - tm.height / 2.0), &al, &brush, D2D1_DRAW_TEXT_OPTIONS_CLIP);
        }
        let r = ctx.EndDraw(None, None);
        gpu.restore_target();
        self.plate_dirty = false;
        r
    }

    unsafe fn build_stack(&mut self, gpu: &Gpu, text: &Text, m: &Model, index: i32, w: f32, h: f32) -> Result<()> {
        let ctx = &gpu.ctx;
        let size = lyric_size(m, w);
        let (sx, sw) = (PAD_X, (w - 2.0 * PAD_X).max(10.0));
        // The stage starts below the progress hairline.
        let (stage_top, stage_bottom) = (PAD_TOP + HEADER_H + BAR_GAP + BAR_H, h - PAD_BOTTOM);

        // What each slot shows, per status (mirrors buildFocus in app.js).
        let mut slots = [String::new(), String::new(), String::new(), String::new()]; // -2,-1,+1,+2
        let mut message: Option<(String, f32, bool, f32)> = None; // text, alpha, breathe, weight
        let mut line = String::new(); // the sung line, lit whole
        match m.status {
            Status::Idle => message = Some(("Waiting for Spotify".into(), 0.46, false, 650.0)),
            Status::Loading => message = Some(("• • •".into(), 0.5, true, 760.0)),
            Status::NoLyrics => message = Some(("No lyrics for this one".into(), 0.46, false, 650.0)),
            Status::Offline => message = Some(("Can\u{2019}t reach lyrics, retrying\u{2026}".into(), 0.46, false, 650.0)),
            Status::Unsynced => {
                message = Some((m.label(0), 1.0, false, 760.0));
                slots[2] = m.label(1);
                slots[3] = m.label(2);
            }
            Status::Ready => {
                slots = [m.label(index - 2), m.label(index - 1), m.label(index + 1), m.label(index + 2)];
                if index < 0 {
                    // Before the first line: a quiet lead-in marker.
                    message = Some(("\u{2022} \u{2022} \u{2022}".into(), 0.46, false, 650.0));
                } else {
                    line = m.label(index);
                }
            }
        }

        // Active line layout: up to two lines, ellipsis after.
        let line_h = size * 1.14;
        let af = text.format(size, message.as_ref().map_or(760.0, |m| m.3))?;
        af.SetLineSpacing(DWRITE_LINE_SPACING_METHOD_UNIFORM, line_h, line_h * 0.8)?;
        let active_text: String = match &message {
            Some((t, ..)) => t.clone(),
            None => line.clone(),
        };
        let layout = text.layout(&active_text, &af, sw, line_h * 2.0 + 1.0, true)?;
        Text::letter_spacing(&layout, active_text.encode_utf16().count(), -0.026 * size);
        let mut tm = DWRITE_TEXT_METRICS::default();
        layout.GetMetrics(&mut tm)?;
        let active_lines = tm.lineCount.clamp(1, 2) as f32;

        // Vertical stack, centred in the stage like the flex column.
        let far_f = size * 0.63;
        let near_f = size * 0.73;
        let gap = size * 0.27;
        let heights = [far_f * 1.2, near_f * 1.2, (active_lines * line_h).max(size * 1.2) + 2.0, near_f * 1.2, far_f * 1.2];
        let total: f32 = heights.iter().sum::<f32>() + gap * 4.0;
        let mut y = stage_top + ((stage_bottom - stage_top) - total) / 2.0;
        let mut tops = [0.0f32; 5];
        for (i, hgt) in heights.iter().enumerate() {
            tops[i] = y;
            y += hgt + gap;
        }

        // Neighbours: drawn once into two scratch layers, blurred into one.
        if self.lyrics.is_none() {
            self.lyrics = Some(gpu.layer_bitmap()?);
        }
        if self.scratch.is_none() {
            self.scratch = Some([gpu.layer_bitmap()?, gpu.layer_bitmap()?]);
        }
        let brush = self.brush.clone().unwrap();
        let scratch = self.scratch.clone().unwrap();
        let far = text.format(far_f, 700.0)?;
        let near = text.format(near_f, 700.0)?;
        // Slot -1 (the line that just finished) is left out of the cached
        // layer: it gets its own bitmap so the roll can soften it per frame.
        let groups: [(&ID2D1Bitmap1, &IDWriteTextFormat, f32, &[(usize, usize)]); 2] = [
            (&scratch[0], &far, 0.2, &[(0, 0), (3, 4)]),
            (&scratch[1], &near, 0.5, &[(2, 3)]),
        ];
        for (target, format, alpha, entries) in groups {
            ctx.SetTarget(target);
            ctx.BeginDraw();
            ctx.Clear(Some(&white(0.0)));
            ctx.SetTransform(&Matrix3x2::identity());
            brush.SetColor(&white(alpha));
            for &(slot, top_i) in entries {
                let s = &slots[slot];
                if s.is_empty() {
                    continue;
                }
                let l = text.layout(s, format, sw, size * 2.0, false)?;
                Text::letter_spacing(&l, s.encode_utf16().count(), -0.018 * format.GetFontSize());
                ctx.DrawTextLayout(v2(sx, tops[top_i]), &l, &brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
            }
            ctx.EndDraw(None, None)?;
        }

        // The departing line (slot -1) and the sung line, each pre-rendered
        // sharp and blurred so the roll is a crossfade, not a live blur.
        let blur_fx = self.blur.clone().unwrap();
        let pad2 = 2.0 * LAYER_PAD;
        if self.prev_bmps.is_none() {
            let (bw, bh) = (sw + pad2, near_f * 1.6 + pad2);
            self.prev_bmps = Some([gpu.bitmap(bw, bh)?, gpu.bitmap(bw, bh)?]);
        }
        if self.active_bmps.is_none() {
            let (bw, bh) = (sw + pad2, line_h * 2.0 + 4.0 + pad2);
            self.active_bmps = Some([gpu.bitmap(bw, bh)?, gpu.bitmap(bw, bh)?]);
        }
        self.prev_layer = None;
        if !slots[1].is_empty() {
            let l = text.layout(&slots[1], &near, sw, near_f * 1.6, false)?;
            Text::letter_spacing(&l, slots[1].encode_utf16().count(), -0.018 * near_f);
            let bmps = self.prev_bmps.clone().unwrap();
            self.prev_layer = Some(line_layer(gpu, &brush, &blur_fx[0], &l, v2(sx, tops[1]), bmps, 1.2)?);
        }
        let active_origin = v2(sx, tops[2] + 2.0);
        self.active_layer = None;
        if message.is_none() && !line.is_empty() {
            let bmps = self.active_bmps.clone().unwrap();
            self.active_layer = Some(line_layer(gpu, &brush, &blur_fx[0], &layout, active_origin, bmps, 3.0)?);
        }
        let blur = self.blur.clone().unwrap();
        ctx.SetTarget(self.lyrics.as_ref().unwrap());
        ctx.BeginDraw();
        ctx.Clear(Some(&white(0.0)));
        ctx.SetTransform(&Matrix3x2::identity());
        // Top and bottom of the stage fade out, as the CSS mask does.
        let fade = [
            D2D1_GRADIENT_STOP { position: 0.0, color: white(0.0) },
            D2D1_GRADIENT_STOP { position: 0.16, color: white(1.0) },
            D2D1_GRADIENT_STOP { position: 0.84, color: white(1.0) },
            D2D1_GRADIENT_STOP { position: 1.0, color: white(0.0) },
        ];
        let fc = ctx.CreateGradientStopCollection(&fade, D2D1_COLOR_SPACE_SRGB, D2D1_COLOR_SPACE_SRGB, D2D1_BUFFER_PRECISION_8BPC_UNORM, D2D1_EXTEND_MODE_CLAMP, D2D1_COLOR_INTERPOLATION_MODE_PREMULTIPLIED)?;
        let mask = ctx.CreateLinearGradientBrush(
            &D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES { startPoint: v2(0.0, stage_top), endPoint: v2(0.0, stage_bottom) },
            None,
            &fc,
        )?;
        let params = D2D1_LAYER_PARAMETERS1 {
            contentBounds: rect(0.0, stage_top, w, stage_bottom),
            geometricMask: std::mem::ManuallyDrop::new(None),
            maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
            maskTransform: Matrix3x2::identity(),
            opacity: 1.0,
            opacityBrush: std::mem::ManuallyDrop::new(Some(mask.clone().into())),
            layerOptions: D2D1_LAYER_OPTIONS1_NONE,
        };
        ctx.PushLayer(&params, None);
        for (i, (sd, src)) in [(2.4f32, &scratch[0]), (1.2f32, &scratch[1])].into_iter().enumerate() {
            let fx = &blur[i];
            fx.SetInput(0, src, true);
            fx.SetValue(D2D1_GAUSSIANBLUR_PROP_STANDARD_DEVIATION.0 as u32, D2D1_PROPERTY_TYPE_FLOAT, &sd.to_le_bytes())?;
            let out = fx.GetOutput()?;
            ctx.DrawImage(&out, None, None, D2D1_INTERPOLATION_MODE_LINEAR, D2D1_COMPOSITE_MODE_SOURCE_OVER);
        }
        ctx.PopLayer();
        let r = ctx.EndDraw(None, None);
        std::mem::ManuallyDrop::into_inner(params.opacityBrush);
        gpu.restore_target();
        r?;

        let active = if let Some((_, alpha, breathe, _)) = message {
            Active::Message { layout, alpha, breathe }
        } else if line.is_empty() {
            Active::None
        } else {
            Active::Line
        };
        self.stack = Some(Stack { index, status: m.status, active, active_origin });
        self.stack_dirty = false;
        Ok(())
    }
}

/// Copy a line layer to the target at (x, y) with an opacity; skips invisible draws.
unsafe fn draw_layer(ctx: &ID2D1DeviceContext, bmp: &ID2D1Bitmap1, x: f32, y: f32, opacity: f32) {
    if opacity <= 0.003 {
        return;
    }
    let size = bmp.GetSize();
    let dest = rect(x, y, x + size.width, y + size.height);
    ctx.DrawBitmap(bmp, Some(&dest), opacity.min(1.0), D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR, None, None);
}

/// Pre-render one line into a reused pair of bitmaps: sharp, and blurred by
/// `std_dev`. `pos` is where the text sits on the card; the bitmaps carry
/// LAYER_PAD of room on every side for the blur to spread.
unsafe fn line_layer(
    gpu: &Gpu,
    brush: &ID2D1SolidColorBrush,
    blur: &ID2D1Effect,
    layout: &IDWriteTextLayout,
    pos: D2D_POINT_2F,
    bitmaps: [ID2D1Bitmap1; 2],
    std_dev: f32,
) -> Result<LineLayer> {
    let ctx = &gpu.ctx;
    let [sharp, soft] = bitmaps;
    ctx.SetTarget(&sharp);
    ctx.BeginDraw();
    ctx.Clear(Some(&white(0.0)));
    ctx.SetTransform(&Matrix3x2::identity());
    brush.SetColor(&white(1.0));
    ctx.DrawTextLayout(v2(LAYER_PAD, LAYER_PAD), layout, brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
    ctx.EndDraw(None, None)?;
    ctx.SetTarget(&soft);
    ctx.BeginDraw();
    ctx.Clear(Some(&white(0.0)));
    ctx.SetTransform(&Matrix3x2::identity());
    blur.SetInput(0, &sharp, true);
    blur.SetValue(D2D1_GAUSSIANBLUR_PROP_STANDARD_DEVIATION.0 as u32, D2D1_PROPERTY_TYPE_FLOAT, &std_dev.to_le_bytes())?;
    ctx.DrawImage(&blur.GetOutput()?, None, None, D2D1_INTERPOLATION_MODE_LINEAR, D2D1_COMPOSITE_MODE_SOURCE_OVER);
    ctx.EndDraw(None, None)?;
    Ok(LineLayer { sharp, soft, x: pos.x - LAYER_PAD, y: pos.y - LAYER_PAD })
}

fn lyric_size(m: &Model, w: f32) -> f32 {
    (w * 0.05 * m.text_scale).clamp(16.0, 64.0)
}

fn instant_ms(now: Instant) -> f32 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let start = *START.get_or_init(Instant::now);
    now.saturating_duration_since(start).as_secs_f32() * 1000.0
}

/// Film grain: a small noise tile, tiled across the plate at low opacity.
unsafe fn make_grain(gpu: &Gpu) -> Result<ID2D1BitmapBrush1> {
    const N: u32 = 128;
    let mut seed = 0x2545F491u32;
    let mut px = Vec::with_capacity((N * N * 4) as usize);
    for _ in 0..N * N {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        let light = seed & 1 == 0;
        let a = ((seed >> 8) % 12) as u8; // up to ~4.7% opacity
        let v = if light { a } else { 0 }; // premultiplied
        px.extend_from_slice(&[v, v, v, a]);
    }
    let props = bitmap_props(96.0, D2D1_BITMAP_OPTIONS_NONE);
    let bitmap = gpu.ctx.CreateBitmap(D2D_SIZE_U { width: N, height: N }, Some(px.as_ptr() as _), N * 4, &props)?;
    gpu.ctx.CreateBitmapBrush(
        &bitmap,
        Some(&D2D1_BITMAP_BRUSH_PROPERTIES1 {
            extendModeX: D2D1_EXTEND_MODE_WRAP,
            extendModeY: D2D1_EXTEND_MODE_WRAP,
            interpolationMode: D2D1_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
        }),
        None,
    )
}

/// Upload a decoded cover (premultiplied BGRA) as a bitmap.
pub fn cover_bitmap(gpu: &Gpu, cover: &crate::art::Cover) -> Result<ID2D1Bitmap1> {
    unsafe {
        let props = bitmap_props(96.0, D2D1_BITMAP_OPTIONS_NONE);
        gpu.ctx.CreateBitmap(
            D2D_SIZE_U { width: cover.width, height: cover.height },
            Some(cover.pbgra.as_ptr() as _),
            cover.width * 4,
            &props,
        )
    }
}
