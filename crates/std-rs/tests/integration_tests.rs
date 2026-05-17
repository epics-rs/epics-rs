use epics_base_rs::types::EpicsValue;
use epics_ca_rs::server::CaServerBuilder;
use std::collections::HashMap;

// ============================================================
// Throttle: ReprocessAfter integration test
// ============================================================

#[tokio::test]
async fn test_throttle_delayed_reprocess() {
    let db_str = r#"
record(throttle, "TEST:THR") {
    field(DLY, "0.2")
    field(PREC, "2")
}
"#;
    let macros = HashMap::new();
    let server = CaServerBuilder::new()
        .register_record_type("throttle", || Box::new(std_rs::ThrottleRecord::default()))
        .db_string(db_str, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();
    let db = server.database().clone();

    // First put + process: should send immediately
    server
        .put("TEST:THR", EpicsValue::Double(10.0))
        .await
        .unwrap();
    db.put_record_field_from_ca("TEST:THR", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let sent = server.get("TEST:THR.SENT").await.unwrap();
    assert_eq!(
        sent,
        EpicsValue::Double(10.0),
        "First value should be sent immediately"
    );

    let wait = server.get("TEST:THR.WAIT").await.unwrap();
    assert_eq!(
        wait,
        EpicsValue::Short(1),
        "WAIT should be 1 during delay period"
    );

    // Second put during delay period — must process to queue the value
    server
        .put("TEST:THR", EpicsValue::Double(20.0))
        .await
        .unwrap();
    db.put_record_field_from_ca("TEST:THR", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let sent = server.get("TEST:THR.SENT").await.unwrap();
    assert_eq!(
        sent,
        EpicsValue::Double(10.0),
        "Second value should NOT be sent yet"
    );

    // Wait for DLY to expire — framework's ReprocessAfter will drain the pending value
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;

    let sent = server.get("TEST:THR.SENT").await.unwrap();
    assert_eq!(
        sent,
        EpicsValue::Double(20.0),
        "After delay, pending value should be sent"
    );
}

#[tokio::test]
async fn test_throttle_no_delay_immediate() {
    let db_str = r#"
record(throttle, "TEST:THR2") {
    field(DLY, "0")
}
"#;
    let macros = HashMap::new();
    let server = CaServerBuilder::new()
        .register_record_type("throttle", || Box::new(std_rs::ThrottleRecord::default()))
        .db_string(db_str, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();
    let db = server.database().clone();

    server
        .put("TEST:THR2", EpicsValue::Double(42.0))
        .await
        .unwrap();
    db.put_record_field_from_ca("TEST:THR2", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let sent = server.get("TEST:THR2.SENT").await.unwrap();
    assert_eq!(sent, EpicsValue::Double(42.0));

    let wait = server.get("TEST:THR2.WAIT").await.unwrap();
    assert_eq!(
        wait,
        EpicsValue::Short(0),
        "No delay means WAIT should be 0"
    );
}

#[tokio::test]
async fn test_throttle_limit_clipping_via_framework() {
    let db_str = r#"
record(throttle, "TEST:THR3") {
    field(DLY, "0")
    field(DRVLH, "100")
    field(DRVLL, "0")
    field(DRVLC, "1")
}
"#;
    let macros = HashMap::new();
    let server = CaServerBuilder::new()
        .register_record_type("throttle", || Box::new(std_rs::ThrottleRecord::default()))
        .db_string(db_str, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();
    let db = server.database().clone();

    server
        .put("TEST:THR3", EpicsValue::Double(150.0))
        .await
        .unwrap();
    db.put_record_field_from_ca("TEST:THR3", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let sent = server.get("TEST:THR3.SENT").await.unwrap();
    assert_eq!(
        sent,
        EpicsValue::Double(100.0),
        "Should be clipped to DRVLH"
    );

    let drvls = server.get("TEST:THR3.DRVLS").await.unwrap();
    assert_eq!(
        drvls,
        EpicsValue::Short(2),
        "DRVLS should indicate high limit"
    );
}

// ============================================================
// Epid: PID runs in process via framework
//
// C `epidRecord.c` clears `udf` (and thus runs `do_pid`) for a
// supervisory epid ONLY via a CONSTANT `STPL` link — `epidRecord.c:
// 160-164` `recGblInitConstantLink` seeds `VAL` and clears `udf` at
// init. A supervisory epid with an empty STPL keeps `udf` TRUE
// forever and `epidRecord.c:195` `return(0)` skips `do_pid` every
// cycle; this test uses a constant STPL so `do_pid` runs.
// ============================================================

#[tokio::test]
async fn test_epid_pid_via_framework() {
    let db_str = r#"
record(epid, "TEST:PID") {
    field(STPL, "100")
    field(KP, "2.0")
    field(KI, "0")
    field(KD, "0")
    field(FBON, "1")
    field(DRVH, "1000")
    field(DRVL, "-1000")
}
"#;
    let macros = HashMap::new();
    let server = CaServerBuilder::new()
        .register_record_type("epid", || Box::new(std_rs::EpidRecord::default()))
        .db_string(db_str, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();
    let db = server.database().clone();

    // Setpoint VAL is seeded from the constant STPL at init
    // (C `recGblInitConstantLink`); no operator put needed.

    // Process twice with a small gap so dt > 0
    db.put_record_field_from_ca("TEST:PID", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    db.put_record_field_from_ca("TEST:PID", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    // P = KP * (VAL - CVAL) = 2.0 * (100 - 0) = 200.0
    let p = server.get("TEST:PID.P").await.unwrap();
    match p {
        EpicsValue::Double(v) => {
            assert!((v - 200.0).abs() < 1.0, "P should be ~200.0, got {}", v);
        }
        other => panic!("expected Double, got {:?}", other),
    }

    // OVAL should be clamped but non-zero
    let oval = server.get("TEST:PID.OVAL").await.unwrap();
    match oval {
        EpicsValue::Double(v) => {
            assert!(v.abs() > 1.0, "OVAL should be non-zero, got {}", v);
        }
        other => panic!("expected Double, got {:?}", other),
    }
}

// ============================================================
// Epid: PID runs when processed through the `process_record`
// (process_local) path — regression guard for the overloaded
// `set_device_did_compute` hook.
//
// A Soft-Channel epid driven via `db.process_record(...)` (the
// foreign-call path used by, e.g., QSRV group `proc` members)
// must still run its built-in `do_pid()`. The `process_local`
// path used to call `set_device_did_compute(true)` for every
// soft input, which `epid` reads as "skip the entire PID
// compute" — so P stayed 0. The fix gates that call on
// `soft_channel_skips_convert()` (false for epid), exactly as
// the `processing.rs` link path already does.
// ============================================================

#[tokio::test]
async fn test_epid_pid_via_process_record_path() {
    let db_str = r#"
record(epid, "TEST:PID2") {
    field(STPL, "100")
    field(KP, "2.0")
    field(KI, "0")
    field(KD, "0")
    field(FBON, "1")
    field(DRVH, "1000")
    field(DRVL, "-1000")
}
"#;
    let macros = HashMap::new();
    let server = CaServerBuilder::new()
        .register_record_type("epid", || Box::new(std_rs::EpidRecord::default()))
        .db_string(db_str, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();
    let db = server.database().clone();

    // Setpoint VAL is seeded from the constant STPL at init
    // (C `recGblInitConstantLink`).

    // Process twice via the `process_record` (process_local) path —
    // NOT the PROC-field / process_record_with_links path — with a
    // gap so dt > 0.
    db.process_record("TEST:PID2").await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    db.process_record("TEST:PID2").await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    // P = KP * (VAL - CVAL) = 2.0 * (100 - 0) = 200.0.
    // Before the fix, process_local set device_did_compute=true and
    // epid skipped do_pid(), leaving P = 0.
    let p = server.get("TEST:PID2.P").await.unwrap();
    match p {
        EpicsValue::Double(v) => {
            assert!(
                (v - 200.0).abs() < 1.0,
                "P should be ~200.0 (do_pid must run on the process_record path), got {}",
                v
            );
        }
        other => panic!("expected Double, got {:?}", other),
    }
}

// ============================================================
// Epid Bug 1 — supervisory epid with empty STPL: do_pid NEVER runs.
//
// C `epidRecord.c`: `special = NULL` (line 105) — no operator UDF
// clear. `udf` starts TRUE (`epidRecord.c` init) and is cleared only
// by a CONSTANT STPL (`epidRecord.c:160-164`) or a closed-loop
// `dbGetLink(stpl)` success (`epidRecord.c:191-193`). A supervisory
// (`SMSL=0`) epid with an empty/non-constant STPL keeps `udf` TRUE
// forever, so `epidRecord.c:195` `return(0)` skips `do_pid` on EVERY
// cycle. An operator `caput` to VAL does NOT clear `udf`.
//
// This exercises the framework auto-clear path (`clears_udf()` /
// `value_is_undefined()` recomputed after `process()`), not a manual
// `set_process_context` push.
// ============================================================

#[tokio::test]
async fn test_epid_supervisory_empty_stpl_never_runs_do_pid() {
    let db_str = r#"
record(epid, "TEST:PIDSUP") {
    field(KP, "2.0")
    field(KI, "0")
    field(KD, "0")
    field(FBON, "1")
    field(DRVH, "1000")
    field(DRVL, "-1000")
}
"#;
    let macros = HashMap::new();
    let server = CaServerBuilder::new()
        .register_record_type("epid", || Box::new(std_rs::EpidRecord::default()))
        .db_string(db_str, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();
    let db = server.database().clone();

    // Operator sets the setpoint directly — C `special = NULL`, so
    // this does NOT clear udf.
    server
        .put("TEST:PIDSUP.VAL", EpicsValue::Double(100.0))
        .await
        .unwrap();

    // Process 5 cycles via the full link path.
    for _ in 0..5 {
        db.put_record_field_from_ca("TEST:PIDSUP", "PROC", EpicsValue::Short(1))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    // do_pid must NEVER have run — P stays 0 across all 5 cycles.
    let p = server.get("TEST:PIDSUP.P").await.unwrap();
    match p {
        EpicsValue::Double(v) => {
            assert_eq!(
                v, 0.0,
                "supervisory epid with empty STPL must NEVER run do_pid; \
                 P must stay 0 after 5 cycles, got {}",
                v
            );
        }
        other => panic!("expected Double, got {:?}", other),
    }

    // The framework must keep UDF set every cycle.
    let udf = server.get("TEST:PIDSUP.UDF").await.unwrap();
    assert_eq!(
        udf,
        EpicsValue::Char(1),
        "UDF must stay set for a supervisory empty-STPL epid"
    );
}

// ============================================================
// Epid Bug 2 — closed-loop epid with a WORKING STPL: udf clears and
// do_pid runs.
//
// C `epidRecord.c:191-193`: closed-loop (`SMSL=1`) with a successful
// `dbGetLink(stpl)` clears `udf` *before* the `if (udf==TRUE)` check
// at `epidRecord.c:195` — so `do_pid` runs in the SAME `process()`
// call the fetch succeeded. The framework fetches STPL->VAL and
// reports the success via `set_resolved_input_links` BEFORE
// `process()`, so the epid clears its UDF projection in-cycle, just
// as C does. Therefore do_pid runs from cycle 1 (not "cycle 2").
// ============================================================

#[tokio::test]
async fn test_epid_closed_loop_working_stpl_runs_do_pid() {
    let db_str = r#"
record(ao, "TEST:PIDSRC") {
    field(VAL, "100")
}
record(epid, "TEST:PIDCL") {
    field(SMSL, "1")
    field(STPL, "TEST:PIDSRC.VAL")
    field(KP, "2.0")
    field(KI, "0")
    field(KD, "0")
    field(FBON, "1")
    field(DRVH, "1000")
    field(DRVL, "-1000")
}
"#;
    let macros = HashMap::new();
    let server = CaServerBuilder::new()
        .register_record_type("epid", || Box::new(std_rs::EpidRecord::default()))
        .db_string(db_str, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();
    let db = server.database().clone();

    // Cycle 1: STPL fetch succeeds → VAL becomes 100 and udf is
    // cleared in-cycle (C `epidRecord.c:191-193` clears udf before the
    // line-195 gate), so do_pid runs.
    db.put_record_field_from_ca("TEST:PIDCL", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let p_c1 = server.get("TEST:PIDCL.P").await.unwrap();
    match p_c1 {
        EpicsValue::Double(v) => {
            // P = KP * (VAL - CVAL) = 2.0 * (100 - 0) = 200.0
            assert!(
                (v - 200.0).abs() < 1.0,
                "cycle 1: closed-loop epid with a resolved STPL must run \
                 do_pid (udf cleared in-cycle); P should be ~200.0, got {}",
                v
            );
        }
        other => panic!("expected Double, got {:?}", other),
    }

    // UDF must be cleared after a successful STPL fetch.
    let udf = server.get("TEST:PIDCL.UDF").await.unwrap();
    assert_eq!(
        udf,
        EpicsValue::Char(0),
        "closed-loop epid with a resolved STPL must have UDF cleared"
    );

    // Cycle 2: udf stays clear, do_pid keeps running.
    db.put_record_field_from_ca("TEST:PIDCL", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let p_c2 = server.get("TEST:PIDCL.P").await.unwrap();
    match p_c2 {
        EpicsValue::Double(v) => {
            assert!(
                (v - 200.0).abs() < 1.0,
                "cycle 2: closed-loop epid keeps running do_pid; P should \
                 be ~200.0, got {}",
                v
            );
        }
        other => panic!("expected Double, got {:?}", other),
    }
}

// ============================================================
// Epid Bug 2 — closed-loop epid with a FAILING/empty STPL: udf stays
// set, do_pid NEVER runs.
//
// C `epidRecord.c:191-193` clears `udf` ONLY on
// `RTN_SUCCESS(dbGetLink(&prec->stpl, ...))`. A closed-loop epid
// whose STPL link points at a non-existent record (the fetch fails)
// must keep `udf` TRUE — the pre-fix `!val.is_nan()` proxy would
// wrongly clear it because VAL stays at its finite default 0.0 when
// the link read fails.
// ============================================================

#[tokio::test]
async fn test_epid_closed_loop_failing_stpl_keeps_udf() {
    let db_str = r#"
record(epid, "TEST:PIDCLF") {
    field(SMSL, "1")
    field(STPL, "TEST:NOSUCHREC.VAL")
    field(KP, "2.0")
    field(KI, "0")
    field(KD, "0")
    field(FBON, "1")
    field(DRVH, "1000")
    field(DRVL, "-1000")
}
"#;
    let macros = HashMap::new();
    let server = CaServerBuilder::new()
        .register_record_type("epid", || Box::new(std_rs::EpidRecord::default()))
        .db_string(db_str, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();
    let db = server.database().clone();

    // Process 5 cycles — the STPL link can never resolve.
    for _ in 0..5 {
        db.put_record_field_from_ca("TEST:PIDCLF", "PROC", EpicsValue::Short(1))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    // do_pid must NEVER have run — the STPL fetch failed every cycle.
    let p = server.get("TEST:PIDCLF.P").await.unwrap();
    match p {
        EpicsValue::Double(v) => {
            assert_eq!(
                v, 0.0,
                "closed-loop epid with a failing STPL must keep udf set \
                 and never run do_pid; P must stay 0, got {}",
                v
            );
        }
        other => panic!("expected Double, got {:?}", other),
    }

    let udf = server.get("TEST:PIDCLF.UDF").await.unwrap();
    assert_eq!(
        udf,
        EpicsValue::Char(1),
        "UDF must stay set when the STPL fetch fails"
    );
}

// ============================================================
// Timestamp: process produces output
// ============================================================

#[tokio::test]
async fn test_timestamp_via_framework() {
    let db_str = r#"
record(timestamp, "TEST:TS") {
    field(TST, "4")
}
"#;
    let macros = HashMap::new();
    let server = CaServerBuilder::new()
        .register_record_type("timestamp", || Box::new(std_rs::TimestampRecord::default()))
        .db_string(db_str, &macros)
        .unwrap()
        .build()
        .await
        .unwrap();
    let db = server.database().clone();

    // Trigger process
    db.put_record_field_from_ca("TEST:TS", "PROC", EpicsValue::Short(1))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let val = server.get("TEST:TS").await.unwrap();
    match val {
        EpicsValue::String(s) => {
            assert!(!s.is_empty(), "Timestamp should be non-empty");
            assert!(s.contains(':'), "Format 4 (HH:MM:SS) should contain ':'");
        }
        other => panic!("expected String, got {:?}", other),
    }

    let rval = server.get("TEST:TS.RVAL").await.unwrap();
    match rval {
        EpicsValue::Long(v) => assert!(v > 0, "RVAL should be positive"),
        other => panic!("expected Long, got {:?}", other),
    }
}
