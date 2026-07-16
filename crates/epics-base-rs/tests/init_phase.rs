//! `INIT` is a phase, and every boundary of that phase is a C transition.
//!
//! Oracle, on the compiled softIoc (`record(ai,"P:AI"){}`, never processed):
//!
//! ```text
//! $ caget -t P:AI.INIT
//! 1
//! ```
//!
//! C sets `prec->init = TRUE` in `init_record` (`aiRecord.c:114`,
//! `aoRecord.c:120`), clears it at the end of EVERY `process` (`aiRecord.c:170`,
//! `aoRecord.c:237`), and sets it again on an `SPC_LINCONV` special — a put to
//! `LINR`, `EGUF` or `EGUL` (`aiRecord.c:187`, `aoRecord.c:254`). The port had
//! it backwards on `ai` ("has been primed", the inverse bit) and on `ao` set it
//! in `convert()` and never cleared it, so both served the wrong number and
//! `ao` served it wrong forever.
//!
//! The cases below are the boundaries of the phase, not a usage story:
//! constructed / initialised / after one process / after a second process /
//! after each SPC_LINCONV field.

use epics_base_rs::error::CaResult;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::ai::AiRecord;
use epics_base_rs::server::records::ao::AoRecord;
use epics_base_rs::types::EpicsValue;

const INITIAL: EpicsValue = EpicsValue::Short(1);
const CONVERTED: EpicsValue = EpicsValue::Short(0);

fn init(rec: &mut dyn Record) -> CaResult<()> {
    rec.init_record(0)
}

fn init_field(rec: &dyn Record) -> EpicsValue {
    rec.get_field("INIT").expect("INIT is a declared field")
}

#[test]
fn r21_ai_init_walks_the_phase() {
    let mut rec = AiRecord::default();
    assert_eq!(
        init_field(&rec),
        CONVERTED,
        "a calloc'd record no init_record has touched reads 0"
    );

    init(&mut rec).unwrap();
    assert_eq!(init_field(&rec), INITIAL, "init_record: C `init = TRUE`");

    rec.process().unwrap();
    assert_eq!(
        init_field(&rec),
        CONVERTED,
        "the first process clears the phase"
    );

    rec.process().unwrap();
    assert_eq!(
        init_field(&rec),
        CONVERTED,
        "and every process after it keeps it clear"
    );
}

#[test]
fn r21_ao_init_walks_the_phase() {
    let mut rec = AoRecord::default();
    assert_eq!(init_field(&rec), CONVERTED);

    init(&mut rec).unwrap();
    assert_eq!(init_field(&rec), INITIAL, "init_record: C `init = TRUE`");

    rec.process().unwrap();
    assert_eq!(
        init_field(&rec),
        CONVERTED,
        "ao clears the phase at the end of process, not inside convert()"
    );

    rec.process().unwrap();
    assert_eq!(init_field(&rec), CONVERTED);
}

/// Every `special(SPC_LINCONV)` field re-arms the phase, on both records — it is
/// the special that sets it, not the particular field written.
#[test]
fn r21_every_spc_linconv_put_re_arms_the_phase() {
    for field in ["LINR", "EGUF", "EGUL"] {
        let mut ai = AiRecord::default();
        init(&mut ai).unwrap();
        ai.process().unwrap();
        assert_eq!(init_field(&ai), CONVERTED);
        let v = if field == "LINR" {
            EpicsValue::Short(2)
        } else {
            EpicsValue::Double(7.25)
        };
        ai.put_field(field, v.clone()).unwrap();
        assert_eq!(
            init_field(&ai),
            INITIAL,
            "ai.{field} is special(SPC_LINCONV): C sets init = TRUE"
        );

        let mut ao = AoRecord::default();
        init(&mut ao).unwrap();
        ao.process().unwrap();
        assert_eq!(init_field(&ao), CONVERTED);
        ao.put_field(field, v).unwrap();
        assert_eq!(
            init_field(&ao),
            INITIAL,
            "ao.{field} is special(SPC_LINCONV): C sets init = TRUE"
        );
    }
}

/// The phase is what makes SMOO's first sample its own initial condition: C
/// seeds `prec->val = val` when `init` is TRUE (`aiRecord.c:441`), so the first
/// converted value is not blended with the pre-init VAL. The second sample IS
/// blended. `INIT` and the smoothing therefore cannot disagree — one bit drives
/// both.
#[test]
fn r21_the_initial_phase_is_smoos_initial_condition() {
    let mut rec = AiRecord::default();
    rec.val = 1000.0; // a stale VAL the filter must not blend into the first sample
    rec.smoo = 0.5;
    rec.rval = 10;
    init(&mut rec).unwrap();

    rec.process().unwrap();
    assert_eq!(rec.val, 10.0, "the initial conversion IS the initial value");

    rec.rval = 20;
    rec.process().unwrap();
    assert_eq!(
        rec.val, 15.0,
        "once converted, SMOO blends: 20*(1-0.5) + 10*0.5"
    );
}
