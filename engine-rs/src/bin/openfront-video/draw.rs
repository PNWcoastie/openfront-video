//! Draws video frames like render.ts createFrameRenderer: the map with
//! territory and borders, unit and structure sprites, name labels and a
//! progress bar.

use std::collections::HashMap;

use tiny_skia::{
    Color, FillRule, FilterQuality, LineJoin, Paint, PathBuilder, Pixmap, PixmapPaint, Rect, Stroke, Transform,
};

use crate::assets;
use crate::look::{channels, mix, rgba, Settings, Style, GRAY};

/// A name label: the engine's placement and the text to draw.
pub struct Label {
    pub owner: u16,
    pub name: String,
    pub troops: String,
    pub x: f64,
    pub y: f64,
    pub size: f64,
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

/// The name font, shaped with rustybuzz (HarfBuzz, as the canvas does) so
/// kerning matches.
struct Font {
    face: rustybuzz::Face<'static>,
    /// Ascender + descender (font units): textBaseline "middle" puts the
    /// baseline half of that below the given y.
    middle: f32,
}

/// A shaped line: glyphs with their pen offsets, in font units.
struct Shaped {
    glyphs: Vec<(ttf_parser::GlyphId, f32, f32)>,
    width: f32,
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
        Font { face, middle }
    }

    fn shape(&self, text: &str) -> Shaped {
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

    /// Width of the text at this size, in pixels.
    fn measure(&self, line: &Shaped, size: f32) -> f32 {
        line.width * size / self.face.units_per_em() as f32
    }

    /// The text's outline with its left end at x and the "middle" baseline at y.
    fn path(&self, line: &Shaped, size: f32, x: f32, y: f32) -> Option<tiny_skia::Path> {
        let s = size / self.face.units_per_em() as f32;
        let mut pb = PathBuilder::new();
        let baseline = y + self.middle * s;
        for &(g, dx, dy) in &line.glyphs {
            self.face.outline_glyph(g, &mut Outline { pb: &mut pb, x: x + dx * s, y: baseline - dy * s, s });
        }
        pb.finish()
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

pub struct Renderer {
    w: usize,
    h: usize,
    out_w: usize,
    out_h: usize,
    settings: Settings,
    terrain: Vec<u32>,
    owner: Vec<u16>,
    fallout: Vec<u8>,
    /// Each tile's color.
    tile_colors: Vec<u32>,
    styles: HashMap<u16, Style>,
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
    /// The scaled map, without overlays.
    base: Vec<u32>,
    dirty: Vec<bool>,
    bx_n: usize,
    full_pending: bool,
    canvas: Pixmap,
    scale: f64,
    u: f64,
    icon_atlas: Atlas,
    unit_atlas: Atlas,
    font: Font,
    unit_sprites: HashMap<(u16, i32), Pixmap>,
    structure_sprites: HashMap<(u16, i32, bool), Pixmap>,
    /// Units and text lines drawn once and copied in at whole pixels, per
    /// quarter-pixel offset; entries unused for a while are dropped.
    placed_units: HashMap<(u16, i32, u8, u8), Placed>,
    placed_text: HashMap<TextKey, Placed>,
    frame_no: u64,
    /// OPENFRONT_DRAW_PROFILE=1: time per drawing step.
    pub profile: Option<Vec<(&'static str, std::time::Duration)>>,
}

impl Renderer {
    pub fn new(w: usize, h: usize, out_w: usize, out_h: usize, terrain_bytes: &[u8], settings: Settings) -> Renderer {
        let n = w * h;
        let terrain: Vec<u32> = terrain_bytes.iter().map(|&t| settings.terrain_color(t)).collect();
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
            styles: HashMap::new(),
            touched: Vec::new(),
            stamp: vec![0; n],
            stamp_gen: 1,
            src,
            col_lo,
            col_hi,
            row_lo,
            row_hi,
            base: vec![0; out_w * out_h],
            dirty: vec![false; bx_n * by_n],
            bx_n,
            full_pending: true,
            canvas: Pixmap::new(out_w as u32, out_h as u32).unwrap(),
            scale: out_w as f64 / w as f64,
            u: out_w as f64 / 1280.0,
            icon_atlas: load_atlas(assets::ICON_ATLAS),
            unit_atlas: load_atlas(assets::UNIT_ATLAS),
            font: Font::new(),
            unit_sprites: HashMap::new(),
            structure_sprites: HashMap::new(),
            placed_units: HashMap::new(),
            placed_text: HashMap::new(),
            frame_no: 0,
            profile: std::env::var("OPENFRONT_DRAW_PROFILE").is_ok().then(Vec::new),
        }
    }

    pub fn set_style(&mut self, id: u16, s: Style) {
        self.styles.insert(id, s);
    }

    fn style(&self, id: u16) -> Style {
        self.styles.get(&id).copied().unwrap_or(GRAY)
    }

    fn tile_color(&self, r: usize) -> u32 {
        let o = self.owner[r];
        let s = &self.settings;
        if self.fallout[r] != 0 {
            // As territory.frag: any fallout tile, owned or not, is flat green
            // ground brightened by a little per-tile noise.
            let noise = ((r as u64 * 2654435761) as u32 >> 24) as f64 / 255.0;
            let n = (s.stale_nuke_base + noise * s.stale_nuke_variation) * 255.0;
            let c = |i: usize| (crate::look::js_round(s.stale_nuke_rgb[i] * 255.0) + n).min(255.0);
            return rgba(
                crate::look::js_round(c(0)) as u32,
                crate::look::js_round(c(1)) as u32,
                crate::look::js_round(c(2)) as u32,
            );
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

    /// Brings the scaled map up to date where tiles changed.
    fn update_base(&mut self) {
        if self.full_pending {
            for (px, &s) in self.base.iter_mut().zip(&self.src) {
                *px = self.tile_colors[s as usize];
            }
            self.full_pending = false;
            self.dirty.fill(false);
            return;
        }
        for (b, d) in self.dirty.iter_mut().enumerate() {
            if !*d {
                continue;
            }
            *d = false;
            let (x0, y0) = ((b % self.bx_n) * BLOCK, (b / self.bx_n) * BLOCK);
            let (x1, y1) = ((x0 + BLOCK).min(self.out_w), (y0 + BLOCK).min(self.out_h));
            for y in y0..y1 {
                let row = y * self.out_w;
                for i in row + x0..row + x1 {
                    self.base[i] = self.tile_colors[self.src[i] as usize];
                }
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
        self.canvas.draw_pixmap(ix + p.dx, iy + p.dy, p.pixmap.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
    }

    fn draw_structure(&mut self, col: i32, tile: i32, o: u16, building: bool) {
        let (tx, ty) = (self.to_x(tile), self.to_y(tile));
        self.structure_sprite(col, o, building);
        let sprite = &self.structure_sprites[&(o, col, building)];
        let half = sprite.width() as f64 / 2.0;
        let x = crate::look::js_round(tx - half) as i32;
        let y = crate::look::js_round(ty - half) as i32;
        self.canvas.draw_pixmap(x, y, sprite.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
    }

    /// Names: the engine's placement, sized like the client's name pass.
    fn draw_names(&mut self, labels: &[Label]) {
        let s = &self.settings;
        let (scale, u, out_w) = (self.scale, self.u, self.out_w as f64);
        for n in labels {
            let base = n.size.floor().max(1.0);
            let name_size = (base * s.name_scale_factor).floor().max(4.0);
            let name_scale = (base * 0.25).min(s.name_scale_cap);
            let em = name_size * name_scale * scale;
            if em < 6.0 * u {
                continue; // too small to read
            }
            let st = self.styles.get(&n.owner).copied().unwrap_or(GRAY);
            let y = n.y * scale;
            // The troop line sits 1.1 baselines below the name (name.vert.glsl);
            // the font's baseline is 36/48 of its size.
            let lines = [(&n.name, y, em), (&n.troops, y + em * 0.75 * 1.1, em * s.troop_size_multiplier)];
            for (text, ly, size) in lines {
                // ctx.font = `${size.toFixed(1)}px ...`
                let size10 = (size * 10.0).round() as u32;
                let font_size = size10 as f32 / 10.0;
                let line = self.font.shape(text);
                // Keep the text inside the video; the game would let you pan to it.
                let half = self.font.measure(&line, font_size) as f64 / 2.0 + 2.0 * u;
                let x = (n.x * scale).max(half).min(half.max(out_w - half));
                let ((ix, fx), (iy, fy)) = (quarter(x - (half - 2.0 * u)), quarter(ly));
                let key = TextKey { text: text.clone(), size10, owner: n.owner, fx, fy };
                let frame = self.frame_no;
                if !self.placed_text.contains_key(&key) {
                    let (ox, oy) = (fx as f32 / 4.0, fy as f32 / 4.0);
                    let Some(path) = self.font.path(&line, font_size, ox, oy) else { continue };
                    let width = ((1.5 * u).max(size * 0.12) * (s.outline_width / 1.4)) as f32;
                    let b = path.bounds();
                    let m = width / 2.0 + 2.0;
                    let (dx, dy) = ((b.left() - m).floor() as i32, (b.top() - m).floor() as i32);
                    let (w, h) = ((b.right() + m).ceil() as i32 - dx, (b.bottom() + m).ceil() as i32 - dy);
                    let mut px = Pixmap::new(w.max(1) as u32, h.max(1) as u32).unwrap();
                    let t = Transform::from_translate(-dx as f32, -dy as f32);
                    let stroke = Stroke { width, line_join: LineJoin::Round, ..Default::default() };
                    px.stroke_path(&path, &paint(st.fill), &stroke, t, None);
                    px.fill_path(&path, &paint(st.name_shade), FillRule::Winding, t, None);
                    self.placed_text.insert(TextKey { text: text.clone(), size10, owner: n.owner, fx, fy }, Placed { pixmap: px, dx, dy, used: frame });
                }
                let p = self.placed_text.get_mut(&key).unwrap();
                p.used = frame;
                self.canvas.draw_pixmap(ix + p.dx, iy + p.dy, p.pixmap.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
            }
        }
    }

    /// Saves the last drawn frame as a PNG.
    pub fn save_png(&self, path: &str) -> Result<(), String> {
        self.canvas.save_png(path).map_err(|e| format!("writing {path}: {e}"))
    }

    /// Draws the current state; returns the frame as RGBA.
    pub fn draw(&mut self, units: &[i32], labels: &[Label], progress: f64) -> &[u8] {
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
        // The base map is opaque, so its bytes are already premultiplied.
        let bytes = unsafe { std::slice::from_raw_parts(self.base.as_ptr() as *const u8, self.base.len() * 4) };
        self.canvas.data_mut().copy_from_slice(bytes);
        step(self, "copy map");
        // Boats and trains, then structures, then missiles on top (as UnitPass).
        for pass in 0..3 {
            for u in units.chunks_exact(4) {
                let (code, tile, o) = (u[0], u[1], u[2] as u16);
                let structure = code >= STRUCTURE_CODE;
                let missile = !structure && is_missile(code);
                match pass {
                    0 if !structure && !missile => self.draw_unit(code, tile, o),
                    1 if structure => self.draw_structure(code - STRUCTURE_CODE, tile, o, u[3] != 0),
                    2 if missile => self.draw_unit(code, tile, o),
                    _ => {}
                }
            }
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
