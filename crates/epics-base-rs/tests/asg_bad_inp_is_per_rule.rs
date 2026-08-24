//! A3 — an unresolvable ASG `INP*` link disables the rules that READ it, not
//! every CALC rule in the group.
//!
//! C `asComputePvt` (`asLibRoutines.c:1048-1049`):
//!
//! ```c
//! if(!pasgrule->calc
//! || (!(pasg->inpBad & pasgrule->inpUsed) && (pasgrule->result==1)))
//! ```
//!
//! `inpBad` is per-INPUT — `asCa.c connectCallback:91-105` sets a bit for a
//! channel that is not connected. `inpUsed` is per-RULE — `calcArgUsage`
//! computes it once at load (`asLibRoutines.c:1416`). The intersection is what
//! disables a rule, so a typo in an `INPB` that no rule mentions costs
//! nothing.
//!
//! Both of this port's resolvers aborted their link walk on the first
//! unresolvable link and handed the evaluator `None`, which failed EVERY
//! CALC-gated rule in the group — on the CA and PVA paths alike. The
//! fail-closed instinct was right and is kept; what is gone is one bad input
//! poisoning rules that never read it.

use std::sync::Arc;

use epics_base_rs::server::access_security::{
    AccessGate, AccessLevel, AsgAslResolver, AsgInputs, InpResolver, new_acf_cell, parse_acf,
};

/// `gate:enable` resolves to 1; `typo:pv` does not exist. The WRITE rule reads
/// only A, the READ rule reads only B.
const ACF: &str = r#"
ASG(RW) {
    INPA("gate:enable")
    INPB("typo:pv")
    RULE(1, WRITE) { CALC("A=1") }
}
"#;

/// Resolve `gate:enable` and nothing else — `typo:pv` is the bad input.
fn one_bad_link() -> InpResolver {
    Arc::new(|link: String| Box::pin(async move { (link == "gate:enable").then_some(1.0) }))
}

/// The finding's own trigger, through the async gate.
#[epics_macros_rs::epics_test]
async fn a_bad_inp_no_rule_reads_does_not_disable_the_rule() {
    let cell = new_acf_cell(Some(parse_acf(ACF).unwrap()));
    let asg: AsgAslResolver = Arc::new(|_n| Box::pin(async { ("RW".to_string(), 0u8) }));
    let gate = AccessGate::required(cell, asg).with_inp_resolver(one_bad_link());

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
    let cell = new_acf_cell(Some(parse_acf(acf).unwrap()));
    let asg: AsgAslResolver = Arc::new(|_n| Box::pin(async { ("RW".to_string(), 0u8) }));
    let gate = AccessGate::required(cell, asg).with_inp_resolver(one_bad_link());

    assert!(
        !gate.check("x", "h", "u", "ca", "").await.allows_write(),
        "the rule reads B and B is bad, so inpBad & inpUsed != 0 and C \
         disables it"
    );
}

/// Two rules, one bad input, opposite dependencies — the discrimination the
/// group-wide abort could not express. `A=1` grants, `B=1` does not, in the
/// same evaluation.
#[test]
fn one_bad_input_splits_the_rules_that_read_it_from_the_rest() {
    let cfg = parse_acf(
        r#"
ASG(SPLIT) {
    INPA("gate:enable")
    INPB("typo:pv")
    RULE(0, READ)  { CALC("A=1") }
    RULE(1, WRITE) { CALC("B=1") }
}
"#,
    )
    .unwrap();

    let mut inputs = AsgInputs::default();
    inputs.record(0, Some(1.0)); // gate:enable = 1
    inputs.record(1, None); // typo:pv is bad
    assert_eq!(inputs.bad, 0b10, "only INPB is marked bad");

    let (level, _) = cfg.compute_for_name("SPLIT", "h", "u", &[], 0, "ca", "", Some(&inputs));
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
    let rules = &cfg.asg["G"].rules;
    assert_eq!(rules[0].inp_used, 0b001, "\"A=1\" reads A");
    assert_eq!(rules[1].inp_used, 0b110, "\"B+C>0\" reads B and C");
    assert_eq!(rules[2].inp_used, 0, "a rule with no CALC reads nothing");
}

/// A bad input cannot rescue a rule either: the intersection gates BEFORE the
/// band test, so a rule whose CALC would be false is still just false.
#[test]
fn a_good_input_that_evaluates_false_still_denies() {
    let cfg = parse_acf(ACF).unwrap();
    let mut inputs = AsgInputs::default();
    inputs.record(0, Some(0.0)); // gate:enable = 0
    inputs.record(1, None);
    let (level, _) = cfg.compute_for_name("RW", "h", "u", &[], 0, "ca", "", Some(&inputs));
    assert_eq!(level, AccessLevel::NoAccess, "A=1 is false when A is 0");
}
