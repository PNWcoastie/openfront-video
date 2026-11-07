use super::*;

// ── Buildings (ConstructionExecution, CityExecution, DeleteUnitExecution) ──

pub const CITY: &str = "City";
pub const PORT: &str = "Port";
pub const FACTORY: &str = "Factory";
pub const DEFENSE_POST: &str = "Defense Post";
pub const SAM_LAUNCHER: &str = "SAM Launcher";
pub const MISSILE_SILO: &str = "Missile Silo";
pub const STRUCTURES: [&str; 6] = [CITY, PORT, FACTORY, DEFENSE_POST, SAM_LAUNCHER, MISSILE_SILO];

pub fn is_structure(t: &str) -> bool {
    STRUCTURES.contains(&t)
}

/// The engine's UnitType string, interned for Unit::type_name.
pub fn unit_type(name: &str) -> &'static str {
    STRUCTURES
        .iter()
        .chain([TRANSPORT_SHIP, "Warship", "Atom Bomb", "Hydrogen Bomb", "MIRV"].iter())
        .find(|&&t| t == name)
        .copied()
        .unwrap_or_else(|| panic!("unit type {name} is not ported yet"))
}

pub struct ConstructionExec {
    pub(super) active: bool,
    pub(super) player: usize,
    pub(super) unit_type: &'static str,
    pub(super) tile: f64,
    pub(super) structure: Option<usize>,
    pub(super) ticks_until_complete: i64,
    pub(super) rocket_up: bool,
    pub(super) amount: u32,
}

impl Game {
    pub fn new_construction(&self, player: usize, unit_type: &'static str, tile: f64, rocket_up: bool, amount: Option<f64>) -> Exec {
        Exec::Construction(Box::new(ConstructionExec {
            active: true,
            player,
            unit_type,
            tile,
            structure: None,
            ticks_until_complete: 0,
            rocket_up,
            amount: amount.unwrap_or(1.0) as u32,
        }))
    }

    /// unitsOwned: under construction counts 1, built counts its level.
    pub(super) fn units_owned(&self, p: usize, t: &str) -> u32 {
        self.players[p]
            .units
            .iter()
            .map(|&u| &self.units[u])
            .filter(|u| u.type_name == t)
            .map(|u| if u.under_construction { 1 } else { u.level })
            .sum()
    }

    pub(super) fn units_constructed(&self, p: usize, t: &str) -> u32 {
        self.players[p].units_constructed.get(t).copied().unwrap_or(0)
    }

    /// EngineConfig unit costs (costWrapper: min(owned, constructed) per type).
    pub(super) fn unit_cost(&self, p: usize, t: &str) -> i64 {
        self.unit_cost_extra(p, t, 0.0)
    }

    /// The cost with `extra` more units counted (costWrapper's extraUnits).
    pub(super) fn unit_cost_extra(&self, p: usize, t: &str, extra: f64) -> i64 {
        let n = |types: &[&str]| -> f64 {
            types.iter().map(|ty| self.units_owned(p, ty).min(self.units_constructed(p, ty))).sum::<u32>() as f64 + extra
        };
        let v = match t {
            CITY => js_min(1_000_000.0, crate::detmath::pow2(n(&[CITY])) * 125_000.0),
            PORT => js_min(1_000_000.0, crate::detmath::pow2(n(&[PORT, FACTORY])) * 125_000.0),
            FACTORY => js_min(1_000_000.0, crate::detmath::pow2(n(&[FACTORY, PORT])) * 125_000.0),
            DEFENSE_POST => js_min(250_000.0, (n(&[DEFENSE_POST]) + 1.0) * 50_000.0),
            SAM_LAUNCHER => js_min(3_000_000.0, (n(&[SAM_LAUNCHER]) + 1.0) * 1_500_000.0),
            MISSILE_SILO => 1_000_000.0,
            "Warship" => js_min(1_000_000.0, (n(&["Warship"]) + 1.0) * 250_000.0),
            TRANSPORT_SHIP | TRADE_SHIP | TRAIN | SHELL | SAM_MISSILE | MIRV_WARHEAD => 0.0,
            ATOM_BOMB => 750_000.0,
            "MIRV" => 25_000_000.0 + self.mirvs_launched as f64 * 15_000_000.0,
            HYDROGEN_BOMB => 5_000_000.0,
            other => panic!("cost of {other} is not ported yet"),
        };
        v as i64
    }

    pub(super) fn construction_duration(t: &str) -> i64 {
        match t {
            PORT | DEFENSE_POST => 50,
            MISSILE_SILO => 100,
            CITY | FACTORY => 20,
            SAM_LAUNCHER => 300,
            _ => 0,
        }
    }

    /// PlayerImpl.validStructureSpawnTiles: own tiles within radius 15,
    /// at least 15 from other structures, nearest first.
    pub(super) fn valid_structure_spawn_tiles(&mut self, p: usize, tile: u32) -> Vec<u32> {
        if self.owner_index(tile) != Some(p) {
            return Vec::new();
        }
        let (cx, cy) = (self.map.x(tile) as i64, self.map.y(tile) as i64);
        let small = self.players[p].small_id;
        let inside = |g: &Game, t: u32| {
            let (dx, dy) = (g.map.x(t) as i64 - cx, g.map.y(t) as i64 - cy);
            dx * dx + dy * dy < 225 && g.map.owner_id(t) == small
        };
        // nearbyUnits(tile, 30, structures, includeUnderConstruction)
        let blockers: Vec<u32> = STRUCTURES
            .iter()
            .flat_map(|t| self.type_units(t).iter().map(|&i| &self.units[i]))
            .filter(|u| u.active && is_structure(u.type_name) && self.dist2(u.tile, tile) <= 900 && self.in_grid_cells(tile, 30, u.tile))
            .map(|u| u.tile)
            .collect();
        let gen = self.bump_generation();
        let mut nearby = Vec::new();
        let mut stack = Vec::new();
        if inside(self, tile) {
            self.visited[tile as usize] = gen;
            nearby.push(tile);
            stack.push(tile);
        }
        let mut nb = [0u32; 4];
        while let Some(t) = stack.pop() {
            let n = self.map.neighbors4(t, &mut nb);
            for &x in &nb[..n] {
                if self.visited[x as usize] != gen && inside(self, x) {
                    self.visited[x as usize] = gen;
                    nearby.push(x);
                    stack.push(x);
                }
            }
        }
        let mut valid: Vec<u32> = nearby.into_iter().filter(|&t| !blockers.iter().any(|&b| self.dist2(b, t) < 225)).collect();
        valid.sort_by_key(|&t| self.dist2(t, tile)); // stable, like Array.sort
        valid
    }

    #[inline]
    pub(super) fn dist2(&self, a: u32, b: u32) -> i64 {
        let (dx, dy) = (self.map.x(a) as i64 - self.map.x(b) as i64, self.map.y(a) as i64 - self.map.y(b) as i64);
        dx * dx + dy * dy
    }

    /// PlayerImpl.canBuild for structures.
    pub(super) fn can_build_structure(&mut self, p: usize, t: &'static str, tile: u32) -> Option<u32> {
        if self.unit_cost(p, t) > self.players[p].gold || !self.players[p].is_alive() {
            return None;
        }
        match t {
            CITY | FACTORY | DEFENSE_POST | SAM_LAUNCHER | MISSILE_SILO => self.valid_structure_spawn_tiles(p, tile).first().copied(),
            PORT => self.port_spawn(p, tile),
            other => panic!("building {other} is not ported yet"),
        }
    }

    /// GameImpl.hasUnitNearby
    /// UnitGrid.getCellsInRange: the cells a range query visits. Its bounds
    /// skip the next cell when a unit sits exactly at the range on an axis,
    /// so a query can miss a unit that is in range; mirror that.
    pub(super) fn in_grid_cells(&self, center: u32, range: i64, unit_tile: u32) -> bool {
        let (sx, ex, sy, ey) = self.grid_cell_bounds(center, range);
        let (ux, uy) = (self.map.x(unit_tile) as i64 / 100, self.map.y(unit_tile) as i64 / 100);
        ux >= sx && ux <= ex && uy >= sy && uy <= ey
    }

    /// The cell columns and rows (inclusive) UnitGrid.getCellsInRange visits.
    pub(super) fn grid_cell_bounds(&self, center: u32, range: i64) -> (i64, i64, i64, i64) {
        let (x, y) = (self.map.x(center) as i64, self.map.y(center) as i64);
        let cols = (self.map.width as i64 + 99) / 100;
        let rows = (self.map.height as i64 + 99) / 100;
        let ceil = |v: i64| (v as f64 / 100.0).ceil() as i64;
        let (gx, gy) = (x / 100, y / 100);
        let sx = (gx - ceil(range - x % 100)).max(0);
        let ex = (gx + ceil(range - (100 - x % 100))).min(cols - 1);
        let sy = (gy - ceil(range - y % 100)).max(0);
        let ey = (gy + ceil(range - (100 - y % 100))).min(rows - 1);
        (sx, ex, sy, ey)
    }

    pub(super) fn cell_of(&self, tile: u32) -> usize {
        let cols = (self.map.width + 99) / 100;
        ((self.map.y(tile) / 100) * cols + self.map.x(tile) / 100) as usize
    }

    pub(super) fn type_units(&self, t: &str) -> &[usize] {
        self.units_by_type.get(t).map_or(&[], |v| v.as_slice())
    }

    pub(super) fn has_unit_nearby(&self, tile: u32, range: i64, t: &str, owner: Option<usize>) -> bool {
        let units = self.type_units(t);
        if units.is_empty() {
            return false;
        }
        let r2 = range * range;
        if let Some(o) = owner {
            let p_units = &self.players[o].units;
            if p_units.is_empty() {
                return false;
            }
            let mut bounds = None;
            if units.len() <= p_units.len() {
                return units.iter().map(|&i| &self.units[i]).any(|u| {
                    u.owner == o
                        && u.active
                        && !u.under_construction
                        && self.dist2(u.tile, tile) <= r2
                        && {
                            let (sx, ex, sy, ey) = *bounds.get_or_insert_with(|| self.grid_cell_bounds(tile, range));
                            let (ux, uy) = (self.map.x(u.tile) as i64 / 100, self.map.y(u.tile) as i64 / 100);
                            ux >= sx && ux <= ex && uy >= sy && uy <= ey
                        }
                });
            }
            return p_units.iter().map(|&i| &self.units[i]).any(|u| {
                u.type_name == t
                    && u.active
                    && !u.under_construction
                    && self.dist2(u.tile, tile) <= r2
                    && {
                        let (sx, ex, sy, ey) = *bounds.get_or_insert_with(|| self.grid_cell_bounds(tile, range));
                        let (ux, uy) = (self.map.x(u.tile) as i64 / 100, self.map.y(u.tile) as i64 / 100);
                        ux >= sx && ux <= ex && uy >= sy && uy <= ey
                    }
            });
        }
        let mut bounds = None;
        units.iter().map(|&i| &self.units[i]).any(|u| {
            u.active
                && !u.under_construction
                && self.dist2(u.tile, tile) <= r2
                && {
                    let (sx, ex, sy, ey) = *bounds.get_or_insert_with(|| self.grid_cell_bounds(tile, range));
                    let (ux, uy) = (self.map.x(u.tile) as i64 / 100, self.map.y(u.tile) as i64 / 100);
                    ux >= sx && ux <= ex && uy >= sy && uy <= ey
                }
        })
    }

    pub(super) fn init_construction(&mut self, ce: &mut ConstructionExec) {
        if !self.is_valid_ref(ce.tile) {
            ce.active = false;
        }
    }

    pub(super) fn tick_construction(&mut self, ce: &mut ConstructionExec) {
        let Some(s) = ce.structure else {
            if matches!(ce.unit_type, ATOM_BOMB | HYDROGEN_BOMB) {
                for _ in 0..ce.amount {
                    let e = self.new_nuke(ce.unit_type, ce.player, ce.tile as u32, ce.rocket_up);
                    self.add_execution(e);
                }
                ce.active = false;
                return;
            }
            if ce.unit_type == "MIRV" {
                let e = self.new_mirv(ce.player, ce.tile as u32);
                self.add_execution(e);
                ce.active = false;
                return;
            }
            if ce.unit_type == WARSHIP {
                let e = self.new_warship(ce.player, ce.tile as u32);
                self.add_execution(e);
                ce.active = false;
                return;
            }
            if !is_structure(ce.unit_type) {
                panic!("building {} is not ported yet", ce.unit_type);
            }
            let Some(spawn) = self.can_build_structure(ce.player, ce.unit_type, ce.tile as u32) else {
                ce.active = false;
                return;
            };
            let s = self.build_unit(ce.player, ce.unit_type, spawn, 0.0, None);
            ce.structure = Some(s);
            let duration = Self::construction_duration(ce.unit_type);
            if duration > 0 {
                self.units[s].under_construction = true;
                ce.ticks_until_complete = duration;
                return;
            }
            self.complete_construction(ce);
            ce.active = false;
            return;
        };
        if !self.units[s].active {
            ce.active = false;
            return;
        }
        ce.player = self.units[s].owner;
        if ce.ticks_until_complete == 0 {
            self.complete_construction(ce);
            ce.active = false;
            return;
        }
        ce.ticks_until_complete -= 1;
    }

    pub(super) fn complete_construction(&mut self, ce: &mut ConstructionExec) {
        let s = ce.structure.expect("structure");
        self.units[s].under_construction = false;
        match ce.unit_type {
            CITY => self.add_execution(Exec::City { unit: s, station_created: false, active: true }),
            SAM_LAUNCHER => self.add_execution(Exec::Sam(Box::new(SamExec { active: true, unit: s, last_level: 1, cache: Vec::new() }))),
            MISSILE_SILO => self.add_execution(Exec::Silo { unit: s, active: true }),
            FACTORY => self.add_execution(Exec::Factory { unit: s, station_created: false, active: true }),
            DEFENSE_POST => self.add_execution(Exec::DefensePost { unit: s, active: true }),
            PORT => self.add_execution(Exec::Port(Box::new(PortExec { active: true, port: s, random: None, check_offset: 0, rejections: 0 }))),
            other => panic!("{other} execution is not ported yet"),
        }
    }

    pub(super) fn tick_city(&mut self, unit: usize, station_created: &mut bool, active: &mut bool) {
        if !*station_created {
            self.create_station_if_factory_nearby(unit);
            *station_created = true;
        }
        if !self.units[unit].active {
            *active = false;
        }
    }

    pub(super) fn set_unit_owner(&mut self, u: usize, new_owner: usize) {
        self.units[u].deletion_at = None;
        let old = self.units[u].owner;
        self.players[old].units.retain(|&x| x != u);
        self.units[u].owner = new_owner;
        self.players[new_owner].units.push(u);
    }

    /// PlayerExecution: structures on tiles the player no longer owns are
    /// captured (defense posts are destroyed), or deleted on unowned land.
    pub(super) fn check_structures(&mut self, p: usize) {
        if !self.players[p]
            .units
            .iter()
            .any(|&u| is_structure(self.units[u].type_name) && self.owner_index(self.units[u].tile) != Some(p))
        {
            return;
        }
        for u in self.players[p].units.clone() {
            if !is_structure(self.units[u].type_name) {
                continue;
            }
            match self.owner_index(self.units[u].tile) {
                None => self.delete_unit(u),
                Some(o) if o == p => {}
                Some(captor) => {
                    if self.units[u].type_name == DEFENSE_POST {
                        self.units[u].destroyer = Some(captor);
                        self.delete_unit(u);
                    } else {
                        self.set_unit_owner(u, captor);
                    }
                }
            }
        }
    }

    pub(super) fn city_troops(&self, p: usize) -> f64 {
        let levels: u32 = self.players[p]
            .units
            .iter()
            .map(|&u| &self.units[u])
            .filter(|u| u.type_name == CITY && !u.under_construction)
            .map(|u| u.level)
            .sum();
        levels as f64 * 250_000.0
    }

    /// TribeExecution.deleteNextStructure
    pub(super) fn tribe_delete_next_structure(&mut self, p: usize) {
        if (self.ticks as i64 - self.players[p].last_delete_unit_tick) < 300 {
            return;
        }
        let next = self.players[p]
            .units
            .iter()
            .copied()
            .find(|&u| is_structure(self.units[u].type_name) && self.units[u].deletion_at.is_none());
        if let Some(u) = next {
            let id = self.units[u].id;
            self.add_execution(Exec::DeleteUnit { active: true, player: p, unit_id: id, unit: None });
        }
    }

    pub(super) fn init_delete_unit(&mut self, player: usize, unit_id: u32, unit: &mut Option<usize>, active: &mut bool) {
        let idx = (unit_id as usize).wrapping_sub(1);
        let ok = idx < self.units.len() && self.units[idx].active && self.units[idx].owner == player;
        if !ok {
            *active = false;
            return;
        }
        *unit = Some(idx);
        let tile = self.units[idx].tile;
        if self.owner_index(tile) != Some(player)
            || !self.map.is_land(tile)
            || self.in_spawn_phase()
            || (self.ticks as i64 - self.players[player].last_delete_unit_tick) < 300
        {
            *active = false;
            return;
        }
        self.players[player].last_delete_unit_tick = self.ticks as i64;
        self.units[idx].deletion_at = Some(self.ticks + 300);
    }

    pub(super) fn tick_delete_unit(&mut self, unit: Option<usize>, active: &mut bool) {
        let Some(u) = unit else { return };
        if !self.units[u].active {
            *active = false;
            return;
        }
        if self.units[u].deletion_at.is_some_and(|d| self.ticks as i64 - d as i64 > 0) {
            self.delete_unit(u);
            *active = false;
        }
    }
}


// ── UpgradeStructureExecution ──

impl Game {
    pub(super) fn upgrade_structure(&mut self, player: usize, unit_id: u32, amount: u32) {
        let idx = unit_id as usize;
        if idx == 0 || idx > self.units.len() || !self.units[idx - 1].active {
            return;
        }
        let u = idx - 1;
        if self.units[u].owner != player {
            return;
        }
        for _ in 0..amount {
            // canUpgradeUnit
            let t = self.units[u].type_name;
            let upgradable = matches!(t, PORT | MISSILE_SILO | SAM_LAUNCHER | CITY | FACTORY);
            let unit = &self.units[u];
            if !upgradable
                || self.unit_cost(player, t) > self.players[player].gold
                || !self.players[player].is_alive()
                || unit.under_construction
                || unit.deletion_at.is_some()
                || unit.owner != player
            {
                break;
            }
            // upgradeUnit
            let cost = self.unit_cost(player, t);
            self.remove_gold(player, cost);
            if t == SAM_LAUNCHER {
                let current = self.dynamic_sam_range(u, self.ticks as i64);
                self.units[u].sam_state = Some(SamState { start_tick: self.ticks, start_range: current, target_level: self.units[u].level + 1, duration: 45 });
            }
            self.units[u].level += 1;
            if matches!(t, MISSILE_SILO | SAM_LAUNCHER) {
                // SAM range growth only matters for targeting (not ported).
                self.units[u].missile_timers.push(self.ticks);
            }
            *self.players[player].units_constructed.entry(t).or_default() += 1;
        }
    }
}

// ── SAM launchers and missile silos (SAMLauncherExecution, MissileSiloExecution) ──

impl Game {
    pub(super) fn tick_silo(&mut self, silo: usize, active: &mut bool) {
        if self.units[silo].under_construction {
            return;
        }
        if !self.units[silo].active {
            *active = false;
            return;
        }
        let Some(&front) = self.units[silo].missile_timers.first() else { return };
        if 90 - (self.ticks as i64 - front as i64) <= 0 {
            self.units[silo].missile_timers.remove(0);
        }
    }
}

