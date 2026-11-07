//! engine-lib TileSet: a set of tile refs that iterates in insertion order.
//! Adding a present value leaves it in place; deleting and re-adding moves it
//! to the end. Iterating while changing the set works like the engine's:
//! deleted entries are skipped and added ones are visited, because compaction
//! waits until no iteration is in progress (see `pause_compaction`).

use crate::fxhash::FxHashMap;

const TOMBSTONE: u32 = u32::MAX;

#[derive(Default, Clone, Debug)]
pub struct TileSet {
    dense: Vec<u32>,
    index: FxHashMap<u32, u32>,
    live_len: u32,
    paused: u32,
}

impl TileSet {
    #[inline]
    pub fn len(&self) -> usize {
        self.live_len as usize
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.live_len == 0
    }

    #[inline]
    pub fn clear(&mut self) {
        self.dense.clear();
        self.index.clear();
        self.live_len = 0;
        self.paused = 0;
    }

    #[inline]
    pub fn contains(&self, v: u32) -> bool {
        self.index.contains_key(&v)
    }

    #[inline]
    pub fn insert(&mut self, v: u32) -> bool {
        let pos = self.dense.len() as u32;
        if let std::collections::hash_map::Entry::Vacant(e) = self.index.entry(v) {
            e.insert(pos);
            self.dense.push(v);
            self.live_len += 1;
            true
        } else {
            false
        }
    }

    #[inline]
    pub fn add(&mut self, v: u32) {
        self.insert(v);
    }

    #[inline]
    pub fn delete(&mut self, v: u32) -> bool {
        match self.index.remove(&v) {
            Some(i) => {
                self.dense[i as usize] = TOMBSTONE;
                self.live_len -= 1;
                self.maybe_compact();
                true
            }
            None => false,
        }
    }

    #[inline]
    pub fn insert_unordered(&mut self, v: u32) -> bool {
        if let std::collections::hash_map::Entry::Vacant(e) = self.index.entry(v) {
            e.insert(0);
            self.live_len += 1;
            true
        } else {
            false
        }
    }

    #[inline]
    pub fn delete_unordered(&mut self, v: u32) -> bool {
        if self.index.remove(&v).is_some() {
            self.live_len -= 1;
            true
        } else {
            false
        }
    }

    fn maybe_compact(&mut self) {
        if self.paused == 0 && self.dense.len() > 64 && (self.live_len as usize) * 2 < self.dense.len() {
            self.dense.retain(|&v| v != TOMBSTONE);
            for (i, &v) in self.dense.iter().enumerate() {
                self.index.insert(v, i as u32);
            }
        }
    }

    #[inline]
    pub fn contains_indexed(&self, v: u32, slots: &[u32]) -> bool {
        let idx = slots[v as usize] as usize;
        idx < self.dense.len() && self.dense[idx] == v
    }

    #[inline]
    pub fn add_indexed(&mut self, v: u32, slots: &mut [u32]) {
        let idx = slots[v as usize] as usize;
        if idx < self.dense.len() && self.dense[idx] == v {
            return;
        }
        slots[v as usize] = self.dense.len() as u32;
        self.dense.push(v);
        self.live_len += 1;
    }

    #[inline]
    pub fn delete_indexed(&mut self, v: u32, slots: &mut [u32]) -> bool {
        let idx = slots[v as usize] as usize;
        if idx < self.dense.len() && self.dense[idx] == v {
            self.dense[idx] = TOMBSTONE;
            slots[v as usize] = TOMBSTONE;
            self.live_len -= 1;
            self.maybe_compact_indexed(slots);
            true
        } else {
            false
        }
    }

    fn maybe_compact_indexed(&mut self, slots: &mut [u32]) {
        if self.paused == 0 && self.dense.len() > 64 && (self.live_len as usize) * 2 < self.dense.len() {
            self.dense.retain(|&v| v != TOMBSTONE);
            for (i, &v) in self.dense.iter().enumerate() {
                slots[v as usize] = i as u32;
            }
        }
    }

    /// Live values in insertion order.
    pub fn values(&self) -> impl Iterator<Item = u32> + '_ {
        self.dense.iter().copied().filter(|&v| v != TOMBSTONE)
    }

    /// For iterating by position while the set changes: slots past the end
    /// at the start are new entries, visited like the engine visits them.
    pub fn slot_count(&self) -> usize {
        self.dense.len()
    }
    pub fn slot(&self, i: usize) -> Option<u32> {
        match self.dense[i] {
            TOMBSTONE => None,
            v => Some(v),
        }
    }
    pub fn pause_compaction(&mut self) {
        self.paused += 1;
    }
    pub fn resume_compaction(&mut self) {
        self.paused -= 1;
        self.maybe_compact();
    }
    pub fn resume_compaction_indexed(&mut self, slots: &mut [u32]) {
        self.paused -= 1;
        self.maybe_compact_indexed(slots);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order() {
        let mut s = TileSet::default();
        for v in [5, 1, 9] {
            s.add(v);
        }
        s.add(1);
        assert_eq!(s.values().collect::<Vec<_>>(), vec![5, 1, 9]);
        s.delete(5);
        s.add(5);
        assert_eq!(s.values().collect::<Vec<_>>(), vec![1, 9, 5]);
    }
}
