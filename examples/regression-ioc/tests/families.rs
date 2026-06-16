//! Regression tests for recurring bug-fix families (v0.15.x-v0.20.x), each
//! driven over the wire against the live in-process CA+PVA regression IOC.
//!
//! One test per recurring-bug *boundary* (not per narrative): a future
//! regression in the same family fails the matching test.
//!
//!   A — processing-chain ordering (FLNK runs / OUT-link writes)
//!   B — monitor posts only on change
//!   C — periodic SCAN runs while a server is up
//!   E — enum served as DBR_ENUM with choice labels (caput-by-label is
//!       CLI-scoped; see the note above Family F)
//!   F — DBF_MENU record field served as DBR_ENUM
//!   G — alarm severity raised on limit violation
//!   H — timestamp advances on each process
//!   I — monitor event-MASK routing (MDEL/ADEL → DBE_VALUE/DBE_LOG class)
//!   K — .PROC=0 forces a process; put-callback waits for completion

use std::time::Duration;

use epics_base_rs::server::snapshot::Snapshot;
use epics_ca_rs::client::MonitorHandle;
use epics_ca_rs::protocol::{DBE_LOG, DBE_VALUE};
use epics_ca_rs::{DbFieldType, EpicsValue};
use regression_ioc::RegressionIoc;
use serial_test::serial;

// ---- helpers --------------------------------------------------------------

/// Receive the next monitor event within `ms`, or `None` (timeout/closed).
async fn recv_within(mon: &mut MonitorHandle, ms: u64) -> Option<Snapshot> {
    match tokio::time::timeout(Duration::from_millis(ms), mon.recv()).await {
        Ok(Some(Ok(s))) => Some(s),
        _ => None,
    }
}

/// Receive events until one satisfies `pred` or `ms` elapses.
async fn recv_until<F>(mon: &mut MonitorHandle, ms: u64, pred: F) -> Option<Snapshot>
where
    F: Fn(&Snapshot) -> bool,
{
    tokio::time::timeout(Duration::from_millis(ms), async {
        loop {
            match mon.recv().await {
                Some(Ok(s)) => {
                    if pred(&s) {
                        return Some(s);
                    }
                }
                Some(Err(_)) => continue,
                None => return None,
            }
        }
    })
    .await
    .ok()
    .flatten()
}

/// Best-effort numeric view of a value (covers the record types this suite
/// reads: Double/Long/Short/ULong/Enum).
fn as_f64(v: &EpicsValue) -> Option<f64> {
    match v {
        EpicsValue::Double(x) => Some(*x),
        EpicsValue::Long(x) => Some(*x as f64),
        EpicsValue::Short(x) => Some(*x as f64),
        EpicsValue::ULong(x) => Some(*x as f64),
        EpicsValue::Enum(x) => Some(*x as f64),
        _ => None,
    }
}

/// Predicate: the snapshot's numeric value equals `want` (±eps).
fn val_is(want: f64) -> impl Fn(&Snapshot) -> bool {
    move |s: &Snapshot| as_f64(&s.value).is_some_and(|x| (x - want).abs() < 1e-9)
}

/// Poll `caget` until the numeric VAL equals `want` (±eps) or `ms` elapses.
async fn caget_num_eq(ca: &epics_ca_rs::client::CaClient, pv: &str, want: f64, ms: u64) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
    loop {
        if let Ok((_t, v)) = ca.caget(pv).await
            && as_f64(&v).is_some_and(|x| (x - want).abs() < 1e-9)
        {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

// ---- Family A: processing-chain ordering ----------------------------------

/// A caput to a Passive record must walk its FLNK to a downstream record,
/// which reads the source fresh through a DB input link. Pins the
/// "FLNK chain runs on caput" / processTarget family.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn a_flnk_chain_runs_on_caput() {
    let ioc = RegressionIoc::boot().await.expect("boot");
    let ca = ioc.ca_client().await;

    ca.caput("REG:A:SRC", "7").await.expect("caput SRC");
    assert!(
        caget_num_eq(&ca, "REG:A:SINK", 7.0, 2000).await,
        "FLNK from REG:A:SRC must process REG:A:SINK, which reads SRC=7 through INPA"
    );
}

/// A calcout result must be written through its PP OUT link to the target
/// record on process. Pins the "OUT-link write happens" family.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn a_calcout_writes_out_link() {
    let ioc = RegressionIoc::boot().await.expect("boot");
    let ca = ioc.ca_client().await;

    // Seed the A input, then force a process; CALC = A+1 -> OUT target = 6.
    ca.caput("REG:A:CO.A", "5").await.expect("caput CO.A");
    ca.caput("REG:A:CO.PROC", "1")
        .await
        .expect("force process CO");
    assert!(
        caget_num_eq(&ca, "REG:A:OTGT", 6.0, 2000).await,
        "calcout REG:A:CO must write A+1=6 through its PP OUT link to REG:A:OTGT"
    );
}

// ---- Family B: monitor posts only on change -------------------------------

/// A re-put of the same value must NOT produce a monitor event; only an actual
/// change posts. Pins "post binary-record VAL monitor only on change".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn b_monitor_posts_only_on_change() {
    let ioc = RegressionIoc::boot().await.expect("boot");
    let ca = ioc.ca_client().await;

    let ch = ca.create_channel("REG:B:BO");
    ch.wait_connected(Duration::from_secs(3))
        .await
        .expect("connect");
    let mut mon = ch.subscribe().await.expect("subscribe");

    // Drain the initial connection event (value 0).
    let _initial = recv_within(&mut mon, 1500).await.expect("initial event");

    // Change 0 -> 1 must post.
    ca.caput("REG:B:BO", "1").await.expect("caput 1");
    let e1 = recv_until(&mut mon, 1500, |s| matches!(s.value, EpicsValue::Enum(1)))
        .await
        .expect("change to 1 must post");
    assert!(matches!(e1.value, EpicsValue::Enum(1)));

    // Re-put the same value: NO event within the window.
    ca.caput("REG:B:BO", "1").await.expect("caput 1 again");
    let noop = recv_within(&mut mon, 600).await;
    assert!(
        noop.is_none(),
        "a no-op re-put must NOT post a monitor event, got {noop:?}"
    );

    // Change 1 -> 0 must post again.
    ca.caput("REG:B:BO", "0").await.expect("caput 0");
    let e2 = recv_until(&mut mon, 1500, |s| matches!(s.value, EpicsValue::Enum(0)))
        .await
        .expect("change to 0 must post");
    assert!(matches!(e2.value, EpicsValue::Enum(0)));
}

// ---- Family C: periodic SCAN runs while a server is up --------------------

/// A `SCAN="1 second"` record must process on its own (the CA server spawns the
/// scan scheduler), mirroring its input. Pins the periodic-scan-runs family —
/// the structural sibling of the v0.20.0 SCAN-gating regression.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn c_periodic_scan_runs() {
    let ioc = RegressionIoc::boot().await.expect("boot");
    let ca = ioc.ca_client().await;

    ca.caput("REG:C:SRC", "42").await.expect("caput SRC");
    assert!(
        caget_num_eq(&ca, "REG:C:SCAN", 42.0, 3000).await,
        "1-second SCAN record REG:C:SCAN must process on its own and mirror REG:C:SRC=42"
    );
}

// ---- Family E: enum served as DBR_ENUM, caput-by-label --------------------

/// An mbbo VAL must be served over CA as DBR_ENUM (type 3) and round-trip by
/// index; over PVA the NTEnum must carry its choice labels inline. Pins the
/// "serve mbb fields as DBR_ENUM with choice labels" family.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn e_enum_served_as_dbr_enum_with_choices() {
    let ioc = RegressionIoc::boot().await.expect("boot");
    let ca = ioc.ca_client().await;

    ca.caput("REG:E:MBBO", "2").await.expect("caput index 2");
    let (dbf, val) = ca.caget("REG:E:MBBO").await.expect("caget");
    assert_eq!(
        dbf,
        DbFieldType::Enum,
        "mbbo VAL must be served as DBR_ENUM"
    );
    assert!(
        matches!(val, EpicsValue::Enum(2)),
        "index round-trip, got {val:?}"
    );

    // PVA NTEnum carries the choices.
    let pva = ioc.pva_client();
    let field = tokio::time::timeout(Duration::from_secs(5), pva.pvget("REG:E:MBBO"))
        .await
        .expect("pvget timed out")
        .expect("pvget");
    let dbg = format!("{field:?}");
    for choice in ["Off", "On", "Standby"] {
        assert!(
            dbg.contains(choice),
            "PVA NTEnum must carry choice {choice:?}, got {dbg}"
        );
    }
}

// NOTE: the caput-by-label regression ("match ENUM value against the menu
// before numeric index", commit fdac3112) lives entirely in the `caput` CLI
// (crates/epics-ca-rs/src/bin/caput-rs.rs::classify_enum_token: read
// DBR_GR_ENUM, match a state name, send it as DBR_STRING). The server put path
// was not touched by that fix, and the in-process library client has no menu
// to classify against, so this in-process harness cannot faithfully exercise
// it. It is covered by caput-rs's own tests, not here.

// ---- Family F: DBF_MENU record field served as DBR_ENUM -------------------

/// A DBF_MENU record field (here `.SCAN`) must be served over CA as DBR_ENUM,
/// not as a string or long. Pins "serve DBF_MENU record fields as DBR_ENUM".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn f_menu_field_served_as_dbr_enum() {
    let ioc = RegressionIoc::boot().await.expect("boot");
    let ca = ioc.ca_client().await;

    let (dbf, val) = ca.caget("REG:H:AI.SCAN").await.expect("caget .SCAN");
    assert_eq!(
        dbf,
        DbFieldType::Enum,
        "DBF_MENU .SCAN must be served as DBR_ENUM"
    );
    assert!(
        matches!(val, EpicsValue::Enum(_)),
        "SCAN value must be an enum index, got {val:?}"
    );
}

// ---- Family G: alarm severity raised on limit violation -------------------

/// Driving a value past HIHI must raise the record to MAJOR severity, observable
/// in the monitor snapshot's alarm. Pins the alarm-severity family.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn g_alarm_severity_on_limit_violation() {
    let ioc = RegressionIoc::boot().await.expect("boot");
    let ca = ioc.ca_client().await;

    let ch = ca.create_channel("REG:G:AI");
    ch.wait_connected(Duration::from_secs(3))
        .await
        .expect("connect");
    let mut mon = ch.subscribe().await.expect("subscribe");

    // Initial state (VAL=50, PINI) must be within limits: NO_ALARM.
    let init = recv_within(&mut mon, 1500).await.expect("initial");
    assert_eq!(
        init.alarm.severity, 0,
        "initial VAL=50 must be NO_ALARM, got {:?}",
        init.alarm
    );

    // Drive past HIHI=90 -> MAJOR (severity 2).
    ca.caput("REG:G:AI", "95").await.expect("caput 95");
    let hi = recv_until(&mut mon, 2000, |s| s.alarm.severity == 2)
        .await
        .expect("HIHI must raise MAJOR severity");
    assert_eq!(hi.alarm.severity, 2, "VAL past HIHI must be MAJOR");

    // Return within limits -> NO_ALARM again.
    ca.caput("REG:G:AI", "50").await.expect("caput 50");
    let back = recv_until(&mut mon, 2000, |s| s.alarm.severity == 0)
        .await
        .expect("returning within limits must clear to NO_ALARM");
    assert_eq!(back.alarm.severity, 0);
}

// ---- Family H: timestamp advances on each process -------------------------

/// Each process must stamp a real wall-clock timestamp that does not go
/// backwards between successive puts. Pins the nsec-WallTime family.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn h_timestamp_advances_on_process() {
    let ioc = RegressionIoc::boot().await.expect("boot");
    let ca = ioc.ca_client().await;

    let ch = ca.create_channel("REG:H:AI");
    ch.wait_connected(Duration::from_secs(3))
        .await
        .expect("connect");
    let mut mon = ch.subscribe().await.expect("subscribe");
    let _init = recv_within(&mut mon, 1500).await.expect("initial");

    ca.caput("REG:H:AI", "1").await.expect("caput 1");
    let s1 = recv_until(
        &mut mon,
        1500,
        |s| matches!(s.value, EpicsValue::Double(v) if v == 1.0),
    )
    .await
    .expect("event for VAL=1");

    tokio::time::sleep(Duration::from_millis(20)).await;
    ca.caput("REG:H:AI", "2").await.expect("caput 2");
    let s2 = recv_until(
        &mut mon,
        1500,
        |s| matches!(s.value, EpicsValue::Double(v) if v == 2.0),
    )
    .await
    .expect("event for VAL=2");

    // Real wall-clock (post-2023), and non-decreasing across processes.
    let t1 = s1.timestamp.since_unix_epoch();
    let t2 = s2.timestamp.since_unix_epoch();
    assert!(
        t1 >= Duration::from_secs(1_700_000_000),
        "timestamp must be a real wall-clock time, got {t1:?}"
    );
    assert!(
        t2 >= t1,
        "timestamp must not go backwards across processes: {t1:?} -> {t2:?}"
    );
}

// ---- Family I: monitor event-MASK routing (MDEL/ADEL → DBE class) ----------

/// A field's monitor post must carry the correct DBE *class*, and a subscriber
/// filtering by an explicit `DBE_*` mask must receive exactly the posts whose
/// class intersects it. With `MDEL=10`/`ADEL=0`, a sub-MDEL VAL change crosses
/// ADEL but not MDEL, so it posts `DBE_LOG` only: a `DBE_LOG` subscriber sees
/// it, a `DBE_VALUE`-only subscriber must NOT; a supra-MDEL change posts
/// `DBE_VALUE|DBE_LOG` and reaches the `DBE_VALUE` subscriber. Pins the
/// per-field event-mask contract (which DBE class a field posts) — the family
/// behind the motor force-post, the scaler idle-`DBE_LOG` sweep, and the scaler
/// value-only change. Family B (post-only-on-change) does not cover the *class*
/// of the post, so a field posting the wrong DBE class passes every other test.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn i_deadband_routes_value_vs_log_event_masks() {
    let ioc = RegressionIoc::boot().await.expect("boot");
    let ca = ioc.ca_client().await;

    // Two independent subscriptions on the same PV, like two `camonitor`
    // processes: one DBE_VALUE-only, one DBE_LOG-only. deadband 0.0 leaves the
    // server's MDEL/ADEL gate (not a client-side deadband) in control of which
    // DBE class each VAL post carries.
    let ch_val = ca.create_channel("REG:I:AI");
    ch_val
        .wait_connected(Duration::from_secs(3))
        .await
        .expect("connect VALUE");
    let mut mon_val = ch_val
        .subscribe_with_mask(0.0, DBE_VALUE)
        .await
        .expect("subscribe DBE_VALUE");

    let ch_log = ca.create_channel("REG:I:AI");
    ch_log
        .wait_connected(Duration::from_secs(3))
        .await
        .expect("connect LOG");
    let mut mon_log = ch_log
        .subscribe_with_mask(0.0, DBE_LOG)
        .await
        .expect("subscribe DBE_LOG");

    // Prime: the first process fires unconditionally (NaN MLST/ALST sentinel),
    // advancing MLST=ALST=100 so the next change is gated by the real
    // deadbands. Sync BOTH subscribers to the primed value before the
    // discriminating puts, so any later event is strictly post-priming.
    ca.caput("REG:I:AI", "100").await.expect("caput 100");
    recv_until(&mut mon_val, 2000, val_is(100.0))
        .await
        .expect("DBE_VALUE sub primed at 100");
    recv_until(&mut mon_log, 2000, val_is(100.0))
        .await
        .expect("DBE_LOG sub primed at 100");

    // Sub-MDEL change 100 -> 105 (|Δ|=5 ≤ MDEL=10, > ADEL=0): VAL posts
    // DBE_LOG only.
    ca.caput("REG:I:AI", "105").await.expect("caput 105");
    // The DBE_LOG subscriber MUST receive it...
    recv_until(&mut mon_log, 2000, val_is(105.0))
        .await
        .expect("DBE_LOG sub must receive the sub-MDEL (archive-only) post");
    // ...and the DBE_VALUE-only subscriber MUST NOT (a LOG-classed post does
    // not intersect DBE_VALUE). The LOG receive above proves the post cycle
    // already dispatched to all subscriptions, so this window is conclusive.
    let leaked = recv_within(&mut mon_val, 700).await;
    assert!(
        leaked.is_none(),
        "a sub-MDEL change posts DBE_LOG only; a DBE_VALUE subscriber must not \
         receive it, got {leaked:?}"
    );

    // Supra-MDEL change 105 -> 120 (|Δ| from MLST=100 is 20 > 10): VAL posts
    // DBE_VALUE|DBE_LOG, so the DBE_VALUE subscriber now DOES receive it —
    // proving it was alive and correctly filtered the LOG-only post above.
    ca.caput("REG:I:AI", "120").await.expect("caput 120");
    recv_until(&mut mon_val, 2000, val_is(120.0))
        .await
        .expect("DBE_VALUE sub must receive the supra-MDEL value post");
}

// ---- Family K: .PROC=0 force-process + put-callback completion -------------

/// A caput to `REC.PROC` with value **0** must force a process (a PROC write of
/// 0 was once a silent no-op), and a put-with-callback (CA WRITE_NOTIFY) must
/// return only after processing completes. Seed K:SRC, confirm the Passive
/// K:CALC has not yet mirrored it, then force `K:CALC.PROC=0`; an IMMEDIATE
/// caget (no polling) must already equal SRC — proving both the forced process
/// and that the put waited for completion. Distinct from Family A, which forces
/// with `.PROC=1` and then *polls* the OUT-link target.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn k_proc_zero_forces_process_and_put_completes() {
    let ioc = RegressionIoc::boot().await.expect("boot");
    let ca = ioc.ca_client().await;

    // Seed the source. K:CALC is Passive with no link back from SRC, so this
    // does NOT process K:CALC.
    ca.caput("REG:K:SRC", "9").await.expect("caput SRC");
    let (_dbf, before) = ca.caget("REG:K:CALC").await.expect("caget CALC before");
    assert!(
        !matches!(as_f64(&before), Some(x) if (x - 9.0).abs() < 1e-9),
        "K:CALC must not mirror SRC until it is processed, got {before:?}"
    );

    // Force a process via a PROC write of *0* — the regression: 0 was a no-op.
    ca.caput("REG:K:CALC.PROC", "0")
        .await
        .expect("force process via PROC=0");

    // The put-callback has returned, so processing is complete: read ONCE, no
    // poll. A correct WRITE_NOTIFY put makes this deterministic.
    let (_dbf, after) = ca.caget("REG:K:CALC").await.expect("caget CALC after");
    assert!(
        matches!(as_f64(&after), Some(x) if (x - 9.0).abs() < 1e-9),
        "PROC=0 must force a process and the put-callback must complete it \
         before returning; immediate caget got {after:?}"
    );
}
