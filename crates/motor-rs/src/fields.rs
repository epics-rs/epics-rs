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
    /// Raw step difference. 64-bit to cover high-resolution / long-travel
    /// axes beyond the 32-bit range (epics-modules/motor #192).
    pub rdif: i64,
    pub dval: f64,
    pub drbv: f64,
    pub rval: i64,
    pub rrbv: i64,
    pub rmp: i64,
    pub rep: i64,
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
    pub igset: bool,
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
            igset: false,
            mres: 1.0,
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
    pub frac: f64,
    pub rdbd: f64,
    pub spdb: f64,
    pub rtry: i16,
    pub rmod: RetryMode,
    pub rcnt: i16,
    pub miss: bool,
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
            miss: false,
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
    pub lvio: bool,
    pub hls: bool,
    pub lls: bool,
    /// Raw high limit switch readback (C 3727: `rhls = msta.Bits.RA_PLUS_LS`),
    /// unmapped by DIR/MRES polarity — HLS/LLS derive from these (C 3733-3734).
    pub rhls: bool,
    /// Raw low limit switch readback (C 3728: `rlls = msta.Bits.RA_MINUS_LS`).
    pub rlls: bool,
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
            // C dbd LVIO has no initial() (0), and init_record resets it
            // before the initial-readback check (motorRecord.cc:734-743).
            lvio: false,
            hls: false,
            lls: false,
            rhls: false,
            rlls: false,
            hlsv: 0,
        }
    }
}

/// Control fields (user commands).
#[derive(Debug, Clone)]
pub struct ControlFields {
    pub spmg: SpmgMode,
    pub stop: bool,
    pub homf: bool,
    pub homr: bool,
    pub jogf: bool,
    pub jogr: bool,
    pub twf: bool,
    pub twr: bool,
    pub twv: f64,
    pub cnen: bool,
}

impl Default for ControlFields {
    fn default() -> Self {
        Self {
            spmg: SpmgMode::Go,
            stop: false,
            homf: false,
            homr: false,
            jogf: false,
            jogr: false,
            twf: false,
            twr: false,
            twv: 1.0,
            cnen: false,
        }
    }
}

/// Status fields.
#[derive(Debug, Clone)]
pub struct StatusFields {
    pub dmov: bool,
    pub movn: bool,
    pub msta: MstaFlags,
    pub mip: MipFlags,
    pub phase: MotionPhase,
    pub cdir: bool,
    pub tdir: bool,
    pub athm: bool,
    pub stup: i16,
    /// Raw velocity reported by the driver, in motor steps/sec.
    /// C: `motorRecord.dbd` `field(RVEL,DBF_LONG)` "Raw Velocity"; devMotorAsyn
    /// fills it with `floor(status.velocity)`. 64-bit here for consistency
    /// with the other raw fields (epics-modules/motor #192).
    pub rvel: i64,
}

impl Default for StatusFields {
    fn default() -> Self {
        Self {
            dmov: true,
            movn: false,
            msta: MstaFlags::empty(),
            mip: MipFlags::empty(),
            phase: MotionPhase::Idle,
            cdir: false,
            tdir: false,
            athm: false,
            stup: 0,
            rvel: 0,
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
/// `DBF_INLINK` / `DBF_OUTLINK` specification clients open by C name; `post` is
/// the `DBF_STRING` post-move command. `omsl` is the `menuOmsl` selector
/// (0 = supervisory, 1 = closed_loop).
#[derive(Debug, Clone, Default)]
pub struct LinkFields {
    pub out: String,
    /// `field(CARD,DBF_SHORT)` (motorRecord.dbd:239-243), the one field that is
    /// a pure function of [`Self::out`]: `init_record` switches on the OUT link
    /// TYPE and stores the card number, -1, or 0 (`motorRecord.cc:653-670`).
    /// It lives beside the link it is derived from; `motor_derive_card` is the
    /// only writer.
    pub card: i16,
    pub rdbl: String,
    pub dol: String,
    pub rlnk: String,
    pub stoo: String,
    pub dinp: String,
    pub rinp: String,
    pub post: String,
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

/// Internal bookkeeping fields (not directly exposed as PVs).
#[derive(Debug, Clone, Default)]
pub struct InternalFields {
    pub lval: f64,
    pub ldvl: f64,
    pub lrvl: i64,
    /// Last RLV value posted (C `motorRecord.dbd` LRLV, DBF_DOUBLE, SPC_NOMOD).
    pub lrlv: f64,
    /// Monitored-field bitmap (C `motorRecord.dbd` MMAP, DBF_ULONG, SPC_NOMOD).
    pub mmap: i64,
    /// Non-monitored-field bitmap (C `motorRecord.dbd` NMAP, DBF_ULONG,
    /// SPC_NOMOD).
    pub nmap: i64,
    pub lspg: SpmgMode,
    /// C `pmr->sync` — latched SYNC request. Set by a nonzero SYNC put,
    /// consumed by `apply_latent_sync` only when the record is idle under
    /// SPMG Go/Move on a pass that dispatched nothing
    /// (motorRecord.cc:2540-2544 chain end).
    pub sync: bool,
    /// C `pmr->pp` — post-process this motion when the axis stops. Armed by
    /// the commanded stop while moving (motorRecord.cc:1892), jog/home
    /// arming (2025, 2110, 2125), jog stop (2152) and backlash arming
    /// (2523); cleared by the NTM stop-and-replan (1341, "Don't post
    /// process the previous move") and consumed at completion. A Pause
    /// never sets it — that is what keeps a paused move's target alive in
    /// VAL/DVAL for the Go resume instead of syncing it back to readback.
    pub pp: bool,
    /// Backlash final move pending after MainMove completes
    pub backlash_pending: bool,
    /// Pending retarget value (for NTM stop-and-replan)
    pub pending_retarget: Option<f64>,
    /// A jog/home command that arrived while the axis was already moving.
    /// The record stops the current motion first and re-issues this once the
    /// driver reports done. Kept separate from the MIP JOGF/JOGR/HOMF/HOMR
    /// bits so a plain STOP on an *active* jog/home is not mistaken for a
    /// queued request.
    pub queued_motion: Option<QueuedMotion>,
    /// Remember jog direction for backlash (cleared by stop_jog)
    pub jog_was_forward: bool,
    /// True after the initial DMOV 1→0 notification has been sent.
    /// Reset when DMOV returns to 1.
    pub dmov_notified: bool,
    /// Set when a same-direction retarget (ExtendMove) occurred during a
    /// motion. On completion, evaluate_position_error verifies that the
    /// driver actually followed the retarget and, if not, replans once
    /// independent of retry settings. Cleared after the check.
    pub verify_retarget_on_completion: bool,
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
    /// One-pass mark: `process_motor_info` recomputed DIFF/RDIF this
    /// cycle — the C `MARK(M_DIFF)` / `MARK(M_RDIF)` of
    /// `process_motor_info` (motorRecord.cc:3764-3767), set on every
    /// `CALLBACK_DATA` pass. C `monitor()` (3522-3531) then posts both
    /// with `monitor_mask | DBE_VAL_LOG` regardless of whether the value
    /// changed, so a `camonitor DIFF/RDIF` on a settled axis parked at a
    /// constant non-zero following error gets an event every poll. Read by
    /// `force_posted_fields`; reset at the top of each `process()`.
    pub diff_rdif_marked: bool,
    /// Driver commands a put queued at special() time (C sends them
    /// from special() before the pp pass runs do_work: pidcof
    /// 3003-3026, set_user/dial_*limit 4076-4328). Every motor put is
    /// followed by a process pass; `do_process` drains this buffer in
    /// front of the pass's own commands, preserving C's emission
    /// order.
    pub special_cmds: Vec<crate::flags::MotorCommand>,
}
