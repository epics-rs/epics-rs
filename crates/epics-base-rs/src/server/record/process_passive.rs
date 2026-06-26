//! `pp(TRUE)` field tables for the `dbPutField` processing gate.
//!
//! C `dbPutField` (`dbAccess.c:1263-1268`) re-processes a record on a CA/PVA
//! put **only** when the put target is the `PROC` field, or when the put
//! field is declared `process_passive` (`pp(TRUE)`) in the record's DBD
//! **and** the record's `SCAN` is `Passive`:
//!
//! ```c
//! if (paddr->pfield == &precord->proc ||
//!     (pfldDes->process_passive && precord->scan == 0 &&
//!      dbrType < DBR_PUT_ACKT)) {
//!     ... dbProcess(precord); ...
//! }
//! ```
//!
//! The Rust port previously processed on **every** field put, which
//! double-processes scanned records and spuriously processes puts to
//! non-`pp` fields (extra FLNK / monitors / device writes / timestamps).
//!
//! These tables carry the `pp(TRUE)` field names for each record type,
//! transcribed verbatim from the upstream DBD:
//! - base records: `epics-base/modules/database/src/std/rec/<rec>Record.dbd.pod`
//! - synApps calc records: `epics-modules/calc/calcApp/src/`
//! - busy: `epics-modules/busy/busyApp/src/busyRecord.dbd`
//! - motor: `epics-modules/motor/motorApp/MotorSrc/motorRecord.dbd`
//!   (plus motor-rs extension fields documented at the entry)
//!
//! A record type that is **not** listed here returns `None`: the put gate
//! then falls back to the legacy "process on every put" behavior, so
//! record types whose DBD pp-flags have not been modeled (records in other
//! crates — scaler, std, optics — and test records) keep working
//! unchanged. `asynRecord` is intentionally omitted: it has two impls
//! across crates (`epics-base-rs` and `asyn-rs`) and is covered by the
//! ASYN parity findings, so it stays on the legacy path here.

/// `pp(TRUE)` field names for `record_type`, sourced from the upstream DBD,
/// or `None` when the type has not been modeled (legacy always-process).
///
/// The empty slice (`Some(&[])`, e.g. `event`, `histogram`) is distinct
/// from `None`: it means "modeled, no field forces processing" — a put to
/// any field of such a record never drives processing (only `PROC` does).
pub fn pp_fields_for(record_type: &str) -> Option<&'static [&'static str]> {
    // mbbi and mbbo share the identical 50-name pp(TRUE) set.
    const MBB_PP: &[&str] = &[
        "VAL", "ZRVL", "ONVL", "TWVL", "THVL", "FRVL", "FVVL", "SXVL", "SVVL", "EIVL", "NIVL",
        "TEVL", "ELVL", "TVVL", "TTVL", "FTVL", "FFVL", "ZRST", "ONST", "TWST", "THST", "FRST",
        "FVST", "SXST", "SVST", "EIST", "NIST", "TEST", "ELST", "TVST", "TTST", "FTST", "FFST",
        "ZRSV", "ONSV", "TWSV", "THSV", "FRSV", "FVSV", "SXSV", "SVSV", "EISV", "NISV", "TESV",
        "ELSV", "TVSV", "TTSV", "FTSV", "FFSV", "UNSV", "COSV", "RVAL",
    ];

    Some(match record_type {
        "ai" => &[
            "VAL", "LINR", "EGUF", "EGUL", "AOFF", "ASLO", "HIHI", "LOLO", "HIGH", "LOW", "HHSV",
            "LLSV", "HSV", "LSV", "ESLO", "EOFF", "ROFF", "RVAL",
        ],
        "ao" => &[
            "VAL", "LINR", "EGUF", "EGUL", "ROFF", "EOFF", "ESLO", "DRVH", "DRVL", "AOFF", "ASLO",
            "HIHI", "LOLO", "HIGH", "LOW", "HHSV", "LLSV", "HSV", "LSV", "RVAL",
        ],
        "bi" => &["VAL", "ZSV", "OSV", "COSV", "ZNAM", "ONAM", "RVAL"],
        "bo" => &["VAL", "ZNAM", "ONAM", "RVAL", "ZSV", "OSV", "COSV"],
        "busy" => &["VAL", "ZNAM", "ONAM", "RVAL", "ZSV", "OSV", "COSV"],
        "calc" => &[
            "CALC", "HIHI", "LOLO", "HIGH", "LOW", "HHSV", "LLSV", "HSV", "LSV", "A", "B", "C",
            "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q", "R", "S", "T",
            "U",
        ],
        "calcout" => &[
            "CALC", "OCAL", "HIHI", "LOLO", "HIGH", "LOW", "HHSV", "LLSV", "HSV", "LSV", "A", "B",
            "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q", "R", "S",
            "T", "U",
        ],
        "compress" => &["VAL"],
        "dfanout" => &[
            "VAL", "HIHI", "LOLO", "HIGH", "LOW", "HHSV", "LLSV", "HSV", "LSV",
        ],
        "event" => &[],
        "fanout" => &["VAL"],
        "histogram" => &[],
        "int64in" => &[
            "VAL", "HIHI", "LOLO", "HIGH", "LOW", "HHSV", "LLSV", "HSV", "LSV",
        ],
        "int64out" => &[
            "VAL", "DRVH", "DRVL", "HIHI", "LOLO", "HIGH", "LOW", "HHSV", "LLSV", "HSV", "LSV",
        ],
        "longin" => &[
            "VAL", "HIHI", "LOLO", "HIGH", "LOW", "HHSV", "LLSV", "HSV", "LSV",
        ],
        "longout" => &[
            "VAL", "DRVH", "DRVL", "HIHI", "LOLO", "HIGH", "LOW", "HHSV", "LLSV", "HSV", "LSV",
        ],
        "lsi" => &["VAL"],
        "lso" => &["VAL"],
        "mbbi" => MBB_PP,
        "mbbo" => MBB_PP,
        "mbbiDirect" => &[
            "VAL", "RVAL", "B0", "B1", "B2", "B3", "B4", "B5", "B6", "B7", "B8", "B9", "BA", "BB",
            "BC", "BD", "BE", "BF", "B10", "B11", "B12", "B13", "B14", "B15", "B16", "B17", "B18",
            "B19", "B1A", "B1B", "B1C", "B1D", "B1E", "B1F",
        ],
        "mbboDirect" => &[
            "VAL", "OMSL", "RVAL", "B0", "B1", "B2", "B3", "B4", "B5", "B6", "B7", "B8", "B9",
            "BA", "BB", "BC", "BD", "BE", "BF", "B10", "B11", "B12", "B13", "B14", "B15", "B16",
            "B17", "B18", "B19", "B1A", "B1B", "B1C", "B1D", "B1E", "B1F",
        ],
        // motorRecord.dbd pp(TRUE) set (OFF through SYNC), plus five
        // motor-rs extension fields (PCOF/ICOF/DCOF/JVEL/PCO_ENABLE)
        // whose driver commands ride the process pass that follows the
        // put — C sends those from special() without processing, but
        // motor-rs has no put-time driver channel, so dropping their
        // pass would strand the commands in the special_cmds buffer.
        "motor" => &[
            "OFF",
            "DIR",
            "SREV",
            "UREV",
            "MRES",
            "ERES",
            "UEIP",
            "URIP",
            "HLM",
            "LLM",
            "DHLM",
            "DLLM",
            "HIHI",
            "LOLO",
            "HIGH",
            "LOW",
            "HHSV",
            "LLSV",
            "HSV",
            "LSV",
            "HLSV",
            "SPMG",
            "STOP",
            "HOMF",
            "HOMR",
            "JOGF",
            "JOGR",
            "TWF",
            "TWR",
            "VAL",
            "DVAL",
            "RVAL",
            "RLV",
            "CNEN",
            "STUP",
            "SYNC",
            "PCOF",
            "ICOF",
            "DCOF",
            "JVEL",
            "PCO_ENABLE",
        ],
        // permissiveRecord.dbd.pod: VAL, WFLG, LABL are pp(TRUE)
        // (OVAL/OFLG are SPC_NOMOD trackers).
        "permissive" => &["VAL", "WFLG", "LABL"],
        "printf" => &["VAL", "FMT"],
        "sel" => &[
            "HIHI", "LOLO", "HIGH", "LOW", "HHSV", "LLSV", "HSV", "LSV", "A", "B", "C", "D", "E",
            "F", "G", "H", "I", "J", "K", "L",
        ],
        "seq" => &["VAL"],
        // stateRecord.dbd.pod: only VAL is pp(TRUE) (OVAL is SPC_NOMOD).
        "state" => &["VAL"],
        "stringin" => &["VAL", "SVAL"],
        "stringout" => &["VAL"],
        "sub" => &[
            "VAL", "HIHI", "LOLO", "HIGH", "LOW", "BRSV", "HHSV", "LLSV", "HSV", "LSV", "A", "B",
            "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q", "R", "S",
            "T", "U",
        ],
        "aSub" => &["BRSV"],
        // waveform-family: WaveformRecord::record_type() is dynamic
        // (waveform / aai / aao / subArray).
        "waveform" => &["VAL", "RARM"],
        "aai" => &["VAL"],
        "aao" => &["VAL"],
        "subArray" => &["VAL", "NELM", "INDX"],
        // synApps calc module records.
        "scalcout" => &[
            "CALC", "OCAL", "HIHI", "LOLO", "HIGH", "LOW", "HHSV", "LLSV", "HSV", "LSV", "A", "B",
            "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "AA", "BB", "CC", "DD", "EE", "FF",
            "GG", "HH", "II", "JJ", "KK", "LL",
        ],
        // aCalcout pp(TRUE) set (aCalcoutRecord.dbd): scalar A..L + array
        // AA..LL inputs, CALC/OCAL, alarm limits, plus the array result/sizing
        // fields AVAL/OAV/PAVL/POAV/NUSE that C marks pp(TRUE).
        "acalcout" => &[
            "AVAL", "NUSE", "PAVL", "CALC", "OCAL", "HIHI", "LOLO", "HIGH", "LOW", "HHSV", "LLSV",
            "HSV", "LSV", "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "AA", "BB",
            "CC", "DD", "EE", "FF", "GG", "HH", "II", "JJ", "KK", "LL", "OAV", "POAV",
        ],
        "swait" => &["A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L"],
        "transform" => &[
            "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P",
        ],
        "sseq" => &["VAL"],
        // std module throttleRecord.dbd: only VAL is pp(TRUE). OUT, SINP,
        // DLY and SYNC are special(SPC_MOD) with no pp — a put to them is
        // handled by throttle `special()` (link re-classify / valueSync /
        // delay clamp) and must NOT process the record.
        "throttle" => &["VAL"],
        _ => return None,
    })
}
