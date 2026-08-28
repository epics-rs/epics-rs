//! "I wrote VAL" and "I declared it defined" are two facts in C, and the
//! record's `process()` combines them differently per record type.
//!
//! A C dset returning 2 has ALWAYS written `prec->udf` itself first —
//! `devBiSoft.c:54-59`, `devMbbiSoft.c:55-60`, `devMbbiDirectSoft.c:55-60`,
//! `devBiDbState.c:67-70`, `devTimestamp.c:40-41` — and only then does the
//! record decide whether to write it again:
//!
//! | record                            | C `process()` after a `return 2`                       |
//! |-----------------------------------|--------------------------------------------------------|
//! | ai                                | re-derives: `else if (status==2) status=0;` sits BEFORE `if (status == 0) prec->udf = isnan(prec->val)` (`aiRecord.c:158-161`) |
//! | bi / mbbi / mbbiDirect            | does not: `prec->udf = FALSE` is INSIDE `if (status == 0)` and the fold is after it (`biRecord.c:136-141`, `mbbiRecord.c:168-193`, `mbbiDirectRecord.c:152-166`) |
//! | longin / int64in                  | does not: `if (status==0) prec->udf = FALSE;` with no fold at all (`longinRecord.c:148`, `int64inRecord.c:144`) |
//!
//! So on those five the dset is the ONLY writer of `udf` for a `return 2`
//! cycle, and a port that always re-derived on "the device computed" could
//! not reproduce them: it would silently define a record whose dset said
//! nothing. Each arm below is one boundary of that pair, not one story.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use epics_base_rs::error::CaResult;
use epics_base_rs::server::device_support::{DeviceReadOutcome, DeviceSupport, DeviceUdf};
use epics_base_rs::server::ioc_app::DeviceSupportContext;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::{AlarmSeverity, Record};
use epics_base_rs::types::EpicsValue;

const REC: &str = "TEST:UDF";

/// What the scripted dset does on its one read: whether it writes VAL, and
/// what it says about `prec->udf`. The two are independent here precisely
/// because they are independent in C.
#[derive(Clone, Copy)]
struct Step {
    wrote_val: bool,
    udf: DeviceUdf,
}

struct ScriptedDevice {
    step: Arc<Mutex<Option<Step>>>,
    sourced: EpicsValue,
}

impl DeviceSupport for ScriptedDevice {
    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        let step = self.step.lock().unwrap().take().expect("one read only");
        if step.wrote_val {
            record.put_field("VAL", self.sourced.clone())?;
            Ok(DeviceReadOutcome::computed(step.udf))
        } else {
            Ok(DeviceReadOutcome::no_value(step.udf))
        }
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }

    fn dtyp(&self) -> &str {
        "scriptedUdf"
    }
}

/// Build a fresh record of `record_type` — UDF starts at 1, the state
/// `iocInit` leaves behind — process it once through `step`, and return
/// `(UDF, SEVR, STAT)` as a client reads them.
async fn read_once(record_type: &str, sourced: EpicsValue, step: Step) -> (u8, AlarmSeverity, u16) {
    let cell = Arc::new(Mutex::new(Some(step)));
    // lsi sizes its VAL buffer from SIZV; the default would truncate the
    // scripted string to nothing and the arm below would prove nothing.
    let extra = if record_type == "lsi" {
        r#" field(SIZV, "64")"#
    } else {
        ""
    };
    let db = format!(r#"record({record_type}, "{REC}") {{ field(DTYP, "scriptedUdf"){extra} }}"#);
    let (database, _) = IocBuilder::new()
        .register_dynamic_device_support(move |ctx: &DeviceSupportContext| {
            (ctx.dtyp == "scriptedUdf").then(|| {
                Box::new(ScriptedDevice {
                    step: Arc::clone(&cell),
                    sourced: sourced.clone(),
                }) as Box<dyn DeviceSupport>
            })
        })
        .db_string(&db, &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    assert_eq!(
        database.get_record(REC).unwrap().read().common.udf,
        1,
        "a bare record starts undefined, or the arm below proves nothing"
    );
    let mut visited = HashSet::new();
    database
        .process_record_with_links(REC, &mut visited, 0)
        .await
        .unwrap();
    let rec = database.get_record(REC).unwrap();
    let inst = rec.read();
    (inst.common.udf, inst.common.sevr, inst.common.stat)
}

/// The five record types whose `process()` leaves UDF to the dset, each with
/// a VAL its own field type accepts.
fn dset_owns_udf_records() -> Vec<(&'static str, EpicsValue)> {
    vec![
        ("bi", EpicsValue::Enum(1)),
        ("mbbi", EpicsValue::Enum(1)),
        ("mbbiDirect", EpicsValue::Long(1)),
        ("longin", EpicsValue::Long(7)),
        ("int64in", EpicsValue::Int64(7)),
    ]
}

#[epics_macros_rs::epics_test]
async fn a_dset_that_wrote_val_and_declared_it_defined_defines_the_record() {
    // C `devBiSoft.c::readLocked`: `prec->udf = FALSE; … return 2`.
    for (ty, val) in dset_owns_udf_records() {
        let (udf, sevr, _) = read_once(
            ty,
            val,
            Step {
                wrote_val: true,
                udf: DeviceUdf::Defined,
            },
        )
        .await;
        assert_eq!(
            udf, 0,
            "{ty}: the dset's `prec->udf = FALSE` is the one that counts"
        );
        assert_eq!(sevr, AlarmSeverity::NoAlarm, "{ty}: defined ⇒ no UDF_ALARM");
    }
}

#[epics_macros_rs::epics_test]
async fn a_dset_that_wrote_val_without_declaring_it_leaves_these_records_undefined() {
    // The arm the port could not express before: `return 2` with no
    // `prec->udf` write. On these five nothing else writes it, so the record
    // stays undefined and `checkAlarms` raises UDF_ALARM at UDFS — even
    // though VAL now holds a perfectly good number.
    for (ty, val) in dset_owns_udf_records() {
        let (udf, sevr, stat) = read_once(
            ty,
            val,
            Step {
                wrote_val: true,
                udf: DeviceUdf::Untouched,
            },
        )
        .await;
        assert_eq!(
            udf, 1,
            "{ty}: `process()` must not re-derive UDF on a computed read"
        );
        assert_eq!(sevr, AlarmSeverity::Invalid, "{ty}: UDF ⇒ INVALID at UDFS");
        assert_eq!(stat, alarm_status::UDF_ALARM, "{ty}");
    }
}

#[epics_macros_rs::epics_test]
async fn ai_rederives_on_the_same_undeclared_computed_read() {
    // Same dset statement, opposite record rule — which is the point of
    // making it per-record. `aiRecord.c:159` folds the 2 into a 0 before
    // `:161`, so ai re-derives from VAL and the record ends DEFINED.
    let (udf, sevr, _) = read_once(
        "ai",
        EpicsValue::Double(42.0),
        Step {
            wrote_val: true,
            udf: DeviceUdf::Untouched,
        },
    )
    .await;
    assert_eq!(udf, 0, "ai re-derives `udf = isnan(val)` = 0");
    assert_eq!(sevr, AlarmSeverity::NoAlarm);
}

#[epics_macros_rs::epics_test]
async fn a_dset_can_declare_the_record_defined_without_writing_val() {
    // The other diagonal: no value sourced, but the dset asserts the record
    // is defined. C reaches this shape wherever a dset writes `prec->udf`
    // and returns a status the record's gate rejects — `devTimestamp.c:61-63`
    // is the mirror image (`prec->udf = TRUE; … return -1`). Nothing
    // re-derives afterwards, so the dset's word stands.
    for (ty, val) in dset_owns_udf_records() {
        let _ = val;
        let (udf, sevr, _) = read_once(
            ty,
            EpicsValue::Enum(0),
            Step {
                wrote_val: false,
                udf: DeviceUdf::Defined,
            },
        )
        .await;
        assert_eq!(udf, 0, "{ty}: the dset declared it defined");
        assert_eq!(sevr, AlarmSeverity::NoAlarm, "{ty}");
    }
}

#[epics_macros_rs::epics_test]
async fn a_dset_that_sourced_nothing_and_said_nothing_leaves_udf_alone() {
    // C `return -1` with no `prec->udf` write — `devAsynInt32.c:924-927`.
    // The record was undefined going in and stays undefined.
    for (ty, val) in dset_owns_udf_records() {
        let _ = val;
        let (udf, sevr, stat) = read_once(
            ty,
            EpicsValue::Enum(0),
            Step {
                wrote_val: false,
                udf: DeviceUdf::Untouched,
            },
        )
        .await;
        assert_eq!(udf, 1, "{ty}: nothing sourced, nothing declared");
        assert_eq!(sevr, AlarmSeverity::Invalid, "{ty}");
        assert_eq!(stat, alarm_status::UDF_ALARM, "{ty}");
    }
}

/// The string-valued input records whose `process()` likewise never writes
/// UDF on the device path — the only `prec->udf = FALSE` in each is the
/// simulated SIOL branch (`stringinRecord.c:211`, `lsiRecord.c:245`,
/// `eventRecord.c:191`). None of them raises UDF_ALARM either, so UDF here
/// is a silent flag and the assertion has to read it directly.
fn dset_owns_udf_string_records() -> Vec<(&'static str, EpicsValue)> {
    vec![
        ("stringin", EpicsValue::String("ok".into())),
        ("lsi", EpicsValue::String("ok".into())),
        ("event", EpicsValue::String("ok".into())),
    ]
}

#[epics_macros_rs::epics_test]
async fn a_dset_that_stored_a_string_without_declaring_it_leaves_udf_set() {
    // `devAsynOctet.c::callbackSiRead:918-932` is the live shape: `readIt`
    // stores into `psi->val` whatever happens, but `psi->udf = 0` runs only
    // inside `if (status == asynSuccess)`. A partial read on a transport
    // error therefore leaves a truncated VAL in an UNDEFINED record.
    for (ty, val) in dset_owns_udf_string_records() {
        let (udf, sevr, _) = read_once(
            ty,
            val,
            Step {
                wrote_val: true,
                udf: DeviceUdf::Untouched,
            },
        )
        .await;
        assert_eq!(
            udf, 1,
            "{ty}: `process()` must not define what the dset did not"
        );
        assert_eq!(sevr, AlarmSeverity::NoAlarm, "{ty}: no UDF_ALARM on these");
    }
}

#[epics_macros_rs::epics_test]
async fn a_dset_that_stored_a_string_and_declared_it_defines_the_record() {
    // The success half of the same callback: `status == asynSuccess`, so
    // `psi->udf = 0` runs and the stored value is a defined reading.
    for (ty, val) in dset_owns_udf_string_records() {
        let (udf, sevr, _) = read_once(
            ty,
            val,
            Step {
                wrote_val: true,
                udf: DeviceUdf::Defined,
            },
        )
        .await;
        assert_eq!(udf, 0, "{ty}: the dset declared it defined");
        assert_eq!(sevr, AlarmSeverity::NoAlarm, "{ty}");
    }
}
