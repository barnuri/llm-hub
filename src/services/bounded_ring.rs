//! A fixed-capacity log that drops its oldest entry first, shared by the Smart Auto and Copilot decision logs.

use std::collections::VecDeque;

/// Entries in push order, at most `capacity` of them; pushing past that drops the oldest.
#[derive(Debug)]
pub struct BoundedRing<T> {
    capacity: usize,
    items: VecDeque<T>,
}

impl<T> BoundedRing<T> {
    /// An empty ring that holds at most `capacity` entries.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            items: VecDeque::new(),
        }
    }

    /// Appends `item`, dropping the oldest entry when the ring is full.
    pub fn push(&mut self, item: T) {
        if self.items.len() >= self.capacity {
            self.items.pop_front();
        }
        self.items.push_back(item);
    }

    /// The entries, oldest first.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &T> {
        self.items.iter()
    }
}
