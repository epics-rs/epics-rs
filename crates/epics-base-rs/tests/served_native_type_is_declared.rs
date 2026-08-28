//! What the port SERVES, pinned against the compiled C IOC.
//!
//! `dbd_generated_c_oracle.rs` pins the generated *tables* against
//! `cainfo`. That is only half the chain: a correct table buys nothing if the
//! type on the wire is derived from whatever variant the record happens to
//! store. This file pins the other half — the three delivery paths — against
//! the same measured fixture:
//!
//! * CREATE-CHANNEL: the native type CA announces, `client_field_value`;
//! * GET: `snapshot_for_field`;
//! * MONITOR: the snapshot the record posts on change.
//!
//! All three must report the DECLARED type, and all three must report the SAME
//! type — a client told `DBR_ENUM` at create time and posted a `DBR_SHORT`
//! update is a protocol violation even if each half is individually plausible.
//!
//! `tests/fixtures/c_native_types.tsv` is measured, not derived: `cainfo`
//! against `/home/stevek/work/epics-base/bin/linux-x86_64/softIoc`. See its
//! header for the instantiation the array records were probed under. Where the
//! port and the fixture disagree, the port is wrong.

use epics_base_rs::server::db_loader::create_record;
use epics_base_rs::server::record::RecordInstance;
use epics_base_rs::server::record::dbd_generated::record_fields;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const ORACLE: &str = include_str!("fixtures/c_native_types.tsv");

/// The CA type code C's `cainfo` names, as `DbFieldType` numbers it.
fn ca_code(dbf: &str) -> Option<u16> {
    Some(match dbf {
        "DBF_STRING" => DbFieldType::String,
        "DBF_SHORT" => DbFieldType::Short,
        "DBF_FLOAT" => DbFieldType::Float,
        "DBF_ENUM" => DbFieldType::Enum,
        "DBF_CHAR" => DbFieldType::Char,
        "DBF_LONG" => DbFieldType::Long,
        "DBF_DOUBLE" => DbFieldType::Double,
        _ => return None,
    } as u16)
}

fn oracle_rows() -> impl Iterator<Item = (&'static str, &'static str, &'static str)> {
    ORACLE
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(|l| {
            let c: Vec<&str> = l.split('\t').collect();
            assert!(c.len() >= 5, "malformed oracle row: {l}");
            (c[0], c[1], c[4])
        })
}

/// The CA native type the port would announce for `value` — the same
/// `value.dbr_type().ca_wire_type()` the create-channel handler sends in the
/// `CREATE_CHAN` reply.
fn served_code(value: &EpicsValue) -> u16 {
    value.dbr_type().ca_wire_type()
}

/// A record instantiated the way the fixture's C IOC was.
///
/// The fixture header states it: the array records were probed under `NELM=10
/// FTVL=DOUBLE`, because a `cvt_dbaddr` field's served type comes from the
/// record's live state and there is no such thing as "the" waveform VAL type. A
/// bare port record is NOT that instantiation — C's own bare waveform has
/// `FTVL=STRING` (menuFtype index 0, no `initial()` in any of the four `.dbd`s;
/// measured: `cainfo` on `record(waveform,"P:WF"){}` reports `DBF_STRING`). The
/// port's FTVL default used to be DOUBLE, which made the bare record accidentally
/// agree with the fixture's DOUBLE row and hid the deviation; with the default
/// corrected, the FIXTURE'S instantiation has to be applied here or the comparison
/// is against a record C never probed.
fn instance(record_type: &str) -> Option<RecordInstance> {
    let mut rec = create_record(record_type).ok()?;
    if matches!(record_type, "waveform" | "aai" | "aao" | "subArray") {
        // menuFtype index 10 = DOUBLE, and NELM=10 (MALM for subArray, whose
        // buffer is the source view).
        rec.put_field("FTVL", EpicsValue::Short(10)).ok()?;
        rec.put_field("NELM", EpicsValue::Long(10)).ok()?;
        if record_type == "subArray" {
            rec.put_field("MALM", EpicsValue::Long(10)).ok()?;
        }
    }
    Some(RecordInstance::new_boxed(format!("T:{record_type}"), rec))
}

/// The native type a CA client is told at create time is the field's DECLARED
/// type — for every field of every record type the C IOC serves.
///
/// This is the whole finding: the tables were already right, and the wire was
/// still wrong, because `client_field_value` read the type off the stored
/// variant. A `DBF_MENU` field stored as a short went out `DBF_SHORT`; a
/// `DBF_UCHAR` field stored as a short went out `DBF_SHORT`; a `DBF_ULONG`
/// field stored as a double went out `DBF_DOUBLE`.
#[test]
fn create_channel_announces_the_declared_type() {
    let mut checked = 0usize;
    let mut wrong = Vec::new();

    for (record, field, c_native) in oracle_rows() {
        if record_fields(record).is_none() {
            continue;
        }
        let Some(inst) = instance(record) else {
            continue;
        };
        let Some(value) = inst.client_field_value(field) else {
            // A field the port does not resolve at all is a coverage gap, not
            // a type defect; `dbd_generated_c_oracle` owns that assertion.
            continue;
        };
        let want = ca_code(c_native).unwrap_or_else(|| panic!("{record}.{field}: {c_native}?"));
        let got = served_code(&value);
        if got != want {
            wrong.push(format!(
                "{record}.{field}: C serves {c_native} (code {want}), \
                 port announces code {got} from {value:?}"
            ));
        }
        checked += 1;
    }

    assert!(
        wrong.is_empty(),
        "{} field(s) are served at a type C does not serve them at:\n  {}",
        wrong.len(),
        wrong.join("\n  ")
    );
    assert!(checked > 2000, "oracle covered only {checked} fields");
}

/// CREATE-CHANNEL, GET and MONITOR agree. The create-channel reply is a
/// promise about every later `DBR_x` on that channel; a GET or a monitor
/// update at a different native type breaks the client's decode.
///
/// Boundary: the monitor path is fed the record's STORED variant (it is posted
/// from the change-detection loop, not re-resolved), so it is the path most
/// likely to skip the projection. Feeding it the raw stored value here is the
/// point of the case.
#[test]
fn get_and_monitor_serve_what_create_channel_announced() {
    let mut disagree = Vec::new();

    for (record, field, _) in oracle_rows() {
        if record_fields(record).is_none() {
            continue;
        }
        let Some(inst) = instance(record) else {
            continue;
        };
        let (Some(announced), Some(get)) = (
            inst.client_field_value(field),
            inst.snapshot_for_field(field),
        ) else {
            continue;
        };
        let announced = served_code(&announced);
        if served_code(&get.value) != announced {
            disagree.push(format!(
                "{record}.{field}: create-channel says {announced}, GET serves {} ({:?})",
                served_code(&get.value),
                get.value
            ));
        }
        // The monitor path, fed the raw stored variant the record would post.
        let Some(stored) = inst.resolve_field(field) else {
            continue;
        };
        let posted = inst.make_monitor_snapshot(
            field,
            stored,
            epics_base_rs::server::database::LinkBacking::none(),
        );
        if served_code(&posted.value) != announced {
            disagree.push(format!(
                "{record}.{field}: create-channel says {announced}, MONITOR posts {} ({:?})",
                served_code(&posted.value),
                posted.value
            ));
        }
    }

    assert!(
        disagree.is_empty(),
        "{} field(s) do not serve what they announced:\n  {}",
        disagree.len(),
        disagree.join("\n  ")
    );
}

/// The projection is idempotent: serving an already-served value does not move
/// it again. This is what lets the CA path derive the announced native type
/// from the very value it is about to write, instead of keeping a second copy
/// of the type rule.
#[test]
fn projecting_a_served_value_is_a_no_op() {
    for (record, field, _) in oracle_rows() {
        if record_fields(record).is_none() {
            continue;
        }
        let Some(inst) = instance(record) else {
            continue;
        };
        let Some(once) = inst.client_field_value(field) else {
            continue;
        };
        let twice = inst.project_to_declared_type(field, once.clone());
        // Compared through `Debug`, not `PartialEq`: several fields initialise
        // to NaN (`dfanout.ALST`), and NaN != NaN would make even an identity
        // projection look like a move.
        assert_eq!(
            format!("{once:?}"),
            format!("{twice:?}"),
            "{record}.{field}: projecting a served value moved it"
        );
    }
}

/// A `cvt_dbaddr` field with a SELECTOR is the one field whose declared type is
/// NOT what it is served as: C overwrites `paddr->field_type` from the
/// selector's live value. `waveform.VAL` under `FTVL=LONG` is served
/// `DBF_LONG`, not the `DBF_DOUBLE` the `.dbd` placeholder declares — so the
/// projection must leave it alone, or every non-DOUBLE waveform in the world
/// would be coerced to doubles on the wire.
///
/// The negative half of the boundary: `histogram.VAL` is `special(SPC_DBADDR)`
/// too, but its type is FIXED (`epicsUInt32`, no selector), so it IS projected
/// — and that projection is what makes C's `DBF_DOUBLE` promotion of
/// `DBF_ULONG` come out right.
#[test]
fn a_selector_typed_field_keeps_its_runtime_type() {
    let mut inst = instance("waveform").expect("waveform is registered");
    // `menuFtype` index 5 is LONG (`menuFtype.dbd`); the port defaults FTVL to
    // 10, DOUBLE, which is what the C oracle fixture was probed under.
    inst.record
        .put_field("FTVL", EpicsValue::Short(5))
        .expect("FTVL is writable");
    inst.record
        .put_field("VAL", EpicsValue::LongArray(vec![7, 8, 9]))
        .expect("VAL takes a long array under FTVL=LONG");

    let served = inst
        .client_field_value("VAL")
        .expect("waveform.VAL resolves");
    assert_eq!(
        served.dbr_type(),
        DbFieldType::Long,
        "FTVL=LONG re-types VAL to DBF_LONG (cvt_dbaddr); the .dbd's \
         DBF_DOUBLE placeholder must not be projected onto it, got {served:?}"
    );

    // Fixed-type SPC_DBADDR: no selector, so the declaration is the truth.
    let hist = instance("histogram").expect("histogram is registered");
    let served = hist
        .client_field_value("VAL")
        .expect("histogram.VAL resolves");
    assert_eq!(
        served.dbr_type().ca_wire_type(),
        DbFieldType::Double as u16,
        "histogram.VAL is DBF_ULONG, which CA has no type for and C promotes \
         to DBR_DOUBLE, got {served:?}"
    );
}

/// A record type with NO vendored `.dbd` has no declaration at all, and then
/// its hand-written `field_list()` is what reaches the wire — declaration by
/// accident. `subArray` was that record type: its hand table typed `FTVL`
/// `Short`, so C's `DBF_ENUM` (`field(FTVL,DBF_MENU) menu(menuFtype)`) went out
/// as a bare `DBF_SHORT` and a client got an index with no `menuFtype` labels.
///
/// Asserted on the resolved value, not on the table, because a field the record
/// does not resolve is SKIPPED by the sweep above — a missing `.dbd` would
/// otherwise look like coverage.
#[test]
fn sub_array_is_declared_by_the_dbd_not_by_its_hand_table() {
    let mut inst = instance("subArray").expect("subArray is registered");

    let ftvl = inst
        .client_field_value("FTVL")
        .expect("subArray.FTVL resolves");
    assert_eq!(
        ftvl.dbr_type(),
        DbFieldType::Enum,
        "subArray.FTVL is menu(menuFtype) — C serves DBF_ENUM, got {ftvl:?}"
    );

    // VAL is the same `cvt_dbaddr` selector shape as waveform's: the element
    // type IS FTVL (subArrayRecord.c:225-234), so the .dbd's DBF_NOACCESS
    // placeholder must not be projected onto it.
    inst.record
        .put_field("FTVL", EpicsValue::Short(5))
        .expect("FTVL is writable");
    inst.record
        .put_field("VAL", EpicsValue::LongArray(vec![1, 2, 3]))
        .expect("VAL takes a long array under FTVL=LONG");
    let val = inst
        .client_field_value("VAL")
        .expect("subArray.VAL resolves");
    assert_eq!(
        val.dbr_type(),
        DbFieldType::Long,
        "FTVL=LONG re-types subArray.VAL to DBF_LONG, got {val:?}"
    );
}

/// The `SDEF` selector, both halves. `mbbo.VAL` is the one field whose runtime
/// type is chosen by whether the record has any state table at all
/// (`mbboRecord.c:300-313`: `if (!prec->sdef) paddr->field_type = DBF_USHORT`),
/// and `DBF_USHORT` has no CA wire type, so a stateless mbbo goes out
/// `DBF_LONG`. Measured on the C softIoc, both records in one `.db`:
///
/// ```text
/// record(mbbo,"P:BARE") {}                                    -> DBF_LONG
/// record(mbbo,"P:SDEF") { field(ZRST,"zero") field(ONST,"one")
///                         field(ZRVL,"0")   field(ONVL,"1") } -> DBF_ENUM
/// ```
///
/// A client that asked a stateless mbbo for `DBR_ENUM` would be handed an index
/// with no labels behind it — which is precisely why C refuses to call it an
/// enum. The declaration cannot answer this; only the record can.
#[test]
fn a_stateless_mbbo_serves_its_val_as_dbf_long() {
    let mut inst = instance("mbbo").expect("mbbo is registered");
    let bare = inst.client_field_value("VAL").expect("mbbo.VAL resolves");
    assert_eq!(
        bare.dbr_type().ca_wire_type(),
        DbFieldType::Long as u16,
        "a stateless mbbo has no labels to serve, so C degenerates VAL to \
         DBF_USHORT -> DBF_LONG on the wire, got {bare:?}"
    );

    // One state string is enough to define the table (C tests every ZRVL..FFVL
    // and every ZRST..FFST; any non-zero or non-empty entry sets sdef).
    inst.record
        .put_field("ZRST", EpicsValue::String("zero".into()))
        .expect("ZRST is writable");
    let with_states = inst.client_field_value("VAL").expect("mbbo.VAL resolves");
    assert_eq!(
        with_states.dbr_type(),
        DbFieldType::Enum,
        "an mbbo WITH a state table serves VAL as DBF_ENUM, got {with_states:?}"
    );

    // And the put path must accept what the get path advertised: a stateless
    // mbbo is served DBF_USHORT, so an inbound put is coerced to `UShort`
    // before it reaches `put_field` — a `TypeMismatch` here would make the
    // field unwritable over CA.
    let mut bare = instance("mbbo").expect("mbbo is registered");
    bare.record
        .put_field("VAL", EpicsValue::UShort(3))
        .expect("a stateless mbbo takes the UShort it is served as");
    assert_eq!(bare.client_field_value("VAL"), Some(EpicsValue::UShort(3)));
}
