//! table posts only what C `process()` and `monitor()` post
//! (`tableRecord.c:419-597`, `:847-918`), against oracleTableIoc with three
//! `camonitor -m v|a|l` clients on every field in `FIELDS`, driven with `caput
//! T.PROC 1; T.X 1; T.PROC 1; T.Y 2; T.AX 0.5; T.ZERO 1; T.PROC 1; T.X 1;
//! T.READ 1; T.SYNC 1; T.INIT 1; T.PROC 1`.
//!
//! A cycle posts the thirteen arrays and LVIO that moved off the copy taken at
//! its top, and ZERO, READ, SYNC or INIT as it clears them; never VAL, UDF, the
//! motor readbacks R0X.., speeds V0X.. or limits H0X../L0X.. it writes, and
//! never on an alarm move. `table_monitor_posts_from_its_saved_copy` pins the
//! values.

// RTEMS-EXEC-MODEL-ALLOW(2): checked, not waived — all 2 ran and passed on the
// exec backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread`); no CA server is built.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};
use optics_rs::records::table::TableRecord;

const STEPS: [(&str, &str); 12] = [
    ("PROC", "1"),
    ("X", "1"),
    ("PROC", "1"),
    ("Y", "2"),
    ("AX", "0.5"),
    ("ZERO", "1"),
    ("PROC", "1"),
    ("X", "1"),
    ("READ", "1"),
    ("SYNC", "1"),
    ("INIT", "1"),
    ("PROC", "1"),
];

/// Every field oracleTableIoc's `tableRecord.dbd` serves to a client, less the
/// links and VERS, plus the dbCommon fields a cycle writes.
const FIELDS: [(&str, DbFieldType); 138] = [
    ("VAL", DbFieldType::Double),
    ("LX", DbFieldType::Double),
    ("LZ", DbFieldType::Double),
    ("SX", DbFieldType::Double),
    ("SY", DbFieldType::Double),
    ("SZ", DbFieldType::Double),
    ("RX", DbFieldType::Double),
    ("RY", DbFieldType::Double),
    ("RZ", DbFieldType::Double),
    ("YANG", DbFieldType::Double),
    ("AX", DbFieldType::Double),
    ("AY", DbFieldType::Double),
    ("AZ", DbFieldType::Double),
    ("X", DbFieldType::Double),
    ("Y", DbFieldType::Double),
    ("Z", DbFieldType::Double),
    ("AX0", DbFieldType::Double),
    ("AY0", DbFieldType::Double),
    ("AZ0", DbFieldType::Double),
    ("X0", DbFieldType::Double),
    ("Y0", DbFieldType::Double),
    ("Z0", DbFieldType::Double),
    ("AXL", DbFieldType::Double),
    ("AYL", DbFieldType::Double),
    ("AZL", DbFieldType::Double),
    ("XL", DbFieldType::Double),
    ("YL", DbFieldType::Double),
    ("ZL", DbFieldType::Double),
    ("AXRB", DbFieldType::Double),
    ("AYRB", DbFieldType::Double),
    ("AZRB", DbFieldType::Double),
    ("XRB", DbFieldType::Double),
    ("YRB", DbFieldType::Double),
    ("ZRB", DbFieldType::Double),
    ("EAX", DbFieldType::Double),
    ("EAY", DbFieldType::Double),
    ("EAZ", DbFieldType::Double),
    ("EX", DbFieldType::Double),
    ("EY", DbFieldType::Double),
    ("EZ", DbFieldType::Double),
    ("HLAX", DbFieldType::Double),
    ("HLAY", DbFieldType::Double),
    ("HLAZ", DbFieldType::Double),
    ("HLX", DbFieldType::Double),
    ("HLY", DbFieldType::Double),
    ("HLZ", DbFieldType::Double),
    ("LLAX", DbFieldType::Double),
    ("LLAY", DbFieldType::Double),
    ("LLAZ", DbFieldType::Double),
    ("LLX", DbFieldType::Double),
    ("LLY", DbFieldType::Double),
    ("LLZ", DbFieldType::Double),
    ("UHAX", DbFieldType::Double),
    ("UHAY", DbFieldType::Double),
    ("UHAZ", DbFieldType::Double),
    ("UHX", DbFieldType::Double),
    ("UHY", DbFieldType::Double),
    ("UHZ", DbFieldType::Double),
    ("ULAX", DbFieldType::Double),
    ("ULAY", DbFieldType::Double),
    ("ULAZ", DbFieldType::Double),
    ("ULX", DbFieldType::Double),
    ("ULY", DbFieldType::Double),
    ("ULZ", DbFieldType::Double),
    ("UHAXR", DbFieldType::Double),
    ("UHAYR", DbFieldType::Double),
    ("UHAZR", DbFieldType::Double),
    ("UHXR", DbFieldType::Double),
    ("UHYR", DbFieldType::Double),
    ("UHZR", DbFieldType::Double),
    ("ULAXR", DbFieldType::Double),
    ("ULAYR", DbFieldType::Double),
    ("ULAZR", DbFieldType::Double),
    ("ULXR", DbFieldType::Double),
    ("ULYR", DbFieldType::Double),
    ("ULZR", DbFieldType::Double),
    ("M0X", DbFieldType::Double),
    ("M0Y", DbFieldType::Double),
    ("M1Y", DbFieldType::Double),
    ("M2X", DbFieldType::Double),
    ("M2Y", DbFieldType::Double),
    ("M2Z", DbFieldType::Double),
    ("R0X", DbFieldType::Double),
    ("R0Y", DbFieldType::Double),
    ("R1Y", DbFieldType::Double),
    ("R2X", DbFieldType::Double),
    ("R2Y", DbFieldType::Double),
    ("R2Z", DbFieldType::Double),
    ("E0X", DbFieldType::Double),
    ("E0Y", DbFieldType::Double),
    ("E1Y", DbFieldType::Double),
    ("E2X", DbFieldType::Double),
    ("E2Y", DbFieldType::Double),
    ("E2Z", DbFieldType::Double),
    ("V0X", DbFieldType::Double),
    ("V0Y", DbFieldType::Double),
    ("V1Y", DbFieldType::Double),
    ("V2X", DbFieldType::Double),
    ("V2Y", DbFieldType::Double),
    ("V2Z", DbFieldType::Double),
    ("H0X", DbFieldType::Double),
    ("H0Y", DbFieldType::Double),
    ("H1Y", DbFieldType::Double),
    ("H2X", DbFieldType::Double),
    ("H2Y", DbFieldType::Double),
    ("H2Z", DbFieldType::Double),
    ("L0X", DbFieldType::Double),
    ("L0Y", DbFieldType::Double),
    ("L1Y", DbFieldType::Double),
    ("L2X", DbFieldType::Double),
    ("L2Y", DbFieldType::Double),
    ("L2Z", DbFieldType::Double),
    ("INIT", DbFieldType::Double),
    ("ZERO", DbFieldType::Double),
    ("SYNC", DbFieldType::Double),
    ("READ", DbFieldType::Double),
    ("SET", DbFieldType::Double),
    ("SSET", DbFieldType::Double),
    ("SUSE", DbFieldType::Double),
    ("LVIO", DbFieldType::Double),
    ("PP0", DbFieldType::Double),
    ("PP1", DbFieldType::Double),
    ("PP2", DbFieldType::Double),
    ("PPO0", DbFieldType::Double),
    ("PPO1", DbFieldType::Double),
    ("PPO2", DbFieldType::Double),
    ("A", DbFieldType::Double),
    ("B", DbFieldType::Double),
    ("LEGU", DbFieldType::String),
    ("AEGU", DbFieldType::String),
    ("PREC", DbFieldType::Double),
    ("MMAP", DbFieldType::Double),
    ("GEOM", DbFieldType::Double),
    ("TORAD", DbFieldType::Double),
    ("AUNIT", DbFieldType::Double),
    ("UDF", DbFieldType::Double),
    ("SEVR", DbFieldType::Double),
    ("STAT", DbFieldType::Double),
];

/// Appends the name of each PV an event reaches.
struct Name {
    pv: String,
    stream: Arc<Mutex<String>>,
}

impl SubscriptionFilter for Name {
    fn name(&self) -> &'static str {
        "name"
    }

    fn apply(&self, event: FilteredMonitorEvent) -> Option<FilteredMonitorEvent> {
        let mut stream = self.stream.lock().unwrap();
        stream.push_str(&self.pv);
        stream.push('\n');
        Some(event)
    }
}

/// Starts each step's posts in a stream.
const STEP: &str = "--\n";

/// A stream as each step's posts, sorted: C posts an axis's fields together in
/// `monitor()`'s loop, the port in its walk's order.
fn steps_of(stream: &str) -> Vec<Vec<&str>> {
    stream
        .split(STEP)
        .map(|step| {
            let mut posts: Vec<&str> = step.lines().collect();
            posts.sort_unstable();
            posts
        })
        .collect()
}

/// The DBE_VALUE, DBE_ALARM and DBE_LOG streams of `T` in `db` over `STEPS`.
async fn streams(db: &str) -> Vec<String> {
    let (db, _) = IocBuilder::new()
        .register_record_type("table", || Box::new(TableRecord::default()))
        .register_record_type("ao", || Box::new(AoRecord::default()))
        .db_string(db, &HashMap::new())
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
            for (field, ty) in FIELDS {
                readers.push(inst.add_subscriber(field, sid, ty, class.bits()).unwrap());
                sid += 1;
                assert!(inst.attach_filter_to_last_subscriber(
                    field,
                    Arc::new(Name {
                        pv: format!("T.{field}"),
                        stream: stream.clone(),
                    }),
                ));
            }
        }
    }
    for (field, value) in STEPS {
        for stream in &streams {
            stream.lock().unwrap().push_str(STEP);
        }
        db.put_record_field_from_ca("T", field, EpicsValue::String(value.into()))
            .await
            .unwrap();
    }
    streams.iter().map(|s| s.lock().unwrap().clone()).collect()
}

const UNLINKED: &str = r#"
record(table, "T") {
  field(LX, "100")
  field(LZ, "200")
}
"#;

const VALUE: &str = "\
--
--
T.X
T.XL
T.HLX
T.LLX
T.M0X
T.M2X
--
--
T.Y
T.YL
T.HLY
T.LLY
T.M0Y
T.M1Y
T.M2Y
--
T.AX
T.AXL
T.M2Y
--
T.AX
T.AXRB
T.EAX
T.AX0
T.X
T.XRB
T.X0
T.Y
T.YRB
T.Y0
T.UHAXR
T.HLX
T.EX
T.UHXR
T.HLY
T.EY
T.UHYR
T.ULAXR
T.ULXR
T.ULYR
T.ZERO
T.ZERO
T.ZERO
--
T.LVIO
--
T.X
T.X
--
T.READ
T.READ
T.READ
T.LVIO
--
T.SYNC
T.SYNC
T.SYNC
T.AX
T.AXL
T.X
T.XL
T.Y
T.YL
T.LLX
T.LLY
T.M0X
T.M0Y
T.M1Y
T.M2X
T.M2Y
--
T.AX
T.AXRB
T.EAX
T.AX0
T.X
T.XRB
T.X0
T.Y
T.YRB
T.Y0
T.UHAXR
T.HLX
T.EX
T.UHXR
T.HLY
T.EY
T.UHYR
T.ULAXR
T.ULXR
T.ULYR
T.INIT
T.INIT
T.INIT
--
";

const ALARM: &str = "\
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

const LOG: &str = "\
--
--
T.X
--
--
T.Y
--
T.AX
--
T.ZERO
--
--
T.X
--
T.READ
--
T.SYNC
--
T.INIT
--
";

#[tokio::test]
async fn table_posts_only_what_monitor_posts_as_the_oracle_does() {
    let got = streams(UNLINKED).await;
    for (got, want, class) in [
        (&got[0], VALUE, "DBE_VALUE"),
        (&got[1], ALARM, "DBE_ALARM"),
        (&got[2], LOG, "DBE_LOG"),
    ] {
        assert_eq!(steps_of(got), steps_of(want), "{class}");
    }
}

/// X's motor linked to ao records for its drive, readback, encoder, limits
/// and speed, so a cycle writes R0X, H0X, L0X and V0X.
const LINKED: &str = r#"
record(ao, "T:RB") {
  field(VAL, "3")
}
record(ao, "T:HL") {
  field(VAL, "50")
}
record(ao, "T:LL") {
  field(VAL, "-50")
}
record(ao, "T:V") {
  field(VAL, "2")
}
record(table, "T") {
  field(LX, "100")
  field(LZ, "200")
  field(M0XL, "T:RB")
  field(R0XI, "T:RB")
  field(E0XI, "T:RB")
  field(H0XL, "T:HL")
  field(L0XL, "T:LL")
  field(V0XI, "T:V")
  field(V0XL, "T:V")
}
"#;

/// The fields outside C's post list; none posts in the oracle's run of
/// `LINKED`, in any class.
const NEVER_POSTED: [&str; 55] = [
    "VAL", "LX", "LZ", "SX", "SY", "SZ", "RX", "RY", "RZ", "YANG", "R0X", "R0Y", "R1Y", "R2X",
    "R2Y", "R2Z", "V0X", "V0Y", "V1Y", "V2X", "V2Y", "V2Z", "H0X", "H0Y", "H1Y", "H2X", "H2Y",
    "H2Z", "L0X", "L0Y", "L1Y", "L2X", "L2Y", "L2Z", "SET", "SSET", "SUSE", "PP0", "PP1", "PP2",
    "PPO0", "PPO1", "PPO2", "A", "B", "LEGU", "AEGU", "PREC", "MMAP", "GEOM", "TORAD", "AUNIT",
    "UDF", "SEVR", "STAT",
];

/// Only the silence is compared: the port reads the motor links before the
/// cycle writes the drive, where C reads them after, so its readbacks trail
/// C's by a cycle and the posted values differ.
#[tokio::test]
async fn table_never_posts_the_motor_fields_it_writes() {
    for (stream, class) in streams(LINKED)
        .await
        .iter()
        .zip(["DBE_VALUE", "DBE_ALARM", "DBE_LOG"])
    {
        let posted: Vec<&str> = stream
            .lines()
            .filter(|pv| NEVER_POSTED.contains(&pv.trim_start_matches("T.")))
            .collect();
        assert!(posted.is_empty(), "{class}: {posted:?}");
    }
}
