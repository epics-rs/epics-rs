//! table's `GetReadback` and `ReadEncoders` against a readback link that
//! fails, against oracleTableIoc with three `camonitor -m v|a|l` clients on the
//! fields in `FIELDS`, driven with `caput T:S 5; T:RY 7; T.READ 1; T:S abc;
//! T:RY 9; T.SYNC 1; T:S 5; T.READ 1; T:S abc; T:RY 7; T.INIT 1; T.PROC 1`.
//! The streams are the oracle's with the timestamps and the connect values
//! cut.
//!
//! `abc` fails the DBR_DOUBLE read of `T:S`, X's readback and encoder. Each
//! loop then zeroes X and returns (`tableRecord.c:1104-1110`, `:1128-1135`),
//! so Y keeps the value it had: SYNC drives M0Y to 7, not the 9 `T:RY` holds
//! by then, and INIT to 9, not 7. X reads 5 just before each, so the zero
//! shows in X and AX.

// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — ran and passed on the exec
// backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread`); no CA server is built.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{EventMask, alarm_condition_string};
use epics_base_rs::server::record::MENU_ALARM_SEVR;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::server::records::stringin::StringinRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};
use optics_rs::records::table::TableRecord;

const DB: &str = r#"
record(stringin, "T:S") {
  field(VAL, "0")
}
record(ao, "T:RY") {
}
record(ao, "T:MX") {
}
record(ao, "T:MY") {
}
record(table, "T") {
  field(LX, "100")
  field(LZ, "200")
  field(M0XL, "T:MX")
  field(R0XI, "T:S")
  field(E0XI, "T:S")
  field(M0YL, "T:MY")
  field(R0YI, "T:RY")
  field(E0YI, "T:RY")
}
"#;

const STEPS: [(&str, &str, &str); 12] = [
    ("T:S", "VAL", "5"),
    ("T:RY", "VAL", "7"),
    ("T", "READ", "1"),
    ("T:S", "VAL", "abc"),
    ("T:RY", "VAL", "9"),
    ("T", "SYNC", "1"),
    ("T:S", "VAL", "5"),
    ("T", "READ", "1"),
    ("T:S", "VAL", "abc"),
    ("T:RY", "VAL", "7"),
    ("T", "INIT", "1"),
    ("T", "PROC", "1"),
];

const FIELDS: [&str; 26] = [
    "X", "Y", "Z", "AX", "AY", "AZ", "XRB", "YRB", "ZRB", "AXRB", "AYRB", "AZRB", "EX", "EY", "EZ",
    "EAX", "EAY", "EAZ", "M0X", "M0Y", "M1Y", "M2X", "M2Y", "M2Z", "SEVR", "STAT",
];

const VALUE: &str = "\
--
--
--
T.AXRB -1.00306
T.EAX -1.00306
T.AYRB 1.47288
T.EAY 1.47288
T.AZRB -4.04039
T.EAZ -4.04039
T.XRB 5.28149
T.EX 5.28149
T.ZRB -1.24675
T.EZ -1.24675
--
--
--
T.SEVR INVALID LINK INVALID
T.STAT LINK LINK INVALID
T.AX -1.00273 LINK INVALID
T.AXRB -1.00273 LINK INVALID
T.EAX -1.00273 LINK INVALID
T.AY 0.035161 LINK INVALID
T.AYRB 0.035161 LINK INVALID
T.M0Y 7 LINK INVALID
T.EAY 0.035161 LINK INVALID
T.AZ -4.01522 LINK INVALID
T.AZRB -4.01522 LINK INVALID
T.M1Y 8.88178e-16 LINK INVALID
T.EAZ -4.01522 LINK INVALID
T.X 0.24547 LINK INVALID
T.XRB 0.24547 LINK INVALID
T.EX 0.24547 LINK INVALID
T.Y 8.88178e-16 LINK INVALID
T.YRB 8.88178e-16 LINK INVALID
T.EY 8.88178e-16 LINK INVALID
T.Z -0.0612075 LINK INVALID
T.ZRB -0.0612075 LINK INVALID
T.EZ -0.0612075 LINK INVALID
--
--
T.SEVR NO_ALARM
T.STAT NO_ALARM
T.AXRB -1.2897
T.EAX -1.2897
T.AYRB 1.49635
T.EAY 1.49635
T.AZRB -5.1986
T.EAZ -5.1986
T.XRB 5.4453
T.EX 5.4453
T.YRB -8.88178e-16
T.EY -8.88178e-16
T.ZRB -1.28308
T.EZ -1.28308
--
--
--
T.SEVR INVALID LINK INVALID
T.STAT LINK LINK INVALID
T.AX -1.28926 LINK INVALID
T.AXRB -1.28926 LINK INVALID
T.EAX -1.28926 LINK INVALID
T.AY 0.0581963 LINK INVALID
T.AYRB 0.0581963 LINK INVALID
T.M0Y 9 LINK INVALID
T.EAY 0.0581963 LINK INVALID
T.AZ -5.16623 LINK INVALID
T.AZRB -5.16623 LINK INVALID
T.EAZ -5.16623 LINK INVALID
T.X 0.406287 LINK INVALID
T.XRB 0.406287 LINK INVALID
T.EX 0.406287 LINK INVALID
T.YRB 8.88178e-16 LINK INVALID
T.EY 8.88178e-16 LINK INVALID
T.Z -0.101133 LINK INVALID
T.ZRB -0.101133 LINK INVALID
T.EZ -0.101133 LINK INVALID
--
";

const ALARM: &str = "\
--
--
--
--
--
--
T.STAT LINK LINK INVALID
T.AX -1.00273 LINK INVALID
T.AXRB -1.00273 LINK INVALID
T.EAX -1.00273 LINK INVALID
T.AY 0.035161 LINK INVALID
T.AYRB 0.035161 LINK INVALID
T.M0Y 7 LINK INVALID
T.EAY 0.035161 LINK INVALID
T.AZ -4.01522 LINK INVALID
T.AZRB -4.01522 LINK INVALID
T.M1Y 8.88178e-16 LINK INVALID
T.EAZ -4.01522 LINK INVALID
T.X 0.24547 LINK INVALID
T.XRB 0.24547 LINK INVALID
T.EX 0.24547 LINK INVALID
T.Y 8.88178e-16 LINK INVALID
T.YRB 8.88178e-16 LINK INVALID
T.EY 8.88178e-16 LINK INVALID
T.Z -0.0612075 LINK INVALID
T.ZRB -0.0612075 LINK INVALID
T.EZ -0.0612075 LINK INVALID
--
--
T.STAT NO_ALARM
T.AXRB -1.2897
T.EAX -1.2897
T.AYRB 1.49635
T.EAY 1.49635
T.AZRB -5.1986
T.EAZ -5.1986
T.XRB 5.4453
T.EX 5.4453
T.YRB -8.88178e-16
T.EY -8.88178e-16
T.ZRB -1.28308
T.EZ -1.28308
--
--
--
T.STAT LINK LINK INVALID
T.AX -1.28926 LINK INVALID
T.AXRB -1.28926 LINK INVALID
T.EAX -1.28926 LINK INVALID
T.AY 0.0581963 LINK INVALID
T.AYRB 0.0581963 LINK INVALID
T.M0Y 9 LINK INVALID
T.EAY 0.0581963 LINK INVALID
T.AZ -5.16623 LINK INVALID
T.AZRB -5.16623 LINK INVALID
T.EAZ -5.16623 LINK INVALID
T.X 0.406287 LINK INVALID
T.XRB 0.406287 LINK INVALID
T.EX 0.406287 LINK INVALID
T.YRB 8.88178e-16 LINK INVALID
T.EY 8.88178e-16 LINK INVALID
T.Z -0.101133 LINK INVALID
T.ZRB -0.101133 LINK INVALID
T.EZ -0.101133 LINK INVALID
--
";

const LOG: &str = "\
--
--
--
--
--
--
--
--
--
--
--
--
";

/// C's `%g`, the format `camonitor` prints a double in.
fn g(v: f64) -> String {
    let e = format!("{v:.5e}");
    let (mantissa, exp) = e.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    if v == 0.0 {
        "0".into()
    } else if !(-4..6).contains(&exp) {
        let mantissa = mantissa.trim_end_matches('0').trim_end_matches('.');
        format!(
            "{mantissa}e{}{:02}",
            if exp < 0 { '-' } else { '+' },
            exp.abs()
        )
    } else {
        let fixed = format!("{v:.*}", (5 - exp) as usize);
        fixed.trim_end_matches('0').trim_end_matches('.').into()
    }
}

/// Appends each event it passes, as `camonitor` prints it without the time.
struct Line {
    field: &'static str,
    stream: Arc<Mutex<String>>,
}

impl SubscriptionFilter for Line {
    fn name(&self) -> &'static str {
        "line"
    }

    fn apply(&self, event: FilteredMonitorEvent) -> Option<FilteredMonitorEvent> {
        let e = &event.event;
        let v = e.value().as_value().and_then(EpicsValue::to_f64).unwrap();
        let value = match self.field {
            "SEVR" => MENU_ALARM_SEVR[v as usize].to_string(),
            "STAT" => alarm_condition_string(v as u16).to_string(),
            _ => g(v),
        };
        let mut line = format!("T.{} {value}", self.field);
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

/// Starts each step's posts in a stream.
const STEP: &str = "--\n";

/// A stream as each step's posts, grouped by PV in each PV's own order: C posts
/// an axis's fields together in `monitor()`'s loop, the port in its walk's.
fn steps_of(stream: &str) -> Vec<Vec<&str>> {
    let mut steps: Vec<&str> = stream.split(STEP).collect();
    assert_eq!(steps.remove(0), "", "a stream starts with its first step");
    steps
        .into_iter()
        .map(|step| {
            let mut posts: Vec<&str> = step.lines().collect();
            posts.sort_by_key(|post| post.split_once(' ').map_or(*post, |(pv, _)| pv));
            posts
        })
        .collect()
}

#[tokio::test]
async fn table_stops_each_read_loop_at_a_failed_read_as_the_oracle_does() {
    let (db, _) = IocBuilder::new()
        .register_record_type("table", || Box::new(TableRecord::default()))
        .register_record_type("ao", || Box::new(AoRecord::default()))
        .register_record_type("stringin", || Box::new(StringinRecord::default()))
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
            for field in FIELDS {
                readers.push(
                    inst.add_subscriber(field, sid, DbFieldType::Double, class.bits())
                        .unwrap(),
                );
                sid += 1;
                assert!(inst.attach_filter_to_last_subscriber(
                    field,
                    Arc::new(Line {
                        field,
                        stream: stream.clone(),
                    }),
                ));
            }
        }
    }
    for (rec, field, value) in STEPS {
        for stream in &streams {
            stream.lock().unwrap().push_str(STEP);
        }
        db.put_record_field_from_ca(rec, field, EpicsValue::String(value.into()))
            .await
            .unwrap();
    }
    for (stream, want, class) in [
        (&streams[0], VALUE, "DBE_VALUE"),
        (&streams[1], ALARM, "DBE_ALARM"),
        (&streams[2], LOG, "DBE_LOG"),
    ] {
        let got = stream.lock().unwrap().clone();
        assert_eq!(steps_of(&got), steps_of(want), "{class}");
    }
}
