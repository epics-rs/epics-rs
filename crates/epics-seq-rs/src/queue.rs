//! `QUEUE` (`seq_queue.c`): the bounded queue of a `syncQ` channel.
//!
//! C's ring of `numElems` slots plus an overflow flag holds up to
//! `numElems` elements, and a put on a full queue overwrites the newest
//! one. That is a bounded FIFO, kept here as a `VecDeque` reserved at
//! creation so a put or get never allocates.

use std::collections::VecDeque;

use epics_base_rs::runtime::log::{ErrlogSevEnum, errlog_sev_printf};
use epics_base_rs::types::EpicsValue;
use parking_lot::Mutex;

use crate::pvt::PvMeta;

/// `seqQueueMaxNumElems`.
const MAX_NUM_ELEMS: usize = usize::MAX >> 1;

/// One queued value of a `syncQ` channel.
pub struct QueueElem {
    pub value: EpicsValue,
    pub meta: PvMeta,
}

/// A queue of `num_elems` whose put, when full, overwrites the newest
/// element. Its lock is a leaf.
pub struct SeqQueue {
    pub num_elems: usize,
    pub elems: Mutex<VecDeque<QueueElem>>,
}

impl SeqQueue {
    /// `seqQueueCreate`: `None` for 0 elements, more than C's
    /// `seqQueueMaxNumElems` or a failed allocation, with C's errlog line.
    pub fn new(num_elems: usize) -> Option<SeqQueue> {
        if num_elems == 0 {
            errlog_sev_printf(
                ErrlogSevEnum::Fatal,
                "seqQueueCreate: numElems must be positive\n",
            );
            return None;
        }
        if num_elems > MAX_NUM_ELEMS {
            errlog_sev_printf(ErrlogSevEnum::Fatal, "seqQueueCreate: numElems too large\n");
            return None;
        }
        let mut elems = VecDeque::new();
        if elems.try_reserve_exact(num_elems).is_err() {
            errlog_sev_printf(ErrlogSevEnum::Fatal, "seqQueueCreate: out of memory\n");
            return None;
        }
        Some(SeqQueue {
            num_elems,
            elems: Mutex::new(elems),
        })
    }

    /// `seqQueuePut`: true when the queue was already full and its newest
    /// element was overwritten.
    pub fn put(&self, elem: QueueElem) -> bool {
        let mut q = self.elems.lock();
        if q.len() < self.num_elems {
            q.push_back(elem);
            return false;
        }
        match q.back_mut() {
            Some(last) => *last = elem,
            None => unreachable!("a full queue has an element"),
        }
        true
    }

    /// `seqQueueGet`: the oldest element, `None` when empty.
    pub fn get(&self) -> Option<QueueElem> {
        self.elems.lock().pop_front()
    }

    /// `seqQueueFlush`.
    pub fn flush(&self) {
        self.elems.lock().clear()
    }

    /// `seqQueueUsed`.
    pub fn used(&self) -> usize {
        self.elems.lock().len()
    }

    /// `seqQueueFree`.
    pub fn free(&self) -> usize {
        self.num_elems - self.used()
    }

    /// `seqQueueIsEmpty`.
    pub fn is_empty(&self) -> bool {
        self.elems.lock().is_empty()
    }

    /// `seqQueueIsFull`.
    pub fn is_full(&self) -> bool {
        self.used() == self.num_elems
    }

    /// `seqQueueNumElems`.
    pub fn num_elems(&self) -> usize {
        self.num_elems
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use epics_base_rs::types::WallTime;

    use super::*;
    use crate::types::{PvSevr, PvStat};

    fn elem(n: i32) -> QueueElem {
        QueueElem {
            value: EpicsValue::Long(n),
            meta: PvMeta {
                timestamp: WallTime::UNIX_EPOCH,
                status: PvStat::Ok,
                severity: PvSevr::None,
                message: crate::types::CharPtr::default(),
            },
        }
    }

    fn value(e: QueueElem) -> i32 {
        match e.value {
            EpicsValue::Long(n) => n,
            v => panic!("{v:?}"),
        }
    }

    /// `queueTest.c` `check`.
    fn check(q: &SeqQueue, expected_free: usize) {
        let expected_used = q.num_elems - expected_free;
        assert_eq!(q.free(), expected_free);
        assert_eq!(q.used(), expected_used);
        assert_eq!(q.is_empty(), expected_used == 0);
        assert_eq!(q.is_full(), expected_free == 0);
    }

    #[test]
    fn a_zero_size_queue_is_refused() {
        assert!(SeqQueue::new(0).is_none());
    }

    /// `queueTest.c`'s sequential test: put `i` elements, get them back,
    /// for each size and `i` up to twice the size.
    #[test]
    fn a_full_queue_overwrites_its_newest_element() {
        for n in 1..=3usize {
            let q = SeqQueue::new(n).unwrap();
            check(&q, n);
            for i in 1..=2 * n {
                for j in 0..i {
                    assert_eq!(q.put(elem(j as i32)), j >= n, "n={n} i={i} put {j}");
                }
                check(&q, n.saturating_sub(i));
                let got: Vec<_> = (0..i).map(|_| q.get().map(value)).collect();
                check(&q, n);
                for (j, g) in got.into_iter().enumerate() {
                    let want = if j + 1 < n {
                        Some(j as i32)
                    } else if j + 1 == n {
                        Some(i as i32 - 1)
                    } else {
                        None
                    };
                    assert_eq!(g, want, "n={n} i={i} get {j}");
                }
            }
        }
    }

    #[test]
    fn flush_empties_a_full_queue() {
        let q = SeqQueue::new(2).unwrap();
        q.put(elem(1));
        q.put(elem(2));
        q.flush();
        check(&q, 2);
        assert!(q.get().is_none());
        assert!(!q.put(elem(3)));
        assert_eq!(q.get().map(value), Some(3));
    }

    /// `queueTest.c`'s concurrent test: what the reader skips is exactly
    /// what the writer overwrote.
    #[test]
    fn a_concurrent_reader_loses_what_the_writer_overwrote() {
        const ITERATIONS: i32 = 100_000;
        for n in 1..=20 {
            let q = Arc::new(SeqQueue::new(n).unwrap());
            let writer = {
                let q = Arc::clone(&q);
                std::thread::spawn(move || (0..ITERATIONS).filter(|&i| q.put(elem(i))).count())
            };
            let mut reader_lost = 0;
            let mut i = 0;
            while i < ITERATIONS {
                let j = loop {
                    match q.get() {
                        Some(e) => break value(e),
                        None => std::hint::spin_loop(),
                    }
                };
                assert!(j >= i && j < ITERATIONS, "n={n}: {j} after {i}");
                reader_lost += (j - i) as usize;
                i = j + 1;
            }
            assert_eq!(reader_lost, writer.join().unwrap(), "n={n}");
        }
    }
}
