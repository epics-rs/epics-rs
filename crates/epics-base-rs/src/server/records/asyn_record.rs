use epics_macros_rs::EpicsRecord;

/// The `asyn` record — its full CA field surface (76 own fields from
/// `asynRecord.dbd`) is a faithful port, but held the way this database holds
/// every declared-but-unmodeled config field, NOT as 76 struct members.
///
/// # Where the field surface lives
///
/// A record type in this port is declared by its `.dbd` and by nothing else:
/// the generated [`ASYN_FIELDS`](crate::server::record::dbd_generated) table
/// carries every asyn field with its C `DBF` type, `menu(...)`, `size(N)`,
/// `initial(...)`, `special`, `pp`, and `interest` — the exact contract
/// `asynRecord.dbd` writes. `Record::field_list` resolves `"asyn"` to that
/// table, so the CA create-channel, DBR type, and enum-label layers all see
/// the correct declaration for all 76 fields without this struct naming one.
///
/// # Where the field *values* live
///
/// A field the `.dbd` DECLARES but this struct does not model is served and
/// stored by the database's generic mechanism, the same one `dfanout.HOPR` and
/// `aSub.OVAL` ride:
///
/// * **read** — `RecordInstance::resolve_field` falls through to
///   `declared_default`, which returns the field's `initial(...)` (or the
///   declared type's zero), rendering a `menu()` field as `EpicsValue::Enum`
///   with its `.dbd` choices — so `TMOD`/`IFACE`/`BAUD`/… serve as `DBR_ENUM`
///   with the right labels, `TMOT` as `1.0`, `OMAX`/`IMAX`/`NOWT` as `80`,
///   `UI32MASK` as `0xffffffff`.
/// * **write** — `put_common_field`'s catch-all lands the put in the
///   per-instance declared-override store after coercing it to the field's
///   declared type via `coerce_put_value`: a menu label resolves to its
///   ordinal, a `DBF_STRING` truncates to `size - 1`, a numeric parses with
///   C's range rules. A later read reflects the stored value.
///
/// So the entire *config* surface — every field a client can set and read back
/// with no live asyn port — is already a faithful port through the declaration
/// table plus that generic store. Modeling the 76 fields as struct members
/// would only duplicate the declaration the `.dbd` owns (and is exactly what
/// once typed each `DBF_MENU` field as a bare integer with no `menu()`); the
/// member types the *storage*, the `.dbd` types the *field*.
///
/// # What this struct models, and why so little
///
/// `PORT` is modeled because it is the record's identity — `init_record` and
/// `connectDevice` key the whole port attachment on `strlen(port)` /
/// `connectDevice(port, addr)` — so the step-2 port model reads it here rather
/// than out of the override store. Its `DBF_STRING` type matches
/// `asynRecord.dbd` exactly.
///
/// Everything else asyn-specific is either pure config (served generically, as
/// above) or **port-derived** — a value C computes from a live asyn port in
/// `init_record` / `connectDevice` / `monitorStatus` / `process`, which this
/// port cannot reproduce until an asyn port model exists (`asynManager` +
/// a driver). Those fields — `CNCT`/`ENBL`/`AUCT`/`PCNCT` (connection state),
/// `OCTETIV`/`OPTIONIV`/`GPIBIV`/`I32IV`/`UI32IV`/`F64IV` (interface
/// discovery), `REASON` (`drvUser->create`), the trace mirror fields
/// (`TMSK`/`TB0`..`TB5`/`TIOM`/`TIB0`..`TIB2`/`TINM`/`TINB0`..`TINB3`/`TSIZ`,
/// refreshed from `pasynTrace`), the read results (`AINP`/`TINP`/`NORD`/
/// `NAWT`/`EOMR`/`I32INP`/`UI32INP`/`F64INP`/`SPR`), and the serial options
/// read back by `getOptions` (`BAUD`/`PRTY`/`DBIT`/`SBIT`/`MCTL`/`FCTL`/
/// `IXON`/`IXOFF`/`IXANY`/`DRTO`) — are NOT modeled here: giving them a
/// hardcoded struct value would be a fake that only coincidentally matches a
/// connected port. They serve their `.dbd` default until step 2 lands the port
/// model; see the crate task notes.
///
/// This is why the prior stub's `cnct`/`tib2` were removed: both are
/// `DBF_MENU` fields (`menu(asynCONNECT)` / `menu(asynTRACE)`) that the stub
/// modeled as `DBF_LONG`, so they served `DBR_LONG` where C serves `DBR_ENUM`
/// — a wire-type divergence on top of a faked value. Unmodeled, they now serve
/// as the correct `DBR_ENUM` from the declaration table.
///
/// # `TFIL` is the one config field the `.dbd` cannot express
///
/// `asynRecord.c` `init_record` pass 0 does `strcpy(prec->tfil, "Unknown")`,
/// but `asynRecord.dbd` gives `TFIL` no `initial(...)`. So the generic
/// declared-default path serves `""` where C serves `"Unknown"`, and no `.dbd`
/// edit can fix it (the initial is set in record code, not the declaration).
/// `init_record` on the record struct runs on `&mut Self`, which cannot reach
/// the per-instance override store either. The faithful fix is therefore to
/// model `TFIL` as a struct field whose `Default` is `"Unknown"` — the same
/// justification as `PORT`, and the only way to serve a runtime constant the
/// declaration does not carry. (Its `monitorStatus` refresh — reset to
/// `"Unknown"` when the trace file changes — is port-derived, step 2 in the
/// asyn module; the base display stub only needs the constant default.)
#[derive(EpicsRecord)]
#[record(type = "asyn")]
pub struct AsynRecord {
    /// `field(PORT,DBF_STRING) size(40)` — the asyn port name this record
    /// attaches to. The connect path keys on it, and its `DBF_STRING` type
    /// matches the declaration. Initial `""` matches `asynRecord.dbd`'s
    /// `initial("")` and this struct's `Default`.
    #[field(type = "String")]
    pub port: String,
    /// `field(TFIL,DBF_STRING) size(40)` — trace I/O file. Modeled solely to
    /// carry the `"Unknown"` constant `init_record` seeds, which the `.dbd`
    /// `initial(...)` cannot (see the type-level note).
    #[field(type = "String")]
    pub tfil: String,
}

impl Default for AsynRecord {
    fn default() -> Self {
        Self {
            port: String::new(),
            // C `init_record` pass 0: `strcpy(prec->tfil, "Unknown")`.
            tfil: "Unknown".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AsynRecord;
    use crate::server::record::RecordInstance;
    use crate::types::{DbFieldType, EpicsValue};

    fn instance() -> RecordInstance {
        RecordInstance::new("ASYN:TEST".to_string(), AsynRecord::default())
    }

    /// A `DBF_MENU` config field (`TMOD`, `menu(asynTMOD)`) the struct does not
    /// model serves as `DBR_ENUM` from the declaration table, and a put of a
    /// menu *label* stores its ordinal — C `putStringMenu`. asynTMOD order is
    /// Write/Read(0), Write(1), Read(2), Flush(3), NoI/O(4).
    #[test]
    fn menu_field_stores_ordinal_and_serves_as_enum() {
        let mut inst = instance();

        // Declared type is DBR_ENUM (was DBR_LONG when TMOD-class fields were
        // wrongly modeled as integers).
        assert_eq!(inst.declared_field_type("TMOD"), Some(DbFieldType::Enum));

        // Untouched: index 0 (no `initial(...)`), served as Enum.
        assert_eq!(inst.resolve_field("TMOD"), Some(EpicsValue::Enum(0)));

        // A label put resolves to the menu ordinal.
        inst.put_common_field("TMOD", EpicsValue::String("Read".into()))
            .expect("caput asyn.TMOD Read must resolve against menu(asynTMOD)");
        assert_eq!(inst.resolve_field("TMOD"), Some(EpicsValue::Enum(2)));
    }

    /// The two fields the prior stub modeled as `DBF_LONG` are `DBF_MENU` in
    /// `asynRecord.dbd`; unmodeled, they now serve as the correct `DBR_ENUM`.
    #[test]
    fn connection_and_trace_menu_fields_serve_as_enum() {
        let inst = instance();
        // CNCT is menu(asynCONNECT); TIB2 is menu(asynTRACE). Both index 0 with
        // no live port (the honest portless default — a connected port's CNCT=1
        // is port-derived, step 2).
        assert_eq!(inst.declared_field_type("CNCT"), Some(DbFieldType::Enum));
        assert_eq!(inst.resolve_field("CNCT"), Some(EpicsValue::Enum(0)));
        assert_eq!(inst.declared_field_type("TIB2"), Some(DbFieldType::Enum));
        assert_eq!(inst.resolve_field("TIB2"), Some(EpicsValue::Enum(0)));
    }

    /// A `DBF_STRING size(40)` field truncates a write to `size - 1` = 39 bytes
    /// — C `putStringString`. `DRVINFO` is unmodeled, so this exercises the
    /// generic override store's coercion.
    #[test]
    fn string_field_truncates_to_field_size() {
        let mut inst = instance();
        let long = "X".repeat(45); // > 39
        inst.put_common_field("DRVINFO", EpicsValue::String(long.as_str().into()))
            .expect("caput asyn.DRVINFO <45 chars> must be accepted");
        match inst.resolve_field("DRVINFO") {
            Some(EpicsValue::String(s)) => {
                assert_eq!(s.as_bytes().len(), 39, "size(40) caps stored bytes at 39");
            }
            other => panic!("DRVINFO should read back a String, got {other:?}"),
        }
    }

    /// A numeric config field round-trips through the generic store (`NRRD`,
    /// `DBF_LONG`), and a numeric field with an `initial(...)` serves it
    /// (`TMOT`, `DBF_DOUBLE initial("1.0")`).
    #[test]
    fn numeric_fields_round_trip_and_serve_initial() {
        let mut inst = instance();

        assert_eq!(inst.resolve_field("TMOT"), Some(EpicsValue::Double(1.0)));

        inst.put_common_field("NRRD", EpicsValue::String("42".into()))
            .expect("caput asyn.NRRD 42 must be accepted");
        assert_eq!(inst.resolve_field("NRRD"), Some(EpicsValue::Long(42)));

        // OMAX/IMAX/NOWT carry initial("80"); UI32MASK carries initial 0xffffffff.
        assert_eq!(inst.resolve_field("OMAX"), Some(EpicsValue::Long(80)));
        assert_eq!(inst.resolve_field("NOWT"), Some(EpicsValue::Long(80)));
    }

    /// The one modeled field round-trips through the record's own storage.
    #[test]
    fn modeled_port_round_trips() {
        let mut inst = instance();
        assert_eq!(inst.declared_field_type("PORT"), Some(DbFieldType::String));
        inst.record
            .put_field("PORT", EpicsValue::String("L0".into()))
            .expect("PORT is modeled and writable");
        assert_eq!(
            inst.resolve_field("PORT"),
            Some(EpicsValue::String("L0".into()))
        );
    }

    /// `TFIL` serves the `"Unknown"` constant `init_record` seeds, with no port
    /// — the `.dbd` has no `initial(...)` for it, so the generic path would
    /// serve `""`. This is the one field modeled purely to carry that default.
    #[test]
    fn tfil_serves_unknown_with_no_port() {
        let inst = instance();
        assert_eq!(inst.declared_field_type("TFIL"), Some(DbFieldType::String));
        assert_eq!(
            inst.resolve_field("TFIL"),
            Some(EpicsValue::String("Unknown".into())),
            "C init_record pass 0 sets tfil=\"Unknown\"; the .dbd cannot express it"
        );
    }
}
