//! A bounded top-N collector.
//!
//! The stats tables of this crate are read by the admin UI, which
//! renders a handful of rows out of a table that holds up to
//! [`crate::bot_stats::MAX_UNIQUE_PAIRS`] of them. Materialising the
//! whole table to sort it is wasted work — and it holds the table's
//! read lock across the allocation *and* the sort, which stalls the
//! writers on the request hot path (invariant #5: a side channel
//! must not take part in the forwarding decision).
//!
//! [`TopN`] keeps the best `limit` items of a single pass: the heap
//! never holds more than `limit` elements, and an item that is worse
//! than the worst one already kept is dropped on the spot.
//!
//! ## The ordering convention
//!
//! `T: Ord` ranks the **better** item as the **greater** one — more
//! hits is better, a smaller host is better. The heap therefore
//! keeps the *worst* of the kept items on top (a min-heap over `T`),
//! which is exactly the item an incoming better row replaces.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

/// Collects the best `limit` items of a stream, in one pass.
pub(crate) struct TopN<T> {
    /// How many items are kept. `0` keeps nothing.
    limit: usize,
    /// The kept items, worst first. `Reverse` turns the max-heap into
    /// a min-heap over `T`, so `peek` is the worst kept item.
    heap: BinaryHeap<Reverse<T>>,
}

impl<T: Ord> TopN<T> {
    /// A collector that keeps the best `limit` items.
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            limit,
            heap: BinaryHeap::new(),
        }
    }

    /// Offer one item to the collector.
    ///
    /// The item is kept when the collector is not full yet, or when it
    /// is better than the worst item already kept. A worse item is
    /// dropped here, which is why the caller may offer a *borrowed*
    /// view of its row: only the rows that end up in the result are
    /// ever cloned.
    pub(crate) fn offer(&mut self, item: T) {
        if self.limit == 0 {
            return;
        }
        if self.heap.len() < self.limit {
            self.heap.push(Reverse(item));
            return;
        }
        let Some(Reverse(worst)) = self.heap.peek() else {
            return;
        };
        if item.cmp(worst).is_gt() {
            self.heap.pop();
            self.heap.push(Reverse(item));
        }
    }

    /// The kept items, best first.
    pub(crate) fn into_best_first(mut self) -> Vec<T> {
        let mut items = Vec::with_capacity(self.heap.len());
        while let Some(Reverse(item)) = self.heap.pop() {
            items.push(item);
        }
        // The heap pops the worst item first; the caller wants the
        // best one first.
        items.reverse();
        items
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The collector keeps the `limit` best items — not the first
    /// `limit` items it saw.
    #[test]
    fn topn_keeps_the_best_items() {
        let mut top = TopN::new(3);
        for value in [5, 1, 9, 7, 3] {
            top.offer(value);
        }
        assert_eq!(top.into_best_first(), vec![9, 7, 5]);
    }

    /// A limit of zero keeps nothing — not even the first item.
    #[test]
    fn topn_with_a_zero_limit_keeps_nothing() {
        let mut top = TopN::new(0);
        top.offer(42);
        assert!(top.into_best_first().is_empty());
    }

    /// The result comes back best first, so the caller does not have
    /// to sort it again.
    #[test]
    fn topn_returns_the_items_best_first() {
        let mut top = TopN::new(4);
        for value in [2, 10, 6] {
            top.offer(value);
        }
        assert_eq!(top.into_best_first(), vec![10, 6, 2]);
    }

    /// Fewer items than the limit: everything is kept, in order.
    #[test]
    fn topn_keeps_everything_when_the_stream_is_short() {
        let mut top = TopN::new(10);
        top.offer(1);
        top.offer(2);
        assert_eq!(top.into_best_first(), vec![2, 1]);
    }
}
