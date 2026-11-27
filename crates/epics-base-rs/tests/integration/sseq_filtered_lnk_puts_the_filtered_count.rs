//! An sseq `LNKn` into a CHAR waveform sizes its put by the link's final
//! element count, through its filters: C `sseqRecord.c:760`
//! `dbGetNelements(&plinkGroup->lnk, ...)` is `dbChannelFinalElements`
//! (`dbDbLink.c:157-162`), not `dbNameToAddr`'s `no_elements`.
//!
//! C oracleIoc (calc R2-x, softIoc R7.0.10), CHAR waveforms NELM 40 and
//! `DO1 = 7`, `STR1 = "7"`:
//!
//! ```text
//! LNK1 WF0.VAL{"arr":{"s":0,"e":0}}  ->  WF0 = 7       NORD 1
//! LNK1 WF4.VAL{"arr":{"s":0,"e":4}}  ->  WF4 = "7"[5]  NORD 5
//! LNK1 WFN.VAL                       ->  WFN = "7"[40] NORD 40
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use epics_base_rs::server::database::{ProcStack, PvDatabase};
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::sseq::SseqRecord;
use epics_base_rs::types::EpicsValue;

const DB: &str = r#"
record(waveform, "WF0") { field(FTVL, "CHAR") field(NELM, "40") }
record(waveform, "WF4") { field(FTVL, "CHAR") field(NELM, "40") }
record(waveform, "WFN") { field(FTVL, "CHAR") field(NELM, "40") }
record(sseq, "SQ0") { field(DO1, "7") field(STR1, "7") field(LNK1, "WF0.VAL{\"arr\":{\"s\":0,\"e\":0}}") }
record(sseq, "SQ4") { field(DO1, "7") field(STR1, "7") field(LNK1, "WF4.VAL{\"arr\":{\"s\":0,\"e\":4}}") }
record(sseq, "SQN") { field(DO1, "7") field(STR1, "7") field(LNK1, "WFN.VAL") }
"#;

async fn build() -> Arc<PvDatabase> {
    IocBuilder::new()
        .register_record_type("sseq", || Box::new(SseqRecord::default()))
        .db_string(DB, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap()
        .0
}

/// Process `name` and wait until it leaves PACT: the step fires from a
/// callback, after the process call returns.
async fn run(db: &PvDatabase, name: &str) {
    db.process_record_with_links(name, &mut ProcStack::new())
        .await
        .unwrap();
    let cell = db.get_record(name).unwrap();
    epics_base_rs::runtime::task::timeout(fact_budget::FACT_BUDGET, async {
        while cell.read().is_processing() {
            epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{name} never left PACT"));
}

/// `"7"` NUL-padded to `n` bytes, as C puts it straight out of `char s[40]`.
fn seven(n: usize) -> EpicsValue {
    let mut bytes = vec![0u8; n];
    bytes[0] = b'7';
    EpicsValue::CharArray(bytes.into())
}

fn nord(db: &PvDatabase, wf: &str) -> Option<f64> {
    db.get_pv(format!("{wf}.NORD")).unwrap().to_f64()
}

/// A filter that leaves one element makes the target a scalar, so C's CHAR
/// arm falls back to `DBR_DOUBLE` from `DO1`.
#[epics_macros_rs::epics_test]
async fn a_lnk_filtered_to_one_element_puts_the_double() {
    let db = build().await;
    run(&db, "SQ0").await;
    assert_eq!(
        db.get_pv("WF0").unwrap(),
        EpicsValue::CharArray(vec![7].into())
    );
    assert_eq!(nord(&db, "WF0"), Some(1.0));
}

/// A filter that leaves five elements puts five bytes of `STR1`.
#[epics_macros_rs::epics_test]
async fn a_lnk_filtered_to_five_elements_puts_five_bytes() {
    let db = build().await;
    run(&db, "SQ4").await;
    assert_eq!(db.get_pv("WF4").unwrap(), seven(5));
    assert_eq!(nord(&db, "WF4"), Some(5.0));
}

/// Unfiltered, the count is the waveform's NELM, capped at 40.
#[epics_macros_rs::epics_test]
async fn an_unfiltered_lnk_puts_forty_bytes() {
    let db = build().await;
    run(&db, "SQN").await;
    assert_eq!(db.get_pv("WFN").unwrap(), seven(40));
    assert_eq!(nord(&db, "WFN"), Some(40.0));
}
