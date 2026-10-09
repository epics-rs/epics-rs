#![cfg(unix)]
//! A second `record(...)` block merged onto an installed record writes every
//! field through the instance put funnel, so the stored OMSL/OIF menu indices
//! and the link slots follow the merged text. softIoc R7.0.10, one file holding
//! both blocks, `caput X.PROC 1`:
//!
//! ```text
//! AM  (DOL "S1 NPP", merged OMSL closed_loop)   VAL 5   NO_ALARM
//! AI2 (closed_loop, merged OIF Incremental)     VAL 6, 11, 16: +5 a cycle
//! DM  (merged DOL "S2 NPP")                     VAL 7   NO_ALARM
//! CO  (calcout, merged INPA "S2 NPP", CALC A)   VAL 7   NO_ALARM
//! SN  (aSub LFLG READ, merged SUBL NOEXIST->S1) STAT/SEVR NO_ALARM
//! ```

use std::sync::Arc;

use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::iocsh::IocShell;
use epics_base_rs::types::EpicsValue;

const FIRST: &str = r#"
record(ao,"S1"){field(VAL,"5")}
record(ao,"S2"){field(VAL,"7")}
record(ao,"AM"){field(DOL,"S1 NPP")}
record(ao,"AI2"){field(DOL,"S1 NPP") field(OMSL,"closed_loop") field(VAL,"1")}
record(ao,"AI3"){field(DOL,"S1 NPP") field(OMSL,"closed_loop") field(VAL,"1") field(OIF,"Incremental")}
record(ao,"DM"){field(OMSL,"closed_loop") field(DOL,"S1 NPP")}
record(calcout,"CO"){field(CALC,"A") field(INPA,"S1 NPP")}
record(calcout,"CO1"){field(CALC,"A") field(INPA,"S2 NPP")}
record(aSub,"SN"){field(LFLG,"READ") field(SUBL,"NOEXIST.VAL")}
"#;

const SECOND: &str = r#"
record(ao,"AM"){field(OMSL,"closed_loop")}
record(ao,"AI2"){field(OIF,"Incremental")}
record(ao,"DM"){field(DOL,"S2 NPP")}
record(calcout,"CO"){field(INPA,"S2 NPP")}
record(aSub,"SN"){field(SUBL,"S1")}
"#;

fn load(db: &Arc<PvDatabase>, dir: &std::path::Path, name: &str, body: &str) {
    std::fs::write(dir.join(name), body).unwrap();
    let path = dir.join(name).display().to_string();
    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    let shell_db = Arc::clone(db);
    std::thread::spawn(move || {
        IocShell::new(shell_db, bridge).execute_line(&format!("dbLoadRecords(\"{path}\")"))
    })
    .join()
    .unwrap()
    .unwrap();
}

async fn merged() -> Arc<PvDatabase> {
    let db = Arc::new(PvDatabase::new());
    let dir = tempfile::tempdir().unwrap();
    load(&db, dir.path(), "first.db", FIRST);
    load(&db, dir.path(), "merge.db", SECOND);
    let bridge = epics_base_rs::runtime::task::BlockingBridge::capture();
    let shell_db = Arc::clone(&db);
    std::thread::spawn(move || IocShell::new(shell_db, bridge).execute_line("iocInit"))
        .join()
        .unwrap()
        .unwrap();
    db
}

async fn process(db: &PvDatabase, rec: &str) {
    let mut visited = ProcStack::new();
    db.process_record_with_links(rec, &mut visited)
        .await
        .unwrap();
}

#[epics_macros_rs::epics_test]
async fn a_merged_block_is_seen_by_the_next_cycle() {
    let db = merged().await;
    for (rec, want) in [("AM", 5.0), ("DM", 7.0), ("CO1", 7.0), ("CO", 7.0)] {
        process(&db, rec).await;
        assert_eq!(db.get_pv(rec).unwrap(), EpicsValue::Double(want), "{rec}");
    }
    process(&db, "SN").await;
    let sevr = db.get_pv("SN.SEVR").unwrap();
    assert_eq!(sevr, EpicsValue::Short(0), "SN: {sevr:?}");
}

/// Incremental adds the source each cycle, so a merged OIF is the one that
/// makes the second cycle differ from the first by the source; AI3 is the same
/// record declared in one block.
#[epics_macros_rs::epics_test]
async fn a_merged_oif_increments_like_the_single_block_record() {
    let db = merged().await;
    for rec in ["AI3", "AI2"] {
        let mut seen = Vec::new();
        for _ in 0..3 {
            process(&db, rec).await;
            seen.push(db.get_pv(rec).unwrap());
        }
        let EpicsValue::Double(first) = seen[0] else {
            panic!("{seen:?}")
        };
        assert_eq!(
            seen[1..],
            [
                EpicsValue::Double(first + 5.0),
                EpicsValue::Double(first + 10.0)
            ],
            "{rec}"
        );
    }
}
