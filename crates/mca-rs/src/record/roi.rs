//! `sum_ROIs` — C `mcaRecord.c:1135-1180` and the `PROCESS_ROI` macro it
//! expands (`:358-412`).
//!
//! C writes this once per element type by macro, over a `DATA_TYPE *` cast of
//! the spectrum buffer, dispatched by a `switch (pmca->ftvl)`. Here the
//! arithmetic is written once, in doubles, for the element types that switch
//! sums.

use epics_base_rs::server::record::Ftype;

use super::{McaRecord, NUM_ROI, Roi};

/// What one `sum_ROIs` pass produced.
pub(crate) struct RoiPass {
    /// An armed region reached its preset count — C's `*preset_reached`, which
    /// makes `process` stop acquisition (`mcaRecord.c:797-800`).
    pub preset_reached: bool,
}

impl McaRecord {
    /// Channel `i` of the spectrum as a double.
    pub(crate) fn channel(&self, i: usize) -> f64 {
        use epics_base_rs::types::EpicsValue as V;
        fn at<T: Copy + Into<f64>>(v: &[T], i: usize) -> f64 {
            v.get(i).copied().map(Into::into).unwrap_or(0.0)
        }
        match &self.val {
            V::CharArray(v) => at(v, i),
            V::UCharArray(v) => at(v, i),
            V::ShortArray(v) => at(v, i),
            V::UShortArray(v) => at(v, i),
            V::LongArray(v) => at(v, i),
            V::ULongArray(v) => at(v, i),
            V::Int64Array(v) => v.get(i).map(|x| *x as f64).unwrap_or(0.0),
            V::UInt64Array(v) => v.get(i).map(|x| *x as f64).unwrap_or(0.0),
            V::FloatArray(v) => at(v, i),
            V::DoubleArray(v) => at(v, i),
            _ => 0.0,
        }
    }

    /// Channel `i` of the background curve.
    fn background(&self, i: usize) -> f64 {
        use epics_base_rs::types::EpicsValue as V;
        fn at<T: Copy + Into<f64>>(v: &[T], i: usize) -> f64 {
            v.get(i).copied().map(Into::into).unwrap_or(0.0)
        }
        match &self.bg {
            V::CharArray(v) => at(v, i),
            V::UCharArray(v) => at(v, i),
            V::ShortArray(v) => at(v, i),
            V::UShortArray(v) => at(v, i),
            V::LongArray(v) => at(v, i),
            V::ULongArray(v) => at(v, i),
            V::Int64Array(v) => v.get(i).map(|x| *x as f64).unwrap_or(0.0),
            V::UInt64Array(v) => v.get(i).map(|x| *x as f64).unwrap_or(0.0),
            V::FloatArray(v) => at(v, i),
            V::DoubleArray(v) => at(v, i),
            _ => 0.0,
        }
    }

    /// Write channel `i` of the background curve, truncating to the element type
    /// the way C's `*pb = bg_lo + ...` assignment through a `DATA_TYPE *` does —
    /// a background curve under an integer spectrum is integer-valued in C, and
    /// `net` is then computed from the TRUNCATED value (`mcaRecord.c:389-394`
    /// reads `*pb` back). Rounding here instead would move every net count.
    fn set_background(&mut self, i: usize, x: f64) {
        use epics_base_rs::types::EpicsValue as V;
        macro_rules! put {
            ($v:expr, $t:ty) => {
                if let Some(slot) = $v.make_mut().get_mut(i) {
                    *slot = x as $t;
                }
            };
        }
        match &mut self.bg {
            V::CharArray(v) => put!(v, u8),
            V::UCharArray(v) => put!(v, u8),
            V::ShortArray(v) => put!(v, i16),
            V::UShortArray(v) => put!(v, u16),
            V::LongArray(v) => put!(v, i32),
            V::ULongArray(v) => put!(v, u32),
            V::Int64Array(v) => put!(v, i64),
            V::UInt64Array(v) => put!(v, u64),
            V::FloatArray(v) => put!(v, f32),
            V::DoubleArray(v) => put!(v, f64),
            _ => {}
        }
    }

    /// The mean of the spectrum over the inclusive channel window
    /// `[centre-n, centre+n]`, clipped to `[0, max]`.
    ///
    /// The divisor is the NOMINAL window width `2n+1`, not the number of
    /// channels the clip left — C `mcaRecord.c:376-384` sums a clipped window and
    /// divides by `2*n + 1` regardless, so a region at the very edge of the
    /// spectrum gets a background that is biased low. That is NOT corrected here:
    /// `R{i}N` is a published physics quantity (net counts under a peak), and
    /// silently changing the number an existing beamline reads back is a worse
    /// failure than the bias. It is a deliberate, documented C-parity choice, not
    /// an oversight.
    fn background_mean(&self, centre: i32, n: i32, max: i32) -> f64 {
        let lo = (centre - n).max(0);
        let hi = (centre + n).min(max);
        let mut acc = 0.0;
        for c in lo..=hi {
            acc += self.channel(c as usize);
        }
        acc / (2 * n + 1) as f64
    }

    /// C's `*p - *pb` (`:393`), taken in the element type: ULONG wraps
    /// unsigned, LONG wraps as x86 int and FLOAT rounds to float. SHORT and
    /// USHORT promote to int, where the f64 difference is exact.
    fn net_term(&self, c: usize) -> f64 {
        let (y, b) = (self.channel(c), self.background(c));
        match self.ftvl {
            Ftype::Long => f64::from((y as i32).wrapping_sub(b as i32)),
            Ftype::ULong => f64::from((y as u32).wrapping_sub(b as u32)),
            Ftype::Float => f64::from(y as f32 - b as f32),
            _ => y - b,
        }
    }

    /// C `sum_ROIs` (`mcaRecord.c:1135-1180`).
    ///
    /// Recomputes EVERY region — C's `newr` bitmap gates whether `sum_ROIs` runs
    /// at all (`:793`), not which region it visits; the macro loops over all 32
    /// and clears each one's bit (`:401`). So the port's caller gates on
    /// `newr != 0` and this clears `newr` whole.
    pub(crate) fn sum_rois(&mut self) -> RoiPass {
        debug_print!(5, "sum_ROIs: entry\n");
        let max = self.nord - 1;
        // C memsets `pbg` in place (`:1143`).
        let cap = self.capacity();
        super::zero_channels(&mut self.bg, cap);
        let mut pass = RoiPass {
            preset_reached: false,
        };
        // C's switch sums these six; CHAR, UCHAR, STRING and the 64-bit types
        // take its `default: break` (`:1147-1151`), which leaves the sums,
        // their marks and NEWR as they were.
        if !matches!(
            self.ftvl,
            Ftype::Short
                | Ftype::UShort
                | Ftype::Long
                | Ftype::ULong
                | Ftype::Float
                | Ftype::Double
        ) {
            return pass;
        }

        // The height of the region end markers on the background curve
        // (`:404-411`). C's `for (i=0; i<max; i++)` with `max = nord-1`
        // (`:361`) never looks at the last channel read.
        let mut ymax: f64 = 0.0;
        for i in 0..max.max(0) {
            ymax = super::cycle::c_max(ymax, self.channel(i as usize));
        }

        for i in 0..NUM_ROI {
            let Roi { lo, hi, nbg, .. } = self.roi[i];
            let hi = hi.min(max);
            let mut sum = 0.0;
            let mut net = 0.0;

            if lo >= 0 && hi >= lo {
                // C marks BG for every summed region, background or not (`:395`).
                self.mmap |= super::cycle::M_BG;
                let (bg_lo, bg_hi) = if nbg >= 0 {
                    let n = nbg as i32;
                    (
                        self.background_mean(lo, n, max),
                        self.background_mean(hi, n, max),
                    )
                } else {
                    (0.0, 0.0)
                };
                let n = hi - lo;
                for j in 0..=n {
                    let c = (lo + j) as usize;
                    let y = self.channel(c);
                    sum += y;
                    if nbg >= 0 {
                        let interpolated = if n != 0 {
                            bg_lo + j as f64 * (bg_hi - bg_lo) / n as f64
                        } else {
                            bg_lo
                        };
                        self.set_background(c, interpolated);
                    }
                    // Read the background back: under an integer spectrum the
                    // store above truncated it, and C's `net += *p - *pb` reads
                    // the truncated value.
                    net += self.net_term(c);
                }
            }

            let roi = &mut self.roi[i];
            // C `ROI_MARK(M_R0<<i)` (`:397`): `monitor()` posts both sums.
            if sum != roi.sum || net != roi.net {
                self.rmap |= 1 << i;
            }
            roi.sum = sum;
            roi.net = net;
            if roi.is_preset != 0 && roi.net >= roi.preset {
                pass.preset_reached = true;
            }
        }

        // The end markers, planted after every region's background is in place
        // (C's second loop, `:403-412`).
        for i in 0..NUM_ROI {
            let Roi { lo, hi, .. } = self.roi[i];
            let hi = hi.min(max);
            if lo >= 0 && hi >= lo {
                self.set_background(lo as usize, ymax);
                self.set_background(hi as usize, ymax);
            }
        }

        self.newr = 0;
        pass
    }
}
