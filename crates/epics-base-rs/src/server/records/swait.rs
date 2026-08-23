use super::calc_compile;
use super::link_status::{LinkStatusGen, SWAIT_NO_PV, SWAIT_PV_STATUS_CHOICES, classify_swait_pv};
use crate::calc::NumericInputs;
use crate::calc::{CompiledExpr, ExprKind, eval as calc_eval};
use crate::error::{CaError, CaResult};
use crate::server::database::AsyncDbHandle;
use crate::server::record::{
    FieldMetadataOverride, InputFetchPolicy, MENU_YES_NO, ProcessAction, ProcessOutcome, Record,
    RecordProcessResult,
};
use crate::types::EpicsValue;

/// The PV-status fields, in C's `NUM_LINKS` order: the twelve inputs
/// (`INAV`..`INLV`), then `DOLV`, then `OUTV`. C keeps the statuses in one
/// contiguous array starting at `pwait->inav` and walks it with the link names
/// starting at `pwait->inan` (swaitRecord.c:334-338, 686-687), so the index of
/// a status field IS the index of its link.
const SWAIT_PV_STATUS_FIELDS: [&str; 14] = [
    "INAV", "INBV", "INCV", "INDV", "INEV", "INFV", "INGV", "INHV", "INIV", "INJV", "INKV", "INLV",
    "DOLV", "OUTV",
];

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
    compiled_calc: CompiledExpr,
    /// CLCV ("CALC Valid", `swaitRecord.dbd:433`, `DBF_LONG`) — `postfix()`'s
    /// return status, stored on every compile (`swaitRecord.c:304` at init,
    /// `:561` in `special(SPC_CALC)`) and posted `DBE_VALUE` (`:309`, `:566`).
    /// 0 when the CALC compiled, -1 when it did not; the same
    /// `calc_compile`-owned status calcout/scalcout/acalcout store.
    pub clcv: i32,
    /// INIT ("Initialized?", `swaitRecord.dbd:42`, `DBF_SHORT`, `SPC_NOMOD`).
    /// C `swaitRecord.c:330` and `:374` set `pwait->init = TRUE` in
    /// `init_record` and NOTHING ever clears it — unlike `ai`/`ao`'s INIT
    /// (`ConvertPhase`) this is not a conversion phase but a latch:
    /// "this record has been through init_record". The port served 0 for the
    /// life of the IOC.
    initialized: bool,
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
    // (swaitRecord.c:763-772) fetches DOLN's PV into DOLD at OUTPUT time and
    // writes DOLD to OUT; see [`Record::output_time_input_links`]. With DOLN
    // unset (C `dolv == NO_PV`) the get is skipped and DOLD keeps whatever was
    // written to it (an operator/client put, or the init value).
    pub doln: String,
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
    pub out: String,
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
    pub inp_names: [String; 12], // INAN..INLN
    pub inp_passive: [i16; 12],  // INAP..INLP
    // numeric input values A-L
    pub num_vals: [f64; 12],
    // LA..LL ("Last Val of Input x", C `swaitRecord.dbd:298-331`, DBF_DOUBLE,
    // no SPC_NOMOD so a client may write them). C `monitor()`
    // (swaitRecord.c:646-653) is the single writer during processing: for each
    // input it changed, it posts the input, advances `*pprev = *pnew`, and
    // posts the previous-value field too — both with `monitor_mask | DBE_VALUE`
    // (see `fields_posted_with_monitor_mask`). The record had no LA..LL at all,
    // so the change history C publishes was unreadable and a put was rejected
    // with `FieldNotFound`.
    pub prev_vals: [f64; 12],
    // INAV..INLV / DOLV / OUTV — the `menu(swaitINAV)` PV-connection status of
    // each link, indexed as `SWAIT_PV_STATUS_FIELDS` (C keeps the same array,
    // swaitRecord.c:334-338). Read-only to clients (`special(SPC_NOMOD)`,
    // swaitRecord.dbd:166-250); written only by `refresh_link_status`, the
    // single owner of the classification, through `put_field_internal`.
    pv_status: [i16; 14],
    // This cycle's `fetch_values()` outcome, pushed by the framework through
    // `set_fetch_gate_failed`. C `swaitRecord.c::process` (407-414):
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
    // INVALID severity. The OOPT switch that follows (:424) is outside the
    // gate and runs against the frozen VAL.
    fetch_gate_failed: bool,
    // Simulation mode — C `swaitRecord.dbd:497-517` (SIOL, SVAL, SIML, SIMM,
    // SIMS) and `swaitRecord.c:401-421`. The record had none of these fields, so
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
    pub siml: String,
    pub simm: i16,
    pub siol: String,
    pub sval: f64,
    pub sims: i16,
    // This cycle's simulation state, pushed by the framework through
    // `set_simulation_active` before `process()` — the twin of
    // `fetch_gate_failed`. C `swaitRecord.c:407/415`: a simulated cycle takes the
    // `else` branch, which runs NEITHER `fetch_values()` NOR `calcPerform()`.
    simulation_active: bool,
    // Async context + generation gate for `refresh_link_status`, the same
    // shape calcout/sseq use (see `link_status::LinkStatusGen`): a refresh
    // classifies a snapshot of the link names off-thread, and only the latest
    // one issued may publish.
    async_ctx: Option<(String, AsyncDbHandle)>,
    link_gen: LinkStatusGen,
    cached_should_output: bool,
    // ODLY delay state (C `cbStruct.outputWait`, an internal flag — swait has
    // no DLYA database field, unlike scalcout). `output_wait` marks that the
    // current `process()` call is the watchdog continuation re-entry; on it the
    // captured `pending_output` decision is restored so the framework writes
    // OUT + posts OEVT exactly once after the delay.
    output_wait: bool,
    pending_output: bool,
}

impl Default for SwaitRecord {
    fn default() -> Self {
        Self {
            val: 0.0,
            calc: String::new(),
            compiled_calc: CompiledExpr::empty(ExprKind::Numeric),
            clcv: 0,
            initialized: false,
            calc_alarm: false,
            calc_succeeded: false,
            oopt: 0,
            dopt: 0,
            doln: String::new(),
            dold: 0.0,
            oval: 0.0,
            oevt: 0,
            odly: 0.0,
            out: String::new(),
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
            fetch_gate_failed: false,
            siml: String::new(),
            simm: 0,
            siol: String::new(),
            sval: 0.0,
            sims: 0,
            simulation_active: false,
            async_ctx: None,
            link_gen: LinkStatusGen::default(),
            cached_should_output: true,
            output_wait: false,
            pending_output: false,
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
        let compiled = calc_compile::postfix("swait", "CALC", &self.calc);
        self.clcv = compiled.status;
        self.compiled_calc = compiled.program;
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
        // Single letter A-L
        if name.len() == 1 {
            CHAN.iter().position(|&c| c.to_string() == name)
        } else {
            None
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

    /// True for the link-NAME fields whose put re-classifies the PV status —
    /// C `special()` (swaitRecord.c:507-553) re-runs the search for any name in
    /// `INAN`..`INLN`, `DOLN`, `OUTN`. `OUTN` is a common field, so its put is
    /// not visible in `special()`; it is caught by `check_alarms` instead, the
    /// same split calcout uses.
    fn is_pv_name_field(name: &str) -> bool {
        Self::inp_name_index(name).is_some() || name == "DOLN"
    }

    /// Re-classify every PV name into its `menu(swaitINAV)` status and post the
    /// result. The SINGLE writer of `pv_status`: C's statuses are set only by
    /// `init_record`, `special()` and `pvSearchCallback` — all of them "the
    /// name changed, re-run the search" — so the port funnels all three through
    /// here. No-op without an async context.
    fn refresh_link_status(&self) {
        let Some((name, handle)) = &self.async_ctx else {
            return;
        };
        let rec_name = name.clone();
        let handle = handle.clone();
        let mut links: Vec<String> = self.inp_names.to_vec();
        links.push(self.doln.clone());
        links.push(self.out.clone());
        let link_gen = self.link_gen.clone();
        // Stamp this refresh; a later one supersedes it (see `LinkStatusGen`).
        let token = link_gen.next();
        let sched = handle.clone();
        // Through the database's `iocInit` owner — see `schedule_record_init`.
        // The parking key; `rec_name` itself moves into the future below.
        let init_key = rec_name.clone();
        sched.schedule_record_init(&init_key, async move {
            let mut fields: Vec<(String, EpicsValue)> = Vec::with_capacity(links.len());
            for (i, link) in links.iter().enumerate() {
                let status = classify_swait_pv(&handle, link);
                fields.push((
                    SWAIT_PV_STATUS_FIELDS[i].to_string(),
                    EpicsValue::Enum(status as u16),
                ));
            }
            if link_gen.is_current(token) {
                let _ = handle.post_fields(&rec_name, fields);
            }
        });
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

impl Record for SwaitRecord {
    /// C `swaitRecord.c::init_record` (:266-) ends without touching
    /// MLST/ALST; both writes are in `monitor()` (:630,:639).
    fn seed_deadband_tracking(&mut self) {}

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

    fn record_type(&self) -> &'static str {
        "swait"
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
        if pass == 0 {
            self.recompile();
            // C `swaitRecord.c:330`: `pwait->init = TRUE;` — and never cleared.
            self.initialized = true;
        }
        Ok(())
    }

    fn set_async_context(&mut self, name: String, db: AsyncDbHandle) {
        self.async_ctx = Some((name, db));
        // C `init_record` (swaitRecord.c:334-373) searches every PV name and
        // seeds INAV..INLV/DOLV/OUTV. The INxN/DOLN names are swait fields
        // (applied before `add_record`), so classify them now; OUTN is a common
        // field not yet applied, and `init_links` re-runs the refresh once it
        // is. The generation gate lets that later, fuller refresh win.
        self.refresh_link_status();
    }

    fn init_links(&mut self, common: &crate::server::record::CommonFields) {
        self.out = common.out.clone();
        self.refresh_link_status();
    }

    /// A put to a PV-name field re-runs the search — C `special()`
    /// (swaitRecord.c:507-553) sets the status to `PV_NC` and re-issues
    /// `recDynLinkAddInput`/`AddOutput`, or to `NO_PV` when the name was
    /// cleared. `refresh_link_status` re-reads the name the put just stored.
    fn special(&mut self, field: &str, after: bool) -> CaResult<()> {
        if !after {
            return Ok(());
        }
        if Self::is_pv_name_field(field) {
            self.refresh_link_status();
        }
        Ok(())
    }

    fn check_alarms(&mut self, common: &mut crate::server::record::CommonFields) {
        use crate::server::recgbl::{self, alarm_status};
        use crate::server::record::AlarmSeverity;

        // OUTN lives in the common fields, so a runtime re-point is invisible to
        // `special()`. Catch it here — the same split calcout uses for its OUT.
        if self.out != common.out {
            self.out = common.out.clone();
            self.refresh_link_status();
        }

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
        if self.fetch_gate_failed {
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

    /// C `swaitRecord.c::fetch_values` (686-705) returns at the FIRST input that
    /// is `PV_NC` or whose `recDynLinkGet` fails, and `process` (408) gates
    /// `calcPerform` on the status.
    fn input_fetch_policy(&self) -> InputFetchPolicy {
        InputFetchPolicy::AbortOnFirstFailure
    }

    fn set_fetch_gate_failed(&mut self, failed: bool) {
        self.fetch_gate_failed = failed;
    }

    /// swait's input fetch is NOT `dbGetLink`. `fetch_values`
    /// (`swaitRecord.c:686-705`) reads INAA..INPL with `recDynLinkGet`, a
    /// CA-style get that has no `setLinkAlarm`; the failure is answered by
    /// `process` with `recGblSetSevr(pwait, READ_ALARM, INVALID_ALARM)`
    /// (`swaitRecord.c:412`). Its SIML/SIOL reads ARE `dbGetLink`
    /// (`:401`, `:415`) and keep the framework's uniform rule.
    fn multi_input_fetch_is_db_get_link(&self) -> bool {
        false
    }

    /// C `swaitRecord.c:401-421` — swait's simulation replaces `fetch_values()`
    /// and `calcPerform()`, and nothing else: the OOPT switch (`:424`),
    /// `execOutput`, the monitors and the forward link all still run. So it is
    /// the input-STAGE shape, not the whole-cycle `readValue` of ai/bi.
    /// `swaitRecord.c:407-421` has no `default:` arm — the simulation test is a
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

    /// C `swaitRecord.c:415` — the simulation branch never calls
    /// `fetch_values()`, so a simulated cycle reads no input link at all (A..L
    /// keep their previous values, and no input's connection state can gate the
    /// cycle). `Some(&[])` is "no active inputs this cycle", the same per-cycle
    /// restriction sel uses for `Specified`.
    fn select_input_links(
        &self,
        _selector: Option<u16>,
    ) -> Option<Vec<(&'static str, &'static str)>> {
        if self.simulation_active {
            Some(Vec::new())
        } else {
            None
        }
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // ODLY continuation: this is the watchdog re-process scheduled by a
        // previous cycle (C `swaitRecord.c::process` `if (pact && outputWait)
        // execOutput`, line 394). Do NOT re-evaluate CALC / OOPT — C runs
        // `execOutput` directly. Restore the captured output decision so the
        // framework writes the OUT link + posts OEVT this cycle, then clear the
        // wait flag. Mirrors scalcout's `dlya == 1` branch (swait has no DLYA
        // field — the wait state is the internal `output_wait` flag, as C uses
        // `cbStruct.outputWait`).
        if self.output_wait {
            self.output_wait = false;
            self.cached_should_output = self.pending_output;
            self.pending_output = false;
            return Ok(ProcessOutcome::complete());
        }

        // C `swaitRecord.c:432-446` compares against `pwait->oval`, which still
        // holds the PREVIOUS cycle's VAL at this point; `:471` advances it after
        // the OOPT decision.
        let old_val = self.oval;

        // C `swaitRecord.c:407-414` runs the calc only `if (fetch_values(pwait)
        // ==0)`; its `fetch_values` (686-705) returns at the first input that is
        // PV_NC or whose get fails. A failed input freezes VAL and UDF and
        // raises READ_ALARM/INVALID instead (see `check_alarms`). The OOPT
        // decision below stays outside the gate, as in C — it runs against the
        // frozen VAL.
        // C `swaitRecord.c:415-421` — a SIMULATED cycle takes the `else` branch,
        // which runs neither `fetch_values()` nor `calcPerform()`: VAL already
        // holds SVAL and SIMM_ALARM is already raised (both by the framework's
        // simulation owner). No calc runs, so no CALC_ALARM — and none can leak
        // in from an earlier failed cycle, because `check_alarms` consumed it.
        if !self.simulation_active && !self.fetch_gate_failed {
            // C `swaitRecord.c:409-410` — inside that gate, `calcPerform` runs
            // unconditionally, and a -1 is CALC_ALARM/INVALID with VAL left
            // alone. An empty or uncompilable CALC is the empty program and
            // fails here every cycle.
            let mut inputs = self.build_inputs(self.val);
            let outcome = calc_eval(&self.compiled_calc, &mut inputs);
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
        // C `swaitRecord.c:471`: `pwait->oval = pwait->val;` — unconditional,
        // after the OOPT test. OVAL is the old-value tracker, NOT the output
        // value: `execOutput` composes the output from VAL/DOLD at write time
        // (see `output_link_value`).
        self.oval = self.val;

        // C `swaitRecord.c::monitor` (646-653) advances each previous-input
        // field to the input it just posted (`*pprev = *pnew`) — so LA..LL is
        // "the value of input A..L as of the last cycle that posted it", which
        // for these fields is every cycle the input changed. Assigning
        // unconditionally is the same state: the C guard only skips the
        // assignment when the two are already equal.
        self.prev_vals = self.num_vals;

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
        if self.cached_should_output && self.odly > 0.0 {
            self.pending_output = self.cached_should_output;
            self.output_wait = true;
            self.cached_should_output = false;
            let delay = std::time::Duration::from_secs_f64(self.odly as f64);
            // `CompleteDeferOutput`, NOT bare `AsyncPending`: swait posts the
            // value side at the START of the delay (C `monitor()` at line 475,
            // reached because `schedOutput` set `async=TRUE` but `process` falls
            // through to `monitor()` before the `if(!async)` forward-link tail).
            // The framework therefore runs its full monitor epilogue this cycle
            // (VAL with MDEL/ADEL deadband + alarm mask, changed inputs) and
            // defers only the OUT/OEVT/FLNK tail, holding PACT for the watchdog
            // window via the `ReprocessAfter` continuation that releases it —
            // matching C `swaitRecord.c:716` "THE RECORD REMAINS ACTIVE WHILE
            // WAITING ON THE WATCHDOG". A bare `AsyncPending` would have deferred
            // the value side to delay-end too (the calcout/scalcout/acalcout
            // shape, whose C `process` returns BEFORE `monitor()`); swait's C
            // does not, so VAL must post now.
            return Ok(ProcessOutcome {
                result: RecordProcessResult::CompleteDeferOutput,
                actions: vec![ProcessAction::ReprocessAfter(delay)],
                device_did_compute: false,
            });
        }

        Ok(ProcessOutcome::complete())
    }

    fn should_output(&self) -> bool {
        self.cached_should_output
    }

    /// C `swaitRecord.c::execOutput` (761-774): the value written to OUT is
    /// composed at output time, not staged during `process()` —
    /// `outValue = pwait->dopt ? pwait->dold : pwait->val`. With DOPT="Use DOL"
    /// the DOLD it reads is the one the framework just refreshed from the DOL
    /// link (see [`Record::output_time_input_links`]).
    fn output_link_value(&self) -> Option<EpicsValue> {
        Some(EpicsValue::Double(if self.dopt == 1 {
            self.dold
        } else {
            self.val
        }))
    }

    /// DOL is read at OUTPUT time, and only under DOPT="Use DOL" — C
    /// `execOutput` (763-772) guards the `recDynLinkGet` with `if (pwait->dopt)`
    /// and, inside it, `if (!pwait->dolv)` (DOLV == PV_OK).
    ///
    /// The DOLV half of that guard needs no arm here: the framework's
    /// output-time fetch skips an empty name and writes the value field only
    /// when the read SUCCEEDS (`processing.rs`), so a DOL name that is unset or
    /// does not resolve leaves DOLD at its last value — exactly what C's
    /// `if (!pwait->dolv)` produces, and C still writes that stale DOLD to OUT.
    /// Gating on `pv_status` instead would ADD a divergence: that field is
    /// filled by an async classification task, so a cycle running before the
    /// task lands would skip a fetch C performs.
    fn output_time_input_links(&self) -> &'static [(&'static str, &'static str)] {
        if self.dopt == 1 {
            &[("DOLN", "DOLD")]
        } else {
            &[]
        }
    }

    /// C posts CLCV explicitly from `special()` (`db_post_events(pwait,
    /// &pwait->clcv, DBE_VALUE)`, swaitRecord.c:566) — CLCV is not `pp(TRUE)`,
    /// so nothing else would post it. Same shape as scalcout/acalcout.
    fn monitor_side_effect_fields(&self, put_field: &str) -> &'static [&'static str] {
        match put_field {
            "CALC" => &["CLCV"],
            _ => &[],
        }
    }

    /// C `execOutput` posts the refreshed DOLD with `DBE_VALUE` alone
    /// (swaitRecord.c:770), not the framework default `DBE_VALUE | DBE_LOG`;
    /// CLCV's post (`:309`, `:566`) carries a literal `DBE_VALUE` too.
    fn value_only_change_fields(&self) -> &'static [&'static str] {
        &["DOLD", "CLCV"]
    }

    /// C `swaitRecord.c::monitor` (646-653) posts a changed input A..L with
    /// `monitor_mask | DBE_VALUE` — no forced `DBE_LOG`:
    ///
    /// ```c
    /// for (i=0, pnew=&pwait->a, pprev=&pwait->la; i<MAX_FIELDS; i++, pnew++, pprev++) {
    ///     if (*pnew != *pprev) {
    ///         db_post_events(pwait, pnew, monitor_mask|DBE_VALUE);
    ///         ...
    /// ```
    ///
    /// so an archiver subscribed `DBE_LOG` to `swait.A` is sent a value only on
    /// a cycle where VAL's own ADEL deadband crossed (which puts `DBE_LOG` into
    /// `monitor_mask`), not on every change. `calcRecord.c:420` writes
    /// `monitor_mask | DBE_VALUE | DBE_LOG` in the same loop and keeps the
    /// framework default.
    ///
    /// LA..LL ride the same post: C advances `*pprev = *pnew` between the two
    /// `db_post_events` calls, so the previous-value field is published with the
    /// same mask as the input that moved it.
    fn fields_posted_with_monitor_mask(&self) -> &'static [&'static str] {
        &[
            "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "LA", "LB", "LC", "LD",
            "LE", "LF", "LG", "LH", "LI", "LJ", "LK", "LL",
        ]
    }

    /// `OEVT` ("Output Event"): post the numeric output event when output
    /// fires. C `swaitRecord.c` `execOutput` runs `if (pwait->oevt > 0)
    /// post_event((int)pwait->oevt);` (swaitRecord.c:797) right after the OUT
    /// write / forward link, on every cycle where output fires
    /// (`cached_should_output`). swait has no IVOA field, so — like its C —
    /// the post is never IVOA-suppressed. Stringified so the numeric event
    /// matches a `SCAN="Event"` record's `EVNT`.
    fn output_event(&self) -> Option<String> {
        if self.cached_should_output && self.oevt > 0 {
            Some(self.oevt.to_string())
        } else {
            None
        }
    }

    fn val(&self) -> Option<EpicsValue> {
        Some(EpicsValue::Double(self.val))
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
            "INIT" => Some(EpicsValue::Short(if self.initialized { 1 } else { 0 })),
            "OOPT" => Some(EpicsValue::Short(self.oopt)),
            "DOPT" => Some(EpicsValue::Short(self.dopt)),
            "DOLN" => Some(EpicsValue::String(self.doln.clone().into())),
            "DOLD" => Some(EpicsValue::Double(self.dold)),
            "OVAL" => Some(EpicsValue::Double(self.oval)),
            "OEVT" => Some(EpicsValue::UShort(self.oevt)),
            "ODLY" => Some(EpicsValue::Float(self.odly)),
            // OUTN is aliased to common.out via RecordInstance; not stored locally.
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
                    self.doln = s.as_str_lossy().into_owned();
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
                self.odly = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("ODLY".into()))?
                    as f32;
            }
            // OUTN falls through to put_common_field which mirrors to common.out.
            "SIML" => match value {
                EpicsValue::String(s) => self.siml = s.as_str_lossy().into_owned(),
                _ => return Err(CaError::TypeMismatch("SIML".into())),
            },
            "SIOL" => match value {
                EpicsValue::String(s) => self.siol = s.as_str_lossy().into_owned(),
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
                    // SPC_NOMOD to clients; this is where `refresh_link_status`'s
                    // `post_fields` -> `put_field_internal` stores the status it
                    // just classified.
                    self.pv_status[idx] = value
                        .to_f64()
                        .ok_or_else(|| CaError::TypeMismatch(name.into()))?
                        as i16;
                } else if let Some(idx) = Self::inp_name_index(name) {
                    if let EpicsValue::String(s) = value {
                        self.inp_names[idx] = s.as_str_lossy().into_owned();
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

    fn multi_input_links(&self) -> &[(&'static str, &'static str)] {
        &[
            ("INAN", "A"),
            ("INBN", "B"),
            ("INCN", "C"),
            ("INDN", "D"),
            ("INEN", "E"),
            ("INFN", "F"),
            ("INGN", "G"),
            ("INHN", "H"),
            ("ININ", "I"),
            ("INJN", "J"),
            ("INKN", "K"),
            ("INLN", "L"),
        ]
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
