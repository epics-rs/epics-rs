//! A multi-input link that asks `dbGetLink` for a DBR type (printfRecord.c
//! `:185-230`) is delivered the value converted at that type, not an `f64`,
//! and a conversion C refuses is a failed read (`F_BADLNK`, LINK/INVALID on
//! the reader) with the old slot left alone.
//!
//! One case per boundary: a narrowing int->int (`dbConvert` truncates), an
//! int64 above 2^53, a non-numeric string for a numeric request, and a
//! non-finite double (the `c_cast.rs` saturating policy).

use std::sync::{Arc, Mutex};

use epics_base_rs::error::CaResult;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::{
    AlarmSeverity, FieldDesc, InputFetchPolicy, InputLinkRequest, LinkReadAs, ProcessOutcome,
    Record, register_declared_fields,
};
use epics_base_rs::types::{DbFieldType, DbfCode, EpicsValue};

struct Source(EpicsValue, &'static str);

impl Record for Source {
    fn record_type(&self) -> &'static str {
        self.1
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::complete())
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        (name == "VAL").then(|| self.0.clone())
    }
    fn put_field(&mut self, _: &str, _: EpicsValue) -> CaResult<()> {
        Ok(())
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        match self.1 {
            "src_i64" => SRC_I64,
            "src_u64" => SRC_U64,
            "src_str" => SRC_STR,
            _ => SRC_F64,
        }
    }
}

static SRC_I64: &[FieldDesc] = &[FieldDesc::new("VAL", DbFieldType::Int64, false)];
static SRC_U64: &[FieldDesc] = &[FieldDesc::new("VAL", DbFieldType::UInt64, false)];
static SRC_STR: &[FieldDesc] = &[FieldDesc::new("VAL", DbFieldType::String, false)];
static SRC_F64: &[FieldDesc] = &[FieldDesc::new("VAL", DbFieldType::Double, false)];

/// Reads `LNK` into `VAL` through the multi-input fetch at `request` and
/// keeps the last value stored into `VAL`.
struct Reader {
    lnk: &'static str,
    request: LinkReadAs,
    landed: Arc<Mutex<Option<EpicsValue>>>,
    /// The fetch-gate verdict the framework handed the record this cycle.
    gated: Arc<Mutex<bool>>,
}

impl Record for Reader {
    fn record_type(&self) -> &'static str {
        "typed_reader"
    }
    fn process(&mut self) -> CaResult<ProcessOutcome> {
        Ok(ProcessOutcome::complete())
    }
    fn multi_input_links(&self) -> &'static [(&'static str, &'static str)] {
        &[("LNK", "VAL")]
    }
    fn input_fetch_policy(&self) -> InputFetchPolicy {
        InputFetchPolicy::ReadAllGateOnFailure
    }
    fn set_fetch_gate_failed(&mut self, failed: bool) {
        *self.gated.lock().unwrap() = failed;
    }
    fn input_link_request(&self, _: &str) -> InputLinkRequest {
        InputLinkRequest::As(self.request)
    }
    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "LNK" => Some(EpicsValue::String(self.lnk.into())),
            "VAL" => Some(EpicsValue::Double(0.0)),
            _ => None,
        }
    }
    fn put_field(&mut self, _: &str, _: EpicsValue) -> CaResult<()> {
        Ok(())
    }
    fn put_field_internal(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        if name == "VAL" {
            *self.landed.lock().unwrap() = Some(value);
        }
        Ok(())
    }
    fn declared_fields(&self) -> &'static [FieldDesc] {
        READER_FIELDS
    }
}

static READER_FIELDS: &[FieldDesc] = &[
    FieldDesc {
        declared_dbf: DbfCode::Inlink,
        ..FieldDesc::new("LNK", DbFieldType::String, false)
    },
    FieldDesc::new("VAL", DbFieldType::Double, false),
];

/// What the reader's `VAL` received (None: nothing) and its severity.
async fn read(
    src: EpicsValue,
    src_type: &'static str,
    request: LinkReadAs,
) -> (Option<EpicsValue>, AlarmSeverity) {
    read_via("SRC.VAL", src, src_type, request).await
}

/// [`read`] with the reader's `LNK` text given: a `PP` source is not a read
/// the reader's own hold can make, so it takes the general fetch path.
async fn read_via(
    lnk: &'static str,
    src: EpicsValue,
    src_type: &'static str,
    request: LinkReadAs,
) -> (Option<EpicsValue>, AlarmSeverity) {
    let db = PvDatabase::new();
    register_declared_fields("typed_reader", READER_FIELDS);
    for (t, f) in [
        ("src_i64", SRC_I64),
        ("src_str", SRC_STR),
        ("src_f64", SRC_F64),
    ] {
        register_declared_fields(t, f);
    }
    db.add_record("SRC", Box::new(Source(src, src_type)))
        .await
        .unwrap();
    let landed = Arc::new(Mutex::new(None));
    db.add_record(
        "RD",
        Box::new(Reader {
            lnk,
            request,
            landed: landed.clone(),
            gated: Arc::new(Mutex::new(false)),
        }),
    )
    .await
    .unwrap();
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    let _ = db.process_record_with_links("RD", &mut visited).await;
    let sevr = db.get_record("RD").unwrap().read().common.sevr;
    let v = landed.lock().unwrap().take();
    (v, sevr)
}

#[epics_macros_rs::epics_test]
async fn an_int64_requested_as_long_truncates_like_dbconvert() {
    let (v, sevr) = read(
        EpicsValue::Int64(5_000_000_000),
        "src_i64",
        LinkReadAs::Long,
    )
    .await;
    assert_eq!(v, Some(EpicsValue::Long(705_032_704)));
    assert_eq!(sevr, AlarmSeverity::NoAlarm);
}

#[epics_macros_rs::epics_test]
async fn an_int64_requested_as_ushort_truncates_like_dbconvert() {
    let (v, _) = read(
        EpicsValue::Int64(5_000_000_000),
        "src_i64",
        LinkReadAs::UShort,
    )
    .await;
    assert_eq!(v, Some(EpicsValue::UShort(61_952)));
}

#[epics_macros_rs::epics_test]
async fn an_int64_above_2_pow_53_requested_as_int64_is_exact() {
    let big = (1i64 << 53) + 1;
    let (v, _) = read(EpicsValue::Int64(big), "src_i64", LinkReadAs::Int64).await;
    assert_eq!(v, Some(EpicsValue::Int64(big)));
}

#[epics_macros_rs::epics_test]
async fn a_string_requested_as_long_is_a_failed_read() {
    let (v, sevr) = read(
        EpicsValue::String("hello".into()),
        "src_str",
        LinkReadAs::Long,
    )
    .await;
    assert_eq!(v, None, "the slot keeps its old value");
    assert_eq!(sevr, AlarmSeverity::Invalid, "LINK alarm on the reader");
}

#[epics_macros_rs::epics_test]
async fn an_infinite_double_requested_as_long_saturates() {
    let (v, _) = read(
        EpicsValue::Double(f64::INFINITY),
        "src_f64",
        LinkReadAs::Long,
    )
    .await;
    assert_eq!(v, Some(EpicsValue::Long(i32::MAX)));
}

/// The numeric funnel's own request fails the same way: a non-numeric
/// string read at `DBR_DOUBLE` is a failed read, not a silent skip.
#[epics_macros_rs::epics_test]
async fn a_string_requested_as_double_is_a_failed_read() {
    let (v, sevr) = read(
        EpicsValue::String("hello".into()),
        "src_str",
        LinkReadAs::Double,
    )
    .await;
    assert_eq!(v, None);
    assert_eq!(sevr, AlarmSeverity::Invalid);
}

/// A native request funnels the source through `DBR_DOUBLE`: the same
/// refusal, from the funnel instead of the typed conversion.
#[epics_macros_rs::epics_test]
async fn a_string_read_natively_is_a_failed_read() {
    let (v, sevr) = read(
        EpicsValue::String("hello".into()),
        "src_str",
        LinkReadAs::Native,
    )
    .await;
    assert_eq!(v, None);
    assert_eq!(sevr, AlarmSeverity::Invalid);
}

#[epics_macros_rs::epics_test]
async fn an_int64_requested_as_short_char_uchar_truncates_like_dbconvert() {
    let five_billion = || EpicsValue::Int64(5_000_000_000);
    let (v, _) = read(five_billion(), "src_i64", LinkReadAs::Short).await;
    assert_eq!(v, Some(EpicsValue::Short(-3584)));
    let (v, _) = read(five_billion(), "src_i64", LinkReadAs::Char).await;
    assert_eq!(v, Some(EpicsValue::Char(0)));
    let (v, _) = read(five_billion(), "src_i64", LinkReadAs::UChar).await;
    assert_eq!(v, Some(EpicsValue::UChar(0)));
}

#[epics_macros_rs::epics_test]
async fn a_double_requested_as_float_stays_single_precision() {
    let (v, _) = read(EpicsValue::Double(3.25), "src_f64", LinkReadAs::Float).await;
    assert_eq!(v, Some(EpicsValue::Float(3.25)));
}

#[epics_macros_rs::epics_test]
async fn a_u64_max_requested_as_uint64_is_exact() {
    let (v, _) = read(EpicsValue::UInt64(u64::MAX), "src_u64", LinkReadAs::UInt64).await;
    assert_eq!(v, Some(EpicsValue::UInt64(u64::MAX)));
}

/// x86-64 softIoc R7.0.10: inf, -inf, NaN and 1e10 land in a `DBF_SHORT` as
/// 0 — `cvttsd2si`'s `INT_MIN`, cut to 16 bits — and 65537 as 1.
#[epics_macros_rs::epics_test]
async fn an_out_of_range_double_requested_as_short_is_zero_like_softioc() {
    for (src, want) in [
        (f64::INFINITY, 0),
        (f64::NEG_INFINITY, 0),
        (f64::NAN, 0),
        (1e10, 0),
        (65537.0, 1),
        (-2.7, -2),
    ] {
        let (v, _) = read(EpicsValue::Double(src), "src_f64", LinkReadAs::Short).await;
        assert_eq!(v, Some(EpicsValue::Short(want)), "{src}");
    }
}

/// The general fetch path (a `PP` source is not a read the reader's own
/// hold can make) refuses the same conversion the held path does.
#[epics_macros_rs::epics_test]
async fn a_string_read_natively_through_the_general_path_is_a_failed_read() {
    let (v, sevr) = read_via(
        "SRC.VAL PP",
        EpicsValue::String("hello".into()),
        "src_str",
        LinkReadAs::Native,
    )
    .await;
    assert_eq!(v, None);
    assert_eq!(sevr, AlarmSeverity::Invalid);
}

/// A simple PV spelled `REC.FLD` shadows that field; a string there read
/// natively is the same refused conversion, raised from the shadow arm.
#[epics_macros_rs::epics_test]
async fn a_string_simple_pv_shadowing_the_field_is_a_failed_read() {
    let db = PvDatabase::new();
    register_declared_fields("typed_reader", READER_FIELDS);
    db.add_record(
        "SRC",
        Box::new(epics_base_rs::server::records::ai::AiRecord::new(1.0)),
    )
    .await
    .unwrap();
    db.add_pv("SRC.DESC", EpicsValue::String("hello".into()))
        .await
        .unwrap();
    let landed = Arc::new(Mutex::new(None));
    let gated = Arc::new(Mutex::new(false));
    db.add_record(
        "RD",
        Box::new(Reader {
            lnk: "SRC.DESC",
            request: LinkReadAs::Native,
            landed: landed.clone(),
            gated: gated.clone(),
        }),
    )
    .await
    .unwrap();
    let mut visited = epics_base_rs::server::database::ProcStack::new();
    let _ = db.process_record_with_links("RD", &mut visited).await;
    assert_eq!(*landed.lock().unwrap(), None);
    let sevr = db.get_record("RD").unwrap().read().common.sevr;
    assert_eq!(sevr, AlarmSeverity::Invalid, "LINK alarm on the reader");
    assert!(*gated.lock().unwrap(), "the fetch reports the failed read");
}

/// A `DBR_DOUBLE` request lands a DOUBLE source as read (sign of zero and
/// all) and still converts any other class to a double.
#[epics_macros_rs::epics_test]
async fn a_double_requested_as_double_lands_as_read() {
    let (v, sevr) = read(EpicsValue::Double(-0.0), "src_f64", LinkReadAs::Double).await;
    assert!(matches!(v, Some(EpicsValue::Double(d)) if d == 0.0 && d.is_sign_negative()));
    assert_eq!(sevr, AlarmSeverity::NoAlarm);
    let (v, _) = read(EpicsValue::Int64(7), "src_i64", LinkReadAs::Double).await;
    assert_eq!(v, Some(EpicsValue::Double(7.0)));
}
