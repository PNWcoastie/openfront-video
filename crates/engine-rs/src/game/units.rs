use super::*;
use crate::water::Bounds;

// ── Units, spatial queries, transport ships (milestone 3) ──

pub struct TransportShipExec {
    pub(super) active: bool,
    pub(super) attacker: usize,
    pub(super) original_owner: usize,
    /// The tile the intent named (resolved to a landing tile at init).
    pub(super) ref_tile: f64,
    pub(super) troops: Option<f64>,
    pub(super) target: Option<usize>,
    pub(super) dst: Option<u32>,
    pub(super) boat: Option<usize>,
    pub(super) last_move: u32,
    pub(super) stepper: Stepper,
    /// Where a retreating boat heads (resolved once; None: nowhere).
    pub(super) retreat_dst: Option<Option<u32>>,
}

/// PathFinderStepper over the water chain: the path is kept until the
/// destination changes or the unit leaves it.
#[derive(Default)]
pub struct Stepper {
    pub(super) path: Option<Vec<u32>>,
    pub(super) index: usize,
    pub(super) last_to: Option<u32>,
}

pub enum Step {
    Next(u32),
    Complete,
    NotFound,
}

impl Game {
    pub fn new_transport(&self, attacker: usize, ref_tile: f64, troops: Option<f64>) -> Exec {
        Exec::TransportShip(Box::new(TransportShipExec {
            active: true,
            attacker,
            original_owner: attacker,
            ref_tile,
            troops,
            target: None,
            dst: None,
            boat: None,
            last_move: 0,
            stepper: Stepper::default(),
            retreat_dst: None,
        }))
    }

    pub fn new_landing_attack(&self, troops: f64, owner: usize, target_id: Option<String>, source_tile: u32) -> Exec {
        let mut e = self.new_attack(Some(troops), owner, target_id);
        if let Exec::Attack(a) = &mut e {
            a.source_tile = Some(source_tile);
            a.remove_troops = false;
        }
        e
    }

    pub(super) fn unit_count(&self, p: usize, type_name: &str) -> usize {
        // Every unit is level 1 so far.
        self.players[p].units.iter().filter(|&&u| self.units[u].type_name == type_name).count()
    }

    pub(super) fn build_unit(&mut self, p: usize, type_name: &'static str, tile: u32, troops: f64, target_tile: Option<u32>) -> usize {
        let id = self.next_unit_id;
        self.next_unit_id += 1;
        let idx = self.units.len();
        self.units.push(Unit {
            id,
            type_name,
            owner: p,
            tile,
            last_tile: tile,
            troops,
            target_tile,
            active: true,
            retreating: false,
            level: 1,
            under_construction: false,
            deletion_at: None,
            has_train_station: false,
            health: 1,
            warship: None,
            target_unit: None,
            last_safe_from_pirates: 0,
            grid_seq: self.grid_seq + 1,
            missile_timers: Vec::new(),
            nuke: None,
            targetable: true,
            sam_state: None,
            train_engine: false,
            destroyer: None,
            mirv_target: None,
            reached_target: false,
        });
        self.grid_seq += 1;
        if type_name == TRAIN {
            self.train_units += 1;
        }
        self.units_by_type.entry(type_name).or_default().push(idx);
        if type_name != TRAIN {
            let c = self.cell_of(tile);
            self.cells[c].push(idx);
        }
        let cost = self.unit_cost(p, type_name);
        self.players[p].units.push(idx);
        *self.players[p].units_constructed.entry(type_name).or_default() += 1;
        self.remove_gold(p, cost);
        self.remove_troops(p, troops);
        idx
    }

    pub(super) fn delete_unit(&mut self, u: usize) {
        assert!(self.units[u].active, "cannot delete an inactive unit");
        let owner = self.units[u].owner;
        self.players[owner].units.retain(|&x| x != u);
        self.units[u].active = false;
        // Only live units stay in the type lists (none of their users depend on order).
        let list = self.units_by_type.get_mut(self.units[u].type_name).expect("type list");
        let pos = list.iter().position(|&x| x == u).expect("unit in its type list");
        list.swap_remove(pos);
        if self.units[u].type_name != TRAIN {
            let c = self.cell_of(self.units[u].tile);
            let cell = &mut self.cells[c];
            let pos = cell.iter().position(|&x| x == u).expect("unit in its cell");
            cell.swap_remove(pos);
        }
        if self.units[u].type_name == TRAIN {
            self.train_units -= 1;
        }
        if self.units[u].has_train_station {
            self.remove_station(u);
        }
    }

    // ── WaterManager.getWaterComponent ──

    pub(super) fn water_component(&self, tile: u32) -> Option<u32> {
        let mini = &self.water.mini;
        let mt = (self.map.y(tile) / 2) * mini.width + self.map.x(tile) / 2;
        if mini.is_water(mt) {
            return Some(self.water.component_id(mt));
        }
        let mut nb = [0u32; 4];
        let n = mini.neighbors4(mt, &mut nb);
        for &t in &nb[..n] {
            if mini.is_water(t) {
                return Some(self.water.component_id(t));
            }
        }
        for &t in &nb[..n] {
            let mut nb2 = [0u32; 4];
            let n2 = mini.neighbors4(t, &mut nb2);
            for &t2 in &nb2[..n2] {
                if mini.is_water(t2) {
                    return Some(self.water.component_id(t2));
                }
            }
        }
        None
    }

    // ── SpatialQuery ──

    /// bfsNearest: LIFO walk within `max_dist` (Manhattan) of `from`; the
    /// first matching tile at the smallest distance wins.
    pub(super) fn bfs_nearest(&mut self, from: u32, max_dist: u32, pred: &dyn Fn(&Game, u32) -> bool) -> Option<u32> {
        let gen = self.bump_generation();
        let mut stack = Vec::new();
        let mut best = None;
        let mut best_dist = u32::MAX;
        self.visited[from as usize] = gen;
        stack.push(from);
        if pred(self, from) {
            best = Some(from);
            best_dist = 0;
        }
        let mut nb = [0u32; 4];
        while let Some(curr) = stack.pop() {
            let n = self.map.neighbors4(curr, &mut nb);
            for &t in &nb[..n] {
                if self.visited[t as usize] != gen && self.map.manhattan_dist(from, t) <= max_dist {
                    self.visited[t as usize] = gen;
                    stack.push(t);
                    if pred(self, t) {
                        let d = self.map.manhattan_dist(from, t);
                        if d < best_dist {
                            best = Some(t);
                            best_dist = d;
                        }
                    }
                }
            }
        }
        best
    }

    pub(super) fn reachable_components(&self, p: usize) -> Vec<u32> {
        let mut comps = Vec::new();
        for t in self.players[p].border_tiles.values() {
            if !self.map.is_shore(t) {
                continue;
            }
            if let Some(c) = self.water_component(t) {
                if !comps.contains(&c) {
                    comps.push(c);
                }
            }
        }
        comps
    }

    pub(super) fn closest_reachable_shore(&mut self, target_small: u16, attacker: usize, tile: u32) -> Option<u32> {
        let reachable = self.reachable_components(attacker);
        if reachable.is_empty() {
            return None;
        }
        let valid = move |g: &Game, t: u32| {
            g.map.is_shore(t)
                && g.map.owner_id(t) == target_small
                && g.water_component(t).is_some_and(|c| reachable.contains(&c))
        };
        if valid(self, tile) {
            return Some(tile);
        }
        self.bfs_nearest(tile, 50, &valid)
    }

    pub(super) fn target_transport_tile(&mut self, attacker: usize, tile: u32) -> Option<u32> {
        let owner_small = self.map.owner_id(tile);
        self.closest_reachable_shore(owner_small, attacker, tile)
    }

    pub(super) fn closest_shore_by_water(&mut self, p: usize, target: u32) -> Option<u32> {
        if !self.map.is_water(target) && !self.map.is_shore(target) {
            return None;
        }
        let target_comp = self.water_component(target)?;
        let shores: Vec<u32> = self.players[p]
            .border_tiles
            .values()
            .filter(|&t| self.map.is_shore(t) && self.water_component(t) == Some(target_comp))
            .collect();
        if shores.is_empty() {
            return None;
        }
        let path = self.water.find_path(&self.map, &shores, target)?;
        if path.is_empty() {
            return None;
        }
        Some(self.refine_start_tile(&path, &shores))
    }

    pub(super) fn refine_start_tile(&mut self, path: &[u32], shores: &[u32]) -> u32 {
        const MAX_AREA: i64 = 100 * 100;
        if path.len() <= 50 {
            return path[0];
        }
        let best = path[0];
        let m = &self.map;
        let candidates: Vec<u32> = shores.iter().copied().filter(|&s| m.manhattan_dist(s, best) <= 20).collect();
        if candidates.len() <= 1 {
            return best;
        }
        let (mut cmin_x, mut cmin_y) = (m.x(candidates[0]) as i64, m.y(candidates[0]) as i64);
        let (mut cmax_x, mut cmax_y) = (cmin_x, cmin_y);
        for &c in &candidates[1..] {
            let (x, y) = (m.x(c) as i64, m.y(c) as i64);
            cmin_x = cmin_x.min(x);
            cmax_x = cmax_x.max(x);
            cmin_y = cmin_y.min(y);
            cmax_y = cmax_y.max(y);
        }
        let (mut lo, mut hi) = (50i64, 200.min(path.len() as i64 - 1));
        let mut best_wp = lo;
        let mut i = 0;
        while i < 5 && lo <= hi {
            let mid = (lo + hi) >> 1;
            let wp = path[mid as usize];
            let (wx, wy) = (m.x(wp) as i64, m.y(wp) as i64);
            let (min_x, max_x) = (cmin_x.min(wx) - 10, cmax_x.max(wx) + 10);
            let (min_y, max_y) = (cmin_y.min(wy) - 10, cmax_y.max(wy) + 10);
            if (max_x - min_x + 1) * (max_y - min_y + 1) <= MAX_AREA {
                best_wp = mid;
                lo = mid + 1;
            } else {
                hi = mid - 1;
            }
            i += 1;
        }
        let wp = path[best_wp as usize];
        let (wx, wy) = (m.x(wp) as i64, m.y(wp) as i64);
        let b = Bounds {
            min_x: 0.max(cmin_x.min(wx) - 10),
            max_x: (m.width as i64 - 1).min(cmax_x.max(wx) + 10),
            min_y: 0.max(cmin_y.min(wy) - 10),
            max_y: (m.height as i64 - 1).min(cmax_y.max(wy) + 10),
        };
        if (b.max_x - b.min_x + 1) * (b.max_y - b.min_y + 1) > MAX_AREA {
            return best;
        }
        self.water.refine_astar.search(&self.map, &candidates, wp, b).and_then(|p| p.first().copied()).unwrap_or(best)
    }

    /// TransportShipUtils.canBuildTransportShip
    pub(super) fn can_build_transport_ship(&mut self, p: usize, tile: u32) -> Option<u32> {
        if self.unit_count(p, TRANSPORT_SHIP) >= 3 {
            return None;
        }
        let dst = self.target_transport_tile(p, tile)?;
        let other = self.owner_index(tile);
        if other == Some(p) {
            return None;
        }
        if let Some(o) = other {
            if !self.can_attack_player(p, o) {
                return None;
            }
        }
        self.closest_shore_by_water(p, dst)
    }

    // ── TransportShipExecution ──

    pub(super) fn init_transport(&mut self, te: &mut TransportShipExec) {
        if !self.is_valid_ref(te.ref_tile) {
            te.active = false;
            return;
        }
        let ref_tile = te.ref_tile as u32;
        te.last_move = self.ticks;
        te.target = self.owner_index(ref_tile);
        if self.unit_count(te.attacker, TRANSPORT_SHIP) >= 3 {
            te.active = false;
            return;
        }
        if let Some(t) = te.target {
            if self.players[t].info.player_type != PlayerType::Bot && self.players[te.attacker].info.player_type != PlayerType::Bot {
                self.reject_incoming_alliance_requests(te.attacker, t);
            }
        }
        if te.target == Some(te.attacker) {
            te.active = false;
            return;
        }
        if let Some(t) = te.target {
            if !self.can_attack_player(te.attacker, t) {
                te.active = false;
                return;
            }
        }
        let troops = te.troops.unwrap_or_else(|| (self.players[te.attacker].troops_f() / 5.0).floor());
        let troops = js_min(troops, self.players[te.attacker].troops_f());
        te.troops = Some(troops);
        let Some(dst) = self.target_transport_tile(te.attacker, ref_tile) else {
            te.active = false;
            return;
        };
        te.dst = Some(dst);
        // canBuild: the player must be alive (the ship costs nothing).
        let src = if self.players[te.attacker].is_alive() { self.can_build_transport_ship(te.attacker, dst) } else { None };
        let Some(src) = src else {
            te.active = false;
            return;
        };
        te.boat = Some(self.build_unit(te.attacker, TRANSPORT_SHIP, src, troops, Some(dst)));
        // The motion plan path is for the client only.
    }

    pub(super) fn step(&mut self, st: &mut Stepper, from: u32, to: u32) -> Step {
        if from == to {
            return Step::Complete;
        }
        if st.last_to != Some(to) {
            st.path = None;
            st.index = 0;
            st.last_to = Some(to);
        }
        if st.path.is_none() {
            let Some(path) = self.water.find_path(&self.map, &[from], to) else {
                return Step::NotFound;
            };
            st.index = if !path.is_empty() && path[0] == from { 1 } else { 0 };
            st.path = Some(path);
        }
        let path = st.path.as_ref().unwrap();
        if st.index > 0 && path[st.index - 1] != from {
            st.path = None;
            st.index = 0;
            st.last_to = Some(to);
            return self.step(st, from, to);
        }
        if st.index >= path.len() {
            return Step::Complete;
        }
        let next = path[st.index];
        st.index += 1;
        Step::Next(next)
    }

    pub(super) fn tick_transport(&mut self, te: &mut TransportShipExec) {
        let Some(dst) = te.dst else {
            te.active = false;
            return;
        };
        if !te.active {
            return;
        }
        let boat = te.boat.expect("boat");
        if !self.units[boat].active {
            te.active = false;
            return;
        }
        if self.ticks - te.last_move < 1 {
            return;
        }
        te.last_move = self.ticks;
        // A teammate who conquered a disconnected player took its boats: it sails on for them.
        let boat_owner = self.units[boat].owner;
        if self.players[te.original_owner].is_disconnected
            && boat_owner != te.original_owner
            && self.is_on_same_team(boat_owner, te.original_owner)
        {
            te.attacker = boat_owner;
            te.original_owner = boat_owner;
        }
        let mut dst = dst;
        if self.units[boat].retreating {
            if te.retreat_dst.is_none() {
                let from = self.units[boat].tile;
                te.retreat_dst = Some(self.closest_shore_by_water(te.attacker, from));
            }
            match te.retreat_dst.unwrap() {
                None => {
                    let troops = self.units[boat].troops;
                    self.add_troops(te.attacker, troops);
                    self.delete_unit(boat);
                    te.active = false;
                    return;
                }
                Some(r) => {
                    te.dst = Some(r);
                    dst = r;
                    self.units[boat].target_tile = Some(r);
                }
            }
        }
        let mut stepper = std::mem::take(&mut te.stepper);
        let from = self.units[boat].tile;
        let result = self.step(&mut stepper, from, dst);
        te.stepper = stepper;
        match result {
            Step::Complete => {
                let troops = self.units[boat].troops;
                if self.owner_index(dst) == Some(te.attacker) {
                    let deaths = troops * (25.0 / 100.0);
                    self.add_troops(te.attacker, troops - deaths);
                    self.delete_unit(boat);
                    te.active = false;
                    return;
                }
                self.conquer(te.attacker, dst);
                let friendly = te.target.is_some_and(|t| self.is_friendly(te.attacker, t, false));
                if friendly {
                    self.add_troops(te.attacker, troops);
                } else {
                    let target_id = te.target.map(|t| self.players[t].info.id.clone());
                    let e = self.new_landing_attack(troops, te.attacker, target_id, dst);
                    self.add_execution(e);
                }
                self.delete_unit(boat);
                te.active = false;
            }
            Step::Next(tile) => self.move_unit(boat, tile),
            Step::NotFound => {
                let troops = self.units[boat].troops;
                self.add_troops(te.attacker, troops);
                self.delete_unit(boat);
                te.active = false;
            }
        }
    }

    // ── AiAttackBehavior boats ──

    pub(super) fn shore_tiles(&self, p: usize) -> Vec<u32> {
        self.players[p].border_tiles.values().filter(|&t| self.map.is_shore(t)).collect()
    }

    pub(super) fn send_boat_attack(&mut self, te: &mut TribeExec, target: usize) -> bool {
        let Some(landing) = self.plan_boat_route(te.player, target) else { return false };
        let troops = self.players[te.player].troops_f() / 5.0;
        if troops < 1.0 {
            return false;
        }
        let e = self.new_transport(te.player, landing as f64, Some(troops));
        self.add_execution(e);
        true
    }

    /// planBoatRoute for tribes, whose sail() never reports a blocker: the
    /// direct route to the closest pair of shore tiles, or when it has no
    /// launch, the shortest of the other landings.
    pub(super) fn plan_boat_route(&mut self, p: usize, target: usize) -> Option<u32> {
        if self.unit_count(p, TRANSPORT_SHIP) >= 3 {
            return None;
        }
        let ours = self.shore_tiles(p);
        let theirs = self.shore_tiles(target);
        let (_, closest) = closest_two_tiles(&self.map, &ours, &theirs)?;
        // routeTo: the length is from the launch to the tile the boat lands on.
        let route_length = |g: &mut Game, landing: u32| -> Option<u32> {
            let launch = g.can_build_transport_ship(p, landing)?;
            let land = g.target_transport_tile(p, landing).unwrap_or(landing);
            Some(g.map.manhattan_dist(launch, land))
        };
        if route_length(self, closest).is_some() {
            return Some(closest);
        }
        // Hostile warships only matter for where the other landings may be.
        let warships = self.hostile_warships(p);
        let mut best: Option<(u32, u32)> = None;
        for landing in self.other_landings(&ours, &theirs, closest, &warships) {
            if let Some(len) = route_length(self, landing) {
                if best.is_none_or(|b| len < b.1) {
                    best = Some((landing, len));
                }
            }
        }
        best.map(|b| b.0)
    }

    pub(super) fn send_boat_attack_to_nearby_terra_nullius(&mut self, te: &mut TribeExec) -> bool {
        let p = te.player;
        if self.unit_count(p, TRANSPORT_SHIP) >= 3 {
            return false;
        }
        let shores = self.shore_tiles(p);
        let (w, h) = (self.map.width as i64, self.map.height as i64);
        let mut i = 0;
        while i < shores.len() {
            let border = shores[i];
            let (bx, by) = (self.map.x(border) as i64, self.map.y(border) as i64);
            for (dx, dy) in [(0i64, -1i64), (0, 1), (-1, 0), (1, 0)] {
                let (x1, y1) = (bx + dx, by + dy);
                if x1 < 0 || y1 < 0 || x1 >= w || y1 >= h || !self.map.is_water(self.map.ref_at(x1 as u32, y1 as u32)) {
                    continue;
                }
                let (nx, ny) = (bx + dx * 5, by + dy * 5);
                if nx < 0 || ny < 0 || nx >= w || ny >= h {
                    continue;
                }
                let tile = self.map.ref_at(nx as u32, ny as u32);
                if !self.map.is_land(tile) || self.map.is_impassable(tile) || self.map.has_owner(tile) || self.map.has_fallout(tile) {
                    continue;
                }
                if self.can_build_transport_ship(p, tile).is_none() {
                    continue;
                }
                // sail(): resolves the landing for its length; no warships block.
                let _ = self.target_transport_tile(p, tile);
                let troops = self.players[p].troops_f() / 5.0;
                if troops < 1.0 {
                    return false;
                }
                let e = self.new_transport(p, tile as f64, Some(troops));
                self.add_execution(e);
                return true;
            }
            i += 10;
        }
        false
    }
}

/// execution/Util closestTwoTiles: a sweep over both lists sorted by x.
pub(super) fn closest_two_tiles(map: &GameMap, x: &[u32], y: &[u32]) -> Option<(u32, u32)> {
    let mut xs = x.to_vec();
    let mut ys = y.to_vec();
    xs.sort_by_key(|&t| map.x(t));
    ys.sort_by_key(|&t| map.x(t));
    if xs.is_empty() || ys.is_empty() {
        return None;
    }
    let (mut i, mut j) = (0, 0);
    let mut min = u32::MAX;
    let mut result = (xs[0], ys[0]);
    while i < xs.len() && j < ys.len() {
        let (cx, cy) = (xs[i], ys[j]);
        let (cxx, cyx) = (map.x(cx), map.x(cy));
        let d = cxx.abs_diff(cyx) + map.y(cx).abs_diff(map.y(cy));
        if d < min {
            min = d;
            result = (cx, cy);
        }
        if i == xs.len() - 1 {
            j += 1;
        } else if j == ys.len() - 1 {
            i += 1;
        } else if cxx < cyx {
            i += 1;
        } else {
            j += 1;
        }
    }
    Some(result)
}

