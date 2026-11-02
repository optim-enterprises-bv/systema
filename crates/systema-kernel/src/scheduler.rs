//! GPU scheduler. titan-engine runs a small, fixed number of sequences at once on one
//! 16 GB card, so every agent's inference call queues here. Higher priority waiters are
//! served first; equal priority is FIFO.

use std::collections::BinaryHeap;
use std::cmp::Ordering;
use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;

use crate::manifest::Priority;

struct Waiter {
    priority: Priority,
    seq: u64,
    tx: oneshot::Sender<()>,
}

impl PartialEq for Waiter {
    fn eq(&self, o: &Self) -> bool {
        self.priority == o.priority && self.seq == o.seq
    }
}
impl Eq for Waiter {}
impl PartialOrd for Waiter {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Waiter {
    fn cmp(&self, o: &Self) -> Ordering {
        // Max-heap: higher priority first, then lower seq (earlier) first.
        self.priority.cmp(&o.priority).then_with(|| o.seq.cmp(&self.seq))
    }
}

struct State {
    in_use: usize,
    max: usize,
    next_seq: u64,
    waiters: BinaryHeap<Waiter>,
}

#[derive(Clone)]
pub struct Scheduler {
    state: Arc<Mutex<State>>,
}

/// Holding a permit = holding one engine sequence slot. Dropping it hands the slot on.
pub struct Permit {
    state: Arc<Mutex<State>>,
}

impl Scheduler {
    pub fn new(max_concurrent: usize) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                in_use: 0,
                max: max_concurrent.max(1),
                next_seq: 0,
                waiters: BinaryHeap::new(),
            })),
        }
    }

    pub async fn acquire(&self, priority: Priority) -> Permit {
        let rx = {
            let mut s = self.state.lock().unwrap();
            if s.in_use < s.max && s.waiters.is_empty() {
                s.in_use += 1;
                return Permit { state: self.state.clone() };
            }
            let (tx, rx) = oneshot::channel();
            let seq = s.next_seq;
            s.next_seq += 1;
            s.waiters.push(Waiter { priority, seq, tx });
            rx
        };
        // The slot is transferred to us by the releasing permit; sender is never dropped unsent
        // while the scheduler lives, but treat a drop as "slot handed over" to avoid leaking.
        let _ = rx.await;
        Permit { state: self.state.clone() }
    }

    pub fn queued(&self) -> usize {
        self.state.lock().unwrap().waiters.len()
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut s = self.state.lock().unwrap();
        // Hand our slot directly to the best live waiter; skip waiters that gave up.
        while let Some(w) = s.waiters.pop() {
            if w.tx.send(()).is_ok() {
                return;
            }
        }
        s.in_use -= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn interactive_jumps_queue() {
        let sched = Scheduler::new(1);
        let held = sched.acquire(Priority::Normal).await;
        let order = Arc::new(Mutex::new(Vec::new()));

        let mut handles = Vec::new();
        for (name, prio) in [("bg", Priority::Background), ("normal", Priority::Normal), ("ui", Priority::Interactive)] {
            let (s, o) = (sched.clone(), order.clone());
            handles.push(tokio::spawn(async move {
                let _p = s.acquire(prio).await;
                o.lock().unwrap().push(name);
            }));
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(sched.queued(), 3);
        drop(held);
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(*order.lock().unwrap(), vec!["ui", "normal", "bg"]);
    }

    #[tokio::test]
    async fn concurrency_limit_respected() {
        let sched = Scheduler::new(2);
        let a = sched.acquire(Priority::Normal).await;
        let _b = sched.acquire(Priority::Normal).await;
        let s2 = sched.clone();
        let t = tokio::spawn(async move { s2.acquire(Priority::Normal).await; });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!t.is_finished());
        drop(a);
        tokio::time::timeout(Duration::from_secs(1), t).await.unwrap().unwrap();
    }
}
