use super::*;

// ── Warships, shells, trade ship capture (WarshipExecution, ShellExecution,
// MoveWarshipExecution, TradeShipExecution) ──

pub const WARSHIP: &str = "Warship";
pub const SHELL: &str = "Shell";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WarshipMode {
    Patrolling,
    Retreating,
    Docked,
}

#[derive(Clone, Debug)]
pub struct WarshipState {
    pub mode: WarshipMode,
    pub patrol_tile: u32,
    pub retreat_port: Option<u32>,
    pub veterancy: u32,
    pub progress: u32,
}

pub struct WarshipExec {
    pub(super) owner: usize,
    pub(super) patrol_tile: u32,
    pub(super) warship: Option<usize>,
    pub(super) random: Option<PseudoRandom>,
    pub(super) stepper: Stepper,
    pub(super) last_shell_attack: u32,
    pub(super) already_sent: Vec<usize>,
    pub(super) last_manual_move_tick: u32,
    pub(super) last_observed_patrol: Option<u32>,
    pub(super) healing_remainder: f64,
}

pub struct ShellExec {
    pub(super) active: bool,
    pub(super) spawn: u32,
    pub(super) owner: usize,
    pub(super) owner_unit: usize,
    pub(super) target: usize,
    pub(super) shell: Option<usize>,
    pub(super) destroy_at: i64,
    pub(super) random: Option<PseudoRandom>,
    pub(super) air_seed: u32,
    pub(super) air: Stepper,
}

/// PathFinderStepper.next with the node a COMPLETE result carries.
pub enum StepTo {
    Next(u32),
    Complete(u32),
    NotFound,
}

impl Game {
    pub fn new_warship(&self, owner: usize, patrol_tile: u32) -> Exec {
        Exec::Warship(Box::new(WarshipExec {
            owner,
            patrol_tile,
            warship: None,
            random: None,
            stepper: Stepper::default(),
            last_shell_attack: 0,
            already_sent: Vec::new(),
            last_manual_move_tick: 0,
            last_observed_patrol: None,
            healing_remainder: 0.0,
        }))
    }

    /// Unit.move: also re-files the unit in the unit grid when it changes cell.
    pub(super) fn move_unit(&mut self, u: usize, tile: u32) {
        let old = self.units[u].tile;
        let cell = |t: u32| (self.map.x(t) / 100, self.map.y(t) / 100);
        let changed = cell(old) != cell(tile);
        if changed {
            self.grid_seq += 1;
            self.units[u].grid_seq = self.grid_seq;
            let (from, to) = (self.cell_of(old), self.cell_of(tile));
            let cell = &mut self.cells[from];
            let pos = cell.iter().position(|&x| x == u).expect("unit in its cell");
            cell.swap_remove(pos);
            self.cells[to].push(u);
        }
        let unit = &mut self.units[u];
        unit.last_tile = unit.tile;
        unit.tile = tile;
    }

    pub(super) fn ws(&self, u: usize) -> &WarshipState {
        self.units[u].warship.as_ref().expect("warship")
    }
    pub(super) fn ws_mut(&mut self, u: usize) -> &mut WarshipState {
        self.units[u].warship.as_mut().expect("warship")
    }

    pub(super) fn max_health(&self, u: usize) -> i64 {
        let base: i64 = if self.units[u].type_name == WARSHIP { 1000 } else { 1 };
        let vet = self.units[u].warship.as_ref().map_or(0, |w| w.veterancy) as i64;
        if vet <= 0 {
            base
        } else {
            base + (base * vet * 20).div_euclid(100)
        }
    }

    pub(super) fn modify_health(&mut self, u: usize, delta: f64, attacker: Option<usize>) {
        let prev = self.units[u].health;
        let next = (prev + delta.floor() as i64).clamp(0, self.max_health(u));
        if next == prev {
            return;
        }
        self.units[u].health = next;
        if next == 0 {
            self.units[u].destroyer = attacker;
            self.delete_unit(u);
        }
    }

    pub(super) fn increase_veterancy(&mut self, u: usize) {
        let w = self.ws_mut(u);
        if w.veterancy >= 3 {
            return;
        }
        w.veterancy += 1;
    }

    pub(super) fn add_veterancy_progress(&mut self, u: usize, from_transport: bool) {
        if self.ws(u).veterancy >= 3 {
            return;
        }
        self.ws_mut(u).progress += if from_transport { 25 } else { 10 };
        while self.ws(u).progress >= 250 && self.ws(u).veterancy < 3 {
            self.ws_mut(u).progress -= 250;
            self.increase_veterancy(u);
        }
        if self.ws(u).veterancy >= 3 {
            self.ws_mut(u).progress = 0;
        }
    }

    pub(super) fn can_attack_player_afk(&self, attacker: usize, target: usize, treat_afk: bool) -> bool {
        if self.players[attacker].info.player_type != PlayerType::Human {
            return !self.is_friendly(attacker, target, treat_afk);
        }
        !self.is_immune(target) && !self.is_friendly(attacker, target, treat_afk)
    }

    pub(super) fn step_to(&mut self, st: &mut Stepper, from: u32, to: u32, dist: Option<u32>) -> StepTo {
        let _p = crate::prof::scope("step_to");
        if from == to {
            return StepTo::Complete(to);
        }
        if let Some(d) = dist {
            if d > 0 && self.map.manhattan_dist(from, to) <= d {
                return StepTo::Complete(from);
            }
        }
        match self.step(st, from, to) {
            Step::Next(t) => StepTo::Next(t),
            Step::Complete => StepTo::Complete(to),
            Step::NotFound => StepTo::NotFound,
        }
    }

    /// PlayerImpl.warshipSpawn
    pub(super) fn warship_spawn(&self, p: usize, tile: u32) -> Option<u32> {
        if !self.map.is_water(tile) {
            return None;
        }
        let comp = self.water_component(tile);
        let mut best = None;
        let mut best_d = u32::MAX;
        for &port in &self.players[p].units {
            let u = &self.units[port];
            if !(u.type_name == PORT && u.active && !u.under_construction && comp.is_some_and(|c| self.has_water_component(u.tile, c))) {
                continue;
            }
            let d = self.map.manhattan_dist(u.tile, tile);
            if d < best_d {
                best_d = d;
                best = Some(u.tile);
            }
        }
        best
    }

    pub(super) fn init_warship(&mut self, we: &mut WarshipExec) {
        we.random = Some(PseudoRandom::new(self.ticks as f64));
        let p = we.owner;
        if self.unit_cost(p, WARSHIP) > self.players[p].gold || !self.players[p].is_alive() {
            return;
        }
        let Some(spawn) = self.warship_spawn(p, we.patrol_tile) else { return };
        let w = self.build_unit(p, WARSHIP, spawn, 0.0, None);
        self.units[w].health = 1000;
        self.units[w].warship = Some(WarshipState {
            mode: WarshipMode::Patrolling,
            patrol_tile: we.patrol_tile,
            retreat_port: None,
            veterancy: 0,
            progress: 0,
        });
        we.warship = Some(w);
        we.last_observed_patrol = Some(we.patrol_tile);
    }

    pub(super) fn tick_warship(&mut self, we: &mut WarshipExec) {
        let w = we.warship.expect("warship");
        if self.units[w].health <= 0 {
            self.delete_unit(w);
            return;
        }
        let health_before = self.units[w].health;
        // A side under the Doomsday Clock can't repair its navy.
        if !self.in_doomsday(self.units[w].owner) {
            self.heal_warship(we, w);
        }
        // handleManualPatrolOverride
        let pt = self.ws(w).patrol_tile;
        if we.last_observed_patrol.is_some_and(|o| o != pt) {
            we.last_manual_move_tick = self.ticks;
            if self.ws(w).mode != WarshipMode::Patrolling {
                self.cancel_repair_retreat(we, w, false);
            }
        }
        we.last_observed_patrol = Some(pt);
        // Nothing to retreat to a port for while it can't repair: keep patrolling.
        if self.in_doomsday(self.units[w].owner) && self.ws(w).mode != WarshipMode::Patrolling {
            self.cancel_repair_retreat(we, w, true);
        }

        if self.ws(w).mode == WarshipMode::Docked {
            if self.current_retreat_port(w).is_none() {
                self.cancel_repair_retreat(we, w, true);
            }
            if self.units[w].health >= self.max_health(w) {
                self.cancel_repair_retreat(we, w, true);
            }
            if self.ws(w).mode == WarshipMode::Docked {
                return;
            }
        }
        if self.handle_repair_retreat(we, w) {
            return;
        }
        if self.should_start_repair_retreat(we, w, health_before) {
            self.start_repair_retreat(we, w);
            if self.handle_repair_retreat(we, w) {
                return;
            }
        }
        let target = self.find_best_target(we, w, &[TRANSPORT_SHIP, WARSHIP, TRADE_SHIP], true);
        self.units[w].target_unit = target;
        match target.map(|t| self.units[t].type_name) {
            Some(TRANSPORT_SHIP) | Some(WARSHIP) => {
                self.shoot_target(we, w);
                self.patrol(we, w);
            }
            Some(TRADE_SHIP) => self.hunt_trade_ship(we, w),
            _ => self.patrol(we, w),
        }
    }

    pub(super) fn heal_warship(&mut self, we: &mut WarshipExec, w: usize) {
        let owner = self.units[w].owner;
        let tile = self.units[w].tile;
        if self.players[owner]
            .units
            .iter()
            .any(|&p| self.units[p].type_name == PORT && self.dist2(tile, self.units[p].tile) <= 150 * 150)
        {
            self.modify_health(w, 1.0, None);
        }
        if self.ws(w).mode == WarshipMode::Docked {
            // applyActiveDockedHealing
            let Some(port) = self.current_retreat_port(w) else { return };
            let docked = self.docked_ships_at_port(w, port, None);
            if !docked.contains(&w) {
                return;
            }
            let pool = self.units[port].level as f64 * 5.0;
            if pool <= 0.0 || docked.is_empty() {
                return;
            }
            we.healing_remainder += pool / docked.len() as f64;
            let ih = we.healing_remainder.floor();
            if ih <= 0.0 {
                return;
            }
            we.healing_remainder -= ih;
            self.modify_health(w, ih, None);
        }
    }

    pub(super) fn current_retreat_port(&self, w: usize) -> Option<usize> {
        let rp = self.ws(w).retreat_port?;
        self.players[self.units[w].owner]
            .units
            .iter()
            .copied()
            .find(|&p| self.units[p].type_name == PORT && self.units[p].tile == rp)
    }

    pub(super) fn docked_ships_at_port(&self, w: usize, port: usize, exclude: Option<usize>) -> Vec<usize> {
        let owner = self.units[w].owner;
        self.nearby_units(self.units[port].tile, 5, &[WARSHIP])
            .into_iter()
            .map(|(u, _)| u)
            .filter(|&s| {
                Some(s) != exclude
                    && self.units[s].owner == owner
                    && self.ws(s).mode != WarshipMode::Patrolling
                    && self.units[s].target_tile.is_none()
            })
            .collect()
    }

    pub(super) fn is_port_full(&self, w: usize, port: usize, exclude: Option<usize>) -> bool {
        self.docked_ships_at_port(w, port, exclude).len() as u32 >= self.units[port].level
    }

    pub(super) fn cancel_repair_retreat(&mut self, we: &mut WarshipExec, w: usize, clear_target: bool) {
        we.healing_remainder = 0.0;
        let s = self.ws_mut(w);
        s.mode = WarshipMode::Patrolling;
        s.retreat_port = None;
        if clear_target {
            self.units[w].target_tile = None;
        }
    }

    pub(super) fn nearest_available_port(&self, w: usize, exclude: Option<usize>) -> Option<(u32, i64)> {
        let tile = self.units[w].tile;
        let comp = self.water_component(tile)?;
        let mut best = None;
        let mut best_d = f64::INFINITY;
        for &port in &self.players[self.units[w].owner].units {
            if self.units[port].type_name != PORT || self.is_port_full(w, port, exclude) {
                continue;
            }
            let pt = self.units[port].tile;
            if !self.has_water_component(pt, comp) {
                continue;
            }
            let d = self.dist2(tile, pt);
            if (d as f64) < best_d {
                best_d = d as f64;
                best = Some((pt, d));
            }
        }
        best
    }

    pub(super) fn refresh_retreat_port(&mut self, w: usize) -> bool {
        let owner = self.units[w].owner;
        if !self.players[owner].units.iter().any(|&p| self.units[p].type_name == PORT) {
            return false;
        }
        let cur = self.ws(w).retreat_port;
        let cur_port = cur.and_then(|c| {
            self.players[owner]
                .units
                .iter()
                .copied()
                .find(|&p| self.units[p].type_name == PORT && self.units[p].tile == c)
        });
        let Some(cp) = cur_port else {
            let np = self.nearest_available_port(w, Some(w)).map(|r| r.0);
            self.ws_mut(w).retreat_port = np;
            return np.is_some();
        };
        if self.is_port_full(w, cp, None) {
            if let Some(alt) = self.nearest_available_port(w, None) {
                self.ws_mut(w).retreat_port = Some(alt.0);
            }
            return self.ws(w).retreat_port.is_some();
        }
        // findBetterPortTile
        if let Some((t, d)) = self.nearest_available_port(w, None) {
            let cur_d = cur.map_or(f64::INFINITY, |c| self.dist2(self.units[w].tile, c) as f64);
            if (d as f64) < cur_d * 0.75 && Some(t) != cur {
                self.ws_mut(w).retreat_port = Some(t);
            }
        }
        true
    }

    pub(super) fn handle_repair_retreat(&mut self, we: &mut WarshipExec, w: usize) -> bool {
        if self.ws(w).mode == WarshipMode::Patrolling {
            return false;
        }
        let aggro = self.find_best_target(we, w, &[TRANSPORT_SHIP, WARSHIP], false);
        if let Some(a) = aggro {
            self.units[w].target_unit = Some(a);
            self.shoot_target(we, w);
        }
        if !self.refresh_retreat_port(w) {
            self.cancel_repair_retreat(we, w, true);
            return false;
        }
        if aggro.is_none() {
            self.units[w].target_unit = None;
        }
        let Some(rp) = self.ws(w).retreat_port else { return false };
        if self.dist2(self.units[w].tile, rp) <= 25 {
            let port = self.players[self.units[w].owner]
                .units
                .iter()
                .copied()
                .find(|&p| self.units[p].type_name == PORT && self.units[p].tile == rp);
            if port.is_some_and(|p| !self.is_port_full(w, p, Some(w))) {
                self.units[w].target_tile = None;
                self.ws_mut(w).mode = WarshipMode::Docked;
                return true;
            }
            if self.units[w].health >= self.max_health(w) {
                self.cancel_repair_retreat(we, w, true);
                return false;
            }
            return true;
        }
        self.units[w].target_tile = Some(rp);
        let mut st = std::mem::take(&mut we.stepper);
        let r = self.step_to(&mut st, self.units[w].tile, rp, None);
        we.stepper = st;
        match r {
            StepTo::Complete(n) => {
                self.move_unit(w, n);
                if n == rp {
                    self.units[w].target_tile = None;
                }
            }
            StepTo::Next(n) => self.move_unit(w, n),
            StepTo::NotFound => {
                let np = self.nearest_available_port(w, Some(w)).map(|r| r.0);
                self.ws_mut(w).retreat_port = np;
                if np.is_none() {
                    self.cancel_repair_retreat(we, w, true);
                }
            }
        }
        true
    }

    pub(super) fn should_start_repair_retreat(&self, we: &WarshipExec, w: usize, health_before: i64) -> bool {
        if self.ws(w).mode != WarshipMode::Patrolling || self.in_doomsday(self.units[w].owner) {
            return false;
        }
        if (self.ticks as i64 - we.last_manual_move_tick as i64) < 50 {
            return false;
        }
        let threshold = (self.max_health(w) * 75).div_euclid(100);
        if health_before >= threshold {
            return false;
        }
        self.players[self.units[w].owner].units.iter().any(|&p| self.units[p].type_name == PORT)
    }

    pub(super) fn start_repair_retreat(&mut self, we: &mut WarshipExec, w: usize) {
        // findNearestPort
        let owner = self.units[w].owner;
        if !self.players[owner].units.iter().any(|&p| self.units[p].type_name == PORT) {
            return;
        }
        let tile = self.units[w].tile;
        let Some(comp) = self.water_component(tile) else { return };
        let mut best = None;
        let mut best_d = f64::INFINITY;
        for &p in &self.players[owner].units {
            if self.units[p].type_name != PORT {
                continue;
            }
            let pt = self.units[p].tile;
            if self.water_component(pt) != Some(comp) {
                continue;
            }
            let d = self.dist2(tile, pt) as f64;
            if d < best_d {
                best_d = d;
                best = Some(pt);
            }
        }
        let Some(port) = best else { return };
        let s = self.ws_mut(w);
        s.retreat_port = Some(port);
        s.mode = WarshipMode::Retreating;
        we.healing_remainder = 0.0;
        self.units[w].target_unit = None;
    }

    pub(super) fn find_best_target(&self, we: &WarshipExec, w: usize, types: &[&str], include_trade: bool) -> Option<usize> {
        let owner = self.units[w].owner;
        let ships = self.nearby_units(self.units[w].tile, 130, types);
        let mut lazy: Option<(bool, u32)> = None;
        let mut best: Option<(usize, u32, i64)> = None;
        for (u, d2) in ships {
            let unit = &self.units[u];
            if u == w
                || unit.owner == owner
                || !self.can_attack_player_afk(owner, unit.owner, true)
                || we.already_sent.contains(&u)
                || (unit.type_name == WARSHIP && self.ws(u).mode == WarshipMode::Docked)
            {
                continue;
            }
            if include_trade && unit.type_name == TRADE_SHIP {
                let (reachable, patrol) = *lazy.get_or_insert_with(|| {
                    let comp = self.water_component(self.units[w].tile);
                    let reachable = comp.is_some_and(|c| {
                        self.players[owner].units.iter().any(|&p| {
                            let pu = &self.units[p];
                            pu.type_name == PORT && pu.active && pu.deletion_at.is_none() && !pu.under_construction && self.has_water_component(pu.tile, c)
                        })
                    });
                    (reachable, self.ws(w).patrol_tile)
                });
                let target_owner = unit.target_unit.map(|t| self.units[t].owner);
                if !reachable
                    || (self.ticks as i64 - unit.last_safe_from_pirates as i64) < 20
                    || target_owner == Some(owner)
                    || target_owner.is_some_and(|o| self.is_friendly(o, owner, false))
                {
                    continue;
                }
                if self.dist2(patrol, unit.tile) > 100 * 100 {
                    continue;
                }
            }
            let prio = match unit.type_name {
                TRANSPORT_SHIP => 0,
                WARSHIP => 1,
                _ => 2,
            };
            if best.is_none_or(|(_, bp, bd)| prio < bp || (prio == bp && d2 < bd)) {
                best = Some((u, prio, d2));
            }
        }
        best.map(|b| b.0)
    }

    pub(super) fn shoot_target(&mut self, we: &mut WarshipExec, w: usize) {
        if self.ticks as i64 - we.last_shell_attack as i64 > 20 {
            let target = self.units[w].target_unit.expect("target");
            if self.units[target].type_name != TRANSPORT_SHIP {
                we.last_shell_attack = self.ticks;
            }
            let e = Exec::Shell(Box::new(ShellExec {
                active: true,
                spawn: self.units[w].tile,
                owner: self.units[w].owner,
                owner_unit: w,
                target,
                shell: None,
                destroy_at: -1,
                random: None,
                air_seed: 0,
                air: Stepper::default(),
            }));
            self.add_execution(e);
            if self.units[target].type_name != WARSHIP {
                we.already_sent.push(target);
                self.units[w].target_unit = None;
            }
        }
    }

    pub(super) fn hunt_trade_ship(&mut self, we: &mut WarshipExec, w: usize) {
        for _ in 0..2 {
            let target = self.units[w].target_unit.expect("target");
            let tt = self.units[target].tile;
            let dist = self.map.manhattan_dist(self.units[w].tile, tt);
            if dist <= 5 {
                self.capture_trade_ship(w, target);
                return;
            }
            if dist <= 20 {
                let wt = self.units[w].tile;
                let mut best = None;
                let mut best_d = dist;
                let mut nb = [0u32; 4];
                let n = self.map.neighbors4(wt, &mut nb);
                for &t in &nb[..n] {
                    if !self.map.is_water(t) {
                        continue;
                    }
                    let d = self.map.manhattan_dist(t, tt);
                    if d < best_d {
                        best_d = d;
                        best = Some(t);
                    }
                }
                if let Some(t) = best {
                    self.move_unit(w, t);
                    continue;
                }
            }
            let mut st = std::mem::take(&mut we.stepper);
            let r = self.step_to(&mut st, self.units[w].tile, tt, Some(5));
            we.stepper = st;
            match r {
                StepTo::Complete(_) => {
                    self.capture_trade_ship(w, target);
                    return;
                }
                StepTo::Next(n) => self.move_unit(w, n),
                StepTo::NotFound => {}
            }
        }
    }

    pub(super) fn capture_trade_ship(&mut self, w: usize, target: usize) {
        let owner = self.units[w].owner;
        assert_ne!(self.units[target].owner, owner, "Cannot capture unit");
        self.set_unit_owner(target, owner);
        self.add_veterancy_progress(w, false);
        self.units[w].target_unit = None;
    }

    pub(super) fn patrol(&mut self, we: &mut WarshipExec, w: usize) {
        if self.units[w].target_tile.is_none() {
            let t = self.random_patrol_tile(we, w, false);
            self.units[w].target_tile = t;
            if t.is_none() {
                return;
            }
        }
        let tt = self.units[w].target_tile.unwrap();
        let mut st = std::mem::take(&mut we.stepper);
        let r = self.step_to(&mut st, self.units[w].tile, tt, None);
        we.stepper = st;
        match r {
            StepTo::Complete(n) => {
                self.units[w].target_tile = None;
                self.move_unit(w, n);
            }
            StepTo::Next(n) => self.move_unit(w, n),
            StepTo::NotFound => self.units[w].target_tile = None,
        }
    }

    pub(super) fn random_patrol_tile(&mut self, we: &mut WarshipExec, w: usize, allow_shoreline: bool) -> Option<u32> {
        let mut range: f64 = 100.0;
        let mut attempts = 0;
        let mut expand = 0;
        let comp = self.water_component(self.units[w].tile);
        let pt = self.ws(w).patrol_tile;
        let (px, py) = (self.map.x(pt) as f64, self.map.y(pt) as f64);
        let (mw, mh) = (self.map.width as f64, self.map.height as f64);
        while expand < 3 {
            let random = we.random.as_mut().unwrap();
            let x = px + random.next_int(-range / 2.0, range / 2.0);
            let y = py + random.next_int(-range / 2.0, range / 2.0);
            if !(x >= 0.0 && x < mw && y >= 0.0 && y < mh) {
                continue;
            }
            let tile = self.map.ref_at(x as u32, y as u32);
            let bad = !self.map.is_water(tile)
                || (!allow_shoreline && self.map.is_shoreline(tile))
                || comp.is_some_and(|c| !self.has_water_component(tile, c));
            if bad {
                attempts += 1;
                if attempts == 500 {
                    expand += 1;
                    attempts = 0;
                    range += (range / 2.0).floor();
                }
                continue;
            }
            return Some(tile);
        }
        if !allow_shoreline {
            return self.random_patrol_tile(we, w, true);
        }
        None
    }

    // ── ShellExecution ──

    pub(super) fn tick_shell(&mut self, se: &mut ShellExec) {
        let shell = match se.shell {
            Some(s) => s,
            None => {
                let s = self.build_unit(se.owner, SHELL, se.spawn, 0.0, None);
                se.shell = Some(s);
                s
            }
        };
        if !self.units[shell].active {
            se.active = false;
            return;
        }
        if !self.units[se.target].active
            || self.units[se.target].owner == self.units[shell].owner
            || (se.destroy_at != -1 && self.ticks as i64 >= se.destroy_at)
        {
            self.delete_unit(shell);
            se.active = false;
            return;
        }
        if se.destroy_at == -1 && !self.units[se.owner_unit].active {
            se.destroy_at = self.ticks as i64 + 50;
        }
        for _ in 0..3 {
            let (from, to) = (self.units[shell].tile, self.units[se.target].tile);
            match self.air_step(se, from, to) {
                Some(next) => self.move_unit(shell, next),
                None => {
                    se.active = false;
                    let target = se.target;
                    let target_type = self.units[target].type_name;
                    let was_active = self.units[target].active;
                    // effectOnTarget
                    let roll = se.random.as_mut().unwrap().next_int(1.0, 6.0) as i64;
                    let mut mult = (roll - 1) * 25 + 200;
                    let vet = self.units[se.owner_unit].warship.as_ref().map_or(0, |w| w.veterancy) as i64;
                    if vet > 0 {
                        mult = (mult * (100 + vet * 20)).div_euclid(100);
                    }
                    self.modify_health(target, -(mult as f64), Some(se.owner));
                    let ou = se.owner_unit;
                    if was_active && !self.units[target].active && self.units[ou].active && self.units[ou].type_name == WARSHIP {
                        if target_type == WARSHIP {
                            self.ws_mut(ou).progress = 0;
                            self.increase_veterancy(ou);
                        } else if target_type == TRANSPORT_SHIP {
                            self.add_veterancy_progress(ou, true);
                        }
                    }
                    self.delete_unit(shell);
                    return;
                }
            }
        }
    }

    /// The Air stepper: Some(next) for NEXT, None for COMPLETE.
    pub(super) fn air_step(&mut self, se: &mut ShellExec, from: u32, to: u32) -> Option<u32> {
        if from == to {
            return None;
        }
        let st = &mut se.air;
        if st.last_to != Some(to) {
            st.path = None;
            st.index = 0;
            st.last_to = Some(to);
        }
        if st.path.is_none() {
            let path = self.air_path(se.air_seed, from, to);
            let st = &mut se.air;
            st.index = if path[0] == from { 1 } else { 0 };
            st.path = Some(path);
        }
        let st = &mut se.air;
        let path = st.path.as_ref().unwrap();
        if st.index > 0 && path[st.index - 1] != from {
            st.path = None;
            st.index = 0;
            st.last_to = Some(to);
            return self.air_step(se, from, to);
        }
        if st.index >= path.len() {
            return None;
        }
        let next = path[st.index];
        st.index += 1;
        Some(next)
    }

    /// AirPathFinder.findPath
    pub(super) fn air_path(&self, seed: u32, from: u32, to: u32) -> Vec<u32> {
        let mut random = PseudoRandom::new(seed as f64);
        let mut path = vec![from];
        let mut cur = from;
        let (dx, dy) = (self.map.x(to) as i64, self.map.y(to) as i64);
        while cur != to {
            let (x, y) = (self.map.x(cur) as i64, self.map.y(cur) as i64);
            let next = if x == dx && y == dy {
                to
            } else {
                let (mut nx, mut ny) = (x, y);
                let ratio = (1.0 + ((dy - y).abs() as f64) / ((dx - x).abs() as f64 + 1.0)).floor();
                if x == dx {
                    ny += if y < dy { 1 } else { -1 };
                } else if y == dy {
                    nx += if x < dx { 1 } else { -1 };
                } else if random.next_int(0.0, ratio) == 0.0 {
                    nx += if x < dx { 1 } else { -1 };
                } else {
                    ny += if y < dy { 1 } else { -1 };
                }
                self.map.ref_at(nx as u32, ny as u32)
            };
            if next == cur {
                break;
            }
            cur = next;
            path.push(cur);
        }
        path
    }

    // ── TradeShipExecution ──

    pub(super) fn tick_trade_ship(&mut self, te: &mut TradeShipExec) {
        let ship = match te.ship {
            Some(s) => s,
            None => {
                let src_tile = self.units[te.src_port].tile;
                let o = te.orig_owner;
                let ok = self.players[o].is_alive()
                    && self.players[o]
                        .units
                        .iter()
                        .any(|&u| self.units[u].type_name == PORT && self.units[u].tile == src_tile);
                if !ok {
                    te.active = false;
                    return;
                }
                let s = self.build_unit(o, TRADE_SHIP, src_tile, 0.0, None);
                self.units[s].target_unit = Some(te.dst_port);
                self.units[s].last_safe_from_pirates = self.ticks;
                te.ship = Some(s);
                s
            }
        };
        if !self.units[ship].active {
            te.active = false;
            return;
        }
        let ship_owner = self.units[ship].owner;
        let dst_owner = self.units[te.dst_port].owner;
        if !te.was_captured && te.orig_owner != ship_owner {
            te.was_captured = true;
        }
        if dst_owner == self.units[te.src_port].owner {
            self.delete_unit(ship);
            te.active = false;
            return;
        }
        if !te.was_captured && (!self.units[te.dst_port].active || !self.can_trade(ship_owner, dst_owner)) {
            self.delete_unit(ship);
            te.active = false;
            return;
        }
        let cur = self.units[ship].tile;
        if te.was_captured && (ship_owner != dst_owner || !self.units[te.dst_port].active) {
            let comp = self.water_component(cur);
            let mut nearest = None;
            let mut best = u32::MAX;
            for &p in &self.players[ship_owner].units {
                let u = &self.units[p];
                if !(u.type_name == PORT && u.active && u.deletion_at.is_none() && !u.under_construction && comp.is_some_and(|c| self.has_water_component(u.tile, c))) {
                    continue;
                }
                let d = self.map.manhattan_dist(u.tile, cur);
                if d < best {
                    best = d;
                    nearest = Some(p);
                }
            }
            match nearest {
                None => {
                    self.delete_unit(ship);
                    te.active = false;
                    return;
                }
                Some(p) => {
                    te.dst_port = p;
                    self.units[ship].target_unit = Some(p);
                }
            }
        }
        let dst = self.units[te.dst_port].tile;
        if cur == dst {
            self.complete_trade(te, ship);
            return;
        }
        let mut stepper = std::mem::take(&mut te.stepper);
        let result = self.step(&mut stepper, cur, dst);
        te.stepper = stepper;
        match result {
            Step::Next(t) => {
                if self.map.is_water(t) && self.map.is_shoreline(t) {
                    self.units[ship].last_safe_from_pirates = self.ticks;
                }
                self.move_unit(ship, t);
                te.tiles_traveled += 1;
            }
            Step::Complete => self.complete_trade(te, ship),
            Step::NotFound => {
                if self.units[ship].active {
                    self.delete_unit(ship);
                }
                te.active = false;
            }
        }
    }

    pub(super) fn complete_trade(&mut self, te: &mut TradeShipExec, ship: usize) {
        te.active = false;
        self.delete_unit(ship);
        let dist = te.tiles_traveled as f64;
        let base = 75_000.0 / (1.0 + crate::detmath::exp(-0.03 * (dist - 300.0))) + 50.0 * dist;
        let gold = (base * self.config.gold_multiplier).floor() as i64;
        let ship_owner = self.units[ship].owner;
        if te.was_captured {
            self.add_gold(ship_owner, gold);
        } else {
            let (src, dst) = (self.units[te.src_port].owner, self.units[te.dst_port].owner);
            self.add_gold(src, gold);
            self.add_gold(dst, gold);
        }
    }

    // ── MoveWarshipExecution ──

    pub fn move_warships(&mut self, owner: usize, ids: &[u32], position: f64) {
        if !self.is_valid_ref(position) {
            return;
        }
        let position = position as u32;
        let comp = self.water_component(position);
        let mut seen = Vec::new();
        for &id in ids {
            if seen.contains(&id) {
                continue;
            }
            seen.push(id);
            let Some(w) = self.players[owner]
                .units
                .iter()
                .copied()
                .find(|&u| self.units[u].type_name == WARSHIP && self.units[u].id == id)
            else {
                continue;
            };
            if !self.units[w].active {
                continue;
            }
            if !comp.is_some_and(|c| self.has_water_component(self.units[w].tile, c)) {
                continue;
            }
            self.ws_mut(w).patrol_tile = position;
            self.units[w].target_tile = None;
        }
    }
}

