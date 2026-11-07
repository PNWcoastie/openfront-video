use super::*;

// ── Nukes, fallout, SAM targeting (NukeExecution, SAMLauncherExecution,
// SAMMissileExecution, ParabolaUniversalPathFinder, DistanceBasedBezierCurve) ──

pub const ATOM_BOMB: &str = "Atom Bomb";
pub const HYDROGEN_BOMB: &str = "Hydrogen Bomb";
pub const MIRV_WARHEAD: &str = "MIRV Warhead";
pub const SAM_MISSILE: &str = "SAMMissile";

pub struct NukeState {
    pub trajectory: Vec<(u32, bool)>,
    pub index: usize,
    pub wait_ticks: i64,
    pub targeted_by_sam: bool,
}

/// SamLauncherState: (upgradeStartTick, startRange, targetLevel, duration).
#[derive(Clone, Copy)]
pub struct SamState {
    pub start_tick: u32,
    pub start_range: f64,
    pub target_level: u32,
    pub duration: u32,
}

fn js_round(v: f64) -> f64 {
    (v + 0.5).floor()
}

/// DistanceBasedBezierCurve
pub struct Curve {
    pub(super) points: Vec<(i64, i64)>,
    pub(super) index: usize,
    pub(super) acc: i64,
    pub(super) spacing: i64,
}

impl Curve {
    pub(super) fn new(p: [(f64, f64); 4], increment: f64) -> Curve {
        let spacing = (js_round(increment * 256.0) as i64).max(256);
        let r = |v: f64| js_round(v) as i64 * 256;
        let (p0x, p0y, p3x, p3y) = (r(p[0].0), r(p[0].1), r(p[3].0), r(p[3].1));
        let mut st = (p0x, p0y, 0i64);
        let mut points = vec![((p0x + 128) >> 8, (p0y + 128) >> 8)];
        Self::subdivide([p0x, p0y, r(p[1].0), r(p[1].1), r(p[2].0), r(p[2].1), p3x, p3y], 0, &mut st, spacing, &mut points);
        let last = ((p3x + 128) >> 8, (p3y + 128) >> 8);
        if *points.last().unwrap() != last {
            points.push(last);
        }
        Curve { points, index: 0, acc: 0, spacing }
    }

    pub(super) fn subdivide(c: [i64; 8], depth: u32, st: &mut (i64, i64, i64), step: i64, points: &mut Vec<(i64, i64)>) {
        let [ax, ay, bx, by, cx, cy, dx, dy] = c;
        let dist = (bx - ax).abs() + (by - ay).abs() + (cx - bx).abs() + (cy - by).abs() + (dx - cx).abs() + (dy - cy).abs();
        if dist <= 256 || depth >= 10 {
            let (edx, edy) = ((ax - st.0) as f64, (ay - st.1) as f64);
            st.2 += (edx * edx + edy * edy).sqrt().floor() as i64;
            st.0 = ax;
            st.1 = ay;
            while st.2 >= step {
                points.push(((ax + 128) >> 8, (ay + 128) >> 8));
                st.2 -= step;
            }
            return;
        }
        let (m01x, m01y) = ((ax + bx) >> 1, (ay + by) >> 1);
        let (m12x, m12y) = ((bx + cx) >> 1, (by + cy) >> 1);
        let (m23x, m23y) = ((cx + dx) >> 1, (cy + dy) >> 1);
        let (m012x, m012y) = ((m01x + m12x) >> 1, (m01y + m12y) >> 1);
        let (m123x, m123y) = ((m12x + m23x) >> 1, (m12y + m23y) >> 1);
        let (mx, my) = ((m012x + m123x) >> 1, (m012y + m123y) >> 1);
        Self::subdivide([ax, ay, m01x, m01y, m012x, m012y, mx, my], depth + 1, st, step, points);
        Self::subdivide([mx, my, m123x, m123y, m23x, m23y, dx, dy], depth + 1, st, step, points);
    }

    pub(super) fn increment(&mut self, distance: f64) -> Option<(i64, i64)> {
        self.acc += (js_round(distance * 256.0) as i64).max(1);
        while self.index < self.points.len() - 1 && self.acc >= self.spacing {
            self.index += 1;
            self.acc -= self.spacing;
        }
        if self.index >= self.points.len() - 1 {
            None
        } else {
            Some(self.points[self.index])
        }
    }
}

pub struct NukeExec {
    pub(super) active: bool,
    pub(super) nuke_type: &'static str,
    pub(super) player: usize,
    pub(super) dst: u32,
    pub(super) src: Option<u32>,
    pub(super) speed: f64,
    pub(super) wait_ticks: i64,
    pub(super) rocket_up: bool,
    pub(super) nuke: Option<usize>,
    pub(super) curve: Option<Curve>,
    pub(super) warhead_id: Option<usize>,
}

#[derive(Clone, Copy)]
pub(super) struct Cached {
    pub(super) tick: i64,
    pub(super) tile: u32,
    pub(super) last_seen: u32,
}

pub struct SamExec {
    pub(super) active: bool,
    pub(super) unit: usize,
    pub(super) last_level: u32,
    pub(super) cache: Vec<(u32, Cached)>,
}

pub struct SamMissileExec {
    pub(super) active: bool,
    pub(super) spawn: u32,
    pub(super) owner: usize,
    pub(super) owner_unit: usize,
    pub(super) target: usize,
    pub(super) target_tile: u32,
    pub(super) missile: Option<usize>,
    pub(super) seed: u32,
    pub(super) air: Stepper,
}

impl Game {
    pub fn new_nuke(&self, nuke_type: &'static str, player: usize, dst: u32, rocket_up: bool) -> Exec {
        Exec::Nuke(Box::new(NukeExec {
            active: true,
            nuke_type,
            player,
            dst,
            src: None,
            speed: -1.0,
            wait_ticks: 0,
            rocket_up,
            nuke: None,
            curve: None,
            warhead_id: None,
        }))
    }

    pub(super) fn parabola_points(&self, from: u32, to: u32, up: bool) -> [(f64, f64); 4] {
        let p0 = (self.map.x(from) as f64, self.map.y(from) as f64);
        let p3 = (self.map.x(to) as f64, self.map.y(to) as f64);
        let (dx, dy) = (p3.0 - p0.0, p3.1 - p0.1);
        let distance = (dx * dx + dy * dy).sqrt();
        let max_height = crate::detmath::js_max(distance / 3.0, 50.0);
        let mult = if up { -1.0 } else { 1.0 };
        let h = self.map.height as f64;
        let p1y = p0.1 + dy / 4.0 + mult * max_height;
        let p2y = p0.1 + (dy * 3.0) / 4.0 + mult * max_height;
        let p1 = (p0.0 + dx / 4.0, within(p1y, 0.0, h - 1.0));
        let p2 = (p0.0 + (dx * 3.0) / 4.0, within(p2y, 0.0, h - 1.0));
        [p0, p1, p2, p3]
    }

    pub(super) fn curve_tile(&self, p: (i64, i64)) -> u32 {
        self.map.ref_at(p.0 as u32, p.1 as u32)
    }

    pub(super) fn dynamic_sam_range(&self, sam: usize, tick: i64) -> f64 {
        let range = |level: u32| 150.0 - 480.0 / (level as f64 + 5.0);
        match self.units[sam].sam_state {
            None => range(self.units[sam].level),
            Some(s) => {
                let elapsed = tick - s.start_tick as i64;
                if elapsed >= s.duration as i64 {
                    return range(s.target_level);
                }
                let target = range(s.target_level);
                s.start_range + ((target - s.start_range) * elapsed as f64) / s.duration as f64
            }
        }
    }

    pub(super) fn game_over(&self) -> bool {
        self.winner.is_some() && !self.config.is_singleplayer
    }

    /// PlayerImpl.nukeSpawn (canBuild for atom and hydrogen bombs)
    pub(super) fn nuke_spawn(&self, p: usize, tile: u32, nuke_type: &str) -> Option<u32> {
        if self.unit_cost(p, nuke_type) > self.players[p].gold || !self.players[p].is_alive() {
            return None;
        }
        if self.is_spawn_immunity_active() || self.map.is_impassable(tile) {
            return None;
        }
        let game_over = self.game_over();
        if let Some(o) = self.owner_index(tile) {
            if self.is_on_same_team(p, o) && !game_over {
                return None;
            }
        }
        if self.config.is_team_mode && nuke_type != "MIRV" && !game_over {
            let outer = if nuke_type == HYDROGEN_BOMB { 100 } else { 30 };
            let hit = STRUCTURES.iter().flat_map(|t| self.type_units(t).iter().map(|&i| &self.units[i])).any(|u| {
                u.active && !u.under_construction && is_structure(u.type_name) && self.dist2(u.tile, tile) <= outer * outer && self.in_grid_cells(tile, outer, u.tile) && self.is_on_same_team(p, u.owner)
            });
            if hit {
                return None;
            }
        }
        let mut silos: Vec<usize> = self
            .player_units_of(p, MISSILE_SILO)
            .into_iter()
            .filter(|&s| {
                let u = &self.units[s];
                u.active && u.missile_timers.len() as u32 != u.level && !u.under_construction
            })
            .collect();
        silos.sort_by_key(|&s| self.map.manhattan_dist(self.units[s].tile, tile));
        silos.first().map(|&s| self.units[s].tile)
    }

    pub(super) fn tick_nuke(&mut self, ne: &mut NukeExec) {
        if ne.speed == -1.0 {
            ne.speed = 10.0;
        }
        let Some(nuke) = ne.nuke else {
            // canBuild(MIRVWarhead) is the target tile.
            let spawn = if ne.nuke_type == MIRV_WARHEAD { Some(ne.dst) } else { self.nuke_spawn(ne.player, ne.dst, ne.nuke_type) };
            let Some(spawn) = spawn else {
                ne.active = false;
                return;
            };
            let src = *ne.src.get_or_insert(spawn);
            let silo = self.player_units_of(ne.player, MISSILE_SILO).into_iter().find(|&s| self.units[s].tile == spawn);
            if let Some(s) = silo {
                let mut last_dep: i64 = 0;
                for &lt in &self.units[s].missile_timers {
                    last_dep = (lt as i64 + 1).max(last_dep + 1);
                }
                if last_dep > self.ticks as i64 {
                    ne.wait_ticks += last_dep - self.ticks as i64;
                }
            }
            // getTrajectory
            let pts = self.parabola_points(src, ne.dst, ne.rocket_up);
            let all = Curve::new(pts, ne.speed).points;
            let r2 = 150i64 * 150;
            let trajectory: Vec<(u32, bool)> = all
                .iter()
                .map(|&p| {
                    let t = self.curve_tile(p);
                    (t, self.dist2(t, ne.dst) < r2 || self.dist2(src, t) < r2)
                })
                .collect();
            let u = self.build_unit(ne.player, ne.nuke_type, src, 0.0, Some(ne.dst));
            self.units[u].nuke = Some(Box::new(NukeState { trajectory, index: 0, wait_ticks: ne.wait_ticks, targeted_by_sam: false }));
            ne.nuke = Some(u);
            if let Some(w) = ne.warhead_id {
                self.warhead_units[w] = Some(u);
            }
            if ne.nuke_type != MIRV_WARHEAD {
                self.nuke_break_alliances(ne, u);
            }
            if let Some(s) = silo {
                self.units[s].missile_timers.push(self.ticks);
            }
            return;
        };
        if !self.units[nuke].active {
            ne.active = false;
            return;
        }
        if ne.wait_ticks > 0 {
            ne.wait_ticks -= 1;
            self.units[nuke].nuke.as_mut().unwrap().wait_ticks = ne.wait_ticks;
            return;
        }
        let src = ne.src.unwrap();
        if ne.curve.is_none() {
            ne.curve = Some(Curve::new(self.parabola_points(src, ne.dst, ne.rocket_up), ne.speed));
        }
        match ne.curve.as_mut().unwrap().increment(ne.speed) {
            None => {
                self.move_unit(nuke, ne.dst);
                let owner = self.units[nuke].owner;
                let intercepted = self.any_nearby_unit(ne.dst, 12, &[SAM_MISSILE], |m, _| {
                    self.units[m].owner != owner && self.units[m].target_unit == Some(nuke)
                });
                if !intercepted {
                    self.detonate(ne, nuke);
                }
            }
            Some(p) => {
                let t = self.curve_tile(p);
                if let Some(tt) = self.units[nuke].target_tile {
                    let cur = self.units[nuke].tile;
                    let r2 = 150i64 * 150;
                    self.units[nuke].targetable = self.dist2(cur, tt) < r2 || self.dist2(src, cur) < r2;
                }
                self.move_unit(nuke, t);
                let idx = ne.curve.as_ref().unwrap().index;
                let ns = self.units[nuke].nuke.as_mut().unwrap();
                ns.index = idx.min(ns.trajectory.len() - 1);
            }
        }
    }

    pub(super) fn nuke_magnitude(t: &str) -> (i64, i64) {
        match t {
            MIRV_WARHEAD => (12, 18),
            ATOM_BOMB => (12, 30),
            HYDROGEN_BOMB => (80, 100),
            other => panic!("Unknown nuke type: {other}"),
        }
    }

    pub(super) fn nuke_break_alliances(&mut self, ne: &NukeExec, nuke: usize) {
        let (inner, outer) = Self::nuke_magnitude(self.units[nuke].type_name);
        let (inner2, outer2) = (inner * inner, outer * outer);
        // computeNukeBlastCounts (circleSearch: x outer, y inner)
        let mut counts: Vec<(u16, f64)> = Vec::new();
        let (cx, cy) = (self.map.x(ne.dst) as i64, self.map.y(ne.dst) as i64);
        let (w, h) = (self.map.width as i64, self.map.height as i64);
        for x in (cx - outer).max(0)..=(cx + outer).min(w - 1) {
            for y in (cy - outer).max(0)..=(cy + outer).min(h - 1) {
                let d2 = (x - cx) * (x - cx) + (y - cy) * (y - cy);
                if d2 > outer2 {
                    continue;
                }
                let owner = self.map.owner_id((y * w + x) as u32);
                if owner > 0 {
                    let weight = if d2 <= inner2 { 1.0 } else { 0.5 };
                    match counts.iter_mut().find(|c| c.0 == owner) {
                        Some(c) => c.1 += weight,
                        None => counts.push((owner, weight)),
                    }
                }
            }
        }
        let mut set: Vec<usize> = Vec::new();
        for &(sid, wgt) in &counts {
            if wgt > 100.0 && !set.contains(&(sid as usize - 1)) {
                set.push(sid as usize - 1);
            }
        }
        let structures = [CITY, DEFENSE_POST, SAM_LAUNCHER, MISSILE_SILO, PORT, FACTORY];
        for (u, _) in self.nearby_units(ne.dst, outer, &structures) {
            let o = self.units[u].owner;
            if !set.contains(&o) {
                set.push(o);
            }
        }
        let player = ne.player;
        for r in self.incoming_alliance_requests(player) {
            if set.contains(&self.requests[r].requestor) {
                self.reject_alliance_request(r);
            }
        }
        for attacked in set {
            if let Some(r) = self.incoming_alliance_requests(attacked).into_iter().find(|&r| self.requests[r].requestor == player) {
                self.reject_alliance_request(r);
                continue;
            }
            if let Some(al) = self.alliance_with(player, attacked) {
                self.break_alliance(player, al);
            }
            if attacked != player {
                self.update_relation(attacked, player, -100.0);
            }
        }
    }

    pub(super) fn detonate(&mut self, ne: &mut NukeExec, nuke: usize) {
        let (inner, outer) = Self::nuke_magnitude(self.units[nuke].type_name);
        let (inner2, outer2) = (inner * inner, outer * outer);
        // tilesToDestroy (no water nukes)
        let rand = std::cell::RefCell::new(PseudoRandom::new(self.ticks as f64));
        let dst = ne.dst;
        let (dx, dy) = (self.map.x(dst) as i64, self.map.y(dst) as i64);
        let to_destroy = self.map.bfs(dst, |m, n| {
            let (x, y) = (m.x(n) as i64, m.y(n) as i64);
            let d2 = (x - dx) * (x - dx) + (y - dy) * (y - dy);
            d2 <= outer2 && (d2 <= inner2 || rand.borrow_mut().next_int(0.0, 2.0) == 0.0) && !m.is_impassable(n)
        });
        let mut per_player: Vec<(usize, i64)> = Vec::new();
        for &tile in &to_destroy {
            if let Some(o) = self.owner_index(tile) {
                self.relinquish(tile);
                match per_player.iter_mut().find(|e| e.0 == o) {
                    Some(e) => e.1 += 1,
                    None => per_player.push((o, 1)),
                }
            }
            if self.map.is_land(tile) {
                // queueWaterConversion without water nukes: fallout.
                self.map.set_fallout(tile, true);
            }
        }
        let warhead = ne.nuke_type == MIRV_WARHEAD;
        for &(p, n) in &per_player {
            let max_troops = self.max_troops(p);
            let death = |humans: f64, tiles: i64| {
                if !warhead {
                    return (5.0 * humans) / (tiles.max(1) as f64);
                }
                let excess = crate::detmath::js_max(0.0, humans - 0.03 * max_troops);
                500.0 * (1.0 - crate::detmath::exp(-2.0 * (excess / max_troops)))
            };
            let before = self.players[p].tiles.len() as i64 + n;
            let ships: Vec<usize> = self.player_units_of(p, TRANSPORT_SHIP);
            let mut ship_troops: Vec<f64> = ships.iter().map(|&s| self.units[s].troops).collect();
            let attacks = self.players[p].outgoing.clone();
            for i in 0..n {
                let left = before - i;
                let d = death(self.players[p].troops_f(), left);
                self.remove_troops(p, d);
                for &a in &attacks {
                    let at = self.attacks[a].troops;
                    let deaths = death(at, left);
                    self.attacks[a].troops = (at - deaths).max(0.0);
                }
                for t in ship_troops.iter_mut() {
                    let deaths = death(*t, left);
                    *t = (*t - deaths).max(0.0);
                }
            }
            for (k, &s) in ships.iter().enumerate() {
                self.units[s].troops = ship_troops[k].max(0.0);
            }
        }
        let all: Vec<usize> = (0..self.players.len()).flat_map(|p| self.players[p].units.clone()).collect();
        for u in all {
            let t = self.units[u].type_name;
            if matches!(t, ATOM_BOMB | HYDROGEN_BOMB | MIRV_WARHEAD | "MIRV" | SAM_MISSILE) {
                continue;
            }
            if self.dist2(dst, self.units[u].tile) < outer2 {
                if !self.is_friendly(ne.player, self.units[u].owner, true) {
                    self.units[u].destroyer = Some(ne.player);
                }
                self.delete_unit(u);
            }
        }
        ne.active = false;
        self.units[nuke].reached_target = true;
        self.delete_unit(nuke);
    }

    // ── SAMLauncherExecution ──

    pub(super) fn tick_sam(&mut self, se: &mut SamExec) {
        let sam = se.unit;
        if se.last_level != self.units[sam].level {
            se.last_level = self.units[sam].level;
            se.cache.retain(|c| c.1.tick != -1);
        }
        if self.units[sam].under_construction {
            return;
        }
        if !self.units[sam].active {
            se.active = false;
            return;
        }
        while let Some(&front) = self.units[sam].missile_timers.first() {
            if 90 - (self.ticks as i64 - front as i64) > 0 {
                break;
            }
            self.units[sam].missile_timers.remove(0);
        }
        if self.units[sam].missile_timers.len() as u32 == self.units[sam].level {
            return;
        }
        if ![ATOM_BOMB, HYDROGEN_BOMB, MIRV_WARHEAD].iter().any(|t| self.type_units(t).iter().any(|&i| self.units[i].active)) {
            return;
        }
        let targets = self.sam_valid_targets(se, self.ticks);
        for (nuke, tile) in targets {
            if self.units[sam].missile_timers.len() as u32 == self.units[sam].level {
                break;
            }
            self.units[sam].missile_timers.push(self.ticks);
            self.units[nuke].nuke.as_mut().unwrap().targeted_by_sam = true;
            self.add_execution(Exec::SamMissile(Box::new(SamMissileExec {
                active: true,
                spawn: self.units[sam].tile,
                owner: self.units[sam].owner,
                owner_unit: sam,
                target: nuke,
                target_tile: tile,
                missile: None,
                seed: 0,
                air: Stepper::default(),
            })));
        }
    }

    pub(super) fn valid_nuke_target(&self, sam: usize, u: usize) -> bool {
        let unit = &self.units[u];
        let sam_owner = self.units[sam].owner;
        if unit.nuke.as_ref().is_some_and(|n| n.targeted_by_sam) || unit.owner == sam_owner {
            return false;
        }
        if self.is_friendly(sam_owner, unit.owner, false) {
            return self.game_over() && self.is_on_same_team(sam_owner, unit.owner);
        }
        true
    }

    pub(super) fn tick_to_reach(&self, a: u32, b: u32) -> i64 {
        (self.map.manhattan_dist(a, b) as f64 / 12.0).ceil() as i64
    }

    pub(super) fn compute_interception(&self, sam: usize, nuke: usize, ticks: u32) -> Cached {
        let sam_tile = self.units[sam].tile;
        let ns = self.units[nuke].nuke.as_ref().unwrap();
        let traj = &ns.trajectory;
        let cur = ns.index as i64;
        let wait = ns.wait_ticks;
        let max_idx = traj.len() as i64 - 2;
        let max_sq = 150.0 * 150.0;
        let mut min_dist_sq = f64::INFINITY;
        let mut closest = sam_tile;
        let mut inc_steps = 0;
        let mut last_dist_sq: f64 = -1.0;
        let mut i = cur;
        while i <= max_idx {
            let (tile, targetable) = traj[i as usize];
            let dist_sq = self.dist2(sam_tile, tile) as f64;
            if dist_sq < min_dist_sq {
                min_dist_sq = dist_sq;
                closest = tile;
            }
            inc_steps = if last_dist_sq != -1.0 && dist_sq > last_dist_sq { inc_steps + 1 } else { 0 };
            last_dist_sq = dist_sq;
            let nuke_ticks = i - cur + wait;
            let sam_ticks = self.tick_to_reach(sam_tile, tile);
            let allowed = self.dynamic_sam_range(sam, ticks as i64 + nuke_ticks);
            if targetable && dist_sq <= allowed * allowed && nuke_ticks >= sam_ticks {
                return Cached { tick: nuke_ticks - sam_ticks, tile, last_seen: ticks };
            }
            if inc_steps > 3 && dist_sq > max_sq {
                break;
            }
            i += 1;
        }
        // checkDetonationInterception
        let det = (|| {
            let &(final_tile, final_ok) = traj.last()?;
            if !final_ok {
                return None;
            }
            let exp_ticks = traj.len() as i64 - 1 - cur + wait;
            let range = self.dynamic_sam_range(sam, ticks as i64 + exp_ticks);
            if self.dist2(sam_tile, final_tile) as f64 > range * range {
                return None;
            }
            if max_idx < 0 {
                return None;
            }
            let (flight, flight_ok) = traj[max_idx as usize];
            if !flight_ok {
                return None;
            }
            let before = (max_idx - cur + wait) - self.tick_to_reach(sam_tile, flight);
            if before >= 0 {
                Some((before, flight))
            } else {
                None
            }
        })();
        if let Some((tick, tile)) = det {
            return Cached { tick, tile, last_seen: ticks };
        }
        Cached { tick: if min_dist_sq > max_sq { -2 } else { -1 }, tile: closest, last_seen: ticks }
    }

    pub(super) fn sam_valid_targets(&self, se: &mut SamExec, ticks: u32) -> Vec<(usize, u32)> {
        let sam = se.unit;
        let nukes: Vec<usize> = self
            .nearby_units(self.units[sam].tile, 600, &[ATOM_BOMB, HYDROGEN_BOMB, MIRV_WARHEAD])
            .into_iter()
            .map(|(u, _)| u)
            .filter(|&u| self.valid_nuke_target(sam, u))
            .collect();
        let mut targets: Vec<(usize, u32)> = Vec::new();
        for nuke in nukes {
            let id = self.units[nuke].id;
            if let Some(pos) = se.cache.iter().position(|c| c.0 == id) {
                se.cache[pos].1.last_seen = ticks;
                let c = se.cache[pos].1;
                if c.tick == -2 || c.tick == -1 {
                    continue;
                }
                if c.tick == ticks as i64 || c.tick == ticks as i64 + 1 {
                    targets.push((nuke, c.tile));
                    se.cache.remove(pos);
                    continue;
                }
                if c.tick > ticks as i64 {
                    continue;
                }
                se.cache.remove(pos);
            }
            let res = self.compute_interception(sam, nuke, ticks);
            if res.tick >= 0 && res.tick <= 1 {
                targets.push((nuke, res.tile));
            } else {
                let entry = Cached { tick: if res.tick >= 0 { res.tick + ticks as i64 } else { res.tick }, ..res };
                match se.cache.iter_mut().find(|c| c.0 == id) {
                    Some(c) => c.1 = entry,
                    None => se.cache.push((id, entry)),
                }
            }
        }
        se.cache.retain(|c| c.1.last_seen == ticks);
        if targets.len() <= 1 {
            return targets;
        }
        let sam_tile = self.units[sam].tile;
        let score = |&(u, _): &(usize, u32)| -> f64 {
            let unit = &self.units[u];
            let ns = unit.nuke.as_ref().unwrap();
            let time_to_explode = (ns.trajectory.len() as f64 - ns.index as f64).max(1.0);
            let target = unit.target_tile.unwrap_or_else(|| ns.trajectory.last().map_or(sam_tile, |t| t.0));
            let dist = self.map.manhattan_dist(sam_tile, target) as f64;
            let type_bonus = if unit.type_name == HYDROGEN_BOMB { 70_001.0 } else { 0.0 };
            type_bonus + (200_000.0 - dist * 1000.0).max(0.0) + (10_000.0 - time_to_explode * 100.0).max(0.0)
        };
        let mut scored: Vec<(f64, (usize, u32))> = targets.iter().map(|t| (score(t), *t)).collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
        scored.into_iter().map(|s| s.1).collect()
    }

    // ── SAMMissileExecution ──

    pub(super) fn tick_sam_missile(&mut self, me: &mut SamMissileExec) {
        let missile = match me.missile {
            Some(m) => m,
            None => {
                let m = self.build_unit(me.owner, SAM_MISSILE, me.spawn, 0.0, None);
                self.units[m].target_unit = Some(me.target);
                me.missile = Some(m);
                m
            }
        };
        if !self.units[missile].active {
            me.active = false;
            return;
        }
        let target = me.target;
        if !self.units[target].active
            || !self.units[me.owner_unit].active
            || self.units[target].owner == self.units[missile].owner
            || !matches!(self.units[target].type_name, ATOM_BOMB | HYDROGEN_BOMB | MIRV_WARHEAD)
        {
            if self.units[target].active {
                self.units[target].nuke.as_mut().unwrap().targeted_by_sam = false;
            }
            self.delete_unit(missile);
            me.active = false;
            return;
        }
        for _ in 0..12 {
            let mut st = std::mem::take(&mut me.air);
            let r = self.air_next(&mut st, me.seed, self.units[missile].tile, me.target_tile);
            me.air = st;
            match r {
                None => {
                    me.active = false;
                    self.delete_unit(target);
                    self.delete_unit(missile);
                    return;
                }
                Some(n) => self.move_unit(missile, n),
            }
        }
    }

    /// The Air stepper: Some(next) for NEXT, None for COMPLETE.
    pub(super) fn air_next(&self, st: &mut Stepper, seed: u32, from: u32, to: u32) -> Option<u32> {
        if from == to {
            return None;
        }
        if st.last_to != Some(to) {
            st.path = None;
            st.index = 0;
            st.last_to = Some(to);
        }
        if st.path.is_none() {
            let path = self.air_path(seed, from, to);
            st.index = if path[0] == from { 1 } else { 0 };
            st.path = Some(path);
        }
        let path = st.path.as_ref().unwrap();
        if st.index > 0 && path[st.index - 1] != from {
            st.path = None;
            st.index = 0;
            st.last_to = Some(to);
            return self.air_next(st, seed, from, to);
        }
        if st.index >= path.len() {
            return None;
        }
        let next = path[st.index];
        st.index += 1;
        Some(next)
    }
}

// ── MIRVs (MirvExecution) ──

pub struct MirvExec {
    pub(super) active: bool,
    pub(super) player: usize,
    pub(super) dst: u32,
    pub(super) nuke: Option<usize>,
    pub(super) base_x: i64,
    pub(super) base_y: i64,
    pub(super) random: Option<PseudoRandom>,
    pub(super) full_path: Vec<u32>,
    pub(super) path_index: usize,
    pub(super) target_player: Option<usize>,
    pub(super) separate_dst: u32,
    pub(super) staged: Vec<u32>,
    pub(super) warheads_spawned: bool,
    pub(super) warheads: Vec<usize>,
}

/// DistanceBasedBezierCurve.getLength
fn curve_length(p: [(f64, f64); 4]) -> f64 {
    let r = |v: f64| js_round(v) as i64 * 256;
    let (p0x, p0y, p3x, p3y) = (r(p[0].0), r(p[0].1), r(p[3].0), r(p[3].1));
    let mut st = (p0x, p0y, 0i64);
    let mut sink = Vec::new();
    // stepThreshold undefined in getLength: no points, so an unreachable step.
    Curve::subdivide([p0x, p0y, r(p[1].0), r(p[1].1), r(p[2].0), r(p[2].1), p3x, p3y], 0, &mut st, i64::MAX, &mut sink);
    let (edx, edy) = ((p3x - st.0) as f64, (p3y - st.1) as f64);
    st.2 += (edx * edx + edy * edy).sqrt().floor() as i64;
    st.2 as f64 / 256.0
}

impl Game {
    pub fn new_mirv(&self, player: usize, dst: u32) -> Exec {
        Exec::Mirv(Box::new(MirvExec {
            active: true,
            player,
            dst,
            nuke: None,
            base_x: 0,
            base_y: 0,
            random: None,
            full_path: Vec::new(),
            path_index: 0,
            target_player: None,
            separate_dst: 0,
            staged: Vec::new(),
            warheads_spawned: false,
            warheads: Vec::new(),
        }))
    }

    pub(super) fn init_mirv(&mut self, me: &mut MirvExec) {
        me.random = Some(PseudoRandom::new(self.ticks as f64 + simple_hash(&self.players[me.player].info.id) as f64));
        me.target_player = self.owner_index(me.dst);
        me.base_x = self.map.x(me.dst) as i64;
        me.base_y = self.map.y(me.dst) as i64;
        me.staged = vec![me.dst];
    }

    pub(super) fn parabola_points_opts(&self, from: u32, to: u32, ignore_bounds: bool) -> [(f64, f64); 4] {
        let p0 = (self.map.x(from) as f64, self.map.y(from) as f64);
        let p3 = (self.map.x(to) as f64, self.map.y(to) as f64);
        let (dx, dy) = (p3.0 - p0.0, p3.1 - p0.1);
        let distance = (dx * dx + dy * dy).sqrt();
        let max_height = crate::detmath::js_max(distance / 3.0, 50.0);
        let h = self.map.height as f64;
        let p1y = p0.1 + dy / 4.0 - max_height;
        let p2y = p0.1 + (dy * 3.0) / 4.0 - max_height;
        let clamp = |v: f64| if ignore_bounds { v } else { within(v, 0.0, h - 1.0) };
        [p0, (p0.0 + dx / 4.0, clamp(p1y)), (p0.0 + (dx * 3.0) / 4.0, clamp(p2y)), p3]
    }

    pub(super) fn mirv_speed(&self, spawn: u32, sep: u32) -> f64 {
        let ideal = curve_length(self.parabola_points_opts(spawn, sep, true));
        let ideal_ticks = ideal / 15.0;
        let base_scaled: i64 = 14 * 100;
        let ideal_int = (ideal_ticks * 100.0).floor() as i64;
        let mut target = base_scaled;
        if ideal_int > base_scaled {
            let diff = ideal_int - base_scaled;
            target = base_scaled + ((diff as f64).sqrt().floor() as i64) * 14 + (diff * 10).div_euclid(100);
        } else if ideal_int < base_scaled {
            let diff = base_scaled - ideal_int;
            target = base_scaled - ((diff as f64).sqrt().floor() as i64) * 10;
        }
        target = target.max(100);
        let actual = curve_length(self.parabola_points_opts(spawn, sep, false));
        let actual_int = js_round(actual * 256.0) as i64;
        let num = actual_int * 100;
        let speed_scaled = ((num + target / 2) as f64 / target as f64).floor();
        crate::detmath::js_max(1.0, speed_scaled / 256.0)
    }

    pub(super) fn tick_mirv(&mut self, me: &mut MirvExec) {
        if me.nuke.is_none() {
            // canBuild(MIRV): cost, alive, an owned target, then nukeSpawn.
            let p = me.player;
            let spawn = if self.unit_cost(p, "MIRV") > self.players[p].gold || !self.players[p].is_alive() || self.owner_index(me.dst).is_none() {
                None
            } else {
                self.nuke_spawn(p, me.dst, "MIRV")
            };
            let Some(spawn) = spawn else {
                me.active = false;
                return;
            };
            me.target_player = self.owner_index(me.dst);
            let n = self.build_unit(p, "MIRV", spawn, 0.0, Some(me.dst));
            self.units[n].mirv_target = me.target_player;
            me.nuke = Some(n);
            self.mirvs_launched += 1;
            if let Some(t) = me.target_player {
                if let Some(al) = self.alliance_with(p, t) {
                    self.break_alliance(p, al);
                }
                if t != p {
                    self.update_relation(t, p, -100.0);
                    self.update_relation(p, t, -100.0);
                }
            }
            let x = (me.base_x + self.map.x(self.units[n].tile) as i64).div_euclid(2);
            let y = (me.base_y - 500).max(0) + 50;
            me.separate_dst = self.map.ref_at(x as u32, y as u32);
            let speed = self.mirv_speed(spawn, me.separate_dst);
            let mut curve = Curve::new(self.parabola_points(spawn, me.separate_dst, true), speed);
            while let Some(pt) = curve.increment(speed) {
                me.full_path.push(self.curve_tile(pt));
            }
            if let Some(s) = self.player_units_of(p, MISSILE_SILO).into_iter().find(|&s| self.units[s].tile == spawn) {
                self.units[s].missile_timers.push(self.ticks);
            }
        }
        let nuke = me.nuke.unwrap();
        if !self.units[nuke].active {
            me.active = false;
            for w in std::mem::take(&mut me.warheads) {
                // NukeExecution.cancel
                self.warhead_cancelled[w] = true;
                if let Some(u) = self.warhead_units[w] {
                    if self.units[u].active {
                        self.delete_unit(u);
                    }
                }
            }
            return;
        }
        let remaining = me.full_path.len() as i64 - me.path_index as i64;
        if remaining <= 20 && remaining > 10 {
            for _ in 0..100 {
                if me.staged.len() >= 350 {
                    break;
                }
                if let Some(t) = self.mirv_try_target(me) {
                    me.staged.push(t);
                }
            }
        }
        if remaining <= 10 && !me.warheads_spawned {
            me.warheads_spawned = true;
            let extra = (10 - remaining) * 50;
            // finalizeDestinations
            let (dst, target) = (me.dst, me.target_player);
            let staged = std::mem::take(&mut me.staged);
            me.staged = staged.into_iter().filter(|&t| t == dst || self.owner_index(t) == target).collect();
            for _ in 0..(500 + extra) {
                if me.staged.len() >= 350 {
                    break;
                }
                if let Some(t) = self.mirv_try_target(me) {
                    me.staged.push(t);
                }
            }
            me.staged.sort_by(|&a, &b| self.map.manhattan_dist(b, dst).cmp(&self.map.manhattan_dist(a, dst)));
            // spawnWarheadsWithWait
            let wait_base = remaining.max(0);
            for (i, &t) in me.staged.clone().iter().enumerate() {
                let offset = match i {
                    0..=69 => 0.0,
                    70..=139 => 1.0,
                    140..=209 => 2.0,
                    210..=279 => 3.0,
                    _ => 4.0,
                };
                let wait = wait_base + me.random.as_mut().unwrap().next_int(0.0, 15.0) as i64;
                let id = self.warhead_units.len();
                self.warhead_units.push(None);
                self.warhead_cancelled.push(false);
                me.warheads.push(id);
                self.add_execution(Exec::Nuke(Box::new(NukeExec {
                    active: true,
                    nuke_type: MIRV_WARHEAD,
                    player: me.player,
                    dst: t,
                    src: Some(me.separate_dst),
                    speed: 22.0 + offset,
                    wait_ticks: wait,
                    rocket_up: true,
                    nuke: None,
                    curve: None,
                    warhead_id: Some(id),
                })));
            }
        }
        if me.path_index < me.full_path.len() {
            let t = me.full_path[me.path_index];
            me.path_index += 1;
            self.move_unit(nuke, t);
        } else {
            self.delete_unit(nuke);
            me.active = false;
        }
    }

    pub(super) fn mirv_try_target(&self, me: &mut MirvExec) -> Option<u32> {
        let range = 1500.0;
        let (w, h) = (self.map.width as f64, self.map.height as f64);
        for _ in 0..100 {
            let random = me.random.as_mut().unwrap();
            let r1 = random.next();
            let r2 = (r1 * 15485863.0) % 1.0;
            let x = js_round(r1 * range * 2.0 - range + me.base_x as f64);
            let y = js_round(r2 * range * 2.0 - range + me.base_y as f64);
            if !(x >= 0.0 && x < w && y >= 0.0 && y < h) {
                continue;
            }
            let tile = self.map.ref_at(x as u32, y as u32);
            if !self.map.is_land(tile) {
                continue;
            }
            let (dx, dy) = (x - me.base_x as f64, y - me.base_y as f64);
            if dx * dx + dy * dy > range * range {
                continue;
            }
            if self.owner_index(tile) != me.target_player {
                continue;
            }
            let (xi, yi) = (x as i64, y as i64);
            if me.staged.iter().any(|&e| (xi - self.map.x(e) as i64).abs() + (yi - self.map.y(e) as i64).abs() < 55) {
                continue;
            }
            return Some(tile);
        }
        None
    }
}

