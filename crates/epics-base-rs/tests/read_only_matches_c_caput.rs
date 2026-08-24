//! Field writability, pinned to what the C IOC actually accepts.
//!
//! `read_only` is `special(SPC_NOMOD)`, and `rsrv/camessage.c:2608-2619` is
//! where it becomes a CA write-access denial. Every expectation below was
//! measured with `caput` against the compiled softIoc at
//! `/home/stevek/work/epics-base/bin/linux-x86_64/` — not read off the `.dbd`,
//! because for `lsi.OVAL`/`lso.OVAL` the `.dbd` and the IOC disagree (the record
//! raises SPC_NOMOD inside `cvt_dbaddr`, which the `.dbd` cannot show).
//!
//! The port's hand-written tables got a number of these wrong in both
//! directions: it allowed writes C refuses, and refused writes C allows.

use epics_base_rs::server::db_loader::create_record;
use epics_base_rs::server::record::RecordInstance;

fn is_no_mod(record_type: &str, field: &str) -> bool {
    let rec =
        create_record(record_type).unwrap_or_else(|e| panic!("cannot create {record_type}: {e:?}"));
    RecordInstance::new_boxed(format!("T:{record_type}"), rec).is_no_mod(field)
}

/// `caput` on the C IOC answers "Write access denied" for each of these.
#[test]
fn fields_c_refuses_are_read_only() {
    for (record, field) in [
        // The alarm / monitor deadband caches: what was last posted. C makes
        // every one of them SPC_NOMOD; the port's tables had all nine writable.
        ("ao", "LALM"),
        ("ao", "ALST"),
        ("ao", "MLST"),
        ("longout", "LALM"),
        ("longout", "ALST"),
        ("longout", "MLST"),
        ("int64out", "LALM"),
        ("int64out", "ALST"),
        ("int64out", "MLST"),
        // The hardware mask and bit count are computed by init_record.
        ("bi", "MASK"),
        ("bo", "MASK"),
        ("mbbi", "MASK"),
        ("mbbi", "NOBT"),
        ("mbbo", "MASK"),
        ("mbbo", "NOBT"),
        // The VAL buffer size is fixed at init: C clamps it to [16, 0x7fff]
        // and will not let a client resize the buffer afterwards.
        ("lsi", "SIZV"),
        ("lso", "SIZV"),
        ("printf", "SIZV"),
        // sel.VAL is SPC_NOMOD: the selection drives it, a client cannot.
        ("sel", "VAL"),
        // OVAL is the previously-posted string. `lsiRecord.c:131-134` raises
        // paddr->special = SPC_NOMOD inside cvt_dbaddr — the .dbd does NOT say
        // so, which is why the generator needs the cvt_dbaddr override table.
        ("lsi", "OVAL"),
        ("lso", "OVAL"),
    ] {
        assert!(
            is_no_mod(record, field),
            "C refuses `caput {record}.{field}` (Write access denied), \
             but the port allows the write"
        );
    }
}

/// `caput` on the C IOC SUCCEEDS for each of these. The port's hand-written
/// tables marked them read-only and refused writes C accepts.
#[test]
fn fields_c_accepts_are_writable() {
    for (record, field) in [("calcout", "PVAL"), ("longout", "PVAL"), ("printf", "VAL")] {
        assert!(
            !is_no_mod(record, field),
            "C accepts `caput {record}.{field}`, but the port refuses it as read-only"
        );
    }
}
