//! R9-79 — NDCircBuffControl gates the whole plugin, not just the completion.
//!
//! C `NDPluginCircularBuff::processCallbacks` (NDPluginCircularBuff.cpp:121-203)
//! wraps its ENTIRE body in the acquisition gate:
//!
//! ```cpp
//! // Are we running?
//! if (scopeControl) {
//!     ... soft/calc trigger evaluation ...
//!     pArrayCpy = this->pNDArrayPool->copy(pArray, NULL, 1);
//!     ... pre-buffer add / flush / post count / doCallbacks ...
//!     if (currentPostCount >= postCount) { ... }   // completion, also inside
//! } else {
//!     // Currently do nothing
//! }
//! ```
//!
//! So while `NDCircBuffControl` is 0 an arriving frame is not triggered on, not
//! copied, not buffered, and does not advance the completion test — and Control
//! is 0 in three distinct situations: before the first `Control = 1` write (the
//! param's default), after a user stop (`writeInt32(Control, 0)`, :257-260), and
//! after the preset trigger count completes the last sequence (:194).
//!
//! The port had no Control state at all. `push()` recorded every frame it was
//! handed and inferred "not running" from the status string, which only covered
//! the completed case.

use std::sync::Arc;

use ad_core_rs::ndarray::{NDArray, NDDataType, NDDimension};
use ad_plugins_rs::circular_buff::{CircularBuffer, FrameParams, TriggerCondition};

fn make_array(id: i32) -> Arc<NDArray> {
    let mut arr = NDArray::new(vec![NDDimension::new(4)], NDDataType::UInt8);
    arr.unique_id = id;
    Arc::new(arr)
}

/// Control has never been turned on: every frame is ignored outright.
#[test]
fn r9_79_frames_are_ignored_until_control_turns_acquisition_on() {
    let mut cb = CircularBuffer::new(3, 1, TriggerCondition::External);

    for i in 0..5 {
        let r = cb.push(make_array(i));
        assert!(r.forward.is_empty(), "a stopped plugin forwards nothing");
        assert_eq!(
            r.params,
            FrameParams::default(),
            "a stopped plugin posts no parameters — C's else arm is \
             `// Currently do nothing`"
        );
    }
    assert_eq!(
        cb.pre_buffer_len(),
        0,
        "with Control = 0 no frame reaches the pre-buffer ring"
    );
}

/// A user stop mid-capture (C `writeInt32(Control, 0)`) closes the same gate:
/// the very next frame is ignored.
#[test]
fn r9_79_user_stop_stops_recording_immediately() {
    let mut cb = CircularBuffer::new(3, 2, TriggerCondition::External);
    cb.start();
    cb.push(make_array(1));
    cb.push(make_array(2));
    assert_eq!(cb.pre_buffer_len(), 2, "recording while Control = 1");

    cb.stop();
    let r = cb.push(make_array(3));

    assert!(r.forward.is_empty(), "stopped: nothing forwarded");
    assert_eq!(
        cb.pre_buffer_len(),
        2,
        "stopped: the frame does not join the ring (C leaves the ring alone on \
         stop, but records nothing more into it)"
    );
}

/// A soft trigger while stopped cannot start a flush — C latches `triggered`
/// only inside the `if (scopeControl)` body.
#[test]
fn r9_79_trigger_while_stopped_does_not_arm_a_flush() {
    let mut cb = CircularBuffer::new(2, 1, TriggerCondition::External);
    cb.start();
    cb.push(make_array(1));
    cb.stop();

    cb.trigger();
    assert!(!cb.is_triggered(), "a stopped plugin cannot be triggered");

    let r = cb.push(make_array(2));
    assert!(
        r.forward.is_empty(),
        "and no flush happens on the next frame"
    );
}

/// The preset-trigger-count completion turns Control off (C `:194`), and that IS
/// what stops the following frames — the same gate, not a separate "completed"
/// special case.
#[test]
fn r9_79_completion_turns_control_off() {
    let mut cb = CircularBuffer::new(1, 1, TriggerCondition::External);
    cb.start();
    cb.set_preset_trigger_count(1);

    cb.push(make_array(1)); // pre-buffer
    cb.trigger();
    let done = cb.push(make_array(2)); // post-trigger frame completes the sequence
    assert!(done.sequence_done);
    assert_eq!(done.params.control, Some(0), "C posts Control = 0");
    assert!(!cb.is_running(), "and the gate itself is closed");

    let r = cb.push(make_array(3));
    assert!(r.forward.is_empty(), "so the next frame is ignored");
}
