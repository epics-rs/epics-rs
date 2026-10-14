//! A3 — an unresolvable ASG `INP*` link disables the rules that READ it, not
//! every CALC rule in the group.
//!
//! C `asComputePvt` (`asLibRoutines.c:1039-1040`):
//!
//! ```c
//! if(!pasgrule->calc
//! || (!(pasg->inpBad & pasgrule->inpUsed) && (pasgrule->result==1)))
//! ```
//!
//! `inpBad` is per-INPUT — `connectCallback` (`asCa.c:91-105`) sets a bit for a
//! channel that is not connected. `inpUsed` is per-RULE — `calcArgUsage`
//! computes it once at load (`asLibRoutines.c:1407`). The intersection is what
//! disables a rule, so a typo in an `INPB` that no rule mentions costs
//! nothing.
//!
//! Both of this port's resolvers aborted their link walk on the first
//! unresolvable link and handed the evaluator `None`, which failed EVERY
//! CALC-gated rule in the group — on the CA and PVA paths alike. The
//! fail-closed instinct was right and is kept; what is gone is one bad input
//! poisoning rules that never read it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use epics_base_rs::runtime::task::sleep;
use epics_base_rs::server::access_security::{
    AccessGate, AccessLevel, AcfCell, AsgAslResolver, new_acf_cell_watching, parse_acf,
};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::types::EpicsValue;

/// `gate:enable` resolves to 1; `typo:pv` does not exist. The WRITE rule reads
/// only A, the READ rule reads only B.
const ACF: &str = r#"
ASG(RW) {
    INPA("gate:enable")
    INPB("typo:pv")
    RULE(1, WRITE) { CALC("A=1") }
}
"#;

/// `acf` served over a database holding `gate:enable` at `value` and no
/// `typo:pv`, once the `INP*` watcher has delivered what exists.
async fn served(acf: &str, asg: &[u8], value: f64) -> (Arc<PvDatabase>, AcfCell) {
    let db = Arc::new(PvDatabase::new());
    db.add_record("gate:enable", Box::new(AiRecord::new(value)))
        .await
        .unwrap();
    // Processed, so it is not UDF.
    db.put_record_field_from_ca("gate:enable", "VAL", EpicsValue::Double(value))
        .await
        .unwrap();
    let cell = new_acf_cell_watching(Some(parse_acf(acf).unwrap()), &db);
    let deadline = Instant::now() + Duration::from_secs(10);
    while cell.load_full().unwrap().asg[asg].inp_bad() != 0b10 {
        assert!(Instant::now() < deadline, "gate:enable never delivered");
        sleep(Duration::from_millis(10)).await;
    }
    (db, cell)
}

/// The finding's own trigger, through the async gate.
#[epics_macros_rs::epics_test]
async fn a_bad_inp_no_rule_reads_does_not_disable_the_rule() {
    let (_db, cell) = served(ACF, b"RW", 1.0).await;
    let asg: AsgAslResolver = Arc::new(|_n| Box::pin(async { ("RW".to_string(), 0u8) }));
    let gate = AccessGate::required(cell, asg);

    assert!(
        gate.check("x", "h", "u", "ca", "").await.allows_write(),
        "the rule's CALC is \"A=1\" and A resolved to 1; INPB is bad but \
         inpUsed has only bit A, so inpBad & inpUsed == 0 and C grants WRITE"
    );
}

/// The other half of the boundary: a bad input the rule DOES read still
/// denies it. Fail-closed where it belongs.
#[epics_macros_rs::epics_test]
async fn a_bad_inp_the_rule_reads_still_denies_it() {
    let acf = r#"
ASG(RW) {
    INPA("gate:enable")
    INPB("typo:pv")
    RULE(1, WRITE) { CALC("B=1") }
}
"#;
    let (_db, cell) = served(acf, b"RW", 1.0).await;
    let asg: AsgAslResolver = Arc::new(|_n| Box::pin(async { ("RW".to_string(), 0u8) }));
    let gate = AccessGate::required(cell, asg);

    assert!(
        !gate.check("x", "h", "u", "ca", "").await.allows_write(),
        "the rule reads B and B is bad, so inpBad & inpUsed != 0 and C \
         disables it"
    );
}

/// Two rules, one bad input, opposite dependencies — the discrimination the
/// group-wide abort could not express. `A=1` grants, `B=1` does not, in the
/// same evaluation.
#[epics_macros_rs::epics_test]
async fn one_bad_input_splits_the_rules_that_read_it_from_the_rest() {
    let acf = r#"
ASG(SPLIT) {
    INPA("gate:enable")
    INPB("typo:pv")
    RULE(0, READ)  { CALC("A=1") }
    RULE(1, WRITE) { CALC("B=1") }
}
"#;
    // gate:enable = 1; typo:pv never delivers, so only INPB is bad.
    let (_db, cell) = served(acf, b"SPLIT", 1.0).await;
    let cfg = cell.load_full().unwrap();
    let (level, _) = cfg.compute_for_name("SPLIT", "h", "u", &[], 0, "ca", "");
    assert_eq!(
        level,
        AccessLevel::Read,
        "the READ rule reads only A and applies; the WRITE rule reads B, \
         which is bad, and does not"
    );
}

/// The per-rule bitmap is what makes the discrimination possible, and it is
/// stored at parse the way C stores `pasgrule->inpUsed`.
#[test]
fn each_rule_carries_the_arguments_its_calc_reads() {
    let cfg = parse_acf(
        r#"
ASG(G) {
    INPA("a") INPB("b") INPC("c")
    RULE(0, READ)  { CALC("A=1") }
    RULE(1, WRITE) { CALC("B+C>0") }
    RULE(1, READ)
}
"#,
    )
    .unwrap();
    let rules = &cfg.asg[b"G".as_slice()].rules;
    assert_eq!(rules[0].inp_used, 0b001, "\"A=1\" reads A");
    assert_eq!(rules[1].inp_used, 0b110, "\"B+C>0\" reads B and C");
    assert_eq!(rules[2].inp_used, 0, "a rule with no CALC reads nothing");
}

/// A bad input cannot rescue a rule either: the intersection gates BEFORE the
/// band test, so a rule whose CALC would be false is still just false.
#[epics_macros_rs::epics_test]
async fn a_good_input_that_evaluates_false_still_denies() {
    // gate:enable = 0; typo:pv never delivers.
    let (_db, cell) = served(ACF, b"RW", 0.0).await;
    let cfg = cell.load_full().unwrap();
    let (level, _) = cfg.compute_for_name("RW", "h", "u", &[], 0, "ca", "");
    assert_eq!(level, AccessLevel::NoAccess, "A=1 is false when A is 0");
}
