//! A2 — a `RULE(...) { CALC(...) }` applies only when the result lands in
//! C's `(0.99, 1.01)` band, not merely when it is non-zero.
//!
//! `asLibRoutines.c:963`:
//!
//! ```c
//! pasgrule->result = ((result>.99) && (result<1.01)) ? 1 : 0;
//! ```
//!
//! consumed at `:1039` as `pasgrule->result==1`. The open interval is a
//! tolerance for float error around 1; it is deliberately not `== 1.0` and
//! deliberately not "truthy". A CALC returning 2, -1, 0.5 or 3 does NOT apply
//! its rule, so `caput` is refused `ECA_NOWTACCESS`.
//!
//! The port tested `result != 0.0` — in TWO places, `access_security.rs` and
//! `epics-ca-rs/src/server/tcp.rs`, each caller having written its own
//! evaluator. Both granted WRITE on every truthy non-unity result, which is a
//! privilege escalation on both the CA and PVA paths. The evaluator now lives
//! only in `AccessSecurityConfig::compute_rules`, the way C keeps it only in
//! `asComputeAsg`, and both callers read the results it publishes.

use std::sync::Arc;
use std::time::{Duration, Instant};

use epics_base_rs::runtime::task::sleep;
use epics_base_rs::server::access_security::{
    AccessGate, AccessLevel, AcfCell, AsgAslResolver, new_acf_cell_watching, parse_acf,
};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::types::EpicsValue;

/// `CALC("A")` makes the rule's result the INP value itself, so each case
/// names the exact number C's band sees.
const ACF: &str = r#"ASG(OPS) { INPA("gate:mode") RULE(1, WRITE) { CALC("A") } }"#;

/// C's band, stated once: grant on the open interval, deny everywhere else.
/// `0.99` and `1.01` are the excluded endpoints — `>` and `<`, not `>=`/`<=`.
const CASES: &[(f64, bool)] = &[
    (0.0, false),
    (0.5, false),
    (0.98, false),
    (0.99, false),
    (0.991, true),
    (1.0, true),
    (1.009, true),
    (1.01, false),
    (1.02, false),
    (2.0, false),
    (3.0, false),
    (-1.0, false),
];

/// `gate:mode` at `value`, once the `INP*` watcher has delivered it.
async fn mode_at(value: f64) -> (Arc<PvDatabase>, AcfCell) {
    let db = Arc::new(PvDatabase::new());
    db.add_record("gate:mode", Box::new(AiRecord::new(value)))
        .await
        .unwrap();
    // Processed, so it is not UDF.
    db.put_record_field_from_ca("gate:mode", "VAL", EpicsValue::Double(value))
        .await
        .unwrap();
    let cell = new_acf_cell_watching(Some(parse_acf(ACF).unwrap()), &db);
    let deadline = Instant::now() + Duration::from_secs(10);
    while cell.load_full().unwrap().asg[b"OPS".as_slice()].inp_bad() != 0 {
        assert!(Instant::now() < deadline, "gate:mode never delivered");
        sleep(Duration::from_millis(10)).await;
    }
    (db, cell)
}

/// The async gate — the path `epics-pva-rs` and QSRV take.
#[epics_macros_rs::epics_test]
async fn calc_gated_rule_grants_only_inside_the_band() {
    let asg: AsgAslResolver = Arc::new(|_name| Box::pin(async { ("OPS".to_string(), 0u8) }));
    for &(value, expected) in CASES {
        let (_db, cell) = mode_at(value).await;
        let gate = AccessGate::required(cell, asg.clone());
        assert_eq!(
            gate.check("x", "h", "u", "ca", "").await.allows_write(),
            expected,
            "gate:mode = {value}: C computes result = {value}, \
             ((result>.99)&&(result<1.01)) = {expected}"
        );
    }
}

/// The synchronous entry the CA server calls — the second copy of the
/// evaluator used to live behind this one.
#[epics_macros_rs::epics_test]
async fn compute_for_name_applies_the_band() {
    for &(value, expected) in CASES {
        let (_db, cell) = mode_at(value).await;
        let cfg = cell.load_full().unwrap();
        let (level, _trap) = cfg.compute_for_name("OPS", "h", "u", &[], 0, "ca", "");
        assert_eq!(
            level == AccessLevel::ReadWrite,
            expected,
            "gate:mode = {value} through the CA server's entry point"
        );
    }
}

/// The escalation as the finding states it: `gate:mode = 2` is truthy, and C
/// refuses the write.
#[epics_macros_rs::epics_test]
async fn a_truthy_non_unity_calc_does_not_grant_write() {
    let (_db, cell) = mode_at(2.0).await;
    let asg: AsgAslResolver = Arc::new(|_name| Box::pin(async { ("OPS".to_string(), 0u8) }));
    let gate = AccessGate::required(cell, asg);
    let checked = gate.check("x", "h", "u", "ca", "").await;
    assert!(
        !checked.allows_write(),
        "result = 2 is outside (0.99, 1.01), so the rule does not apply and \
         C refuses the caput with ECA_NOWTACCESS"
    );
    assert!(
        !checked.allows_read(),
        "the WRITE rule is the only rule in the ASG; it not applying leaves \
         asNOACCESS"
    );
}
