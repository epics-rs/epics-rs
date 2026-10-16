//! `recDynLink` against records of the same database: the connect sequence
//! (`connectCallback` → `getCallback` → search callback → monitor), puts
//! with and without completion, the get callback, and a cleared link's
//! events going nowhere.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::runtime::task::sleep;
use epics_base_rs::server::database::{AsyncDbHandle, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{FieldDesc, Record};
use epics_base_rs::types::{DbFieldType, EpicsValue, RecordName};
use sscan_rs::rec_dyn_link::{
    ACCESS_READ, ACCESS_WRITE, CaType, Deliver, FATAL_ERROR, Io, LinkHost, LinkSpec,
    NOTIFY_IN_PROGRESS, RecDynLink,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cb {
    Search,
    Monitor,
    Notify,
    Get,
}

static FIELDS: &[FieldDesc] = &[FieldDesc::new("VAL", DbFieldType::Double, false)];

#[derive(Default)]
struct DynTest {
    link: RecDynLink<Cb>,
    log: Vec<(Cb, i16)>,
    handle: Option<AsyncDbHandle>,
}

impl Record for DynTest {
    fn record_type(&self) -> &'static str {
        "dyn_test"
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

fn with_test<R>(db: &PvDatabase, f: impl FnOnce(&mut DynTest) -> R) -> R {
    let rec = db.get_record("T").unwrap();
    let mut inst = rec.write();
    f(inst
        .record
        .as_any_mut()
        .unwrap()
        .downcast_mut::<DynTest>()
        .unwrap())
}

fn deliver(handle: AsyncDbHandle) -> Deliver {
    Arc::new(move |generation, event| {
        handle.with_record("T", |r| {
            let t = r.as_any_mut().unwrap().downcast_mut::<DynTest>().unwrap();
            if let Some(cb) = t.link.apply(generation, event) {
                t.log.push((cb, t.link.status));
            }
        })
    })
}

fn add(db: &PvDatabase, pv: &str, io: Io, ca_type: CaType) -> i64 {
    with_test(db, |t| {
        let handle = t.handle.clone().unwrap();
        let spec = LinkSpec {
            io,
            ca_type,
            scalar: true,
            search_callback: Some(Cb::Search),
            monitor_callback: Some(Cb::Monitor),
        };
        t.link.add(
            LinkHost {
                handle: &handle,
                record: b"T",
            },
            pv,
            spec,
            deliver(handle.clone()),
        )
    })
}

async fn wait_for(db: &PvDatabase, cb: Cb, count: usize) {
    for _ in 0..500 {
        if with_test(db, |t| t.log.iter().filter(|(c, _)| *c == cb).count()) >= count {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "{cb:?} x{count} never arrived: {:?}",
        with_test(db, |t| t.log.clone())
    );
}

async fn ioc() -> Arc<PvDatabase> {
    let (db, _) = IocBuilder::new()
        .register_record_type("dyn_test", || Box::new(DynTest::default()))
        .db_string(
            r#"
record(dyn_test, "T") {}
record(ao, "M") {
  field(PREC, "3")
  field(EGU, "millimetre")
  field(DRVH, "10")
  field(DRVL, "-10")
  field(HOPR, "20")
  field(LOPR, "-20")
}
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db
}

#[epics_base_rs::epics_test]
async fn an_input_connects_then_reports_metadata_then_monitors() {
    let db = ioc().await;
    assert_eq!(add(&db, "M", Io::Input, CaType::Double), 0);
    wait_for(&db, Cb::Search, 1).await;
    with_test(&db, |t| {
        assert_eq!(t.link.connection_status(), 0);
        assert_eq!(t.link.control_limits(), Some((-10.0, 10.0)));
        assert_eq!(t.link.graphic_limits(), Some((-20.0, 20.0)));
        assert_eq!(t.link.precision(), Some(3));
        // `recDynLinkGetUnits(.., 8)` keeps seven characters.
        assert_eq!(t.link.units(8).as_deref(), Some("millime"));
        assert_eq!(t.link.check_access(), ACCESS_READ | ACCESS_WRITE);
        assert_eq!(t.link.nelem(), Some(1));
        assert_eq!(t.log[0], (Cb::Search, 0), "search precedes the monitor");
    });
    db.put_pv_and_post("M", EpicsValue::Double(2.5))
        .await
        .unwrap();
    for _ in 0..500 {
        if with_test(&db, |t| t.link.get(1) == Some(&[2.5][..])) {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    with_test(&db, |t| {
        assert_eq!(t.link.get(1), Some(&[2.5][..]), "{:?}", t.log)
    });
    wait_for(&db, Cb::Monitor, 1).await;

    let mut n = 5;
    assert_eq!(with_test(&db, |t| t.link.get_callback(&mut n, Cb::Get)), 0);
    assert_eq!(n, 1, "clamped to the element count");
    wait_for(&db, Cb::Get, 1).await;
    with_test(&db, |t| {
        assert_eq!(t.link.get(1), Some(&[2.5][..]));
        assert_eq!(t.log.last(), Some(&(Cb::Get, 0)));
    });
}

#[epics_base_rs::epics_test]
async fn an_output_puts_with_and_without_completion() {
    let db = ioc().await;
    assert_eq!(add(&db, "M", Io::Output, CaType::Float), 0);
    wait_for(&db, Cb::Search, 1).await;
    // An output has no monitor and refuses a get.
    let mut n = 1;
    assert_eq!(with_test(&db, |t| t.link.get_callback(&mut n, Cb::Get)), -1);

    assert_eq!(with_test(&db, |t| t.link.put_callback(4.0, Cb::Notify)), 0);
    wait_for(&db, Cb::Notify, 1).await;
    assert_eq!(db.get_pv("M").unwrap().to_f64(), Some(4.0));

    assert_eq!(with_test(&db, |t| t.link.put(-3.0)), 0);
    for _ in 0..500 {
        if db.get_pv("M").unwrap().to_f64() == Some(-3.0) {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(db.get_pv("M").unwrap().to_f64(), Some(-3.0));
    with_test(&db, |t| {
        assert!(!t.log.iter().any(|(c, _)| *c == Cb::Monitor))
    });
}

#[epics_base_rs::epics_test]
async fn a_second_put_callback_waits_for_the_first() {
    let db = ioc().await;
    add(&db, "M", Io::Output, CaType::Double);
    wait_for(&db, Cb::Search, 1).await;
    let (first, second) = with_test(&db, |t| {
        (
            t.link.put_callback(1.0, Cb::Notify),
            t.link.put_callback(2.0, Cb::Notify),
        )
    });
    assert_eq!((first, second), (0, NOTIFY_IN_PROGRESS));
    wait_for(&db, Cb::Notify, 1).await;
    assert_eq!(with_test(&db, |t| t.link.put_callback(2.0, Cb::Notify)), 0);
    wait_for(&db, Cb::Notify, 2).await;
    assert_eq!(db.get_pv("M").unwrap().to_f64(), Some(2.0));
}

#[epics_base_rs::epics_test]
async fn a_cleared_link_hears_nothing_more() {
    let db = ioc().await;
    add(&db, "M", Io::Input, CaType::Double);
    wait_for(&db, Cb::Search, 1).await;
    wait_for(&db, Cb::Monitor, 1).await;
    let before = with_test(&db, |t| {
        t.link.clear();
        t.log.len()
    });
    db.put_pv_and_post("M", EpicsValue::Double(7.0))
        .await
        .unwrap();
    sleep(Duration::from_millis(200)).await;
    with_test(&db, |t| {
        assert_eq!(t.log.len(), before);
        assert_eq!(t.link.connection_status(), -1);
        assert_eq!(t.link.get(1), None);
        assert_eq!(t.link.put(1.0), -1);
    });
}

#[epics_base_rs::epics_test]
async fn a_name_nobody_serves_never_connects() {
    let db = ioc().await;
    assert_eq!(add(&db, "NOBODY", Io::Output, CaType::Double), 0);
    assert_eq!(add(&db, "", Io::Output, CaType::Double), -1);
    sleep(Duration::from_millis(100)).await;
    with_test(&db, |t| {
        assert_eq!(t.link.connection_status(), -1);
        assert_eq!(t.link.put_callback(1.0, Cb::Notify), -1);
        assert!(t.log.is_empty());
        assert_ne!(t.link.status, FATAL_ERROR);
    });
}
