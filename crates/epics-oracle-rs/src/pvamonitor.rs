//! The PVA **monitor** phase: subscribe on both sides, drive one value
//! sequence, and diff the events each server posted.
//!
//! The analogue of the CA monitor probe ([`crate::runner::Runner::probe_monitor`]),
//! one protocol over, and it exists because a whole class of port behaviour is
//! invisible to [`crate::pvaread`]: a *read* is one reply, and the port's
//! seed reply is already correct. What an update frames, whether an update is
//! posted at all, and how many — none of it is reachable without subscribing.
//!
//! # The denominator, and what it excludes
//!
//! The read phase's denominator is every `record.FIELD` channel the `.dbd`
//! enumerates (3386 of them). This phase does **not** use it, and the reason is
//! not only cost. A monitor case needs a *drive*, and a put to `REC.FIELD`
//! processes the record — which posts on `REC`'s sibling channels too. So the
//! channels of one record cannot be driven concurrently without making every
//! event unattributable, and driven serially they cost one settle window each:
//! 3386 channels x 2 sides x ~2s is over three hours.
//!
//! So this phase takes [`crate::runner::Runner::probe_monitor`]'s scoping precedent — **the
//! `VAL` channel of each record type** — and states what that leaves out:
//!
//! - **the 3346 non-`VAL` channels**, for the reason above. Not measured here.
//!   Measured on the read phase's two contracts, which is not the same thing:
//!   nothing here says `REC.EGU`'s *monitor* behaves. A real gap, and a future
//!   phase.
//! - **9 record types whose `VAL` is `DBF_NOACCESS`** (`aai`, `aao`, `compress`,
//!   `histogram`, `lsi`, `lso`, `printf`, `subArray`, `waveform`). pvxs refuses
//!   an NT of `Null`, so those channels are outside the enumerated surface
//!   already ([`crate::surface`]) and there is nothing to subscribe to.
//! - **`sel`, whose `VAL` is `special(SPC_NOMOD)`** — the `.dbd` states no
//!   client may write it, so no client can drive it. See [`val_status`].
//! - **QSRV2 group PVs** (`dbLoadGroup` JSON), exactly as in the read phase:
//!   a configured surface no `.dbd` enumerates.
//!
//! # Two reproducers per record type, because one cannot attribute its own
//! failure
//!
//! Each record type is measured on two reproducers in one boot, and the pair is
//! what makes a silent port attributable:
//!
//! - [`Drive::Passive`] — `record(t, "ORACLE:MON:T") {}`. A put to `VAL` must
//!   process the record itself (`VAL` is `pp(TRUE)`, and `dbPutField` processes
//!   a Passive record), and processing must post.
//! - [`Drive::Scanned`] — the same record with `field(SCAN, ".1 second")`.
//!   Processing is then the scan's job, not the put's, so a missing event can
//!   only be a *posting* failure.
//!
//! One reproducer alone conflates two defects. A Passive record that posts
//! nothing may have failed to process OR failed to post, and the harness cannot
//! say which — the same unattributable diff the read phase's `.dbd` shape
//! contract exists to avoid. Measured together they separate: silent on Passive
//! but posting on Scanned indicts the *put* path; silent on both indicts the
//! *monitor* path.
//!
//! # Why `.1 second`, and why not for three record types
//!
//! A scanned reproducer is only usable if its own scanning is quiet — an event
//! this harness did not drive is an event it cannot attribute. Measured against
//! `softIocPVX`: 28 of 30 record types post **nothing** across 8 seconds of
//! unchanged `.1 second` scanning, and the two that do not post on *every* scan
//! regardless of change — `event` (its process posts the event name) and `sseq`
//! — 80 events in 8s, undriven. Their event count would then be a function of
//! wall-clock, so they get no scanned reproducer. `asyn` gets none either: its
//! process drives device I/O against the deliberately disconnected
//! `ORACLEASYN` port ([`crate::ORACLE_ASYN_PORT`]), which is the timing
//! nondeterminism [`crate::puts_are_measurable`] already refuses. All three
//! keep their Passive reproducer, and [`scan_is_measurable`] is the single,
//! loud owner of the exclusion.
//!
//! # The drive must be proven, or agreement is a lie
//!
//! `pvxput` **exits 0 when the put was refused** (see [`PvaTools::pvxput`]).
//! That matters here more than anywhere: a refused put posts no event, a port
//! that posts nothing then matches a ground truth that posted nothing, and the
//! case scores AGREED on an experiment that never ran. So a drive that failed on
//! either side makes the case ERRORED, and no hand-kept list of undrivable
//! fields is needed for the cases the `.dbd` does not speak to: the drive
//! reports itself.
//!
//! That runtime rule and [`val_status`]'s static exclusions are not duplicates —
//! they have different jobs. The `.dbd` removes what it *proves* undrivable
//! (`SPC_NOMOD`) before a case is ever built; the drive check catches what the
//! `.dbd` could not know. Note which one is *not* allowed to decide: nothing is
//! ever excluded because of what a side *did*, or a port could shrink the
//! denominator by misbehaving.
//!
//! Note the converse is **not** an error. `fanout`, `seq` and `sub` accept the
//! put (silent stderr) and post nothing — measured. That is ground truth's real
//! behaviour, so the case is measured normally, and a port that posted an event
//! there would be a DEFECT rather than a missing one.
//!
//! # The one normalization, and its evidence
//!
//! The read phase compares text byte for byte and normalizes nothing but the
//! block header. This phase must normalize one more thing: **the timestamp**.
//! Two independently booted IOCs process at different instants, so
//! `timeStamp.secondsPastEpoch` can never match, and comparing it verbatim would
//! score every case DEFECT for a difference the harness itself created — the
//! same argument that removed the `pvxinfo` port header, and no wider.
//!
//! It is normalized to its **contract class**, not erased, so what the leaf
//! actually promises still gets compared:
//!
//! | reading | class | what it means |
//! |---|---|---|
//! | `secondsPastEpoch == 631152000` | `<epics-epoch>` | the EPICS epoch: never processed |
//! | anything else | `<wall-clock>` | processed at some real time |
//! | `nanoseconds == 0` | `<zero>` | |
//! | anything else | `<sub-second>` | |
//!
//! A port that leaves the stamp undefined after processing, or that has no
//! sub-second resolution, therefore still shows as a DEFECT; only the instant
//! itself is dropped. Nothing else is normalized — not the alarm, not the value,
//! not which leaves were framed.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::allowlist::{Allowlist, MatchContext};
use crate::catool::{ToolError, unattributed};
use crate::dbd::DbfType;
use crate::diff::Verdict;
use crate::ioc::{Ioc, PvaPair, PvxTools, Side};
use crate::ntshape::{NT_ENUM, NtShape};
use crate::pvatool::{PvaEvent, PvaTools};
use crate::report::{Counts, StaleRow};
use crate::surface::{Coverage, Surface, ValStatus, drives_val, val_status};

/// The scan rate of the [`Drive::Scanned`] reproducer.
///
/// Fast enough that the next scan lands well inside `PUT_SPACING` (so a driven
/// put is always picked up before the next one is issued), slow enough to stay
/// far from the settle window.
const SCAN_RATE: &str = ".1 second";

/// The driven sequence, and the repeat is the point of it — the same contract
/// [`crate::runner::Runner::probe_monitor`] drives over CA. C suppresses a
/// monitor when the value did not change (subject to MDEL/ADEL), so a faithful
/// port posts three updates, not four. "Posts an event per put regardless of
/// change" is exactly the divergence a value-only diff cannot see: the final
/// value agrees either way.
const SEQ: [&str; 4] = ["1", "2", "2", "3"];

/// How long between puts.
///
/// Wider than the CA probe's 120ms because of [`Drive::Scanned`]: a put lands
/// as an event only at the next scan, so puts spaced closer than [`SCAN_RATE`]
/// could be coalesced into one scan and the count would become a function of
/// timing rather than of the server. 4x the scan period, measured deterministic
/// across two identical runs of all 28 scanned types.
const PUT_SPACING: Duration = Duration::from_millis(400);

/// How long the subscription is held open after the last put.
///
/// Not a nicety: a port that posts events C suppresses must be caught, so the
/// window cannot close the instant the puts return.
const MONITOR_SETTLE: Duration = Duration::from_millis(1000);

/// How long a subscription has to produce its seed events before the case is an
/// ERROR. Generous because it covers a cold search on both PVs, and because a
/// tight bound here would turn load into a fake defect.
const MONITOR_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The EPICS epoch (1990-01-01) in POSIX seconds — what pvxs prints for a record
/// that has never processed, on both sides.
const EPICS_EPOCH_POSIX_SECONDS: &str = "631152000";

/// Which reproducer drove a case's events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Drive {
    /// `record(t, "…") {}` — the put must process the record, and processing
    /// must post.
    Passive,
    /// `field(SCAN, ".1 second")` — the scan processes, so only posting is
    /// under test.
    Scanned,
}

impl Drive {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passive => "passive",
            Self::Scanned => "scanned",
        }
    }

    /// The reproducer's record name for `record_type`. Distinct names so both
    /// live in one `.db` and one subscription covers both.
    fn pv(self, record_type: &str) -> String {
        match self {
            Self::Passive => format!("ORACLE:MON:{}", record_type.to_uppercase()),
            Self::Scanned => format!("ORACLE:MONSCAN:{}", record_type.to_uppercase()),
        }
    }

    fn fields(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Self::Passive => &[],
            Self::Scanned => &[("SCAN", SCAN_RATE)],
        }
    }
}

/// Whether a record type can carry a [`Drive::Scanned`] reproducer.
///
/// `false` for the three types whose own scanning would produce events this
/// harness did not drive, which makes the event count a function of wall-clock
/// rather than of the server. Measured against `softIocPVX`, undriven, over 8
/// seconds of `.1 second` scanning:
///
/// - `event` — 80 events. Its `process` posts the event name every time.
/// - `sseq`  — 80 events.
/// - `asyn`  — excluded on the same grounds [`crate::puts_are_measurable`]
///   excludes it: its `process` drives device I/O against the disconnected
///   `ORACLEASYN` port, injecting socket timing into the oracle.
///
/// Every other type posted **nothing** undriven, which is what makes a driven
/// event attributable.
///
/// This is not a silent blind spot: all three keep their [`Drive::Passive`]
/// reproducer, the exclusion is counted in the report's denominator, and callers
/// MUST surface it rather than let a skipped reproducer read as a measured-clean
/// one.
pub fn scan_is_measurable(record_type: &str) -> bool {
    !matches!(record_type, "event" | "sseq" | "asyn")
}

/// The named contracts a monitored channel can differ on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MonSurface {
    /// The connection-time event: what the server frames before anything is
    /// driven.
    SeedEvent,
    /// The post-seed events' text, in order — which leaves each update framed.
    UpdateEvents,
    /// How many post-seed events. Named apart from their text so an extra or
    /// missing event is never reported as a framing difference.
    EventCount,
}

impl MonSurface {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SeedEvent => "seed_event",
            Self::UpdateEvents => "update_events",
            Self::EventCount => "event_count",
        }
    }

    pub const ALL: [MonSurface; 3] = [Self::SeedEvent, Self::UpdateEvents, Self::EventCount];
}

/// One concrete disagreement on one contract.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct MonDifference {
    pub surface: MonSurface,
    /// What pvxs posted.
    pub reference: String,
    /// What the port posted.
    pub observed: String,
}

/// The event stream one side posted for one channel.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct MonTrace {
    /// The connection-time event. Its arrival is what proved the subscription
    /// live, so a trace cannot exist without one.
    pub seed: String,
    /// Every event after the seed, in order.
    pub updates: Vec<String>,
}

/// Everything one side reported about one monitored channel.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct MonObservation {
    pub trace: Option<MonTrace>,
    /// Anything that prevented a measurement — a subscription that would not
    /// establish, a drive that was refused. Non-empty => the case is ERRORED.
    pub errors: Vec<ToolError>,
}

impl MonObservation {
    /// Did this side produce a trace at all?
    ///
    /// Not `errors.is_empty()`: a side that reported no error and no trace has
    /// still not been measured, and the two must never be confused.
    pub fn is_complete(&self) -> bool {
        self.errors.is_empty() && self.trace.is_some()
    }
}

/// One monitored channel on one reproducer, measured on both sides.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MonCase {
    pub record_type: String,
    pub field: String,
    pub pv: String,
    pub drive: Drive,
    pub verdict: Verdict,
    pub c_side: MonObservation,
    pub rust_side: MonObservation,
    pub differences: Vec<MonDifference>,
    /// CBUG ids that justified the differences (empty unless EXPECTED
    /// DEVIATION). Same contract as the read phase.
    #[serde(default)]
    pub allowlisted: Vec<String>,
    pub errors: Vec<ToolError>,
    /// The `.db` that reproduces it.
    pub db: String,
}

impl MonCase {
    pub fn id(&self) -> String {
        format!(
            "{}.{} [{}]",
            self.record_type,
            self.field,
            self.drive.as_str()
        )
    }
}

/// What this phase measured, and what it could not.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MonDenominator {
    pub dbd: String,
    /// Every channel the read phase enumerates — stated so a monitor run is
    /// never mistaken for covering it.
    pub channels_in_surface: usize,
    /// Record types with a `VAL` channel in the surface: the candidates.
    pub record_types_with_val: usize,
    /// Record types whose `VAL` is `DBF_NOACCESS`, so no channel exists.
    pub excluded_noaccess_val: Vec<String>,
    /// Record types whose `VAL` is `special(SPC_NOMOD)`: the `.dbd` says no
    /// client can write it, so no client can drive it.
    pub excluded_nomod_val: Vec<String>,
    /// Record types whose `VAL` is a link or is otherwise not client-writable.
    pub excluded_unwritable_val: Vec<String>,
    /// Record types with no [`Drive::Scanned`] reproducer, and why.
    pub excluded_scanned: Vec<String>,
    /// Non-`VAL` channels, which this phase does not drive.
    pub excluded_non_val_channels: usize,
}

/// The monitor phase's report.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MonReport {
    pub denominator: MonDenominator,
    pub case_coverage: Coverage,
    pub counts: Counts,
    #[serde(default)]
    pub stale_allowlist_rows: Vec<StaleRow>,
    #[serde(default)]
    pub unexercised_allowlist_rows: Vec<StaleRow>,
    #[serde(default)]
    pub fired_allowlist_rows: Vec<String>,
    pub cases: Vec<MonCase>,
}

impl MonReport {
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
    }

    pub fn defects(&self) -> impl Iterator<Item = &MonCase> {
        self.cases.iter().filter(|c| c.verdict == Verdict::Defect)
    }

    pub fn defects_on(&self, s: MonSurface) -> usize {
        self.defects()
            .filter(|c| c.differences.iter().any(|d| d.surface == s))
            .count()
    }

    pub fn human(&self) -> String {
        use std::fmt::Write;
        let mut s = String::new();
        let d = &self.denominator;
        let c = &self.counts;

        s.push_str("=== PVA MONITOR ORACLE: pvxs QSRV2 (softIocPVX) vs oracle-ioc --pva ===\n\n");

        s.push_str("DENOMINATOR (from the .dbd, not hand-listed)\n");
        let _ = writeln!(s, "  spec                       : {}", d.dbd);
        let _ = writeln!(
            s,
            "  channels in the PVA surface: {}   <-- the READ phase's denominator, NOT this one",
            d.channels_in_surface
        );
        let _ = writeln!(
            s,
            "  ...driven here             : the VAL channel of each record type ({} of them)",
            d.record_types_with_val
        );
        let _ = writeln!(
            s,
            "  ...NOT driven              : {} non-VAL channels (a put processes the record, so\n  \
             {:27}  siblings cannot be driven concurrently and attributed; serially\n  \
             {:27}  they are >3h. Read-measured only — a future phase)",
            d.excluded_non_val_channels, "", ""
        );
        if !d.excluded_noaccess_val.is_empty() {
            let _ = writeln!(
                s,
                "  excluded (VAL is NOACCESS) : {} — {}\n  {:27}  (pvxs refuses the channel: NT of Null)",
                d.excluded_noaccess_val.len(),
                d.excluded_noaccess_val.join(", "),
                ""
            );
        }
        if !d.excluded_nomod_val.is_empty() {
            let _ = writeln!(
                s,
                "  excluded (VAL is SPC_NOMOD): {} — {}\n  {:27}  (the .dbd forbids writing it, so no client can drive it)",
                d.excluded_nomod_val.len(),
                d.excluded_nomod_val.join(", "),
                ""
            );
        }
        if !d.excluded_unwritable_val.is_empty() {
            let _ = writeln!(
                s,
                "  excluded (VAL not writable): {} — {}",
                d.excluded_unwritable_val.len(),
                d.excluded_unwritable_val.join(", ")
            );
        }
        let _ = writeln!(
            s,
            "  no scanned reproducer      : {} — {}  (own scanning posts undriven events)\n",
            d.excluded_scanned.len(),
            d.excluded_scanned.join(", ")
        );

        let cov = &self.case_coverage;
        s.push_str("COVERAGE\n");
        let _ = writeln!(
            s,
            "  cases measured on BOTH sides: {}/{} = {:.1}%",
            cov.measured,
            cov.enumerated,
            cov.percent()
        );
        let _ = writeln!(s, "  cases that errored (NOT coverage): {}\n", cov.errored);

        s.push_str("CASES (one per record type per reproducer: passive + scanned)\n");
        let _ = writeln!(s, "  ran                : {}", c.ran);
        let _ = writeln!(s, "  agreed             : {}", c.agreed);
        let _ = writeln!(
            s,
            "  expected deviation : {}  (allowlisted against expected-deviations.toml)",
            c.expected_deviation
        );
        let _ = writeln!(s, "  DEFECT             : {}", c.defect);
        let _ = writeln!(
            s,
            "  ERROR              : {}  (could not run — never counted as agreement)",
            c.errored
        );
        match c.check() {
            Ok(()) => s.push_str("  (buckets reconcile with `ran`)\n\n"),
            Err(e) => {
                let _ = writeln!(s, "  !!! {e}\n");
            }
        }

        if !self.fired_allowlist_rows.is_empty() || !self.stale_allowlist_rows.is_empty() {
            s.push_str("ALLOWLIST (expected-deviations.toml)\n");
            if !self.fired_allowlist_rows.is_empty() {
                let mut fired: Vec<&String> = self.fired_allowlist_rows.iter().collect();
                fired.sort();
                let _ = writeln!(
                    s,
                    "  fired: {}",
                    fired
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            for r in &self.stale_allowlist_rows {
                let _ = writeln!(s, "  STALE (deviation stopped): {} — {}", r.id, r.why);
            }
            s.push('\n');
        }

        s.push_str(
            "DEFECTS BY CONTRACT (a case may differ on more than one, so these\n\
             do NOT sum to the DEFECT count)\n",
        );
        for surface in MonSurface::ALL {
            let _ = writeln!(s, "  {:<18}{}", surface.as_str(), self.defects_on(surface));
        }
        s.push('\n');

        let defects: Vec<_> = self.defects().collect();
        if !defects.is_empty() {
            let _ = writeln!(s, "DEFECTS ({})", defects.len());
            for case in defects.iter().take(20) {
                let _ = writeln!(s, "\n  [{}]  {}", case.id(), case.pv);
                for diff in &case.differences {
                    let _ = writeln!(s, "    {} :", diff.surface.as_str());
                    for line in
                        crate::pvaread::first_differing_lines(&diff.reference, &diff.observed)
                    {
                        let _ = writeln!(s, "      {line}");
                    }
                }
            }
            if defects.len() > 20 {
                let _ = writeln!(s, "\n  ... and {} more (see --json)", defects.len() - 20);
            }
            s.push('\n');
        }

        s.push_str(&crate::report::errors_section(
            "cases",
            self.cases
                .iter()
                .filter(|c| c.verdict == Verdict::Errored)
                .map(|c| c.errors.as_slice())
                .collect(),
        ));

        s.push_str(
            "NOT MEASURED: non-VAL channels' monitors, and QSRV2 group PVs. A clean run\n\
             here says each record type's VAL channel posts what pvxs posts, no more.\n",
        );
        s
    }
}

/// Everything a case needs to identify itself. Passed as one value so
/// [`adjudicate`] keeps a signature a caller can read.
#[derive(Debug, Clone)]
pub struct CaseRef {
    pub record_type: String,
    pub pv: String,
    /// `VAL`'s declared `.dbd` type, carried so an allowlist row scoped by
    /// destination type is enforced here exactly as on the CA path.
    pub val_dbf: DbfType,
    pub drive: Drive,
    pub db: String,
}

/// Boot the pair per record type and measure each reproducer's event stream.
pub fn probe(
    tools: &PvxTools,
    workdir: &Path,
    surface: &Surface,
    record_types: &[String],
    allowlist: &mut Allowlist,
) -> Vec<MonCase> {
    let mut cases = Vec::new();
    for (i, rt) in record_types.iter().enumerate() {
        // A skip is loud and says WHICH rule excluded the type, so a shrunken
        // denominator can never be mistaken for a measured-clean one.
        if let Some(why) = val_status(surface, rt).why() {
            eprintln!(
                "[{}/{}] pva monitor: {rt} — EXCLUDED: {why}",
                i + 1,
                record_types.len()
            );
            continue;
        }
        if !scan_is_measurable(rt) {
            eprintln!(
                "[{}/{}] pva monitor: {rt} — passive only (own scanning posts undriven events)",
                i + 1,
                record_types.len()
            );
        }
        eprintln!("[{}/{}] pva monitor: {rt}", i + 1, record_types.len());
        // `Drivable` was just proven, so VAL exists in the surface.
        let val_dbf = surface
            .fields_of(rt)
            .find(|f| f.field.name == "VAL")
            .expect("val_status returned Drivable, so VAL is in the surface")
            .field
            .dbf;
        cases.extend(probe_type(tools, workdir, rt, val_dbf, allowlist));
    }
    cases
}

/// The reproducers this record type gets.
fn drives_for(record_type: &str) -> Vec<Drive> {
    if scan_is_measurable(record_type) {
        vec![Drive::Passive, Drive::Scanned]
    } else {
        vec![Drive::Passive]
    }
}

fn write_db(workdir: &Path, name: &str, text: &str) -> Result<PathBuf, String> {
    let p = workdir.join(format!("{name}.db"));
    std::fs::write(&p, text).map_err(|e| format!("write {}: {e}", p.display()))?;
    Ok(p)
}

/// One record type: boot the pair once, subscribe to every reproducer on both
/// sides, drive them, and diff.
fn probe_type(
    tools: &PvxTools,
    workdir: &Path,
    record_type: &str,
    val_dbf: DbfType,
    allowlist: &mut Allowlist,
) -> Vec<MonCase> {
    let drives = drives_for(record_type);
    let db_text: String = drives
        .iter()
        .map(|d| crate::record_stmt_fields(record_type, &d.pv(record_type), d.fields()))
        .collect();

    let refs: Vec<CaseRef> = drives
        .iter()
        .map(|d| CaseRef {
            record_type: record_type.to_string(),
            pv: d.pv(record_type),
            val_dbf,
            drive: *d,
            db: db_text.clone(),
        })
        .collect();
    let pvs: Vec<String> = refs.iter().map(|r| r.pv.clone()).collect();

    let db = match write_db(workdir, &format!("pva_mon_{record_type}"), &db_text) {
        Ok(p) => p,
        Err(e) => return errored_cases(&refs, &unattributed("write-db", &e)),
    };
    let pair = match PvaPair::boot(tools, &db, &pvs[0]) {
        Ok(p) => p,
        Err(e) => return errored_cases(&refs, &e.tool_errors("boot")),
    };

    let c = PvaTools::new(tools, pair.c.port(), Side::C);
    let r = PvaTools::new(tools, pair.rust.port(), Side::Rust);

    // The drive's spelling depends on the channel's NT shape, and it is taken
    // from GROUND TRUTH rather than from the .dbd: `mbbo.VAL` declares DBF_ENUM
    // but its cvt_dbaddr serves an NTScalar, so the .dbd's answer would put the
    // wrong syntax on the wire (see `NtShape::expected`). Both sides are then
    // driven with the byte-identical command — a port that declares a different
    // shape has its put refused, which is an ERROR here and a DEFECT in the read
    // phase's declared_type contract, exactly where it belongs.
    let shape = match c.pvxinfo(&pvs[0]).map(|b| NtShape::observed(&b)) {
        Ok(Some(s)) => s,
        Ok(None) => {
            return errored_cases(
                &refs,
                &[ToolError {
                    side: Side::C,
                    tool: "pvxinfo".into(),
                    message: "ground truth's pvxinfo declared no parseable NT shape".into(),
                }],
            );
        }
        Err(e) => return errored_cases(&refs, &[e]),
    };

    // Two independent servers on two ports with no shared state: observing them
    // concurrently changes nothing either one posts, and halves a phase whose
    // wall-clock is settle-bound.
    let (obs_c, obs_r) = std::thread::scope(|s| {
        let hc = s.spawn(|| observe(&c, &pvs, &shape));
        let hr = s.spawn(|| observe(&r, &pvs, &shape));
        (
            hc.join().expect("C monitor lane panicked"),
            hr.join().expect("Rust monitor lane panicked"),
        )
    });

    refs.iter()
        .enumerate()
        .map(|(i, cr)| adjudicate(cr, &obs_c[i], &obs_r[i], allowlist))
        .collect()
}

/// How `pvxput` must be told to write `value` to a channel of this shape.
///
/// `NTEnum`'s `value` is a **struct**, so a bare scalar cannot be assigned to
/// it: pvxs answers `Unable to assign struct with String` — and exits 0 while
/// doing so. Measured, and the reason this is not just `value.to_string()`.
fn assignment(shape: &NtShape, value: &str) -> String {
    if shape.type_id == NT_ENUM {
        format!("value.index={value}")
    } else {
        value.to_string()
    }
}

/// Subscribe to every reproducer on one side, drive them all, and split the
/// stream back out per PV.
fn observe(t: &PvaTools, pvs: &[String], shape: &NtShape) -> Vec<MonObservation> {
    // The drive runs inside the subscription window, so its failures are
    // collected here rather than returned: a refused put is what turns a case
    // into an ERROR instead of a false AGREED.
    let drive_errors: RefCell<Vec<ToolError>> = RefCell::new(Vec::new());
    let drive = |t: &PvaTools| {
        for v in SEQ {
            for pv in pvs {
                if let Err(e) = t.pvxput(pv, &assignment(shape, v)) {
                    drive_errors.borrow_mut().push(e);
                }
            }
            // Space the puts so a server that posts per change has time to emit
            // each one, and so a scanned reproducer's next scan lands between
            // them rather than coalescing two.
            std::thread::sleep(PUT_SPACING);
        }
    };

    let events = t.pvxmonitor(pvs, MONITOR_SETTLE, MONITOR_CONNECT_TIMEOUT, drive);
    let drive_errors = drive_errors.into_inner();

    match events {
        // The subscription never established: nothing was measured for any PV
        // in it, and every one carries the same cause.
        Err(e) => pvs
            .iter()
            .map(|_| MonObservation {
                trace: None,
                errors: vec![e.clone()],
            })
            .collect(),
        Ok(events) => pvs
            .iter()
            .map(|pv| {
                let mut o = split_trace(pv, &events);
                // A drive that was refused is attributed to the PV it named, so
                // one undrivable channel never errors its reproducer-mates.
                o.errors.extend(
                    drive_errors
                        .iter()
                        .filter(|e| e.message.contains(pv.as_str()))
                        .cloned(),
                );
                o
            })
            .collect(),
    }
}

/// One PV's events out of the shared stream, normalized for comparison.
fn split_trace(pv: &str, events: &[PvaEvent]) -> MonObservation {
    let mut mine = events.iter().filter(|e| e.pv == pv);
    match mine.next() {
        Some(seed) => MonObservation {
            trace: Some(MonTrace {
                seed: normalize(&seed.body),
                updates: mine.map(|e| normalize(&e.body)).collect(),
            }),
            errors: Vec::new(),
        },
        // `subscribe` returns only once every PV has seeded, so this is
        // unreachable in practice — but it is not an empty trace either, and the
        // two must not be confused.
        None => MonObservation {
            trace: None,
            errors: Vec::new(),
        },
    }
}

/// Replace each timestamp reading with the contract class it belongs to.
///
/// See the module docs for why this one normalization is earned and how far it
/// goes: the instant is dropped, the promise is kept.
fn normalize(body: &str) -> String {
    body.lines()
        .map(|line| {
            let Some((lhs, rhs)) = line.split_once(" = ") else {
                return line.to_string();
            };
            let leaf = lhs.trim();
            let class = if leaf.starts_with("timeStamp.secondsPastEpoch") {
                if rhs == EPICS_EPOCH_POSIX_SECONDS {
                    "<epics-epoch: never processed>"
                } else {
                    "<wall-clock>"
                }
            } else if leaf.starts_with("timeStamp.nanoseconds") {
                if rhs == "0" { "<zero>" } else { "<sub-second>" }
            } else {
                return line.to_string();
            };
            format!("{lhs} = {class}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn errored_cases(refs: &[CaseRef], errors: &[ToolError]) -> Vec<MonCase> {
    refs.iter()
        .map(|cr| MonCase {
            record_type: cr.record_type.clone(),
            field: "VAL".into(),
            pv: cr.pv.clone(),
            drive: cr.drive,
            verdict: Verdict::Errored,
            c_side: MonObservation::default(),
            rust_side: MonObservation::default(),
            differences: Vec::new(),
            allowlisted: Vec::new(),
            errors: errors.to_vec(),
            db: cr.db.clone(),
        })
        .collect()
}

/// The verdict for one case, from the two sides' observations.
///
/// The order of the checks is the policy, and it is the read phase's:
///
/// 1. **Either side unmeasured => ERROR.** Checked first, so a case that could
///    not run can never score agreement. Two sides that both failed have not
///    agreed — nothing was measured, so there is nothing to agree about. A
///    refused drive lands here, which is what stops `sel.VAL` from scoring
///    AGREED on two servers that both posted nothing.
/// 2. No differences => AGREED.
/// 3. Anything left => DEFECT.
///
/// A difference is EXPECTED DEVIATION only when a NOT-REPRODUCED row justifies
/// it — the same contract as [`crate::pvaread::adjudicate`]. The monitor SEED
/// carries the same `getProperties` leaves a read does, so CBUG-G1's
/// `display.precision` add shows on `MonSurface::SeedEvent` for the family VAL a
/// monitor drives (`transform.VAL`); the row (surface `seed_event`) justifies
/// it. As on the read side, a case is EXPECTED DEVIATION only if EVERY
/// difference is justified — one unjustified diff makes it a DEFECT.
pub fn adjudicate(
    cr: &CaseRef,
    c: &MonObservation,
    r: &MonObservation,
    allowlist: &mut Allowlist,
) -> MonCase {
    let mut errors: Vec<ToolError> = Vec::new();
    errors.extend(c.errors.iter().cloned());
    errors.extend(r.errors.iter().cloned());

    let mut case = MonCase {
        record_type: cr.record_type.clone(),
        field: "VAL".into(),
        pv: cr.pv.clone(),
        drive: cr.drive,
        verdict: Verdict::Errored,
        c_side: c.clone(),
        rust_side: r.clone(),
        differences: Vec::new(),
        allowlisted: Vec::new(),
        errors,
        db: cr.db.clone(),
    };

    // Both sides complete is checked BEFORE anything is compared, so a case that
    // could not run cannot reach the agreement path below.
    if !c.is_complete() || !r.is_complete() {
        return case;
    }
    let (Some(ct), Some(rt)) = (&c.trace, &r.trace) else {
        return case;
    };

    let ctx = MatchContext {
        record_type: &cr.record_type,
        field: "VAL",
        dbf: cr.val_dbf,
        class: None,
    };
    // Record what this case looked at before the agreed early-out, so a row that
    // stopped firing reads as stale, not unexercised.
    allowlist.note_compared_pva(
        &ctx,
        &[
            MonSurface::SeedEvent.as_str(),
            MonSurface::EventCount.as_str(),
            MonSurface::UpdateEvents.as_str(),
        ],
    );

    if ct.seed != rt.seed {
        case.differences.push(MonDifference {
            surface: MonSurface::SeedEvent,
            reference: ct.seed.clone(),
            observed: rt.seed.clone(),
        });
    }
    if ct.updates.len() != rt.updates.len() {
        case.differences.push(MonDifference {
            surface: MonSurface::EventCount,
            reference: format!("{} update(s) after the seed", ct.updates.len()),
            observed: format!("{} update(s) after the seed", rt.updates.len()),
        });
    }
    if ct.updates != rt.updates {
        case.differences.push(MonDifference {
            surface: MonSurface::UpdateEvents,
            reference: render_updates(&ct.updates),
            observed: render_updates(&rt.updates),
        });
    }

    if case.differences.is_empty() {
        case.verdict = Verdict::Agreed;
        return case;
    }

    let hits: Vec<Option<String>> = case
        .differences
        .iter()
        .map(|d| allowlist.match_pva_diff(&ctx, d.surface.as_str(), &d.reference, &d.observed))
        .collect();
    let all_justified = hits.iter().all(Option::is_some);
    case.allowlisted = hits.into_iter().flatten().collect();
    case.verdict = if all_justified {
        Verdict::ExpectedDeviation
    } else {
        Verdict::Defect
    };
    case
}

/// The update sequence as one comparable block, numbered so a diff of two
/// sequences of different length stays readable.
fn render_updates(updates: &[String]) -> String {
    if updates.is_empty() {
        return "<no update posted>".to_string();
    }
    updates
        .iter()
        .enumerate()
        .map(|(i, u)| format!("--- update {} ---\n{u}", i + 1))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Assemble the report from the cases and the surface they were drawn from.
pub fn report(
    dbd_path: &str,
    surface: &Surface,
    cases: Vec<MonCase>,
    allowlist: &Allowlist,
) -> MonReport {
    let measured = cases
        .iter()
        .filter(|c| c.verdict != Verdict::Errored)
        .count();
    let stale = |rows: Vec<&crate::allowlist::Deviation>| -> Vec<StaleRow> {
        rows.into_iter().map(StaleRow::of).collect()
    };

    let of_status = |want: ValStatus| -> Vec<String> {
        surface
            .covered_types
            .iter()
            .filter(|rt| val_status(surface, rt) == want)
            .cloned()
            .collect()
    };
    let with_val: Vec<&String> = surface
        .covered_types
        .iter()
        .filter(|rt| drives_val(surface, rt))
        .collect();
    let excluded_scanned: Vec<String> = with_val
        .iter()
        .filter(|rt| !scan_is_measurable(rt))
        .map(|rt| rt.to_string())
        .collect();
    let val_channels = surface
        .covered_types
        .iter()
        .filter(|rt| surface.fields_of(rt).any(|f| f.field.name == "VAL"))
        .count();

    // The candidate count over the WHOLE surface, not over the cases this run
    // happened to produce: a `--record-types` filter must show as LOW coverage
    // rather than as a full sweep of a smaller world. Same rule as the read
    // phase's denominator.
    let candidates: usize = with_val.iter().map(|rt| drives_for(rt).len()).sum();

    MonReport {
        denominator: MonDenominator {
            dbd: dbd_path.to_string(),
            channels_in_surface: surface.denominator(),
            record_types_with_val: with_val.len(),
            excluded_noaccess_val: of_status(ValStatus::NoChannel),
            excluded_nomod_val: of_status(ValStatus::NoMod),
            excluded_unwritable_val: of_status(ValStatus::NotWritable),
            excluded_scanned,
            excluded_non_val_channels: surface.denominator().saturating_sub(val_channels),
        },
        case_coverage: Coverage {
            enumerated: candidates,
            measured,
            errored: cases.len().saturating_sub(measured),
        },
        counts: Counts::tally_verdicts(cases.iter().map(|c| c.verdict)),
        stale_allowlist_rows: stale(allowlist.stale_rows()),
        unexercised_allowlist_rows: stale(allowlist.unexercised_rows()),
        fired_allowlist_rows: allowlist.fired_rows().iter().cloned().collect(),
        cases,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ntshape::ENUM_T;

    fn cref(drive: Drive) -> CaseRef {
        CaseRef {
            record_type: "ai".into(),
            pv: drive.pv("ai"),
            val_dbf: DbfType::Double,
            drive,
            db: String::new(),
        }
    }

    /// Adjudicate against an empty allowlist — pre-allowlist behaviour, so every
    /// difference is a DEFECT.
    fn adj(cr: &CaseRef, c: &MonObservation, r: &MonObservation) -> MonCase {
        adjudicate(cr, c, r, &mut Allowlist::empty())
    }

    fn obs(seed: &str, updates: &[&str]) -> MonObservation {
        MonObservation {
            trace: Some(MonTrace {
                seed: seed.to_string(),
                updates: updates.iter().map(|u| u.to_string()).collect(),
            }),
            errors: Vec::new(),
        }
    }

    fn tool_error(message: &str) -> ToolError {
        ToolError {
            side: Side::Rust,
            tool: "pvxput".into(),
            message: message.into(),
        }
    }

    /// CBUG-G1 on the monitor seed: `transform.VAL`'s seed differs from pvxs by
    /// the one `display.precision` line the port serves and pvxs drops. With the
    /// row loaded (surface `seed_event`) the case is EXPECTED DEVIATION; the
    /// update stream is identical, so nothing else is at stake.
    #[test]
    fn cbug_g1_seed_precision_add_is_expected_deviation() {
        let al_text = "schema = 1\n\
            [[deviation]]\n\
            id = \"CBUG-G1\"\n\
            bucket = \"NOT-REPRODUCED\"\n\
            record_types = [\"transform\"]\n\
            surface = [\"seed_event\"]\n\
            port_adds_leaves = [\"display.precision\"]\n\
            why = \"port serves precision pvxs drops on the seed\"\n";
        let mut al = Allowlist::parse(al_text).expect("valid allowlist");
        let cr = CaseRef {
            record_type: "transform".into(),
            pv: "ORACLE:MON:TRANSFORM".into(),
            val_dbf: DbfType::Double,
            drive: Drive::Passive,
            db: String::new(),
        };
        let c = obs(
            "value double = 0\ncontrol.limitHigh double = 0",
            &["value double = 1"],
        );
        let r = obs(
            "control.limitHigh double = 0\ndisplay.precision int32_t = 0\nvalue double = 0",
            &["value double = 1"],
        );
        let case = adjudicate(&cr, &c, &r, &mut al);
        assert_eq!(case.verdict, Verdict::ExpectedDeviation);
        assert_eq!(case.allowlisted, vec!["CBUG-G1".to_string()]);
        assert!(al.fired_rows().contains("CBUG-G1"));
    }

    /// The rule the whole harness rests on, at this phase's most dangerous
    /// point. A refused drive posts no event on EITHER side, so the two traces
    /// come out identical — and scoring that AGREED would be a false clean on an
    /// experiment that never ran. (`SPC_NOMOD` fields no longer reach here; this
    /// is the net under every refusal the `.dbd` does not predict.)
    #[test]
    fn a_refused_drive_is_an_error_even_though_both_sides_look_identical() {
        let mut c = obs("value double = 0", &[]);
        let mut r = obs("value double = 0", &[]);
        c.errors.push(tool_error(
            "ORACLE:MON:SEL <- 1: Attempt to modify noMod field",
        ));
        r.errors.push(tool_error(
            "ORACLE:MON:SEL <- 1: Attempt to modify noMod field",
        ));
        assert_eq!(c.trace, r.trace, "the traces really are identical");

        let case = adj(&cref(Drive::Passive), &c, &r);
        assert_eq!(
            case.verdict,
            Verdict::Errored,
            "two sides that both failed have NOT agreed"
        );
    }

    /// A ground truth that legitimately posts nothing (`fanout`, `seq`, `sub` —
    /// measured: the put is accepted, no event follows) is a real measurement,
    /// and a port matching it really has agreed. The converse of the test above,
    /// and the reason the drive's own error is what separates them.
    #[test]
    fn an_accepted_drive_that_posts_nothing_on_both_sides_is_agreement() {
        let case = adj(
            &cref(Drive::Passive),
            &obs("value double = 0", &[]),
            &obs("value double = 0", &[]),
        );
        assert_eq!(case.verdict, Verdict::Agreed);
    }

    #[test]
    fn a_side_that_never_produced_a_trace_is_an_error() {
        let case = adj(
            &cref(Drive::Scanned),
            &obs("value double = 0", &["value double = 1"]),
            &MonObservation::default(),
        );
        assert_eq!(case.verdict, Verdict::Errored);
    }

    /// The signature this phase exists to measure: both sides post one update,
    /// but the port frames every leaf where QSRV2 frames only what changed.
    /// The event COUNT agrees, so only a text diff can see it.
    #[test]
    fn an_update_that_frames_extra_leaves_is_a_defect_on_update_events_alone() {
        let c = obs(
            "    value double = 0",
            &["    value double = 1.5\n    timeStamp.secondsPastEpoch int64_t = 1784213743"],
        );
        let r = obs(
            "    value double = 0",
            &[
                "    value double = 1.5\n    timeStamp.secondsPastEpoch int64_t = 1784299999\n    display.units string = \"\"\n    control.limitLow double = 0",
            ],
        );
        let case = adj(&cref(Drive::Scanned), &c, &r);
        assert_eq!(case.verdict, Verdict::Defect);
        let surfaces: Vec<MonSurface> = case.differences.iter().map(|d| d.surface).collect();
        assert_eq!(
            surfaces,
            [MonSurface::UpdateEvents],
            "the counts agree and the seeds agree; only the framing differs"
        );
    }

    /// A port that posts no update where C posts three differs on the count as
    /// well as the text, and the two are named apart so a missing event is never
    /// reported as a framing difference.
    #[test]
    fn a_missing_update_is_a_defect_on_the_count_and_the_text() {
        let case = adj(
            &cref(Drive::Passive),
            &obs("s", &["u1", "u2", "u3"]),
            &obs("s", &[]),
        );
        assert_eq!(case.verdict, Verdict::Defect);
        let surfaces: Vec<MonSurface> = case.differences.iter().map(|d| d.surface).collect();
        assert!(surfaces.contains(&MonSurface::EventCount));
        assert!(surfaces.contains(&MonSurface::UpdateEvents));
        assert!(!surfaces.contains(&MonSurface::SeedEvent));
    }

    /// Two IOCs process at different instants, so an un-normalized timestamp
    /// would score every case DEFECT for a difference the harness created.
    #[test]
    fn two_live_timestamps_from_different_instants_compare_equal() {
        let c = "    timeStamp.secondsPastEpoch int64_t = 1784213743\n    timeStamp.nanoseconds int32_t = 19204686";
        let r = "    timeStamp.secondsPastEpoch int64_t = 1784299999\n    timeStamp.nanoseconds int32_t = 777";
        assert_eq!(normalize(c), normalize(r));
    }

    /// ...but the normalization keeps the leaf's contract, so a port that never
    /// stamps the record still differs from a ground truth that did.
    #[test]
    fn an_undefined_timestamp_never_launders_into_a_live_one() {
        let never = "    timeStamp.secondsPastEpoch int64_t = 631152000";
        let live = "    timeStamp.secondsPastEpoch int64_t = 1784213743";
        assert_ne!(normalize(never), normalize(live));
        assert!(normalize(never).contains("never processed"));
    }

    /// A port with no sub-second resolution differs from one with it — the
    /// nanoseconds leaf is classified, not erased.
    #[test]
    fn a_zero_nanoseconds_field_is_distinguishable_from_a_real_one() {
        assert_ne!(
            normalize("    timeStamp.nanoseconds int32_t = 0"),
            normalize("    timeStamp.nanoseconds int32_t = 19204686")
        );
    }

    /// Nothing but the timestamp is normalized: the value and the alarm are the
    /// measurement.
    #[test]
    fn no_other_leaf_is_normalized() {
        let body = "    value double = 1.5\n    alarm.status int32_t = 2\n    display.units string = \"V\"";
        assert_eq!(normalize(body), body);
    }

    /// `NTEnum`'s `value` is a struct: a bare scalar cannot be assigned to it
    /// (pvxs: "Unable to assign struct with String" — and exit 0).
    #[test]
    fn an_enum_channel_is_driven_through_its_index() {
        let enum_shape = NtShape {
            type_id: NT_ENUM.to_string(),
            value: ENUM_T.to_string(),
        };
        assert_eq!(assignment(&enum_shape, "2"), "value.index=2");
    }

    /// ...and a scalar takes the bare value. `mbbo.VAL` declares DBF_ENUM but
    /// its cvt_dbaddr serves an NTScalar, which is why the shape comes from
    /// ground truth rather than from the .dbd.
    #[test]
    fn a_scalar_channel_is_driven_with_a_bare_value() {
        let scalar = NtShape {
            type_id: crate::ntshape::NT_SCALAR.to_string(),
            value: "uint16_t".to_string(),
        };
        assert_eq!(assignment(&scalar, "2"), "2");
    }

    /// The three types whose own scanning posts events nobody drove. They keep
    /// their passive reproducer — the exclusion is of one reproducer, not of the
    /// record type.
    #[test]
    fn the_self_posting_types_get_no_scanned_reproducer_but_keep_a_passive_one() {
        for rt in ["event", "sseq", "asyn"] {
            assert!(!scan_is_measurable(rt), "{rt}");
            assert_eq!(drives_for(rt), [Drive::Passive], "{rt}");
        }
        assert_eq!(drives_for("ai"), [Drive::Passive, Drive::Scanned]);
    }

    /// Both reproducers live in one `.db` and one subscription, so their names
    /// must not collide.
    #[test]
    fn the_two_reproducers_are_different_records() {
        assert_ne!(Drive::Passive.pv("ai"), Drive::Scanned.pv("ai"));
        assert!(Drive::Scanned.fields().iter().any(|(k, _)| *k == "SCAN"));
        assert!(Drive::Passive.fields().is_empty());
    }

    /// A put spaced closer than the scan period could be coalesced into one
    /// scan, making the event count a function of timing rather than of the
    /// server.
    #[test]
    fn puts_are_spaced_wider_than_the_scan_period() {
        assert_eq!(SCAN_RATE, ".1 second");
        assert!(
            PUT_SPACING >= Duration::from_millis(300),
            "a put must not race the next scan: {PUT_SPACING:?}"
        );
    }
}
