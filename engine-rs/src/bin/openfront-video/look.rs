//! How the game client colors things: the default theme's player colors
//! (ThemeProvider.ts, ColorAllocator.ts, PlayerColors.ts, with the parts of
//! the colord library they use), terrain colors (ColorUtils.ts) and the
//! render settings render.ts draws with.

use std::collections::HashMap;

use engine_rs::js::simple_hash;
use engine_rs::rng::PseudoRandom;
use serde_json::Value;

/// A color packed as little-endian RGBA (r in the low byte), alpha 255.
pub fn rgba(r: u32, g: u32, b: u32) -> u32 {
    0xff00_0000 | (b << 16) | (g << 8) | r
}

pub fn channels(c: u32) -> [u32; 3] {
    [c & 0xff, (c >> 8) & 0xff, (c >> 16) & 0xff]
}

/// JavaScript's Math.round.
pub fn js_round(x: f64) -> f64 {
    (x + 0.5).floor()
}

/// render.ts mix: a toward b by t, per channel.
pub fn mix(a: u32, b: u32, t: f64) -> u32 {
    let (a, b) = (channels(a), channels(b));
    let m = |i: usize| js_round(a[i] as f64 + (b[i] as f64 - a[i] as f64) * t) as u32;
    rgba(m(0), m(1), m(2))
}

// ── colord 2.10 ──

/// colord's internal color: unrounded RGB in 0..255 and alpha.
#[derive(Clone, Copy, Debug)]
pub struct Colord {
    r: f64,
    g: f64,
    b: f64,
    a: f64,
}

/// colord's round(value, digits).
fn round_to(x: f64, digits: i32) -> f64 {
    let p = 10f64.powi(digits);
    js_round(p * x) / p + 0.0
}

fn clamp(x: f64, lo: f64, hi: f64) -> f64 {
    if x > hi {
        hi
    } else if x > lo {
        x
    } else {
        lo
    }
}

fn clamp_hue(h: f64) -> f64 {
    let h = if h.is_finite() { h % 360.0 } else { 0.0 };
    if h < 0.0 {
        h + 360.0
    } else {
        h
    }
}

struct Hsla {
    h: f64,
    s: f64,
    l: f64,
    a: f64,
}

// D50 white point and the Bradford-adapted sRGB conversions of the lab and lch plugins.
const WX: f64 = 96.422;
const WY: f64 = 100.0;
const WZ: f64 = 82.521;
const E: f64 = 216.0 / 24389.0;
const K: f64 = 24389.0 / 27.0;

fn linear(c: f64) -> f64 {
    let t = c / 255.0;
    if t < 0.04045 {
        t / 12.92
    } else {
        ((t + 0.055) / 1.055).powf(2.4)
    }
}

fn gamma(c: f64) -> f64 {
    255.0 * if c > 0.0031308 { 1.055 * c.powf(1.0 / 2.4) - 0.055 } else { 12.92 * c }
}

impl Colord {
    /// colord("#rrggbb") (also #rgb, #rgba, #rrggbbaa).
    pub fn hex(s: &str) -> Option<Colord> {
        let s = s.trim();
        let digits = s.strip_prefix('#')?;
        if !digits.bytes().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let d = |i: usize| u32::from_str_radix(&digits[i..i + 1], 16).unwrap() as f64;
        let h = |i: usize| u32::from_str_radix(&digits[i..i + 2], 16).unwrap() as f64;
        match digits.len() {
            3 | 4 => Some(Colord {
                r: 17.0 * d(0),
                g: 17.0 * d(1),
                b: 17.0 * d(2),
                a: if digits.len() == 4 { round_to(17.0 * d(3) / 255.0, 2) } else { 1.0 },
            }),
            6 | 8 => Some(Colord {
                r: h(0),
                g: h(2),
                b: h(4),
                a: if digits.len() == 8 { round_to(h(6) / 255.0, 2) } else { 1.0 },
            }),
            _ => None,
        }
    }

    /// toRgb(): rounded channels.
    pub fn to_rgb(&self) -> u32 {
        let c = |v: f64| clamp(js_round(v), 0.0, 255.0) as u32;
        rgba(c(self.r), c(self.g), c(self.b))
    }

    /// colord(c.toHex()): the color with rounded channels (and alpha).
    fn via_hex(&self) -> Colord {
        Colord {
            r: clamp(js_round(self.r), 0.0, 255.0),
            g: clamp(js_round(self.g), 0.0, 255.0),
            b: clamp(js_round(self.b), 0.0, 255.0),
            a: if self.a < 1.0 { round_to(clamp(js_round(255.0 * round_to(self.a, 3)), 0.0, 255.0) / 255.0, 2) } else { 1.0 },
        }
    }

    fn to_hsla(&self) -> Hsla {
        // rgba -> hsva -> hsla, as colord does it.
        let (r, g, b) = (self.r, self.g, self.b);
        let max = r.max(g).max(b);
        let delta = max - r.min(g).min(b);
        let hh = if delta != 0.0 {
            if max == r {
                (g - b) / delta
            } else if max == g {
                2.0 + (b - r) / delta
            } else {
                4.0 + (r - g) / delta
            }
        } else {
            0.0
        };
        let h = 60.0 * if hh < 0.0 { hh + 6.0 } else { hh };
        let s = if max != 0.0 { delta / max * 100.0 } else { 0.0 };
        let v = max / 255.0 * 100.0;
        let u = (200.0 - s) * v / 100.0;
        Hsla {
            h,
            s: if u > 0.0 && u < 200.0 { s * v / 100.0 / if u <= 100.0 { u } else { 200.0 - u } * 100.0 } else { 0.0 },
            l: u / 2.0,
            a: self.a,
        }
    }

    /// colord({h, s, l, a}).
    fn from_hsla(c: Hsla) -> Colord {
        let (h, s, l, a) = (clamp_hue(c.h), clamp(c.s, 0.0, 100.0), clamp(c.l, 0.0, 100.0), clamp(c.a, 0.0, 1.0));
        // hsla -> hsva -> rgba
        let s2 = s * if l < 50.0 { l } else { 100.0 - l } / 100.0;
        let hs = if s2 > 0.0 { 2.0 * s2 / (l + s2) * 100.0 } else { 0.0 };
        let v = l + s2;
        let (t, n, e) = (h / 360.0 * 6.0, hs / 100.0, v / 100.0);
        let o = t.floor();
        let lo = e * (1.0 - n);
        let mid1 = e * (1.0 - (t - o) * n);
        let mid2 = e * (1.0 - (1.0 - t + o) * n);
        let d = (o as i64).rem_euclid(6) as usize;
        Colord {
            r: 255.0 * [e, mid1, lo, lo, mid2, e][d],
            g: 255.0 * [mid2, e, e, mid1, lo, lo][d],
            b: 255.0 * [lo, lo, mid2, e, e, mid1][d],
            a,
        }
    }

    /// darken(amount): HSL lightness down by amount * 100.
    pub fn darken(&self, amount: f64) -> Colord {
        let mut hsl = self.to_hsla();
        hsl.l = clamp(hsl.l - 100.0 * amount, 0.0, 100.0);
        Colord::from_hsla(hsl)
    }

    /// The color as D50 XYZ, clamped to the white point.
    fn to_xyz(&self) -> (f64, f64, f64) {
        let (r, g, b) = (linear(self.r), linear(self.g), linear(self.b));
        let (x, y, z) = (
            100.0 * (0.4124564 * r + 0.3575761 * g + 0.1804375 * b),
            100.0 * (0.2126729 * r + 0.7151522 * g + 0.072175 * b),
            100.0 * (0.0193339 * r + 0.119192 * g + 0.9503041 * b),
        );
        let (x, y, z) = (
            1.0478112 * x + 0.0228866 * y + -0.050127 * z,
            0.0295424 * x + 0.9904844 * y + -0.0170491 * z,
            -0.0092345 * x + 0.0150436 * y + 0.7521316 * z,
        );
        (clamp(x, 0.0, WX), clamp(y, 0.0, WY), clamp(z, 0.0, WZ))
    }

    fn from_xyz(x: f64, y: f64, z: f64, a: f64) -> Colord {
        let (x, y, z) = (
            0.9555766 * x + -0.0230393 * y + 0.0631636 * z,
            -0.0282895 * x + 1.0099416 * y + 0.0210077 * z,
            0.0122982 * x + -0.020483 * y + 1.3299098 * z,
        );
        Colord {
            r: clamp(gamma(0.032404542 * x - 0.015371385 * y - 0.004985314 * z), 0.0, 255.0),
            g: clamp(gamma(-0.00969266 * x + 0.018760108 * y + 0.00041556 * z), 0.0, 255.0),
            b: clamp(gamma(0.000556434 * x - 0.002040259 * y + 0.010572252 * z), 0.0, 255.0),
            a: clamp(a, 0.0, 1.0),
        }
    }

    /// Unrounded CIE Lab.
    fn lab_raw(&self) -> (f64, f64, f64) {
        let (x, y, z) = self.to_xyz();
        let f = |v: f64| if v > E { v.cbrt() } else { (K * v + 16.0) / 116.0 };
        let (fx, fy, fz) = (f(x / WX), f(y / WY), f(z / WZ));
        (116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz))
    }

    /// toLab(): rounded to 2 decimals.
    fn to_lab(&self) -> (f64, f64, f64) {
        let (l, a, b) = self.lab_raw();
        (round_to(l, 2), round_to(a, 2), round_to(b, 2))
    }

    /// toLch(): l and c rounded to 2 decimals, h too (mod 360).
    fn to_lch(&self) -> (f64, f64, f64) {
        let (l, a, b) = self.lab_raw();
        let (a, b) = (round_to(a, 3), round_to(b, 3));
        let h = b.atan2(a) / std::f64::consts::PI * 180.0;
        let h = if h < 0.0 { h + 360.0 } else { h };
        (round_to(l, 2), round_to((a * a + b * b).sqrt(), 2), round_to(h, 2) % 360.0)
    }

    /// colord({l, c, h}).
    fn from_lch(l: f64, c: f64, h: f64) -> Colord {
        let (l, h) = (clamp(l, 0.0, 100.0), clamp_hue(h));
        let (la, lb) = (c * (h * std::f64::consts::PI / 180.0).cos(), c * (h * std::f64::consts::PI / 180.0).sin());
        let fy = (l + 16.0) / 116.0;
        let fx = la / 500.0 + fy;
        let fz = fy - lb / 200.0;
        let x = if fx.powi(3) > E { fx.powi(3) } else { (116.0 * fx - 16.0) / K } * WX;
        let y = if l > 8.0 { ((l + 16.0) / 116.0).powi(3) } else { l / K } * WY;
        let z = if fz.powi(3) > E { fz.powi(3) } else { (116.0 * fz - 16.0) / K } * WZ;
        Colord::from_xyz(x, y, z, 1.0)
    }

    /// delta(other): CIEDE2000 / 100, rounded to 3 decimals.
    fn delta(&self, other: &Colord) -> f64 {
        let ((o, h, r), (n, p, m)) = (self.to_lab(), other.to_lab());
        let (u, w) = (180.0 / std::f64::consts::PI, std::f64::consts::PI / 180.0);
        let e = (h * h + r * r).powf(0.5);
        let b = (p * p + m * m).powf(0.5);
        let i = (o + n) / 2.0;
        let l = ((e + b) / 2.0).powi(7);
        let c = 0.5 * (1.0 - (l / (l + 25f64.powi(7))).powf(0.5));
        let f = h * (1.0 + c);
        let y = p * (1.0 + c);
        let v = (f * f + r * r).powf(0.5);
        let x = (y * y + m * m).powf(0.5);
        let z = (v + x) / 2.0;
        let mut s = if f == 0.0 && r == 0.0 { 0.0 } else { r.atan2(f) * u };
        let mut d = if y == 0.0 && m == 0.0 { 0.0 } else { m.atan2(y) * u };
        if s < 0.0 {
            s += 360.0;
        }
        if d < 0.0 {
            d += 360.0;
        }
        let mut g = d - s;
        let dm = (d - s).abs();
        if dm > 180.0 && d <= s {
            g += 360.0;
        } else if dm > 180.0 && d > s {
            g -= 360.0;
        }
        let mut nn = s + d;
        if dm <= 180.0 {
            nn /= 2.0;
        } else {
            nn = (if s + d < 360.0 { nn + 360.0 } else { nn - 360.0 }) / 2.0;
        }
        let ff = 1.0 - 0.17 * (w * (nn - 30.0)).cos() + 0.24 * (2.0 * w * nn).cos() + 0.32 * (w * (3.0 * nn + 6.0)).cos()
            - 0.2 * (w * (4.0 * nn - 63.0)).cos();
        let dl = n - o;
        let dc = x - v;
        let dh = 2.0 * (w * g / 2.0).sin() * (v * x).powf(0.5);
        let sl = 1.0 + 0.015 * (i - 50.0).powi(2) / (20.0 + (i - 50.0).powi(2)).powf(0.5);
        let sc = 1.0 + 0.045 * z;
        let sh = 1.0 + 0.015 * z * ff;
        let a = 30.0 * (-((nn - 275.0) / 25.0).powi(2)).exp();
        let bb = z.powi(7);
        let rt = -2.0 * (bb / (bb + 25f64.powi(7))).powf(0.5) * (2.0 * w * a).sin();
        let de = ((dl / sl).powi(2) + (dc / sc).powi(2) + (dh / sh).powi(2) + rt * dc * dh / (sc * sh)).powf(0.5);
        clamp(round_to(de / 100.0, 3), 0.0, 1.0)
    }
}

// ── The default theme (SettingsTheme) ──

/// ColorAllocator: a distinct color per id from a pool.
struct ColorAllocator {
    available: Vec<Colord>,
    fallback: Vec<Colord>,
    assigned: Vec<(String, Colord)>,
}

impl ColorAllocator {
    fn new(colors: &[Colord], fallback: &[Colord]) -> Self {
        ColorAllocator { available: colors.to_vec(), fallback: [colors, fallback].concat(), assigned: Vec::new() }
    }

    fn assign(&mut self, id: &str) -> Colord {
        if let Some((_, c)) = self.assigned.iter().find(|(k, _)| k == id) {
            return *c;
        }
        if self.available.is_empty() {
            self.available = self.fallback.clone();
        }
        let index = if self.assigned.is_empty() || self.assigned.len() > 50 {
            PseudoRandom::new(simple_hash(id)).next_int(0.0, self.available.len() as f64) as usize
        } else {
            // selectDistinctColorIndex: farthest from its nearest assigned color.
            let mut max_delta = 0.0;
            let mut max_index = 0;
            for (i, c) in self.available.iter().enumerate() {
                let d = self.assigned.iter().fold(f64::INFINITY, |m, (_, a)| m.min(c.delta(a)));
                if d > max_delta {
                    max_delta = d;
                    max_index = i;
                }
            }
            max_index
        };
        let c = self.available.remove(index);
        self.assigned.push((id.to_string(), c));
        c
    }
}

/// generateTeamColors: 64 variations around a team's color.
fn team_colors(base: Colord) -> Vec<Colord> {
    let (l0, c0, h0) = base.to_lch();
    let golden = 137.508;
    (0..64)
        .map(|i| {
            if i == 0 {
                return base;
            }
            let fi = i as f64;
            let hue_shift = ((fi * golden) % 12.0) - 6.0;
            let h = (h0 + hue_shift + 360.0) % 360.0;
            let chroma = 1.0 + 0.1 * (fi * 0.7).sin();
            let c = (c0 * chroma).min(130.0).max(10.0);
            let light = 18.0 * (fi * golden * (std::f64::consts::PI / 180.0)).sin();
            let l = (l0 + light).min(80.0).max(25.0);
            Colord::from_lch(l, c, h)
        })
        .collect()
}

pub struct Theme {
    human: ColorAllocator,
    nation: ColorAllocator,
    team_palettes: HashMap<String, Vec<Colord>>,
    team_player: HashMap<String, Colord>,
    border_darken: f64,
    border_lightness_scale: f64,
}

impl Theme {
    pub fn new(settings: &Value) -> Theme {
        let list = |k: &str| -> Vec<Colord> {
            settings[k].as_array().unwrap().iter().map(|c| Colord::hex(c.as_str().unwrap()).unwrap()).collect()
        };
        let (human, nation, fallback) = (list("humanColors"), list("nationColors"), list("fallbackColors"));
        let mut team_palettes = HashMap::new();
        for (team, hex) in settings["teamColors"].as_object().unwrap() {
            let base = Colord::hex(hex.as_str().unwrap()).unwrap();
            team_palettes.insert(team.clone(), if team == "Bot" { vec![base] } else { team_colors(base) });
        }
        Theme {
            human: ColorAllocator::new(&human, &fallback),
            nation: ColorAllocator::new(&nation, &nation),
            team_palettes,
            team_player: HashMap::new(),
            border_darken: settings["borderDarken"].as_f64().unwrap(),
            border_lightness_scale: settings["borderLightnessScale"].as_f64().unwrap(),
        }
    }

    fn team_color_for_player(&mut self, team: &str, id: &str) -> Colord {
        if let Some(c) = self.team_player.get(id) {
            return *c;
        }
        let colors = match self.team_palettes.get(team) {
            Some(p) => p.clone(),
            None => vec![self.human.assign(team)],
        };
        let c = colors[simple_hash(id) as usize % colors.len()];
        self.team_player.insert(id.to_string(), c);
        c
    }

    /// territoryColor (classic bot colors off, as by default).
    fn territory_color(&mut self, id: &str, player_type: &str, team: Option<&str>) -> Colord {
        if let Some(team) = team {
            return self.team_color_for_player(team, id);
        }
        match player_type {
            "HUMAN" => self.human.assign(id),
            "BOT" => self.team_color_for_player("Bot", id),
            _ => self.nation.assign(id),
        }
    }

    fn border_color(&self, c: Colord) -> Colord {
        let mut out = c;
        if self.border_lightness_scale != 1.0 {
            let mut hsl = out.to_hsla();
            // toHsl() rounds before the lightness is scaled.
            hsl = Hsla { h: js_round(hsl.h) % 360.0, s: js_round(hsl.s), l: js_round(hsl.l), a: round_to(hsl.a, 3) };
            hsl.l *= self.border_lightness_scale;
            out = Colord::from_hsla(hsl);
        }
        if self.border_darken != 0.0 {
            out = out.darken(self.border_darken);
        }
        out
    }

    /// resolvePlayerColors (not the focused player): territory and border.
    pub fn player_colors(&mut self, id: &str, player_type: &str, team: Option<&str>, cosmetics: &Value) -> (u32, u32) {
        let themed = self.territory_color(id, player_type, team);
        let themed_border = self.border_color(themed);
        let pattern = &cosmetics["pattern"];
        let own_color = cosmetics["color"]["color"].as_str().and_then(Colord::hex);
        let (primary, secondary) = if pattern.is_object() {
            match &pattern["colorPalette"] {
                p if p.is_object() => (
                    p["primaryColor"].as_str().and_then(Colord::hex),
                    p["secondaryColor"].as_str().and_then(Colord::hex),
                ),
                _ => (Some(themed.via_hex()), Some(themed_border.via_hex())),
            }
        } else {
            (None, None)
        };
        let territory = if team.is_none() { own_color.or(primary).unwrap_or_else(|| themed.via_hex()) } else { themed };
        let border = secondary.or(own_color).unwrap_or_else(|| themed_border.via_hex());
        (territory.to_rgb(), border.to_rgb())
    }
}

// ── What render.ts takes from render-settings.json ──

pub struct Settings {
    pub terrain: [[u32; 3]; 6],
    pub territory_alpha: f64,
    pub territory_saturation: f64,
    pub stale_nuke_rgb: [f64; 3],
    pub stale_nuke_base: f64,
    pub stale_nuke_variation: f64,
    /// Structure shapes in render.ts STRUCTURES order: (scale, iconFill).
    pub shapes: [(f64, f64); 6],
    pub fill_darken: f64,
    pub border_darken: f64,
    pub name_scale_factor: f64,
    pub name_scale_cap: f64,
    pub troop_size_multiplier: f64,
    pub outline_width: f64,
    /// How far above the name the status icons sit, in name heights.
    pub status_row_offset: f64,
    pub name_shade_nation: f64,
    pub name_shade_bot: f64,
}

pub const STRUCTURES: [&str; 6] = ["City", "Port", "Factory", "Defense Post", "SAM Launcher", "Missile Silo"];

fn hex_rgb(s: &str) -> [u32; 3] {
    let c = Colord::hex(s).unwrap().to_rgb();
    channels(c)
}

impl Settings {
    pub fn new(rs: &Value) -> Settings {
        let t = &rs["terrain"];
        let ov = &rs["mapOverlay"];
        let st = &rs["structure"];
        let name = &rs["name"];
        let f = |v: &Value| v.as_f64().unwrap();
        Settings {
            terrain: ["backgroundColor", "oceanColor", "sandColor", "plainsColor", "highlandColor", "mountainColor"]
                .map(|k| hex_rgb(t[k].as_str().unwrap())),
            territory_alpha: f(&ov["territoryAlpha"]),
            territory_saturation: f(&ov["territorySaturation"]),
            stale_nuke_rgb: [f(&ov["staleNukeR"]), f(&ov["staleNukeG"]), f(&ov["staleNukeB"])],
            stale_nuke_base: f(&ov["staleNukeBase"]),
            stale_nuke_variation: f(&ov["staleNukeVariation"]),
            shapes: STRUCTURES.map(|s| (f(&st["shapes"][s]["scale"]), f(&st["shapes"][s]["iconFill"]))),
            fill_darken: f(&st["fillDarken"]),
            border_darken: f(&st["borderDarken"]),
            name_scale_factor: f(&name["nameScaleFactor"]),
            name_scale_cap: f(&name["nameScaleCap"]),
            troop_size_multiplier: f(&name["troopSizeMultiplier"]),
            outline_width: f(&name["outlineWidth"]),
            status_row_offset: f(&name["statusRowOffset"]),
            name_shade_nation: f(&name["nameShadeNation"]),
            name_shade_bot: f(&name["nameShadeBot"]),
        }
    }

    /// ColorUtils encodeTerrainTile with the settings' terrain colors.
    pub fn terrain_color(&self, tb: u8) -> u32 {
        let [background, ocean, sand, plains, highland, mountain] = self.terrain;
        let is_land = tb & 0x80 != 0;
        let is_shoreline = tb & 0x40 != 0;
        let magnitude = (tb & 0x1f) as u32;
        let [r, g, b] = if is_land && magnitude == 31 {
            background
        } else if is_land && is_shoreline {
            sand
        } else if is_land {
            if magnitude < 10 {
                [plains[0], plains[1] - 2 * magnitude, plains[2]]
            } else if magnitude < 20 {
                let m = magnitude - 10;
                highland.map(|c| (c + 2 * m).min(255))
            } else {
                let m = magnitude / 2;
                mountain.map(|c| (c + m).min(255))
            }
        } else if is_shoreline {
            ocean.map(|c| js_round(0.7 * c as f64 + 76.5) as u32)
        } else {
            let m = magnitude.min(10);
            ocean.map(|c| c.saturating_sub(m))
        };
        rgba(r, g, b)
    }
}

/// A player's colors as render.ts draws them.
#[derive(Clone, Copy)]
pub struct Style {
    /// Territory color.
    pub fill: u32,
    pub border: u32,
    /// The territory color as drawn over terrain (desaturated).
    pub tile_fill: u32,
    pub struct_fill: u32,
    pub struct_border: u32,
    pub name_shade: u32,
}

pub const GRAY: Style = Style {
    fill: 0xff80_8080,
    border: 0xff40_4040,
    tile_fill: 0xff80_8080,
    struct_fill: 0xff80_8080,
    struct_border: 0xff40_4040,
    name_shade: 0xff00_0000,
};

impl Style {
    /// render.ts makeStyle.
    pub fn new(fill: u32, border: u32, player_type: &str, s: &Settings) -> Style {
        let saturate = |c: u32, t: f64| {
            let [r, g, b] = channels(c);
            let luma = js_round(0.299 * r as f64 + 0.587 * g as f64 + 0.114 * b as f64) as u32;
            mix(rgba(luma, luma, luma), c, t)
        };
        let darken_value = |c: u32, v: f64| {
            let [r, g, b] = channels(c);
            let d = |x: u32| js_round(x as f64 * v) as u32;
            rgba(d(r), d(g), d(b))
        };
        let shade = match player_type {
            "NATION" => s.name_shade_nation,
            "BOT" => s.name_shade_bot,
            _ => 0.0,
        };
        let g = js_round(shade * 255.0) as u32;
        Style {
            fill,
            border,
            tile_fill: saturate(fill, s.territory_saturation),
            struct_fill: darken_value(fill, s.fill_darken),
            struct_border: darken_value(border, s.border_darken),
            name_shade: rgba(g, g, g),
        }
    }
}

/// engine-lib renderTroops: troops are stored x10.
pub fn troop_text(troops: f64) -> String {
    let num = (troops / 10.0).max(0.0);
    let fixed = |v: f64, d: usize| format!("{v:.d$}");
    if num >= 10_000_000_000.0 {
        fixed((num / 100000000.0).floor() / 10.0, 1) + "B"
    } else if num >= 1_000_000_000.0 {
        fixed((num / 10000000.0).floor() / 100.0, 2) + "B"
    } else if num >= 10_000_000.0 {
        fixed((num / 100000.0).floor() / 10.0, 1) + "M"
    } else if num >= 1_000_000.0 {
        fixed((num / 10000.0).floor() / 100.0, 2) + "M"
    } else if num >= 100000.0 {
        format!("{}K", (num / 1000.0).floor())
    } else if num >= 10000.0 {
        fixed((num / 100.0).floor() / 10.0, 1) + "K"
    } else if num >= 1000.0 {
        fixed((num / 10.0).floor() / 100.0, 2) + "K"
    } else {
        format!("{}", num.floor())
    }
}
