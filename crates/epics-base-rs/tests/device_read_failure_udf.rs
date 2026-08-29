//! A device support read that produced no value must not let the framework
//! re-derive UDF — and a `-2` read must leave the record undefined.
//!
//! C's `read_ai()` return convention is three-valued, and `aiRecord.c:159-161`
//! is the whole contract:
//!
//! ```c
//!   if (status==0) convert(prec);
//!   else if (status==2) status=0;
//!   if (status == 0) prec->udf = isnan(prec->val);
//! ```
//!
//! `0` and `2` both reach the UDF re-derive (C folds `2` down first); the
//! negative returns do not. `processAiAverage` uses both of them
//! (`devAsynInt32.c`, with the `devAsynFloat64.c` and `devAsynInt64.c` twins):
//! `-2` with `prec->udf = 1` when the averaging period held no samples
//! (`:900-904`), `-1` when the period's transport failed (`:924-927`), where
//! UDF is left alone. The port modelled the convention as one boolean
//! (`did_compute`), so the failing half was unrepresentable and the framework
//! re-derived UDF to 0 on the very cycle device support declared the value
//! undefined — `caget REC.UDF` read 0 where C reads 1.
//!
//! The gate is NOT ai-only, and the other record types do not all agree with
//! ai, so each class is asserted here against its own C source:
//!
//! | record            | C `process()` assigns UDF                                  |
//! |-------------------|------------------------------------------------------------|
//! | ai                | `if (status==0) udf = isnan(val)`, `2` folded to `0` (:158-161) |
//! | longin / int64in  | `if (status==0) udf = FALSE` (`longinRecord.c:148`)         |
//! | stringin / lsi    | never — `devSiSoft.c::read_stringin` clears it itself       |
//! | waveform / aai    | `udf = FALSE` UNCONDITIONALLY, after `readValue` whatever it returned (`waveformRecord.c:143-144`) |
//!
//! so a negative read leaves ai/longin/stringin exactly as they were and still
//! leaves waveform DEFINED. The fix must reproduce both halves: freezing UDF
//! for every record on a failed read would be as wrong as re-deriving it for
//! every record.
//!
//! Every assertion here is driven through `DeviceSupport::read`, never by
//! writing `common.udf`: the point is the device-support contract, and a test
//! that sets the field by hand proves nothing about the path.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::device_support::{DeviceReadOutcome, DeviceSupport, DeviceUdf};
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_app::DeviceSupportContext;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::{EventMask, alarm_status};
use epics_base_rs::server::record::{AlarmSeverity, Record};
use epics_base_rs::types::{DbFieldType, EpicsValue};

use epics_base_rs::error::CaResult;

const REC: &str = "TEST:AVG";

/// One C `read_ai()` return, as a script step. Named for the C return rather
/// than for the port's types so the table stays readable when the port splits
/// the convention differently from C's single `long`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CReturn {
    /// `return 0` — wrote RVAL, said nothing about `prec->udf`
    /// (`devAiSoftRaw.c`).
    Zero,
    /// `prec->udf = FALSE; return 2` — wrote VAL and declared it defined, the
    /// shape of every C dset that returns 2 (`devBiSoft.c:54-59`).
    TwoDefined,
    /// `return -1` — sourced nothing, said nothing about `prec->udf`
    /// (`devAsynInt32.c:924-927`).
    MinusOne,
    /// `pr->udf = 1; return -2` — sourced nothing and declared the record
    /// undefined (`devAsynInt32.c:900-904`).
    MinusTwo,
}

/// Replays a scripted sequence of read outcomes, one per process cycle, and
/// stores `sourced` only on the cycles that produced a value — exactly as C's
/// `processAiAverage` leaves the record's fields untouched on both of its
/// negative returns.
struct ScriptedDevice {
    script: Arc<Mutex<Vec<CReturn>>>,
    /// `(field, value)` a producing read writes. `RVAL` for a C `return 0`
    /// that hands the record a raw reading to convert, `VAL` for a `return 2`
    /// that produced the engineering value itself.
    sourced: (&'static str, EpicsValue),
}

impl DeviceSupport for ScriptedDevice {
    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        let mut script = self.script.lock().unwrap();
        assert!(!script.is_empty(), "device read past the end of the script");
        let status = script.remove(0);
        Ok(match status {
            CReturn::Zero => {
                record.put_field(self.sourced.0, self.sourced.1.clone())?;
                DeviceReadOutcome::ok()
            }
            CReturn::TwoDefined => {
                record.put_field(self.sourced.0, self.sourced.1.clone())?;
                DeviceReadOutcome::computed(DeviceUdf::Defined)
            }
            CReturn::MinusOne => DeviceReadOutcome::failed(),
            CReturn::MinusTwo => DeviceReadOutcome::undefined(),
        })
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }

    fn dtyp(&self) -> &str {
        "scriptedRead"
    }
}

/// One record of `record_type` whose device support replays `script`, freshly
/// built so UDF starts at 1 — the state `iocInit` leaves behind and the state
/// asyn's empty-average arm re-asserts.
async fn build(
    record_type: &str,
    fields: &str,
    sourced: (&'static str, EpicsValue),
    script: Vec<CReturn>,
) -> Arc<PvDatabase> {
    let script = Arc::new(Mutex::new(script));
    let db =
        format!(r#"record({record_type}, "{REC}") {{ field(DTYP, "scriptedRead") {fields} }}"#);
    let (database, _) = IocBuilder::new()
        .register_dynamic_device_support(move |ctx: &DeviceSupportContext| {
            (ctx.dtyp == "scriptedRead").then(|| {
                Box::new(ScriptedDevice {
                    script: Arc::clone(&script),
                    sourced: sourced.clone(),
                }) as Box<dyn DeviceSupport>
            })
        })
        .db_string(&db, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    database
}

/// The ai the bulk of these cases use: a `return 2` device writing VAL.
async fn ai(script: Vec<CReturn>) -> Arc<PvDatabase> {
    build("ai", "", ("VAL", EpicsValue::Double(VALUE)), script).await
}

const VALUE: f64 = 42.0;
/// A raw reading a `return 0` device hands the record to convert. With the
/// default LINR/ASLO/AOFF/ROFF an ai's `convert()` is the identity, so VAL
/// lands on this number.
const RAW: i32 = 7;

async fn process(db: &Arc<PvDatabase>) {
    let mut visited = HashSet::new();
    db.process_record_with_links(REC, &mut visited, 0)
        .await
        .unwrap();
}

/// `(UDF, SEVR, STAT)` as a client reads them.
fn state(db: &Arc<PvDatabase>) -> (u8, AlarmSeverity, u16) {
    let rec = db.get_record(REC).unwrap();
    let inst = rec.read();
    (inst.common.udf, inst.common.sevr, inst.common.stat)
}

fn field(db: &Arc<PvDatabase>, name: &str) -> EpicsValue {
    db.get_record(REC)
        .unwrap()
        .read()
        .record
        .get_field(name)
        .unwrap_or_else(|| panic!("{name} exists"))
}

fn val(db: &Arc<PvDatabase>) -> f64 {
    match field(db, "VAL") {
        EpicsValue::Double(v) => v,
        other => panic!("VAL is not a double: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The producing returns: `0` and `2` both re-derive.
// ---------------------------------------------------------------------------

/// C `return 0`: `convert(prec)` runs and `if (status == 0) prec->udf =
/// isnan(prec->val)` clears UDF. The scripted device writes RVAL only, so this
/// also proves the record ran its own conversion rather than reading a VAL the
/// device never wrote.
#[epics_macros_rs::epics_test]
async fn a_raw_read_converts_and_rederives_udf() {
    let db = build(
        "ai",
        "",
        ("RVAL", EpicsValue::Long(RAW)),
        vec![CReturn::Zero],
    )
    .await;

    process(&db).await;

    let (udf, sevr, stat) = state(&db);
    assert_eq!(udf, 0, "a sourced raw reading defines the record");
    assert_eq!(sevr, AlarmSeverity::NoAlarm);
    assert_eq!(stat, alarm_status::NO_ALARM);
    assert_eq!(
        val(&db),
        RAW as f64,
        "C `return 0` runs convert(), so RVAL reaches VAL"
    );
}

/// C `return 2` is folded to `0` by `else if (status==2) status=0;` on the line
/// before the re-derive, so it clears UDF exactly like `return 0` — and the
/// UDF_ALARM/INVALID a fresh record carries goes with it.
#[epics_macros_rs::epics_test]
async fn a_computed_read_rederives_udf() {
    let db = ai(vec![CReturn::TwoDefined]).await;

    process(&db).await;

    let (udf, sevr, stat) = state(&db);
    assert_eq!(udf, 0, "a sourced computed reading defines the record");
    assert_eq!(sevr, AlarmSeverity::NoAlarm);
    assert_eq!(stat, alarm_status::NO_ALARM);
    assert_eq!(val(&db), VALUE);
}

/// The other side of the same gate, walked in sequence: a `-2` read does not
/// freeze UDF forever — the next computed read still clears it.
#[epics_macros_rs::epics_test]
async fn a_computed_device_read_still_re_derives_udf() {
    let db = ai(vec![CReturn::MinusTwo, CReturn::TwoDefined]).await;

    process(&db).await;
    assert_eq!(state(&db).0, 1);

    process(&db).await;
    assert_eq!(
        state(&db).0,
        0,
        "C folds `2` to `0` before the re-derive, so a computed read clears UDF"
    );
}

// ---------------------------------------------------------------------------
// The failing returns: `-1` and `-2` re-derive nothing.
// ---------------------------------------------------------------------------

/// C's empty-average arm. `processAiAverage` raises UDF_ALARM/INVALID, sets
/// `prec->udf = 1` and returns `-2`, so neither the convert nor the re-derive
/// runs and `recGblCheckUDF` leaves the record reporting itself undefined.
/// A client reading `.UDF` gets 1, not 0.
#[epics_macros_rs::epics_test]
async fn an_undefined_read_leaves_the_record_undefined() {
    let db = ai(vec![CReturn::MinusTwo]).await;

    process(&db).await;

    let (udf, sevr, stat) = state(&db);
    assert_eq!(
        udf, 1,
        "a cycle that sourced nothing cannot define the record"
    );
    assert_eq!(sevr, AlarmSeverity::Invalid);
    assert_eq!(stat, alarm_status::UDF_ALARM);
}

/// The transport-error arm (`devAsynInt32.c:924-927`) returns `-1` and says
/// nothing about UDF. It misses the same `if (status == 0)` gate, so an
/// already-undefined record stays undefined and keeps its UDF alarm.
#[epics_macros_rs::epics_test]
async fn a_failed_read_leaves_the_record_undefined() {
    let db = ai(vec![CReturn::MinusOne]).await;

    process(&db).await;

    let (udf, sevr, stat) = state(&db);
    assert_eq!(udf, 1);
    assert_eq!(sevr, AlarmSeverity::Invalid);
    assert_eq!(stat, alarm_status::UDF_ALARM);
}

/// The row: a `-2` read leaves the record undefined even though VAL holds a
/// perfectly good number from an earlier cycle. Before the fix the framework
/// re-derived `udf = value_is_undefined()` on this cycle and a client read 0.
#[epics_macros_rs::epics_test]
async fn an_undefined_device_read_keeps_udf_set() {
    let db = ai(vec![CReturn::TwoDefined, CReturn::MinusTwo]).await;

    process(&db).await;
    assert_eq!(
        state(&db).0,
        0,
        "a computed read re-derives UDF from a real VAL"
    );
    assert_eq!(val(&db), VALUE);

    process(&db).await;
    assert_eq!(
        state(&db).0,
        1,
        "C `return -2` sets udf and skips the re-derive (devAsynInt32.c:902, aiRecord.c:161)"
    );
    assert_eq!(
        val(&db),
        VALUE,
        "a -2 read must leave VAL at its previous value"
    );
}

/// `-1` and `-2` are not the same value. Both suppress the re-derive; only `-2`
/// asserts undefined. C's transport-error average branch returns `-1` and says
/// nothing about UDF, so a defined record must stay defined across it.
#[epics_macros_rs::epics_test]
async fn a_failed_device_read_leaves_udf_alone() {
    let db = ai(vec![
        CReturn::TwoDefined,
        CReturn::MinusOne,
        CReturn::MinusTwo,
        CReturn::MinusOne,
    ])
    .await;

    process(&db).await;
    assert_eq!(state(&db).0, 0);
    process(&db).await;
    assert_eq!(
        state(&db).0,
        0,
        "C `return -1` neither sets UDF nor re-derives it (devAsynInt32.c:924-927)"
    );

    process(&db).await;
    assert_eq!(state(&db).0, 1);
    process(&db).await;
    assert_eq!(
        state(&db).0,
        1,
        "a -1 read after a -2 read must not clear the undefined state either"
    );
}

/// A failing read stores nothing, so C's `monitor()` finds VAL unchanged
/// against MLST and posts no value event: a camonitor client sees the good
/// reading once and then silence, not a repeat or a zero. This is the half a
/// UDF-only assertion misses — a framework that re-published the stale VAL
/// every failed cycle would still pass every test above.
#[epics_macros_rs::epics_test]
async fn a_failing_read_posts_no_new_value() {
    let db = ai(vec![
        CReturn::TwoDefined,
        CReturn::MinusOne,
        CReturn::MinusTwo,
    ])
    .await;

    let mut sub: EventReader = {
        let rec = db.get_record(REC).unwrap();
        let mut inst = rec.write();
        inst.add_subscriber("VAL", 1, DbFieldType::Double, EventMask::VALUE.bits())
            .expect("VAL subscription accepted")
    };
    let drain = |rx: &mut EventReader| {
        let mut n = 0;
        while rx.try_recv().is_ok() {
            n += 1;
        }
        n
    };

    process(&db).await;
    assert_eq!(drain(&mut sub), 1, "the sourced reading posts once");

    process(&db).await;
    assert_eq!(drain(&mut sub), 0, "a -1 read publishes nothing");
    assert_eq!(val(&db), VALUE);

    process(&db).await;
    assert_eq!(drain(&mut sub), 0, "a -2 read publishes nothing");
    assert_eq!(val(&db), VALUE);
}

// ---------------------------------------------------------------------------
// The gate is not ai-only, and the record types disagree.
// ---------------------------------------------------------------------------

/// `longinRecord.c:148` is `if (status==0) prec->udf = FALSE;` — the same gate
/// as ai without the `isnan`. A failing read therefore leaves an undefined
/// longin undefined, UDF alarm and all.
#[epics_macros_rs::epics_test]
async fn a_failing_read_leaves_longin_undefined() {
    let db = build(
        "longin",
        "",
        ("VAL", EpicsValue::Long(RAW)),
        vec![CReturn::MinusOne, CReturn::Zero],
    )
    .await;

    process(&db).await;
    let (udf, sevr, stat) = state(&db);
    assert_eq!(udf, 1, "longin gates its UDF clear on status == 0");
    assert_eq!(sevr, AlarmSeverity::Invalid);
    assert_eq!(stat, alarm_status::UDF_ALARM);

    process(&db).await;
    assert_eq!(state(&db).0, 0, "a status-0 read defines it");
    assert_eq!(field(&db, "VAL"), EpicsValue::Long(RAW));
}

/// `stringinRecord.c::process` never assigns UDF at all — `devSiSoft.c`
/// clears it inside `read_stringin` — and the record names no UDF_ALARM. So a
/// failing read leaves UDF standing with NO alarm, which is a different
/// observable from ai's INVALID and must not be collapsed into it.
#[epics_macros_rs::epics_test]
async fn a_failing_read_leaves_stringin_undefined_without_an_alarm() {
    let db = build(
        "stringin",
        "",
        ("VAL", EpicsValue::String("hello".into())),
        vec![CReturn::MinusTwo, CReturn::TwoDefined],
    )
    .await;

    process(&db).await;
    let (udf, sevr, stat) = state(&db);
    assert_eq!(udf, 1);
    assert_eq!(
        sevr,
        AlarmSeverity::NoAlarm,
        "stringinRecord.c has no recGblCheckUdf, so UDF raises nothing"
    );
    assert_eq!(stat, alarm_status::NO_ALARM);

    process(&db).await;
    assert_eq!(state(&db).0, 0, "a sourced read still clears it");
}

/// The opposite boundary, and the reason the gate cannot simply be "a failed
/// read freezes UDF": `waveformRecord.c:143-144` runs `prec->pact = TRUE;
/// prec->udf = FALSE;` on the line after `readValue` returns, whatever it
/// returned. A `-2` read must therefore leave a waveform DEFINED — the
/// framework's `-2` UDF assertion is overwritten by the record's own
/// unconditional clear, exactly as in C.
#[epics_macros_rs::epics_test]
async fn a_failing_read_still_defines_a_waveform() {
    let db = build(
        "waveform",
        r#"field(FTVL,"DOUBLE") field(NELM,"4")"#,
        ("VAL", EpicsValue::DoubleArray(vec![1.0, 2.0])),
        vec![CReturn::MinusTwo, CReturn::MinusOne],
    )
    .await;

    process(&db).await;
    assert_eq!(
        state(&db).0,
        0,
        "waveform clears UDF unconditionally after readValue (waveformRecord.c:144)"
    );

    process(&db).await;
    assert_eq!(state(&db).0, 0, "and a -1 read is no different");
}
