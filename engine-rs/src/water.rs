//! Water pathfinding (engine pathfinding/*): the minimap's water components,
//! the abstract graph over 32x32 clusters, hierarchical A* with bounded local
//! A*, and the transformer chain the game routes boats through:
//!
//!   MiniMap -> ShoreCoercing -> Smoothing -> ComponentCheck -> Hierarchical
//!
//! It reads terrain only, so a (from, to) query has one answer per map; the
//! harness compares whole paths against the TypeScript chain.

use crate::map::GameMap;

const LAND_MASK: u8 = 0x80;
const MAGNITUDE_MASK: u8 = 0x1f;

// ── Priority queue (PriorityQueue.ts MinHeap): float32 priorities ──

struct MinHeap {
    heap: Vec<i32>,
    pri: Vec<f32>,
}

impl MinHeap {
    fn new() -> Self {
        MinHeap { heap: Vec::new(), pri: Vec::new() }
    }
    fn clear(&mut self) {
        self.heap.clear();
        self.pri.clear();
    }
    fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }
    fn push(&mut self, node: i32, priority: f64) {
        let mut i = self.heap.len();
        self.heap.push(node);
        self.pri.push(priority as f32);
        while i > 0 {
            let parent = (i - 1) >> 1;
            if self.pri[parent] <= self.pri[i] {
                break;
            }
            self.heap.swap(parent, i);
            self.pri.swap(parent, i);
            i = parent;
        }
    }
    fn pop(&mut self) -> i32 {
        let result = self.heap[0];
        let last = self.heap.len() - 1;
        let (ln, lp) = (self.heap[last], self.pri[last]);
        self.heap.pop();
        self.pri.pop();
        let size = self.heap.len();
        if size > 0 {
            self.heap[0] = ln;
            self.pri[0] = lp;
            let mut i = 0;
            loop {
                let left = (i << 1) + 1;
                let right = left + 1;
                let mut smallest = i;
                if left < size && self.pri[left] < self.pri[smallest] {
                    smallest = left;
                }
                if right < size && self.pri[right] < self.pri[smallest] {
                    smallest = right;
                }
                if smallest == i {
                    break;
                }
                self.heap.swap(smallest, i);
                self.pri.swap(smallest, i);
                i = smallest;
            }
        }
        result
    }
}

// ── ConnectedComponents: water bodies of the minimap ──

pub struct ConnectedComponents {
    ids: Vec<u16>,
    land_marker: u16,
    parents: Vec<u32>,
}

impl ConnectedComponents {
    fn new(map: &GameMap) -> Self {
        let n = map.num_tiles() as usize;
        let w = map.width as usize;
        // Land is pre-marked 0xff; water 0.
        let mut ids: Vec<u16> = (0..n).map(|i| if map.terrain_byte(i as u32) & LAND_MASK != 0 { 0xff } else { 0 }).collect();
        let mut land_marker: u16 = 0xff;
        let mut queue: Vec<usize> = vec![0; n];
        let last_row_start = (map.height as usize - 1) * w;
        let mut next_id: u32 = 0;
        for start in 0..n {
            let v = ids[start];
            if v == land_marker || v > 0 {
                continue;
            }
            next_id += 1;
            if next_id == 253 && land_marker == 0xff {
                for x in ids.iter_mut() {
                    if *x == 0xff {
                        *x = 0xffff;
                    }
                }
                land_marker = 0xffff;
            }
            if next_id == 0xffff {
                break;
            }
            // Scanline flood fill.
            let id = next_id as u16;
            let (mut head, mut tail) = (0, 0);
            queue[tail] = start;
            tail += 1;
            while head < tail {
                let seed = queue[head];
                head += 1;
                if ids[seed] != 0 {
                    continue;
                }
                let row_start = seed - seed % w;
                let mut left = seed;
                while left > row_start && ids[left - 1] == 0 {
                    left -= 1;
                }
                let row_end = row_start + w - 1;
                let mut right = seed;
                while right < row_end && ids[right + 1] == 0 {
                    right += 1;
                }
                for x in left..=right {
                    ids[x] = id;
                    if x >= w && ids[x - w] == 0 {
                        queue[tail] = x - w;
                        tail += 1;
                    }
                    if x < last_row_start && ids[x + w] == 0 {
                        queue[tail] = x + w;
                        tail += 1;
                    }
                }
            }
        }
        let parents = (0..=next_id).collect();
        ConnectedComponents { ids, land_marker, parents }
    }

    pub fn component_id(&self, tile: u32) -> u32 {
        let id = self.ids[tile as usize];
        if id == 0 || id == self.land_marker {
            return id as u32;
        }
        let mut root = id as u32;
        while self.parents[root as usize] != root {
            root = self.parents[root as usize];
        }
        root
    }
}

// ── BFSGrid: 4-direction BFS with a visitor ──

enum Visit<R> {
    Found(R),
    Reject,
    Continue,
}

struct BfsGrid {
    stamp: u32,
    visited: Vec<u32>,
    queue: Vec<u32>,
    dist: Vec<u16>,
}

impl BfsGrid {
    fn new(n: usize) -> Self {
        BfsGrid { stamp: 1, visited: vec![0; n], queue: vec![0; n], dist: vec![0; n] }
    }

    fn search<R>(
        &mut self,
        width: u32,
        height: u32,
        start: u32,
        max_distance: u32,
        is_valid: impl Fn(u32) -> bool,
        mut visitor: impl FnMut(u32, u32) -> Visit<R>,
    ) -> Option<R> {
        let stamp = self.stamp;
        self.stamp += 1;
        if self.stamp == u32::MAX {
            self.visited.fill(0);
            self.stamp = 1;
        }
        let last_row_start = (height - 1) * width;
        let (mut head, mut tail) = (0usize, 0usize);
        self.visited[start as usize] = stamp;
        self.dist[start as usize] = 0;
        self.queue[tail] = start;
        tail += 1;
        while head < tail {
            let node = self.queue[head];
            head += 1;
            let dist = self.dist[node as usize] as u32;
            match visitor(node, dist) {
                Visit::Found(r) => return Some(r),
                Visit::Reject => continue,
                Visit::Continue => {}
            }
            let next = dist + 1;
            if next > max_distance {
                continue;
            }
            let x = node % width;
            let mut try_add = |n: u32, s: &mut Self, tail: &mut usize| {
                if s.visited[n as usize] != stamp && is_valid(n) {
                    s.visited[n as usize] = stamp;
                    s.dist[n as usize] = next as u16;
                    s.queue[*tail] = n;
                    *tail += 1;
                }
            };
            if node >= width {
                try_add(node - width, self, &mut tail);
            }
            if node < last_row_start {
                try_add(node + width, self, &mut tail);
            }
            if x != 0 {
                try_add(node - 1, self, &mut tail);
            }
            if x != width - 1 {
                try_add(node + 1, self, &mut tail);
            }
        }
        None
    }
}

// ── AbstractGraph ──

#[derive(Clone, Copy)]
struct Node {
    x: u32,
    y: u32,
    tile: u32,
    component: u32,
}

#[derive(Clone, Copy)]
struct Edge {
    a: u32,
    b: u32,
    cost: u32,
    cluster_x: u32,
    cluster_y: u32,
}

pub struct AbstractGraph {
    cluster_size: u32,
    clusters_x: u32,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    node_edges: Vec<Vec<u32>>,
    clusters: Vec<Vec<u32>>,
}

impl AbstractGraph {
    fn cluster(&self, cx: i64, cy: i64) -> Option<&Vec<u32>> {
        if cx < 0 || cy < 0 || cx >= self.clusters_x as i64 {
            return None;
        }
        self.clusters.get((cy as u32 * self.clusters_x + cx as u32) as usize)
    }

    fn edge_between(&self, a: u32, b: u32) -> Option<&Edge> {
        self.node_edges[a as usize].iter().map(|&e| &self.edges[e as usize]).find(|e| e.a == b || e.b == b)
    }

    fn build(map: &GameMap, cc: &ConnectedComponents) -> AbstractGraph {
        const CLUSTER_SIZE: u32 = 32;
        let (w, h) = (map.width, map.height);
        let clusters_x = w.div_ceil(CLUSTER_SIZE);
        let clusters_y = h.div_ceil(CLUSTER_SIZE);
        let mut g = AbstractGraph {
            cluster_size: CLUSTER_SIZE,
            clusters_x,
            nodes: Vec::new(),
            edges: Vec::new(),
            node_edges: Vec::new(),
            clusters: vec![Vec::new(); (clusters_x * clusters_y) as usize],
        };
        let mut tile_to_node: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
        let is_water = |t: u32| map.terrain_byte(t) & LAND_MASK == 0;

        let mut get_or_create = |g: &mut AbstractGraph, x: u32, y: u32| -> u32 {
            let tile = y * w + x;
            if let Some(&id) = tile_to_node.get(&tile) {
                return id;
            }
            let id = g.nodes.len() as u32;
            g.nodes.push(Node { x, y, tile, component: cc.component_id(tile) });
            g.node_edges.push(Vec::new());
            tile_to_node.insert(tile, id);
            id
        };
        let add_to_cluster = |g: &mut AbstractGraph, cx: u32, cy: u32, node: u32| {
            let c = &mut g.clusters[(cy * clusters_x + cx) as usize];
            if !c.contains(&node) {
                c.push(node);
            }
        };

        for cy in 0..clusters_y {
            for cx in 0..clusters_x {
                let base_x = cx * CLUSTER_SIZE;
                let base_y = cy * CLUSTER_SIZE;
                if cx < clusters_x - 1 {
                    let edge_x = (base_x + CLUSTER_SIZE - 1).min(w - 1);
                    let max_y = (base_y + CLUSTER_SIZE).min(h);
                    let mut nodes = Vec::new();
                    let mut span_start: i64 = -1;
                    for y in base_y..=max_y {
                        let entrance = y < max_y && {
                            let tile = y * w + edge_x;
                            is_water(tile) && edge_x + 1 < w && is_water(tile + 1)
                        };
                        if entrance {
                            if span_start == -1 {
                                span_start = y as i64;
                            }
                        } else if span_start != -1 {
                            let len = y as i64 - span_start;
                            let mid = span_start + len / 2;
                            span_start = -1;
                            nodes.push(get_or_create(&mut g, edge_x, mid as u32));
                        }
                    }
                    for n in nodes {
                        add_to_cluster(&mut g, cx, cy, n);
                        add_to_cluster(&mut g, cx + 1, cy, n);
                    }
                }
                if cy < clusters_y - 1 {
                    let edge_y = (base_y + CLUSTER_SIZE - 1).min(h - 1);
                    let max_x = (base_x + CLUSTER_SIZE).min(w);
                    let mut nodes = Vec::new();
                    let mut span_start: i64 = -1;
                    for x in base_x..=max_x {
                        let entrance = x < max_x && {
                            let tile = edge_y * w + x;
                            is_water(tile) && edge_y + 1 < h && is_water(tile + w)
                        };
                        if entrance {
                            if span_start == -1 {
                                span_start = x as i64;
                            }
                        } else if span_start != -1 {
                            let len = x as i64 - span_start;
                            let mid = span_start + len / 2;
                            span_start = -1;
                            nodes.push(get_or_create(&mut g, mid as u32, edge_y));
                        }
                    }
                    for n in nodes {
                        add_to_cluster(&mut g, cx, cy, n);
                        add_to_cluster(&mut g, cx, cy + 1, n);
                    }
                }
            }
        }

        // Edges between nodes of a cluster, by bounded BFS.
        let mut bfs = BfsGrid::new(map.num_tiles() as usize);
        let mut edge_between: std::collections::HashMap<(u32, u32), usize> = std::collections::HashMap::new();
        for cy in 0..clusters_y {
            for cx in 0..clusters_x {
                let node_ids = g.clusters[(cy * clusters_x + cx) as usize].clone();
                if node_ids.is_empty() {
                    continue;
                }
                let min_x = cx * CLUSTER_SIZE;
                let min_y = cy * CLUSTER_SIZE;
                let max_x = (w - 1).min(min_x + CLUSTER_SIZE - 1);
                let max_y = (h - 1).min(min_y + CLUSTER_SIZE - 1);
                for i in 0..node_ids.len() {
                    let from = g.nodes[node_ids[i] as usize];
                    let targets: Vec<u32> = node_ids[i + 1..]
                        .iter()
                        .copied()
                        .filter(|&j| g.nodes[j as usize].component == from.component)
                        .collect();
                    if targets.is_empty() {
                        continue;
                    }
                    let mut max_manhattan = 0;
                    for &t in &targets {
                        let n = g.nodes[t as usize];
                        max_manhattan = max_manhattan.max(n.x.abs_diff(from.x) + n.y.abs_diff(from.y));
                    }
                    let tile_to_target: std::collections::HashMap<u32, u32> =
                        targets.iter().map(|&t| (g.nodes[t as usize].tile, t)).collect();
                    // Map insertion order: first time each target is reached.
                    let mut reachable: Vec<(u32, u32)> = Vec::new();
                    let mut found = 0;
                    bfs.search(w, h, from.tile, max_manhattan * 4, is_water, |tile, dist| {
                        let (x, y) = (tile % w, tile / w);
                        let is_target = tile_to_target.get(&tile).copied();
                        if !(tile == from.tile || is_target.is_some()) && (x < min_x || x > max_x || y < min_y || y > max_y) {
                            return Visit::Reject;
                        }
                        if let Some(id) = is_target {
                            match reachable.iter_mut().find(|(n, _)| *n == id) {
                                Some(e) => e.1 = dist,
                                None => reachable.push((id, dist)),
                            }
                            found += 1;
                            if found == targets.len() {
                                return Visit::Found(());
                            }
                        }
                        Visit::Continue
                    });
                    for (target, cost) in reachable {
                        let (lo, hi) = if from_id(node_ids[i]) < target { (node_ids[i], target) } else { (target, node_ids[i]) };
                        match edge_between.get(&(lo, hi)) {
                            Some(&e) => {
                                if cost < g.edges[e].cost {
                                    g.edges[e].cost = cost;
                                    g.edges[e].cluster_x = cx;
                                    g.edges[e].cluster_y = cy;
                                }
                            }
                            None => {
                                edge_between.insert((lo, hi), g.edges.len());
                                g.edges.push(Edge { a: lo, b: hi, cost, cluster_x: cx, cluster_y: cy });
                            }
                        }
                    }
                }
            }
        }
        // _sortEdges: renumber by (a, b) (Array.sort is stable).
        g.edges.sort_by(|x, y| (x.a, x.b).cmp(&(y.a, y.b)));
        for (i, e) in g.edges.iter().enumerate() {
            g.node_edges[e.a as usize].push(i as u32);
            g.node_edges[e.b as usize].push(i as u32);
        }
        g
    }
}

fn from_id(x: u32) -> u32 {
    x
}

// ── AStarWaterBounded ──

pub struct BoundedAStar {
    stamp: u32,
    max_area: usize,
    closed: Vec<u32>,
    g_stamp: Vec<u32>,
    g: Vec<u32>,
    came_from: Vec<i32>,
    queue: MinHeap,
}

#[derive(Clone, Copy)]
pub struct Bounds {
    pub min_x: i64,
    pub max_x: i64,
    pub min_y: i64,
    pub max_y: i64,
}

fn magnitude_penalty(m: u8) -> u32 {
    if m < 3 {
        300
    } else if m <= 10 {
        0
    } else {
        100
    }
}

impl BoundedAStar {
    pub fn new(max_area: usize) -> Self {
        BoundedAStar {
            stamp: 1,
            max_area,
            closed: vec![0; max_area],
            g_stamp: vec![0; max_area],
            g: vec![0; max_area],
            came_from: vec![0; max_area],
            queue: MinHeap::new(),
        }
    }

    pub fn search(&mut self, map: &GameMap, starts: &[u32], goal: u32, b: Bounds) -> Option<Vec<u32>> {
        self.stamp += 1;
        if self.stamp == u32::MAX {
            self.closed.fill(0);
            self.g_stamp.fill(0);
            self.stamp = 1;
        }
        let stamp = self.stamp;
        let w = map.width as i64;
        let bw = b.max_x - b.min_x + 1;
        let bh = b.max_y - b.min_y + 1;
        let num_local = bw * bh;
        if num_local > self.max_area as i64 {
            return None;
        }
        let goal_x = goal as i64 % w;
        let goal_y = goal as i64 / w;
        let to_local = |tile: u32| -> i64 {
            let x = (tile as i64 % w).clamp(b.min_x, b.max_x);
            let y = (tile as i64 / w).clamp(b.min_y, b.max_y);
            (y - b.min_y) * bw + (x - b.min_x)
        };
        let to_global = |local: i64| -> i64 { ((local / bw) + b.min_y) * w + (local % bw + b.min_x) };
        let goal_local = to_local(goal);
        if goal_local < 0 || goal_local >= num_local {
            return None;
        }
        self.queue.clear();
        let s0 = starts[0] as i64;
        let (start_x, start_y) = (s0 % w, s0 / w);
        let dx_goal = goal_x - start_x;
        let dy_goal = goal_y - start_y;
        let cross_norm = 1.max(dx_goal.abs() + dy_goal.abs()) as f64;
        let cross = |nx: i64, ny: i64| -> f64 {
            let c = (dx_goal * (ny - goal_y) - dy_goal * (nx - goal_x)).abs() as f64;
            ((c * 99.0) / cross_norm / cross_norm).floor()
        };
        for &s in starts {
            let sl = to_local(s);
            if sl < 0 || sl >= num_local {
                continue;
            }
            let sl = sl as usize;
            self.g[sl] = 0;
            self.g_stamp[sl] = stamp;
            self.came_from[sl] = -1;
            let (sx, sy) = (s as i64 % w, s as i64 / w);
            self.queue.push(sl as i32, 3.0 * 100.0 * ((sx - goal_x).abs() + (sy - goal_y).abs()) as f64);
        }
        let mut iterations: i64 = 100_000;
        while !self.queue.is_empty() {
            iterations -= 1;
            if iterations <= 0 {
                return None;
            }
            let cur_local = self.queue.pop() as i64;
            let cl = cur_local as usize;
            if self.closed[cl] == stamp {
                continue;
            }
            self.closed[cl] = stamp;
            if cur_local == goal_local {
                let mut path = Vec::new();
                let mut c = goal_local;
                let mut it = 0;
                while c != -1 && it < num_local {
                    path.push(to_global(c) as u32);
                    c = self.came_from[c as usize] as i64;
                    it += 1;
                }
                path.reverse();
                return Some(path);
            }
            let cur_g = self.g[cl];
            let current = to_global(cur_local);
            let (cx, cy) = (current % w, current / w);
            // N, S, W, E
            let dirs: [(bool, i64, i64, i64, i64); 4] = [
                (cy > b.min_y, current - w, cur_local - bw, cx, cy - 1),
                (cy < b.max_y, current + w, cur_local + bw, cx, cy + 1),
                (cx > b.min_x, current - 1, cur_local - 1, cx - 1, cy),
                (cx < b.max_x, current + 1, cur_local + 1, cx + 1, cy),
            ];
            for (ok, neighbor, nl, nx, ny) in dirs {
                if !ok {
                    continue;
                }
                let nlu = nl as usize;
                let terrain = map.terrain_byte(neighbor as u32);
                if self.closed[nlu] == stamp || !(neighbor == goal as i64 || terrain & LAND_MASK == 0) {
                    continue;
                }
                let dist_to_goal = (nx - goal_x).abs() + (ny - goal_y).abs();
                let cost = 100 + magnitude_penalty(terrain & MAGNITUDE_MASK);
                let tentative = cur_g + cost;
                if self.g_stamp[nlu] != stamp || tentative < self.g[nlu] {
                    self.came_from[nlu] = cur_local as i32;
                    self.g[nlu] = tentative;
                    self.g_stamp[nlu] = stamp;
                    let h = 3.0 * 100.0 * dist_to_goal as f64;
                    self.queue.push(nl as i32, tentative as f64 + h + cross(nx, ny));
                }
            }
        }
        None
    }
}

// ── AbstractGraphAStar ──

struct GraphAStar {
    stamp: u32,
    closed: Vec<u32>,
    g_stamp: Vec<u32>,
    g: Vec<f32>,
    came_from: Vec<i32>,
    start_node: Vec<i32>,
    queue: MinHeap,
}

impl GraphAStar {
    fn new(n: usize) -> Self {
        GraphAStar {
            stamp: 1,
            closed: vec![0; n],
            g_stamp: vec![0; n],
            g: vec![0.0; n],
            came_from: vec![0; n],
            start_node: vec![0; n],
            queue: MinHeap::new(),
        }
    }

    fn find(&mut self, graph: &AbstractGraph, starts: &[u32], goal: u32) -> Option<Vec<u32>> {
        if starts.is_empty() {
            return None;
        }
        self.stamp += 1;
        if self.stamp == u32::MAX {
            self.closed.fill(0);
            self.g_stamp.fill(0);
            self.stamp = 1;
        }
        let stamp = self.stamp;
        let goal_node = graph.nodes[goal as usize];
        self.queue.clear();
        for &s in starts {
            let n = graph.nodes[s as usize];
            self.g[s as usize] = 0.0;
            self.g_stamp[s as usize] = stamp;
            self.came_from[s as usize] = -1;
            self.start_node[s as usize] = s as i32;
            let h = (n.x.abs_diff(goal_node.x) + n.y.abs_diff(goal_node.y)) as f64;
            self.queue.push(s as i32, h);
        }
        let mut iterations: i64 = 100_000;
        while !self.queue.is_empty() {
            iterations -= 1;
            if iterations <= 0 {
                return None;
            }
            let current = self.queue.pop() as usize;
            if self.closed[current] == stamp {
                continue;
            }
            self.closed[current] = stamp;
            if current as u32 == goal {
                let mut path = Vec::new();
                let mut c = goal as i32;
                while c != -1 {
                    path.push(c as u32);
                    c = self.came_from[c as usize];
                }
                path.reverse();
                return Some(path);
            }
            let cur_g = self.g[current] as f64;
            let cur_start = self.start_node[current];
            for &e in &graph.node_edges[current] {
                let edge = graph.edges[e as usize];
                let neighbor = if edge.a == current as u32 { edge.b } else { edge.a } as usize;
                if self.closed[neighbor] == stamp {
                    continue;
                }
                let tentative = cur_g + edge.cost as f64;
                if self.g_stamp[neighbor] != stamp || tentative < self.g[neighbor] as f64 {
                    self.came_from[neighbor] = current as i32;
                    self.g[neighbor] = tentative as f32;
                    self.g_stamp[neighbor] = stamp;
                    self.start_node[neighbor] = cur_start;
                    let n = graph.nodes[neighbor];
                    let h = (n.x.abs_diff(goal_node.x) + n.y.abs_diff(goal_node.y)) as f64;
                    self.queue.push(neighbor as i32, tentative + h);
                }
            }
        }
        None
    }
}

// ── The chain ──

pub struct WaterPathfinder {
    pub mini: GameMap,
    cc: ConnectedComponents,
    graph: AbstractGraph,
    tile_bfs: BfsGrid,
    graph_astar: GraphAStar,
    local: BoundedAStar,
    local_multi: BoundedAStar,
    local_short: BoundedAStar,
    smooth_astar: BoundedAStar,
}

impl WaterPathfinder {
    pub fn new(mini: GameMap) -> Self {
        let cc = ConnectedComponents::new(&mini);
        let graph = AbstractGraph::build(&mini, &cc);
        assert!(graph.nodes.len() >= 100, "small water graphs use AStarWater, not ported yet");
        let n = mini.num_tiles() as usize;
        let nodes = graph.nodes.len();
        WaterPathfinder {
            cc,
            tile_bfs: BfsGrid::new(n),
            graph_astar: GraphAStar::new(nodes),
            local: BoundedAStar::new(32 * 32),
            local_multi: BoundedAStar::new(96 * 96),
            local_short: BoundedAStar::new(260 * 260),
            smooth_astar: BoundedAStar::new(100 * 100),
            graph,
            mini,
        }
    }

    /// Water component of a minimap tile.
    pub fn component_id(&self, t: u32) -> u32 {
        self.cc.component_id(t)
    }

    fn mini_is_water(&self, t: u32) -> bool {
        self.mini.terrain_byte(t) & LAND_MASK == 0
    }

    /// The whole chain on full-map tiles (MiniMapTransformer outermost).
    pub fn find_path(&mut self, map: &GameMap, from: &[u32], to: u32) -> Option<Vec<u32>> {
        let _p = crate::prof::scope("water_find_path");
        let mini_ref = |t: u32| (map.y(t) / 2) * self.mini.width + map.x(t) / 2;
        let mini_from: Vec<u32> = from.iter().map(|&f| mini_ref(f)).collect();
        let mini_to = mini_ref(to);
        let path = self.shore_coercing(&mini_from, mini_to)?;
        if path.is_empty() {
            return None;
        }
        // upscalePath
        let mw = self.mini.width;
        let mut up: Vec<u32> = Vec::new();
        for i in 0..path.len() - 1 {
            let (cx, cy) = ((path[i] % mw) as f64 * 2.0, (path[i] / mw) as f64 * 2.0);
            let (nx, ny) = ((path[i + 1] % mw) as f64 * 2.0, (path[i + 1] / mw) as f64 * 2.0);
            up.push(map.ref_at(cx as u32, cy as u32));
            let (dx, dy) = (nx - cx, ny - cy);
            let steps = dx.abs().max(dy.abs()) as i64;
            for step in 1..steps {
                let x = js_round(cx + (dx * step as f64) / steps as f64);
                let y = js_round(cy + (dy * step as f64) / steps as f64);
                up.push(map.ref_at(x as u32, y as u32));
            }
        }
        let last = path[path.len() - 1];
        up.push(map.ref_at((last % mw) * 2, (last / mw) * 2));
        // The source: the given tile, or the one nearest the path's start.
        let src = if from.len() > 1 {
            let (sx, sy) = (map.x(up[0]) as i64, map.y(up[0]) as i64);
            let mut best = None;
            let mut min = f64::INFINITY;
            for &f in from {
                let d = ((map.x(f) as i64 - sx).abs() + (map.y(f) as i64 - sy).abs()) as f64;
                if d < min {
                    min = d;
                    best = Some(f);
                }
            }
            best
        } else {
            Some(from[0])
        };
        // fixExtremes
        if let Some(src) = src {
            match up.iter().position(|&t| t == src) {
                None => up.insert(0, src),
                Some(0) => {}
                Some(i) => up = up[i..].to_vec(),
            }
        }
        match up.iter().position(|&t| t == to) {
            None => up.push(to),
            Some(i) if i != up.len() - 1 => up.truncate(i + 1),
            _ => {}
        }
        Some(up)
    }

    fn best_water_neighbor(&self, tile: u32) -> Option<u32> {
        let mut best = None;
        let mut max_score: i32 = -1;
        let mut nb = [0u32; 4];
        let n = self.mini.neighbors4(tile, &mut nb);
        for &t in &nb[..n] {
            if !self.mini_is_water(t) {
                continue;
            }
            let mut nb2 = [0u32; 4];
            let n2 = self.mini.neighbors4(t, &mut nb2);
            let score = nb2[..n2].iter().filter(|&&x| self.mini_is_water(x)).count() as i32;
            if score > max_score {
                max_score = score;
                best = Some(t);
            }
        }
        best
    }

    fn shore_coercing(&mut self, from: &[u32], to: u32) -> Option<Vec<u32>> {
        // Map from water tile to the land tile it stands in for (Map semantics).
        let mut water_to_original: Vec<(u32, u32)> = Vec::new();
        let mut water_from: Vec<u32> = Vec::new();
        for &f in from {
            if self.mini_is_water(f) {
                water_from.push(f);
                water_to_original.retain(|(w, _)| *w != f);
            } else if let Some(w) = self.best_water_neighbor(f) {
                water_from.push(w);
                match water_to_original.iter_mut().find(|(x, _)| *x == w) {
                    Some(e) => e.1 = f,
                    None => water_to_original.push((w, f)),
                }
            }
        }
        if water_from.is_empty() {
            return None;
        }
        let mut water_to = to;
        let mut original_to = None;
        if !self.mini_is_water(to) {
            water_to = self.best_water_neighbor(to)?;
            original_to = Some(to);
        }
        let mut path = self.smoothing(&water_from, water_to)?;
        if path.is_empty() {
            return None;
        }
        if let Some(&(_, orig)) = water_to_original.iter().find(|(w, _)| *w == path[0]) {
            path.insert(0, orig);
        }
        if let Some(o) = original_to {
            if *path.last().unwrap() != o {
                path.push(o);
            }
        }
        Some(path)
    }

    fn smoothing(&mut self, from: &[u32], to: u32) -> Option<Vec<u32>> {
        let path = self.component_check(from, to)?;
        if path.len() <= 2 {
            return Some(path);
        }
        let s = self.los_smooth(&path, 2);
        let s = self.refine_endpoints(s);
        Some(self.los_smooth(&s, 3))
    }

    fn component_check(&mut self, from: &[u32], to: u32) -> Option<Vec<u32>> {
        let to_c = self.cc.component_id(to);
        let valid: Vec<u32> = from.iter().copied().filter(|&f| self.cc.component_id(f) == to_c).collect();
        if valid.is_empty() {
            return None;
        }
        if valid.len() == 1 {
            self.hpa_single(valid[0], to)
        } else {
            self.hpa_multi(&valid, to)
        }
    }

    // ── SmoothingWaterTransformer ──

    fn los_smooth(&self, path: &[u32], min_mag: u8) -> Vec<u32> {
        let mut result = vec![path[0]];
        let mut current = 0;
        while current < path.len() - 1 {
            let (mut lo, mut hi) = (current as i64 + 1, path.len() as i64 - 1);
            let mut farthest = lo;
            while lo <= hi {
                let mid = (lo + hi) >> 1;
                if self.can_see(path[current], path[mid as usize], min_mag) {
                    farthest = mid;
                    lo = mid + 1;
                } else {
                    hi = mid - 1;
                }
            }
            let farthest = farthest as usize;
            if farthest > current + 1 {
                if let Some(trace) = self.trace_path(path[current], path[farthest]) {
                    for i in 1..trace.len().saturating_sub(1) {
                        result.push(trace[i]);
                    }
                }
            }
            current = farthest;
            if current < path.len() - 1 {
                result.push(path[current]);
            }
        }
        result.push(path[path.len() - 1]);
        result
    }

    fn passes(&self, t: i64, min_mag: u8) -> bool {
        let b = self.mini.terrain_byte(t as u32);
        b & LAND_MASK == 0 && b & MAGNITUDE_MASK >= min_mag
    }

    fn can_see(&self, from: u32, to: u32, min_mag: u8) -> bool {
        let w = self.mini.width as i64;
        let (x0, y0, x1, y1) = (from as i64 % w, from as i64 / w, to as i64 % w, to as i64 / w);
        let (dx, dy) = ((x1 - x0).abs(), (y1 - y0).abs());
        let (sx, sy) = (if x0 < x1 { 1 } else { -1 }, if y0 < y1 { 1 } else { -1 });
        let mut err = dx - dy;
        let (mut x, mut y) = (x0, y0);
        let mut iterations = 0;
        loop {
            iterations += 1;
            if iterations > 100_001 {
                return false;
            }
            if !self.passes(y * w + x, min_mag) {
                return false;
            }
            if x == x1 && y == y1 {
                return true;
            }
            let e2 = 2 * err;
            let move_x = e2 > -dy;
            let move_y = e2 < dx;
            if move_x && move_y {
                x += sx;
                err -= dy;
                if !self.passes(y * w + x, min_mag) {
                    x -= sx;
                    err += dy;
                    y += sy;
                    err += dx;
                    if !self.passes(y * w + x, min_mag) {
                        return false;
                    }
                    x += sx;
                    err -= dy;
                } else {
                    y += sy;
                    err += dx;
                }
            } else {
                if move_x {
                    x += sx;
                    err -= dy;
                }
                if move_y {
                    y += sy;
                    err += dx;
                }
            }
        }
    }

    fn trace_path(&self, from: u32, to: u32) -> Option<Vec<u32>> {
        let w = self.mini.width as i64;
        let water = |t: i64| self.mini_is_water(t as u32);
        let (x0, y0, x1, y1) = (from as i64 % w, from as i64 / w, to as i64 % w, to as i64 / w);
        let (dx, dy) = ((x1 - x0).abs(), (y1 - y0).abs());
        let (sx, sy) = (if x0 < x1 { 1 } else { -1 }, if y0 < y1 { 1 } else { -1 });
        let mut err = dx - dy;
        let (mut x, mut y) = (x0, y0);
        let mut tiles = Vec::new();
        let mut iterations = 0;
        loop {
            iterations += 1;
            if iterations > 100_001 {
                return None;
            }
            let tile = y * w + x;
            if !water(tile) {
                return None;
            }
            tiles.push(tile as u32);
            if x == x1 && y == y1 {
                break;
            }
            let e2 = 2 * err;
            let move_x = e2 > -dy;
            let move_y = e2 < dx;
            if move_x && move_y {
                x += sx;
                err -= dy;
                let inter = y * w + x;
                if !water(inter) {
                    x -= sx;
                    err += dy;
                    y += sy;
                    err += dx;
                    let alt = y * w + x;
                    if !water(alt) {
                        return None;
                    }
                    tiles.push(alt as u32);
                    x += sx;
                    err -= dy;
                } else {
                    tiles.push(inter as u32);
                    y += sy;
                    err += dx;
                }
            } else {
                if move_x {
                    x += sx;
                    err -= dy;
                }
                if move_y {
                    y += sy;
                    err += dx;
                }
            }
        }
        Some(tiles)
    }

    fn mini_manhattan(&self, a: u32, b: u32) -> u32 {
        self.mini.manhattan_dist(a, b)
    }

    fn tile_at_distance(&self, path: &[u32], start: usize, distance: u32, forward: bool) -> usize {
        let mut cum = 0;
        let mut idx = start;
        if forward {
            while idx < path.len() - 1 && cum < distance {
                cum += self.mini_manhattan(path[idx], path[idx + 1]);
                idx += 1;
            }
        } else {
            while idx > 0 && cum < distance {
                cum += self.mini_manhattan(path[idx], path[idx - 1]);
                idx -= 1;
            }
        }
        idx
    }

    fn refine_segment(&mut self, from: u32, to: u32) -> Option<Vec<u32>> {
        let m = &self.mini;
        let (x0, y0, x1, y1) = (m.x(from) as i64, m.y(from) as i64, m.x(to) as i64, m.y(to) as i64);
        let b = Bounds {
            min_x: 0.max(x0.min(x1) - 10),
            max_x: (m.width as i64 - 1).min(x0.max(x1) + 10),
            min_y: 0.max(y0.min(y1) - 10),
            max_y: (m.height as i64 - 1).min(y0.max(y1) + 10),
        };
        self.smooth_astar.search(&self.mini, &[from], to, b)
    }

    fn refine_endpoints(&mut self, path: Vec<u32>) -> Vec<u32> {
        if path.len() <= 2 {
            return path;
        }
        let mut result = path.clone();
        let start_end = self.tile_at_distance(&path, 0, 50, true);
        if start_end > 1 {
            if let Some(seg) = self.refine_segment(path[0], path[start_end]) {
                if !seg.is_empty() {
                    let mut r = seg[..seg.len() - 1].to_vec();
                    r.extend_from_slice(&result[start_end..]);
                    result = r;
                }
            }
        }
        let end_start = self.tile_at_distance(&result, result.len() - 1, 50, false);
        if (end_start as i64) < result.len() as i64 - 2 {
            if let Some(mut seg) = self.refine_segment(result[result.len() - 1], result[end_start]) {
                if !seg.is_empty() {
                    seg.reverse();
                    let mut r = result[..end_start].to_vec();
                    r.extend(seg);
                    result = r;
                }
            }
        }
        result
    }

    // ── AStarWaterHierarchical ──

    fn cluster_of(&self, t: u32) -> (u32, u32) {
        (self.mini.x(t) / self.graph.cluster_size, self.mini.y(t) / self.graph.cluster_size)
    }

    fn hpa_multi(&mut self, sources: &[u32], target: u32) -> Option<Vec<u32>> {
        // tryShortPathMultiSource
        let candidates: Vec<u32> = sources.iter().copied().filter(|&s| self.mini.manhattan_dist(s, target) <= 120).collect();
        if !candidates.is_empty() {
            let m = &self.mini;
            let (tx, ty) = (m.x(target) as i64, m.y(target) as i64);
            let (mut min_x, mut max_x, mut min_y, mut max_y) = (tx, tx, ty, ty);
            for &s in &candidates {
                let (sx, sy) = (m.x(s) as i64, m.y(s) as i64);
                min_x = min_x.min(sx);
                max_x = max_x.max(sx);
                min_y = min_y.min(sy);
                max_y = max_y.max(sy);
            }
            let b = Bounds {
                min_x: 0.max(min_x - 10),
                max_x: (m.width as i64 - 1).min(max_x + 10),
                min_y: 0.max(min_y - 10),
                max_y: (m.height as i64 - 1).min(max_y + 10),
            };
            if let Some(p) = self.local_short.search(&self.mini, &candidates, target, b) {
                return Some(p);
            }
        }
        let target_node = self.closest_cluster_node(target)?;
        // resolveSourcesToNodes: Map<node, source>, closest source per node.
        let mut node_to_source: Vec<(u32, u32, u32)> = Vec::new(); // (node, source, dist)
        for &s in sources {
            let Some(node) = self.closest_cluster_node(s) else { continue };
            let n = self.graph.nodes[node as usize];
            let d = n.x.abs_diff(self.mini.x(s)) + n.y.abs_diff(self.mini.y(s));
            match node_to_source.iter_mut().find(|e| e.0 == node) {
                Some(e) => {
                    if d < e.2 {
                        e.1 = s;
                        e.2 = d;
                    }
                }
                None => node_to_source.push((node, s, d)),
            }
        }
        if node_to_source.is_empty() {
            return None;
        }
        let ids: Vec<u32> = node_to_source.iter().map(|e| e.0).collect();
        let node_path = self.graph_astar.find(&self.graph, &ids, target_node)?;
        let winner = node_to_source.iter().find(|e| e.0 == node_path[0]).unwrap().1;
        self.hpa_single(winner, target)
    }

    fn closest_cluster_node(&self, tile: u32) -> Option<u32> {
        let (cx, cy) = self.cluster_of(tile);
        let cluster = self.graph.cluster(cx as i64, cy as i64)?;
        let (x, y) = (self.mini.x(tile), self.mini.y(tile));
        let mut best = None;
        let mut best_d = u32::MAX;
        for &id in cluster {
            let n = self.graph.nodes[id as usize];
            let d = n.x.abs_diff(x) + n.y.abs_diff(y);
            if d < best_d {
                best_d = d;
                best = Some(id);
            }
        }
        best
    }

    fn hpa_single(&mut self, from: u32, to: u32) -> Option<Vec<u32>> {
        let cs = self.graph.cluster_size;
        if self.mini.manhattan_dist(from, to) <= cs {
            let (cx, cy) = self.cluster_of(from);
            if let Some(p) = self.local_path(from, to, cx, cy, true) {
                return Some(p);
            }
        }
        let start = self.nearest_node(from)?;
        let end = self.nearest_node(to)?;
        if start == end {
            let n = self.graph.nodes[start as usize];
            return self.local_path(from, to, n.x / cs, n.y / cs, true);
        }
        let node_path = self.graph_astar.find(&self.graph, &[start], end)?;
        let first = self.graph.nodes[node_path[0] as usize].tile;
        let (sx, sy) = self.cluster_of(from);
        let mut path = self.local_path(from, first, sx, sy, false)?;
        for i in 0..node_path.len() - 1 {
            let edge = *self.graph.edge_between(node_path[i], node_path[i + 1])?;
            let a = self.graph.nodes[node_path[i] as usize].tile;
            let b = self.graph.nodes[node_path[i + 1] as usize].tile;
            // The engine caches these per edge and direction; the local
            // search is deterministic, so recomputing gives the same tiles.
            let seg = self.local_path(a, b, edge.cluster_x, edge.cluster_y, false)?;
            path.extend_from_slice(&seg[1..]);
        }
        let last = self.graph.nodes[*node_path.last().unwrap() as usize].tile;
        let (ex, ey) = self.cluster_of(to);
        let end_seg = self.local_path(last, to, ex, ey, false)?;
        path.extend_from_slice(&end_seg[1..]);
        Some(path)
    }

    fn nearest_node(&mut self, tile: u32) -> Option<u32> {
        let cs = self.graph.cluster_size;
        let (cx, cy) = self.cluster_of(tile);
        let (min_x, min_y) = (cx * cs, cy * cs);
        let max_x = (self.mini.width - 1).min(min_x + cs - 1);
        let max_y = (self.mini.height - 1).min(min_y + cs - 1);
        let cluster = self.graph.cluster(cx as i64, cy as i64)?;
        if cluster.is_empty() {
            return None;
        }
        let candidates: Vec<Node> = cluster.iter().map(|&id| self.graph.nodes[id as usize]).collect();
        let ids: Vec<u32> = cluster.clone();
        let mini = &self.mini;
        let (w, h) = (mini.width, mini.height);
        self.tile_bfs.search(w, h, tile, cs * cs, |t| mini.terrain_byte(t) & LAND_MASK == 0, |t, _| {
            let (tx, ty) = (t % w, t / w);
            for (i, n) in candidates.iter().enumerate() {
                if n.x == tx && n.y == ty {
                    return Visit::Found(ids[i]);
                }
            }
            if tx < min_x || tx > max_x || ty < min_y || ty > max_y {
                return Visit::Reject;
            }
            Visit::Continue
        })
    }

    fn local_path(&mut self, from: u32, to: u32, cx: u32, cy: u32, multi: bool) -> Option<Vec<u32>> {
        let cs = self.graph.cluster_size as i64;
        let (cx, cy) = (cx as i64, cy as i64);
        let (w, h) = (self.mini.width as i64, self.mini.height as i64);
        let b = if multi {
            Bounds {
                min_x: 0.max((cx - 1) * cs),
                min_y: 0.max((cy - 1) * cs),
                max_x: (w - 1).min((cx + 2) * cs - 1),
                max_y: (h - 1).min((cy + 2) * cs - 1),
            }
        } else {
            Bounds {
                min_x: cx * cs,
                min_y: cy * cs,
                max_x: (w - 1).min(cx * cs + cs - 1),
                max_y: (h - 1).min(cy * cs + cs - 1),
            }
        };
        let astar = if multi { &mut self.local_multi } else { &mut self.local };
        let mut path = astar.search(&self.mini, &[from], to, b)?;
        if path.is_empty() {
            return None;
        }
        if path[0] != from {
            path.insert(0, from);
        }
        if *path.last().unwrap() != to {
            path.push(to);
        }
        Some(path)
    }
}

/// Math.round: halves round up.
fn js_round(x: f64) -> f64 {
    (x + 0.5).floor()
}
