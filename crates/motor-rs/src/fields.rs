use std::collections::VecDeque;

use asyn_rs::interfaces::motor::PostMessage;
use epics_base_rs::server::device_support::DevicePost;
use epics_base_rs::types::PvString;

use crate::flags::*;

/// Position-related fields.
#[derive(Debug, Clone)]
pub struct PositionFields {
    pub val: f64,
    pub rbv: f64,
    pub rlv: f64,
    pub off: f64,
    pub diff: f64,
    /// Raw step difference (DBF_LONG).
    pub rdif: i32,
    pub dval: f64,
    pub drbv: f64,
    pub rval: i32,
    pub rrbv: i32,
    pub rmp: i32,
    pub rep: i32,
}

impl Default for PositionFields {
    fn default() -> Self {
        Self {
            val: 0.0,
            rbv: 0.0,
            rlv: 0.0,
            off: 0.0,
            diff: 0.0,
            rdif: 0,
            dval: 0.0,
            drbv: 0.0,
            rval: 0,
            rrbv: 0,
            rmp: 0,
            rep: 0,
        }
    }
}

/// Coordinate conversion fields.
#[derive(Debug, Clone)]
pub struct ConversionFields {
    pub dir: MotorDir,
    pub foff: FreezeOffset,
    /// FOF/VOF momentary command fields (C special, motorRecord.cc:
    /// 2975-2984): any write forces FOFF to Frozen/Variable. The cells
    /// only echo the last written value back on reads, like the C
    /// fields the special() handler leaves untouched.
    pub fof: i16,
    pub vof: i16,
    pub set: bool,
    /// SSET/SUSE momentary command fields (C special, motorRecord.cc:
    /// 2963-2972): any write forces SET to Set/Use mode.
    pub sset: i16,
    pub suse: i16,
    pub igset: i16,
    pub mres: f64,
    pub eres: f64,
    pub srev: i32,
    pub urev: f64,
    pub ueip: bool,
    pub urip: bool,
    pub rres: f64,
    pub rdbl_value: Option<f64>,
    /// Restore mode for autosaved DVAL at init (C: `2906f3d8`, PR #160).
    pub rstm: RestoreMode,
    /// External URIP readback link is in error. C: `db5da2f0` (2017-05),
    /// `7493d50b` (2018-04). When `urip` is true and this flag is set, the
    /// record stops any in-progress motion and refuses to start new ones.
    pub rdbl_error: bool,
    /// Block LOAD_POS (SetPosition). epics-modules/motor issue #231 — for
    /// absolute-encoder axes where redefining the position would leave
    /// DVAL/OFF inconsistent with the controller. When set, SET-mode
    /// coordinate redefinition and RSTM restore are both refused.
    pub loadpos_blocked: bool,
}

impl Default for ConversionFields {
    fn default() -> Self {
        Self {
            dir: MotorDir::Pos,
            foff: FreezeOffset::Variable,
            fof: 0,
            vof: 0,
            set: false,
            sset: 0,
            suse: 0,
            igset: 0,
            // dbd-true default — MRES has no initial(); init sets 1.0 and
            // marks M_MRES (motorRecord.cc:3917-3921).
            mres: 0.0,
            eres: 0.0,
            srev: 200,
            // dbd-true default — UREV has no initial(). The init-time
            // resolution reconcile (C check_speed_and_resolution,
            // motorRecord.cc:3912-3916) treats a nonzero UREV as
            // "configured by the .db" and derives MRES from it; a
            // nonzero default here would clobber every loaded MRES.
            urev: 0.0,
            ueip: false,
            urip: false,
            rres: 0.0,
            rdbl_value: None,
            rstm: RestoreMode::NearZero,
            rdbl_error: false,
            loadpos_blocked: false,
        }
    }
}

/// Velocity and acceleration fields.
#[derive(Debug, Clone)]
pub struct VelocityFields {
    pub velo: f64,
    pub vbas: f64,
    pub vmax: f64,
    pub s: f64,
    pub sbas: f64,
    pub smax: f64,
    pub accl: f64,
    /// Acceleration in EGU/sec² (C: `36177f7b`, PR #122 / #203).
    /// Cross-calculated with ACCL via `(velo - vbas) / accl` when ACCU is `Accl`,
    /// or `(velo - vbas) / accs` when ACCU is `Accs`.
    pub accs: f64,
    /// Which of ACCL/ACCS is the master (autosave target). C: `63bfe5d0`.
    pub accu: AccsUsed,
    pub bvel: f64,
    pub bacc: f64,
    pub hvel: f64,
    pub jvel: f64,
    pub jar: f64,
    pub sbak: f64,
}

impl Default for VelocityFields {
    fn default() -> Self {
        Self {
            // VELO/BVEL/ACCS have no initial() in motorRecord.dbd — they
            // load as 0.0 like JVEL/HVEL below; only ACCL ("0.2") and
            // BACC ("0.5") carry dbd initials. The init pass derives the
            // speed family from the configured side (S/SBAK drive
            // VELO/BVEL when nonzero, ACCS derives from ACCL) in
            // check_speed_and_resolution (C 3954-4067).
            velo: 0.0,
            vbas: 0.0,
            vmax: 0.0,
            s: 0.0,
            sbas: 0.0,
            smax: 0.0,
            accl: 0.2,
            accs: 0.0,
            accu: AccsUsed::Accl,
            bvel: 0.0,
            bacc: 0.5,
            // JVEL/HVEL have no initial() in motorRecord.dbd — 0.0 means
            // "not configured"; init derives JVEL from VELO and HVEL from
            // VBAS (C check_speed_and_resolution, motorRecord.cc:4055-4067,
            // ported in motor_sync_speed_at_init).
            hvel: 0.0,
            jvel: 0.0,
            jar: 0.0,
            sbak: 0.0,
        }
    }
}

/// Retry and backlash fields.
#[derive(Debug, Clone)]
pub struct RetryFields {
    pub bdst: f64,
    pub frac: f32,
    pub rdbd: f64,
    pub spdb: f64,
    pub rtry: i16,
    pub rmod: RetryMode,
    pub rcnt: i16,
    pub miss: i16,
}

impl Default for RetryFields {
    fn default() -> Self {
        Self {
            bdst: 0.0,
            frac: 1.0,
            rdbd: 0.0,
            spdb: 0.0,
            rtry: 10,
            rmod: RetryMode::Default,
            rcnt: 0,
            miss: 0,
        }
    }
}

/// Limit fields.
#[derive(Debug, Clone)]
pub struct LimitFields {
    pub hlm: f64,
    pub llm: f64,
    pub dhlm: f64,
    pub dllm: f64,
    /// Raw high limit (in motor steps) — invariant for MRES changes
    pub rhlm: f64,
    /// Raw low limit (in motor steps) — invariant for MRES changes
    pub rllm: f64,
    pub lvio: i16,
    pub hls: i16,
    pub lls: i16,
    /// Raw high limit switch readback (C 3727: `rhls = msta.Bits.RA_PLUS_LS`),
    /// unmapped by DIR/MRES polarity — HLS/LLS derive from these (C 3733-3734).
    pub rhls: i16,
    /// Raw low limit switch readback (C 3728: `rlls = msta.Bits.RA_MINUS_LS`).
    pub rlls: i16,
    pub hlsv: i16,
}

impl Default for LimitFields {
    fn default() -> Self {
        Self {
            hlm: 0.0,
            llm: 0.0,
            dhlm: 0.0,
            dllm: 0.0,
            rhlm: 0.0,
            rllm: 0.0,
            // C dbd initial("1"); init_record resets it before the
            // initial-readback check (motorRecord.cc:734-743).
            lvio: 1,
            hls: 0,
            lls: 0,
            rhls: 0,
            rlls: 0,
            hlsv: 0,
        }
    }
}

/// Control fields (user commands).
#[derive(Debug, Clone)]
pub struct ControlFields {
    pub spmg: SpmgMode,
    pub stop: i16,
    pub homf: i16,
    pub homr: i16,
    pub jogf: i16,
    pub jogr: i16,
    pub twf: i16,
    pub twr: i16,
    pub twv: f64,
    pub cnen: bool,
}

impl Default for ControlFields {
    fn default() -> Self {
        Self {
            spmg: SpmgMode::Go,
            stop: 0,
            homf: 0,
            homr: 0,
            jogf: 0,
            jogr: 0,
            twf: 0,
            twr: 0,
            twv: 0.0,
            cnen: false,
        }
    }
}

/// Status fields.
#[derive(Debug, Clone)]
pub struct StatusFields {
    pub dmov: i16,
    pub movn: i16,
    pub msta: MstaFlags,
    pub mip: MipFlags,
    pub phase: MotionPhase,
    pub cdir: i16,
    pub tdir: i16,
    pub athm: i16,
    pub stup: i16,
    /// Raw velocity reported by the driver, in motor steps/sec.
    /// C: `motorRecord.dbd` `field(RVEL,DBF_LONG)` "Raw Velocity"; devMotorAsyn
    /// fills it with `floor(status.velocity)`.
    pub rvel: i32,
    /// The driver ignores a base velocity, so accel math drops VBAS
    /// (epics-modules/motor #76, unmerged). Not on MSTA: C never sets
    /// bit 15.
    pub vbas_unsupported: bool,
}

impl Default for StatusFields {
    fn default() -> Self {
        Self {
            dmov: 1,
            movn: 0,
            msta: MstaFlags::empty(),
            mip: MipFlags::empty(),
            phase: MotionPhase::Idle,
            cdir: 0,
            tdir: 0,
            athm: 0,
            stup: 0,
            rvel: 0,
            vbas_unsupported: false,
        }
    }
}

/// PID fields (placeholder).
#[derive(Debug, Clone, Default)]
pub struct PidFields {
    pub pcof: f64,
    pub icof: f64,
    pub dcof: f64,
}

/// Display fields.
#[derive(Debug, Clone)]
pub struct DisplayFields {
    pub egu: PvString,
    pub prec: i16,
    pub adel: f64,
    pub mdel: f64,
    pub alst: f64,
    pub mlst: f64,
    /// High operating range (C `motorRecord.dbd` HOPR, DBF_DOUBLE).
    pub hopr: f64,
    /// Low operating range (C `motorRecord.dbd` LOPR, DBF_DOUBLE).
    pub lopr: f64,
}

impl Default for DisplayFields {
    fn default() -> Self {
        Self {
            egu: PvString::new(),
            prec: 0,
            adel: 0.0,
            mdel: 0.0,
            alst: 0.0,
            mlst: 0.0,
            hopr: 0.0,
            lopr: 0.0,
        }
    }
}

/// Database-link and menu fields imported from the public C `motorRecord.dbd`
/// surface (motorRecord.dbd:233-265, 739-760). The link strings hold the
/// `DBF_INLINK` / `DBF_OUTLINK` specification clients open by C name; `init`,
/// `prem` and `post` are the `DBF_STRING` startup, pre-move and post-move
/// commands. `omsl` is the `menuOmsl` selector (0 = supervisory, 1 =
/// closed_loop).
#[derive(Debug, Clone, Default)]
pub struct LinkFields {
    pub out: PvString,
    /// `field(CARD,DBF_SHORT)` (motorRecord.dbd:239-243), the one field that is
    /// a pure function of [`Self::out`]: `init_record` switches on the OUT link
    /// TYPE and stores the card number, -1, or 0 (`motorRecord.cc:653-670`).
    /// It lives beside the link it is derived from; `motor_derive_card`
    /// writes it, and `motor_failed_dset_init` forces C's -1 after it.
    pub card: i16,
    pub rdbl: PvString,
    pub dol: PvString,
    pub rlnk: PvString,
    pub stoo: PvString,
    pub dinp: PvString,
    pub rinp: PvString,
    pub init: PvString,
    pub prem: PvString,
    /// Shared with the device a move armed with it, which reads it when
    /// that move is done.
    pub post: PostMessage,
    pub omsl: i16,
}

/// Alarm-limit fields imported from the public C `motorRecord.dbd` surface
/// (motorRecord.dbd:396-441). `hhsv` / `llsv` are `menuAlarmSevr` selectors
/// (0 = NO_ALARM, 1 = MINOR, 2 = MAJOR, 3 = INVALID).
#[derive(Debug, Clone, Default)]
pub struct AlarmFields {
    pub hihi: f64,
    pub high: f64,
    pub low: f64,
    pub lolo: f64,
    pub hhsv: i16,
    pub llsv: i16,
}

/// Timing fields.
#[derive(Debug, Clone)]
pub struct TimingFields {
    pub dly: f64,
    pub ntm: bool,
    /// C motorRecord.dbd:789 — NTMF is DBF_USHORT, not a float.
    pub ntmf: u16,
}

impl Default for TimingFields {
    fn default() -> Self {
        Self {
            dly: 0.0,
            ntm: true,
            ntmf: 2,
        }
    }
}

/// A row of the record's post table: C `mmap_field`'s bits as they sit in
/// MMAP, `nmap_field`'s 32 up (motorRecord.cc:333-389), and between and past
/// them the posts C makes without a mark.
pub mod m {
    pub const VAL: u64 = 1 << 0;
    pub const DVAL: u64 = 1 << 1;
    pub const HLM: u64 = 1 << 2;
    pub const LLM: u64 = 1 << 3;
    pub const DMOV: u64 = 1 << 4;
    pub const SPMG: u64 = 1 << 5;
    pub const RCNT: u64 = 1 << 6;
    pub const MRES: u64 = 1 << 7;
    pub const ERES: u64 = 1 << 8;
    pub const UEIP: u64 = 1 << 9;
    pub const STOP: u64 = 1 << 10;
    pub const LVIO: u64 = 1 << 11;
    pub const RVAL: u64 = 1 << 12;
    pub const RLV: u64 = 1 << 13;
    pub const OFF: u64 = 1 << 14;
    pub const RBV: u64 = 1 << 15;
    pub const DHLM: u64 = 1 << 16;
    pub const DLLM: u64 = 1 << 17;
    pub const DRBV: u64 = 1 << 18;
    pub const MOVN: u64 = 1 << 19;
    pub const HLS: u64 = 1 << 20;
    pub const LLS: u64 = 1 << 21;
    pub const RRBV: u64 = 1 << 22;
    pub const MSTA: u64 = 1 << 23;
    pub const ATHM: u64 = 1 << 24;
    pub const TDIR: u64 = 1 << 25;
    pub const MIP: u64 = 1 << 26;
    pub const DIFF: u64 = 1 << 27;
    pub const RDIF: u64 = 1 << 28;
    /// The raw switch `monitor()` posts beside HLS or LLS (3576-3589).
    pub const RHLS: u64 = 1 << 29;
    pub const RLLS: u64 = 1 << 30;
    /// `process_motor_info`'s own UEIP post (3671-3675), no `MARK`.
    pub const UEIP_POSTED: u64 = 1 << 31;
    pub const SBAS: u64 = 1 << 32;
    pub const SREV: u64 = 1 << 33;
    pub const UREV: u64 = 1 << 34;
    pub const VELO: u64 = 1 << 35;
    pub const VBAS: u64 = 1 << 36;
    pub const MISS: u64 = 1 << 37;
    pub const STUP: u64 = 1 << 38;
    pub const JOGF: u64 = 1 << 39;
    pub const JOGR: u64 = 1 << 40;
    pub const HOMF: u64 = 1 << 41;
    pub const HOMR: u64 = 1 << 42;
    pub const RHLM: u64 = 1 << 43;
    pub const RLLM: u64 = 1 << 44;
    /// The rows `MARK` / `MARK_AUX` name, as they sit in MMAP and NMAP.
    pub const MMAP: u64 = (1 << 29) - 1;
    pub const NMAP: u64 = ((1 << 13) - 1) << 32;
    /// What `monitor()` posts above its short circuit (3462-3549).
    pub const UPPER: u64 = RBV | RRBV | DRBV | DIFF | RDIF | MSTA;
    // `special()`'s literal posts, drained by the put.
    pub const PUT_DMOV: u64 = 1 << 45;
    pub const PUT_VBAS: u64 = 1 << 46;
    pub const PUT_SBAS: u64 = 1 << 47;
    pub const PUT_VELO: u64 = 1 << 48;
    pub const PUT_DLLM: u64 = 1 << 49;
    pub const PUT_DHLM: u64 = 1 << 50;
    pub const PUT_HLM: u64 = 1 << 51;
    pub const PUT_LLM: u64 = 1 << 52;
    pub const PUT_STUP: u64 = 1 << 53;
    /// `monitor()`'s own CNEN post (3541-3548), raised and drained inside the
    /// take that derives it.
    pub const CNEN: u64 = 1 << 54;
    /// velcheckB's MRES<0 branch posts both user limits (2903-2905) ahead of
    /// the tail post every MRES put makes.
    pub const PUT_HLM_SWAP: u64 = 1 << 55;
    pub const PUT_LLM_SWAP: u64 = 1 << 56;
}

/// Internal bookkeeping fields (not directly exposed as PVs).
#[derive(Debug, Clone, Default)]
pub struct InternalFields {
    pub lval: f64,
    pub ldvl: f64,
    pub lrvl: i32,
    /// Last RLV value posted (C `motorRecord.dbd` LRLV, DBF_DOUBLE, SPC_NOMOD).
    pub lrlv: f64,
    /// C `pmr->vers` (DBF_FLOAT, SPC_NOMOD): a `.db` value until
    /// `init_record` pass 0 stamps the version (motorRecord.cc:608).
    pub vers: f32,
    /// C `mmap` and `nmap` (DBF_ULONG): the fields the next `monitor()`
    /// posts, set by `MotorRecord::mark` at C's `MARK` sites and drained
    /// by the cycle ([`m`] has the bits). A loaded word keeps all 32 bits.
    pub mmap: u32,
    pub nmap: u32,
    /// `process_motor_info`'s UEIP post, made without a mark (3671-3675).
    pub ueip_posted: bool,
    /// The cycle that set this ran `process()`, so the next take of the
    /// post marks is its `monitor()`; any other take is a put's.
    pub monitor_due: bool,
    /// The last pass committed STAT = COMM_ALARM: what C `build_trans`
    /// reads during the next pass's `do_work`, ahead of its `alarm_sub`.
    pub comm_alarm: bool,
    /// `monitor()` found a mark below its short circuit this cycle
    /// (motorRecord.cc:3550-3551), so an alarm move posts that group too.
    pub monitor_lower_marked: bool,
    /// The literal `DBE_VAL_LOG` posts `special()` makes itself, drained
    /// by the put that ran it ([`m`]'s `PUT_*` bits).
    pub put_posted: u64,
    /// The posts a put's `special()` made with the value C's call copied:
    /// `motor_end_trans_com`'s DMOV, one per transaction, drained by the
    /// put after its marks.
    pub put_post_values: VecDeque<DevicePost>,
    pub lspg: SpmgMode,
    /// C `pmr->sync` — latched SYNC request. Set by a nonzero SYNC put,
    /// consumed by `apply_latent_sync` only when the record is idle under
    /// SPMG Go/Move on a pass that dispatched nothing
    /// (motorRecord.cc:2540-2544 chain end).
    pub sync: i16,
    /// C `pmr->pp` — post-process this motion when the axis stops. Armed by
    /// the commanded stop while moving (motorRecord.cc:1892), jog/home
    /// arming (2025, 2110, 2125), jog stop (2152) and backlash arming
    /// (2523); cleared by the NTM stop-and-replan (1341, "Don't post
    /// process the previous move") and consumed at completion. A Pause
    /// never sets it — that is what keeps a paused move's target alive in
    /// VAL/DVAL for the Go resume instead of syncing it back to readback.
    pub pp: i16,
    /// Backlash final move pending after MainMove completes
    pub backlash_pending: bool,
    /// A jog/home command that arrived while the axis was already moving.
    /// The record stops the current motion first and re-issues this once the
    /// driver reports done. Kept separate from the MIP JOGF/JOGR/HOMF/HOMR
    /// bits so a plain STOP on an *active* jog/home is not mistaken for a
    /// queued request.
    pub queued_motion: Option<QueuedMotion>,
    /// Remember jog direction for backlash (cleared by stop_jog)
    pub jog_was_forward: bool,
    /// Set once `init_record` pass 1 has reconciled the load-time
    /// invariants: raw limits (RHLM/RLLM derived from the loaded DHLM/DLLM
    /// at the final MRES) and the rev↔EGU speed pairs (S/VELO, SBAS/VBAS,
    /// SMAX/VMAX, SBAK/BVEL).
    ///
    /// Until then — i.e. while `dbLoadRecords` is still applying `field()`
    /// entries through `put_field` — the MRES/UREV/SREV cascade and the
    /// speed cross-calcs must stay inert: C applies `field()` as raw struct
    /// writes and reconciles once in `init_record`
    /// (`check_speed_and_resolution` / `set_dial_highlimit`). The standard
    /// `motor.template` lists `field(VELO,…)` and `field(DHLM,…)` before
    /// `field(MRES,…)`, so cascading mid-load would derive S against the
    /// default UREV (and then rewrite VELO from that stale S), or rescale a
    /// freshly-loaded DHLM against the pre-MRES default resolution.
    pub init_invariants_synced: bool,
    /// C `MARK(M_MRES)` / `M_ERES` / `M_UEIP` — a runtime resolution (or
    /// encoder-use) change is pending its do_work re-anchor pass
    /// (motorRecord.cc:1937-1991). Armed by the MRES/SREV/UREV/ERES put
    /// arms (and the UEIP/URIP override paths) once init has completed;
    /// consumed — or dropped, when the pass takes a C top-block stop
    /// return — by the next process pass. Like C's mmap marks, it never
    /// outlives that pass.
    pub res_reanchor: bool,
    /// The device callback consumed this pass acknowledged a BUSY STUP
    /// (C process_exit 1498-1502 returned it to OFF). One-pass mark: the
    /// done branch applies the C 1345 gate — a GET_INFO acknowledgement
    /// is not a motion completion — and the pass consumes it.
    pub stup_ack: bool,
    /// Outcome of this cycle's closed-loop DOL read (C motorRecord.cc
    /// 1999-2005): `Some(true)` — the dbGetLink failed, VAL is
    /// undefined (C `udf = TRUE; return(ERROR)`); `Some(false)` — the
    /// read succeeded (C `udf = FALSE`). Latched by
    /// `set_resolved_input_links`, consumed by `check_alarms`, which
    /// owns the framework `CommonFields.udf` handoff.
    pub dol_udf: Option<bool>,
    /// One-pass mark: this process pass carries an idle-phase device
    /// status that `determine_event` consumed in place (it applies the
    /// readback and reports no event). The C equivalent is
    /// `process_reason == CALLBACK_DATA` — the pass must run the
    /// idle-completion pipeline (LOAD_P collapse, pp sync, C
    /// 1396-1409) and must NOT fire the put-pass-only implicit
    /// GET_INFO (C 2546 gates on `proc_ind == NOTHING_DONE`). Set by
    /// `determine_event`, consumed at `do_process_inner` entry.
    pub idle_status_pass: bool,
}
