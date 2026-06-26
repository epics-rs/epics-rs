use super::link_status::{LINK_CON, LINK_STATUS_CHOICES, LinkStatusGen, classify_link};
use crate::error::{CaError, CaResult};
use crate::server::database::AsyncDbHandle;
use crate::server::record::{
    FieldDesc, ProcessAction, ProcessOutcome, Record, RecordProcessResult,
};
use crate::types::{DbFieldType, EpicsValue, PvString};

/// Per-input link-status diagnostic field names (`INAV`..`INUV`), one per
/// calc input A..U, in C `menu(calcoutINAV)` order
/// (calcoutRecord.dbd.pod:865-1005). `OUTV` (the OUT-link status) is handled
/// separately because the OUT link is a common field, not a calcout field.
const CALCOUT_INAV_FIELDS: [&str; 21] = [
    "INAV", "INBV", "INCV", "INDV", "INEV", "INFV", "INGV", "INHV", "INIV", "INJV", "INKV", "INLV",
    "INMV", "INNV", "INOV", "INPV", "INQV", "INRV", "INSV", "INTV", "INUV",
];

/// Calcout record — calc with output.
pub struct CalcoutRecord {
    pub val: f64,
    pub calc: String,
    pub oopt: i16, // Output Option: 0=Every, 1=OnChange, 2=WhenZero, 3=WhenNonzero, 4=TransZero, 5=TransNonzero
    cached_should_output: bool, // Cached result from process() for framework
    // C `calcoutRecord.c::execOutput:620-625`: on a DOPT=Use_OVAL output cycle,
    // a successful OCAL `calcPerform` sets `udf = isnan(oval)` (NOT VAL-based),
    // which raises UDF_ALARM and lets IVOA gate the OUT write. `Some(_)` carries
    // that per-cycle decision to `value_is_undefined()`; `None` (Use_VAL, an OCAL
    // calc error, or a non-output cycle) leaves udf VAL-based, matching C.
    ocal_udf_override: Option<bool>,
    pub dopt: i16, // Data Option: 0=Use CALC, 1=Use OCAL
    pub ocal: String,
    pub oval: f64,
    pub ivoa: i16, // Invalid Output Action: 0=Continue, 1=Don't drive, 2=Set to IVOV
    pub ivov: f64,
    // Input links (INPA..INPU)
    pub inpa: String,
    pub inpb: String,
    pub inpc: String,
    pub inpd: String,
    pub inpe: String,
    pub inpf: String,
    pub inpg: String,
    pub inph: String,
    pub inpi: String,
    pub inpj: String,
    pub inpk: String,
    pub inpl: String,
    pub inpm: String,
    pub inpn: String,
    pub inpo: String,
    pub inpp: String,
    pub inpq: String,
    pub inpr: String,
    pub inps: String,
    pub inpt: String,
    pub inpu: String,
    // Input values (A..U)
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub e: f64,
    pub f: f64,
    pub g: f64,
    pub h: f64,
    pub i: f64,
    pub j: f64,
    pub k: f64,
    pub l: f64,
    pub m: f64,
    pub n: f64,
    pub o: f64,
    pub p: f64,
    pub q: f64,
    pub r: f64,
    pub s: f64,
    pub t: f64,
    pub u: f64,
    // Previous values LA..LU
    pub la: f64,
    pub lb: f64,
    pub lc: f64,
    pub ld: f64,
    pub le: f64,
    pub lf: f64,
    pub lg: f64,
    pub lh: f64,
    pub li: f64,
    pub lj: f64,
    pub lk: f64,
    pub ll: f64,
    pub lm: f64,
    pub ln: f64,
    pub lo: f64,
    pub lp: f64,
    pub lq: f64,
    pub lr: f64,
    pub ls: f64,
    pub lt: f64,
    pub lu: f64,
    // Display/engineering
    pub egu: PvString,
    pub prec: i16,
    pub hopr: f64,
    pub lopr: f64,
    // Monitor deadband
    pub adel: f64,
    pub mdel: f64,
    pub lalm: f64,
    pub alst: f64,
    pub mlst: f64,
    // Previous values for output determination
    pub pval: f64, // previous VAL (externally readable like C)
    // Output delay (ODLY) — C `calcoutRecord.c` `prec->odly`. When an
    // output should fire and ODLY > 0, the OUT-link write is deferred
    // by ODLY seconds via a delayed re-process.
    pub odly: f64,
    // Delay-active flag (DLYA) — C `prec->dlya`. Set to 1 while an ODLY
    // delay is pending, cleared on the delayed re-process. Externally
    // readable (DBF_SHORT) so clients can observe the pending state.
    pub dlya: i16,
    // Internal: captured output decision while an ODLY delay is pending.
    // The delayed re-process must write the output that the *original*
    // cycle decided on, not re-evaluate should_output() against the
    // (by then stale) pval/val.
    pending_output: bool,
    // CALC_ALARM flag
    pub calc_alarm: bool,
    // Cached compiled expressions (RPCL/ORPC equivalents)
    rpcl: Option<crate::calc::CompiledExpr>,
    orpc: Option<crate::calc::CompiledExpr>,
    // Per-input link connection status INAV..INUV and the OUT-link status
    // OUTV, menu(calcoutINAV). C `calcoutRecord.c::init_record`
    // (calcoutRecord.c:160-189) classifies each INPA..INPU input link and
    // the OUT link into these. Index 0..21 maps to inputs A..U.
    in_status: [i16; 21],
    out_status: i16,
    // Mirror of the common OUT-link string, synced from `CommonFields::out`
    // in `check_alarms`. The OUT link is a common field, not a calcout-owned
    // field, so this is the only in-record path to observe it for OUTV
    // classification (see `check_alarms`).
    out: String,
    // Async surface for posting the live INAV..INUV/OUTV diagnostics
    // (C `checkLinks`), wired by `set_async_context`.
    async_ctx: Option<(String, AsyncDbHandle)>,
    // Monotonic generation guarding the link-status refresh. Each refresh
    // classifies a *snapshot* of the link strings off-thread; a later
    // refresh (an INP/OUT re-point) must win over an earlier one regardless
    // of which spawned task finishes first. The shared `LinkStatusGen` gate
    // enforces the invariant — only the latest classification may be
    // published. See `link_status::LinkStatusGen`.
    link_gen: LinkStatusGen,
}

impl Default for CalcoutRecord {
    fn default() -> Self {
        Self {
            val: 0.0,
            calc: String::new(),
            oopt: 0,
            cached_should_output: false,
            ocal_udf_override: None,
            dopt: 0,
            ocal: String::new(),
            oval: 0.0,
            ivoa: 0,
            ivov: 0.0,
            inpa: String::new(),
            inpb: String::new(),
            inpc: String::new(),
            inpd: String::new(),
            inpe: String::new(),
            inpf: String::new(),
            inpg: String::new(),
            inph: String::new(),
            inpi: String::new(),
            inpj: String::new(),
            inpk: String::new(),
            inpl: String::new(),
            inpm: String::new(),
            inpn: String::new(),
            inpo: String::new(),
            inpp: String::new(),
            inpq: String::new(),
            inpr: String::new(),
            inps: String::new(),
            inpt: String::new(),
            inpu: String::new(),
            a: 0.0,
            b: 0.0,
            c: 0.0,
            d: 0.0,
            e: 0.0,
            f: 0.0,
            g: 0.0,
            h: 0.0,
            i: 0.0,
            j: 0.0,
            k: 0.0,
            l: 0.0,
            m: 0.0,
            n: 0.0,
            o: 0.0,
            p: 0.0,
            q: 0.0,
            r: 0.0,
            s: 0.0,
            t: 0.0,
            u: 0.0,
            la: 0.0,
            lb: 0.0,
            lc: 0.0,
            ld: 0.0,
            le: 0.0,
            lf: 0.0,
            lg: 0.0,
            lh: 0.0,
            li: 0.0,
            lj: 0.0,
            lk: 0.0,
            ll: 0.0,
            lm: 0.0,
            ln: 0.0,
            lo: 0.0,
            lp: 0.0,
            lq: 0.0,
            lr: 0.0,
            ls: 0.0,
            lt: 0.0,
            lu: 0.0,
            egu: PvString::new(),
            prec: 0,
            hopr: 0.0,
            lopr: 0.0,
            adel: 0.0,
            mdel: 0.0,
            lalm: 0.0,
            alst: 0.0,
            mlst: 0.0,
            pval: 0.0,
            odly: 0.0,
            dlya: 0,
            pending_output: false,
            calc_alarm: false,
            rpcl: None,
            orpc: None,
            // C `init_record` leaves an empty/unconfigured link CON
            // (calcoutRecord.c:166-167); the refresh re-classifies once the
            // async context exists.
            in_status: [LINK_CON; 21],
            out_status: LINK_CON,
            out: String::new(),
            async_ctx: None,
            link_gen: LinkStatusGen::default(),
        }
    }
}

impl CalcoutRecord {
    /// C `calcoutRecord.c::monitor`: advance the `LX` previous-value
    /// field only when the input `X` actually changed since the last
    /// monitor post.
    fn advance_prev(new: f64, prev: &mut f64) {
        if new != *prev {
            *prev = new;
        }
    }

    fn get_vars(&self) -> [f64; 21] {
        [
            self.a, self.b, self.c, self.d, self.e, self.f, self.g, self.h, self.i, self.j, self.k,
            self.l, self.m, self.n, self.o, self.p, self.q, self.r, self.s, self.t, self.u,
        ]
    }

    fn should_output(&self) -> bool {
        match self.oopt {
            0 => true,                                     // Every Time
            1 => (self.pval - self.val).abs() > self.mdel, // On Change (use MDEL like C)
            2 => self.val == 0.0,                          // When Zero
            3 => self.val != 0.0,                          // When Non-zero
            4 => self.pval != 0.0 && self.val == 0.0,      // Transition to Zero
            5 => self.pval == 0.0 && self.val != 0.0,      // Transition to Non-zero
            _ => false,                                    // Unknown: don't output (like C)
        }
    }

    /// The 21 input link strings (INPA..INPU) in input order A..U.
    fn input_links(&self) -> [String; 21] {
        [
            self.inpa.clone(),
            self.inpb.clone(),
            self.inpc.clone(),
            self.inpd.clone(),
            self.inpe.clone(),
            self.inpf.clone(),
            self.inpg.clone(),
            self.inph.clone(),
            self.inpi.clone(),
            self.inpj.clone(),
            self.inpk.clone(),
            self.inpl.clone(),
            self.inpm.clone(),
            self.inpn.clone(),
            self.inpo.clone(),
            self.inpp.clone(),
            self.inpq.clone(),
            self.inpr.clone(),
            self.inps.clone(),
            self.inpt.clone(),
            self.inpu.clone(),
        ]
    }

    /// Map an `INAV`..`INUV` field name to the input index 0..21 (A..U), or
    /// `None` for any other name (including `OUTV`, which the caller handles
    /// separately). The status fields are `IN<letter>V`, distinct from the
    /// `INP<letter>` link fields (which have no trailing `V`).
    fn input_status_index(name: &str) -> Option<usize> {
        let mid = name.strip_prefix("IN")?.strip_suffix('V')?;
        let [c] = mid.as_bytes() else { return None };
        match c {
            b'A'..=b'U' => Some((c - b'A') as usize),
            _ => None,
        }
    }

    /// True for the INP link-config fields whose put must re-classify the
    /// link diagnostics (C `calcoutRecord.c::special` SPC_MOD → `checkLinks`).
    /// `OUT` is excluded: it is a common field, so its post-put string is not
    /// visible here — OUTV re-classifies from `check_alarms` instead.
    fn is_link_config_field(name: &str) -> bool {
        match name.strip_prefix("INP") {
            Some(rest) => matches!(rest.as_bytes(), [b'A'..=b'U']),
            None => false,
        }
    }

    /// Classify every INP A..U link and the OUT link into their
    /// `menu(calcoutINAV)` connection status and post the live
    /// `INAV`..`INUV`/`OUTV` diagnostics, mirroring C
    /// `calcoutRecord.c::init_record` (calcoutRecord.c:160-189) and the
    /// `checkLinksCallback` re-poll. epics-base-rs surfaces no link
    /// connection-change signal, so (like sseq) the refresh runs at record
    /// init (`set_async_context`), on `special()` of an INP field, and when
    /// `check_alarms` observes the OUT link change. No-op without an async
    /// context.
    fn refresh_link_status(&self) {
        let Some((name, handle)) = &self.async_ctx else {
            return;
        };
        let name = name.clone();
        let handle = handle.clone();
        let inputs = self.input_links();
        let out = self.out.clone();
        let link_gen = self.link_gen.clone();
        // Stamp this refresh. A concurrent later refresh issues a newer
        // token; this task then sees its token is stale and drops its post so
        // it cannot clobber the newer classification (e.g. an init-time
        // snapshot finishing after a runtime INP re-point).
        let token = link_gen.next();
        tokio::spawn(async move {
            // Let `add_record` finish registering this record before the init
            // post (this task may be spawned from `set_async_context`, which
            // runs just before the record is inserted into the map).
            tokio::task::yield_now().await;
            let mut fields: Vec<(String, EpicsValue)> = Vec::with_capacity(22);
            for (i, link) in inputs.iter().enumerate() {
                let (status, _ft) = classify_link(&handle, link).await;
                fields.push((
                    CALCOUT_INAV_FIELDS[i].to_string(),
                    EpicsValue::Enum(status as u16),
                ));
            }
            let (out_status, _ft) = classify_link(&handle, &out).await;
            fields.push(("OUTV".to_string(), EpicsValue::Enum(out_status as u16)));
            // Publish only if no newer refresh was issued meanwhile.
            if link_gen.is_current(token) {
                let _ = handle.post_fields(&name, fields).await;
            }
        });
    }
}

static CALCOUT_FIELDS: &[FieldDesc] = &[
    FieldDesc {
        name: "VAL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "CALC",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "EGU",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "PREC",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "HOPR",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LOPR",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "ADEL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "MDEL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LALM",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "ALST",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "MLST",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "PVAL",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "OOPT",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "ODLY",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "DLYA",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    FieldDesc {
        name: "DOPT",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "OCAL",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OVAL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "IVOA",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "IVOV",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "INPA",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPB",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPC",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPD",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPE",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPF",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPG",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPH",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPI",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPJ",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPK",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPL",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPM",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPN",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPO",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPP",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPQ",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPR",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPS",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPT",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "INPU",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "A",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "B",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "C",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "D",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "E",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "F",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "G",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "H",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "I",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "J",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "K",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "L",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "M",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "N",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "O",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "P",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "Q",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "R",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "S",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "T",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "U",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "LA",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "LB",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "LC",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "LD",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "LE",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "LF",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "LG",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "LH",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "LI",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "LJ",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "LK",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "LL",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "LM",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "LN",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "LO",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "LP",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "LQ",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "LR",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "LS",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "LT",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "LU",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    // INAV..INUV / OUTV link-status menus (menu(calcoutINAV),
    // calcoutRecord.dbd.pod:865-1012): DBF_MENU served as DBR_ENUM, SPC_NOMOD
    // (read-only to clients; the link-status refresh posts them internally).
    FieldDesc {
        name: "INAV",
        dbf_type: DbFieldType::Enum,
        read_only: true,
    },
    FieldDesc {
        name: "INBV",
        dbf_type: DbFieldType::Enum,
        read_only: true,
    },
    FieldDesc {
        name: "INCV",
        dbf_type: DbFieldType::Enum,
        read_only: true,
    },
    FieldDesc {
        name: "INDV",
        dbf_type: DbFieldType::Enum,
        read_only: true,
    },
    FieldDesc {
        name: "INEV",
        dbf_type: DbFieldType::Enum,
        read_only: true,
    },
    FieldDesc {
        name: "INFV",
        dbf_type: DbFieldType::Enum,
        read_only: true,
    },
    FieldDesc {
        name: "INGV",
        dbf_type: DbFieldType::Enum,
        read_only: true,
    },
    FieldDesc {
        name: "INHV",
        dbf_type: DbFieldType::Enum,
        read_only: true,
    },
    FieldDesc {
        name: "INIV",
        dbf_type: DbFieldType::Enum,
        read_only: true,
    },
    FieldDesc {
        name: "INJV",
        dbf_type: DbFieldType::Enum,
        read_only: true,
    },
    FieldDesc {
        name: "INKV",
        dbf_type: DbFieldType::Enum,
        read_only: true,
    },
    FieldDesc {
        name: "INLV",
        dbf_type: DbFieldType::Enum,
        read_only: true,
    },
    FieldDesc {
        name: "INMV",
        dbf_type: DbFieldType::Enum,
        read_only: true,
    },
    FieldDesc {
        name: "INNV",
        dbf_type: DbFieldType::Enum,
        read_only: true,
    },
    FieldDesc {
        name: "INOV",
        dbf_type: DbFieldType::Enum,
        read_only: true,
    },
    FieldDesc {
        name: "INPV",
        dbf_type: DbFieldType::Enum,
        read_only: true,
    },
    FieldDesc {
        name: "INQV",
        dbf_type: DbFieldType::Enum,
        read_only: true,
    },
    FieldDesc {
        name: "INRV",
        dbf_type: DbFieldType::Enum,
        read_only: true,
    },
    FieldDesc {
        name: "INSV",
        dbf_type: DbFieldType::Enum,
        read_only: true,
    },
    FieldDesc {
        name: "INTV",
        dbf_type: DbFieldType::Enum,
        read_only: true,
    },
    FieldDesc {
        name: "INUV",
        dbf_type: DbFieldType::Enum,
        read_only: true,
    },
    FieldDesc {
        name: "OUTV",
        dbf_type: DbFieldType::Enum,
        read_only: true,
    },
];

/// Choice labels for the output-execute-option menu, in index order.
/// C `menu(calcoutOOPT)` (`calcoutRecord.dbd.pod:33-39`).
const CALCOUT_OOPT_CHOICES: &[&str] = &[
    "Every Time",
    "On Change",
    "When Zero",
    "When Non-zero",
    "Transition To Zero",
    "Transition To Non-zero",
];

/// Choice labels for the output-data-option menu, in index order.
/// C `menu(calcoutDOPT)` (`calcoutRecord.dbd.pod:41-43`).
const CALCOUT_DOPT_CHOICES: &[&str] = &["Use CALC", "Use OCAL"];

impl Record for CalcoutRecord {
    fn record_type(&self) -> &'static str {
        "calcout"
    }

    // C raises UDF_ALARM from BOTH `checkAlarms` and `execOutput`, and
    // `recGblSetSevr` only raises (never lowers), so the effective UDF
    // condition is the OR of the two:
    //   * `checkAlarms` (`calcoutRecord.c:244`, BEFORE the output switch) sees
    //     `udf = isnan(VAL)` (set at line 241) and raises UDF_ALARM if VAL is
    //     NaN — independent of OVAL.
    //   * `execOutput` (`calcoutRecord.c:620-630`, Use_OVAL output cycle) then
    //     sets `udf = isnan(OVAL)` and raises UDF_ALARM if OVAL is NaN.
    // So a NaN VAL keeps the record INVALID even when OCAL yields a finite OVAL
    // (the `checkAlarms` raise stands). `ocal_udf_override` carries the
    // execOutput half — `Some(true)` when a Use_OVAL output cycle produced a
    // NaN OVAL — and is OR'd with the VAL-NaN half. `None` (Use_VAL / OCAL calc
    // error / non-output cycle) leaves udf purely VAL-based, matching the trait
    // default. (Residual: C's udf *field* ends at `isnan(OVAL)` on a Use_OVAL
    // output cycle, so for NaN VAL + finite OVAL C reports UDF field 0 with
    // SEVR INVALID; Rust's single value_is_undefined() reports the field as 1.
    // The SEVR/STAT — the parity-critical observables — match.)
    fn value_is_undefined(&self) -> bool {
        self.val.is_nan() || matches!(self.ocal_udf_override, Some(true))
    }

    // C recCalcout.c IVOA=set_to_IVOV: oval = ivov; the OUT writeback
    // then sends OVAL. VAL is the calc *result* and remains intact.
    //
    // The `oval = ivov` substitution lives inside `execOutput`
    // (calcoutRecord.c:646), which `process` calls ONLY under the
    // `if (doOutput)` gate (calcoutRecord.c:276). So a non-output INVALID
    // cycle (OOPT condition not met) must NOT clobber OVAL to IVOV — the
    // retained OVAL stands and no spurious OVAL monitor is posted.
    // `cached_should_output` is this cycle's doOutput decision. This is NOT
    // additionally gated on a calc-failure (unlike acalcout): calcout's hook
    // runs after the framework's `evaluate_alarms`, so the INVALID severity it
    // sees already covers calc/limit/MS — exactly as C `execOutput` applies
    // IVOA on any `nsev >= INVALID_ALARM`.
    fn apply_invalid_output_value(&mut self, ivov: EpicsValue) -> CaResult<()> {
        if self.cached_should_output {
            self.put_field("OVAL", ivov)
        } else {
            Ok(())
        }
    }

    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 0 {
            if !self.calc.is_empty() {
                self.rpcl = crate::calc::compile(&self.calc).ok();
            }
            if !self.ocal.is_empty() {
                self.orpc = crate::calc::compile(&self.ocal).ok();
            }
            self.pval = self.val;
            self.mlst = self.val;
            self.alst = self.val;
            self.lalm = self.val;
        }
        Ok(())
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // ODLY continuation: this is the delayed re-process scheduled by a
        // previous cycle (C `calcoutRecord.c::process` `pact==TRUE` +
        // `dlya` branch). Do NOT re-evaluate CALC / should_output here —
        // C runs `execOutput` directly. Honour the output decision the
        // original cycle captured, clear DLYA, and let the framework
        // write the OUT link.
        if self.dlya == 1 {
            self.dlya = 0;
            self.cached_should_output = self.pending_output;
            self.pending_output = false;
            return Ok(ProcessOutcome::complete());
        }

        // NOTE: pval is updated AFTER CALC evaluation (at the end),
        // not before. It holds the previous cycle's value for
        // transition detection in should_output().

        // Evaluate CALC using cached RPCL
        if let Some(ref compiled) = self.rpcl {
            let mut inputs = crate::calc::NumericInputs::with_vars(self.get_vars());
            // C `calcPerform(&prec->a, &prec->val, rpcl)` (calcoutRecord.c:238)
            // passes `presult = &val`, so the CALC `VAL` token reads the
            // *previous* VAL. Seed before `self.val` is overwritten below.
            inputs.prev_val = self.val;
            match crate::calc::eval(compiled, &mut inputs) {
                Ok(v) => {
                    self.val = v;
                    self.calc_alarm = false;
                }
                Err(_) => {
                    self.calc_alarm = true;
                }
            }
        }

        // Determine output and evaluate OCAL if needed. C runs this in
        // `execOutput`, called only when the OOPT predicate (`should_output`)
        // fired — so the OCAL-derived udf below is reset every cycle and set
        // only on an actual Use_OVAL output cycle.
        self.ocal_udf_override = None;
        if self.should_output() {
            if self.dopt == 1 {
                // Use OCAL
                if let Some(ref compiled) = self.orpc {
                    let mut inputs = crate::calc::NumericInputs::with_vars(self.get_vars());
                    // C `calcPerform(&prec->a, &prec->oval, orpc)`
                    // (calcoutRecord.c:621) passes `presult = &oval`, so the
                    // OCAL `VAL` token reads the *previous* OVAL, not VAL.
                    inputs.prev_val = self.oval;
                    match crate::calc::eval(compiled, &mut inputs) {
                        Ok(v) => {
                            self.oval = v;
                            // C `execOutput:624`: `prec->udf = isnan(prec->oval)`
                            // on the successful-OCAL branch. A NaN OVAL then
                            // raises UDF_ALARM (execOutput:628) so IVOA gates the
                            // OUT write — without this a finite VAL but NaN OVAL
                            // drives NaN to OUT with NO_ALARM (silent-wrong-value).
                            self.ocal_udf_override = Some(self.oval.is_nan());
                        }
                        // C `execOutput:622`: OCAL calcPerform failure raises
                        // CALC_ALARM and leaves udf VAL-based (no override).
                        Err(_) => self.calc_alarm = true,
                    }
                }
            } else {
                self.oval = self.val;
            }
        }
        // Update LA-LU. C `calcoutRecord.c::monitor` (lines 679-685)
        // advances `*pprev = *pnew` only inside the per-field change test
        // (`if (*pnew != *pprev || monitor_mask & DBE_ALARM)`), i.e. only
        // for inputs that actually changed since the last monitor post.
        Self::advance_prev(self.a, &mut self.la);
        Self::advance_prev(self.b, &mut self.lb);
        Self::advance_prev(self.c, &mut self.lc);
        Self::advance_prev(self.d, &mut self.ld);
        Self::advance_prev(self.e, &mut self.le);
        Self::advance_prev(self.f, &mut self.lf);
        Self::advance_prev(self.g, &mut self.lg);
        Self::advance_prev(self.h, &mut self.lh);
        Self::advance_prev(self.i, &mut self.li);
        Self::advance_prev(self.j, &mut self.lj);
        Self::advance_prev(self.k, &mut self.lk);
        Self::advance_prev(self.l, &mut self.ll);
        Self::advance_prev(self.m, &mut self.lm);
        Self::advance_prev(self.n, &mut self.ln);
        Self::advance_prev(self.o, &mut self.lo);
        Self::advance_prev(self.p, &mut self.lp);
        Self::advance_prev(self.q, &mut self.lq);
        Self::advance_prev(self.r, &mut self.lr);
        Self::advance_prev(self.s, &mut self.ls);
        Self::advance_prev(self.t, &mut self.lt);
        Self::advance_prev(self.u, &mut self.lu);

        // Cache should_output result BEFORE updating pval, because
        // framework calls should_output() after process() returns,
        // but by then pval would already equal val.
        let do_output = self.should_output();

        // Now update pval for next cycle
        self.pval = self.val;

        // ODLY (C `calcoutRecord.c::process` lines 276-288): when an
        // output should fire and ODLY > 0, defer the OUT-link write by
        // ODLY seconds. Set DLYA, suppress this cycle's output, and ask
        // the framework to re-process after the delay. The continuation
        // branch at the top of process() then emits the captured output.
        if do_output && self.odly > 0.0 {
            self.dlya = 1;
            self.pending_output = true;
            self.cached_should_output = false;
            let delay = std::time::Duration::from_secs_f64(self.odly);
            // C `calcoutRecord.c::process` (lines 277-282): the delaying
            // cycle sets DLYA, posts it (`db_post_events(&prec->dlya,
            // DBE_VALUE)`), schedules the delayed callback, and `return 0`
            // — BEFORE `monitor()` (306) and `recGblFwdLink()` (307). So
            // VAL/OVAL monitors and the forward link are NOT emitted on the
            // delaying cycle; they fire once on the delayed (callback) cycle.
            // Model this as an async-pending-notify pass: the framework
            // posts only DLYA now and defers the FLNK + VAL/OVAL snapshot to
            // the Complete continuation (the `dlya == 1` branch at the top
            // of process()). The previous `complete_with` ran the full
            // snapshot + FLNK tail on the delaying cycle, so VAL/OVAL posted
            // ODLY-seconds early and the forward link fired twice.
            return Ok(ProcessOutcome {
                result: RecordProcessResult::AsyncPendingNotify(vec![(
                    "DLYA".to_string(),
                    EpicsValue::Short(1),
                )]),
                actions: vec![ProcessAction::ReprocessAfter(delay)],
                device_did_compute: false,
            });
        }

        self.cached_should_output = do_output;
        Ok(ProcessOutcome::complete())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => Some(EpicsValue::Double(self.val)),
            "CALC" => Some(EpicsValue::String(self.calc.clone().into())),
            "EGU" => Some(EpicsValue::String(self.egu.clone())),
            "PREC" => Some(EpicsValue::Short(self.prec)),
            "HOPR" => Some(EpicsValue::Double(self.hopr)),
            "LOPR" => Some(EpicsValue::Double(self.lopr)),
            "ADEL" => Some(EpicsValue::Double(self.adel)),
            "MDEL" => Some(EpicsValue::Double(self.mdel)),
            "LALM" => Some(EpicsValue::Double(self.lalm)),
            "ALST" => Some(EpicsValue::Double(self.alst)),
            "MLST" => Some(EpicsValue::Double(self.mlst)),
            "CALC_ALARM" => Some(EpicsValue::Char(if self.calc_alarm { 1 } else { 0 })),
            "PVAL" => Some(EpicsValue::Double(self.pval)),
            "OOPT" => Some(EpicsValue::Short(self.oopt)),
            "ODLY" => Some(EpicsValue::Double(self.odly)),
            "DLYA" => Some(EpicsValue::Short(self.dlya)),
            "DOPT" => Some(EpicsValue::Short(self.dopt)),
            "OCAL" => Some(EpicsValue::String(self.ocal.clone().into())),
            "OVAL" => Some(EpicsValue::Double(self.oval)),
            "IVOA" => Some(EpicsValue::Short(self.ivoa)),
            "IVOV" => Some(EpicsValue::Double(self.ivov)),
            "INPA" => Some(EpicsValue::String(self.inpa.clone().into())),
            "INPB" => Some(EpicsValue::String(self.inpb.clone().into())),
            "INPC" => Some(EpicsValue::String(self.inpc.clone().into())),
            "INPD" => Some(EpicsValue::String(self.inpd.clone().into())),
            "INPE" => Some(EpicsValue::String(self.inpe.clone().into())),
            "INPF" => Some(EpicsValue::String(self.inpf.clone().into())),
            "INPG" => Some(EpicsValue::String(self.inpg.clone().into())),
            "INPH" => Some(EpicsValue::String(self.inph.clone().into())),
            "INPI" => Some(EpicsValue::String(self.inpi.clone().into())),
            "INPJ" => Some(EpicsValue::String(self.inpj.clone().into())),
            "INPK" => Some(EpicsValue::String(self.inpk.clone().into())),
            "INPL" => Some(EpicsValue::String(self.inpl.clone().into())),
            "INPM" => Some(EpicsValue::String(self.inpm.clone().into())),
            "INPN" => Some(EpicsValue::String(self.inpn.clone().into())),
            "INPO" => Some(EpicsValue::String(self.inpo.clone().into())),
            "INPP" => Some(EpicsValue::String(self.inpp.clone().into())),
            "INPQ" => Some(EpicsValue::String(self.inpq.clone().into())),
            "INPR" => Some(EpicsValue::String(self.inpr.clone().into())),
            "INPS" => Some(EpicsValue::String(self.inps.clone().into())),
            "INPT" => Some(EpicsValue::String(self.inpt.clone().into())),
            "INPU" => Some(EpicsValue::String(self.inpu.clone().into())),
            "A" => Some(EpicsValue::Double(self.a)),
            "B" => Some(EpicsValue::Double(self.b)),
            "C" => Some(EpicsValue::Double(self.c)),
            "D" => Some(EpicsValue::Double(self.d)),
            "E" => Some(EpicsValue::Double(self.e)),
            "F" => Some(EpicsValue::Double(self.f)),
            "G" => Some(EpicsValue::Double(self.g)),
            "H" => Some(EpicsValue::Double(self.h)),
            "I" => Some(EpicsValue::Double(self.i)),
            "J" => Some(EpicsValue::Double(self.j)),
            "K" => Some(EpicsValue::Double(self.k)),
            "L" => Some(EpicsValue::Double(self.l)),
            "M" => Some(EpicsValue::Double(self.m)),
            "N" => Some(EpicsValue::Double(self.n)),
            "O" => Some(EpicsValue::Double(self.o)),
            "P" => Some(EpicsValue::Double(self.p)),
            "Q" => Some(EpicsValue::Double(self.q)),
            "R" => Some(EpicsValue::Double(self.r)),
            "S" => Some(EpicsValue::Double(self.s)),
            "T" => Some(EpicsValue::Double(self.t)),
            "U" => Some(EpicsValue::Double(self.u)),
            "LA" => Some(EpicsValue::Double(self.la)),
            "LB" => Some(EpicsValue::Double(self.lb)),
            "LC" => Some(EpicsValue::Double(self.lc)),
            "LD" => Some(EpicsValue::Double(self.ld)),
            "LE" => Some(EpicsValue::Double(self.le)),
            "LF" => Some(EpicsValue::Double(self.lf)),
            "LG" => Some(EpicsValue::Double(self.lg)),
            "LH" => Some(EpicsValue::Double(self.lh)),
            "LI" => Some(EpicsValue::Double(self.li)),
            "LJ" => Some(EpicsValue::Double(self.lj)),
            "LK" => Some(EpicsValue::Double(self.lk)),
            "LL" => Some(EpicsValue::Double(self.ll)),
            "LM" => Some(EpicsValue::Double(self.lm)),
            "LN" => Some(EpicsValue::Double(self.ln)),
            "LO" => Some(EpicsValue::Double(self.lo)),
            "LP" => Some(EpicsValue::Double(self.lp)),
            "LQ" => Some(EpicsValue::Double(self.lq)),
            "LR" => Some(EpicsValue::Double(self.lr)),
            "LS" => Some(EpicsValue::Double(self.ls)),
            "LT" => Some(EpicsValue::Double(self.lt)),
            "LU" => Some(EpicsValue::Double(self.lu)),
            // INAV..INUV / OUTV link-status menus (menu(calcoutINAV),
            // calcoutRecord.dbd.pod:865-1012), served as DBR_ENUM; labels
            // from menu_field_choices. Live status from refresh_link_status.
            _ => {
                if let Some(idx) = Self::input_status_index(name) {
                    Some(EpicsValue::Enum(self.in_status[idx] as u16))
                } else if name == "OUTV" {
                    Some(EpicsValue::Enum(self.out_status as u16))
                } else {
                    None
                }
            }
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => match value {
                EpicsValue::Double(v) => {
                    self.val = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("VAL".into())),
            },
            "CALC" => match value {
                EpicsValue::String(s) => {
                    self.rpcl = crate::calc::compile(&s.as_str_lossy()).ok();
                    self.calc = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("CALC".into())),
            },
            "EGU" => match value {
                EpicsValue::String(s) => {
                    self.egu = s;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "PREC" => match value {
                EpicsValue::Short(v) => {
                    self.prec = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "HOPR" => match value {
                EpicsValue::Double(v) => {
                    self.hopr = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "LOPR" => match value {
                EpicsValue::Double(v) => {
                    self.lopr = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "ADEL" => match value {
                EpicsValue::Double(v) => {
                    self.adel = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "MDEL" => match value {
                EpicsValue::Double(v) => {
                    self.mdel = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "LALM" => match value {
                EpicsValue::Double(v) => {
                    self.lalm = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "ALST" => match value {
                EpicsValue::Double(v) => {
                    self.alst = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "MLST" => match value {
                EpicsValue::Double(v) => {
                    self.mlst = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch(name.into())),
            },
            "OOPT" => match value {
                EpicsValue::Short(v) => {
                    self.oopt = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("OOPT".into())),
            },
            "ODLY" => match value {
                EpicsValue::Double(v) => {
                    self.odly = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("ODLY".into())),
            },
            "DLYA" => Err(CaError::ReadOnlyField("DLYA".into())),
            "DOPT" => match value {
                EpicsValue::Short(v) => {
                    self.dopt = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("DOPT".into())),
            },
            "OCAL" => match value {
                EpicsValue::String(s) => {
                    self.orpc = crate::calc::compile(&s.as_str_lossy()).ok();
                    self.ocal = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("OCAL".into())),
            },
            "OVAL" => match value {
                EpicsValue::Double(v) => {
                    self.oval = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("OVAL".into())),
            },
            "IVOA" => match value {
                EpicsValue::Short(v) => {
                    self.ivoa = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("IVOA".into())),
            },
            "IVOV" => match value {
                EpicsValue::Double(v) => {
                    self.ivov = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("IVOV".into())),
            },
            "INPA" => match value {
                EpicsValue::String(s) => {
                    self.inpa = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPA".into())),
            },
            "INPB" => match value {
                EpicsValue::String(s) => {
                    self.inpb = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPB".into())),
            },
            "INPC" => match value {
                EpicsValue::String(s) => {
                    self.inpc = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPC".into())),
            },
            "INPD" => match value {
                EpicsValue::String(s) => {
                    self.inpd = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPD".into())),
            },
            "INPE" => match value {
                EpicsValue::String(s) => {
                    self.inpe = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPE".into())),
            },
            "INPF" => match value {
                EpicsValue::String(s) => {
                    self.inpf = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPF".into())),
            },
            "INPG" => match value {
                EpicsValue::String(s) => {
                    self.inpg = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPG".into())),
            },
            "INPH" => match value {
                EpicsValue::String(s) => {
                    self.inph = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPH".into())),
            },
            "INPI" => match value {
                EpicsValue::String(s) => {
                    self.inpi = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPI".into())),
            },
            "INPJ" => match value {
                EpicsValue::String(s) => {
                    self.inpj = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPJ".into())),
            },
            "INPK" => match value {
                EpicsValue::String(s) => {
                    self.inpk = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPK".into())),
            },
            "INPL" => match value {
                EpicsValue::String(s) => {
                    self.inpl = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPL".into())),
            },
            "INPM" => match value {
                EpicsValue::String(s) => {
                    self.inpm = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPM".into())),
            },
            "INPN" => match value {
                EpicsValue::String(s) => {
                    self.inpn = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPN".into())),
            },
            "INPO" => match value {
                EpicsValue::String(s) => {
                    self.inpo = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPO".into())),
            },
            "INPP" => match value {
                EpicsValue::String(s) => {
                    self.inpp = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPP".into())),
            },
            "INPQ" => match value {
                EpicsValue::String(s) => {
                    self.inpq = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPQ".into())),
            },
            "INPR" => match value {
                EpicsValue::String(s) => {
                    self.inpr = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPR".into())),
            },
            "INPS" => match value {
                EpicsValue::String(s) => {
                    self.inps = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPS".into())),
            },
            "INPT" => match value {
                EpicsValue::String(s) => {
                    self.inpt = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPT".into())),
            },
            "INPU" => match value {
                EpicsValue::String(s) => {
                    self.inpu = s.as_str_lossy().into_owned();
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("INPU".into())),
            },
            "A" | "B" | "C" | "D" | "E" | "F" | "G" | "H" | "I" | "J" | "K" | "L" | "M" | "N"
            | "O" | "P" | "Q" | "R" | "S" | "T" | "U" => {
                let v = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch(name.into()))?;
                match name {
                    "A" => self.a = v,
                    "B" => self.b = v,
                    "C" => self.c = v,
                    "D" => self.d = v,
                    "E" => self.e = v,
                    "F" => self.f = v,
                    "G" => self.g = v,
                    "H" => self.h = v,
                    "I" => self.i = v,
                    "J" => self.j = v,
                    "K" => self.k = v,
                    "L" => self.l = v,
                    "M" => self.m = v,
                    "N" => self.n = v,
                    "O" => self.o = v,
                    "P" => self.p = v,
                    "Q" => self.q = v,
                    "R" => self.r = v,
                    "S" => self.s = v,
                    "T" => self.t = v,
                    "U" => self.u = v,
                    _ => unreachable!(),
                }
                Ok(())
            }
            _ => {
                // INAV..INUV / OUTV link-status menus are read-only to
                // clients (SPC_NOMOD, calcoutRecord.dbd.pod:867); the
                // link-status refresh (`post_fields` → `put_field_internal`)
                // lands here to store the connection status it just computed.
                if let Some(idx) = Self::input_status_index(name) {
                    self.in_status[idx] = value
                        .to_f64()
                        .ok_or_else(|| CaError::TypeMismatch(name.into()))?
                        as i16;
                    Ok(())
                } else if name == "OUTV" {
                    self.out_status = value
                        .to_f64()
                        .ok_or_else(|| CaError::TypeMismatch(name.into()))?
                        as i16;
                    Ok(())
                } else {
                    Err(CaError::FieldNotFound(name.to_string()))
                }
            }
        }
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        CALCOUT_FIELDS
    }

    fn menu_field_choices(&self, field: &str) -> Option<&'static [&'static str]> {
        match field {
            "OOPT" => Some(CALCOUT_OOPT_CHOICES),
            "DOPT" => Some(CALCOUT_DOPT_CHOICES),
            // INAV..INUV / OUTV link-status menus (menu(calcoutINAV)).
            "OUTV" => Some(LINK_STATUS_CHOICES),
            _ if Self::input_status_index(field).is_some() => Some(LINK_STATUS_CHOICES),
            _ => None,
        }
    }

    fn multi_input_links(&self) -> &[(&'static str, &'static str)] {
        &[
            ("INPA", "A"),
            ("INPB", "B"),
            ("INPC", "C"),
            ("INPD", "D"),
            ("INPE", "E"),
            ("INPF", "F"),
            ("INPG", "G"),
            ("INPH", "H"),
            ("INPI", "I"),
            ("INPJ", "J"),
            ("INPK", "K"),
            ("INPL", "L"),
            ("INPM", "M"),
            ("INPN", "N"),
            ("INPO", "O"),
            ("INPP", "P"),
            ("INPQ", "Q"),
            ("INPR", "R"),
            ("INPS", "S"),
            ("INPT", "T"),
            ("INPU", "U"),
        ]
    }

    fn should_output(&self) -> bool {
        self.cached_should_output
    }

    fn can_device_write(&self) -> bool {
        // calcout has a soft OUT link, not device support
        false
    }

    fn set_async_context(&mut self, name: String, db: AsyncDbHandle) {
        self.async_ctx = Some((name, db));
        // C `init_record` (calcoutRecord.c:160-189) classifies every INP and
        // the OUT link into INAV..INUV/OUTV. The INP links are calcout fields
        // already applied (record fields load before `add_record`), so
        // classify them now. The OUT link is a common field not yet applied
        // at `add_record`; it is captured later in `init_links` (load) or
        // `check_alarms` (a runtime OUT re-point). The generation gate lets
        // that later, fuller refresh supersede this one.
        self.refresh_link_status();
    }

    fn init_links(&mut self, common: &crate::server::record::CommonFields) {
        // C `calcoutRecord.c::init_record` (calcoutRecord.c:160-189)
        // classifies the OUT link at `i == CALCPERFORM_NARGS`, at load —
        // before any process. The OUT link is a common field invisible to
        // `set_async_context` (which ran before the common fields were
        // applied), so capture it here, once the framework has resolved it,
        // and classify so a passive never-processed record already shows OUTV.
        self.out = common.out.clone();
        self.refresh_link_status();
    }

    fn special(&mut self, field: &str, after: bool) -> CaResult<()> {
        if !after {
            return Ok(());
        }
        // A put to an INP link re-classifies the link diagnostics: C
        // `calcoutRecord.c::special` (SPC_MOD) re-runs `checkLinks`. The INP
        // string the put just stored is re-read by `refresh_link_status`.
        // OUT is excluded here (see `is_link_config_field`); it re-classifies
        // from `check_alarms`.
        if Self::is_link_config_field(field) {
            self.refresh_link_status();
        }
        Ok(())
    }

    fn check_alarms(&mut self, common: &mut crate::server::record::CommonFields) {
        // The OUT link lives in the common fields, not a calcout-owned field.
        // `init_links` captures it at load; this hook catches a *runtime* OUT
        // re-point (a put to OUT does not process, so `special("OUT")` cannot
        // re-classify it — see `is_link_config_field`). C re-runs the same
        // `init_record`/`checkLinks` OUT classification on any link change
        // (calcoutRecord.c:160-189). Only re-classify when OUT actually moved.
        if self.out != common.out {
            self.out = common.out.clone();
            self.refresh_link_status();
        }
    }
}

#[cfg(test)]
mod link_status_tests {
    use super::*;

    // The link-status menu choice labels, C `menu(calcoutINAV)`
    // (calcoutRecord.dbd.pod:45-50): identical to sseqLNKV.
    const CHOICES: &[&str] = &["Ext PV NC", "Ext PV OK", "Local PV", "Constant"];
    const LOC: u16 = 2;
    const CON: u16 = 3;

    // Boundary: the `IN<letter>V` status field name maps to the input
    // index A..U, and is distinct from the `INP<letter>` link field (no
    // trailing `V`) and from `OUTV` (handled separately).
    #[test]
    fn input_status_index_boundaries() {
        assert_eq!(CalcoutRecord::input_status_index("INAV"), Some(0)); // input A
        assert_eq!(CalcoutRecord::input_status_index("INUV"), Some(20)); // input U (last)
        assert_eq!(CalcoutRecord::input_status_index("INPV"), Some(15)); // status of input P
        // OUTV is not an input-status field (caller handles it).
        assert_eq!(CalcoutRecord::input_status_index("OUTV"), None);
        // INP<letter> link fields have no trailing V → not a status field.
        assert_eq!(CalcoutRecord::input_status_index("INPA"), None);
        assert_eq!(CalcoutRecord::input_status_index("INPU"), None);
        // 'V' is past 'U' (CALCPERFORM_NARGS == 21) → no such input.
        assert_eq!(CalcoutRecord::input_status_index("INVV"), None);
        // Two-letter middle → not a single input.
        assert_eq!(CalcoutRecord::input_status_index("INABV"), None);
    }

    // Boundary: `special()` re-classifies only on an INP link put; OUT is a
    // common field whose post-put string is invisible here.
    #[test]
    fn is_link_config_field_only_inp_links() {
        assert!(CalcoutRecord::is_link_config_field("INPA"));
        assert!(CalcoutRecord::is_link_config_field("INPU"));
        assert!(!CalcoutRecord::is_link_config_field("OUT"));
        assert!(!CalcoutRecord::is_link_config_field("INAV")); // status, not link
        assert!(!CalcoutRecord::is_link_config_field("CALC"));
    }

    // Every INAV..INUV and OUTV serves the menu(calcoutINAV) labels; the
    // INP link fields do not.
    #[test]
    fn link_status_menu_labels_served() {
        let rec = CalcoutRecord::default();
        for f in CALCOUT_INAV_FIELDS.iter().chain(std::iter::once(&"OUTV")) {
            assert_eq!(
                rec.menu_field_choices(f),
                Some(CHOICES),
                "{f} must serve menu(calcoutINAV) labels"
            );
        }
        assert_eq!(rec.menu_field_choices("INPA"), None);
    }

    // Default-constructed record: empty/unconfigured links classify CON
    // (C calcoutRecord.c:166-167), served as DBR_ENUM index 3.
    #[test]
    fn link_status_defaults_to_con() {
        let rec = CalcoutRecord::default();
        assert_eq!(rec.get_field("INAV"), Some(EpicsValue::Enum(CON)));
        assert_eq!(rec.get_field("INUV"), Some(EpicsValue::Enum(CON)));
        assert_eq!(rec.get_field("OUTV"), Some(EpicsValue::Enum(CON)));
    }

    // The internal link-status refresh writes through put_field
    // (post_fields → put_field_internal); a write must round-trip.
    #[test]
    fn link_status_internal_put_roundtrips() {
        let mut rec = CalcoutRecord::default();
        rec.put_field("INAV", EpicsValue::Enum(LOC)).unwrap();
        rec.put_field("OUTV", EpicsValue::Enum(LOC)).unwrap();
        assert_eq!(rec.get_field("INAV"), Some(EpicsValue::Enum(LOC)));
        assert_eq!(rec.get_field("OUTV"), Some(EpicsValue::Enum(LOC)));
        // A non-status unknown field still errors.
        assert!(rec.put_field("NOSUCH", EpicsValue::Enum(0)).is_err());
    }

    // All 22 status fields are in the field table as DBF_MENU→Enum,
    // read-only to clients (SPC_NOMOD, calcoutRecord.dbd.pod:867).
    #[test]
    fn link_status_fields_are_read_only_enum_in_table() {
        for name in CALCOUT_INAV_FIELDS.iter().chain(std::iter::once(&"OUTV")) {
            let fd = CALCOUT_FIELDS
                .iter()
                .find(|f| f.name == *name)
                .unwrap_or_else(|| panic!("{name} missing from CALCOUT_FIELDS"));
            assert_eq!(fd.dbf_type, DbFieldType::Enum, "{name} must be ENUM");
            assert!(fd.read_only, "{name} must be read-only (SPC_NOMOD)");
        }
        assert_eq!(CALCOUT_INAV_FIELDS.len(), 21);
    }
}

#[cfg(test)]
mod process_tests {
    use super::*;

    /// CALC `VAL` token reads the previous VAL (C `presult = &val`,
    /// calcoutRecord.c:238), so `CALC="VAL+1"` counts up.
    #[test]
    fn calc_val_token_reads_previous_val() {
        let mut rec = CalcoutRecord {
            calc: "VAL+1".to_string(),
            ..Default::default()
        };
        rec.init_record(0).unwrap();
        rec.process().unwrap();
        assert_eq!(rec.val, 1.0);
        rec.process().unwrap();
        assert_eq!(rec.val, 2.0);
    }

    /// OCAL `VAL` token reads the previous OVAL, not VAL (C `presult =
    /// &oval`, calcoutRecord.c:621). With DOPT="Use OCAL" and OOPT="Every
    /// Time", `OCAL="VAL+1"` makes OVAL count up while VAL stays 0.
    #[test]
    fn ocal_val_token_reads_previous_oval() {
        let mut rec = CalcoutRecord {
            // CALC empty → VAL stays 0; only OCAL drives OVAL.
            ocal: "VAL+1".to_string(),
            dopt: 1, // Use OCAL
            oopt: 0, // Every Time → should_output() always true
            ..Default::default()
        };
        rec.init_record(0).unwrap();
        rec.process().unwrap();
        assert_eq!(rec.val, 0.0);
        assert_eq!(rec.oval, 1.0);
        rec.process().unwrap();
        assert_eq!(rec.oval, 2.0);
        rec.process().unwrap();
        assert_eq!(rec.oval, 3.0);
    }
}
