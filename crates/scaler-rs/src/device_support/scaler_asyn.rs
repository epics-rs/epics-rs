use std::sync::{Arc, Mutex};

use epics_base_rs::error::CaResult;
use epics_base_rs::server::device_support::{DeviceReadOutcome, DeviceSupport};
use epics_base_rs::server::record::Record;
use epics_base_rs::types::EpicsValue;

use crate::records::scaler::{
    CMD_ARM, CMD_AUTOCOUNT, CMD_RESET, CMD_START_COUNT, CMD_WRITE_PRESET, MAX_SCALER_CHANNELS,
    ScalerRecord,
};

/// Asyn command strings for scaler drivers.
pub const SCALER_RESET_COMMAND: &str = "SCALER_RESET";
pub const SCALER_CHANNELS_COMMAND: &str = "SCALER_CHANNELS";
pub const SCALER_READ_COMMAND: &str = "SCALER_READ";
pub const SCALER_READ_SINGLE_COMMAND: &str = "SCALER_READ_SINGLE";
pub const SCALER_PRESET_COMMAND: &str = "SCALER_PRESET";
pub const SCALER_ARM_COMMAND: &str = "SCALER_ARM";
pub const SCALER_DONE_COMMAND: &str = "SCALER_DONE";

/// Trait for scaler hardware drivers.
pub trait ScalerDriver: Send + Sync + 'static {
    fn reset(&mut self) -> CaResult<()>;
    fn read(&mut self, counts: &mut [u32; MAX_SCALER_CHANNELS]) -> CaResult<()>;
    /// Program `preset` for `channel` and return the count the hardware
    /// will actually use.
    ///
    /// C parity: `scalerRecord.c:397-404` documents that some scalers
    /// (the Joerger VS64) set their own clock frequency and adjust the
    /// preset to match — "if device support has changed the preset, we
    /// recalc the preset from tp ... and call write_preset again". In C
    /// the dset `write_preset(scalerRecord *psr, ...)` mutates
    /// `psr->pr1` in place; the record then reads it back. The Rust
    /// trait makes that explicit by returning the programmed value.
    /// A driver that does not quantize simply returns `preset`
    /// unchanged (see `SoftScalerDriver`).
    fn write_preset(&mut self, channel: usize, preset: u32) -> CaResult<u32>;
    fn arm(&mut self, start: bool) -> CaResult<()>;
    /// The clock frequency (Hz) the driver is actually running at.
    ///
    /// C parity: `scalerRecord.c:399-403` — the VS64 "sets its own
    /// clock frequency"; `updateCounts` (`scalerRecord.c:585-587`) and
    /// the REQSTART reconciliation (`scalerRecord.c:424-430`) use the
    /// frequency the driver chose, not the one originally requested.
    /// A driver that does not own the clock returns `None`, leaving
    /// `FREQ` untouched.
    fn actual_frequency(&self) -> Option<f64> {
        None
    }
    /// Read-and-clear the "counting done" flag.
    ///
    /// C `devScalerAsyn.c:292-301` `scaler_done()` is the dset entry the
    /// record polls each process cycle; it returns 1 exactly once per
    /// completed count and clears `pPvt->done` on that read so the next
    /// poll returns 0. This signature takes `&mut self` so the flag can
    /// be consumed — a `&self` version cannot replicate the clear.
    fn done(&mut self) -> bool;
    fn num_channels(&self) -> usize;
}

/// Asyn-based device support for the scaler record.
///
/// `read()` performs check_done + read_counts (pre-process data).
/// `handle_command()` executes reset/write_preset/arm (post-process actions).
pub struct ScalerAsynDeviceSupport {
    driver: Arc<Mutex<Box<dyn ScalerDriver>>>,
}

impl ScalerAsynDeviceSupport {
    pub fn new(driver: Box<dyn ScalerDriver>) -> Self {
        Self {
            driver: Arc::new(Mutex::new(driver)),
        }
    }

    pub fn driver(&self) -> Arc<Mutex<Box<dyn ScalerDriver>>> {
        Arc::clone(&self.driver)
    }
}

impl DeviceSupport for ScalerAsynDeviceSupport {
    fn dtyp(&self) -> &str {
        "Asyn Scaler"
    }

    fn init(&mut self, record: &mut dyn Record) -> CaResult<()> {
        let scaler = record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<ScalerRecord>())
            .expect("ScalerAsynDeviceSupport requires a ScalerRecord");

        let driver = self.driver.lock().unwrap();
        // A custom `ScalerDriver` may report any channel count. Clamp to the
        // physical array bound so the record's fixed 64-element arrays are
        // never indexed out of range.
        scaler.nch = driver.num_channels().min(MAX_SCALER_CHANNELS) as i16;
        Ok(())
    }

    fn read(&mut self, record: &mut dyn Record) -> CaResult<DeviceReadOutcome> {
        let scaler = record
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<ScalerRecord>())
            .expect("ScalerAsynDeviceSupport requires a ScalerRecord");

        let mut driver = self.driver.lock().unwrap();

        // Check if counting completed. C devScalerAsyn.c:292-301
        // `scaler_done()` is the dset entry the record polls every
        // process cycle (scalerRecord.c:367); it is read-and-clear, so
        // it reports a completed count exactly once. Here device
        // support marks the record done before process() runs.
        if driver.done() {
            scaler.set_done();
        }

        // Read all channel counts into the record
        let mut counts = [0u32; MAX_SCALER_CHANNELS];
        if driver.read(&mut counts).is_ok() {
            scaler.s = counts;
        }

        Ok(DeviceReadOutcome::ok())
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }

    fn handle_command(
        &mut self,
        record: &mut dyn Record,
        command: &str,
        args: &[EpicsValue],
    ) -> CaResult<()> {
        let mut driver = self.driver.lock().unwrap();
        match command {
            CMD_RESET => {
                driver.reset()?;
            }
            CMD_ARM => {
                let start = args
                    .first()
                    .and_then(|v| {
                        if let EpicsValue::Long(i) = v {
                            Some(*i != 0)
                        } else {
                            None
                        }
                    })
                    .unwrap_or(false);
                driver.arm(start)?;
            }
            CMD_WRITE_PRESET => {
                let channel = args
                    .first()
                    .and_then(|v| {
                        if let EpicsValue::Long(i) = v {
                            Some(*i as usize)
                        } else {
                            None
                        }
                    })
                    .unwrap_or(0);
                let preset = args
                    .get(1)
                    .and_then(|v| {
                        if let EpicsValue::Long(i) = v {
                            Some(*i as u32)
                        } else {
                            None
                        }
                    })
                    .unwrap_or(0);
                // Standalone single-channel preset write. The returned
                // (possibly driver-adjusted) value is reconciled into
                // the record only by the count-start paths below; a
                // bare preset write has no PR1/TP/FREQ reconciliation
                // in C either (no caller exists in scalerRecord.c).
                driver.write_preset(channel, preset)?;
            }
            CMD_START_COUNT => {
                let scaler = record
                    .as_any_mut()
                    .and_then(|a| a.downcast_mut::<ScalerRecord>())
                    .expect("ScalerAsynDeviceSupport requires a ScalerRecord");
                Self::run_start_count(&mut **driver, scaler)?;
            }
            CMD_AUTOCOUNT => {
                let scaler = record
                    .as_any_mut()
                    .and_then(|a| a.downcast_mut::<ScalerRecord>())
                    .expect("ScalerAsynDeviceSupport requires a ScalerRecord");
                Self::run_autocount(&mut **driver, scaler)?;
            }
            _ => {}
        }
        Ok(())
    }
}

impl ScalerAsynDeviceSupport {
    /// REQSTART count-start driver sequence — C `scalerRecord.c:408-432`.
    ///
    /// `process()` has already run the `pr[0] = NINT(tp*freq)` guard
    /// (`scalerRecord.c:409-411`) and emitted `CMD_RESET` before this.
    /// Here we reproduce the rest:
    ///
    /// - `:412` capture `save_pr1`; `:406` capture `old_pr1`.
    /// - `:413-419` write every gated channel's preset. Channel 0's
    ///   write returns the count the driver actually programmed; that
    ///   replaces `pr[0]` — the C `write_preset` mutates `psr->pr1`.
    /// - Adopt a driver-reported clock (`actual_frequency`) into
    ///   `freq` so the `:421` recompute uses it (`:399-403`).
    /// - `:420-423` `count_start_rewrite_preset` re-writes preset 0
    ///   when the driver adjusted it; the second `write_preset` may
    ///   adjust it again, and the final value lands in `pr[0]`.
    /// - `:424-428` `count_start_finalize_tp` recomputes `tp` from the
    ///   final `pr[0]`/`freq`.
    /// - `:431` arm.
    fn run_start_count(driver: &mut dyn ScalerDriver, scaler: &mut ScalerRecord) -> CaResult<()> {
        // C scalerRecord.c:406 — old_pr1 captured before any write.
        let old_pr1 = scaler.pr[0];

        // C scalerRecord.c:412 — save_pr1 captured before the loop.
        let save_pr1 = scaler.pr[0];

        let nch = scaler.active_channels();
        for i in 0..nch {
            if scaler.g[i] != 0 {
                let programmed = driver.write_preset(i, scaler.pr[i])?;
                if i == 0 {
                    // C scalerRecord.c:413-419 — write_preset(psr, 0, ...)
                    // mutates psr->pr1; mirror that with the returned value.
                    scaler.pr[0] = programmed;
                }
            }
        }

        // C scalerRecord.c:399-403 — a driver that owns its clock may
        // have changed the frequency; adopt it before reconciling so
        // the :421 NINT(tp*freq) recompute uses the real clock.
        if let Some(freq) = driver.actual_frequency() {
            scaler.freq = freq;
        }

        // C scalerRecord.c:420-423 — re-write preset 0 if the driver
        // adjusted it. The second write_preset may adjust again; the
        // final driver-programmed value is what lands in pr[0].
        if let Some(rewrite) = scaler.count_start_rewrite_preset(save_pr1) {
            let programmed = driver.write_preset(0, rewrite)?;
            scaler.pr[0] = programmed;
        }

        // C scalerRecord.c:424-428 — recompute tp from the final pr1.
        scaler.count_start_finalize_tp(old_pr1);

        // C scalerRecord.c:431 — arm.
        driver.arm(true)?;
        Ok(())
    }

    /// Auto-count driver sequence — C `scalerRecord.c:508-535`.
    ///
    /// `CMD_RESET` was already emitted. Differences from REQSTART:
    /// when `tp1 >= 1ms` only channel 0 is programmed from `tp1*freq`
    /// (truncating, `:514`); the `save_pr1 != pr1` re-write
    /// (`:515-522`) recomputes from `tp1*freq` again; afterward the
    /// user's `PR1` is **restored** (`:532`) and `TP` is left alone.
    fn run_autocount(driver: &mut dyn ScalerDriver, scaler: &mut ScalerRecord) -> CaResult<()> {
        let old_pr1 = scaler.pr[0];

        if scaler.tp1 >= 1.0e-3 {
            // C scalerRecord.c:513-514 — truncating cast, not NINT.
            let save_pr1 = scaler.pr[0];
            let auto_pr1 = ScalerRecord::pr1_trunc(scaler.tp1, scaler.freq);
            let programmed = driver.write_preset(0, auto_pr1)?;
            scaler.pr[0] = programmed;
            // C scalerRecord.c:515-522 — driver adjusted the preset
            // (typically because it picked a different clock); recalc
            // from tp1 with the driver's effective freq and re-write.
            if save_pr1 != scaler.pr[0] {
                if let Some(freq) = driver.actual_frequency() {
                    scaler.freq = freq;
                }
                let recalc = ScalerRecord::pr1_trunc(scaler.tp1, scaler.freq);
                let programmed = driver.write_preset(0, recalc)?;
                scaler.pr[0] = programmed;
            }
        } else {
            // C scalerRecord.c:524-528 — write every gated channel's
            // user preset.
            let nch = scaler.active_channels();
            for i in 0..nch {
                if scaler.g[i] != 0 {
                    driver.write_preset(i, scaler.pr[i])?;
                }
            }
        }

        // C scalerRecord.c:530 — adopt a driver-changed clock so FREQ
        // reflects reality. C compares `old_freq != freq` only to gate
        // `db_post_events`; the monitor layer owns that posting, so the
        // port adopts the driver clock and lets the framework post.
        if let Some(freq) = driver.actual_frequency() {
            scaler.freq = freq;
        }

        // C scalerRecord.c:532 — "Don't let autocount disturb user's
        // channel-1 preset": restore PR1. TP is intentionally NOT
        // recomputed in the auto-count path.
        scaler.pr[0] = old_pr1;

        // C scalerRecord.c:533 — arm.
        driver.arm(true)?;
        Ok(())
    }
}
