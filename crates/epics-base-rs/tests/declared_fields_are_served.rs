//! Every field the port DECLARES, the port must SERVE.
//!
//! `mbbi.SDEF` / `mbbo.SDEF` are declared in the `.dbd` — the C IOC answers
//! `caget T:MBBI.SDEF` — but the port resolved them to nothing, so the CA server
//! refused to create the channel at all. That is not a wrong value: it is a
//! field the C IOC has and the port does not. An OPI screen or an autosave
//! request file naming it fails against the port and works against C.
//!
//! The declaration and the serving are two tables written by different hands
//! (`dbd_generated::record_fields`, generated from the `.dbd`; `get_field`'s
//! `match`, written by hand), and nothing made them agree. This file is the
//! join. It walks every declared field of every record type and demands the port
//! answer for it.
//!
//! Measured against `/home/stevek/work/epics-base/bin/linux-x86_64/softIoc`:
//!
//! ```text
//! record(mbbi, "T:MBBI")  { }                      caget T:MBBI.SDEF  -> 0
//! record(mbbi, "T:MBBIS") { field(ZRST, "Off") }   caget T:MBBIS.SDEF -> 1
//! record(mbbo, "T:MBBO")  { }                      caget T:MBBO.SDEF  -> 0
//! record(mbbo, "T:MBBOS") { field(ONVL, "2")   }   caget T:MBBOS.SDEF -> 1
//! ```

use epics_base_rs::server::db_loader::create_record;
use epics_base_rs::server::record::RecordInstance;
use epics_base_rs::server::record::dbd_generated::{RECORD_TYPES, record_fields};
use epics_base_rs::types::EpicsValue;

mod module_records;

/// The gaps that exist TODAY. SDEF was one of them; the sweep below is what
/// found the other 290, and they are defects, not exemptions — each is a channel
/// the C IOC serves and the port does not.
///
/// This list is a RATCHET, not a licence: a declared field that is unserved and
/// NOT named here fails the sweep. It is deliberately not an exact-set match,
/// because several of these are being closed concurrently and an equality
/// assertion would fail the moment someone else fixed one. Delete entries as
/// they are fixed; never add one.
///
/// Two shapes are mixed in here and want different work:
///
/// * `asyn.*` (73) — `epics-base-rs`'s `asyn` record is a 3-field OPI stub
///   (`records/asyn_record.rs`: CNCT/PORT/TIB2) while this crate's `.dbd`
///   declares the full 76-field asynRecord. The DEPLOYED record comes from
///   `asyn-rs`; the stub is what this crate's registry hands out. The stub and
///   the declaration must be reconciled — asynRecord's field surface is Tier 1.
/// * the rest (217) — genuine missing `get_field` arms on soft records
///   (`transform`, `scalcout`, `sub`, `sel`, `aSub`, `swait`), plus a scatter of
///   singles (`ai.LBRK`, `ao.OMOD`, `bi.SVAL`, `seq.OLDN`, ...).
const KNOWN_UNSERVED: &[&str] = &[
    "aSub.OVAL",
    "aSub.ONVA",
    "aSub.ONVB",
    "aSub.ONVC",
    "aSub.ONVD",
    "aSub.ONVE",
    "aSub.ONVF",
    "aSub.ONVG",
    "aSub.ONVH",
    "aSub.ONVI",
    "aSub.ONVJ",
    "aSub.ONVK",
    "aSub.ONVL",
    "aSub.ONVM",
    "aSub.ONVN",
    "aSub.ONVO",
    "aSub.ONVP",
    "aSub.ONVQ",
    "aSub.ONVR",
    "aSub.ONVS",
    "aSub.ONVT",
    "aSub.ONVU",
    "ai.LBRK",
    "ao.LBRK",
    "ao.OMOD",
    "asyn.VAL",
    "asyn.ADDR",
    "asyn.PCNCT",
    "asyn.DRVINFO",
    "asyn.REASON",
    "asyn.TMOD",
    "asyn.TMOT",
    "asyn.IFACE",
    "asyn.OCTETIV",
    "asyn.OPTIONIV",
    "asyn.GPIBIV",
    "asyn.I32IV",
    "asyn.UI32IV",
    "asyn.F64IV",
    "asyn.AOUT",
    "asyn.OEOS",
    "asyn.BOUT",
    "asyn.OMAX",
    "asyn.NOWT",
    "asyn.NAWT",
    "asyn.OFMT",
    "asyn.AINP",
    "asyn.TINP",
    "asyn.IEOS",
    "asyn.BINP",
    "asyn.IMAX",
    "asyn.NRRD",
    "asyn.NORD",
    "asyn.IFMT",
    "asyn.EOMR",
    "asyn.I32INP",
    "asyn.I32OUT",
    "asyn.UI32INP",
    "asyn.UI32OUT",
    "asyn.UI32MASK",
    "asyn.F64INP",
    "asyn.F64OUT",
    "asyn.BAUD",
    "asyn.LBAUD",
    "asyn.PRTY",
    "asyn.DBIT",
    "asyn.SBIT",
    "asyn.MCTL",
    "asyn.FCTL",
    "asyn.IXON",
    "asyn.IXOFF",
    "asyn.IXANY",
    "asyn.HOSTINFO",
    "asyn.DRTO",
    "asyn.UCMD",
    "asyn.ACMD",
    "asyn.SPR",
    "asyn.TMSK",
    "asyn.TB0",
    "asyn.TB1",
    "asyn.TB2",
    "asyn.TB3",
    "asyn.TB4",
    "asyn.TB5",
    "asyn.TIOM",
    "asyn.TIB0",
    "asyn.TIB1",
    "asyn.TINM",
    "asyn.TINB0",
    "asyn.TINB1",
    "asyn.TINB2",
    "asyn.TINB3",
    "asyn.TSIZ",
    "asyn.TFIL",
    "asyn.AUCT",
    "asyn.ENBL",
    "asyn.ERRS",
    "asyn.AQR",
    "bi.SVAL",
    "calcout.POVL",
    "dfanout.EGU",
    "dfanout.PREC",
    "dfanout.HOPR",
    "dfanout.LOPR",
    "event.SVAL",
    "histogram.PREC",
    "histogram.HOPR",
    "histogram.LOPR",
    "mbbi.SVAL",
    "mbbiDirect.SVAL",
    "mbboDirect.OBIT",
    "scalcout.VERS",
    "scalcout.INAV",
    "scalcout.INBV",
    "scalcout.INCV",
    "scalcout.INDV",
    "scalcout.INEV",
    "scalcout.INFV",
    "scalcout.INGV",
    "scalcout.INHV",
    "scalcout.INIV",
    "scalcout.INJV",
    "scalcout.INKV",
    "scalcout.INLV",
    "scalcout.IAAV",
    "scalcout.IBBV",
    "scalcout.ICCV",
    "scalcout.IDDV",
    "scalcout.IEEV",
    "scalcout.IFFV",
    "scalcout.IGGV",
    "scalcout.IHHV",
    "scalcout.IIIV",
    "scalcout.IJJV",
    "scalcout.IKKV",
    "scalcout.ILLV",
    "scalcout.OUTV",
    "scalcout.EGU",
    "scalcout.HOPR",
    "scalcout.LOPR",
    "scalcout.PAA",
    "scalcout.PBB",
    "scalcout.PCC",
    "scalcout.PDD",
    "scalcout.PEE",
    "scalcout.PFF",
    "scalcout.PGG",
    "scalcout.PHH",
    "scalcout.PII",
    "scalcout.PJJ",
    "scalcout.PKK",
    "scalcout.PLL",
    "scalcout.POSV",
    "scalcout.PA",
    "scalcout.PB",
    "scalcout.PC",
    "scalcout.PD",
    "scalcout.PE",
    "scalcout.PF",
    "scalcout.PG",
    "scalcout.PH",
    "scalcout.PI",
    "scalcout.PJ",
    "scalcout.PK",
    "scalcout.PL",
    "scalcout.POVL",
    "scalcout.ALST",
    "scalcout.MLST",
    "sel.PREC",
    "sel.EGU",
    "sel.HOPR",
    "sel.LOPR",
    "sel.ADEL",
    "sel.MDEL",
    "sel.LA",
    "sel.LB",
    "sel.LC",
    "sel.LD",
    "sel.LE",
    "sel.LF",
    "sel.LG",
    "sel.LH",
    "sel.LI",
    "sel.LJ",
    "sel.LK",
    "sel.LL",
    "sel.ALST",
    "sel.MLST",
    "sel.NLST",
    "seq.OLDN",
    "seq.PREC",
    "stringin.SVAL",
    "sub.EGU",
    "sub.HOPR",
    "sub.LOPR",
    "sub.PREC",
    "sub.LA",
    "sub.LB",
    "sub.LC",
    "sub.LD",
    "sub.LE",
    "sub.LF",
    "sub.LG",
    "sub.LH",
    "sub.LI",
    "sub.LJ",
    "sub.LK",
    "sub.LL",
    "sub.LM",
    "sub.LN",
    "sub.LO",
    "sub.LP",
    "sub.LQ",
    "sub.LR",
    "sub.LS",
    "sub.LT",
    "sub.LU",
    "swait.VERS",
    "swait.HOPR",
    "swait.LOPR",
    "swait.INIT",
    "swait.ALST",
    "swait.MLST",
    "transform.VERS",
    "transform.CAV",
    "transform.CBV",
    "transform.CCV",
    "transform.CDV",
    "transform.CEV",
    "transform.CFV",
    "transform.CGV",
    "transform.CHV",
    "transform.CIV",
    "transform.CJV",
    "transform.CKV",
    "transform.CLV",
    "transform.CMV",
    "transform.CNV",
    "transform.COV",
    "transform.CPV",
    "transform.EGU",
    "transform.MAP",
    "transform.IAV",
    "transform.IBV",
    "transform.ICV",
    "transform.IDV",
    "transform.IEV",
    "transform.IFV",
    "transform.IGV",
    "transform.IHV",
    "transform.IIV",
    "transform.IJV",
    "transform.IKV",
    "transform.ILV",
    "transform.IMV",
    "transform.INV",
    "transform.IOV",
    "transform.IPV",
    "transform.OAV",
    "transform.OBV",
    "transform.OCV",
    "transform.ODV",
    "transform.OEV",
    "transform.OFV",
    "transform.OGV",
    "transform.OHV",
    "transform.OIV",
    "transform.OJV",
    "transform.OKV",
    "transform.OLV",
    "transform.OMV",
    "transform.ONV",
    "transform.OOV",
    "transform.OPV",
];

/// The sweep runs at `RecordInstance::client_field_value` — the CA server's own
/// entry, the one `CREATE_CHAN` calls. NOT `Record::get_field`: the common block
/// (SCAN/PHAS, the alarm limits, the links) is owned by the INSTANCE, so a
/// record struct answers `None` for `HIHI` while the channel is served
/// perfectly. Asking the record would report 138 phantom gaps and bury the real
/// ones among them.
#[test]
fn every_declared_field_is_served() {
    let mut checked = 0usize;
    let mut unserved: Vec<String> = Vec::new();

    for &record_type in RECORD_TYPES {
        // Through the module-record fixture: a `continue` here silently
        // dropped the seven types outside `stdRecords.dbd` from the sweep.
        let rec = module_records::create_any(record_type)
            .unwrap_or_else(|e| panic!("{record_type}: create_record failed: {e}"));
        let Some(fields) = record_fields(record_type) else {
            panic!("{record_type} is instantiable but has no declared field table");
        };
        let inst = RecordInstance::new_boxed(format!("T:{record_type}"), rec);

        for f in fields {
            checked += 1;
            let name = format!("{record_type}.{}", f.name);
            if inst.client_field_value(f.name).is_none() && !KNOWN_UNSERVED.contains(&name.as_str())
            {
                unserved.push(name);
            }
        }
    }

    assert!(checked > 1000, "the sweep did not run: {checked} fields");
    assert!(
        unserved.is_empty(),
        "{} declared field(s) the port refuses to serve — a CA client asking for \
         one gets no channel at all, where the C IOC answers:\n  {}",
        unserved.len(),
        unserved.join("\n  ")
    );
}

/// SDEF is the field that exposed the gap, and it is not a constant: it is C's
/// "are any states defined" predicate (`mbbiRecord.c:93-105`), which is also the
/// predicate `mbbo`'s `cvt_dbaddr` re-types VAL on. Both records, and both ways
/// a state table can come into existence — by a VALUE alone, or by a STRING
/// alone.
#[test]
fn sdef_reports_whether_any_state_is_defined() {
    for record_type in ["mbbi", "mbbo"] {
        let mut rec = create_record(record_type).unwrap();

        assert_eq!(
            rec.get_field("SDEF"),
            Some(EpicsValue::Short(0)),
            "{record_type}: a bare record defines no states"
        );

        rec.put_field("ZRST", EpicsValue::String("Off".into()))
            .unwrap();
        assert_eq!(
            rec.get_field("SDEF"),
            Some(EpicsValue::Short(1)),
            "{record_type}: a non-empty state string defines the table"
        );

        let mut rec = create_record(record_type).unwrap();
        rec.put_field("ONVL", EpicsValue::ULong(2)).unwrap();
        assert_eq!(
            rec.get_field("SDEF"),
            Some(EpicsValue::Short(1)),
            "{record_type}: a non-zero state value defines the table"
        );
    }
}

/// Why SDEF is DERIVED rather than stored. C caches `prec->sdef` and recomputes
/// it from `special()` after a write to any of the 32 state fields — a cache is
/// only ever as good as that hand-written field list. Here the flag is a
/// function OF those fields, so a put to the SIXTEENTH state string, the one
/// furthest from any such list, cannot leave it stale.
#[test]
fn a_state_written_at_runtime_is_visible_in_sdef() {
    for record_type in ["mbbi", "mbbo"] {
        let mut rec = create_record(record_type).unwrap();
        assert_eq!(rec.get_field("SDEF"), Some(EpicsValue::Short(0)));

        rec.put_field("FFST", EpicsValue::String("Fifteen".into()))
            .unwrap();

        assert_eq!(
            rec.get_field("SDEF"),
            Some(EpicsValue::Short(1)),
            "{record_type}: SDEF must follow the state table, not a cache of it"
        );
    }
}

/// `special(SPC_NOMOD)` (mbbiRecord.dbd:476, mbboRecord.dbd:485) — C refuses a
/// put to SDEF. It is a REPORT on the state table, not a setting: a writable
/// copy could contradict the table it describes, and on mbbo it would contradict
/// the wire type VAL is served at.
#[test]
fn sdef_is_read_only() {
    for record_type in ["mbbi", "mbbo"] {
        let sdef = record_fields(record_type)
            .unwrap()
            .iter()
            .find(|f| f.name == "SDEF")
            .unwrap_or_else(|| panic!("{record_type} declares no SDEF"));
        assert!(
            sdef.read_only,
            "{record_type}.SDEF is special(SPC_NOMOD) in the dbd — it must not be writable"
        );
    }
}
