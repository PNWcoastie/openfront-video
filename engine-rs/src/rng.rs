//! engine-lib PseudoRandom: sfc32 seeded with splitmix32.

use crate::js::to_int32;

#[derive(Clone, Debug)]
pub struct PseudoRandom {
    s0: i32,
    s1: i32,
    s2: i32,
    s3: i32,
}

/// Debugging aid: while set, next_int calls are printed to stderr.
pub static TRACE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

const POW36_8: f64 = 2_821_109_907_456.0; // 36 ** 8

impl PseudoRandom {
    pub fn new(seed: f64) -> Self {
        let mut h = to_int32(seed);
        let mut split = || {
            h = h.wrapping_add(0x9e3779b9u32 as i32);
            let mut t = h ^ ((h as u32) >> 16) as i32;
            t = t.wrapping_mul(0x21f0aaad);
            t ^= ((t as u32) >> 15) as i32;
            t = t.wrapping_mul(0x735a2d97);
            t ^ ((t as u32) >> 15) as i32
        };
        let (s0, s1, s2, s3) = (split(), split(), split(), split());
        let mut r = PseudoRandom { s0, s1, s2, s3 };
        for _ in 0..12 {
            r.next();
        }
        r
    }

    /// A float in [0, 1).
    pub fn next(&mut self) -> f64 {
        let t = self.s0.wrapping_add(self.s1).wrapping_add(self.s3);
        self.s3 = self.s3.wrapping_add(1);
        self.s0 = self.s1 ^ ((self.s1 as u32) >> 9) as i32;
        self.s1 = self.s2.wrapping_add(self.s2.wrapping_shl(3));
        self.s2 = self.s2.wrapping_shl(21) | ((self.s2 as u32) >> 11) as i32;
        self.s2 = self.s2.wrapping_add(t);
        (t as u32) as f64 / 4294967296.0
    }

    /// An integer in [min, max).
    pub fn next_int(&mut self, min: f64, max: f64) -> f64 {
        let lo = min.floor();
        let hi = max.floor();
        let v = (self.next() * (hi - lo)).floor() + lo;
        if TRACE.load(std::sync::atomic::Ordering::Relaxed) {
            eprintln!("nextInt({min}, {max}) = {v}");
        }
        v
    }

    /// 8 base-36 characters.
    pub fn next_id(&mut self) -> String {
        let mut n = (self.next() * POW36_8).floor() as u64;
        let mut digits = Vec::new();
        if n == 0 {
            digits.push(b'0');
        }
        while n > 0 {
            let d = (n % 36) as u8;
            digits.push(if d < 10 { b'0' + d } else { b'a' + d - 10 });
            n /= 36;
        }
        while digits.len() < 8 {
            digits.push(b'0');
        }
        digits.reverse();
        String::from_utf8(digits).unwrap()
    }

    /// Fisher-Yates over a copy, from the end.
    pub fn shuffle<T: Clone>(&mut self, array: &[T]) -> Vec<T> {
        let mut result = array.to_vec();
        for i in (1..result.len()).rev() {
            let j = self.next_int(0.0, (i + 1) as f64) as usize;
            result.swap(i, j);
        }
        result
    }
}
