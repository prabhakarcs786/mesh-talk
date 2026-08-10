//! Milestone 2D.1: fast, in-memory-only, non-durable loop/duplicate suppression.
//!
//! This is deliberately separate from `replay_store.rs`'s durable `ReplayStore`, for two
//! different reasons depending on what it's protecting:
//!
//! 1. **Live call media (`MessageType::CallFrame`).** A call is inherently ephemeral --
//!    it doesn't (and shouldn't) survive an app restart, and frames arrive at a high
//!    rate (tens per second for audio, more for video). Routing every single frame
//!    through a durable SQLite-backed store, the way `ReplayStore` does for chat/control
//!    messages, would be needless disk I/O on a hot path for something that gains
//!    nothing from durability. `FloodGuard` gives frames the same "don't process the
//!    exact same packet twice" property, purely in memory, at a fraction of the cost.
//!
//! 2. **A cheap first-pass filter, independent of durability semantics.** See
//!    `node.rs`'s Milestone 2D.1 doc section for why relay-forwarding state and
//!    endpoint-acceptance state are *not* simply "have I seen this envelope before" --
//!    this type intentionally has no opinion about that; it only ever answers "have I
//!    processed this exact `(sender, message_id)` pair at all during this process's
//!    current uptime," which resets to empty on every restart by design.

use std::collections::{HashSet, VecDeque};
use std::sync::Mutex;

use crate::identity::NodeId;

/// Bounded so a long-running node handling a lot of traffic doesn't grow this
/// unboundedly -- oldest entries are evicted first once full, same trade-off as the
/// old (now-removed) `SeenCache` this replaces for the non-durable use case.
const DEFAULT_CAPACITY: usize = 4096;

pub struct FloodGuard {
    set: Mutex<Inner>,
}

struct Inner {
    seen: HashSet<(NodeId, [u8; 16])>,
    order: VecDeque<(NodeId, [u8; 16])>,
    capacity: usize,
}

impl FloodGuard {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            set: Mutex::new(Inner { seen: HashSet::with_capacity(capacity), order: VecDeque::with_capacity(capacity), capacity }),
        }
    }

    /// Returns `true` if this `(sender, message_id)` pair was newly recorded (i.e. not
    /// already present this session), `false` if it was already seen.
    pub fn check_and_insert(&self, sender: &NodeId, message_id: &[u8; 16]) -> bool {
        let key = (*sender, *message_id);
        let mut inner = self.set.lock().unwrap();
        if !inner.seen.insert(key) {
            return false;
        }
        inner.order.push_back(key);
        if inner.order.len() > inner.capacity {
            if let Some(oldest) = inner.order.pop_front() {
                inner.seen.remove(&oldest);
            }
        }
        true
    }
}

impl Default for FloodGuard {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_seen_is_new_second_is_not() {
        let guard = FloodGuard::new();
        let sender = [1u8; 32];
        let id = [2u8; 16];
        assert!(guard.check_and_insert(&sender, &id));
        assert!(!guard.check_and_insert(&sender, &id));
    }

    #[test]
    fn different_senders_with_the_same_message_id_do_not_collide() {
        let guard = FloodGuard::new();
        let alice = [1u8; 32];
        let bob = [2u8; 32];
        let id = [9u8; 16];
        assert!(guard.check_and_insert(&alice, &id));
        assert!(guard.check_and_insert(&bob, &id));
    }

    #[test]
    fn capacity_is_bounded_and_evicts_oldest_first() {
        let guard = FloodGuard::with_capacity(2);
        let sender = [1u8; 32];
        assert!(guard.check_and_insert(&sender, &[1u8; 16]));
        assert!(guard.check_and_insert(&sender, &[2u8; 16]));
        assert!(guard.check_and_insert(&sender, &[3u8; 16])); // evicts [1u8; 16]
        // The oldest entry was evicted, so it's treated as new again.
        assert!(guard.check_and_insert(&sender, &[1u8; 16]));
    }
}
