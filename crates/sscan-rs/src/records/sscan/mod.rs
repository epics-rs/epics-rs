//! The `sscan` record — port of `sscanRecord.c` (sscan R2-12).
//!
//! The record drives up to four positioners through a 1-D scan, triggering
//! and reading up to 70 detectors at every point, through `recDynLink`
//! channels it opens from PV names written at run time. C's per-field
//! `db_post_events` calls are kept one for one: each post stores the value
//! it was made with ([`Record::take_cycle_posted_value`]), because a scan
//! posts one field several times in a pass (`DATA`, `CPT`, `FAZE`).
//!
//! The scan logic is split as C's file is: `scan` (process and the scan
//! state machine), `special` (field-write handling and the linear-scan
//! parameter algebra) and `links` (the `recDynLink` callbacks).

mod links;
mod scan;
mod special;

use std::collections::{HashMap, VecDeque};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Instant;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::runtime::task::CallbackPriority;
use epics_base_rs::server::database::AsyncDbHandle;
use epics_base_rs::server::record::dbd_generated::DB_COMMON_FIELDS;
use epics_base_rs::server::record::{
    CycleHooks, DeclaredFields, EventMask, FieldDesc, FieldIx, FieldMetadataOverride,
    InitLinkReads, OwnFrame, ProcessContext, ProcessOutcome, Record, ScanType, Special,
};
use epics_base_rs::types::{DbFieldType, EpicsValue, PvString, RecordName, WallTime};

use super::dbd_generated::{
    MENU_SSCAN_DSTATE, MENU_SSCAN_FAZE, MENU_SSCAN_PASM, SSCAN_DBADDR, SSCAN_FIELDS, SSCAN_NOACCESS,
};
use crate::rec_dyn_link::RecDynLink;

/// C `#define VERSION 5.53`, stored into VERS by `init_record` pass 0.
const VERSION: f64 = 5.53;

pub(crate) const NUM_POS: usize = 4;
pub(crate) const NUM_DET: usize = 70;
pub(crate) const NUM_TRGS: usize = 4;
pub(crate) const NUM_PVS: usize = 85;
pub(crate) const NUM_LINKS: usize = 89;
/// The `pvIndex` of the first link of each kind (`sscanRecord.c` `P1_IN`..).
pub(crate) const R1_IN: usize = 4;
pub(crate) const D1_IN: usize = 8;
pub(crate) const T1_OUT: usize = 78;
pub(crate) const A1_OUT: usize = 82;
pub(crate) const BS_OUT: usize = 83;
pub(crate) const AS_OUT: usize = 84;
pub(crate) const P1_OUT: usize = 85;
/// C `DEF_WF_SIZE`, the least MPTS `init_record` accepts.
const DEF_WF_SIZE: i32 = 10;
/// C `MIN_MON`: seconds between the value posts `checkMonitors` makes.
pub(crate) const MIN_MON: f64 = 0.05;

/// `P1NV`..`ASNV` states (C `PV_OK`..`PV_NC`).
pub(crate) const PV_OK: u16 = 0;
pub(crate) const NO_PV: u16 = 1;
pub(crate) const PV_NO_READ: u16 = 2;
pub(crate) const PV_NO_WRITE: u16 = 4;
pub(crate) const PV_NC: u16 = 6;

/// `sscanFAZE` choices.
pub(crate) mod faze {
    pub const IDLE: u16 = 0;
    pub const INIT_SCAN: u16 = 1;
    pub const BEFORE_SCAN: u16 = 2;
    pub const BEFORE_SCAN_WAIT: u16 = 3;
    pub const MOVE_MOTORS: u16 = 4;
    pub const CHECK_MOTORS: u16 = 5;
    pub const TRIG_DETCTRS: u16 = 6;
    pub const READ_DETCTRS: u16 = 7;
    pub const RETRACE_MOVE: u16 = 8;
    pub const RETRACE_WAIT: u16 = 9;
    pub const AFTER_SCAN_DO: u16 = 10;
    pub const AFTER_SCAN_WAIT: u16 = 11;
    pub const SCAN_DONE: u16 = 12;
    pub const SCAN_PENDING: u16 = 13;
    pub const PREVIEW: u16 = 14;
    pub const RECORD_SCALAR_DATA: u16 = 15;
}

/// `sscanDSTATE` choices.
pub(crate) mod dstate {
    pub const UNPACKED: u16 = 0;
    pub const TRIG_ARRAY_READ: u16 = 1;
    pub const ARRAY_READ_WAIT: u16 = 2;
    pub const ARRAY_GET_CALLBACK_WAIT: u16 = 3;
    pub const RECORD_ARRAY_DATA: u16 = 4;
    pub const SAVE_DATA_WAIT: u16 = 5;
    pub const PACKED: u16 = 6;
    pub const POSTED: u16 = 7;
}

/// `sscanCMND` choices.
pub(crate) mod cmnd {
    pub const CLEAR_MSG: u16 = 0;
    pub const CHECK_LIMITS: u16 = 1;
    pub const PREVIEW_SCAN: u16 = 2;
    pub const CLEAR_ALL_PVS: u16 = 3;
    pub const CLEAR_POS_PVS_ETC: u16 = 4;
    pub const CLEAR_POS_PVS: u16 = 5;
    pub const CLEAR_POS_RDBK_PVS_ETC: u16 = 6;
    pub const CLEAR_POS_RDBK_PVS: u16 = 7;
}

/// `sscanPASM` choices.
pub(crate) mod pasm {
    pub const STAY: u16 = 0;
    pub const START: u16 = 1;
    pub const PEAK: u16 = 3;
    pub const VALLEY: u16 = 4;
    pub const RISING_EDGE: u16 = 5;
    pub const FALLING_EDGE: u16 = 6;
    pub const COM: u16 = 7;
}

/// `sscanP1SM` choices.
pub(crate) const SM_LINEAR: u16 = 0;
pub(crate) const SM_TABLE: u16 = 1;
pub(crate) const SM_FLY: u16 = 2;
pub(crate) const ACQM_NORMAL: u16 = 0;
pub(crate) const ACQM_ACC: u16 = 1;
pub(crate) const ACQM_ADD: u16 = 2;
pub(crate) const ACQT_1D_ARRAY: u16 = 1;
pub(crate) const LINKWAIT_YES: u16 = 0;
pub(crate) const NOYES_YES: u16 = 1;

/// C `calledBy` values, kept for the messages that print them.
pub(crate) mod called_by {
    pub const SPECIAL_PAUS: u16 = 0x01;
    pub const SPECIAL_EXSC: u16 = 0x02;
    pub const SPECIAL_WAIT: u16 = 0x03;
    pub const SPECIAL_AWAIT: u16 = 0x04;
    pub const NOTIFY_TRIG: u16 = 0x05;
    pub const NOTIFY_READ_ARRAY_TRIG: u16 = 0x06;
    pub const NOTIFY: u16 = 0x07;
    pub const USERGETCALLBACK: u16 = 0x08;
    pub const PVSEARCH: u16 = 0x09;
    pub const POSMON: u16 = 0x0a;
    pub const DO_PUTS: u16 = 0x0b;
    pub const DO_PUTS_TRIG: u16 = 0x0c;
    pub const DELAY: u16 = 0x10;
}

/// C `calledByNames`.
pub(crate) fn called_by_name(c: u16) -> String {
    const NAMES: [&str; 16] = [
        "UNKNOWN",
        "SPECIAL_PAUS",
        "SPECIAL_EXSC",
        "SPECIAL_WAIT",
        "SPECIAL_AWAIT",
        "NOTIFY_TRIG",
        "NOTIFY_READ_ARRAY_TRIG",
        "NOTIFY",
        "USERGETCALLBACK",
        "PVSEARCH",
        "POSMON",
        "DO_PUTS",
        "DO_PUTS_TRIG",
        "0x0d",
        "0x0e",
        "0x0f",
    ];
    let name = NAMES[usize::from(c & 0x0f)];
    if c & called_by::DELAY != 0 {
        format!("{name} (D)")
    } else {
        name.to_string()
    }
}

pub(crate) fn faze_name(f: u16) -> &'static str {
    MENU_SSCAN_FAZE.get(usize::from(f)).copied().unwrap_or("")
}

pub(crate) fn dstate_name(d: u16) -> &'static str {
    MENU_SSCAN_DSTATE.get(usize::from(d)).copied().unwrap_or("")
}

pub(crate) fn pasm_name(p: u16) -> &'static str {
    MENU_SSCAN_PASM.get(usize::from(p)).copied().unwrap_or("")
}

/// iocsh `sscanRecordDebug`. The port carries none of C's debug traces.
pub static SSCAN_RECORD_DEBUG: AtomicI32 = AtomicI32::new(0);
/// iocsh `sscanRecordViewPos`: preview after every linear-parameter edit.
pub static SSCAN_RECORD_VIEW_POS: AtomicI32 = AtomicI32::new(0);
/// iocsh `sscanRecordDontCheckLimits`.
pub static SSCAN_RECORD_DONT_CHECK_LIMITS: AtomicI32 = AtomicI32::new(0);
/// iocsh `sscanRecordLookupTime`: seconds after which a scan start renews
/// its positioner links.
pub static SSCAN_RECORD_LOOKUP_TIME: AtomicI32 = AtomicI32::new(10);
/// iocsh `sscanRecordConnectWaitSeconds`.
pub static SSCAN_RECORD_CONNECT_WAIT_SECONDS: AtomicI32 = AtomicI32::new(1);
/// C `sscan_fit_smooth`: smoothing passes before an after-scan peak search.
pub static SSCAN_FIT_SMOOTH: AtomicI32 = AtomicI32::new(3);
/// C `sscan_test_fit`: write the smoothed (and derived) reference data into
/// detectors 2 and 3.
pub static SSCAN_TEST_FIT: AtomicI32 = AtomicI32::new(0);

pub(crate) fn knob(k: &AtomicI32) -> i32 {
    k.load(Ordering::Relaxed)
}

/// The iocsh variables `sscanSupport.dbd` declares for the record.
pub(crate) fn iocsh_vars() -> Vec<epics_base_rs::server::iocsh::vars::VarDef> {
    use epics_base_rs::server::iocsh::vars::{VarAccess, VarDef};
    macro_rules! int_var {
        ($name:literal, $k:ident) => {
            VarDef {
                name: $name,
                access: VarAccess::Int {
                    get: || i64::from($k.load(Ordering::Relaxed)),
                    set: |v| $k.store(v as i32, Ordering::Relaxed),
                },
            }
        };
    }
    vec![
        int_var!("sscanRecordDebug", SSCAN_RECORD_DEBUG),
        int_var!("sscanRecordViewPos", SSCAN_RECORD_VIEW_POS),
        int_var!("sscanRecordDontCheckLimits", SSCAN_RECORD_DONT_CHECK_LIMITS),
        int_var!("sscanRecordLookupTime", SSCAN_RECORD_LOOKUP_TIME),
        int_var!(
            "sscanRecordConnectWaitSeconds",
            SSCAN_RECORD_CONNECT_WAIT_SECONDS
        ),
    ]
}

// ---------------------------------------------------------------------
// Own-field indices, checked against the generated table.
// ---------------------------------------------------------------------

const fn str_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

const fn own(name: &str) -> usize {
    let f = SSCAN_FIELDS;
    let mut i = 0;
    while i < f.len() {
        if str_eq(f[i].name, name) {
            return i;
        }
        i += 1;
    }
    panic!("not an sscan field")
}

pub(crate) const VERS: usize = own("VERS");
pub(crate) const VAL: usize = own("VAL");
pub(crate) const SMSG: usize = own("SMSG");
pub(crate) const CMND: usize = own("CMND");
pub(crate) const ALRT: usize = own("ALRT");
pub(crate) const MPTS: usize = own("MPTS");
pub(crate) const EXSC: usize = own("EXSC");
pub(crate) const XSC: usize = own("XSC");
pub(crate) const PXSC: usize = own("PXSC");
pub(crate) const BUSY: usize = own("BUSY");
pub(crate) const KILL: usize = own("KILL");
pub(crate) const WAIT: usize = own("WAIT");
pub(crate) const WCNT: usize = own("WCNT");
pub(crate) const AWCT: usize = own("AWCT");
pub(crate) const WTNG: usize = own("WTNG");
pub(crate) const AWAIT: usize = own("AWAIT");
pub(crate) const AAWAIT: usize = own("AAWAIT");
pub(crate) const DATA: usize = own("DATA");
pub(crate) const REFD: usize = own("REFD");
pub(crate) const NPTS: usize = own("NPTS");
pub(crate) const FPTS: usize = own("FPTS");
pub(crate) const FFO: usize = own("FFO");
pub(crate) const CPT: usize = own("CPT");
pub(crate) const BCPT: usize = own("BCPT");
pub(crate) const DPT: usize = own("DPT");
pub(crate) const PCPT: usize = own("PCPT");
pub(crate) const PASM: usize = own("PASM");
pub(crate) const ATIME: usize = own("ATIME");
/// `P1PV`..`ASPV`, in `pvIndex` order.
pub(crate) const PV0: usize = own("P1PV");
pub(crate) const BSWAIT: usize = own("BSWAIT");
pub(crate) const ASWAIT: usize = own("ASWAIT");
/// `P1NV`..`ASNV`, in `pvIndex` order.
pub(crate) const NV0: usize = own("P1NV");
/// The positioner blocks, [`POS_SIZE`] fields each from `P1PP`.
pub(crate) const POS0: usize = own("P1PP");
pub(crate) const POS_SIZE: usize = own("P2PP") - own("P1PP");
/// The detector blocks, [`DET_SIZE`] fields each from `D01HR`.
pub(crate) const DET0: usize = own("D01HR");
pub(crate) const DET_SIZE: usize = own("D02HR") - own("D01HR");
/// `T1CD`..`T4CD`, `A1CD`, `BSCD`, `ASCD`.
pub(crate) const CD0: usize = own("T1CD");
pub(crate) const PAUS: usize = own("PAUS");
pub(crate) const LPAU: usize = own("LPAU");
pub(crate) const PDLY: usize = own("PDLY");
pub(crate) const DDLY: usize = own("DDLY");
pub(crate) const RDLY: usize = own("RDLY");
pub(crate) const FAZE: usize = own("FAZE");
pub(crate) const ACQM: usize = own("ACQM");
pub(crate) const ACQT: usize = own("ACQT");
pub(crate) const DSTATE: usize = own("DSTATE");
pub(crate) const COPYTO: usize = own("COPYTO");

/// Offsets inside a positioner block.
pub(crate) mod p {
    pub const PP: usize = 0;
    pub const CV: usize = 1;
    pub const DV: usize = 2;
    pub const LV: usize = 3;
    pub const SP: usize = 4;
    pub const SI: usize = 5;
    pub const EP: usize = 6;
    pub const CP: usize = 7;
    pub const WD: usize = 8;
    pub const R_CV: usize = 9;
    pub const R_LV: usize = 10;
    pub const R_DL: usize = 11;
    pub const HR: usize = 12;
    pub const LR: usize = 13;
    pub const PA: usize = 14;
    pub const RA: usize = 15;
    pub const CA: usize = 16;
    pub const FS: usize = 17;
    pub const FI: usize = 18;
    pub const FE: usize = 19;
    pub const FC: usize = 20;
    pub const FW: usize = 21;
    pub const SM: usize = 22;
    pub const AR: usize = 23;
    pub const EU: usize = 24;
    pub const PR: usize = 25;
}

/// Offsets inside a detector block.
pub(crate) mod d {
    pub const HR: usize = 0;
    pub const LR: usize = 1;
    pub const DA: usize = 2;
    pub const CA: usize = 3;
    pub const CV: usize = 4;
    pub const LV: usize = 5;
    pub const NE: usize = 6;
    pub const EU: usize = 7;
    pub const PR: usize = 8;
}

/// Own index of positioner `i`'s field `off`.
pub(crate) const fn pf(i: usize, off: usize) -> usize {
    POS0 + POS_SIZE * i + off
}

/// Own index of detector `i`'s field `off`.
pub(crate) const fn df(i: usize, off: usize) -> usize {
    DET0 + DET_SIZE * i + off
}

const _: () = {
    assert!(SSCAN_FIELDS.len() == COPYTO + 1);
    assert!(BSWAIT == PV0 + NUM_PVS && NV0 == ASWAIT + 1);
    assert!(POS0 == NV0 + NUM_PVS && DET0 == pf(NUM_POS, 0));
    assert!(CD0 == df(NUM_DET, 0) && PAUS == CD0 + 7);
    assert!(pf(0, p::PR) == own("P1PR") && pf(3, p::PA) == own("P4PA"));
    assert!(pf(1, p::R_DL) == own("R2DL") && pf(2, p::FW) == own("P3FW"));
    assert!(df(0, d::PR) == own("D01PR") && df(69, d::CA) == own("D70CA"));
    assert!(str_eq(SSCAN_FIELDS[PV0 + AS_OUT].name, "ASPV"));
    assert!(str_eq(SSCAN_FIELDS[NV0 + BS_OUT].name, "BSNV"));
    assert!(str_eq(SSCAN_FIELDS[CD0 + A1_OUT - T1_OUT].name, "A1CD"));
};

const COMMON: usize = DB_COMMON_FIELDS.len();

/// The common `SCAN` field's index.
const SCAN_IX: u16 = {
    let mut i = 0;
    while !str_eq(DB_COMMON_FIELDS[i].name, "SCAN") {
        i += 1;
    }
    i as u16
};

pub(crate) const fn fix(o: usize) -> FieldIx {
    FieldIx((COMMON + o) as u16)
}

/// Field name → own index.
fn own_index(name: &str) -> Option<usize> {
    static MAP: OnceLock<HashMap<&'static str, usize>> = OnceLock::new();
    MAP.get_or_init(|| {
        SSCAN_FIELDS
            .iter()
            .enumerate()
            .map(|(i, f)| (f.name, i))
            .collect()
    })
    .get(name)
    .copied()
}

// ---------------------------------------------------------------------
// Storage.
// ---------------------------------------------------------------------

/// C `posFields`: one positioner's fields, readback included.
#[derive(Clone, Default)]
pub(crate) struct Pos {
    pub pp: f64,
    pub cv: f64,
    pub dv: f64,
    pub lv: f64,
    pub sp: f64,
    pub si: f64,
    pub ep: f64,
    pub cp: f64,
    pub wd: f64,
    pub r_cv: f64,
    pub r_lv: f64,
    pub r_dl: f64,
    pub hr: f64,
    pub lr: f64,
    pub fs: u16,
    pub fi: u16,
    pub fe: u16,
    pub fc: u16,
    pub fw: u16,
    pub sm: u16,
    pub ar: u16,
    pub eu: PvString,
    pub pr: i16,
}

/// C `detFields`.
#[derive(Clone, Default)]
pub(crate) struct Det {
    pub hr: f64,
    pub lr: f64,
    pub cv: f32,
    pub lv: f32,
    pub ne: u32,
    pub eu: PvString,
    pub pr: i16,
}

/// C `recDynLinkPvt`, less what the link index already says.
#[derive(Clone, Default)]
pub(crate) struct LinkPvt {
    pub nelem: usize,
    /// A readback named `TIME`/`time`: the scan's elapsed time.
    pub ts: bool,
    pub lookup_time: Option<Instant>,
    pub connect_in_progress: bool,
    pub waiting_for_pos_mon: bool,
    /// A read-back or detector PV of this database (C `dbAddrNv == 0` on a
    /// link not `useDynLinkAlways`), read by `dbGet`: `pAddr->no_elements`.
    pub local: Option<usize>,
}

/// What a link's `recDynLink` calls back into, C's callback functions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cb {
    Search,
    PosMon,
    PosMonGet,
    Notify,
    UserGet,
}

/// C `linkType`, from the link index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LinkType {
    PositionerIn,
    Readback,
    Detector,
    Trigger,
    ReadArrayTrig,
    BsAs,
    PositionerOut,
}

pub(crate) fn link_type(li: usize) -> LinkType {
    match li {
        0..R1_IN => LinkType::PositionerIn,
        R1_IN..D1_IN => LinkType::Readback,
        D1_IN..T1_OUT => LinkType::Detector,
        T1_OUT..A1_OUT => LinkType::Trigger,
        A1_OUT..BS_OUT => LinkType::ReadArrayTrig,
        BS_OUT..P1_OUT => LinkType::BsAs,
        _ => LinkType::PositionerOut,
    }
}

/// C `linkNames`.
pub(crate) fn link_name(li: usize) -> String {
    match link_type(li) {
        LinkType::PositionerIn => format!("P{}mon", li + 1),
        LinkType::Readback => format!("R{}", li - R1_IN + 1),
        LinkType::Detector => format!("D{:02}", li - D1_IN + 1),
        LinkType::Trigger => format!("T{}", li - T1_OUT + 1),
        LinkType::ReadArrayTrig => "A1".to_string(),
        LinkType::BsAs if li == BS_OUT => "BS".to_string(),
        LinkType::BsAs => "AS".to_string(),
        LinkType::PositionerOut => format!("P{}out", li - P1_OUT + 1),
    }
}

/// One positioner's saved linear parameters (C `posParms`).
#[derive(Clone, Copy, Default)]
pub(crate) struct PosParms {
    pub sp: f64,
    pub si: f64,
    pub ep: f64,
    pub cp: f64,
    pub wd: f64,
}

/// Which of a double-buffered array a channel reads.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Served {
    A,
    B,
    Null,
}

pub struct SscanRecord {
    pub(crate) vers: f64,
    pub(crate) val: f64,
    pub(crate) smsg: PvString,
    pub(crate) cmnd: u16,
    pub(crate) alrt: u8,
    pub(crate) mpts: i32,
    pub(crate) exsc: i16,
    pub(crate) xsc: i16,
    pub(crate) pxsc: u8,
    pub(crate) busy: u8,
    pub(crate) kill: u8,
    pub(crate) wait: i16,
    pub(crate) wcnt: i16,
    pub(crate) awct: i16,
    pub(crate) wtng: i16,
    pub(crate) await_: i16,
    pub(crate) aawait: u16,
    pub(crate) data: i16,
    pub(crate) refd: i16,
    pub(crate) npts: i32,
    pub(crate) fpts: u16,
    pub(crate) ffo: u16,
    pub(crate) cpt: i32,
    pub(crate) bcpt: i32,
    pub(crate) dpt: i32,
    pub(crate) pcpt: i32,
    pub(crate) pasm: u16,
    pub(crate) atime: f32,
    pub(crate) pvn: Vec<PvString>,
    pub(crate) bswait: u16,
    pub(crate) aswait: u16,
    pub(crate) nv: Vec<u16>,
    pub(crate) pos: [Pos; NUM_POS],
    pub(crate) det: Vec<Det>,
    /// `T1CD`..`T4CD`, `A1CD`, `BSCD`, `ASCD`: indexed by `pvIndex - T1_OUT`.
    pub(crate) cd: [f32; 7],
    pub(crate) paus: u16,
    pub(crate) lpau: u16,
    pub(crate) pdly: f32,
    pub(crate) ddly: f32,
    pub(crate) rdly: f32,
    pub(crate) faze: u16,
    pub(crate) acqm: u16,
    pub(crate) acqt: u16,
    pub(crate) dstate: u16,
    pub(crate) copyto: i32,
    /// `TOLP`/`TLAP`: C's zero stamp is `None`, longer ago than any interval.
    pub(crate) tolp: Option<Instant>,
    pub(crate) tlap: Option<Instant>,

    // C `recPvtStruct`.
    /// `validBuf`: 0 is buffer A. The fill buffer is always the other one.
    pub(crate) valid: usize,
    pub(crate) links: Vec<RecDynLink<Cb>>,
    pub(crate) lpvt: Vec<LinkPvt>,
    pub(crate) pa: [Vec<f64>; NUM_POS],
    pub(crate) pos_buf: [[Vec<f64>; 2]; NUM_POS],
    /// Allocated for the first four detectors at init and for the others
    /// when their PV first connects; `None` reads as C's `nullArray`.
    pub(crate) det_buf: Vec<Option<[Vec<f32>; 2]>>,
    pub(crate) null: Vec<f32>,
    /// C `dataBuffer`: where an add-to-previous array read lands. One buffer
    /// for every detector, kept across scans, so a failed get leaves the
    /// last read in it.
    pub(crate) data_buffer: Vec<f32>,
    pub(crate) val_pos: usize,
    pub(crate) val_rdbk: usize,
    pub(crate) val_det: usize,
    pub(crate) val_trig: usize,
    pub(crate) val_atrig: usize,
    pub(crate) acq: [bool; NUM_DET],
    pub(crate) pffo: u16,
    /// `fpts` backup of the freeze flags, C `precPvt->fpts` + `posParms`.
    pub(crate) frz_fpts: u16,
    pub(crate) frz: [[u16; 5]; NUM_POS],
    pub(crate) prev_sm: [u16; NUM_POS],
    pub(crate) pos_parms: [PosParms; NUM_POS],
    pub(crate) table_pts: [usize; NUM_POS],
    pub(crate) have_fly: bool,
    pub(crate) flying: bool,
    pub(crate) last_scan_end: Instant,
    pub(crate) scan_err: u8,
    pub(crate) bad_output: bool,
    pub(crate) bad_input: bool,
    pub(crate) npts_cause: i8,
    pub(crate) num_pos_cb: i16,
    pub(crate) num_trig_cb: i16,
    pub(crate) num_aread_cb: i16,
    pub(crate) num_get_cb: i16,
    pub(crate) prev_acqm: u16,
    pub(crate) prev_num_pts: i32,
    pub(crate) scan_by_search: bool,
    pub(crate) user_set_await: bool,
    pub(crate) called_by: u16,
    /// C `ticsPerSecond`, for rounding the delays.
    pub(crate) tics: f64,

    // Framework plumbing.
    pub(crate) name: String,
    pub(crate) handle: Option<AsyncDbHandle>,
    pub(crate) inbox: std::sync::Arc<links::Inbox>,
    pub(crate) prio: CallbackPriority,
    /// The record's TIME at the start of this pass (the scan's start stamp
    /// once a scan is running).
    pub(crate) time: WallTime,
    /// Generation of the one delayed callback C's `dlyCallback` timer is:
    /// a newer request or a cancel retires the older ones.
    pub(crate) dly_gen: u64,
    /// `psscan->udf = 0` from an acquisition (`sscanRecord.c:3549`, `:3813`),
    /// for the pass's `check_alarms`: C writes nothing else to UDF, so a
    /// client's UDF put stands until data is next taken.
    pub(crate) udf_clear: bool,
    pub(crate) fire_fwd: bool,
    pub(crate) stamp: bool,
    /// `psscan->scan = 0` from `special()`, for [`Record::take_scan_store`].
    pub(crate) scan_store: Option<ScanType>,
    pub(crate) posts: VecDeque<(FieldIx, EventMask, EpicsValue)>,
}

impl Default for SscanRecord {
    fn default() -> Self {
        let mut r = Self {
            vers: 0.0,
            val: 0.0,
            smsg: PvString::default(),
            cmnd: 0,
            alrt: 0,
            mpts: 0,
            exsc: 0,
            xsc: 0,
            pxsc: 0,
            busy: 0,
            kill: 0,
            wait: 0,
            wcnt: 0,
            awct: 0,
            wtng: 0,
            await_: 0,
            aawait: 0,
            data: 0,
            refd: 0,
            npts: 0,
            fpts: 0,
            ffo: 0,
            cpt: 0,
            bcpt: 0,
            dpt: 0,
            pcpt: 0,
            pasm: 0,
            atime: 0.0,
            pvn: vec![PvString::default(); NUM_PVS],
            bswait: 0,
            aswait: 0,
            nv: vec![0; NUM_PVS],
            pos: Default::default(),
            det: vec![Det::default(); NUM_DET],
            cd: [0.0; 7],
            paus: 0,
            lpau: 0,
            pdly: 0.0,
            ddly: 0.0,
            rdly: 0.0,
            faze: 0,
            acqm: 0,
            acqt: 0,
            dstate: 0,
            copyto: 0,
            tolp: None,
            tlap: None,
            valid: 0,
            links: (0..NUM_LINKS).map(|_| RecDynLink::default()).collect(),
            lpvt: vec![LinkPvt::default(); NUM_LINKS],
            pa: Default::default(),
            pos_buf: Default::default(),
            det_buf: vec![None; NUM_DET],
            null: Vec::new(),
            data_buffer: Vec::new(),
            val_pos: 0,
            val_rdbk: 0,
            val_det: 0,
            val_trig: 0,
            val_atrig: 0,
            acq: [false; NUM_DET],
            pffo: 0,
            frz_fpts: 0,
            frz: [[0; 5]; NUM_POS],
            prev_sm: [0; NUM_POS],
            pos_parms: [PosParms::default(); NUM_POS],
            table_pts: [0; NUM_POS],
            have_fly: false,
            flying: false,
            last_scan_end: Instant::now(),
            scan_err: 0,
            bad_output: false,
            bad_input: false,
            npts_cause: -1,
            num_pos_cb: 0,
            num_trig_cb: 0,
            num_aread_cb: 0,
            num_get_cb: 0,
            prev_acqm: 0,
            prev_num_pts: 0,
            scan_by_search: false,
            user_set_await: false,
            called_by: 0,
            tics: 1.0,
            name: String::new(),
            handle: None,
            inbox: Default::default(),
            prio: CallbackPriority::Low,
            time: WallTime::now(),
            dly_gen: 0,
            udf_clear: false,
            fire_fwd: false,
            stamp: false,
            scan_store: None,
            posts: VecDeque::new(),
        };
        // The `.dbd` initials. Base seeds only its own record types.
        for (o, f) in SSCAN_FIELDS.iter().enumerate() {
            let Some(init) = f.initial else { continue };
            let value = match f.menu {
                Some(m) => m
                    .iter()
                    .position(|c| *c == init)
                    .map(|i| EpicsValue::Enum(i as u16)),
                None => EpicsValue::parse_bytes(f.dbf_type, init.as_bytes()).ok(),
            };
            if let Some(v) = value {
                let _ = r.put_own(o, v);
            }
        }
        r
    }
}

// ---------------------------------------------------------------------
// Value conversion.
// ---------------------------------------------------------------------

fn mismatch(o: usize) -> CaError {
    CaError::TypeMismatch(SSCAN_FIELDS[o].name.into())
}

fn f64_of(o: usize, v: &EpicsValue) -> CaResult<f64> {
    v.to_f64().ok_or_else(|| mismatch(o))
}

fn i16_of(o: usize, v: &EpicsValue) -> CaResult<i16> {
    match v.convert_to(DbFieldType::Short) {
        EpicsValue::Short(x) => Ok(x),
        _ => Err(mismatch(o)),
    }
}

fn i32_of(o: usize, v: &EpicsValue) -> CaResult<i32> {
    match v.convert_to(DbFieldType::Long) {
        EpicsValue::Long(x) => Ok(x),
        _ => Err(mismatch(o)),
    }
}

fn u16_of(o: usize, v: &EpicsValue) -> CaResult<u16> {
    match v.convert_to(DbFieldType::Enum) {
        EpicsValue::Enum(x) => Ok(x),
        _ => Err(mismatch(o)),
    }
}

fn u8_of(o: usize, v: &EpicsValue) -> CaResult<u8> {
    match v.convert_to(DbFieldType::UChar) {
        EpicsValue::UChar(x) => Ok(x),
        EpicsValue::Char(x) => Ok(x),
        _ => Err(mismatch(o)),
    }
}

fn u32_of(o: usize, v: &EpicsValue) -> CaResult<u32> {
    match v.convert_to(DbFieldType::ULong) {
        EpicsValue::ULong(x) => Ok(x),
        _ => Err(mismatch(o)),
    }
}

/// A `DBF_STRING` field of `size` bytes keeps `size - 1` and stops at a NUL.
fn str_of(o: usize, v: &EpicsValue) -> CaResult<PvString> {
    let EpicsValue::String(s) = v else {
        return Err(mismatch(o));
    };
    let size = usize::from(SSCAN_FIELDS[o].size).max(1);
    let b = s.as_bytes();
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    Ok(PvString::from_bytes(&b[..end.min(size - 1)]))
}

/// The elements of an array put, as C's `dbPut` hands them to the buffer.
fn f64s_of(o: usize, v: &EpicsValue) -> CaResult<Vec<f64>> {
    match v.convert_to(DbFieldType::Double) {
        EpicsValue::DoubleArray(a) => Ok(a.to_vec()),
        EpicsValue::Double(x) => Ok(vec![x]),
        _ => Err(mismatch(o)),
    }
}

fn f32s_of(o: usize, v: &EpicsValue) -> CaResult<Vec<f32>> {
    match v.convert_to(DbFieldType::Float) {
        EpicsValue::FloatArray(a) => Ok(a.to_vec()),
        EpicsValue::Float(x) => Ok(vec![x]),
        _ => Err(mismatch(o)),
    }
}

/// C `epicsSnprintf(psscan->smsg, sizeof(psscan->smsg), ...)`.
pub(crate) fn msg(s: &str) -> PvString {
    let size = usize::from(SSCAN_FIELDS[SMSG].size);
    let b = s.as_bytes();
    PvString::from_bytes(&b[..b.len().min(size - 1)])
}

impl SscanRecord {
    // -----------------------------------------------------------------
    // Field access by own index.
    // -----------------------------------------------------------------

    pub(crate) fn get_own(&self, o: usize) -> Option<EpicsValue> {
        use EpicsValue as V;
        Some(match o {
            VERS => V::Double(self.vers),
            VAL => V::Double(self.val),
            SMSG => V::String(self.smsg.clone()),
            CMND => V::Enum(self.cmnd),
            ALRT => V::UChar(self.alrt),
            MPTS => V::Long(self.mpts),
            EXSC => V::Short(self.exsc),
            XSC => V::Short(self.xsc),
            PXSC => V::UChar(self.pxsc),
            BUSY => V::UChar(self.busy),
            KILL => V::UChar(self.kill),
            WAIT => V::Short(self.wait),
            WCNT => V::Short(self.wcnt),
            AWCT => V::Short(self.awct),
            WTNG => V::Short(self.wtng),
            AWAIT => V::Short(self.await_),
            AAWAIT => V::Enum(self.aawait),
            DATA => V::Short(self.data),
            REFD => V::Short(self.refd),
            NPTS => V::Long(self.npts),
            FPTS => V::Enum(self.fpts),
            FFO => V::Enum(self.ffo),
            CPT => V::Long(self.cpt),
            BCPT => V::Long(self.bcpt),
            DPT => V::Long(self.dpt),
            PCPT => V::Long(self.pcpt),
            PASM => V::Enum(self.pasm),
            ATIME => V::Float(self.atime),
            BSWAIT => V::Enum(self.bswait),
            ASWAIT => V::Enum(self.aswait),
            PAUS => V::Enum(self.paus),
            LPAU => V::Enum(self.lpau),
            PDLY => V::Float(self.pdly),
            DDLY => V::Float(self.ddly),
            RDLY => V::Float(self.rdly),
            FAZE => V::Enum(self.faze),
            ACQM => V::Enum(self.acqm),
            ACQT => V::Enum(self.acqt),
            DSTATE => V::Enum(self.dstate),
            COPYTO => V::Long(self.copyto),
            _ if (PV0..PV0 + NUM_PVS).contains(&o) => V::String(self.pvn[o - PV0].clone()),
            _ if (NV0..NV0 + NUM_PVS).contains(&o) => V::Enum(self.nv[o - NV0]),
            _ if (POS0..DET0).contains(&o) => {
                let (i, off) = ((o - POS0) / POS_SIZE, (o - POS0) % POS_SIZE);
                let q = &self.pos[i];
                match off {
                    p::PP => V::Double(q.pp),
                    p::CV => V::Double(q.cv),
                    p::DV => V::Double(q.dv),
                    p::LV => V::Double(q.lv),
                    p::SP => V::Double(q.sp),
                    p::SI => V::Double(q.si),
                    p::EP => V::Double(q.ep),
                    p::CP => V::Double(q.cp),
                    p::WD => V::Double(q.wd),
                    p::R_CV => V::Double(q.r_cv),
                    p::R_LV => V::Double(q.r_lv),
                    p::R_DL => V::Double(q.r_dl),
                    p::HR => V::Double(q.hr),
                    p::LR => V::Double(q.lr),
                    p::PA => V::DoubleArray(self.pa[i].clone().into()),
                    p::RA | p::CA => V::DoubleArray(self.pos_array(i, off == p::RA).into()),
                    p::FS => V::Enum(q.fs),
                    p::FI => V::Enum(q.fi),
                    p::FE => V::Enum(q.fe),
                    p::FC => V::Enum(q.fc),
                    p::FW => V::Enum(q.fw),
                    p::SM => V::Enum(q.sm),
                    p::AR => V::Enum(q.ar),
                    p::EU => V::String(q.eu.clone()),
                    _ => V::Short(q.pr),
                }
            }
            _ if (DET0..CD0).contains(&o) => {
                let (i, off) = ((o - DET0) / DET_SIZE, (o - DET0) % DET_SIZE);
                let q = &self.det[i];
                match off {
                    d::HR => V::Double(q.hr),
                    d::LR => V::Double(q.lr),
                    d::DA | d::CA => V::FloatArray(self.det_array(i, off == d::DA).into()),
                    d::CV => V::Float(q.cv),
                    d::LV => V::Float(q.lv),
                    d::NE => V::ULong(q.ne),
                    d::EU => V::String(q.eu.clone()),
                    _ => V::Short(q.pr),
                }
            }
            _ if (CD0..PAUS).contains(&o) => V::Float(self.cd[o - CD0]),
            // RPVT, TOLP, TLAP: C pointers and stamps, no value a client reads.
            _ => return None,
        })
    }

    pub(crate) fn put_own(&mut self, o: usize, v: EpicsValue) -> CaResult<()> {
        let v = &v;
        match o {
            VERS => self.vers = f64_of(o, v)?,
            VAL => self.val = f64_of(o, v)?,
            SMSG => self.smsg = str_of(o, v)?,
            CMND => self.cmnd = u16_of(o, v)?,
            ALRT => self.alrt = u8_of(o, v)?,
            MPTS => self.mpts = i32_of(o, v)?,
            EXSC => self.exsc = i16_of(o, v)?,
            XSC => self.xsc = i16_of(o, v)?,
            PXSC => self.pxsc = u8_of(o, v)?,
            BUSY => self.busy = u8_of(o, v)?,
            KILL => self.kill = u8_of(o, v)?,
            WAIT => self.wait = i16_of(o, v)?,
            WCNT => self.wcnt = i16_of(o, v)?,
            AWCT => self.awct = i16_of(o, v)?,
            WTNG => self.wtng = i16_of(o, v)?,
            AWAIT => self.await_ = i16_of(o, v)?,
            AAWAIT => self.aawait = u16_of(o, v)?,
            DATA => self.data = i16_of(o, v)?,
            REFD => self.refd = i16_of(o, v)?,
            NPTS => self.npts = i32_of(o, v)?,
            FPTS => self.fpts = u16_of(o, v)?,
            FFO => self.ffo = u16_of(o, v)?,
            CPT => self.cpt = i32_of(o, v)?,
            BCPT => self.bcpt = i32_of(o, v)?,
            DPT => self.dpt = i32_of(o, v)?,
            PCPT => self.pcpt = i32_of(o, v)?,
            PASM => self.pasm = u16_of(o, v)?,
            ATIME => self.atime = f64_of(o, v)? as f32,
            BSWAIT => self.bswait = u16_of(o, v)?,
            ASWAIT => self.aswait = u16_of(o, v)?,
            PAUS => self.paus = u16_of(o, v)?,
            LPAU => self.lpau = u16_of(o, v)?,
            PDLY => self.pdly = f64_of(o, v)? as f32,
            DDLY => self.ddly = f64_of(o, v)? as f32,
            RDLY => self.rdly = f64_of(o, v)? as f32,
            FAZE => self.faze = u16_of(o, v)?,
            ACQM => self.acqm = u16_of(o, v)?,
            ACQT => self.acqt = u16_of(o, v)?,
            DSTATE => self.dstate = u16_of(o, v)?,
            COPYTO => self.copyto = i32_of(o, v)?,
            _ if (PV0..PV0 + NUM_PVS).contains(&o) => self.pvn[o - PV0] = str_of(o, v)?,
            _ if (NV0..NV0 + NUM_PVS).contains(&o) => self.nv[o - NV0] = u16_of(o, v)?,
            _ if (POS0..DET0).contains(&o) => {
                let (i, off) = ((o - POS0) / POS_SIZE, (o - POS0) % POS_SIZE);
                match off {
                    p::PA => return self.put_pa(i, f64s_of(o, v)?),
                    p::RA | p::CA => {
                        let n = self.mpts_len();
                        let which = self.pos_served(off == p::RA);
                        let src = f64s_of(o, v)?;
                        let buf = &mut self.pos_buf[i][which];
                        let k = src.len().min(n).min(buf.len());
                        buf[..k].copy_from_slice(&src[..k]);
                        return Ok(());
                    }
                    _ => {}
                }
                let q = &mut self.pos[i];
                match off {
                    p::PP => q.pp = f64_of(o, v)?,
                    p::CV => q.cv = f64_of(o, v)?,
                    p::DV => q.dv = f64_of(o, v)?,
                    p::LV => q.lv = f64_of(o, v)?,
                    p::SP => q.sp = f64_of(o, v)?,
                    p::SI => q.si = f64_of(o, v)?,
                    p::EP => q.ep = f64_of(o, v)?,
                    p::CP => q.cp = f64_of(o, v)?,
                    p::WD => q.wd = f64_of(o, v)?,
                    p::R_CV => q.r_cv = f64_of(o, v)?,
                    p::R_LV => q.r_lv = f64_of(o, v)?,
                    p::R_DL => q.r_dl = f64_of(o, v)?,
                    p::HR => q.hr = f64_of(o, v)?,
                    p::LR => q.lr = f64_of(o, v)?,
                    p::FS => q.fs = u16_of(o, v)?,
                    p::FI => q.fi = u16_of(o, v)?,
                    p::FE => q.fe = u16_of(o, v)?,
                    p::FC => q.fc = u16_of(o, v)?,
                    p::FW => q.fw = u16_of(o, v)?,
                    p::SM => q.sm = u16_of(o, v)?,
                    p::AR => q.ar = u16_of(o, v)?,
                    p::EU => q.eu = str_of(o, v)?,
                    _ => q.pr = i16_of(o, v)?,
                }
            }
            _ if (DET0..CD0).contains(&o) => {
                let (i, off) = ((o - DET0) / DET_SIZE, (o - DET0) % DET_SIZE);
                if off == d::DA || off == d::CA {
                    let src = f32s_of(o, v)?;
                    // A write to a detector nobody acquires lands in C's
                    // shared `nullArray`; the port keeps that array zero.
                    if let Some((buf, which)) = self.det_served(i, off == d::DA) {
                        let buf = &mut buf[which];
                        let k = src.len().min(buf.len());
                        buf[..k].copy_from_slice(&src[..k]);
                    }
                    return Ok(());
                }
                let q = &mut self.det[i];
                match off {
                    d::HR => q.hr = f64_of(o, v)?,
                    d::LR => q.lr = f64_of(o, v)?,
                    d::CV => q.cv = f64_of(o, v)? as f32,
                    d::LV => q.lv = f64_of(o, v)? as f32,
                    d::NE => q.ne = u32_of(o, v)?,
                    d::EU => q.eu = str_of(o, v)?,
                    _ => q.pr = i16_of(o, v)?,
                }
            }
            _ if (CD0..PAUS).contains(&o) => self.cd[o - CD0] = f64_of(o, v)? as f32,
            _ => return Err(CaError::FieldNotFound(SSCAN_FIELDS[o].name.into())),
        }
        Ok(())
    }

    /// C `dbPut` into `p_pa`, then `put_array_info` (`sscanRecord.c:2000-2045`).
    fn put_pa(&mut self, i: usize, src: Vec<f64>) -> CaResult<()> {
        let n_new = src.len().min(self.pa[i].len());
        self.pa[i][..n_new].copy_from_slice(&src[..n_new]);
        self.clamp_npts();
        self.table_pts[i] = n_new;
        if (n_new as i64) < i64::from(self.npts) {
            self.set_smsg(&format!("Pts in P{} Table < # of Steps.", i + 1));
            if self.alrt == 0 {
                self.set_alrt(1);
            }
        } else {
            self.set_smsg("");
            if self.alrt != 0 {
                self.set_alrt(0);
            }
        }
        Ok(())
    }

    pub(crate) fn mpts_len(&self) -> usize {
        self.mpts.max(0) as usize
    }

    /// `get_array_info` for `PnRA` (`ra`) / `PnCA`: the buffer index served.
    fn pos_served(&self, ra: bool) -> usize {
        if ra || self.dstate >= dstate::PACKED {
            self.valid
        } else {
            1 - self.valid
        }
    }

    pub(crate) fn pos_array(&self, i: usize, ra: bool) -> Vec<f64> {
        self.pos_buf[i][self.pos_served(ra)].clone()
    }

    fn det_served(&mut self, i: usize, da: bool) -> Option<(&mut [Vec<f32>; 2], usize)> {
        let which = match self.det_which(i, da) {
            Served::A => 0,
            Served::B => 1,
            Served::Null => return None,
        };
        self.det_buf[i].as_mut().map(|b| (b, which))
    }

    /// `get_array_info` for `DnnDA` (`da`) / `DnnCA`.
    pub(crate) fn det_which(&self, i: usize, da: bool) -> Served {
        let live = self.acq[i] || (i < NUM_POS && self.faze == faze::PREVIEW);
        if !live || self.det_buf[i].is_none() {
            return Served::Null;
        }
        let which = if da || self.dstate >= dstate::PACKED {
            self.valid
        } else {
            1 - self.valid
        };
        if which == 0 { Served::A } else { Served::B }
    }

    pub(crate) fn det_array(&self, i: usize, da: bool) -> Vec<f32> {
        match (self.det_which(i, da), &self.det_buf[i]) {
            (Served::A, Some(b)) => b[0].clone(),
            (Served::B, Some(b)) => b[1].clone(),
            _ => self.null.clone(),
        }
    }

    pub(crate) fn fill(&self) -> usize {
        1 - self.valid
    }

    // -----------------------------------------------------------------
    // Posting: C `db_post_events`, the value read at the call.
    // -----------------------------------------------------------------

    pub(crate) fn post_mask(&mut self, o: usize, mask: EventMask) {
        if let Some(v) = self.get_own(o) {
            self.posts.push_back((fix(o), mask, v));
        }
    }

    /// C `POST(&field)`: `db_post_events(psscan, &field, DBE_VALUE)`.
    pub(crate) fn post(&mut self, o: usize) {
        self.post_mask(o, EventMask::VALUE);
    }

    pub(crate) fn post_log(&mut self, o: usize) {
        self.post_mask(o, EventMask::VALUE | EventMask::LOG);
    }

    pub(crate) fn set_smsg(&mut self, s: &str) {
        self.smsg = msg(s);
        self.post(SMSG);
    }

    pub(crate) fn set_alrt(&mut self, a: u8) {
        self.alrt = a;
        self.post(ALRT);
    }

    pub(crate) fn set_faze(&mut self, f: u16) {
        self.faze = f;
        self.post(FAZE);
    }

    pub(crate) fn set_dstate(&mut self, s: u16) {
        self.dstate = s;
        self.post(DSTATE);
    }

    /// C's npts guard at the top of `process`, `special` and
    /// `put_array_info`: autosave may have changed it after `init_record`.
    pub(crate) fn clamp_npts(&mut self) {
        if self.npts > self.mpts {
            self.npts = self.mpts;
            self.post(NPTS);
        }
        if self.npts <= 0 {
            self.npts = 1;
            self.post(NPTS);
        }
    }

    pub(crate) fn errlog(&self, s: &str) {
        epics_base_rs::runtime::log::errlog_printf(&format!("{s}\n"));
    }

    // -----------------------------------------------------------------
    // RSET metadata (`sscanRecord.c:2047-2150`).
    // -----------------------------------------------------------------

    /// `(units, precision, graphic limits (upper, lower))` C's `get_units`,
    /// `get_precision` and `get_graphic_double` answer for own field `o`.
    fn metadata(&self, o: usize) -> (PvString, i16, Option<(f64, f64)>) {
        let seven = |eu: &PvString| {
            let b = eu.as_bytes();
            PvString::from_bytes(&b[..b.len().min(7)])
        };
        if (POS0..DET0).contains(&o) {
            let q = &self.pos[(o - POS0) / POS_SIZE];
            return (seven(&q.eu), q.pr.clamp(0, 10), Some((q.hr, q.lr)));
        }
        // C tests `fieldIndex >= sscanRecordP1PP` first, and every detector
        // field is past P1PP, so its `else if` detector branch never runs and
        // a detector reports no units, precision 3 and the field-type limits.
        // The detector branch is the one meant for them.
        if (DET0..CD0).contains(&o) {
            let q = &self.det[(o - DET0) / DET_SIZE];
            return (seven(&q.eu), q.pr.clamp(0, 10), Some((q.hr, q.lr)));
        }
        let prec = if o == ATIME { 1 } else { 3 };
        (PvString::default(), prec, None)
    }

    // -----------------------------------------------------------------
    // Work done outside the record's own pass.
    // -----------------------------------------------------------------

    /// C `scanOnce(psscan)`.
    pub(crate) fn scan_once(&self) {
        if let Some(h) = &self.handle {
            h.scan_once(&self.name);
        }
    }

    /// Run `f` on this record under its lock from a callback task.
    pub(crate) fn spawn_locked(&self, f: impl FnOnce(&mut SscanRecord) + Send + 'static) {
        let Some(h) = self.handle.clone() else {
            return;
        };
        let name = self.name.clone();
        epics_base_rs::runtime::task::spawn_background(self.prio, async move {
            with_sscan(&h, &name, |s, _| f(s));
        });
    }

    /// C `callbackRequest(&precPvt->doPutsCallback)`.
    pub(crate) fn request_do_puts(&self) {
        self.spawn_locked(|s| s.do_puts());
    }

    /// C `callbackRequestDelayed(&precPvt->dlyCallback, secs)`: one timer,
    /// so a new request replaces the pending one.
    pub(crate) fn request_delay(&mut self, secs: f32) {
        self.dly_gen += 1;
        let generation = self.dly_gen;
        let Some(h) = self.handle.clone() else {
            return;
        };
        let name = self.name.clone();
        let wait = epics_base_rs::runtime::time::duration_from_secs(f64::from(secs));
        epics_base_rs::runtime::task::spawn_background_after(self.prio, wait, async move {
            with_sscan(&h, &name, |s, _| {
                if s.dly_gen == generation {
                    s.delay_callback();
                }
            });
        });
    }

    /// C `epicsTimerCancel(precPvt->dlyCallback.timer)`.
    pub(crate) fn cancel_delay(&mut self) {
        self.dly_gen += 1;
    }

    /// `psscan->scan = 0; POST(&psscan->scan)` (`sscanRecord.c:1571`): a
    /// store, so the record stays on the scan list it was on, as in C.
    pub(crate) fn store_scan_passive(&mut self) {
        self.scan_store = Some(ScanType::Passive);
        self.posts
            .push_back((FieldIx(SCAN_IX), EventMask::VALUE, EpicsValue::Enum(0)));
    }
}

/// C's callbacks reach the record by pointer and do not take its lock
/// (`sscanRecord.c:2646` leaves `dbScanLock` commented out); the port takes
/// it, which is safe because no callback here waits on the record's pass.
pub(crate) fn with_sscan(
    h: &AsyncDbHandle,
    name: &str,
    f: impl FnOnce(&mut SscanRecord, OwnFrame<'_>),
) {
    h.with_own(name, |r, own| {
        if let Some(s) = r.as_any_mut().and_then(|a| a.downcast_mut::<SscanRecord>()) {
            f(s, own);
        }
    });
}

impl Record for SscanRecord {
    fn record_type(&self) -> &'static str {
        "sscan"
    }

    /// Pass 1 is [`Self::init_record_own`]'s.
    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 0 {
            self.init_pass0();
        }
        Ok(())
    }

    fn init_record_own(&mut self, _reads: &mut InitLinkReads, own: OwnFrame<'_>) -> CaResult<()> {
        self.init_pass1(own);
        Ok(())
    }

    fn process_own(&mut self, own: OwnFrame<'_>) -> CaResult<ProcessOutcome> {
        self.fire_fwd = false;
        Ok(if self.process_pass(own) {
            ProcessOutcome::complete()
        } else {
            ProcessOutcome::complete_no_emit()
        })
    }

    fn special_own(&mut self, field: &str, after: bool, own: OwnFrame<'_>) -> CaResult<()> {
        let Some(o) = own_index(field) else {
            return Ok(());
        };
        // C `dbPutSpecial` calls the rset's `special` only for
        // `special >= SPC_MOD`.
        match SSCAN_FIELDS[o].special {
            Special::Mod | Special::Record(_) => {}
            _ => return Ok(()),
        }
        if self.special_field(own, o, after) {
            Ok(())
        } else {
            Err(CaError::PutDisabled(field.into()))
        }
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        self.get_own(own_index(name)?)
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match own_index(name) {
            Some(o) => self.put_own(o, value),
            None => Err(CaError::FieldNotFound(name.into())),
        }
    }

    fn declared_fields(&self) -> &'static [FieldDesc] {
        SSCAN_FIELDS
    }

    fn declared_dbaddr_fields(&self) -> Option<&'static [&'static str]> {
        Some(SSCAN_DBADDR)
    }

    fn declared_noaccess_fields(&self) -> &'static [&'static str] {
        SSCAN_NOACCESS
    }

    /// `cvt_dbaddr`: every array serves MPTS elements.
    fn dbaddr_capacity(&self, _field: &str) -> Option<u32> {
        Some(self.mpts.max(0) as u32)
    }

    fn dbaddr_capacity_inputs(&self) -> Option<&'static [&'static str]> {
        Some(&[])
    }

    fn field_metadata_override(&self, field: &str) -> Option<FieldMetadataOverride> {
        let (units, precision, limits) = self.metadata(own_index(field)?);
        Some(FieldMetadataOverride {
            units: Some(units),
            precision: Some(precision),
            disp_limits: limits,
            ctrl_limits: limits,
            ..Default::default()
        })
    }

    fn cycle_hooks(&self) -> CycleHooks {
        CycleHooks::HAND_POSTED
            .union(CycleHooks::STAMP_REQUEST)
            .union(CycleHooks::CONTEXT)
            .union(CycleHooks::OWN_FRAME)
    }

    fn set_process_context(&mut self, ctx: &ProcessContext) {
        self.time = ctx.time;
        self.prio = ctx.callback_priority;
    }

    fn set_async_context(&mut self, name: RecordName, db: AsyncDbHandle) {
        self.name = String::from_utf8_lossy(name.as_bytes()).into_owned();
        self.handle = Some(db);
    }

    fn take_time_stamp_request(&mut self) -> bool {
        std::mem::take(&mut self.stamp)
    }

    fn take_scan_store(&mut self) -> Option<ScanType> {
        self.scan_store.take()
    }

    fn take_cycle_posted_value(&mut self) -> Option<(FieldIx, EventMask, EpicsValue)> {
        self.posts.pop_front()
    }

    /// Every post a pass makes is one of C's `db_post_events` calls.
    fn process_posted_fields(&self) -> Option<DeclaredFields> {
        Some(&const { epics_base_rs::server::record::declared_set(SSCAN_FIELDS, []) })
    }

    fn process_posts_value_monitor(&self) -> bool {
        false
    }

    /// C calls `recGblFwdLink` only when a scan completes or is killed.
    fn should_fire_forward_link(&self) -> bool {
        self.fire_fwd
    }

    fn clears_udf(&self) -> bool {
        false
    }

    fn check_alarms(&mut self, common: &mut epics_base_rs::server::record::CommonFields) {
        if std::mem::take(&mut self.udf_clear) {
            common.udf = 0;
        }
    }

    fn raises_udf_alarm(&self) -> bool {
        false
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

impl SscanRecord {
    /// `init_record` pass 0 (`sscanRecord.c:736-866`).
    fn init_pass0(&mut self) {
        self.vers = VERSION;
        if self.mpts < DEF_WF_SIZE {
            self.mpts = DEF_WF_SIZE;
        }
        let quantum = epics_base_rs::runtime::time::thread_sleep_quantum();
        self.tics = if quantum > 0.0 {
            (1.0 / quantum).round()
        } else {
            1000.0
        };
        self.faze = faze::IDLE;
        for i in 0..NUM_POS {
            self.prev_sm[i] = self.pos[i].sm;
        }
        let n = self.mpts_len();
        self.pa = std::array::from_fn(|_| vec![0.0; n]);
        self.valid = 0;
        self.pos_buf = std::array::from_fn(|_| [vec![0.0; n], vec![0.0; n]]);
        self.null = vec![0.0; n];
        self.data_buffer = vec![0.0; n];
        for i in 0..NUM_DET {
            let li = D1_IN + i;
            if i < 4 {
                self.det_buf[i] = Some([vec![0.0; n], vec![0.0; n]]);
                self.lpvt[li].nelem = 1;
            } else {
                self.det_buf[i] = None;
                self.lpvt[li].nelem = 0;
            }
        }
    }

    /// `init_record` pass 1 (`sscanRecord.c:867-930`).
    fn init_pass1(&mut self, own: OwnFrame<'_>) {
        self.last_scan_end = Instant::now();
        if self.npts > self.mpts {
            self.npts = self.mpts;
        }
        if self.npts <= 0 {
            self.npts = 1;
        }
        self.npts_cause = -1;
        self.changed_npts();
        if self.ffo != 0 {
            self.save_frz_flags();
            self.reset_frz_flags();
        }
        self.exsc = 0;
        self.xsc = 0;
        self.pxsc = 0;
        self.data = 0;
        for i in 0..NUM_PVS {
            if is_blank(&self.pvn[i]) {
                self.pvn[i] = PvString::default();
                self.nv[i] = NO_PV;
            } else if !self.pvn[i].as_bytes().is_empty() {
                self.nv[i] = PV_NC;
                self.lookup_pv(own, i);
            } else {
                self.nv[i] = NO_PV;
            }
        }
        self.dstate = dstate::UNPACKED;
        // Nobody can be subscribed yet: C's init posts reach no one.
        self.posts.clear();
    }
}

/// C `isBlank`: non-empty and all whitespace.
pub(crate) fn is_blank(s: &PvString) -> bool {
    let b = s.as_bytes();
    !b.is_empty() && b.iter().all(|c| c.is_ascii_whitespace())
}

/// C `NINT`.
pub(crate) fn nint(x: f64) -> f64 {
    if x >= 0.0 {
        (x + 0.5).trunc()
    } else {
        (x - 0.5).trunc()
    }
}
