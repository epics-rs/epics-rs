//! `double -> float` narrowing is C `epicsConvertDoubleToFloat`
//! (`epicsConvert.c:19-34`), not a plain cast.
//!
//! ```c
//! if (value == 0 || !finite(value))  return (float) value;
//! abs = fabs(value);
//! if (abs >= FLT_MAX)  return (value > 0) ?  FLT_MAX : -FLT_MAX;
//! if (abs <= FLT_MIN)  return (value > 0) ?  FLT_MIN : -FLT_MIN;
//! return (float) value;
//! ```
//!
//! Four arms, and the two clamps are what a plain `as f32` gets wrong: a
//! `DBR_FLOAT` read of `1e300` must be `3.40282e+38` on the wire, not `inf`,
//! and `1e-300` must be `1.17549e-38`, not `0`. `FLT_MIN` is the smallest
//! NORMAL float, so an f32-representable denormal such as `1e-40` clamps up to
//! it as well — the lower bound is not "everything that rounds to zero".
//!
//! The last three cases pin the routing rather than the arithmetic: the helper
//! is only a fix if every field and wire conversion actually reaches it, so
//! `EpicsValue::convert_to` (scalar and array), the `DBR_GR_FLOAT` limit fill,
//! and a `DBF_FLOAT` record field put are each exercised through their public
//! entry point.

use std::time::SystemTime;

use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::swait::SwaitRecord;
use epics_base_rs::server::snapshot::{DisplayInfo, PropertySupport, Snapshot};
use epics_base_rs::types::c_cast::f64_to_f32;
use epics_base_rs::types::{DbFieldType, EpicsValue, encode_dbr};

const DBR_GR_FLOAT: u16 = 23;

#[test]
fn overflow_clamps_to_flt_max_with_sign() {
    assert_eq!(f64_to_f32(1e300), f32::MAX);
    assert_eq!(f64_to_f32(-1e300), -f32::MAX);
    assert_eq!(f64_to_f32(f64::MAX), f32::MAX);
    assert_eq!(f64_to_f32(f64::MIN), -f32::MAX);
    // `>=` is inclusive, and clamping at the bound is also what keeps a double
    // just under `FLT_MAX` from rounding up to `inf`.
    assert_eq!(f64_to_f32(f32::MAX as f64), f32::MAX);
    assert_eq!(f64_to_f32(-(f32::MAX as f64)), -f32::MAX);
}

#[test]
fn underflow_clamps_to_flt_min_with_sign() {
    assert_eq!(f64_to_f32(1e-300), f32::MIN_POSITIVE);
    assert_eq!(f64_to_f32(-1e-300), -f32::MIN_POSITIVE);
    // Representable as an f32 denormal, so a plain cast keeps it; C does not.
    assert_eq!(f64_to_f32(1e-40), f32::MIN_POSITIVE);
    assert_eq!(f64_to_f32(-1e-40), -f32::MIN_POSITIVE);
    // `<=` is inclusive.
    assert_eq!(f64_to_f32(f32::MIN_POSITIVE as f64), f32::MIN_POSITIVE);
    assert_eq!(f64_to_f32(-(f32::MIN_POSITIVE as f64)), -f32::MIN_POSITIVE);
}

#[test]
fn zero_passes_through_keeping_its_sign() {
    // The `value == 0` arm runs BEFORE the `abs <= FLT_MIN` one, so a zero is
    // never promoted to `FLT_MIN`; `-0.0` also never reaches the `value > 0`
    // test that would have sent it to `-FLT_MIN`.
    assert_eq!(f64_to_f32(0.0), 0.0f32);
    assert!(f64_to_f32(0.0).is_sign_positive());
    assert_eq!(f64_to_f32(-0.0), 0.0f32);
    assert!(f64_to_f32(-0.0).is_sign_negative());
}

#[test]
fn non_finite_passes_through() {
    assert!(f64_to_f32(f64::NAN).is_nan());
    assert_eq!(f64_to_f32(f64::INFINITY), f32::INFINITY);
    assert_eq!(f64_to_f32(f64::NEG_INFINITY), f32::NEG_INFINITY);
}

#[test]
fn in_band_values_are_the_plain_cast() {
    assert_eq!(f64_to_f32(1.5), 1.5f32);
    assert_eq!(f64_to_f32(-1.5), -1.5f32);
    assert_eq!(f64_to_f32(0.1), 0.1f64 as f32);
}

#[test]
fn dbr_float_scalar_convert_to_clamps() {
    let cases: &[(f64, f32)] = &[
        (1e300, f32::MAX),
        (-1e300, -f32::MAX),
        (1e-300, f32::MIN_POSITIVE),
        (-1e-300, -f32::MIN_POSITIVE),
        (0.0, 0.0),
        (2.5, 2.5),
    ];
    for &(src, want) in cases {
        let got = EpicsValue::Double(src).convert_to(DbFieldType::Float);
        assert_eq!(
            got,
            EpicsValue::Float(want),
            "DBF_DOUBLE {src} -> DBF_FLOAT"
        );
    }
    let nan = EpicsValue::Double(f64::NAN).convert_to(DbFieldType::Float);
    match nan {
        EpicsValue::Float(v) => assert!(v.is_nan(), "NaN must survive the narrowing"),
        other => panic!("expected Float, got {other:?}"),
    }
}

#[test]
fn dbr_float_array_convert_to_clamps() {
    let src = EpicsValue::DoubleArray(vec![1e300, -1e300, 1e-300, -1e-300, 0.0, 2.5]);
    assert_eq!(
        src.convert_to(DbFieldType::Float),
        EpicsValue::FloatArray(vec![
            f32::MAX,
            -f32::MAX,
            f32::MIN_POSITIVE,
            -f32::MIN_POSITIVE,
            0.0,
            2.5,
        ])
    );
}

#[test]
fn gr_float_limits_clamp_on_the_wire() {
    // `db_access.c:480-485` fills every `DBR_GR_FLOAT` limit through the
    // helper, so an `ao` with `DRVH=1e300` serves `3.40282e+38`, not `inf`.
    let mut snap = Snapshot::new(EpicsValue::Float(1.5), 0, 0, SystemTime::UNIX_EPOCH);
    snap.display = Some(DisplayInfo {
        units: "V".into(),
        precision: 2,
        upper_disp_limit: 1e300,
        lower_disp_limit: -1e300,
        upper_alarm_limit: 1e-300,
        lower_alarm_limit: -1e-300,
        ..Default::default()
    });
    // The encoder reads the rset-slot MASK, not `display.is_some()`: a
    // `DisplayInfo` is minted for every snapshot to carry the DESC leaf, so
    // its `Option` says nothing about which `get_*` slots the record type
    // has. `ao` is the `NUMERIC` shape, which is what makes this a
    // `DRVH=1e300` reply rather than a memset zero.
    snap.properties = PropertySupport::NUMERIC.narrowed_to_field(snap.value.db_field_type(), false);
    let data = encode_dbr(DBR_GR_FLOAT, &snap).unwrap();
    let word = |i: usize| f32::from_be_bytes(data[i..i + 4].try_into().unwrap());
    assert_eq!(word(16), f32::MAX, "upper_disp_limit");
    assert_eq!(word(20), -f32::MAX, "lower_disp_limit");
    assert_eq!(word(24), f32::MIN_POSITIVE, "upper_alarm_limit");
    assert_eq!(word(36), -f32::MIN_POSITIVE, "lower_alarm_limit");
}

#[test]
fn swait_odly_double_put_clamps() {
    // ODLY is `DBF_FLOAT` in `swaitRecord.dbd`, so a double put reaches it
    // through C's `putDoubleFloat` and therefore through the helper.
    let mut w = SwaitRecord::default();
    w.put_field("ODLY", EpicsValue::Double(1e300)).unwrap();
    assert_eq!(w.get_field("ODLY"), Some(EpicsValue::Float(f32::MAX)));
    w.put_field("ODLY", EpicsValue::Double(-1e-300)).unwrap();
    assert_eq!(
        w.get_field("ODLY"),
        Some(EpicsValue::Float(-f32::MIN_POSITIVE))
    );
}
