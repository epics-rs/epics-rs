//! End-to-end: boot the real **PVA** differential pair (pvxs QSRV2's
//! `softIocPVX` vs `oracle-ioc --pva`) and check the harness's own invariants
//! against it.
//!
//! Like `tests/oracle.rs`, these assert properties of the **harness**, not of
//! the port — with one deliberate exception noted on
//! [`at_least_one_scalar_record_agrees`]. The property the harness stakes its
//! credibility on is that it never turns "I could not measure this" into "this
//! agreed", and that is what is pinned here.
//!
//! Requires the built pvxs tree (`PVXS_BIN`, or the default path). If it is
//! absent the tests fail loudly rather than skipping — a silently skipped
//! oracle is exactly the false-clean this project exists to escape.

use epics_oracle_rs::diff::Verdict;
use epics_oracle_rs::ioc::{Ioc, PvaPair, PvxTools, Side};
use epics_oracle_rs::pvatool::PvaTools;
use epics_oracle_rs::runner::workdir;

fn tools() -> PvxTools {
    PvxTools::discover().expect(
        "the pvxs tree must be built for the PVA oracle to have ground truth; \
         set PVXS_BIN if it is not at the default path",
    )
}

/// Boot the pair on a one-record `.db` and hand back the pair plus its tools.
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

/// Each side is the **only** server on its port, proven by measurement.
///
/// `PvaPair::boot` already refuses to return otherwise; this pins the proof
/// itself, because it is the one guard with no CA analogue and it is invisible
/// when it silently stops working.
#[test]
fn each_side_is_the_sole_server_on_its_port() {
    let (t, pair) = boot("bi", "ORACLE:BI");
    for (port, side) in [(pair.c.port(), Side::C), (pair.rust.port(), Side::Rust)] {
        let n = PvaTools::new(&t, port, side)
            .server_count()
            .unwrap_or_else(|e| panic!("counting servers on the {side} side's port {port}: {e}"));
        assert_eq!(
            n, 1,
            "exactly one PVA server must answer on the {side} side's port {port}",
        );
    }
}

/// A scalar record reads the same on both sides — the harness can produce
/// AGREED, not merely DEFECT and ERROR.
///
/// This is the one test here that asserts something about the **port** rather
/// than the harness, and it is deliberate: a harness that could never return
/// AGREED would pass every other test in this file while being useless.
///
/// `bi` is chosen because it is one of the three types that currently agree,
/// and the reason they do is worth stating: `bi`/`bo`/`mbbi` are `DBR_ENUM`
/// channels, and QSRV2 projects those as `NTEnum` (`singlesource.cpp:201`),
/// which carries no `display`/`control`/`valueAlarm` at all. They agree
/// because they have the least metadata to disagree about — so this test
/// proves the harness *can* reach AGREED, and proves nothing broader.
/// If the port's `bi` projection later diverges, this test failing is the
/// oracle doing its job — investigate it, do not relax the comparison.
#[test]
fn at_least_one_scalar_record_agrees() {
    let (t, pair) = boot("bi", "ORACLE:BI");
    let c = PvaTools::new(&t, pair.c.port(), Side::C);
    let r = PvaTools::new(&t, pair.rust.port(), Side::Rust);

    let cv = c
        .pvxget("ORACLE:BI")
        .expect("pvxs side must read ORACLE:BI");
    let rv = r
        .pvxget("ORACLE:BI")
        .expect("Rust side must read ORACLE:BI");
    assert_eq!(
        cv.trim_end(),
        rv.trim_end(),
        "bi must read identically on both sides",
    );
}

/// **The rule the whole harness rests on.** A PV that does not exist yields no
/// reading, and no reading is an ERROR — never agreement.
///
/// The trap this guards is precise: both sides fail identically on an unknown
/// PV, so a harness that compared *outcomes* rather than *readings* would find
/// them equal and score AGREED. That is "exit 0 because I could not look",
/// which is exactly what produced the false-clean verdicts this crate replaced.
#[test]
fn an_unreachable_pv_scores_error_never_agreement() {
    let (t, pair) = boot("ai", "ORACLE:AI");
    let c = PvaTools::new(&t, pair.c.port(), Side::C);
    let r = PvaTools::new(&t, pair.rust.port(), Side::Rust);

    const MISSING: &str = "ORACLE:NO-SUCH-PV";
    let cv = c.pvxget(MISSING);
    let rv = r.pvxget(MISSING);
    assert!(
        cv.is_err(),
        "a PV that does not exist must not yield a reading from the pvxs side, got {cv:?}",
    );
    assert!(
        rv.is_err(),
        "a PV that does not exist must not yield a reading from the Rust side, got {rv:?}",
    );

    // Both sides failed *identically*. The verdict must still be ERRORED.
    let case = epics_oracle_rs::pvaread::adjudicate("ai", MISSING, "", cv, rv);
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
