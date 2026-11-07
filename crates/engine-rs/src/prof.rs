//! ENGINE_RS_PROFILE: time spent in named scopes, for finding hot spots.
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::fxhash::FxHashMap;

pub static ON: AtomicBool = AtomicBool::new(false);

thread_local! {
    static TIMES: RefCell<FxHashMap<&'static str, (Duration, u64)>> = RefCell::new(FxHashMap::default());
}

pub struct Scope(&'static str, Option<Instant>);

impl Drop for Scope {
    fn drop(&mut self) {
        if let Some(t0) = self.1 {
            let d = t0.elapsed();
            TIMES.with(|t| {
                let mut t = t.borrow_mut();
                let e = t.entry(self.0).or_default();
                e.0 += d;
                e.1 += 1;
            });
        }
    }
}

/// Times until the returned guard drops (nested scopes count in both).
pub fn scope(name: &'static str) -> Scope {
    Scope(name, ON.load(Ordering::Relaxed).then(Instant::now))
}

pub fn report() {
    TIMES.with(|t| {
        let mut v: Vec<_> = t.borrow().iter().map(|(k, v)| (*k, *v)).collect();
        v.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));
        for (k, (d, n)) in v {
            eprintln!("  scope {:<30} {:>8.1} ms  {:>10} calls", k, d.as_secs_f64() * 1000.0, n);
        }
    });
}
