//! Family D — motor record, driven over the wire against the live IOC.
//!
//! The headline regression: v0.20.0 shipped a motor that did not move on a
//! caput to a `SCAN=Passive` motor's VAL (the I/O-Intr-vs-Passive dbPutField
//! processing gate). These tests pin "a caput to a Passive motor VAL drives the
//! move: RBV converges on the target and DMOV returns to done".
// The harness crate is `tokio_backend`-only, so this file is too:
// `regression_ioc::RegressionIoc` does not exist on the reactor-free backend.
#![cfg(tokio_backend)]

use std::time::Duration;

use epics_ca_rs::EpicsValue;
use regression_ioc::RegressionIoc;
use serial_test::serial;

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

/// Poll `caget pv` until its numeric value is within `tol` of `want`, or fail.
async fn await_value(
    ca: &epics_ca_rs::client::CaClient,
    pv: &str,
    want: f64,
    tol: f64,
    ms: u64,
) -> f64 {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
    let mut last = f64::NAN;
    loop {
        if let Ok((_t, v)) = ca.caget(pv).await
            && let Some(x) = as_f64(&v)
        {
            last = x;
            if (x - want).abs() <= tol {
                return x;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("{pv} never reached {want} (±{tol}); last read {last}");
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

/// A caput to a SCAN=Passive motor's VAL must drive a real move: RBV converges
/// on the target. Pins the v0.20.0 "caput-to-Passive-motor does not move"
/// regression.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn d_motor_moves_on_caput_to_passive_val() {
    let ioc = RegressionIoc::boot().await.expect("boot");
    let ca = ioc.ca_client().await;

    // Settle the initial readback (motor starts homed near 0).
    await_value(&ca, "REG:D:MTR.RBV", 0.0, 0.5, 3000).await;

    // Commanding VAL=5.0 must move the motor; RBV converges on 5.0.
    ca.caput("REG:D:MTR", "5.0").await.expect("caput VAL");
    let rbv = await_value(&ca, "REG:D:MTR.RBV", 5.0, 0.2, 6000).await;
    assert!(
        (rbv - 5.0).abs() <= 0.2,
        "RBV must converge on commanded 5.0, got {rbv}"
    );

    // When the move completes the motor reports done (DMOV=1).
    let dmov = await_value(&ca, "REG:D:MTR.DMOV", 1.0, 0.0, 4000).await;
    assert_eq!(dmov, 1.0, "DMOV must return to done (1) after the move");
}

/// A second commanded move in the opposite direction must also converge — the
/// motor is not a one-shot. Guards the move state machine across re-commands.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn d_motor_moves_again_on_second_caput() {
    let ioc = RegressionIoc::boot().await.expect("boot");
    let ca = ioc.ca_client().await;

    await_value(&ca, "REG:D:MTR.RBV", 0.0, 0.5, 3000).await;

    ca.caput("REG:D:MTR", "3.0").await.expect("caput VAL 3");
    await_value(&ca, "REG:D:MTR.RBV", 3.0, 0.2, 6000).await;

    ca.caput("REG:D:MTR", "-2.0").await.expect("caput VAL -2");
    let rbv = await_value(&ca, "REG:D:MTR.RBV", -2.0, 0.2, 6000).await;
    assert!(
        (rbv + 2.0).abs() <= 0.2,
        "RBV must converge on commanded -2.0, got {rbv}"
    );
}
