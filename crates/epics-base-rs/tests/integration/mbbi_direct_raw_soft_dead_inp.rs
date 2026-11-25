//! `devMbbiDirectSoftRaw::read_mbbi` returns 0 whatever `dbGetLink` did, so
//! a dead INP still converts: softIoc R7.0.10 after one process reads VAL
//! 10, UDF 0 and LINK/INVALID for RVAL 10, where the Soft Channel twin keeps
//! VAL 10 unconverted.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::AlarmSeverity;
use epics_base_rs::types::EpicsValue;
use std::collections::HashMap;

#[epics_macros_rs::epics_test]
async fn raw_soft_dead_inp_converts_like_softioc() {
    let (db, _) = IocBuilder::new()
        .db_string(
            r#"
record(mbbiDirect, "M") { field(DTYP, "Raw Soft Channel") field(INP, "NOPE CA") field(RVAL, "10") }
record(mbbiDirect, "MD") { field(DTYP, "Raw Soft Channel") field(INP, "NOPE") field(RVAL, "10") }
record(mbbiDirect, "MS") { field(DTYP, "Soft Channel") field(INP, "NOPE CA") field(VAL, "10") }
"#,
            &HashMap::new(),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    let mut wrong = Vec::new();
    for (name, init_udf) in [("M", 1), ("MD", 1), ("MS", 0)] {
        let rec = db.get_record(name).unwrap();
        let before = rec.read().common.udf;
        let mut visited = epics_base_rs::server::database::ProcStack::new();
        db.process_record_with_links(name, &mut visited)
            .await
            .or_else(epics_base_rs::error::CaError::unless_process_status)
            .unwrap();
        let inst = rec.read();
        let got = (
            before,
            inst.record.get_field("VAL"),
            inst.common.udf,
            inst.common.stat,
            inst.common.sevr,
        );
        let want = (
            init_udf,
            Some(EpicsValue::Long(10)),
            0,
            alarm_status::LINK_ALARM,
            AlarmSeverity::Invalid,
        );
        if got != want {
            wrong.push(format!("{name}: got {got:?}, softIoc {want:?}"));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
