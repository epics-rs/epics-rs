//! compress fires FLNK / monitor / timestamp only when it emits a compressed
//! sample, not on every input cycle.
//!
//! C `compressRecord.c::process` runs the completion epilogue
//! (`prec->udf = FALSE; recGblGetTimeStamp; monitor; recGblFwdLink`) inside
//! `if (status != 1)` — i.e. only when the cycle emitted (`status == 0`). A
//! record still accumulating toward its next compressed sample returns
//! `status == 1` and fires none of them. The Rust port computed the
//! compression in `push_array`/`push_value` (driven by the pre-process INP
//! read) but `process()` returned `complete()` unconditionally, so every
//! input cycle fired the forward link. This pins the gate: a rolling-Average
//! compress with N=4, fed one sample per cycle, must fire its FLNK exactly
//! once over four cycles — on the 4th, when the average is emitted.

use std::collections::HashSet;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::calc::CalcRecord;
use epics_base_rs::server::records::compress::CompressRecord;
use epics_base_rs::types::EpicsValue;

#[epics_macros_rs::epics_test]
async fn compress_fires_flnk_only_on_emit_not_every_cycle() {
    let db = Arc::new(PvDatabase::new());

    // Source scalar the compress reads each cycle.
    db.add_record("src", Box::new(AiRecord::new(0.0)))
        .await
        .unwrap();

    // Rolling Average (ALG index 3), N=4, NSAM=1: accumulates one sample per
    // cycle and emits the average only on the 4th. INP reads `src`.
    //
    // INP is a dbCommon link on a compress (`COMPRESS_FIELDS` declares no INP,
    // matching `compressRecord.dbd.pod`), so it is set through the common field
    // — the same place a `.db`'s `field(INP,"src")` lands. It used to be set on
    // a `CompressRecord::inp` field that only this construction path could
    // reach; that field is gone (R18-106).
    let mut cmp = CompressRecord::new(1, 3);
    cmp.n = 4;
    db.add_record("cmp", Box::new(cmp)).await.unwrap();
    {
        let rec = db.get_record("cmp").unwrap();
        let mut inst = rec.write();
        inst.put_common_field("INP", EpicsValue::String("src".into()))
            .unwrap();
    }

    // Counter sink: `CALC="VAL+1"` increments its VAL once per process, so its
    // VAL is exactly the number of times the compress's FLNK fired it.
    db.add_record("cnt", Box::new(CalcRecord::new("VAL+1")))
        .await
        .unwrap();

    // Wire cmp.FLNK = cnt (dbCommon forward link).
    {
        let rec = db.get_record("cmp").unwrap();
        let mut inst = rec.write();
        inst.put_common_field("FLNK", EpicsValue::String("cnt".into()))
            .unwrap();
    }

    // Feed four samples; the FLNK must stay silent until the emit cycle.
    for i in 1..=4 {
        db.put_pv("src", EpicsValue::Double(i as f64))
            .await
            .unwrap();
        let mut visited = HashSet::new();
        db.process_record_with_links("cmp", &mut visited, 0)
            .await
            .unwrap();

        let cnt = db.get_pv("cnt").unwrap().to_f64().unwrap();
        if i < 4 {
            assert_eq!(
                cnt, 0.0,
                "cycle {i}: compress is still accumulating (no emit) — FLNK must \
                 NOT have fired, but the counter is {cnt}"
            );
        }
    }

    // Exactly one emit over four cycles → FLNK fired once → counter == 1.
    let cnt = db.get_pv("cnt").unwrap().to_f64().unwrap();
    assert_eq!(
        cnt, 1.0,
        "FLNK must fire once (on the 4th/emit cycle), not on every input cycle"
    );

    // The emitted value is the average of 1..=4 = 2.5, confirming the emit
    // cycle is the one that drove the forward link.
    let val = db.get_pv("cmp").unwrap();
    let avg = match val {
        EpicsValue::DoubleArray(a) => a.first().copied().unwrap_or(f64::NAN),
        EpicsValue::Double(v) => v,
        other => panic!("unexpected compress VAL type: {other:?}"),
    };
    assert!(
        (avg - 2.5).abs() < 1e-9,
        "emitted average should be mean(1,2,3,4)=2.5, got {avg}"
    );
}
