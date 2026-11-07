use super::*;

// ── Ports and trade ships (PortExecution, TradeShipExecution) ──

pub const TRADE_SHIP: &str = "Trade Ship";

pub struct PortExec {
    pub(super) active: bool,
    pub(super) port: usize,
    pub(super) random: Option<PseudoRandom>,
    pub(super) check_offset: u32,
    pub(super) rejections: u32,
}

pub struct TradeShipExec {
    pub(super) active: bool,
    pub(super) orig_owner: usize,
    pub(super) src_port: usize,
    pub(super) dst_port: usize,
    pub(super) ship: Option<usize>,
    pub(super) tiles_traveled: u32,
    pub(super) stepper: Stepper,
    pub(super) was_captured: bool,
}

impl Game {
    /// Player.unitCount: levels of every unit of the type (built or not).
    pub(super) fn unit_levels(&self, p: usize, t: &str) -> u32 {
        self.players[p].units.iter().map(|&u| &self.units[u]).filter(|u| u.type_name == t).map(|u| u.level).sum()
    }

    pub(super) fn player_units_of(&self, p: usize, t: &str) -> Vec<usize> {
        self.players[p].units.iter().copied().filter(|&u| self.units[u].type_name == t).collect()
    }

    pub(super) fn can_trade(&self, a: usize, b: usize) -> bool {
        !self.has_embargo_against(b, a) && !self.has_embargo_against(a, b) && a != b
    }

    /// PlayerImpl.portSpawn: our shore tiles within Manhattan 20, nearest
    /// first, that are valid structure tiles.
    pub(super) fn port_spawn(&mut self, p: usize, tile: u32) -> Option<u32> {
        let map = &self.map;
        let mut spawns: Vec<u32> = map
            .bfs(tile, |m, n| m.manhattan_dist(tile, n) <= 20)
            .into_iter()
            .filter(|&t| self.owner_index(t) == Some(p) && self.map.is_shore(t))
            .collect();
        spawns.sort_by_key(|&t| self.map.manhattan_dist(t, tile));
        let valid = self.valid_structure_spawn_tiles(p, tile);
        spawns.into_iter().find(|t| valid.contains(t))
    }

    /// WaterManager.hasWaterComponent
    pub(super) fn has_water_component(&self, tile: u32, comp: u32) -> bool {
        let mini = &self.water.mini;
        let mt = (self.map.y(tile) / 2) * mini.width + self.map.x(tile) / 2;
        if mini.is_water(mt) && self.water.component_id(mt) == comp {
            return true;
        }
        let mut nb = [0u32; 4];
        let n = mini.neighbors4(mt, &mut nb);
        for &t in &nb[..n] {
            if mini.is_water(t) && self.water.component_id(t) == comp {
                return true;
            }
        }
        for &t in &nb[..n] {
            let mut nb2 = [0u32; 4];
            let n2 = mini.neighbors4(t, &mut nb2);
            for &t2 in &nb2[..n2] {
                if mini.is_water(t2) && self.water.component_id(t2) == comp {
                    return true;
                }
            }
        }
        false
    }

    pub(super) fn trade_ship_spawn_rate(&self, rejections: u32, num_ships: u32, port_levels: u32) -> f64 {
        let rejection_modifier = 1.0 / (rejections as f64 + 1.0);
        let levels = port_levels as f64;
        let boost = if port_levels == 0 { 1.0 } else { 1.0 + (0.5 * within((6.0 - levels) / 5.0, 0.0, 1.0)) / levels };
        let base = 100.0 / (boost * boost);
        let n = num_ships as f64;
        let ln2 = std::f64::consts::LN_2;
        let damping = 1.0 - sigmoid(n, ln2 / 50.0, 330.0);
        let plateau = 0.25 * (1.0 - sigmoid(n, ln2 / 100.0, 800.0));
        let saturation = crate::detmath::js_max(damping, plateau);
        crate::detmath::js_max(1.0, ((base * rejection_modifier) / saturation).floor())
    }

    pub(super) fn tick_port(&mut self, pe: &mut PortExec) {
        let port = pe.port;
        if !self.units[port].active {
            pe.active = false;
            return;
        }
        if self.units[port].under_construction {
            return;
        }
        if !self.units[port].has_train_station {
            self.create_station_if_factory_nearby(port);
        }
        if (self.ticks + pe.check_offset) % 10 != 0 {
            return;
        }
        // shouldSpawnTradeShip
        let num_ships: u32 = self.type_units(TRADE_SHIP).iter().map(|&u| self.units[u].level).sum();
        let owner = self.units[port].owner;
        let port_levels = self.unit_levels(owner, PORT);
        let mut spawn = false;
        for _ in 0..self.units[port].level {
            let rate = self.trade_ship_spawn_rate(pe.rejections, num_ships, port_levels);
            if pe.random.as_mut().unwrap().next_int(0.0, rate) == 0.0 {
                pe.rejections = 0;
                spawn = true;
                break;
            }
            pe.rejections += 1;
        }
        if !spawn {
            return;
        }
        let ports = self.trading_ports(port);
        if ports.is_empty() {
            return;
        }
        let i = pe.random.as_mut().unwrap().next_int(0.0, ports.len() as f64) as usize;
        let dst = ports[i];
        self.add_execution(Exec::TradeShip(Box::new(TradeShipExec {
            active: true,
            orig_owner: self.units[port].owner,
            src_port: port,
            dst_port: dst,
            ship: None,
            tiles_traveled: 0,
            stepper: Stepper::default(),
            was_captured: false,
        })));
    }

    /// PortExecution.tradingPorts: a weighted list of partner ports.
    pub(super) fn trading_ports(&self, port: usize) -> Vec<usize> {
        if self.type_units(PORT).len() <= 1 {
            return Vec::new();
        }
        let tile = self.units[port].tile;
        let owner = self.units[port].owner;
        let mut comps: Vec<u32> = Vec::new();
        let mut nb = [0u32; 4];
        let n = self.map.neighbors4(tile, &mut nb);
        for &t in &nb[..n] {
            if self.map.is_water(t) {
                if let Some(c) = self.water_component(t) {
                    if !comps.contains(&c) {
                        comps.push(c);
                    }
                }
            }
        }
        let mut ports: Vec<usize> = Vec::new();
        for p in 0..self.players.len() {
            if !self.players[p].is_alive() || p == owner || !self.can_trade(p, owner) {
                continue;
            }
            for &u in &self.players[p].units {
                if self.units[u].type_name == PORT && comps.iter().any(|&c| self.has_water_component(self.units[u].tile, c)) {
                    ports.push(u);
                }
            }
        }
        ports.sort_by_key(|&u| self.map.manhattan_dist(tile, self.units[u].tile));
        let len = ports.len() as f64;
        let bonus_count = within(len / 3.0, 4.0, len);
        let mut weighted = Vec::new();
        for (i, &other) in ports.iter().enumerate() {
            let level = self.units[other].level as usize;
            let expanded = vec![other; level];
            weighted.extend_from_slice(&expanded);
            let too_close = self.map.manhattan_dist(tile, self.units[other].tile) < 300;
            let close_bonus = (i as f64) < bonus_count;
            if !too_close && close_bonus {
                weighted.extend_from_slice(&expanded);
            }
            if !too_close && self.is_friendly(owner, self.units[other].owner, false) {
                weighted.extend_from_slice(&expanded);
            }
        }
        weighted
    }

}

