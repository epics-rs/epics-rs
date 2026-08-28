//! The differential oracle, as a command.
//!
//! Exit code is the verdict, and the rule lives in one place for every phase:
//! [`epics_oracle_rs::report::run_failures`]. Non-zero for any DEFECT, any
//! ERROR, and any record type the port could not implement. An unmeasurable
//! case fails the run exactly like a wrong one, because a harness that exits 0
//! when it could not look is the thing that produced 21 false-clean verdicts in
//! the audit loop.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, ValueEnum};
use epics_oracle_rs::allowlist::Allowlist;
use epics_oracle_rs::dbd::Dbd;
use epics_oracle_rs::ioc::{CTools, PvxTools};
use epics_oracle_rs::report::CaseResult;
use epics_oracle_rs::report::{Counts, Denominator, Report, StaleRow, exit_status, run_failures};
use epics_oracle_rs::runner::{Runner, select_types, workdir};
use epics_oracle_rs::surface::{Surface, probe_supported_record_types};
use epics_oracle_rs::{pvamonitor, pvaread};

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum Phase {
    /// Native type, element count, access rights, value (string + numeric).
    Read,
    /// Boundary-value puts: accept/reject, stored value, STAT/SEVR.
    Put,
    /// Monitor event sequence and count.
    Monitor,
    /// Array put-and-readback across element-count boundaries (zero-length,
    /// single, partial, exactly NELM, one past NELM) for record types whose
    /// VAL is a bounded
    /// array (waveform/aai/aao/subArray). Part of `All`: same C ground truth,
    /// same CA tools, same allowlist — it just reaches the `DBF_NOACCESS` array
    /// VAL the scalar phases exclude.
    Array,
    /// **PVA** read: every channel of the `.dbd` surface on pvxs QSRV2
    /// (`softIocPVX`) and on `oracle-ioc --pva`, on two contracts — the
    /// declared type (`pvxinfo`) and the value+marking (`pvxget`).
    ///
    /// Deliberately outside `All`. It shares the CA phases' denominator (QSRV2
    /// serves base's dbChannel namespace) but nothing else: a different ground
    /// truth (`softIocPVX`, not base's `softIoc`), a different instrument
    /// (`pvxget`, not `caget`), and no allowlist — the expected-deviation rows
    /// are about C's CA behaviour and justify nothing about QSRV2. Folding it
    /// into `All` would merge two populations of cases whose verdicts are not
    /// comparable, into one set of counts.
    PvaRead,
    /// **PVA** monitor: subscribe on both sides, drive one value sequence, and
    /// diff the events each server posted — seed text, update text, event count.
    ///
    /// Outside `All` for the same reason as [`Phase::PvaRead`], and its
    /// denominator is narrower still: the `VAL` channel of each record type, on
    /// two reproducers (passive and scanned). A monitor case needs a *drive*,
    /// and a put processes the record — so sibling channels cannot be driven
    /// concurrently and attributed. See [`epics_oracle_rs::pvamonitor`].
    PvaMonitor,
    All,
}

#[derive(Parser)]
#[command(
    name = "oracle",
    about = "Differential oracle: boots the C softIoc and the Rust IOC on the same .db and diffs observable CA behavior"
)]
struct Args {
    /// The expanded dbd that supplies the denominator.
    #[arg(long, default_value_os_t = CTools::dbd_path())]
    dbd: PathBuf,

    /// Which probes to run.
    #[arg(long, value_enum, default_value_t = Phase::All)]
    phase: Phase,

    /// Restrict to these record types (default: every type the port implements).
    #[arg(long, value_delimiter = ',')]
    record_types: Option<Vec<String>>,

    /// Cap the put cases per record type. Use for a fast pass; the report still
    /// states the true denominator, so a capped run shows as LOW coverage
    /// rather than as a full sweep.
    #[arg(long)]
    max_put_cases: Option<usize>,

    /// Write the machine-readable report here.
    #[arg(long)]
    json: Option<PathBuf>,

    /// Expected-deviation allowlist (defaults to the shipped file).
    #[arg(long)]
    allowlist: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("oracle: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<ExitCode, String> {
    let args = Args::parse();

    if args.phase == Phase::PvaRead {
        return run_pva_read(&args).await;
    }
    if args.phase == Phase::PvaMonitor {
        return run_pva_monitor(&args).await;
    }

    // Ground truth must exist. Without it there is nothing to diff against, and
    // pretending otherwise would be the worst possible failure mode.
    let tools = CTools::discover().map_err(|e| e.to_string())?;
    let dbd = Dbd::parse_file(&args.dbd)?;

    let mut allowlist = match &args.allowlist {
        Some(p) => Allowlist::load(p)?,
        None => Allowlist::load(&Allowlist::default_path())?,
    };

    // Which record types the port implements is MEASURED, not read out of its
    // source -- the field tables are being regenerated concurrently, so source
    // would be stale.
    eprintln!("probing which record types the port implements...");
    let supported = probe_supported_record_types(&dbd).await?;
    let surface = Surface::build(&dbd, &supported);
    let types = select_types(&surface, &args.record_types);

    eprintln!(
        "denominator: {} CA-observable fields across {} record types ({} unimplemented)",
        surface.denominator(),
        surface.covered_types.len(),
        surface.unimplemented_types.len()
    );

    let runner = Runner::new(tools, dbd, workdir(None)?);
    let mut cases: Vec<CaseResult> = Vec::new();
    let drives_puts = matches!(args.phase, Phase::Put | Phase::Monitor | Phase::All);

    for (i, rt) in types.iter().enumerate() {
        eprintln!("[{}/{}] {rt}", i + 1, types.len());

        if matches!(args.phase, Phase::Read | Phase::All) {
            cases.extend(runner.probe_reads(rt, &surface, &mut allowlist));
        }
        // One predicate for every phase that drives a put, announced once.
        // The monitor phase stimulates its subscription with puts, so a record
        // whose puts cannot complete cannot be monitored either: its trace
        // would be empty on both sides and score AGREED on an experiment that
        // never ran. Loud and by policy — NOT a silent false-clean — so a clean
        // `--phase all` exit never implies these were measured.
        let drivable = epics_oracle_rs::puts_are_measurable(rt);
        if drives_puts && !drivable {
            eprintln!(
                "    put-driven phases (put, monitor) skipped for {rt}: read-only \
                 (unmeasurable against the disconnected ORACLEASYN port)"
            );
        }

        if matches!(args.phase, Phase::Put | Phase::All) && drivable {
            cases.extend(runner.probe_puts(rt, &surface, &mut allowlist, args.max_put_cases));
        }
        if matches!(args.phase, Phase::Monitor | Phase::All) && drivable {
            cases.extend(runner.probe_monitor(rt, &surface, &mut allowlist));
        }
        if matches!(args.phase, Phase::Array | Phase::All) {
            cases.extend(runner.probe_array(rt, &mut allowlist));
        }
    }

    // Field coverage counts the READ probe and nothing else, by the phase each
    // case records; see `report::field_coverage` for why that has to be the
    // recorded phase and not the absence of a boundary class.
    let field_coverage = epics_oracle_rs::report::field_coverage(&cases, surface.denominator());

    let counts = Counts::tally(&cases);
    counts.check()?;

    let stale: Vec<StaleRow> = allowlist
        .stale_rows()
        .into_iter()
        .map(StaleRow::of)
        .collect();
    let unexercised: Vec<StaleRow> = allowlist
        .unexercised_rows()
        .into_iter()
        .map(StaleRow::of)
        .collect();
    let fired: Vec<String> = allowlist.fired_rows().iter().cloned().collect();

    let report = Report {
        denominator: Denominator {
            dbd: args.dbd.display().to_string(),
            record_types_in_dbd: surface.covered_types.len() + surface.unimplemented_types.len(),
            record_types_covered: surface.covered_types.clone(),
            record_types_unimplemented: surface.unimplemented_types.clone(),
            observable_fields: surface.denominator(),
            excluded_noaccess_fields: surface.excluded_noaccess,
        },
        field_coverage,
        counts,
        stale_allowlist_rows: stale,
        unexercised_allowlist_rows: unexercised,
        fired_allowlist_rows: fired,
        cases,
    };

    if let Some(p) = &args.json {
        std::fs::write(p, report.to_json()).map_err(|e| format!("write {}: {e}", p.display()))?;
        eprintln!("wrote {}", p.display());
    }
    println!("{}", report.human());

    Ok(verdict_exit(
        &report.counts,
        &surface.unimplemented_types,
        &report.stale_allowlist_rows,
    ))
}

/// The exit code, from the single owner in [`run_failures`].
///
/// Every phase ends here so the rule cannot diverge between them; each failure
/// reason is printed by name, because a bare non-zero exit tells a CI log
/// nothing about which finding produced it.
fn verdict_exit(counts: &Counts, unimplemented: &[String], stale: &[StaleRow]) -> ExitCode {
    let failures = run_failures(counts, unimplemented, stale);
    if failures.is_empty() {
        return ExitCode::from(exit_status(&failures));
    }
    eprintln!("oracle: run FAILED:");
    for f in &failures {
        eprintln!("  - {f}");
    }
    ExitCode::from(exit_status(&failures))
}

/// The PVA read phase. Its own path rather than a branch inside the CA run: it
/// has a different ground truth (pvxs `softIocPVX`, not base's `softIoc`), a
/// different instrument (`pvxget`/`pvxinfo`, not `caget`), and no allowlist, so
/// the CA allowlist/report machinery above has nothing to say about it.
///
/// What it *does* share is the denominator, and that is not a convenience: QSRV2
/// hands channel names straight to base's `dbChannelTest`
/// (`singlesource.cpp:469`), so its namespace is exactly the `record.FIELD` set
/// [`Surface`] already enumerates from the `.dbd`.
async fn run_pva_read(args: &Args) -> Result<ExitCode, String> {
    // Ground truth must exist. Without it there is nothing to diff against, and
    // pretending otherwise would be the worst possible failure mode.
    let tools = PvxTools::discover().map_err(|e| e.to_string())?;
    let dbd = Dbd::parse_file(&args.dbd)?;

    // Which record types the port implements is MEASURED, not read out of its
    // source -- the same probe the CA phases use, for the same reason.
    eprintln!("probing which record types the port implements...");
    let supported = probe_supported_record_types(&dbd).await?;
    let surface = Surface::build(&dbd, &supported);
    let types = select_types(&surface, &args.record_types);

    eprintln!(
        "denominator: {} PVA channels across {} record types ({} unimplemented)",
        surface.denominator(),
        surface.covered_types.len(),
        surface.unimplemented_types.len()
    );

    let mut allowlist = match &args.allowlist {
        Some(p) => Allowlist::load(p)?,
        None => Allowlist::load(&Allowlist::default_path())?,
    };
    let cases = pvaread::probe(&tools, &workdir(None)?, &surface, &types, &mut allowlist);
    let report = pvaread::report(&args.dbd.display().to_string(), &surface, cases, &allowlist);
    report.counts.check()?;

    if let Some(p) = &args.json {
        std::fs::write(p, report.to_json()).map_err(|e| format!("write {}: {e}", p.display()))?;
        eprintln!("wrote {}", p.display());
    }
    println!("{}", report.human());

    Ok(verdict_exit(
        &report.counts,
        &surface.unimplemented_types,
        &report.stale_allowlist_rows,
    ))
}

/// The PVA monitor phase: the analogue of CA's Phase C, one protocol over.
async fn run_pva_monitor(args: &Args) -> Result<ExitCode, String> {
    // Ground truth must exist. Without it there is nothing to diff against.
    let tools = PvxTools::discover().map_err(|e| e.to_string())?;
    let dbd = Dbd::parse_file(&args.dbd)?;

    eprintln!("probing which record types the port implements...");
    let supported = probe_supported_record_types(&dbd).await?;
    let surface = Surface::build(&dbd, &supported);
    let types = select_types(&surface, &args.record_types);

    eprintln!(
        "denominator: the VAL channel of each of {} record types, on 2 reproducers \
         (the {} channels of the read surface are NOT all driven -- see the report)",
        surface.covered_types.len(),
        surface.denominator()
    );

    let mut allowlist = match &args.allowlist {
        Some(p) => Allowlist::load(p)?,
        None => Allowlist::load(&Allowlist::default_path())?,
    };
    let cases = pvamonitor::probe(&tools, &workdir(None)?, &surface, &types, &mut allowlist);
    let report = pvamonitor::report(&args.dbd.display().to_string(), &surface, cases, &allowlist);
    report.counts.check()?;

    if let Some(p) = &args.json {
        std::fs::write(p, report.to_json()).map_err(|e| format!("write {}: {e}", p.display()))?;
        eprintln!("wrote {}", p.display());
    }
    println!("{}", report.human());

    Ok(verdict_exit(
        &report.counts,
        &surface.unimplemented_types,
        &report.stale_allowlist_rows,
    ))
}
