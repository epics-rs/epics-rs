//! The `snl` state machines wired to the databases they were ported from.
//!
//! `delay_do` and `femto` are pure — they take inputs and return a state. What
//! makes them an SNL *program* is the half that reads `delayDo.db` /
//! `femto.db`, decides when to evaluate, and writes back. These cases exercise
//! that half over the shipped templates, one per boundary the runner has to
//! get right rather than one per narrative.

#![cfg(tokio_backend)]
// Every case here drives an SNL runner through a real CA server. The
// reactor-free `exec_backend` — selected on a host build by
// `EPICS_RS_BUILD_EXEC_BACKEND=thread`, and unconditionally on RTEMS and
// VxWorks — has no `epics_ca_rs::server::CaServer` to build, so this
// file has no subject there. `[[test]] required-features` cannot name a
// build-script cfg, which is why the gate is here and not in
// `Cargo.toml`.

use std::collections::HashMap;
use std::time::Duration;

use epics_base_rs::types::EpicsValue;
use epics_ca_rs::server::{CaServer, CaServerBuilder};
use std_rs::snl::delay_do::{DelayDoConfig, DelayDoState};
use std_rs::snl::femto::FemtoConfig;

const P: &str = "TEST:";
const R: &str = "dd";

/// Load the shipped template plus a sink for `doSeq`'s first link, so a
/// processed `doSeq` is observable as a value rather than only as a side
/// effect.
async fn delay_do_ioc() -> CaServer {
    let template =
        std::fs::read_to_string(format!("{}/delayDo.db", std_rs::STD_DB_DIR)).expect("delayDo.db");
    let db_str = format!("{template}\nrecord(ao, \"TEST:dd:sink\") {{ field(VAL, \"0\") }}\n");
    let macros: HashMap<String, String> = [
        ("P", P),
        ("R", R),
        ("D_LNK1", "TEST:dd:sink PP"),
        ("D_STR1", "7"),
        ("DELAY", "0.15"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();

    CaServerBuilder::new()
        .port(0)
        // `sseq` comes from the calc module, not `stdRecords.dbd`, so a .db
        // that names it has to be told where the factory is.
        .register_record_type("sseq", || {
            Box::new(epics_base_rs::server::records::sseq::SseqRecord::default())
        })
        .db_string(&db_str, &macros)
        .unwrap()
        .build()
        .await
        .unwrap()
}

/// Poll until `probe` holds, or give up. Returns whether it held: the runner
/// is a task, so every assertion here is about a value arriving, not about a
/// value already being there.
async fn eventually<F>(mut probe: F) -> bool
where
    F: AsyncFnMut() -> bool,
{
    for _ in 0..300 {
        if probe().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

async fn state_of(server: &CaServer) -> String {
    match server.get(&format!("{P}{R}:state")).await.unwrap() {
        EpicsValue::String(s) => s.to_string(),
        other => panic!("state PV is not a string: {other:?}"),
    }
}

/// Write a field the way a CA client does — which processes the record, so
/// its `monitor()` posts. `CaServer::put` sets the value without processing
/// and the runner would never hear it.
async fn ca_put(server: &CaServer, pv: &str, v: EpicsValue) {
    server
        .database()
        .put_record_field_from_ca(pv, "VAL", v)
        .await
        .unwrap();
}

async fn put_calc(server: &CaServer, leaf: &str, v: f64) {
    ca_put(server, &format!("{P}{R}:{leaf}"), EpicsValue::Double(v)).await;
}

/// `init`'s `when (pvConnectCount() == pvAssignCount())` (`delayDo.st:35`)
/// fires by itself, so a started program lands in `idle` with no input at all
/// — and `{P}{R}:state` says so, which is the whole point of binding it.
#[tokio::test]
async fn a_started_program_publishes_idle_without_being_driven() {
    let server = delay_do_ioc().await;
    let db = (**server.database()).clone();
    let task = tokio::spawn(std_rs::snl::delay_do::run(DelayDoConfig::new(P, R), db));

    assert!(
        eventually(async || state_of(&server).await == "idle").await,
        "state stayed at {:?}",
        state_of(&server).await
    );
    task.abort();
}

/// `idle → standby → maybeWait → idle` with no active event in between
/// (`delayDo.st:86-90`, `:109-114`, `:138-142`). The two transient states are
/// the boundary: `maybeWait` must be passed through in the same evaluation
/// and must never appear on the state PV, because upstream comments its
/// `PVPUTSTR` out (`:112-113`).
#[tokio::test]
async fn standby_that_clears_with_nothing_active_returns_to_idle_via_no_visible_state() {
    let server = delay_do_ioc().await;
    let db = (**server.database()).clone();
    let task = tokio::spawn(std_rs::snl::delay_do::run(DelayDoConfig::new(P, R), db));
    assert!(eventually(async || state_of(&server).await == "idle").await);

    put_calc(&server, "standbyCalc", 1.0).await;
    assert!(
        eventually(async || state_of(&server).await == "standby").await,
        "a raised standby must be published"
    );

    put_calc(&server, "standbyCalc", 0.0).await;
    assert!(
        eventually(async || state_of(&server).await == "idle").await,
        "no active event arrived during standby, so maybeWait must fall to idle"
    );
    assert_ne!(
        state_of(&server).await,
        DelayDoState::MaybeWait.to_string(),
        "maybeWait is never published"
    );
    task.abort();
}

/// The STD-12 case end to end: an `activeCalc` event that lands *while the
/// machine is in `standby`* is tested by no clause there, so it has to survive
/// until `maybeWait` reads it (`delayDo.st:130`). Surviving means the machine
/// waits out DELAY and processes `doSeq` — observable here as the sink taking
/// `STR1`.
#[tokio::test]
async fn an_active_event_during_standby_survives_to_arm_the_wait_and_process_doseq() {
    let server = delay_do_ioc().await;
    let db = (**server.database()).clone();
    let task = tokio::spawn(std_rs::snl::delay_do::run(DelayDoConfig::new(P, R), db));
    assert!(eventually(async || state_of(&server).await == "idle").await);

    put_calc(&server, "standbyCalc", 1.0).await;
    assert!(eventually(async || state_of(&server).await == "standby").await);

    // Raise and drop `active` entirely inside `standby`: only the latched
    // event flag can carry it out.
    put_calc(&server, "activeCalc", 1.0).await;
    put_calc(&server, "activeCalc", 0.0).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        state_of(&server).await,
        "standby",
        "standby names no active clause, so neither edge may move the machine"
    );

    put_calc(&server, "standbyCalc", 0.0).await;
    assert!(
        eventually(async || state_of(&server).await == "waiting").await,
        "maybeWait must find the latched active event"
    );
    assert!(
        eventually(async || {
            server.get("TEST:dd:sink").await.unwrap() == EpicsValue::Double(7.0)
        })
        .await,
        "the delay must expire on its own and process doSeq"
    );
    assert!(
        eventually(async || state_of(&server).await == "idle").await,
        "action returns to idle"
    );
    task.abort();
}

/// `enable` is the one input whose clause is first in every state that has
/// one. Dropping it parks the machine in `disable` (`:80-84`); raising it
/// crosses `maybeStandby`, which — like `maybeWait` — is never published
/// (`:51-52`).
#[tokio::test]
async fn dropping_enable_parks_in_disable_and_raising_it_crosses_an_unpublished_state() {
    let server = delay_do_ioc().await;
    let db = (**server.database()).clone();
    let task = tokio::spawn(std_rs::snl::delay_do::run(DelayDoConfig::new(P, R), db));
    assert!(eventually(async || state_of(&server).await == "idle").await);

    ca_put(&server, &format!("{P}{R}:enable"), EpicsValue::Short(0)).await;
    assert!(eventually(async || state_of(&server).await == "disable").await);

    ca_put(&server, &format!("{P}{R}:enable"), EpicsValue::Short(1)).await;
    assert!(
        eventually(async || state_of(&server).await == "idle").await,
        "re-enabling falls through maybeStandby to idle"
    );
    assert_ne!(
        state_of(&server).await,
        DelayDoState::MaybeStandby.to_string(),
        "maybeStandby is never published"
    );
    task.abort();
}

/// SNL evaluates a `delay()` argument once, when the state is entered, so a
/// `{P}{R}:delay` monitor that lands mid-wait retimes the next wait and not
/// the one in flight. Raising it to 30 s after the machine is already
/// `waiting` must not stop this wait from finishing.
#[tokio::test]
async fn a_delay_change_mid_wait_does_not_retime_the_wait_in_flight() {
    let server = delay_do_ioc().await;
    let db = (**server.database()).clone();
    let task = tokio::spawn(std_rs::snl::delay_do::run(DelayDoConfig::new(P, R), db));
    assert!(eventually(async || state_of(&server).await == "idle").await);

    put_calc(&server, "activeCalc", 1.0).await;
    assert!(eventually(async || state_of(&server).await == "active").await);
    put_calc(&server, "activeCalc", 0.0).await;
    assert!(eventually(async || state_of(&server).await == "waiting").await);

    ca_put(&server, &format!("{P}{R}:delay"), EpicsValue::Double(30.0)).await;
    assert!(
        eventually(async || {
            server.get("TEST:dd:sink").await.unwrap() == EpicsValue::Double(7.0)
        })
        .await,
        "the armed 0.15 s delay must still be the one that fires"
    );
    task.abort();
}

/// C sits in `init` forever when an assigned PV never connects, which reads as
/// a hung program. A PV the database never loaded is reported instead.
#[tokio::test]
async fn a_program_whose_pvs_are_absent_reports_instead_of_hanging() {
    let server = delay_do_ioc().await;
    let db = (**server.database()).clone();

    let err = std_rs::snl::delay_do::run(DelayDoConfig::new("NOSUCH:", "dd"), db)
        .await
        .expect_err("no delayDo records exist under that prefix");
    let msg = err.to_string();
    assert!(
        msg.contains("5 of the 5 PVs"),
        "every assigned PV should be named: {msg}"
    );
}

// ---------------------------------------------------------------------------
// femto
// ---------------------------------------------------------------------------

/// `femto.st` assigns its four gain-bit PVs by whole name, so the test owns
/// them; `{P}{H}{F}` composes the rest, as `femto.db` names them.
async fn femto_ioc() -> CaServer {
    let template =
        std::fs::read_to_string(format!("{}/femto.db", std_rs::STD_DB_DIR)).expect("femto.db");
    let db_str = format!(
        "{template}\n\
         record(longout, \"TEST:fem:bit0\") {{ field(VAL, \"0\") }}\n\
         record(longout, \"TEST:fem:bit1\") {{ field(VAL, \"0\") }}\n\
         record(longout, \"TEST:fem:bit2\") {{ field(VAL, \"0\") }}\n\
         record(longout, \"TEST:fem:noise\") {{ field(VAL, \"0\") }}\n"
    );
    let macros: HashMap<String, String> = [("P", "TEST:"), ("H", "fem"), ("F", "01")]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

    CaServerBuilder::new()
        .port(0)
        .db_string(&db_str, &macros)
        .unwrap()
        .build()
        .await
        .unwrap()
}

fn femto_config() -> FemtoConfig {
    FemtoConfig::new(
        "TEST:",
        "fem",
        "01",
        "TEST:fem:bit0",
        "TEST:fem:bit1",
        "TEST:fem:bit2",
        "TEST:fem:noise",
    )
}

/// `init` reads the bits, finds them all clear, and takes the documented
/// default of index 8 = 1e3 (`femto.st:58-62`), publishing `gainidx` and
/// `gain` before anything drives it.
#[tokio::test]
async fn femto_init_publishes_the_all_bits_off_default() {
    let server = femto_ioc().await;
    let db = (**server.database()).clone();
    let task = tokio::spawn(std_rs::snl::femto::run(femto_config(), db));

    assert!(
        eventually(async || {
            server.get("TEST:fem01gain").await.unwrap() == EpicsValue::Double(1e3)
        })
        .await,
        "gain stayed at {:?}",
        server.get("TEST:fem01gain").await.unwrap()
    );
    task.abort();
}

/// A `gainidx` put takes `changeGain`'s applying arm, which is the only arm
/// that drives the amplifier's bit PVs (`femto.st:157-281`). Index 3 is
/// `t0=1, t1=1, t2=0, tx=0`.
#[tokio::test]
async fn femto_gain_index_put_drives_the_bit_pvs() {
    let server = femto_ioc().await;
    let db = (**server.database()).clone();
    let task = tokio::spawn(std_rs::snl::femto::run(femto_config(), db));
    assert!(
        eventually(async || {
            server.get("TEST:fem01gain").await.unwrap() == EpicsValue::Double(1e3)
        })
        .await
    );

    ca_put(&server, "TEST:fem01gainidx", EpicsValue::Short(3)).await;

    assert!(
        eventually(async || {
            server.get("TEST:fem:bit0").await.unwrap() == EpicsValue::Long(1)
                && server.get("TEST:fem:bit1").await.unwrap() == EpicsValue::Long(1)
                && server.get("TEST:fem:bit2").await.unwrap() == EpicsValue::Long(0)
                && server.get("TEST:fem:noise").await.unwrap() == EpicsValue::Long(0)
        })
        .await,
        "bits are {:?} {:?} {:?} {:?}",
        server.get("TEST:fem:bit0").await.unwrap(),
        server.get("TEST:fem:bit1").await.unwrap(),
        server.get("TEST:fem:bit2").await.unwrap(),
        server.get("TEST:fem:noise").await.unwrap()
    );
    assert!(
        eventually(async || {
            server.get("TEST:fem01gain").await.unwrap() == EpicsValue::Double(1e8)
        })
        .await,
        "index 3 is 10^8"
    );
    task.abort();
}

/// A bit PV moved by something other than the program takes `updateGain`,
/// which recomputes the index from all four bits and publishes it
/// (`femto.st:98-110`) without driving the bits back.
#[tokio::test]
async fn femto_external_bit_change_recomputes_the_index() {
    let server = femto_ioc().await;
    let db = (**server.database()).clone();
    let task = tokio::spawn(std_rs::snl::femto::run(femto_config(), db));
    assert!(
        eventually(async || {
            server.get("TEST:fem01gain").await.unwrap() == EpicsValue::Double(1e3)
        })
        .await
    );

    // `init` applied index 8, which leaves the noise bit set. Raising t2 on
    // top of it is index 12 = `(tx << 3) | (t2 << 2)`, gain 1e7 — the index
    // comes from all four bits, not from the one that moved.
    ca_put(&server, "TEST:fem:bit2", EpicsValue::Long(1)).await;

    assert!(
        eventually(async || {
            server.get("TEST:fem01gainidx").await.unwrap() == EpicsValue::Enum(12)
        })
        .await,
        "gainidx is {:?}",
        server.get("TEST:fem01gainidx").await.unwrap()
    );
    assert!(
        eventually(async || {
            server.get("TEST:fem01gain").await.unwrap() == EpicsValue::Double(1e7)
        })
        .await,
        "gain is {:?}",
        server.get("TEST:fem01gain").await.unwrap()
    );
    task.abort();
}
