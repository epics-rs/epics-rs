//! Integration tests for the #[derive(EpicsRecord)] procedural macro.
//!
//! Tests verify:
//! - record_type() returns the correct type string
//! - the derive is NOT a field-declaration source (the `.dbd` is)
//! - get_field() retrieves typed values
//! - put_field() sets typed values
//! - Field type mappings (Double, String, Short, Long, Float)
//! - read_only fields reject writes
//! - snake_case -> UPPER_CASE field name conversion
//! - Type mismatch error handling
//! - Unknown field error handling

use epics_base_rs::error::CaError;
use epics_base_rs::server::record::{FieldDeclaration, Record};
use epics_base_rs::types::{DbFieldType, EpicsValue};
use epics_macros_rs::EpicsRecord;

// ---------------------------------------------------------------------------
// Test struct definitions
// ---------------------------------------------------------------------------

#[derive(EpicsRecord)]
#[record(type = "ai", crate_path = "epics_base_rs")]
struct AiRecord {
    #[field(type = "Double")]
    val: f64,
    #[field(type = "Double")]
    high_limit: f64,
    #[field(type = "Double")]
    low_limit: f64,
    #[field(type = "Short")]
    precision: i16,
    #[field(type = "String")]
    engineering_units: String,
}

#[derive(EpicsRecord)]
#[record(type = "stringin", crate_path = "epics_base_rs")]
struct TestStringinRecord {
    #[field(type = "String")]
    val: String,
    #[field(type = "Short")]
    simm: i16,
    #[field(type = "Short")]
    sims: i16,
    #[field(type = "Double")]
    sdly: f64,
}

#[derive(EpicsRecord)]
#[record(type = "ao", crate_path = "epics_base_rs")]
struct AoRecord {
    #[field(type = "Double")]
    val: f64,
    #[field(type = "Float")]
    some_float: f32,
    #[field(type = "Long")]
    some_long: i32,
    #[field(type = "Short", read_only)]
    status: i16,
}

#[derive(EpicsRecord)]
#[record(type = "calc", crate_path = "epics_base_rs")]
struct CalcRecord {
    #[field(type = "Double")]
    val: f64,
    #[field(type = "Double")]
    a: f64,
    #[field(type = "Double")]
    b: f64,
    #[field(type = "String")]
    calc_expr: String,
    #[field(type = "Double", read_only)]
    last_val: f64,
}

// ---------------------------------------------------------------------------
// record_type()
// ---------------------------------------------------------------------------

#[test]
fn record_type_returns_correct_string() {
    let ai = AiRecord {
        val: 0.0,
        high_limit: 0.0,
        low_limit: 0.0,
        precision: 0,
        engineering_units: String::new(),
    };
    assert_eq!(ai.record_type(), "ai");

    let stringin = TestStringinRecord {
        val: String::new(),
        simm: 0,
        sims: 0,
        sdly: -1.0,
    };
    assert_eq!(stringin.record_type(), "stringin");

    let ao = AoRecord {
        val: 0.0,
        some_float: 0.0,
        some_long: 0,
        status: 0,
    };
    assert_eq!(ao.record_type(), "ao");

    let calc = CalcRecord {
        val: 0.0,
        a: 0.0,
        b: 0.0,
        calc_expr: String::new(),
        last_val: 0.0,
    };
    assert_eq!(calc.record_type(), "calc");
}

// ---------------------------------------------------------------------------
// The derive is not a declaration source
// ---------------------------------------------------------------------------

/// `#[derive(EpicsRecord)]` declares no field.
///
/// It used to emit a `field_list()` built from the Rust struct members, which
/// named each field after its member (`HIGH_LIMIT`, `ENGINEERING_UNITS` — names
/// no `.dbd` has) and typed it from the member's Rust type. That is how
/// `longin.ADEL`, whose member is an `f64`, was declared `DBF_DOUBLE` where
/// `longinRecord.dbd` declares it `DBF_LONG`: the member types the *storage*,
/// only the `.dbd` types the *field*. A record type has exactly one
/// declaration, and it is the `.dbd`.
#[test]
fn the_derive_declares_no_fields() {
    let ai = AiRecord {
        val: 0.0,
        high_limit: 0.0,
        low_limit: 0.0,
        precision: 0,
        engineering_units: String::new(),
    };
    assert!(
        Record::declared_fields(&ai).is_empty(),
        "the derive must not hand a record type a second declaration"
    );

    let ao = AoRecord {
        val: 0.0,
        some_float: 0.0,
        some_long: 0,
        status: 0,
    };
    assert!(
        Record::declared_fields(&ao).is_empty(),
        "the derive must not hand a record type a second declaration"
    );
}

/// ...and what a derived record IS declared by is the generated `.dbd` table.
/// The struct-member names never reach the declaration.
#[test]
fn a_derived_record_is_declared_by_its_dbd() {
    let ai = AiRecord {
        val: 0.0,
        high_limit: 0.0,
        low_limit: 0.0,
        precision: 0,
        engineering_units: String::new(),
    };
    let names: Vec<&str> = ai.field_list().iter().map(|f| f.name).collect();

    // `aiRecord.dbd` fields, present because the `.dbd` declares them — not
    // because this struct has a member of that name.
    for declared in ["VAL", "RVAL", "EGU", "PREC", "LINR", "AOFF", "ASLO"] {
        assert!(
            names.contains(&declared),
            "ai.{declared} is in aiRecord.dbd but not in the served declaration"
        );
    }
    // The struct-member-derived names are gone.
    for member in ["HIGH_LIMIT", "LOW_LIMIT", "PRECISION", "ENGINEERING_UNITS"] {
        assert!(
            !names.contains(&member),
            "{member} is a Rust member name, not an aiRecord.dbd field"
        );
    }

    // And the served type is the `.dbd`'s. `PREC` is `DBF_SHORT` there; the
    // struct types its `precision` member `i16`, which happens to agree — so
    // pin a field where the two would NOT agree if the members were consulted:
    // `EGU` is a `DBF_STRING` the struct has no member for at all.
    let egu = ai.field_list().iter().find(|f| f.name == "EGU").unwrap();
    assert_eq!(egu.dbf_type, DbFieldType::String);
}

// ---------------------------------------------------------------------------
// get_field()
// ---------------------------------------------------------------------------

#[test]
fn get_field_double() {
    let ai = AiRecord {
        val: 3.15,
        high_limit: 100.0,
        low_limit: -100.0,
        precision: 3,
        engineering_units: "volts".into(),
    };
    assert_eq!(ai.get_field("VAL"), Some(EpicsValue::Double(3.15)));
    assert_eq!(ai.get_field("HIGH_LIMIT"), Some(EpicsValue::Double(100.0)));
    assert_eq!(ai.get_field("LOW_LIMIT"), Some(EpicsValue::Double(-100.0)));
}

#[test]
fn get_field_short() {
    let ai = AiRecord {
        val: 0.0,
        high_limit: 0.0,
        low_limit: 0.0,
        precision: 5,
        engineering_units: String::new(),
    };
    assert_eq!(ai.get_field("PRECISION"), Some(EpicsValue::Short(5)));
}

#[test]
fn get_field_string() {
    let ai = AiRecord {
        val: 0.0,
        high_limit: 0.0,
        low_limit: 0.0,
        precision: 0,
        engineering_units: "degrees".into(),
    };
    assert_eq!(
        ai.get_field("ENGINEERING_UNITS"),
        Some(EpicsValue::String("degrees".into()))
    );
}

#[test]
fn get_field_float() {
    let ao = AoRecord {
        val: 0.0,
        some_float: 2.5,
        some_long: 0,
        status: 0,
    };
    assert_eq!(ao.get_field("SOME_FLOAT"), Some(EpicsValue::Float(2.5)));
}

#[test]
fn get_field_long() {
    let ao = AoRecord {
        val: 0.0,
        some_float: 0.0,
        some_long: 42,
        status: 0,
    };
    assert_eq!(ao.get_field("SOME_LONG"), Some(EpicsValue::Long(42)));
}

#[test]
fn get_field_unknown_returns_none() {
    let ai = AiRecord {
        val: 0.0,
        high_limit: 0.0,
        low_limit: 0.0,
        precision: 0,
        engineering_units: String::new(),
    };
    assert_eq!(ai.get_field("NONEXISTENT"), None);
    assert_eq!(ai.get_field(""), None);
}

// ---------------------------------------------------------------------------
// put_field()
// ---------------------------------------------------------------------------

#[test]
fn put_field_double() {
    let mut ai = AiRecord {
        val: 0.0,
        high_limit: 0.0,
        low_limit: 0.0,
        precision: 0,
        engineering_units: String::new(),
    };
    ai.put_field("VAL", EpicsValue::Double(42.0)).unwrap();
    assert_eq!(ai.val, 42.0);
    assert_eq!(ai.get_field("VAL"), Some(EpicsValue::Double(42.0)));
}

#[test]
fn put_field_short() {
    let mut ai = AiRecord {
        val: 0.0,
        high_limit: 0.0,
        low_limit: 0.0,
        precision: 0,
        engineering_units: String::new(),
    };
    ai.put_field("PRECISION", EpicsValue::Short(7)).unwrap();
    assert_eq!(ai.precision, 7);
}

#[test]
fn put_field_string() {
    let mut ai = AiRecord {
        val: 0.0,
        high_limit: 0.0,
        low_limit: 0.0,
        precision: 0,
        engineering_units: String::new(),
    };
    ai.put_field("ENGINEERING_UNITS", EpicsValue::String("mA".into()))
        .unwrap();
    assert_eq!(ai.engineering_units, "mA");
}

#[test]
fn put_field_float() {
    let mut ao = AoRecord {
        val: 0.0,
        some_float: 0.0,
        some_long: 0,
        status: 0,
    };
    ao.put_field("SOME_FLOAT", EpicsValue::Float(1.5)).unwrap();
    assert_eq!(ao.some_float, 1.5);
}

#[test]
fn put_field_long() {
    let mut ao = AoRecord {
        val: 0.0,
        some_float: 0.0,
        some_long: 0,
        status: 0,
    };
    ao.put_field("SOME_LONG", EpicsValue::Long(99)).unwrap();
    assert_eq!(ao.some_long, 99);
}

// ---------------------------------------------------------------------------
// read_only fields
// ---------------------------------------------------------------------------

#[test]
fn put_field_read_only_rejected() {
    let mut ao = AoRecord {
        val: 0.0,
        some_float: 0.0,
        some_long: 0,
        status: 0,
    };
    let result = ao.put_field("STATUS", EpicsValue::Short(1));
    assert!(result.is_err());
    match result.unwrap_err() {
        CaError::ReadOnlyField(field) => assert_eq!(field, "STATUS"),
        other => panic!("expected ReadOnlyField, got: {other:?}"),
    }
    // Value should be unchanged
    assert_eq!(ao.status, 0);
}

#[test]
fn get_field_read_only_still_works() {
    let ao = AoRecord {
        val: 0.0,
        some_float: 0.0,
        some_long: 0,
        status: 7,
    };
    assert_eq!(ao.get_field("STATUS"), Some(EpicsValue::Short(7)));
}

#[test]
fn put_field_read_only_in_calc() {
    let mut calc = CalcRecord {
        val: 0.0,
        a: 0.0,
        b: 0.0,
        calc_expr: String::new(),
        last_val: 0.0,
    };
    let result = calc.put_field("LAST_VAL", EpicsValue::Double(5.0));
    assert!(result.is_err());
    match result.unwrap_err() {
        CaError::ReadOnlyField(field) => assert_eq!(field, "LAST_VAL"),
        other => panic!("expected ReadOnlyField, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// snake_case -> UPPER_CASE field name conversion
// ---------------------------------------------------------------------------

#[test]
fn snake_case_to_upper_case_conversion() {
    let ai = AiRecord {
        val: 0.0,
        high_limit: 10.0,
        low_limit: -10.0,
        precision: 0,
        engineering_units: "mm".into(),
    };

    // snake_case field names should be accessible as UPPER_CASE
    assert_eq!(ai.get_field("HIGH_LIMIT"), Some(EpicsValue::Double(10.0)));
    assert_eq!(ai.get_field("LOW_LIMIT"), Some(EpicsValue::Double(-10.0)));
    assert_eq!(
        ai.get_field("ENGINEERING_UNITS"),
        Some(EpicsValue::String("mm".into()))
    );

    // The original snake_case name should NOT work
    assert_eq!(ai.get_field("high_limit"), None);
    assert_eq!(ai.get_field("engineering_units"), None);
}

// ---------------------------------------------------------------------------
// Type mismatch error handling
// ---------------------------------------------------------------------------

#[test]
fn put_field_type_mismatch_double_gets_string() {
    let mut ai = AiRecord {
        val: 0.0,
        high_limit: 0.0,
        low_limit: 0.0,
        precision: 0,
        engineering_units: String::new(),
    };
    let result = ai.put_field("VAL", EpicsValue::String("not a number".into()));
    assert!(result.is_err());
    match result.unwrap_err() {
        CaError::TypeMismatch(field) => assert_eq!(field, "VAL"),
        other => panic!("expected TypeMismatch, got: {other:?}"),
    }
}

#[test]
fn put_field_type_mismatch_short_gets_double() {
    let mut ai = AiRecord {
        val: 0.0,
        high_limit: 0.0,
        low_limit: 0.0,
        precision: 0,
        engineering_units: String::new(),
    };
    let result = ai.put_field("PRECISION", EpicsValue::Double(3.15));
    assert!(result.is_err());
    match result.unwrap_err() {
        CaError::TypeMismatch(field) => assert_eq!(field, "PRECISION"),
        other => panic!("expected TypeMismatch, got: {other:?}"),
    }
}

#[test]
fn put_field_type_mismatch_string_gets_long() {
    let mut ai = AiRecord {
        val: 0.0,
        high_limit: 0.0,
        low_limit: 0.0,
        precision: 0,
        engineering_units: String::new(),
    };
    let result = ai.put_field("ENGINEERING_UNITS", EpicsValue::Long(42));
    assert!(result.is_err());
    match result.unwrap_err() {
        CaError::TypeMismatch(field) => assert_eq!(field, "ENGINEERING_UNITS"),
        other => panic!("expected TypeMismatch, got: {other:?}"),
    }
}

#[test]
fn put_field_type_mismatch_long_gets_short() {
    let mut ao = AoRecord {
        val: 0.0,
        some_float: 0.0,
        some_long: 0,
        status: 0,
    };
    let result = ao.put_field("SOME_LONG", EpicsValue::Short(5));
    assert!(result.is_err());
    match result.unwrap_err() {
        CaError::TypeMismatch(field) => assert_eq!(field, "SOME_LONG"),
        other => panic!("expected TypeMismatch, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Unknown field error handling
// ---------------------------------------------------------------------------

#[test]
fn put_field_unknown_field_error() {
    let mut ai = AiRecord {
        val: 0.0,
        high_limit: 0.0,
        low_limit: 0.0,
        precision: 0,
        engineering_units: String::new(),
    };
    let result = ai.put_field("NONEXISTENT", EpicsValue::Double(1.0));
    assert!(result.is_err());
    match result.unwrap_err() {
        CaError::FieldNotFound(field) => assert_eq!(field, "NONEXISTENT"),
        other => panic!("expected FieldNotFound, got: {other:?}"),
    }
}

#[test]
fn put_field_empty_name_error() {
    let mut ai = AiRecord {
        val: 0.0,
        high_limit: 0.0,
        low_limit: 0.0,
        precision: 0,
        engineering_units: String::new(),
    };
    let result = ai.put_field("", EpicsValue::Double(1.0));
    assert!(result.is_err());
    match result.unwrap_err() {
        CaError::FieldNotFound(field) => assert_eq!(field, ""),
        other => panic!("expected FieldNotFound, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Multiple put/get round-trips
// ---------------------------------------------------------------------------

#[test]
fn put_get_roundtrip_multiple_fields() {
    let mut calc = CalcRecord {
        val: 0.0,
        a: 0.0,
        b: 0.0,
        calc_expr: String::new(),
        last_val: 0.0,
    };

    calc.put_field("A", EpicsValue::Double(1.0)).unwrap();
    calc.put_field("B", EpicsValue::Double(2.0)).unwrap();
    calc.put_field("VAL", EpicsValue::Double(3.0)).unwrap();
    calc.put_field("CALC_EXPR", EpicsValue::String("A+B".into()))
        .unwrap();

    assert_eq!(calc.get_field("A"), Some(EpicsValue::Double(1.0)));
    assert_eq!(calc.get_field("B"), Some(EpicsValue::Double(2.0)));
    assert_eq!(calc.get_field("VAL"), Some(EpicsValue::Double(3.0)));
    assert_eq!(
        calc.get_field("CALC_EXPR"),
        Some(EpicsValue::String("A+B".into()))
    );
    // read-only field still readable
    assert_eq!(calc.get_field("LAST_VAL"), Some(EpicsValue::Double(0.0)));
}

// ---------------------------------------------------------------------------
// link_text_ref
// ---------------------------------------------------------------------------

#[derive(EpicsRecord)]
#[record(type = "longin", crate_path = "epics_base_rs")]
struct LinkFieldRecord {
    #[field(type = "String", link)]
    siol: String,
    #[field(type = "String")]
    egu: String,
}

#[test]
fn link_fields_are_lent_and_others_are_not() {
    let mut rec = LinkFieldRecord {
        siol: String::new(),
        egu: String::new(),
    };
    assert_eq!(rec.link_text_ref("SIOL"), Some(""));
    rec.put_field("SIOL", EpicsValue::String("OTHER.VAL".into()))
        .unwrap();
    assert_eq!(rec.link_text_ref("SIOL"), Some("OTHER.VAL"));
    assert_eq!(rec.link_text_ref("EGU"), None);
    assert_eq!(rec.link_text_ref("VAL"), None);
}

#[test]
fn a_type_with_no_link_field_lends_nothing() {
    let rec = TestStringinRecord {
        val: String::new(),
        simm: 0,
        sims: 0,
        sdly: -1.0,
    };
    assert_eq!(rec.link_text_ref("VAL"), None);
}

#[test]
fn simulation_cells_are_lent_and_a_type_without_them_lends_nothing() {
    let rec = TestStringinRecord {
        val: String::new(),
        simm: 1,
        sims: 2,
        sdly: 0.5,
    };
    assert_eq!(
        rec.simm_cells(),
        Some(epics_base_rs::server::record::SimmCells {
            simm: 1,
            sims: 2,
            sdly: 0.5
        })
    );
    let ai = AiRecord {
        val: 0.0,
        high_limit: 0.0,
        low_limit: 0.0,
        precision: 0,
        engineering_units: String::new(),
    };
    assert_eq!(ai.simm_cells(), None);
}

#[derive(EpicsRecord)]
#[record(type = "longin", crate_path = "epics_base_rs")]
struct CycleCellRecord {
    #[field(type = "Long")]
    val: i32,
    #[field(type = "Long")]
    mdel: i32,
    #[field(type = "Double")]
    mlst: f64,
    #[field(type = "Long", read_only)]
    lalm: i32,
}

/// `cycle_cell`/`put_cycle_cell` are emitted from the member names and must
/// answer exactly as the member's own `get_field`/`put_field` arm, including
/// a cell the struct does not store and a read-only one.
#[test]
fn cycle_cells_answer_as_their_fields_do() {
    use epics_base_rs::server::record::{CycleCell, CycleRead, CycleValue};

    let rec = || CycleCellRecord {
        val: 3,
        mdel: 5,
        mlst: 7.0,
        lalm: 9,
    };
    let (mut fast, mut slow) = (rec(), rec());
    for cell in CycleCell::ALL {
        let by_name = fast.get_field(cell.name());
        match fast.cycle_cell(cell) {
            CycleRead::Value(v) => assert_eq!(Some(v.into()), by_name, "{cell:?}"),
            CycleRead::Absent => assert_eq!(by_name, None, "{cell:?}"),
            CycleRead::ByName => panic!("{cell:?} is stored as a number"),
        }
        for value in [CycleValue::Long(11), CycleValue::Double(13.0)] {
            let stored = fast.put_cycle_cell(cell, value);
            let put = slow.put_field(cell.name(), value.into());
            assert_eq!(format!("{stored:?}"), format!("{put:?}"), "{cell:?}");
            assert_eq!(fast.get_field(cell.name()), slow.get_field(cell.name()));
        }
    }
}

#[derive(EpicsRecord)]
#[record(type = "longin", crate_path = "epics_base_rs", soft_input_read_as = Long)]
struct SoftLonginRecord {
    #[field(type = "Long")]
    val: i32,
}

/// `soft_input_read_as` answers its variant under every soft DTYP, and its
/// absence leaves the record's own INP request.
#[test]
fn soft_input_read_as_names_every_soft_dsets_request() {
    use epics_base_rs::server::device_support::SoftDtyp;
    use epics_base_rs::server::record::{InputLinkRequest, LinkReadAs};

    let rec = SoftLonginRecord { val: 0 };
    for soft in [SoftDtyp::Plain, SoftDtyp::Raw, SoftDtyp::Async] {
        assert_eq!(
            rec.soft_input_request(soft),
            InputLinkRequest::As(LinkReadAs::Long)
        );
    }
    let plain = AiRecord {
        val: 0.0,
        high_limit: 0.0,
        low_limit: 0.0,
        precision: 0,
        engineering_units: String::new(),
    };
    assert_eq!(
        plain.soft_input_request(SoftDtyp::Plain),
        plain.input_link_request("INP")
    );
}

#[derive(EpicsRecord)]
#[record(
    type = "seq",
    crate_path = "epics_base_rs",
    take_cycle_posted_fields = take_marks,
    fields_posted_only_when_marked = MARKED_ONLY,
    process_posted_fields = POSTED
)]
struct MarkingRecord {
    /// The marks, as bits: a derive member is a field.
    #[field(type = "Long")]
    val: i32,
}

static POSTS: [(&str, epics_base_rs::server::record::CyclePostMask); 2] = [
    (
        "A",
        epics_base_rs::server::record::CyclePostMask::MonitorValueLog,
    ),
    ("B", epics_base_rs::server::record::CyclePostMask::Value),
];
const MARKED_ONLY: [&str; 2] = ["A", "B"];
const POSTED: [&str; 3] = ["VAL", "A", "B"];

fn take_marks(rec: &mut MarkingRecord) -> epics_base_rs::server::record::CycleMarks {
    epics_base_rs::server::record::CycleMarks::of(&POSTS, std::mem::take(&mut rec.val) as u128)
}

/// The post attributes reach the named function and sets, and make the cycle
/// take the marks; a type without them keeps the trait's empty answers.
#[test]
fn post_attributes_forward_the_marks_and_the_claimed_set() {
    use epics_base_rs::server::record::CycleHooks;

    let mut rec = MarkingRecord { val: 0b10 };
    assert!(rec.cycle_hooks().has(CycleHooks::CYCLE_POSTED));
    assert_eq!(rec.fields_posted_only_when_marked(), &MARKED_ONLY);
    assert_eq!(rec.process_posted_fields(), Some(&POSTED[..]));
    assert_eq!(
        rec.take_cycle_posted_fields().collect::<Vec<_>>(),
        [POSTS[1]]
    );
    assert!(rec.take_cycle_posted_fields().is_empty());

    let mut plain = SoftLonginRecord { val: 0 };
    assert_eq!(plain.cycle_hooks(), CycleHooks::NONE);
    assert!(plain.fields_posted_only_when_marked().is_empty());
    assert_eq!(plain.process_posted_fields(), None);
    assert!(plain.take_cycle_posted_fields().is_empty());
}
