use std::sync::{Arc, Mutex};
use std::time::Instant;

use epics_base_rs::error::CaResult;

use super::scaler_asyn::ScalerDriver;
use crate::records::scaler::MAX_SCALER_CHANNELS;

/// Where a soft scaler's counters come from: C `drvScalerSoft`'s input PVs
/// ([`super::soft_port::PvCounts`]) or an in-process array standing in for
/// them ([`SharedCounts`]).
pub trait CountSource: Send + Sync + 'static {
    /// C `dbGetField` of every input (`drvScalerSoft.c:421-423`).
    fn sample(&mut self, counts: &mut [u32; MAX_SCALER_CHANNELS]) -> CaResult<()>;
    /// C `dbPutField` of `0` to every input (`drvScalerSoft.c:319-322`).
    fn clear(&mut self) -> CaResult<()>;
    /// What `reset` does to the source; C touches no input (`:303-313`).
    fn reset(&mut self) {}
}

/// An in-process count source, written directly by tests.
pub struct SharedCounts(Arc<Mutex<[u32; MAX_SCALER_CHANNELS]>>);

impl CountSource for SharedCounts {
    fn sample(&mut self, counts: &mut [u32; MAX_SCALER_CHANNELS]) -> CaResult<()> {
        *counts = *self.0.lock().unwrap();
        Ok(())
    }

    fn clear(&mut self) -> CaResult<()> {
        *self.0.lock().unwrap() = [0; MAX_SCALER_CHANNELS];
        Ok(())
    }

    /// Unlike C, zeroes the array too, so a fresh test run starts clean.
    fn reset(&mut self) {
        *self.0.lock().unwrap() = [0; MAX_SCALER_CHANNELS];
    }
}

/// Software scaler driver — C `drvScalerSoft.c`, over a [`CountSource`].
pub struct SoftScalerDriver<S: CountSource = SharedCounts> {
    num_channels: usize,
    counts: [u32; MAX_SCALER_CHANNELS],
    presets: [u32; MAX_SCALER_CHANNELS],
    armed: bool,
    done_flag: bool,
    start_time: Option<Instant>,
    source: S,
}

impl SoftScalerDriver {
    pub fn new(num_channels: usize) -> Self {
        Self::with_source(
            num_channels,
            SharedCounts(Arc::new(Mutex::new([0; MAX_SCALER_CHANNELS]))),
        )
    }

    /// Get a shared handle to the counter values for external updating.
    pub fn shared_counts(&self) -> Arc<Mutex<[u32; MAX_SCALER_CHANNELS]>> {
        Arc::clone(&self.source.0)
    }
}

impl<S: CountSource> SoftScalerDriver<S> {
    pub fn with_source(num_channels: usize, source: S) -> Self {
        Self {
            num_channels: num_channels.min(MAX_SCALER_CHANNELS),
            counts: [0; MAX_SCALER_CHANNELS],
            presets: [0; MAX_SCALER_CHANNELS],
            armed: false,
            done_flag: false,
            start_time: None,
            source,
        }
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

impl<S: CountSource> SoftScalerDriver<S> {
    /// C `int32ArrayRead` (`drvScalerSoft.c:409-424`): `checkAcquireDone`
    /// first, on the counts the previous read left, then the inputs. A count
    /// that reaches its preset is therefore seen done one read after the read
    /// that fetched it.
    fn sample(&mut self) -> CaResult<()> {
        if self.armed && self.check_presets() {
            self.done_flag = true;
            self.armed = false;
        }
        self.source.sample(&mut self.counts)
    }
}

impl<S: CountSource> ScalerDriver for SoftScalerDriver<S> {
    /// C `drvScalerSoft.c:303-313` — `scalerResetCommand`: stop
    /// acquiring (`acquiring = prevAcquiring = 0`) and **clear all
    /// presets** (`presetCounts[i] = 0`). What happens to the counters
    /// is the source's ([`CountSource::reset`]).
    fn reset(&mut self) -> CaResult<()> {
        self.counts = [0; MAX_SCALER_CHANNELS];
        self.presets = [0; MAX_SCALER_CHANNELS];
        self.done_flag = false;
        self.armed = false;
        self.start_time = None;
        self.source.reset();
        Ok(())
    }

    fn read(&mut self, counts: &mut [u32; MAX_SCALER_CHANNELS]) -> CaResult<()> {
        self.sample()?;
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

    /// C `drvScalerSoft.c:315-329` — `scalerArmCommand`: the driver
    /// **clears the scaler data** (`counts[i] = 0` plus a `0` write to
    /// each input PV) for EVERY arm command — both arm (`value != 0`)
    /// and disarm (`value == 0`) — before setting `acquiring = value`.
    /// On arm the clear stops a stale count from making the scaler
    /// report "done" immediately; on disarm it zeroes the counts a
    /// subsequent idle read returns, so a disarmed scaler reads `0`
    /// rather than its last value. An unconnected source refuses, and C
    /// then leaves `acquiring` alone (`:318`).
    ///
    /// Note: a normal preset-reached completion disarms inside `read()`
    /// (`self.armed = false`, no count clear), NOT via an arm command —
    /// matching C, where the done path sets `ss = IDLE` and the record's
    /// stop branch skips `arm(0)`. So the final counts survive a normal
    /// completion; only an explicit disarm (manual stop / pre-restart)
    /// clears them.
    fn arm(&mut self, start: bool) -> CaResult<()> {
        // C:319-322 — clear scaler data on arm AND disarm.
        self.source.clear()?;
        self.counts = [0; MAX_SCALER_CHANNELS];
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
    ///
    /// C reaches `scaler_done()` only after a read's `checkAcquireDone`
    /// raised the done interrupt (`acquireDoneCallbacks`); the poll is that
    /// read. It judges the counts the previous read fetched, so the count
    /// completes on the refresh after the one that displayed it, as C's
    /// interrupt-driven process does.
    fn done(&mut self) -> bool {
        // An unreadable source leaves the done latch as the last read left it.
        let _ = self.sample();
        let was_done = self.done_flag;
        self.done_flag = false;
        was_done
    }

    fn num_channels(&self) -> usize {
        self.num_channels
    }
}
