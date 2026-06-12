//! `dbPutField` processing-gate parity for the motor record.
//!
//! C `dbPutField` (`dbAccess.c:1263-1268`) processes a Passive record on
//! a put only when the put field is `pp(TRUE)` in `motorRecord.dbd`. The
//! motor entry in `epics-base-rs::server::record::process_passive` models
//! that set (plus the motor-rs special()-transport extensions); these
//! tests pin the gate end-to-end through the database put path.

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::types::EpicsValue;
use motor_rs::MotorRecord;
use motor_rs::flags::*;

fn make_record(state: motor_rs::device_state::SharedDeviceState) -> MotorRecord {
    let mut rec = MotorRecord::new();
    rec.set_device_state(state);
    rec.stat.msta = MstaFlags::DONE;
    rec
}

#[tokio::test]
async fn non_pp_put_stores_without_processing() {
    // VELO has no pp(TRUE) in motorRecord.dbd: C stores it via special()
    // and runs NO process pass — no do_work chain end, no implicit
    // GET_INFO, STUP stays OFF.
    let state = motor_rs::device_state::new_shared_state();
    let db = PvDatabase::new();
    db.add_record("M1", Box::new(make_record(state.clone())))
        .await
        .unwrap();

    db.put_record_field_from_ca_no_notify("M1", "VELO", EpicsValue::Double(2.0))
        .await
        .unwrap();

    assert_eq!(
        db.get_pv("M1.VELO").await.unwrap(),
        EpicsValue::Double(2.0),
        "the store itself lands without a process pass"
    );
    assert_eq!(
        db.get_pv("M1.STUP").await.unwrap(),
        EpicsValue::Short(0),
        "no process pass -> no implicit GET_INFO, STUP stays OFF"
    );
    assert!(
        state.lock().unwrap().pending_actions.is_none(),
        "no process pass -> nothing written to the device mailbox"
    );
}

#[tokio::test]
async fn sync_zero_release_processes_with_implicit_get_info() {
    // SYNC is pp(TRUE): a SYNC=0 release runs the process pass even
    // though the motor put handler latches nothing for it. The pass
    // falls to the do_work chain end (C 2540-2557) — implicit GET_INFO,
    // STUP -> BUSY.
    let state = motor_rs::device_state::new_shared_state();
    let db = PvDatabase::new();
    db.add_record("M1", Box::new(make_record(state.clone())))
        .await
        .unwrap();

    db.put_record_field_from_ca_no_notify("M1", "SYNC", EpicsValue::Short(0))
        .await
        .unwrap();

    assert_eq!(
        db.get_pv("M1.STUP").await.unwrap(),
        EpicsValue::Short(2),
        "pp(TRUE) pass ends at the chain end: implicit GET_INFO, STUP BUSY"
    );
    let actions = state.lock().unwrap().pending_actions.take();
    assert!(
        actions.is_some_and(|a| a.status_refresh),
        "the implicit GET_INFO requests a device status refresh"
    );
}

#[tokio::test]
async fn user_limit_put_processes_with_implicit_get_info() {
    // HLM is pp(TRUE) with no last_write on the Rust side: the pass it
    // triggers is the src-less housekeeping pass, ending at the chain
    // end with the implicit GET_INFO like C.
    let state = motor_rs::device_state::new_shared_state();
    let db = PvDatabase::new();
    db.add_record("M1", Box::new(make_record(state.clone())))
        .await
        .unwrap();

    db.put_record_field_from_ca_no_notify("M1", "HLM", EpicsValue::Double(50.0))
        .await
        .unwrap();

    assert_eq!(
        db.get_pv("M1.STUP").await.unwrap(),
        EpicsValue::Short(2),
        "pp(TRUE) limit put processes and fires the implicit GET_INFO"
    );
}

#[tokio::test]
async fn pid_gain_put_rides_its_pass_to_the_driver() {
    // PCOF is NOT pp(TRUE) in C (special() forwards SET_PGAIN with no
    // process pass), but motor-rs has no put-time driver channel: the
    // SetPidGain command rides the process pass that follows the put.
    // The motor pp entry therefore keeps PCOF as a special()-transport
    // extension — this pins that the forward still reaches the device
    // mailbox under the modeled gate.
    let state = motor_rs::device_state::new_shared_state();
    let mut rec = make_record(state.clone());
    rec.stat.msta = MstaFlags::DONE | MstaFlags::GAIN_SUPPORT;
    let db = PvDatabase::new();
    db.add_record("M1", Box::new(rec)).await.unwrap();

    db.put_record_field_from_ca_no_notify("M1", "PCOF", EpicsValue::Double(0.5))
        .await
        .unwrap();

    let actions = state
        .lock()
        .unwrap()
        .pending_actions
        .take()
        .expect("the transport pass writes the device mailbox");
    assert!(
        actions
            .commands
            .iter()
            .any(|c| matches!(c, MotorCommand::SetPidGain { .. })),
        "SetPidGain forwarded through the put's process pass"
    );
}
