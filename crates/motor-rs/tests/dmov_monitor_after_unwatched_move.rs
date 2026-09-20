//! A move finished with no `.DMOV` monitor must leave DMOV's published value
//! at the completion's 1, so a client that subscribes afterwards receives the
//! next move's DMOV 1→0→1. ophyd `EpicsMotor` completes a move only on that
//! transition; with the move-start 0 dropped, its first `mv` never returned.
//! C posts both halves on an empty `mlis` too (`motorRecord.cc:2603-2606`,
//! `:3628-3629`), so a later subscriber sees them.

mod common;

use common::{MotorFixture, drain};
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};

#[tokio::test]
async fn subscriber_after_unwatched_move_sees_dmov_zero_then_one() {
    let mut fx = MotorFixture::new(|_| {}).await;

    // Client A: moves with no DMOV monitor.
    fx.move_to(1.0).await;

    // Client B: subscribes, then moves.
    let mut rx = fx.subscribe(
        "DMOV",
        1,
        DbFieldType::Short,
        EventMask::VALUE | EventMask::LOG,
    );
    fx.move_to(2.0).await;

    assert_eq!(
        drain(&mut rx),
        [EpicsValue::Short(0), EpicsValue::Short(1)],
        "the move after an unwatched move must post DMOV 0 at its start"
    );
}
