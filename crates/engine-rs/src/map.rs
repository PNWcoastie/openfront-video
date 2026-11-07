//! engine-lib GameMapImpl: immutable terrain bytes plus a mutable u16 state
//! per tile (owner in the low 12 bits, fallout at bit 13).

use crate::fxhash::FxHashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
const STATIC_OCEAN_SHORE: u8 = 1 << 0;
const STATIC_ON_EDGE: u8 = 1 << 1;
const STATIC_INTERIOR: u8 = 1 << 2;

pub struct GameMap {
    pub width: u32,
    pub height: u32,
    terrain: Vec<u8>,
    static_flags: Vec<u8>,
    state: Vec<u16>,
    pub num_land_tiles: u32,
    num_tiles_with_fallout: u32,
}

impl GameMap {
    pub fn new(width: u32, height: u32, terrain: Vec<u8>, num_land_tiles: u32) -> Self {
        assert_eq!(terrain.len(), (width * height) as usize, "terrain size");
        let len = terrain.len();
        let mut map = GameMap {
            width,
            height,
            state: vec![0; len],
            static_flags: vec![0; len],
            terrain,
            num_land_tiles,
            num_tiles_with_fallout: 0,
        };
        let mut r = 0u32;
        for y in 0..height {
            for x in 0..width {
                let interior = x > 0 && x < width - 1 && y > 0 && y < height - 1;
                let mut flags = if interior { STATIC_INTERIOR } else { 0 };
                if map.is_land(r)
                    && ((x != 0 && map.is_ocean(r - 1))
                        || (x != width - 1 && map.is_ocean(r + 1))
                        || (y != 0 && map.is_ocean(r - width))
                        || (y != height - 1 && map.is_ocean(r + width)))
                {
                    flags |= STATIC_OCEAN_SHORE;
                }
                if !interior
                    || map.is_impassable(r - 1)
                    || map.is_impassable(r + 1)
                    || map.is_impassable(r - width)
                    || map.is_impassable(r + width)
                {
                    flags |= STATIC_ON_EDGE;
                }
                map.static_flags[r as usize] = flags;
                r += 1;
            }
        }
        map
    }

    #[inline]
    pub fn terrain_byte(&self, r: u32) -> u8 {
        self.terrain[r as usize]
    }

    #[inline]
    pub fn num_tiles(&self) -> u32 {
        self.width * self.height
    }
    #[inline]
    pub fn x(&self, r: u32) -> u32 {
        r % self.width
    }
    #[inline]
    pub fn y(&self, r: u32) -> u32 {
        r / self.width
    }
    #[inline]
    pub fn xy(&self, r: u32) -> (u32, u32) {
        (r % self.width, r / self.width)
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
    #[inline]
    pub fn is_border(&self, r: u32) -> bool {
        let w = self.width;
        let o = self.owner_id(r);
        if self.static_flags[r as usize] & STATIC_INTERIOR != 0 {
            return self.owner_id(r - 1) != o
                || self.owner_id(r + 1) != o
                || self.owner_id(r - w) != o
                || self.owner_id(r + w) != o;
        }
        let x = self.x(r);
        (x != 0 && self.owner_id(r - 1) != o)
            || (x != w - 1 && self.owner_id(r + 1) != o)
            || (r >= w && self.owner_id(r - w) != o)
            || (r < (self.height - 1) * w && self.owner_id(r + w) != o)
    }

    /// North, south, west, east (the engine's order).
    #[inline]
    pub fn neighbors4(&self, r: u32, out: &mut [u32; 4]) -> usize {
        let w = self.width;
        if self.static_flags[r as usize] & STATIC_INTERIOR != 0 {
            out[0] = r - w;
            out[1] = r + w;
            out[2] = r - 1;
            out[3] = r + 1;
            return 4;
        }
        let x = self.x(r);
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
    #[inline]
    pub fn neighbors8(&self, r: u32, out: &mut [u32; 8]) -> usize {
        let w = self.width;
        if self.static_flags[r as usize] & STATIC_INTERIOR != 0 {
            out[0] = r - 1 - w;
            out[1] = r - 1;
            out[2] = r - 1 + w;
            out[3] = r - w;
            out[4] = r + w;
            out[5] = r + 1 - w;
            out[6] = r + 1;
            out[7] = r + 1 + w;
            return 8;
        }
        let x = self.x(r);
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
    #[inline]
    pub fn is_ocean_shore(&self, r: u32) -> bool {
        self.static_flags[r as usize] & STATIC_OCEAN_SHORE != 0
    }

    /// On the map's edge, or next to impassable terrain.
    #[inline]
    pub fn is_on_edge_of_map(&self, r: u32) -> bool {
        self.static_flags[r as usize] & STATIC_ON_EDGE != 0
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
        let mut seen = FxHashSet::with_capacity_and_hasher(64, Default::default());
        let mut q = Vec::new();
        if filter(self, tile) {
            seen.insert(tile);
            seen_order.push(tile);
            q.push(tile);
        }
        let w = self.width;
        let south_limit = (self.height - 1) * w;
        while let Some(curr) = q.pop() {
            let x = self.x(curr);
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
