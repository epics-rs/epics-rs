//! Integration tests for asyn-rs port driver functionality.
//!
//! Ported from C EPICS asyn test applications:
//! - testAsynPortDriver: parameter read/write, callbacks, arrays
//! - testConnect: connection state
//! - testErrors: error status propagation
//! - echoDriver: octet read/write

use std::sync::Arc;
use std::time::Duration;

use asyn_rs::error::{AsynError, AsynResult, AsynStatus};
use asyn_rs::interrupt::InterruptFilter;
use asyn_rs::manager::PortManager;
use asyn_rs::param::{EnumEntry, ParamType, ParamValue};
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::user::AsynUser;

// ============================================================
// Test port driver — simulates testAsynPortDriver.cpp
// ============================================================

struct TestScopeDriver {
    base: PortDriverBase,
    p_run: usize,
    p_max_points: usize,
    p_waveform: usize,
    p_min_value: usize,
    p_max_value: usize,
    p_mean_value: usize,
    p_noise: usize,
    #[allow(dead_code)]
    p_volts_per_div: usize,
}

impl TestScopeDriver {
    fn new(port_name: &str) -> Self {
        let mut base = PortDriverBase::new(port_name, 1, PortFlags::default());
        let p_run = base.create_param("Run", ParamType::Int32).unwrap();
        let p_max_points = base.create_param("MaxPoints", ParamType::Int32).unwrap();
        let _p_update_time = base.create_param("UpdateTime", ParamType::Float64).unwrap();
        let p_waveform = base
            .create_param("Waveform", ParamType::Float64Array)
            .unwrap();
        let p_min_value = base.create_param("MinValue", ParamType::Float64).unwrap();
        let p_max_value = base.create_param("MaxValue", ParamType::Float64).unwrap();
        let p_mean_value = base.create_param("MeanValue", ParamType::Float64).unwrap();
        let p_noise = base
            .create_param("NoiseAmplitude", ParamType::Float64)
            .unwrap();
        let p_volts_per_div = base
            .create_param("VoltsPerDivSelect", ParamType::Enum)
            .unwrap();

        base.set_int32_param(p_run, 0, 0).unwrap();
        base.set_int32_param(p_max_points, 0, 128).unwrap();
        base.set_float64_param(p_noise, 0, 0.1).unwrap();

        let choices: Arc<[EnumEntry]> = Arc::from(vec![
            EnumEntry {
                string: "0.1".into(),
                value: 0,
                severity: 0,
            },
            EnumEntry {
                string: "0.2".into(),
                value: 1,
                severity: 0,
            },
            EnumEntry {
                string: "0.5".into(),
                value: 2,
                severity: 0,
            },
            EnumEntry {
                string: "1.0".into(),
                value: 3,
                severity: 0,
            },
        ]);
        base.set_enum_choices_param(p_volts_per_div, 0, choices)
            .unwrap();

        Self {
            base,
            p_run,
            p_max_points,
            p_waveform,
            p_min_value,
            p_max_value,
            p_mean_value,
            p_noise,
            p_volts_per_div,
        }
    }

    fn compute_waveform(&mut self) {
        let max_points = self
            .base
            .get_int32_param(self.p_max_points, 0)
            .unwrap_or(128) as usize;
        let noise = self.base.get_float64_param(self.p_noise, 0).unwrap_or(0.0);

        let mut waveform = Vec::with_capacity(max_points);
        let mut min_val = f64::MAX;
        let mut max_val = f64::MIN;
        let mut sum = 0.0;

        for i in 0..max_points {
            let t = i as f64 / max_points as f64 * std::f64::consts::TAU;
            let v = t.sin() + noise * 0.5;
            waveform.push(v);
            min_val = min_val.min(v);
            max_val = max_val.max(v);
            sum += v;
        }

        let _ = self
            .base
            .params
            .set_float64_array(self.p_waveform, 0, waveform);
        let _ = self.base.set_float64_param(self.p_min_value, 0, min_val);
        let _ = self.base.set_float64_param(self.p_max_value, 0, max_val);
        let _ = self
            .base
            .set_float64_param(self.p_mean_value, 0, sum / max_points as f64);
    }
}

impl PortDriver for TestScopeDriver {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }

    fn write_int32(&mut self, user: &mut AsynUser, value: i32) -> AsynResult<()> {
        let reason = user.reason;
        self.base.set_int32_param(reason, user.addr, value)?;
        if reason == self.p_run && value != 0 {
            self.compute_waveform();
        }
        self.base.call_param_callbacks(user.addr)?;
        Ok(())
    }

    fn write_float64(&mut self, user: &mut AsynUser, value: f64) -> AsynResult<()> {
        self.base.set_float64_param(user.reason, user.addr, value)?;
        self.base.call_param_callbacks(user.addr)?;
        Ok(())
    }

    fn read_float64_array(&mut self, user: &AsynUser, buf: &mut [f64]) -> AsynResult<usize> {
        let data = self.base.params.get_float64_array(user.reason, user.addr)?;
        let n = buf.len().min(data.len());
        buf[..n].copy_from_slice(&data[..n]);
        Ok(n)
    }
}

// ============================================================
// Echo driver — simulates echoDriver.c
// ============================================================

struct EchoDriver {
    base: PortDriverBase,
    #[allow(dead_code)]
    p_msg: usize,
}

impl EchoDriver {
    fn new(port_name: &str) -> Self {
        let mut base = PortDriverBase::new(port_name, 1, PortFlags::default());
        let p_msg = base.create_param("MSG", ParamType::Octet).unwrap();
        Self { base, p_msg }
    }
}

impl PortDriver for EchoDriver {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }

    fn write_octet(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<()> {
        let s = String::from_utf8_lossy(data).to_string();
        self.base.set_string_param(user.reason, user.addr, s)?;
        self.base.call_param_callbacks(user.addr)?;
        Ok(())
    }
}

// ============================================================
// Error driver — simulates testErrors.cpp
// ============================================================

struct ErrorDriver {
    base: PortDriverBase,
    #[allow(dead_code)]
    p_val: usize,
    p_status: usize,
    fail_reads: bool,
}

impl ErrorDriver {
    fn new(port_name: &str) -> Self {
        let mut base = PortDriverBase::new(port_name, 1, PortFlags::default());
        let p_val = base.create_param("VAL", ParamType::Int32).unwrap();
        let p_status = base.create_param("STATUS", ParamType::Int32).unwrap();
        base.set_int32_param(p_val, 0, 0).unwrap();
        base.set_int32_param(p_status, 0, 0).unwrap();
        Self {
            base,
            p_val,
            p_status,
            fail_reads: false,
        }
    }
}

impl PortDriver for ErrorDriver {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }

    fn read_int32(&mut self, user: &AsynUser) -> AsynResult<i32> {
        if self.fail_reads {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: "read disabled".into(),
            });
        }
        self.base.get_int32_param(user.reason, user.addr)
    }

    fn write_int32(&mut self, user: &mut AsynUser, value: i32) -> AsynResult<()> {
        let reason = user.reason;
        if reason == self.p_status {
            self.fail_reads = value != 0;
        }
        self.base.set_int32_param(reason, user.addr, value)?;
        self.base.call_param_callbacks(user.addr)?;
        Ok(())
    }
}

// ============================================================
// Helpers
// ============================================================

fn setup_scope() -> (PortManager, asyn_rs::port_handle::PortHandle) {
    let mgr = PortManager::new();
    let rt = mgr.register_port(TestScopeDriver::new("SCOPE"));
    let handle = rt.port_handle().clone();
    (mgr, handle)
}

fn setup_echo() -> (PortManager, asyn_rs::port_handle::PortHandle) {
    let mgr = PortManager::new();
    let rt = mgr.register_port(EchoDriver::new("ECHO"));
    let handle = rt.port_handle().clone();
    (mgr, handle)
}

fn setup_error() -> (PortManager, asyn_rs::port_handle::PortHandle) {
    let mgr = PortManager::new();
    let rt = mgr.register_port(ErrorDriver::new("ERRTEST"));
    let handle = rt.port_handle().clone();
    (mgr, handle)
}

// ============================================================
// Tests: Parameter read/write (from testAsynPortDriver)
// ============================================================

#[tokio::test]
async fn test_int32_write_and_read() {
    let (_mgr, h) = setup_scope();
    let reason = h.drv_user_create("Run").await.unwrap();
    h.write_int32(reason, 0, 1).await.unwrap();
    let val = h.read_int32(reason, 0).await.unwrap();
    assert_eq!(val, 1);
}

#[tokio::test]
async fn test_float64_write_and_read() {
    let (_mgr, h) = setup_scope();
    let reason = h.drv_user_create("NoiseAmplitude").await.unwrap();
    h.write_float64(reason, 0, 0.25).await.unwrap();
    let val = h.read_float64(reason, 0).await.unwrap();
    assert!((val - 0.25).abs() < 1e-10);
}

#[tokio::test]
async fn test_float64_array_after_run() {
    let (_mgr, h) = setup_scope();
    let run = h.drv_user_create("Run").await.unwrap();
    let wf = h.drv_user_create("Waveform").await.unwrap();
    h.write_int32(run, 0, 1).await.unwrap();
    let data = h.read_float64_array(wf, 0, 256).await.unwrap();
    assert_eq!(data.len(), 128, "Default MaxPoints=128");
    // Verify it's a sine wave: first value should be ~0 (sin(0))
    assert!(
        data[0].abs() < 0.2,
        "First point should be near 0, got {}",
        data[0]
    );
}

#[tokio::test]
async fn test_computed_statistics() {
    let (_mgr, h) = setup_scope();
    let run = h.drv_user_create("Run").await.unwrap();
    let min_r = h.drv_user_create("MinValue").await.unwrap();
    let max_r = h.drv_user_create("MaxValue").await.unwrap();
    let mean_r = h.drv_user_create("MeanValue").await.unwrap();
    h.write_int32(run, 0, 1).await.unwrap();
    let min_val = h.read_float64(min_r, 0).await.unwrap();
    let max_val = h.read_float64(max_r, 0).await.unwrap();
    let mean_val = h.read_float64(mean_r, 0).await.unwrap();
    assert!(min_val < max_val);
    assert!(mean_val > min_val && mean_val < max_val);
}

#[tokio::test]
async fn test_enum_read_write() {
    let (_mgr, h) = setup_scope();
    let reason = h.drv_user_create("VoltsPerDivSelect").await.unwrap();
    h.write_enum(reason, 0, 2).await.unwrap();
    let idx = h.read_enum(reason, 0).await.unwrap();
    assert_eq!(idx, 2);
}

#[tokio::test]
async fn test_max_points_controls_waveform_size() {
    let (_mgr, h) = setup_scope();
    let mp = h.drv_user_create("MaxPoints").await.unwrap();
    let run = h.drv_user_create("Run").await.unwrap();
    let wf = h.drv_user_create("Waveform").await.unwrap();
    h.write_int32(mp, 0, 64).await.unwrap();
    h.write_int32(run, 0, 1).await.unwrap();
    let data = h.read_float64_array(wf, 0, 256).await.unwrap();
    assert_eq!(data.len(), 64, "Waveform length should match MaxPoints=64");
}

// ============================================================
// Tests: Octet echo (from echoDriver.c)
// ============================================================

#[tokio::test]
async fn test_octet_write_read() {
    let (_mgr, h) = setup_echo();
    let reason = h.drv_user_create("MSG").await.unwrap();
    h.write_octet(reason, 0, b"Hello, EPICS!".to_vec())
        .await
        .unwrap();
    let data = h.read_octet(reason, 0, 64).await.unwrap();
    assert_eq!(String::from_utf8_lossy(&data), "Hello, EPICS!");
}

#[tokio::test]
async fn test_octet_overwrite() {
    let (_mgr, h) = setup_echo();
    let reason = h.drv_user_create("MSG").await.unwrap();
    h.write_octet(reason, 0, b"first".to_vec()).await.unwrap();
    h.write_octet(reason, 0, b"second".to_vec()).await.unwrap();
    let data = h.read_octet(reason, 0, 64).await.unwrap();
    assert_eq!(String::from_utf8_lossy(&data), "second");
}

// ============================================================
// Tests: Error handling (from testErrors.cpp)
// ============================================================

#[tokio::test]
async fn test_error_normal_read() {
    let (_mgr, h) = setup_error();
    let val_r = h.drv_user_create("VAL").await.unwrap();
    h.write_int32(val_r, 0, 42).await.unwrap();
    let val = h.read_int32(val_r, 0).await.unwrap();
    assert_eq!(val, 42);
}

#[tokio::test]
async fn test_error_read_failure() {
    let (_mgr, h) = setup_error();
    let val_r = h.drv_user_create("VAL").await.unwrap();
    let sts_r = h.drv_user_create("STATUS").await.unwrap();
    h.write_int32(sts_r, 0, 1).await.unwrap();
    let result = h.read_int32(val_r, 0).await;
    assert!(result.is_err(), "Read should fail when STATUS=1");
}

#[tokio::test]
async fn test_error_recovery() {
    let (_mgr, h) = setup_error();
    let val_r = h.drv_user_create("VAL").await.unwrap();
    let sts_r = h.drv_user_create("STATUS").await.unwrap();
    h.write_int32(val_r, 0, 99).await.unwrap();
    h.write_int32(sts_r, 0, 1).await.unwrap();
    assert!(h.read_int32(val_r, 0).await.is_err());
    h.write_int32(sts_r, 0, 0).await.unwrap();
    let val = h.read_int32(val_r, 0).await.unwrap();
    assert_eq!(val, 99);
}

// ============================================================
// Tests: Interrupt/callback system
// ============================================================

#[tokio::test]
async fn test_interrupt_on_param_change() {
    let (_mgr, h) = setup_scope();
    let reason = h.drv_user_create("NoiseAmplitude").await.unwrap();
    let (_sub, mut rx) = h.interrupts().register_interrupt_user(InterruptFilter {
        reason: Some(reason),
        addr: Some(0),
    });
    h.write_float64(reason, 0, 0.5).await.unwrap();
    let iv = tokio::time::timeout(Duration::from_millis(200), rx.recv())
        .await
        .expect("timeout")
        .expect("closed");
    assert_eq!(iv.reason, reason);
    match iv.value {
        ParamValue::Float64(v) => assert!((v - 0.5).abs() < 1e-10),
        other => panic!("expected Float64, got {:?}", other),
    }
}

#[tokio::test]
async fn test_interrupt_filter_excludes_other_params() {
    let (_mgr, h) = setup_scope();
    let noise_r = h.drv_user_create("NoiseAmplitude").await.unwrap();
    let update_r = h.drv_user_create("UpdateTime").await.unwrap();

    // Flush initial changed state from constructor
    h.write_float64(noise_r, 0, 0.1).await.unwrap();
    h.write_float64(update_r, 0, 0.5).await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Subscribe only to NoiseAmplitude
    let (_sub, mut rx) = h.interrupts().register_interrupt_user(InterruptFilter {
        reason: Some(noise_r),
        addr: Some(0),
    });

    // Write only to UpdateTime — should NOT trigger NoiseAmplitude interrupt
    h.write_float64(update_r, 0, 1.0).await.unwrap();

    let result = tokio::time::timeout(Duration::from_millis(50), rx.recv()).await;
    assert!(
        result.is_err(),
        "Should not receive interrupt for unrelated param"
    );
}

#[tokio::test]
async fn test_multiple_interrupt_subscribers() {
    let (_mgr, h) = setup_scope();
    let reason = h.drv_user_create("Run").await.unwrap();
    let (_sub1, mut rx1) = h.interrupts().register_interrupt_user(InterruptFilter {
        reason: Some(reason),
        addr: Some(0),
    });
    let (_sub2, mut rx2) = h.interrupts().register_interrupt_user(InterruptFilter {
        reason: Some(reason),
        addr: Some(0),
    });
    h.write_int32(reason, 0, 1).await.unwrap();
    let iv1 = tokio::time::timeout(Duration::from_millis(200), rx1.recv())
        .await
        .expect("timeout")
        .expect("closed");
    let iv2 = tokio::time::timeout(Duration::from_millis(200), rx2.recv())
        .await
        .expect("timeout")
        .expect("closed");
    assert_eq!(iv1.reason, reason);
    assert_eq!(iv2.reason, reason);
}

// ============================================================
// Tests: DrvUser create (parameter name resolution)
// ============================================================

#[tokio::test]
async fn test_drv_user_known_param() {
    let (_mgr, h) = setup_scope();
    let reason = h.drv_user_create("Run").await.unwrap();
    assert_eq!(reason, 0);
}

#[tokio::test]
async fn test_drv_user_unknown_param() {
    let (_mgr, h) = setup_scope();
    let result = h.drv_user_create("NonExistent").await;
    assert!(result.is_err());
}

// ============================================================
// Tests: Blocking API
// ============================================================

#[test]
fn test_blocking_int32_roundtrip() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (_mgr, h) = rt.block_on(async { setup_scope() });
    let reason = h.drv_user_create_blocking("Run").unwrap();
    h.write_int32_blocking(reason, 0, 1).unwrap();
    let val = h.read_int32_blocking(reason, 0).unwrap();
    assert_eq!(val, 1);
}

#[test]
fn test_blocking_float64_roundtrip() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (_mgr, h) = rt.block_on(async { setup_scope() });
    let reason = h.drv_user_create_blocking("NoiseAmplitude").unwrap();
    h.write_float64_blocking(reason, 0, 3.15).unwrap();
    let val = h.read_float64_blocking(reason, 0).unwrap();
    assert!((val - 3.15).abs() < 1e-10);
}

// ============================================================
// Tests: Connection state (from testConnect.cpp)
// ============================================================

#[tokio::test]
async fn test_port_connected_by_default() {
    let (_mgr, h) = setup_scope();
    let reason = h.drv_user_create("Run").await.unwrap();
    assert!(h.read_int32(reason, 0).await.is_ok());
}

// ============================================================
// Tests: Callback flushes all changes
// ============================================================

#[tokio::test]
async fn test_callback_flushes_multiple_changes() {
    let (_mgr, h) = setup_scope();
    let mut broadcast_rx = h.interrupts().subscribe_async();
    let run = h.drv_user_create("Run").await.unwrap();
    h.write_int32(run, 0, 1).await.unwrap();

    let mut received_reasons = std::collections::HashSet::new();
    while let Ok(Ok(iv)) =
        tokio::time::timeout(Duration::from_millis(100), broadcast_rx.recv()).await
    {
        received_reasons.insert(iv.reason);
    }
    assert!(
        received_reasons.len() > 1,
        "Should receive interrupts for multiple params, got {:?}",
        received_reasons
    );
}
