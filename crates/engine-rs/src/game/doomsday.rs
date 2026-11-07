//! The Doomsday Clock (DoomsdayClockExecution and engine-lib DoomsdayClock.ts):
//! once a game passes its grace period, every side must hold a rising share of
//! the map. A side below the bar, other than the leader, is marked; after a
//! warning its troops and warships bleed down to a decaying floor, and then its
//! territory rots into fallout until it is gone. Integer math throughout, as in
//! the engine.

use std::collections::HashMap;

use super::{Game, PlayerType, WARSHIP};

// The "normal" etc. presets: (grace, ramp per wave, pause per wave).
const LEVELS: [i64; 7] = [200, 400, 700, 1100, 1700, 2500, 3500];
const LEVELS_TEAM: [i64; 7] = [300, 600, 1000, 1500, 2100, 2800, 3500];

fn schedule(speed: &str) -> (f64, f64, f64) {
    match speed {
        "slow" => (600.0, 240.0, 70.0),
        "fast" => (600.0, 102.0, 31.0),
        "veryfast" => (600.0, 36.0, 8.0),
        _ => (600.0, 168.0, 54.0),
    }
}

// DOOMSDAY_CLOCK_DEFAULTS (not configurable on the wire).
const WARN_SECONDS: i64 = 30;
const DRAIN_START: i64 = 2;
const DRAIN_MAX: i64 = 5;
const DRAIN_RAMP_SECONDS: i64 = 90;
const DRAIN_FLOOR: i64 = 5;
const FLOOR_START: i64 = 40;
const FLOOR_DECAY_SECONDS: i64 = 90;
const ROT_DEATH_SECONDS: i64 = 150;
const ROT_GRAIN_SECONDS: i64 = 10;
const ROT_SPECKLE_PERCENT: i64 = 15;
const WARSHIP_DRAIN_START: i64 = 1;
const WARSHIP_DRAIN_MAX: i64 = 50;
const WARSHIP_DRAIN_EXPONENT: u32 = 8;
const ROT_NOISE_SCALE: u32 = 1 << 16;
const DRAIN_CURVE_SCALE: i64 = 1_000_000;

/// requiredBasisPoints: 0 through the grace, then a linear ramp to each level
/// with a flat pause after each.
fn required_basis_points(speed: &str, team: bool, elapsed: f64) -> f64 {
    let (grace, ramp, pause) = schedule(speed);
    let levels = if team { &LEVELS_TEAM } else { &LEVELS };
    if elapsed <= grace {
        return 0.0;
    }
    let mut t = elapsed - grace;
    let mut prev = 0.0;
    for (i, &target) in levels.iter().enumerate() {
        let target = target as f64;
        if t < ramp {
            return prev + (((target - prev) * t) / ramp).floor();
        }
        t -= ramp;
        let pause = if i == levels.len() - 1 { 0.0 } else { pause };
        if t < pause {
            return target;
        }
        t -= pause;
        prev = target;
    }
    *levels.last().unwrap() as f64
}

fn required_tiles(speed: &str, team: bool, land: f64, elapsed: f64) -> f64 {
    if land <= 0.0 {
        return 0.0;
    }
    ((required_basis_points(speed, team, elapsed) * land) / 10000.0).floor()
}

fn troop_floor(max: f64, seconds_past_warn: i64) -> f64 {
    let span = FLOOR_START - DRAIN_FLOOR;
    let t = seconds_past_warn.max(0);
    let pct = if span > 0 && t < FLOOR_DECAY_SECONDS { FLOOR_START - (span * t).div_euclid(FLOOR_DECAY_SECONDS) } else { DRAIN_FLOOR };
    ((max * pct as f64) / 100.0).floor()
}

/// doomsdayClockDrain: a ramp from start to max percent of `max`, linear or
/// (exponent > 1) through the fixed-point convex curve.
fn drain(max: f64, seconds_past_warn: i64, start: i64, end: i64, exponent: u32) -> f64 {
    let t = seconds_past_warn.max(0);
    let r = DRAIN_RAMP_SECONDS;
    let span = end - start;
    let mut pct = end;
    if t < r {
        let grown = if exponent <= 1 {
            (span * t).div_euclid(r)
        } else {
            let ratio = (t * DRAIN_CURVE_SCALE).div_euclid(r);
            let mut acc = DRAIN_CURVE_SCALE;
            for _ in 0..exponent {
                acc = (acc * ratio).div_euclid(DRAIN_CURVE_SCALE);
            }
            (span * acc).div_euclid(DRAIN_CURVE_SCALE)
        };
        pct = start + grown;
    }
    1f64.max(((max * pct as f64) / 100.0).floor())
}

fn rot_quota(tiles_left: i64, seconds_under: i64) -> i64 {
    if tiles_left <= 0 {
        return 0;
    }
    let seconds_left = (ROT_DEATH_SECONDS - seconds_under).max(1);
    (tiles_left + seconds_left - 1) / seconds_left
}

/// rotSpeckleNoise: an evenly spread (R2 lattice) per-tile value.
fn speckle_noise(x: u32, y: u32, salt: u32) -> u32 {
    let h = x.wrapping_mul(3242174889).wrapping_add(y.wrapping_mul(2447445413)).wrapping_add(salt.wrapping_mul(0x9e3779b9));
    (h >> 16) % ROT_NOISE_SCALE
}

/// rotFrontNoise: a hashed per-tile value.
fn front_noise(tile: u32, salt: u32) -> u32 {
    let mut h = tile.wrapping_mul(0x27d4eb2d) ^ salt.wrapping_mul(0x9e3779b9);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2545f491);
    h ^= h >> 13;
    (h >> 16) % ROT_NOISE_SCALE
}

/// LowestN: the `n` lowest-keyed tiles offered, in key order (earlier offers win ties).
struct LowestN {
    n: usize,
    tiles: Vec<u32>,
    keys: Vec<u32>,
}

impl LowestN {
    fn new(n: usize) -> Self {
        LowestN { n, tiles: Vec::new(), keys: Vec::new() }
    }
    fn offer(&mut self, tile: u32, key: u32) {
        if self.tiles.len() == self.n && key >= self.keys[self.tiles.len() - 1] {
            return;
        }
        let mut i = self.keys.len();
        while i > 0 && self.keys[i - 1] > key {
            i -= 1;
        }
        self.tiles.insert(i, tile);
        self.keys.insert(i, key);
        if self.tiles.len() > self.n {
            self.tiles.pop();
            self.keys.pop();
        }
    }
}

/// A Map<TileRef, number> with JavaScript's insertion order.
#[derive(Default, Clone)]
struct Front {
    entries: Vec<Option<(u32, u32)>>,
    index: HashMap<u32, usize>,
    live: usize,
}

impl Front {
    fn get(&self, t: u32) -> Option<u32> {
        self.index.get(&t).map(|&i| self.entries[i].unwrap().1)
    }
    fn set(&mut self, t: u32, v: u32) {
        match self.index.get(&t) {
            Some(&i) => self.entries[i] = Some((t, v)),
            None => {
                self.index.insert(t, self.entries.len());
                self.entries.push(Some((t, v)));
                self.live += 1;
            }
        }
    }
    fn delete(&mut self, t: u32) {
        if let Some(i) = self.index.remove(&t) {
            self.entries[i] = None;
            self.live -= 1;
        }
    }
    fn iter(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        self.entries.iter().flatten().copied()
    }
}

struct RotState {
    since: u32,
    held: i64,
    front: Front,
}

#[derive(Default)]
pub struct DoomsdayExec {
    pub active: bool,
    rot: HashMap<u16, RotState>,
}

impl DoomsdayExec {
    pub fn new() -> Self {
        DoomsdayExec { active: true, rot: HashMap::new() }
    }
}

impl Game {
    pub(super) fn in_doomsday(&self, p: usize) -> bool {
        self.players[p].is_alive() && self.players[p].marked_doomsday_tick >= 0
    }

    pub(super) fn tick_doomsday(&mut self, de: &mut DoomsdayExec) {
        if self.ticks % 10 != 0 {
            return;
        }
        let Some(speed) = self.config.doomsday_speed.clone() else { return };
        let elapsed = self.ticks_since_start() as f64 / 10.0;
        let contenders: Vec<usize> = self.alive_players().into_iter().filter(|&p| self.players[p].info.player_type != PlayerType::Bot).collect();
        let ffa = !self.config.is_team_mode;
        let sides: Vec<Vec<usize>> = if ffa {
            contenders.iter().map(|&p| vec![p]).collect()
        } else {
            let mut order: Vec<(String, Vec<usize>)> = Vec::new();
            for &p in &contenders {
                let Some(team) = self.players[p].team.clone() else { continue };
                match order.iter_mut().find(|(t, _)| *t == team) {
                    Some(e) => e.1.push(p),
                    None => order.push((team, vec![p])),
                }
            }
            order.into_iter().map(|e| e.1).collect()
        };
        if self.winner.is_some() || sides.len() < 2 {
            for &p in &contenders {
                self.players[p].marked_doomsday_tick = -1;
            }
            de.active = false;
            return;
        }
        let land = self.map.num_land_tiles as f64 - self.map.num_tiles_with_fallout() as f64;
        let required = required_tiles(&speed, !ffa, land, elapsed);
        let side_tiles: Vec<usize> = sides.iter().map(|m| m.iter().map(|&p| self.players[p].tiles.len()).sum()).collect();
        let mut leader = 0;
        for i in 1..side_tiles.len() {
            if side_tiles[i] > side_tiles[leader] {
                leader = i;
            }
        }
        for (i, members) in sides.iter().enumerate() {
            if i != leader && (side_tiles[i] as f64) < required {
                for &m in members {
                    if self.players[m].marked_doomsday_tick < 0 {
                        self.players[m].marked_doomsday_tick = self.ticks as i64;
                    }
                    let ticks_under = if self.in_doomsday(m) { self.ticks as i64 - self.players[m].marked_doomsday_tick } else { 0 };
                    let seconds_under = ticks_under.div_euclid(10);
                    if seconds_under < WARN_SECONDS {
                        continue;
                    }
                    let past_warn = seconds_under - WARN_SECONDS;
                    let max = self.max_troops(m);
                    let floor = troop_floor(max, past_warn);
                    let chunk = drain(max, past_warn, DRAIN_START, DRAIN_MAX, 1);
                    let above = (self.players[m].troops_f() - floor).max(0.0);
                    self.remove_troops(m, chunk.min(above));
                    if past_warn >= FLOOR_DECAY_SECONDS && self.players[m].troops_f() <= floor {
                        self.rot(de, m, seconds_under);
                    }
                    for ws in self.player_units_of(m, WARSHIP) {
                        let max_health = self.max_health(ws) as f64;
                        let ship_floor = ((max_health * DRAIN_FLOOR as f64) / 100.0).floor();
                        let removable = (self.units[ws].health as f64 - ship_floor).max(0.0);
                        if removable <= 0.0 {
                            continue;
                        }
                        let dmg = drain(max_health, past_warn, WARSHIP_DRAIN_START, WARSHIP_DRAIN_MAX, WARSHIP_DRAIN_EXPONENT);
                        self.modify_health(ws, -dmg.min(removable), None);
                    }
                }
            } else {
                for &m in members {
                    self.players[m].marked_doomsday_tick = -1;
                    de.rot.remove(&self.players[m].small_id);
                }
            }
        }
    }

    /// Takes territory so the player holds none ROT_DEATH_SECONDS after the mark.
    fn rot(&mut self, de: &mut DoomsdayExec, p: usize, seconds_under: i64) {
        let owned = self.players[p].tiles.len() as i64;
        if owned <= 0 {
            return;
        }
        let id = self.players[p].small_id;
        let state = de.rot.remove(&id).unwrap_or(RotState { since: self.ticks, held: owned, front: Front::default() });
        let even = rot_quota(owned, seconds_under);
        let grainy = (self.ticks - state.since) < (ROT_GRAIN_SECONDS * 10) as u32;
        let specks = if grainy {
            let per = (state.held * ROT_SPECKLE_PERCENT) as f64 / 100.0 / ROT_GRAIN_SECONDS as f64;
            1i64.max(per.ceil() as i64)
        } else {
            0
        };
        let mut budget = owned.min(even.max(specks));
        let mut front = Front::default();
        for (t, r) in state.front.iter() {
            if self.players[p].tiles.contains(t) {
                front.set(t, r);
            }
        }
        budget -= self.speckle(p, specks.min(budget), &mut front);
        budget -= self.spread(p, budget, &mut front);
        if budget > 0 {
            self.speckle(p, budget, &mut front);
        }
        de.rot.insert(id, RotState { since: state.since, held: state.held, front });
    }

    /// Opens `count` holes, interior first, at the lowest speckle noise.
    fn speckle(&mut self, p: usize, count: i64, front: &mut Front) -> i64 {
        if count <= 0 {
            return 0;
        }
        let salt = self.players[p].small_id as u32;
        let mut interior = LowestN::new(count as usize);
        let mut edge = LowestN::new(count as usize);
        for t in self.players[p].tiles.values() {
            let key = speckle_noise(self.map.x(t), self.map.y(t), salt);
            if self.players[p].border_tiles.contains(t) {
                edge.offer(t, key);
            } else {
                interior.offer(t, key);
            }
        }
        let picked: Vec<u32> = if interior.tiles.len() as i64 >= count {
            interior.tiles
        } else {
            interior.tiles.into_iter().chain(edge.tiles).collect()
        };
        let mut opened = 0;
        for t in picked {
            if opened == count {
                break;
            }
            if self.consume(p, t, front) {
                opened += 1;
            }
        }
        opened
    }

    /// Grows the holes by up to `count` tiles, tips (fewest rotted neighbours) first.
    fn spread(&mut self, p: usize, count: i64, front: &mut Front) -> i64 {
        if count <= 0 || front.live == 0 {
            return 0;
        }
        let salt = self.players[p].small_id as u32;
        let mut pick = LowestN::new(count as usize);
        for (t, rotted) in front.iter() {
            pick.offer(t, rotted * ROT_NOISE_SCALE + front_noise(t, salt));
        }
        let mut taken = 0;
        for t in pick.tiles {
            if taken == count {
                break;
            }
            if self.consume(p, t, front) {
                taken += 1;
            }
        }
        taken
    }

    /// Rots one tile into fallout and queues its neighbours on the front.
    fn consume(&mut self, p: usize, t: u32, front: &mut Front) -> bool {
        if !self.players[p].tiles.contains(t) {
            return false;
        }
        self.relinquish(t);
        if !self.map.has_fallout(t) {
            self.map.set_fallout(t, true);
        }
        front.delete(t);
        let mut nb = [0u32; 4];
        let n = self.map.neighbors4(t, &mut nb);
        for &x in &nb[..n] {
            if self.players[p].tiles.contains(x) {
                front.set(x, front.get(x).unwrap_or(0) + 1);
            }
        }
        true
    }
}
