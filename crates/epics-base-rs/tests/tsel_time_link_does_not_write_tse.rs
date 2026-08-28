//! A `.TIME` `TSEL` must not move `TSE`, which is a field clients read.
//!
//! C keeps the fact that `TSEL` names `.TIME` in `plink->flags`
//! (`DBLINK_FLAG_TSELisTIME`, set by `TSEL_modified`, `dbLink.c:81-85`) and
//! `recGblGetTimeStampSimm` `return`s on that flag before the `TSE` half runs
//! (`recGbl.c:316-321`). There is no assignment to `prec->tse` anywhere in
//! epics-base — every occurrence is a comparison — so the operator's declared
//! `TSE` survives a `.TIME` `TSEL` untouched.
//!
//! The port used to write `-2` there to make its own `apply_timestamp` skip
//! the event lookup, which put a value into `TSE` that the database never
//! declared and overwrote one that did. Measured over CA on the same `.db`,
//! `caget <rec>.TSE`, C softIoc `R7.0.10-146-g8f5015b66` vs `softioc-rs`:
//!
//! | record                                  | C  | port (pre-fix) |
//! |-----------------------------------------|----|----------------|
//! | `.TIME` TSEL, `TSE` undeclared          |  0 | -2             |
//! | `.TIME` TSEL, `field(TSE,"5")`          |  5 | -2             |
//! | non-`.TIME` TSEL, `field(TSE,"7")`      |  1 | 1              |
//!
//! The third row is the control: a `TSEL` that is not `.TIME` IS loaded into
//! `TSE` (`recGbl.c:322`, `dbGetLink(..., &prec->tse, ...)`), and both agree.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

type Db = Arc<epics_base_rs::server::database::PvDatabase>;

const SRC_STAMP: Duration = Duration::new(4_000_000, 123_456_789);

const DB_TEXT: &str = r#"
record(ai, "TSEW_SRC")   { field(DTYP, "Soft Channel") field(VAL, "1") }
record(ai, "TSEW_UNDEC") { field(DTYP, "Soft Channel") field(TSEL, "TSEW_SRC.TIME") }
record(ai, "TSEW_FIVE")  { field(DTYP, "Soft Channel") field(TSEL, "TSEW_SRC.TIME") field(TSE, "5") }
"#;

fn tse_of(db: &Db, rec: &str) -> i16 {
    let inst = db.get_record(rec).expect("record exists");
    let g = inst.read();
    match g.client_field_value("TSE").expect("TSE resolves") {
        EpicsValue::Short(v) => v,
        other => panic!("{rec}.TSE is not DBF_SHORT: {other:?}"),
    }
}

#[epics_macros_rs::epics_test]
async fn a_time_tsel_leaves_the_declared_tse_alone() {
    let db: Db = IocBuilder::new()
        .db_string(DB_TEXT, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0;

    {
        let rec = db.get_record("TSEW_SRC").expect("source exists");
        let mut inst = rec.write();
        inst.common.time = SystemTime::UNIX_EPOCH + SRC_STAMP;
    }

    for rec in ["TSEW_UNDEC", "TSEW_FIVE"] {
        let mut visited = HashSet::new();
        db.process_record_with_links(rec, &mut visited, 0)
            .await
            .unwrap();
    }

    // The adoption still happened — otherwise TSE could be right for the
    // wrong reason.
    for rec in ["TSEW_UNDEC", "TSEW_FIVE"] {
        let inst = db.get_record(rec).expect("record exists");
        let g = inst.read();
        assert_eq!(
            g.common.time,
            SystemTime::UNIX_EPOCH + SRC_STAMP,
            "{rec} must still adopt the source stamp"
        );
    }

    assert_eq!(tse_of(&db, "TSEW_UNDEC"), 0, "undeclared TSE stays 0, as C");
    assert_eq!(
        tse_of(&db, "TSEW_FIVE"),
        5,
        "field(TSE,\"5\") survives, as C"
    );
}
