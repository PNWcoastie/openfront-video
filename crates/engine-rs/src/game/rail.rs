use super::*;
use crate::fxhash::FxHashSet;

// ── Factories, train stations, the rail network and trains ──
// (FactoryExecution, TrainStationExecution, TrainExecution, RailNetworkImpl,
// TrainStation, Cluster, Railroad, RailSpatialGrid, AStar.Rail, StationPathFinder)

pub const TRAIN: &str = "Train";
const STATION_MAX_RANGE: i64 = 110;
const STATION_MIN_RANGE: i64 = 15;
const RAILROAD_MAX_SIZE: f64 = 110.0 * 1.4142;
const STATION_RADIUS: i64 = 3;
const RAIL_GRID_CELL: i64 = 4;
const MAX_CONNECTION_DISTANCE: i64 = 4;

pub struct Station {
    pub(super) id: u32,
    pub(super) unit: usize,
    pub(super) cluster: Option<usize>,
    pub(super) railroads: TileSet,
    pub(super) by_neighbor: FxHashMap<usize, usize>,
}

pub struct Railroad {
    pub(super) from: usize,
    pub(super) to: usize,
    pub(super) tiles: Vec<u32>,
}

#[derive(Default)]
pub struct Cluster {
    pub(super) stations: TileSet,
    pub(super) trade: TileSet,
}

/// Generic AStar's BucketQueue: LIFO within a bucket.
#[derive(Default)]
struct BucketQueue {
    buckets: Vec<Vec<u32>>,
    touched: Vec<usize>,
    min_bucket: usize,
    size: usize,
}

impl BucketQueue {
    fn reset(&mut self, max_priority: usize) {
        for &b in &self.touched {
            self.buckets[b].clear();
        }
        self.touched.clear();
        if self.buckets.len() < max_priority + 1 {
            self.buckets.resize_with(max_priority + 1, Vec::new);
        }
        self.min_bucket = max_priority + 1;
        self.size = 0;
    }
    fn push(&mut self, node: u32, priority: u32, max_bucket: usize) {
        let b = (priority as usize).min(max_bucket - 1);
        if self.buckets[b].is_empty() {
            self.touched.push(b);
        }
        self.buckets[b].push(node);
        self.size += 1;
        if b < self.min_bucket {
            self.min_bucket = b;
        }
    }
    fn pop(&mut self, max_bucket: usize) -> u32 {
        while self.min_bucket < max_bucket {
            if let Some(n) = self.buckets[self.min_bucket].pop() {
                self.size -= 1;
                return n;
            }
            self.min_bucket += 1;
        }
        u32::MAX
    }
}

/// Stamped scratch arrays for the generic AStar (fresh arrays per call in the
/// engine; stamps give the same result).
#[derive(Default)]
struct AStarScratch {
    stamp: u32,
    closed: Vec<u32>,
    g_stamp: Vec<u32>,
    g: Vec<u32>,
    came_from: Vec<i64>,
    queue: BucketQueue,
}

impl AStarScratch {
    fn begin(&mut self, n: usize, max_priority: usize) {
        if self.closed.len() < n {
            self.closed.resize(n, 0);
            self.g_stamp.resize(n, 0);
            self.g.resize(n, 0);
            self.came_from.resize(n, -1);
        }
        self.stamp += 1;
        self.queue.reset(max_priority);
    }
    fn build_path(&self, goal: u32) -> Vec<u32> {
        let mut path = Vec::new();
        let mut cur = goal as i64;
        while cur != -1 {
            path.push(cur as u32);
            cur = self.came_from[cur as usize];
        }
        path.reverse();
        path
    }
}

#[derive(Default)]
pub struct RailNetwork {
    pub(super) stations: Vec<Station>,
    pub(super) manager: TileSet,
    pub(super) by_id: Vec<Option<usize>>,
    pub(super) next_station_id: u32,
    pub(super) rails: Vec<Railroad>,
    pub(super) clusters: Vec<Cluster>,
    pub(super) dirty: TileSet,
    pub(super) grid_cells: FxHashMap<(i64, i64), TileSet>,
    pub(super) rail_cells: FxHashMap<usize, Vec<(i64, i64)>>,
    rail_astar: AStarScratch,
    station_astar: AStarScratch,
}

pub struct TrainStationExec {
    pub(super) active: bool,
    pub(super) unit: usize,
    pub(super) spawn_trains: bool,
    pub(super) random: Option<PseudoRandom>,
    pub(super) station: Option<usize>,
    pub(super) last_spawn_tick: u32,
}

pub struct TrainExec {
    pub(super) active: bool,
    pub(super) player: usize,
    pub(super) source: usize,
    pub(super) destination: usize,
    pub(super) train: Option<usize>,
    pub(super) cars: Vec<usize>,
    pub(super) current_tile: i64,
    pub(super) used_tiles: std::collections::VecDeque<u32>,
    pub(super) stations: Vec<usize>,
    pub(super) current: Option<Vec<u32>>,
    pub(super) trade_stops: u32,
    pub(super) path_tiles: Vec<u32>,
    pub(super) path_index: i64,
}

const SPEED: i64 = 2;
const SPACING: usize = 2;
const NUM_CARS: usize = 5;

impl Game {
    // ── Unit queries ──

    /// UnitGrid.nearbyUnits: completed, active units of the types within
    /// range, in grid order (cell row, cell column, type, insertion).
    pub(super) fn nearby_units(&self, tile: u32, range: i64, types: &[&str]) -> Vec<(usize, i64)> {
        let _p = crate::prof::scope("nearby_units");
        debug_assert!(!types.contains(&TRAIN), "trains are not in the cell index");
        if types.iter().all(|t| self.type_units(t).is_empty()) {
            return Vec::new();
        }
        let mut found: Vec<(i64, i64, usize, u64, usize, i64)> = Vec::new();
        let (sx, ex, sy, ey) = self.grid_cell_bounds(tile, range);
        let cols = (self.map.width as i64 + 99) / 100;
        let r2 = range * range;
        for cy in sy..=ey {
            for cx in sx..=ex {
                for &i in &self.cells[(cy * cols + cx) as usize] {
                    let u = &self.units[i];
                    if u.under_construction {
                        continue;
                    }
                    let Some(ti) = types.iter().position(|&t| t == u.type_name) else { continue };
                    let d = self.dist2(u.tile, tile);
                    if d > r2 {
                        continue;
                    }
                    found.push((cy, cx, ti, u.grid_seq, i, d));
                }
            }
        }
        // grid_seq is when the unit last entered its cell (Unit.move re-files it).
        found.sort_by_key(|&(cy, cx, ti, seq, _, _)| (cy, cx, ti, seq));
        found.into_iter().map(|(_, _, _, _, i, d)| (i, d)).collect()
    }

    pub(super) fn any_nearby_unit(&self, tile: u32, range: i64, types: &[&str], mut pred: impl FnMut(usize, i64) -> bool) -> bool {
        debug_assert!(!types.contains(&TRAIN), "trains are not in the cell index");
        if types.iter().all(|t| self.type_units(t).is_empty()) {
            return false;
        }
        let (sx, ex, sy, ey) = self.grid_cell_bounds(tile, range);
        let cols = (self.map.width as i64 + 99) / 100;
        let r2 = range * range;
        for cy in sy..=ey {
            for cx in sx..=ex {
                for &i in &self.cells[(cy * cols + cx) as usize] {
                    let u = &self.units[i];
                    if u.under_construction || !types.contains(&u.type_name) {
                        continue;
                    }
                    let d = self.dist2(u.tile, tile);
                    if d <= r2 && pred(i, d) {
                        return true;
                    }
                }
            }
        }
        false
    }

    pub(super) fn train_station_exec(&mut self, unit: usize, spawn_trains: bool) -> Exec {
        self.units[unit].has_train_station = true;
        Exec::TrainStation(Box::new(TrainStationExec {
            active: true,
            unit,
            spawn_trains,
            random: None,
            station: None,
            last_spawn_tick: 0,
        }))
    }

    /// CityExecution / PortExecution.createStation
    pub(super) fn create_station_if_factory_nearby(&mut self, unit: usize) {
        if self.has_unit_nearby(self.units[unit].tile, STATION_MAX_RANGE, FACTORY, None) {
            let e = self.train_station_exec(unit, false);
            self.add_execution(e);
        }
    }

    pub(super) fn tick_factory(&mut self, unit: usize, station_created: &mut bool, active: &mut bool) {
        if !*station_created {
            let structures = self.nearby_units(self.units[unit].tile, STATION_MAX_RANGE, &[CITY, PORT, FACTORY]);
            let e = self.train_station_exec(unit, true);
            self.add_execution(e);
            for (u, _) in structures {
                if !self.units[u].has_train_station {
                    let e = self.train_station_exec(u, false);
                    self.add_execution(e);
                }
            }
            *station_created = true;
        }
        if !self.units[unit].active {
            *active = false;
        }
    }

    // ── TrainStationExecution ──

    pub(super) fn tick_train_station(&mut self, te: &mut TrainStationExec) {
        if !te.active {
            return;
        }
        let station = match te.station {
            Some(s) => s,
            None => {
                let s = self.rail.stations.len();
                self.rail.stations.push(Station {
                    id: 0,
                    unit: te.unit,
                    cluster: None,
                    railroads: TileSet::default(),
                    by_neighbor: FxHashMap::default(),
                });
                te.station = Some(s);
                self.connect_station(s);
                s
            }
        };
        if !self.units[te.unit].active {
            te.active = false;
            return;
        }
        if te.spawn_trains {
            self.spawn_train(te, station);
        }
    }

    pub(super) fn train_spawn_rate(factories: u32, trains: u32) -> f64 {
        let f = factories as f64;
        let rate = (f + 10.0) * 15.0;
        let boost = if factories == 0 { 1.0 } else { 1.0 + (0.5 * within((6.0 - f) / 5.0, 0.0, 1.0)) / f };
        let t = trains as f64;
        let ln2 = std::f64::consts::LN_2;
        let damping = 1.0 - sigmoid(t, ln2 / 100.0, 560.0);
        let plateau = 0.25 * (1.0 - sigmoid(t, ln2 / 150.0, 900.0));
        let saturation = crate::detmath::js_max(damping, plateau);
        crate::detmath::js_max(1.0, (rate / (saturation * boost)).floor())
    }

    pub(super) fn spawn_train(&mut self, te: &mut TrainStationExec, station: usize) {
        let ticks = self.ticks;
        if ticks < te.last_spawn_tick + 10 {
            return;
        }
        let Some(cluster) = self.rail.stations[station].cluster else { return };
        let owner = self.units[te.unit].owner;
        if !self.rail.clusters[cluster].trade.values().any(|s| self.trade_available(s as usize, owner)) {
            return;
        }
        let rate = Self::train_spawn_rate(self.unit_levels(owner, FACTORY), self.train_units);
        let random = te.random.as_mut().expect("initialized");
        let mut spawn = false;
        for _ in 0..self.units[te.unit].level {
            if random.next_int(0.0, rate) == 0.0 {
                spawn = true;
                break;
            }
        }
        if !spawn {
            return;
        }
        // Cluster.randomTradeDestination: reservoir sampling.
        let mut selected = None;
        let mut seen = 0.0;
        let trade: Vec<u32> = self.rail.clusters[cluster].trade.values().collect();
        for s in trade {
            if !self.trade_available(s as usize, owner) {
                continue;
            }
            seen += 1.0;
            if te.random.as_mut().unwrap().next_int(0.0, seen) == 0.0 {
                selected = Some(s as usize);
            }
        }
        let Some(dest) = selected else { return };
        if dest == station {
            return;
        }
        self.add_execution(Exec::Train(Box::new(TrainExec {
            active: true,
            player: owner,
            source: station,
            destination: dest,
            train: None,
            cars: Vec::new(),
            current_tile: 0,
            used_tiles: Default::default(),
            stations: Vec::new(),
            current: None,
            trade_stops: 0,
            path_tiles: Vec::new(),
            path_index: 0,
        })));
        te.last_spawn_tick = ticks;
    }

    pub(super) fn trade_available(&self, station: usize, other: usize) -> bool {
        let owner = self.units[self.rail.stations[station].unit].owner;
        other == owner || self.can_trade(owner, other)
    }

    // ── RailNetworkImpl ──

    pub(super) fn connect_station(&mut self, s: usize) {
        let r = &mut self.rail;
        r.next_station_id = r.next_station_id.max(1);
        let id = r.next_station_id;
        r.next_station_id += 1;
        r.stations[s].id = id;
        if r.by_id.len() <= id as usize {
            r.by_id.resize(id as usize + 1, None);
        }
        r.by_id[id as usize] = Some(s);
        r.manager.add(s as u32);
        if !self.connect_to_existing_rails(s) {
            self.connect_to_nearby_stations(s);
        }
    }

    pub(super) fn station_tile(&self, s: usize) -> u32 {
        self.units[self.rail.stations[s].unit].tile
    }

    pub(super) fn add_railroad(&mut self, s: usize, rail: usize) {
        let r = &mut self.rail;
        r.stations[s].railroads.add(rail as u32);
        let neighbor = if r.rails[rail].from == s { r.rails[rail].to } else { r.rails[rail].from };
        r.stations[s].by_neighbor.insert(neighbor, rail);
    }

    pub(super) fn remove_railroad(&mut self, s: usize, rail: usize) {
        let r = &mut self.rail;
        r.stations[s].railroads.delete(rail as u32);
        let neighbor = if r.rails[rail].from == s { r.rails[rail].to } else { r.rails[rail].from };
        r.stations[s].by_neighbor.remove(&neighbor);
    }

    pub(super) fn station_neighbors(&self, s: usize) -> Vec<usize> {
        let r = &self.rail;
        r.stations[s]
            .railroads
            .values()
            .map(|rail| {
                let rr = &r.rails[rail as usize];
                if rr.from != s {
                    rr.from
                } else {
                    rr.to
                }
            })
            .collect()
    }

    pub(super) fn new_railroad(&mut self, from: usize, to: usize, tiles: Vec<u32>) -> usize {
        self.rail.rails.push(Railroad { from, to, tiles });
        self.rail.rails.len() - 1
    }

    pub(super) fn rail_cell(&self, tile: u32) -> (i64, i64) {
        let (x, y) = (self.map.x(tile) as i64, self.map.y(tile) as i64);
        (x.div_euclid(RAIL_GRID_CELL), y.div_euclid(RAIL_GRID_CELL))
    }

    pub(super) fn grid_register(&mut self, rail: usize) {
        self.grid_unregister(rail);
        let mut cells: Vec<(i64, i64)> = Vec::new();
        for i in 0..self.rail.rails[rail].tiles.len() {
            let k = self.rail_cell(self.rail.rails[rail].tiles[i]);
            if cells.contains(&k) {
                continue;
            }
            self.rail.grid_cells.entry(k).or_default().add(rail as u32);
            cells.push(k);
        }
        if !cells.is_empty() {
            self.rail.rail_cells.insert(rail, cells);
        }
    }

    pub(super) fn grid_unregister(&mut self, rail: usize) {
        let Some(keys) = self.rail.rail_cells.remove(&rail) else { return };
        for k in keys {
            if let Some(set) = self.rail.grid_cells.get_mut(&k) {
                set.delete(rail as u32);
                if set.len() == 0 {
                    self.rail.grid_cells.remove(&k);
                }
            }
        }
    }

    pub(super) fn grid_query(&self, tile: u32, radius: i64) -> Vec<usize> {
        let (x, y) = (self.map.x(tile) as i64, self.map.y(tile) as i64);
        let c0 = ((x - radius).div_euclid(RAIL_GRID_CELL), (y - radius).div_euclid(RAIL_GRID_CELL));
        let c1 = ((x + radius).div_euclid(RAIL_GRID_CELL), (y + radius).div_euclid(RAIL_GRID_CELL));
        let mut result = TileSet::default();
        for cx in c0.0..=c1.0 {
            for cy in c0.1..=c1.1 {
                if let Some(set) = self.rail.grid_cells.get(&(cx, cy)) {
                    for rail in set.values() {
                        result.add(rail);
                    }
                }
            }
        }
        result.values().map(|r| r as usize).collect()
    }

    pub(super) fn set_cluster(&mut self, s: usize, c: Option<usize>) {
        if let Some(old) = self.rail.stations[s].cluster {
            if Some(old) != c {
                self.cluster_remove_station(old, s);
            }
        }
        self.rail.stations[s].cluster = c;
    }

    pub(super) fn cluster_add_station(&mut self, c: usize, s: usize) {
        self.rail.clusters[c].stations.add(s as u32);
        if matches!(self.units[self.rail.stations[s].unit].type_name, CITY | PORT) {
            self.rail.clusters[c].trade.add(s as u32);
        }
        self.set_cluster(s, Some(c));
    }

    pub(super) fn cluster_remove_station(&mut self, c: usize, s: usize) {
        self.rail.clusters[c].stations.delete(s as u32);
        self.rail.clusters[c].trade.delete(s as u32);
    }

    pub(super) fn new_cluster(&mut self) -> usize {
        self.rail.clusters.push(Cluster::default());
        self.rail.clusters.len() - 1
    }

    pub(super) fn merge_clusters(&mut self, clusters: &TileSet) {
        let merged = self.new_cluster();
        for c in clusters.values() {
            let members: Vec<u32> = self.rail.clusters[c as usize].stations.values().collect();
            for s in members {
                self.cluster_add_station(merged, s as usize);
            }
        }
    }

    pub(super) fn closest_tile_index(&self, rail: usize, to: u32) -> i64 {
        let tiles = &self.rail.rails[rail].tiles;
        if tiles.is_empty() {
            return -1;
        }
        let mut best = 0;
        let mut min = i64::MAX;
        for (i, &t) in tiles.iter().enumerate() {
            let d = self.dist2(t, to);
            if d < min {
                min = d;
                best = i;
            }
        }
        best as i64
    }

    pub(super) fn connect_to_existing_rails(&mut self, s: usize) -> bool {
        let tile = self.station_tile(s);
        let rails = self.grid_query(tile, STATION_RADIUS);
        let mut edited = TileSet::default();
        for rail in rails {
            let (from, to) = (self.rail.rails[rail].from, self.rail.rails[rail].to);
            let idx = self.closest_tile_index(rail, tile);
            if idx == 0 || idx >= self.rail.rails[rail].tiles.len() as i64 {
                continue;
            }
            let idx = idx as usize;
            self.remove_railroad(from, rail);
            self.remove_railroad(to, rail);
            self.grid_unregister(rail);
            let head = self.rail.rails[rail].tiles[..idx].to_vec();
            let tail = self.rail.rails[rail].tiles[idx..].to_vec();
            let a = self.new_railroad(from, s, head);
            let b = self.new_railroad(s, to, tail);
            self.add_railroad(s, a);
            self.add_railroad(s, b);
            self.add_railroad(from, a);
            self.add_railroad(to, b);
            self.grid_register(b);
            self.grid_register(a);
            if let Some(c) = self.rail.stations[from].cluster {
                self.cluster_add_station(c, s);
                edited.add(c as u32);
            }
        }
        if edited.len() > 1 {
            self.merge_clusters(&edited);
        }
        edited.len() != 0
    }

    pub(super) fn find_station(&self, unit: usize) -> Option<usize> {
        self.rail.manager.values().map(|s| s as usize).find(|&s| self.rail.stations[s].unit == unit)
    }

    pub(super) fn connect_to_nearby_stations(&mut self, s: usize) {
        let tile = self.station_tile(s);
        let mut neighbors = self.nearby_units(tile, STATION_MAX_RANGE, &[CITY, FACTORY, PORT]);
        neighbors.sort_by_key(|&(_, d)| d);
        let mut edited = TileSet::default();
        for (u, d2) in neighbors {
            if u == self.rail.stations[s].unit {
                continue;
            }
            let Some(ns) = self.find_station(u) else { continue };
            let dist = self.distance_from(ns, s, MAX_CONNECTION_DISTANCE);
            let Some(nc) = self.rail.stations[ns].cluster else { continue };
            let available = dist > MAX_CONNECTION_DISTANCE || dist == -1;
            if available && d2 > STATION_MIN_RANGE * STATION_MIN_RANGE && self.connect(s, ns) {
                self.cluster_add_station(nc, s);
                edited.add(nc as u32);
            }
        }
        if edited.len() > 1 {
            self.merge_clusters(&edited);
        } else if edited.len() == 0 {
            let c = self.new_cluster();
            self.cluster_add_station(c, s);
        }
    }

    pub(super) fn connect(&mut self, from: usize, to: usize) -> bool {
        let path = self.rail_tile_path(self.station_tile(from), self.station_tile(to));
        if !path.is_empty() && (path.len() as f64) < RAILROAD_MAX_SIZE {
            let rail = self.new_railroad(from, to, path);
            self.add_railroad(from, rail);
            self.add_railroad(to, rail);
            self.grid_register(rail);
            return true;
        }
        false
    }

    pub(super) fn distance_from(&self, start: usize, dest: usize, max: i64) -> i64 {
        if start == dest {
            return 0;
        }
        let mut visited: FxHashSet<usize> = FxHashSet::default();
        let mut queue = std::collections::VecDeque::from([(start, 0i64)]);
        while let Some((st, d)) = queue.pop_front() {
            if !visited.insert(st) {
                continue;
            }
            if d >= max {
                continue;
            }
            for n in self.station_neighbors(st) {
                if n == dest {
                    return d + 1;
                }
                if !visited.contains(&n) {
                    queue.push_back((n, d + 1));
                }
            }
        }
        -1
    }

    pub(super) fn compute_cluster(&self, start: usize) -> Vec<usize> {
        let mut visited = TileSet::default();
        let mut queue = std::collections::VecDeque::from([start]);
        while let Some(cur) = queue.pop_front() {
            if visited.contains(cur as u32) {
                continue;
            }
            visited.add(cur as u32);
            for n in self.station_neighbors(cur) {
                if !visited.contains(n as u32) {
                    queue.push_back(n);
                }
            }
        }
        visited.values().map(|v| v as usize).collect()
    }

    pub(super) fn recompute_clusters(&mut self) {
        if self.rail.dirty.len() == 0 {
            return;
        }
        let dirty: Vec<u32> = self.rail.dirty.values().collect();
        for c in dirty {
            let mut original = TileSet::default();
            for s in self.rail.clusters[c as usize].stations.values() {
                original.add(s);
            }
            while original.len() > 0 {
                let next = original.values().next().unwrap() as usize;
                let connected = self.compute_cluster(next);
                for &s in &connected {
                    original.delete(s as u32);
                }
                if original.len() > 0 {
                    let nc = self.new_cluster();
                    for &s in &connected {
                        self.cluster_add_station(nc, s);
                    }
                }
            }
        }
        self.rail.dirty.clear();
    }

    /// RailNetworkImpl.removeStation (GameImpl.removeUnit for station units).
    pub(super) fn remove_station(&mut self, unit: usize) {
        let Some(s) = self.find_station(unit) else { return };
        let rails: Vec<u32> = self.rail.stations[s].railroads.values().collect();
        for rail in rails {
            let (from, to) = (self.rail.rails[rail as usize].from, self.rail.rails[rail as usize].to);
            self.remove_railroad(from, rail as usize);
            self.remove_railroad(to, rail as usize);
            self.grid_unregister(rail as usize);
        }
        self.rail.stations[s].railroads.clear();
        self.rail.stations[s].by_neighbor.clear();
        let id = self.rail.stations[s].id as usize;
        self.rail.by_id[id] = None;
        while self.rail.by_id.last() == Some(&None) {
            self.rail.by_id.pop();
        }
        self.rail.manager.delete(s as u32);
        self.units[unit].has_train_station = false;
        let Some(c) = self.rail.stations[s].cluster else { return };
        self.cluster_remove_station(c, s);
        if self.rail.clusters[c].stations.len() == 0 {
            let members: Vec<u32> = self.rail.clusters[c].stations.values().collect();
            for m in members {
                self.set_cluster(m as usize, None);
            }
            self.rail.clusters[c] = Cluster::default();
            self.rail.dirty.delete(c as u32);
            return;
        }
        self.rail.dirty.add(c as u32);
    }

    // ── Path finding ──

    /// PathFinding.Rail: AStar.Rail on the mini map, upscaled.
    pub(super) fn rail_tile_path(&mut self, from: u32, to: u32) -> Vec<u32> {
        let mini = &self.water.mini;
        let (mw, mh) = (mini.width, mini.height);
        let mini_from = (self.map.y(from) / 2) * mw + self.map.x(from) / 2;
        let mini_to = (self.map.y(to) / 2) * mw + self.map.x(to) / 2;
        let Some(path) = self.rail_astar(mini_from, mini_to) else { return Vec::new() };
        if path.is_empty() {
            return Vec::new();
        }
        let w = self.map.width;
        let _ = mh;
        let mut up: Vec<u32> = Vec::new();
        for i in 0..path.len() - 1 {
            let (cx, cy) = ((path[i] % mw) as i64 * 2, (path[i] / mw) as i64 * 2);
            let (nx, ny) = ((path[i + 1] % mw) as i64 * 2, (path[i + 1] / mw) as i64 * 2);
            up.push((cy * w as i64 + cx) as u32);
            let (dx, dy) = (nx - cx, ny - cy);
            let steps = dx.abs().max(dy.abs());
            for step in 1..steps {
                let x = (cx as f64 + (dx * step) as f64 / steps as f64 + 0.5).floor() as i64;
                let y = (cy as f64 + (dy * step) as f64 / steps as f64 + 0.5).floor() as i64;
                up.push((y * w as i64 + x) as u32);
            }
        }
        let last = *path.last().unwrap();
        up.push((last / mw) * 2 * w + (last % mw) * 2);
        // fixExtremes
        match up.iter().position(|&t| t == from) {
            None => up.insert(0, from),
            Some(0) => {}
            Some(i) => {
                up.drain(..i);
            }
        }
        match up.iter().position(|&t| t == to) {
            None => up.push(to),
            Some(i) => up.truncate(i + 1),
        }
        up
    }

    pub(super) fn rail_astar(&mut self, start: u32, goal: u32) -> Option<Vec<u32>> {
        let mini = &self.water.mini;
        let (w, h) = (mini.width, mini.height);
        let n = (w * h) as usize;
        let max_priority = (2 * (w + h) * 9) as usize;
        let max_bucket = max_priority + 1;
        let heuristic = |a: u32| 2 * ((a % w).abs_diff(goal % w) + (a / w).abs_diff(goal / w));
        let sc = &mut self.rail.rail_astar;
        sc.begin(n, max_priority);
        let stamp = sc.stamp;
        sc.g[start as usize] = 0;
        sc.g_stamp[start as usize] = stamp;
        sc.came_from[start as usize] = -1;
        sc.queue.push(start, heuristic(start), max_bucket);
        let mut iterations: i64 = 500_000;
        let mut buf = [0u32; 4];
        while sc.queue.size > 0 {
            iterations -= 1;
            if iterations <= 0 {
                return None;
            }
            let cur = sc.queue.pop(max_bucket);
            if sc.closed[cur as usize] == stamp {
                continue;
            }
            sc.closed[cur as usize] = stamp;
            if cur == goal {
                return Some(sc.build_path(goal));
            }
            let cur_g = sc.g[cur as usize];
            let prev = sc.came_from[cur as usize];
            // RailAdapter.neighbors: up, down, left, right.
            let from_shore = mini.is_shoreline(cur);
            let impassable = mini.is_impassable(cur);
            let mut count = 0;
            let x = cur % w;
            let mut consider = |nb: u32, count: &mut usize| {
                if impassable {
                    return;
                }
                if !mini.is_water(nb) || from_shore || mini.is_shoreline(nb) {
                    buf[*count] = nb;
                    *count += 1;
                }
            };
            if cur >= w {
                consider(cur - w, &mut count);
            }
            if cur < w * h - w {
                consider(cur + w, &mut count);
            }
            if x != 0 {
                consider(cur - 1, &mut count);
            }
            if x != w - 1 {
                consider(cur + 1, &mut count);
            }
            for &nb in &buf[..count] {
                if sc.closed[nb as usize] == stamp {
                    continue;
                }
                let mut c = if mini.is_water(nb) || mini.is_shoreline(nb) { 6 } else { 1 };
                if prev != -1 && (cur as i64 - prev) != (nb as i64 - cur as i64) {
                    c += 3;
                }
                let tentative = cur_g + c;
                if sc.g_stamp[nb as usize] != stamp || tentative < sc.g[nb as usize] {
                    sc.came_from[nb as usize] = cur as i64;
                    sc.g[nb as usize] = tentative;
                    sc.g_stamp[nb as usize] = stamp;
                    sc.queue.push(nb, tentative + heuristic(nb), max_bucket);
                }
            }
        }
        None
    }

    /// PathFinding.Stations: AStar over station ids, same cluster only.
    pub(super) fn find_stations_path(&mut self, from: usize, to: usize) -> Option<Vec<usize>> {
        if self.rail.stations[from].cluster != self.rail.stations[to].cluster {
            return None;
        }
        let n = self.rail.next_station_id.max(1) as usize;
        let max_priority = (self.map.width + self.map.height) as usize;
        let max_bucket = max_priority + 1;
        let (start, goal) = (self.rail.stations[from].id, self.rail.stations[to].id);
        let tile_of = |g: &Game, id: u32| g.rail.by_id.get(id as usize).copied().flatten().map(|s| g.station_tile(s));
        let heuristic = |g: &Game, a: u32| match (tile_of(g, a), tile_of(g, goal)) {
            (Some(a), Some(b)) => g.map.manhattan_dist(a, b),
            _ => 0,
        };
        let mut sc = std::mem::take(&mut self.rail.station_astar);
        sc.begin(n, max_priority);
        let stamp = sc.stamp;
        sc.g[start as usize] = 0;
        sc.g_stamp[start as usize] = stamp;
        sc.came_from[start as usize] = -1;
        sc.queue.push(start, heuristic(self, start), max_bucket);
        let mut iterations: i64 = 500_000;
        let mut result = None;
        while sc.queue.size > 0 {
            iterations -= 1;
            if iterations <= 0 {
                break;
            }
            let cur = sc.queue.pop(max_bucket);
            if sc.closed[cur as usize] == stamp {
                continue;
            }
            sc.closed[cur as usize] = stamp;
            if cur == goal {
                result = Some(sc.build_path(goal));
                break;
            }
            let cur_g = sc.g[cur as usize];
            let neighbors: Vec<u32> = match self.rail.by_id.get(cur as usize).copied().flatten() {
                Some(s) => self.station_neighbors(s).into_iter().map(|x| self.rail.stations[x].id).collect(),
                None => Vec::new(),
            };
            assert!(neighbors.len() <= 32, "station with more than 32 neighbors");
            for nb in neighbors {
                if sc.closed[nb as usize] == stamp {
                    continue;
                }
                let tentative = cur_g + 1;
                if sc.g_stamp[nb as usize] != stamp || tentative < sc.g[nb as usize] {
                    sc.came_from[nb as usize] = cur as i64;
                    sc.g[nb as usize] = tentative;
                    sc.g_stamp[nb as usize] = stamp;
                    sc.queue.push(nb, tentative + heuristic(self, nb), max_bucket);
                }
            }
        }
        self.rail.station_astar = sc;
        result.map(|ids| ids.into_iter().map(|id| self.rail.by_id[id as usize].expect("station")).collect())
    }

    // ── TrainExecution ──

    pub(super) fn oriented(&self, from: usize, to: usize) -> Option<Vec<u32>> {
        let rail = *self.rail.stations[from].by_neighbor.get(&to)?;
        let r = &self.rail.rails[rail];
        if r.to == to {
            Some(r.tiles.clone())
        } else {
            Some(r.tiles.iter().rev().copied().collect())
        }
    }

    pub(super) fn init_train(&mut self, te: &mut TrainExec) {
        let stations = match self.find_stations_path(te.source, te.destination) {
            Some(s) if s.len() > 1 => s,
            _ => {
                te.active = false;
                return;
            }
        };
        te.stations = stations;
        let Some(rr) = self.oriented(te.stations[0], te.stations[1]) else {
            te.active = false;
            return;
        };
        te.current = Some(rr);
        let tile = self.station_tile(te.stations[0]);
        if !self.players[te.player].is_alive() || !self.map.is_land(tile) || self.map.is_impassable(tile) {
            te.active = false;
            return;
        }
        let train = self.build_unit(te.player, TRAIN, tile, 0.0, None);
        self.units[train].train_engine = true;
        te.train = Some(train);
        for _ in 0..NUM_CARS + 1 {
            let car = self.build_unit(te.player, TRAIN, tile, 0.0, None);
            te.cars.push(car);
        }
        let mut path: Vec<u32> = Vec::new();
        for i in 0..te.stations.len() - 1 {
            let Some(seg) = self.oriented(te.stations[i], te.stations[i + 1]) else {
                te.active = false;
                return;
            };
            path.extend(seg);
        }
        if path.first() != Some(&tile) {
            path.insert(0, tile);
            te.path_index = 1;
        }
        te.path_tiles = path;
    }

    pub(super) fn tick_train(&mut self, te: &mut TrainExec) {
        let train = te.train.expect("Not initialized");
        if !self.units[train].active {
            self.delete_train(te);
            return;
        }
        self.reconcile_split(te);
        let ok = te.stations.len() > 1
            && self.units[self.rail.stations[te.stations[1]].unit].active
            && self.units[self.rail.stations[te.stations[0]].unit].active;
        if !ok {
            self.delete_train(te);
            return;
        }
        match self.next_train_tile(te) {
            Some(t) => {
                for i in (0..te.cars.len()).rev() {
                    let idx = (i + 1) * SPACING + 2;
                    if te.used_tiles.len() > idx {
                        let car = &mut self.units[te.cars[i]];
                        car.last_tile = car.tile;
                        car.tile = te.used_tiles[idx];
                    }
                }
                let u = &mut self.units[train];
                u.last_tile = u.tile;
                u.tile = t;
            }
            None => self.delete_train(te),
        }
    }

    pub(super) fn delete_train(&mut self, te: &mut TrainExec) {
        te.active = false;
        if let Some(t) = te.train {
            if self.units[t].active {
                self.delete_unit(t);
            }
        }
        for &c in &te.cars {
            if self.units[c].active {
                self.delete_unit(c);
            }
        }
    }

    pub(super) fn save_traversed(te: &mut TrainExec, from: i64, speed: i64) {
        let Some(tiles) = &te.current else { return };
        let mut t = from;
        let mut i = 0;
        while i < speed && t < tiles.len() as i64 {
            te.used_tiles.push_back(tiles[t as usize]);
            if te.used_tiles.len() > te.cars.len() * SPACING + 3 {
                te.used_tiles.pop_front();
            }
            t += 1;
            i += 1;
        }
    }

    pub(super) fn next_train_tile(&mut self, te: &mut TrainExec) -> Option<u32> {
        if te.current.is_none() || !(te.stations.len() > 1 && self.trade_available(te.stations[1], te.player)) {
            return None;
        }
        Self::save_traversed(te, te.current_tile, SPEED);
        te.current_tile += SPEED;
        let left = te.current_tile - te.current.as_ref().unwrap().len() as i64;
        if left >= 0 {
            self.train_station_reached(te);
            if !self.next_station(te) {
                return None;
            }
            te.current_tile = left;
            Self::save_traversed(te, 0, left);
        }
        // An index past the end (or tile 0) is falsy in the engine.
        te.current.as_ref().unwrap().get(te.current_tile as usize).copied().filter(|&t| t != 0)
    }

    pub(super) fn train_station_reached(&mut self, te: &mut TrainExec) {
        let st = te.stations[1];
        let unit = self.rail.stations[st].unit;
        if !matches!(self.units[unit].type_name, CITY | PORT) {
            return;
        }
        let station_owner = self.units[unit].owner;
        let owner = te.player;
        let base = if owner == station_owner {
            10_000.0
        } else if self.is_on_same_team(owner, station_owner) {
            25_000.0
        } else if self.is_allied_with(owner, station_owner) {
            35_000.0
        } else {
            25_000.0
        };
        let visited = (te.trade_stops as f64 - 9.0).max(0.0);
        let gold = crate::detmath::js_max(5000.0, base - visited * 5000.0);
        let gold = (gold * self.config.gold_multiplier).floor() as i64;
        if owner != station_owner {
            self.add_gold(station_owner, gold);
        }
        self.add_gold(owner, gold);
        te.trade_stops += 1;
    }

    pub(super) fn next_station(&mut self, te: &mut TrainExec) -> bool {
        if te.stations.len() > 2 {
            te.path_index += te.current.as_ref().map_or(0, |t| t.len() as i64);
            te.stations.remove(0);
            let rr = match self.oriented(te.stations[0], te.stations[1]) {
                Some(r) => Some(r),
                None => self.resolve_split(te),
            };
            if let Some(r) = rr {
                te.current = Some(r);
                return true;
            }
        }
        false
    }

    pub(super) fn resolve_split(&mut self, te: &mut TrainExec) -> Option<Vec<u32>> {
        let (s0, s1) = (te.stations[0], te.stations[1]);
        let path = self.find_stations_path(s0, s1)?;
        if path.len() <= 2 {
            return None;
        }
        let mut cursor = te.path_index as usize;
        for i in 0..path.len() - 1 {
            let seg = self.oriented(path[i], path[i + 1])?;
            for t in seg {
                if te.path_tiles.get(cursor) != Some(&t) {
                    return None;
                }
                cursor += 1;
            }
        }
        te.stations.splice(0..2, path);
        self.oriented(te.stations[0], te.stations[1])
    }

    pub(super) fn reconcile_split(&mut self, te: &mut TrainExec) {
        if te.current.is_none() || te.stations.len() < 2 {
            return;
        }
        let (s0, s1) = (te.stations[0], te.stations[1]);
        if self.rail.stations[s0].by_neighbor.contains_key(&s1) {
            return;
        }
        let Some(path) = self.find_stations_path(s0, s1) else { return };
        if path.len() <= 2 {
            return;
        }
        let mut segments: Vec<Vec<u32>> = Vec::new();
        let mut cursor = te.path_index as usize;
        for i in 0..path.len() - 1 {
            let Some(seg) = self.oriented(path[i], path[i + 1]) else { return };
            for &t in &seg {
                if te.path_tiles.get(cursor) != Some(&t) {
                    return;
                }
                cursor += 1;
            }
            segments.push(seg);
        }
        let old = te.current.clone().unwrap();
        if cursor as i64 != te.path_index + old.len() as i64 {
            return;
        }
        let mut active = 0;
        let mut seg_start = 0i64;
        let mut boundary = 0i64;
        for i in 0..segments.len() - 1 {
            boundary += segments[i].len() as i64;
            let st = self.station_tile(path[i + 1]);
            let (before, after) = ((boundary - 1) as usize, boundary as usize);
            let stop = if self.dist2(old[before], st) <= self.dist2(old[after], st) { before } else { after };
            if te.current_tile <= stop as i64 {
                break;
            }
            active = i + 1;
            seg_start = boundary;
        }
        te.path_index += seg_start;
        te.current_tile -= seg_start;
        te.current = Some(segments.swap_remove(active));
        te.stations.splice(0..2, path[active..].iter().copied());
    }
}

