//! engine-lib GameMapImpl: immutable terrain bytes plus a mutable u16 state
//! per tile (owner in the low 12 bits, fallout at bit 13).

#[derive(Debug, PartialEq)]
pub enum Terrain {
    Plains,
    Highland,
    Mountain,
    Impassable,
    Ocean,
}

const IS_LAND_BIT: u8 = 7;
const SHORELINE_BIT: u8 = 6;
const OCEAN_BIT: u8 = 5;
const MAGNITUDE_MASK: u8 = 0x1f;
const IMPASSABLE_MAGNITUDE: u8 = 31;
const PLAYER_ID_MASK: u16 = 0xfff;
const FALLOUT_BIT: u16 = 13;

pub struct GameMap {
    pub width: u32,
    pub height: u32,
    terrain: Vec<u8>,
    state: Vec<u16>,
    pub num_land_tiles: u32,
    num_tiles_with_fallout: u32,
}

impl GameMap {
    pub fn new(width: u32, height: u32, terrain: Vec<u8>, num_land_tiles: u32) -> Self {
        assert_eq!(terrain.len(), (width * height) as usize, "terrain size");
        GameMap {
            width,
            height,
            state: vec![0; terrain.len()],
            terrain,
            num_land_tiles,
            num_tiles_with_fallout: 0,
        }
    }

    pub fn terrain_byte(&self, r: u32) -> u8 {
        self.terrain[r as usize]
    }

    pub fn num_tiles(&self) -> u32 {
        self.width * self.height
    }
    pub fn x(&self, r: u32) -> u32 {
        r % self.width
    }
    pub fn y(&self, r: u32) -> u32 {
        r / self.width
    }
    pub fn ref_at(&self, x: u32, y: u32) -> u32 {
        assert!(x < self.width && y < self.height, "invalid coordinates {x},{y}");
        y * self.width + x
    }
    pub fn is_land(&self, r: u32) -> bool {
        self.terrain[r as usize] & (1 << IS_LAND_BIT) != 0
    }
    pub fn is_impassable(&self, r: u32) -> bool {
        self.is_land(r) && self.terrain[r as usize] & MAGNITUDE_MASK == IMPASSABLE_MAGNITUDE
    }
    /// Owner and fallout of every tile, packed like `owner_id | has_fallout << 16`
    /// reads them (other bits are internal), for diffing whole maps quickly.
    pub fn raw_state(&self) -> &[u16] {
        &self.state
    }

    pub fn unpack_owner_fallout(s: u16) -> u32 {
        (s & PLAYER_ID_MASK) as u32 | if s & (1 << FALLOUT_BIT) != 0 { 1 << 16 } else { 0 }
    }

    pub fn owner_id(&self, r: u32) -> u16 {
        self.state[r as usize] & PLAYER_ID_MASK
    }
    pub fn has_owner(&self, r: u32) -> bool {
        self.owner_id(r) != 0
    }
    pub fn set_owner_id(&mut self, r: u32, id: u16) {
        assert!(id <= PLAYER_ID_MASK, "player id {id} too large");
        let s = &mut self.state[r as usize];
        *s = (*s & !PLAYER_ID_MASK) | id;
    }
    pub fn has_fallout(&self, r: u32) -> bool {
        self.state[r as usize] & (1 << FALLOUT_BIT) != 0
    }
    pub fn set_fallout(&mut self, r: u32, value: bool) {
        let had = self.has_fallout(r);
        if value == had {
            return;
        }
        let s = &mut self.state[r as usize];
        if value {
            *s |= 1 << FALLOUT_BIT;
            self.num_tiles_with_fallout += 1;
        } else {
            *s &= !(1 << FALLOUT_BIT);
            self.num_tiles_with_fallout -= 1;
        }
    }

    /// Any 4-neighbor with a different owner (map edges don't count).
    pub fn is_border(&self, r: u32) -> bool {
        let w = self.width;
        let x = r % w;
        let o = self.owner_id(r);
        (x != 0 && self.owner_id(r - 1) != o)
            || (x != w - 1 && self.owner_id(r + 1) != o)
            || (r >= w && self.owner_id(r - w) != o)
            || (r < (self.height - 1) * w && self.owner_id(r + w) != o)
    }

    /// North, south, west, east (the engine's order).
    pub fn neighbors4(&self, r: u32, out: &mut [u32; 4]) -> usize {
        let w = self.width;
        let x = r % w;
        let mut n = 0;
        if r >= w {
            out[n] = r - w;
            n += 1;
        }
        if r < (self.height - 1) * w {
            out[n] = r + w;
            n += 1;
        }
        if x != 0 {
            out[n] = r - 1;
            n += 1;
        }
        if x != w - 1 {
            out[n] = r + 1;
            n += 1;
        }
        n
    }

    /// West column (NW, W, SW), then N, S, then east column (NE, E, SE).
    pub fn neighbors8(&self, r: u32, out: &mut [u32; 8]) -> usize {
        let w = self.width;
        let x = r % w;
        let has_n = r >= w;
        let has_s = r < (self.height - 1) * w;
        let mut n = 0;
        let mut push = |v: u32, n: &mut usize| {
            out[*n] = v;
            *n += 1;
        };
        if x != 0 {
            if has_n {
                push(r - 1 - w, &mut n);
            }
            push(r - 1, &mut n);
            if has_s {
                push(r - 1 + w, &mut n);
            }
        }
        if has_n {
            push(r - w, &mut n);
        }
        if has_s {
            push(r + w, &mut n);
        }
        if x != w - 1 {
            if has_n {
                push(r + 1 - w, &mut n);
            }
            push(r + 1, &mut n);
            if has_s {
                push(r + 1 + w, &mut n);
            }
        }
        n
    }

    pub fn is_water(&self, r: u32) -> bool {
        !self.is_land(r)
    }
    pub fn is_ocean(&self, r: u32) -> bool {
        self.terrain[r as usize] & (1 << OCEAN_BIT) != 0
    }
    pub fn is_shoreline(&self, r: u32) -> bool {
        self.terrain[r as usize] & (1 << SHORELINE_BIT) != 0
    }
    pub fn is_shore(&self, r: u32) -> bool {
        self.is_land(r) && self.is_shoreline(r)
    }
    pub fn magnitude(&self, r: u32) -> u8 {
        self.terrain[r as usize] & MAGNITUDE_MASK
    }
    pub fn num_tiles_with_fallout(&self) -> u32 {
        self.num_tiles_with_fallout
    }

    /// Land with ocean on a side.
    pub fn is_ocean_shore(&self, r: u32) -> bool {
        if !self.is_land(r) {
            return false;
        }
        let w = self.width;
        let x = r % w;
        (x != 0 && self.is_ocean(r - 1))
            || (x != w - 1 && self.is_ocean(r + 1))
            || (r >= w && self.is_ocean(r - w))
            || (r < (self.height - 1) * w && self.is_ocean(r + w))
    }

    /// On the map's edge, or next to impassable terrain.
    pub fn is_on_edge_of_map(&self, r: u32) -> bool {
        let w = self.width;
        let x = r % w;
        if x == 0 || x == w - 1 || r < w || r >= (self.height - 1) * w {
            return true;
        }
        self.is_impassable(r - 1) || self.is_impassable(r + 1) || self.is_impassable(r - w) || self.is_impassable(r + w)
    }

    pub fn terrain_type(&self, r: u32) -> Terrain {
        if !self.is_land(r) {
            return Terrain::Ocean;
        }
        let m = self.magnitude(r);
        if m >= IMPASSABLE_MAGNITUDE {
            Terrain::Impassable
        } else if m < 10 {
            Terrain::Plains
        } else if m < 20 {
            Terrain::Highland
        } else {
            Terrain::Mountain
        }
    }

    pub fn manhattan_dist(&self, a: u32, b: u32) -> u32 {
        self.x(a).abs_diff(self.x(b)) + self.y(a).abs_diff(self.y(b))
    }

    /// GameMapImpl.bfs: despite the name it pops from the end (depth first);
    /// returns tiles in the order they were first seen.
    pub fn bfs(&self, tile: u32, filter: impl Fn(&GameMap, u32) -> bool) -> Vec<u32> {
        let mut seen_order = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut q = Vec::new();
        if filter(self, tile) {
            seen.insert(tile);
            seen_order.push(tile);
            q.push(tile);
        }
        let w = self.width;
        let south_limit = (self.height - 1) * w;
        while let Some(curr) = q.pop() {
            let x = curr % w;
            let mut visit = |n: u32| {
                if !seen.contains(&n) && filter(self, n) {
                    seen.insert(n);
                    seen_order.push(n);
                    q.push(n);
                }
            };
            if curr >= w {
                visit(curr - w);
            }
            if curr < south_limit {
                visit(curr + w);
            }
            if x != 0 {
                visit(curr - 1);
            }
            if x != w - 1 {
                visit(curr + 1);
            }
        }
        seen_order
    }
}

/// euclDistFN(root, dist, center = true): within `dist` of the corner
/// between the root's four pixels.
pub fn eucl_dist_center(map: &GameMap, root: u32, dist: f64, n: u32) -> bool {
    let root_x = map.x(root) as f64 - 0.5;
    let root_y = map.y(root) as f64 - 0.5;
    let dx = map.x(n) as f64 - root_x;
    let dy = map.y(n) as f64 - root_y;
    dx * dx + dy * dy <= dist * dist
}
