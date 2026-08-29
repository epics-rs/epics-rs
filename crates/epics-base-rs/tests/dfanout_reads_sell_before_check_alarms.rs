//! dfanout reads SELL BEFORE `checkAlarms`, so a dead SELL gates its outputs
//! through IVOA.
//!
//! ```c
//! /* dfanoutRecord.c::process */
//! prec->pact = TRUE;                                              /* :123 */
//! recGblGetTimeStamp(prec);                                       /* :124 */
//! dbGetLink(&(prec->sell), DBR_USHORT, &(prec->seln), 0, 0);      /* :126 */
//! checkAlarms(prec);                                              /* :127 */
//! if (prec->nsev < INVALID_ALARM)                                 /* :128 */
//!     push_values(prec);
//! else switch (prec->ivoa) { ... }
//! ```
//!
//! The SELL read is a `dbGetLink`, so a failure runs `setLinkAlarm` —
//! `LINK_ALARM` / `INVALID` — into `nsev`, and the very next line is the test
//! that sends the cycle down the IVOA branch. The port read SELL at the top of
//! the multi-output dispatch instead, which runs AFTER `check_alarms` and after
//! the IVOA decision, so a dfanout with an unreachable SELL drove its outputs
//! anyway.
//!
//! dfanout is the only one of the three SELL records where the order is
//! observable: `fanoutRecord.c:103` and `seqRecord.c:152` read SELL in the same
//! routine that consumes SELN and neither record has a `checkAlarms` at all.
//!
//! UDF has to be cleared for the test to isolate the SELL read, hence
//! `field(DOL,"5")` — a constant DOL, which `init_record` loads into VAL with
//! `prec->udf = isnan(prec->val)` (`dfanoutRecord.c:105-106`). Without it a
//! bare dfanout stays UDF=1 and `checkAlarms` raises INVALID every cycle on its
//! own, which would make the assertion pass for the wrong reason.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::AlarmSeverity;

const DB: &str = r#"
record(ai, "SRC")  { field(VAL, "0") }
record(ai, "DEAD:T") { field(VAL, "0") }
record(ai, "LIVE:T") { field(VAL, "0") }

# SELL cannot be read: the failure must reach nsev before checkAlarms.
record(dfanout, "DEAD") {
    field(DOL,  "5")
    field(SELM, "All")
    field(IVOA, "Don't drive outputs")
    field(SELL, "NOSUCH")
    field(OUTA, "DEAD:T")
}

# The control: an identical record whose SELL reads fine, so nsev stays
# NO_ALARM and push_values runs.
record(dfanout, "LIVE") {
    field(DOL,  "5")
    field(SELM, "All")
    field(IVOA, "Don't drive outputs")
    field(SELL, "SRC")
    field(OUTA, "LIVE:T")
}
"#;

type Db = Arc<PvDatabase>;

async fn build() -> Db {
    IocBuilder::new()
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

async fn process(db: &Db, name: &str) {
    let mut visited = HashSet::new();
    db.process_record_with_links(name, &mut visited, 0)
        .await
        .unwrap();
}

fn val(db: &Db, name: &str) -> f64 {
    db.get_pv(name).unwrap().to_f64().unwrap()
}

#[epics_macros_rs::epics_test]
async fn a_dead_sell_gates_the_outputs_through_ivoa() {
    let db = build().await;
    process(&db, "DEAD").await;

    assert_eq!(
        val(&db, "DEAD:T"),
        0.0,
        "the SELL read failed before checkAlarms, so IVOA=Don't drive outputs applies"
    );

    let (stat, sevr) = {
        let inst = db.get_record("DEAD").unwrap();
        let g = inst.read();
        (g.common.stat, g.common.sevr)
    };
    assert_eq!(
        stat,
        alarm_status::LINK_ALARM,
        "setLinkAlarm on the SELL read"
    );
    assert_eq!(sevr, AlarmSeverity::Invalid);
}

/// The boundary: a healthy SELL leaves `nsev` below INVALID, so the same record
/// pushes as usual. Moving the read earlier must not suppress that.
#[epics_macros_rs::epics_test]
async fn a_live_sell_still_drives_the_outputs() {
    let db = build().await;
    process(&db, "LIVE").await;

    assert_eq!(val(&db, "LIVE:T"), 5.0);
    let sevr = db.get_record("LIVE").unwrap().read().common.sevr;
    assert_eq!(sevr, AlarmSeverity::NoAlarm);
}
