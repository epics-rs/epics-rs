use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use super::calc_compile;
use super::link_status::{SWAIT_NO_PV, SWAIT_PV_NC, SWAIT_PV_OK, SWAIT_PV_STATUS_CHOICES};
use crate::calc::NumericInputs;
use crate::calc::{Rpcl, eval as calc_eval};
use crate::error::{CaError, CaResult};
use crate::runtime::task::{CallbackPriority, spawn_blocking_background};
use crate::server::database::AsyncDbHandle;
use crate::server::rec_dyn_link::{
    CaType, Deliver, Io, LinkEvent, LinkHost, LinkSpec, NOTIFY_IN_PROGRESS, RecDynLink,
};
use crate::server::recgbl::PutReport;
use crate::server::record::{CycleCell, CycleRead, CycleValue, ScanType};
use crate::server::record::{
    FieldMetadataOverride, MENU_YES_NO, ProcessAction, ProcessOutcome, Record, RecordProcessResult,
};
use crate::types::PvString;
use crate::types::{EpicsValue, c_cast};

/// The PV-status fields, in C's `NUM_LINKS` order: the twelve inputs
/// (`INAV`..`INLV`), then `DOLV`, then `OUTV`. C keeps the statuses in one
/// contiguous array starting at `pwait->inav` and walks it with the link names
/// starting at `pwait->inan` (swaitRecord.c:334-338, 686-687), so the index of
/// a status field IS the index of its link.
const SWAIT_PV_STATUS_FIELDS: [&str; 14] = [
    "INAV", "INBV", "INCV", "INDV", "INEV", "INFV", "INGV", "INHV", "INIV", "INJV", "INKV", "INLV",
    "DOLV", "OUTV",
];

/// C `DOL_INDEX` and `OUT_INDEX`: the links after the twelve inputs.
const DOL_INDEX: usize = 12;
const OUT_INDEX: usize = 13;

/// The callbacks swait hands `recDynLink` (`swaitRecord.c`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cb {
    /// `pvSearchCallback`.
    Search,
    /// `notifyCallback`.
    Notify,
    /// `inputChanged`.
    Input,
}

/// Link events in arrival order: queued by a `recDynLink` thread without the
/// record's lock, applied under it from a callback worker.
#[derive(Default)]
struct Inbox(Mutex<VecDeque<(usize, u64, LinkEvent)>>);

/// What a drained link event leaves the callback worker to do outside the
/// record's borrow.
enum Drain {
    Done,
    /// `ioIntProcess`'s `dbProcess`.
    Process,
    /// `notifyCallback`'s `recGblFwdLink`.
    FwdLink,
}

/// C `isBlank`: a non-empty name of white space alone.
fn is_blank(name: &[u8]) -> bool {
    !name.is_empty() && name.iter().all(|b| matches!(b, b' ' | b'\t'..=b'\r'))
}

/// `swaitRecord.dbd:431` `char rpcl[184]`.
const SWAIT_RPCL_SIZE: usize = 184;

// swait (string wait) record from synApps calc module.
// Functionally equivalent to scalcout, but uses INxN/INxP naming for input links
// (INxN = link name, INxP = process passive flag) instead of INPx.
// Supports 12 numeric inputs (A-L), a CALC expression, and a single output link.
// DOL/DOLD: desired output; DOPT: 0=use CALC, 1=use DOLD; OOPT: output condition.
pub struct SwaitRecord {
    pub val: f64,
    pub calc: String,
    /// C `RPCL`. Always a program: an empty or uncompilable CALC carries C's
    /// empty `END_EXPRESSION` postfix, which `calcPerform` refuses to run, so
    /// the record alarms on every process. See [`calc_compile`].
    rpcl: Rpcl<SWAIT_RPCL_SIZE>,
    /// CLCV ("CALC Valid", `swaitRecord.dbd:433`, `DBF_LONG`) — `postfix()`'s
    /// return status, stored on every compile (`swaitRecord.c:304` at init,
    /// `:561` in `special(SPC_CALC)`) and posted `DBE_VALUE` (`:309`, `:566`).
    /// 0 when the CALC compiled, -1 when it did not; the same
    /// `calc_compile`-owned status calcout/scalcout/acalcout store.
    pub clcv: i32,
    /// INIT ("Initialized?", `swaitRecord.dbd:42`, `DBF_SHORT`, `SPC_NOMOD`).
    /// C `swaitRecord.c:330` and `:374` set `pwait->init = TRUE` in
    /// `init_record` and NOTHING ever clears it — unlike `ai`/`ao`'s INIT this
    /// is not a conversion phase but a latch: "this record has been through
    /// init_record".
    init: i16,
    /// This cycle's `calcPerform` outcome. C `swaitRecord.c:409-410`:
    /// `if (calcPerform(...)) recGblSetSevr(pwait, CALC_ALARM, INVALID_ALARM)`.
    ///
    /// A per-cycle fact, not record state: [`Record::check_alarms`] — the single
    /// owner of this record's alarm transitions — CONSUMES it (`mem::take`), so
    /// it cannot outlive the cycle that set it. A cycle whose calc never ran
    /// (the fetch gate failed, or the record is simulated) therefore raises no
    /// CALC_ALARM, exactly as C's per-cycle `nsev`/`nsta` do.
    calc_alarm: bool,
    /// A successful `calcPerform` ran this cycle — C `swaitRecord.c:411`
    /// (`} else pwait->udf = FALSE;`), the ONLY place swait's own code clears
    /// UDF (the other, `:419`, is the simulation SIOL read, which the
    /// framework's simulation owner performs). Same producer/consumer shape as
    /// [`Self::calc_alarm`]: raised by the calc, consumed by
    /// [`Record::check_alarms`], which is the only code allowed to touch
    /// `common.udf` on swait's behalf. C never sets `udf = TRUE` and never
    /// re-derives it from VAL, so nothing else may either — see
    /// [`Record::clears_udf`]/[`Record::raises_udf_alarm`] below.
    calc_succeeded: bool,
    pub oopt: i16,
    pub dopt: i16,
    // DOLN ("DOL PV Name", C `swaitRecord.dbd:150`, DBF_STRING/SPC_MOD) and
    // DOLD ("Desired Output Data", :466). With DOPT="Use DOL", C `execOutput`
    // (swaitRecord.c:763-772) reads DOLN's link into DOLD at OUTPUT time and
    // writes DOLD to OUT. With DOLV not PV_OK the get is skipped and DOLD keeps
    // whatever was written to it (an operator/client put, or the init value).
    pub doln: PvString,
    pub dold: f64,
    // OVAL ("Old Value", C `swaitRecord.dbd:440`): the VAL of the PREVIOUS
    // cycle. C sets it at `swaitRecord.c:471` (`pwait->oval = pwait->val`,
    // after the OOPT test that reads it) and never posts it or writes it out —
    // its only consumer is the OOPT comparison (:432-446). It is NOT an output
    // staging cell: `execOutput` composes the output value from VAL or DOLD.
    pub oval: f64,
    // OEVT ("Output Event") — C `swaitRecord.c` `pwait->oevt` (DBF_USHORT).
    // When output fires and `oevt > 0`, `execOutput` posts the numeric
    // software event (`post_event((int)oevt)`, swaitRecord.c:797); see
    // [`Record::output_event`]. swait has no IVOA field, so the post is
    // never suppressed by the framework Don't_drive veto.
    pub oevt: u16,
    // ODLY ("Output Execute Delay", seconds) — C `swaitRecord.c` `pwait->odly`
    // (DBF_FLOAT). When output fires and `odly > 0`, `schedOutput`
    // (swaitRecord.c:719) defers the OUT write + forward link + OEVT post by
    // `odly` seconds via the watchdog, holding the record active (PACT=1); when
    // `odly == 0` it calls `execOutput` immediately. `f32` mirrors the C
    // `float` field so a CA client sees DBR_FLOAT (not DBR_DOUBLE).
    pub odly: f32,
    /// OUTN ("OUT PV Name", `swaitRecord.dbd:158`, DBF_STRING/SPC_MOD): the
    /// name of the output link, a PV name and not a link field.
    pub outn: PvString,
    pub prec: i16,
    // MDEL / ADEL (C `swaitRecord.dbd:477-486`, both DBF_DOUBLE): the monitor
    // and archive deadbands `monitor()` (swaitRecord.c:622-640) tests VAL
    // against to build `monitor_mask`. The record had neither field, so both
    // read back as the framework's 0.0 default and every VAL change crossed
    // both deadbands — and a client could not set them at all
    // (`FieldNotFound`). The A..L input posts inherit this mask, which is what
    // makes their DBE_LOG bit conditional (see `fields_posted_with_monitor_mask`).
    pub mdel: f64,
    pub adel: f64,
    /// MLST / ALST — C `swaitRecord.dbd`, both `DBF_DOUBLE`
    /// `special(SPC_NOMOD)`. `monitor()` deadbands VAL against them
    /// (swaitRecord.c:630,639). The record served neither, so the
    /// framework had no cell to remember the last posted value in and
    /// treated every cycle as the first one.
    pub mlst: f64,
    pub alst: f64,
    // INxN: input link names; INxP: process passive flags (0/1)
    pub inp_names: [PvString; 12], // INAN..INLN
    pub inp_passive: [i16; 12],    // INAP..INLP
    // numeric input values A-L
    pub num_vals: [f64; 12],
    // LA..LL ("Last Val of Input x", C `swaitRecord.dbd:298-331`, DBF_DOUBLE,
    // no SPC_NOMOD so a client may write them). C `monitor()`
    // (swaitRecord.c:647-654) is the single writer during processing: for each
    // input it changed, it posts the input, advances `*pprev = *pnew`, and
    // posts the previous-value field too — both with `monitor_mask | DBE_VALUE`
    // (see `fields_posted_with_monitor_mask`). The record had no LA..LL at all,
    // so the change history C publishes was unreadable and a put was rejected
    // with `FieldNotFound`.
    pub prev_vals: [f64; 12],
    // INAV..INLV / DOLV / OUTV — the `menu(swaitINAV)` PV-connection status of
    // each link, indexed as `SWAIT_PV_STATUS_FIELDS` (C keeps the same array,
    // swaitRecord.c:334-338). Read-only to clients (`special(SPC_NOMOD)`,
    // swaitRecord.dbd:166-250). Each is its link's state, written by
    // `name_changed` and `pv_search_callback`.
    pv_status: [i16; 14],
    /// C `caLinkStruct[NUM_LINKS]`, indexed as `pv_status`.
    links: [RecDynLink<Cb>; 14],
    inbox: Arc<Inbox>,
    /// C `ioProcCb`'s priority, set from PRIO at init (`swaitRecord.c:313`):
    /// the band link events are applied on.
    prio: CallbackPriority,
    /// The `pv_status` and link-name posts owed, a bit per link.
    status_posts: u16,
    name_posts: u16,
    // This cycle's `fetch_values()` outcome. C `swaitRecord.c::process`
    // (407-414):
    //
    // ```c
    // if (fetch_values(pwait)==0) {
    //     if (calcPerform(...)) recGblSetSevr(pwait,CALC_ALARM,INVALID_ALARM);
    //     else pwait->udf = FALSE;
    // } else {
    //     recGblSetSevr(pwait,READ_ALARM,INVALID_ALARM);
    // }
    // ```
    //
    // — so unlike the calc family, a failed input ALSO raises READ_ALARM at
    // INVALID severity. The OOPT switch that follows (:425) is outside the
    // gate and runs against the frozen VAL.
    fetch_failed: bool,
    // Simulation mode — C `swaitRecord.dbd:497-517` (SIOL, SVAL, SIML, SIMM,
    // SIMS) and `swaitRecord.c:402-422`. The record had none of these fields, so
    // a swait could not be put into simulation at all: SIMM was unreadable and
    // unwritable, a SIML/SIOL link could not be configured, and every cycle ran
    // the real input fetch + calc.
    //
    // SIML ("Sim Mode Location", DBF_INLINK) refreshes SIMM every process
    // (`:402`); SIMM ("Simulation Mode", menuYesNo) selects the branch; SIOL
    // ("Sim Input Specifctn", DBF_INLINK) feeds SVAL ("Simulation Value",
    // DBF_DOUBLE), which becomes VAL; SIMS ("Sim mode Alarm Svrty",
    // menuAlarmSevr) is the severity of the SIMM_ALARM every simulated cycle
    // raises (`:421`).
    //
    // The framework owns the branch (`Record::simulation_substitutes_input_stage`
    // + `check_simulation_mode`): it resolves SIMM from SIML, reads SIOL into
    // SVAL, writes VAL, raises SIMM_ALARM, and pushes `simulation_active` here.
    pub siml: PvString,
    pub simm: i16,
    pub siol: PvString,
    pub sval: f64,
    pub sims: i16,
    // This cycle's simulation state, pushed by the framework through
    // `set_simulation_active` before `process()`. C `swaitRecord.c:407/415`: a
    // simulated cycle takes the `else` branch, which runs NEITHER
    // `fetch_values()` NOR `calcPerform()`.
    simulation_active: bool,
    // The record's name and database, which every link is added on.
    async_ctx: Option<(crate::types::RecordName, AsyncDbHandle)>,
    /// This cycle's SCAN is I/O Intr.
    io_intr: bool,
    /// C `cbStruct.procPending`: an input changed during the ODLY wait.
    proc_pending: bool,
    /// This pass runs `recGblFwdLink`: C `process` when it schedules no
    /// output (swaitRecord.c:477-480), `execOutput` when OUTV is not PV OK
    /// (`:791-795`); otherwise OUT's put callback runs it (`:378-386`).
    fwd_link: bool,
    // A refused CALC put's report, made by `special()` under the put's hold
    // and printed by the put's owner after it.
    put_report: Option<PutReport>,
    cached_should_output: bool,
    // ODLY delay state (C `cbStruct.outputWait`, an internal flag — swait has
    // no DLYA database field, unlike scalcout). `output_wait` marks that the
    // current `process()` call is the watchdog continuation re-entry; on it the
    // captured `pending_output` decision is restored so the framework writes
    // OUT + posts OEVT exactly once after the delay.
    output_wait: bool,
    pending_output: bool,
    /// The CLCV/CALC/CLCV posts owed by a CALC `special()`.
    calc_post: bool,
    /// This cycle's `monitor()` posts of A..L and LA..LL, a bit per input.
    input_changes: u16,
}

impl Default for SwaitRecord {
    fn default() -> Self {
        Self {
            val: 0.0,
            calc: String::new(),
            rpcl: Rpcl::empty(),
            clcv: 0,
            init: 0,
            calc_alarm: false,
            calc_succeeded: false,
            oopt: 0,
            dopt: 0,
            doln: PvString::new(),
            dold: 0.0,
            oval: 0.0,
            oevt: 0,
            odly: 0.0,
            outn: PvString::new(),
            prec: 0,
            mdel: 0.0,
            adel: 0.0,
            mlst: 0.0,
            alst: 0.0,
            inp_names: Default::default(),
            inp_passive: [0; 12],
            num_vals: [0.0; 12],
            prev_vals: [0.0; 12],
            // C `init_record` sets NO_PV for every blank name (swaitRecord.c:349),
            // and every name starts blank.
            pv_status: [SWAIT_NO_PV; 14],
            links: std::array::from_fn(|_| RecDynLink::default()),
            inbox: Arc::default(),
            prio: CallbackPriority::Low,
            status_posts: 0,
            name_posts: 0,
            fetch_failed: false,
            siml: PvString::new(),
            simm: 0,
            siol: PvString::new(),
            sval: 0.0,
            sims: 0,
            simulation_active: false,
            async_ctx: None,
            io_intr: false,
            proc_pending: false,
            fwd_link: true,
            put_report: None,
            cached_should_output: true,
            output_wait: false,
            pending_output: false,
            calc_post: false,
            input_changes: 0,
        }
    }
}

// Channel letters A-L in order
const CHAN: [char; 12] = ['A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I', 'J', 'K', 'L'];

impl SwaitRecord {
    /// C `swaitRecord.c:304,561` — swait compiles its CALC with the **numeric**
    /// `postfix()`, not `sCalcPostfix()`. Its grammar is therefore epics-base
    /// calc's: no SVAL, no string literals, no sCalc string functions.
    fn recompile(&mut self) {
        // Through the compile owner, so a bad CALC gets C's errlog line and
        // leaves the empty program behind — `.ok()` discarded both, and the
        // record then had nothing to run and nothing to alarm about.
        //
        // C `swaitRecord.c:304`/`:561` — `pwait->clcv = postfix(pwait->calc,
        // pwait->rpcl, &error_number)`. swait keeps the status in CLCV and
        // returns 0 from `special()`, so the put SUCCEEDS and the client reads
        // the verdict back from CLCV (the calcout/scalcout/acalcout disposition,
        // not calcRecord's S_db_badField rejection).
        let compiled = calc_compile::postfix_into(&self.calc, &mut self.rpcl);
        self.clcv = compiled.status;
    }

    /// C `recGblRecordError(S_db_badField, pwait, pmessage)` on a CALC
    /// `postfix()` refused (swaitRecord.c:305-307, :562-564); swait writes no
    /// `calcErrorStr` line. Returned, not printed: `special()` runs under the
    /// put's hold.
    fn bad_calc_report(&self, pmessage: &'static str) -> Option<PutReport> {
        (self.clcv != 0).then_some(PutReport::RecordError {
            status: "Illegal field value",
            message: pmessage,
            then: None,
        })
    }

    /// Build the calc inputs. `prev_val` is the cell C passes as `presult`, which
    /// the `VAL` token (`FETCH_VAL`) pushes — for swait that is `&pwait->val`
    /// (C `swaitRecord.c:409`), i.e. the *previous* VAL.
    ///
    /// swait supplies TWELVE args, not the engine's 21: `swaitRecord.dbd:250-331`
    /// declares A..L and nothing else numbered, so `&pwait->a` is twelve doubles
    /// long. Handing the count to the engine is what makes M..U not exist here —
    /// see [`NumericInputs::with_counts`] and CBUG-G3 for what C does instead.
    fn build_inputs(&self, prev_val: f64) -> NumericInputs {
        let mut inputs = NumericInputs::with_counts(CHAN.len());
        inputs.vars[..CHAN.len()].copy_from_slice(&self.num_vals);
        inputs.prev_val = prev_val;
        inputs
    }

    /// Land the calc pass's variable stores back in A..L — the inverse of
    /// [`Self::build_inputs`], and the record's ONLY write-back of an engine var
    /// set.
    ///
    /// C `swaitRecord.c:409` is `calcPerform(&pwait->a, &pwait->val, pwait->rpcl)`
    /// — a pointer INTO the record, so the store opcode
    /// (`calcPerform.c:101-123`) IS the field write and `CALC="A:=A+1;A"`
    /// increments the record's A. The engine here evaluates an owned copy, so
    /// without this the store was dropped on the floor.
    ///
    /// Only the twelve args swait supplies come back, because only those twelve
    /// can have been written: the engine takes the count from
    /// [`Self::build_inputs`] and a store past L never lands.
    fn apply_stores(&mut self, inputs: &NumericInputs) {
        self.num_vals.copy_from_slice(&inputs.vars[..CHAN.len()]);
    }

    /// C `swaitRecord.c:425-450` — the OOPT switch, whose "old value" operand
    /// is `pwait->oval` (the previous cycle's VAL), passed in as `old` because
    /// C reads it BEFORE `:471` overwrites it with the new VAL.
    ///
    /// "On Change" is a MDEL-deadband test, not an inequality: C
    /// `swaitRecord.c:432` is `if (fabs(pwait->oval - pwait->val) > pwait->mdel)`,
    /// the same rule as sCalcout (`sCalcoutRecord.c:379`) and aCalcout
    /// (`aCalcoutRecord.c:318`). With the default MDEL=0 the two rules agree; a
    /// configured MDEL makes a sub-deadband change fire the OUT link on the port
    /// and not on C.
    ///
    /// `calcout` is NOT in that group and must not be copied from here: base
    /// writes it as `!(fabs(pval-val) <= mdel)` (`calcoutRecord.c:257`), which
    /// takes the opposite branch on NaN. These four records have four separate
    /// upstreams and only three of them agree.
    fn eval_should_output(&self, old: f64) -> bool {
        match self.oopt {
            0 => true,
            1 => (old - self.val).abs() > self.mdel,
            2 => self.val == 0.0,
            3 => self.val != 0.0,
            4 => old != 0.0 && self.val == 0.0,
            5 => old == 0.0 && self.val != 0.0,
            _ => false,
        }
    }

    fn inp_name_index(name: &str) -> Option<usize> {
        // INxN: INAN, INBN, INCN, INDN, INEN, INFN, INGN, INHN, ININ, INJN, INKN, INLN
        let bytes = name.as_bytes();
        if bytes.len() == 4 && bytes[0] == b'I' && bytes[1] == b'N' && bytes[3] == b'N' {
            CHAN.iter().position(|&c| c == bytes[2] as char)
        } else {
            None
        }
    }

    fn inp_passive_index(name: &str) -> Option<usize> {
        // INxP: INAP, INBP, INCP, ...
        let bytes = name.as_bytes();
        if bytes.len() == 4 && bytes[0] == b'I' && bytes[1] == b'N' && bytes[3] == b'P' {
            CHAN.iter().position(|&c| c == bytes[2] as char)
        } else {
            None
        }
    }

    fn num_val_index(name: &str) -> Option<usize> {
        match name.as_bytes() {
            [c @ b'A'..=b'L'] => Some(usize::from(c - b'A')),
            _ => None,
        }
    }

    /// LA..LL — the previous value of input A..L.
    fn prev_val_index(name: &str) -> Option<usize> {
        let bytes = name.as_bytes();
        if bytes.len() == 2 && bytes[0] == b'L' {
            CHAN.iter().position(|&c| c == bytes[1] as char)
        } else {
            None
        }
    }

    /// `INAV`..`INLV` / `DOLV` / `OUTV` → the index of the link they describe.
    fn pv_status_index(name: &str) -> Option<usize> {
        SWAIT_PV_STATUS_FIELDS.iter().position(|&f| f == name)
    }

    /// The `recDynLink` a name field names: INAN..INLN, DOLN and OUTN.
    fn dyn_link_index(name: &str) -> Option<usize> {
        match name {
            "DOLN" => Some(DOL_INDEX),
            "OUTN" => Some(OUT_INDEX),
            _ => Self::inp_name_index(name),
        }
    }

    fn link_name(&mut self, li: usize) -> &mut PvString {
        match li {
            DOL_INDEX => &mut self.doln,
            OUT_INDEX => &mut self.outn,
            _ => &mut self.inp_names[li],
        }
    }

    /// The closure link `li`'s events are handed to: queued, then applied
    /// under the record's lock from a callback worker, so the `recDynLink`
    /// threads never wait on that lock.
    fn deliver(&self, li: usize) -> Option<Deliver> {
        let (name, h) = self.async_ctx.clone()?;
        let inbox = self.inbox.clone();
        let prio = self.prio;
        Some(Arc::new(move |generation, event| {
            inbox.0.lock().unwrap().push_back((li, generation, event));
            let (h, name) = (h.clone(), name.clone());
            drop(spawn_blocking_background(prio, move |_| {
                h.with_held(&name, |held| {
                    loop {
                        let next = held.with(|rec, common| {
                            rec.as_any_mut()
                                .and_then(|a| a.downcast_mut::<Self>())
                                .map_or(Drain::Done, |s| s.drain_inbox(common.scan))
                        });
                        match next {
                            Drain::Done => break,
                            Drain::Process => {
                                let _ = held.db_process(&name);
                            }
                            Drain::FwdLink => held.fwd_link(),
                        }
                    }
                });
            }));
        }))
    }

    /// `recDynLinkAddInput` / `recDynLinkAddOutput` on link `li`'s name.
    fn add_link(&mut self, li: usize) {
        let Some(deliver) = self.deliver(li) else {
            return;
        };
        let Some((name, handle)) = &self.async_ctx else {
            return;
        };
        let pvname = match li {
            DOL_INDEX => &self.doln,
            OUT_INDEX => &self.outn,
            _ => &self.inp_names[li],
        };
        let spec = LinkSpec {
            io: if li < OUT_INDEX {
                Io::Input
            } else {
                Io::Output
            },
            ca_type: CaType::Double,
            scalar: true,
            search_callback: Some(Cb::Search),
            // C hands DOL `inputChanged` too, whose `&pwait->inap + 12` reads
            // the first bytes of CALC and whose `&pwait->a + 12` writes LA, so
            // a DOL change processes an I/O Intr record. Not copied.
            monitor_callback: (li < DOL_INDEX).then_some(Cb::Input),
        };
        let host = LinkHost {
            handle,
            record: name.as_bytes(),
        };
        self.links[li].add(host, &pvname.as_str_lossy(), spec, deliver);
    }

    /// C `special(after)` on a link name (swaitRecord.c:507-552): a blank name
    /// is reset to empty, a name searched for afresh as `PV_NC`, and an empty
    /// one leaves the link `NO_PV`.
    fn name_changed(&mut self, li: usize) {
        let old = self.pv_status[li];
        let name = self.link_name(li);
        let blank = is_blank(name.as_bytes());
        if blank {
            *name = PvString::new();
        }
        let named = !name.is_empty();
        if blank {
            self.name_posts |= 1 << li;
        }
        if named {
            self.pv_status[li] = SWAIT_PV_NC;
            self.add_link(li);
        } else if old != SWAIT_NO_PV {
            self.pv_status[li] = SWAIT_NO_PV;
            self.links[li].clear();
        }
        if self.pv_status[li] != old {
            self.status_posts |= 1 << li;
        }
    }

    /// Every queued link event, oldest first: `recDynLink` applies it, then
    /// the callback C would call runs, up to the first that leaves the
    /// record to be processed or forward-linked, the later events left
    /// queued.
    fn drain_inbox(&mut self, scan: ScanType) -> Drain {
        loop {
            let next = self.inbox.0.lock().unwrap().pop_front();
            let Some((li, generation, event)) = next else {
                return Drain::Done;
            };
            match self.links[li].apply(generation, event) {
                Some(Cb::Search) => self.pv_search_callback(li),
                Some(Cb::Input) if self.input_changed(li, scan) => return Drain::Process,
                Some(Cb::Notify) => {
                    self.fwd_link = true;
                    return Drain::FwdLink;
                }
                Some(Cb::Input) | None => {}
            }
        }
    }

    /// C `inputChanged` and the `ioIntProcess` it requests
    /// (swaitRecord.c:818-900), one lock hold here: on an I/O Intr record an
    /// INxP input takes the monitored value, and the record processes, or
    /// owes a process once the ODLY wait ends.
    fn input_changed(&mut self, li: usize, scan: ScanType) -> bool {
        if scan != ScanType::IoIntr || self.inp_passive[li] == 0 {
            return false;
        }
        let Some(&[v]) = self.links[li].get(1) else {
            return false;
        };
        self.num_vals[li] = v;
        if self.output_wait {
            self.proc_pending = true;
            return false;
        }
        true
    }

    /// C `pvSearchCallback` (swaitRecord.c:976-1000).
    fn pv_search_callback(&mut self, li: usize) {
        let status = if self.links[li].connection_status() == 0 {
            SWAIT_PV_OK
        } else {
            SWAIT_PV_NC
        };
        if self.pv_status[li] != status {
            self.pv_status[li] = status;
            self.status_posts |= 1 << li;
        }
    }

    /// C `execOutput` (swaitRecord.c:752-790): with OUTV PV_OK, DOLD read from
    /// DOL when DOPT says so and DOLV is PV_OK, then a put with completion,
    /// or a plain put when one is still in progress, and the forward link
    /// left to the completion; otherwise the cycle's forward link. OEVT goes
    /// out from [`Record::output_event`].
    fn exec_output(&mut self) {
        // C's `recGblFwdLink` here (`:794`) runs inside `schedOutput` (`:427`)
        // ahead of `monitor()` (`:475`), so the FLNK target reads this VAL with
        // the last cycle's SEVR, STAT and TIME. The cycle's tail runs it after
        // the posts, as the put callback's comes after them. Not copied.
        self.fwd_link = self.pv_status[OUT_INDEX] != SWAIT_PV_OK;
        if !self.fwd_link {
            let value = if self.dopt != 0 {
                if self.pv_status[DOL_INDEX] == SWAIT_PV_OK
                    && let Some(&[v]) = self.links[DOL_INDEX].get(1)
                {
                    self.dold = v;
                }
                self.dold
            } else {
                self.val
            };
            if self.links[OUT_INDEX].put_callback(value, Cb::Notify) == NOTIFY_IN_PROGRESS {
                self.links[OUT_INDEX].put(value);
            }
        }
        // C `:801-805`: an input changed during the wait processes it again.
        if self.io_intr
            && self.proc_pending
            && let Some((name, h)) = &self.async_ctx
        {
            h.scan_once(&name.to_str_lossy());
        }
    }

    /// C `fetch_values` (swaitRecord.c:686-705): a `PV_NC` input, or a
    /// `PV_OK` one whose `recDynLinkGet` fails, fails the fetch at once; a
    /// `NO_PV` input keeps its value, as an I/O Intr record's INxP input
    /// keeps the one its monitor brought.
    fn fetch_values(&mut self) -> bool {
        for i in 0..DOL_INDEX {
            match self.pv_status[i] {
                SWAIT_PV_NC => return false,
                SWAIT_PV_OK if self.io_intr && self.inp_passive[i] != 0 => {}
                SWAIT_PV_OK => match self.links[i].get(1) {
                    Some(&[v]) => self.num_vals[i] = v,
                    Some(_) => {}
                    None => return false,
                },
                _ => {}
            }
        }
        true
    }
}

/// Choice labels for the `swait` output-execute-option menu, in index
/// order. C `menu(swaitOOPT)` (`swaitRecord.dbd`): the six `longoutOOPT`
/// choices plus a trailing "Never" (index 6) that suppresses output.
const SWAIT_OOPT_CHOICES: &[&str] = &[
    "Every Time",
    "On Change",
    "When Zero",
    "When Non-zero",
    "Transition To Zero",
    "Transition To Non-zero",
    "Never",
];

/// Choice labels for the `swait` output-data-option menu, in index order.
/// C `menu(swaitDOPT)` (`swaitRecord.dbd`): 0="Use VAL" (the calculated
/// result), 1="Use DOL" (the value fetched through the `DOL` link).
const SWAIT_DOPT_CHOICES: &[&str] = &["Use VAL", "Use DOL"];

/// C `swaitRecord.c` `get_precision`: `else if (paddr->pfield == (void *)&pwait->odly)
/// *precision = 3;` — the one field swait answers with a literal instead of PREC.
const SWAIT_ODLY_PRECISION: i16 = 3;

/// The link-name fields, in `pv_status` order.
const SWAIT_LINK_NAME_FIELDS: [&str; 14] = [
    "INAN", "INBN", "INCN", "INDN", "INEN", "INFN", "INGN", "INHN", "ININ", "INJN", "INKN", "INLN",
    "DOLN", "OUTN",
];

/// VAL, then each input beside its LA..LL, in `monitor()`'s post order.
const SWAIT_MONITORED: [&str; 25] = [
    "VAL", "A", "LA", "B", "LB", "C", "LC", "D", "LD", "E", "LE", "F", "LF", "G", "LG", "H", "LH",
    "I", "LI", "J", "LJ", "K", "LK", "L", "LL",
];

/// What a process cycle may post: `monitor()`'s fields and the DOLD
/// `execOutput` posts when its read moved it (swaitRecord.c:769-770).
const SWAIT_PROCESS_POSTED: [&str; 26] = {
    let mut t = ["DOLD"; 26];
    let mut i = 0;
    while i < 25 {
        t[i] = SWAIT_MONITORED[i];
        i += 1;
    }
    t
};

/// `special()`'s CALC posts, the input pairs `monitor()` marks, then each
/// link's status and name, all of them a literal `DBE_VALUE` in C.
const SWAIT_POSTS_NAMED: [(&str, crate::server::record::CyclePostMask); 55] = {
    use crate::server::record::CyclePostMask::{MonitorValue, Value};
    let mut t = [("", MonitorValue); 55];
    t[0] = ("CLCV", Value);
    t[1] = ("CALC", Value);
    t[2] = ("CLCV", Value);
    let mut i = 0;
    while i < 24 {
        t[3 + i] = (SWAIT_MONITORED[1 + i], MonitorValue);
        i += 1;
    }
    let mut i = 0;
    while i < 14 {
        t[27 + i] = (SWAIT_PV_STATUS_FIELDS[i], Value);
        t[41 + i] = (SWAIT_LINK_NAME_FIELDS[i], Value);
        i += 1;
    }
    t
};

impl Record for SwaitRecord {
    crate::slot_map!(crate::server::record::dbd_generated::slots::swait;
        fields:
        OVAL => oval,
        INIT => init;
        custom: INAN, INBN, INCN, INDN, INEN, INFN, INGN, INHN, ININ, INJN, INKN, INLN, DOLN,
            OUTN, INAV, INBV, INCV, INDV, INEV, INFV, INGV, INHV, INIV, INJV, INKV, INLV, DOLV, OUTV,
            A, B, C, D, E, F, G, H, I, J, K, L, LA, LB, LC, LD, LE, LF, LG, LH, LI, LJ, LK, LL,
            INAP, INBP, INCP, INDP, INEP, INFP, INGP, INHP, INIP, INJP, INKP, INLP, CALC, CLCV,
            VAL, PREC, OOPT, ODLY, DOPT, DOLD, OEVT, ADEL, MDEL, ALST, MLST, SIOL, SVAL, SIML,
            SIMM, SIMS;
        spill: VERS, HOPR, LOPR, CBST);

    /// `swaitRecord.c:416` reads SIOL at `DBR_DOUBLE` and `:402` SIML at
    /// `DBR_ENUM`, so a source that request cannot convert is a failed read.
    fn input_link_request(&self, link_field: &str) -> crate::server::record::InputLinkRequest {
        crate::server::record::InputLinkRequest::As(match link_field {
            "SIOL" => crate::server::record::LinkReadAs::Double,
            "SIML" => crate::server::record::LinkReadAs::Enum,
            _ => crate::server::record::LinkReadAs::Native,
        })
    }

    fn acts_before_process(&self) -> bool {
        false
    }

    fn cycle_hooks(&self) -> crate::server::record::CycleHooks {
        crate::server::record::CycleHooks::WRITE_POSTED
            .union(crate::server::record::CycleHooks::PUT_REPORT)
            .union(crate::server::record::CycleHooks::OUTPUT_HOOKS)
            .union(crate::server::record::CycleHooks::CONTEXT)
    }

    fn set_process_context(&mut self, ctx: &crate::server::record::ProcessContext) {
        self.io_intr = ctx.scan == ScanType::IoIntr;
    }

    fn should_fire_forward_link(&self) -> bool {
        self.fwd_link
    }

    fn owns_io_intr_list(&self) -> bool {
        true
    }

    fn take_put_report(&mut self) -> Option<PutReport> {
        self.put_report.take()
    }

    /// The same rset shape as `bo`'s `HIGH` and `busy`'s `HIGH`: one named
    /// field answered with a constant. `ODLY` is `DBF_FLOAT`, so it clears
    /// `dbAccess.c:388-389`'s float/double gate and the 3 reaches the wire.
    fn field_metadata_override(&self, field: &str) -> Option<FieldMetadataOverride> {
        field
            .eq_ignore_ascii_case("ODLY")
            .then(|| FieldMetadataOverride {
                precision: Some(SWAIT_ODLY_PRECISION),
                ..Default::default()
            })
    }

    fn has_array_monitor_post(&self) -> bool {
        false
    }

    fn record_type(&self) -> &'static str {
        "swait"
    }

    /// `if (pwait->siml.type == CONSTANT)` and the same test on SIOL
    /// (`swaitRecord.c:663-670`).
    fn sim_seed_guard(&self) -> crate::server::record::SeedGuard {
        crate::server::record::SeedGuard::TypeConstant
    }

    /// Record-specific `DBF_MENU` fields, served as `DBR_ENUM` with the
    /// menu's choice labels in `.dbd` index order (`swaitRecord.dbd`):
    /// `OOPT` is `menu(swaitOOPT)`, `DOPT` is `menu(swaitDOPT)`,
    /// `INAV`..`INLV`/`DOLV`/`OUTV` are `menu(swaitINAV)`, and `SIMM` is
    /// `menu(menuYesNo)` (`swaitRecord.dbd:510-513`) — the two-choice menu, not
    /// the NO/YES/RAW `menuSimm` of the base analog records: swait has no raw
    /// value to simulate. (`SIMS` is the shared `menuAlarmSevr`.)
    fn menu_field_choices(&self, field: &str) -> Option<&'static [&'static str]> {
        match field {
            "OOPT" => Some(SWAIT_OOPT_CHOICES),
            "DOPT" => Some(SWAIT_DOPT_CHOICES),
            "SIMM" => Some(MENU_YES_NO),
            _ if Self::pv_status_index(field).is_some() => Some(SWAIT_PV_STATUS_CHOICES),
            _ => None,
        }
    }

    fn uses_monitor_deadband(&self) -> bool {
        true
    }

    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 1 {
            // C `swaitRecord.c:302`, after pass 0 returned at :280.
            self.recompile();
            // Not under a put's hold, so it prints at once.
            if let Some(report) = self.bad_calc_report("swait:init_record: Illegal CALC field") {
                let name = self
                    .async_ctx
                    .as_ref()
                    .map_or("Unknown".into(), |(n, _)| n.to_str_lossy());
                report.emit(&name, "");
            }
            // C `swaitRecord.c:330`: `pwait->init = TRUE;` — and never cleared.
            self.init = 1;
        }
        Ok(())
    }

    /// A `.db` CALC is stored as text, as `dbPutString` stores it; C compiles
    /// it in `init_record` pass 1, and only a put's `special` compiles it
    /// earlier.
    fn load_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match (name, value) {
            ("CALC", EpicsValue::String(s)) => self.calc = s.as_str_lossy().into_owned(),
            (name, value) => return self.put_field(name, value),
        }
        Ok(())
    }

    fn set_async_context(&mut self, name: crate::types::RecordName, db: AsyncDbHandle) {
        self.async_ctx = Some((name, db));
    }

    /// C `init_record` pass 1's search of every PV name (swaitRecord.c:334-371),
    /// here where PRIO is known: C sets `ioProcCb`'s band in the same pass.
    /// `iocInit` calls this again after the loader did; C adds each link
    /// once, so one already added keeps its channel.
    fn init_links(&mut self, common: &crate::server::record::CommonFields) {
        self.prio = common.callback_priority();
        for li in 0..self.links.len() {
            if self.links[li].is_added() {
                continue;
            }
            let name = self.link_name(li);
            if is_blank(name.as_bytes()) {
                *name = PvString::new();
            }
            if name.is_empty() {
                self.pv_status[li] = SWAIT_NO_PV;
            } else {
                self.pv_status[li] = SWAIT_PV_NC;
                self.add_link(li);
            }
        }
    }

    /// A put to a PV-name field re-runs the search — see `name_changed`.
    fn special(
        &mut self,
        field: &str,
        after: bool,
        _common: &crate::server::record::CommonFields,
    ) -> CaResult<()> {
        if !after {
            return Ok(());
        }
        if let Some(li) = Self::dyn_link_index(field) {
            self.name_changed(li);
        }
        self.calc_post = field == "CALC";
        if self.calc_post {
            self.put_report = self.bad_calc_report("swaitRecord:special: Illegal CALC field");
        }
        Ok(())
    }

    /// C `special(SPC_CALC)` posts CLCV, CALC, CLCV, each a literal
    /// `DBE_VALUE` (swaitRecord.c:566-568), and does not process; `monitor()`
    /// posts each moved input then its LA..LL with `monitor_mask | DBE_VALUE`
    /// (`:647-654`).
    fn take_cycle_posted_fields(&mut self) -> crate::server::record::CycleMarks {
        use crate::server::record::CycleMarks;
        let calc = if std::mem::take(&mut self.calc_post) {
            0b111
        } else {
            0
        };
        let mut inputs = 0u64;
        for i in 0..12 {
            if self.input_changes & (1 << i) != 0 {
                inputs |= 0b11 << (2 * i);
            }
        }
        self.input_changes = 0;
        let links = u128::from(std::mem::take(&mut self.status_posts)) << 27
            | u128::from(std::mem::take(&mut self.name_posts)) << 41;
        static SWAIT_POSTS: [(
            crate::server::record::FieldIx,
            crate::server::record::CyclePostMask,
        ); 55] = crate::server::record::declared_post_table(
            crate::server::record::dbd_generated::SWAIT_FIELDS,
            SWAIT_POSTS_NAMED,
        );
        CycleMarks::of(&SWAIT_POSTS, u128::from(calc | inputs << 3) | links)
    }

    /// `monitor()` posts VAL and the A..L / LA..LL pairs alone, and
    /// `execOutput` DOLD.
    fn process_posted_fields(&self) -> Option<crate::server::record::DeclaredFields> {
        Some(
            &const {
                crate::server::record::declared_set(
                    crate::server::record::dbd_generated::SWAIT_FIELDS,
                    SWAIT_PROCESS_POSTED,
                )
            },
        )
    }

    fn fields_posted_only_when_marked(&self) -> &'static [&'static str] {
        &SWAIT_MONITORED[1..]
    }

    /// The deadband classes, with the walk told to take this cycle's input
    /// marks only when there are any.
    fn monitor_value_classes(
        &mut self,
        gate: crate::server::record::MonitorValueGate,
        last_posted: &mut [f64; 2],
    ) -> crate::server::record::MonitorValueClasses {
        let mut classes = crate::server::record::monitor_deadband_classes(
            self,
            gate.array_monitor_post,
            last_posted,
        );
        classes.marked = self.input_changes != 0;
        classes
    }

    fn check_alarms(&mut self, common: &mut crate::server::record::CommonFields) {
        use crate::server::recgbl::{self, alarm_status};
        use crate::server::record::AlarmSeverity;

        // C `swaitRecord.c:409-410` — a failed `calcPerform` is
        // `recGblSetSevr(pwait, CALC_ALARM, INVALID_ALARM)`. Consuming the flag
        // here is what keeps it a per-cycle fact: a cycle that ran no calc (the
        // fetch gate failed, or the record is simulated) finds it already
        // cleared and raises nothing, exactly as C's `nsev`/`nsta` — reset by
        // `recGblResetAlarms` every cycle — behave.
        if std::mem::take(&mut self.calc_alarm) {
            // C `swaitRecord.c:410` uses PLAIN `recGblSetSevr(pwait, CALC_ALARM,
            // INVALID_ALARM)` — NULL message (empty namsg); PVA falls back to
            // the "CALC" condition string. No fabricated literal.
            recgbl::rec_gbl_set_sevr(common, alarm_status::CALC_ALARM, AlarmSeverity::Invalid);
        }

        // C `swaitRecord.c:412-414`: the `else` arm of the fetch gate —
        // `recGblSetSevr(pwait, READ_ALARM, INVALID_ALARM)`. This is what makes
        // swait's gate visible to a client even though VAL simply freezes; the
        // calc family raises nothing at all on the same failure.
        if self.fetch_failed {
            recgbl::rec_gbl_set_sevr(common, alarm_status::READ_ALARM, AlarmSeverity::Invalid);
        }

        // C `swaitRecord.c:411` — the ONE swait-owned UDF write, and it only
        // ever clears. Applied here because `check_alarms` is the only hook
        // that owns `common`; the calc raised the flag, this consumes it. A
        // cycle whose calc failed, or that never ran one (fetch gate, or a
        // simulated cycle — where the framework's SIOL owner has already done
        // C `:419` itself), leaves UDF exactly as it was.
        if std::mem::take(&mut self.calc_succeeded) {
            common.udf = 0;
        }
    }

    /// C `swaitRecord.c` never assigns `udf = TRUE` and never re-derives UDF
    /// from VAL — the record has no `checkAlarms` at all. So the framework's
    /// per-cycle `udf = value_is_undefined()` must not run: it invented a UDF
    /// that C does not have (a `0/0` VAL is NaN, yet C's `calcPerform` returns
    /// 0 and `:411` clears UDF) and, conversely, cleared the UDF of a swait
    /// whose calc had never once succeeded, just because VAL happened not to be
    /// NaN. UDF is set only at init and cleared only by `check_alarms` (:411)
    /// and by the simulation SIOL read (:419).
    fn clears_udf(&self) -> bool {
        false
    }

    /// C swait has no UDF guard anywhere (`swaitRecord.c` names UDF_ALARM in no
    /// line), so an undefined swait reports NO alarm from UDF — only the
    /// CALC/READ/SIMM alarms above. See [`Record::raises_udf_alarm`].
    fn raises_udf_alarm(&self) -> bool {
        false
    }

    /// C `swaitRecord.c:402-422` — swait's simulation replaces `fetch_values()`
    /// and `calcPerform()`, and nothing else: the OOPT switch (`:425`),
    /// `execOutput`, the monitors and the forward link all still run. So it is
    /// the input-STAGE shape, not the whole-cycle `readValue` of ai/bi.
    /// `swaitRecord.c:407-422` has no `default:` arm — the simulation test is a
    /// plain `if (pwait->simm == menuYesNoNO) { … } else { /* SIMULATION MODE */
    /// … }`, so ANY non-NO SIMM (including the 2 that every other menuYesNo
    /// record rejects with SOFT_ALARM/INVALID) simulates from SIOL.
    fn rejects_illegal_sim_mode(&self) -> bool {
        false
    }

    fn simulation_substitutes_input_stage(&self) -> bool {
        true
    }

    fn set_simulation_active(&mut self, active: bool) {
        self.simulation_active = active;
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // ODLY continuation: this is the watchdog re-process scheduled by a
        // previous cycle (C `swaitRecord.c::process` `if (pact && outputWait)
        // execOutput`, line 394). Do NOT re-evaluate CALC / OOPT — C runs
        // `execOutput` directly. Restore the captured output decision so the
        // framework posts OEVT this cycle, then clear the wait flag. Mirrors
        // scalcout's `dlya == 1` branch (swait has no DLYA field — the wait
        // state is the internal `output_wait` flag, as C uses
        // `cbStruct.outputWait`).
        if self.output_wait {
            self.output_wait = false;
            self.exec_output();
            self.cached_should_output = self.pending_output;
            self.pending_output = false;
            return Ok(ProcessOutcome::complete());
        }

        // C `:404`, before the inputs are read.
        self.proc_pending = false;
        self.fwd_link = true;

        // C `swaitRecord.c:432-446` compares against `pwait->oval`, which still
        // holds the PREVIOUS cycle's VAL at this point; `:471` advances it after
        // the OOPT decision.
        let old_val = self.oval;

        // C `swaitRecord.c:408-414` runs the calc only `if (fetch_values(pwait)
        // ==0)`. A failed input freezes VAL and UDF and raises
        // READ_ALARM/INVALID instead (see `check_alarms`). The OOPT decision
        // below stays outside the gate, as in C — it runs against the frozen
        // VAL.
        // C `swaitRecord.c:415-422` — a SIMULATED cycle takes the `else` branch,
        // which runs neither `fetch_values()` nor `calcPerform()`: VAL already
        // holds SVAL and SIMM_ALARM is already raised (both by the framework's
        // simulation owner). No calc runs, so no CALC_ALARM — and none can leak
        // in from an earlier failed cycle, because `check_alarms` consumed it.
        self.fetch_failed = !self.simulation_active && !self.fetch_values();
        if !self.simulation_active && !self.fetch_failed {
            // C `swaitRecord.c:409-410` — inside that gate, `calcPerform` runs
            // unconditionally, and a -1 is CALC_ALARM/INVALID with VAL left
            // alone. An empty or uncompilable CALC is the empty program and
            // fails here every cycle.
            let mut inputs = self.build_inputs(self.val);
            let outcome = calc_eval(&self.rpcl, &mut inputs);
            // C writes the stores through `&pwait->a` as the expression runs, so
            // they stand whether or not a later operator failed the perform.
            self.apply_stores(&inputs);
            match outcome {
                Ok(v) => {
                    self.val = v;
                    // C `:411` — `} else pwait->udf = FALSE;`. calcPerform's
                    // status is the whole test: a NaN/Inf result is a SUCCESS
                    // to base's calcPerform (it has no isnan check, unlike
                    // sCalcPerform), so `0/0` clears UDF here just as `1+1`
                    // does. `check_alarms` applies it.
                    self.calc_succeeded = true;
                }
                Err(_) => self.calc_alarm = true,
            }
        }

        // Cache before framework calls should_output() via trait dispatch.
        self.cached_should_output = self.eval_should_output(old_val);
        // C `schedOutput` (swaitRecord.c:719-729) without ODLY: `execOutput`
        // at once, ahead of the OVAL update.
        let delayed = self.odly > 0.0;
        if self.cached_should_output && !delayed {
            self.exec_output();
        }
        // C `swaitRecord.c:471`: `pwait->oval = pwait->val;` — unconditional,
        // after the OOPT test. OVAL is the old-value tracker, NOT the output
        // value: `execOutput` composes the output from VAL/DOLD.
        self.oval = self.val;

        // C `swaitRecord.c::monitor` (647-654): each input that differs from
        // its LA..LL posts, advances `*pprev = *pnew`, and posts LA..LL.
        let mut changes = 0;
        for (i, (new, prev)) in self.num_vals.iter().zip(&mut self.prev_vals).enumerate() {
            if *new != *prev {
                *prev = *new;
                changes |= 1 << i;
            }
        }
        self.input_changes = changes;

        // ODLY (C `swaitRecord.c::schedOutput`, lines 719-729): when output
        // should fire and ODLY > 0, defer ONLY the OUT write + OEVT + forward
        // link by ODLY seconds via the watchdog, holding the record active
        // (C keeps PACT=1). The value side (VAL + changed inputs + alarm fields)
        // is NOT deferred: C `process` calls `monitor()` (line 475) on THIS
        // (delay-start) cycle, before returning async — only `execOutput`
        // (delay-end) is delayed, and it posts no monitors. The delaying cycle
        // captures the output decision and suppresses this cycle's OUT/OEVT
        // (`cached_should_output = false`), then re-processes after the delay;
        // the `output_wait` branch above emits the output exactly once.
        if self.cached_should_output && delayed {
            self.pending_output = self.cached_should_output;
            self.output_wait = true;
            self.fwd_link = false;
            self.cached_should_output = false;
            let delay = crate::runtime::time::duration_from_secs(self.odly as f64);
            // `CompleteDeferOutput`, NOT bare `AsyncPending`: swait posts the
            // value side at the START of the delay (C `monitor()` at line 475,
            // reached because `schedOutput` set `async=TRUE` but `process` falls
            // through to `monitor()` before the `if(!async)` forward-link tail).
            // The framework therefore runs its full monitor epilogue this cycle
            // (VAL with MDEL/ADEL deadband + alarm mask, changed inputs) and
            // defers only the OUT/OEVT/FLNK tail, holding PACT for the watchdog
            // window via the `ReprocessAfter` continuation that releases it —
            // matching the C comment (not a statement) at `swaitRecord.c:716`,
            // "THE RECORD REMAINS ACTIVE WHILE WAITING ON THE WATCHDOG". A bare
            // `AsyncPending` would have deferred
            // the value side to delay-end too (the calcout/scalcout/acalcout
            // shape, whose C `process` returns BEFORE `monitor()`); swait's C
            // does not, so VAL must post now.
            return Ok(ProcessOutcome {
                result: RecordProcessResult::CompleteDeferOutput,
                actions: vec![ProcessAction::ReprocessAfter(delay)],
                device_did_compute: false,
                status: Ok(()),
            });
        }

        Ok(ProcessOutcome::complete())
    }

    fn should_output(&self) -> bool {
        self.cached_should_output
    }

    /// C `execOutput` posts the refreshed DOLD with `DBE_VALUE` alone
    /// (swaitRecord.c:770), not the framework default `DBE_VALUE | DBE_LOG`;
    /// CLCV's post (`:309`, `:566`) carries a literal `DBE_VALUE` too.
    fn value_only_change_fields(&self) -> &'static [&'static str] {
        &["DOLD", "CLCV"]
    }

    /// `OEVT` ("Output Event"): post the numeric output event when output
    /// fires. C `swaitRecord.c` `execOutput` runs `if (pwait->oevt > 0)
    /// post_event((int)pwait->oevt);` (swaitRecord.c:797) right after the OUT
    /// write / forward link, on every cycle where output fires
    /// (`cached_should_output`). swait has no IVOA field, so — like its C —
    /// the post is never IVOA-suppressed. Stringified so the numeric event
    /// matches a `SCAN="Event"` record's `EVNT`.
    fn output_event(&self) -> Option<crate::types::PvString> {
        if self.cached_should_output && self.oevt > 0 {
            Some(self.oevt.to_string().into())
        } else {
            None
        }
    }

    fn val(&self) -> Option<EpicsValue> {
        Some(EpicsValue::Double(self.val))
    }

    fn link_text_ref(&self, link_field: &str) -> Option<&[u8]> {
        match link_field {
            "SIML" => Some(self.siml.as_bytes()),
            "SIOL" => Some(self.siol.as_bytes()),
            _ => None,
        }
    }

    // C `swaitRecord.dbd` declares SIML/SIMM/SIOL/SIMS and no SDLY, so the
    // simulation is always synchronous: the -1.0 `check_simulation_mode`
    // takes for an undeclared SDLY.
    fn simm_cells(&self) -> Option<crate::server::record::SimmCells> {
        Some(crate::server::record::SimmCells {
            simm: self.simm,
            sims: self.sims,
            sdly: -1.0,
        })
    }

    fn cycle_cell(&self, cell: CycleCell) -> CycleRead {
        match cell {
            CycleCell::Val => CycleRead::Value(CycleValue::Double(self.val)),
            CycleCell::Oval => CycleRead::Value(CycleValue::Double(self.oval)),
            CycleCell::Mdel => CycleRead::Value(CycleValue::Double(self.mdel)),
            CycleCell::Adel => CycleRead::Value(CycleValue::Double(self.adel)),
            CycleCell::Mlst => CycleRead::Value(CycleValue::Double(self.mlst)),
            CycleCell::Alst => CycleRead::Value(CycleValue::Double(self.alst)),
            _ => CycleRead::Absent,
        }
    }

    fn put_cycle_cell(&mut self, cell: CycleCell, value: CycleValue) -> CaResult<()> {
        match (cell, value) {
            (CycleCell::Alst, CycleValue::Double(v)) => self.alst = v,
            (CycleCell::Mlst, CycleValue::Double(v)) => self.mlst = v,
            (cell, value) => return self.put_field(cell.name(), value.into()),
        }
        Ok(())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(self.val)),
            "SIML" => Some(EpicsValue::String(self.siml.clone().into())),
            "SIMM" => Some(EpicsValue::Short(self.simm)),
            "SIOL" => Some(EpicsValue::String(self.siol.clone().into())),
            "SVAL" => Some(EpicsValue::Double(self.sval)),
            "SIMS" => Some(EpicsValue::Short(self.sims)),
            "CALC" => Some(EpicsValue::String(self.calc.clone().into())),
            "CLCV" => Some(EpicsValue::Long(self.clcv)),
            "INIT" => Some(EpicsValue::Short(self.init)),
            "OOPT" => Some(EpicsValue::Short(self.oopt)),
            "DOPT" => Some(EpicsValue::Short(self.dopt)),
            "DOLN" => Some(EpicsValue::String(self.doln.clone().into())),
            "DOLD" => Some(EpicsValue::Double(self.dold)),
            "OVAL" => Some(EpicsValue::Double(self.oval)),
            "OEVT" => Some(EpicsValue::UShort(self.oevt)),
            "ODLY" => Some(EpicsValue::Float(self.odly)),
            "OUTN" => Some(EpicsValue::String(self.outn.clone().into())),
            "PREC" => Some(EpicsValue::Short(self.prec)),
            "MDEL" => Some(EpicsValue::Double(self.mdel)),
            "ADEL" => Some(EpicsValue::Double(self.adel)),
            "MLST" => Some(EpicsValue::Double(self.mlst)),
            "ALST" => Some(EpicsValue::Double(self.alst)),
            _ => {
                if let Some(idx) = Self::num_val_index(name) {
                    return Some(EpicsValue::Double(self.num_vals[idx]));
                }
                if let Some(idx) = Self::prev_val_index(name) {
                    return Some(EpicsValue::Double(self.prev_vals[idx]));
                }
                if let Some(idx) = Self::pv_status_index(name) {
                    return Some(EpicsValue::Enum(self.pv_status[idx] as u16));
                }
                if let Some(idx) = Self::inp_name_index(name) {
                    return Some(EpicsValue::String(self.inp_names[idx].clone().into()));
                }
                if let Some(idx) = Self::inp_passive_index(name) {
                    return Some(EpicsValue::Short(self.inp_passive[idx]));
                }
                None
            }
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => {
                self.val = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("VAL".into()))?;
            }
            "CALC" => {
                if let EpicsValue::String(s) = value {
                    self.calc = s.as_str_lossy().into_owned();
                    self.recompile();
                } else {
                    return Err(CaError::TypeMismatch("CALC".into()));
                }
            }
            "OVAL" => {
                self.oval = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("OVAL".into()))?;
            }
            "CLCV" => {
                self.clcv = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("CLCV".into()))?
                    as i32;
            }
            "OOPT" => {
                if let EpicsValue::Short(v) = value {
                    self.oopt = v;
                }
            }
            "DOPT" => {
                if let EpicsValue::Short(v) = value {
                    self.dopt = v;
                }
            }
            "DOLN" => {
                if let EpicsValue::String(s) = value {
                    self.doln = s;
                } else {
                    return Err(CaError::TypeMismatch("DOLN".into()));
                }
            }
            "DOLD" => {
                self.dold = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("DOLD".into()))?;
            }
            "OEVT" => {
                self.oevt = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("OEVT".into()))?
                    as u16;
            }
            "ODLY" => {
                self.odly = c_cast::f64_to_f32(
                    value
                        .to_f64()
                        .ok_or_else(|| CaError::TypeMismatch("ODLY".into()))?,
                );
            }
            "OUTN" => {
                if let EpicsValue::String(s) = value {
                    self.outn = s;
                } else {
                    return Err(CaError::TypeMismatch("OUTN".into()));
                }
            }
            "SIML" => match value {
                EpicsValue::String(s) => self.siml = s,
                _ => return Err(CaError::TypeMismatch("SIML".into())),
            },
            "SIOL" => match value {
                EpicsValue::String(s) => self.siol = s,
                _ => return Err(CaError::TypeMismatch("SIOL".into())),
            },
            "SIMM" => {
                self.simm = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("SIMM".into()))?
                    as i16;
            }
            "SIMS" => {
                self.sims = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("SIMS".into()))?
                    as i16;
            }
            "SVAL" => {
                self.sval = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("SVAL".into()))?;
            }
            "MDEL" => {
                self.mdel = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("MDEL".into()))?;
            }
            "ADEL" => {
                self.adel = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("ADEL".into()))?;
            }
            "MLST" => {
                self.mlst = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("MLST".into()))?;
            }
            "ALST" => {
                self.alst = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("ALST".into()))?;
            }
            "PREC" => {
                if let EpicsValue::Short(v) = value {
                    self.prec = v;
                }
            }
            _ => {
                if let Some(idx) = Self::num_val_index(name) {
                    self.num_vals[idx] = value
                        .to_f64()
                        .ok_or_else(|| CaError::TypeMismatch(name.into()))?;
                } else if let Some(idx) = Self::prev_val_index(name) {
                    self.prev_vals[idx] = value
                        .to_f64()
                        .ok_or_else(|| CaError::TypeMismatch(name.into()))?;
                } else if let Some(idx) = Self::pv_status_index(name) {
                    // SPC_NOMOD to clients.
                    self.pv_status[idx] = value
                        .to_f64()
                        .ok_or_else(|| CaError::TypeMismatch(name.into()))?
                        as i16;
                } else if let Some(idx) = Self::inp_name_index(name) {
                    if let EpicsValue::String(s) = value {
                        self.inp_names[idx] = s;
                    } else {
                        return Err(CaError::TypeMismatch(name.into()));
                    }
                } else if let Some(idx) = Self::inp_passive_index(name) {
                    if let EpicsValue::Short(v) = value {
                        self.inp_passive[idx] = v;
                    } else {
                        return Err(CaError::TypeMismatch(name.into()));
                    }
                } else {
                    return Err(CaError::FieldNotFound(name.to_string()));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod process_tests {
    use super::*;

    /// R9-74: OOPT="On Change" is `fabs(oval - val) > mdel`
    /// (C `swaitRecord.c:432`), not `val != oval`. A change that stays inside
    /// MDEL must not drive OUT; one that crosses it must.
    #[test]
    fn r9_74_swait_on_change_honours_mdel_deadband() {
        let mut rec = SwaitRecord::default();
        rec.put_field("CALC", EpicsValue::String("A".into()))
            .unwrap();
        rec.put_field("OOPT", EpicsValue::Short(1)).unwrap();
        rec.put_field("MDEL", EpicsValue::Double(2.0)).unwrap();

        rec.put_field("A", EpicsValue::Double(1.0)).unwrap();
        rec.process().unwrap();
        assert_eq!(rec.val, 1.0);
        assert!(
            !rec.should_output(),
            "|oval - val| = 1.0 is inside MDEL=2.0 — C does not schedule output"
        );

        rec.put_field("A", EpicsValue::Double(5.0)).unwrap();
        rec.process().unwrap();
        assert!(
            rec.should_output(),
            "|1.0 - 5.0| = 4.0 exceeds MDEL=2.0 — C schedules output"
        );
    }

    /// The CALC `VAL` token reads the *previous* VAL: C `swaitRecord.c:409`
    /// calls `sCalcPerform(&pwait->a, ..., &pwait->val, ...)`, so `FETCH_VAL`
    /// pushes `*presult` = the VAL from the last cycle. `CALC="VAL+A"` therefore
    /// accumulates instead of collapsing to `A` every cycle.
    #[test]
    fn r5_2_sibling_calc_val_token_reads_previous_val() {
        let mut rec = SwaitRecord::default();
        rec.put_field("CALC", EpicsValue::String("VAL+A".into()))
            .unwrap();
        rec.put_field("A", EpicsValue::Double(2.0)).unwrap();

        rec.process().unwrap();
        assert_eq!(rec.val, 2.0, "first cycle: 0 + A");
        rec.process().unwrap();
        assert_eq!(rec.val, 4.0, "second cycle: previous VAL (2) + A");
        rec.process().unwrap();
        assert_eq!(rec.val, 6.0, "third cycle: previous VAL (4) + A");
    }

    /// Boundary: a CALC with no `VAL` token is unaffected by the seeding — the
    /// result is still a pure function of the inputs.
    #[test]
    fn r5_2_sibling_calc_without_val_token_is_unchanged() {
        let mut rec = SwaitRecord::default();
        rec.put_field("CALC", EpicsValue::String("A+1".into()))
            .unwrap();
        rec.put_field("A", EpicsValue::Double(5.0)).unwrap();

        rec.process().unwrap();
        assert_eq!(rec.val, 6.0);
        rec.process().unwrap();
        assert_eq!(rec.val, 6.0, "no VAL token: no accumulation");
    }
}
