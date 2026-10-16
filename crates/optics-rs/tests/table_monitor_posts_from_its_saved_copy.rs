//! table's `monitor()` (`tableRecord.c:860-918`) against oracleTableIoc,
//! three `camonitor -m v|a|l` clients on `T.X T.Y T.AX T.XRB T.M0X T.M0Y T.M1Y
//! T.X0 T.LVIO T.ZERO`, driven with `caput T.PROC 1; T.X 1; T.PROC 1; T.Y 2;
//! T.AX 0.5; T.ZERO 1; T.PROC 1; T.X 1`. The streams below are the oracle's
//! with the timestamps and the connect values cut.
//!
//! The boundaries: a field posts when it moved off the copy
//! `SaveRecordValues` took at the top of the cycle, never on an alarm move;
//! nothing reaches `DBE_ALARM`; a drive put posts from `dbPut` with
//! `DBE_VALUE|DBE_LOG` and the cycle's own posts carry `DBE_VALUE` alone;
//! ZERO's `special()` zeroes the user coordinates and its put a limit
//! violation (`LVIO`) restores X. The oracle reads X 0, M0X 1, XRB -1, LVIO 1
//! after the steps.
//!
//! Compared per PV: C posts each axis's fields together in `monitor()`'s loop
//! order, the port's `monitor_cycle` in its subscriber walk's.

// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — passes on the exec backend
// (`EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p optics-rs
// --test table_monitor_posts_from_its_saved_copy`); no CA server is built.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::types::{DbFieldType, EpicsValue};
use optics_rs::records::table::TableRecord;

const DB: &str = r#"
record(table, "T") {
  field(LX, "100")
  field(LZ, "200")
}
"#;

const STEPS: [(&str, &str); 8] = [
    ("PROC", "1"),
    ("X", "1"),
    ("PROC", "1"),
    ("Y", "2"),
    ("AX", "0.5"),
    ("ZERO", "1"),
    ("PROC", "1"),
    ("X", "1"),
];

const VALUE: &str = "\
T.X 1
T.M0X 1
T.Y 2
T.M0Y 2
T.M1Y 2
T.AX 0.5
T.ZERO 1
T.ZERO 1
T.ZERO 0
T.AX 0
T.X 0
T.XRB -1
T.X0 1
T.Y 0
T.LVIO 1
T.X 1
T.X 0
";

const LOG: &str = "\
T.X 1
T.Y 2
T.AX 0.5
T.ZERO 1
T.X 1
";

/// Appends each event it passes, as `camonitor` prints it without the time.
struct Line {
    pv: &'static str,
    stream: Arc<Mutex<String>>,
}

impl SubscriptionFilter for Line {
    fn name(&self) -> &'static str {
        "line"
    }

    fn apply(&self, event: FilteredMonitorEvent) -> Option<FilteredMonitorEvent> {
        let e = &event.event;
        let value = match e.value().as_value() {
            Some(EpicsValue::Double(v)) => format!("{v}"),
            Some(EpicsValue::Short(v)) => format!("{v}"),
            other => panic!("{}: {other:?}", self.pv),
        };
        let mut line = format!("{} {value}", self.pv);
        if e.sevr() != 0 {
            line += &format!(
                " {} {}",
                alarm_condition_string(e.stat()),
                MENU_ALARM_SEVR[e.sevr() as usize]
            );
        }
        let mut stream = self.stream.lock().unwrap();
        stream.push_str(&line);
        stream.push('\n');
        Some(event)
    }
}

#[tokio::test]
async fn table_posts_from_its_saved_copy_as_the_oracle_does() {
    let (db, _) = IocBuilder::new()
        .register_record_type("table", || Box::new(TableRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let cell = db.get_record("T").unwrap();
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let mut readers = Vec::new();
    {
        let mut inst = cell.write();
        let mut sid = 0;
        for (class, stream) in classes.iter().zip(&streams) {
            for (pv, field, ty) in [
                ("T.X", "X", DbFieldType::Double),
                ("T.Y", "Y", DbFieldType::Double),
                ("T.AX", "AX", DbFieldType::Double),
                ("T.XRB", "XRB", DbFieldType::Double),
                ("T.M0X", "M0X", DbFieldType::Double),
                ("T.M0Y", "M0Y", DbFieldType::Double),
                ("T.M1Y", "M1Y", DbFieldType::Double),
                ("T.X0", "X0", DbFieldType::Double),
                ("T.LVIO", "LVIO", DbFieldType::Short),
                ("T.ZERO", "ZERO", DbFieldType::Short),
            ] {
                readers.push(inst.add_subscriber(field, sid, ty, class.bits()).unwrap());
                sid += 1;
                assert!(inst.attach_filter_to_last_subscriber(
                    field,
                    Arc::new(Line {
                        pv,
                        stream: stream.clone(),
                    }),
                ));
            }
        }
    }
    for (field, value) in STEPS {
        db.put_record_field_from_ca("T", field, EpicsValue::String(value.into()))
            .await
            .unwrap();
    }
    let got: Vec<String> = streams.iter().map(|s| s.lock().unwrap().clone()).collect();
    assert_eq!(by_pv(&got[0]), by_pv(VALUE), "DBE_VALUE");
    assert_eq!(got[1], "", "DBE_ALARM");
    assert_eq!(by_pv(&got[2]), by_pv(LOG), "DBE_LOG");
    // The oracle's `caget` after the steps.
    for (pv, v) in [("T.X", 0.0), ("T.M0X", 1.0), ("T.XRB", -1.0)] {
        assert_eq!(db.get_pv(pv).unwrap(), EpicsValue::Double(v), "{pv}");
    }
    assert_eq!(db.get_pv("T.LVIO").unwrap(), EpicsValue::Short(1), "T.LVIO");
}

/// Each PV's lines in the order it received them.
fn by_pv(stream: &str) -> std::collections::BTreeMap<&str, Vec<&str>> {
    let mut m = std::collections::BTreeMap::<&str, Vec<&str>>::new();
    for line in stream.lines() {
        m.entry(line.split(' ').next().unwrap())
            .or_default()
            .push(line);
    }
    m
}
