//! `motor.CARD` is a function of the OUT link's TYPE, decided once at
//! `init_record`.
//!
//! ```c
//! /* motorRecord.cc:653-670 */
//! switch (pmr->out.type)
//! {
//!     case (VME_IO):
//!         pmr->card = pmr->out.value.vmeio.card;
//!         break;
//!     case (CONSTANT):
//!     case (PV_LINK):
//!     case (DB_LINK):
//!     case (CA_LINK):
//!         pmr->card = -1;
//!         break;
//!     case (INST_IO):
//!         pmr->card = 0;
//!         break;
//!     default:
//!         recGblRecordError(S_db_badField, (void *) pmr, (char *) errmsg);
//!         return(ERROR);
//! }
//! ```
//!
//! The port had no CARD at all: `dbd_generated.rs` declared the field and
//! `motor_get_field` served nothing for it, so every motor answered with the
//! framework's fallback — the same 0 C reserves for INST_IO. A soft motor
//! (`OUT` unset, C's CONSTANT) therefore reported a real controller address
//! where C reports -1.

use epics_base_rs::server::record::Record;
use epics_base_rs::types::EpicsValue;
use motor_rs::MotorRecord;

fn card_for(out: &str) -> i16 {
    let mut rec = MotorRecord::new();
    rec.put_field("OUT", EpicsValue::String(out.into()))
        .unwrap();
    rec.init_record(0).unwrap();
    rec.init_record(1).unwrap();
    match rec.get_field("CARD") {
        Some(EpicsValue::Short(c)) => c,
        other => panic!("CARD served {other:?}, not a DBF_SHORT"),
    }
}

/// `case (INST_IO): pmr->card = 0;` — the `@…` form every asyn-based motor
/// driver uses.
#[test]
fn an_inst_io_out_is_card_zero() {
    assert_eq!(card_for("@asyn(MOTOR,0)"), 0);
    assert_eq!(card_for("@"), 0);
}

/// `case (VME_IO): pmr->card = pmr->out.value.vmeio.card;` — the number after
/// `#C`, not the signal and not a constant. `dbParseLink` scans it with `%i`
/// (`dbStaticLib.c:2301`), so the hex spelling is the same card.
#[test]
fn a_vme_io_out_carries_its_own_card_number() {
    assert_eq!(card_for("#C0 S0"), 0);
    assert_eq!(card_for("#C3 S1 @parm"), 3);
    assert_eq!(card_for("#C15 S14"), 15);
    assert_eq!(card_for("#C0x10 S0"), 16, "%i reads 0x10 as hex");
}

/// `case (CONSTANT):` — an unset OUT is a CONSTANT link in C, and the answer
/// is -1, not the INST_IO 0 the port used to give it.
#[test]
fn a_soft_motor_is_card_minus_one() {
    assert_eq!(card_for(""), -1);
    assert_eq!(card_for("5"), -1, "a constant OUT is still CONSTANT");
}

/// `case (PV_LINK): case (DB_LINK): case (CA_LINK):` — every link that names a
/// PV rather than hardware.
#[test]
fn a_pv_named_out_is_card_minus_one() {
    assert_eq!(card_for("OTHER.VAL"), -1);
    assert_eq!(card_for("OTHER PP NMS"), -1);
    assert_eq!(card_for("OTHER.VAL CA"), -1);
    assert_eq!(card_for("pva://OTHER"), -1);
}

/// A `#` link whose card number cannot be read is C's `default:` arm — C
/// refuses the record outright, which this port has no way to do from
/// `init_record`; -1 (unknown hardware) is the closest available answer, and
/// the one thing it must not be is 0.
#[test]
fn an_unreadable_hardware_out_is_not_card_zero() {
    assert_eq!(card_for("#B0 C1 N2 A3 F4"), -1);
    assert_eq!(card_for("#Cx S0"), -1);
}

/// The channel a CA client opens, which is where the old answer came from:
/// with no `get_field` arm, `RecordInstance::resolve_field` fell through to
/// `declared_default`, and CARD's `FieldDesc` carries `initial: None` — so the
/// last resort served a type-zero `Short(0)`, C's INST_IO answer, to a soft
/// motor.
#[test]
fn the_client_channel_carries_the_derived_card() {
    use epics_base_rs::server::record::RecordInstance;

    let mut rec = MotorRecord::new();
    rec.init_record(0).unwrap();
    rec.init_record(1).unwrap();
    let inst = RecordInstance::new("M".to_string(), rec);
    assert_eq!(
        inst.client_field_value("CARD"),
        Some(EpicsValue::Short(-1)),
        "a soft motor's OUT is CONSTANT (motorRecord.cc:658-662)"
    );
}
