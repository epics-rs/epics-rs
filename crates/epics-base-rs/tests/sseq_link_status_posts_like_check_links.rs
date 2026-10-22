//! sseq's link-status posts, counted per field against oracleIoc (calc R2-x,
//! x86-64 softIoc R7.0.10) running the same puts under camonitor.
//!
//! C posts what each trigger sets: `special(DOLn/LNKn)` that step's status,
//! `checkLinks` a local link's status every pass, a CA link's only on a
//! toggle, a constant's never, `WERRn` only on change, and `DTn`/`LTn` never.
//! A `DOLn`/`LNKn` put naming nothing schedules no `checkLinks`, and neither
//! does a process of a record whose links all resolved.

use epics_base_rs::runtime::task::{
    BackgroundTaskHandle, CallbackPriority, sleep_background, spawn_background,
};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::records::sseq::SseqRecord;
use epics_base_rs::types::{DbFieldType, EpicsValue};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Past the 0.5 s `checkLinksCallback` a put arms (`sseqRecord.c:1018`).
const PASS: Duration = Duration::from_millis(600);

const DB: &str = r#"
record(ao, "T") {}
record(ao, "T2") {}
record(sseq, "Q") {
    field(DOL1, "T") field(LNK1, "T2")
    field(DOL2, "T") field(LNK2, "T2")
}
"#;

const FIELDS: [&str; 11] = [
    "DOL1V", "LNK1V", "WERR1", "DT1", "LT1", "DOL2V", "LNK2V", "WERR2", "DOL3V", "LNK3V", "WERR3",
];

async fn build() -> Arc<PvDatabase> {
    IocBuilder::new()
        .register_record_type("sseq", || Box::new(SseqRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

fn drain(rx: &mut EventReader) -> usize {
    let mut n = 0;
    while rx.try_recv().is_ok() {
        n += 1;
    }
    n
}

/// Whether `rx` has a post 750 ms on the delayed-callback timer after its
/// next one, read on cbLow. A pass's post wakes this there only once the pass
/// has re-armed, and the timer fires in deadline order, so a 0.5 s re-arm has
/// posted by then and a 1 s one has not, however late the host runs both.
fn reposted_within_750_ms(mut rx: EventReader) -> BackgroundTaskHandle<(EventReader, bool)> {
    spawn_background(CallbackPriority::Low, async move {
        drain(&mut rx);
        rx.recv().await.expect("subscriber open");
        sleep_background(Duration::from_millis(750)).await;
        let again = drain(&mut rx) > 0;
        (rx, again)
    })
}

/// The events each of `rxs` gets once every one has `least`, plus any in a
/// further [`PASS`]: a slow host runs the pass late, not never.
async fn events(rxs: &mut [EventReader], least: &[usize]) -> Vec<usize> {
    let mut got = vec![0; rxs.len()];
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        for (n, rx) in got.iter_mut().zip(rxs.iter_mut()) {
            *n += drain(rx);
        }
        if got.iter().zip(least).all(|(n, l)| n >= l) || Instant::now() >= deadline {
            break;
        }
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    epics_base_rs::runtime::task::sleep(PASS).await;
    for (n, rx) in got.iter_mut().zip(rxs.iter_mut()) {
        *n += drain(rx);
    }
    got
}

#[epics_macros_rs::epics_test]
async fn sseq_link_status_event_counts_match_oracle_ioc() {
    let db = build().await;
    epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(100)).await;
    let mut rxs: Vec<EventReader> = FIELDS
        .iter()
        .enumerate()
        .map(|(sid, f)| {
            let ftype = if f.ends_with('V') {
                DbFieldType::Enum
            } else {
                DbFieldType::Short
            };
            let inst = db.get_record("Q").unwrap();
            let mut g = inst.write();
            g.add_subscriber(f, sid as u32 + 1, ftype, EventMask::VALUE.bits())
                .unwrap()
        })
        .collect();

    // Events each put adds, in `FIELDS` order — oracleIoc's camonitor
    // counts minus the connect event.
    let steps: [(&str, EpicsValue, [usize; 11]); 9] = [
        (
            "WAIT1",
            EpicsValue::Short(1),
            [1, 1, 1, 0, 0, 1, 1, 0, 0, 0, 0],
        ),
        (
            "WAIT1",
            EpicsValue::Short(0),
            [1, 1, 1, 0, 0, 1, 1, 0, 0, 0, 0],
        ),
        (
            "WAIT2",
            EpicsValue::Short(1),
            [1, 1, 0, 0, 0, 1, 1, 1, 0, 0, 0],
        ),
        (
            "DOL3",
            EpicsValue::String("".into()),
            [0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0],
        ),
        ("PROC", EpicsValue::Short(1), [0; 11]),
        (
            "LNK3",
            EpicsValue::String("T2".into()),
            [1, 1, 0, 0, 0, 1, 1, 0, 0, 2, 0],
        ),
        (
            "DOL2",
            EpicsValue::String("5".into()),
            [0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0],
        ),
        (
            "LNK1",
            EpicsValue::String("".into()),
            [0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        ),
        (
            "WAIT2",
            EpicsValue::Short(0),
            [1, 0, 0, 0, 0, 0, 1, 1, 0, 1, 0],
        ),
    ];
    for (field, value, want) in steps {
        db.put_record_field_from_ca("Q", field, value)
            .await
            .unwrap();
        let got = events(&mut rxs, &want).await;
        assert_eq!(got, want, "events after the {field} put, per {FIELDS:?}");
    }
}

/// C never posts `DTn`/`LTn` (`checkLinks` and `special` write them with no
/// `db_post_events`), so neither the link-status passes nor a later process
/// cycle may reach a subscriber.
#[epics_macros_rs::epics_test]
async fn sseq_link_types_never_post() {
    let db = build().await;
    epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(100)).await;
    const TYPES: [&str; 6] = ["DT1", "LT1", "DT2", "LT2", "DT3", "LT3"];
    let mut rxs: Vec<EventReader> = TYPES
        .iter()
        .enumerate()
        .map(|(sid, f)| {
            let inst = db.get_record("Q").unwrap();
            let mut g = inst.write();
            g.add_subscriber(
                f,
                sid as u32 + 1,
                DbFieldType::Short,
                (EventMask::VALUE | EventMask::LOG | EventMask::ALARM).bits(),
            )
            .unwrap()
        })
        .collect();
    let puts: [(&str, EpicsValue); 7] = [
        ("WAIT1", EpicsValue::Short(1)),
        ("LNK3", EpicsValue::String("T2".into())),
        ("DOL2", EpicsValue::String("5".into())),
        ("LNK1", EpicsValue::String("".into())),
        ("WAIT2", EpicsValue::Short(1)),
        ("PROC", EpicsValue::Short(1)),
        ("PROC", EpicsValue::Short(1)),
    ];
    for (field, value) in puts {
        db.put_record_field_from_ca("Q", field, value)
            .await
            .unwrap();
        epics_base_rs::runtime::task::sleep(PASS).await;
        let got: Vec<usize> = rxs.iter_mut().map(drain).collect();
        assert_eq!(got, [0; 6], "events after the {field} put, per {TYPES:?}");
    }
}

/// oracleIoc: a constant DOL reads `DBF_NOACCESS` (17) only until `special`
/// or `checkLinks` rewrites it as `DBF_unknown` (-1) (sseqRecord.c:206,
/// :1014, :881).
#[epics_macros_rs::epics_test]
async fn sseq_constant_dol_type_matches_oracle_ioc() {
    const DB: &str = r#"
record(ao, "T") {}
record(ao, "T2") {}
record(sseq, "Q") {
    field(DOL1, "5") field(LNK1, "T2")
    field(DOL2, "T") field(LNK2, "T2")
}
"#;
    let db = IocBuilder::new()
        .register_record_type("sseq", || Box::new(SseqRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    epics_base_rs::runtime::task::sleep(std::time::Duration::from_millis(100)).await;
    let types = |db: &PvDatabase| -> Vec<EpicsValue> {
        let inst = db.get_record("Q").unwrap();
        let g = inst.read();
        ["DT1", "DT2", "DT3"]
            .iter()
            .map(|f| g.record.get_field(f).unwrap())
            .collect()
    };
    let short = |v: [i16; 3]| v.map(EpicsValue::Short).to_vec();
    assert_eq!(types(&db), short([17, 10, 17]), "after init");
    let steps: [(&str, [i16; 3]); 2] = [("6", [-1, 10, 17]), ("NOPE", [-1, -1, -1])];
    for ((value, want), field) in steps.into_iter().zip(["DOL1", "DOL2"]) {
        db.put_record_field_from_ca("Q", field, EpicsValue::String(value.into()))
            .await
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while types(&db) != short(want) && Instant::now() < deadline {
            epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
        }
        // And no pass after it moves them on.
        epics_base_rs::runtime::task::sleep(PASS).await;
        assert_eq!(types(&db), short(want), "after the {field} put");
    }
}

/// oracleIoc camonitor: a CA link that stays `Ext PV NC` is posted at init
/// only, though every `checkLinks` pass re-posts the local `DOL2V` beside it
/// (sseqRecord.c:863-950 posts a CA status on a toggle alone).
#[epics_macros_rs::epics_test]
async fn sseq_unchanged_ca_link_status_is_not_reposted() {
    const DB: &str = r#"
record(ao, "T") {}
record(ao, "T2") {}
record(sseq, "Q") {
    field(DOL1, "NOPE CA") field(LNK1, "NOPE2 CA")
    field(DOL2, "T") field(LNK2, "T2")
}
"#;
    let db = IocBuilder::new()
        .register_record_type("sseq", || Box::new(SseqRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;
    const STATUS: [&str; 3] = ["DOL1V", "LNK1V", "DOL2V"];
    let mut rxs: Vec<EventReader> = STATUS
        .iter()
        .enumerate()
        .map(|(sid, f)| {
            let inst = db.get_record("Q").unwrap();
            let mut g = inst.write();
            g.add_subscriber(
                f,
                sid as u32 + 1,
                DbFieldType::Enum,
                EventMask::VALUE.bits(),
            )
            .unwrap()
        })
        .collect();
    let mut dol2v = rxs.pop().expect("DOL2V");
    for field in ["WAIT2", "WAIT1", "PROC"] {
        db.put_record_field_from_ca("Q", field, EpicsValue::Short(1))
            .await
            .unwrap();
        let (rx, rearmed) = reposted_within_750_ms(dol2v).await.unwrap();
        dol2v = rx;
        let got: Vec<usize> = rxs.iter_mut().map(drain).collect();
        assert_eq!(got, [0, 0], "CA status events after the {field} put");
        assert!(
            rearmed,
            "no checkLinks pass 750 ms after one, after the {field} put"
        );
    }
}
