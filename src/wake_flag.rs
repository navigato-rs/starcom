//! Coalesce notifications without coalescing or discarding the underlying data.

use std::sync::atomic;

#[derive(Default)]
pub(super) struct WakeFlag(atomic::AtomicBool);

impl WakeFlag {
    pub(super) fn post(&self, send: impl FnOnce() -> bool) {
        if !self.0.swap(true, atomic::Ordering::AcqRel) && !send() {
            // A closed event loop must not leave the flag permanently latched.
            self.0.store(false, atomic::Ordering::Release);
        }
    }

    /// Clear before inspecting model revisions, never after draining them.
    /// A concurrent producer can then enqueue the next wake without being lost.
    pub(super) fn acknowledge(&self) {
        self.0.store(false, atomic::Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell, sync, thread};

    #[test]
    fn a_burst_posts_one_notification() {
        let flag = WakeFlag::default();
        let count = cell::Cell::new(0);
        for _ in 0..100 {
            flag.post(|| {
                count.set(count.get() + 1);
                true
            });
        }
        assert_eq!(count.get(), 1);
        flag.acknowledge();
        flag.post(|| {
            count.set(count.get() + 1);
            true
        });
        assert_eq!(count.get(), 2);
    }

    #[test]
    fn failed_delivery_does_not_latch_the_flag() {
        let flag = WakeFlag::default();
        flag.post(|| false);
        let delivered = cell::Cell::new(false);
        flag.post(|| {
            delivered.set(true);
            true
        });
        assert!(delivered.get());
    }

    #[test]
    fn producers_after_acknowledgement_can_wake_the_consumer() {
        let flag = WakeFlag::default();
        flag.post(|| true);
        flag.acknowledge();
        let delivered = cell::Cell::new(false);
        flag.post(|| {
            delivered.set(true);
            true
        });
        assert!(delivered.get());
    }

    #[test]
    fn concurrent_producers_share_one_pending_wake() {
        let flag = sync::Arc::new(WakeFlag::default());
        let count = sync::Arc::new(atomic::AtomicUsize::new(0));
        let mut threads = Vec::new();
        for _ in 0..16 {
            let flag = sync::Arc::clone(&flag);
            let count = sync::Arc::clone(&count);
            threads.push(thread::spawn(move || {
                flag.post(|| {
                    count.fetch_add(1, atomic::Ordering::Relaxed);
                    true
                });
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(count.load(atomic::Ordering::Relaxed), 1);
    }
}
