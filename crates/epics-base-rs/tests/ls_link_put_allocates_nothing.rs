//! An lso OUT put allocates nothing. C `dbPutLinkLS` (`dbLink.c:527-538`)
//! puts from the record's own VAL buffer, as one `DBR_STRING` into a field
//! that is not `DBF_CHAR`/`DBF_UCHAR`. Counted with this binary's own
//! allocator on the calling thread: a cycle with the OUT link against the
//! same cycle without it, nobody subscribed. A scalar into a numeric
//! waveform is element 0 of its buffer (`dbAccess.c:1350-1367`); a buffer a
//! monitor still holds is copied, as before.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};

struct Counting;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            ALLOCS.with(|n| n.set(n.get() + 1));
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            ALLOCS.with(|n| n.set(n.get() + 1));
        }
        unsafe { System.realloc(ptr, layout, size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

const WF_LONG: &str = r#"record(waveform, "T") { field(FTVL, "LONG") field(NELM, "8") }"#;

async fn build(db: &str) -> Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(db, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

/// Allocations per process cycle of `src`'s record `S`, an lso holding
/// `text` when given, with T subscribed when `watch`.
async fn per_cycle(src: &str, text: Option<&str>, target: &str, watch: bool) -> usize {
    let db = build(&format!("{src}\n{target}")).await;
    if let Some(text) = text {
        let mut chars = text.as_bytes().to_vec();
        chars.push(0);
        db.put_record_field_from_ca("S", "VAL$", EpicsValue::CharArray(chars.into()))
            .await
            .unwrap();
    }
    let _reader: Option<EventReader> = watch.then(|| {
        let mask = (EventMask::VALUE | EventMask::LOG).bits();
        db.get_record("T")
            .unwrap()
            .write()
            .add_subscriber("VAL", 1, DbFieldType::Long, mask)
            .unwrap()
    });
    for _ in 0..10 {
        db.process_record("S").await.unwrap();
    }
    const N: usize = 100;
    ALLOCS.with(|n| n.set(0));
    COUNTING.with(|c| c.set(true));
    for _ in 0..N {
        db.process_record("S").await.unwrap();
    }
    COUNTING.with(|c| c.set(false));
    assert!(
        epics_base_rs::runtime::task::in_test_ioc_thread(),
        "counted off an IOC thread"
    );
    ALLOCS.with(Cell::get) / N
}

fn lso(out: &str) -> String {
    format!(r#"record(lso, "S") {{ field(SIZV, "64") field(OUT, "{out}") }}"#)
}

async fn put_allocs(target: &str, text: &str) -> usize {
    let with = per_cycle(&lso("T.VAL NPP"), Some(text), target, false).await;
    let without = per_cycle(&lso(""), Some(text), target, false).await;
    with.saturating_sub(without)
}

#[test]
fn an_lso_put_into_ao_allocates_nothing() {
    epics_base_rs::runtime::task::test_ioc_thread(|| {
        epics_base_rs::runtime::task::test_block_on(an_lso_put_into_ao_allocates_nothing_body())
    });
}

async fn an_lso_put_into_ao_allocates_nothing_body() {
    assert_eq!(put_allocs(r#"record(ao, "T") {}"#, "12.5").await, 0);
}

#[test]
fn an_lso_put_into_longout_allocates_nothing() {
    epics_base_rs::runtime::task::test_ioc_thread(|| {
        epics_base_rs::runtime::task::test_block_on(an_lso_put_into_longout_allocates_nothing_body())
    });
}

async fn an_lso_put_into_longout_allocates_nothing_body() {
    assert_eq!(put_allocs(r#"record(longout, "T") {}"#, "42").await, 0);
}

#[test]
fn an_lso_put_into_a_char_waveform_allocates_nothing() {
    epics_base_rs::runtime::task::test_ioc_thread(|| {
        epics_base_rs::runtime::task::test_block_on(
            an_lso_put_into_a_char_waveform_allocates_nothing_body(),
        )
    });
}

async fn an_lso_put_into_a_char_waveform_allocates_nothing_body() {
    let wf = r#"record(waveform, "T") { field(FTVL, "CHAR") field(NELM, "16") }"#;
    assert_eq!(put_allocs(wf, "hello").await, 0);
}

#[test]
fn an_lso_put_into_a_long_waveform_allocates_nothing() {
    epics_base_rs::runtime::task::test_ioc_thread(|| {
        epics_base_rs::runtime::task::test_block_on(
            an_lso_put_into_a_long_waveform_allocates_nothing_body(),
        )
    });
}

async fn an_lso_put_into_a_long_waveform_allocates_nothing_body() {
    assert_eq!(put_allocs(WF_LONG, "7").await, 0);
}

#[test]
fn an_ao_put_into_a_long_waveform_allocates_nothing() {
    epics_base_rs::runtime::task::test_ioc_thread(|| {
        epics_base_rs::runtime::task::test_block_on(
            an_ao_put_into_a_long_waveform_allocates_nothing_body(),
        )
    });
}

async fn an_ao_put_into_a_long_waveform_allocates_nothing_body() {
    let ao = |out: &str| format!(r#"record(ao, "S") {{ field(VAL, "3.9") field(OUT, "{out}") }}"#);
    let with = per_cycle(&ao("T.VAL NPP"), None, WF_LONG, false).await;
    let without = per_cycle(&ao(""), None, WF_LONG, false).await;
    assert_eq!(with.saturating_sub(without), 0);
}

/// A caput of a scalar into `T.VAL` against one into `T.PROC`: both process
/// T, so the difference is the VAL store.
#[test]
fn a_scalar_caput_into_a_long_waveform_allocates_nothing() {
    epics_base_rs::runtime::task::test_ioc_thread(|| {
        epics_base_rs::runtime::task::test_block_on(
            a_scalar_caput_into_a_long_waveform_allocates_nothing_body(),
        )
    });
}

async fn a_scalar_caput_into_a_long_waveform_allocates_nothing_body() {
    let db = build(WF_LONG).await;
    let mut per = [0usize; 2];
    for (i, (field, value)) in [
        ("VAL", EpicsValue::Double(3.9)),
        ("PROC", EpicsValue::Short(1)),
    ]
    .into_iter()
    .enumerate()
    {
        for _ in 0..10 {
            db.put_record_field_from_ca("T", field, value.clone())
                .await
                .unwrap();
        }
        const N: usize = 100;
        let values: Vec<EpicsValue> = (0..N).map(|_| value.clone()).collect();
        ALLOCS.with(|n| n.set(0));
        COUNTING.with(|c| c.set(true));
        for v in values {
            db.put_record_field_from_ca("T", field, v).await.unwrap();
        }
        COUNTING.with(|c| c.set(false));
        assert!(
            epics_base_rs::runtime::task::in_test_ioc_thread(),
            "counted off an IOC thread"
        );
        per[i] = ALLOCS.with(Cell::get) / N;
    }
    assert_eq!(
        per[0].saturating_sub(per[1]),
        0,
        "VAL {} PROC {}",
        per[0],
        per[1]
    );
}

/// A monitor on T: its post does not hold the buffer, so a PP put into VAL
/// costs what one into PROC does.
#[test]
fn an_lso_put_into_a_monitored_long_waveform_allocates_nothing() {
    epics_base_rs::runtime::task::test_ioc_thread(|| {
        epics_base_rs::runtime::task::test_block_on(
            an_lso_put_into_a_monitored_long_waveform_allocates_nothing_body(),
        )
    });
}

async fn an_lso_put_into_a_monitored_long_waveform_allocates_nothing_body() {
    let val = per_cycle(&lso("T.VAL PP"), Some("7"), WF_LONG, true).await;
    let proc = per_cycle(&lso("T.PROC PP"), Some("7"), WF_LONG, true).await;
    assert_eq!(val.saturating_sub(proc), 0, "VAL {val} PROC {proc}");
}

/// A VAL snapshot held across the put shares T's buffer: the put copies it
/// and the snapshot keeps the old element.
#[test]
fn an_lso_put_into_a_shared_long_waveform_copies_the_buffer() {
    epics_base_rs::runtime::task::test_ioc_thread(|| {
        epics_base_rs::runtime::task::test_block_on(
            an_lso_put_into_a_shared_long_waveform_copies_the_buffer_body(),
        )
    });
}

async fn an_lso_put_into_a_shared_long_waveform_copies_the_buffer_body() {
    let mut per = [0usize; 2];
    for (i, out) in ["T.VAL NPP", ""].into_iter().enumerate() {
        let db = build(&format!("{}\n{WF_LONG}", lso(out))).await;
        db.put_record_field_from_ca("S", "VAL$", EpicsValue::CharArray(b"7\0".to_vec().into()))
            .await
            .unwrap();
        db.put_record_field_from_ca("T", "VAL", EpicsValue::LongArray(vec![1, 2].into()))
            .await
            .unwrap();
        const N: usize = 100;
        let mut allocs = 0;
        for _ in 0..N {
            let snapshot = db.get_pv("T").unwrap();
            ALLOCS.with(|n| n.set(0));
            COUNTING.with(|c| c.set(true));
            db.process_record("S").await.unwrap();
            COUNTING.with(|c| c.set(false));
            assert!(
                epics_base_rs::runtime::task::in_test_ioc_thread(),
                "counted off an IOC thread"
            );
            allocs += ALLOCS.with(Cell::get);
            if i == 0 {
                assert_eq!(snapshot, EpicsValue::LongArray(vec![1, 2].into()));
                assert_eq!(
                    db.get_pv("T").unwrap(),
                    EpicsValue::LongArray(vec![7].into())
                );
                db.put_record_field_from_ca("T", "VAL", EpicsValue::LongArray(vec![1, 2].into()))
                    .await
                    .unwrap();
            }
            drop(snapshot);
        }
        per[i] = allocs / N;
    }
    assert!(per[0] > per[1], "with {} without {}", per[0], per[1]);
}
