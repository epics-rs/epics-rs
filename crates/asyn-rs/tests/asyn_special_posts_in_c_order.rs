//! The posts `special()` makes under the record's lock (`asynRecord.c:375-578`)
//! against the asyn R4-46 oracle IOC (oracleIoc), three `camonitor -m v|a|l`
//! clients on `A.ERRS A.PORT A.REASON A.DRVINFO A.PCNCT A.CNCT A.TFIL A.BAUD
//! A.OCTETIV A.TMSK` of a record with no port, driven with `caput A.PORT
//! NOSUCH; caput A.REASON 3; caput A.DRVINFO X; caput A.TFIL
//! /nonexistent/dir/f; caput A.BAUD 9600; caput A.PORT NOSUCH2`. The streams
//! below are the oracle's with the timestamps and the connect values cut.
//!
//! Every put posts ERRS from `special()`'s `resetError` (:390) or its
//! `reportError` first, then whatever `monitorStatus` moved, then dbPut's own
//! post of the field it wrote. The REASON put posts REASON twice: once from
//! `monitorStatus` (:1127), against the copy it last posted, and once from
//! dbPut. The first PORT put moves TMSK and TFIL off `asynRecPvt.old`'s zeroes;
//! the second finds them unchanged.
//!
//! dbEvent drops a by-reference post while one is pending for the same
//! subscription (dbEvent.c:794-800), so a string or a char array posted twice
//! in one put reaches the client once or twice by timing: each step keeps
//! only such a field's first post, and a line carries no value for it. The
//! alarm class sees nothing.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use asyn_rs::asyn_record::AsynRecord;
use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const STEPS: [(&str, &str); 6] = [
    ("PORT", "NOSUCH"),
    ("REASON", "3"),
    ("DRVINFO", "X"),
    ("TFIL", "/nonexistent/dir/f"),
    ("BAUD", "9600"),
    ("PORT", "NOSUCH2"),
];

/// The value and log streams: identical in the oracle.
const POSTS: &str = "\
--
A.ERRS
A.TMSK 1
A.TFIL
A.PORT
--
A.ERRS
A.REASON 3
A.REASON 3
--
A.ERRS
A.DRVINFO
--
A.ERRS
A.TFIL
--
A.ERRS
A.BAUD 9600
--
A.ERRS
A.PORT
";

const ALARM: &str = "--\n--\n--\n--\n--\n--\n";

/// ERRS after each step: the oracle's last ERRS post of the step, read at
/// delivery once the put returned. The BAUD put is `special()`'s
/// `queueRequest` refused on a user with no port (asynRecord.c:571-578,
/// asynManager.c:1528-1531).
const ERRS_AFTER: [&str; 6] = [
    "connectDevice failed: asynManager:isEnabled asynUser not connected to device",
    "",
    "connectDevice failed: asynManager:isEnabled asynUser not connected to device",
    "Error opening trace file: /nonexistent/dir/f",
    "asynManager::queueRequest asynUser not associated with a port",
    "connectDevice failed: asynManager:isEnabled asynUser not connected to device",
];

/// Fields posted by reference.
const BY_REFERENCE: [&str; 4] = ["A.ERRS", "A.PORT", "A.DRVINFO", "A.TFIL"];

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
        let value = match (self.pv, event.event.value().as_value()) {
            (_, None) => String::new(),
            ("A.BAUD", Some(EpicsValue::Enum(v))) => {
                ["Unknown", "300", "600", "1200", "2400", "4800", "9600"][*v as usize].to_string()
            }
            (_, Some(EpicsValue::Long(v))) => v.to_string(),
            (_, Some(EpicsValue::Enum(v))) => v.to_string(),
            (pv, other) => panic!("{pv}: {other:?}"),
        };
        let line = format!("{} {value}", self.pv);
        let mut stream = self.stream.lock().unwrap();
        stream.push_str(line.trim_end());
        stream.push('\n');
        Some(event)
    }
}

/// Starts each step's posts in a stream.
const STEP: &str = "--\n";

/// `stream` with each by-reference field's repeat posts in a step cut.
fn first_by_reference(stream: &str) -> String {
    let mut out = String::new();
    let mut seen = Vec::new();
    for line in stream.lines() {
        if line == "--" {
            seen.clear();
        } else {
            let pv = line.split(' ').next().unwrap();
            if BY_REFERENCE.contains(&pv) {
                if seen.contains(&pv) {
                    continue;
                }
                seen.push(pv);
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[epics_base_rs::epics_test]
async fn special_posts_in_the_order_the_oracle_does() {
    let (db, _) = IocBuilder::new()
        .register_record_type("asyn", || Box::new(AsynRecord::default()))
        .db_string("record(asyn, \"A\") {\n}\n", &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let cell = db.get_record("A").unwrap();
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let mut readers = Vec::new();
    {
        let mut inst = cell.write();
        let mut sid = 0;
        for (class, stream) in classes.iter().zip(&streams) {
            for (pv, field, ty) in [
                ("A.ERRS", "ERRS", DbFieldType::Char),
                ("A.PORT", "PORT", DbFieldType::String),
                ("A.REASON", "REASON", DbFieldType::Long),
                ("A.DRVINFO", "DRVINFO", DbFieldType::String),
                ("A.PCNCT", "PCNCT", DbFieldType::Enum),
                ("A.CNCT", "CNCT", DbFieldType::Enum),
                ("A.TFIL", "TFIL", DbFieldType::String),
                ("A.BAUD", "BAUD", DbFieldType::Enum),
                ("A.OCTETIV", "OCTETIV", DbFieldType::Long),
                ("A.TMSK", "TMSK", DbFieldType::Long),
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
    let mut errs = Vec::new();
    for (field, value) in STEPS {
        for stream in &streams {
            stream.lock().unwrap().push_str(STEP);
        }
        db.put_record_field_from_ca("A", field, EpicsValue::String(value.into()))
            .await
            .unwrap();
        let Some(EpicsValue::CharArray(buf)) = cell.read().record.get_field("ERRS") else {
            panic!("ERRS is a char array");
        };
        let text = buf.split(|b| *b == 0).next().unwrap();
        errs.push(String::from_utf8(text.to_vec()).unwrap());
    }
    assert_eq!(errs, ERRS_AFTER);
    for (got, want, class) in [
        (&streams[0], POSTS, "DBE_VALUE"),
        (&streams[1], ALARM, "DBE_ALARM"),
        (&streams[2], POSTS, "DBE_LOG"),
    ] {
        assert_eq!(first_by_reference(&got.lock().unwrap()), want, "{class}");
    }
}
