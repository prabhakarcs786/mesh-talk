//! Bounded "seen" cache used for de-duplication: without this, flood routing would relay
//! the same message forever in a loop between nodes.

use std::collections::{HashSet, VecDeque};

pub struct SeenCache {
    set: HashSet<[u8; 16]>,
    order: VecDeque<[u8; 16]>,
    capacity: usize,
}

impl SeenCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            set: HashSet::with_capacity(capacity),
            order: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    /// Returns true if this id was newly inserted (i.e. not seen before).
    pub fn check_and_insert(&mut self, id: [u8; 16]) -> bool {
        if !self.set.insert(id) {
            return false;
        }
        self.order.push_back(id);
        if self.order.len() > self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.set.remove(&oldest);
            }
        }
        true
    }
}
