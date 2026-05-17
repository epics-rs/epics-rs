use std::time::Instant;

use epics_base_rs::error::CaResult;
use epics_base_rs::server::device_support::{DeviceReadOutcome, DeviceSupport};
use epics_base_rs::server::record::Record;

use crate::records::epid::EpidRecord;

/// Soft Channel device support for the epid record.
///
/// Implements the PID and MaxMin feedback algorithms.
/// Ported from `devEpidSoft.c`.
///
/// PID algorithm:
/// ```text
/// E(n) = Setpoint - ControlledValue
/// P(n) = KP * E(n)
/// I(n) = I(n-1) + KP * KI * E(n) * dT  (with anti-windup)
/// D(n) = KP * KD * (E(n) - E(n-1)) / dT
/// Output = P + I + D
/// ```
pub struct EpidSoftDeviceSupport;

impl Default for EpidSoftDeviceSupport {
    fn default() -> Self {
        Self::new()
    }
}

impl EpidSoftDeviceSupport {
    pub fn new() -> Self {
        Self
    }

    /// Execute the PID algorithm on the epid record.
    /// This is the core computation, equivalent to `do_pid()` in devEpidSoft.c.
    pub fn do_pid(epid: &mut EpidRecord) {
        // Previous controlled value: CVLP, maintained by
        // `EpidRecord::update_monitors()` (`epid.rs` — `self.cvlp = self.cval`).
        // The MaxMin sign-detection in `fmod==1` needs the value from the
        // *previous* cycle, not the current CVAL. Reading `epid.cval` for
        // both would make `e = cval - pcval` identically 0.0. This matches
        // the in-tree fast path `epid_fast.rs` which uses `pcval = self.cval`
        // captured before `self.cval = cval`.
        let pcval = epid.cvlp;
        let setp = epid.val;
        let cval = epid.cval;

        // Compute delta time
        let ctp = epid.ct;
        let ct = Instant::now();
        let dt = ct.duration_since(ctp).as_secs_f64();

        // Skip if delta time is less than minimum
        if dt < epid.mdt {
            return;
        }

        let kp = epid.kp;
        let ki = epid.ki;
        let kd = epid.kd;
        let ep = epid.err;
        let mut oval = epid.oval;
        let mut p = epid.p;
        let mut i = epid.i;
        let mut d = epid.d;
        // C `devEpidSoft.c:98` declares `double e = 0.;` at function scope.
        // `devEpidSoft.c:208` writes `pepid->err = e;` *unconditionally*,
        // regardless of feedback mode. So ERR must always be assigned:
        //   - PID mode      → e = setp - cval (devEpidSoft.c:139)
        //   - MaxMin, FB on after the OFF->ON edge → e = cval - pcval
        //     (devEpidSoft.c:186)
        //   - MaxMin bumpless edge / MaxMin FB off / invalid mode → e = 0.0
        //     (the initial value from devEpidSoft.c:98 is never overwritten)
        let mut e = 0.0_f64;

        match epid.fmod {
            0 => {
                // PID mode
                e = setp - cval;
                let de = e - ep;
                p = kp * e;

                // Integral term with sanity checks
                let di = kp * ki * e * dt;
                if epid.fbon != 0 {
                    if epid.fbop == 0 {
                        // Feedback just transitioned OFF -> ON (bumpless turn-on).
                        // Set integral term to current output value.
                        // In the C code this reads from OUTL link; here we use
                        // the current OVAL as the best available approximation.
                        i = epid.oval;
                    } else {
                        // Anti-windup: only accumulate integral if output not saturated,
                        // or if the integral change would move away from saturation.
                        if (oval > epid.drvl && oval < epid.drvh)
                            || (oval >= epid.drvh && di < 0.0)
                            || (oval <= epid.drvl && di > 0.0)
                        {
                            i += di;
                            if i < epid.drvl {
                                i = epid.drvl;
                            }
                            if i > epid.drvh {
                                i = epid.drvh;
                            }
                        }
                    }
                }
                // If KI is zero, zero the integral term
                if ki == 0.0 {
                    i = 0.0;
                }
                // Derivative term
                d = if dt > 0.0 { kp * kd * (de / dt) } else { 0.0 };
                oval = p + i + d;
            }
            1 => {
                // MaxMin mode
                if epid.fbon != 0 {
                    if epid.fbop == 0 {
                        // Feedback just transitioned OFF -> ON.
                        // Set output to current value (bumpless).
                        oval = epid.oval;
                    } else {
                        e = cval - pcval;
                        let sign = if d > 0.0 { 1.0 } else { -1.0 };
                        let sign = if (kp > 0.0 && e < 0.0) || (kp < 0.0 && e > 0.0) {
                            -sign
                        } else {
                            sign
                        };
                        d = kp * sign;
                        oval = epid.oval + d;
                    }
                }
            }
            _ => {
                tracing::warn!("Invalid feedback mode {} in epid record", epid.fmod);
            }
        }

        // Clamp output to drive limits
        if oval > epid.drvh {
            oval = epid.drvh;
        }
        if oval < epid.drvl {
            oval = epid.drvl;
        }

        // Update record fields — C `devEpidSoft.c:206-209`.
        epid.ct = ct;
        epid.dt = dt;
        // C `devEpidSoft.c:208` writes ERR unconditionally for every mode.
        epid.err = e;
        epid.cval = cval;

        // Apply output deadband
        if epid.odel == 0.0 || (epid.oval - oval).abs() > epid.odel {
            epid.oval = oval;
        }

        epid.p = p;
        epid.i = i;
        epid.d = d;
        epid.fbop = epid.fbon;
    }
}

impl DeviceSupport for EpidSoftDeviceSupport {
    fn dtyp(&self) -> &str {
        "Epid Soft"
    }

    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        let epid = record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<EpidRecord>())
            .expect("EpidSoftDeviceSupport requires an EpidRecord");

        Self::do_pid(epid);
        Ok(DeviceReadOutcome::computed())
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }
}
