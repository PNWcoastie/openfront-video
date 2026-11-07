use super::*;

// ── Severed land (PlayerExecution.severedLand and friends): land a nuke's
// fallout cuts off from the main body goes to the enemy around it. ──

#[derive(Default, Clone)]
pub struct Candidates {
    pub(super) clusters: Vec<Vec<u32>>,
    pub(super) boxes: Vec<i64>,
}

#[derive(Default)]
pub(super) struct TerritoryParts {
    clusters: Vec<Vec<u32>>,
    boxes: Vec<i64>,
    classes: Vec<u8>,
}

#[derive(Default)]
struct RacePieces {
    clusters: Vec<Vec<u32>>,
    boxes: Vec<i64>,
    open: Vec<bool>,
}

fn whole_cluster(cluster: &[u32], b: &[i64], cls: u8) -> TerritoryParts {
    TerritoryParts { clusters: vec![cluster.to_vec()], boxes: b[..4].to_vec(), classes: vec![cls] }
}

fn add_race_pieces(pieces: &mut RacePieces, parts: &TerritoryParts, all: bool, with_open: bool) {
    for k in 0..parts.clusters.len() {
        let cls = parts.classes[k];
        let open = cls & CLUSTER_OPEN != 0;
        if !all && cls & CLUSTER_SEVERED_CANDIDATE == 0 && !(with_open && open) {
            continue;
        }
        pieces.clusters.push(parts.clusters[k].clone());
        pieces.open.push(open);
        pieces.boxes.extend_from_slice(&parts.boxes[k * 4..k * 4 + 4]);
    }
}

fn root_of(roots: &[usize], mut part: usize) -> usize {
    while roots[part] != part {
        part = roots[part];
    }
    part
}

impl Game {
    pub(super) fn severed_land(
        &mut self,
        p: usize,
        largest_cluster: &[u32],
        lbox: [i64; 4],
        severed: Option<&Candidates>,
        open_clusters: Option<&Candidates>,
    ) -> Option<(Vec<Vec<u32>>, Candidates)> {
        if self.map.num_tiles_with_fallout() == 0 {
            return None;
        }
        let largest = match self.split_cluster(p, largest_cluster, severed.is_none()) {
            Some(parts) => parts,
            None => {
                if severed.is_none() && open_clusters.is_none() {
                    return None;
                }
                if severed.is_none() {
                    let mut nb = [0u32; 4];
                    let has_fallout = largest_cluster.iter().any(|&t| {
                        let n = self.map.neighbors4(t, &mut nb);
                        nb[..n].iter().any(|&x| self.map.has_fallout(x) && self.map.owner_id(x) == 0)
                    }) || open_clusters.is_some_and(|op| {
                        op.clusters.iter().any(|cl| {
                            cl.iter().any(|&t| {
                                let n = self.map.neighbors4(t, &mut nb);
                                nb[..n].iter().any(|&x| self.map.has_fallout(x) && self.map.owner_id(x) == 0)
                            })
                        })
                    });
                    if !has_fallout {
                        return None;
                    }
                }
                let cls = self.classify_cluster(p, largest_cluster, lbox);
                whole_cluster(largest_cluster, &lbox, cls)
            }
        };
        let add_open = !largest.classes.iter().any(|&c| c & CLUSTER_OPEN != 0)
            && largest.classes.iter().any(|&c| c & CLUSTER_SEVERED_CANDIDATE != 0);
        let mut pieces = RacePieces::default();
        add_race_pieces(&mut pieces, &largest, true, true);
        let largest_parts = pieces.clusters.len();
        if let Some(sev) = severed {
            for c in 0..sev.clusters.len() {
                let parts = self
                    .split_cluster(p, &sev.clusters[c], false)
                    .unwrap_or_else(|| whole_cluster(&sev.clusters[c], &sev.boxes[c * 4..], CLUSTER_SEVERED_CANDIDATE));
                add_race_pieces(&mut pieces, &parts, false, false);
            }
        }
        if let Some(op) = open_clusters {
            for c in 0..op.clusters.len() {
                match self.split_cluster(p, &op.clusters[c], !add_open) {
                    Some(split) => add_race_pieces(&mut pieces, &split, false, add_open),
                    None => {
                        if add_open {
                            let whole = whole_cluster(&op.clusters[c], &op.boxes[c * 4..], CLUSTER_OPEN);
                            add_race_pieces(&mut pieces, &whole, false, true);
                        }
                    }
                }
            }
        }
        if pieces.clusters.len() < 2 || !pieces.open.contains(&false) {
            return None;
        }
        let count = pieces.clusters.len();
        let mut roots = vec![0usize; count];
        let main = self.largest_territory_part(p, &pieces.clusters, &mut roots, Some(&pieces.boxes))?;
        let mut main_tiles = Vec::new();
        let mut candidates = Candidates::default();
        for k in 0..count {
            let i = (k + largest_parts) % count;
            if root_of(&roots, i) == main {
                main_tiles.push(pieces.clusters[i].clone());
                continue;
            }
            if pieces.open[i] {
                continue;
            }
            candidates.clusters.push(pieces.clusters[i].clone());
            candidates.boxes.extend_from_slice(&pieces.boxes[i * 4..i * 4 + 4]);
        }
        if candidates.clusters.is_empty() {
            None
        } else {
            Some((main_tiles, candidates))
        }
    }

    pub(super) fn split_cluster(&mut self, p: usize, cluster: &[u32], candidates_only: bool) -> Option<TerritoryParts> {
        let me = self.players[p].small_id;
        let (w, h) = (self.map.width, self.map.height);
        if cluster.is_empty() || self.map.owner_id(cluster[0]) != me {
            return None;
        }
        let mut pinched = false;
        for &t in cluster {
            if pinched {
                break;
            }
            if t >= (h - 1) * w {
                continue;
            }
            let below = t + w;
            if self.map.owner_id(below) == me {
                continue;
            }
            let x = t % w;
            pinched = (x < w - 1 && self.map.owner_id(below + 1) == me && self.map.owner_id(t + 1) != me)
                || (x > 0 && self.map.owner_id(below - 1) == me && self.map.owner_id(t - 1) != me);
        }
        if !pinched {
            return None;
        }
        if candidates_only {
            let mut nb = [0u32; 4];
            let fallout = cluster.iter().any(|&t| {
                let n = self.map.neighbors4(t, &mut nb);
                nb[..n].iter().any(|&x| self.map.owner_id(x) == 0 && self.map.has_fallout(x))
            });
            if !fallout {
                return None;
            }
        }
        let member = self.bump_generations(2);
        let done = member + 1;
        for &t in cluster {
            self.visited[t as usize] = member;
        }
        let mut parts = TerritoryParts::default();
        let mut nb8 = [0u32; 8];
        for &start in cluster {
            if self.visited[start as usize] != member || self.map.owner_id(start) != me {
                continue;
            }
            self.visited[start as usize] = done;
            let mut stack = vec![start];
            let mut part = Vec::new();
            let (mut touches_open, mut has_enemy, mut has_fallout) = (false, false, false);
            let (mut min_x, mut min_y, mut max_x, mut max_y) = (w as i64, h as i64, -1i64, -1i64);
            while let Some(t) = stack.pop() {
                part.push(t);
                let (x, y) = ((t % w) as i64, (t / w) as i64);
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
                if self.map.is_shore(t) || self.map.is_on_edge_of_map(t) {
                    touches_open = true;
                }
                let n = self.map.neighbors8(t, &mut nb8);
                for &nn in &nb8[..n] {
                    let owner = self.map.owner_id(nn);
                    let dx = (nn % w) as i64 - x;
                    let diagonal = dx != 0 && (nn as i64 - dx) != t as i64;
                    if !diagonal && owner != me {
                        if owner != 0 {
                            has_enemy = true;
                        } else if self.map.has_fallout(nn) {
                            has_fallout = true;
                        } else {
                            touches_open = true;
                        }
                    }
                    if self.visited[nn as usize] != member || owner != me {
                        continue;
                    }
                    if diagonal
                        && self.map.owner_id((t as i64 + dx) as u32) != me
                        && self.map.owner_id((nn as i64 - dx) as u32) != me
                    {
                        continue;
                    }
                    self.visited[nn as usize] = done;
                    stack.push(nn);
                }
            }
            parts.clusters.push(part);
            parts.boxes.extend_from_slice(&[min_x, min_y, max_x, max_y]);
            parts.classes.push(if touches_open {
                CLUSTER_OPEN
            } else if has_enemy && has_fallout {
                CLUSTER_SEVERED_CANDIDATE
            } else {
                0
            });
        }
        if parts.clusters.len() < 2 {
            None
        } else {
            Some(parts)
        }
    }

    pub(super) fn largest_territory_part(&mut self, p: usize, parts: &[Vec<u32>], roots: &mut [usize], boxes: Option<&[i64]>) -> Option<usize> {
        let me = self.players[p].small_id;
        let (w, h) = (self.map.width as i64, self.map.height as i64);
        let count = parts.len();
        let base = self.bump_generations(count as u32);
        let mut bounds: Option<Vec<i64>> = boxes.map(|b| b[..count * 4].to_vec());
        if let Some(bd) = bounds.as_mut() {
            for b in (0..bd.len()).step_by(4) {
                if bd[b] == 0 || bd[b + 1] == 0 || bd[b + 2] == w - 1 || bd[b + 3] == h - 1 {
                    bd[b] = 0;
                    bd[b + 1] = 0;
                    bd[b + 2] = w - 1;
                    bd[b + 3] = h - 1;
                }
            }
        }
        let mut stacks: Vec<Vec<u32>> = Vec::with_capacity(count);
        let mut sizes: Vec<i64> = Vec::with_capacity(count);
        let mut dropped = vec![false; count];
        for i in 0..count {
            roots[i] = i;
            for &t in &parts[i] {
                self.visited[t as usize] = base + i as u32;
            }
            stacks.push(parts[i].clone());
            sizes.push(parts[i].len() as i64);
        }
        let mut growing = count as i64;
        let mut best: Option<usize> = None;
        let mut nb = [0u32; 4];
        loop {
            for i in 0..count {
                if roots[i] != i || dropped[i] || stacks[i].is_empty() {
                    continue;
                }
                let t = stacks[i].pop().unwrap();
                if let Some(bd) = bounds.as_ref() {
                    let (x, y) = ((t as i64) % w, (t as i64) / w);
                    let b = i * 4;
                    if x < bd[b] || y < bd[b + 1] || x > bd[b + 2] || y > bd[b + 3] {
                        stacks[i].push(t);
                        dropped[i] = true;
                        growing -= 1;
                        continue;
                    }
                }
                let n = self.map.neighbors4(t, &mut nb);
                for &nn in &nb[..n] {
                    if self.map.owner_id(nn) != me {
                        continue;
                    }
                    let part = self.visited[nn as usize] as i64 - base as i64;
                    if part < 0 || part >= count as i64 {
                        self.visited[nn as usize] = base + i as u32;
                        sizes[i] += 1;
                        stacks[i].push(nn);
                        continue;
                    }
                    let other = root_of(roots, part as usize);
                    if other == i {
                        continue;
                    }
                    roots[other] = i;
                    sizes[i] += sizes[other];
                    let moved = std::mem::take(&mut stacks[other]);
                    stacks[i].extend(moved);
                    if !dropped[other] {
                        growing -= 1;
                    }
                    if let Some(bd) = bounds.as_mut() {
                        let (b, o) = (i * 4, other * 4);
                        if bd[o] < bd[b] {
                            bd[b] = bd[o];
                        }
                        if bd[o + 1] < bd[b + 1] {
                            bd[b + 1] = bd[o + 1];
                        }
                        if bd[o + 2] > bd[b + 2] {
                            bd[b + 2] = bd[o + 2];
                        }
                        if bd[o + 3] > bd[b + 3] {
                            bd[b + 3] = bd[o + 3];
                        }
                    }
                }
                if stacks[i].is_empty() {
                    growing -= 1;
                    if best.is_none_or(|bi| sizes[i] > sizes[bi] || (sizes[i] == sizes[bi] && i < bi)) {
                        best = Some(i);
                    }
                }
            }
            if growing == 0 {
                return best;
            }
            if growing == 1 {
                let mut last = 0;
                while roots[last] != last || dropped[last] || stacks[last].is_empty() {
                    last += 1;
                }
                if best.is_none_or(|bi| sizes[last] > sizes[bi]) {
                    return Some(last);
                }
            }
        }
    }

    pub(super) fn annex_severed_clusters(&mut self, p: usize, candidates: &Candidates, main_tiles: &[Vec<u32>]) {
        let me = self.players[p].small_id;
        for c in 0..candidates.clusters.len() {
            let mut main_gen = self.stamp_main_body(main_tiles);
            for &start in &candidates.clusters[c] {
                if self.map.owner_id(start) != me {
                    continue;
                }
                let mark = self.visited[start as usize];
                if mark >= main_gen && mark <= main_gen + 2 {
                    continue;
                }
                let Some(territory) = self.severed_territory(p, start, main_gen, &candidates.boxes[c * 4..c * 4 + 4]) else { continue };
                let Some(capturing) = self.capturing_player(p, &territory) else { continue };
                if self.players[p].tiles.len() == territory.len() {
                    self.conquer_player(capturing, p);
                }
                for &t in &territory {
                    self.conquer(capturing, t);
                }
                main_gen = self.stamp_main_body(main_tiles);
            }
        }
    }

    pub(super) fn stamp_main_body(&mut self, main_tiles: &[Vec<u32>]) -> u32 {
        let gen = self.bump_generations(3);
        for piece in main_tiles {
            for &t in piece {
                self.visited[t as usize] = gen;
            }
        }
        gen
    }

    pub(super) fn severed_territory(&mut self, p: usize, start: u32, main_gen: u32, bx: &[i64]) -> Option<Vec<u32>> {
        let me = self.players[p].small_id;
        let (seen, fail) = (main_gen + 1, main_gen + 2);
        let (min_x, min_y, max_x, max_y) = (bx[0], bx[1], bx[2], bx[3]);
        let mut tiles = vec![start];
        self.visited[start as usize] = seen;
        let mut stack = vec![start];
        let (mut has_enemy, mut has_fallout) = (false, false);
        let mut nb = [0u32; 4];
        let stamp = |g: &mut Game, tiles: &[u32], gen: u32| {
            for &t in tiles {
                g.visited[t as usize] = gen;
            }
        };
        while let Some(tile) = stack.pop() {
            let (x, y) = (self.map.x(tile) as i64, self.map.y(tile) as i64);
            if x < min_x || x > max_x || y < min_y || y > max_y || self.map.is_shore(tile) || self.map.is_on_edge_of_map(tile) {
                stamp(self, &tiles, fail);
                return None;
            }
            let n = self.map.neighbors4(tile, &mut nb);
            for &nn in &nb[..n] {
                let owner = self.map.owner_id(nn);
                if owner == me {
                    let mark = self.visited[nn as usize];
                    if mark == main_gen {
                        stamp(self, &tiles, main_gen);
                        return None;
                    }
                    if mark == fail {
                        stamp(self, &tiles, fail);
                        return None;
                    }
                    if mark == seen {
                        continue;
                    }
                    self.visited[nn as usize] = seen;
                    tiles.push(nn);
                    stack.push(nn);
                } else if owner != 0 {
                    has_enemy = true;
                } else if self.map.has_fallout(nn) {
                    has_fallout = true;
                } else {
                    stamp(self, &tiles, fail);
                    return None;
                }
            }
        }
        if has_enemy && has_fallout {
            Some(tiles)
        } else {
            None
        }
    }
}

