//! End-to-end: boot the real **PVA** differential pair (pvxs QSRV2's
//! `softIocPVX` vs `oracle-ioc --pva`) and check the harness's own invariants
//! against it.
//!
//! Like `tests/oracle.rs`, these assert properties of the **harness**, not of
//! the port — with one deliberate exception noted on
//! [`at_least_one_channel_agrees`]. The property the harness stakes its
//! credibility on is that it never turns "I could not measure this" into "this
//! agreed", and that is what is pinned here.
//!
//! # Why these run on one record type, not on the denominator
//!
//! The binary sweeps the full surface; these tests sweep `bi`. The cost is
//! entirely in booting the pair — a batched read of a whole record type's
//! channels takes ~30ms per tool per side, but a pair boot takes ~6.6s, most of
//! it the two mandatory `pvxlist` exclusivity proofs ([`PvaPair::boot`]). Forty
//! record types is fine for a deliberate run and not for `cargo nextest`. The
//! invariants pinned here are per-channel and per-case rules that do not become
//! more true at 40 types than at one.
//!
//! # What is deliberately NOT pinned
//!
//! No absolute defect count. The port's PVA divergences are being fixed, so a
//! pinned count would fail on every improvement and teach the next person to
//! edit the number rather than read it. These pin the harness's invariants —
//! the pair boots, the denominator is the `.dbd` surface, an unmeasured channel
//! is an ERROR, and at least one channel can reach AGREED.
//!
//! Requires the built pvxs tree (`PVXS_BIN`, or the default path). If it is
//! absent the tests fail loudly rather than skipping — a silently skipped
//! oracle is exactly the false-clean this project exists to escape.
//!
//! # Five of these have no subject on the reactor-free backend
//!
//! The Rust side is `oracle-ioc --pva`, and it refuses to start under
//! `exec_backend` — the reactor-free backend, selected on a host build by
//! `EPICS_RS_BUILD_EXEC_BACKEND=thread` and unconditionally on RTEMS and
//! VxWorks. The five that reach `PvaPair::boot`, through `boot` or through
//! `sweep`'s real phase, cannot pass there under any treatment and are gated
//! rather than selected and failed. The four that stay assert per-case
//! accounting that holds over an all-ERROR report. `[[test]]
//! required-features` cannot name a build-script cfg, which is why the gates
//! are on the items.

#[cfg(tokio_backend)]
use std::collections::BTreeMap;
use std::collections::BTreeSet;

use epics_oracle_rs::dbd::Dbd;
use epics_oracle_rs::diff::Verdict;
use epics_oracle_rs::ioc::{CTools, PvxTools, Side, alloc_free_port};
#[cfg(tokio_backend)]
use epics_oracle_rs::ioc::{Ioc, PvaPair};
use epics_oracle_rs::pvaread::{self, PvaReport};
use epics_oracle_rs::pvatool::PvaTools;
use epics_oracle_rs::runner::workdir;
use epics_oracle_rs::surface::Surface;

/// The record type these tests sweep. `bi` is a `DBR_ENUM` channel type, whose
/// `VAL` QSRV2 projects as `NTEnum` (`singlesource.cpp:201`).
const RT: &str = "bi";

fn tools() -> PvxTools {
    PvxTools::discover().expect(
        "the pvxs tree must be built for the PVA oracle to have ground truth; \
         set PVXS_BIN if it is not at the default path",
    )
}

/// The surface restricted to one record type, built from the **real** `.dbd`.
///
/// `supported` is supplied rather than probed: `probe_supported_record_types`
/// boots every type in the dbd, which is the binary's job, not a unit's. The
/// denominator under test is still the `.dbd`'s — that is the claim being
/// pinned — just narrowed to the one type these tests can afford to boot.
fn surface_of(record_type: &str) -> Surface {
    let dbd = Dbd::parse_file(&CTools::dbd_path()).unwrap_or_else(|e| {
        panic!("the fat dbd must be readable for the denominator to exist: {e}")
    });
    let supported: BTreeSet<String> = [record_type.to_string()].into_iter().collect();
    Surface::build(&dbd, &supported)
}

/// Sweep one record type through the real phase and report on it.
/// The same sweep over a surface cut down to named fields.
///
/// The `.dbd` is the denominator everywhere else in this crate for good reason,
/// and this is the one place a smaller one is right. What a sweep costs is not
/// its reads: it is two boots of a re-proven replacement server, measured at
/// 3.6 s each, for every channel that kills one. So the caller keeps ONE killer
/// plus the channels its assertions name, and `scalcout`'s whole surface stops
/// spending two minutes on a fallback eleven channels exercise identically. The
/// record the IOCs load is untouched and still has all 171 fields.
fn sweep_fields(record_type: &str, keep: &[&str]) -> (Surface, PvaReport) {
    let dbd = Dbd::parse(&trimmed_recordtype(record_type, keep))
        .unwrap_or_else(|e| panic!("the trimmed {record_type} recordtype must parse: {e}"));
    let supported: BTreeSet<String> = [record_type.to_string()].into_iter().collect();
    sweep_surface(record_type, Surface::build(&dbd, &supported))
}

/// One `recordtype(...)` block from the fat `.dbd`, carrying only `keep`'s
/// fields — verbatim, so every type, size and `special` is the real one.
fn trimmed_recordtype(record_type: &str, keep: &[&str]) -> String {
    let text = std::fs::read_to_string(CTools::dbd_path()).expect("the fat dbd must be readable");
    let head = format!("recordtype({record_type}) {{");
    let start = text
        .find(&head)
        .unwrap_or_else(|| panic!("no {head} in the dbd"));
    let mut out = format!("{head}\n");
    let mut depth = 1usize;
    let mut wanted = false;
    let mut found: BTreeSet<&str> = BTreeSet::new();
    for line in text[start..].lines().skip(1) {
        if depth == 1 {
            if line.trim() == "}" {
                break;
            }
            wanted = line
                .trim()
                .strip_prefix("field(")
                .and_then(|r| r.split(',').next())
                .is_some_and(|f| {
                    let hit = keep.contains(&f);
                    if hit {
                        found.insert(keep.iter().find(|k| **k == f).copied().unwrap());
                    }
                    hit
                });
        }
        if wanted {
            out.push_str(line);
            out.push('\n');
        }
        depth = depth + line.matches('{').count() - line.matches('}').count();
        if depth == 1 {
            wanted = false;
        }
    }
    out.push_str("}\n");
    let missing: Vec<&&str> = keep.iter().filter(|k| !found.contains(**k)).collect();
    assert!(
        missing.is_empty(),
        "the dbd declares no {record_type} field(s) {missing:?} — the trim would silently \
         shrink the surface instead of failing"
    );
    out
}

fn sweep(record_type: &str) -> (Surface, PvaReport) {
    sweep_surface(record_type, surface_of(record_type))
}

fn sweep_surface(record_type: &str, surface: Surface) -> (Surface, PvaReport) {
    let mut allowlist = epics_oracle_rs::allowlist::Allowlist::load(
        &epics_oracle_rs::allowlist::Allowlist::default_path(),
    )
    .expect("load shipped allowlist");
    let cases = pvaread::probe(
        &tools(),
        &workdir(None).expect("workdir"),
        &surface,
        &[record_type.to_string()],
        &mut allowlist,
    );
    let report = pvaread::report(
        &CTools::dbd_path().display().to_string(),
        &surface,
        cases,
        &allowlist,
    );
    (surface, report)
}

/// Boot the pair on a one-record `.db` and hand back the pair plus its tools.
#[cfg(tokio_backend)]
fn boot(record_type: &str, rec: &str) -> (PvxTools, PvaPair) {
    let t = tools();
    let dir = workdir(None).expect("workdir");
    let db = dir.join(format!("pva_test_{record_type}.db"));
    std::fs::write(&db, epics_oracle_rs::record_stmt(record_type, rec)).expect("write db");
    let pair = PvaPair::boot(&t, &db, rec).expect("the PVA pair must boot");
    (t, pair)
}

/// The pair boots, and the two sides land on **different** search ports.
///
/// Not a formality. Both sides serve identical PV names from the same `.db`, so
/// one shared port would mean a `pvxget` aimed at one could be answered by the
/// other — and PVA gives no warning when that happens (the search socket sets
/// SO_REUSEPORT, so the collision binds silently).
#[cfg(tokio_backend)]
#[test]
fn the_pva_pair_boots_on_distinct_ports() {
    let (_t, pair) = boot("ai", "ORACLE:AI");
    assert_ne!(pair.c.port(), 0, "pvxs side must report a real search port");
    assert_ne!(
        pair.rust.port(),
        0,
        "Rust side must report a real search port"
    );
    assert_ne!(
        pair.c.port(),
        pair.rust.port(),
        "the two sides must not share a UDP search port — readings would not be attributable",
    );
    assert_eq!(pair.c.side(), Side::C);
    assert_eq!(pair.rust.side(), Side::Rust);
}

/// The read phase runs four clients at once (two sides × `pvxinfo`+`pvxget`),
/// so concurrent client tools must not break each other.
///
/// This is NOT the regression test for the beacon-port fault that made 45
/// channels ERROR — that one was a *server* stealing the client's beacon port,
/// and it is structurally gone (see `PvaTools::run_raw`). Concurrency was the
/// first suspect and was measured innocent: pvxs clients share a beacon port
/// happily, and this test is what established that. It stays because batched
/// reads depend on the property either way.
///
/// No server is booted: pointing at a port nothing serves isolates the client
/// question from the reachability question. Finding no server there is fine;
/// failing to *look* because of the harness's own plumbing is not.
#[test]
fn concurrent_client_tools_do_not_break_each_other() {
    let t = tools();
    let dead = alloc_free_port().expect("a port to aim at");
    let tool = PvaTools::new(&t, dead, Side::C);

    let results: Vec<_> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..8).map(|_| s.spawn(|| tool.servers())).collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("client thread must not panic"))
            .collect()
    });

    for r in &results {
        if let Err(e) = r {
            assert!(
                !e.message.contains("Address already in use") && !e.message.contains("beacon"),
                "a client failed for a reason of the harness's own making: {}",
                e.message,
            );
        }
    }
}

/// Each side is the **only** server on its port, proven by measurement.
///
/// `PvaPair::boot` already refuses to return otherwise; this pins the proof
/// itself, because it is the one guard with no CA analogue and it is invisible
/// when it silently stops working.
#[cfg(tokio_backend)]
#[test]
fn each_side_is_the_sole_server_on_its_port() {
    let (t, pair) = boot(RT, "ORACLE:BI");
    for (port, side) in [(pair.c.port(), Side::C), (pair.rust.port(), Side::Rust)] {
        let seen = PvaTools::new(&t, port, side)
            .servers()
            .unwrap_or_else(|e| panic!("counting servers on the {side} side's port {port}: {e}"));
        assert_eq!(
            seen.len(),
            1,
            "exactly one PVA server must answer on the {side} side's port {port}, \
             pvxlist named: {seen:?}",
        );
    }
}

/// **The denominator is the `.dbd`'s, and every channel of it gets a case.**
///
/// The property that separates this phase from the skeleton it replaced: the
/// case list is derived from the spec, not hand-picked, so coverage is a
/// percentage of a stated number. A case silently dropped — or invented — would
/// break the count both ways.
#[test]
fn every_channel_of_the_dbd_surface_gets_exactly_one_case() {
    let (surface, report) = sweep(RT);
    let enumerated_for_rt = surface.fields_of(RT).count();
    assert!(
        enumerated_for_rt > 1,
        "the surface must be non-trivial, got {enumerated_for_rt}"
    );
    assert_eq!(
        report.counts.ran, enumerated_for_rt,
        "one case per enumerated channel — no channel dropped, none invented",
    );
    // The reported denominator stays the whole `.dbd`'s, so a one-type sweep
    // reports partial coverage rather than 100% of what it chose to run.
    assert_eq!(report.channel_coverage.enumerated, surface.denominator());
    assert!(
        surface.denominator() > report.counts.ran,
        "a one-type sweep must not read as a full sweep"
    );
    report.counts.check().expect("buckets must reconcile");

    let want: BTreeSet<String> = surface
        .fields_of(RT)
        .map(|f| f.field.name.clone())
        .collect();
    let got: BTreeSet<String> = report.cases.iter().map(|c| c.field.clone()).collect();
    assert_eq!(got, want, "the cases ARE the .dbd's fields for this type");
}

/// Coverage counts only channels measured on **both sides** and **both
/// contracts** — and the buckets reconcile with the denominator.
#[test]
fn coverage_counts_only_fully_measured_channels() {
    let (surface, report) = sweep(RT);
    let cov = &report.channel_coverage;
    assert_eq!(
        cov.measured + cov.errored,
        surface.fields_of(RT).count(),
        "every channel VISITED is either measured or errored — there is no third, silent bucket",
    );
    assert_eq!(
        cov.enumerated,
        surface.denominator(),
        "the denominator is the whole .dbd's, not the subset this sweep ran",
    );
    assert_eq!(
        cov.measured,
        report.counts.ran - report.counts.errored,
        "an errored channel is never coverage",
    );

    // Every channel counted as measured really did produce all four readings.
    for c in report
        .cases
        .iter()
        .filter(|c| c.verdict != Verdict::Errored)
    {
        assert!(
            c.c_side.declared_type.is_some() && c.c_side.value.is_some(),
            "{}: counted as measured without both C-side contracts",
            c.pv
        );
        assert!(
            c.rust_side.declared_type.is_some() && c.rust_side.value.is_some(),
            "{}: counted as measured without both port-side contracts",
            c.pv
        );
    }
}

/// The distinct reasons the errored cases give, most-repeated first.
///
/// A count of ERRORs says how much was not measured and never why, and the two
/// causes it conflates need opposite responses. A pair that would not boot
/// stamps ONE `boot` error onto every channel of the type
/// (`pvaread::errored_cases`), so ran=56 errored=56 is a single infrastructure
/// failure; a port that answered wrongly arrives as per-channel
/// `pvxget`/`pvxinfo` errors. Both print the identical counts. Only this text
/// separates "the harness could not look" from "the port is broken", and
/// without it the reader of a failed run has a budget and no cause.
#[cfg(tokio_backend)]
fn why_unmeasured(report: &PvaReport) -> String {
    let mut n: BTreeMap<String, usize> = BTreeMap::new();
    for case in &report.cases {
        for e in &case.errors {
            *n.entry(format!("{} {}: {}", e.side, e.tool, e.message))
                .or_default() += 1;
        }
    }
    if n.is_empty() {
        return "  (no channel recorded an error)".to_string();
    }
    let mut rows: Vec<(usize, String)> = n.into_iter().map(|(text, c)| (c, text)).collect();
    rows.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    rows.iter()
        .map(|(c, text)| format!("  {c}x {text}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Both contracts are really driven, by their own tool, against the real pair.
///
/// The type contract exists so a type gap cannot hide inside a value diff. If
/// `pvxinfo` silently stopped being run, every test above would still pass
/// while the separation quietly became fiction — so the readings are checked to
/// be what those tools actually print.
#[cfg(tokio_backend)]
#[test]
fn each_measured_channel_carries_a_real_type_and_a_real_value_reading() {
    let (_surface, report) = sweep(RT);
    let measured: Vec<_> = report
        .cases
        .iter()
        .filter(|c| c.verdict != Verdict::Errored)
        .collect();
    assert!(
        !measured.is_empty(),
        "nothing was measured at all — the pair or the tools are broken\nwhy \
         nothing was measured:\n{}",
        why_unmeasured(&report),
    );
    for c in &measured {
        let ty = c.c_side.declared_type.as_deref().unwrap_or("");
        assert!(
            ty.starts_with("struct \""),
            "{}: the type contract must carry a pvxinfo struct declaration, got {ty:?}",
            c.pv
        );
        assert!(
            !ty.contains("127.0.0.1"),
            "{}: the pvxinfo header carries the server port and must never reach \
             the compared text — the two sides can never agree on it",
            c.pv
        );
        // pvxget prints `<indent><member> <type> = <value>`; the value contract
        // must be a value reading, not a type declaration.
        let val = c.c_side.value.as_deref().unwrap_or("");
        assert!(
            val.contains(" = "),
            "{}: the value contract must carry a pvxget reading, got {val:?}",
            c.pv
        );
    }
}

/// The shape derived from the `.dbd` matches what **pvxs itself** declares.
///
/// This is the harness auditing its own derivation against ground truth. If it
/// fails, the prediction in `crate::ntshape` is wrong — not the port — and
/// every `port_shape_vs_dbd` finding it produced is suspect.
#[test]
fn the_dbd_derived_shape_matches_the_ground_truth_on_every_measured_channel() {
    let (_surface, report) = sweep(RT);
    let wrong: Vec<String> = report
        .defects()
        .flat_map(|c| {
            c.differences
                .iter()
                .filter(|d| d.surface == pvaread::PvaSurface::GroundTruthShapeVsDbd)
                .map(move |d| {
                    format!(
                        "{}: dbd says {} but pvxs declares {}",
                        c.pv, d.reference, d.observed
                    )
                })
        })
        .collect();
    assert!(
        wrong.is_empty(),
        "the .dbd-derived NT shape disagrees with pvxs on {} channel(s); the \
         derivation is what is wrong, not the port:\n  {}",
        wrong.len(),
        wrong.join("\n  ")
    );
}

/// A channel reads the same on both sides — the harness can produce AGREED, not
/// merely DEFECT and ERROR.
///
/// This is the one test here that asserts something about the **port** rather
/// than the harness, and it is deliberate: a harness that could never return
/// AGREED would pass every other test in this file while being useless. It is a
/// lower bound on purpose — the port's PVA divergences are being fixed
/// concurrently, so any exact count would be wrong by the time it was read.
#[cfg(tokio_backend)]
#[test]
fn at_least_one_channel_agrees() {
    let (_surface, report) = sweep(RT);
    assert!(
        report.counts.agreed > 0,
        "no channel of `{RT}` agreed on both contracts, so this harness cannot \
         distinguish a real defect from a broken instrument. counts: ran={} \
         agreed={} defect={} errored={}\nwhy nothing was measured:\n{}",
        report.counts.ran,
        report.counts.agreed,
        report.counts.defect,
        report.counts.errored,
        why_unmeasured(&report),
    );
}

/// **The rule the whole harness rests on.** A PV that does not exist yields no
/// reading, and no reading is an ERROR — never agreement.
///
/// The trap this guards is precise: both sides fail identically on an unknown
/// PV, so a harness that compared *outcomes* rather than *readings* would find
/// them equal and score AGREED. That is "exit 0 because I could not look", which
/// is exactly what produced the false-clean verdicts this crate replaced.
/// A channel that kills the ground-truth server must cost its batch-mates
/// nothing.
///
/// `scalcout`'s PAA..PLL are `SPC_DBADDR` fields whose `cvt_dbaddr`
/// (`sCalcoutRecord.c:579-599`) reports `field_type=DBF_STRING`,
/// `no_elements=40`, `field_size=1`. pvxs sizes its buffer
/// `dbChannelFinalElements * dbChannelFinalFieldSize` = 40 bytes
/// (`ioc/iocsource.cpp:124`) and then indexes it at `n * MAX_STRING_SIZE` =
/// 1600 (`:142`), so `softIocPVX` aborts on a corrupted heap. Read as one
/// batch per side, that single channel took 44 of the type's 171 channels down
/// with it and reported all 44 as unmeasurable.
///
/// What is pinned is NOT how many channels kill the server — pvxs may fix
/// theirs, and then none do. It is that a channel whose only misfortune was
/// sharing a batch with a killer is still measured.
#[cfg(tokio_backend)]
#[test]
fn a_channel_that_kills_the_server_does_not_cost_its_batch_mates_their_verdicts() {
    let (_surface, report) = sweep_fields(
        "scalcout",
        &[
            "NAME", "DESC", "VAL", "AA", "LL", "MLST", "ALST", "LALM", "OVAL", "OSV", "PAA",
        ],
    );
    // The twelve `special(SPC_DBADDR)` fields, the only ones whose `dbAddr`
    // reaches sCalcoutRecord.c's cvt_dbaddr and so the only ones that can
    // overflow pvxs's buffer. Named here, not derived from the run, so the run
    // cannot define its own killers.
    const KILLERS: [&str; 12] = [
        "PAA", "PBB", "PCC", "PDD", "PEE", "PFF", "PGG", "PHH", "PII", "PJJ", "PKK", "PLL",
    ];
    let charged: Vec<&str> = report.aborts().map(|c| c.field.as_str()).collect();
    for field in &charged {
        assert!(
            KILLERS.contains(field),
            "scalcout.{field} reaches no cvt_dbaddr and cannot overflow anything, so an \
             abort charged to it is a heap-delayed crash convicting the wrong channel; \
             charged: {charged:?}"
        );
    }
    let measured: BTreeSet<&str> = report
        .cases
        .iter()
        .filter(|c| c.verdict != Verdict::Errored && !charged.contains(&c.field.as_str()))
        .map(|c| c.field.as_str())
        .collect();
    // Every one of these was reported unmeasurable by the batched reader, and
    // not one of them is a killer.
    for field in ["AA", "LL", "OVAL", "OSV", "MLST", "ALST", "LALM"] {
        assert!(
            measured.contains(field),
            "scalcout.{field} shared a batch with a killer, not its defect: {}",
            why_unmeasured(&report)
        );
    }
    let unmeasured: Vec<&str> = report
        .cases
        .iter()
        .filter(|c| c.verdict == Verdict::Errored)
        .map(|c| c.field.as_str())
        .collect();
    assert!(
        unmeasured.len() <= 12,
        "only the 12 SPC_DBADDR channels can kill the server; {} went unmeasured: \
         {unmeasured:?}",
        unmeasured.len()
    );
}

#[cfg(tokio_backend)]
#[test]
fn an_unreachable_pv_scores_error_never_agreement() {
    let (t, pair) = boot("ai", "ORACLE:AI");
    let c = PvaTools::new(&t, pair.c.port(), Side::C);
    let r = PvaTools::new(&t, pair.rust.port(), Side::Rust);

    const MISSING: &str = "ORACLE:NO-SUCH-PV";
    let pvs = vec![MISSING.to_string()];
    let cv = c.pvxget_batch(&pvs).pop().expect("one reading per PV");
    let rv = r.pvxget_batch(&pvs).pop().expect("one reading per PV");
    assert!(
        cv.is_err(),
        "a PV that does not exist must not yield a reading from the pvxs side, got {cv:?}",
    );
    assert!(
        rv.is_err(),
        "a PV that does not exist must not yield a reading from the Rust side, got {rv:?}",
    );

    // Both sides failed *identically*. The verdict must still be ERRORED.
    let ch = pvaread::ChannelRef {
        record_type: "ai".into(),
        field: "NOPE".into(),
        dbf: epics_oracle_rs::dbd::DbfType::Double,
        pv: MISSING.into(),
        expected_shape: None,
        db: String::new(),
    };
    let obs = |v: Result<String, epics_oracle_rs::catool::ToolError>| pvaread::PvaObservation {
        declared_type: None,
        value: None,
        errors: vec![v.unwrap_err()],
        aborted: None,
    };
    let case = pvaread::adjudicate(
        &ch,
        &obs(cv),
        &obs(rv),
        &mut epics_oracle_rs::allowlist::Allowlist::empty(),
    );
    assert_eq!(
        case.verdict,
        Verdict::Errored,
        "an unmeasurable PV must score ERRORED; two failed reads are not an agreement",
    );
    assert_eq!(
        case.errors.len(),
        2,
        "both sides' failures must be reported, so an ERROR says which side could not look",
    );
}
