//! End-to-end: boot the real differential pair and check the harness's own
//! invariants against it.
//!
//! These tests assert properties of the **harness**, not of the port. They must
//! keep passing while the port's field tables are being regenerated underneath
//! them, so nothing here asserts a specific value the port reports. What they do
//! assert is the thing the harness stakes its credibility on: that it never
//! turns "I could not measure this" into "this agreed".
//!
//! Requires the built C EPICS tree (`EPICS_BASE_BIN`, or the default path). If
//! it is absent the tests fail loudly rather than skipping — a silently skipped
//! oracle is exactly the false-clean this project is trying to escape.

use std::collections::BTreeSet;

use epics_oracle_rs::allowlist::Allowlist;
use epics_oracle_rs::catool::CaTools;
use epics_oracle_rs::dbd::Dbd;
use epics_oracle_rs::diff::Verdict;
use epics_oracle_rs::ioc::{CTools, Ioc, Pair, Side};
use epics_oracle_rs::report::{Counts, exit_status, field_coverage, run_failures};
use epics_oracle_rs::runner::{Runner, workdir};
use epics_oracle_rs::surface::{Surface, probe_supported_record_types};

fn tools() -> CTools {
    CTools::discover().expect(
        "the C EPICS tree must be built for the oracle to have ground truth; \
         set EPICS_BASE_BIN if it is not at the default path",
    )
}

fn dbd() -> Dbd {
    Dbd::parse_file(&CTools::dbd_path()).expect("expanded softIoc.dbd parses")
}

/// The denominator is derived from the spec, and it is not empty or absurd.
#[test]
fn the_dbd_yields_a_real_denominator() {
    let d = dbd();
    assert!(
        d.record_types.len() >= 30,
        "softIoc.dbd should carry ~34 record types, got {}",
        d.record_types.len()
    );
    let all: BTreeSet<String> = d.record_types.iter().map(|r| r.name.clone()).collect();
    let s = Surface::build(&d, &all);
    assert!(
        s.denominator() > 2000,
        "expected a surface of thousands of observable fields, got {}",
        s.denominator()
    );
    // The NOACCESS exclusion must be real and visible, not silent.
    assert!(
        s.excluded_noaccess > 0,
        "DBF_NOACCESS fields exist and must be excluded, and counted"
    );
    // dbCommon must be inlined -- if it is not, we parsed the unexpanded dbd
    // and the denominator is short by ~48 fields per record.
    let ai = d.record_type("ai").expect("ai");
    assert!(ai.field("SCAN").is_some(), "dbCommon must be inlined");
    assert!(ai.field("STAT").is_some());
}

/// Both IOCs boot on the same `.db`, on **different, exclusive** ports, and both
/// serve the record. This is the load-bearing precondition for everything else:
/// if the two ever shared a port, a reading could come from the wrong IOC.
#[test]
fn the_pair_boots_on_distinct_ports_and_both_serve_the_record() {
    let t = tools();
    let dir = workdir(None).unwrap();
    let db = dir.join("pair_smoke.db");
    std::fs::write(
        &db,
        "record(ai, \"ORACLE:SMOKE\") { field(VAL, \"1.5\") }\n",
    )
    .unwrap();

    let pair = Pair::boot(&t, &db, "ORACLE:SMOKE").expect("both IOCs must boot");
    assert_ne!(
        pair.c.port(),
        pair.rust.port(),
        "the two IOCs must never share a CA port, or answers are unattributable"
    );
    assert_ne!(pair.c.port(), 5064, "never the CA default");
    assert_ne!(pair.rust.port(), 5064);

    // Each side must answer for itself.
    let c = CaTools::new(&t, pair.c.port(), Side::C);
    let r = CaTools::new(&t, pair.rust.port(), Side::Rust);
    assert_eq!(c.caget_string("ORACLE:SMOKE").unwrap(), "1.5");
    assert_eq!(r.caget_string("ORACLE:SMOKE").unwrap(), "1.5");
}

/// A PV that does not exist must produce an ERROR on both sides — never an
/// empty-but-successful reading that the diff would score as agreement.
#[test]
fn an_unconnectable_pv_errors_rather_than_silently_agreeing() {
    let t = tools();
    let dir = workdir(None).unwrap();
    let db = dir.join("pair_missing.db");
    std::fs::write(&db, "record(ai, \"ORACLE:PRESENT\") {}\n").unwrap();
    let pair = Pair::boot(&t, &db, "ORACLE:PRESENT").expect("boot");

    for (port, side) in [(pair.c.port(), Side::C), (pair.rust.port(), Side::Rust)] {
        let tools = CaTools::new(&t, port, side);
        let err = tools
            .caget_string("ORACLE:NOSUCHPV")
            .expect_err("a missing PV must be an error, not an empty string");
        assert_eq!(
            err.side, side,
            "the error must name the side it happened on"
        );
    }
}

/// The whole-run invariant: every case lands in exactly one bucket, and the
/// buckets reconcile with the number of cases run. A harness whose counts do not
/// add up has silently dropped cases.
#[test]
fn a_real_run_reconciles_its_counts_and_reports_coverage() {
    let t = tools();
    let d = dbd();
    let supported: BTreeSet<String> = ["ai", "bi"].iter().map(|s| s.to_string()).collect();
    let surface = Surface::build(&d, &supported);
    let mut allowlist =
        Allowlist::load(&Allowlist::default_path()).expect("shipped allowlist loads");

    let runner = Runner::new(t, d, workdir(None).unwrap());
    let mut cases = runner.probe_reads("ai", &surface, &mut allowlist);
    cases.extend(runner.probe_reads("bi", &surface, &mut allowlist));

    assert!(!cases.is_empty(), "the run must produce cases");
    let counts = Counts::tally(&cases);
    counts
        .check()
        .expect("every case must land in exactly one bucket");

    // An errored case must never also be counted as agreement.
    for c in &cases {
        match c.verdict {
            Verdict::Errored => assert!(
                !c.errors.is_empty(),
                "{} is ERRORED but carries no error — that is a silent pass",
                c.id()
            ),
            _ => assert!(
                c.errors.is_empty(),
                "{} has errors but was not scored ERRORED",
                c.id()
            ),
        }
    }

    // Coverage is a fraction of the FULL denominator, not of what we chose to
    // run. A two-record-type run must therefore report low coverage, not 100%.
    //
    // Asserting that against `cases.len()` was not enough: it bounds how many
    // cases ran, not what the coverage line claims. The claim has to be checked
    // against the read probe's own arithmetic — it visits every field of the
    // two types exactly once, so measured + errored is exactly that many, no
    // matter what other phases contributed to `cases`.
    let read_fields = surface.fields_of("ai").count() + surface.fields_of("bi").count();
    let mon = runner
        .probe_monitor("ai", &surface, &mut allowlist)
        .expect("ai.VAL is drivable, so the monitor phase must produce a case");
    cases.push(mon);

    let cov = field_coverage(&cases, surface.denominator());
    assert_eq!(
        cov.measured + cov.errored,
        read_fields,
        "the coverage line must account for the fields read and nothing else"
    );
    assert!(
        cov.measured + cov.errored <= cov.enumerated,
        "coverage can never exceed the denominator"
    );
    assert!(
        cov.percent() < 100.0,
        "a two-record-type run is not a full sweep"
    );
}

/// Every case that reports a difference must carry a reproducer that names the
/// record type and field it is about. A finding you cannot re-run is an opinion.
#[test]
fn every_reported_difference_carries_a_runnable_reproducer() {
    let t = tools();
    let d = dbd();
    let supported: BTreeSet<String> = ["ai"].iter().map(|s| s.to_string()).collect();
    let surface = Surface::build(&d, &supported);
    let mut allowlist = Allowlist::load(&Allowlist::default_path()).unwrap();
    let runner = Runner::new(t, d, workdir(None).unwrap());

    let cases = runner.probe_reads("ai", &surface, &mut allowlist);
    for c in cases.iter().filter(|c| !c.differences.is_empty()) {
        assert!(
            c.reproducer.db.contains("record(ai"),
            "{}: reproducer must carry the .db",
            c.id()
        );
        assert!(
            !c.reproducer.ops.is_empty(),
            "{}: reproducer must carry the operation sequence",
            c.id()
        );
        let rendered = c.reproducer.render("");
        assert!(rendered.contains("softIoc"), "must show how to run C");
        assert!(rendered.contains("oracle-ioc"), "must show how to run port");
    }
}

/// The denominator probe must be able to configure itself, and what it reports
/// must not fail a clean run.
///
/// `asyn` is the type where the probe's configuration and the measured IOC's
/// diverge — base's CNCT-only stub record versus asyn-rs's `AsynRecord` on
/// `ORACLEASYN` — so it is the one to pin now that the exit rule consults the
/// unimplemented list. This is a guard, not a regression test: measured on this
/// host, both configurations accept `asyn`, so it passes with the probe built
/// either way. It fails if the shared configuration owner stops configuring, or
/// if `asyn` ever stops loading under the configuration actually measured.
#[tokio::test]
async fn the_denominator_probe_uses_the_configuration_under_test() {
    let mut d = dbd();
    d.record_types.retain(|r| r.name == "asyn");
    assert_eq!(
        d.record_types.len(),
        1,
        "the oracle dbd must declare asyn for this to measure anything"
    );

    let supported = probe_supported_record_types(&d)
        .await
        .expect("the probe must be able to configure itself");
    let surface = Surface::build(&d, &supported);

    // A run in which nothing disagreed and nothing errored.
    let failures = run_failures(&Counts::default(), &surface.unimplemented_types, &[]);
    assert_eq!(
        exit_status(&failures),
        0,
        "asyn must not read as unimplemented: {failures:?}"
    );
}

/// A refusal the server actually issued must stay a **reading**.
///
/// The other direction of the put contract: `caput` failing because the server
/// said no is observable behaviour (`NAME` is `special(SPC_NOMOD)`, so both
/// sides must refuse), and a fix that turned every non-zero `caput` exit into a
/// measurement failure would bury that finding under ERROR instead of scoring
/// it. Driven against the real pair, because the discriminator is the C tool's
/// own message.
#[test]
fn a_server_issued_refusal_is_a_reading_not_a_measurement_failure() {
    let t = tools();
    let dir = workdir(None).unwrap();
    let db = dir.join("put_nomod.db");
    std::fs::write(&db, "record(ai, \"ORACLE:NOMOD\") {}\n").unwrap();
    let pair = Pair::boot(&t, &db, "ORACLE:NOMOD").expect("both IOCs must boot");

    for (port, side) in [(pair.c.port(), Side::C), (pair.rust.port(), Side::Rust)] {
        let out = CaTools::new(&t, port, side)
            .caput("ORACLE:NOMOD.NAME", "SOMETHINGELSE")
            .unwrap_or_else(|e| {
                panic!("{side}: a refusal by the server must be a reading, got ERROR: {e}")
            });
        assert!(
            !out.accepted,
            "{side}: NAME is special(SPC_NOMOD); the write must be refused"
        );
        assert!(
            out.error.is_some(),
            "{side}: a refusal must carry the server's complaint"
        );
    }
}
