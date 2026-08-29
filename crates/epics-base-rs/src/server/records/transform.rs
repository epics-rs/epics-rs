use crate::error::{CaError, CaResult};
use crate::server::record::{
    AlarmSeverity, CyclePostMask, FieldMetadataOverride, ParsedLink, ProcessAction, ProcessContext,
    ProcessOutcome, Record, parse_link_v2, parse_output_link_v2,
};
use crate::types::EpicsValue;

use crate::calc::{CompiledExpr, StringInputs, scalc_compile, scalc_perform};

use super::link_status::{
    LINK_CON, LINK_STATUS_CHOICES, LinkRole, LinkStatusGen, post_link_status,
};
use crate::server::database::AsyncDbHandle;

const NUM_CHANNELS: usize = 16; // A-P

/// The sixteen channel value fields, in `A..P` order — the set C's `monitor()`
/// walks (`transformRecord.c:797-806`) and the only fields it posts.
const CHANNEL_FIELDS: &[&str] = &[
    "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P",
];

/// C `transformRecord.c:210` `#define COMMENT_SIZE 39` — the `CMTx` field width
/// (`transformRecord.dbd:681` `size(39)`). `getMacros` plants the terminator at
/// `[COMMENT_SIZE-1]` and scans the macro name with `j < COMMENT_SIZE-1`, so a
/// macro name is at most 38 bytes including its leading `$`.
const COMMENT_SIZE: usize = 39;

/// Code version reported by `VERS` (C `transformRecord.c:92 #define VERSION 5.8`).
const VERSION: f64 = 5.8;

/// C `transformRecord.c:752-767` `get_precision` names `VERS` and answers a
/// LITERAL 2, ahead of the `fieldIndex >= transformRecordVAL` arm that serves
/// `prec->prec` — so `caget -s T.VERS` reads `5.80` however PREC is set.
const TRANSFORM_VERS_PRECISION: i16 = 2;

/// Transform record — 16 input/output channels (A-P), each with its own calc expression.
///
/// Processing: reads inputs via INPA-INPP links, evaluates CLCA-CLCP expressions
/// (each can reference all 16 variables A-P), stores results back into A-P,
/// then writes outputs via OUTA-OUTP links.
pub struct TransformRecord {
    /// `VAL` — a dummy. C `transformRecord.c:422` sets `ptran->val = 0` once in
    /// `init_record` ("Gotta have a .val field.  Make its value reproducible.")
    /// and NOTHING else ever touches it: `process()` iterates the channels from
    /// `&ptran->a`, `monitor()` posts from `&ptran->a`, and the calc loop writes
    /// `&ptran->a + i` — `->val` is never read, written or posted. It is a plain
    /// writable DBF_DOUBLE (`transformRecord.dbd:43`, no `special`), so a client
    /// put stores here and a later `caget .VAL` reads it back; that is the
    /// field's entire behaviour. Aliasing VAL to channel A (the port's previous
    /// shape) made `caget .VAL` return A and fired a `.VAL` monitor on every A
    /// change — neither happens on C.
    pub val: f64,
    pub vals: [f64; NUM_CHANNELS],
    /// `LA..LP` — "Prev Value of A".."Prev Value of P"
    /// (`transformRecord.dbd:505-584`, DBF_DOUBLE, `special(SPC_NOMOD)`).
    ///
    /// C's `monitor()` (`transformRecord.c:797-806`) is the only writer: it
    /// posts each changed channel and copies the posted value into its `l*`
    /// cell, so once `monitor()` has run, `l* == *` for every channel. They
    /// diverge only between cycles — a client writing `A` on a non-Passive
    /// record, or a record that has never processed — and that window is what a
    /// `caget .LA` is FOR.
    ///
    /// Monitor posting stays on the framework's `last_posted`
    /// (`collect_subscriber_posts`). These cells are C's OTHER use of `l*`:
    /// they are the right-hand side of the `same` test that gates the
    /// conditional calc (`:575`, see `process`), so they must be read there and
    /// nowhere else derived.
    lvals: [f64; NUM_CHANNELS],
    pub calcs: [String; NUM_CHANNELS],
    compiled: [Option<CompiledExpr>; NUM_CHANNELS],
    /// `CMTA..CMTP` — "Comment A".."Comment P" (`transformRecord.dbd:681-741`,
    /// DBF_STRING `size(39)`, and note NO `special`).
    ///
    /// They are OPI labels, and a comment whose FIRST character is `$` also
    /// DEFINES A MACRO for the calc expressions: `getMacros`
    /// (`transformRecord.c:257-295`) takes `$` plus every non-space character
    /// that follows as the name and this channel's own letter as the
    /// replacement, so `CMTA="$in"` makes `$in` mean `A` in every CLCx. Without
    /// them the expression side has no macro source at all and a macro-using
    /// CLCx cannot compile.
    ///
    /// Because the `.dbd` gives them no `special`, a runtime put here does NOT
    /// recompile anything — C picks the new macro up at the next CLCx compile
    /// (`special`, `:682`), and so does [`Self::recompile`].
    cmts: [String; NUM_CHANNELS],
    pub inp_links: [String; NUM_CHANNELS],
    pub out_links: [String; NUM_CHANNELS],
    pub copt: i16, // calc option: 0=Conditional (calc only an unlinked, unchanged channel), 1=Always. Gates CALC-eval, NOT the OUTx write.
    pub ivla: i16, // 0=Ignore error, 1=Do Nothing
    pub prec: i16,
    /// C's `map` bitmap (`transformRecord.c:423`, `:584`, `:600`, `:703`) — one
    /// bit per channel, set by `special()` when a put lands on the value field
    /// `A..P` itself while the record is not processing. It is ONE of the two
    /// terms of C's `new_value`; the other is the `same` test against
    /// [`Self::lvals`]. It is NOT a synonym for either.
    map: [bool; NUM_CHANNELS],
    /// This cycle's pending input-link severity (`dbCommon.nsev`), pushed by
    /// the framework through [`Record::set_process_context`] before
    /// `process()` runs — C folds an MS-class link's severity into `nsev`
    /// inside `dbGetLink`, i.e. before the record body reads it
    /// (`transformRecord.c:554`).
    nsev: AlarmSeverity,
    /// `dbCommon.udf` as transform maintains it. C `transformRecord.c:524`
    /// clears `ptran->udf` at the top of every `process()` and sets it TRUE
    /// only where a channel's `sCalcPerform` fails (`:593-596`, alongside
    /// `recGblSetSevr(CALC_ALARM, INVALID_ALARM)`); `checkAlarms` (`:771-782`)
    /// then raises `UDF_ALARM` at `UDFS`. It is a per-cycle flag, not a
    /// property of any value — transform's VAL is an inert dummy (R9-62), so
    /// the framework's default `value_is_undefined()` (VAL is NaN) can never
    /// express it. Both [`Record::value_is_undefined`] and
    /// [`Record::check_alarms`] read this cell.
    calc_failed: bool,
    /// `IAV..IPV` / `OAV..OPV` — the per-channel link-connection status C
    /// derives in `init_record` (`transformRecord.c:444-473`) and re-derives in
    /// `special()` on any INPx/OUTx put (`:709-742`). Written only by
    /// [`Self::refresh_link_status`] (through `post_fields`), never by a
    /// client: the fields are `special(SPC_NOMOD)`.
    in_status: [i16; NUM_CHANNELS],
    out_status: [i16; NUM_CHANNELS],
    /// Async surface + generation gate for `refresh_link_status`, the shape
    /// calcout/scalcout/acalcout use (see `link_status::post_link_status`).
    /// sseq and swait own link-status fields too but post them from their own
    /// refresh, because their field sets are not one enum per link.
    async_ctx: Option<(String, AsyncDbHandle)>,
    link_gen: LinkStatusGen,
    /// C `transformRecord.c:807` `prpvt->firstCalcPosted`. `rpvt` is
    /// `calloc`'d in `init_record`, `monitor()` sets this to 1
    /// UNCONDITIONALLY after its post loop (`:807`), and nothing ever clears
    /// it — so it fires on exactly one process cycle per IOC lifetime, and it
    /// is the ONLY reason an unchanged channel is ever posted. C's
    /// `init_record` copies `*plvalue = *pvalue`, so without it the first
    /// cycle of a record nobody has written finds every channel unchanged and
    /// a `DBE_LOG` archiver takes no initial sample.
    first_calc_posted: bool,
}

impl Default for TransformRecord {
    fn default() -> Self {
        Self {
            val: 0.0,
            vals: [0.0; NUM_CHANNELS],
            lvals: [0.0; NUM_CHANNELS],
            calcs: Default::default(),
            compiled: Default::default(),
            cmts: Default::default(),
            inp_links: Default::default(),
            out_links: Default::default(),
            copt: 0,
            ivla: 0,
            prec: 0,
            map: [false; NUM_CHANNELS],
            nsev: AlarmSeverity::NoAlarm,
            calc_failed: false,
            // C's post-`init_record` value for a default record: every link is
            // CONSTANT, so every status is `transformIAV_CON`.
            in_status: [LINK_CON; NUM_CHANNELS],
            out_status: [LINK_CON; NUM_CHANNELS],
            async_ctx: None,
            link_gen: LinkStatusGen::default(),
            first_calc_posted: false,
        }
    }
}

impl TransformRecord {
    pub fn new() -> Self {
        Self::default()
    }

    /// C `transformRecord.c` compiles every CLCx with **sCalcPostfix**, not
    /// base's `postfix()`: `POSTFIX_SIZE` is `SCALC_INFIX_TO_POSTFIX_SIZE(...)`
    /// (`:208`) and the evaluator is `sCalcPerform` (`:593`). The two engines
    /// are not interchangeable — they have different element tables, and, the
    /// reason this matters here, different failure rules (see the eval site in
    /// `process`).
    ///
    /// C's `postfix_ok = *pclcbuf && (*prpcbuf != BAD_EXPRESSION)` (`:585`): an
    /// EMPTY CLCx is not compiled and not evaluated — which is why the empty
    /// case is `None` rather than sCalc's empty-but-valid program.
    fn recompile(&mut self, idx: usize) {
        if self.calcs[idx].is_empty() {
            self.compiled[idx] = None;
        } else {
            let converted = self.convert_expression(&self.calcs[idx]);
            self.compiled[idx] = scalc_compile(&converted).ok();
        }
    }

    /// C `convertExpression` (`transformRecord.c:384-390`): shortcuts first,
    /// then macros. The order is the point — `convertShortcuts` runs first
    /// precisely so that a user macro cannot shadow a built-in function
    /// spelling (`:228-238` says so at length).
    ///
    /// The macro table is rebuilt here rather than cached, which is what C
    /// does: `getMacros` runs at `:426` for the init-time compile and again at
    /// `:682` for the `special()` compile, and nothing else can invalidate a
    /// cached copy because CMTx carries no `special`.
    fn convert_expression(&self, src: &str) -> String {
        let expanded = convert_macros(&convert_shortcuts(src.as_bytes()), &self.macros());
        String::from_utf8_lossy(&expanded).into_owned()
    }

    /// C `getMacros` (`transformRecord.c:257-295`) followed by `sortMacros`
    /// (`:241-255`).
    ///
    /// A comment defines a macro only when its first character is `$`; the name
    /// then runs to the first whitespace, bounded by the field width, and the
    /// replacement is the channel's own letter. The sort is by DESCENDING name
    /// length so a longer name is tried before any shorter name it starts with
    /// — otherwise `$x` would eat the head of `$xy`. C's insertion sort moves
    /// only strictly-shorter entries, so equal lengths keep declaration order;
    /// `sort_by_key` is stable and does the same. C sorts all sixteen slots and
    /// stops the match loop at the first empty name; the empties sort last, so
    /// omitting them here is the same table.
    fn macros(&self) -> Vec<TransformMacro> {
        let mut macros: Vec<TransformMacro> = Vec::new();
        for (i, cmt) in self.cmts.iter().enumerate() {
            let bytes = cmt.as_bytes();
            if bytes.first() != Some(&b'$') {
                continue;
            }
            let mut end = 1;
            while end < COMMENT_SIZE - 1 && end < bytes.len() && !is_c_space(bytes[end]) {
                end += 1;
            }
            macros.push(TransformMacro {
                name: String::from_utf8_lossy(&bytes[..end]).into_owned(),
                letter: b'A' + i as u8,
            });
        }
        macros.sort_by_key(|m| std::cmp::Reverse(m.name.len()));
        macros
    }

    fn channel_index(name: &str) -> Option<usize> {
        if name.len() == 1 {
            let c = name.as_bytes()[0];
            if c >= b'A' && c <= b'P' {
                return Some((c - b'A') as usize);
            }
        }
        None
    }

    fn calc_field_index(name: &str) -> Option<usize> {
        if name.len() == 4 && name.starts_with("CLC") {
            let c = name.as_bytes()[3];
            if c >= b'A' && c <= b'P' {
                return Some((c - b'A') as usize);
            }
        }
        None
    }

    fn cmt_field_index(name: &str) -> Option<usize> {
        if name.len() == 4 && name.starts_with("CMT") {
            let c = name.as_bytes()[3];
            if c >= b'A' && c <= b'P' {
                return Some((c - b'A') as usize);
            }
        }
        None
    }

    fn inp_field_index(name: &str) -> Option<usize> {
        if name.len() == 4 && name.starts_with("INP") {
            let c = name.as_bytes()[3];
            if c >= b'A' && c <= b'P' {
                return Some((c - b'A') as usize);
            }
        }
        None
    }

    fn out_field_index(name: &str) -> Option<usize> {
        if name.len() == 4 && name.starts_with("OUT") {
            let c = name.as_bytes()[3];
            if c >= b'A' && c <= b'P' {
                return Some((c - b'A') as usize);
            }
        }
        None
    }

    /// C's link classification, `plink->type == CONSTANT` — the ONE test every
    /// one of this record's three link decisions is written in:
    /// `no_inlink` (`transformRecord.c:571`), "has an input link" (`:535`),
    /// "has an output link" (`:606`).
    ///
    /// A link field is CONSTANT when it is EMPTY *or* holds a literal number:
    /// dbStatic gives `field(INPA,"5")` the type CONSTANT, not PV_LINK. Reading
    /// the stored text's EMPTINESS instead (what the port did at all three
    /// sites) makes a constant-seeded channel look link-driven — and in the
    /// default Conditional mode its CLCx was then never evaluated at all:
    /// `field(INPA,"2")` + `field(CLCA,"A+1")` sat at 2 forever.
    fn link_is_constant(parsed: &ParsedLink) -> bool {
        matches!(parsed, ParsedLink::None | ParsedLink::Constant(_))
    }

    /// C's `no_inlink` (`transformRecord.c:571`).
    fn no_inlink(&self, i: usize) -> bool {
        Self::link_is_constant(&parse_link_v2(&self.inp_links[i]))
    }

    /// C's `if (plink->type != CONSTANT)` on OUTx (`transformRecord.c:606`),
    /// negated. An OUT field parses under the OUT modifier mask.
    fn no_outlink(&self, i: usize) -> bool {
        Self::link_is_constant(&parse_output_link_v2(&self.out_links[i]))
    }

    /// C's `same` test, `transformRecord.c:573-575`:
    ///
    /// ```c
    /// pu = (int *)pval;  plu = (int *)plval;
    /// same = (*pval==0. && *plval==0.) || ((pu[0] == plu[0]) && (pu[1] == plu[1]));
    /// ```
    ///
    /// A BIT-PATTERN comparison, not `==`, with one exception carved out. The
    /// two differ exactly where IEEE `==` is not reflexive-or-not-distinguishing:
    ///   * `-0.0` vs `0.0` — different bits, and the `==0.` clause exists to
    ///     call them the SAME anyway.
    ///   * `NaN` vs the same `NaN` — `==` says different, the bits say same, and
    ///     C takes the bits: a channel parked at NaN is not "new" every cycle.
    ///
    /// `f64::to_bits` is that `int[2]` view.
    fn value_is_same(cur: f64, last: f64) -> bool {
        (cur == 0.0 && last == 0.0) || cur.to_bits() == last.to_bits()
    }

    /// The input/output link-connection-status fields IAV..IPV / OAV..OPV
    /// (`transformRecord.dbd:766-989`, DBF_MENU `menu(transformIAV)`,
    /// `special(SPC_NOMOD)`). Names are `I<c>V` / `O<c>V` for channel `c` in
    /// A..P.
    ///
    /// Each is DERIVED from its link, never client-set: C `init_record`
    /// (`transformRecord.c:444-473`) and `special()` (`:709-742`) classify the
    /// link and store `transformIAV_CON` (=3) for every CONSTANT link, and a
    /// default record has all links constant. As with the sibling `acalcout`
    /// calc record, this port classifies the status STATICALLY (there is no
    /// live re-derivation on a link re-point), so every field reads
    /// `Constant`. The dbd `initial("1")` is C's pre-init placeholder that
    /// `init_record` overwrites — serving it raw (what `declared_default` did
    /// for these un-modeled fields) reported `Ext PV OK` where C reports
    /// `Constant`.
    fn link_status_index(name: &str) -> Option<(LinkRole, usize)> {
        let b = name.as_bytes();
        if name.len() != 3 || b[2] != b'V' || !(b'A'..=b'P').contains(&b[1]) {
            return None;
        }
        let role = match b[0] {
            b'I' => LinkRole::Input,
            b'O' => LinkRole::Output,
            _ => return None,
        };
        Some((role, (b[1] - b'A') as usize))
    }

    fn is_link_status_field(name: &str) -> bool {
        Self::link_status_index(name).is_some()
    }

    /// Classify all 32 links and publish `IAV..IPV` / `OAV..OPV`, mirroring C
    /// `transformRecord.c::init_record` (444-473) and the `special()`
    /// re-classification (709-742). No-op without an async context.
    fn refresh_link_status(&self) {
        let mut links: Vec<(&'static str, String, LinkRole)> = Vec::with_capacity(2 * NUM_CHANNELS);
        for i in 0..NUM_CHANNELS {
            links.push((
                IAV_FIELD_NAMES[i],
                self.inp_links[i].clone(),
                LinkRole::Input,
            ));
            links.push((
                OAV_FIELD_NAMES[i],
                self.out_links[i].clone(),
                LinkRole::Output,
            ));
        }
        // C `transformRecord.c:741`, `:858`, `:863`.
        post_link_status(
            self.async_ctx.as_ref(),
            &self.link_gen,
            links,
            crate::server::recgbl::EventMask::VALUE | crate::server::recgbl::EventMask::LOG,
        );
    }

    /// LA..LP — "Prev Value of A".."Prev Value of P"
    /// (`transformRecord.dbd:505-584`).
    fn last_value_index(name: &str) -> Option<usize> {
        if name.len() == 2 && name.as_bytes()[0] == b'L' {
            let c = name.as_bytes()[1];
            if c.is_ascii_uppercase() && c <= b'P' {
                return Some((c - b'A') as usize);
            }
        }
        None
    }
}

/// One `CMTx`-defined macro — C `struct macro` (`transformRecord.c:215-218`):
/// the name (`$` plus the comment's leading non-space run) and the single
/// character it expands to, which is the defining channel's own letter.
struct TransformMacro {
    name: String,
    letter: u8,
}

/// C's `isspace()` in the "C" locale, which is what `getMacros` (`:276`) ends
/// the macro name on: space and the five control characters `\t`..`\r`.
fn is_c_space(b: u8) -> bool {
    b == b' ' || (b'\t'..=b'\r').contains(&b)
}

/// C `shortcuts[]` (`transformRecord.c:333-343`), applied before macros so a
/// user macro can never shadow a built-in function spelling. The six rows are
/// `:337-342` — `$P(`, `$T(`, `$W(`, `$S(`, `$R(`, `$E(` — inside the
/// `struct shortcut ... shortcuts[NUMSHORTCUTS] = {` opening at `:333-336` and
/// the `};` at `:343`. (`cdbcd708`, which widened this citation from the
/// earlier `:333-341`, said that range dropped two rows, `$R(` and `$E(`. It
/// dropped one row and the terminator: `:341` IS `{"$R(", "$READ("},`, so what
/// fell outside was `$E(` at `:342` and the closing `};` at `:343`.)
///
/// **One deliberate deviation, and it is the whole reason this comment is
/// long.** C's replacements carry a leading `$` — `{"$P(", "$PRINTF("}` — and
/// `sCalcPostfix` has no `$PRINTF` element: it carries `$P` and `PRINTF`
/// separately, and its scan takes the longest table symbol prefixing the text
/// (`sCalcPostfix.c:272-280`, walking the table backwards), so `$PRINTF(`
/// lexes as `$P` followed by the unknown `RINTF`. Measured against the real
/// `libcalc` on this host: `$S("7","%d")` compiles (status 0),
/// `$SSCANF("7","%d")` is status -1 error 11 `CALC_ERR_SYNTAX`, and the same
/// pair holds for `$P`/`$PRINTF`, `$E`/`$ESC` and `$W`/`$WRITE`. So on C every
/// shortcut this table expands becomes an expression that cannot compile, and
/// the channel is silently never evaluated.
///
/// The port drops the `$` from the replacement. `PRINTF` and `$P` are the SAME
/// element (`sCalcPostfix.c:173-174`), so a working expression keeps working
/// and keeps its meaning, while the ordering the table exists for still holds:
/// the shortcut is consumed before `convert_macros` runs, and the result
/// carries no `$` for a macro to match.
const SHORTCUTS: [(&str, &str); 6] = [
    ("$P(", "PRINTF("),
    ("$T(", "TR_ESC("),
    ("$W(", "WRITE("),
    ("$S(", "SSCANF("),
    ("$R(", "READ("),
    ("$E(", "ESC("),
];

/// C `convertShortcuts` (`:370-381`) — every entry of [`SHORTCUTS`] applied in
/// table order.
fn convert_shortcuts(src: &[u8]) -> Vec<u8> {
    SHORTCUTS
        .iter()
        .fold(src.to_vec(), |acc, (target, replacement)| {
            replace_ci(&acc, target.as_bytes(), replacement.as_bytes())
        })
}

/// C `convertShortcut` (`:345-368`): a case-insensitive (`epicsStrnCaseCmp`)
/// literal scan-and-replace across the whole expression, quoted string
/// literals included — C draws no such boundary.
fn replace_ci(src: &[u8], target: &[u8], replacement: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len());
    let mut i = 0;
    while i < src.len() {
        if src.len() - i >= target.len() && src[i..i + target.len()].eq_ignore_ascii_case(target) {
            out.extend_from_slice(replacement);
            i += target.len();
        } else {
            out.push(src[i]);
            i += 1;
        }
    }
    out
}

/// C `convertMacros` (`:297-329`). At a `$` the sorted table is walked and each
/// entry matching at the cursor — case-insensitively, longest name first — is
/// consumed in turn; anything else is copied one byte and the scan advances.
fn convert_macros(src: &[u8], macros: &[TransformMacro]) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len());
    let mut i = 0;
    while i < src.len() {
        let mut taken = false;
        if src[i] == b'$' {
            for m in macros {
                let name = m.name.as_bytes();
                if i < src.len()
                    && src.len() - i >= name.len()
                    && src[i..i + name.len()].eq_ignore_ascii_case(name)
                {
                    out.push(m.letter);
                    i += name.len();
                    taken = true;
                }
            }
        }
        if !taken {
            out.push(src[i]);
            i += 1;
        }
    }
    out
}

/// Choice labels for the calculation-option menu, in index order.
/// C `menu(transformCOPT)` (synApps `transformRecord.dbd`): 0=Conditional
/// (only recompute outputs whose inputs changed), 1=Always.
const TRANSFORM_COPT_CHOICES: &[&str] = &["Conditional", "Always"];

/// Choice labels for the invalid-link-action menu, in index order.
/// C `menu(transformIVLA)` (synApps `transformRecord.dbd`): 0="Ignore
/// error", 1="Do Nothing".
const TRANSFORM_IVLA_CHOICES: &[&str] = &["Ignore error", "Do Nothing"];

impl Record for TransformRecord {
    /// The one field C's `get_precision` names. Every other field takes
    /// `prec->prec` through `route_field_metadata`, which is the generic seed
    /// arm — a literal cannot come from there.
    fn field_metadata_override(&self, field: &str) -> Option<FieldMetadataOverride> {
        field
            .eq_ignore_ascii_case("VERS")
            .then(|| FieldMetadataOverride {
                precision: Some(TRANSFORM_VERS_PRECISION),
                ..Default::default()
            })
    }

    fn record_type(&self) -> &'static str {
        "transform"
    }

    /// C compiles every CLCx in `init_record`, AFTER the whole record has
    /// loaded and after one `getMacros` over the final CMTx set
    /// (`transformRecord.c:426`, then `convertExpression`/`sCalcPostfix` per
    /// channel at `:481-482`). `put_field` compiling on the way in — the port's
    /// stand-in for C's `special()` — is correct for a runtime put but makes
    /// the LOAD order-dependent: a `.db` naming `CLCB` above `CMTA` would
    /// compile the expression before its macro exists. Re-establishing the
    /// whole set here, once, is what makes the order irrelevant, as it is on C.
    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 1 {
            for i in 0..NUM_CHANNELS {
                self.recompile(i);
            }
        }
        Ok(())
    }

    /// The link-status menus (IAV..IPV, OAV..OPV) are served read-only by
    /// `get_field`, but the record owns no WRITE path for them — they are
    /// `SPC_NOMOD`, derived from the link (see `Self::is_link_status_field`).
    /// The loader's `.dbd`-initial seed and `.db field()` apply both key on this
    /// predicate to decide whether to WRITE a field; answering `false` keeps
    /// them from storing the `.dbd` `initial("1")` over the init-derived
    /// `Constant`. The read is unaffected: `resolve_field` consults `get_field`
    /// independently.
    fn implements_field(&self, name: &str) -> bool {
        if Self::is_link_status_field(name) {
            return false;
        }
        self.get_field(name).is_some()
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // C `transformRecord.c:524` `ptran->udf = FALSE;` — the very first
        // thing every cycle does, including the IVLA-abandoned one below (C
        // clears it before the test), so the flag only ever reports THIS
        // cycle's calc failures.
        self.calc_failed = false;

        // IVLA="Do Nothing" + an INVALID input severity: C
        // `transformRecord.c:554-560` abandons the WHOLE cycle —
        //
        //   if ((ptran->nsev >= INVALID_ALARM) && (ptran->ivla == transformIVLA_DO_NOTHING)) {
        //       recGblGetTimeStamp(ptran); checkAlarms(ptran);
        //       recGblResetAlarms(ptran); ptran->pact = FALSE; return (0);
        //   }
        //
        // — no calc for ANY channel, none of the 16 OUTx `dbPutLink` writes,
        // no `monitor()`, no `recGblFwdLink()`. Only the timestamp and the
        // alarm commit run, which is exactly `CompleteAlarmOnly`. The input
        // links have already been read into A..P by this point in C (the fetch
        // loop precedes this test), and the framework's multi-input apply is
        // likewise already done, so the channels carry the fresh input values;
        // they are simply not published, calculated on, or driven out.
        //
        // `nsev` is the framework's pending severity for THIS cycle, folded
        // from the MS-class input links before `process()` — the same cell C
        // reads. IVLA is NOT a per-channel calc-failure policy: C never
        // restores a channel's previous value on a calc error (see the eval
        // arm below), so the port's old per-channel value-restore was invented
        // behaviour and is gone.
        if self.nsev >= AlarmSeverity::Invalid && self.ivla == 1 {
            return Ok(ProcessOutcome::complete_alarm_only());
        }

        // Snapshot and clear C's `map` bitmap for this cycle (`:600` clears it
        // after the calc loop; the IVLA abandon above returns before `:600`, so
        // a mark set during an abandoned cycle SURVIVES into the next — hence
        // the take sits below that return, not above it).
        let map = std::mem::take(&mut self.map);

        // Evaluate each calc expression A-P. synApps `transformRecord.c:584-598`:
        //
        //   new_value   = (!same || ((ptran->map & (1<<i)) != 0));
        //   postfix_ok  = *pclcbuf && (*prpcbuf != BAD_EXPRESSION);
        //   if (((no_inlink && !new_value) || ptran->copt==transformCOPT_ALWAYS)
        //           && postfix_ok) { ... sCalcPerform ... }
        //
        // COPT decides whether CLCx is EVALUATED — it does NOT gate the OUTx
        // write below.
        //   Conditional (COPT=0): compute a channel only when it has NO input
        //     link AND its value is NOT new. "New" is the union of two
        //     independent facts, and BOTH are load-bearing:
        //       - the `map` bit: a put landed on the value field itself, or
        //       - `!same`: the channel's value differs from the LA..LP cell C's
        //         `monitor()` left behind — which catches every write that does
        //         NOT go through `special()`, notably the CONSTANT-INPx re-seed
        //         (`:718`, R19-1) and a store opcode in a SIBLING channel's
        //         expression (`CLCB="A:=A+1"` writes A through `&ptran->a`).
        //     A channel holding such a value keeps it for one cycle instead of
        //     being overwritten by its own CLCx.
        //   Always (COPT=1): compute whenever CLCx is valid, regardless.
        // `postfix_ok` is the `if let Some(compiled)` below: `recompile` leaves
        // `None` for both of C's failing halves — an empty CLCx (never compiled)
        // and one `sCalcPostfix` rejected (BAD_EXPRESSION).
        for i in 0..NUM_CHANNELS {
            let new_value = !Self::value_is_same(self.vals[i], self.lvals[i]) || map[i];
            let do_calc = (self.no_inlink(i) && !new_value) || self.copt == 1;
            if !do_calc {
                continue;
            }
            if let Some(ref compiled) = self.compiled[i] {
                // C `transformRecord.c:593`:
                //
                //   sCalcPerform(&ptran->a, 16, NULL, 0, pval, NULL, 0, prpcbuf, ptran->prec)
                //
                // — the sCalc engine, with the record's sixteen channels as the
                // numeric args and no string args. NOT base's `calcPerform`.
                // The engines differ in the rule that decides this record's
                // alarm: `sCalcPerform` ends with
                //
                //   return (((isnan(*presult)||isinf(*presult)) ? -1 : 0));   // :2056
                //
                // so a non-finite result — `1e308*10` → +inf, `ACOS(2)` → NaN —
                // is a FAILURE, and `:593-596` turns it into CALC_ALARM/INVALID
                // + udf. Base's `calcPerform` has no such check and returns 0
                // with the infinity in hand, which is what the port used to do:
                // `CLCx = "1/0"` yielded `inf` with NO_ALARM.
                //
                // But the failing status does NOT cancel the write: C's epilogue
                // stores `*presult` and only THEN returns -1, so the channel
                // KEEPS the inf/NaN and `:594-595` alarms beside it — and the
                // OUTx loop below fans that non-finite value out. Only the
                // OTHER -1 (an operator refusing outright: `1/0`, `SQRT(-1)`)
                // leaves the channel untouched, because C returns before the
                // epilogue runs. [`ScalcResult`] carries both halves so the two
                // cannot be confused.
                // C `sCalcPerform(&ptran->a, 16, NULL, 0, pval, NULL, 0, ...)`
                // (`transformRecord.c:593`): SIXTEEN numeric args — A..P, the
                // record's channels — and ZERO string args, with a NULL `psarg` to
                // match. transform has no string fields at all, so every guard on
                // `numSArgs` refuses: `AA` reads as the empty string and `AA:=`
                // stores nowhere. The count travels with the args ([`StringInputs`])
                // so the engine cannot reach a field this record does not have.
                let mut inputs = StringInputs::with_counts(NUM_CHANNELS, 0);
                inputs.num_vars[..NUM_CHANNELS].copy_from_slice(&self.vals);
                // `pval = &ptran->a + i` (`:564`, `:569`) is C's `presult`, and
                // the `VAL` token (`FETCH_VAL`) pushes `*presult` — *this
                // channel's* current value, not a record-wide previous VAL.
                inputs.prev_val = self.vals[i];
                let outcome = scalc_perform(compiled, &mut inputs, self.prec);
                // C's store opcodes write through `&ptran->a`
                // (`sCalcPerform.c:429-433`), so `CLCB="A:=A+1"` mutates the
                // record's A — before channel C's expression fetches it, and
                // whether or not this channel's perform then failed. The engine
                // works on an owned copy, so the copy is landed back here, ahead
                // of the `*presult` write below (C's epilogue is last).
                self.vals[..NUM_CHANNELS].copy_from_slice(&inputs.num_vars[..NUM_CHANNELS]);
                match outcome {
                    Ok(result) => {
                        // C's epilogue with `psresult == NULL`: `*presult` takes
                        // the double, coercing a string result through `atof`.
                        // transform never consumes the SVAL half, so PREC plays
                        // no part here (C passes `ptran->prec` but a NULL
                        // `psresult` makes it moot) — the `val` half is the
                        // whole of `*presult`.
                        //
                        // Written FIRST, unconditionally: a non-finite result is
                        // stored in the channel and THEN alarmed
                        // (`transformRecord.c:593-596` keeps `*pval` = inf and
                        // fans it through OUTx).
                        self.vals[i] = result.val;
                        if result.non_finite {
                            self.calc_failed = true;
                        }
                    }
                    Err(_) => {
                        // C `transformRecord.c:593-596`:
                        //
                        //   if (sCalcPerform(...)) {
                        //       recGblSetSevr(ptran, CALC_ALARM, INVALID_ALARM);
                        //       ptran->udf = TRUE;
                        //   }
                        //
                        // This is the -1 an operator raised BEFORE the epilogue,
                        // so `*pval` is left untouched and the loop continues
                        // with the next channel. The severity is raised by
                        // `check_alarms` below (the framework's `checkAlarms`
                        // slot) off this flag — the same flag the non-finite
                        // status above sets, because C tests one `if` for both.
                        // IVLA plays no part here — it gates the whole cycle on
                        // the INPUT severity (see the top of `process`), never a
                        // single channel's calc.
                        self.calc_failed = true;
                    }
                }
            }
        }

        // Write every channel with a non-constant OUTx, UNCONDITIONALLY.
        // synApps `transformRecord.c` consults COPT only for calc-eval
        // (above); its output loop writes every `plink->type != CONSTANT`
        // OUTx each process, COPT untouched. The classic INPx -> A -> OUTx
        // passthrough / fan-out (empty CLCx) must therefore drive its OUTx
        // even in the default Conditional mode; the prior COPT/CLCx gate
        // here silently dropped it.
        let mut actions = Vec::new();
        for i in 0..NUM_CHANNELS {
            if self.no_outlink(i) {
                continue;
            }
            actions.push(ProcessAction::WriteDbLink {
                link_field: OUT_FIELD_NAMES[i],
                value: EpicsValue::Double(self.vals[i]),
            });
        }

        // C `monitor()` (`transformRecord.c:797-806`): every channel it posts,
        // it copies into the channel's `l*` cell — and it posts exactly the
        // channels that differ from it. So the state `monitor()` leaves behind
        // is `l* == *` for ALL channels, which is this one assignment. It is
        // deliberately NOT reached by the IVLA "Do Nothing" early return above:
        // that path (`:554-560`) runs checkAlarms/recGblResetAlarms and returns
        // WITHOUT calling `monitor()`, so LA..LP keep the values from the last
        // cycle that did.
        self.lvals = self.vals;

        Ok(ProcessOutcome::complete_with(actions))
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        if name == "VAL" {
            // The dummy result field — never written by process()/monitor().
            return Some(EpicsValue::Double(self.val));
        }
        if name == "COPT" {
            return Some(EpicsValue::Short(self.copt));
        }
        if name == "IVLA" {
            return Some(EpicsValue::Short(self.ivla));
        }
        if name == "PREC" {
            return Some(EpicsValue::Short(self.prec));
        }
        if name == "VERS" {
            // Fixed code-version constant, C `ptran->vers = VERSION` in
            // init_record; never the `.dbd` initial.
            return Some(EpicsValue::Double(VERSION));
        }
        if let Some(idx) = Self::channel_index(name) {
            return Some(EpicsValue::Double(self.vals[idx]));
        }
        if let Some(idx) = Self::calc_field_index(name) {
            return Some(EpicsValue::String(self.calcs[idx].clone().into()));
        }
        if let Some(idx) = Self::cmt_field_index(name) {
            return Some(EpicsValue::String(self.cmts[idx].clone().into()));
        }
        if let Some(idx) = Self::inp_field_index(name) {
            return Some(EpicsValue::String(self.inp_links[idx].clone().into()));
        }
        if let Some(idx) = Self::out_field_index(name) {
            return Some(EpicsValue::String(self.out_links[idx].clone().into()));
        }
        if let Some(idx) = Self::last_value_index(name) {
            return Some(EpicsValue::Double(self.lvals[idx]));
        }
        if let Some((role, idx)) = Self::link_status_index(name) {
            let status = match role {
                LinkRole::Input => self.in_status[idx],
                LinkRole::Output => self.out_status[idx],
            };
            return Some(EpicsValue::Enum(status as u16));
        }
        None
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        if name == "VAL" {
            // Stored and readable back, but inert: no calc, output link or
            // monitor consumes it (C `transformRecord.c` never reads `->val`).
            self.val = value
                .to_f64()
                .ok_or_else(|| CaError::TypeMismatch("VAL".into()))?;
            return Ok(());
        }
        if name == "COPT" {
            match value {
                EpicsValue::Short(v) => {
                    self.copt = v;
                    return Ok(());
                }
                _ => return Err(CaError::TypeMismatch("COPT".into())),
            }
        }
        if name == "IVLA" {
            match value {
                EpicsValue::Short(v) => {
                    self.ivla = v;
                    return Ok(());
                }
                _ => return Err(CaError::TypeMismatch("IVLA".into())),
            }
        }
        if name == "PREC" {
            match value {
                EpicsValue::Short(v) => {
                    self.prec = v;
                    return Ok(());
                }
                _ => return Err(CaError::TypeMismatch("PREC".into())),
            }
        }
        if name == "VERS" {
            // VERS is a fixed code-version constant; accept and ignore writes.
            return Ok(());
        }
        if let Some(idx) = Self::channel_index(name) {
            self.vals[idx] = value
                .to_f64()
                .ok_or_else(|| CaError::TypeMismatch(name.into()))?;
            return Ok(());
        }
        if let Some(idx) = Self::calc_field_index(name) {
            match value {
                EpicsValue::String(s) => {
                    self.calcs[idx] = s.as_str_lossy().into_owned();
                    self.recompile(idx);
                    return Ok(());
                }
                _ => return Err(CaError::TypeMismatch(name.into())),
            }
        }
        if let Some(idx) = Self::cmt_field_index(name) {
            match value {
                EpicsValue::String(s) => {
                    // Stored only. CMTx has no `special` in the `.dbd`, so C
                    // does not recompile any CLCx here either — the new macro
                    // reaches the programs at the next compile.
                    self.cmts[idx] = s.as_str_lossy().into_owned();
                    return Ok(());
                }
                _ => return Err(CaError::TypeMismatch(name.into())),
            }
        }
        if let Some(idx) = Self::inp_field_index(name) {
            match value {
                EpicsValue::String(s) => {
                    self.inp_links[idx] = s.as_str_lossy().into_owned();
                    return Ok(());
                }
                _ => return Err(CaError::TypeMismatch(name.into())),
            }
        }
        if let Some(idx) = Self::out_field_index(name) {
            match value {
                EpicsValue::String(s) => {
                    self.out_links[idx] = s.as_str_lossy().into_owned();
                    return Ok(());
                }
                _ => return Err(CaError::TypeMismatch(name.into())),
            }
        }
        // IAV..IPV / OAV..OPV are `special(SPC_NOMOD)` to clients; this arm
        // exists for the link-status refresh (`post_fields` ->
        // `put_field_internal`), which is their only writer.
        if let Some((role, idx)) = Self::link_status_index(name) {
            let status = value
                .to_f64()
                .ok_or_else(|| CaError::TypeMismatch(name.into()))? as i16;
            match role {
                LinkRole::Input => self.in_status[idx] = status,
                LinkRole::Output => self.out_status[idx] = status,
            }
            return Ok(());
        }
        Err(CaError::FieldNotFound(name.to_string()))
    }

    fn set_async_context(&mut self, name: String, db: AsyncDbHandle) {
        self.async_ctx = Some((name, db));
        // C `init_record` (transformRecord.c:444-473) classifies all 32 links
        // before any process. Both link tables are transform-owned fields,
        // already applied by the time `add_record` runs, so one refresh here
        // covers what C's init loop does.
        self.refresh_link_status();
    }

    /// S5 — set C's `map` bit for a value channel `A..P` written by an
    /// *external* put, so the next `process()` does not overwrite it with the
    /// channel's own CLCx. The framework calls `special(field, true)` only on
    /// the CA / database-access put path (`field_io.rs`); the
    /// multi-input-link propagation (`processing.rs`) writes A..P via
    /// `put_field` directly *without* `special()`, which is C's shape too — a
    /// link-fed channel has `no_inlink == false` and is never gated by
    /// `new_value` at all.
    fn special(&mut self, field: &str, after: bool) -> CaResult<()> {
        if after {
            // C `transformRecord.c:699-705` marks the bitmap only for a field
            // in the `A..P` range (`i = fieldIndex - transformRecordA; if ((i
            // >= 0) && (i < MAX_FIELDS))`). VAL sits below `transformRecordA`,
            // so a put to VAL marks nothing — it is not a channel.
            if let Some(i) = Self::channel_index(field) {
                self.map[i] = true;
            }
            // C `transformRecord.c:709-742` re-classifies the link a put just
            // re-pointed, over the whole INPA..OUTP range.
            if Self::inp_field_index(field).is_some() || Self::out_field_index(field).is_some() {
                self.refresh_link_status();
            }
        }
        Ok(())
    }

    /// C's `init_record` TAIL for this record: `*plvalue = *pvalue`
    /// (`transformRecord.c:490`), run per channel after the CONSTANT-INPx seed
    /// two lines earlier (`:445`). LA..LP is transform's tracking cell, and
    /// this hook is the framework's owner of "re-derive init-time tracking
    /// state from the value the seed just loaded" — it runs at the tail of
    /// `seed_constant_links`, i.e. exactly where C's line sits.
    ///
    /// It is load-bearing, not cosmetic: `process` tests `same(vals, lvals)`.
    /// Without this, `field(A,"5")` would leave LA=0, make A read "new" on the
    /// very first cycle, and suppress the calc C runs. transform serves no
    /// MLST/ALST/LALM, so there is nothing of the default's work to keep.
    fn seed_deadband_tracking(&mut self) {
        self.lvals = self.vals;
    }

    /// Adopt the framework's per-cycle `dbCommon` snapshot. `nsev` — this
    /// cycle's pending severity, already carrying every MS-class input link's
    /// alarm — is what C `transformRecord.c:554` tests against `IVLA`.
    fn set_process_context(&mut self, ctx: &ProcessContext) {
        self.nsev = ctx.nsev;
    }

    /// A channel whose INPx link failed to read is ZEROED. C
    /// `transformRecord.c:537-541`, in the input loop:
    ///
    /// ```c
    /// if (plink->type != CONSTANT) {
    ///     status = dbGetLink(plink, DBR_DOUBLE, pval, NULL, NULL);
    ///     if (!RTN_SUCCESS(status)) { *pval = 0.; }
    /// }
    /// ```
    ///
    /// This is transform-specific: `calcRecord.c::fetch_values` (427-443)
    /// leaves `*pvalue` at its stale value on the same failure, and so do
    /// sub/sel/swait. So the zeroing lives here, not in the framework's shared
    /// multi-input apply.
    ///
    /// The framework reports the links that produced a value this cycle; a
    /// channel is zeroed when its link is CONFIGURED (non-empty — C's `type !=
    /// CONSTANT`) yet absent from that list. An unset channel is C's CONSTANT
    /// link: not read, not zeroed. A constant-valued link ("5") always
    /// resolves, so it never reaches the zeroing branch either.
    ///
    /// Runs before `process()` (the framework's report point), which is where C
    /// does it — the zero is what the calc loop and the OUTx write then see.
    fn set_resolved_input_links(&mut self, resolved: &[&'static str]) {
        for i in 0..NUM_CHANNELS {
            if !self.no_inlink(i) && !resolved.contains(&INP_FIELD_NAMES[i]) {
                self.vals[i] = 0.0;
            }
        }
    }

    /// C `transformRecord.c:593-596`: a channel whose `sCalcPerform` failed
    /// raises `recGblSetSevr(ptran, CALC_ALARM, INVALID_ALARM)`. Raised from
    /// the `checkAlarms` slot, which the framework runs BEFORE
    /// `rec_gbl_check_udf` — so on a calc failure CALC_ALARM lands first and
    /// the equal-severity UDF_ALARM (`checkAlarms`, `:771-782`) cannot displace
    /// it under `rec_gbl_set_sevr`'s strict-greater rule. Same order, same
    /// outcome as C.
    fn check_alarms(&mut self, common: &mut crate::server::record::CommonFields) {
        if self.calc_failed {
            crate::server::recgbl::rec_gbl_set_sevr(
                common,
                crate::server::recgbl::alarm_status::CALC_ALARM,
                AlarmSeverity::Invalid,
            );
        }
    }

    /// C `transformRecord.c:793-794` throws away `recGblResetAlarms`'s mask —
    /// it assigns `monitor_mask = DBE_VALUE|DBE_LOG` over it — and the A..P
    /// change loop (:797-806) posts every one of the sixteen value fields with
    /// that literal. No transform field ever carries an alarm bit, so a
    /// `DBE_ALARM`-only subscriber on `.A` is notified on no cycle at all.
    fn fields_posted_without_alarm_bits(&self) -> &'static [&'static str] {
        CHANNEL_FIELDS
    }

    /// C `transformRecord.c::monitor()` (`:786-809`) is the record's ONLY
    /// `db_post_events` caller, and it posts exactly the sixteen channels
    /// A..P — the changed ones (plus every one of them on the first post).
    /// Nothing else. In particular it does NOT post `VAL`: transform's VAL is
    /// an inert dummy that `init_record` zeroes once (`:422`, *"Gotta have a
    /// .val field"*) and no other line of the record reads, writes or posts.
    ///
    /// Declaring the closed set is what stops the framework from inventing a
    /// `.VAL` monitor: the deadband post fires whenever ANY class fired, and
    /// on an alarm cycle the alarm bits alone are enough — so a transform
    /// whose input went INVALID was posting `.VAL` where C posts nothing.
    fn process_posted_fields(&self) -> Option<&'static [&'static str]> {
        Some(CHANNEL_FIELDS)
    }

    /// The `firstCalcPosted == 0` half of C's post condition
    /// (`transformRecord.c:798`): on the first process cycle of the IOC every
    /// channel posts whether or not it moved, and the flag is set
    /// unconditionally afterwards (`:807`) so no later cycle can.
    ///
    /// `take_first_monitor_cycle`, not `take_cycle_posted_fields`: iocInit
    /// drains the per-cycle marks and would consume this flag before any
    /// process cycle ran. Both are called once per process cycle by
    /// `collect_subscriber_posts`, subscribers or not, exactly as C runs
    /// `monitor()` every cycle — so a client that connects after the record
    /// has processed sees what C shows it, nothing. The mask is C's own
    /// `monitor_mask = DBE_VALUE|DBE_LOG` (`:794`), which overwrites
    /// `recGblResetAlarms`'s: no alarm bit reaches these posts.
    fn take_first_monitor_cycle(&mut self) -> Vec<(&'static str, CyclePostMask)> {
        if self.first_calc_posted {
            return Vec::new();
        }
        self.first_calc_posted = true;
        CHANNEL_FIELDS
            .iter()
            .map(|f| (*f, CyclePostMask::ValueLog))
            .collect()
    }

    /// Transform's UDF is C's `ptran->udf`: cleared at the top of every
    /// `process()` and set only by a failing channel calc. It is NOT derived
    /// from VAL — VAL is an inert dummy (R9-62).
    fn value_is_undefined(&self) -> bool {
        self.calc_failed
    }

    /// C `transformRecord.c:445`: every CONSTANT input link is loaded into its value
    /// field ONCE, at `init_record` (`recGblInitConstantLink(plink,
    /// DBF_DOUBLE, pvalue)`); `dbGetLink` then delivers nothing for it on
    /// every later process, so a client's `caput REC.A 99` stands.
    fn constant_init_links(&self) -> Vec<crate::server::record::ConstantInitLink> {
        crate::server::record::seed_input_links(self.multi_input_links())
    }

    /// C `transformRecord.c::special` (715-723) — a runtime put to an INPn that
    /// leaves the link CONSTANT re-runs `recGblInitConstantLink(plink,
    /// DBF_DOUBLE, pvalue)` and posts the value field. C guards it with
    /// `if (fieldIndex < transformRecordOUTA)`: the OUT half of the same
    /// `&ptran->inpa + i` sweep is not an input and gets only `IAV/OAV = CON`.
    /// `multi_input_links` IS that input half.
    fn special_reseed_input_links(&self) -> &[(&'static str, &'static str)] {
        self.multi_input_links()
    }

    /// C `transformRecord.c:719` posts the re-seeded value with
    /// `DBE_VALUE | DBE_LOG` — unlike the calcout family's bare `DBE_VALUE`.
    fn special_reseed_post_mask(&self) -> crate::server::recgbl::EventMask {
        crate::server::recgbl::EventMask::VALUE | crate::server::recgbl::EventMask::LOG
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
        ]
    }

    /// Record-specific `DBF_MENU` fields, served as `DBR_ENUM` with the
    /// menu's choice labels in `.dbd` index order (`transformRecord.dbd`):
    /// `COPT` is `menu(transformCOPT)`, `IVLA` is `menu(transformIVLA)`.
    fn menu_field_choices(&self, field: &str) -> Option<&'static [&'static str]> {
        match field {
            "COPT" => Some(TRANSFORM_COPT_CHOICES),
            "IVLA" => Some(TRANSFORM_IVLA_CHOICES),
            _ if Self::is_link_status_field(field) => Some(LINK_STATUS_CHOICES),
            _ => None,
        }
    }
}

/// `IAV..IPV` — input-link status field names, indexed by channel 0..15.
static IAV_FIELD_NAMES: [&str; NUM_CHANNELS] = [
    "IAV", "IBV", "ICV", "IDV", "IEV", "IFV", "IGV", "IHV", "IIV", "IJV", "IKV", "ILV", "IMV",
    "INV", "IOV", "IPV",
];

/// `OAV..OPV` — output-link status field names, indexed by channel 0..15.
static OAV_FIELD_NAMES: [&str; NUM_CHANNELS] = [
    "OAV", "OBV", "OCV", "ODV", "OEV", "OFV", "OGV", "OHV", "OIV", "OJV", "OKV", "OLV", "OMV",
    "ONV", "OOV", "OPV",
];

/// OUTA..OUTP field names, indexed by channel 0..15. Used by
/// `process()` to name the per-channel OUT link for a `WriteDbLink`
/// action — `ProcessAction::link_field` requires a `&'static str`.
static OUT_FIELD_NAMES: [&str; NUM_CHANNELS] = [
    "OUTA", "OUTB", "OUTC", "OUTD", "OUTE", "OUTF", "OUTG", "OUTH", "OUTI", "OUTJ", "OUTK", "OUTL",
    "OUTM", "OUTN", "OUTO", "OUTP",
];

/// INPA..INPP field names, indexed by channel 0..15 — the link-field spelling
/// the framework reports back through [`Record::set_resolved_input_links`].
static INP_FIELD_NAMES: [&str; NUM_CHANNELS] = [
    "INPA", "INPB", "INPC", "INPD", "INPE", "INPF", "INPG", "INPH", "INPI", "INPJ", "INPK", "INPL",
    "INPM", "INPN", "INPO", "INPP",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::record::CommonFields;
    use crate::server::record::FieldDeclaration;

    #[test]
    fn link_status_fields_read_derived_constant_and_reject_put() {
        use crate::server::record::RecordInstance;
        // IAV..IPV / OAV..OPV are `special(SPC_NOMOD)`, DERIVED from the link. A
        // default record's links are all constant, so C reports `Constant`(3).
        // Before this fix these fields were declared but un-modeled, so
        // `resolve_field` fell through to `declared_default` and served the dbd
        // `initial("1")` (`Ext PV OK`) — the C-parity divergence the oracle
        // measured (`caget TRANSFORM.IAV` → 1, C → 3). The put is already
        // refused by the framework read-only gate (`is_no_mod`), matching C
        // `S_db_noMod`; the fix is the read value.
        let inst = RecordInstance::new("T:LS".into(), TransformRecord::new());
        for name in ["IAV", "IPV", "IOV", "OAV", "OPV"] {
            assert_eq!(
                inst.resolve_field(name),
                Some(EpicsValue::Enum(LINK_CON as u16)),
                "{name} should read derived Constant(3), not the dbd initial"
            );
            assert!(
                inst.is_no_mod(name),
                "{name} is SPC_NOMOD — a client put must be refused"
            );
            assert_eq!(
                inst.record.menu_field_choices(name),
                Some(LINK_STATUS_CHOICES),
                "{name} exposes the link-status choice labels"
            );
        }
        // The name matcher is bounded to channels A..P and the I/O prefix — it
        // must not sweep in look-alikes (a would-be `IQV` past channel P, the
        // 4-char link/menu fields, or the L-prefixed prev-value fields).
        assert!(TransformRecord::is_link_status_field("IAV"));
        assert!(TransformRecord::is_link_status_field("OPV"));
        assert!(!TransformRecord::is_link_status_field("IQV"));
        assert!(!TransformRecord::is_link_status_field("IVLA"));
        assert!(!TransformRecord::is_link_status_field("LAV"));
        assert!(!TransformRecord::is_link_status_field("INPA"));
    }

    /// VERS is the code-version constant (C `transformRecord.c:92 #define
    /// VERSION 5.8`, written `ptran->vers = VERSION` in init_record), NOT the
    /// `.dbd` `initial("1")`. A write is accepted-and-ignored, matching
    /// acalcout.
    #[test]
    fn vers_is_the_version_constant_and_ignores_writes() {
        let mut rec = TransformRecord::new();
        assert_eq!(rec.get_field("VERS"), Some(EpicsValue::Double(5.8)));
        assert!(rec.put_field("VERS", EpicsValue::Double(99.0)).is_ok());
        assert_eq!(rec.get_field("VERS"), Some(EpicsValue::Double(5.8)));
    }

    /// C's `init_record` tail, `*plvalue = *pvalue` (`transformRecord.c:490`).
    /// The DB path runs it for every record (`seed_constant_links` ends in
    /// `seed_deadband_tracking`); a record built field-by-field in a unit test
    /// must run it too, or it enters its first `process()` with `LA..LP` at 0
    /// while `A..P` hold their loaded values — i.e. as if every loaded field had
    /// just been written, which suppresses the very calc C performs.
    fn ioc_init(rec: &mut TransformRecord) {
        rec.seed_deadband_tracking();
    }

    /// An expression that compiles clean and FAILS at eval — C
    /// `sCalcPerform()` returning non-zero.
    ///
    /// `"1/0"` is the whole thing: it is a well-formed sCalc expression, it
    /// evaluates to `+inf`, and `sCalcPerform` ends
    /// `return (((isnan(*presult)||isinf(*presult)) ? -1 : 0));`
    /// (sCalcPerform.c:2056) — so a non-finite result IS the failure. This is
    /// reachable through the record's own CLCx put; no hand-built postfix
    /// program is needed (the port previously evaluated CLCx with base's
    /// numeric engine, which has no such check and hands back the infinity
    /// with a zero return — hence `1/0` yielding `inf` and NO_ALARM).
    const DIVIDE_BY_ZERO: &str = "1/0";

    /// R9-63 — a failing channel calc raises CALC_ALARM/INVALID and sets UDF.
    ///
    /// C `transformRecord.c:593-596`:
    /// `if (sCalcPerform(...)) { recGblSetSevr(ptran, CALC_ALARM, INVALID_ALARM);
    /// ptran->udf = TRUE; }`, and `checkAlarms` (`:771-782`) then raises
    /// UDF_ALARM at UDFS. The port raised nothing at all.
    #[test]
    fn r9_63_calc_failure_raises_calc_alarm_and_udf() {
        let mut rec = TransformRecord::new();
        rec.put_field("CLCA", EpicsValue::String(DIVIDE_BY_ZERO.into()))
            .unwrap();
        rec.process().unwrap();

        assert!(
            rec.value_is_undefined(),
            "a failing calc sets udf=TRUE (C transformRecord.c:595)"
        );

        let mut common = CommonFields::default();
        rec.check_alarms(&mut common);
        assert_eq!(
            common.nsev,
            AlarmSeverity::Invalid,
            "CALC_ALARM is raised at INVALID_ALARM severity"
        );
        assert_eq!(
            common.nsta,
            crate::server::recgbl::alarm_status::CALC_ALARM,
            "the status is CALC_ALARM, not UDF_ALARM — C raises CALC first and \
             recGblSetSevr is strict-greater, so the equal-severity UDF_ALARM \
             that checkAlarms adds cannot displace it"
        );
    }

    /// The flag is per-cycle: C clears `ptran->udf` at the top of every
    /// `process()` (`transformRecord.c:524`), so a cycle whose calc succeeds
    /// clears the alarm the previous failure raised.
    #[test]
    fn r9_63_calc_success_clears_the_previous_failure() {
        let mut rec = TransformRecord::new();
        rec.put_field("CLCA", EpicsValue::String(DIVIDE_BY_ZERO.into()))
            .unwrap();
        rec.process().unwrap();
        assert!(rec.value_is_undefined());

        rec.put_field("CLCA", EpicsValue::String("5".into()))
            .unwrap();
        rec.process().unwrap();
        assert!(
            !rec.value_is_undefined(),
            "a clean cycle clears udf (C sets udf = FALSE on entry)"
        );
        let mut common = CommonFields::default();
        rec.check_alarms(&mut common);
        assert_eq!(
            common.nsev,
            AlarmSeverity::NoAlarm,
            "no CALC_ALARM on a cycle whose calcs all succeeded"
        );
    }

    #[test]
    fn test_transform_default() {
        let rec = TransformRecord::new();
        assert_eq!(rec.record_type(), "transform");
        assert_eq!(rec.vals, [0.0; 16]);
        assert_eq!(rec.copt, 0);
    }

    #[test]
    fn test_transform_put_get_values() {
        let mut rec = TransformRecord::new();
        rec.put_field("A", EpicsValue::Double(1.0)).unwrap();
        rec.put_field("B", EpicsValue::Double(2.0)).unwrap();
        assert_eq!(rec.get_field("A"), Some(EpicsValue::Double(1.0)));
        assert_eq!(rec.get_field("B"), Some(EpicsValue::Double(2.0)));
    }

    #[test]
    fn test_transform_put_get_calc() {
        let mut rec = TransformRecord::new();
        rec.put_field("CLCA", EpicsValue::String("B+C".into()))
            .unwrap();
        assert_eq!(
            rec.get_field("CLCA"),
            Some(EpicsValue::String("B+C".into()))
        );
    }

    #[test]
    fn test_transform_put_get_links() {
        let mut rec = TransformRecord::new();
        rec.put_field("INPA", EpicsValue::String("pv1".into()))
            .unwrap();
        rec.put_field("OUTA", EpicsValue::String("pv2".into()))
            .unwrap();
        assert_eq!(
            rec.get_field("INPA"),
            Some(EpicsValue::String("pv1".into()))
        );
        assert_eq!(
            rec.get_field("OUTA"),
            Some(EpicsValue::String("pv2".into()))
        );
    }

    /// A `VAL` token in `CLCx` reads *that channel's* current value: C
    /// `transformRecord.c:593` passes `pval = &ptran->a + i` as `presult`
    /// (`:564`, `:569`), so each channel gets its own result cell — not one
    /// record-wide previous VAL, and not 0.
    #[test]
    fn r5_2_sibling_clc_val_token_reads_that_channels_value() {
        let mut rec = TransformRecord::new();
        rec.put_field("B", EpicsValue::Double(1.0)).unwrap();
        rec.put_field("C", EpicsValue::Double(100.0)).unwrap();
        rec.put_field("CLCB", EpicsValue::String("VAL*2".into()))
            .unwrap();
        rec.put_field("CLCC", EpicsValue::String("VAL+1".into()))
            .unwrap();
        ioc_init(&mut rec);

        // Each CLCx evaluates against its own channel's value: a single
        // record-wide previous VAL (or a 0 seed) could not produce both.
        rec.process().unwrap();
        assert_eq!(rec.vals[1], 2.0, "B = VAL(B)*2 = 1*2");
        assert_eq!(rec.vals[2], 101.0, "C = VAL(C)+1 = 100+1");
        rec.process().unwrap();
        assert_eq!(rec.vals[1], 4.0, "B = 2*2");
        assert_eq!(rec.vals[2], 102.0, "C = 101+1");
    }

    #[test]
    fn test_transform_process_simple() {
        let mut rec = TransformRecord::new();
        rec.put_field("B", EpicsValue::Double(3.0)).unwrap();
        rec.put_field("C", EpicsValue::Double(4.0)).unwrap();
        rec.put_field("CLCA", EpicsValue::String("B+C".into()))
            .unwrap();
        rec.process().unwrap();
        assert_eq!(rec.vals[0], 7.0); // A = B+C = 3+4 = 7
    }

    #[test]
    fn test_transform_process_chain() {
        let mut rec = TransformRecord::new();
        rec.put_field("A", EpicsValue::Double(2.0)).unwrap();
        rec.put_field("CLCB", EpicsValue::String("A*3".into()))
            .unwrap();
        rec.put_field("CLCC", EpicsValue::String("B+1".into()))
            .unwrap();
        rec.process().unwrap();
        assert_eq!(rec.vals[1], 6.0); // B = A*3 = 6
        assert_eq!(rec.vals[2], 7.0); // C = B+1 = 7 (uses updated B)
    }

    #[test]
    fn test_transform_process_no_calc() {
        let mut rec = TransformRecord::new();
        rec.put_field("A", EpicsValue::Double(5.0)).unwrap();
        rec.process().unwrap();
        assert_eq!(rec.vals[0], 5.0); // A unchanged — no calc
    }

    #[test]
    fn test_transform_ivla_do_nothing() {
        let mut rec = TransformRecord::new();
        rec.put_field("A", EpicsValue::Double(10.0)).unwrap();
        rec.put_field("IVLA", EpicsValue::Short(1)).unwrap();
        // Use invalid expression that fails to compile — compiled[0] stays None
        rec.calcs[0] = "???invalid".into();
        rec.compiled[0] = None;
        rec.process().unwrap();
        assert_eq!(rec.vals[0], 10.0); // Unchanged — no valid calc
    }

    #[test]
    fn test_transform_ivla_ignore() {
        let mut rec = TransformRecord::new();
        rec.put_field("A", EpicsValue::Double(10.0)).unwrap();
        rec.put_field("B", EpicsValue::Double(5.0)).unwrap();
        rec.put_field("IVLA", EpicsValue::Short(0)).unwrap();
        // CLCA has no valid calc (empty), CLCB evaluates
        rec.put_field("CLCB", EpicsValue::String("A+1".into()))
            .unwrap();
        ioc_init(&mut rec);
        rec.process().unwrap();
        assert_eq!(rec.vals[0], 10.0); // A unchanged
        assert_eq!(rec.vals[1], 11.0); // B = A+1 = 10+1 = 11
    }

    #[test]
    fn test_transform_all_channels() {
        let mut rec = TransformRecord::new();
        // Set all 16 channels
        for (i, ch) in ('A'..='P').enumerate() {
            let name = ch.to_string();
            rec.put_field(&name, EpicsValue::Double(i as f64)).unwrap();
            assert_eq!(rec.get_field(&name), Some(EpicsValue::Double(i as f64)));
        }
    }

    #[test]
    fn test_transform_field_list() {
        let rec = TransformRecord::new();
        let fields = rec.field_list();
        assert!(fields.len() > 60); // 4 + 16*4 = 68 fields
    }

    #[test]
    fn test_transform_field_not_found() {
        let mut rec = TransformRecord::new();
        assert!(rec.put_field("ZZZ", EpicsValue::Double(1.0)).is_err());
        assert!(rec.get_field("ZZZ").is_none());
    }

    #[test]
    fn test_transform_type_mismatch() {
        let mut rec = TransformRecord::new();
        assert!(rec.put_field("CLCA", EpicsValue::Double(1.0)).is_err());
        assert!(
            rec.put_field("COPT", EpicsValue::String("x".into()))
                .is_err()
        );
    }

    #[test]
    fn test_transform_recompile_on_calc_change() {
        let mut rec = TransformRecord::new();
        rec.put_field("A", EpicsValue::Double(2.0)).unwrap();
        rec.put_field("CLCB", EpicsValue::String("A*2".into()))
            .unwrap();
        rec.process().unwrap();
        assert_eq!(rec.vals[1], 4.0);

        // Change calc expression
        rec.put_field("CLCB", EpicsValue::String("A*3".into()))
            .unwrap();
        rec.process().unwrap();
        assert_eq!(rec.vals[1], 6.0);
    }

    /// R9-62 — `VAL` is a constant-0 dummy, NOT an alias of channel A.
    ///
    /// C `transformRecord.c:422` sets `ptran->val = 0` once at init and no
    /// other line in the record reads or writes `->val`: `process()` and
    /// `monitor()` both walk the channels from `&ptran->a`. So `caget .VAL`
    /// returns 0 no matter what A computes, and a `.VAL` monitor never fires.
    /// The superseded `test_transform_val_is_a` asserted `VAL == 42` here,
    /// pinning an alias C does not have.
    #[test]
    fn r9_62_val_is_a_constant_zero_dummy_not_channel_a() {
        let mut rec = TransformRecord::new();
        rec.put_field("CLCA", EpicsValue::String("42".into()))
            .unwrap();
        rec.process().unwrap();
        assert_eq!(
            rec.get_field("A"),
            Some(EpicsValue::Double(42.0)),
            "CLCA computed channel A"
        );
        assert_eq!(
            rec.get_field("VAL"),
            Some(EpicsValue::Double(0.0)),
            "VAL stays at its init value — process() never touches ->val"
        );
    }

    /// A client put to VAL is stored and read back (plain writable DBF_DOUBLE,
    /// `transformRecord.dbd:43`), but it is inert: it does not become channel
    /// A, and a subsequent process leaves it alone.
    #[test]
    fn r9_62_val_put_is_stored_but_never_feeds_a_channel() {
        let mut rec = TransformRecord::new();
        rec.put_field("VAL", EpicsValue::Double(7.0)).unwrap();
        assert_eq!(rec.get_field("VAL"), Some(EpicsValue::Double(7.0)));
        assert_eq!(
            rec.get_field("A"),
            Some(EpicsValue::Double(0.0)),
            "a put to VAL must not land in channel A"
        );
        rec.put_field("CLCA", EpicsValue::String("3".into()))
            .unwrap();
        rec.process().unwrap();
        assert_eq!(rec.get_field("A"), Some(EpicsValue::Double(3.0)));
        assert_eq!(
            rec.get_field("VAL"),
            Some(EpicsValue::Double(7.0)),
            "process() leaves the put-stored VAL untouched"
        );
    }
}
