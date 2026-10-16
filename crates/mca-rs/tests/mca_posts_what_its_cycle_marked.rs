//! mca's posts against C's (`mcaRecord.c:1005-1041`): the oracle IOC
//! (oracleMcaIoc: base, `mcaRecord.c`, `devMCA_soft.c`), three `camonitor -m
//! v|a|l` clients on the fields below, driven with `caput M.PROC 1; caput
//! M.STRT 1; caput -a M 8 0 5 7 9 1 0 0 0; caput M.PROC 1; caput M.R0HI 4;
//! caput M.PROC 1; caput M.R0BG 1; caput M.ERAS 1; caput M.PROC 1; caput
//! M.STOP 1`. The streams are the oracle's with the connect values cut, STIM's
//! and the arrays' values dropped; DBE_LOG equals DBE_VALUE. Each step's posts
//! compare as a set until records-b's 4(c)(ii) post-order rule lands.
//!
//! C posts only what `process()` MARKed: STIM on every idle cycle, BG for a
//! summed region, both sums of a region either of which moved (R0BG moves
//! R0N alone, and R0 posts with it), DTIM on every erase (M_ERAS is M_DTIM's
//! bit), never ERAS's own reset or a NORD the VAL put moved. STRT lands NORD
//! before the VAL put, because the port's `post_array_info` posts a moved NORD
//! that mca's C `put_array_info` does not. DWEL and RTIM are not monitored: the
//! soft device keeps DWEL where C zeroes it, and C's RTIM reads an unwritten
//! status.

// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — ran and passed on the exec
// backend (`EPICS_RS_BUILD_EXEC_BACKEND=thread`); no CA server is built.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::filters::{FilteredMonitorEvent, SubscriptionFilter};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset};
use epics_base_rs::types::{DbFieldType, EpicsValue};

static SOFT_MCA: DevSup = DevSup {
    name: "devMCA_soft",
    choice: "Soft Channel",
    link_type: DbLinkType::Constant,
    dset: Dset::new(6, &[2, 4, 5], false),
};

const DB: &str = r#"
record(mca, "M") {
    field(DTYP, "Soft Channel")
    field(NMAX, "8")
    field(NUSE, "8")
    field(R0LO, "1")
    field(R0HI, "3")
}
"#;

fn menu(pv: &str) -> &'static [&'static str] {
    match pv {
        "M.ERAS" => &["Done", "Erase"],
        "M.READ" | "M.RDNG" => &["Done", "Read"],
        _ => &["Done", "Acquire"],
    }
}

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
            Some(EpicsValue::Enum(v)) => menu(self.pv)[*v as usize].to_string(),
            Some(EpicsValue::String(_)) => String::new(),
            Some(EpicsValue::LongArray(a)) if a.len() == 1 => format!("{}", a[0]),
            Some(EpicsValue::LongArray(a)) => {
                let mut s = format!("{}", a.len());
                for v in a.iter() {
                    s += &format!(" {v}");
                }
                s
            }
            Some(v) => format!("{v}"),
            None => String::new(),
        };
        let mut line = format!("{} {value}", self.pv).trim_end().to_string();
        if e.sevr() != 0 {
            line += " UDF INVALID";
        }
        let mut stream = self.stream.lock().unwrap();
        stream.push_str(&line);
        stream.push('\n');
        Some(event)
    }
}

/// Starts each step's posts in a stream.
const STEP: &str = "--\n";

/// A stream as each step's posts, sorted.
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

const VALUE: &str = "\
--
M.STIM
--
M.STRT Acquire
M
M.BG
M.READ Done
M.STRT Done
M.STIM
M.NORD 8
M.ACQG Done
--
M.STIM
--
M.STIM
--
M.R0HI 4
M.BG
M.STIM
M.R0 22
M.R0N 11
--
M.STIM
--
M.R0BG 1
M.BG
M.STIM
M.R0 22
M.R0N 9
--
M.ERAS Erase
M
M.STIM
M.DTIM 0
M.NORD 0
M.R0 0
M.R0N 0
--
M.STIM
--
M.STOP Acquire
M.STOP Done
M.STIM
";

const ALARM: &str = "\
--
M.STIM
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

#[tokio::test]
async fn mca_posts_what_its_cycle_marked_as_the_oracle_does() {
    let (name, factory) = mca_rs::mca_record_factory();
    let (db, _) = IocBuilder::new()
        .register_record_type(name, factory)
        .register_device_support("mca", &SOFT_MCA, || Box::new(mca_rs::SoftMca::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    let cell = db.get_record("M").unwrap();
    let classes = [EventMask::VALUE, EventMask::ALARM, EventMask::LOG];
    let streams: Vec<Arc<Mutex<String>>> = classes.iter().map(|_| Arc::default()).collect();
    let mut readers = Vec::new();
    {
        let mut inst = cell.write();
        let mut sid = 0;
        for (class, stream) in classes.iter().zip(&streams) {
            for (pv, field, ty) in [
                ("M", "VAL", DbFieldType::Long),
                ("M.ACQG", "ACQG", DbFieldType::Enum),
                ("M.R0", "R0", DbFieldType::Double),
                ("M.R0N", "R0N", DbFieldType::Double),
                ("M.NORD", "NORD", DbFieldType::Long),
                ("M.ERTM", "ERTM", DbFieldType::Double),
                ("M.ELTM", "ELTM", DbFieldType::Double),
                ("M.ACT", "ACT", DbFieldType::Long),
                ("M.DTIM", "DTIM", DbFieldType::Double),
                ("M.IDTIM", "IDTIM", DbFieldType::Double),
                ("M.STIM", "STIM", DbFieldType::String),
                ("M.READ", "READ", DbFieldType::Enum),
                ("M.RDNG", "RDNG", DbFieldType::Enum),
                ("M.NACK", "NACK", DbFieldType::Short),
                ("M.STRT", "STRT", DbFieldType::Enum),
                ("M.STOP", "STOP", DbFieldType::Enum),
                ("M.BG", "BG", DbFieldType::Long),
                ("M.ERAS", "ERAS", DbFieldType::Enum),
                ("M.R0HI", "R0HI", DbFieldType::Long),
                ("M.R0BG", "R0BG", DbFieldType::Short),
            ] {
                readers.push(inst.add_subscriber(field, sid, ty, class.bits()).unwrap());
                sid += 1;
                assert!(inst.attach_filter_to_last_subscriber(
                    field,
                    Arc::new(Line {
                        pv,
                        stream: stream.clone()
                    }),
                ));
            }
        }
    }
    let steps: [(&str, EpicsValue); 10] = [
        ("PROC", EpicsValue::String("1".into())),
        ("STRT", EpicsValue::String("1".into())),
        (
            "VAL",
            EpicsValue::LongArray(vec![0, 5, 7, 9, 1, 0, 0, 0].into()),
        ),
        ("PROC", EpicsValue::String("1".into())),
        ("R0HI", EpicsValue::String("4".into())),
        ("PROC", EpicsValue::String("1".into())),
        ("R0BG", EpicsValue::String("1".into())),
        ("ERAS", EpicsValue::String("1".into())),
        ("PROC", EpicsValue::String("1".into())),
        ("STOP", EpicsValue::String("1".into())),
    ];
    for (field, value) in steps {
        for stream in &streams {
            stream.lock().unwrap().push_str(STEP);
        }
        db.put_record_field_from_ca("M", field, value)
            .await
            .unwrap();
    }
    let got: Vec<String> = streams.iter().map(|s| s.lock().unwrap().clone()).collect();
    for (got, want, class) in [
        (&got[0], VALUE, "DBE_VALUE"),
        (&got[1], ALARM, "DBE_ALARM"),
        (&got[2], VALUE, "DBE_LOG"),
    ] {
        assert_eq!(steps_of(got), steps_of(want), "{class}");
    }
}
