use super::*;

// ── What the video needs: name placement (NameBoxCalculator) and units as
// render.ts draws them ──

/// Status icon bits in frame_status.
pub const STATUS_CROWN: u8 = 1;
pub const STATUS_TRAITOR: u8 = 2;
pub const STATUS_DISCONNECTED: u8 = 4;
pub const STATUS_NUKE: u8 = 8;
pub const STATUS_DOOMSDAY: u8 = 16;

/// render.ts STRUCTURES order; structure codes start at 16.
const RENDER_STRUCTURES: [&str; 6] = [CITY, PORT, FACTORY, DEFENSE_POST, SAM_LAUNCHER, MISSILE_SILO];

impl Game {
    /// PlayerInfo.displayName
    pub fn display_name(&self, p: usize) -> String {
        let info = &self.players[p].info;
        match &info.clan_tag {
            Some(tag) if !tag.is_empty() => format!("[{tag}] {}", info.name),
            _ => info.name.clone(),
        }
    }

    pub(super) fn name_font_size(width: f64, height: f64, name: &str) -> f64 {
        let len = name.encode_utf16().count() as f64;
        js_min((width / len) * 2.0, height / 3.0)
    }

    /// NameBoxCalculator.placeSpawnName
    pub fn place_spawn_name(&self, p: usize) -> Option<(f64, f64, f64)> {
        let tile = self.players[p].spawn_tile?;
        let size = Self::name_font_size(8.0, 8.0, &self.display_name(p));
        let (x, y) = (self.map.x(tile) as f64, self.map.y(tile) as f64);
        Some((x.ceil(), (y - size / 3.0).ceil(), size))
    }

    /// NameBoxCalculator.placeName
    pub fn place_name(&self, p: usize) -> (f64, f64, f64) {
        let [min_x, min_y, max_x, max_y] = match self.players[p].largest_cluster_box {
            Some(b) => b,
            None => {
                let mut b = [i64::MAX, i64::MAX, i64::MIN, i64::MIN];
                for t in self.players[p].border_tiles.values() {
                    let (x, y) = (self.map.x(t) as i64, self.map.y(t) as i64);
                    b = [b[0].min(x), b[1].min(y), b[2].max(x), b[3].max(y)];
                }
                b
            }
        };
        let size = (max_x - min_x).min(max_y - min_y);
        let sf: i64 = match size {
            s if s < 25 => 1,
            s if s < 50 => 2,
            s if s < 100 => 4,
            s if s < 250 => 8,
            s if s < 500 => 16,
            _ => 32,
        };
        let (sx0, sy0, sx1, sy1) = (min_x.div_euclid(sf), min_y.div_euclid(sf), max_x.div_euclid(sf), max_y.div_euclid(sf));
        let cols = (sx1 - sx0 + 1) as usize;
        let rows = (sy1 - sy0 + 1) as usize;
        let small = self.players[p].small_id;
        let (w, h) = (self.map.width as i64, self.map.height as i64);
        let cell = |cx: usize, ry: usize| -> bool {
            let (x, y) = ((cx as i64 + sx0) * sf, (ry as i64 + sy0) * sf);
            if x < 0 || y < 0 || x >= w || y >= h {
                return false;
            }
            let t = (y * w + x) as u32;
            self.map.is_shore(t) || (self.map.is_ocean(t) && self.map.magnitude(t) < 10) || self.map.owner_id(t) == small || self.map.has_fallout(t)
        };
        // findLargestInscribedRectangle
        let mut heights = vec![0i64; cols];
        let mut stack: Vec<usize> = Vec::with_capacity(cols + 1);
        let mut best = (0i64, 0i64, 0i64, 0i64); // x, y, width, height
        for row in 0..rows {
            for (col, hgt) in heights.iter_mut().enumerate() {
                if cell(col, row) {
                    *hgt += 1;
                } else {
                    *hgt = 0;
                }
            }
            // largestRectangleInHistogram
            stack.clear();
            let mut max_area = 0;
            let mut rect = (0i64, 0i64, 0i64);
            for i in 0..=cols {
                let hh = if i == cols { 0 } else { heights[i] };
                while let Some(&top) = stack.last() {
                    if hh >= heights[top] {
                        break;
                    }
                    stack.pop();
                    let height = heights[top];
                    let width = match stack.last() {
                        None => i as i64,
                        Some(&s) => i as i64 - s as i64 - 1,
                    };
                    if height * width > max_area {
                        max_area = height * width;
                        rect = (stack.last().map_or(0, |&s| s as i64 + 1), width, height);
                    }
                }
                stack.push(i);
            }
            if rect.1 * rect.2 > best.2 * best.3 {
                best = (rect.0, row as i64 - rect.2 + 1, rect.1, rect.2);
            }
        }
        let (rx, ry, rw, rh) = ((best.0 * sf) as f64, (best.1 * sf) as f64, (best.2 * sf) as f64, (best.3 * sf) as f64);
        let cx = (rx + rw / 2.0 + min_x as f64).floor();
        let cy = (ry + rh / 2.0 + min_y as f64).floor();
        let size = Self::name_font_size(rw, rh, &self.display_name(p));
        (cx.ceil(), (cy - size / 3.0).ceil(), size)
    }

    /// Active units as render.ts collectUnits builds them: [code, tile, owner, underConstruction].
    /// The client's status icons for each alive player (PlayerStatus.ts,
    /// replay mode): STATUS_* bits by small ID. Only players with a bit set.
    pub fn frame_status(&self) -> Vec<(u16, u8)> {
        // Crown: the alive player with the most tiles (the first on ties).
        let mut crown = None;
        let mut max_tiles = 0;
        for p in &self.players {
            if p.is_alive() && p.tiles.len() > max_tiles {
                max_tiles = p.tiles.len();
                crown = Some(p.small_id);
            }
        }
        let mut nuking: Vec<u16> = Vec::new();
        for p in &self.players {
            for &u in &p.units {
                let unit = &self.units[u];
                if unit.active && matches!(unit.type_name, ATOM_BOMB | HYDROGEN_BOMB | "MIRV" | MIRV_WARHEAD) && !nuking.contains(&p.small_id) {
                    nuking.push(p.small_id);
                }
            }
        }
        let mut out = Vec::new();
        for (i, p) in self.players.iter().enumerate() {
            if !p.is_alive() {
                continue;
            }
            let mut bits = 0u8;
            if crown == Some(p.small_id) {
                bits |= STATUS_CROWN;
            }
            if self.is_traitor(i) {
                bits |= STATUS_TRAITOR;
            }
            if p.is_disconnected {
                bits |= STATUS_DISCONNECTED;
            }
            if nuking.contains(&p.small_id) {
                bits |= STATUS_NUKE;
            }
            if self.in_doomsday(i) {
                bits |= STATUS_DOOMSDAY;
            }
            if bits != 0 {
                out.push((p.small_id, bits));
            }
        }
        out
    }

    /// NukeTelegraphs: the target and blast radii (inner, outer) of every
    /// nuke in flight that has launched (a MIRV warhead waits before it does).
    pub fn frame_nuke_targets(&self) -> Vec<(u32, u16, u16)> {
        let mut out = Vec::new();
        for p in &self.players {
            for &u in &p.units {
                let unit = &self.units[u];
                if !unit.active || !matches!(unit.type_name, ATOM_BOMB | HYDROGEN_BOMB | MIRV_WARHEAD) {
                    continue;
                }
                let Some(target) = unit.target_tile else { continue };
                if unit.nuke.as_ref().is_some_and(|n| n.wait_ticks > 0) {
                    continue;
                }
                let (inner, outer) = Self::nuke_magnitude(unit.type_name);
                out.push((target, inner as u16, outer as u16));
            }
        }
        out
    }

    pub fn frame_units(&self) -> Vec<i32> {
        let mut out = Vec::new();
        for p in &self.players {
            for &u in &p.units {
                let unit = &self.units[u];
                let code = if let Some(s) = RENDER_STRUCTURES.iter().position(|&t| t == unit.type_name) {
                    16 + s as i32
                } else {
                    match unit.type_name {
                        TRAIN => {
                            if unit.train_engine {
                                9
                            } else {
                                10
                            }
                        }
                        TRANSPORT_SHIP => 0,
                        TRADE_SHIP => 1,
                        WARSHIP => 2,
                        ATOM_BOMB => 3,
                        HYDROGEN_BOMB => 4,
                        "MIRV" => 5,
                        SAM_MISSILE => 6,
                        SHELL => 7,
                        MIRV_WARHEAD => 8,
                        _ => continue,
                    }
                };
                out.extend_from_slice(&[code, unit.tile as i32, p.small_id as i32, unit.under_construction as i32]);
            }
        }
        out
    }
}

