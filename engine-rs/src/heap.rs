//! engine FlatBinaryHeap: a min-heap of (priority, tile). Priorities are
//! stored as float32 but a new priority is compared at float64 before it is
//! stored, exactly as the TypeScript does with its Float32Array.

#[derive(Default)]
pub struct FlatBinaryHeap {
    pri: Vec<f32>,
    tiles: Vec<u32>,
}

impl FlatBinaryHeap {
    pub fn clear(&mut self) {
        self.pri.clear();
        self.tiles.clear();
    }

    pub fn size(&self) -> usize {
        self.pri.len()
    }

    pub fn enqueue(&mut self, tile: u32, priority: f64) {
        let mut i = self.pri.len();
        self.pri.push(0.0);
        self.tiles.push(0);
        while i > 0 {
            let parent = (i - 1) >> 1;
            if priority >= self.pri[parent] as f64 {
                break;
            }
            self.pri[i] = self.pri[parent];
            self.tiles[i] = self.tiles[parent];
            i = parent;
        }
        self.pri[i] = priority as f32;
        self.tiles[i] = tile;
    }

    pub fn dequeue(&mut self) -> u32 {
        assert!(!self.pri.is_empty(), "heap empty");
        let top = self.tiles[0];
        let last_pri = self.pri.pop().unwrap();
        let last_tile = self.tiles.pop().unwrap();
        let len = self.pri.len();
        if len == 0 {
            return top;
        }
        let mut i = 0;
        loop {
            let left = (i << 1) + 1;
            if left >= len {
                break;
            }
            let right = left + 1;
            let child = if right < len && self.pri[right] < self.pri[left] { right } else { left };
            if last_pri <= self.pri[child] {
                break;
            }
            self.pri[i] = self.pri[child];
            self.tiles[i] = self.tiles[child];
            i = child;
        }
        self.pri[i] = last_pri;
        self.tiles[i] = last_tile;
        top
    }
}
