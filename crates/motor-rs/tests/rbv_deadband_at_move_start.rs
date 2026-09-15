//! The move-start pass must leave the readback to the deadband, exactly as the
//! completion pass does.
//!
//! C motor `monitor()` runs on the put pass too (`motorRecord.cc:1507`) and
//! posts RBV only when `MARKED(M_RBV)` — i.e. when `process_motor_info` saw the
//! readback move (`:3715-3719`) — throttled by MDEL/ADEL, moving `mlst`/`alst`
//! to RBV (`:3468-3507`). The port's move-start notify instead change-detected
//! RBV against the framework's `last_posted` cache, which the deadband post
//! never advances (MLST is where that field's published value lives), and wrote
//! MLST from the literal field name `"VAL"`. So a move start re-posted the
//! PREVIOUS readback, and MLST held the SETPOINT — which then throttled away
//! the readback post the deadband exists to make.
//!
//! One case per boundary: MDEL = 0 (post on any change) and MDEL > 0 (post on a
//! crossing).

mod common;

use common::{MotorFixture, drain};
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};

/// MDEL = 0: each move posts its own new readback exactly once. The move-start
/// pass has no readback of its own to report.
#[tokio::test]
async fn move_start_does_not_repost_the_previous_readback() {
    let mut fx = MotorFixture::new(|_| {}).await;
    fx.move_to(1.0).await;

    let mut rx = fx.subscribe(
        "RBV",
        1,
        DbFieldType::Double,
        EventMask::VALUE | EventMask::LOG,
    );

    fx.move_to(2.0).await;
    assert_eq!(
        drain(&mut rx),
        [EpicsValue::Double(2.0)],
        "the move must post its own readback once"
    );

    fx.move_to(3.0).await;
    assert_eq!(
        drain(&mut rx),
        [EpicsValue::Double(3.0)],
        "the move start must not re-post the readback the previous move published"
    );
}

/// MDEL = 0.5: MLST tracks the last POSTED readback, never the setpoint, so the
/// completion's 1.0 crosses the deadband from 0.0 and posts.
#[tokio::test]
async fn move_start_leaves_mlst_tracking_the_readback() {
    let mut fx = MotorFixture::new(|rec| rec.disp.mdel = 0.5).await;
    let mut rx = fx.subscribe("RBV", 1, DbFieldType::Double, EventMask::VALUE);

    fx.start_move(1.0).await;
    assert_eq!(
        fx.db.get_pv("M1.MLST").unwrap(),
        EpicsValue::Double(0.0),
        "the move-start pass must leave MLST at the last posted readback, not the setpoint"
    );
    assert_eq!(
        drain(&mut rx),
        [],
        "the readback has not moved at the move start"
    );

    fx.finish_move().await;
    assert_eq!(
        drain(&mut rx),
        [EpicsValue::Double(1.0)],
        "the readback crossed MDEL and must post"
    );
    assert_eq!(
        fx.db.get_pv("M1.MLST").unwrap(),
        EpicsValue::Double(1.0),
        "the posted readback becomes MLST"
    );
}
