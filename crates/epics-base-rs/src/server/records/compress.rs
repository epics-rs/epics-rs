use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, ProcessOutcome, Record};
use crate::types::{DbFieldType, EpicsValue};

/// Compress record — circular buffer with compression algorithms.
///
/// `alg` follows C `menuCompressALG` (compressRecord.dbd.pod):
///   0 = N to 1 Low Value
///   1 = N to 1 High Value
///   2 = N to 1 Average
///   3 = Average (rolling) — not yet implemented; falls through to 0.0
///   4 = Circular Buffer
///   5 = N to 1 Median — not yet implemented; falls through to 0.0
///
/// The numeric values are CA-wire-visible (DBR_SHORT) and must match
/// `menuCompressALG` so a C client setting `ALG=4` reaches the
/// Circular-Buffer code path.
pub struct CompressRecord {
    pub val: Vec<f64>,
    pub nsam: i32,   // Number of samples (buffer size)
    pub inp: String, // input link
    pub alg: i16,    // See top-of-struct doc — matches menuCompressALG
    pub n: i32,      // Number of values to compress
    pub nuse: i32,   // Number of elements used
    pub off: i32,    // Current write offset (see `put_one` for BALG-dependent role)
    pub res: i16,    // Reset flag
    pub balg: i16,   // 0=FIFO, 1=LIFO — `menuBufferingALG`
    /// `ILIL` (input low limit). When `ILIL < IHIL`, samples outside
    /// `[ILIL, IHIL]` are dropped before compression (C
    /// `compress_array` skip loop, compressRecord.c:163-170).
    pub ilil: f64,
    /// `IHIL` (input high limit). See [`Self::ilil`].
    pub ihil: f64,
    /// `INX` cycle counter for alg=Average. C `prec->inx` —
    /// increments on every accumulator update, resets to 0 after the
    /// N-th waveform when the average is emitted.
    pub inx: i32,
    /// `PBUF` (partial buffer) — epics-base 7.0.8.
    /// `0 = NO` (default): VAL is read by clients as the whole NSAM
    /// vector; the leading `NUSE` elements are valid, the rest are
    /// zeros from initial allocation. The record acts "undefined"
    /// until the buffer fills.
    /// `1 = YES`: VAL truncates to the first `NUSE` elements while
    /// the buffer is still filling, so a downstream consumer sees
    /// a growing array of only valid data instead of trailing zeros.
    /// Both modes update internal state identically; the difference
    /// is purely in what `get_field("VAL")` returns.
    pub pbuf: i16,
    // Internal accumulator for N-to-1 algorithms
    accum: Vec<f64>,
}

impl Default for CompressRecord {
    fn default() -> Self {
        Self {
            val: vec![0.0; 10],
            nsam: 10,
            inp: String::new(),
            alg: 4, // Circular Buffer by default (menuCompressALG_Circular_Buffer)
            n: 1,
            nuse: 0,
            off: 0,
            res: 0,
            balg: 0,
            pbuf: 0,
            ilil: 0.0,
            ihil: 0.0,
            inx: 0,
            accum: Vec::new(),
        }
    }
}

impl CompressRecord {
    pub fn new(nsam: i32, alg: i16) -> Self {
        Self {
            val: vec![0.0; nsam as usize],
            nsam,
            alg,
            ..Default::default()
        }
    }

    /// True when ILIL < IHIL and `input` falls outside the limits.
    /// C `compress_array` (compressRecord.c:163-170) drops these
    /// samples before compression so the configured algorithm
    /// (min/max/mean/median) never sees spurious outliers.
    #[inline]
    fn ilil_ihil_rejects(&self, input: f64) -> bool {
        self.ilil < self.ihil && (input < self.ilil || input > self.ihil)
    }

    /// Write one value into the circular buffer, advancing `off`
    /// and `nuse` per BALG. Mirrors C `put_value` (compressRecord.c).
    fn put_one(&mut self, value: f64) {
        let nsam = self.nsam.max(1) as usize;
        if self.balg == 1 {
            // LIFO: pre-decrement modulo nsam, then write.
            self.off = ((self.off - 1).rem_euclid(nsam as i32)) as i32;
            self.val[self.off as usize] = value;
        } else {
            // FIFO: write at off, post-increment.
            let idx = self.off as usize % nsam;
            self.val[idx] = value;
            self.off = ((self.off as i64 + 1) % nsam as i64) as i32;
        }
        if (self.nuse as usize) < nsam {
            self.nuse += 1;
        }
    }

    /// Push a value into the compress record.
    pub fn push_value(&mut self, input: f64) {
        if self.ilil_ihil_rejects(input) {
            return;
        }
        match self.alg {
            // menuCompressALG_Circular_Buffer
            4 => self.put_one(input),
            // alg=3 (Average rolling) is array-oriented in C
            // (`array_average` operates on a whole waveform). A
            // scalar push degrades to a 1-element array call so the
            // running average behaves predictably for either input
            // shape.
            3 => self.push_array_average(&[input]),
            // N-to-1 algorithms (Low/High/N_to_1_Average/Median):
            // accumulate N samples then compress.
            _ => {
                self.accum.push(input);
                if self.accum.len() >= self.n as usize {
                    self.flush_accum();
                }
            }
        }
    }

    /// C `array_average` (compressRecord.c:223-270) per-cycle entry.
    /// Element-wise sums up to N consecutive waveforms in a
    /// `nsam`-sized accumulator (`sptr` in C, `accum` here), divides
    /// by N after the N-th waveform, emits one `put_one` per
    /// accumulator slot. Caller is expected to be `push_array`
    /// (single-waveform `push_value` degrades to a one-element call).
    fn push_array_average(&mut self, input: &[f64]) {
        let nsam = self.nsam.max(1) as usize;
        // C `nuse = min(nsam, no_elements)`. Effective length of the
        // averaged output for this call.
        let nuse = nsam.min(input.len());
        if nuse == 0 {
            return;
        }
        // Accumulator is the C `sptr` — `nsam` doubles sized.
        // `accum.len() != nsam` triggers a fresh allocation either
        // on first call or when NSAM was retuned mid-life.
        if self.accum.len() != nsam {
            self.accum = vec![0.0; nsam];
        }
        if self.inx == 0 {
            // Start of a new N-cycle: replace contents with the
            // incoming waveform (zero-pad the tail to nuse..nsam).
            for (i, slot) in self.accum.iter_mut().take(nuse).enumerate() {
                *slot = input[i];
            }
            for slot in self.accum.iter_mut().take(nsam).skip(nuse) {
                *slot = 0.0;
            }
        } else {
            for i in 0..nuse {
                self.accum[i] += input[i];
            }
        }
        self.inx += 1;
        let n = self.n.max(1);
        if self.inx < n {
            return;
        }
        // N waveforms accumulated — divide and emit.
        let multiplier = 1.0 / n as f64;
        let mut out = Vec::with_capacity(nuse);
        for slot in self.accum.iter().take(nuse) {
            out.push(slot * multiplier);
        }
        self.inx = 0;
        for v in out {
            self.put_one(v);
        }
    }

    /// epics-base PR #84f4771: array-input form of `push_value`.
    /// Feeds each element of `input` through the configured
    /// algorithm. For N-to-1 algorithms, this is the only path that
    /// can observe the "partial buffer at end of input" case —
    /// `push_value` always returns after a single element, so it
    /// can't tell whether more samples are coming.
    ///
    /// When `PBUF=YES` and the array ends mid-chunk (i.e. the
    /// accumulator has 0<k<N samples after consumption), emit the
    /// compressed value of those k samples immediately instead of
    /// dropping them. PBUF=NO retains the legacy "wait until N
    /// samples available" behaviour — partial accumulation persists
    /// for the next array.
    pub fn push_array(&mut self, input: &[f64]) {
        match self.alg {
            // Circular Buffer (alg=4): every sample independent.
            4 => {
                for &v in input {
                    self.push_value(v);
                }
            }
            // Average (rolling, alg=3): single array_average call.
            3 => {
                // ILIL/IHIL apply per element; reject before averaging.
                if self.ilil < self.ihil {
                    let filtered: Vec<f64> = input
                        .iter()
                        .copied()
                        .filter(|v| !(*v < self.ilil || *v > self.ihil))
                        .collect();
                    self.push_array_average(&filtered);
                } else {
                    self.push_array_average(input);
                }
            }
            // N-to-1 algorithms (Low/High/N_to_1_Average/Median).
            _ => {
                for &v in input {
                    if self.ilil_ihil_rejects(v) {
                        continue;
                    }
                    self.accum.push(v);
                    if self.accum.len() >= self.n as usize {
                        self.flush_accum();
                    }
                }
                if self.pbuf != 0 && !self.accum.is_empty() {
                    self.flush_accum();
                }
            }
        }
    }

    /// Compress `self.accum` via the configured algorithm and push
    /// the result into the circular VAL buffer via [`Self::put_one`]
    /// so BALG (FIFO vs LIFO) is honoured. Clears `accum` regardless
    /// of partial vs full — callers decide *whether* to flush; this
    /// just executes it.
    fn flush_accum(&mut self) {
        if self.accum.is_empty() {
            return;
        }
        let compressed = match self.alg {
            // menuCompressALG_N_to_1_Low_Value
            0 => self.accum.iter().cloned().fold(f64::INFINITY, f64::min),
            // menuCompressALG_N_to_1_High_Value
            1 => self.accum.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
            // menuCompressALG_N_to_1_Average
            2 => self.accum.iter().sum::<f64>() / self.accum.len() as f64,
            // menuCompressALG_N_to_1_Median: middle element after sort.
            // C `compressRecord.c:212` uses `psource[n/2]` after `qsort`
            // — identical for integer-indexed mid pick.
            5 => {
                let mut sorted = self.accum.clone();
                sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                sorted[sorted.len() / 2]
            }
            // alg=3 (Average rolling) doesn't use the N-element
            // accumulator — push_array_average handles it directly.
            _ => 0.0,
        };
        self.accum.clear();
        self.put_one(compressed);
    }

    /// Linearise the circular buffer per BALG: returns NUSE elements
    /// in oldest-to-newest order (FIFO) or newest-to-oldest (LIFO).
    /// Mirrors C `get_array_info` (compressRecord.c:409-431).
    pub(crate) fn linearise_val(&self) -> Vec<f64> {
        let nsam = self.nsam.max(0) as usize;
        let nuse = self.nuse.max(0) as usize;
        if nuse == 0 || nsam == 0 {
            return Vec::new();
        }
        let off = self.off.rem_euclid(nsam as i32) as usize;
        let start = if self.balg == 0 {
            // FIFO: `(off + nsam - nuse) % nsam`.
            (off + nsam - nuse) % nsam
        } else {
            // LIFO: `off` already points at the newest element.
            off
        };
        let mut out = Vec::with_capacity(nuse);
        for i in 0..nuse {
            out.push(self.val[(start + i) % nsam]);
        }
        out
    }
}

static COMPRESS_FIELDS: &[FieldDesc] = &[
    FieldDesc {
        name: "VAL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "NSAM",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "ALG",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "N",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "OFF",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "NUSE",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "RES",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "BALG",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "PBUF",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "ILIL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "IHIL",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "INX",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
];

impl Record for CompressRecord {
    fn record_type(&self) -> &'static str {
        "compress"
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        if self.res != 0 {
            self.off = 0;
            self.nuse = 0;
            for v in &mut self.val {
                *v = 0.0;
            }
            self.res = 0;
        }
        Ok(ProcessOutcome::complete())
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "VAL" => {
                // C `get_array_info` (compressRecord.c:409-431):
                // `*no_elements = nuse` regardless of PBUF, with the
                // circular buffer linearised per BALG. The PBUF field
                // is purely a processing-time control (early-emit for
                // N-to-1 algorithms); it does NOT change what a CA
                // client sees on read.
                Some(EpicsValue::DoubleArray(self.linearise_val()))
            }
            "INP" => Some(EpicsValue::String(self.inp.clone())),
            "NSAM" => Some(EpicsValue::Long(self.nsam)),
            "NUSE" => Some(EpicsValue::Long(self.nuse)),
            "RES" => Some(EpicsValue::Short(self.res)),
            "BALG" => Some(EpicsValue::Short(self.balg)),
            "ALG" => Some(EpicsValue::Short(self.alg)),
            "N" => Some(EpicsValue::Long(self.n)),
            "OFF" => Some(EpicsValue::Long(self.off)),
            "PBUF" => Some(EpicsValue::Short(self.pbuf)),
            "ILIL" => Some(EpicsValue::Double(self.ilil)),
            "IHIL" => Some(EpicsValue::Double(self.ihil)),
            "INX" => Some(EpicsValue::Long(self.inx)),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => match value {
                EpicsValue::DoubleArray(arr) => {
                    self.val = arr;
                    Ok(())
                }
                EpicsValue::Double(v) => {
                    self.push_value(v);
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("VAL".into())),
            },
            "ALG" => match value {
                EpicsValue::Short(v) => {
                    self.alg = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("ALG".into())),
            },
            "N" => match value {
                EpicsValue::Long(v) => {
                    self.n = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("N".into())),
            },
            "RES" => match value {
                EpicsValue::Short(_) => {
                    // epics-base 8ac2c87 (2025): writing any value to
                    // RES triggers SPC_RESET — clear the circular
                    // buffer and acknowledge by zeroing RES itself.
                    // The framework should post a monitor event so
                    // CA clients see the empty array immediately.
                    self.nuse = 0;
                    self.off = 0;
                    self.res = 0;
                    self.accum.clear();
                    let nsam = self.nsam.max(0) as usize;
                    self.val.clear();
                    self.val.resize(nsam, 0.0);
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("RES".into())),
            },
            "BALG" => match value {
                EpicsValue::Short(v) => {
                    self.balg = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("BALG".into())),
            },
            "PBUF" => match value {
                EpicsValue::Short(v) => {
                    self.pbuf = v;
                    Ok(())
                }
                EpicsValue::String(s) => {
                    // epics-base menu field accepts YES/NO strings.
                    self.pbuf = match s.to_ascii_uppercase().as_str() {
                        "YES" => 1,
                        "NO" | "" => 0,
                        _ => return Err(CaError::TypeMismatch(format!("PBUF: {s}"))),
                    };
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("PBUF".into())),
            },
            "ILIL" => match value {
                EpicsValue::Double(v) => {
                    self.ilil = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("ILIL".into())),
            },
            "IHIL" => match value {
                EpicsValue::Double(v) => {
                    self.ihil = v;
                    Ok(())
                }
                _ => Err(CaError::TypeMismatch("IHIL".into())),
            },
            "NSAM" | "OFF" | "NUSE" | "INX" => Err(CaError::ReadOnlyField(name.to_string())),
            _ => Err(CaError::FieldNotFound(name.to_string())),
        }
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        COMPRESS_FIELDS
    }

    fn primary_field(&self) -> &'static str {
        "VAL"
    }
}

#[cfg(test)]
mod pbuf_tests {
    use super::*;

    /// C `get_array_info` (compressRecord.c:409-431) returns
    /// `*no_elements = nuse` regardless of PBUF — only the valid
    /// elements are exposed on the wire. PBUF is a processing-time
    /// option (early N-to-1 emit), not a read-side toggle.
    #[test]
    fn val_read_always_nuse_clamped_regardless_of_pbuf() {
        let mut rec = CompressRecord::new(4, 4); // circular buffer NSAM=4
        rec.push_value(1.0);
        rec.push_value(2.0);
        // nuse=2 < nsam=4: VAL must surface exactly the 2 valid samples.
        match rec.get_field("VAL").unwrap() {
            EpicsValue::DoubleArray(v) => {
                assert_eq!(v, vec![1.0, 2.0]);
            }
            other => panic!("expected DoubleArray, got {other:?}"),
        }
        // Setting PBUF doesn't change what the reader sees — same 2
        // valid samples.
        rec.pbuf = 1;
        match rec.get_field("VAL").unwrap() {
            EpicsValue::DoubleArray(v) => {
                assert_eq!(v, vec![1.0, 2.0]);
            }
            other => panic!("expected DoubleArray, got {other:?}"),
        }
    }

    /// Buffer fill progresses incrementally; VAL grows with NUSE.
    /// When the buffer fills, VAL reaches its final NSAM length.
    #[test]
    fn val_grows_with_nuse_to_full_buffer() {
        let mut rec = CompressRecord::new(4, 4);
        rec.push_value(10.0);
        rec.push_value(20.0);
        match rec.get_field("VAL").unwrap() {
            EpicsValue::DoubleArray(v) => assert_eq!(v, vec![10.0, 20.0]),
            other => panic!("expected DoubleArray, got {other:?}"),
        }
        rec.push_value(30.0);
        rec.push_value(40.0);
        match rec.get_field("VAL").unwrap() {
            EpicsValue::DoubleArray(v) => assert_eq!(v, vec![10.0, 20.0, 30.0, 40.0]),
            other => panic!("expected DoubleArray, got {other:?}"),
        }
    }

    /// `PBUF` is writable via the menu string form (`"YES"`/`"NO"`)
    /// as well as the raw `Short`.
    #[test]
    fn pbuf_accepts_yes_no_menu_string() {
        let mut rec = CompressRecord::default();
        rec.put_field("PBUF", EpicsValue::String("YES".into()))
            .unwrap();
        assert_eq!(rec.pbuf, 1);
        rec.put_field("PBUF", EpicsValue::String("no".into()))
            .unwrap();
        assert_eq!(rec.pbuf, 0);
        // Invalid string → TypeMismatch.
        let err = rec
            .put_field("PBUF", EpicsValue::String("maybe".into()))
            .unwrap_err();
        assert!(matches!(err, CaError::TypeMismatch(_)));
    }

    /// PBUF=YES with an empty buffer returns an empty array, not a
    /// panicked underflow.
    #[test]
    fn pbuf_yes_empty_buffer_returns_empty() {
        let mut rec = CompressRecord::new(4, 4);
        rec.pbuf = 1;
        match rec.get_field("VAL").unwrap() {
            EpicsValue::DoubleArray(v) => assert!(v.is_empty()),
            other => panic!("expected empty DoubleArray, got {other:?}"),
        }
    }

    /// epics-base PR #84f4771: N-to-1 push_array must emit a partial
    /// chunk when PBUF=YES and the input ends mid-chunk. Pre-fix the
    /// partial accumulator was silently dropped, so the operator
    /// configured a 5-sample average over a 12-element waveform with
    /// N=5 and saw only 2 averages (10 of 12 samples), losing the
    /// tail-of-input data.
    #[test]
    fn pbuf_yes_n_to_1_partial_tail_emits_one_more_compressed_value() {
        // alg=2 (Mean), N=5: 12-element input yields 2 full chunks
        // + 2 leftover samples. With PBUF=YES the leftover is
        // averaged and pushed as a third compressed value.
        let mut rec = CompressRecord::new(8, 2);
        rec.n = 5;
        rec.pbuf = 1;
        let input: Vec<f64> = (1..=12).map(|i| i as f64).collect();
        rec.push_array(&input);
        // Chunks: [1..5] mean=3, [6..10] mean=8, [11,12] mean=11.5
        assert_eq!(rec.nuse, 3, "PBUF=YES must emit tail chunk");
        if let Some(EpicsValue::DoubleArray(v)) = rec.get_field("VAL") {
            assert_eq!(v[0], 3.0);
            assert_eq!(v[1], 8.0);
            assert!((v[2] - 11.5).abs() < 1e-10);
        } else {
            panic!("expected DoubleArray with the 3 compressed values");
        }
    }

    #[test]
    fn pbuf_no_n_to_1_partial_tail_held_for_next_array() {
        // Same input, PBUF=NO (default): tail [11,12] stays in accum
        // for the next push_array call. nuse=2 (only full chunks).
        let mut rec = CompressRecord::new(8, 2);
        rec.n = 5;
        rec.pbuf = 0;
        let first: Vec<f64> = (1..=12).map(|i| i as f64).collect();
        rec.push_array(&first);
        assert_eq!(rec.nuse, 2, "PBUF=NO must defer partial chunk");
        // Next array of 3 more samples fills the chunk (2+3=5), so
        // a third compressed value emits — [11,12,13,14,15] mean=13.
        let second: Vec<f64> = vec![13.0, 14.0, 15.0];
        rec.push_array(&second);
        assert_eq!(rec.nuse, 3, "completed chunk emits on next array");
        if let Some(EpicsValue::DoubleArray(v)) = rec.get_field("VAL") {
            assert_eq!(v[0], 3.0);
            assert_eq!(v[1], 8.0);
            assert_eq!(v[2], 13.0);
        } else {
            panic!("expected DoubleArray");
        }
    }

    #[test]
    fn push_array_circular_buffer_passes_through_unchanged() {
        // alg=4 (Circular Buffer) doesn't compress; every element lands
        // in the circular buffer directly. VAL reads NUSE-clamped per
        // C `get_array_info`.
        let mut rec = CompressRecord::new(4, 4);
        rec.push_array(&[1.0, 2.0, 3.0]);
        assert_eq!(rec.nuse, 3);
        if let Some(EpicsValue::DoubleArray(v)) = rec.get_field("VAL") {
            assert_eq!(v, vec![1.0, 2.0, 3.0]);
        } else {
            panic!("expected DoubleArray");
        }
    }

    /// LIFO mode (BALG=1): newest sample is exposed FIRST on read.
    /// Mirrors C `put_value`'s pre-decrement + `get_array_info`'s
    /// LIFO branch.
    #[test]
    fn lifo_reads_newest_first() {
        let mut rec = CompressRecord::new(4, 4);
        rec.balg = 1; // LIFO
        rec.push_value(10.0);
        rec.push_value(20.0);
        rec.push_value(30.0);
        if let Some(EpicsValue::DoubleArray(v)) = rec.get_field("VAL") {
            assert_eq!(v, vec![30.0, 20.0, 10.0], "LIFO emits newest→oldest");
        } else {
            panic!("expected DoubleArray");
        }
    }

    /// ILIL/IHIL input range filter (C `compress_array`
    /// compressRecord.c:163-170): samples outside `[ILIL, IHIL]` are
    /// dropped before compression.
    #[test]
    fn ilil_ihil_filters_out_of_range_samples() {
        let mut rec = CompressRecord::new(8, 2);
        rec.n = 3;
        rec.ilil = 0.0;
        rec.ihil = 100.0;
        // 50 and 75 pass, -5 and 200 rejected. After 3 valid samples
        // the mean (10+50+75)/3 = 45 is emitted.
        rec.push_array(&[10.0, -5.0, 50.0, 200.0, 75.0]);
        assert_eq!(
            rec.nuse, 1,
            "exactly one chunk emitted after 3 valid samples"
        );
        if let Some(EpicsValue::DoubleArray(v)) = rec.get_field("VAL") {
            assert!((v[0] - 45.0).abs() < 1e-9, "mean of {{10,50,75}} == 45");
        } else {
            panic!("expected DoubleArray");
        }
    }

    /// Average (rolling, alg=3) — C `array_average` semantic.
    /// Accumulate N waveforms element-wise then divide by N.
    #[test]
    fn average_rolling_emits_after_n_waveforms() {
        // NSAM=4, ALG=Average (3), N=3 — average 3 input waveforms.
        let mut rec = CompressRecord::new(4, 3);
        rec.n = 3;
        rec.push_array(&[1.0, 2.0, 3.0, 4.0]);
        assert_eq!(rec.nuse, 0, "no output until N waveforms accumulated");
        rec.push_array(&[10.0, 20.0, 30.0, 40.0]);
        assert_eq!(rec.nuse, 0, "still accumulating");
        rec.push_array(&[100.0, 200.0, 300.0, 400.0]);
        // After 3 waveforms: ((1+10+100)/3, (2+20+200)/3, ...).
        // Output is one ARRAY of nuse=4 elements via 4 put_one calls
        // → ends up as 4 separate entries in the circular buffer.
        assert_eq!(rec.nuse, 4);
        if let Some(EpicsValue::DoubleArray(v)) = rec.get_field("VAL") {
            // (1+10+100)/3 = 37; (2+20+200)/3 = 222/3 = 74; (3+30+300)/3 = 111;
            // (4+40+400)/3 = 444/3 = 148.
            assert!((v[0] - 37.0).abs() < 1e-9);
            assert!((v[1] - 74.0).abs() < 1e-9);
            assert!((v[2] - 111.0).abs() < 1e-9);
            assert!((v[3] - 148.0).abs() < 1e-9);
        } else {
            panic!("expected DoubleArray");
        }
    }
}
