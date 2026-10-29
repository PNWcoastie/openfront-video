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
    index: FxHashMap<u32, usize>,
    paused: u32,
}

impl TileSet {
    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn contains(&self, v: u32) -> bool {
        self.index.contains_key(&v)
    }

    pub fn add(&mut self, v: u32) {
        if self.index.contains_key(&v) {
            return;
        }
        self.index.insert(v, self.dense.len());
        self.dense.push(v);
    }

    pub fn delete(&mut self, v: u32) -> bool {
        match self.index.remove(&v) {
            Some(i) => {
                self.dense[i] = TOMBSTONE;
                self.maybe_compact();
                true
            }
            None => false,
        }
    }

    fn maybe_compact(&mut self) {
        if self.paused == 0 && self.dense.len() > 64 && self.index.len() * 2 < self.dense.len() {
            self.dense.retain(|&v| v != TOMBSTONE);
            for (i, &v) in self.dense.iter().enumerate() {
                self.index.insert(v, i);
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
