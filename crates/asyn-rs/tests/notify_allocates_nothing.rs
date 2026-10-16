//! `InterruptManager::notify` runs inline in the driver's notify, under a
//! record's hold (adapter.rs `notify`). With a ring-style synchronous client
//! and an enum-table mailbox client registered and the consumer parked, it
//! allocates and frees nothing, enum value changes included: the mailbox
//! coalesces to the latest instead of queueing.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use asyn_rs::interfaces::InterfaceType;
use asyn_rs::interrupt::{InterruptFilter, InterruptManager, InterruptValue};
use asyn_rs::param::{EnumEntry, ParamValue};

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static FREES: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
}

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if COUNTING.try_with(Cell::get).unwrap_or(false) {
            ALLOCS.fetch_add(1, Ordering::SeqCst);
        }
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        if COUNTING.try_with(Cell::get).unwrap_or(false) {
            FREES.fetch_add(1, Ordering::SeqCst);
        }
        unsafe { System.dealloc(p, l) }
    }
}

#[global_allocator]
static A: Counting = Counting;

fn counted<R>(f: impl FnOnce() -> R) -> (R, usize, usize) {
    let (a0, f0) = (ALLOCS.load(Ordering::SeqCst), FREES.load(Ordering::SeqCst));
    COUNTING.with(|c| c.set(true));
    let r = f();
    COUNTING.with(|c| c.set(false));
    (
        r,
        ALLOCS.load(Ordering::SeqCst) - a0,
        FREES.load(Ordering::SeqCst) - f0,
    )
}

fn entry(s: &str, v: i32) -> EnumEntry {
    EnumEntry {
        string: s.into(),
        value: v,
        severity: 0,
    }
}

fn enum_fire(reason: usize, choices: &Arc<[EnumEntry]>, index: usize) -> InterruptValue {
    InterruptValue {
        reason,
        addr: 0,
        value: ParamValue::Enum {
            index,
            choices: choices.clone(),
        },
        ..Default::default()
    }
}

#[test]
fn notify_with_a_ring_and_an_enum_client_allocates_and_frees_nothing() {
    const N: usize = 256;
    let im = InterruptManager::new();
    let filter = |iface| InterruptFilter {
        reason: Some(0),
        addr: Some(0),
        uint32_mask: None,
        iface,
    };

    let ring = Arc::new(AtomicUsize::new(0));
    let hits = ring.clone();
    let _sync = im.register_sync_callback(filter(None), move |_| {
        hits.fetch_add(1, Ordering::Relaxed);
    });
    let (_sub, _parked) = im.register_interrupt_user_when(
        filter(Some(InterfaceType::Enum)),
        asyn_rs::interrupt::MAX_ENUM_STATES * asyn_rs::interrupt::ENUM_ENTRY_BYTES,
        || true,
    );

    let table: Arc<[EnumEntry]> = Arc::from(vec![entry("OFF", 0), entry("ON", 1)]);
    let fires: Vec<InterruptValue> = (0..N).map(|i| enum_fire(0, &table, i % 2)).collect();

    // The iterator, and the buffer it frees on drop, stay outside the count.
    let mut it = fires.into_iter();
    let ((), allocs, frees) = counted(|| {
        for v in it.by_ref() {
            im.notify(v);
        }
    });
    drop(it);
    assert_eq!(ring.load(Ordering::Relaxed), N, "the ring client saw each");
    assert_eq!(
        (allocs, frees),
        (0, 0),
        "{N} enum fires, consumer parked: no allocation on the notify thread"
    );
}

/// `call_param_callbacks` is the flush a synchronous port's write reaches
/// under the record's hold: with an octet and an array parameter changed it
/// takes the changed set, reads each value and notifies, and allocates nothing.
#[test]
fn a_flush_with_an_octet_and_an_array_changed_allocates_nothing() {
    use asyn_rs::param::ParamType;
    use asyn_rs::port::{PortDriverBase, PortFlags};
    use epics_libcom_rs::runtime::interrupt_accept::InterruptGate;

    let mut base = PortDriverBase::new("flushAlloc", 1, PortFlags::default());
    base.interrupts = InterruptManager::owned_by(InterruptGate::opened());
    let msg = base.create_param("MSG", ParamType::Octet).unwrap();
    let wave = base.create_param("WAVE", ParamType::Int32Array).unwrap();
    let num = base.create_param("NUM", ParamType::Int32).unwrap();

    let seen = Arc::new(AtomicUsize::new(0));
    let hits = seen.clone();
    let _sync = base.interrupts.register_sync_callback(
        InterruptFilter {
            reason: None,
            addr: Some(0),
            uint32_mask: None,
            iface: None,
        },
        move |_| {
            hits.fetch_add(1, Ordering::Relaxed);
        },
    );
    let (_sub, _parked) = base.interrupts.register_interrupt_user(
        InterruptFilter {
            reason: None,
            addr: Some(0),
            uint32_mask: None,
            iface: None,
        },
        4096,
    );

    // Warm: the first flush may size internal tables.
    base.set_string_param(msg, 0, "warm").unwrap();
    base.set_int32_param(num, 0, 1).unwrap();
    base.call_param_callbacks(0).unwrap();

    base.set_string_param(msg, 0, "a long enough octet value")
        .unwrap();
    base.params
        .set_int32_array(wave, 0, vec![1, 2, 3, 4])
        .unwrap();
    base.set_int32_param(num, 0, 2).unwrap();

    let (r, allocs, frees) = counted(|| base.call_param_callbacks(0));
    r.unwrap();
    assert!(seen.load(Ordering::Relaxed) >= 3, "all three fired");
    assert_eq!(
        (allocs, frees),
        (0, 0),
        "octet + array + scalar flush: no allocation on the flush thread"
    );
}

/// C `paramVal::setString` (paramVal.cpp:271-281) compares, then assigns
/// into the string it owns. An octet set of a new value is one allocation —
/// the shared buffer — and an unchanged value allocates nothing, whatever
/// type the caller hands in.
#[test]
fn an_octet_set_allocates_once_for_a_new_value_and_never_for_an_unchanged_one() {
    use asyn_rs::param::ParamType;
    use asyn_rs::port::{PortDriverBase, PortFlags};

    let mut pl = PortDriverBase::new("ALLOCSET", 1, PortFlags::default());
    let msg = pl.create_param("MSG", ParamType::Octet).unwrap();
    pl.set_string_param(msg, 0, "first value").unwrap();

    let owned = String::from("second value");
    let (r, allocs, _) = counted(|| pl.set_string_param(msg, 0, &owned));
    r.unwrap();
    assert_eq!(allocs, 1, "a new value: the shared buffer, and no Vec");

    let (r, allocs, _) = counted(|| pl.set_string_param(msg, 0, "second value"));
    r.unwrap();
    assert_eq!(allocs, 0, "an unchanged &str");
    let (r, allocs, _) = counted(|| pl.set_string_param(msg, 0, &owned));
    r.unwrap();
    assert_eq!(allocs, 0, "an unchanged String");
    let bytes: &[u8] = b"second value";
    let (r, allocs, _) = counted(|| pl.set_string_param(msg, 0, bytes));
    r.unwrap();
    assert_eq!(allocs, 0, "an unchanged byte slice");
}

/// The changed-flag list is reserved to the parameter count at creation
/// (`createParam`, asynPortDriver.cpp:135): setting and flushing 256 params
/// in an order other than index order allocates nothing.
#[test]
fn setting_and_flushing_256_params_allocates_nothing() {
    use asyn_rs::param::ParamType;
    use asyn_rs::port::{PortDriverBase, PortFlags};

    let mut pl = PortDriverBase::new("ALLOCSET", 1, PortFlags::default());
    let ids: Vec<usize> = (0..256)
        .map(|i| pl.create_param(&format!("P{i}"), ParamType::Int32).unwrap())
        .collect();

    let mut fired = 0usize;
    let (r, allocs, frees) = counted(|| {
        for &i in ids.iter().rev() {
            pl.set_int32_param(i, 0, 7).unwrap();
        }
        pl.params.flush_changed(0, |_, _| {
            fired += 1;
            Ok(true)
        })
    });
    r.unwrap();
    assert_eq!(fired, 256);
    assert_eq!((allocs, frees), (0, 0));
}

/// One mailbox per reason, as records register: a flush with an octet, an
/// `Int32Array(64)` and an `Int32` changed neither allocates nor frees on the
/// flush thread. The mailbox copies the payload into a buffer sized at
/// registration instead of holding the `Arc`, so overwriting a slot drops
/// nothing the notifier owned the last reference to.
#[test]
fn a_flush_into_per_reason_mailboxes_neither_allocates_nor_frees() {
    use asyn_rs::param::ParamType;
    use asyn_rs::port::{PortDriverBase, PortFlags};
    use epics_libcom_rs::runtime::interrupt_accept::InterruptGate;

    let mut base = PortDriverBase::new("mbxFlush", 1, PortFlags::default());
    base.interrupts = InterruptManager::owned_by(InterruptGate::opened());
    let msg = base.create_param("MSG", ParamType::Octet).unwrap();
    let wave = base.create_param("WAVE", ParamType::Int32Array).unwrap();
    let num = base.create_param("NUM", ParamType::Int32).unwrap();

    let _subs: Vec<_> = [msg, wave, num]
        .into_iter()
        .map(|r| {
            base.interrupts.register_interrupt_user(
                InterruptFilter {
                    reason: Some(r),
                    addr: Some(0),
                    uint32_mask: None,
                    iface: None,
                },
                256,
            )
        })
        .collect();

    // Each set builds a fresh buffer, as a driver's write does: the flush
    // then sees the previous one's last reference go.
    let mut flush = |i: i32| {
        base.set_string_param(msg, 0, format!("message number {i}"))
            .unwrap();
        base.params
            .set_int32_array(wave, 0, (0..64).map(|k| k + i).collect::<Vec<i32>>())
            .unwrap();
        base.set_int32_param(num, 0, i).unwrap();
        counted(|| base.call_param_callbacks(0))
    };
    for i in 0..2 {
        flush(i).0.unwrap();
    }
    let (mut allocs, mut frees) = (0, 0);
    for i in 2..258 {
        let (r, a, f) = flush(i);
        r.unwrap();
        allocs += a;
        frees += f;
    }
    assert_eq!(
        (allocs, frees),
        (0, 0),
        "256 flushes, octet + Int32Array(64) + Int32 changed"
    );
}

/// A value longer than the registered size is clamped as C clamps it: an
/// octet keeps `valSize - 1` bytes, the NUL taking the last
/// (devAsynOctet.c:489), an array keeps whole elements up to `NELM`
/// (devAsynXXXArray.cpp:415).
#[epics_base_rs::epics_test]
async fn a_mailbox_clamps_a_payload_to_its_registered_size() {
    use asyn_rs::param::ParamValue;

    let im = InterruptManager::new();
    let (_sub, mut rx) = im.register_interrupt_user(
        InterruptFilter {
            reason: Some(0),
            addr: Some(0),
            uint32_mask: None,
            iface: None,
        },
        16,
    );
    im.notify(InterruptValue {
        reason: 0,
        addr: 0,
        value: ParamValue::Octet(Arc::from(&b"0123456789abcdefOVERFLOW"[..])),
        ..Default::default()
    });
    im.notify(InterruptValue {
        reason: 0,
        addr: 0,
        value: ParamValue::Int32Array(Arc::from((0..10).collect::<Vec<i32>>())),
        ..Default::default()
    });
    // The mailbox coalesces: the array is the latest, clamped to 16/4 = 4.
    let got = rx.recv().await.unwrap();
    match got.value {
        ParamValue::Int32Array(a) => assert_eq!(&a[..], &[0, 1, 2, 3]),
        other => panic!("expected the array, got {other:?}"),
    }
    im.notify(InterruptValue {
        reason: 0,
        addr: 0,
        value: ParamValue::Octet(Arc::from(&b"0123456789abcdefOVERFLOW"[..])),
        ..Default::default()
    });
    match rx.recv().await.unwrap().value {
        ParamValue::Octet(b) => assert_eq!(&b[..], b"0123456789abcde"),
        other => panic!("expected the octet, got {other:?}"),
    }
}

/// The registered size is the caller's, not a constant: a 100000-element
/// `Float64Array` into a mailbox registered for it arrives whole.
#[epics_base_rs::epics_test]
async fn a_mailbox_registered_for_a_large_array_delivers_it_whole() {
    use asyn_rs::param::ParamValue;

    let im = InterruptManager::new();
    let (_sub, mut rx) = im.register_interrupt_user(
        InterruptFilter {
            reason: Some(0),
            addr: Some(0),
            uint32_mask: None,
            iface: None,
        },
        100_000 * 8,
    );
    let src: Vec<f64> = (0..100_000).map(f64::from).collect();
    im.notify(InterruptValue {
        reason: 0,
        addr: 0,
        value: ParamValue::Float64Array(Arc::from(src.clone())),
        ..Default::default()
    });
    match rx.recv().await.unwrap().value {
        ParamValue::Float64Array(a) => assert_eq!(&a[..], &src[..]),
        other => panic!("expected the array, got {other:?}"),
    }
}

/// An unfiltered async subscriber owns a ring of copied elements, so the flush
/// retains nothing: with one present, 256 flushes of octet + `Int32Array(64)` +
/// `Int32` neither allocate nor free on the flush thread. The tokio broadcast
/// this replaces held an `Arc` clone per element and dropped the overwritten
/// one under the hold.
#[test]
fn a_flush_with_an_async_subscriber_neither_allocates_nor_frees() {
    use asyn_rs::param::ParamType;
    use asyn_rs::port::{PortDriverBase, PortFlags};
    use epics_libcom_rs::runtime::interrupt_accept::InterruptGate;

    let mut base = PortDriverBase::new("asyncFlush", 1, PortFlags::default());
    base.interrupts = InterruptManager::owned_by(InterruptGate::opened());
    let msg = base.create_param("MSG", ParamType::Octet).unwrap();
    let wave = base.create_param("WAVE", ParamType::Int32Array).unwrap();
    let num = base.create_param("NUM", ParamType::Int32).unwrap();
    // Ring shorter than the 256 x 3 elements the loop fires, so it overflows
    // and overwrites on every flush after the first few.
    let _rx = base.interrupts.subscribe_async(8, 256);

    let mut flush = |i: i32| {
        base.set_string_param(msg, 0, format!("message number {i}"))
            .unwrap();
        base.params
            .set_int32_array(wave, 0, (0..64).map(|k| k + i).collect::<Vec<i32>>())
            .unwrap();
        base.set_int32_param(num, 0, i).unwrap();
        counted(|| base.call_param_callbacks(0))
    };
    for i in 0..4 {
        flush(i).0.unwrap();
    }
    let (mut allocs, mut frees) = (0, 0);
    for i in 4..260 {
        let (r, a, f) = flush(i);
        r.unwrap();
        allocs += a;
        frees += f;
    }
    assert_eq!(
        (allocs, frees),
        (0, 0),
        "256 flushes with an async subscriber attached"
    );
}

/// C's ring drops its oldest element when full and keeps the rest in arrival
/// order (devAsynOctet.c:528-560); the consumer is told once how many went.
#[epics_base_rs::epics_test]
async fn a_full_async_ring_drops_the_oldest_and_keeps_arrival_order() {
    use asyn_rs::interrupt::{AsyncRecvError, AsyncTryRecvError};
    use asyn_rs::param::ParamValue;

    let im = InterruptManager::new();
    let mut rx = im.subscribe_async(3, 0);
    for v in 0..5 {
        im.notify(InterruptValue {
            reason: 0,
            addr: 0,
            value: ParamValue::Int32(v),
            ..Default::default()
        });
    }
    // Five arrived into a ring of three: 0 and 1 were dropped.
    assert_eq!(rx.recv().await.unwrap_err(), AsyncRecvError::Lagged(2));
    for want in [2, 3, 4] {
        match rx.recv().await.unwrap().value {
            ParamValue::Int32(got) => assert_eq!(got, want),
            other => panic!("expected Int32({want}), got {other:?}"),
        }
    }
    assert_eq!(rx.try_recv().unwrap_err(), AsyncTryRecvError::Empty);
    drop(im);
    assert_eq!(rx.recv().await.unwrap_err(), AsyncRecvError::Closed);
}
