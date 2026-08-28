//! **`DBF_FLOAT` text runs C's `epicsParseFloat` gate on every path.**
//!
//! `epicsParseFloat` (`libcom/src/misc/epicsStdlib.c:318-335`) parses a double
//! and then refuses what the `float` cast would destroy:
//!
//! ```c
//! abs = fabs(value);
//! if (value > 0 && abs <= FLT_MIN)     return S_stdlib_underflow;
//! if (finite(value) && abs >= FLT_MAX) return S_stdlib_overflow;
//! *to = (float) value;
//! ```
//!
//! Every C route into a `DBF_FLOAT` field runs it: `putStringFloat`
//! (`dbConvert.c:1119`) on a put, `getStringFloat` (`:379`) on a get, and
//! `dbStaticLib.c:2797` on the `.db` load. The port ran it on the record-put
//! route (`c_parse`) but not on `EpicsValue::parse`, so `1e300` was refused as
//! a `caput` and stored as `inf` from a `.db` file — the same text, two
//! answers.
//!
//! Cases are per boundary of that C body, not per scenario: the overflow edge
//! is `>=` and the underflow edge is `<=` and one-sided (`value > 0`), and a
//! non-finite LITERAL is exempt from both.

use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::swait::SwaitRecord;
use epics_base_rs::types::c_parse::{Converted, NumericField, put_string};
use epics_base_rs::types::{DbFieldType, EpicsValue};
use std::collections::HashMap;

/// `EpicsValue::parse`'s verdict on `s` as a `DBF_FLOAT`.
fn parsed(s: &str) -> Option<f32> {
    match EpicsValue::parse(DbFieldType::Float, s) {
        Ok(EpicsValue::Float(v)) => Some(v),
        Ok(other) => panic!("Float parse produced {other:?}"),
        Err(_) => None,
    }
}

/// The record-put route's verdict on the same text, through `c_parse`.
fn put(s: &str) -> Option<f32> {
    match put_string("F", NumericField::Float, s) {
        Ok(Converted::Stored(EpicsValue::Float(v))) => Some(v),
        Ok(other) => panic!("Float put produced {other:?}"),
        Err(_) => None,
    }
}

#[test]
fn a_magnitude_at_or_above_flt_max_is_refused() {
    // `1e300` — the reviewer's trigger. `as f32` yields `inf`; C refuses.
    assert_eq!(parsed("1e300"), None);
    // The `>=` edge itself: exactly FLT_MAX is refused, not stored.
    let flt_max = (f32::MAX as f64).to_string();
    assert_eq!(parsed(&flt_max), None, "FLT_MAX is refused by C's `>=`");
    // Just inside the edge still converts.
    assert_eq!(parsed("3.4e38"), Some(3.4e38f32));
}

#[test]
fn a_positive_magnitude_at_or_below_flt_min_is_refused() {
    assert_eq!(parsed("1e-40"), None);
    let flt_min = (f32::MIN_POSITIVE as f64).to_string();
    assert_eq!(parsed(&flt_min), None, "FLT_MIN is refused by C's `<=`");
    assert_eq!(parsed("1.2e-38"), Some(1.2e-38f32));
    // Zero is not "> 0", so it is not an underflow.
    assert_eq!(parsed("0"), Some(0.0f32));
}

#[test]
fn the_underflow_test_stays_one_sided() {
    // C tests `value > 0` only, so the negative twin of a refused underflow is
    // accepted and stored as the subnormal `float` it casts to. Pinning the
    // asymmetry keeps a later "tidy-up" from making the rule symmetric and
    // diverging from C.
    assert_eq!(parsed("-1e-40"), Some(-1e-40f32));
    assert!(parsed("-1e-40").is_some_and(|v| v != 0.0 && v.is_subnormal()));
}

#[test]
fn a_non_finite_literal_is_exempt_from_both_edges() {
    // `finite(value)` guards the overflow test, so an explicit infinity is
    // stored rather than refused.
    assert_eq!(parsed("inf"), Some(f32::INFINITY));
    assert!(parsed("nan").is_some_and(f32::is_nan));
}

#[test]
fn both_string_to_float_routes_give_the_same_verdict() {
    // The invariant the fix closes: `EpicsValue::parse` (the `.db` load and
    // `dbpf`) and `c_parse::put_string` (the record put) run ONE gate, so no
    // text is accepted by one route and refused by the other.
    for s in [
        "1e300", "3.4e38", "1e-40", "1.2e-38", "0", "-1e-40", "inf", "12.5",
    ] {
        assert_eq!(
            parsed(s).map(f32::to_bits),
            put(s).map(f32::to_bits),
            "the two string->float routes disagree on {s:?}"
        );
    }
}

#[epics_macros_rs::epics_test]
async fn a_db_file_cannot_load_an_out_of_range_float_field() {
    // `swait.ODLY` is `DBF_FLOAT` (`swaitRecord.dbd:454`), so the `.db` load
    // reaches `EpicsValue::parse_bytes` -> `parse` for it.
    // The refusal may surface at either stage, so accept both.
    let refused = match IocBuilder::new().db_string(
        r#"record(swait,"W:OVER"){ field(ODLY,"1e300") }"#,
        &HashMap::new(),
    ) {
        Err(_) => true,
        Ok(b) => b.build().await.is_err(),
    };
    assert!(refused, "a .db field C refuses must not load as `inf`");

    let (db, _) = IocBuilder::new()
        .register_record_type("swait", || Box::new(SwaitRecord::default()))
        .db_string(
            r#"record(swait,"W:OK"){ field(ODLY,"1.5") }"#,
            &HashMap::new(),
        )
        .expect("parse db")
        .build()
        .await
        .expect("an in-range ODLY still loads");
    assert_eq!(
        db.get_pv("W:OK.ODLY").expect("ODLY"),
        EpicsValue::Float(1.5)
    );
}
