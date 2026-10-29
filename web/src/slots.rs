//! The render slots (MAX_RENDERS, default 2), shared by people's requests and
//! the auto renderer. A request never waits behind an auto render: if every
//! slot is busy, the auto render holding one is stopped and the request takes
//! its slot (the auto game goes back on the queue). Auto renders only take a
//! slot while nobody is waiting for one.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{oneshot, Notify};

struct Running {
    token: u64,
    id: String,
    started: std::time::Instant,
    auto: bool,
    /// Stops an auto render so a request can have its slot.
    preempt: Option<oneshot::Sender<()>>,
}

pub struct Slots {
    capacity: usize,
    running: Mutex<Vec<Running>>,
    freed: Notify,
    next_token: AtomicU64,
    /// Requests waiting for a slot; auto renders hold back while any do.
    pub demand_waiting: AtomicUsize,
}

/// A held slot, given back when dropped.
pub struct Slot {
    slots: Arc<Slots>,
    token: u64,
    /// Fires when a request takes this (auto) render's slot.
    pub preempted: Option<oneshot::Receiver<()>>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.slots.running.lock().unwrap().retain(|r| r.token != self.token);
        self.slots.freed.notify_waiters();
    }
}

impl Slots {
    pub fn new(capacity: usize) -> Arc<Slots> {
        Arc::new(Slots {
            capacity: capacity.max(1),
            running: Mutex::new(Vec::new()),
            freed: Notify::new(),
            next_token: AtomicU64::new(0),
            demand_waiting: AtomicUsize::new(0),
        })
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Whether an auto render could start now.
    pub fn auto_can_start(&self) -> bool {
        self.demand_waiting.load(Ordering::SeqCst) == 0 && self.running.lock().unwrap().len() < self.capacity
    }

    /// What each busy slot is rendering: game ID, automatic, seconds so far.
    pub fn running(&self) -> Vec<(String, bool, f64)> {
        self.running.lock().unwrap().iter().map(|r| (r.id.clone(), r.auto, r.started.elapsed().as_secs_f64())).collect()
    }

    pub async fn acquire(self: &Arc<Self>, id: &str, auto: bool) -> Slot {
        if !auto {
            self.demand_waiting.fetch_add(1, Ordering::SeqCst);
        }
        loop {
            {
                let mut running = self.running.lock().unwrap();
                let open = running.len() < self.capacity && (!auto || self.demand_waiting.load(Ordering::SeqCst) == 0);
                if open {
                    let token = self.next_token.fetch_add(1, Ordering::Relaxed);
                    let (tx, rx) = oneshot::channel();
                    running.push(Running { token, id: id.to_string(), started: std::time::Instant::now(), auto, preempt: auto.then_some(tx) });
                    if !auto {
                        self.demand_waiting.fetch_sub(1, Ordering::SeqCst);
                    }
                    return Slot { slots: self.clone(), token, preempted: auto.then_some(rx) };
                }
                // A request takes the slot of an auto render (the latest started).
                if !auto {
                    if let Some(r) = running.iter_mut().rev().find(|r| r.auto && r.preempt.is_some()) {
                        let _ = r.preempt.take().unwrap().send(());
                    }
                }
            }
            // Slots are freed on drop; the timeout covers a wake-up missed between
            // the check above and starting to wait.
            let _ = tokio::time::timeout(Duration::from_millis(250), self.freed.notified()).await;
        }
    }
}
