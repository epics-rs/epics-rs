//! Invariant: the `special(SPC_NOMOD)` declaration of every record type the
//! port vendors a `.dbd` for is **the `.dbd`**, and nothing else.
//!
//! C reads that one declaration in two unrelated places — `dbPut`
//! (`dbAccess.c:123-126`) refuses the write, and `rsrvCheckPut`
//! (`rsrv/camessage.c:2608-2619`) clears the `CA_PROTO_ACCESS_RIGHTS` write bit
//! so the client never sends the doomed put. The port routes both through
//! `RecordInstance::is_no_mod`, so `is_no_mod` is the thing this file pins.
//!
//! It used to read `Record::field_list()`, and six record types (`aSub`,
//! `mbbiDirect`, `mbboDirect`, `sseq`, `swait`, `waveform`) hand-wrote that
//! table through `FieldDesc::new` rather than taking the generated `.dbd`
//! transcription. Every one of them under-declared SPC_NOMOD, so the CA server
//! advertised `read, write` on fields the C IOC serves as `read, no write`.
//! Those hand tables are gone (see `one_declaration_per_record_type.rs`);
//! this file keeps walking the boundary they used to break. Measured on the
//! built C `softIoc` (7.0.10.1-DEV):
//!
//! ```text
//! cainfo T:MBBOD.RVAL   Access: read, no write     port: read, write
//! cainfo T:MBBOD.MASK   Access: read, no write     port: read, write
//! cainfo T:MBBOD.NOBT   Access: read, no write     port: read, write
//! cainfo T:ASUB.NOA     Access: read, no write     port: read, write
//! cainfo T:ASUB.FTA     Access: read, no write     port: read, write
//! cainfo T:SA.NELM      Access: read, write        <- subArray NELM is NOT
//!                                                     SPC_NOMOD; MALM is.
//! ```
//!
//! The boundary this test walks is per-field, not per-record: for every field a
//! vendored `.dbd` declares, `is_no_mod` must answer exactly what that `.dbd`
//! says — `true` on the NOMOD side of the boundary and `false` on the writable
//! side. A hand-written table that disagrees can no longer change the answer.

use epics_base_rs::server::db_loader::create_record;
use epics_base_rs::server::record::{RecordInstance, dbd_generated};

/// Fields whose no-modify-ness is NOT a static `.dbd` fact and so are exempt
/// from the equality: `Record::field_no_mod` raises SPC_NOMOD from record state
/// the way C's `cvt_dbaddr` does (compress `VAL` under `BALG=LIFO`,
/// `compressRecord.c:398-407`). Those are covered by
/// `epics-ca-rs/tests/access_rights_spc_nomod.rs`.
fn state_raised_nomod(record_type: &str, field: &str) -> bool {
    matches!((record_type, field), ("compress", "VAL"))
}

#[test]
fn is_no_mod_answers_the_dbd_for_every_declared_field() {
    let mut checked = 0usize;
    let mut nomod = 0usize;

    for &record_type in dbd_generated::RECORD_TYPES {
        let Ok(record) = create_record(record_type) else {
            panic!("{record_type}: the port declares a .dbd but cannot instantiate the record");
        };
        let instance = RecordInstance::new_boxed(format!("T:{record_type}"), record);
        let declared = dbd_generated::record_fields(record_type)
            .unwrap_or_else(|| panic!("{record_type} is in RECORD_TYPES but has no field table"));

        for desc in declared {
            if state_raised_nomod(record_type, desc.name) {
                continue;
            }
            assert_eq!(
                instance.is_no_mod(desc.name),
                desc.read_only,
                "{record_type}.{}: .dbd says special(SPC_NOMOD)={}, is_no_mod() says {}",
                desc.name,
                desc.read_only,
                instance.is_no_mod(desc.name),
            );
            checked += 1;
            nomod += usize::from(desc.read_only);
        }
    }

    // The walk must actually have crossed the boundary in both directions — a
    // table that silently emptied would otherwise pass vacuously.
    assert!(checked > 1000, "only {checked} declared fields walked");
    assert!(nomod > 100, "only {nomod} SPC_NOMOD fields walked");
}

/// The six record types that used to hand-write `field_list()`, named. Each
/// pair is one field the C IOC serves `read, no write` and one the C IOC serves
/// `read, write` — the boundary, per record type, measured with `cainfo`.
#[test]
fn hand_written_field_tables_cannot_under_declare_no_modify() {
    // (record type, SPC_NOMOD field, writable field)
    const BOUNDARY: &[(&str, &str, &str)] = &[
        ("mbboDirect", "RVAL", "VAL"),
        ("mbboDirect", "MASK", "OMSL"),
        ("mbboDirect", "NOBT", "SHFT"),
        ("mbbiDirect", "MASK", "SHFT"),
        ("mbbiDirect", "NOBT", "VAL"),
        ("aSub", "NOA", "VALA"),
        ("aSub", "FTA", "SNAM"),
        ("aSub", "OVAL", "VAL"),
        ("waveform", "NELM", "RARM"),
        ("waveform", "FTVL", "SIML"),
        ("waveform", "BUSY", "SIMM"),
        ("subArray", "MALM", "NELM"),
        ("subArray", "FTVL", "INDX"),
        ("sseq", "BUSY", "SELM"),
        ("swait", "INIT", "CALC"),
    ];

    for &(record_type, no_mod, writable) in BOUNDARY {
        let record = create_record(record_type).expect("record type is implemented");
        let instance = RecordInstance::new_boxed(format!("T:{record_type}"), record);
        assert!(
            instance.is_no_mod(no_mod),
            "{record_type}.{no_mod} is special(SPC_NOMOD) in the .dbd — the C IOC serves it \
             `read, no write`"
        );
        assert!(
            !instance.is_no_mod(writable),
            "{record_type}.{writable} is writable in the .dbd — the C IOC serves it `read, write`"
        );
    }
}
