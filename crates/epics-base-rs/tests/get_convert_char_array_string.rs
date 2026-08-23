//! A `DBF_CHAR` waveform read as `DBR_STRING` is `getCharString`
//! (`dbConvert.c:417-437`), one 40-byte slot per element:
//!
//! ```c
//! while (nRequest--) {
//!     cvtCharToString(*psrc, pdst);
//!     pdst += MAX_STRING_SIZE;
//!     ...
//! }
//! ```
//!
//! `cvtCharToString` is `cvtInt32ToString` (`cvtFast.h:71`), so a waveform
//! holding the bytes `A`..`J` serves the ten decimal strings `"65"`..`"74"` in
//! `10 * 40` bytes — not one slot holding `"ABCDEFGHIJ"`, which is the
//! long-string presentation and belongs to `apply_native_long_string`. The
//! collapsed form left nine elements absent under a header still declaring ten,
//! so a client indexed 400 bytes into a 40-byte body.
//!
//! `DBF_UCHAR` is the same routine through `cvtUInt32ToString` (`cvtFast.h:72`),
//! which is why both signs are pinned here: `0xFF` reads `-1` from a CHAR
//! waveform and `255` from a UCHAR one.

use std::time::SystemTime;

use epics_base_rs::server::snapshot::Snapshot;
use epics_base_rs::types::{DbFieldType, EpicsValue, PvString, encode_dbr};

const DBR_STRING: u16 = 0;
const MAX_STRING_SIZE: usize = 40;

fn strs(v: &[&str]) -> EpicsValue {
    EpicsValue::StringArray(v.iter().map(|s| PvString::from(s.to_string())).collect())
}

#[test]
fn char_array_get_converts_one_string_slot_per_element() {
    let wf = EpicsValue::CharArray(b"ABCDEFGHIJ".to_vec());
    assert_eq!(
        wf.get_convert(DbFieldType::String).unwrap(),
        strs(&["65", "66", "67", "68", "69", "70", "71", "72", "73", "74"])
    );
}

#[test]
fn char_array_element_is_signed_and_uchar_is_not() {
    assert_eq!(
        EpicsValue::CharArray(vec![0xFF, 0x80, 0x7F])
            .get_convert(DbFieldType::String)
            .unwrap(),
        strs(&["-1", "-128", "127"])
    );
    assert_eq!(
        EpicsValue::UCharArray(vec![0xFF, 0x80, 0x7F])
            .get_convert(DbFieldType::String)
            .unwrap(),
        strs(&["255", "128", "127"])
    );
}

#[test]
fn dbr_string_payload_is_forty_bytes_per_element() {
    let snap = Snapshot::new(
        EpicsValue::CharArray(b"ABCDEFGHIJ".to_vec()),
        0,
        0,
        SystemTime::UNIX_EPOCH,
    );
    let data = encode_dbr(DBR_STRING, &snap).unwrap();
    assert_eq!(
        data.len(),
        10 * MAX_STRING_SIZE,
        "ten elements, one MAX_STRING_SIZE slot each"
    );
    for (i, want) in ["65", "66", "67", "68", "69", "70", "71", "72", "73", "74"]
        .iter()
        .enumerate()
    {
        let slot = &data[i * MAX_STRING_SIZE..(i + 1) * MAX_STRING_SIZE];
        let end = slot.iter().position(|&b| b == 0).unwrap_or(slot.len());
        assert_eq!(
            std::str::from_utf8(&slot[..end]).unwrap(),
            *want,
            "slot {i}"
        );
    }
}

#[test]
fn the_whole_buffer_projection_is_still_reachable_by_its_own_name() {
    // `convert_to` keeps the put/projection contract — the long-string
    // presentation depends on it — so the split has to leave it alone.
    assert_eq!(
        EpicsValue::CharArray(b"ABCDEFGHIJ".to_vec()).convert_to(DbFieldType::String),
        EpicsValue::String(PvString::from("ABCDEFGHIJ".to_string()))
    );
}
