//! C's `recDynInp` makes every input's search in the order the links were
//! added (`recDynLink.c:830-842`), and the one event context it creates
//! hands every local link's monitors over in post order: a link held in
//! its callback is not overtaken by another link. A link's clear comes
//! after the commands queued before it.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::runtime::task::sleep;
use epics_base_rs::server::database::{AsyncDbHandle, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::rec_dyn_link::{
    CaType, Deliver, Io, LinkEvent, LinkHost, LinkSpec, RecDynLink,
};
use epics_base_rs::server::record::{FieldDesc, Record};
use epics_base_rs::types::{DbFieldType, EpicsValue, RecordName};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cb {
    Search,
    Monitor,
}

static FIELDS: &[FieldDesc] = &[FieldDesc::new("VAL", DbFieldType::Double, false)];

#[derive(Default)]
struct TwoLinks {
    links: [RecDynLink<Cb>; 2],
    log: Vec<(usize, Cb)>,
    handle: Option<AsyncDbHandle>,
}

impl Record for TwoLinks {
    fn record_type(&self) -> &'static str {
        "two_links"
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        (name == "VAL").then_some(EpicsValue::Double(0.0))
    }
    fn put_field(&mut self, name: &str, _value: EpicsValue) -> CaResult<()> {
        Err(CaError::FieldNotFound(name.into()))
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        FIELDS
    }
    fn set_async_context(&mut self, _name: RecordName, db: AsyncDbHandle) {
        self.handle = Some(db);
    }
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

fn with_t<R>(db: &PvDatabase, f: impl FnOnce(&mut TwoLinks) -> R) -> R {
    let rec = db.get_record("T").unwrap();
    let mut inst = rec.write();
    f(inst
        .record
        .as_any_mut()
        .unwrap()
        .downcast_mut::<TwoLinks>()
        .unwrap())
}

fn logged(db: &PvDatabase, cb: Cb) -> Vec<usize> {
    with_t(db, |t| {
        t.log
            .iter()
            .filter(|(_, c)| *c == cb)
            .map(|(li, _)| *li)
            .collect()
    })
}

/// Link `li`'s events, applied under T's lock; `hold` runs first, on the
/// delivering thread, with no lock held.
fn deliver(
    handle: AsyncDbHandle,
    li: usize,
    hold: impl Fn(&LinkEvent) + Send + Sync + 'static,
) -> Deliver {
    Arc::new(move |generation, event| {
        hold(&event);
        handle.with_record("T", |r| {
            let t = r.as_any_mut().unwrap().downcast_mut::<TwoLinks>().unwrap();
            if let Some(cb) = t.links[li].apply(generation, event) {
                t.log.push((li, cb));
            }
        })
    })
}

/// Hold the calling thread until `done`, for at most a fifth of a second:
/// long enough for any other thread to overtake.
fn hold_until(done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_millis(200);
    while !done() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Add an input on `M1` to link 0 and one on `M2` to link 1, in that order,
/// link 0's events going through `hold` first.
fn add_both(db: &Arc<PvDatabase>, hold: impl Fn(&LinkEvent) + Send + Sync + 'static) {
    let hold = Arc::new(hold);
    with_t(db, |t| {
        let handle = t.handle.clone().unwrap();
        for (li, pv) in ["M1", "M2"].into_iter().enumerate() {
            let spec = LinkSpec {
                io: Io::Input,
                ca_type: CaType::Double,
                scalar: true,
                search_callback: Some(Cb::Search),
                monitor_callback: Some(Cb::Monitor),
            };
            let hold = hold.clone();
            let deliver = deliver(handle.clone(), li, move |event| {
                if li == 0 {
                    hold(event)
                }
            });
            let host = LinkHost {
                handle: &handle,
                record: b"T",
            };
            assert_eq!(t.links[li].add(host, pv, spec, deliver), 0);
        }
    });
}

async fn wait_for(db: &PvDatabase, cb: Cb, count: usize) {
    let deadline = Instant::now() + fact_budget::FACT_BUDGET;
    while logged(db, cb).len() < count {
        assert!(
            Instant::now() < deadline,
            "{cb:?} x{count} never arrived: {:?}",
            with_t(db, |t| t.log.clone())
        );
        sleep(Duration::from_millis(5)).await;
    }
}

async fn ioc() -> Arc<PvDatabase> {
    let (db, _) = IocBuilder::new()
        .register_record_type("two_links", || Box::new(TwoLinks::default()))
        .db_string(
            r#"
record(two_links, "T") {}
record(ao, "M1") {}
record(ao, "M2") {}
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db
}

/// `recDynLinkClear` queues `cmdClear` behind the link's put, so the put
/// reaches its channel even when `recDynOut` is still held on another
/// link's put.
#[cfg_attr(
    tokio_backend,
    tokio::test(flavor = "multi_thread", worker_threads = 4)
)]
#[cfg_attr(exec_backend, epics_base_rs::epics_test)]
async fn a_put_queued_before_the_clear_is_made() {
    let db = ioc().await;
    with_t(&db, |t| {
        let handle = t.handle.clone().unwrap();
        for (li, pv) in ["M1", "M2"].into_iter().enumerate() {
            let spec = LinkSpec {
                io: Io::Output,
                ca_type: CaType::Double,
                scalar: true,
                search_callback: Some(Cb::Search),
                monitor_callback: None,
            };
            let host = LinkHost {
                handle: &handle,
                record: b"T",
            };
            let deliver = deliver(handle.clone(), li, |_| {});
            assert_eq!(t.links[li].add(host, pv, spec, deliver), 0);
        }
    });
    wait_for(&db, Cb::Search, 2).await;
    let m1 = db.get_record("M1").unwrap();
    let held = m1.write();
    with_t(&db, |t| {
        assert_eq!(t.links[0].put(1.0), 0);
        assert_eq!(t.links[1].put(5.0), 0);
        t.links[1].clear();
    });
    drop(held);
    let deadline = Instant::now() + fact_budget::FACT_BUDGET;
    while db.get_pv("M2").unwrap() != EpicsValue::Double(5.0) && Instant::now() < deadline {
        sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        (db.get_pv("M1").unwrap(), db.get_pv("M2").unwrap()),
        (EpicsValue::Double(1.0), EpicsValue::Double(5.0)),
        "M1, then M2 put before its link's clear"
    );
}

#[cfg_attr(
    tokio_backend,
    tokio::test(flavor = "multi_thread", worker_threads = 4)
)]
#[cfg_attr(exec_backend, epics_base_rs::epics_test)]
async fn searches_are_made_in_add_order() {
    let db = ioc().await;
    let watch = db.clone();
    add_both(&db, move |event| {
        if matches!(event, LinkEvent::Up { .. }) {
            hold_until(|| !logged(&watch, Cb::Search).is_empty());
        }
    });
    wait_for(&db, Cb::Search, 2).await;
    assert_eq!(logged(&db, Cb::Search), [0, 1]);
}

#[cfg_attr(
    tokio_backend,
    tokio::test(flavor = "multi_thread", worker_threads = 4)
)]
#[cfg_attr(exec_backend, epics_base_rs::epics_test)]
async fn monitors_arrive_in_post_order() {
    let db = ioc().await;
    let watch = db.clone();
    add_both(&db, move |event| {
        // Link 0's monitor of the put below, not its first.
        if matches!(event, LinkEvent::Monitor(v) if v.values.first() == Some(&1.0)) {
            hold_until(|| logged(&watch, Cb::Monitor).len() > 2);
        }
    });
    wait_for(&db, Cb::Monitor, 2).await;
    db.put_pv_and_post("M1", EpicsValue::Double(1.0))
        .await
        .unwrap();
    db.put_pv_and_post("M2", EpicsValue::Double(1.0))
        .await
        .unwrap();
    wait_for(&db, Cb::Monitor, 4).await;
    assert_eq!(logged(&db, Cb::Monitor)[2..], [0, 1]);
}
