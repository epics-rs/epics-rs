//! C answers four of the six metadata slots for some fields from a LINK, not
//! from the record: `get_units`, `get_precision`, `get_graphic_double` and
//! `get_alarm_double` call `dbGetUnits` / `dbGetPrecision` /
//! `dbGetGraphicLimits` / `dbGetAlarmLimits` on the link that backs the field
//! (`calcRecord.c:161-280`, `calcoutRecord.c:417-555`, `subRecord.c:198-317`,
//! `seqRecord.c:279-367`, `aSubRecord.c:294-404` — twenty-four call sites over
//! five record types).
//!
//! Every assertion here is on a SERVED value, read through the same
//! supply-gated accessors the CA and PVA encoders read.
//!
//! `get_control_double` is deliberately absent from every case: no base record
//! routes control through a link, `dbGetControlLimits` has zero callers in all
//! of base, and `aSubRecord.c:372-376` is the type specimen calling
//! `recGblGetControlDouble` with no link branch at all.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::asub_record::ASubRecord;
use epics_base_rs::server::records::calc::CalcRecord;
use epics_base_rs::server::records::seq::SeqRecord;
use epics_base_rs::server::records::sub_record::SubRecord;
use epics_base_rs::server::snapshot::Snapshot;
use epics_base_rs::types::EpicsValue;

/// The upstream record every link in this file points at: EGU `mm`, PREC 3,
/// operator range ±10, alarm ladder 8/9 with both severities set so
/// `get_alarm_double` has something to serve.
///
/// The alarm ladder lands through `put_pv` because `ai`'s eight alarm fields
/// live on `CommonFields::analog_alarm`, not on `AiRecord`.
async fn add_source(db: &PvDatabase) {
    let mut src = AiRecord::new(1.0);
    src.hopr = 10.0;
    src.lopr = -10.0;
    src.egu = "mm".into();
    src.prec = 3;
    db.add_record("SRC", Box::new(src)).await.unwrap();
    for (field, value) in [
        ("HIHI", EpicsValue::Double(9.0)),
        ("HIGH", EpicsValue::Double(8.0)),
        ("LOW", EpicsValue::Double(-8.0)),
        ("LOLO", EpicsValue::Double(-9.0)),
        ("HHSV", EpicsValue::String("MAJOR".into())),
        ("HSV", EpicsValue::String("MINOR".into())),
        ("LSV", EpicsValue::String("MINOR".into())),
        ("LLSV", EpicsValue::String("MAJOR".into())),
    ] {
        db.put_pv(&format!("SRC.{field}"), value)
            .await
            .unwrap_or_else(|e| panic!("SRC.{field} failed to load: {e:?}"));
    }
}

fn snapshot_of(db: &PvDatabase, record: &str, field: &str) -> Snapshot {
    let rec = db
        .get_record(record)
        .unwrap_or_else(|| panic!("{record} not in the database"));
    let guard = rec.read();
    guard
        .snapshot_for_field(field)
        .unwrap_or_else(|| panic!("{record}.{field} served no snapshot"))
}

fn units_of(snap: &Snapshot) -> String {
    snap.units()
        .map(|u| u.as_str_lossy().into_owned())
        .unwrap_or_else(|| panic!("channel supplies no units"))
}

/// `calcRecord.c:169-181` (`dbGetUnits`), `:184-203` (`dbGetPrecision`),
/// `:205-233` (`dbGetGraphicLimits`), `:257-280` (`dbGetAlarmLimits`) — all
/// four keyed on `get_linkNumber`, which maps the arg letter `A` and its
/// previous-value twin `LA` onto `INPA`.
///
/// The record's OWN EGU/PREC/HOPR/LOPR are deliberately different from the
/// target's, so serving them is a visible failure rather than a coincidence.
#[epics_macros_rs::epics_test]
async fn a_calc_arg_serves_its_links_target_not_the_calc_record() {
    let db = PvDatabase::new();
    add_source(&db).await;

    let mut calc = CalcRecord::default();
    calc.egu = "V".into();
    calc.prec = 7;
    calc.hopr = 100.0;
    calc.lopr = -100.0;
    calc.inpa = "SRC".into();
    db.add_record("CALC", Box::new(calc)).await.unwrap();
    db.ioc_init().await;

    for field in ["A", "LA"] {
        let snap = snapshot_of(&db, "CALC", field);
        assert_eq!(units_of(&snap), "mm", "CALC.{field} units");
        assert_eq!(snap.precision(), Some(3), "CALC.{field} precision");
        assert_eq!(
            snap.graphic_limits(),
            Some((-10.0, 10.0)),
            "CALC.{field} display limits"
        );
        assert_eq!(
            snap.alarm_limits(),
            Some((-9.0, -8.0, 8.0, 9.0)),
            "CALC.{field} alarm limits"
        );
        // `get_control_double` (`calcRecord.c:235-255`) has no link branch:
        // its `default:` arm is `recGblGetControlDouble`, the DBF_DOUBLE type
        // range. Routing control through the link would break this.
        assert_eq!(
            snap.control_limits(),
            Some((-1e300, 1e300)),
            "CALC.{field} control limits must stay recGblGetControlDouble"
        );
    }

    // VAL is not link-backed — `get_precision` returns early for it and
    // `get_graphic_double` lists it explicitly on HOPR/LOPR.
    let val = snapshot_of(&db, "CALC", "VAL");
    assert_eq!(units_of(&val), "V");
    assert_eq!(val.precision(), Some(7));
    assert_eq!(val.graphic_limits(), Some((-100.0, 100.0)));
}

/// A CONSTANT link has no metadata getters (`dbConstLink.c:234-248` NULLs all
/// five slots), so every `dbGet*` writes nothing and `dbAccess.c`'s seeds
/// stand: units the `:378` memset, display the `:216` 0/0, alarm the `:294`
/// four NaN — and precision the record's own PREC, which `get_precision`
/// seeds before it consults the link.
#[epics_macros_rs::epics_test]
async fn a_constant_arg_keeps_every_dbaccess_seed() {
    let db = PvDatabase::new();
    let mut calc = CalcRecord::default();
    calc.egu = "V".into();
    calc.prec = 7;
    calc.hopr = 100.0;
    calc.lopr = -100.0;
    calc.inpb = "5".into();
    db.add_record("CALC", Box::new(calc)).await.unwrap();
    db.ioc_init().await;

    let snap = snapshot_of(&db, "CALC", "B");
    assert_eq!(units_of(&snap), "");
    assert_eq!(snap.precision(), Some(7));
    assert_eq!(snap.graphic_limits(), Some((0.0, 0.0)));
    let (lolo, low, high, hihi) = snap.alarm_limits().expect("calc supplies get_alarm_double");
    assert!(
        lolo.is_nan() && low.is_nan() && high.is_nan() && hihi.is_nan(),
        "constant-backed alarm limits must stay the four NaN seed, got \
         ({lolo}, {low}, {high}, {hihi})"
    );
}

/// `aSubRecord.c:294-304` — the ONE record type that routes metadata through
/// OUT links as well, and the type a central `match rtype { "calc" |
/// "calcout" | "sub" => ... }` had silently omitted. `A` reads `INPA`,
/// `VALA` reads `OUTA`, and all four slots consult both (`:306-403`).
#[epics_macros_rs::epics_test]
async fn asub_routes_metadata_through_both_its_in_and_out_links() {
    let db = PvDatabase::new();
    add_source(&db).await;

    let mut asub = ASubRecord::default();
    asub.put_field("PREC", EpicsValue::Short(7)).unwrap();
    asub.inp[0] = "SRC".into();
    asub.out[0] = "SRC".into();
    db.add_record("ASUB", Box::new(asub)).await.unwrap();
    db.ioc_init().await;

    for field in ["A", "VALA"] {
        let snap = snapshot_of(&db, "ASUB", field);
        assert_eq!(units_of(&snap), "mm", "ASUB.{field} units");
        assert_eq!(snap.precision(), Some(3), "ASUB.{field} precision");
        assert_eq!(
            snap.graphic_limits(),
            Some((-10.0, 10.0)),
            "ASUB.{field} display limits"
        );
        assert_eq!(
            snap.alarm_limits(),
            Some((-9.0, -8.0, 8.0, 9.0)),
            "ASUB.{field} alarm limits"
        );
    }
}

/// `seqRecord.c:279-280` `get_dol`: within each of the sixteen `linkGrp`s the
/// value field `DOn` reads its metadata from that group's `DOLn` (`:293`,
/// `:311`, `:333`, `:361`). `DOL1` is left a constant, so the second group is
/// the seed control in the same record.
#[epics_macros_rs::epics_test]
async fn a_seq_group_serves_its_own_dol_target() {
    let db = PvDatabase::new();
    add_source(&db).await;

    let seq = SeqRecord {
        dol0: "SRC".into(),
        dol1: "5".into(),
        ..Default::default()
    };
    db.add_record("SEQ", Box::new(seq)).await.unwrap();
    db.ioc_init().await;

    let backed = snapshot_of(&db, "SEQ", "DO0");
    assert_eq!(units_of(&backed), "mm");
    assert_eq!(backed.precision(), Some(3));
    assert_eq!(backed.graphic_limits(), Some((-10.0, 10.0)));

    let constant = snapshot_of(&db, "SEQ", "DO1");
    assert_eq!(units_of(&constant), "");
    assert_eq!(constant.graphic_limits(), Some((0.0, 0.0)));
}

/// `subRecord.c:198-204` `get_linkNumber` over `INP_ARG_MAX` (`:89`, 21) —
/// the same shape as calc's, and the fourth of the five routing types.
#[epics_macros_rs::epics_test]
async fn a_sub_arg_serves_its_links_target() {
    let db = PvDatabase::new();
    add_source(&db).await;

    let mut sub = SubRecord::default();
    sub.inp[0] = "SRC".into();
    db.add_record("SUB", Box::new(sub)).await.unwrap();
    db.ioc_init().await;

    let snap = snapshot_of(&db, "SUB", "A");
    assert_eq!(units_of(&snap), "mm");
    assert_eq!(snap.precision(), Some(3));
    assert_eq!(snap.graphic_limits(), Some((-10.0, 10.0)));
    assert_eq!(snap.alarm_limits(), Some((-9.0, -8.0, 8.0, 9.0)));
}
