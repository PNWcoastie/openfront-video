use super::*;

// ── Nation structures (NationStructureBehavior, NationStructurePlacement,
// SharedWaterCache), MIRV saving (isMirvWorthSavingFor) and warship spawning
// (NationWarshipBehavior.maybeSpawnWarship). Medium difficulty, FFA. ──

const OCEAN_SENTINEL: i64 = -1;

#[derive(Default)]
pub struct NationStructureState {
    pub(super) last_structure_tick: Option<u32>,
    pub(super) placements_count: u32,
    pub(super) built_crowded_first: bool,
    pub(super) post_save_up_start: Option<u32>,
    pub(super) mirv_memo: Option<(u32, bool)>,
    /// Placement: shared water components as of startRound (None: no trade water).
    pub(super) shared: Option<Vec<i64>>,
    pub(super) reachable_stations: Option<Vec<(u32, Option<usize>, f64)>>,
}

/// SharedWaterCache: rebuilt at most every 30 ticks; callers read it stale.
#[derive(Default)]
pub struct SharedWaterCache {
    pub(super) tick: Option<u32>,
    pub(super) by_player: Vec<(usize, Option<Vec<i64>>)>,
}

impl Game {
    pub(super) fn shared_water_components(&mut self, p: usize) -> Option<Vec<i64>> {
        let tick = self.ticks;
        if self.shared_water.tick.is_none_or(|t| tick as i64 - t as i64 >= 30) {
            self.shared_water.by_player = self.build_shared_water();
            self.shared_water.tick = Some(tick);
        }
        self.shared_water.by_player.iter().find(|e| e.0 == p).and_then(|e| e.1.clone())
    }

    pub(super) fn build_shared_water(&self) -> Vec<(usize, Option<Vec<i64>>)> {
        let mut player_water: Vec<(usize, bool, Vec<u32>)> = Vec::new();
        let mut lake_partners: Vec<(u32, Vec<usize>)> = Vec::new();
        let mut nb = [0u32; 4];
        for p in self.alive_players() {
            if self.players[p].info.player_type == PlayerType::Bot {
                continue;
            }
            let mut has_ocean = false;
            let mut lakes: Vec<u32> = Vec::new();
            for t in self.players[p].border_tiles.values() {
                if !self.map.is_shore(t) {
                    continue;
                }
                let n = self.map.neighbors4(t, &mut nb);
                for &x in &nb[..n] {
                    if !self.map.is_water(x) {
                        continue;
                    }
                    if self.map.is_ocean(x) {
                        has_ocean = true;
                        continue;
                    }
                    if let Some(c) = self.water_component(x) {
                        if !lakes.contains(&c) {
                            lakes.push(c);
                        }
                    }
                }
            }
            for &c in &lakes {
                match lake_partners.iter_mut().find(|e| e.0 == c) {
                    Some(e) => e.1.push(p),
                    None => lake_partners.push((c, vec![p])),
                }
            }
            player_water.push((p, has_ocean, lakes));
        }
        let mut result = Vec::new();
        for (p, has_ocean, lakes) in player_water {
            let mut shared: Vec<i64> = Vec::new();
            if has_ocean {
                shared.push(OCEAN_SENTINEL);
            }
            for c in lakes {
                let Some(partners) = lake_partners.iter().find(|e| e.0 == c) else { continue };
                if partners.1.iter().any(|&o| o != p && self.can_trade(p, o)) {
                    shared.push(c as i64);
                }
            }
            result.push((p, if shared.is_empty() { None } else { Some(shared) }));
        }
        result
    }

    /// WaterManager.getWaterComponentSize: full-map tiles (minimap tiles x4).
    pub(super) fn water_component_size(&self, tile: u32) -> Option<u64> {
        let c = self.water_component(tile)?;
        Some(self.water_sizes.get(c as usize).copied().unwrap_or(0) as u64 * 4)
    }

    pub(super) fn player_units_of_types(&self, p: usize, types: &[&str]) -> Vec<usize> {
        self.players[p].units.iter().copied().filter(|&u| types.contains(&self.units[u].type_name)).collect()
    }

    pub(super) fn sam_range(level: u32) -> f64 {
        150.0 - 480.0 / (level as f64 + 5.0)
    }

    // ── NationUtils.randTerritoryTileArray ──

    pub(super) fn rand_territory_tiles(&self, random: &mut PseudoRandom, p: usize, n: usize) -> Vec<u32> {
        let mut b = [i64::MAX, i64::MAX, i64::MIN, i64::MIN];
        for t in self.players[p].border_tiles.values() {
            let (x, y) = (self.map.x(t) as i64, self.map.y(t) as i64);
            b = [b[0].min(x), b[1].min(y), b[2].max(x), b[3].max(y)];
        }
        let small = self.players[p].small_id;
        let (w, h) = (self.map.width as f64, self.map.height as f64);
        let mut out = Vec::new();
        for _ in 0..n {
            let mut found = None;
            for _ in 0..100 {
                let x = random.next_int(b[0] as f64, b[2] as f64);
                let y = random.next_int(b[1] as f64, b[3] as f64);
                if !(x >= 0.0 && x < w && y >= 0.0 && y < h) {
                    continue;
                }
                let t = self.map.ref_at(x as u32, y as u32);
                if self.map.owner_id(t) == small {
                    found = Some(t);
                    break;
                }
            }
            if found.is_none() {
                let tiles = self.players[p].tiles.len();
                if tiles > 0 && tiles <= 100 {
                    let all: Vec<u32> = self.players[p].tiles.values().collect();
                    found = Some(all[random.next_int(0.0, all.len() as f64) as usize]);
                }
            }
            if let Some(t) = found {
                out.push(t);
            }
        }
        out
    }

    // ── NationMIRVBehavior helpers ──

    pub(super) fn mirv_leak_share(&self, attacker: usize, target: usize) -> f64 {
        if self.type_units(SAM_LAUNCHER).is_empty() {
            return 1.0;
        }
        let warheads = js_min(350.0, crate::detmath::js_max(1.0, self.players[target].tiles.len() as f64 / 2000.0));
        let mut random = PseudoRandom::new(self.ticks as f64 * 1024.0 + self.players[target].small_id as f64);
        let samples = self.rand_territory_tiles(&mut random, target, 40);
        if samples.is_empty() {
            return 1.0;
        }
        let per = warheads / samples.len() as f64;
        let mut in_range: Vec<(usize, f64)> = Vec::new();
        let mut covered_samples = 0.0;
        for &tile in &samples {
            let mut covered = false;
            for (sam, d2) in self.nearby_units(tile, 150, &[SAM_LAUNCHER]) {
                let owner = self.units[sam].owner;
                if owner == attacker || self.is_friendly(owner, attacker, false) {
                    continue;
                }
                let r = Self::sam_range(self.units[sam].level);
                if d2 as f64 > r * r {
                    continue;
                }
                match in_range.iter_mut().find(|e| e.0 == sam) {
                    Some(e) => e.1 += per,
                    None => in_range.push((sam, per)),
                }
                covered = true;
            }
            if covered {
                covered_samples += 1.0;
            }
        }
        let mut shots = 0.0;
        for &(sam, n) in &in_range {
            shots += js_min(self.units[sam].level as f64, n);
        }
        let intercepted = js_min(covered_samples * per, shots);
        1.0 - intercepted / warheads
    }

    pub(super) fn mirv_gets_through(&self, attacker: usize, target: usize) -> bool {
        let min = match self.config.difficulty.as_str() {
            "Easy" => 0.0,
            "Medium" => 0.1,
            "Hard" => 0.2,
            _ => 0.25,
        };
        min == 0.0 || self.mirv_leak_share(attacker, target) >= min
    }

    pub(super) fn is_mirv_worth_saving_for(&self, p: usize) -> bool {
        let (mut land, mut city): (Option<usize>, Option<usize>) = (None, None);
        for q in self.alive_players() {
            if q == p || self.players[q].info.player_type == PlayerType::Bot || self.is_on_same_team(p, q) {
                continue;
            }
            if land.is_none_or(|l| self.players[q].tiles.len() > self.players[l].tiles.len()) {
                land = Some(q);
            }
            if city.is_none_or(|c| self.unit_levels(q, CITY) > self.unit_levels(c, CITY)) {
                city = Some(q);
            }
        }
        if land.is_some_and(|l| self.mirv_gets_through(p, l)) {
            return true;
        }
        city.is_some_and(|c| Some(c) != land && self.mirv_gets_through(p, c))
    }

    // ── NationStructureBehavior ──

    pub(super) fn structure_city_count(&self, p: usize) -> u32 {
        self.units_owned(p, CITY)
    }

    pub(super) fn save_up_target(&self, ne: &mut NationExec) -> i64 {
        let p = ne.player.unwrap();
        // Team games save up a limited amount, synced with the nuke behavior.
        if self.config.is_team_mode {
            return self.unit_cost(p, HYDROGEN_BOMB);
        }
        let tick = self.ticks;
        let worth = match ne.structures.mirv_memo {
            Some((t, w)) if t == tick => w,
            _ => {
                let w = self.is_mirv_worth_saving_for(p);
                ne.structures.mirv_memo = Some((tick, w));
                w
            }
        };
        if worth {
            return self.unit_cost(p, "MIRV") + self.unit_cost(p, HYDROGEN_BOMB);
        }
        self.unit_cost(p, HYDROGEN_BOMB) * 5
    }

    pub(super) fn levels_due(&self, p: usize, t: &str, city_count: u32, coastal: bool) -> i64 {
        let mut ratio = match t {
            PORT => if self.config.is_team_mode && self.config.difficulty != "Easy" { 1.0 } else { 0.75 },
            FACTORY => 0.75,
            SAM_LAUNCHER => self.by_difficulty([0.15, 0.2, 0.25, 0.3]),
            MISSILE_SILO => 0.2,
            _ => return 0,
        };
        if t == FACTORY && coastal {
            ratio *= self.by_difficulty([0.33, 0.45, 0.55, 0.6]);
        }
        let owned = self.units_owned(p, t) as i64;
        let target = if t == MISSILE_SILO {
            if owned == 0 {
                ratio = 0.4;
            }
            ((city_count as f64 * ratio).floor() as i64).min(3)
        } else {
            (city_count as f64 * ratio).floor() as i64
        };
        target - owned
    }

    pub(super) fn perceived_cost(&self, ne: &mut NationExec, t: &str) -> i64 {
        let p = ne.player.unwrap();
        let real = self.unit_cost(p, t);
        let save_up = self.save_up_target(ne);
        if save_up == 0 || self.players[p].gold >= save_up {
            return real;
        }
        // isSaveUpWithinReach: Hard only saves for what 5 minutes of income buys.
        if let Some(horizon) = self.by_difficulty([None, None, Some(5.0), Some(6.0)]) {
            let missing = (save_up - self.players[p].gold) as f64;
            let minutes = (self.ticks as f64 / 600.0).max(1.0);
            let per_minute = 2.0 * self.players[p].gold_earned as f64 / minutes;
            if missing > per_minute * horizon {
                return real;
            }
        }
        // CITIES_BEFORE_SAVING
        if self.structure_city_count(p) < self.by_difficulty([0, 3, 4, 5]) {
            return real;
        }
        let owned = self.units_owned(p, t) as f64;
        let mut inc = match t {
            CITY => 1.0,
            PORT | FACTORY | MISSILE_SILO => 1.0,
            SAM_LAUNCHER => 0.3,
            _ => 0.1,
        };
        if matches!(t, CITY | PORT | FACTORY) {
            inc *= self.by_difficulty([1.0, 0.8, 0.6, 0.4]);
        }
        (real as f64 * (1.0 + inc * owned)).ceil() as i64
    }

    pub(super) fn can_upgrade_unit(&self, p: usize, u: usize) -> bool {
        let unit = &self.units[u];
        matches!(unit.type_name, PORT | MISSILE_SILO | SAM_LAUNCHER | CITY | FACTORY)
            && self.unit_cost(p, unit.type_name) <= self.players[p].gold
            && self.players[p].is_alive()
            && !unit.under_construction
            && unit.deletion_at.is_none()
            && unit.owner == p
    }

    pub(super) fn nation_handle_structures(&mut self, ne: &mut NationExec) {
        self.assert_nation_difficulty();
        let p = ne.player.unwrap();
        if ne.structures.placements_count > 0 {
            if self.try_build_defense_post(ne) {
                return;
            }
            if self.defense_post_needed(p) {
                return;
            }
        }
        // isOnStructureCooldown: high (but below 10M) starting gold paces the first placements.
        if let Some(last) = ne.structures.last_structure_tick {
            let gold = self.config.starting_gold(&self.players[p].info);
            if self.high_starting_gold(p) && gold < 10_000_000 {
                let gap = [0, 0, 250, 150, 100].get(ne.structures.placements_count as usize).copied().unwrap_or(0);
                if gap > 0 && self.ticks - last < gap {
                    return;
                }
            }
        }
        // isInPostSaveUpBlockedPhase
        let save_up = self.save_up_target(ne);
        let gold = self.players[p].gold;
        let mut blocked = false;
        let mut check = true;
        if ne.structures.post_save_up_start.is_none() {
            if gold < save_up {
                check = false;
            } else {
                ne.structures.post_save_up_start = Some(self.ticks);
            }
        }
        if check && gold < save_up * 2 {
            let elapsed = self.ticks - ne.structures.post_save_up_start.unwrap();
            blocked = elapsed % 300 >= 150;
        }
        if blocked {
            return;
        }
        if self.do_handle_structures(ne) {
            ne.structures.last_structure_tick = Some(self.ticks);
            ne.structures.placements_count += 1;
        }
    }

    pub(super) fn do_handle_structures(&mut self, ne: &mut NationExec) -> bool {
        let p = ne.player.unwrap();
        // placement.startRound
        ne.structures.reachable_stations = None;
        ne.structures.shared = self.shared_water_components(p);
        let city_count = self.structure_city_count(p);
        let coastal = ne.structures.shared.is_some();
        if ne.structures.placements_count == 0
            && self.lands_beachheads()
            && self.high_starting_gold(p)
            && self.maybe_spawn_structure(ne, SAM_LAUNCHER)
        {
            return true;
        }
        // On crowded maps the first structure is a port (a factory if landlocked)
        // instead of a city; so does a nation starting in a team spawn area (not Easy).
        let in_area = self.config.difficulty != "Easy" && self.team_spawn_area(p).is_some();
        if !ne.structures.built_crowded_first && (self.is_high_nation_density() || in_area) {
            let first = if coastal { PORT } else { FACTORY };
            if self.maybe_spawn_structure(ne, first) {
                ne.structures.built_crowded_first = true;
                return true;
            }
        }
        if self.maybe_build_proactive_defense_post(ne) {
            return true;
        }
        for t in [PORT, FACTORY, SAM_LAUNCHER, MISSILE_SILO] {
            if t == PORT && !coastal {
                continue;
            }
            if self.levels_due(p, t, city_count, coastal) > 0 && self.maybe_spawn_structure(ne, t) {
                return true;
            }
        }
        self.maybe_spawn_structure(ne, CITY)
    }

    /// More than one nation per 7,500 land tiles (all nations of the game, alive or not).
    pub(super) fn is_high_nation_density(&self) -> bool {
        let land = self.map.num_land_tiles as f64;
        let nations = self.players.iter().filter(|q| q.info.player_type == PlayerType::Nation).count() as f64;
        land > 0.0 && nations / land > 1.0 / 7500.0
    }

    pub(super) fn maybe_spawn_structure(&mut self, ne: &mut NationExec, t: &'static str) -> bool {
        let p = ne.player.unwrap();
        if self.players[p].gold < self.perceived_cost(ne, t) {
            return false;
        }
        if t == SAM_LAUNCHER && self.should_upgrade_sam_instead(ne) {
            let sam = self.sam_protecting_most(p);
            return self.nation_upgrade_structure(ne, sam);
        }
        let structures = self.player_units_of(p, t);
        let upgradable = matches!(t, PORT | MISSILE_SILO | SAM_LAUNCHER | CITY | FACTORY);
        let density_types = [CITY, SAM_LAUNCHER, MISSILE_SILO, PORT, FACTORY];
        let tiles = self.players[p].tiles.len();
        let density = if tiles > 0 { self.player_units_of_types(p, &density_types).len() as f64 / tiles as f64 } else { 0.0 };
        if density > 1.0 / 1500.0 && upgradable {
            let best = self.find_best_structure_to_upgrade(ne, &structures);
            if self.nation_upgrade_structure(ne, best) {
                return true;
            }
            if !structures.is_empty() {
                return false;
            }
        }
        let Some(tile) = self.placement_spawn_tile(ne, t) else {
            if upgradable && self.config.difficulty != "Easy" {
                let best = self.find_best_structure_to_upgrade(ne, &structures);
                return self.nation_upgrade_structure(ne, best);
            }
            return false;
        };
        if self.can_build_structure(p, t, tile).is_none() {
            return false;
        }
        let e = self.new_construction(p, t, tile as f64, true, None);
        self.add_execution(e);
        true
    }

    pub(super) fn nation_upgrade_structure(&mut self, ne: &mut NationExec, s: Option<usize>) -> bool {
        let Some(u) = s else { return false };
        let p = ne.player.unwrap();
        let t = self.units[u].type_name;
        // upgradeAmount
        let due = if t == CITY {
            f64::INFINITY
        } else {
            let coastal = ne.structures.shared.is_some();
            self.levels_due(p, t, self.structure_city_count(p), coastal) as f64
        };
        let max = js_min(self.by_difficulty([3.0, 10.0, f64::INFINITY, f64::INFINITY]), due);
        let mut spare = self.players[p].gold - self.save_up_target(ne);
        let mut amount = 0.0;
        while amount < max {
            let cost = self.unit_cost_extra(p, t, amount);
            if cost > spare {
                break;
            }
            spare -= cost;
            amount += 1.0;
        }
        let amount = crate::detmath::js_max(1.0, amount) as u32;
        let id = self.units[u].id;
        self.add_execution(Exec::UpgradeStructure { player: p, unit_id: id, amount });
        true
    }

    pub(super) fn should_upgrade_sam_instead(&self, ne: &mut NationExec) -> bool {
        let p = ne.player.unwrap();
        let chance = self.by_difficulty([0.0, 50.0, 100.0, 100.0]);
        let sams = self.player_units_of(p, SAM_LAUNCHER);
        if chance == 0.0 || sams.is_empty() {
            return false;
        }
        for u in self.player_units_of_types(p, &[CITY, FACTORY, MISSILE_SILO, PORT]) {
            let covered = sams.iter().any(|&s| {
                let r = Self::sam_range(self.units[s].level);
                self.dist2(self.units[u].tile, self.units[s].tile) as f64 <= r * r
            });
            if !covered {
                return false;
            }
        }
        chance == 100.0 || ne.random.next_int(0.0, 100.0) < chance
    }

    pub(super) fn sam_protecting_most(&self, p: usize) -> Option<usize> {
        let protectable = self.player_units_of_types(p, &[CITY, FACTORY, MISSILE_SILO, PORT]);
        let mut best = None;
        let mut best_levels: i64 = -1;
        for sam in self.player_units_of(p, SAM_LAUNCHER) {
            if !self.can_upgrade_unit(p, sam) {
                continue;
            }
            let r = Self::sam_range(self.units[sam].level);
            let levels: i64 = protectable
                .iter()
                .filter(|&&u| self.dist2(self.units[u].tile, self.units[sam].tile) as f64 <= r * r)
                .map(|&u| self.units[u].level as i64)
                .sum();
            if levels > best_levels {
                best = Some(sam);
                best_levels = levels;
            }
        }
        best
    }

    pub(super) fn find_best_structure_to_upgrade(&self, ne: &mut NationExec, structures: &[usize]) -> Option<usize> {
        let p = ne.player.unwrap();
        if structures.is_empty() {
            return None;
        }
        let upgradable: Vec<usize> = structures.iter().copied().filter(|&s| self.can_upgrade_unit(p, s)).collect();
        if upgradable.is_empty() {
            return None;
        }
        if ne.random.next_int(0.0, 100.0) < self.by_difficulty([70.0, 40.0, 25.0, 10.0]) {
            return Some(upgradable[ne.random.next_int(0.0, upgradable.len() as f64) as usize]);
        }
        let sams = self.player_units_of(p, SAM_LAUNCHER);
        let mut scored: Vec<(usize, f64)> = Vec::new();
        for &s in &upgradable {
            let mut score = 0.0;
            for &sam in &sams {
                let r = Self::sam_range(self.units[sam].level);
                if self.dist2(self.units[s].tile, self.units[sam].tile) as f64 <= r * r {
                    score += 10.0;
                    if self.units[sam].level > 1 {
                        score += (self.units[sam].level - 1) as f64 * 7.5;
                    }
                }
            }
            score += ne.random.next_int(0.0, 5.0);
            scored.push((s, score));
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        if scored.len() >= 2 && ne.random.next_int(0.0, 2.0) == 0.0 {
            let i = if scored.len() >= 3 { ne.random.next_int(1.0, 3.0) as usize } else { 1 };
            return Some(scored[i].0);
        }
        Some(scored[0].0)
    }

    // ── Defense posts (Medium: one behind an attack front, 50% of the time) ──

    pub(super) fn defense_post_needed(&self, p: usize) -> bool {
        if self.config.difficulty == "Easy" {
            return false;
        }
        let attacks = self.incoming_attacks(p);
        let ours = self.players[p].troops_f();
        !attacks.is_empty() && ours > 0.0 && self.sum_troops(&attacks) / ours >= 0.35
    }

    pub(super) fn try_build_defense_post(&mut self, ne: &mut NationExec) -> bool {
        let medium = self.config.difficulty == "Medium";
        if self.config.difficulty == "Easy" || (medium && ne.random.next_int(0.0, 2.0) != 0.0) {
            return false;
        }
        let p = ne.player.unwrap();
        let attacks = self.incoming_attacks(p);
        if attacks.is_empty() {
            return false;
        }
        let ours = self.players[p].troops_f();
        if ours <= 0.0 || self.sum_troops(&attacks) / ours < 0.35 {
            return false;
        }
        let ratio = self.sum_troops(&attacks) / ours;
        // Hard: one post per 0.4 of incoming-to-own troops, set further back the heavier it is.
        let allowed = if medium { 1 } else { (ratio / 0.4).ceil() as usize };
        let scaled = if medium { 0.0 } else { ratio.min(2.0) };
        let range = 30.0;
        let min_dist = range * (2.0 / 3.0 + scaled / 3.0);
        let max_dist = min_dist + range * 0.5;
        // getAttackFrontTiles
        let mut land: Vec<usize> = Vec::new();
        for &a in &attacks {
            if self.attacks[a].source_tile.is_none() && !land.contains(&self.attacks[a].attacker) {
                land.push(self.attacks[a].attacker);
            }
        }
        let mut front = self.border_tiles_facing(p, &land);
        for &a in &attacks {
            if self.attacks[a].source_tile.is_some() && !land.contains(&self.attacks[a].attacker) {
                let small = self.players[p].small_id;
                for t in self.attacks[a].border.values() {
                    if self.map.owner_id(t) == small {
                        front.push(t);
                    }
                }
            }
        }
        front.shrink_to_fit();
        if self.count_posts_near_front(p, &front, max_dist, allowed) >= allowed {
            return false;
        }
        self.build_defense_post_behind(ne, &front, min_dist, max_dist)
    }

    /// countDefensePostsNearFront: our posts within `range` of any front tile, up to `cap`.
    pub(super) fn count_posts_near_front(&self, p: usize, front: &[u32], range: f64, cap: usize) -> usize {
        if front.is_empty() {
            return 0;
        }
        let r2 = range * range;
        let mut count = 0;
        for d in self.player_units_of(p, DEFENSE_POST) {
            if front.iter().any(|&f| self.dist2(self.units[d].tile, f) as f64 <= r2) {
                count += 1;
                if count >= cap {
                    return count;
                }
            }
        }
        count
    }

    pub(super) fn build_defense_post_behind(&mut self, ne: &mut NationExec, front: &[u32], min_dist: f64, max_dist: f64) -> bool {
        let p = ne.player.unwrap();
        if self.players[p].gold < self.unit_cost(p, DEFENSE_POST) {
            return false;
        }
        let Some(tile) = self.find_defense_post_tile(ne, front, min_dist, max_dist) else { return false };
        let e = self.new_construction(p, DEFENSE_POST, tile as f64, true, None);
        self.add_execution(e);
        true
    }

    /// maybeBuildProactiveDefensePost (Hard): a post facing hostile neighbors at
    /// least as strong as us, before they attack.
    pub(super) fn maybe_build_proactive_defense_post(&mut self, ne: &mut NationExec) -> bool {
        let Some(ratio) = self.by_difficulty([None, None, Some(1.0), Some(0.5)]) else { return false };
        let p = ne.player.unwrap();
        let cities = self.structure_city_count(p) as f64;
        if cities < 2.0 || self.player_units_of(p, DEFENSE_POST).len() as f64 >= cities * 0.5 {
            return false;
        }
        if self.players[p].gold < self.unit_cost(p, DEFENSE_POST) {
            return false;
        }
        let min_troops = self.players[p].troops_f() * ratio;
        let mut threats: Vec<usize> = Vec::new();
        for n in self.nearby(p).into_iter().flatten() {
            if self.players[n].info.player_type == PlayerType::Bot || self.is_friendly(p, n, false) {
                continue;
            }
            if self.relation(p, n) == 0 && self.players[n].troops_f() >= min_troops && !threats.contains(&n) {
                threats.push(n);
            }
        }
        if threats.is_empty() {
            return false;
        }
        let range = 30.0;
        let front = self.border_tiles_facing(p, &threats);
        if self.count_posts_near_front(p, &front, range * 1.5, threats.len()) >= threats.len() {
            return false;
        }
        self.build_defense_post_behind(ne, &front, range * 0.5, range)
    }

    pub(super) fn border_tiles_facing(&self, p: usize, players: &[usize]) -> Vec<u32> {
        let mut out = Vec::new();
        if players.is_empty() {
            return out;
        }
        let mut nb = [0u32; 4];
        for t in self.players[p].border_tiles.values() {
            let n = self.map.neighbors4(t, &mut nb);
            if nb[..n].iter().any(|&x| self.owner_index(x).is_some_and(|o| players.contains(&o))) {
                out.push(t);
            }
        }
        out
    }

    pub(super) fn find_defense_post_tile(&mut self, ne: &mut NationExec, front: &[u32], min_dist: f64, max_dist: f64) -> Option<u32> {
        if front.is_empty() {
            return None;
        }
        let p = ne.player.unwrap();
        let spread2 = (30.0f64 * 1.5) * (30.0 * 1.5);
        let posts: Vec<u32> = self.player_units_of(p, DEFENSE_POST).iter().map(|&u| self.units[u].tile).collect();
        let near_post = |g: &Game, t: u32| posts.iter().any(|&d| (g.dist2(t, d) as f64) < spread2);
        let anchors: Vec<u32> = front.iter().copied().filter(|&f| !near_post(self, f)).collect();
        if anchors.is_empty() {
            return None;
        }
        let (min2, max2) = (min_dist * min_dist, max_dist * max_dist);
        let r = max_dist.ceil();
        let (w, h) = (self.map.width as f64, self.map.height as f64);
        let small = self.players[p].small_id;
        for _ in 0..150 {
            let anchor = anchors[ne.random.next_int(0.0, anchors.len() as f64) as usize];
            let (ax, ay) = (self.map.x(anchor) as f64, self.map.y(anchor) as f64);
            let x = ne.random.next_int(ax - r, ax + r + 1.0);
            let y = ne.random.next_int(ay - r, ay + r + 1.0);
            if !(x >= 0.0 && x < w && y >= 0.0 && y < h) {
                continue;
            }
            let t = self.map.ref_at(x as u32, y as u32);
            if self.map.owner_id(t) != small || near_post(self, t) {
                continue;
            }
            let mut nearest = f64::INFINITY;
            for &f in front {
                nearest = js_min(nearest, self.dist2(t, f) as f64);
                if nearest < min2 {
                    break;
                }
            }
            if nearest < min2 || nearest > max2 {
                continue;
            }
            if self.can_build_structure(p, DEFENSE_POST, t).is_none() {
                continue;
            }
            return Some(t);
        }
        None
    }

    // ── NationStructurePlacement ──

    pub(super) fn placement_spawn_tile(&mut self, ne: &mut NationExec, t: &'static str) -> Option<u32> {
        let p = ne.player.unwrap();
        let tiles = if t == PORT {
            self.rand_coastal_tiles(ne, 25)
        } else {
            self.rand_territory_tiles(&mut ne.random, p, 25)
        };
        if tiles.is_empty() {
            return None;
        }
        let value = self.structure_value(ne, t);
        let mut best: Option<u32> = None;
        let mut best_value = 0.0;
        for tile in tiles {
            let v = value(self, tile);
            if v <= best_value && best.is_some() {
                continue;
            }
            if self.can_build_structure(p, t, tile).is_none() {
                continue;
            }
            best = Some(tile);
            best_value = v;
        }
        best
    }

    pub(super) fn rand_coastal_tiles(&mut self, ne: &mut NationExec, n: usize) -> Vec<u32> {
        let p = ne.player.unwrap();
        let Some(shared) = ne.structures.shared.clone() else { return Vec::new() };
        let mut nb = [0u32; 4];
        let mut tiles = Vec::new();
        for t in self.players[p].border_tiles.values() {
            if !self.map.is_shore(t) {
                continue;
            }
            let k = self.map.neighbors4(t, &mut nb);
            let ok = nb[..k].iter().any(|&x| {
                if !self.map.is_water(x) {
                    return false;
                }
                if self.map.is_ocean(x) {
                    return true;
                }
                let Some(c) = self.water_component(x) else { return false };
                if !shared.contains(&(c as i64)) {
                    return false;
                }
                if self.config.difficulty != "Easy" && self.water_component_size(x).is_some_and(|s| s < 3000) {
                    return false;
                }
                true
            });
            if ok {
                tiles.push(t);
            }
        }
        if tiles.len() <= n {
            return tiles;
        }
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let i = ne.random.next_int(0.0, tiles.len() as f64) as usize;
            out.push(tiles.remove(i));
        }
        out
    }

    pub(super) fn nearest_tile_dist(&self, tiles: &[u32], tile: u32) -> f64 {
        tiles.iter().map(|&t| self.map.manhattan_dist(t, tile) as f64).fold(f64::INFINITY, js_min)
    }

    /// nearestTileDistCapped over the border tile set (ring search).
    pub(super) fn nearest_border_dist_capped(&self, p: usize, tile: u32, cap: i64) -> f64 {
        let set = &self.players[p].border_tiles;
        if set.len() == 0 {
            return f64::INFINITY;
        }
        if set.contains_indexed(tile, &self.border_slot) {
            return 0.0;
        }
        let (w, h) = (self.map.width as i64, self.map.height as i64);
        let (cx, cy) = (self.map.x(tile) as i64, self.map.y(tile) as i64);
        for d in 1..=cap {
            for dx in -d..=d {
                let x = cx + dx;
                if x < 0 || x >= w {
                    continue;
                }
                let dy = d - dx.abs();
                let y1 = cy - dy;
                if y1 >= 0 && set.contains_indexed((y1 * w + x) as u32, &self.border_slot) {
                    return d as f64;
                }
                if dy != 0 {
                    let y2 = cy + dy;
                    if y2 < h && set.contains_indexed((y2 * w + x) as u32, &self.border_slot) {
                        return d as f64;
                    }
                }
            }
        }
        f64::INFINITY
    }

    pub(super) fn use_connectivity_score(&self, ne: &mut NationExec) -> bool {
        let chance = match self.config.difficulty.as_str() {
            "Easy" => 0.0,
            "Medium" => 60.0,
            "Hard" => 75.0,
            _ => 100.0,
        };
        ne.random.next_int(0.0, 100.0) < chance
    }

    pub(super) fn reachable_stations(&mut self, ne: &mut NationExec) -> Vec<(u32, Option<usize>, f64)> {
        if let Some(r) = &ne.structures.reachable_stations {
            return r.clone();
        }
        let p = ne.player.unwrap();
        let gold = |g: &Game, base: f64| (crate::detmath::js_max(5000.0, base) * g.config.gold_multiplier).floor();
        let max_trade = crate::detmath::js_max(gold(self, 35_000.0), 1.0);
        let station_of = |g: &Game, u: usize| -> Option<Option<usize>> {
            g.rail.manager.values().map(|s| s as usize).find(|&s| g.rail.stations[s].unit == u).map(|s| g.rail.stations[s].cluster)
        };
        let mut result = Vec::new();
        let self_weight = gold(self, 10_000.0) / max_trade;
        for u in self.player_units_of_types(p, &[CITY, PORT, FACTORY]) {
            if let Some(c) = station_of(self, u) {
                result.push((self.units[u].tile, c, self_weight));
            }
        }
        for n in self.nearby(p).into_iter().flatten() {
            if self.players[n].info.player_type == PlayerType::Bot || !self.can_trade(p, n) {
                continue;
            }
            let base = if self.is_on_same_team(p, n) {
                25_000.0
            } else if self.is_allied_with(p, n) {
                35_000.0
            } else {
                25_000.0
            };
            let weight = gold(self, base) / max_trade;
            for u in self.player_units_of_types(n, &[CITY, PORT, FACTORY]) {
                if let Some(c) = station_of(self, u) {
                    result.push((self.units[u].tile, c, weight));
                }
            }
        }
        ne.structures.reachable_stations = Some(result.clone());
        result
    }

    pub(super) fn connectivity_score(&self, tile: u32, stations: &[(u32, Option<usize>, f64)], min2: i64, max2: i64) -> f64 {
        let mut clusters: Vec<(usize, f64)> = Vec::new();
        let mut isolated = 0.0;
        for &(t, cluster, weight) in stations {
            let d = self.dist2(tile, t);
            if d < min2 || d > max2 {
                continue;
            }
            match cluster {
                Some(c) => match clusters.iter_mut().find(|e| e.0 == c) {
                    Some(e) => e.1 = crate::detmath::js_max(e.1, weight),
                    None => clusters.push((c, crate::detmath::js_max(0.0, weight))),
                },
                None => isolated += weight,
            }
        }
        let mut score = isolated;
        for (_, w) in clusters {
            score += w;
        }
        score
    }

    /// structureSpawnTileValue: a scoring function for candidate tiles.
    pub(super) fn structure_value(&mut self, ne: &mut NationExec, t: &str) -> Box<dyn Fn(&Game, u32) -> f64> {
        let p = ne.player.unwrap();
        let (border_spacing, structure_spacing) = (30i64, 60.0);
        let tiles_of = |g: &Game, ty: &str| -> Vec<u32> { g.player_units_of(p, ty).iter().map(|&u| g.units[u].tile).collect() };
        match t {
            MISSILE_SILO => {
                let others = tiles_of(self, MISSILE_SILO);
                Box::new(move |g: &Game, tile: u32| {
                    let mut w = g.map.magnitude(tile) as f64;
                    w += js_min(g.nearest_border_dist_capped(p, tile, border_spacing), border_spacing as f64);
                    let o: Vec<u32> = others.iter().copied().filter(|&x| x != tile).collect();
                    let d = g.nearest_tile_dist(&o, tile);
                    if d != f64::INFINITY {
                        w += js_min(d, structure_spacing);
                    }
                    w
                })
            }
            PORT => {
                let others = tiles_of(self, PORT);
                Box::new(move |g: &Game, tile: u32| {
                    let o: Vec<u32> = others.iter().copied().filter(|&x| x != tile).collect();
                    g.nearest_tile_dist(&o, tile)
                })
            }
            CITY | FACTORY => {
                let use_conn = self.use_connectivity_score(ne);
                let stations = if use_conn { self.reachable_stations(ne) } else { Vec::new() };
                let others = tiles_of(self, t);
                let cross = tiles_of(self, if t == CITY { FACTORY } else { CITY });
                let factory = t == FACTORY;
                Box::new(move |g: &Game, tile: u32| {
                    let mut w = g.map.magnitude(tile) as f64;
                    w += js_min(g.nearest_border_dist_capped(p, tile, border_spacing), border_spacing as f64);
                    let o: Vec<u32> = others.iter().copied().filter(|&x| x != tile).collect();
                    let d = g.nearest_tile_dist(&o, tile);
                    if d != f64::INFINITY {
                        w += js_min(d, if factory { 110.0 } else { structure_spacing });
                    }
                    let d2 = g.nearest_tile_dist(&cross, tile);
                    if d2 != f64::INFINITY {
                        w += js_min(d2, structure_spacing);
                    }
                    if !use_conn {
                        return w;
                    }
                    w + g.connectivity_score(tile, &stations, 15 * 15, 110 * 110) * structure_spacing
                })
            }
            SAM_LAUNCHER => {
                let others = tiles_of(self, SAM_LAUNCHER);
                let weight_by_level = self.lands_beachheads();
                let mut protect: Vec<(u32, f64)> = Vec::new();
                for t in [CITY, FACTORY, MISSILE_SILO, PORT] {
                    for u in self.player_units_of(p, t) {
                        let weight = if weight_by_level { self.units[u].level as f64 } else { 1.0 };
                        protect.push((self.units[u].tile, weight));
                    }
                }
                let use_coverage = self.config.difficulty != "Easy" && ne.random.next_int(0.0, 100.0) < 25.0;
                let coverage: Vec<f64> = if use_coverage {
                    let sams = self.player_units_of(p, SAM_LAUNCHER);
                    protect
                        .iter()
                        .map(|&(pt, _)| {
                            sams.iter()
                                .filter(|&&s| {
                                    let r = Self::sam_range(self.units[s].level);
                                    self.dist2(pt, self.units[s].tile) as f64 <= r * r
                                })
                                .map(|&s| self.units[s].level as f64)
                                .sum()
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                let has_border = self.players[p].border_tiles.len() > 0;
                Box::new(move |g: &Game, tile: u32| {
                    let mut w = g.map.magnitude(tile) as f64;
                    if has_border {
                        w += js_min(g.nearest_border_dist_capped(p, tile, border_spacing), border_spacing as f64);
                    }
                    let o: Vec<u32> = others.iter().copied().filter(|&x| x != tile).collect();
                    let d = g.nearest_tile_dist(&o, tile);
                    if d != f64::INFINITY {
                        w += js_min(d, structure_spacing);
                    }
                    // Prefer to be in range of other structures (not on Easy).
                    let near_structures: &[(u32, f64)] = if g.config.difficulty == "Easy" { &[] } else { &protect };
                    for (i, &(pt, weight)) in near_structures.iter().enumerate() {
                        if g.dist2(tile, pt) > 70 * 70 {
                            continue;
                        }
                        if use_coverage {
                            let coverage_weight = 1.0 / (1.0 + coverage[i]);
                            w += structure_spacing * weight * coverage_weight;
                        } else {
                            w += structure_spacing * weight;
                        }
                    }
                    w
                })
            }
            other => panic!("Value function not implemented for {other}"),
        }
    }

    // ── NationWarshipBehavior.maybeSpawnWarship ──

    pub(super) fn nation_maybe_spawn_warship(&mut self, ne: &mut NationExec) {
        if ne.random.next_int(0.0, 50.0) != 0.0 {
            return;
        }
        let p = ne.player.unwrap();
        let ports = self.player_units_of(p, PORT);
        let ships = self.player_units_of(p, WARSHIP);
        if ports.is_empty() || !ships.is_empty() || self.players[p].gold <= self.unit_cost(p, WARSHIP) {
            return;
        }
        let port = ports[ne.random.next_int(0.0, ports.len() as f64) as usize];
        let pt = self.units[port].tile;
        let (px, py) = (self.map.x(pt) as f64, self.map.y(pt) as f64);
        let (w, h) = (self.map.width as f64, self.map.height as f64);
        let mut target = None;
        for _ in 0..50 {
            let x = ne.random.next_int(px - 250.0, px + 250.0);
            let y = ne.random.next_int(py - 250.0, py + 250.0);
            if !(x >= 0.0 && x < w && y >= 0.0 && y < h) {
                continue;
            }
            let t = self.map.ref_at(x as u32, y as u32);
            if !self.map.is_water(t) {
                continue;
            }
            target = Some(t);
            break;
        }
        let Some(t) = target else { return };
        // canBuild(Warship): cost, alive, a port on the same water.
        if self.unit_cost(p, WARSHIP) > self.players[p].gold || !self.players[p].is_alive() || self.warship_spawn(p, t).is_none() {
            return;
        }
        let e = self.new_construction(p, WARSHIP, t as f64, true, None);
        self.add_execution(e);
    }
}

