//! Draws video frames like render.ts createFrameRenderer: the map with
//! territory and borders, unit and structure sprites, name labels and a
//! progress bar.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

use tiny_skia::{
    Color, FillRule, FilterQuality, LineJoin, Paint, PathBuilder, Pixmap, PixmapPaint, Rect, Stroke, Transform,
};

use crate::assets;
use crate::look::{channels, mix, rgba, Settings, Style, GRAY};

#[derive(Default)]
struct FxHasher(u64);

impl Hasher for FxHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut h = self.0;
        for &b in bytes {
            h = (h.rotate_left(5) ^ (b as u64)).wrapping_mul(0x517cc1b727220a95);
        }
        self.0 = h;
    }
    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.0 = (self.0.rotate_left(5) ^ (i as u64)).wrapping_mul(0x517cc1b727220a95);
    }
    #[inline]
    fn write_u16(&mut self, i: u16) {
        self.0 = (self.0.rotate_left(5) ^ (i as u64)).wrapping_mul(0x517cc1b727220a95);
    }
    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.0 = (self.0.rotate_left(5) ^ (i as u64)).wrapping_mul(0x517cc1b727220a95);
    }
    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.0 = (self.0.rotate_left(5) ^ i).wrapping_mul(0x517cc1b727220a95);
    }
    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.write_u64(i as u64);
    }
    #[inline]
    fn write_i32(&mut self, i: i32) {
        self.write_u32(i as u32);
    }
}

type FxHashMap<K, V> = HashMap<K, V, BuildHasherDefault<FxHasher>>;

/// A name label: the engine's placement and the text to draw.
pub struct Label {
    pub owner: u16,
    pub name: String,
    pub troops: String,
    pub x: f64,
    pub y: f64,
    pub size: f64,
    /// Status icon bits (engine_rs::game::STATUS_*).
    pub status: u8,
}

/// Status icons in the client's order (StatusIconProgram slots) with their
/// atlas cells: crown, traitor, disconnected, nuke (white), doomsday skull.
const STATUS_ICONS: [(u8, u32); 5] = [
    (engine_rs::game::STATUS_CROWN, 0),
    (engine_rs::game::STATUS_TRAITOR, 1),
    (engine_rs::game::STATUS_DISCONNECTED, 2),
    (engine_rs::game::STATUS_NUKE, 8),
    (engine_rs::game::STATUS_DOOMSDAY, 10),
];
const STATUS_CELL: u32 = 256;
const STATUS_COLS: u32 = 3;
const STATUS_PAD: u32 = 16;

/// Halves an image with a 2x2 box filter (premultiplied, so averaging is exact enough).
fn halve(src: &Pixmap) -> Pixmap {
    let (w, h) = ((src.width() / 2).max(1), (src.height() / 2).max(1));
    let mut out = Pixmap::new(w, h).unwrap();
    let s = src.data();
    let sw = src.width() as usize;
    let o = out.data_mut();
    for y in 0..h as usize {
        for x in 0..w as usize {
            for c in 0..4 {
                let at = |dx: usize, dy: usize| s[((2 * y + dy) * sw + 2 * x + dx) * 4 + c] as u32;
                o[(y * w as usize + x) * 4 + c] = ((at(0, 0) + at(1, 0) + at(0, 1) + at(1, 1) + 2) / 4) as u8;
            }
        }
    }
    out
}

/// Unit codes in the units array: unit atlas columns 0-11, structures 16-21.
const STRUCTURE_CODE: i32 = 16;
const UNIT_CELL: u32 = 13;
// Structure shapes (StructurePass + structure.frag): sides (0 = circle) and rotation.
const STRUCTURE_SIDES: [u32; 6] = [0, 5, 6, 8, 4, 3];
const STRUCTURE_ROT: [f32; 6] = [0.0, std::f32::consts::FRAC_PI_2, std::f32::consts::FRAC_PI_6, 0.0, 0.0, std::f32::consts::FRAC_PI_2];
/// The scaled map is kept up to date in BLOCK x BLOCK pixel blocks.
const BLOCK: usize = 32;

fn is_missile(code: i32) -> bool {
    (3..=8).contains(&code)
}

/// Names this small aren't drawn (the renderer checks em < 6 video units,
/// which doesn't depend on the video width).
pub fn name_is_readable(size: f64, s: &Settings, map_w: u32) -> bool {
    let base = size.floor().max(1.0);
    let name_size = (base * s.name_scale_factor).floor().max(4.0);
    let name_scale = (base * 0.25).min(s.name_scale_cap);
    name_size * name_scale * 1280.0 / map_w as f64 >= 5.9
}

fn paint(c: u32) -> Paint<'static> {
    let [r, g, b] = channels(c);
    let mut p = Paint::default();
    p.set_color_rgba8(r as u8, g as u8, b as u8, 255);
    p.anti_alias = true;
    p
}

/// An atlas as straight (not premultiplied) RGBA.
struct Atlas {
    w: u32,
    h: u32,
    rgba: Vec<u8>,
    pixmap: Pixmap,
}

fn load_atlas(png: &[u8]) -> Atlas {
    let pixmap = Pixmap::decode_png(png).expect("atlas png");
    let rgba = pixmap
        .pixels()
        .iter()
        .flat_map(|p| {
            let c = p.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        })
        .collect();
    Atlas { w: pixmap.width(), h: pixmap.height(), rgba, pixmap }
}

const TROOP_CHARS: [u8; 14] = *b"0123456789.KMB";

#[inline]
fn troop_char_idx(b: u8) -> Option<usize> {
    match b {
        b'0'..=b'9' => Some((b - b'0') as usize),
        b'.' => Some(10),
        b'K' => Some(11),
        b'M' => Some(12),
        b'B' => Some(13),
        _ => None,
    }
}

/// The name font, shaped with rustybuzz (HarfBuzz, as the canvas does) so
/// kerning matches.
struct Font {
    face: rustybuzz::Face<'static>,
    /// Ascender + descender (font units): textBaseline "middle" puts the
    /// baseline half of that below the given y.
    middle: f32,
    units_per_em: f32,
    /// Pre-shaped single-glyph metrics for the 14 troop ASCII characters: (glyph_id, x_offset, y_offset, x_advance).
    troop_glyphs: [(ttf_parser::GlyphId, f32, f32, f32); 14],
    /// Pairwise x_advance for troop ASCII character pairs (includes GPOS kerning).
    troop_pair_adv: [[f32; 14]; 14],
    /// Cached shaped lines for player names and strings.
    shaped_cache: FxHashMap<String, Shaped>,
}

/// A shaped line: glyphs with their pen offsets, in font units.
#[derive(Clone)]
struct Shaped {
    glyphs: Vec<(ttf_parser::GlyphId, f32, f32)>,
    width: f32,
}

#[derive(PartialEq, Eq, Hash, Clone, Copy)]
struct GlyphKey {
    glyph_id: u16,
    size10: u32,
    fx: u8,
    fy: u8,
}

struct CachedGlyph {
    w: u32,
    h: u32,
    dx: i32,
    dy: i32,
    stroke_alpha: Vec<u8>,
    fill_alpha: Vec<u8>,
}

struct Outline<'a> {
    pb: &'a mut PathBuilder,
    x: f32,
    y: f32,
    s: f32,
}

impl ttf_parser::OutlineBuilder for Outline<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        self.pb.move_to(self.x + x * self.s, self.y - y * self.s);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.pb.line_to(self.x + x * self.s, self.y - y * self.s);
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.pb.quad_to(self.x + x1 * self.s, self.y - y1 * self.s, self.x + x * self.s, self.y - y * self.s);
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.pb.cubic_to(
            self.x + x1 * self.s,
            self.y - y1 * self.s,
            self.x + x2 * self.s,
            self.y - y2 * self.s,
            self.x + x * self.s,
            self.y - y * self.s,
        );
    }
    fn close(&mut self) {
        self.pb.close();
    }
}

impl Font {
    fn new() -> Font {
        let face = rustybuzz::Face::from_slice(assets::FONT, 0).expect("font");
        let middle = (face.ascender() as f32 + face.descender() as f32) / 2.0;
        let units_per_em = face.units_per_em() as f32;
        let mut troop_glyphs = [(ttf_parser::GlyphId(0), 0.0, 0.0, 0.0); 14];
        for (i, &b) in TROOP_CHARS.iter().enumerate() {
            let s = std::str::from_utf8(std::slice::from_ref(&b)).unwrap();
            let mut buf = rustybuzz::UnicodeBuffer::new();
            buf.push_str(s);
            let out = rustybuzz::shape(&face, &[], buf);
            if !out.glyph_infos().is_empty() {
                let info = out.glyph_infos()[0];
                let pos = out.glyph_positions()[0];
                troop_glyphs[i] = (
                    ttf_parser::GlyphId(info.glyph_id as u16),
                    pos.x_offset as f32,
                    pos.y_offset as f32,
                    pos.x_advance as f32,
                );
            }
        }
        let mut troop_pair_adv = [[0.0f32; 14]; 14];
        for (i, &b0) in TROOP_CHARS.iter().enumerate() {
            for (j, &b1) in TROOP_CHARS.iter().enumerate() {
                let pair = [b0, b1];
                let s = std::str::from_utf8(&pair).unwrap();
                let mut buf = rustybuzz::UnicodeBuffer::new();
                buf.push_str(s);
                let out = rustybuzz::shape(&face, &[], buf);
                troop_pair_adv[i][j] = if !out.glyph_positions().is_empty() {
                    out.glyph_positions()[0].x_advance as f32
                } else {
                    troop_glyphs[i].3
                };
            }
        }
        Font { face, middle, units_per_em, troop_glyphs, troop_pair_adv, shaped_cache: FxHashMap::default() }
    }

    fn shape_raw(&self, text: &str) -> Shaped {
        let mut buf = rustybuzz::UnicodeBuffer::new();
        buf.push_str(text);
        let out = rustybuzz::shape(&self.face, &[], buf);
        let mut pen = 0.0;
        let mut glyphs = Vec::with_capacity(out.len());
        for (info, pos) in out.glyph_infos().iter().zip(out.glyph_positions()) {
            // Characters the font doesn't have are left out (the game falls
            // back to other fonts; this binary only carries this one).
            if info.glyph_id != 0 {
                glyphs.push((ttf_parser::GlyphId(info.glyph_id as u16), pen + pos.x_offset as f32, pos.y_offset as f32));
                pen += pos.x_advance as f32;
            }
        }
        Shaped { glyphs, width: pen }
    }

    /// Fast troop-string width in font units without rustybuzz, or None if non-troop chars present.
    #[inline]
    fn troop_width_units(&self, text: &str) -> Option<f32> {
        let bytes = text.as_bytes();
        let mut pen = 0.0f32;
        let mut prev: Option<usize> = None;
        for &b in bytes {
            let idx = troop_char_idx(b)?;
            if let Some(p) = prev {
                pen += self.troop_pair_adv[p][idx];
            }
            prev = Some(idx);
        }
        if let Some(last) = prev {
            pen += self.troop_glyphs[last].3;
        }
        Some(pen)
    }

    /// Width of `text` at `size` pixels without shaping on cache hits or troop strings.
    #[inline]
    fn text_width(&mut self, text: &str, size: f32) -> f32 {
        let width_units = if let Some(w) = self.troop_width_units(text) {
            w
        } else if let Some(shaped) = self.shaped_cache.get(text) {
            shaped.width
        } else {
            let shaped = self.shape_raw(text);
            let w = shaped.width;
            self.shaped_cache.insert(text.to_string(), shaped);
            w
        };
        width_units * size / self.units_per_em
    }

    fn shape(&mut self, text: &str) -> Shaped {
        if let Some(shaped) = self.shaped_cache.get(text) {
            return shaped.clone();
        }
        let bytes = text.as_bytes();
        if !bytes.is_empty() && bytes.iter().all(|&b| troop_char_idx(b).is_some()) {
            let mut pen = 0.0f32;
            let mut glyphs = Vec::with_capacity(bytes.len());
            for k in 0..bytes.len() {
                let idx = troop_char_idx(bytes[k]).unwrap();
                let (gid, x_off, y_off, x_adv) = self.troop_glyphs[idx];
                if gid.0 != 0 {
                    glyphs.push((gid, pen + x_off, y_off));
                    if k + 1 < bytes.len() {
                        let next_idx = troop_char_idx(bytes[k + 1]).unwrap();
                        pen += self.troop_pair_adv[idx][next_idx];
                    } else {
                        pen += x_adv;
                    }
                }
            }
            return Shaped { glyphs, width: pen };
        }
        let shaped = self.shape_raw(text);
        self.shaped_cache.insert(text.to_string(), shaped.clone());
        shaped
    }

    fn render_glyph(&self, key: GlyphKey, u: f64, outline_width: f64) -> Option<CachedGlyph> {
        let font_size = key.size10 as f32 / 10.0;
        let s = font_size / self.units_per_em;
        let (ox, oy) = (key.fx as f32 / 4.0, key.fy as f32 / 4.0);
        let mut pb = PathBuilder::new();
        self.face.outline_glyph(ttf_parser::GlyphId(key.glyph_id), &mut Outline { pb: &mut pb, x: ox, y: oy, s });
        let path = pb.finish()?;
        let size = key.size10 as f64 / 10.0;
        let width = ((1.5 * u).max(size * 0.12) * (outline_width / 1.4)) as f32;
        let b = path.bounds();
        let m = width / 2.0 + 2.0;
        let (dx, dy) = ((b.left() - m).floor() as i32, (b.top() - m).floor() as i32);
        let (w, h) = (((b.right() + m).ceil() as i32 - dx).max(1) as u32, ((b.bottom() + m).ceil() as i32 - dy).max(1) as u32);
        let t = Transform::from_translate(-dx as f32, -dy as f32);
        let white = paint(0xffff_ffff);
        let stroke = Stroke { width, line_join: LineJoin::Round, ..Default::default() };
        let mut spx = Pixmap::new(w, h)?;
        spx.stroke_path(&path, &white, &stroke, t, None);
        let stroke_alpha: Vec<u8> = spx.data().chunks_exact(4).map(|p| p[3]).collect();
        let mut fpx = Pixmap::new(w, h)?;
        fpx.fill_path(&path, &white, FillRule::Winding, t, None);
        let fill_alpha: Vec<u8> = fpx.data().chunks_exact(4).map(|p| p[3]).collect();
        Some(CachedGlyph { w, h, dx, dy, stroke_alpha, fill_alpha })
    }
}

/// A regular polygon or circle (structure.frag's sdPolygon, y down): corners
/// at radius r, edge midpoints at angles rot + k * 2pi/n.
fn polygon(cx: f32, cy: f32, r: f32, sides: u32, rot: f32) -> tiny_skia::Path {
    let mut pb = PathBuilder::new();
    if sides == 0 {
        pb.push_circle(cx, cy, r);
    } else {
        for i in 0..sides {
            let a = rot + std::f32::consts::PI * (2 * i + 1) as f32 / sides as f32;
            let (px, py) = (cx + r * a.cos(), cy + r * a.sin());
            if i == 0 {
                pb.move_to(px, py);
            } else {
                pb.line_to(px, py);
            }
        }
        pb.close();
    }
    pb.finish().unwrap()
}

/// A pre-drawn image, its offset from the whole-pixel position it is drawn
/// at, and the last frame that used it.
struct Placed {
    pixmap: Pixmap,
    dx: i32,
    dy: i32,
    used: u64,
}

#[derive(PartialEq, Eq, Hash)]
struct TextKey {
    text: String,
    /// Font size x 10 (the canvas font string rounds it so).
    size10: u32,
    owner: u16,
    fx: u8,
    fy: u8,
}

/// A position as whole pixels plus quarter pixels.
fn quarter(v: f64) -> (i32, u8) {
    let q = (v * 4.0).round() as i64;
    (q.div_euclid(4) as i32, q.rem_euclid(4) as u8)
}

/// Blits a premultiplied `src` Pixmap onto the opaque `canvas` at integer `(dst_x, dst_y)`
/// and records the clipped destination rectangle for next-frame dirty-rect restoration.
#[inline]
fn blit_pixmap(
    canvas: &mut Pixmap,
    overlay_rects: &mut Vec<(u16, u16, u16, u16)>,
    out_w: i32,
    out_h: i32,
    dst_x: i32,
    dst_y: i32,
    src: &Pixmap,
) {
    let sw = src.width() as i32;
    let sh = src.height() as i32;
    let x0 = dst_x.max(0);
    let y0 = dst_y.max(0);
    let x1 = (dst_x + sw).min(out_w);
    let y1 = (dst_y + sh).min(out_h);
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    overlay_rects.push((x0 as u16, y0 as u16, x1 as u16, y1 as u16));
    let cw = out_w as usize;
    let sw_u = sw as usize;
    let sx_start = (x0 - dst_x) as usize;
    let len = (x1 - x0) as usize;
    let s_data = src.data();
    let d_data = canvas.data_mut();
    for y in y0..y1 {
        let sy = (y - dst_y) as usize;
        let s_row = &s_data[(sy * sw_u + sx_start) * 4..(sy * sw_u + sx_start + len) * 4];
        let d_row = &mut d_data[(y as usize * cw + x0 as usize) * 4..(y as usize * cw + x0 as usize + len) * 4];
        for (sp, dp) in s_row.chunks_exact(4).zip(d_row.chunks_exact_mut(4)) {
            let sa = sp[3];
            if sa == 0 {
                continue;
            } else if sa == 255 {
                dp.copy_from_slice(sp);
            } else {
                let inv = 255 - sa as u32;
                dp[0] = sp[0] + ((dp[0] as u32 * inv + 127) / 255) as u8;
                dp[1] = sp[1] + ((dp[1] as u32 * inv + 127) / 255) as u8;
                dp[2] = sp[2] + ((dp[2] as u32 * inv + 127) / 255) as u8;
            }
        }
    }
}

pub struct Renderer {
    w: usize,
    h: usize,
    out_w: usize,
    out_h: usize,
    settings: Settings,
    terrain: Vec<u32>,
    owner: Vec<u16>,
    fallout: Vec<u8>,
    fallout_colors: [u32; 256],
    /// Each tile's color.
    tile_colors: Vec<u32>,
    styles: Vec<Style>,
    /// Tiles to recolor at the next frame (changed tiles and their neighbors).
    touched: Vec<u32>,
    stamp: Vec<u32>,
    stamp_gen: u32,
    /// The map tile each video pixel shows.
    src: Vec<u32>,
    /// The video pixels each map column and row covers (-1: none).
    col_lo: Vec<i32>,
    col_hi: Vec<i32>,
    row_lo: Vec<i32>,
    row_hi: Vec<i32>,
    /// The scaled map, without overlays (RGBA8 pixels: `out_w * out_h`).
    base: Vec<[u8; 4]>,
    dirty: Vec<bool>,
    bx_n: usize,
    full_pending: bool,
    canvas: Pixmap,
    overlay_rects: Vec<(u16, u16, u16, u16)>,
    scale: f64,
    u: f64,
    icon_atlas: Atlas,
    unit_atlas: Atlas,
    status_atlas: Pixmap,
    /// Status icons scaled down, by (atlas cell, size in pixels).
    status_icons: FxHashMap<(u32, u32), Pixmap>,
    font: Font,
    unit_sprites: FxHashMap<(u16, i32), Pixmap>,
    structure_sprites: FxHashMap<(u16, i32, bool), Pixmap>,
    /// Units and text lines drawn once and copied in at whole pixels, per
    /// quarter-pixel offset; entries unused for a while are dropped.
    placed_units: FxHashMap<(u16, i32, u8, u8), Placed>,
    placed_text: FxHashMap<TextKey, Placed>,
    glyph_cache: FxHashMap<GlyphKey, Option<CachedGlyph>>,
    glyph_pos_buf: Vec<(i32, i32, GlyphKey)>,
    structures_buf: Vec<(i32, i32, u16, bool)>,
    missiles_buf: Vec<(i32, i32, u16)>,
    frame_no: u64,
    /// Frames with nuke targets, and status icons drawn per cell (for OPENFRONT_DRAW_PROFILE).
    pub overlay_counts: (u64, [u64; 12]),
    /// OPENFRONT_DRAW_PROFILE=1: time per drawing step.
    pub profile: Option<Vec<(&'static str, std::time::Duration)>>,
}

impl Renderer {
    pub fn new(w: usize, h: usize, out_w: usize, out_h: usize, terrain_bytes: &[u8], settings: Settings) -> Renderer {
        let n = w * h;
        let terrain: Vec<u32> = terrain_bytes.iter().map(|&t| settings.terrain_color(t)).collect();
        let mut fallout_colors = [0u32; 256];
        for (idx, fc) in fallout_colors.iter_mut().enumerate() {
            let noise = idx as f64 / 255.0;
            let nv = (settings.stale_nuke_base + noise * settings.stale_nuke_variation) * 255.0;
            let c = |i: usize| (crate::look::js_round(settings.stale_nuke_rgb[i] * 255.0) + nv).min(255.0);
            *fc = rgba(
                crate::look::js_round(c(0)) as u32,
                crate::look::js_round(c(1)) as u32,
                crate::look::js_round(c(2)) as u32,
            );
        }
        let mut src = vec![0u32; out_w * out_h];
        for y in 0..out_h {
            let sy = (((y as f64 + 0.5) * h as f64 / out_h as f64).floor() as usize).min(h - 1);
            for x in 0..out_w {
                let sx = (((x as f64 + 0.5) * w as f64 / out_w as f64).floor() as usize).min(w - 1);
                src[y * out_w + x] = (sy * w + sx) as u32;
            }
        }
        let (mut col_lo, mut col_hi, mut row_lo, mut row_hi) = (vec![-1; w], vec![-1; w], vec![-1; h], vec![-1; h]);
        for x in 0..out_w {
            let sx = src[x] as usize % w;
            if col_lo[sx] < 0 {
                col_lo[sx] = x as i32;
            }
            col_hi[sx] = x as i32;
        }
        for y in 0..out_h {
            let sy = src[y * out_w] as usize / w;
            if row_lo[sy] < 0 {
                row_lo[sy] = y as i32;
            }
            row_hi[sy] = y as i32;
        }
        let bx_n = out_w.div_ceil(BLOCK);
        let by_n = out_h.div_ceil(BLOCK);
        Renderer {
            w,
            h,
            out_w,
            out_h,
            settings,
            tile_colors: terrain.clone(),
            terrain,
            owner: vec![0; n],
            fallout: vec![0; n],
            fallout_colors,
            styles: vec![GRAY; 1],
            touched: Vec::new(),
            stamp: vec![0; n],
            stamp_gen: 1,
            src,
            col_lo,
            col_hi,
            row_lo,
            row_hi,
            base: vec![[0; 4]; out_w * out_h],
            dirty: vec![false; bx_n * by_n],
            bx_n,
            full_pending: true,
            canvas: Pixmap::new(out_w as u32, out_h as u32).unwrap(),
            overlay_rects: Vec::with_capacity(4096),
            scale: out_w as f64 / w as f64,
            u: out_w as f64 / 1280.0,
            icon_atlas: load_atlas(assets::ICON_ATLAS),
            unit_atlas: load_atlas(assets::UNIT_ATLAS),
            status_atlas: Pixmap::decode_png(assets::STATUS_ATLAS).expect("status atlas png"),
            status_icons: FxHashMap::default(),
            font: Font::new(),
            unit_sprites: FxHashMap::default(),
            structure_sprites: FxHashMap::default(),
            placed_units: FxHashMap::default(),
            placed_text: FxHashMap::default(),
            glyph_cache: FxHashMap::default(),
            glyph_pos_buf: Vec::new(),
            structures_buf: Vec::new(),
            missiles_buf: Vec::new(),
            frame_no: 0,
            overlay_counts: (0, [0; 12]),
            profile: std::env::var("OPENFRONT_DRAW_PROFILE").is_ok().then(Vec::new),
        }
    }

    pub fn set_style(&mut self, id: u16, s: Style) {
        let idx = id as usize;
        if idx >= self.styles.len() {
            self.styles.resize(idx + 1, GRAY);
        }
        self.styles[idx] = s;
    }

    #[inline]
    fn style(&self, id: u16) -> Style {
        self.styles.get(id as usize).copied().unwrap_or(GRAY)
    }

    #[inline]
    fn tile_color(&self, r: usize) -> u32 {
        let o = self.owner[r];
        let s = &self.settings;
        if self.fallout[r] != 0 {
            // As territory.frag: any fallout tile, owned or not, is flat green
            // ground brightened by a little per-tile noise.
            let noise_idx = ((r as u64).wrapping_mul(2654435761) as u32 >> 24) as usize;
            return self.fallout_colors[noise_idx];
        }
        if o == 0 {
            return self.terrain[r];
        }
        let (w, n) = (self.w, self.w * self.h);
        let x = r % w;
        let border = (x > 0 && self.owner[r - 1] != o)
            || (x < w - 1 && self.owner[r + 1] != o)
            || (r >= w && self.owner[r - w] != o)
            || (r < n - w && self.owner[r + w] != o);
        let st = self.style(o);
        if border {
            st.border
        } else {
            mix(self.terrain[r], st.tile_fill, s.territory_alpha)
        }
    }

    fn touch(&mut self, r: i64) {
        if r < 0 || r as usize >= self.owner.len() || self.stamp[r as usize] == self.stamp_gen {
            return;
        }
        self.stamp[r as usize] = self.stamp_gen;
        self.touched.push(r as u32);
    }

    /// Changed tiles as [ref, owner | fallout << 16] pairs.
    pub fn apply_tiles(&mut self, tiles: &[u32]) {
        let w = self.w as i64;
        for pair in tiles.chunks_exact(2) {
            let r = pair[0] as usize;
            self.owner[r] = (pair[1] & 0xffff) as u16;
            self.fallout[r] = (pair[1] >> 16) as u8;
            let r = r as i64;
            for t in [r, r - 1, r + 1, r - w, r + w] {
                self.touch(t);
            }
        }
    }

    fn recolor(&mut self) {
        if self.full_pending {
            for r in 0..self.owner.len() {
                self.tile_colors[r] = self.tile_color(r);
            }
            self.touched.clear();
        } else {
            let touched = std::mem::take(&mut self.touched);
            for &r in &touched {
                let r = r as usize;
                self.tile_colors[r] = self.tile_color(r);
                let (tx, ty) = (r % self.w, r / self.w);
                if self.col_lo[tx] < 0 || self.row_lo[ty] < 0 {
                    continue;
                }
                let (bx0, bx1) = (self.col_lo[tx] as usize / BLOCK, self.col_hi[tx] as usize / BLOCK);
                let (by0, by1) = (self.row_lo[ty] as usize / BLOCK, self.row_hi[ty] as usize / BLOCK);
                for by in by0..=by1 {
                    for bx in bx0..=bx1 {
                        self.dirty[by * self.bx_n + bx] = true;
                    }
                }
            }
            self.touched = touched;
            self.touched.clear();
        }
        self.stamp_gen += 1;
    }

    /// Brings the scaled map up to date where tiles changed, and copies dirty
    /// blocks to the canvas.
    fn update_base(&mut self) {
        if self.full_pending {
            for (px, &s) in self.base.iter_mut().zip(&self.src) {
                *px = self.tile_colors[s as usize].to_le_bytes();
            }
            self.canvas.data_mut().copy_from_slice(self.base.as_flattened());
            self.full_pending = false;
            self.dirty.fill(false);
            self.overlay_rects.clear();
            return;
        }
        let out_w = self.out_w;
        let canvas_bytes = self.canvas.data_mut();
        for (b, d) in self.dirty.iter_mut().enumerate() {
            if !*d {
                continue;
            }
            *d = false;
            let (x0, y0) = ((b % self.bx_n) * BLOCK, (b / self.bx_n) * BLOCK);
            let (x1, y1) = ((x0 + BLOCK).min(out_w), (y0 + BLOCK).min(self.out_h));
            for y in y0..y1 {
                let row = y * out_w;
                for i in row + x0..row + x1 {
                    self.base[i] = self.tile_colors[self.src[i] as usize].to_le_bytes();
                }
                let b0 = (row + x0) * 4;
                let b1 = (row + x1) * 4;
                canvas_bytes[b0..b1].copy_from_slice(self.base[row + x0..row + x1].as_flattened());
            }
        }
    }

    /// Restores rectangles where overlays were drawn on the previous frame from `self.base`.
    fn restore_overlays(&mut self) {
        if self.overlay_rects.is_empty() {
            return;
        }
        let out_w = self.out_w;
        let canvas_bytes = self.canvas.data_mut();
        for (x0, y0, x1, y1) in self.overlay_rects.drain(..) {
            let (x0, y0, x1, y1) = (x0 as usize, y0 as usize, x1 as usize, y1 as usize);
            for y in y0..y1 {
                let row = y * out_w;
                let b0 = (row + x0) * 4;
                let b1 = (row + x1) * 4;
                canvas_bytes[b0..b1].copy_from_slice(self.base[row + x0..row + x1].as_flattened());
            }
        }
    }

    fn to_x(&self, tile: i32) -> f64 {
        ((tile as usize % self.w) as f64 + 0.5) * self.scale
    }

    fn to_y(&self, tile: i32) -> f64 {
        ((tile as usize / self.w) as f64 + 0.5) * self.scale
    }

    /// A unit sprite colored per owner with the unit shader's gray bands:
    /// light -> territory, mid -> halfway, center -> territory, dark -> border.
    fn unit_sprite(&mut self, col: i32, o: u16) -> &Pixmap {
        let st = self.style(o);
        let atlas = &self.unit_atlas;
        self.unit_sprites.entry((o, col)).or_insert_with(|| {
            let mid = mix(st.fill, st.border, 0.5);
            let mut sprite = Pixmap::new(UNIT_CELL, UNIT_CELL).unwrap();
            let px = sprite.pixels_mut();
            for y in 0..UNIT_CELL {
                for x in 0..UNIT_CELL {
                    let si = ((y * atlas.w + col as u32 * UNIT_CELL + x) * 4) as usize;
                    let a = atlas.rgba[si + 3];
                    if a == 0 {
                        continue;
                    }
                    let gray = atlas.rgba[si] as f64 / 255.0;
                    let c = if gray > 0.6 {
                        st.fill
                    } else if gray > 0.45 {
                        mid
                    } else if gray > 0.34 {
                        st.fill
                    } else {
                        st.border
                    };
                    let [r, g, b] = channels(c);
                    px[(y * UNIT_CELL + x) as usize] =
                        tiny_skia::ColorU8::from_rgba(r as u8, g as u8, b as u8, a).premultiply();
                }
            }
            sprite
        })
    }

    /// A structure drawn once per type, owner and build state.
    fn structure_sprite(&mut self, col: i32, o: u16, building: bool) -> &Pixmap {
        let st = self.style(o);
        let (shape_scale, icon_fill) = self.settings.shapes[col as usize];
        let r = (6.5 * self.u * shape_scale) as f32;
        let icons = &self.icon_atlas;
        self.structure_sprites.entry((o, col, building)).or_insert_with(|| {
            let size = (2.0 * r).ceil() as u32 + 2;
            let mut sprite = Pixmap::new(size, size).unwrap();
            let mid = size as f32 / 2.0;
            let (sides, rot) = (STRUCTURE_SIDES[col as usize], STRUCTURE_ROT[col as usize]);
            let outer = if building { rgba(127, 127, 127) } else { st.struct_border };
            sprite.fill_path(&polygon(mid, mid, r, sides, rot), &paint(outer), FillRule::Winding, Transform::identity(), None);
            // Border band: 0.06 of the 0.45 shape radius in structure.frag.
            let inner_r = r * (1.0 - 0.06 / 0.45 / shape_scale as f32);
            let inner = if building { rgba(198, 198, 198) } else { st.struct_fill };
            sprite.fill_path(&polygon(mid, mid, inner_r, sides, rot), &paint(inner), FillRule::Winding, Transform::identity(), None);
            let cell = icons.h as f32;
            let icon = 2.0 * r * icon_fill as f32 * 0.75;
            let k = icon / cell;
            // drawImage(atlas, col * cell, 0, cell, cell, mid - icon/2, mid - icon/2, icon, icon)
            let (dx, dy) = (mid - icon / 2.0, mid - icon / 2.0);
            let clip = Rect::from_xywh(dx, dy, icon, icon).unwrap();
            let mut mask = tiny_skia::Mask::new(size, size).unwrap();
            mask.fill_path(&PathBuilder::from_rect(clip), FillRule::Winding, true, Transform::identity());
            sprite.draw_pixmap(
                0,
                0,
                icons.pixmap.as_ref(),
                &PixmapPaint { quality: FilterQuality::Bilinear, ..Default::default() },
                Transform::from_row(k, 0.0, 0.0, k, dx - col as f32 * cell * k, dy),
                Some(&mask),
            );
            sprite
        })
    }

    fn draw_unit(&mut self, col: i32, tile: i32, o: u16) {
        let sprite_scale = self.scale.max(1.4 * self.u);
        let size = UNIT_CELL as f64 * sprite_scale;
        let (x, y) = (self.to_x(tile), self.to_y(tile));
        let ((ix, fx), (iy, fy)) = (quarter(x - size / 2.0), quarter(y - size / 2.0));
        let frame = self.frame_no;
        if !self.placed_units.contains_key(&(o, col, fx, fy)) {
            self.unit_sprite(col, o);
            let sprite = &self.unit_sprites[&(o, col)];
            let k = sprite_scale as f32;
            let side = size.ceil() as u32 + 2;
            let mut px = Pixmap::new(side, side).unwrap();
            let t = Transform::from_row(k, 0.0, 0.0, k, 1.0 + fx as f32 / 4.0, 1.0 + fy as f32 / 4.0);
            let paint = PixmapPaint { quality: FilterQuality::Bilinear, ..Default::default() };
            px.draw_pixmap(0, 0, sprite.as_ref(), &paint, t, None);
            self.placed_units.insert((o, col, fx, fy), Placed { pixmap: px, dx: -1, dy: -1, used: frame });
        }
        let p = self.placed_units.get_mut(&(o, col, fx, fy)).unwrap();
        p.used = frame;
        blit_pixmap(&mut self.canvas, &mut self.overlay_rects, self.out_w as i32, self.out_h as i32, ix + p.dx, iy + p.dy, &p.pixmap);
    }

    fn draw_structure(&mut self, col: i32, tile: i32, o: u16, building: bool) {
        let (tx, ty) = (self.to_x(tile), self.to_y(tile));
        self.structure_sprite(col, o, building);
        let sprite = &self.structure_sprites[&(o, col, building)];
        let half = sprite.width() as f64 / 2.0;
        let x = crate::look::js_round(tx - half) as i32;
        let y = crate::look::js_round(ty - half) as i32;
        blit_pixmap(&mut self.canvas, &mut self.overlay_rects, self.out_w as i32, self.out_h as i32, x, y, sprite);
    }

    /// A status icon at `size` pixels: the atlas cell's content, halved down
    /// to near the size first (bilinear alone would alias from 224 px).
    fn status_icon(&mut self, cell: u32, size: u32) -> &Pixmap {
        let atlas = &self.status_atlas;
        self.status_icons.entry((cell, size)).or_insert_with(|| {
            let inner = STATUS_CELL - 2 * STATUS_PAD;
            let (cx, cy) = ((cell % STATUS_COLS) * STATUS_CELL + STATUS_PAD, (cell / STATUS_COLS) * STATUS_CELL + STATUS_PAD);
            let mut img = Pixmap::new(inner, inner).unwrap();
            img.draw_pixmap(-(cx as i32), -(cy as i32), atlas.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
            while img.width() >= size * 2 && img.width() > 2 {
                img = halve(&img);
            }
            let mut out = Pixmap::new(size.max(1), size.max(1)).unwrap();
            let k = size as f32 / img.width() as f32;
            let paint = PixmapPaint { quality: FilterQuality::Bilinear, ..Default::default() };
            out.draw_pixmap(0, 0, img.as_ref(), &paint, Transform::from_scale(k, k), None);
            out
        })
    }

    /// The status row above a name (status-icon.vert.glsl): icons 1.1 name
    /// heights tall, 0.15 of that apart, centred on the name, top edge 1.4
    /// name heights above its centre.
    fn draw_status(&mut self, status: u8, cx: f64, cy: f64, em: f64) {
        let icons: Vec<u32> = STATUS_ICONS.iter().filter(|(bit, _)| status & bit != 0).map(|&(_, cell)| cell).collect();
        if icons.is_empty() {
            return;
        }
        let size = (em * 1.1).round().max(4.0);
        let gap = size * 0.15;
        let total = icons.len() as f64 * size + (icons.len() - 1) as f64 * gap;
        let top = (cy - em * self.settings.status_row_offset).round() as i32;
        for (i, cell) in icons.into_iter().enumerate() {
            self.overlay_counts.1[cell as usize] += 1;
            let x = (cx - total / 2.0 + i as f64 * (size + gap)).round() as i32;
            self.status_icon(cell, size as u32);
            let icon = &self.status_icons[&(cell, size as u32)];
            blit_pixmap(&mut self.canvas, &mut self.overlay_rects, self.out_w as i32, self.out_h as i32, x, top, icon);
        }
    }

    /// NukeTelegraphPass: at each nuke's target, the inner blast radius as a
    /// faint red disc with a solid edge and the outer radius as a dashed red
    /// ring that turns slowly.
    fn draw_nuke_targets(&mut self, nukes: &[(u32, u16, u16)]) {
        if !nukes.is_empty() {
            self.overlay_counts.0 += 1;
        }
        let t = self.frame_no as f32 / 30.0;
        let alpha = 0.85 + 0.1 * (t * 3.0).sin();
        let scale = self.scale as f32;
        let stroke_w = (1.5 * scale).max(1.0);
        let red = |a: f32| {
            let mut p = Paint::default();
            p.set_color(Color::from_rgba(1.0, 0.0, 0.0, a.clamp(0.0, 1.0)).unwrap());
            p.anti_alias = true;
            p
        };
        for &(tile, inner, outer) in nukes {
            let (x, y) = (self.to_x(tile as i32) as f32, self.to_y(tile as i32) as f32);
            let (ri, ro) = (inner as f32 * scale, outer as f32 * scale);
            let r_max = ri.max(ro) + stroke_w + 2.0;
            let nx0 = (x - r_max).floor().max(0.0) as usize;
            let ny0 = (y - r_max).floor().max(0.0) as usize;
            let nx1 = ((x + r_max).ceil().max(0.0) as usize).min(self.out_w);
            let ny1 = ((y + r_max).ceil().max(0.0) as usize).min(self.out_h);
            if nx0 < nx1 && ny0 < ny1 {
                self.overlay_rects.push((nx0 as u16, ny0 as u16, nx1 as u16, ny1 as u16));
            }
            let Some(disc) = PathBuilder::from_circle(x, y, ri) else { continue };
            self.canvas.fill_path(&disc, &red(alpha - 0.6), FillRule::Winding, Transform::identity(), None);
            let edge = Stroke { width: stroke_w, ..Default::default() };
            self.canvas.stroke_path(&disc, &red(alpha), &edge, Transform::identity(), None);
            let Some(ring) = PathBuilder::from_circle(x, y, ro) else { continue };
            let dash = tiny_skia::StrokeDash::new(vec![12.0 * scale, 6.0 * scale], t * 20.0 * scale);
            let ring_stroke = Stroke { width: stroke_w, dash, ..Default::default() };
            self.canvas.stroke_path(&ring, &red(alpha), &ring_stroke, Transform::identity(), None);
        }
    }

    /// Names: the engine's placement, sized like the client's name pass.
    fn draw_names(&mut self, labels: &[Label]) {
        let (scale, u, out_w) = (self.scale, self.u, self.out_w as f64);
        let (out_w_i, out_h_i) = (self.out_w as i32, self.out_h as i32);
        let (name_scale_factor, name_scale_cap, troop_size_multiplier, outline_width) = (
            self.settings.name_scale_factor,
            self.settings.name_scale_cap,
            self.settings.troop_size_multiplier,
            self.settings.outline_width,
        );
        for n in labels {
            let base = n.size.floor().max(1.0);
            let name_size = (base * name_scale_factor).floor().max(4.0);
            let name_scale = (base * 0.25).min(name_scale_cap);
            let em = name_size * name_scale * scale;
            if em < 6.0 * u {
                continue; // too small to read
            }
            let st = self.style(n.owner);
            let y = n.y * scale;
            // The troop line sits 1.1 baselines below the name (name.vert.glsl);
            // the font's baseline is 36/48 of its size.
            let lines = [(&n.name, y, em), (&n.troops, y + em * 0.75 * 1.1, em * troop_size_multiplier)];
            let mut name_x = None;
            for (text, ly, size) in lines {
                // ctx.font = `${size.toFixed(1)}px ...`
                let size10 = (size * 10.0).round() as u32;
                let font_size = size10 as f32 / 10.0;
                // Keep the text inside the video; the game would let you pan to it.
                let half = self.font.text_width(text, font_size) as f64 / 2.0 + 2.0 * u;
                let x = (n.x * scale).max(half).min(half.max(out_w - half));
                name_x.get_or_insert(x);
                let ((ix, fx), (iy, fy)) = (quarter(x - (half - 2.0 * u)), quarter(ly));
                let key = TextKey { text: text.clone(), size10, owner: n.owner, fx, fy };
                let frame = self.frame_no;
                if let Some(p) = self.placed_text.get_mut(&key) {
                    p.used = frame;
                    blit_pixmap(&mut self.canvas, &mut self.overlay_rects, out_w_i, out_h_i, ix + p.dx, iy + p.dy, &p.pixmap);
                    continue;
                }
                let line = self.font.shape(text);
                let (ox, oy) = (fx as f32 / 4.0, fy as f32 / 4.0);
                let s_em = font_size / self.font.units_per_em;
                let baseline = oy + self.font.middle * s_em;
                self.glyph_pos_buf.clear();
                let (mut min_x, mut min_y, mut max_x, mut max_y) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
                for &(g, gx_font, gy_font) in &line.glyphs {
                    let gx = ox + gx_font * s_em;
                    let gy = baseline - gy_font * s_em;
                    let (g_ix, g_fx) = quarter(gx as f64);
                    let (g_iy, g_fy) = quarter(gy as f64);
                    let gkey = GlyphKey { glyph_id: g.0, size10, fx: g_fx, fy: g_fy };
                    if !self.glyph_cache.contains_key(&gkey) {
                        let rendered = self.font.render_glyph(gkey, u, outline_width);
                        self.glyph_cache.insert(gkey, rendered);
                    }
                    if let Some(cg) = &self.glyph_cache[&gkey] {
                        min_x = min_x.min(g_ix + cg.dx);
                        min_y = min_y.min(g_iy + cg.dy);
                        max_x = max_x.max(g_ix + cg.dx + cg.w as i32);
                        max_y = max_y.max(g_iy + cg.dy + cg.h as i32);
                        self.glyph_pos_buf.push((g_ix, g_iy, gkey));
                    }
                }
                if min_x >= max_x || min_y >= max_y {
                    continue;
                }
                let (pw, ph) = ((max_x - min_x) as usize, (max_y - min_y) as usize);
                let mut px = Pixmap::new(pw as u32, ph as u32).unwrap();
                let data = px.data_mut();
                let [sr, sg, sb] = channels(st.fill);
                let [fr, fg, fb] = channels(st.name_shade);
                // Pass 1: composite all glyph outlines first so adjacent strokes never paint over fills.
                for &(g_ix, g_iy, gkey) in &self.glyph_pos_buf {
                    let Some(cg) = &self.glyph_cache[&gkey] else { continue };
                    let (bx, by) = ((g_ix + cg.dx - min_x) as usize, (g_iy + cg.dy - min_y) as usize);
                    let gw = cg.w as usize;
                    for row in 0..cg.h as usize {
                        let src_row = &cg.stroke_alpha[row * gw..(row + 1) * gw];
                        let dst_row = &mut data[((by + row) * pw + bx) * 4..((by + row) * pw + bx + gw) * 4];
                        for (sa, dst) in src_row.iter().copied().zip(dst_row.chunks_exact_mut(4)) {
                            if sa > dst[3] {
                                let sa32 = sa as u32;
                                dst[0] = ((sr * sa32 + 127) / 255) as u8;
                                dst[1] = ((sg * sa32 + 127) / 255) as u8;
                                dst[2] = ((sb * sa32 + 127) / 255) as u8;
                                dst[3] = sa;
                            }
                        }
                    }
                }
                // Pass 2: composite all glyph fills on top of the outlines.
                for &(g_ix, g_iy, gkey) in &self.glyph_pos_buf {
                    let Some(cg) = &self.glyph_cache[&gkey] else { continue };
                    let (bx, by) = ((g_ix + cg.dx - min_x) as usize, (g_iy + cg.dy - min_y) as usize);
                    let gw = cg.w as usize;
                    for row in 0..cg.h as usize {
                        let src_row = &cg.fill_alpha[row * gw..(row + 1) * gw];
                        let dst_row = &mut data[((by + row) * pw + bx) * 4..((by + row) * pw + bx + gw) * 4];
                        for (fa, dst) in src_row.iter().copied().zip(dst_row.chunks_exact_mut(4)) {
                            if fa == 255 {
                                dst[0] = fr as u8;
                                dst[1] = fg as u8;
                                dst[2] = fb as u8;
                                dst[3] = 255;
                            } else if fa > 0 {
                                let fa32 = fa as u32;
                                let inv = 255 - fa32;
                                let src_r = (fr * fa32 + 127) / 255;
                                let src_g = (fg * fa32 + 127) / 255;
                                let src_b = (fb * fa32 + 127) / 255;
                                dst[0] = (src_r + (dst[0] as u32 * inv + 127) / 255) as u8;
                                dst[1] = (src_g + (dst[1] as u32 * inv + 127) / 255) as u8;
                                dst[2] = (src_b + (dst[2] as u32 * inv + 127) / 255) as u8;
                                dst[3] = (fa32 + (dst[3] as u32 * inv + 127) / 255) as u8;
                            }
                        }
                    }
                }
                blit_pixmap(&mut self.canvas, &mut self.overlay_rects, out_w_i, out_h_i, ix + min_x, iy + min_y, &px);
                self.placed_text.insert(key, Placed { pixmap: px, dx: min_x, dy: min_y, used: frame });
            }
            if n.status != 0 {
                self.draw_status(n.status, name_x.unwrap_or(n.x * scale), y, em);
            }
        }
    }

    /// Saves the last drawn frame as a PNG.
    pub fn save_png(&self, path: &str) -> Result<(), String> {
        self.canvas.save_png(path).map_err(|e| format!("writing {path}: {e}"))
    }

    /// Draws the current state; returns the frame as RGBA.
    pub fn draw(&mut self, units: &[i32], labels: &[Label], nukes: &[(u32, u16, u16)], progress: f64) -> &[u8] {
        let mut lap = std::time::Instant::now();
        let mut step = |r: &mut Renderer, name: &'static str| {
            if let Some(p) = &mut r.profile {
                let now = std::time::Instant::now();
                match p.iter_mut().find(|e| e.0 == name) {
                    Some(e) => e.1 += now - lap,
                    None => p.push((name, now - lap)),
                }
                lap = now;
            }
        };
        self.recolor();
        step(self, "recolor tiles");
        self.update_base();
        step(self, "scaled map");
        self.restore_overlays();
        step(self, "copy map");
        self.draw_nuke_targets(nukes);
        // Single-pass unit bucketing: draw boats/trains immediately, bucket structures and missiles.
        self.structures_buf.clear();
        self.missiles_buf.clear();
        for u in units.chunks_exact(4) {
            let (code, tile, o) = (u[0], u[1], u[2] as u16);
            if code >= STRUCTURE_CODE {
                self.structures_buf.push((code - STRUCTURE_CODE, tile, o, u[3] != 0));
            } else if is_missile(code) {
                self.missiles_buf.push((code, tile, o));
            } else {
                self.draw_unit(code, tile, o);
            }
        }
        for i in 0..self.structures_buf.len() {
            let (col, tile, o, building) = self.structures_buf[i];
            self.draw_structure(col, tile, o, building);
        }
        for i in 0..self.missiles_buf.len() {
            let (code, tile, o) = self.missiles_buf[i];
            self.draw_unit(code, tile, o);
        }
        step(self, "units and structures");
        self.draw_names(labels);
        step(self, "names");
        self.frame_no += 1;
        if self.frame_no % 64 == 0 {
            let keep = self.frame_no.saturating_sub(64);
            self.placed_units.retain(|_, p| p.used >= keep);
            self.placed_text.retain(|_, p| p.used >= keep);
        }
        let bar_h = ((self.out_h as f64 / 180.0).round() as usize).max(3) as f32;
        let (w, h) = (self.out_w as f32, self.out_h as f32);
        let filled = (self.out_w as f64 * progress).round() as f32;
        let mut bg = Paint::default();
        bg.set_color(Color::from_rgba8(30, 30, 30, 255));
        self.canvas.fill_rect(Rect::from_xywh(0.0, h - bar_h, w, bar_h).unwrap(), &bg, Transform::identity(), None);
        if filled > 0.0 {
            let mut fg = Paint::default();
            fg.set_color(Color::WHITE);
            self.canvas.fill_rect(Rect::from_xywh(0.0, h - bar_h, filled, bar_h).unwrap(), &fg, Transform::identity(), None);
        }
        self.canvas.data()
    }
}
