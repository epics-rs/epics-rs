//! The compiled `delayDo.st` and `femto.st` programs started on the databases
//! they ship with: `seq` starts the program, the state PVs and the records the
//! program writes are what the cases observe. One case per boundary rather
//! than one per narrative.

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

use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::server::{CaServer, CaServerBuilder};
use epics_seq_rs::{SeqThreadId, seq, seq_stop};
use std_rs::snl::delay_do::delayDo;
use std_rs::snl::femto::femto;

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

fn start_delay_do(server: &CaServer) -> SeqThreadId {
    // The sequencer holds a program's first state until the PINI pass is
    // over, which a bare `CaServer` has not run.
    server.database().mark_pini_done();
    seq(&delayDo, &format!("P={P},R={R}"), 0, server.database()).expect("delayDo starts")
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
    let task = start_delay_do(&server);

    assert!(
        eventually(async || state_of(&server).await == "idle").await,
        "state stayed at {:?}",
        state_of(&server).await
    );
    seq_stop(task);
}

/// `idle → standby → maybeWait → idle` with no active event in between
/// (`delayDo.st:86-90`, `:109-114`, `:138-142`). The two transient states are
/// the boundary: `maybeWait` must be passed through in the same evaluation
/// and must never appear on the state PV, because upstream comments its
/// `PVPUTSTR` out (`:112-113`).
#[tokio::test]
async fn standby_that_clears_with_nothing_active_returns_to_idle_via_no_visible_state() {
    let server = delay_do_ioc().await;
    let task = start_delay_do(&server);
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
        "maybeWait",
        "maybeWait is never published"
    );
    seq_stop(task);
}

/// The STD-12 case end to end: an `activeCalc` event that lands *while the
/// machine is in `standby`* is tested by no clause there, so it has to survive
/// until `maybeWait` reads it (`delayDo.st:130`). Surviving means the machine
/// waits out DELAY and processes `doSeq` — observable here as the sink taking
/// `STR1`.
#[tokio::test]
async fn an_active_event_during_standby_survives_to_arm_the_wait_and_process_doseq() {
    let server = delay_do_ioc().await;
    let task = start_delay_do(&server);
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
    seq_stop(task);
}

/// Every monitor queued before an evaluation reaches it. `activeCalc` raised
/// and dropped and `standbyCalc` dropped with nothing yielding between them
/// (the puts complete synchronously on this one-thread runtime) are all
/// queued when the runner next evaluates, so `maybeWait`, entered on the
/// standby drop, finds the active event (`delayDo.st:130`): C's monitor
/// callbacks have raised every flag by then.
#[tokio::test]
async fn monitors_queued_together_all_reach_the_evaluation() {
    let server = delay_do_ioc().await;
    let task = start_delay_do(&server);
    assert!(eventually(async || state_of(&server).await == "idle").await);
    put_calc(&server, "standbyCalc", 1.0).await;
    assert!(eventually(async || state_of(&server).await == "standby").await);

    put_calc(&server, "activeCalc", 1.0).await;
    put_calc(&server, "activeCalc", 0.0).await;
    put_calc(&server, "standbyCalc", 0.0).await;
    assert!(
        eventually(async || state_of(&server).await == "waiting").await,
        "maybeWait must find the active event queued before the standby drop"
    );
    assert!(
        eventually(async || {
            server.get("TEST:dd:sink").await.unwrap() == EpicsValue::Double(7.0)
        })
        .await,
        "the wait must expire and process doSeq"
    );
    seq_stop(task);
}

/// `enable` is the one input whose clause is first in every state that has
/// one. Dropping it parks the machine in `disable` (`:80-84`); raising it
/// crosses `maybeStandby`, which — like `maybeWait` — is never published
/// (`:51-52`).
#[tokio::test]
async fn dropping_enable_parks_in_disable_and_raising_it_crosses_an_unpublished_state() {
    let server = delay_do_ioc().await;
    let task = start_delay_do(&server);
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
        "maybeStandby",
        "maybeStandby is never published"
    );
    seq_stop(task);
}

/// sequencer R2-2-9 evaluates `delay(delayPeriod)` at every evaluation of the
/// state (`seq_delay`, `seq_if.c`), and `delayPeriod`'s monitor is in the
/// `waiting` event mask. So a `{P}{R}:delay` change after the machine is
/// already `waiting` retimes the wait in flight: raised to 30 s the wait does
/// not finish at 0.15 s, and lowered again below the time already waited it
/// finishes at once. (The hand port armed the delay once on entry; C wins.)
#[tokio::test]
async fn a_delay_change_mid_wait_retimes_the_wait_in_flight() {
    let server = delay_do_ioc().await;
    let task = start_delay_do(&server);
    assert!(eventually(async || state_of(&server).await == "idle").await);

    put_calc(&server, "activeCalc", 1.0).await;
    assert!(eventually(async || state_of(&server).await == "active").await);
    put_calc(&server, "activeCalc", 0.0).await;
    assert!(eventually(async || state_of(&server).await == "waiting").await);

    ca_put(&server, &format!("{P}{R}:delay"), EpicsValue::Double(30.0)).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        server.get("TEST:dd:sink").await.unwrap(),
        EpicsValue::Double(0.0),
        "the 0.15 s wait was retimed to 30 s, so doSeq must not have run"
    );
    assert_eq!(state_of(&server).await, "waiting");

    ca_put(&server, &format!("{P}{R}:delay"), EpicsValue::Double(0.05)).await;
    assert!(
        eventually(async || {
            server.get("TEST:dd:sink").await.unwrap() == EpicsValue::Double(7.0)
        })
        .await,
        "0.05 s is already past, so the wait ends"
    );
    seq_stop(task);
}

/// `{P}{R}:delay` is a monitor var (`delayDo.st:26`), written as it arrives:
/// a delay posted just before `activeCalc` drops already times the wait the
/// drop arms, though the active event is taken first.
#[tokio::test(flavor = "current_thread")]
async fn a_delay_posted_before_the_active_drop_times_its_wait() {
    let server = delay_do_ioc().await;
    let db = (**server.database()).clone();
    let task = start_delay_do(&server);
    assert!(eventually(async || state_of(&server).await == "idle").await);
    ca_put(&server, &format!("{P}{R}:delay"), EpicsValue::Double(30.0)).await;
    put_calc(&server, "activeCalc", 1.0).await;
    assert!(eventually(async || state_of(&server).await == "active").await);

    // No await between the posts: on this one-thread runtime the runner
    // finds both queued.
    for (leaf, v) in [("delay", 0.05), ("activeCalc", 0.0)] {
        let rec = db.get_record(format!("{P}{R}:{leaf}")).unwrap();
        let mut inst = rec.write();
        inst.record.put_field("VAL", EpicsValue::Double(v)).unwrap();
        inst.notify_field("VAL", EventMask::VALUE | EventMask::LOG);
    }
    assert!(
        eventually(async || {
            server.get("TEST:dd:sink").await.unwrap() == EpicsValue::Double(7.0)
        })
        .await,
        "the wait must be the 0.05 s delay, not the 30 s one"
    );
    seq_stop(task);
}

/// C sits in `init` forever when an assigned PV never connects
/// (`pvConnectCount() == pvAssignCount()`, `delayDo.st:35`), so nothing is
/// ever published to `{P}{R}:state`, which keeps `delayDo.db`'s "init". The hand port returned an error here;
/// the compiled program does what C does.
#[tokio::test]
async fn a_program_whose_pvs_are_absent_stays_in_init() {
    let server = delay_do_ioc().await;
    server.database().mark_pini_done();
    let tid = seq(&delayDo, "P=NOSUCH:,R=dd", 0, server.database()).expect("seq starts");

    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        state_of(&server).await,
        "init",
        "init never completes, so the record keeps delayDo.db's VAL"
    );
    seq_stop(tid);
}

/// STD-12 boundary 2: the flag `active` consumes (`delayDo.st:160`) does not
/// survive into the next standby cycle, so a standby with no `active` traffic
/// returns to idle without a second `doSeq`.
#[tokio::test]
async fn the_active_flag_does_not_survive_into_the_next_standby_cycle() {
    let server = delay_do_ioc().await;
    let task = start_delay_do(&server);
    assert!(eventually(async || state_of(&server).await == "idle").await);

    put_calc(&server, "standbyCalc", 1.0).await;
    assert!(eventually(async || state_of(&server).await == "standby").await);
    put_calc(&server, "activeCalc", 1.0).await;
    put_calc(&server, "standbyCalc", 0.0).await;
    assert!(eventually(async || state_of(&server).await == "active").await);
    put_calc(&server, "activeCalc", 0.0).await;
    assert!(
        eventually(async || {
            server.get("TEST:dd:sink").await.unwrap() == EpicsValue::Double(7.0)
        })
        .await
    );
    assert!(eventually(async || state_of(&server).await == "idle").await);

    server
        .put("TEST:dd:sink", EpicsValue::Double(0.0))
        .await
        .unwrap();
    put_calc(&server, "standbyCalc", 1.0).await;
    assert!(eventually(async || state_of(&server).await == "standby").await);
    put_calc(&server, "standbyCalc", 0.0).await;
    assert!(eventually(async || state_of(&server).await == "idle").await);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        server.get("TEST:dd:sink").await.unwrap(),
        EpicsValue::Double(0.0),
        "no active event in this cycle, so doSeq must not run"
    );
    seq_stop(task);
}

/// `DEBUG_PRINT_HEADER` prints `SNLtaskName` with `%s`; delayDo.st leaves it
/// unset in C, so `init` now assigns it. The program prints on stdout, so the
/// case re-runs itself as a child process and reads the line from there.
const DEBUG_CHILD: &str = "STD_DELAYDO_DEBUG_CHILD";

#[tokio::test]
async fn rv7_delaydo_debug_names_the_program() {
    if std::env::var_os(DEBUG_CHILD).is_none() {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "rv7_delaydo_debug_names_the_program",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(DEBUG_CHILD, "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8(out.stdout).unwrap();
        let line = stdout
            .lines()
            .find(|l| l.ends_with("idle -> standby"))
            .unwrap_or_else(|| panic!("no idle -> standby line in {stdout}"));
        assert!(line.contains(",delayDo,3> "), "{line}");
        return;
    }
    let server = delay_do_ioc().await;
    server
        .put(&format!("{P}{R}:debug"), EpicsValue::Long(3))
        .await
        .unwrap();
    let task = start_delay_do(&server);
    assert!(eventually(async || state_of(&server).await == "idle").await);
    put_calc(&server, "standbyCalc", 1.0).await;
    assert!(eventually(async || state_of(&server).await == "standby").await);
    seq_stop(task);
}

// ---------------------------------------------------------------------------
// femto
// ---------------------------------------------------------------------------

/// `femto.st` assigns its four gain-bit PVs by whole name, so the test owns
/// them; `{P}{H}{F}` composes the rest, as `femto.db` names them.
async fn femto_ioc() -> CaServer {
    femto_ioc_with_bits([0; 4]).await
}

/// The same, with the four bit PVs (`bit0`, `bit1`, `bit2`, `noise`) loaded at
/// `bits`, so `init` reads them.
async fn femto_ioc_with_bits(bits: [i32; 4]) -> CaServer {
    let template =
        std::fs::read_to_string(format!("{}/femto.db", std_rs::STD_DB_DIR)).expect("femto.db");
    let db_str = format!(
        "{template}\n\
         record(longout, \"TEST:fem:bit0\") {{ field(VAL, \"{}\") }}\n\
         record(longout, \"TEST:fem:bit1\") {{ field(VAL, \"{}\") }}\n\
         record(longout, \"TEST:fem:bit2\") {{ field(VAL, \"{}\") }}\n\
         record(longout, \"TEST:fem:noise\") {{ field(VAL, \"{}\") }}\n",
        bits[0], bits[1], bits[2], bits[3]
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

fn start_femto(server: &CaServer) -> SeqThreadId {
    server.database().mark_pini_done();
    let macros =
        "P=TEST:,H=fem,F=01,G1=TEST:fem:bit0,G2=TEST:fem:bit1,G3=TEST:fem:bit2,NO=TEST:fem:noise";
    seq(&femto, macros, 0, server.database()).expect("femto starts")
}

/// `init` reads the bits, finds them all clear, and takes the documented
/// default of index 8 = 1e3 (`femto.st:69-74`), publishing `gainidx` and
/// `gain` before anything drives it.
#[tokio::test]
async fn femto_init_publishes_the_all_bits_off_default() {
    let server = femto_ioc().await;
    let task = start_femto(&server);

    assert!(
        eventually(async || {
            server.get("TEST:fem01gain").await.unwrap() == EpicsValue::Double(1e3)
        })
        .await,
        "gain stayed at {:?}",
        server.get("TEST:fem01gain").await.unwrap()
    );
    seq_stop(task);
}

/// A `gainidx` put takes `changeGain`'s applying arm, which is the only arm
/// that drives the amplifier's bit PVs (`femto.st:196-336`). Index 3 is
/// `t0=1, t1=1, t2=0, tx=0`.
#[tokio::test]
async fn femto_gain_index_put_drives_the_bit_pvs() {
    let server = femto_ioc().await;
    let task = start_femto(&server);
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
    seq_stop(task);
}

/// A bit PV moved by something other than the program takes `updateGain`,
/// which recomputes the index from all four bits and publishes it
/// (`femto.st:120-148`) without driving the bits back.
#[tokio::test]
async fn femto_external_bit_change_recomputes_the_index() {
    let server = femto_ioc().await;
    let task = start_femto(&server);
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
    seq_stop(task);
}

/// A bit PV outside {0,1} takes the invalid-gain fallback (index 6, 1e11)
/// instead of C's unchecked shift, whose negative result indexed `powers[]`.
#[tokio::test]
async fn femto_out_of_range_bit_falls_back_to_the_invalid_gain() {
    let server = femto_ioc().await;
    let task = start_femto(&server);
    assert!(
        eventually(async || {
            server.get("TEST:fem01gain").await.unwrap() == EpicsValue::Double(1e3)
        })
        .await
    );

    ca_put(&server, "TEST:fem:bit0", EpicsValue::Long(-1)).await;

    assert!(
        eventually(async || {
            server.get("TEST:fem01gain").await.unwrap() == EpicsValue::Double(1e11)
        })
        .await,
        "gain is {:?}",
        server.get("TEST:fem01gain").await.unwrap()
    );
    seq_stop(task);
}

/// Two bits raised back to back end on the index of both. An intermediate
/// index from `bit0` alone is not ruled out: C's CAC-event delivers `bit0`
/// within microseconds, `updateGain` reads `t1` about 55 µs later
/// (`femto.st:123`), and two CA puts land 100-160 µs apart, so C normally
/// publishes 9 and then 11.
#[tokio::test]
async fn femto_bits_raised_back_to_back_end_on_the_index_of_both() {
    let server = femto_ioc().await;
    let task = start_femto(&server);
    assert!(
        eventually(async || server.get("TEST:fem:noise").await.unwrap() == EpicsValue::Long(1))
            .await
    );

    // `init` applied index 8, the noise bit alone; bit0 and bit1 on top of
    // it are index 11, 1e6 (`femto.st:52`).
    ca_put(&server, "TEST:fem:bit0", EpicsValue::Long(1)).await;
    ca_put(&server, "TEST:fem:bit1", EpicsValue::Long(1)).await;
    assert!(
        eventually(async || {
            server.get("TEST:fem01gainidx").await.unwrap() == EpicsValue::Enum(11)
        })
        .await,
        "gainidx is {:?}",
        server.get("TEST:fem01gainidx").await.unwrap()
    );
    assert!(
        eventually(async || {
            server.get("TEST:fem01gain").await.unwrap() == EpicsValue::Double(1e6)
        })
        .await,
        "gain is {:?}",
        server.get("TEST:fem01gain").await.unwrap()
    );
    seq_stop(task);
}

/// C computes `(tx << 3) | (t2 << 2) | (t1 << 1) | t0` on whatever the bit PVs
/// hold, so a bit of 2 on top of `tx` is index 10 (1e5). Only a bit that would
/// make the shift undefined or overflow the index range is refused.
#[tokio::test]
async fn femto_a_bit_of_two_keeps_c_s_arithmetic() {
    let server = femto_ioc().await;
    let task = start_femto(&server);
    assert!(
        eventually(async || {
            server.get("TEST:fem01gain").await.unwrap() == EpicsValue::Double(1e3)
        })
        .await
    );

    // `init` left the noise bit set; t0 = 2 is then index 8 | 2 = 10.
    ca_put(&server, "TEST:fem:bit0", EpicsValue::Long(2)).await;

    assert!(
        eventually(async || {
            server.get("TEST:fem01gain").await.unwrap() == EpicsValue::Double(1e5)
                && server.get("TEST:fem01gainidx").await.unwrap() == EpicsValue::Enum(10)
        })
        .await,
        "gain is {:?}",
        server.get("TEST:fem01gain").await.unwrap()
    );
    seq_stop(task);
}

/// `init` with t0 = 1 and t1 = -1: the bits sum to zero, so C takes the
/// all-bits-off default (index 8, 1e3) without shifting a negative value.
#[tokio::test]
async fn femto_init_bits_summing_to_zero_take_the_default() {
    let server = femto_ioc_with_bits([1, -1, 0, 0]).await;
    let task = start_femto(&server);

    assert!(
        eventually(async || {
            server.get("TEST:fem01gainidx").await.unwrap() == EpicsValue::Enum(8)
                && server.get("TEST:fem01gain").await.unwrap() == EpicsValue::Double(1e3)
        })
        .await,
        "gain is {:?}",
        server.get("TEST:fem01gain").await.unwrap()
    );
    seq_stop(task);
}

/// femto runs without `+s`, so a monitor can rewrite `gainidx` or a bit PV
/// while a clause is between checking it and indexing `powers[]` with it. The
/// puts below land inside the apply arm's blocking `PVPUT`s; a program that
/// indexed `powers` with the re-read value panics its state-set thread on 99
/// and then no later put takes effect.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn femto_gainidx_put_between_check_and_use_leaves_the_program_running() {
    let server = femto_ioc().await;
    let task = start_femto(&server);
    assert!(
        eventually(async || {
            server.get("TEST:fem01gain").await.unwrap() == EpicsValue::Double(1e3)
        })
        .await
    );
    ca_put(&server, "TEST:fem01gainidx", EpicsValue::Short(3)).await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    ca_put(&server, "TEST:fem01gainidx", EpicsValue::Short(99)).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let i = server.get("TEST:fem01gainidx").await.unwrap();
    ca_put(&server, "TEST:fem01gainidx", EpicsValue::Short(4)).await;
    let alive =
        eventually(async || server.get("TEST:fem01gain").await.unwrap() == EpicsValue::Double(1e9))
            .await;
    let g = server.get("TEST:fem01gain").await.unwrap();
    seq_stop(task);
    assert!(alive, "program dead, gain {g:?} gainidx after 99 {i:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn femto_bit_put_between_read_and_use_leaves_the_program_running() {
    let server = femto_ioc().await;
    let task = start_femto(&server);
    assert!(
        eventually(async || {
            server.get("TEST:fem01gain").await.unwrap() == EpicsValue::Double(1e3)
        })
        .await
    );
    ca_put(&server, "TEST:fem:bit0", EpicsValue::Long(1)).await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    ca_put(&server, "TEST:fem:bit0", EpicsValue::Long(-1)).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    ca_put(&server, "TEST:fem01gainidx", EpicsValue::Short(4)).await;
    let alive =
        eventually(async || server.get("TEST:fem01gain").await.unwrap() == EpicsValue::Double(1e9))
            .await;
    let g = server.get("TEST:fem01gain").await.unwrap();
    seq_stop(task);
    assert!(alive, "program dead, gain {g:?}");
}
