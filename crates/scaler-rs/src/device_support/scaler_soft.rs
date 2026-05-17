use std::sync::{Arc, Mutex};
use std::time::Instant;

use epics_base_rs::error::CaResult;

use super::scaler_asyn::ScalerDriver;
use crate::records::scaler::MAX_SCALER_CHANNELS;

/// Software scaler driver for testing and simulation.
///
/// Ported from `drvScalerSoft.c`. Implements `ScalerDriver` without
/// requiring real hardware. Counter values can be set externally
/// (e.g., from PV links or test code).
///
/// In the C implementation, this reads from template-based PV names.
/// In Rust, the counters are directly accessible for external updating.
pub struct SoftScalerDriver {
    num_channels: usize,
    counts: [u32; MAX_SCALER_CHANNELS],
    presets: [u32; MAX_SCALER_CHANNELS],
    armed: bool,
    done_flag: bool,
    start_time: Option<Instant>,
    /// Shared reference for external counter updates.
    shared_counts: Arc<Mutex<[u32; MAX_SCALER_CHANNELS]>>,
}

impl SoftScalerDriver {
    pub fn new(num_channels: usize) -> Self {
        let num_channels = num_channels.min(MAX_SCALER_CHANNELS);
        Self {
            num_channels,
            counts: [0; MAX_SCALER_CHANNELS],
            presets: [0; MAX_SCALER_CHANNELS],
            armed: false,
            done_flag: false,
            start_time: None,
            shared_counts: Arc::new(Mutex::new([0; MAX_SCALER_CHANNELS])),
        }
    }

    /// Get a shared handle to the counter values for external updating.
    pub fn shared_counts(&self) -> Arc<Mutex<[u32; MAX_SCALER_CHANNELS]>> {
        Arc::clone(&self.shared_counts)
    }

    /// Port of C `checkAcquireDone()` (drvScalerSoft.c:588-600): the
    /// scaler is done when, for any channel with a non-zero preset, the
    /// count has reached the preset. The C soft driver has no separate
    /// "gate" concept — a channel participates in done-detection iff its
    /// `presetCounts[i] > 0`.
    fn check_presets(&self) -> bool {
        for i in 0..self.num_channels {
            if self.presets[i] > 0 && self.counts[i] >= self.presets[i] {
                return true;
            }
        }
        false
    }
}

impl ScalerDriver for SoftScalerDriver {
    /// C `drvScalerSoft.c:303-313` — `scalerResetCommand`: stop
    /// acquiring (`acquiring = prevAcquiring = 0`) and **clear all
    /// presets** (`presetCounts[i] = 0`). The original C does not zero
    /// `counts` here — but since the soft driver's counts are sourced
    /// from external PVs, the port keeps the previous behaviour of also
    /// zeroing its local/shared count buffers so a fresh test run starts
    /// clean. The load-bearing C-parity fix is clearing `presets`.
    fn reset(&mut self) -> CaResult<()> {
        self.counts = [0; MAX_SCALER_CHANNELS];
        self.presets = [0; MAX_SCALER_CHANNELS];
        self.done_flag = false;
        self.armed = false;
        self.start_time = None;
        let mut shared = self.shared_counts.lock().unwrap();
        *shared = [0; MAX_SCALER_CHANNELS];
        Ok(())
    }

    fn read(&mut self, counts: &mut [u32; MAX_SCALER_CHANNELS]) -> CaResult<()> {
        // Copy from shared state (externally updated)
        let shared = self.shared_counts.lock().unwrap();
        self.counts = *shared;

        // Check if any preset reached
        if self.armed && self.check_presets() {
            self.done_flag = true;
            self.armed = false;
        }

        *counts = self.counts;
        Ok(())
    }

    /// C `drvScalerSoft.c:331-336` — `scalerPresetCommand`: store the
    /// preset for the channel. C does not derive any gate from it.
    ///
    /// The soft driver has a fixed software clock and never quantizes
    /// the preset, so it returns `preset` unchanged. Its
    /// `actual_frequency` is left as the trait default (`None`).
    fn write_preset(&mut self, channel: usize, preset: u32) -> CaResult<u32> {
        if channel < MAX_SCALER_CHANNELS {
            self.presets[channel] = preset;
        }
        Ok(preset)
    }

    /// C `drvScalerSoft.c:315-329` — `scalerArmCommand`: on arm, the
    /// driver first **clears the scaler data** (`counts[i] = 0` and the
    /// PV that backs each channel) so a stale count cannot make the
    /// scaler report "done" immediately; then it sets `acquiring`.
    fn arm(&mut self, start: bool) -> CaResult<()> {
        if start {
            // C:319-322 — clear scaler data to avoid an immediate done.
            self.counts = [0; MAX_SCALER_CHANNELS];
            let mut shared = self.shared_counts.lock().unwrap();
            *shared = [0; MAX_SCALER_CHANNELS];
        }
        self.armed = start;
        if start {
            self.done_flag = false;
            self.start_time = Some(Instant::now());
        } else {
            self.start_time = None;
        }
        Ok(())
    }

    /// C `devScalerAsyn.c:292-301` `scaler_done()` — read-and-clear: a
    /// completed count is reported exactly once, then the flag is
    /// cleared so the next poll returns `false`.
    fn done(&mut self) -> bool {
        let was_done = self.done_flag;
        self.done_flag = false;
        was_done
    }

    fn num_channels(&self) -> usize {
        self.num_channels
    }
}
