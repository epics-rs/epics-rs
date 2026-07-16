//! A direct `caput <rec>.UDF` wins over the calc family's monotonic
//! undefined-cell.
//!
//! acalcout/scalcout maintain UDF as C's `pcalc->udf`: undefined until a calc
//! successfully defines VAL (reported through `value_is_undefined()`). UDF is
//! `pp(TRUE)` (`dbCommon.dbd:552`), so a `caput UDF 0` on a Passive record
//! stores the byte AND triggers a process that re-derives `common.udf` from
//! that cell. Without syncing the cell, the cell (=1 on a fresh empty-CALC
//! record) clobbers the put — the oracle measured `caput ACALCOUT.UDF 0`
//! → caget UDF: C=0, port=1. `Record::set_udf_from_put` makes the put win
//! while a record with no UDF put stays undefined (init 1).

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(acalcout, "A") {}
record(scalcout, "S") {}
"#;

async fn build() -> std::sync::Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(DB, &std::collections::HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

/// `caput <rec>.UDF 0` must STAND at 0 through UDF's `pp(TRUE)` re-process; a
/// fresh empty-CALC record reads 1 (undefined). Both records, matching C.
#[tokio::test]
async fn caput_udf_zero_stands_over_the_monotonic_cell() {
    let db = build().await;
    for rec in ["A", "S"] {
        // Fresh empty-CALC record is undefined (C `iocInit` udf=TRUE).
        let fresh = db.get_pv(&format!("{rec}.UDF")).await.unwrap();
        assert_eq!(
            fresh.to_f64(),
            Some(1.0),
            "{rec}.UDF fresh should read 1 (undefined)"
        );
        // Direct put UDF 0; await the pp-driven process so the re-derivation
        // has run before we read back.
        if let Some(rx) = db
            .put_record_field_from_ca(rec, "UDF", EpicsValue::Long(0))
            .await
            .unwrap()
        {
            let _ = rx.await;
        }
        let got = db.get_pv(&format!("{rec}.UDF")).await.unwrap();
        assert_eq!(
            got.to_f64(),
            Some(0.0),
            "{rec}.UDF must stand at 0 after caput UDF 0 (put wins over the cell)"
        );
    }
}
