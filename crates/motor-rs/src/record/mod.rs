mod command_planner;
pub mod dbd_generated;
mod field_access;
pub(crate) use field_access::motor_failed_dset_init;
mod state_machine;
mod status_update;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::record::{
    CycleMarks, CyclePostMask, FieldDesc, FieldIx, LinkType, MonitorValueClasses, MonitorValueGate,
    ParsedLink, ProcessAction, ProcessActions, ProcessOutcome, Record, ResolvedInputLinks,
    load_constant_link_scalar, parse_link_v2,
};
use epics_base_rs::types::{DbFieldType, EpicsValue};

use crate::coordinate;
use crate::device_state::*;
use crate::device_support::{DialLimitReset, LimitBuild};
use crate::fields::*;
use crate::flags::*;

/// EPICS Motor Record implementation.
#[derive(Debug, Clone)]
pub struct MotorRecord {
    pub pos: PositionFields,
    pub conv: ConversionFields,
    pub vel: VelocityFields,
    pub retry: RetryFields,
    pub limits: LimitFields,
    pub ctrl: ControlFields,
    pub stat: StatusFields,
    pub pid: PidFields,
    pub disp: DisplayFields,
    pub timing: TimingFields,
    pub internal: InternalFields,
    /// Database-link / menu fields imported from the public C motorRecord.dbd
    /// surface (OUT/RDBL/DOL/OMSL/RLNK/STOO/DINP/RINP/POST).
    pub links: LinkFields,
    /// Alarm-limit fields imported from the public C motorRecord.dbd surface
    /// (HIHI/HIGH/LOW/LOLO/HHSV/LLSV).
    pub alarm: AlarmFields,
    /// Pending event for next process() call
    pending_event: Option<MotorEvent>,
    /// The axis's asyn user, which also carries `pmr->name`.
    trace: std::sync::Arc<crate::trace::MotorTrace>,
    /// Track which field was last written (for process)
    last_write: Option<CommandSource>,
    /// Where `special()`'s driver commands go, from the device init on.
    port: Option<crate::device_support::CommandPort>,
    /// Shared state mailbox for device communication
    device_state: Option<SharedDeviceState>,
    /// Whether C `init_record` reaches its tail: it returns before it with
    /// no dset or a failed dset init (motorRecord.cc:612-651).
    initialized: bool,
    /// Monotonic counter for delay request IDs
    next_delay_id: u64,
    /// C `pmr->dol.type`, the framework's ([`Record::set_link_type`]): only
    /// the DOL as `iocInit` resolved it tells a local name from a CA one.
    dol_type: LinkType,
}

/// C `CALLBACK_VALUE`: why a pass reaches do_work.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ProcReason {
    /// A put or scan with no device callback to read.
    NothingDone,
    /// A device callback, or the settle delay's ACK (C 1280-1293).
    CallbackData,
}

/// One row per [`m`] bit: C `monitor()`'s `monitor_mask | DBE_VAL_LOG`
/// posts, then the literal `DBE_VAL_LOG` ones.
static MONITOR_POSTS: [(FieldIx, CyclePostMask); 57] =
    epics_base_rs::server::record::declared_post_table(
        dbd_generated::MOTOR_FIELDS,
        MONITOR_POSTS_NAMED,
    );

const MONITOR_POSTS_NAMED: [(&str, CyclePostMask); 57] = {
    use CyclePostMask::{MonitorValueLog as M, ValueLog as L};
    [
        ("VAL", M),
        ("DVAL", M),
        ("HLM", M),
        ("LLM", M),
        ("DMOV", M),
        ("SPMG", M),
        ("RCNT", M),
        ("MRES", M),
        ("ERES", M),
        ("UEIP", M),
        ("STOP", M),
        ("LVIO", M),
        ("RVAL", M),
        ("RLV", M),
        ("OFF", M),
        ("RBV", M),
        ("DHLM", M),
        ("DLLM", M),
        ("DRBV", M),
        ("MOVN", M),
        ("HLS", M),
        ("LLS", M),
        ("RRBV", M),
        ("MSTA", M),
        ("ATHM", M),
        ("TDIR", M),
        ("MIP", M),
        ("DIFF", M),
        ("RDIF", M),
        ("RHLS", M),
        ("RLLS", M),
        ("UEIP", L),
        ("SBAS", M),
        ("SREV", M),
        ("UREV", M),
        ("VELO", M),
        ("VBAS", M),
        ("MISS", M),
        ("STUP", M),
        ("JOGF", M),
        ("JOGR", M),
        ("HOMF", M),
        ("HOMR", M),
        ("RHLM", M),
        ("RLLM", M),
        ("DMOV", L),
        ("VBAS", L),
        ("SBAS", L),
        ("VELO", L),
        ("DLLM", L),
        ("DHLM", L),
        ("HLM", L),
        ("LLM", L),
        ("STUP", L),
        ("CNEN", M),
        ("HLM", L),
        ("LLM", L),
    ]
};

/// C `monitor()`'s fields in its order, RBV aside: the five above its short
/// circuit (3509-3549), then the rest (3553-3643), then CNEN, which posts only
/// when the MSTA block derives a change.
static MONITOR_FIELDS: [&str; 44] = [
    "RRBV", "DRBV", "DIFF", "RDIF", "MSTA", "VAL", "DVAL", "RVAL", "TDIR", "MIP", "HLM", "LLM",
    "SPMG", "RCNT", "RLV", "OFF", "DHLM", "DLLM", "HLS", "RHLS", "RLLS", "LLS", "ATHM", "MRES",
    "ERES", "UEIP", "LVIO", "STOP", "SBAS", "SREV", "UREV", "VELO", "VBAS", "MISS", "MOVN", "DMOV",
    "STUP", "JOGF", "JOGR", "HOMF", "HOMR", "RHLM", "RLLM", "CNEN",
];

/// The fields C posts outside `monitor()`: devMotorAsyn's RMP/REP/RVEL
/// (devMotorAsyn.c:456-473), `range_check` and the PID clamp
/// (motorRecord.cc:4377, 3025), and the literal posts of `special()` and
/// `process()`. C posts no other field from a process.
const CHANGE_POSTED: [&str; 25] = [
    "ACCL", "ACCS", "BACC", "BVEL", "DCOF", "FOFF", "FRAC", "HVEL", "ICOF", "JVEL", "NTMF", "PCOF",
    "RDBD", "REP", "RMP", "RVEL", "S", "SBAK", "SET", "SMAX", "SYNC", "TWF", "TWR", "URIP", "VMAX",
];

/// [`MONITOR_FIELDS`] in their post order, then every other motor field but
/// RBV, the deadband field, and [`CHANGE_POSTED`]: CDIR, LVAL and the other
/// fields C never posts stay silent however a process changes them.
static POSTED_ONLY_WHEN_MARKED: std::sync::LazyLock<Vec<&'static str>> =
    std::sync::LazyLock::new(|| {
        let never = dbd_generated::MOTOR_FIELDS.iter().map(|field| field.name);
        let never = never.filter(|name| {
            *name != "RBV" && !CHANGE_POSTED.contains(name) && !MONITOR_FIELDS.contains(name)
        });
        MONITOR_FIELDS.iter().copied().chain(never).collect()
    });

impl Default for MotorRecord {
    fn default() -> Self {
        Self {
            pos: PositionFields::default(),
            conv: ConversionFields::default(),
            vel: VelocityFields::default(),
            retry: RetryFields::default(),
            limits: LimitFields::default(),
            ctrl: ControlFields::default(),
            stat: StatusFields::default(),
            pid: PidFields::default(),
            disp: DisplayFields::default(),
            timing: TimingFields::default(),
            internal: InternalFields::default(),
            links: LinkFields::default(),
            alarm: AlarmFields::default(),
            pending_event: None,
            trace: Default::default(),
            last_write: None,
            port: None,
            device_state: None,
            initialized: false,
            next_delay_id: 0,
            dol_type: LinkType::Constant,
        }
    }
}

/// Motion direction for hardware limit checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MotionDirection {
    Positive,
    Negative,
}

impl MotorRecord {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a motor record wired to a shared device state mailbox.
    pub fn with_device_state(mut self, state: SharedDeviceState) -> Self {
        self.device_state = Some(state);
        self
    }

    /// Set the shared device state (for late injection by device support init).
    pub fn set_device_state(&mut self, state: SharedDeviceState) {
        self.device_state = Some(state);
    }

    /// Bind the axis's asyn user; its record name is the one console lines
    /// carry.
    pub fn set_trace(&mut self, trace: std::sync::Arc<crate::trace::MotorTrace>) {
        self.trace = trace;
    }

    pub(crate) fn trace(&self) -> &crate::trace::MotorTrace {
        &self.trace
    }

    /// C 1335: the movn-block NTM stop — "We're going in the wrong
    /// direction. Readback problem?"
    pub(crate) fn report_ntm_stop(&self) {
        println!("{}:tdir = {}", self.trace.record(), self.stat.tdir);
    }

    /// C syncTargetPosition (4414-4458): the readback chain re-derived
    /// (with no initcall gate on RDBL), then the targets synced to it. A
    /// failed RDBL read leaves the readbacks as they were and syncs the
    /// targets to them anyway. DIFF and RDIF wait for the next callback.
    pub(crate) fn sync_target_position(&mut self) {
        if !self.derive_readback(self.conv.urip) {
            println!(
                "{}: syncTargetPosition: error reading RDBL link.",
                self.trace.record()
            );
        }
        self.pos.rbv = coordinate::dial_to_user(self.pos.drbv, self.conv.dir, self.pos.off);
        self.mark(m::RRBV | m::DRBV | m::RBV);
        self.sync_targets();
    }

    /// Set a pending event for the next process() call.
    /// The port `special()` sends through from now on.
    pub fn set_command_port(&mut self, port: crate::device_support::CommandPort) {
        self.port = Some(port);
    }

    /// C `special()`'s `build_trans` and `SEND_MSG`: queued at put time,
    /// with no process. A record with no device support has nowhere to
    /// send. Returns false when the device refused a command, which sends
    /// nothing (`if (rtnval == OK) SEND_MSG()`).
    pub(crate) fn send_special(&mut self, commands: Vec<MotorCommand>) -> bool {
        if self.port.is_none() {
            return true;
        }
        let Some(commands) = self.transaction(commands) else {
            return false;
        };
        if let Some(port) = &self.port {
            port.send(commands, false);
        }
        true
    }

    /// C `set_dial_highlimit`/`set_dial_lowlimit` (4236-4325): send the
    /// dial limit, then, built or not, set the raw limit to what was sent
    /// and the one user limit it maps to (HLM for a high dial limit under
    /// DIR=Pos, LLM under DIR=Neg) from the dial limit as a refusing device
    /// may have reset it.
    pub(crate) fn set_dial_limit(&mut self, high: bool) {
        let dial = if high {
            self.limits.dhlm
        } else {
            self.limits.dllm
        };
        let raw = dial / self.conv.mres;
        let command = self.dial_limit_command(high, dial);
        let raw_high = matches!(command, MotorCommand::SetHighLimit { .. });
        self.send_special(vec![command]);
        if raw_high {
            self.limits.rhlm = raw;
            self.mark(m::RHLM);
        } else {
            self.limits.rllm = raw;
            self.mark(m::RLLM);
        }
        let dial = if high {
            self.limits.dhlm
        } else {
            self.limits.dllm
        };
        let user = coordinate::dial_to_user(dial, self.conv.dir, self.pos.off);
        if high == (self.conv.dir == MotorDir::Pos) {
            self.limits.hlm = user;
            self.mark(m::HLM);
        } else {
            self.limits.llm = user;
            self.mark(m::LLM);
        }
        self.mark(if high { m::DHLM } else { m::DLLM });
    }

    /// C `build_trans` per command, the one place a device's verdict is
    /// asked, once per command. A deferred command (devEMC18011.cc:233-240,
    /// another motor selected) is not built and gives back a retry
    /// (`if (rcnt > 0) rcnt--`, unmarked), ahead of any limit verdict. The
    /// built ones still go out (each is sent by its own
    /// motor_end_trans_com, :405-414), and a last command that deferred
    /// leaves the node INFO, which sends no message (motordrvCom.cc:441): a
    /// status query after them. A refused soft limit (devMM4000.cc:316-336)
    /// may reset one dial limit and makes the call ERROR: `None`.
    fn build(&mut self, commands: Vec<MotorCommand>) -> Option<Vec<MotorCommand>> {
        let Some(port) = &self.port else {
            return Some(commands);
        };
        let mut built = Vec::with_capacity(commands.len());
        let mut last_deferred = false;
        for command in commands {
            last_deferred = port.defers(&command);
            if last_deferred {
                self.retry.rcnt = (self.retry.rcnt - 1).max(0);
                continue;
            }
            if let LimitBuild::Refused(reset) = port.build_limit(&command, self.conv.mres) {
                match reset {
                    Some(DialLimitReset::Dhlm(dial)) => self.limits.dhlm = dial,
                    Some(DialLimitReset::Dllm(dial)) => self.limits.dllm = dial,
                    None => {}
                }
                return None;
            }
            built.push(command);
        }
        if last_deferred {
            built.push(MotorCommand::Poll);
        }
        Some(built)
    }

    /// `commands` as C `build_trans` builds them from the record: led by
    /// the record's fields, which a device reads at any of them, and each move
    /// preceded by the record's CDIR and by the PREM and POST it reads
    /// (devIM483PL.cc:191-205), when either is set; JOG_VELOCITY reads
    /// neither. A record's init, process
    /// and `special()` puts all send through it. `None` is a refusal
    /// ([`build`](Self::build)): nothing to send.
    pub(crate) fn transaction(&mut self, commands: Vec<MotorCommand>) -> Option<Vec<MotorCommand>> {
        let commands = self.build(commands)?;
        let prem = &self.links.prem;
        // C `if (strlen(mr->post) != 0) postmsgptr = &mr->post`: the field,
        // not its text, so the device sends what POST holds when it is done.
        let post = (!self.links.post.is_empty()).then(|| self.links.post.clone());
        let built = |command: &MotorCommand| {
            matches!(
                command,
                MotorCommand::MoveAbsolute { .. }
                    | MotorCommand::MoveRelative { .. }
                    | MotorCommand::MoveVelocity { .. }
                    | MotorCommand::JogVelocity { .. }
                    | MotorCommand::Home { .. }
            )
        };
        if !commands.iter().any(built) {
            return Some(commands);
        }
        let strings = !prem.is_empty() || post.is_some();
        let mut out = Vec::with_capacity(commands.len() + 3);
        out.push(MotorCommand::RecordFields(crate::flags::RecordFields {
            velocity: self.raw_rate(self.vel.velo),
            srev: self.conv.srev,
        }));
        for command in commands {
            if built(&command) {
                out.push(MotorCommand::MoveDirection {
                    forward: self.stat.cdir != 0,
                });
                if strings && !matches!(command, MotorCommand::JogVelocity { .. }) {
                    out.push(MotorCommand::MoveStrings {
                        prem: prem.clone(),
                        post: post.clone(),
                    });
                }
            }
            out.push(command);
        }
        Some(out)
    }

    /// C `init_record` got past the device init (motorRecord.cc:646-651),
    /// and so runs to its closing `monitor()`: devMotorAsyn's `bad:` arm
    /// returns 0, so a record it parks still gets there. A failed dset init
    /// returns before the ERES seed (692-696) and the limit reset (717-718).
    pub(crate) fn finish_init(&mut self) {
        self.initialized = true;
        if self.conv.eres == 0.0 {
            self.conv.eres = self.conv.mres;
            self.mark(m::ERES);
        }
    }

    pub fn set_event(&mut self, event: MotorEvent) {
        self.pending_event = Some(event);
    }

    /// C do_work collection gate (motorRecord.cc:1994): `omsl ==
    /// menuOmslclosed_loop && dol.type == DB_LINK`. While it holds, the
    /// entire button/tweak/relative/raw collection block (2008-2198) is
    /// bypassed and VAL arrives only through the DOL link; a constant or
    /// CA DOL leaves the collection active even under closed loop.
    pub(crate) fn closed_loop_dol_collection(&self) -> bool {
        self.links.omsl == 1 && self.dol_type == LinkType::Db
    }

    /// The DOL under C init_record's `dol.type == CONSTANT` (677), the one
    /// gate of the VAL seed and the UDF clear: an unset DOL is CONSTANT, a
    /// `{const:…}` one a JSON_LINK.
    fn constant_dol(&self) -> Option<ParsedLink> {
        Some(parse_link_v2(&self.links.dol)).filter(ParsedLink::is_constant_type)
    }

    /// C init_record 680: `recGblInitConstantLink(&dol, DBF_DOUBLE, &val)`.
    pub(crate) fn seed_val_from_dol(&mut self) {
        let Some(dol) = self.constant_dol() else {
            return;
        };
        if let Some(EpicsValue::Double(v)) = load_constant_link_scalar(&dol, DbFieldType::Double) {
            self.pos.val = v;
        }
    }

    /// True when a position field (VAL/DVAL/RVAL/RLV) was written during
    /// pass0 — i.e. autosave restored a saved position.
    ///
    /// It reads true only before [`initial_readback`](Self::initial_readback),
    /// which drops the write.
    ///
    /// This is the correct "was a position restored" signal: a genuine
    /// restored position of exactly `0.0` is indistinguishable from the
    /// field default if you only inspect the DVAL value, but the pass0
    /// write still records `last_write`.
    pub fn was_position_restored(&self) -> bool {
        matches!(
            self.last_write,
            Some(
                CommandSource::Val | CommandSource::Dval | CommandSource::Rval | CommandSource::Rlv
            )
        )
    }

    /// Signal that the external URIP readback link is in error or recovered.
    /// While `urip` is true and `error` is set, new motions are refused and
    /// in-progress motion is stopped (C: `db5da2f0`, `7493d50b`).
    pub fn set_rdbl_error(&mut self, error: bool) {
        self.conv.rdbl_error = error;
    }

    /// C `MARK` / `MARK_AUX`: post these fields at the next `monitor()`.
    #[inline]
    pub(crate) fn mark(&mut self, bits: u64) {
        debug_assert_eq!(bits & !(m::MMAP | m::NMAP), 0, "not a C mark");
        self.internal.mmap |= bits as u32;
        self.internal.nmap |= (bits >> 32) as u32;
    }

    /// DMOV as C writes it: marked where it changes (`if (pmr->dmov) { dmov
    /// = FALSE; MARK(M_DMOV); }`).
    pub(crate) fn set_dmov(&mut self, dmov: bool) {
        if (self.stat.dmov != 0) != dmov {
            self.stat.dmov = i16::from(dmov);
            self.mark(m::DMOV);
        }
    }

    /// MOVN as `process_motor_info` posts it, on change (3746-3748).
    pub(crate) fn set_movn(&mut self, movn: bool) {
        if (self.stat.movn != 0) != movn {
            self.stat.movn = i16::from(movn);
            self.mark(m::MOVN);
        }
    }

    /// The collection's `if (val != lval) MARK(M_VAL)` (2204-2206, 2302).
    pub(crate) fn mark_val_collected(&mut self) {
        if self.pos.val != self.internal.lval {
            self.mark(m::VAL);
        }
    }

    /// C `UNMARK`.
    #[inline]
    pub(crate) fn unmark(&mut self, bits: u64) {
        self.internal.mmap &= !(bits as u32);
        self.internal.nmap &= !((bits >> 32) as u32);
    }

    /// C init_record's tail (motorRecord.cc:720-746), after the device
    /// readback it reads DRBV from, ending in `monitor()`: no client hears
    /// that post, but it derives CNEN from MSTA and clears every mark.
    pub(crate) fn init_tail(&mut self) {
        // C 721-723: `dmov = TRUE; movn = FALSE` — DMOV starts done
        // no matter what the load wrote. In particular this
        // neutralizes any `special()` pass-0 blink a load-time /
        // restore write to a drive field may have left behind.
        self.stat.dmov = 1;
        self.stat.movn = 0;
        self.mark(m::DMOV | m::MOVN | m::SPMG | m::DIFF | m::RDIF | m::MSTA);
        // C 734-743: LVIO starts clear; with soft limits enabled
        // (the dial pair not both 0), an initial dial readback
        // outside the dial window by more than one MRES, or an
        // inverted pair, raises it. The slop is the SIGNED
        // resolution (C verbatim — a negative MRES narrows the
        // window).
        self.limits.lvio = 0;
        let limits_disabled = self.limits.dhlm == self.limits.dllm && self.limits.dllm == 0.0;
        if !limits_disabled
            && (self.pos.drbv > self.limits.dhlm + self.conv.mres
                || self.pos.drbv < self.limits.dllm - self.conv.mres
                || self.limits.dllm > self.limits.dhlm)
        {
            self.limits.lvio = 1;
        }
        // The closing monitor() (747) moves MLST/ALST to the init readback.
        let _ = self.rbv_deadband();
        let _ = self.take_monitor_marks();
    }

    /// C `monitor()`'s RBV block (motorRecord.cc:3468-3507): the value and
    /// archive classes RBV posts with, consuming its mark and moving the
    /// record's own MLST/ALST on a crossing.
    fn rbv_deadband(&mut self) -> (bool, bool) {
        let marked = self.internal.mmap & m::RBV as u32 != 0;
        self.unmark(m::RBV);
        let (mdel, adel, rbv) = (self.disp.mdel, self.disp.adel, self.pos.rbv);
        let crossed = |del: f64, last: &mut f64| {
            del == 0.0
                || ((*last - rbv).abs() > del && {
                    *last = rbv;
                    true
                })
        };
        if !marked {
            (false, false)
        } else if mdel == 0.0 && adel == 0.0 {
            (true, true)
        } else {
            (
                crossed(mdel, &mut self.disp.mlst),
                crossed(adel, &mut self.disp.alst),
            )
        }
    }

    /// The marks `monitor()` posts from, cleared as its `UNMARK_ALL` does
    /// (motorRecord.cc:3645), with each switch mark carrying the raw switch
    /// `monitor()` posts beside it (3576-3589).
    fn take_monitor_marks(&mut self) -> u64 {
        let mmap = std::mem::take(&mut self.internal.mmap);
        let nmap = std::mem::take(&mut self.internal.nmap);
        // The short circuit (3552) reads both whole words, past the upper
        // block's UNMARKs (3509-3549).
        self.internal.monitor_lower_marked = mmap & !((m::UPPER | m::RBV) as u32) != 0 || nmap != 0;
        let mut bits = (u64::from(mmap) & m::MMAP | u64::from(nmap) << 32 & m::NMAP) & !m::RBV;
        if std::mem::take(&mut self.internal.ueip_posted) {
            bits |= m::UEIP_POSTED;
        }
        let same = (self.conv.dir == MotorDir::Pos) == (self.conv.mres >= 0.0);
        if bits & m::HLS != 0 {
            bits |= if same { m::RHLS } else { m::RLLS };
        }
        if bits & m::LLS != 0 {
            bits |= if same { m::RLLS } else { m::RHLS };
        }
        // The MSTA block's position-maintenance readback (3541-3548).
        if bits & m::MSTA != 0 && self.stat.msta.contains(MstaFlags::GAIN_SUPPORT) {
            let pos_maint = self.stat.msta.contains(MstaFlags::POSITION);
            if pos_maint != self.ctrl.cnen {
                self.ctrl.cnen = pos_maint;
                bits |= m::CNEN;
            }
        }
        bits
    }

    /// Fire the readback output link (RLNK) with the current RBV.
    ///
    /// C `motorRecord.cc:1495` calls `dbPutLink(&rlnk, DBR_DOUBLE, &rbv, 1)`
    /// just before `process_exit` on every pass that does not jump there, so
    /// a record wired through RLNK re-processes whenever the motor record
    /// does. We mirror that by emitting a `WriteDbLink` on every such cycle,
    /// including the move-start (DMOV 1→0) pass: C fires
    /// `dbPutLink` before `monitor()` (motorRecord.cc:1507) and before the
    /// `recGblFwdLink` gate (motorRecord.cc:1509), so it runs even while the
    /// move is still pending. A `dbPutLink` processes a PP readback target even
    /// when RBV is unchanged, so skipping the pending pass would drop one of
    /// the target's process cycles relative to C. The framework runs the
    /// `WriteDbLink` before the deferred FLNK, exactly where C fires it. The
    /// empty link is skipped so motors with no RLNK emit nothing.
    fn push_readback_link_write(&self, actions: &mut Vec<ProcessAction>) {
        if !self.links.rlnk.is_empty() {
            actions.push(ProcessAction::WriteDbLink {
                link_field: "RLNK",
                value: EpicsValue::Double(self.pos.rbv),
            });
        }
    }
}

impl Record for MotorRecord {
    fn record_type(&self) -> &'static str {
        "motor"
    }

    /// `motorRecord.cc:1495`: `status = dbPutLink(&pmr->rlnk, ...)` is what
    /// `process()` returns.
    fn process_status_rule(&self) -> epics_base_rs::server::record::ProcessStatusRule {
        epics_base_rs::server::record::ProcessStatusRule::ZERO.with_status_links(&["RLNK"])
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }

    fn can_device_write(&self) -> bool {
        true
    }

    fn is_put_complete(&self) -> bool {
        self.stat.dmov != 0
    }

    // C RSET per-field metadata (motorRecord.cc get_units 3156-3208,
    // get_precision 3313-3337, get_graphic_double 3213-3258,
    // get_control_double 3263-3308, get_alarm_double 3344-3361).
    fn field_metadata_override(
        &self,
        field: &str,
    ) -> Option<epics_base_rs::server::record::FieldMetadataOverride> {
        Some(field_access::metadata_override(self, field))
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // If wired to device state, determine event from shared mailbox;
        // an injected event is not overwritten.
        if self.pending_event.is_none() && self.device_state.is_some() {
            if let Some(event) = self.determine_event() {
                self.pending_event = Some(event);
            }
        }

        let effects = self.do_process();

        // Write effects to shared mailbox for DeviceSupport.write() to consume.
        // If a previous batch has not been consumed yet (two process() cycles
        // without an intervening write()), fold the new batch into it rather
        // than overwriting — otherwise the earlier move command is lost.
        if let Some(state) = self.device_state.clone() {
            let actions = self.effects_to_actions(&effects);
            if let Ok(mut ds) = state.lock() {
                ds.set_comm_alarm(self.internal.comm_alarm);
                match ds.pending_actions.take() {
                    Some(mut prev) => {
                        prev.merge_newer(actions);
                        ds.pending_actions = Some(prev);
                    }
                    None => ds.pending_actions = Some(actions),
                }
            }
        }

        // Every pass, the move-start one included, ends in C process_exit
        // (motorRecord.cc:1503-1510): alarm_sub and monitor()'s
        // recGblResetAlarms run whatever DMOV is; only recGblFwdLink, and with
        // it put-notify completion, waits for DMOV (should_fire_forward_link).
        self.internal.monitor_due = true;
        let mut outcome = ProcessOutcome::complete();
        if !effects.process_exit {
            self.push_readback_link_write(&mut outcome.actions);
        }
        Ok(outcome)
    }

    /// C process_exit (motorRecord.cc:1509-1510): `if (dmov != 0)
    /// recGblFwdLink(pmr)` — FLNK fires on EVERY pass that exits with
    /// DMOV true, and on nothing else. (`0ef39053` gated this on the
    /// DMOV false→true transition; `c970afbf` reverted it: "Except for
    /// the condition that DMOV == FALSE, FLNK processing was
    /// standard.") Reading DMOV directly at FLNK time IS the C gate by
    /// construction — there is no per-path suppression state to track.
    fn should_fire_forward_link(&self) -> bool {
        self.stat.dmov != 0
    }

    /// Input links the framework must resolve via `dbGetLink` BEFORE each
    /// `process()`, so their values are available to the move computation —
    /// exactly the synchronous reads C `motorRecord.cc` performs inside
    /// `do_work`/`process_motor_info`.
    ///
    /// - CLOSED_LOOP DOL → VAL (motorRecord.cc:1994), gated below.
    /// - URIP=Yes RDBL → RRBV: C `process_motor_info` (motorRecord.cc:3687)
    ///   does `dbGetLink(&pmr->rdbl, DBR_DOUBLE, &rdblvalue, ...)` and scales it
    ///   into RRBV (`rrbv = NINT(rdblvalue*rres/mres)`). The scaling lives in
    ///   the status-update path; this just feeds it the raw link value through
    ///   the `RDBL_VAL` carrier. C reads with `dbGetLink` regardless of link
    ///   type, so any RDBL link form is read (the framework skips an empty
    ///   link).
    fn pre_process_actions(&mut self) -> ProcessActions {
        let mut actions = Vec::new();

        // CLOSED_LOOP: drive VAL from the DOL link (motorRecord.cc:1994-1999).
        // C reads DOL into .val only when `omsl == closed_loop && dol.type ==
        // DB_LINK`; a constant DOL is initialised once (recGblInitConstantLink,
        // :680) and never re-read, and a CA one is never read, so gate on the
        // resolved DB link. Writing VAL records a VAL command-source, so the
        // following process() plans the move to the link value.
        //
        // Read only when DMOV (done): C calls do_work — the function holding
        // the DOL read — for `pmr->dmov` or a non-callback process, but NOT on
        // a CALLBACK_DATA poll mid-move (motorRecord.cc:1487-1492). Gating on
        // DMOV mirrors that dominant condition and stops a device poll from
        // re-reading DOL and clobbering the in-flight target every cycle.
        if self.stat.dmov != 0 && self.closed_loop_dol_collection() {
            actions.push(ProcessAction::ReadDbLink {
                link_field: "DOL",
                target_field: "VAL",
            });
        }

        // C process_motor_info (3676-3699) is an else-if chain: UEIP=Yes
        // takes the encoder and the RDBL dbGetLink never executes, so a
        // failed RDBL must not stop the axis while the encoder is in use.
        if self.conv.urip && !self.conv.ueip {
            actions.push(ProcessAction::ReadDbLink {
                link_field: "RDBL",
                target_field: "RDBL_VAL",
            });
        }
        actions.into()
    }

    /// Framework report of which requested link reads produced a value
    /// this cycle — the analogue of C inspecting
    /// `RTN_SUCCESS(dbGetLink(&pmr->rdbl, ...))` in `process_motor_info`
    /// (motorRecord.cc:3687-3698). A failed RDBL read latches
    /// `rdbl_error`; while it is set, process_motor_info stops an
    /// in-flight motion and the planner refuses new ones.
    ///
    /// The report is C's status, so every RDBL form C calls successful is
    /// present in it — an empty link and a constant one included
    /// (`dbConstGetValue` returns 0). The gate here is therefore exactly the
    /// gate `pre_process_actions` requested the read under, and nothing more:
    /// a second, local "is the link empty" test was the port re-deriving a rule
    /// the framework owns, and it did not cover `field(RDBL,"5")`.
    fn set_resolved_input_links(&mut self, resolved: ResolvedInputLinks<'_>) {
        // C closed-loop DOL collection (motorRecord.cc:1994-2005): a
        // failed dbGetLink on DOL sets `udf = TRUE` and aborts the pass
        // (return ERROR — the motion side is inert anyway because the
        // failed read delivers no VAL); a successful read clears it.
        // The outcome is latched here — gated exactly like the
        // pre_process_actions request — and applied to the framework's
        // CommonFields.udf in check_alarms (the C alarm_sub consumer).
        if self.stat.dmov != 0 && self.closed_loop_dol_collection() {
            self.internal.dol_udf = Some(!resolved.contains("DOL"));
        }
        if self.conv.urip && !self.conv.ueip {
            self.conv.rdbl_error = !resolved.contains("RDBL");
        }
    }

    fn set_link_type(&mut self, link_field: &str, link_type: LinkType) {
        if link_field == "DOL" {
            self.dol_type = link_type;
        }
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        field_access::motor_get_field(self, name)
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        field_access::motor_put_field(self, name, value)
    }

    fn declared_fields(&self) -> &'static [FieldDesc] {
        dbd_generated::MOTOR_FIELDS
    }

    fn declared_dbaddr_fields(&self) -> Option<&'static [&'static str]> {
        Some(dbd_generated::MOTOR_DBADDR)
    }

    fn declared_noaccess_fields(&self) -> &'static [&'static str] {
        dbd_generated::MOTOR_NOACCESS
    }

    /// C `special()` pass-0 blink (motorRecord.cc:2591-2620): "Someone
    /// wrote to drive field. Blink .dmov unless record is disabled."
    /// Every database put to a drive field drops DMOV before the record
    /// processes — that blink is what carries the put pass through the
    /// do_work move-block entry gate (2241, `dval != ldvl || !dmov`)
    /// even when the target equals the last-dispatched one; the move
    /// block then decides between the sub-step pulse restore
    /// (too_small, 2332-2349), a re-dispatch (2455, `mip == MIP_DONE ||
    /// MIP_RETRY`), and the SET-mode load_pos. The entry gate itself
    /// only ever refuses passes that did NOT come through database put
    /// access — C's closed-loop DOL collection is a bare `dbGetLink`
    /// into VAL (1994) with no special, mirrored here by the framework
    /// link reads landing via `put_field_internal`, which never runs
    /// this hook.
    ///
    /// C's disabled-record gate (2600-2601): the DISP half is enforced
    /// upstream — the framework rejects the put before this hook runs
    /// (`check_put_disabled`, C dbPutField's put-disable test), so no
    /// blink can land on a DISP'd record. The DISA==DISV half is not
    /// reachable from the record trait (DISA/DISV are framework common
    /// fields); a drive put to a processing-disabled motor blinks DMOV
    /// low and it stays low until the record next processes, where C
    /// keeps DMOV=1. Known divergence.
    fn special(
        &mut self,
        field: &str,
        after: bool,
        _common: &epics_base_rs::server::record::CommonFields,
    ) -> CaResult<()> {
        if after {
            return Ok(());
        }
        if ["VAL", "DVAL", "RVAL", "RLV", "TWF", "TWR"]
            .iter()
            .any(|f| field.eq_ignore_ascii_case(f))
            && self.stat.dmov != 0
        {
            // C 2603-2606 posts the blink itself.
            self.stat.dmov = 0;
            self.internal.put_posted |= m::PUT_DMOV;
        }
        Ok(())
    }

    /// C `init_record`: on pass 1, once all `field()` values have been
    /// applied, reconcile the rev↔EGU speed pairs (C
    /// `check_speed_and_resolution`) and establish the limit invariant from
    /// the loaded DHLM/DLLM (C `set_dial_highlimit`/`set_dial_lowlimit`),
    /// then enable the runtime cascade semantics in `put_field`. See
    /// `field_access::motor_sync_speed_at_init` and
    /// `field_access::motor_sync_limits_at_init`.
    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 0 {
            self.internal.vers = field_access::MOTOR_RECORD_VERSION;
        }
        if pass == 1 {
            // C init_record runs check_speed_and_resolution (641) with the
            // resolution triple first (3905-3927), then the speed pairs,
            // then the raw-limit rules.
            field_access::motor_sync_resolution_at_init(self);
            field_access::motor_sync_speed_at_init(self);
            field_access::motor_sync_limits_at_init(self);
            // C 653-670: CARD from the OUT link's type, the record's only
            // read of that link at init.
            field_access::motor_derive_card(self);
            // C 642: the single load-order-independent enforcement of
            // RDBD >= |MRES|, against the reconciled resolution. The RDBD
            // put handler stays inert during load.
            self.enforce_min_retry_deadband();
            self.internal.init_invariants_synced = true;
        }
        Ok(())
    }

    fn primary_field(&self) -> &'static str {
        "VAL"
    }

    /// MDEL/ADEL monitor deadband applies to the readback (RBV), not the
    /// VAL setpoint. C `monitor()` gates RBV value/archive monitors on
    /// MDEL/ADEL; VAL is a setpoint that only changes on a move command.
    fn monitor_deadband_value(&self) -> Option<epics_base_rs::server::record::AlarmLimit> {
        Some(epics_base_rs::server::record::AlarmLimit::Double(
            self.pos.rbv,
        ))
    }

    /// The deadband gates RBV's monitor delivery (C motorRecord.cc
    /// 3468-3507), not VAL's.
    fn monitor_deadband_field(&self) -> &'static str {
        "RBV"
    }

    /// C `monitor()` posts every field it lists with `monitor_mask` on an
    /// alarm move, marked or not, but the group below its short circuit
    /// only when one of them is marked (motorRecord.cc:3550-3551). RBV is
    /// the deadband field and CNEN posts only when the MSTA block derives it.
    fn alarm_cycle_monitored_fields(&self) -> epics_base_rs::server::record::DeclaredFields {
        static MONITORED: [FieldIx; 44] = epics_base_rs::server::record::declared_set(
            dbd_generated::MOTOR_FIELDS,
            MONITOR_FIELDS,
        );
        if self.internal.monitor_lower_marked {
            &MONITORED[..43]
        } else {
            &MONITORED[..5]
        }
    }

    /// C `monitor()` posts from `mmap`/`nmap`, never on a value change.
    fn fields_posted_only_when_marked(&self) -> &'static [&'static str] {
        &POSTED_ONLY_WHEN_MARKED
    }

    /// The cycle's `monitor()` drains the marks; a put drains the literal
    /// posts its `special()` made.
    fn take_cycle_posted_fields(&mut self) -> CycleMarks {
        let bits = if std::mem::take(&mut self.internal.monitor_due) {
            self.take_monitor_marks()
        } else {
            std::mem::take(&mut self.internal.put_posted)
        };
        CycleMarks::of(&MONITOR_POSTS, u128::from(bits))
    }

    /// C `monitor()`'s RBV (motorRecord.cc:3468-3507): posted only when
    /// marked, through MDEL/ADEL unless both are zero.
    fn monitor_value_classes(
        &mut self,
        _gate: MonitorValueGate,
        last_posted: &mut [f64; 2],
    ) -> MonitorValueClasses {
        let (value, archive) = self.rbv_deadband();
        *last_posted = [self.disp.mlst, self.disp.alst];
        MonitorValueClasses {
            value,
            archive,
            hash_changed: false,
            marked: false,
        }
    }

    /// C `special()` STUP after-write (motorRecord.cc:3084-3090): a STUP put
    /// of any value other than ON is clamped to OFF and returns ERROR, which
    /// suppresses the pp(TRUE) reprocess — only a `STUP == ON` put runs the
    /// status-update process. `field_access` has already clamped `stup` to 0
    /// for any non-ON put by the time this gate runs, so the post-clamp value
    /// is the deterministic discriminator. Every other pp(TRUE) field
    /// reprocesses normally (default membership test).
    fn processes_after_put(&self, field: &str, pp: bool) -> bool {
        if field.eq_ignore_ascii_case("STUP") {
            return self.stat.stup == 1;
        }
        pp
    }

    /// C never derives the motor's UDF from VAL. `motorRecord.cc` touches
    /// `udf` in exactly three places: init_record (677-681, CONSTANT .dol →
    /// FALSE), the closed-loop DOL collection (1994-2005, DB_LINK read
    /// fail → TRUE / success → FALSE), and alarm_sub (3372 consumes it).
    /// The framework's default `clears_udf()` would instead recompute
    /// `udf = value_is_undefined()` (VAL.is_nan()) every process pass — a
    /// divergence that both fabricates UDF on a transient NaN VAL and, on a
    /// no-DOL-read pass, clobbers the legitimate closed-loop DOL outcome.
    /// Opt out so motor UDF is owned solely by the DOL channel
    /// (`dol_udf` → check_alarms) and the init clear in
    /// `post_init_finalize_undef`.
    fn clears_udf(&self) -> bool {
        false
    }

    /// alarm_sub (3372) is motor's only UDF_ALARM raise, at INVALID and on
    /// `udf == TRUE`; the framework's truthy UDFS check is not C's.
    fn raises_udf_alarm(&self) -> bool {
        false
    }

    /// The rest of C init_record after the device's init, when it gets
    /// there: its tail and `monitor()` (`MotorRecord::init_tail`), and
    /// 677-681: a CONSTANT .dol, an unset one included, leaves the record
    /// defined. A DB_LINK / CA DOL stays undefined until the closed-loop
    /// collection's first read (1994-2005), and a `{const:…}` one, a
    /// JSON_LINK, until a VAL put.
    fn post_init_finalize_undef(&mut self, udf: &mut u8) -> CaResult<()> {
        if !self.initialized {
            return Ok(());
        }
        self.init_tail();
        if self.constant_dol().is_some() {
            *udf = 0;
        }
        Ok(())
    }

    /// C `init_record` ends in `monitor()`, whose `recGblResetAlarms`
    /// (motorRecord.cc:3466) leaves `NO_ALARM`.
    fn init_resets_alarms(&self) -> bool {
        self.initialized
    }

    /// C `alarm_sub()` (motorRecord.cc:3367-3406) — motor-specific alarm
    /// severities, raised into `nsta`/`nsev` before the framework's
    /// `evaluate_alarms`.
    fn check_alarms(&mut self, common: &mut epics_base_rs::server::record::CommonFields) {
        self.alarm_sub(common);
        // No later alarm displaces an INVALID one: a COMM_ALARM nsta here
        // is the stat this pass commits.
        self.internal.comm_alarm =
            common.nsta == epics_base_rs::server::recgbl::alarm_status::COMM_ALARM;
    }
}

impl MotorRecord {
    fn alarm_sub(&mut self, common: &mut epics_base_rs::server::record::CommonFields) {
        use epics_base_rs::server::recgbl::{alarm_status, rec_gbl_set_sevr};
        use epics_base_rs::server::record::AlarmSeverity;

        // C closed-loop DOL (1999-2005): the DOL read outcome drives
        // UDF — a failed dbGetLink marks VAL undefined, a successful
        // one clears it. Applied here because this hook is where the
        // record sees CommonFields; a pass without a DOL read leaves
        // udf as-is, like C falling through the collection block.
        if let Some(undefined) = self.internal.dol_udf.take() {
            common.udf = undefined as u8;
        }

        // C 3372-3376: an undefined VAL short-circuits every other check.
        // C tests `udf == TRUE`, so a put byte of 2 or 255 is defined.
        if epics_base_rs::server::recgbl::udf_alarm_active(common.udf, true) {
            rec_gbl_set_sevr(common, alarm_status::UDF_ALARM, AlarmSeverity::Invalid);
            return;
        }

        // C 3379-3388: limit-switch and soft-limit violations. BOTH the
        // high and low arms gate on HLSV and raise at HLSV — motorRecord
        // has no low-side severity field. Limit switches count even when
        // not in the direction of travel (externally triggered move),
        // and a DVAL outside the dial soft limits alarms too.
        let hlsv = AlarmSeverity::from_u16(self.limits.hlsv as u16);
        if hlsv != AlarmSeverity::NoAlarm {
            if self.limits.hls != 0 || self.pos.dval > self.limits.dhlm {
                rec_gbl_set_sevr(common, alarm_status::HIGH_ALARM, hlsv);
                return;
            }
            if self.limits.lls != 0 || self.pos.dval < self.limits.dllm {
                rec_gbl_set_sevr(common, alarm_status::LOW_ALARM, hlsv);
                return;
            }
        }

        // C 3392-3398: controller communication error — COMM/INVALID,
        // and the MSTA bit is CLEARED (C `msta.Bits.CNTRL_COMM_ERR = 0`
        // + `MARK(M_MSTA)`) so the alarm is one-shot until the driver
        // re-reports it. The framework builds the monitor snapshot after
        // this hook, so the cleared MSTA posts in the same cycle.
        if self.stat.msta.contains(MstaFlags::COMM_ERR) {
            self.stat.msta.remove(MstaFlags::COMM_ERR);
            rec_gbl_set_sevr(common, alarm_status::COMM_ALARM, AlarmSeverity::Invalid);
        }

        // C 3400-3403: slip/stall or controller problem → STATE/MAJOR.
        // No early return above COMM — C falls through from the comm
        // check, so both can accumulate (set_sevr keeps the higher).
        if self
            .stat
            .msta
            .intersects(MstaFlags::SLIP_STALL | MstaFlags::PROBLEM)
        {
            rec_gbl_set_sevr(common, alarm_status::STATE_ALARM, AlarmSeverity::Major);
        }
    }
}

#[cfg(test)]
mod tests {
    use epics_base_rs::server::record::{FieldDeclaration, field_at};

    use super::*;

    /// A put processes the record exactly where motorRecord.dbd says
    /// pp(TRUE): PCOF/ICOF/DCOF/JVEL send from special() without one.
    #[test]
    fn the_puts_that_process_are_the_dbd_pp_set() {
        let dbd: Vec<&str> = dbd_generated::MOTOR_FIELDS
            .iter()
            .filter(|f| f.pp)
            .map(|f| f.name)
            .collect();
        assert_eq!(MotorRecord::new().process_passive_fields(), dbd);
    }

    #[test]
    fn test_set_mode_updates_offset() {
        let mut rec = MotorRecord::new();
        rec.conv.mres = 0.01;
        rec.pos.dval = 5.0;
        rec.conv.set = true;
        rec.put_field("VAL", EpicsValue::Double(100.0)).unwrap();
        let effects = rec.plan_motion(CommandSource::Val);
        // Offset should be updated, DVAL unchanged
        assert_eq!(rec.pos.dval, 5.0);
        assert_eq!(rec.pos.off, 95.0); // 100 - 1*5
        // C 2206-2227: the FOFF=Variable redefinition completes in the
        // collection — no controller command is dispatched (LOAD_POS
        // belongs to the Frozen/DVAL/RVAL paths) and DMOV stays TRUE.
        assert!(effects.commands.is_empty());
        assert!(rec.stat.dmov != 0);
        assert!(rec.stat.mip.is_empty());
    }

    #[test]
    fn test_should_fire_forward_link() {
        let mut rec = MotorRecord::new();
        assert!(rec.should_fire_forward_link());

        // C motorRecord.cc:1509-1510: DMOV is the only FLNK gate.
        rec.stat.dmov = 0;
        assert!(!rec.should_fire_forward_link());
    }

    // C special() STUP after-write (motorRecord.cc:3084-3090): only a
    // STUP==ON put runs the pp(TRUE) reprocess; any other value is clamped to
    // OFF and C returns ERROR so the record does not process. R24. Fresh
    // records per case so the STUP-in-flight before-write veto (stup != OFF)
    // does not reject the second put.
    #[test]
    fn test_stup_processes_after_put_only_when_on() {
        // STUP == ON -> reprocess.
        let mut on = MotorRecord::new();
        on.internal.init_invariants_synced = true;
        on.put_field("STUP", EpicsValue::Short(1)).unwrap();
        assert_eq!(on.stat.stup, 1);
        assert!(on.processes_after_put("STUP", true));

        // STUP == BUSY -> clamped to OFF, no reprocess (C return ERROR).
        let mut off = MotorRecord::new();
        off.internal.init_invariants_synced = true;
        off.put_field("STUP", EpicsValue::Short(2)).unwrap();
        assert_eq!(off.stat.stup, 0);
        assert!(!off.processes_after_put("STUP", true));

        // Every other field takes the declared pp bit.
        assert!(off.processes_after_put("VAL", true));
        assert!(!off.processes_after_put("VELO", false));
    }

    // C motorRecord.cc:1495 — every process pass fires the RLNK readback
    // output link with the current RBV via dbPutLink().
    #[test]
    fn rlnk_readback_link_fired_with_rbv_each_process() {
        let mut rec = MotorRecord::new();
        rec.links.rlnk = "readback_sink.PROC".into();
        rec.pos.rbv = 12.5;
        let outcome = rec.process().unwrap();
        let rbv = rec.pos.rbv;
        assert!(
            outcome.actions.iter().any(|a| matches!(
                a,
                ProcessAction::WriteDbLink {
                    link_field: "RLNK",
                    value: EpicsValue::Double(v),
                } if *v == rbv
            )),
            "process() must emit a WriteDbLink to RLNK carrying RBV"
        );
    }

    // C motorRecord.cc:1495 fires `dbPutLink(&rlnk, ...)` on the move-start
    // pass too — before `monitor()` (motorRecord.cc:1507) and the
    // `recGblFwdLink` gate (motorRecord.cc:1509) — so the RLNK readback
    // target re-processes even while the move is still pending.
    #[test]
    fn rlnk_readback_link_fired_on_move_start() {
        let mut rec = MotorRecord::new();
        rec.links.rlnk = "readback_sink.PROC".into();
        rec.put_field("VAL", EpicsValue::Double(10.0)).unwrap();
        rec.set_event(MotorEvent::UserWrite(CommandSource::Val));
        let outcome = rec.process().unwrap();
        assert!(rec.stat.dmov == 0, "the pass started the move");
        let rbv = rec.pos.rbv;
        assert!(
            outcome.actions.iter().any(|a| matches!(
                a,
                ProcessAction::WriteDbLink {
                    link_field: "RLNK",
                    value: EpicsValue::Double(v),
                } if *v == rbv
            )),
            "the move-start cycle must still emit the RLNK readback write"
        );
    }

    #[test]
    fn rlnk_not_fired_when_link_unset() {
        let mut rec = MotorRecord::new();
        assert!(rec.links.rlnk.is_empty());
        let outcome = rec.process().unwrap();
        assert!(
            !outcome.actions.iter().any(|a| matches!(
                a,
                ProcessAction::WriteDbLink {
                    link_field: "RLNK",
                    ..
                }
            )),
            "an unset RLNK must emit no readback link write"
        );
    }

    // C do_work marks M_MIP on the move dispatch (2461-2462) and M_RCNT
    // when the reset clears a nonzero count (2352-2356); monitor() posts
    // both on the move-start pass.
    #[test]
    fn move_start_notify_carries_mip_and_rcnt() {
        let mut rec = MotorRecord::new();
        rec.retry.rcnt = 2;
        rec.put_field("VAL", EpicsValue::Double(10.0)).unwrap();
        rec.set_event(MotorEvent::UserWrite(CommandSource::Val));
        rec.process().unwrap();
        assert!(rec.stat.dmov == 0, "the pass started the move");
        let fields: Vec<(&str, EpicsValue)> = rec
            .take_cycle_posted_fields()
            .map(|(ix, _)| {
                let name = field_at(dbd_generated::MOTOR_FIELDS, ix).unwrap().name;
                (name, rec.get_field(name).unwrap())
            })
            .collect();
        let mip = rec.stat.mip.bits();
        assert!(
            fields
                .iter()
                .any(|(n, v)| *n == "MIP" && *v == EpicsValue::UShort(mip)),
            "move-start notify must carry MIP: {fields:?}"
        );
        assert!(
            fields
                .iter()
                .any(|(n, v)| *n == "RCNT" && *v == EpicsValue::Short(rec.retry.rcnt)),
            "move-start notify must carry RCNT: {fields:?}"
        );
    }

    // C monitor() (motorRecord.cc:3468-3507): the MDEL/ADEL deadband
    // gates the RBV post. The framework keys delivery off this hook
    // pair — both must track RBV.
    #[test]
    fn monitor_deadband_hooks_track_rbv() {
        let mut rec = MotorRecord::new();
        rec.pos.rbv = 7.25;
        assert_eq!(rec.monitor_deadband_field(), "RBV");
        assert_eq!(
            rec.monitor_deadband_value(),
            Some(epics_base_rs::server::record::AlarmLimit::Double(7.25))
        );
    }

    // C motorRecord.cc:3687 — URIP=Yes pulls the readback from the RDBL link
    // before process(); the status path then scales it into RRBV.
    #[test]
    fn urip_pulls_rdbl_link_before_process() {
        let mut rec = MotorRecord::new();
        rec.conv.urip = true;
        rec.links.rdbl = "ext_readback.RBV".into();
        let actions = rec.pre_process_actions();
        assert!(
            actions.iter().any(|a| matches!(
                a,
                ProcessAction::ReadDbLink {
                    link_field: "RDBL",
                    target_field: "RDBL_VAL",
                }
            )),
            "URIP=Yes must request a pre-process read of RDBL into RDBL_VAL"
        );
    }

    #[test]
    fn no_rdbl_pull_when_urip_off() {
        let mut rec = MotorRecord::new();
        // Link configured, but URIP=No — C reads RRBV from the controller.
        rec.conv.urip = false;
        rec.links.rdbl = "ext_readback.RBV".into();
        let actions = rec.pre_process_actions();
        assert!(
            !actions.iter().any(|a| matches!(
                a,
                ProcessAction::ReadDbLink {
                    link_field: "RDBL",
                    ..
                }
            )),
            "URIP=No must not read the RDBL link"
        );
    }

    #[test]
    fn no_rdbl_pull_when_ueip_wins() {
        // C process_motor_info (3676-3699) else-if: UEIP=Yes takes the
        // encoder and the RDBL dbGetLink never executes.
        let mut rec = MotorRecord::new();
        rec.conv.urip = true;
        rec.conv.ueip = true;
        rec.links.rdbl = "ext_readback.RBV".into();
        let actions = rec.pre_process_actions();
        assert!(
            !actions.iter().any(|a| matches!(
                a,
                ProcessAction::ReadDbLink {
                    link_field: "RDBL",
                    ..
                }
            )),
            "UEIP=Yes must suppress the RDBL read"
        );
    }

    fn has_dol_read(actions: &[ProcessAction]) -> bool {
        actions.iter().any(|a| {
            matches!(
                a,
                ProcessAction::ReadDbLink {
                    link_field: "DOL",
                    target_field: "VAL",
                }
            )
        })
    }

    // C motorRecord.cc:1994 — in CLOSED_LOOP mode VAL is driven by the DOL DB
    // link each idle pass.
    #[test]
    fn closed_loop_db_dol_drives_val() {
        let mut rec = MotorRecord::new();
        rec.links.omsl = 1; // menuOmsl: closed_loop
        rec.links.dol = "setpoint_src.VAL".into(); // resolves to a DB link
        rec.set_link_type("DOL", LinkType::Db);
        assert!(rec.stat.dmov != 0, "record starts done");
        assert!(
            has_dol_read(&rec.pre_process_actions()),
            "CLOSED_LOOP with a DB DOL must read DOL into VAL"
        );
    }

    #[test]
    fn supervisory_omsl_ignores_dol() {
        let mut rec = MotorRecord::new();
        rec.links.omsl = 0; // supervisory
        rec.links.dol = "setpoint_src.VAL".into();
        rec.set_link_type("DOL", LinkType::Db);
        assert!(
            !has_dol_read(&rec.pre_process_actions()),
            "supervisory OMSL must not read DOL"
        );
    }

    // C closed-loop DOL collection (motorRecord.cc:1999-2005): a failed
    // dbGetLink sets udf = TRUE (UDF/INVALID alarm); a successful read
    // clears it. A pass without a DOL read leaves udf untouched.
    #[test]
    fn closed_loop_dol_read_failure_drives_udf() {
        use epics_base_rs::server::record::CommonFields;
        let mut rec = MotorRecord::new();
        rec.links.omsl = 1; // closed_loop
        rec.links.dol = "setpoint_src.VAL".into();
        rec.set_link_type("DOL", LinkType::Db);
        assert!(rec.stat.dmov != 0, "record starts done");
        let mut common = CommonFields::default();

        // Failed read: the resolved report omits DOL.
        rec.set_resolved_input_links(ResolvedInputLinks::of_names(&[]));
        rec.check_alarms(&mut common);
        assert!(common.udf != 0, "failed DOL read marks VAL undefined");

        // Successful read clears it.
        rec.set_resolved_input_links(ResolvedInputLinks::of_names(&["DOL"]));
        rec.check_alarms(&mut common);
        assert!(common.udf == 0, "successful DOL read clears UDF");

        // Supervisory pass (no DOL request): udf stays as-is.
        common.udf = 1;
        rec.links.omsl = 0;
        rec.set_resolved_input_links(ResolvedInputLinks::of_names(&[]));
        rec.check_alarms(&mut common);
        assert!(
            common.udf != 0,
            "a pass without a DOL read leaves udf alone"
        );
    }

    // A request_poll forces the post; otherwise a held button strands on a
    // stationary axis whose status never changes. A settled pass asks for
    // nothing.
    #[test]
    fn request_poll_forces_poll_directive() {
        let mut rec = MotorRecord::new();
        rec.stat.dmov = 1;
        assert!(
            !rec.effects_to_actions(&ProcessEffects::default())
                .force_poll
        );
        let request_poll = ProcessEffects {
            request_poll: true,
            ..Default::default()
        };
        assert!(
            rec.effects_to_actions(&request_poll).force_poll,
            "request_poll must force a poll"
        );
    }

    // R44: C never derives motor UDF from VAL — motorRecord.cc touches udf
    // only at init_record (677-681), the closed-loop DOL collection
    // (1994-2005), and alarm_sub (3372). clears_udf() opts out of the
    // framework's value_is_undefined() per-cycle derivation.
    #[test]
    fn motor_opts_out_of_val_derived_udf() {
        let rec = MotorRecord::new();
        assert!(
            !rec.clears_udf(),
            "motor UDF must not be recomputed from VAL each process pass"
        );
    }

    // C init_record (677-681): a CONSTANT .dol clears UDF at init. An unset
    // DOL is CONSTANT, so the common (no-DOL) axis is defined after init.
    #[test]
    fn constant_dol_clears_udf_at_init() {
        use epics_base_rs::server::record::CommonFields;
        let mut rec = MotorRecord::new();
        rec.links.dol = "".into(); // unset DOL == CONSTANT
        let status = asyn_rs::interfaces::motor::MotorStatus {
            position: 0.0,
            encoder_position: 0.0,
            done: true,
            moving: false,
            ..Default::default()
        };
        rec.initial_readback(&status);
        let mut udf = CommonFields::default().udf; // dbCommon default
        rec.post_init_finalize_undef(&mut udf).unwrap();
        assert_eq!(udf, 0, "CONSTANT DOL motor is defined after init");
    }

    // C init_record returns before its tail with no dset or a failed dset
    // init (612-651): no UDF clear.
    #[test]
    fn a_record_whose_init_returns_early_stays_undefined() {
        let mut rec = MotorRecord::new();
        let mut udf = 1;
        rec.post_init_finalize_undef(&mut udf).unwrap();
        assert_eq!(udf, 1);
        assert!(!rec.init_resets_alarms());
    }

    // C init_record's closing monitor() copies EA_POSITION into CNEN for a
    // GAIN_SUPPORT axis (3541-3548), so an idle axis shows it unprocessed.
    #[test]
    fn init_copies_cnen_from_msta() {
        let mut rec = MotorRecord::new();
        rec.init_record(1).unwrap();
        let status = asyn_rs::interfaces::motor::MotorStatus {
            done: true,
            powered: true,
            gain_support: true,
            ..Default::default()
        };
        rec.initial_readback(&status);
        rec.post_init_finalize_undef(&mut 1).unwrap();
        assert!(rec.ctrl.cnen, "CNEN follows EA_POSITION at init");
        assert_eq!(rec.internal.mmap, 0, "init monitor() drains the marks");
    }

    /// An idle record at dial 0 with OFF -0.49, through init's closing
    /// monitor().
    fn init_at_offset(mdel: f64, adel: f64) -> MotorRecord {
        let mut rec = MotorRecord::new();
        rec.pos.off = -0.49;
        rec.disp.mdel = mdel;
        rec.disp.adel = adel;
        rec.init_record(1).unwrap();
        rec.initial_readback(&asyn_rs::interfaces::motor::MotorStatus {
            done: true,
            ..Default::default()
        });
        rec.post_init_finalize_undef(&mut 1).unwrap();
        rec
    }

    // C init_record's closing monitor() runs the RBV deadband (3482-3501):
    // the init readback crossing MDEL or ADEL becomes MLST or ALST.
    #[test]
    fn init_monitor_moves_mlst_and_alst_to_the_readback() {
        let rec = init_at_offset(0.001, 0.0);
        assert_eq!((rec.disp.mlst, rec.disp.alst), (-0.49, 0.0));
        let rec = init_at_offset(0.0, 0.001);
        assert_eq!((rec.disp.mlst, rec.disp.alst), (0.0, -0.49));
    }

    // With both deadbands 0 C posts RBV without touching MLST/ALST (3468-3474).
    #[test]
    fn init_monitor_without_deadbands_leaves_mlst_and_alst() {
        let rec = init_at_offset(0.0, 0.0);
        assert_eq!((rec.disp.mlst, rec.disp.alst), (0.0, 0.0));
    }

    // C leaves udf TRUE for a non-CONSTANT .dol (init_record only clears for
    // CONSTANT, 677-681); only the closed-loop collection's first successful
    // read clears it (1994-2005).
    #[test]
    fn db_link_dol_leaves_udf_undefined_at_init() {
        use epics_base_rs::server::record::CommonFields;
        let mut rec = MotorRecord::new();
        rec.links.omsl = 1; // closed_loop
        rec.links.dol = "setpoint_src.VAL".into(); // DB_LINK
        rec.set_link_type("DOL", LinkType::Db);
        let status = asyn_rs::interfaces::motor::MotorStatus {
            position: 0.0,
            encoder_position: 0.0,
            done: true,
            moving: false,
            ..Default::default()
        };
        rec.initial_readback(&status);
        let mut udf = CommonFields::default().udf;
        rec.post_init_finalize_undef(&mut udf).unwrap();
        assert_eq!(
            udf, 1,
            "DB_LINK DOL motor stays undefined until the first DOL read"
        );
    }

    // C reads DOL only when dol.type == DB_LINK; a constant DOL is initialised
    // once and not re-read (motorRecord.cc:1994 vs :680).
    #[test]
    fn closed_loop_constant_dol_not_reread() {
        let mut rec = MotorRecord::new();
        rec.links.omsl = 1;
        rec.links.dol = "5.0".into(); // CONSTANT, not a DB link
        assert!(
            !has_dol_read(&rec.pre_process_actions()),
            "a constant DOL must not be re-read each pass"
        );
    }

    // C calls do_work (the DOL read) for pmr->dmov, not on a CALLBACK_DATA
    // poll mid-move (motorRecord.cc:1487-1492).
    #[test]
    fn closed_loop_dol_not_read_mid_move() {
        let mut rec = MotorRecord::new();
        rec.links.omsl = 1;
        rec.links.dol = "setpoint_src.VAL".into();
        rec.set_link_type("DOL", LinkType::Db);
        rec.stat.dmov = 0; // a move is in flight
        assert!(
            !has_dol_read(&rec.pre_process_actions()),
            "DOL must not be re-read while a move is in flight"
        );
    }

    // C motorRecord.cc:1509-1510 (restored by c970afbf): FLNK fires on
    // every pass exiting with DMOV true — including a bare idle pass.
    #[test]
    fn test_flnk_fires_on_idle_pass_with_dmov_true() {
        let mut rec = MotorRecord::new();
        assert!(rec.stat.dmov != 0);
        let _ = rec.process();
        assert!(
            rec.should_fire_forward_link(),
            "an idle pass exits with DMOV true — C fires FLNK on it"
        );
    }

    #[test]
    fn test_flnk_fires_on_motion_completion_transition() {
        let mut rec = MotorRecord::new();
        rec.conv.mres = 1.0;
        // Enter a move: DMOV goes true→false.
        rec.put_field("VAL", EpicsValue::Double(10.0)).unwrap();
        rec.set_event(MotorEvent::UserWrite(CommandSource::Val));
        let _ = rec.process();
        assert!(rec.stat.dmov == 0); // moving
        assert!(!rec.should_fire_forward_link()); // suppressed while moving

        // Driver reports completion; next process finalizes DMOV false→true.
        rec.set_event(MotorEvent::DeviceUpdate(
            asyn_rs::interfaces::motor::MotorStatus {
                position: 10.0,
                encoder_position: 10.0,
                done: true,
                moving: false,
                ..Default::default()
            },
        ));
        let _ = rec.process();
        assert!(rec.stat.dmov != 0); // completed
        assert!(
            rec.should_fire_forward_link(),
            "FLNK must fire on the DMOV false→true completion transition"
        );
    }

    // Public C motorRecord.dbd field surface: every link/menu/alarm/last-value
    // field a CA or PVA client can address must be discoverable through
    // field_list() and round-trip through get/put (writable) or read (NOMOD).
    // Behavioral link routing (DOL closed-loop drive, RDBL readback pull, RLNK
    // forward firing) is deliberately outside this field-presence import.
    #[test]
    fn dbd_link_menu_alarm_fields_are_field_open() {
        use epics_base_rs::types::DbFieldType;

        let mut rec = MotorRecord::new();

        // Writable DBF_INLINK/OUTLINK, DBF_MENU and DBF_DOUBLE alarm fields:
        // present, writable, and surviving a put/get round-trip.
        let writable: &[(&str, DbFieldType, EpicsValue)] = &[
            (
                "RDBL",
                DbFieldType::String,
                EpicsValue::String("$(P)$(R).RBV CP".into()),
            ),
            (
                "DOL",
                DbFieldType::String,
                EpicsValue::String("$(P)setpoint CP".into()),
            ),
            (
                "RLNK",
                DbFieldType::String,
                EpicsValue::String("$(P)done.PROC PP".into()),
            ),
            (
                "OUT",
                DbFieldType::String,
                EpicsValue::String("@asyn(PORT,0)".into()),
            ),
            // `field(OMSL,DBF_MENU)` (motorRecord.dbd:256). The DECLARED type is
            // the enum a client is served (`mapDBFToDBR`); the stored index is
            // still a short. This asserted `Short` while the hand-written table
            // declared it so — the declaration is the `.dbd`'s now.
            ("OMSL", DbFieldType::Enum, EpicsValue::Short(1)),
            ("HIHI", DbFieldType::Double, EpicsValue::Double(95.0)),
        ];
        for (name, dbf, val) in writable {
            let desc = rec
                .field_list()
                .iter()
                .find(|f| f.name == *name)
                .unwrap_or_else(|| panic!("{name} missing from motor field_list()"));
            assert_eq!(desc.dbf_type, *dbf, "{name} dbf_type");
            assert!(!desc.read_only, "{name} must be writable");
            rec.put_field(name, val.clone())
                .unwrap_or_else(|e| panic!("put {name}: {e:?}"));
            assert_eq!(rec.get_field(name), Some(val.clone()), "{name} round-trip");
        }

        // OMSL/HHSV are menu indices and must clamp to their menu range.
        rec.put_field("OMSL", EpicsValue::Short(7)).unwrap();
        assert_eq!(rec.get_field("OMSL"), Some(EpicsValue::Short(1)));

        // Read-only last-value / monitor-map fields (SPC_NOMOD): present,
        // readable, and flagged read-only so the record layer rejects writes.
        let read_only: &[(&str, DbFieldType)] = &[
            ("ALST", DbFieldType::Double),
            ("MLST", DbFieldType::Double),
            // `field(MMAP,DBF_ULONG)` / `field(NMAP,DBF_ULONG)` — the monitor
            // maps are unsigned 32-bit bitmasks. The hand table said DBF_LONG.
            ("MMAP", DbFieldType::ULong),
            ("NMAP", DbFieldType::ULong),
        ];
        for (name, dbf) in read_only {
            let desc = rec
                .field_list()
                .iter()
                .find(|f| f.name == *name)
                .unwrap_or_else(|| panic!("{name} missing from motor field_list()"));
            assert_eq!(desc.dbf_type, *dbf, "{name} dbf_type");
            assert!(desc.read_only, "{name} must be read-only (SPC_NOMOD)");
            assert!(rec.get_field(name).is_some(), "{name} must be readable");
        }
    }
}

#[cfg(test)]
mod menu_choice_tests {
    use super::MotorRecord;
    use epics_base_rs::server::record::FieldDeclaration;

    /// The choices a client sees are the DECLARATION's — `motorRecord.dbd`'s
    /// `menu()` on each field — and the index↔string mapping is wire-visible.
    /// This used to assert them through `Record::menu_field_choices`, a hand
    /// written table that declared the same menus a second time; `HLSV` needed
    /// a per-record mapping there only because the shared-menu registry keys
    /// `menuAlarmSevr` by the standard field names. The declaration has no such
    /// problem: `field(HLSV,DBF_MENU) { menu(menuAlarmSevr) }` points the
    /// FieldDesc straight at base's `MENU_ALARM_SEVR`.
    #[test]
    fn motor_menu_choices_come_from_the_declaration() {
        let rec = MotorRecord::new();
        let menu = |name: &str| {
            rec.field_list()
                .iter()
                .find(|f| f.name == name)
                .unwrap_or_else(|| panic!("{name} is declared"))
                .menu
        };
        assert_eq!(menu("DIR"), Some(&["Pos", "Neg"][..]));
        assert_eq!(menu("FOFF"), Some(&["Variable", "Frozen"][..]));
        assert_eq!(menu("SPMG"), Some(&["Stop", "Pause", "Move", "Go"][..]));
        assert_eq!(
            menu("RMOD"),
            Some(&["Default", "Arithmetic", "Geometric", "In-Position"][..])
        );
        assert_eq!(menu("STUP"), Some(&["OFF", "ON", "BUSY"][..]));
        // NTM is menu(menuYesNo) — base's table, referenced, not re-declared.
        assert_eq!(menu("NTM"), Some(&["NO", "YES"][..]));
        // HLSV is menu(menuAlarmSevr) — likewise base's.
        assert_eq!(
            menu("HLSV"),
            Some(&["NO_ALARM", "MINOR", "MAJOR", "INVALID"][..])
        );
        // UEIP/URIP share menu(motorUEIP).
        assert_eq!(menu("UEIP"), menu("URIP"));
        assert_eq!(menu("VAL"), None);
    }
}
