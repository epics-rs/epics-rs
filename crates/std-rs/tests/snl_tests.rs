#![allow(clippy::field_reassign_with_default)]
use std::time::Duration;
use std_rs::snl::delay_do::*;
use std_rs::snl::femto::*;

// ============================================================
// Femto controller
// ============================================================

#[test]
fn test_gain_lookup_table() {
    assert_eq!(POWERS[0], 5); // 10^5
    assert_eq!(POWERS[1], 6); // 10^6
    assert_eq!(POWERS[6], 11); // 10^11
    assert_eq!(POWERS[7], 0); // unused
    assert_eq!(POWERS[8], 3); // 10^3
    assert_eq!(POWERS[14], 9); // 10^9
    assert_eq!(POWERS[15], 0); // unused
}

#[test]
fn test_bits_to_gain_index() {
    assert_eq!(bits_to_gain_index(false, false, false, false), 0);
    assert_eq!(bits_to_gain_index(true, false, false, false), 1);
    assert_eq!(bits_to_gain_index(false, true, false, false), 2);
    assert_eq!(bits_to_gain_index(true, true, false, false), 3);
    assert_eq!(bits_to_gain_index(false, false, true, false), 4);
    assert_eq!(bits_to_gain_index(false, false, false, true), 8);
    assert_eq!(bits_to_gain_index(true, true, true, true), 15);
}

#[test]
fn test_gain_index_to_bits() {
    assert_eq!(gain_index_to_bits(0), (false, false, false, false));
    assert_eq!(gain_index_to_bits(1), (true, false, false, false));
    assert_eq!(gain_index_to_bits(5), (true, false, true, false));
    assert_eq!(gain_index_to_bits(8), (false, false, false, true));
}

#[test]
fn test_roundtrip_bits() {
    for idx in 0..16 {
        let (g1, g2, g3, no) = gain_index_to_bits(idx);
        assert_eq!(bits_to_gain_index(g1, g2, g3, no), idx);
    }
}

#[test]
fn test_is_valid_gain_index() {
    assert!(is_valid_gain_index(0));
    assert!(is_valid_gain_index(6));
    assert!(!is_valid_gain_index(7)); // UNUSED
    assert!(is_valid_gain_index(8));
    assert!(is_valid_gain_index(14));
    assert!(!is_valid_gain_index(15)); // MAX_GAIN
    assert!(!is_valid_gain_index(-1));
    assert!(!is_valid_gain_index(16));
}

#[test]
fn test_gain_for_index() {
    assert_eq!(gain_for_index(0), 1e5);
    assert_eq!(gain_for_index(1), 1e6);
    assert_eq!(gain_for_index(6), 1e11);
    assert_eq!(gain_for_index(8), 1e3);
    assert_eq!(gain_for_index(14), 1e9);
}

#[test]
fn test_femto_init_all_bits_off() {
    let mut ctrl = FemtoController::default();
    ctrl.step(None);
    // All bits off → default to gainidx=8 (1e3)
    assert_eq!(ctrl.gain_index, 8);
    assert_eq!(ctrl.gain, 1e3);
    assert_eq!(ctrl.state, FemtoState::ChangeGain);
}

#[test]
fn test_femto_change_gain() {
    let mut ctrl = FemtoController::default();
    ctrl.step(None); // Init → ChangeGain
    ctrl.step(None); // ChangeGain → applies gain, goes Idle
    assert_eq!(ctrl.state, FemtoState::Idle);
    assert_eq!(ctrl.current_gain, ctrl.gain_index);
}

#[test]
fn test_femto_invalid_gain_rejected() {
    let mut ctrl = FemtoController::default();
    ctrl.step(None); // Init
    ctrl.step(None); // ChangeGain → Idle

    // Request invalid gain index 7
    ctrl.step(Some(FemtoEvent::GainIndexChanged(7)));
    assert_eq!(ctrl.state, FemtoState::ChangeGain);
    ctrl.step(None); // Should revert
    assert_eq!(ctrl.state, FemtoState::Idle);
}

#[test]
fn test_femto_bits_changed() {
    let mut ctrl = FemtoController::default();
    ctrl.step(None); // Init
    ctrl.step(None); // ChangeGain → Idle

    // External bit change
    ctrl.step(Some(FemtoEvent::BitsChanged {
        g1: true,
        g2: false,
        g3: true,
        no: false,
    }));
    assert_eq!(ctrl.state, FemtoState::UpdateGain);

    ctrl.step(None); // UpdateGain → Idle
    assert_eq!(ctrl.state, FemtoState::Idle);
    assert_eq!(ctrl.gain_index, 5); // 0b0101 = 5
    assert_eq!(ctrl.gain, 1e10);
}

// ============================================================
// DelayDo controller
// ============================================================

fn make_inputs(
    enable: bool,
    enable_changed: bool,
    standby: bool,
    standby_changed: bool,
    active: bool,
    active_changed: bool,
) -> DelayDoInputs {
    DelayDoInputs {
        enable,
        enable_changed,
        standby,
        standby_changed,
        active,
        active_changed,
    }
}

#[test]
fn test_delay_do_init_to_idle() {
    let mut ctrl = DelayDoController::default();
    assert_eq!(ctrl.state, DelayDoState::Init);

    let inputs = make_inputs(true, false, false, false, false, false);
    let (action, state) = ctrl.step(&inputs);
    assert_eq!(state, DelayDoState::Idle);
    assert_eq!(action, DelayDoAction::None);
}

#[test]
fn test_delay_do_disable() {
    let mut ctrl = DelayDoController::new(0.0);
    ctrl.step(&make_inputs(true, false, false, false, false, false)); // Init→Idle

    // Disable
    let (_, state) = ctrl.step(&make_inputs(false, true, false, false, false, false));
    assert_eq!(state, DelayDoState::Disable);
}

#[test]
fn test_delay_do_idle_to_standby() {
    let mut ctrl = DelayDoController::new(0.0);
    ctrl.step(&make_inputs(true, false, false, false, false, false)); // Init→Idle

    let (_, state) = ctrl.step(&make_inputs(true, false, true, true, false, false));
    assert_eq!(state, DelayDoState::Standby);
}

#[test]
fn test_delay_do_standby_to_waiting() {
    let mut ctrl = DelayDoController::new(0.0);
    ctrl.step(&make_inputs(true, false, false, false, false, false)); // Init→Idle
    ctrl.step(&make_inputs(true, false, true, true, false, false)); // Idle→Standby

    // Active happened during standby
    ctrl.step(&make_inputs(true, false, true, false, true, true)); // Standby (active seen)

    // Standby ends
    let (_, state) = ctrl.step(&make_inputs(true, false, false, true, false, false));
    assert_eq!(state, DelayDoState::MaybeWait);
}

#[test]
fn test_delay_do_action_fires() {
    let mut ctrl = DelayDoController::new(0.0); // 0 delay
    ctrl.step(&make_inputs(true, false, false, false, false, false)); // Init→Idle
    ctrl.step(&make_inputs(true, false, false, false, true, true)); // Idle→Active

    // Active goes false → Waiting
    ctrl.step(&make_inputs(true, false, false, false, false, true));
    assert_eq!(ctrl.state, DelayDoState::Waiting);

    // With 0 delay, next step should move to Action
    std::thread::sleep(Duration::from_millis(5));
    let (_, state) = ctrl.step(&make_inputs(true, false, false, false, false, false));
    assert_eq!(state, DelayDoState::Action);

    // Action step fires ProcessAction and goes to Idle
    let (action, state) = ctrl.step(&make_inputs(true, false, false, false, false, false));
    assert_eq!(action, DelayDoAction::ProcessAction);
    assert_eq!(state, DelayDoState::Idle);
}

#[test]
fn test_delay_do_state_display() {
    assert_eq!(format!("{}", DelayDoState::Init), "init");
    assert_eq!(format!("{}", DelayDoState::Idle), "idle");
    assert_eq!(format!("{}", DelayDoState::Action), "action");
}
