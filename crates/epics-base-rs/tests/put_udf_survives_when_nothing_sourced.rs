//! A `caput REC.UDF <v>` drives processing (wave-23: UDF is `pp(TRUE)`), but a
//! process cycle that SOURCES nothing must NOT re-derive UDF from VAL — the
//! client's UDF put stands. This is the C invariant: UDF is re-derived only
//! where a value is actually sourced/recomputed.
//!
//! `stringinRecord.c::process` / `lsiRecord.c::process` / `aSubRecord.c::process`
//! have NO unconditional `prec->udf = isnan(val)`; they clear UDF only inside a
//! sourced read (`devSiSoft.c::read_stringin`, `devLsiSoft.c`) or a subroutine
//! run (`aSubRecord.c::do_sub`). softIoc (EPICS 7.0.10, linux-x86_64), each
//! record with a DEFINED VAL:
//!
//! ```text
//! record(stringin,"SI"){field(VAL,"hi")}  caput UDF 1 -> UDF 1 ; caput UDF 0 -> 0
//! record(lsi,"LSI"){field(VAL,"hi")}       caput UDF 1 -> UDF 1 ; caput UDF 0 -> 0
//! record(aSub,"ASUB"){}                    caput UDF 1 -> UDF 1 ; caput UDF 0 -> 0
//! record(longin,"LI"){field(VAL,"3")}      caput UDF 1 -> UDF 0  (re-derives)
//! ```
//!
//! The port previously re-derived UDF in the shared process epilogue for EVERY
//! record type, so a UDF-put-driven cycle on stringin/lsi/aSub clobbered the
//! client's `UDF=1` back to 0.

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{Record, SubroutineFn};
use epics_base_rs::server::records::asub_record::ASubRecord;
use epics_base_rs::server::records::longin::LonginRecord;
use epics_base_rs::server::records::lsi::LsiRecord;
use epics_base_rs::server::records::stringin::StringinRecord;
use epics_base_rs::server::records::stringout::StringoutRecord;
use epics_base_rs::types::EpicsValue;

async fn udf(db: &PvDatabase, name: &str) -> bool {
    let rec = db.get_record(name).unwrap();
    let inst = rec.read();
    inst.common.udf != 0
}

/// A UDF put drives processing; because the cycle sources nothing, the put
/// value survives — both `UDF=1` and `UDF=0` stand.
#[epics_macros_rs::epics_test]
async fn stringin_udf_put_survives_both_directions() {
    let db = PvDatabase::new();
    db.add_record("SI", Box::new(StringinRecord::new("hi")))
        .await
        .unwrap();

    db.put_record_field_from_ca_no_notify("SI", "UDF", EpicsValue::Char(1))
        .await
        .expect("UDF put accepted");
    assert!(
        udf(&db, "SI").await,
        "stringin sourced nothing this cycle — the client's UDF=1 stands (softIoc: SI)"
    );

    db.put_record_field_from_ca_no_notify("SI", "UDF", EpicsValue::Char(0))
        .await
        .expect("UDF put accepted");
    assert!(
        !udf(&db, "SI").await,
        "wave-23 preserved: the record still processes and UDF=0 stands (softIoc: SI)"
    );
}

/// lsi: same shape — a constant/empty INP sources nothing at process, so the
/// UDF put is not clobbered.
#[epics_macros_rs::epics_test]
async fn lsi_udf_put_survives() {
    let db = PvDatabase::new();
    db.add_record("LSI", Box::new(LsiRecord::new("hi")))
        .await
        .unwrap();

    db.put_record_field_from_ca_no_notify("LSI", "UDF", EpicsValue::Char(1))
        .await
        .expect("UDF put accepted");
    assert!(
        udf(&db, "LSI").await,
        "lsi sourced nothing this cycle — the client's UDF=1 stands (softIoc: LSI)"
    );
}

/// aSub with NO subroutine runs no `do_sub`, so it sources nothing and the UDF
/// put stands.
#[epics_macros_rs::epics_test]
async fn asub_udf_put_survives_without_subroutine() {
    let db = PvDatabase::new();
    db.add_record("ASUB", Box::new(ASubRecord::default()))
        .await
        .unwrap();

    db.put_record_field_from_ca_no_notify("ASUB", "UDF", EpicsValue::Char(1))
        .await
        .expect("UDF put accepted");
    assert!(
        udf(&db, "ASUB").await,
        "aSub with no SNAM runs no do_sub — the client's UDF=1 stands (softIoc: ASUB)"
    );
}

/// aSub WITH a subroutine that runs and returns `>= 0` DOES clear UDF — C
/// `aSubRecord.c::do_sub` (`else prec->udf = FALSE`). This is aSub's own UDF
/// clear, the compensating source-clear for opting out of the blanket re-derive.
#[epics_macros_rs::epics_test]
async fn asub_clears_udf_when_subroutine_runs() {
    let db = PvDatabase::new();

    let mut registry: HashMap<String, Arc<SubroutineFn>> = HashMap::new();
    registry.insert(
        "writer".into(),
        Arc::new(Box::new(|rec: &mut dyn Record| {
            rec.put_field("VALA", EpicsValue::Double(42.0))?;
            Ok(0_i64)
        }) as SubroutineFn),
    );
    db.install_subroutine_registry(registry).await;

    // Bind the subroutine the realistic way: a SUBL DB link to a record holding
    // the name, with LFLG=READ so the framework resolves + binds it at process
    // (the same wiring `asub_output_links.rs` uses).
    db.add_record("NAME_HOLDER", Box::new(StringoutRecord::new("writer")))
        .await
        .unwrap();
    let mut rec = ASubRecord::default();
    rec.put_field("SUBL", EpicsValue::String("NAME_HOLDER".into()))
        .unwrap();
    rec.put_field("LFLG", EpicsValue::Short(1)).unwrap();
    db.add_record("ASUB", Box::new(rec)).await.unwrap();
    // Start undefined, then process: a running subroutine defines the record.
    {
        let r = db.get_record("ASUB").unwrap();
        r.write().common.udf = 1;
    }

    let mut visited = std::collections::HashSet::new();
    db.process_record_with_links("ASUB", &mut visited, 0)
        .await
        .unwrap();

    assert!(
        !udf(&db, "ASUB").await,
        "a subroutine that ran and returned >= 0 clears UDF (aSubRecord.c:469-470)"
    );
    let r = db.get_record("ASUB").unwrap();
    assert_eq!(
        r.read().record.get_field("VALA").and_then(|v| v.to_f64()),
        Some(42.0),
        "the subroutine actually ran (VALA written) — processing was driven"
    );
}

/// Control: a record whose C `process()` re-derives UDF UNCONDITIONALLY
/// (`longinRecord.c:148` `if(status==0) prec->udf = FALSE`) is UNTOUCHED by the
/// opt-out — a UDF-put-driven cycle re-derives to 0, matching C.
#[epics_macros_rs::epics_test]
async fn longin_still_re_derives_udf_on_process() {
    let db = PvDatabase::new();
    db.add_record("LI", Box::new(LonginRecord::new(3)))
        .await
        .unwrap();

    db.put_record_field_from_ca_no_notify("LI", "UDF", EpicsValue::Char(1))
        .await
        .expect("UDF put accepted");
    assert!(
        !udf(&db, "LI").await,
        "longin re-derives UDF every process; a defined VAL clears it to 0 (softIoc: LI)"
    );
}
