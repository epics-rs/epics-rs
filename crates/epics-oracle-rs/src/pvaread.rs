//! The PVA **read** phase: `pvxget` each PV on both sides and diff the text.
//!
//! A walking skeleton, and deliberately labelled as one. What it does have is
//! the property that makes a verdict mean anything: a case that could not run
//! is [`Verdict::Errored`], never agreement.
//!
//! # What it does NOT have yet: a denominator
//!
//! The CA phases enumerate their surface from the `.dbd` ([`crate::surface`]),
//! so their coverage is a percentage of a stated denominator. This phase has
//! no such number — it sweeps one record per record type and compares whatever
//! `pvxget` prints. So it can find defects but it **cannot** claim coverage,
//! and [`PvaReport`] therefore reports no coverage figure rather than a
//! flattering one. See the crate docs for what the PVA denominator should
//! eventually be (record.FIELD over QSRV2, NT projection per type).
//!
//! # Strict text comparison, on purpose
//!
//! The two sides' `pvxget` output is compared **byte for byte** after trimming
//! trailing whitespace. No field is normalized away first — not the timestamp,
//! not the alarm. A harness that pre-normalizes decides in advance which
//! differences are allowed to exist, and the whole point here is to find out
//! which ones do. A difference that turns out to be an artifact of the
//! instrument (rather than of the port) is a reason to declare a normalization
//! *with evidence*, one at a time, not to start with one.
//!
//! # What it needs next: the type as its own case
//!
//! `pvxget`'s default output is **Delta** format — it prints only the fields
//! the reply *marks*. So a single `pvxget` diff cannot distinguish two
//! different bugs, and both are present in the port today (measured on `ai`):
//!
//! - the **type** differs: the port's `NTScalar` omits `display.form`
//!   (`index`/`choices`), which pvxs has emitted since 1.2.0 (`nt.cpp:67`),
//!   and it declares `valueAlarm.hysteresis` as `uint8_t` where pvxs declares
//!   `Float64` (`nt.cpp:109`).
//! - the **marking** differs: both sides declare `control.minStep` and
//!   `valueAlarm.{active,*Severity,hysteresis}`, but QSRV2 does not mark them
//!   in the GET reply and the port does.
//!
//! Both are real — a client sees a difference either way — but they are
//! different defects with different fixes, and in Delta text they look alike:
//! a field the port marks and QSRV2 does not reads exactly like a field QSRV2
//! lacks. [`PvaTools::pvxinfo`] already reports the declared type without the
//! value, so the phase should carry a second case per PV comparing the type
//! directly. Then a type gap can never hide inside a value diff, and the
//! marking difference stops being attributed to the type.

use std::path::{Path, PathBuf};

use crate::catool::ToolError;
use crate::diff::Verdict;
use crate::ioc::{Ioc, PvaPair, PvxTools, Side};
use crate::pvatool::PvaTools;
use crate::report::Counts;

/// Record types the skeleton sweeps by default.
///
/// Scalars that base's `softIoc.dbd` — which `softIocPVX` is built from —
/// certainly defines. This is a **skeleton scope, not a denominator**: it is a
/// hand-picked list, so a clean run over it says "these types agree", never
/// "PVA reads agree". The fat `oracle-ioc` dbd's extra types (`busy`,
/// `transform`, `sseq`, `acalcout`, `scalcout`, `asyn`) are absent from
/// `softIocPVX`, so asking for one is an honest ERROR (the db fails to load),
/// not a silent skip.
pub const SKELETON_TYPES: &[&str] = &[
    "ai",
    "ao",
    "bi",
    "bo",
    "calc",
    "longin",
    "longout",
    "mbbi",
    "mbbo",
    "stringin",
    "stringout",
];

/// One PV read on both sides.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PvaCase {
    pub record_type: String,
    /// The PVA channel name — over QSRV2 this is the record name.
    pub pv: String,
    pub verdict: Verdict,
    /// `pvxget` output, per side. `None` where that side yielded no reading.
    pub c_side: Option<String>,
    pub rust_side: Option<String>,
    /// Why the case could not run. Non-empty iff the verdict is ERRORED.
    pub errors: Vec<ToolError>,
    /// The `.db` that reproduces it.
    pub db: String,
}

/// The PVA phase's report.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PvaReport {
    pub counts: Counts,
    pub cases: Vec<PvaCase>,
}

impl PvaReport {
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
    }

    pub fn human(&self) -> String {
        use std::fmt::Write;
        let mut s = String::new();
        let _ = writeln!(s, "\nPVA READ PHASE (skeleton — no denominator yet)");
        let _ = writeln!(s, "{:-<64}", "");
        let _ = writeln!(s, "  {:<22}{}", "cases run", self.counts.ran);
        let _ = writeln!(s, "  {:<22}{}", "AGREED", self.counts.agreed);
        let _ = writeln!(
            s,
            "  {:<22}{}",
            "EXPECTED DEVIATION", self.counts.expected_deviation
        );
        let _ = writeln!(s, "  {:<22}{}", "DEFECT", self.counts.defect);
        let _ = writeln!(s, "  {:<22}{}", "ERROR", self.counts.errored);
        let _ = writeln!(s, "{:-<64}", "");

        for c in self.cases.iter().filter(|c| c.verdict != Verdict::Agreed) {
            let _ = writeln!(s, "\n[{:?}] {} ({})", c.verdict, c.pv, c.record_type);
            for e in &c.errors {
                let _ = writeln!(s, "    ! {e}");
            }
            if c.verdict == Verdict::Defect {
                for line in first_differing_lines(
                    c.c_side.as_deref().unwrap_or(""),
                    c.rust_side.as_deref().unwrap_or(""),
                ) {
                    let _ = writeln!(s, "    {line}");
                }
            }
        }
        // No coverage line, deliberately: this phase has no denominator to be
        // a percentage of, and a made-up one would be worse than none.
        let _ = writeln!(
            s,
            "\nNOTE: a hand-picked record-type list, not an enumerated surface. \
             A clean run says those types agree — not that PVA reads agree."
        );
        s
    }
}

/// The first few differing lines, side by side. Enough to act on without
/// dumping two whole NT structures into the terminal.
fn first_differing_lines(c: &str, r: &str) -> Vec<String> {
    const MAX: usize = 6;
    let mut out = Vec::new();
    let (cl, rl): (Vec<&str>, Vec<&str>) = (c.lines().collect(), r.lines().collect());
    for i in 0..cl.len().max(rl.len()) {
        let (a, b) = (cl.get(i).copied(), rl.get(i).copied());
        if a == b {
            continue;
        }
        out.push(format!("C   : {}", a.unwrap_or("<absent>")));
        out.push(format!("rust: {}", b.unwrap_or("<absent>")));
        if out.len() >= MAX * 2 {
            out.push("      ... (truncated; see --json for the full text)".into());
            break;
        }
    }
    out
}

/// Boot the PVA pair on a one-record `.db` per record type and diff `pvxget`.
pub fn probe(tools: &PvxTools, workdir: &Path, record_types: &[String]) -> PvaReport {
    let mut cases = Vec::new();
    for (i, rt) in record_types.iter().enumerate() {
        eprintln!("[{}/{}] pva read: {rt}", i + 1, record_types.len());
        cases.push(probe_one(tools, workdir, rt));
    }
    let counts = Counts::tally_verdicts(cases.iter().map(|c| c.verdict));
    PvaReport { counts, cases }
}

fn write_db(workdir: &Path, name: &str, text: &str) -> Result<PathBuf, String> {
    let p = workdir.join(format!("{name}.db"));
    std::fs::write(&p, text).map_err(|e| format!("write {}: {e}", p.display()))?;
    Ok(p)
}

/// One record type: boot the pair, `pvxget` the record on both sides, adjudicate.
fn probe_one(tools: &PvxTools, workdir: &Path, record_type: &str) -> PvaCase {
    let rec = format!("ORACLE:{}", record_type.to_uppercase());
    let db_text = crate::record_stmt(record_type, &rec);

    let errored = |errors: Vec<ToolError>| PvaCase {
        record_type: record_type.to_string(),
        pv: rec.clone(),
        verdict: Verdict::Errored,
        c_side: None,
        rust_side: None,
        errors,
        db: db_text.clone(),
    };
    // A boot failure is not attributable to one side's tool, but it is still an
    // ERROR and must never be scored as agreement.
    let boot_err = |msg: String| {
        errored(vec![ToolError {
            side: Side::C,
            tool: "boot".into(),
            message: msg,
        }])
    };

    let db = match write_db(workdir, &format!("pva_read_{record_type}"), &db_text) {
        Ok(p) => p,
        Err(e) => return boot_err(e),
    };
    // `PvaPair::boot` returns only once both sides are reachable AND each is
    // proven the sole server on its port, so a reading obtained below is
    // attributable by construction.
    let pair = match PvaPair::boot(tools, &db, &rec) {
        Ok(p) => p,
        Err(e) => return boot_err(e.to_string()),
    };

    let c = PvaTools::new(tools, pair.c.port(), Side::C);
    let r = PvaTools::new(tools, pair.rust.port(), Side::Rust);
    adjudicate(record_type, &rec, &db_text, c.pvxget(&rec), r.pvxget(&rec))
}

/// The three-way verdict for one PV, from the two sides' readings.
///
/// EXPECTED DEVIATION is unreachable here and that is honest, not an
/// oversight: the CA allowlist transcribes `doc/upstream-c-bugs.md`, whose
/// rows are about C's *CA* behaviour and justify nothing about QSRV2. Until a
/// PVA deviation is found, understood, and written down, a difference is a
/// DEFECT — the catalogue must earn its rows rather than start with them.
pub fn adjudicate(
    record_type: &str,
    pv: &str,
    db_text: &str,
    c: Result<String, ToolError>,
    r: Result<String, ToolError>,
) -> PvaCase {
    let mut errors = Vec::new();
    let c_side = match c {
        Ok(v) => Some(v),
        Err(e) => {
            errors.push(e);
            None
        }
    };
    let rust_side = match r {
        Ok(v) => Some(v),
        Err(e) => {
            errors.push(e);
            None
        }
    };

    // Either side missing => ERROR. Note what is deliberately NOT done: two
    // failed reads are not "both sides agree that it fails". Nothing was
    // measured, so there is nothing to agree about.
    let verdict = match (&c_side, &rust_side) {
        (Some(a), Some(b)) if a.trim_end() == b.trim_end() => Verdict::Agreed,
        (Some(_), Some(_)) => Verdict::Defect,
        _ => Verdict::Errored,
    };

    PvaCase {
        record_type: record_type.to_string(),
        pv: pv.to_string(),
        verdict,
        c_side,
        rust_side,
        errors,
        db: db_text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terr(side: Side) -> ToolError {
        ToolError {
            side,
            tool: "pvxget".into(),
            message: "Timeout".into(),
        }
    }

    #[test]
    fn identical_readings_agree() {
        let c = adjudicate(
            "ai",
            "X",
            "",
            Ok("value = 1".into()),
            Ok("value = 1".into()),
        );
        assert_eq!(c.verdict, Verdict::Agreed);
        assert!(c.errors.is_empty());
    }

    #[test]
    fn differing_readings_are_a_defect() {
        let c = adjudicate(
            "ai",
            "X",
            "",
            Ok("value = 1".into()),
            Ok("value = 2".into()),
        );
        assert_eq!(c.verdict, Verdict::Defect);
    }

    /// Trailing whitespace is not a behavioural difference; nothing else is
    /// normalized.
    #[test]
    fn only_trailing_whitespace_is_normalized() {
        let c = adjudicate("ai", "X", "", Ok("v = 1\n".into()), Ok("v = 1".into()));
        assert_eq!(c.verdict, Verdict::Agreed);
        // ...but an interior difference still lands, even a whitespace one.
        let c = adjudicate("ai", "X", "", Ok("v =  1".into()), Ok("v = 1".into()));
        assert_eq!(c.verdict, Verdict::Defect);
    }

    #[test]
    fn a_side_that_did_not_read_is_an_error_not_agreement() {
        let c = adjudicate("ai", "X", "", Err(terr(Side::C)), Ok("v = 1".into()));
        assert_eq!(c.verdict, Verdict::Errored);
        let c = adjudicate("ai", "X", "", Ok("v = 1".into()), Err(terr(Side::Rust)));
        assert_eq!(c.verdict, Verdict::Errored);
    }

    /// The rule this harness exists for. Two sides that both failed have NOT
    /// agreed — nothing was measured, so there is nothing to agree about.
    #[test]
    fn both_sides_failing_is_an_error_never_agreement() {
        let c = adjudicate("ai", "X", "", Err(terr(Side::C)), Err(terr(Side::Rust)));
        assert_eq!(
            c.verdict,
            Verdict::Errored,
            "two failed reads must not score as agreement",
        );
        assert_eq!(c.errors.len(), 2, "both sides' errors must be reported");
    }

    #[test]
    fn counts_reconcile_over_pva_verdicts() {
        let counts = Counts::tally_verdicts([
            Verdict::Agreed,
            Verdict::Defect,
            Verdict::Errored,
            Verdict::Agreed,
        ]);
        assert_eq!(counts.ran, 4);
        assert_eq!(counts.agreed, 2);
        assert_eq!(counts.defect, 1);
        assert_eq!(counts.errored, 1);
        counts.check().expect("buckets must reconcile");
    }
}
