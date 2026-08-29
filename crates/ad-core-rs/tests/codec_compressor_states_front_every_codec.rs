//! The `Compressor` records must front every codec the port implements.
//!
//! `ad-plugins-rs/src/codec.rs` writes `COMPRESSOR = CodecName::ordinal()` on
//! every decompress, and reads the operator's write back through
//! `CodecName::from_ordinal`. The enum spans seven codecs; the template shipped
//! at `db/NDCodec.template` was byte-identical to ADCore's at the pin, which
//! declares five. A record cannot represent an ordinal it has no state for, and
//! neither record layer treats that as an error:
//!
//! * reporting — an `RVAL` matching no `*VL` entry makes `VAL` 65535
//!   (`mbbiRecord.c:180`), and `UNSV` defaults to `NO_ALARM`
//!   (`mbbiRecord.dbd.pod:570-576`), so a Zlib or LZ4HDF5 readback was
//!   unreadable *and* unalarmed;
//! * selecting — `caput Compressor 5` indexes `FVVL`, which the five-state
//!   template never set and which has no `initial()`
//!   (`mbboRecord.dbd.pod:262-264`), so `RVAL` came back 0 and asking for Zlib
//!   silently selected no compression at all.
//!
//! Unfixed, this test fails twice for `Zlib` and twice for `LZ4HDF5`: the
//! forward half finds no state named after the codec, and the reverse half
//! reads `VAL` 65535 back.
#![cfg(feature = "ioc")]

use std::collections::HashMap;
use std::path::Path;

use ad_core_rs::codec::CodecName;
use epics_base_rs::server::record::Record;
use epics_base_rs::server::records::mbbi::MbbiRecord;
use epics_base_rs::server::records::mbbo::MbboRecord;
use epics_base_rs::types::EpicsValue;

/// `mbbi`/`mbbo` state strings and raw values, in the DBD order that gives
/// `fieldIndex - ZRST` its meaning (`mbbiRecord.dbd.pod:272-407`).
const ST: [&str; 16] = [
    "ZRST", "ONST", "TWST", "THST", "FRST", "FVST", "SXST", "SVST", "EIST", "NIST", "TEST", "ELST",
    "TVST", "TTST", "FTST", "FFST",
];
const VL: [&str; 16] = [
    "ZRVL", "ONVL", "TWVL", "THVL", "FRVL", "FVVL", "SXVL", "SVVL", "EIVL", "NIVL", "TEVL", "ELVL",
    "TVVL", "TTVL", "FTVL", "FFVL",
];

/// The state fields of the `NDCodec.template` record called `name`, read from
/// the template the crate actually ships rather than from a copy in the test —
/// a hand-copied state table would keep passing after the template regressed.
fn shipped_states(name: &str) -> HashMap<String, String> {
    let path = Path::new(ad_core_rs::AD_CORE_DIR)
        .join("db")
        .join("NDCodec.template");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{} must be readable: {e}", path.display()));

    let head = format!("\"{name}\")");
    let start = text
        .find(&head)
        .unwrap_or_else(|| panic!("{} declares no record {name}", path.display()));
    let body = &text[start..];
    let end = body
        .find("\n}")
        .unwrap_or_else(|| panic!("record {name} is unterminated"));

    body[..end]
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let inner = line.strip_prefix("field(")?.strip_suffix(')')?;
            let (field, value) = inner.split_once(',')?;
            let value = value.trim().trim_matches('"');
            Some((field.trim().to_string(), value.to_string()))
        })
        .collect()
}

/// Load one record's state table into a real record instance, so the round-trip
/// below runs through `mbboRecord`/`mbbiRecord` conversion rather than a mock.
fn load_states(rec: &mut dyn Record, states: &HashMap<String, String>) {
    for name in ST {
        if let Some(v) = states.get(name) {
            rec.put_field(name, EpicsValue::String(v.clone().into()))
                .unwrap_or_else(|e| panic!("put {name}: {e:?}"));
        }
    }
    for name in VL {
        if let Some(v) = states.get(name) {
            let raw: u32 = v
                .parse()
                .unwrap_or_else(|e| panic!("{name} = {v:?} is not a raw value: {e}"));
            rec.put_field(name, EpicsValue::ULong(raw))
                .unwrap_or_else(|e| panic!("put {name}: {e:?}"));
        }
    }
}

fn labels(rec: &dyn Record) -> Vec<String> {
    rec.enum_state_strings()
        .expect("mbbo/mbbi serve enum state strings")
        .iter()
        .map(|s| s.as_str_lossy().into_owned())
        .collect()
}

#[test]
fn compressor_records_front_every_codec_the_port_implements() {
    // The ordinals the port can put on the parameter. Asserting the round-trip
    // here means a codec added to `CodecName` without an ordinal is caught
    // before it is blamed on the template.
    let codecs: Vec<(i32, CodecName)> = (0..=6).map(|i| (i, CodecName::from_ordinal(i))).collect();
    for (ordinal, codec) in &codecs {
        assert_eq!(
            codec.ordinal(),
            *ordinal,
            "CodecName::from_ordinal({ordinal}) is {codec:?}, whose ordinal is {}",
            codec.ordinal()
        );
    }

    let mut mbbo = MbboRecord::default();
    load_states(&mut mbbo, &shipped_states("$(P)$(R)Compressor"));
    let mut mbbi = MbbiRecord::default();
    load_states(&mut mbbi, &shipped_states("$(P)$(R)Compressor_RBV"));

    let out_labels = labels(&mbbo);
    let rbv_labels = labels(&mbbi);

    for (ordinal, codec) in &codecs {
        // The template names states after the enum variants, which is also what
        // upstream ADCore calls them (`NDCodec.template` at 926bb4c8).
        let want = format!("{codec:?}");

        // Forward: an operator selects the codec by name, and the port must
        // read the ordinal that names that codec.
        let index = out_labels
            .iter()
            .position(|state| *state == want)
            .unwrap_or_else(|| {
                panic!("$(P)$(R)Compressor has no state named {want:?}; it offers {out_labels:?}")
            });
        mbbo.put_field("VAL", EpicsValue::Enum(index as u16))
            .expect("put VAL");
        mbbo.process().expect("process mbbo");
        assert_eq!(
            mbbo.get_field("RVAL"),
            Some(EpicsValue::ULong(*ordinal as u32)),
            "selecting {want:?} (state {index}) must put COMPRESSOR = {ordinal}"
        );

        // Reverse: the port reports the ordinal it decompressed, and the
        // readback must render it as that codec's name.
        mbbi.put_field("RVAL", EpicsValue::ULong(*ordinal as u32))
            .expect("put RVAL");
        mbbi.process().expect("process mbbi");
        let val = match mbbi.get_field("VAL") {
            Some(EpicsValue::Enum(v)) => v,
            other => panic!("mbbi VAL is {other:?}, not an enum"),
        };
        assert!(
            (val as usize) < rbv_labels.len(),
            "COMPRESSOR = {ordinal} ({want}) reads back VAL = {val}, which names no state \
             ({rbv_labels:?}); 65535 is mbbi's unknown-state marker"
        );
        assert_eq!(
            rbv_labels[val as usize], want,
            "COMPRESSOR = {ordinal} must read back as {want:?}"
        );
    }
}
