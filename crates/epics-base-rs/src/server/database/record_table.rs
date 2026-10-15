//! Every record the database ever registered, by the index it was given at
//! registration: what a resolved forward link carries instead of a share of
//! the target's cell.
//!
//! Append-only, and a removed record keeps its slot: C never frees a record
//! after `iocInit`, so a target a forward resolved to is valid for the IOC's
//! life, and a dispatch borrows it from here with two loads and no count.

use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};

use crate::server::record::RecordCell;

/// A record's index in the [`RecordTable`]: fixed at registration, never
/// reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct RecordIdx(NonZeroU32);

impl RecordIdx {
    /// The index as an atomic cell stores it; `0` is no record.
    pub(crate) fn raw(self) -> u32 {
        self.0.get()
    }

    pub(crate) fn from_raw(raw: u32) -> Option<Self> {
        NonZeroU32::new(raw).map(Self)
    }
}

/// Segment 0 holds `1 << FIRST_BITS` slots and each segment doubles, so the
/// `u32` index space fits in [`SEGMENTS`] lazily allocated segments.
const FIRST_BITS: u32 = 10;
const SEGMENTS: usize = (u32::BITS - FIRST_BITS) as usize + 1;

type Segment = Box<[OnceLock<Arc<RecordCell>>]>;

pub(crate) struct RecordTable {
    next: AtomicU32,
    segments: [OnceLock<Segment>; SEGMENTS],
}

impl Default for RecordTable {
    fn default() -> Self {
        Self {
            next: AtomicU32::new(0),
            segments: std::array::from_fn(|_| OnceLock::new()),
        }
    }
}

/// `(segment, offset)` of index `i`.
#[inline(always)]
fn place(i: RecordIdx) -> (usize, usize) {
    let k = u64::from(i.0.get() - 1) + (1 << FIRST_BITS);
    let seg = (u64::BITS - 1 - k.leading_zeros()) - FIRST_BITS;
    (seg as usize, (k - (1 << (seg + FIRST_BITS))) as usize)
}

impl RecordTable {
    /// The next index, for a cell about to be built.
    pub(crate) fn reserve(&self) -> RecordIdx {
        let i = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        RecordIdx(NonZeroU32::new(i).expect("record index space exhausted"))
    }

    /// Publish `cell` at the index it was built with.
    pub(crate) fn publish(&self, i: RecordIdx, cell: Arc<RecordCell>) {
        let (seg, off) = place(i);
        let segment = self.segments[seg].get_or_init(|| {
            (0..1usize << (seg as u32 + FIRST_BITS))
                .map(|_| OnceLock::new())
                .collect()
        });
        let published = segment[off].set(cell).is_ok();
        debug_assert!(published, "record index {i:?} published twice");
    }

    /// The cell at `i`; a removed record's cell too.
    #[inline(always)]
    pub(crate) fn get(&self, i: RecordIdx) -> Option<&Arc<RecordCell>> {
        let (seg, off) = place(i);
        self.segments.get(seg)?.get()?.get(off)?.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indices_map_onto_segment_boundaries() {
        let at = |i: u32| place(RecordIdx(NonZeroU32::new(i).unwrap()));
        assert_eq!(at(1), (0, 0));
        assert_eq!(at(1 << FIRST_BITS), (0, (1 << FIRST_BITS) - 1));
        assert_eq!(at((1 << FIRST_BITS) + 1), (1, 0));
        assert_eq!(at(3 << FIRST_BITS), (1, (2 << FIRST_BITS) - 1));
        assert_eq!(at((3 << FIRST_BITS) + 1), (2, 0));
        assert_eq!(at(u32::MAX).0, SEGMENTS - 1);
    }
}
