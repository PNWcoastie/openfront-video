//! engine FlatBinaryHeap: a min-heap of (priority, tile). Priorities are
//! stored as float32 but a new priority is compared at float64 before it is
//! stored, exactly as the TypeScript does with its Float32Array.

#[derive(Default)]
pub struct FlatBinaryHeap {
    nodes: Vec<(f32, u32)>,
}

impl FlatBinaryHeap {
    #[inline]
    pub fn clear(&mut self) {
        self.nodes.clear();
    }

    #[inline]
    pub fn size(&self) -> usize {
        self.nodes.len()
    }

    #[inline]
    pub fn enqueue(&mut self, tile: u32, priority: f64) {
        let mut i = self.nodes.len();
        self.nodes.push((0.0, 0));
        while i > 0 {
            let parent = (i - 1) >> 1;
            let p_node = self.nodes[parent];
            if priority >= p_node.0 as f64 {
                break;
            }
            self.nodes[i] = p_node;
            i = parent;
        }
        self.nodes[i] = (priority as f32, tile);
    }

    #[inline]
    pub fn dequeue(&mut self) -> u32 {
        assert!(!self.nodes.is_empty(), "heap empty");
        let top = self.nodes[0].1;
        let last = self.nodes.pop().unwrap();
        let len = self.nodes.len();
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
            let child = if right < len && self.nodes[right].0 < self.nodes[left].0 { right } else { left };
            let c_node = self.nodes[child];
            if last.0 <= c_node.0 {
                break;
            }
            self.nodes[i] = c_node;
            i = child;
        }
        self.nodes[i] = last;
        top
    }
}
