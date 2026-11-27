//! SIOL landings through the unsigned SVAL of bi, mbbi and mbbiDirect,
//! against softIoc R7.0.10 (x86-64): `dbpf SRC <v>`, `dbpf <rec>.PROC 1`,
//! then `dbgf` of SVAL, RVAL and VAL. Every SIOL read requests `DBR_ULONG`
//! (`biRecord.c:326`, `mbbiRecord.c:388`, `mbbiDirectRecord.c:260`).

use std::collections::HashMap;
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(ao, "SRC") {}
record(bi, "BR") { field(SIMM, "RAW") field(SIOL, "SRC") }
record(bi, "BY") { field(SIMM, "YES") field(SIOL, "SRC") }
record(mbbi, "MR") { field(SIMM, "RAW") field(SIOL, "SRC") }
record(mbbi, "MY") { field(SIMM, "YES") field(SIOL, "SRC") }
record(mbbiDirect, "DR") { field(SIMM, "RAW") field(SIOL, "SRC") }
record(mbbiDirect, "DY") { field(SIMM, "YES") field(SIOL, "SRC") }
"#;

async fn build() -> Arc<PvDatabase> {
    IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

async fn land(db: &PvDatabase, rec: &str, v: f64) -> [EpicsValue; 3] {
    db.put_record_field_from_ca("SRC", "VAL", EpicsValue::Double(v))
        .await
        .unwrap();
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    db.process_record_with_links(rec, &mut visited)
        .await
        .unwrap();
    let inst = db.get_record(rec).unwrap();
    let g = inst.read();
    ["SVAL", "RVAL", "VAL"].map(|f| g.record.get_field(f).unwrap())
}

/// `biRecord.c:332` `prec->rval = (unsigned short) prec->sval` on RAW, and
/// `:329` `prec->val = prec->sval` into the `epicsEnum16` VAL on YES.
#[epics_macros_rs::epics_test]
async fn bi_raw_cuts_rval_to_sixteen_bits() {
    let db = build().await;
    use EpicsValue::{Enum, ULong};
    for (v, sval, rval) in [
        (70000.0, 70000, 4464),
        (-1.0, u32::MAX, 65535),
        (3e9, 3000000000, 24064),
        (5e9, 705032704, 61952),
    ] {
        let [s, r, _] = land(&db, "BR", v).await;
        assert_eq!((s, r), (ULong(sval), ULong(rval)), "bi RAW of {v}");
        let [s, _, val] = land(&db, "BY", v).await;
        assert_eq!((s, val), (ULong(sval), Enum(sval as u16)), "bi YES of {v}");
    }
}

/// `mbbiRecord.c:394` `prec->rval = prec->sval` and `:391` `prec->val =
/// prec->sval` into the `epicsEnum16` VAL.
#[epics_macros_rs::epics_test]
async fn mbbi_lands_sval_whole_on_raw_and_cut_on_yes() {
    let db = build().await;
    use EpicsValue::{Enum, ULong};
    for (v, sval) in [
        (70000.0, 70000),
        (-1.0, u32::MAX),
        (3e9, 3000000000),
        (5e9, 705032704),
    ] {
        let [s, r, _] = land(&db, "MR", v).await;
        assert_eq!((s, r), (ULong(sval), ULong(sval)), "mbbi RAW of {v}");
        let [s, _, val] = land(&db, "MY", v).await;
        assert_eq!(
            (s, val),
            (ULong(sval), Enum(sval as u16)),
            "mbbi YES of {v}"
        );
    }
}

/// `mbbiDirectRecord.c:260` reads `DBR_ULONG` into the `epicsInt32` SVAL, so
/// the bits land whole: `:266` `prec->rval = prec->sval` and `:263`
/// `prec->val = prec->sval`.
#[epics_macros_rs::epics_test]
async fn mbbi_direct_reads_siol_as_ulong_bits() {
    let db = build().await;
    use EpicsValue::{Long, ULong};
    for (v, bits) in [
        (70000.0, 70000u32),
        (-1.0, u32::MAX),
        (3e9, 3000000000),
        (5e9, 705032704),
    ] {
        let [s, r, _] = land(&db, "DR", v).await;
        assert_eq!(
            (s, r),
            (Long(bits as i32), ULong(bits)),
            "mbbiDirect RAW of {v}"
        );
        let [s, _, val] = land(&db, "DY", v).await;
        assert_eq!(
            (s, val),
            (Long(bits as i32), Long(bits as i32)),
            "mbbiDirect YES of {v}"
        );
    }
}
