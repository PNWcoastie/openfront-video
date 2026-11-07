use super::*;
use super::nation_attack::{emoji_ids, relation_from_value};
use super::units::closest_two_tiles;

// ── NationNukeBehavior and NationNukePlanning helpers ──

#[derive(Default)]
pub struct NationNukeState {
    pub(super) recent: Vec<(u32, u32, &'static str)>,
    pub(super) atom_perceived: i64,
    pub(super) hydro_perceived: i64,
    pub(super) atom_last_launch: Option<u32>,
    pub(super) hydro_last_launch: Option<u32>,
    /// Hard & Impossible: nukes in flight we follow up with an attack when they land.
    strikes: Vec<Strike>,
}

struct Strike {
    target: usize,
    tile: u32,
    tick: u32,
    nuke: Option<usize>,
}

impl Game {
    pub(super) fn nation_consider_mirv(&mut self, ne: &mut NationExec) {
        let p = ne.player.unwrap();
        if self.player_units_of(p, MISSILE_SILO).is_empty() || self.players[p].gold < self.unit_cost(p, "MIRV") {
            return;
        }
        self.assert_nation_difficulty();
        let easy = self.config.difficulty == "Easy";
        // hesitationOdds
        if ne.random.next_int(0.0, if easy { 2.0 } else { 4.0 }) == 0.0 {
            return;
        }
        let targets: Vec<usize> = self
            .alive_players()
            .into_iter()
            .filter(|&q| q != p && self.players[q].info.player_type != PlayerType::Bot && !self.is_on_same_team(p, q))
            .collect();
        let tiles = |q: usize| self.players[q].tiles.len();
        // selectCounterMirvTarget
        let mut attackers: Vec<usize> = targets
            .iter()
            .copied()
            .filter(|&q| {
                self.player_units_of(q, "MIRV").iter().any(|&m| self.units[m].target_tile.is_some_and(|d| self.owner_index(d) == Some(p)))
            })
            .collect();
        attackers.sort_by_key(|&q| std::cmp::Reverse(tiles(q)));
        let mut candidates: Vec<Option<usize>> = vec![attackers.first().copied()];
        // selectVictoryDenialTarget (FFA: no teams): Easy 75%, Medium 65%
        let scaled = self.map.num_land_tiles as usize * if easy { 75 } else { 65 };
        let mut best: Option<usize> = None;
        for &q in &targets {
            let t = tiles(q);
            if t > 0 && t * 100 >= scaled && best.is_none_or(|b| t > tiles(b)) {
                best = Some(q);
            }
        }
        candidates.push(best);
        // selectSteamrollStopTarget: Easy gap x2 and above 20 cities, Medium x1.5 and above 10
        let (gap, min_cities) = if easy { (2.0, 20) } else { (1.5, 10) };
        let mut steam = None;
        if !targets.is_empty() {
            let mut all: Vec<(usize, u32)> = self.alive_players().into_iter().map(|q| (q, self.unit_levels(q, CITY))).collect();
            all.sort_by_key(|&(_, c)| std::cmp::Reverse(c));
            if all.len() >= 2 && all[0].1 > min_cities && all[0].1 as f64 >= all[1].1 as f64 * gap && targets.contains(&all[0].0) {
                steam = Some(all[0].0);
            }
        }
        candidates.push(steam);
        // selectRivalTarget is Hard/Impossible only.
        for target in candidates.into_iter().flatten() {
            let recent = self.nation_mirv_targets.get(&target).is_some_and(|&t| (self.ticks as i64 - t as i64) < 300);
            if !recent && self.mirv_gets_through(p, target) {
                self.nation_send_mirv(ne, target);
                return;
            }
        }
    }

    /// NationMIRVBehavior.maybeSendMIRV
    pub(super) fn nation_send_mirv(&mut self, ne: &mut NationExec, enemy: usize) {
        let p = ne.player.unwrap();
        self.emoji_maybe_send_attack(ne, enemy);
        let Some(center) = self.territory_center(enemy) else { return };
        let can_build = self.unit_cost(p, "MIRV") <= self.players[p].gold
            && self.players[p].is_alive()
            && self.owner_index(center).is_some()
            && self.nuke_spawn(p, center, "MIRV").is_some();
        if !can_build {
            return;
        }
        let e = self.new_mirv(p, center);
        self.add_execution(e);
        self.nation_mirv_targets.insert(enemy, self.ticks);
        self.emoji_send(ne, None, &["☢️", "💥"]);
        // respondToMIRV
        if ne.random.next_int(0.0, 8.0) == 0.0 && self.can_send_emoji(enemy, None) {
            let ids = emoji_ids(&["💀", "🆘", "😱", "🥺", "😭", "😞", "🫡", "👋"]);
            let emoji = ids[ne.random.next_int(0.0, ids.len() as f64) as usize];
            self.add_execution(Exec::Emoji(Box::new(EmojiExec::new(enemy, "AllPlayers".to_string(), emoji))));
        }
    }

    /// Util.calculateTerritoryCenter
    pub(super) fn territory_center(&self, target: usize) -> Option<u32> {
        let border = &self.players[target].border_tiles;
        if border.len() == 0 {
            return None;
        }
        let (mut min_x, mut max_x, mut min_y, mut max_y) = (i64::MAX, i64::MIN, i64::MAX, i64::MIN);
        for t in border.values() {
            let (x, y) = (self.map.x(t) as i64, self.map.y(t) as i64);
            min_x = min_x.min(x);
            max_x = max_x.max(x);
            min_y = min_y.min(y);
            max_y = max_y.max(y);
        }
        let (cx, cy) = ((min_x + max_x).div_euclid(2), (min_y + max_y).div_euclid(2));
        let center = self.map.ref_at(cx as u32, cy as u32);
        if self.owner_index(center) == Some(target) {
            return Some(center);
        }
        let mut closest = None;
        let mut best = i64::MAX;
        for t in border.values() {
            let (dx, dy) = (self.map.x(t) as i64 - cx, self.map.y(t) as i64 - cy);
            if dx * dx + dy * dy < best {
                best = dx * dx + dy * dy;
                closest = Some(t);
            }
        }
        closest
    }

    pub(super) fn mirv_save_up_target_for(&self, p: usize) -> i64 {
        self.unit_cost(p, "MIRV") + self.unit_cost(p, HYDROGEN_BOMB)
    }

    /// getPerceivedNukeCost: each launch makes the next nuke of its type feel pricier.
    pub(super) fn perceived_nuke_cost(&self, ne: &NationExec, t: &str) -> i64 {
        let p = ne.player.unwrap();
        let real = self.unit_cost(p, t);
        if self.pays_real_nuke_price(p) {
            return real;
        }
        // Easy & Medium keep the price up for good, so only while a MIRV is worth saving for.
        if !self.forgets_nuke_prices() && !self.is_mirv_worth_saving_for(p) {
            return real;
        }
        self.inflated_nuke_cost(ne, t)
    }

    pub(super) fn pays_real_nuke_price(&self, p: usize) -> bool {
        if self.alive_players().len() == 2 {
            return true;
        }
        // Team games save up a limited amount, synced with the structure behavior.
        if self.config.is_team_mode && self.players[p].gold > self.unit_cost(p, HYDROGEN_BOMB) {
            return true;
        }
        if self.players[p].gold > self.mirv_save_up_target_for(p) {
            return true;
        }
        // Hard & Impossible go all-in on nukes under heavy attack.
        self.lands_beachheads() && self.incoming_troops(p) >= self.players[p].troops_f()
    }

    /// Hard & Impossible in FFA: the perceived price falls back to the real one within minutes.
    pub(super) fn forgets_nuke_prices(&self) -> bool {
        self.lands_beachheads() && !self.config.is_team_mode
    }

    pub(super) fn inflated_nuke_cost(&self, ne: &NationExec, t: &str) -> i64 {
        let real = self.unit_cost(ne.player.unwrap(), t);
        let (perceived, last) =
            if t == ATOM_BOMB { (ne.nukes.atom_perceived, ne.nukes.atom_last_launch) } else { (ne.nukes.hydro_perceived, ne.nukes.hydro_last_launch) };
        if perceived <= real {
            return real;
        }
        if !self.forgets_nuke_prices() {
            return perceived;
        }
        let left = match last {
            None => 0,
            Some(l) => 3000 - (self.ticks as i64 - l as i64),
        };
        if left <= 0 {
            return real;
        }
        real + (perceived - real) * left / 3000
    }

    /// isSaveUpWithinReach: Hard only saves for what 5 minutes of income buys.
    pub(super) fn is_save_up_within_reach(&self, p: usize, target: i64) -> bool {
        let missing = (target - self.players[p].gold) as f64;
        let Some(horizon) = self.by_difficulty([None, None, Some(5.0), Some(6.0)]) else { return true };
        if missing <= 0.0 {
            return true;
        }
        let minutes = (self.ticks as f64 / 600.0).max(1.0);
        missing <= 2.0 * self.players[p].gold_earned as f64 / minutes * horizon
    }

    pub(super) fn is_saving_for_mirv(&self, p: usize) -> bool {
        self.forgets_nuke_prices()
            && !self.pays_real_nuke_price(p)
            && self.is_mirv_worth_saving_for(p)
            && self.is_save_up_within_reach(p, self.mirv_save_up_target_for(p))
    }

    /// isHittingUs: attacking us, or a bomb flying at our land.
    pub(super) fn is_hitting_us(&self, p: usize, other: usize) -> bool {
        if self.incoming_attacks(p).iter().any(|&a| self.attacks[a].attacker == other) {
            return true;
        }
        self.player_units_of_types(other, &[ATOM_BOMB, HYDROGEN_BOMB, "MIRV", MIRV_WARHEAD])
            .iter()
            .any(|&u| self.units[u].target_tile.is_some_and(|t| self.owner_index(t) == Some(p)))
    }

    /// strikeTroops: above the reserve, and within troopSendCap without target.
    pub(super) fn strike_troops(&self, ne: &NationExec, target: usize) -> f64 {
        let p = ne.player.unwrap();
        (self.players[p].troops_f() - self.max_troops(p) * ne.reserve_ratio).min(self.troop_send_cap(p, Some(target)))
    }

    /// canStrike: the attack right after our nuke could send enough troops in by land.
    pub(super) fn can_strike(&self, ne: &NationExec, target: usize) -> bool {
        let p = ne.player.unwrap();
        self.players[target].info.player_type != PlayerType::Bot
            && !self.is_friendly(p, target, false)
            && self.strike_troops(ne, target) >= self.players[target].troops_f() * 0.5
            && self.shares_border_with(p, self.players[target].small_id)
    }

    /// followUpStrikes: attack a strike's target the moment its bomb, or its MIRV's last warhead, lands.
    pub(super) fn nation_follow_up_strikes(&mut self, ne: &mut NationExec) {
        let p = ne.player.unwrap();
        if self.lands_beachheads() && self.unit_count(p, "MIRV") > 0 {
            // Our MIRVs at bordering players are strikes too.
            for mirv in self.player_units_of(p, "MIRV") {
                if ne.nukes.strikes.iter().any(|s| s.nuke == Some(mirv)) {
                    continue;
                }
                let (Some(target), Some(tile)) = (self.units[mirv].mirv_target, self.units[mirv].target_tile) else { continue };
                if self.is_friendly(p, target, false) || !self.shares_border_with(p, self.players[target].small_id) {
                    continue;
                }
                ne.nukes.strikes.push(Strike { target, tile, tick: self.ticks, nuke: Some(mirv) });
            }
        }
        if ne.nukes.strikes.is_empty() {
            return;
        }
        let mut pending: Vec<Strike> = Vec::new();
        let mut i = 0;
        while i < ne.nukes.strikes.len() {
            if ne.nukes.strikes[i].nuke.is_none() {
                let tile = ne.nukes.strikes[i].tile;
                let found = self
                    .player_units_of_types(p, &[ATOM_BOMB, HYDROGEN_BOMB])
                    .into_iter()
                    .find(|&n| self.units[n].target_tile == Some(tile) && !ne.nukes.strikes.iter().any(|s| s.nuke == Some(n)));
                ne.nukes.strikes[i].nuke = found;
            }
            let st = &ne.nukes.strikes[i];
            let (target, tile, tick, nuke) = (st.target, st.tile, st.tick, st.nuke);
            i += 1;
            let keep = Strike { target, tile, tick, nuke };
            let Some(n) = nuke else {
                if self.ticks - tick <= 10 {
                    pending.push(keep);
                }
                continue;
            };
            if self.units[n].active {
                pending.push(keep);
                continue;
            }
            if self.units[n].type_name == "MIRV" {
                // A MIRV deletes itself when it splits into warheads.
                if self.unit_count(p, MIRV_WARHEAD) > 0 {
                    pending.push(keep);
                    continue;
                }
            } else if !self.units[n].reached_target {
                // Intercepted bombs leave the target's troops alone.
                continue;
            }
            if self.players[target].is_alive() && !self.is_friendly(p, target, false) && self.shares_border_with(p, self.players[target].small_id) {
                self.nation_send_land_attack(ne, Some(target), None, Some(target));
            }
        }
        ne.nukes.strikes = pending;
    }

    /// isTrajectoryInterceptableBySam: whether an enemy SAM covers the bomb's flight
    /// outside its untargetable mid-air stretch.
    pub(super) fn is_trajectory_interceptable_by_sam(&self, p: usize, spawn: u32, target: u32) -> bool {
        if self.type_units(SAM_LAUNCHER).is_empty() {
            return false;
        }
        let pts = self.parabola_points(spawn, target, true);
        let trajectory: Vec<u32> = Curve::new(pts, 10.0).points.iter().map(|&q| self.curve_tile(q)).collect();
        if trajectory.is_empty() {
            return false;
        }
        let r2 = 150i64 * 150;
        let (mut start, mut end): (i64, i64) = (-1, -1);
        for (i, &t) in trajectory.iter().enumerate() {
            if start == -1 {
                if self.dist2(t, spawn) > r2 {
                    if self.dist2(t, target) < r2 {
                        break;
                    }
                    start = i as i64;
                }
            } else if self.dist2(t, target) < r2 {
                end = i as i64;
                break;
            }
        }
        let mut i: i64 = 0;
        while (i as usize) < trajectory.len() {
            if start != -1 && end != -1 && i == start {
                i = end;
                continue;
            }
            let t = trajectory[i as usize];
            if self.any_nearby_unit(t, 150, &[SAM_LAUNCHER], |sam, d2| {
                let owner = self.units[sam].owner;
                if owner == p || self.is_friendly(p, owner, false) {
                    return false;
                }
                let range = Self::sam_range(self.units[sam].level);
                d2 as f64 <= range * range
            }) {
                return true;
            }
            i += 1;
        }
        false
    }

    pub(super) fn structure_levels(&self, p: usize) -> u32 {
        self.players[p].units.iter().map(|&u| &self.units[u]).filter(|u| is_structure(u.type_name)).map(|u| u.level).sum()
    }

    /// findTeammateFightTarget: the enemy attacking our teammates hardest, else the one they attack hardest.
    pub(super) fn find_teammate_fight_target(&self, p: usize) -> Option<usize> {
        if !self.config.is_team_mode || self.config.difficulty == "Easy" {
            return None;
        }
        let mates: Vec<usize> = self.alive_players().into_iter().filter(|&q| q != p && self.is_on_same_team(p, q)).collect();
        let is_enemy = |q: usize| self.players[q].info.player_type != PlayerType::Bot && !self.is_friendly(p, q, false);
        let mut best = None;
        let mut best_troops = 0.0;
        for &m in &mates {
            for a in self.incoming_attacks(m) {
                let attacker = self.attacks[a].attacker;
                if is_enemy(attacker) && self.attacks[a].troops > best_troops {
                    best = Some(attacker);
                    best_troops = self.attacks[a].troops;
                }
            }
        }
        if best.is_some() {
            return best;
        }
        for &m in &mates {
            for &a in &self.players[m].outgoing {
                if let Some(t) = self.attacks[a].target {
                    if is_enemy(t) && self.attacks[a].troops > best_troops {
                        best = Some(t);
                        best_troops = self.attacks[a].troops;
                    }
                }
            }
        }
        best
    }

    /// findStrongestTeamTarget: a player of the team with the most land (not ours).
    pub(super) fn find_strongest_team_target(&self, ne: &mut NationExec) -> Option<usize> {
        if !self.config.is_team_mode {
            return None;
        }
        let p = ne.player.unwrap();
        let players = self.alive_players();
        if players.len() <= 1 {
            return None;
        }
        let mut teams: Vec<(String, usize, Vec<usize>)> = Vec::new();
        for &q in &players {
            let Some(t) = self.players[q].team.clone() else { continue };
            let tiles = self.players[q].tiles.len();
            match teams.iter_mut().find(|e| e.0 == t) {
                Some(e) => {
                    e.1 += tiles;
                    e.2.push(q);
                }
                None => teams.push((t, tiles, vec![q])),
            }
        }
        teams.sort_by(|a, b| b.1.cmp(&a.1));
        if teams.is_empty() {
            return None;
        }
        let mut strongest = 0;
        if self.players[p].team.as_ref() == Some(&teams[0].0) {
            if teams.len() > 1 {
                strongest = 1;
            } else {
                return None;
            }
        }
        let valid: Vec<usize> = teams[strongest].2.iter().copied().filter(|&q| !self.is_friendly(p, q, false)).collect();
        if valid.is_empty() {
            return None;
        }
        // Hard & Impossible: one of the three players with the most structure levels.
        if self.lands_beachheads() {
            let mut most: Vec<(usize, u32)> = valid.iter().map(|&q| (q, self.structure_levels(q))).filter(|e| e.1 > 0).collect();
            most.sort_by(|a, b| b.1.cmp(&a.1));
            most.truncate(3);
            if !most.is_empty() {
                return Some(most[ne.random.next_int(0.0, most.len() as f64) as usize].0);
            }
        }
        if ne.random.next_int(0.0, 2.0) == 0.0 {
            let mut best = valid[0];
            for &q in &valid[1..] {
                best = if self.max_troops(best) > self.max_troops(q) { best } else { q };
            }
            Some(best)
        } else {
            Some(valid[ne.random.next_int(0.0, valid.len() as f64) as usize])
        }
    }

    pub(super) fn nation_maybe_send_nuke(&mut self, ne: &mut NationExec) {
        self.assert_nation_difficulty();
        let p = ne.player.unwrap();
        let silos = self.player_units_of(p, MISSILE_SILO);
        if silos.is_empty() {
            return;
        }
        let Some(target) = self.find_best_nuke_target(ne) else { return };
        // chooseNukeTarget: Easy never strikes; Medium when it's attacking the target;
        // Hard & Impossible when the follow-up attack would be strong enough.
        let strike = match self.config.difficulty.as_str() {
            "Easy" => false,
            "Medium" => self.players[p].outgoing.iter().any(|&a| self.attacks[a].target == Some(target) && self.attacks[a].troops > 0.0),
            _ => self.can_strike(ne, target),
        };
        if self.players[target].info.player_type == PlayerType::Bot || self.is_on_same_team(p, target) || !self.nation_should_attack(ne, Some(target)) {
            return;
        }
        // While saving up for a MIRV, Hard & Impossible only nuke back at players hitting them, with atom bombs.
        let saving = self.is_saving_for_mirv(p);
        if saving && !self.is_hitting_us(p, target) {
            return;
        }
        // nukeTypesToTry: a hydrogen bomb first; Hard & Impossible also an atom bomb.
        let mut types: Vec<&'static str> = Vec::new();
        if !saving && self.players[p].gold >= self.perceived_nuke_cost(ne, HYDROGEN_BOMB) {
            types.push(HYDROGEN_BOMB);
        }
        if (types.is_empty() || self.lands_beachheads()) && self.players[p].gold >= self.perceived_nuke_cost(ne, ATOM_BOMB) {
            types.push(ATOM_BOMB);
        }
        if types.is_empty() {
            return;
        }
        let structures = self.player_units_of_types(target, &[CITY, DEFENSE_POST, SAM_LAUNCHER, MISSILE_SILO, PORT, FACTORY]);
        let random_tiles = self.rand_territory_tiles(&mut ne.random, target, 10);
        let mut candidates: Vec<u32> = Vec::new();
        for t in random_tiles.into_iter().chain(structures.iter().map(|&u| self.units[u].tile)) {
            if !candidates.contains(&t) {
                candidates.push(t);
            }
        }
        // removeOldNukeEvents
        let tick = self.ticks;
        while ne.nukes.recent.first().is_some_and(|r| r.0 + 600 < tick) {
            ne.nukes.recent.remove(0);
        }
        for t in types {
            if let Some(tile) = self.find_best_nuke_tile(ne, t, target, &candidates, &silos, &structures, strike) {
                self.send_nation_nuke(ne, tile, t, target);
                if strike && self.lands_beachheads() {
                    ne.nukes.strikes.push(Strike { target, tile, tick: self.ticks, nuke: None });
                }
                return;
            }
        }
    }

    pub(super) fn find_best_nuke_target(&self, ne: &mut NationExec) -> Option<usize> {
        let p = ne.player.unwrap();
        // Hard & Impossible with only 2 players left target the other one.
        if self.lands_beachheads() {
            let alive = self.alive_players();
            if alive.len() == 2 {
                if let Some(&other) = alive.iter().find(|&&q| q != p) {
                    return Some(other);
                }
            }
        }
        if let Some(a) = self.nation_find_incoming_attack_player(p) {
            return Some(a);
        }
        // findIncomingNukePlayer
        if !self.type_units(ATOM_BOMB).is_empty() || !self.type_units(HYDROGEN_BOMB).is_empty() {
            for q in 0..self.players.len() {
                for &u in &self.players[q].units {
                    let unit = &self.units[u];
                    if !matches!(unit.type_name, ATOM_BOMB | HYDROGEN_BOMB) || self.is_friendly(p, unit.owner, false) {
                        continue;
                    }
                    if unit.target_tile.is_some_and(|t| self.owner_index(t) == Some(p)) {
                        return Some(unit.owner);
                    }
                }
            }
        }
        // In teams, back up teammates in their fights before starting our own.
        if let Some(t) = self.find_teammate_fight_target(p) {
            return Some(t);
        }
        // findStrikeTarget: Medium a player we're attacking about as strong as us,
        // Hard the strongest such neighbor it can follow up on.
        let mine = self.players[p].troops_f();
        let candidate =
            |t: usize| self.players[t].info.player_type != PlayerType::Bot && !self.is_friendly(p, t, false) && self.players[t].troops_f() >= mine * 0.5;
        match self.config.difficulty.as_str() {
            "Easy" => {}
            "Medium" => {
                for &a in &self.players[p].outgoing {
                    if let Some(t) = self.attacks[a].target {
                        if candidate(t) {
                            return Some(t);
                        }
                    }
                }
            }
            _ => {
                let mut best: Option<usize> = None;
                for o in self.nearby(p).into_iter().flatten() {
                    if !candidate(o) || best.is_some_and(|b| self.players[o].troops_f() <= self.players[b].troops_f()) {
                        continue;
                    }
                    if self.can_strike(ne, o) {
                        best = Some(o);
                    }
                }
                if best.is_some() {
                    return best;
                }
            }
        }
        // Allies' targets
        for ally in self.allies(p) {
            let targets = self.targets(ally);
            if targets.is_empty() || self.relation(p, ally) < 3 {
                continue;
            }
            for t in targets {
                if t != p && !self.is_friendly(p, t, false) {
                    return Some(t);
                }
            }
        }
        // The most hated player not much weaker than us
        let my_max = self.max_troops(p);
        let mut rels: Vec<(usize, f64)> = self.players[p].relations.iter().copied().filter(|r| self.players[r.0].is_alive()).collect();
        rels.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        for (other, v) in rels {
            if relation_from_value(v) != 0 || self.is_friendly(p, other, false) {
                continue;
            }
            if my_max >= self.max_troops(other) * 2.0 {
                continue;
            }
            return Some(other);
        }
        if let Some(c) = self.find_ffa_crown_target(p) {
            return Some(c);
        }
        self.find_strongest_team_target(ne)
    }

    pub(super) fn find_ffa_crown_target(&self, p: usize) -> Option<usize> {
        if self.config.is_team_mode {
            return None;
        }
        let mut sorted = self.alive_players();
        if sorted.len() <= 1 {
            return None;
        }
        sorted.sort_by(|&a, &b| self.players[b].tiles.len().cmp(&self.players[a].tiles.len()));
        let first = sorted[0];
        if first == p || self.is_friendly(p, first, false) {
            return None;
        }
        let no_fallout = self.map.num_land_tiles as f64 - self.map.num_tiles_with_fallout() as f64;
        if no_fallout <= 0.0 {
            return None;
        }
        let share = self.players[first].tiles.len() as f64 / no_fallout;
        if share > 0.5 {
            return Some(first);
        }
        if self.find_runaway_leader() != Some(first) {
            return None;
        }
        let mut threshold = match self.config.difficulty.as_str() {
            "Easy" => return None,
            "Medium" => 0.3,
            "Hard" => 0.2,
            _ => 0.1,
        };
        if !self.shares_border_with(p, self.players[first].small_id) {
            threshold *= 2.0;
        }
        let my_share = self.players[p].tiles.len() as f64 / no_fallout;
        if share - my_share > threshold {
            Some(first)
        } else {
            None
        }
    }

    pub(super) fn bounding_box_tiles(&self, center: u32, radius: i64) -> Vec<u32> {
        let (cx, cy) = (self.map.x(center) as i64, self.map.y(center) as i64);
        let (w, h) = (self.map.width as i64, self.map.height as i64);
        let valid = |x: i64, y: i64| x >= 0 && x < w && y >= 0 && y < h;
        let (min_x, max_x, min_y, max_y) = (cx - radius, cx + radius, cy - radius, cy + radius);
        let mut out = Vec::new();
        for x in min_x..=max_x {
            if valid(x, min_y) {
                out.push((min_y * w + x) as u32);
            }
            if valid(x, max_y) && min_y != max_y {
                out.push((max_y * w + x) as u32);
            }
        }
        for y in (min_y + 1)..max_y {
            if valid(min_x, y) {
                out.push((y * w + min_x) as u32);
            }
            if valid(max_x, y) && min_x != max_x {
                out.push((y * w + max_x) as u32);
            }
        }
        out
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn find_best_nuke_tile(
        &mut self,
        ne: &NationExec,
        t: &'static str,
        target: usize,
        candidates: &[u32],
        silos: &[usize],
        structures: &[usize],
        strike: bool,
    ) -> Option<u32> {
        let p = ne.player.unwrap();
        let hard = self.lands_beachheads();
        let (inner, outer) = Self::nuke_magnitude(t);
        let target_small = self.players[target].small_id;
        let mut scored: Vec<(u32, f64)> = Vec::new();
        'outer: for &tile in candidates {
            let mut bbox = self.bounding_box_tiles(tile, outer);
            bbox.extend(self.bounding_box_tiles(tile, outer.div_euclid(2)));
            for b in bbox {
                // isValidNukeTile: Easy & Medium only the target's own land; Hard & Impossible
                // also free land and anyone who isn't a teammate or ally.
                if self.map.owner_id(b) == target_small {
                    continue;
                }
                let hard_ok = hard && self.owner_index(b).is_none_or(|o| !self.is_friendly(p, o, true));
                if !hard_ok {
                    continue 'outer;
                }
            }
            let Some(spawn) = self.nuke_spawn(p, tile, t) else { continue };
            // Above Easy, skip spots already being nuked or where the blast would be mostly wasted.
            let mut coverage = 0.0;
            if self.config.difficulty != "Easy" {
            // isNukeAlreadyLandingNear
            let landing = [ATOM_BOMB, HYDROGEN_BOMB, MIRV_WARHEAD].iter().any(|&ty| {
                self.type_units(ty).iter().any(|&u| {
                    let unit = &self.units[u];
                    unit.active
                        && unit.target_tile.is_some_and(|tt| {
                            let sum = inner + Self::nuke_magnitude(unit.type_name).0;
                            self.dist2(tile, tt) <= sum * sum
                        })
                })
            });
            if landing {
                continue;
            }
            coverage = self.enemy_land_blast_coverage(p, tile, t);
            if coverage < if t == ATOM_BOMB { 0.25 } else { 0.4 } {
                continue;
            }
            }
            // Hard & Impossible avoid trajectories enemy SAMs can intercept.
            if hard && self.is_trajectory_interceptable_by_sam(p, spawn, tile) {
                continue;
            }
            let value = self.nuke_tile_score(ne, tile, silos, structures, t);
            if value < 0.0 {
                continue;
            }
            scored.push((tile, value + coverage * 50_000.0));
        }
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        // Keep a strike's blast off our front, so it survives for the follow-up attack.
        let margin = if strike { 10 } else { 0 };
        scored.iter().find(|&&(tile, _)| !self.blast_hits_friendly_land(p, tile, t, margin)).map(|s| s.0)
    }

    pub(super) fn enemy_land_blast_coverage(&self, p: usize, center: u32, t: &str) -> f64 {
        let (inner, outer) = Self::nuke_magnitude(t);
        let step = (outer / 10).max(1);
        let (cx, cy) = (self.map.x(center) as i64, self.map.y(center) as i64);
        let (w, h) = (self.map.width as i64, self.map.height as i64);
        let (mut total, mut enemy) = (0.0, 0.0);
        let mut dy = -outer;
        while dy <= outer {
            let mut dx = -outer;
            while dx <= outer {
                let d2 = dx * dx + dy * dy;
                if d2 <= outer * outer {
                    let weight = if d2 <= inner * inner { 2.0 } else { 1.0 };
                    total += weight;
                    let (x, y) = (cx + dx, cy + dy);
                    if x >= 0 && x < w && y >= 0 && y < h {
                        if let Some(o) = self.owner_index((y * w + x) as u32) {
                            if !self.is_friendly(p, o, true) {
                                enemy += weight;
                            }
                        }
                    }
                }
                dx += step;
            }
            dy += step;
        }
        enemy / total
    }

    pub(super) fn blast_hits_friendly_land(&self, p: usize, center: u32, t: &str, margin: i64) -> bool {
        let friendly: Vec<u16> = self.alive_players().into_iter().filter(|&q| self.is_friendly(p, q, true)).map(|q| self.players[q].small_id).collect();
        let outer = Self::nuke_magnitude(t).1 + margin;
        let (cx, cy) = (self.map.x(center) as i64, self.map.y(center) as i64);
        let (w, h) = (self.map.width as i64, self.map.height as i64);
        for dy in -outer..=outer {
            for dx in -outer..=outer {
                if dx * dx + dy * dy > outer * outer {
                    continue;
                }
                let (x, y) = (cx + dx, cy + dy);
                if x < 0 || x >= w || y < 0 || y >= h {
                    continue;
                }
                if friendly.contains(&self.map.owner_id((y * w + x) as u32)) {
                    return true;
                }
            }
        }
        false
    }

    pub(super) fn nuke_tile_score(&self, ne: &NationExec, tile: u32, silos: &[usize], targets: &[usize], t: &str) -> f64 {
        let (_, outer) = Self::nuke_magnitude(t);
        let mut value: f64 = targets
            .iter()
            .filter(|&&u| self.dist2(tile, self.units[u].tile) <= outer * outer)
            .map(|&u| {
                let level = self.units[u].level as f64;
                match self.units[u].type_name {
                    CITY => 25_000.0 * level,
                    DEFENSE_POST => 5_000.0 * level,
                    MISSILE_SILO => 50_000.0 * level,
                    PORT | FACTORY => 15_000.0 * level,
                    _ => 0.0,
                }
            })
            .fold(0.0, |a, b| a + b);
        // Medium: a SAM within 50 tiles makes the spot worthless (Easy ignores SAMs)
        if self.config.difficulty == "Medium" && targets.iter().any(|&u| self.units[u].type_name == SAM_LAUNCHER && self.dist2(tile, self.units[u].tile) <= 2500) {
            return -1.0;
        }
        let silo_tiles: Vec<u32> = silos.iter().map(|&s| self.units[s].tile).collect();
        let (closest, _) = closest_two_tiles(&self.map, &silo_tiles, &[tile]).expect("Missing result");
        let dist = (self.dist2(tile, closest) as f64).sqrt();
        let base = value;
        value = crate::detmath::js_max(base * 0.2, value - dist * 30.0);
        let recent: f64 = ne
            .nukes
            .recent
            .iter()
            .filter(|r| {
                let inner = Self::nuke_magnitude(r.2).0;
                self.dist2(tile, r.1) <= inner * inner
            })
            .map(|_| 1_000_000.0)
            .fold(0.0, |a, b| a + b);
        value - recent
    }

    pub(super) fn send_nation_nuke(&mut self, ne: &mut NationExec, tile: u32, t: &'static str, target: usize) {
        let tick = self.ticks;
        ne.nukes.recent.push((tick, tile, t));
        let perceived = self.inflated_nuke_cost(ne, t);
        if t == ATOM_BOMB {
            ne.nukes.atom_perceived = perceived * 150 / 100;
            ne.nukes.atom_last_launch = Some(tick);
        } else {
            ne.nukes.hydro_perceived = perceived * 125 / 100;
            ne.nukes.hydro_last_launch = Some(tick);
        }
        let p = ne.player.unwrap();
        let e = self.new_nuke(t, p, tile, true);
        self.add_execution(e);
        self.emoji_maybe_send(ne, target, &["☢️", "💥"]);
    }
}
