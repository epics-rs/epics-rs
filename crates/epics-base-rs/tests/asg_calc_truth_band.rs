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
//! `asComputeAsg`, and both callers pass resolved values into it.

use std::sync::Arc;

use epics_base_rs::server::access_security::{
    AccessGate, AccessLevel, AsgAslResolver, AsgInputs, InpResolver, new_acf_cell, parse_acf,
};

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

/// The async gate — the path `epics-pva-rs` and QSRV take.
#[epics_macros_rs::epics_test]
async fn calc_gated_rule_grants_only_inside_the_band() {
    let cell = new_acf_cell(Some(parse_acf(ACF).unwrap()));
    let asg: AsgAslResolver = Arc::new(|_name| Box::pin(async { ("OPS".to_string(), 0u8) }));

    for &(value, expected) in CASES {
        let inp: InpResolver = Arc::new(move |_link: String| Box::pin(async move { Some(value) }));
        let gate = AccessGate::required(cell.clone(), asg.clone()).with_inp_resolver(inp);
        assert_eq!(
            gate.check("x", "h", "u", "ca", "").await.allows_write(),
            expected,
            "gate:mode = {value}: C computes result = {value}, \
             ((result>.99)&&(result<1.01)) = {expected}"
        );
    }
}

/// The synchronous entry the CA server calls with its own resolved values —
/// the second copy of the evaluator used to live behind this one.
#[test]
fn compute_for_name_applies_the_band_to_caller_supplied_inputs() {
    let cfg = parse_acf(ACF).unwrap();
    for &(value, expected) in CASES {
        let mut inputs = AsgInputs::default();
        inputs.record(0, Some(value));
        let (level, _trap) = cfg.compute_for_name("OPS", "h", "u", &[], 0, "ca", "", Some(&inputs));
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
    let cell = new_acf_cell(Some(parse_acf(ACF).unwrap()));
    let asg: AsgAslResolver = Arc::new(|_name| Box::pin(async { ("OPS".to_string(), 0u8) }));
    let inp: InpResolver = Arc::new(|_link: String| Box::pin(async { Some(2.0) }));
    let gate = AccessGate::required(cell, asg).with_inp_resolver(inp);
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
