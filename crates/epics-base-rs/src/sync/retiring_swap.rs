//! [`RetiringSwap`]: an `ArcSwap` whose replaced values are freed by the
//! writer, never by a reader.
//!
//! `arc_swap`'s swap pays each reader's outstanding debt with a counted
//! reference, so a reader still holding a guard on the old value can hold its
//! last `Arc`, and the guard's drop then deallocates on the reader's thread.
//! On a cycle thread that is a free inside the scan period. Here the writer
//! keeps every replaced value and frees it itself, once its strong count
//! shows no reader holds it.

use std::marker::PhantomData;
use std::ops::Deref;
use std::sync::Arc;

use arc_swap::ArcSwap;
use parking_lot::Mutex;

/// Replaced values the writer holds without allocating. A burst of stores
/// swaps faster than a reader's phase ends; past four held values the writer
/// spills into its overflow list.
const RING: usize = 4;

/// Call once from every cycle thread's start hook before its first cycle;
/// after it, [`RetiringSwap::load`] never allocates on that thread.
///
/// The one allocation a load can make is `arc_swap`'s per-thread debt node,
/// taken on the thread's first load of any `ArcSwap` and kept for the
/// thread's life. This takes it at thread start instead of in a cycle.
pub fn prime_current_thread() {
    drop(ArcSwap::from_pointee(()).load());
}

/// A shared value that cycle threads read and a writer replaces.
///
/// Readers get a [`RetireGuard`] and nothing else: there is no `load_full`
/// and no `Arc` getter, so a reader can never own the last reference.
///
/// ```compile_fail
/// use std::sync::Arc;
/// use epics_base_rs::sync::{RetireGuard, RetiringSwap};
/// struct Payload;
/// let swap = RetiringSwap::new(Arc::new(Payload));
/// let guard = swap.load();
/// let _second: RetireGuard<'_, Payload> = guard.clone();
/// ```
///
/// ```compile_fail
/// use std::sync::Arc;
/// use epics_base_rs::sync::RetiringSwap;
/// let swap = RetiringSwap::new(Arc::new(1));
/// let _owned: Arc<i32> = swap.load_full();
/// ```
///
/// ```compile_fail
/// use std::sync::Arc;
/// use epics_base_rs::sync::RetiringSwap;
/// let swap = RetiringSwap::new(Arc::new(1));
/// let guard = swap.load();
/// let _owned: Arc<i32> = Arc::clone(&guard);
/// ```
///
/// A guard cannot leave its thread, so it cannot be held across an `.await`
/// in a spawned task:
///
/// ```compile_fail
/// use std::sync::Arc;
/// use epics_base_rs::sync::RetiringSwap;
/// fn spawn<F: std::future::Future + Send>(_: F) {}
/// let swap = RetiringSwap::new(Arc::new(1));
/// let swap = &swap;
/// spawn(async move {
///     let guard = swap.load();
///     std::future::ready(()).await;
///     drop(guard);
/// });
/// ```
///
/// ```compile_fail
/// use std::sync::Arc;
/// use epics_base_rs::sync::RetiringSwap;
/// let swap = RetiringSwap::new(Arc::new(1));
/// let guard = swap.load();
/// std::thread::scope(|s| {
///     s.spawn(move || drop(guard));
/// });
/// ```
pub struct RetiringSwap<T> {
    cell: ArcSwap<T>,
    /// Writer-only leaf mutex: taken by [`Self::store`] alone, never on a
    /// cycle path and never under a record hold.
    retired: Mutex<Retired<T>>,
}

/// Replaced values some reader may still hold.
struct Retired<T> {
    ring: [Option<Arc<T>>; RING],
    /// Past a full ring. Grows only on the writer path, which is off-cycle.
    overflow: Vec<Arc<T>>,
}

/// A reader's borrow of the current value: derefs to `&T` only, is not
/// `Clone`, and is not `Send`, so it lives within one phase on one thread and
/// never yields an owned `Arc`.
pub struct RetireGuard<'a, T> {
    guard: arc_swap::Guard<Arc<T>>,
    _swap: PhantomData<&'a RetiringSwap<T>>,
    _not_send: PhantomData<*const ()>,
}

impl<T> Deref for RetireGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> RetiringSwap<T> {
    pub fn new(value: Arc<T>) -> Self {
        Self {
            cell: ArcSwap::new(value),
            retired: Mutex::new(Retired {
                ring: std::array::from_fn(|_| None),
                overflow: Vec::new(),
            }),
        }
    }

    /// The current value, within one phase on one thread: the guard is not
    /// `Send`, so it cannot cross an `.await` in a spawned task. Its drop never
    /// frees — the writer holds every replaced value, so a guard is never the
    /// last reference.
    /// Allocation-free on a thread that ran [`prime_current_thread`].
    pub fn load(&self) -> RetireGuard<'_, T> {
        RetireGuard {
            guard: self.cell.load(),
            _swap: PhantomData,
            _not_send: PhantomData,
        }
    }

    /// Replace the value. The displaced value goes into a free ring slot, or
    /// the overflow list when all four are held, and every retired value no
    /// reader holds is freed here, on this thread, after the writer mutex is
    /// released. A store never waits for a reader.
    ///
    /// A count of 1 is final: after the swap no reader can reach the value,
    /// and the swap has already turned every reader's debt on it into a
    /// counted reference.
    pub fn store(&self, value: Arc<T>) {
        let mut freed: [Option<Arc<T>>; RING] = std::array::from_fn(|_| None);
        let mut freed_overflow = Vec::new();
        {
            let mut retired = self.retired.lock();
            let displaced = self.cell.swap(value);
            for (slot, out) in retired.ring.iter_mut().zip(freed.iter_mut()) {
                if slot.as_ref().is_some_and(|a| Arc::strong_count(a) == 1) {
                    *out = slot.take();
                }
            }
            let mut i = 0;
            while i < retired.overflow.len() {
                if Arc::strong_count(&retired.overflow[i]) == 1 {
                    freed_overflow.push(retired.overflow.swap_remove(i));
                } else {
                    i += 1;
                }
            }
            match retired.ring.iter_mut().find(|s| s.is_none()) {
                Some(free) => *free = Some(displaced),
                None => retired.overflow.push(displaced),
            }
        }
        drop(freed);
        drop(freed_overflow);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread::{self, ThreadId};

    /// Records the thread each value is freed on.
    struct Probe {
        id: u32,
        drops: Arc<std::sync::Mutex<Vec<(u32, ThreadId)>>>,
    }

    impl Drop for Probe {
        fn drop(&mut self) {
            self.drops
                .lock()
                .unwrap()
                .push((self.id, thread::current().id()));
        }
    }

    type Drops = Arc<std::sync::Mutex<Vec<(u32, ThreadId)>>>;

    fn probe(id: u32, drops: &Drops) -> Arc<Probe> {
        Arc::new(Probe {
            id,
            drops: drops.clone(),
        })
    }

    fn freed(drops: &Drops) -> Vec<u32> {
        drops.lock().unwrap().iter().map(|(id, _)| *id).collect()
    }

    /// A value stored over while a guard holds it is not freed by the guard's
    /// drop; the next store sweeps it, on the storing thread.
    #[test]
    fn retire_with_a_live_guard() {
        let drops = Drops::default();
        let swap = RetiringSwap::new(probe(0, &drops));
        let guard = swap.load();
        swap.store(probe(1, &drops));
        assert_eq!(guard.id, 0);
        assert!(freed(&drops).is_empty());
        drop(guard);
        assert!(freed(&drops).is_empty(), "the guard's drop freed");
        swap.store(probe(2, &drops));
        assert_eq!(*drops.lock().unwrap(), vec![(0, thread::current().id())]);
    }

    /// A thread holding guards on all four retired values can still store:
    /// the displaced value overflows instead of waiting, and once the guards
    /// drop a later store frees everything, on the writer's thread.
    #[test]
    fn ring_full_overflows_without_waiting() {
        let drops = Drops::default();
        let swap = RetiringSwap::new(probe(0, &drops));
        let mut guards = Vec::new();
        for id in 1..=RING as u32 {
            guards.push(swap.load());
            swap.store(probe(id, &drops));
        }
        // The ring is full; these return without any guard dropping. The
        // second frees the first's overflowed value, which no guard holds.
        swap.store(probe(RING as u32 + 1, &drops));
        swap.store(probe(RING as u32 + 2, &drops));
        assert_eq!(freed(&drops), vec![RING as u32]);
        drop(guards);
        assert_eq!(freed(&drops), vec![RING as u32], "a guard's drop freed");
        swap.store(probe(RING as u32 + 3, &drops));
        let mut ids = freed(&drops);
        ids.sort_unstable();
        assert_eq!(ids, (0..=RING as u32 + 1).collect::<Vec<_>>());
        let me = thread::current().id();
        assert!(drops.lock().unwrap().iter().all(|(_, t)| *t == me));
    }

    /// Every replaced value is freed on the thread that stored over it.
    #[test]
    fn the_writer_thread_frees() {
        let drops = Drops::default();
        let swap = Arc::new(RetiringSwap::new(probe(0, &drops)));
        let writer = {
            let swap = swap.clone();
            let drops = drops.clone();
            thread::spawn(move || {
                for id in 1..=20 {
                    swap.store(probe(id, &drops));
                }
                thread::current().id()
            })
        };
        let writer_id = writer.join().unwrap();
        let drops = drops.lock().unwrap().clone();
        // No guard is held, so each store frees the value the one before it
        // retired: only the last replaced value is still in the ring.
        assert_eq!(drops.len(), 19);
        assert!(drops.iter().all(|(_, t)| *t == writer_id), "{drops:?}");
    }

    /// Guards dropped on a reader thread, before, between and after stores
    /// and after a store overflowed the full ring, never free there.
    #[test]
    fn a_reader_never_frees() {
        let drops = Drops::default();
        let swap = Arc::new(RetiringSwap::new(probe(0, &drops)));
        let (to_reader, reader_rx) = mpsc::channel::<u32>();
        let (to_writer, writer_rx) = mpsc::channel::<()>();
        let reader = {
            let swap = swap.clone();
            thread::spawn(move || {
                let mut guards = Vec::new();
                // 0: take a guard; 1: drop the oldest; 2: drop all; 3: stop.
                while let Ok(op) = reader_rx.recv() {
                    match op {
                        0 => guards.push(swap.load()),
                        1 if !guards.is_empty() => drop(guards.remove(0)),
                        2 => guards.clear(),
                        3 => break,
                        _ => {}
                    }
                    to_writer.send(()).unwrap();
                }
                thread::current().id()
            })
        };
        let step = |op| {
            to_reader.send(op).unwrap();
            writer_rx.recv().unwrap();
        };
        let mut id = 0;
        let store = |id: &mut u32, n: u32| {
            for _ in 0..n {
                *id += 1;
                swap.store(probe(*id, &drops));
            }
        };
        // Guard taken before a store and dropped after it.
        step(0);
        store(&mut id, 1);
        step(1);
        store(&mut id, 1);
        // Guards on four replaced values: the ring is full.
        for _ in 0..RING {
            step(0);
            store(&mut id, 1);
        }
        // A store into the full ring overflows, then the reader drops the
        // oldest guard.
        store(&mut id, 1);
        step(1);
        store(&mut id, 1);
        // The rest dropped one at a time between stores.
        for _ in 1..RING {
            step(1);
            store(&mut id, 1);
        }
        // Several guards on one value, all dropped together.
        step(0);
        step(0);
        store(&mut id, 1);
        step(2);
        store(&mut id, RING as u32 + 1);
        to_reader.send(3).unwrap();
        let reader_id = reader.join().unwrap();
        drop(Arc::try_unwrap(swap).ok().expect("sole owner"));
        let drops = drops.lock().unwrap().clone();
        assert_eq!(drops.len(), id as usize + 1, "{drops:?}");
        let me = thread::current().id();
        assert!(
            drops.iter().all(|(_, t)| *t != reader_id && *t == me),
            "{drops:?}"
        );
    }

    /// A value overflowed past a full ring while a reader thread holds it
    /// survives a sweep during the hold; the reader's drop does not free it,
    /// and the next store frees it on the writer's thread.
    #[test]
    fn an_overflowed_value_is_freed_by_the_writer_not_its_reader() {
        let drops = Drops::default();
        let swap = RetiringSwap::new(probe(0, &drops));
        let mut guards = Vec::new();
        for id in 1..=RING as u32 {
            guards.push(swap.load());
            swap.store(probe(id, &drops));
        }
        let held = RING as u32;
        let (to_reader, reader_rx) = mpsc::channel::<()>();
        let (to_writer, writer_rx) = mpsc::channel::<()>();
        let reader_id = thread::scope(|s| {
            let swap = &swap;
            let reader = s.spawn(move || {
                let guard = swap.load();
                assert_eq!(guard.id, held);
                to_writer.send(()).unwrap();
                reader_rx.recv().unwrap();
                drop(guard);
                to_writer.send(()).unwrap();
                thread::current().id()
            });
            writer_rx.recv().unwrap();
            // Overflows `held`, then sweeps the overflow while the guard lives.
            swap.store(probe(held + 1, &drops));
            swap.store(probe(held + 2, &drops));
            assert!(!freed(&drops).contains(&held), "{:?}", freed(&drops));
            to_reader.send(()).unwrap();
            writer_rx.recv().unwrap();
            assert!(!freed(&drops).contains(&held), "the reader's drop freed");
            reader.join().unwrap()
        });
        swap.store(probe(held + 3, &drops));
        let drops = drops.lock().unwrap().clone();
        let me = thread::current().id();
        assert!(drops.contains(&(held, me)), "{drops:?}");
        assert!(drops.iter().all(|(_, t)| *t != reader_id), "{drops:?}");
        drop(guards);
    }

    /// A retired value is freed after `store` releases the writer mutex, from
    /// the ring and from the overflow alike: its `Drop` sees the mutex free.
    #[test]
    fn retired_values_drop_after_the_writer_mutex_is_released() {
        struct Checked;
        static SWAP: std::sync::OnceLock<&'static RetiringSwap<Checked>> =
            std::sync::OnceLock::new();
        static LOCKED_AT_DROP: std::sync::Mutex<Vec<bool>> = std::sync::Mutex::new(Vec::new());
        impl Drop for Checked {
            fn drop(&mut self) {
                if let Some(swap) = SWAP.get() {
                    LOCKED_AT_DROP
                        .lock()
                        .unwrap()
                        .push(swap.retired.is_locked());
                }
            }
        }
        let swap = *SWAP.get_or_init(|| Box::leak(Box::new(RetiringSwap::new(Arc::new(Checked)))));
        let mut guards = Vec::new();
        for _ in 0..RING {
            guards.push(swap.load());
            swap.store(Arc::new(Checked));
        }
        // Overflow, then an overflow free of the unguarded displaced value.
        swap.store(Arc::new(Checked));
        swap.store(Arc::new(Checked));
        drop(guards);
        // Ring frees of the four, and the overflow's last entry.
        swap.store(Arc::new(Checked));
        let seen = LOCKED_AT_DROP.lock().unwrap().clone();
        assert_eq!(seen, vec![false; RING + 2]);
    }
}
