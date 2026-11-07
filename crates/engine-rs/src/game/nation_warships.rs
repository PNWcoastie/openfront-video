use super::*;

// ── NationWarshipBehavior: ship tracking and retaliation (Medium) ──

impl Game {
    pub(super) fn nation_track_ships_and_retaliate(&mut self, ne: &mut NationExec) {
        let p = ne.player.unwrap();
        // trackTransportShipsAndRetaliate
        for u in self.player_units_of(p, TRANSPORT_SHIP) {
            if !ne.tracked_transports.contains(&u) {
                ne.tracked_transports.push(u);
            }
        }
        for u in ne.tracked_transports.clone() {
            if self.units[u].active {
                continue;
            }
            if let Some(d) = self.units[u].destroyer {
                self.maybe_retaliate_with_warship(ne, self.units[u].tile, d, false);
            }
            ne.tracked_transports.retain(|&x| x != u);
        }
        // trackTradeShipsAndRetaliate
        for u in self.player_units_of(p, TRADE_SHIP) {
            if !ne.tracked_trade.contains(&u) {
                ne.tracked_trade.push(u);
            }
        }
        for u in ne.tracked_trade.clone() {
            if !self.units[u].active {
                ne.tracked_trade.retain(|&x| x != u);
                continue;
            }
            let owner = self.units[u].owner;
            if owner != p {
                self.maybe_retaliate_with_warship(ne, self.units[u].tile, owner, true);
                ne.tracked_trade.retain(|&x| x != u);
            }
        }
        // trackIncomingTransportsAndRetaliate
        let small = self.players[p].small_id;
        let mut new_incoming: Vec<usize> = self
            .type_units(TRANSPORT_SHIP)
            .iter()
            .copied()
            .filter(|&u| {
                let unit = &self.units[u];
                unit.active
                    && !unit.retreating
                    && unit.target_tile.is_some_and(|target| target != 0 && self.map.owner_id(target) == small)
                    && self.players[unit.owner].small_id != small
                    && !ne.tracked_incoming.contains(&u)
            })
            .collect();
        if new_incoming.len() > 1 {
            new_incoming.sort_by_key(|&u| {
                let owner = self.units[u].owner;
                let pos = self.players[owner].units.iter().position(|&x| x == u).unwrap_or(0);
                (owner, pos)
            });
        }
        ne.tracked_incoming.extend(new_incoming);
        for u in ne.tracked_incoming.clone() {
            let unit = &self.units[u];
            let Some(target) = unit.target_tile.filter(|_| unit.active && !unit.retreating) else {
                ne.tracked_incoming.retain(|&x| x != u);
                ne.dealt_with.retain(|&x| x != u);
                continue;
            };
            if ne.dealt_with.contains(&u) {
                continue;
            }
            if self.map.manhattan_dist(unit.tile, target) < 20 {
                ne.dealt_with.push(u);
                continue;
            }
            let owner = unit.owner;
            if self.is_allied_with(owner, p) {
                continue;
            }
            let covered = self.player_units_of(p, WARSHIP).iter().any(|&w| self.in_grid_cells(target, 90, self.units[w].tile) && self.dist2(self.units[w].tile, target) <= 90 * 90)
                || self.player_units_of(p, WARSHIP).iter().any(|&w| self.map.manhattan_dist(target, self.ws(w).patrol_tile) < 90);
            if covered {
                ne.dealt_with.push(u);
                continue;
            }
            // warshipSpawnTile(target, 30)
            let (tx, ty) = (self.map.x(target) as f64, self.map.y(target) as f64);
            let (w, h) = (self.map.width as f64, self.map.height as f64);
            let mut ocean = None;
            for _ in 0..50 {
                let x = ne.random.next_int(tx - 30.0, tx + 30.0);
                let y = ne.random.next_int(ty - 30.0, ty + 30.0);
                if !(x >= 0.0 && x < w && y >= 0.0 && y < h) {
                    continue;
                }
                let t = self.map.ref_at(x as u32, y as u32);
                if self.map.is_water(t) {
                    ocean = Some(t);
                    break;
                }
            }
            let Some(tile) = ocean else { continue };
            self.maybe_retaliate_with_warship(ne, tile, owner, false);
            ne.dealt_with.push(u);
            break;
        }
    }

    pub(super) fn hostile_warships_near(&self, p: usize, tile: u32) -> usize {
        let mut count = 0;
        self.any_nearby_unit(tile, 130, &[WARSHIP], |u, _| {
            if self.can_attack_player_afk(self.units[u].owner, p, true) {
                count += 1;
            }
            false
        });
        count
    }

    pub(super) fn can_build_warship_at(&self, p: usize, tile: u32) -> bool {
        self.unit_cost(p, WARSHIP) <= self.players[p].gold && self.players[p].is_alive() && self.warship_spawn(p, tile).is_some()
    }

    /// clearSeaLane: send a squad at the fleet blocking our boats, all at once or
    /// not at all (Hard as many ships as they have, Impossible one more).
    pub(super) fn clear_sea_lane(&mut self, p: usize, blocker: usize) {
        let target = self.units[blocker].tile;
        let fleet = self.hostile_warships_near(p, target) as i64;
        let wanted = match self.config.difficulty.as_str() {
            "Hard" => fleet,
            "Impossible" => fleet + 1,
            _ => return,
        };
        let covering = self.warships_covering(p, target);
        let component = self.water_component(target);
        let mut spare: Vec<usize> = self
            .player_units_of(p, WARSHIP)
            .into_iter()
            .filter(|&w| {
                !covering.contains(&w)
                    && self.ws(w).mode == WarshipMode::Patrolling
                    && self.water_component(self.units[w].tile) == component
                    && self.hostile_warships_near(p, self.ws(w).patrol_tile) == 0
            })
            .collect();
        spare.sort_by_key(|&w| self.map.manhattan_dist(self.units[w].tile, target));
        let recruits: Vec<usize> = spare.into_iter().take((wanted - covering.len() as i64).max(0) as usize).collect();
        let builds = wanted - covering.len() as i64 - recruits.len() as i64;
        if builds > 0 {
            if self.player_units_of(p, WARSHIP).len() as i64 + builds > 10 {
                return;
            }
            // Each ship we own raises the price of the next.
            let total: i64 = (0..builds).map(|i| self.unit_cost_extra(p, WARSHIP, i as f64)).sum();
            if self.players[p].gold < total || !self.can_build_warship_at(p, target) {
                return;
            }
        }
        for w in recruits {
            self.ws_mut(w).patrol_tile = target;
        }
        for _ in 0..builds.max(0) {
            let e = self.new_construction(p, WARSHIP, target as f64, true, None);
            self.add_execution(e);
        }
    }

    /// counterWarshipInfestation: one of the 3 richest nations (of its team) sends
    /// a warship at an enemy hoarding them.
    pub(super) fn nation_counter_warship_infestation(&mut self, ne: &mut NationExec) {
        let p = ne.player.unwrap();
        if !self.lands_beachheads() {
            return;
        }
        let total: usize = self.type_units(WARSHIP).len();
        if total <= 10
            || self.unit_cost(p, WARSHIP) > self.players[p].gold
            || self.player_units_of(p, PORT).is_empty()
            || self.player_units_of(p, WARSHIP).len() >= 10
        {
            return;
        }
        let team = self.players[p].team.clone();
        let mut rich: Vec<usize> = self
            .alive_players()
            .into_iter()
            .filter(|&q| self.players[q].info.player_type != PlayerType::Human && (team.is_none() || self.players[q].team == team))
            .collect();
        rich.sort_by(|&a, &b| self.players[b].gold.cmp(&self.players[a].gold));
        if !rich.iter().take(3).any(|&q| q == p) {
            return;
        }
        let target = if team.is_some() {
            // Teams with more than 15 warships, in order of first appearance.
            let mut teams: Vec<(String, usize, Vec<usize>)> = Vec::new();
            for q in self.alive_players() {
                if self.is_friendly(p, q, false) || q == p {
                    continue;
                }
                let Some(t) = self.players[q].team.clone() else { continue };
                let count = self.player_units_of(q, WARSHIP).len();
                match teams.iter_mut().find(|e| e.0 == t) {
                    Some(e) => {
                        e.1 += count;
                        e.2.push(q);
                    }
                    None => teams.push((t, count, vec![q])),
                }
            }
            let mut found = None;
            for (_, count, members) in teams {
                if count <= 15 {
                    continue;
                }
                let mut most: Option<usize> = None;
                for q in members {
                    let c = self.player_units_of(q, WARSHIP).len();
                    if c > most.map_or(0, |m| self.player_units_of(m, WARSHIP).len()) {
                        most = Some(q);
                    }
                }
                if let Some(m) = most {
                    let ships = self.player_units_of(m, WARSHIP);
                    if ships.len() > 3 {
                        found = Some(ships[ne.random.next_int(0.0, ships.len() as f64) as usize]);
                        break;
                    }
                }
            }
            found
        } else {
            let mut found = None;
            for q in self.alive_players() {
                if self.is_friendly(p, q, false) || q == p {
                    continue;
                }
                let ships = self.player_units_of(q, WARSHIP);
                if ships.len() > 10 {
                    found = Some(ships[ne.random.next_int(0.0, ships.len() as f64) as usize]);
                    break;
                }
            }
            found
        };
        let Some(ship) = target else { return };
        let tile = self.units[ship].tile;
        if !self.can_build_warship_at(p, tile) {
            self.maybe_move_warship(p, tile);
            return;
        }
        let e = self.new_construction(p, WARSHIP, tile as f64, true, None);
        self.add_execution(e);
        self.emoji_send(ne, None, &["⛵"]);
    }

    pub(super) fn warships_covering(&self, p: usize, tile: u32) -> Vec<usize> {
        let component = self.water_component(tile);
        self.player_units_of(p, WARSHIP)
            .into_iter()
            .filter(|&w| self.water_component(self.units[w].tile) == component && self.map.manhattan_dist(self.ws(w).patrol_tile, tile) <= 100)
            .collect()
    }

    pub(super) fn maybe_move_warship(&mut self, p: usize, tile: u32) {
        if !self.map.is_water(tile) {
            return;
        }
        let mut ships: Vec<usize> = self
            .player_units_of(p, WARSHIP)
            .into_iter()
            .filter(|&w| self.map.manhattan_dist(self.units[w].tile, self.ws(w).patrol_tile) < 130)
            .collect();
        ships.sort_by_key(|&w| self.map.manhattan_dist(self.units[w].tile, tile));
        if let Some(&w) = ships.first() {
            self.ws_mut(w).patrol_tile = tile;
        }
    }

    pub(super) fn maybe_retaliate_with_warship(&mut self, ne: &mut NationExec, tile: u32, enemy: usize, trade: bool) {
        let p = ne.player.unwrap();
        if enemy == p || !self.warships_covering(p, tile).is_empty() {
            return;
        }
        if self.player_units_of(p, WARSHIP).len() >= 10 {
            self.maybe_move_warship(p, tile);
            return;
        }
        // Easy never retaliates; Medium 15% of the time, Hard 50%, Impossible 80%.
        let Some(chance) = self.by_difficulty([None, Some(15.0), Some(50.0), Some(80.0)]) else { return };
        if ne.random.next_int(0.0, 100.0) < chance {
            if self.unit_cost(p, WARSHIP) > self.players[p].gold || !self.players[p].is_alive() || self.warship_spawn(p, tile).is_none() {
                self.maybe_move_warship(p, tile);
                return;
            }
            let e = self.new_construction(p, WARSHIP, tile as f64, true, None);
            self.add_execution(e);
            self.emoji_maybe_send(ne, enemy, &["⛵"]);
            self.update_relation(p, enemy, if trade { -7.5 } else { -15.0 });
        }
    }
}

