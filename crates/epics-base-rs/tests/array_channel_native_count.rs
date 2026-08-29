//! B3 — `ca_element_count` is the field's DECLARED capacity, fixed for the
//! channel's lifetime; the live length is what a GET returns.
//!
//! C splits the two across `cvt_dbaddr` and `get_array_info`:
//!
//! | record   | `cvt_dbaddr no_elements`   | `get_array_info *no_elements` |
//! |----------|----------------------------|-------------------------------|
//! | waveform | `NELM` (:183)              | `NORD` (:196)                 |
//! | subArray | `MALM` (:168)              | `NORD` (:184)                 |
//! | aSub     | `NOx` / `NOVx` (:486,:494) | `NEx` / `NEVx` (:515,:519)    |
//! | compress | `NSAM` (:399)              | `NUSE` (:428)                 |
//! | aCalcout | `NELM` under SIZE=NELM (aCalcoutRecord.c:630) | the NUSE window (:672) |
//!
//! `dbChannelElements` (`dbChannel.h:410`) is the FIXED `no_elements`, so a
//! client's `ca_element_count` never moves once the channel exists. The port
//! implemented `Record::field_native_count` only for the waveform family, so
//! aSub / compress / aCalcout announced the current value's length instead:
//! `cainfo X.VALA` read 3 where C reads 10, `caget -# 10 X.VALA` was refused,
//! and the announced count CHANGED across the channel's life (NOVA before the
//! first process, NEVA after) — which the CA create-channel contract does not
//! permit.
//!
//! Deliberately NOT in this family:
//!
//! * `histogram` — C sets both to `NELM` (`histogramRecord.c:303`, `:315`);
//!   there is no split.
//! * `lsi` / `lso` / `printf` — C's `cvt_dbaddr` sets `no_elements = 1` and
//!   `field_type = DBF_STRING` (`lsiRecord.c:141-143`): a long string is ONE
//!   string element on the channel, not a capacity. The CA server already
//!   serves that through `Record::long_string_fields`.
//! * `sCalcout` — `no_elements = STRING_SIZE`, a constant, with no
//!   `get_array_info` to disagree with it.

use epics_base_rs::server::record::{FieldDeclaration, Record};
use epics_base_rs::server::records::acalcout::AcalcoutRecord;
use epics_base_rs::server::records::asub_record::ASubRecord;
use epics_base_rs::server::records::compress::CompressRecord;
use epics_base_rs::types::EpicsValue;

/// aSub: `NOx` for `A..U`, `NOVx` for `VALA..VALU`, and unchanged by a
/// delivery that fills fewer elements.
#[test]
fn asub_channels_announce_nox_and_novx() {
    let mut rec = ASubRecord::default();
    rec.put_field("FTA", EpicsValue::Short(10)).unwrap(); // DOUBLE
    rec.put_field("NOA", EpicsValue::Long(8)).unwrap();
    rec.put_field("FTVA", EpicsValue::Short(10)).unwrap();
    rec.put_field("NOVA", EpicsValue::Long(10)).unwrap();

    assert_eq!(rec.field_native_count("A"), Some(8));
    assert_eq!(rec.field_native_count("VALA"), Some(10));

    // A subroutine writes 3 elements; NEVA follows, the announced count must
    // not.
    rec.put_field("VALA", EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0]))
        .unwrap();
    assert_eq!(rec.get_field("NEVA"), Some(EpicsValue::Long(3)));
    assert_eq!(
        rec.field_native_count("VALA"),
        Some(10),
        "C cvt_dbaddr pins no_elements at NOVA; only get_array_info moves"
    );

    // An input link delivers 2 elements into an 8-wide cell.
    rec.put_field("A", EpicsValue::DoubleArray(vec![4.0, 5.0]))
        .unwrap();
    assert_eq!(rec.get_field("NEA"), Some(EpicsValue::Long(2)));
    assert_eq!(rec.field_native_count("A"), Some(8));

    // The count fields and the scalars are not SPC_DBADDR channels.
    assert_eq!(rec.field_native_count("NEA"), None);
    assert_eq!(rec.field_native_count("NOVA"), None);
    assert_eq!(rec.field_native_count("SNAM"), None);
}

/// compress: `NSAM`, not the `NUSE` elements `linearise_val` serves.
#[test]
fn compress_val_announces_nsam() {
    // Circular Buffer (ALG 4) so each push lands one sample in the buffer.
    let mut rec = CompressRecord::new(10, 4);
    assert_eq!(rec.field_native_count("VAL"), Some(10));

    rec.push_value(1.0);
    rec.push_value(2.0);
    rec.push_value(3.0);
    assert_eq!(rec.get_field("NUSE"), Some(EpicsValue::ULong(3)));
    assert_eq!(
        rec.get_field("VAL"),
        Some(EpicsValue::DoubleArray(vec![1.0, 2.0, 3.0])),
        "get_array_info serves NUSE elements"
    );
    assert_eq!(
        rec.field_native_count("VAL"),
        Some(10),
        "compressRecord.c:399 pins no_elements at NSAM"
    );
    assert_eq!(rec.field_native_count("NUSE"), None);
}

/// aCalcout: `NELM` under the default `SIZE = NELM`, and the NUSE window under
/// `SIZE = NUSE` — the one setting that makes C's two numbers agree.
#[test]
fn acalcout_arrays_announce_the_size_gated_capacity() {
    let mut rec = AcalcoutRecord::default();
    rec.put_field("NELM", EpicsValue::ULong(10)).unwrap();
    rec.put_field("NUSE", EpicsValue::ULong(3)).unwrap();

    for f in ["AA", "LL", "AVAL", "OAV"] {
        assert_eq!(
            rec.field_native_count(f),
            Some(10),
            "{f}: SIZE=NELM announces the whole buffer (aCalcoutRecord.c:630)"
        );
    }
    assert_eq!(
        rec.get_field("AVAL").map(|v| v.count()),
        Some(3),
        "get_array_info serves the NUSE window"
    );

    rec.put_field("SIZE", EpicsValue::Short(1)).unwrap(); // NUSE
    for f in ["AA", "AVAL", "OAV"] {
        assert_eq!(
            rec.field_native_count(f),
            Some(3),
            "{f}: SIZE=NUSE narrows the channel to the window (:624)"
        );
    }

    assert_eq!(rec.field_native_count("VAL"), None, "VAL is a scalar");
    assert_eq!(rec.field_native_count("NUSE"), None);
}
