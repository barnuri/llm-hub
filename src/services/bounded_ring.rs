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
        if self.capacity == 0 {
            return;
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pushing_up_to_capacity_keeps_every_item_in_order() {
        let mut ring = BoundedRing::new(3);
        ring.push(1);
        ring.push(2);
        ring.push(3);

        assert_eq!(ring.iter().copied().collect::<Vec<_>>(), [1, 2, 3]);
    }

    #[test]
    fn one_more_push_evicts_the_oldest_item() {
        let mut ring = BoundedRing::new(3);
        for item in 1..=4 {
            ring.push(item);
        }

        assert_eq!(ring.iter().copied().collect::<Vec<_>>(), [2, 3, 4]);
    }

    #[test]
    fn zero_capacity_keeps_nothing() {
        let mut ring = BoundedRing::new(0);
        ring.push(1);

        assert_eq!(ring.iter().count(), 0);
    }
}
