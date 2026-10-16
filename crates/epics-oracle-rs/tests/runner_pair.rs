//! The adjudication cases that need a real C/Rust pair, as an integration
//! target: cargo builds `oracle-ioc` as a dependency of a `tests/` binary,
//! which `nextest --lib` never does for a unit test. `CARGO_BIN_EXE_oracle-ioc`
//! is the binary cargo handed over, so the pair boots it and not whatever
//! happens to sit beside the test executable.
//!
//! `#![cfg(tokio_backend)]`: these boot `oracle-ioc`, which refuses to start
//! under the reactor-free `exec_backend`, so they cannot pass there under any
//! treatment. The `FakeIoc` cases in `runner.rs` stay selected on every
//! backend.

#![cfg(tokio_backend)]

use std::sync::Once;

use epics_oracle_rs::allowlist::Allowlist;
use epics_oracle_rs::cases::ArrayCase;
use epics_oracle_rs::catool::{CaTools, PutOutcome};
use epics_oracle_rs::dbd::DbfType;
use epics_oracle_rs::diff::{Observation, Verdict};
use epics_oracle_rs::ioc::{CTools, Ioc, Pair, Side};
use epics_oracle_rs::report::{CasePhase, Counts, Reproducer};
use epics_oracle_rs::runner::{
    CaseRef, MONITOR_CONNECT_TIMEOUT, MONITOR_SETTLE, adjudicate, drive_array, readback, workdir,
};

/// Point `RustIoc::binary` at the binary cargo built for this target.
fn built_oracle_ioc() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: first call in the process, before any thread of this test
        // reads the environment; every caller passes the same value.
        unsafe { std::env::set_var("ORACLE_IOC_BIN", env!("CARGO_BIN_EXE_oracle-ioc")) };
    });
}

fn shipped() -> Allowlist {
    Allowlist::load(&Allowlist::default_path()).expect("shipped allowlist loads")
}

fn repro() -> Reproducer {
    Reproducer {
        db: String::new(),
        ops: Vec::new(),
    }
}

/// `value_numeric: None`, so `compare` skipped the surface and the
/// truncation contract was reported AGREED while unverified.
///
/// The plant is a record type that genuinely has no `NORD`: `ai`. The put
/// and every other surface read fine, so only the missing discriminator is
/// under test.
#[test]
fn an_unreadable_nord_errors_the_array_case_instead_of_agreeing() {
    built_oracle_ioc();
    let tools = CTools::discover().expect(
        "the C EPICS tree must be built for the oracle to have ground truth; \
         set EPICS_BASE_BIN if it is not at the default path",
    );
    let dir = workdir(None).expect("workdir");
    let db = dir.join("array_nord.db");
    std::fs::write(&db, epics_oracle_rs::record_stmt("ai", "ORACLE:ARR:0")).expect("write db");
    let pair = Pair::boot(&tools, &db, "ORACLE:ARR:0").expect("both IOCs must boot");

    let plan = vec![ArrayCase {
        prime: Vec::new(),
        values: vec!["1".to_string()],
        class: "array-single-element",
    }];
    let rec_of = |_: usize| "ORACLE:ARR:0".to_string();
    // `Some("NORD")` is the point: the field IS the case's discriminator and
    // the record does not serve it, which is the failure under test. A type
    // that declares no length field at all passes `None` and has nothing to
    // fail on — a different situation, and not this one.
    let side = |port: u16, s: Side| {
        drive_array(&CaTools::new(&tools, port, s), &plan, &rec_of, Some("NORD"))
    };
    let obs_c = side(pair.c.port(), Side::C);
    let obs_r = side(pair.rust.port(), Side::Rust);
    assert!(
        obs_c[0].value_numeric.is_none() && obs_r[0].value_numeric.is_none(),
        "ai has no NORD, so neither side can produce the discriminator"
    );

    let case = adjudicate(
        CaseRef {
            record_type: "ai",
            phase: CasePhase::Array,
            field: "VAL",
            dbf: DbfType::Double,
            class: Some("array-single-element"),
        },
        repro(),
        &obs_c[0],
        &obs_r[0],
        &mut shipped(),
    );
    assert_eq!(
        case.verdict,
        Verdict::Errored,
        "a missing discriminator is not an agreement"
    );
    let counts = Counts::tally(&[case]);
    assert_eq!(
        epics_oracle_rs::report::exit_status(&epics_oracle_rs::report::run_failures(
            &counts,
            &[],
            &[]
        )),
        1,
    );
}

/// A `.STAT` PV that will not connect must ERROR **its own case**, and only
/// its own case.
///
/// Both halves matter. `caget_batch` is all-or-nothing, so the old
/// `caget_batch(stat_pvs, false).ok()` turned one unconnectable `.STAT`
/// into `stat: None` for every case of the record type: `compare` then
/// skipped the alarm surface on all of them and they stayed AGREED, so a
/// port that stopped serving STAT reported as an improvement. The other
/// half is that the fix must not paint every case ERROR — the good PV in
/// the same batch is still measured, via `probe_bisect`.
#[test]
fn a_stat_pv_that_does_not_connect_errors_only_its_own_case() {
    built_oracle_ioc();
    let tools = CTools::discover().expect(
        "the C EPICS tree must be built for the oracle to have ground truth; \
         set EPICS_BASE_BIN if it is not at the default path",
    );
    let dir = workdir(None).expect("workdir");
    let db_text = format!(
        "{}{}",
        epics_oracle_rs::record_stmt("ai", "ORACLE:RB:0"),
        epics_oracle_rs::record_stmt("ai", "ORACLE:RB:1")
    );
    let db = dir.join("readback_stat.db");
    std::fs::write(&db, &db_text).expect("write db");
    let pair = Pair::boot(&tools, &db, "ORACLE:RB:0").expect("both IOCs must boot");

    let val_pvs = ["ORACLE:RB:0.VAL".to_string(), "ORACLE:RB:1.VAL".to_string()];
    // Case 1's STAT does not exist on either side: the reading cannot be
    // taken, which is not the same as the two sides agreeing about it.
    let stat_pvs = [
        "ORACLE:RB:0.STAT".to_string(),
        "ORACLE:NOSUCHRECORD.STAT".to_string(),
    ];
    let sevr_pvs = [
        "ORACLE:RB:0.SEVR".to_string(),
        "ORACLE:RB:1.SEVR".to_string(),
    ];
    let accepted = || vec![Ok(PutOutcome::Completed), Ok(PutOutcome::Completed)];

    let side = |port: u16, s: Side| {
        let t = CaTools::new(&tools, port, s);
        readback(&t, &val_pvs, &stat_pvs, &sevr_pvs, accepted())
    };
    let obs_c = side(pair.c.port(), Side::C);
    let obs_r = side(pair.rust.port(), Side::Rust);

    let verdict = |i: usize| {
        adjudicate(
            CaseRef {
                record_type: "ai",
                phase: CasePhase::Put,
                field: "VAL",
                dbf: DbfType::Double,
                class: Some("over-max"),
            },
            repro(),
            &obs_c[i],
            &obs_r[i],
            &mut shipped(),
        )
    };

    let measured = verdict(0);
    assert_ne!(
        measured.verdict,
        Verdict::Errored,
        "the case whose STAT connected must still be measured: {:?}",
        measured.errors
    );
    assert!(
        obs_c[0].stat.is_some() && obs_r[0].stat.is_some(),
        "the alarm surface must be read where it can be"
    );

    let lost = verdict(1);
    assert_eq!(
        lost.verdict,
        Verdict::Errored,
        "an unread STAT is not an agreement"
    );
    let counts = Counts::tally(&[lost]);
    assert_eq!(
        epics_oracle_rs::report::exit_status(&epics_oracle_rs::report::run_failures(
            &counts,
            &[],
            &[]
        )),
        1,
    );
}

/// A monitor case whose **drive** failed must score ERROR.
///
/// The plant is a real refusal against the real pair: `NAME` is
/// `special(SPC_NOMOD)`, so the put is refused on both sides, the
/// subscription is never stimulated, and both traces are empty. Before
/// this, the drive's outcome was discarded, `compare` found no difference
/// between two empty traces, and the case was scored AGREED and counted as
/// monitor coverage — a positive agreement claim about a subscription that
/// was never given anything to post on.
#[test]
fn a_monitor_whose_drive_was_refused_scores_error_not_agreement() {
    built_oracle_ioc();
    let tools = CTools::discover().expect(
        "the C EPICS tree must be built for the oracle to have ground truth; \
         set EPICS_BASE_BIN if it is not at the default path",
    );
    let dir = workdir(None).expect("workdir");
    let db = dir.join("mon_drive_refused.db");
    std::fs::write(&db, epics_oracle_rs::record_stmt("ai", "ORACLE:MONDRIVE")).expect("write db");
    let pair = Pair::boot(&tools, &db, "ORACLE:MONDRIVE").expect("both IOCs must boot");

    let pvs = vec!["ORACLE:MONDRIVE".to_string()];
    let obs = |port: u16, side: Side| -> Observation {
        let t = CaTools::new(&tools, port, side);
        match t.monitor(&pvs, MONITOR_SETTLE, MONITOR_CONNECT_TIMEOUT, |tt| {
            tt.caput_drive("ORACLE:MONDRIVE.NAME", "NOPE")
        }) {
            Ok(tr) => Observation {
                monitor: Some(tr),
                ..Default::default()
            },
            Err(e) => Observation {
                errors: vec![e],
                ..Default::default()
            },
        }
    };
    let c = obs(pair.c.port(), Side::C);
    let r = obs(pair.rust.port(), Side::Rust);

    let case = adjudicate(
        CaseRef {
            record_type: "ai",
            phase: CasePhase::Monitor,
            field: "VAL",
            dbf: DbfType::Double,
            class: None,
        },
        repro(),
        &c,
        &r,
        &mut shipped(),
    );
    assert_eq!(
        case.verdict,
        Verdict::Errored,
        "c_monitor={:?} rust_monitor={:?}",
        c.monitor,
        r.monitor
    );

    let counts = Counts::tally(&[case]);
    assert_eq!(
        counts.agreed, 0,
        "an unstimulated subscription is not agreement"
    );
    assert_eq!(
        epics_oracle_rs::report::exit_status(&epics_oracle_rs::report::run_failures(
            &counts,
            &[],
            &[]
        )),
        1,
    );
}
