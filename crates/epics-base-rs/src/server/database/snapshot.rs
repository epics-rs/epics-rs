//! Read-mostly index cell: readers take an `Arc` snapshot with no lock,
//! writers serialize on a small `std::sync::Mutex` and publish a rebuilt
//! value.
//!
//! This is the **ArcSwap** shape for the database index rows (L8c
//! `load_order`, L8d `cp_links`, L8e `external_cp_links`, L8i `link_sets`,
//! L8k `breaktable_registry`).
//! Those five are read on banded threads — periodic scan (`scan-N`, EPICS
//! 60–66), the CA/PVA connection threads (EPICS 20), record processing — and
//! written rarely. Under a `tokio::sync::RwLock` a *low*-priority reader that
//! is preempted while holding the read side delays a *high*-priority scan
//! thread for an unbounded time, invisibly to the kernel (§0 finding 4). A
//! reader that never blocks cannot do that, at any priority, so removing the
//! lock closes the inversion outright rather than bounding it the way
//! priority inheritance does.
//!
//! # Invariant
//!
//! **Every mutation of the cell MUST go through [`SnapshotCell::update`].**
//! `update` is the single owner of the read-modify-write transition: it holds
//! the writer mutex across clone → mutate → publish, so two concurrent writers
//! cannot each rebuild from the same base and lose one another's edit. The
//! type exposes no `store` and no `&mut` accessor, so there is no other way to
//! write it — the invariant holds by construction, not by convention.
//!
//! A cell whose only writer replaces the **whole value** needs none of this: a
//! bare `ArcSwap`/`ArcSwapOption` store is already atomic. Those rows
//! (L8f/L8g/L8h/L8j, and L9's ACF cell) use `arc_swap` directly and are not
//! `SnapshotCell`s.
//!
//! # Reader semantics
//!
//! A reader observes one coherent published value for as long as it holds the
//! returned `Arc` — it never sees a half-applied `update`, and it never sees a
//! write published after it loaded. That is *stronger* than the read guard it
//! replaces only in the sense that the guard could not be held across an
//! `.await` either; every converted reader either finished with the map inside
//! one expression or explicitly dropped the guard before awaiting. Per-site
//! evidence is in the doc comment on each converted field.

use std::sync::Arc;

use crate::runtime::background::facility::recover;

/// What the writer gate calls itself when reporting a recovered poisoning.
const FACILITY: &str = "database index";

/// A read-mostly value: lock-free `Arc` snapshot reads, serialized
/// rebuild-and-publish writes. See the module doc for the invariant.
pub(crate) struct SnapshotCell<T> {
    cell: arc_swap::ArcSwap<T>,
    /// Serializes [`Self::update`]'s clone → mutate → publish. `std`, not
    /// `tokio`: it is held for a map clone and never across an `.await`, and
    /// making it async would put the writer back on the park path this cell
    /// exists to get readers off.
    writer: std::sync::Mutex<()>,
}

impl<T> SnapshotCell<T> {
    pub(crate) fn new(value: T) -> Self {
        Self {
            cell: arc_swap::ArcSwap::from_pointee(value),
            writer: std::sync::Mutex::new(()),
        }
    }

    /// The currently published value, borrowed. Cheapest read; the returned
    /// guard is **not** `Send`, so use it only where it dies inside the same
    /// expression or statement, with no `.await` in its live range.
    pub(crate) fn load(&self) -> arc_swap::Guard<Arc<T>> {
        self.cell.load()
    }

    /// The currently published value as an owned `Arc`. Use this whenever the
    /// value is bound across statements or outlives an `.await`.
    pub(crate) fn load_full(&self) -> Arc<T> {
        self.cell.load_full()
    }
}

impl<T: Clone> SnapshotCell<T> {
    /// Serialized read-modify-write: clone the published value, apply `f`,
    /// publish the result. Returns the newly published snapshot.
    ///
    /// A panic inside `f` cannot corrupt the cell — the store never runs, so
    /// the published value is the pre-update one. Recovering the poisoned
    /// writer mutex is therefore the correct reading of the poisoning, exactly
    /// as [`recover`]'s own doc argues for the facility queues.
    pub(crate) fn update(&self, f: impl FnOnce(&mut T)) -> Arc<T> {
        let _writing = recover(FACILITY, self.writer.lock());
        let mut next = (*self.cell.load_full()).clone();
        f(&mut next);
        let published = Arc::new(next);
        self.cell.store(published.clone());
        published
    }
}

#[cfg(test)]
mod tests {
    use super::SnapshotCell;
    use std::collections::HashMap;

    #[test]
    fn update_publishes_and_readers_see_it() {
        let cell = SnapshotCell::new(HashMap::<String, u64>::new());
        assert!(cell.load().is_empty());
        cell.update(|m| {
            m.insert("A".into(), 1);
        });
        assert_eq!(cell.load().get("A").copied(), Some(1));
    }

    #[test]
    fn a_snapshot_taken_before_an_update_does_not_observe_it() {
        let cell = SnapshotCell::new(HashMap::<String, u64>::new());
        cell.update(|m| {
            m.insert("A".into(), 1);
        });
        let before = cell.load_full();
        cell.update(|m| {
            m.insert("B".into(), 2);
        });
        assert_eq!(before.len(), 1, "the held snapshot must stay coherent");
        assert_eq!(cell.load().len(), 2);
    }

    /// The reason the writer gate exists: without it two concurrent
    /// rebuild-from-the-same-base writers lose one edit. With it, every
    /// increment lands.
    #[test]
    fn concurrent_writers_do_not_lose_edits() {
        let cell = std::sync::Arc::new(SnapshotCell::new(HashMap::<usize, usize>::new()));
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let cell = cell.clone();
                std::thread::spawn(move || {
                    for i in 0..64 {
                        cell.update(|m| {
                            m.insert(t * 64 + i, t);
                        });
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(cell.load().len(), 8 * 64);
    }
}
