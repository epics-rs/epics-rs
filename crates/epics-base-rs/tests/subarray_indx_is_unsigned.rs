//! INDX is `DBF_ULONG` (`subArrayRecord.dbd.pod:385`) and C clamps it with an
//! `epicsUInt32` compare in `readValue`:
//!
//! ```c
//! if (prec->nelm > prec->malm)  prec->nelm = prec->malm;
//! if (prec->indx >= prec->malm) prec->indx = prec->malm - 1;
//! ```
//!
//! (`subArrayRecord.c:310-314`). `caput SA.INDX -1` is a legitimate way to
//! reach 0xFFFFFFFF — `epicsParseUInt32` goes through `strtoul`, which negates
//! in unsigned arithmetic (`epicsStdlib.c:263-278`) — and C then clamps it to
//! MALM-1, so `subset` gets `ecount = nRequest - (MALM-1) <= 0` and the record
//! reports itself UNDEFINED.
//!
//! The boundaries below are the clamp's own: MALM-1 (inside), MALM (the first
//! value clamped), and the full unsigned range (the value a signed compare
//! misreads as -1).

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::types::EpicsValue;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

const DB: &str = r#"
record(subArray, "SA") {
    field(FTVL, "DOUBLE") field(MALM, "10") field(NELM, "5") field(INDX, "0")
}
"#;

async fn build() -> Arc<epics_base_rs::server::database::PvDatabase> {
    IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

async fn process(db: &epics_base_rs::server::database::PvDatabase) {
    let mut visited = HashSet::new();
    db.process_record_with_links("SA", &mut visited, 0)
        .await
        .unwrap();
}

/// `caput -a SA 10 0 1 .. 9` then process: VAL is `pp(TRUE)`, so C slices
/// straight away and NORD lands at NELM.
async fn seeded() -> Arc<epics_base_rs::server::database::PvDatabase> {
    let db = build().await;
    db.put_pv(
        "SA",
        EpicsValue::DoubleArray((0..10).map(f64::from).collect()),
    )
    .await
    .unwrap();
    process(&db).await;
    db
}

struct Probe {
    indx: u32,
    nord: i64,
    sevr: AlarmSeverity,
    served: u32,
}

fn probe(db: &epics_base_rs::server::database::PvDatabase) -> Probe {
    let inst = db.get_record("SA").unwrap();
    let g = inst.read();
    let indx = match g.record.get_field("INDX") {
        Some(EpicsValue::ULong(v)) => v,
        other => panic!("INDX is DBF_ULONG, got {other:?}"),
    };
    Probe {
        indx,
        nord: g.record.get_field("NORD").unwrap().to_f64().unwrap() as i64,
        sevr: g.common.sevr,
        served: g.record.get_field("VAL").unwrap().count(),
    }
}

async fn put_indx_and_process(db: &epics_base_rs::server::database::PvDatabase, v: EpicsValue) {
    db.put_pv("SA.INDX", v).await.unwrap();
    process(db).await;
}

#[epics_macros_rs::epics_test]
async fn the_seeded_record_slices_from_the_start() {
    let db = seeded().await;
    let p = probe(&db);
    assert_eq!(p.indx, 0);
    assert_eq!(p.nord, 5, "NELM 5 out of a 10-element buffer");
    assert_eq!(p.served, 5);
}

/// Boundary: INDX == MALM - 1, the largest value the clamp leaves alone.
#[epics_macros_rs::epics_test]
async fn indx_one_below_malm_is_not_clamped() {
    let db = seeded().await;
    put_indx_and_process(&db, EpicsValue::ULong(9)).await;
    let p = probe(&db);
    assert_eq!(p.indx, 9, "9 < MALM, so readValue leaves it");
    assert_eq!(p.nord, 0, "ecount = 5 - 9 <= 0");
    assert_eq!(p.served, 0);
}

/// Boundary: INDX == MALM, the first value the clamp moves.
#[epics_macros_rs::epics_test]
async fn indx_at_malm_is_clamped_to_malm_minus_one() {
    let db = seeded().await;
    put_indx_and_process(&db, EpicsValue::ULong(10)).await;
    assert_eq!(probe(&db).indx, 9);
}

/// Boundary: the full unsigned range, reached the way an operator reaches it.
/// A signed compare reads 0xFFFFFFFF as -1, leaves INDX untouched, and the
/// record goes on serving its slice as if nothing had been asked of it.
#[epics_macros_rs::epics_test]
async fn indx_at_the_full_unsigned_range_is_clamped_like_c() {
    for put in [
        EpicsValue::ULong(u32::MAX),
        EpicsValue::Long(-1),
        EpicsValue::String("-1".into()),
    ] {
        let db = seeded().await;
        put_indx_and_process(&db, put.clone()).await;
        let p = probe(&db);
        assert_eq!(
            p.indx, 9,
            "{put:?}: strtoul gives 0xFFFFFFFF, C clamps to MALM-1"
        );
        assert_eq!(p.nord, 0, "{put:?}: ecount = 5 - 9 <= 0");
        assert_eq!(
            p.sevr,
            AlarmSeverity::Invalid,
            "{put:?}: an empty slice is UDF_ALARM at UDFS"
        );
        assert_eq!(p.served, 0, "{put:?}: an undefined subArray serves nothing");
    }
}
