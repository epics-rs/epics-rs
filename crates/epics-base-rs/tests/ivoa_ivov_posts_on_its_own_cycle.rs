//! The `IVOA = Set_output_to_IVOV` write must post its VAL monitor on the
//! cycle that performs it, for every record type that has the arm.
//!
//! C runs the arm inside `process()`, ahead of `monitor()`, so the
//! previous-value comparison that raises `DBE_VALUE | DBE_LOG` sees the value
//! the arm just stored — `boRecord.c:230-238` then `:395-400`,
//! `busyRecord.c:235-243` then `:365-369` (module `busy` at
//! `R1-7-4-6-g2dfe92d`), `mbboRecord.c:232-236` then `:400-403`,
//! `mbboDirectRecord.c:210-214` then `:311-314`, `lsoRecord.c:131-138` then
//! `:248-252`. C line numbers resolve at epics-base `R7.0.10`.
//!
//! The port hoists the IVOA decision out of each `process()` into one
//! framework owner that runs AFTER `Record::process` returns, so the ordering
//! is process -> arm -> monitor gate rather than C's arm -> monitor. That is
//! only safe because `Record::monitor_value_changed` compares LIVE and commits
//! the tracker at C's position; while each record captured the verdict during
//! `process()`, the flag was the verdict on the PRE-arm value and the IVOV
//! write's post landed a cycle late.
//!
//! Two boundaries, one row per record type each: the post that must land on
//! the arming cycle, and the INVALID cycle that does NOT arm, where VAL is
//! untouched and nothing may be posted at all.

use std::collections::{HashMap, HashSet};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::types::{DbFieldType, EpicsValue};

mod module_records;

/// A record type carrying the `IVOA = Set_output_to_IVOV` arm: its name, the
/// field text that gives it an IVOV distinct from the value a bare instance
/// starts at, the type a VAL subscription asks for, and the value the post
/// must carry.
struct Armed {
    kind: &'static str,
    fields: &'static str,
    dbf: DbFieldType,
    posted: EpicsValue,
}

/// Every record type whose IVOA menu has `Set output to IVOV`. A bare instance
/// of each is UDF, which is what drives the INVALID severity the arm needs.
fn armed_records() -> [Armed; 5] {
    [
        Armed {
            kind: "bo",
            fields: r#"field(IVOV,"1")"#,
            dbf: DbFieldType::Enum,
            posted: EpicsValue::Enum(1),
        },
        Armed {
            kind: "busy",
            fields: r#"field(IVOV,"1")"#,
            dbf: DbFieldType::Enum,
            posted: EpicsValue::Enum(1),
        },
        Armed {
            kind: "mbbo",
            fields: r#"field(IVOV,"2")"#,
            dbf: DbFieldType::Enum,
            posted: EpicsValue::UShort(2),
        },
        Armed {
            kind: "mbboDirect",
            fields: r#"field(IVOV,"5")"#,
            dbf: DbFieldType::Long,
            posted: EpicsValue::Long(5),
        },
        Armed {
            kind: "lso",
            fields: r#"field(IVOV,"fault")"#,
            dbf: DbFieldType::Char,
            // A long string that fits `MAX_STRING_SIZE` is posted as `String`;
            // a longer one arrives as `CharArray`.
            posted: EpicsValue::String("fault".into()),
        },
    ]
}

/// Build a one-record IOC, subscribe to VAL, run exactly one cycle, and return
/// the severity the cycle ended at with every VAL event it posted.
///
/// `busy` is module-owned and is not in Base's default registry, so this is an
/// application that opts in for the row it is about to load — the shape
/// `tests/module_records` documents. The other four rows are Base types and the
/// lookup finds nothing for them.
async fn one_cycle(
    kind: &str,
    db_text: &str,
    dbf: DbFieldType,
) -> (AlarmSeverity, Vec<EpicsValue>) {
    let mut builder = IocBuilder::new();
    if let Some(factory) = module_records::factories().remove(kind) {
        builder = builder.register_record_type(kind, move || factory());
    }
    let db: std::sync::Arc<PvDatabase> = builder
        .db_string(db_text, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;

    let mut rx = {
        let inst = db.get_record("R").unwrap();
        let mut g = inst.write();
        g.add_subscriber("VAL", 1, dbf, EventMask::VALUE.bits())
            .unwrap()
    };
    // iocInit's own initial post is not this cycle's.
    while rx.try_recv().is_ok() {}

    let mut visited = HashSet::new();
    db.process_record_with_links("R", &mut visited, 0)
        .await
        .unwrap();

    let mut posts = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        posts.push(ev.snapshot.value.clone());
    }
    let sevr = db.get_record("R").unwrap().read().common.sevr;
    (sevr, posts)
}

/// The arming cycle: VAL becomes IVOV and the VAL monitor for that write fires
/// on this cycle, not the next one.
#[epics_macros_rs::epics_test]
async fn the_ivov_write_posts_on_the_cycle_that_makes_it() {
    let mut got = Vec::new();
    let mut want = Vec::new();
    for rec in armed_records() {
        let db_text = format!(
            r#"record({},"R") {{ field(IVOA,"Set output to IVOV") {} }}"#,
            rec.kind, rec.fields
        );
        let (sevr, posts) = one_cycle(rec.kind, &db_text, rec.dbf).await;

        assert_eq!(
            sevr,
            AlarmSeverity::Invalid,
            "{}: a bare instance is UDF, which is what arms IVOA",
            rec.kind
        );
        got.push((rec.kind, posts));
        want.push((rec.kind, vec![rec.posted.clone()]));
    }
    // One assertion over the whole table: a record type that regresses is
    // named alongside the four that did not, rather than aborting the loop.
    assert_eq!(
        got, want,
        "the IVOV write must post exactly once, on its own cycle, carrying \
         the value the arm just stored"
    );
}

/// The same INVALID cycle with the arm disabled. VAL is never written, so the
/// value monitor must stay silent — the post above belongs to the arm, not to
/// the INVALID severity, and nothing may be posted early.
#[epics_macros_rs::epics_test]
async fn an_invalid_cycle_that_does_not_arm_posts_no_value() {
    let mut got = Vec::new();
    for rec in armed_records() {
        let db_text = format!(
            r#"record({},"R") {{ field(IVOA,"Don't drive outputs") {} }}"#,
            rec.kind, rec.fields
        );
        let (sevr, posts) = one_cycle(rec.kind, &db_text, rec.dbf).await;

        assert_eq!(
            sevr,
            AlarmSeverity::Invalid,
            "{}: the severity is the same as the armed case",
            rec.kind
        );
        got.push((rec.kind, posts));
    }
    let want: Vec<_> = armed_records()
        .iter()
        .map(|r| (r.kind, Vec::new()))
        .collect();
    assert_eq!(
        got, want,
        "IVOA is not Set_output_to_IVOV, so VAL is untouched and no value \
         event may be posted"
    );
}
