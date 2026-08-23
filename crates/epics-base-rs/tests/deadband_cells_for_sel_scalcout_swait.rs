// RTEMS-EXEC-MODEL-ALLOW(1): a sync test that touches no runtime.
//! `sel`, `scalcout` and `swait` must carry the MLST/ALST cells their C
//! records have.
//!
//! All three deadband VAL in C — `selRecord.c:316,319`,
//! `sCalcoutRecord.c:828,837`, `swaitRecord.c:630,639` — against `prec->mlst`
//! and `prec->alst`, which their `init_record` leaves at 0. The port declared
//! both fields in the DBD but served neither, so the framework had nowhere to
//! remember the last posted value: every cycle was the first one, and a record
//! sitting at its initial 0.0 posted a change that C does not make.
//!
//! `ai` is here as the control — it has always carried the cells, so it pins
//! that these cases measure the new cells rather than a framework-wide change.

use epics_base_rs::server::record::RecordInstance;
use epics_base_rs::server::records::{
    ai::AiRecord, scalcout::ScalcoutRecord, sel::SelRecord, swait::SwaitRecord,
};
use epics_base_rs::types::EpicsValue;

fn instance(rtype: &str) -> RecordInstance {
    match rtype {
        "sel" => RecordInstance::new("D:SEL".into(), SelRecord::default()),
        "scalcout" => RecordInstance::new("D:SCALC".into(), ScalcoutRecord::default()),
        "swait" => RecordInstance::new("D:SWAIT".into(), SwaitRecord::default()),
        "ai" => RecordInstance::new("D:AI".into(), AiRecord::default()),
        other => panic!("unknown case {other}"),
    }
}

const CASES: [&str; 4] = ["sel", "scalcout", "swait", "ai"];

fn set_val(inst: &mut RecordInstance, v: f64) {
    inst.record
        .put_field("VAL", EpicsValue::Double(v))
        .expect("VAL is a double on every case record");
}

#[test]
fn the_cells_the_dbd_declares_are_served_as_doubles() {
    for rtype in CASES {
        let inst = instance(rtype);
        for field in ["MLST", "ALST"] {
            match inst.record.get_field(field) {
                Some(EpicsValue::Double(v)) => assert_eq!(
                    v, 0.0,
                    "{rtype}.{field}: C leaves it at the calloc'd 0 until the first post"
                ),
                other => panic!("{rtype}.{field} is not a DBF_DOUBLE cell: {other:?}"),
            }
        }
    }
}

/// The boundary the missing cell hid: a record still holding the 0.0 it
/// started at has not moved, and C's `delta > deadband` is `0 > 0` — false.
#[test]
fn an_unmoved_initial_zero_does_not_post() {
    for rtype in CASES {
        let mut inst = instance(rtype);
        assert_eq!(
            inst.check_deadband_ext(),
            (false, false),
            "{rtype}: VAL=0 against MLST=0 with MDEL=0 is not a change"
        );
    }
}

/// The other side of the same boundary — a nonzero first value does cross it,
/// so the fix must not turn the first post off wholesale.
#[test]
fn a_nonzero_first_value_posts_once_and_then_settles() {
    for rtype in CASES {
        let mut inst = instance(rtype);
        set_val(&mut inst, 3.0);
        assert_eq!(
            inst.check_deadband_ext(),
            (true, true),
            "{rtype}: |0 - 3| crosses a zero deadband"
        );
        assert_eq!(
            inst.check_deadband_ext(),
            (false, false),
            "{rtype}: the same 3.0 on the next cycle has not moved"
        );
    }
}

/// The post is what writes the cell — C only assigns `*poldval = newval`
/// inside the `delta > deadband` branch, so a cycle that does not post must
/// leave the cell where it was.
#[test]
fn only_a_posting_cycle_writes_the_cell() {
    for rtype in CASES {
        let mut inst = instance(rtype);
        set_val(&mut inst, 3.0);
        let _ = inst.check_deadband_ext();
        for field in ["MLST", "ALST"] {
            assert_eq!(
                inst.record.get_field(field),
                Some(EpicsValue::Double(3.0)),
                "{rtype}.{field} after a posting cycle"
            );
        }

        set_val(&mut inst, 3.0);
        let _ = inst.check_deadband_ext();
        for field in ["MLST", "ALST"] {
            assert_eq!(
                inst.record.get_field(field),
                Some(EpicsValue::Double(3.0)),
                "{rtype}.{field} after a non-posting cycle"
            );
        }
    }
}
