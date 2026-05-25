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
    /// `CVB` (compress value buffer) — C `prec->cvb`. The running
    /// scalar accumulator for the N-to-1 *scalar* path
    /// (`compress_scalar`, compressRecord.c:273-304): Low/High keep a
    /// running extreme, Average keeps the incremental mean
    /// `(inx*cvb + value)/(inx+1)`. Exposed as a readable field so a
    /// CA client can observe the partial accumulation mid-cycle.
    pub cvb: f64,
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
    // Internal element-wise summing buffer for the rolling-Average
    // algorithm (alg=3) — C `prec->sptr`. The N-to-1 algorithms keep
    // their running state in `cvb`/`inx` (`compress_scalar`) or work
    // a whole waveform in one call (`compress_array`).
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
            cvb: 0.0,
            accum: Vec::new(),
        }
    }
}

impl CompressRecord {
    pub fn new(nsam: i32, alg: i16) -> Self {
        // Clamp the allocation length to >= 1: a negative or zero
        // `nsam` would make `nsam as usize` wrap to a huge value and
        // panic the `vec![0.0; ..]` allocation. Mirrors `histogram::new`
        // (`let n = nelm.max(1)`) and the `put_one` `self.nsam.max(1)`
        // guard already in this record.
        let n = nsam.max(1) as usize;
        Self {
            val: vec![0.0; n],
            nsam,
            alg,
            ..Default::default()
        }
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

    /// Push a single scalar value into the compress record.
    ///
    /// C `compressRecord.c::process` routes a 1-element input to
    /// `compress_scalar`, which keeps a running scalar `cvb`/`inx`
    /// rather than an N-element accumulator. ILIL/IHIL filtering is
    /// **not** applied on the scalar path — C's skip loop lives only
    /// in `compress_array` (the `nelements > 1` branch).
    pub fn push_value(&mut self, input: f64) {
        match self.alg {
            // menuCompressALG_Circular_Buffer
            4 => self.put_one(input),
            // alg=3 (Average rolling) is array-oriented in C
            // (`array_average` operates on a whole waveform). A
            // scalar push degrades to a 1-element array call so the
            // running average behaves predictably for either input
            // shape.
            3 => self.push_array_average(&[input]),
            // N-to-1 algorithms — C `compress_scalar`: running `cvb`.
            _ => self.compress_scalar(input),
        }
    }

    /// C `compress_scalar` (compressRecord.c:273-304): fold one
    /// sample into the running `cvb` accumulator and emit a
    /// compressed value once `inx` reaches `n` (or `pbuf == YES`).
    fn compress_scalar(&mut self, value: f64) {
        let inx = self.inx;
        match self.alg {
            // N_to_1_Low_Value
            0 => {
                if value < self.cvb || inx == 0 {
                    self.cvb = value;
                }
            }
            // N_to_1_High_Value
            1 => {
                if value > self.cvb || inx == 0 {
                    self.cvb = value;
                }
            }
            // N_to_1_Average / N_to_1_Median (scalar Median == Average)
            _ => {
                self.cvb = (inx as f64 * self.cvb + value) / (inx as f64 + 1.0);
            }
        }
        let inx = inx + 1;
        let n = self.n.max(1);
        if inx >= n || self.pbuf != 0 {
            let cvb = self.cvb;
            self.put_one(cvb);
            // C: prec->inx = (inx >= n) ? 0 : inx;
            self.inx = if inx >= n { 0 } else { inx };
        } else {
            self.inx = inx;
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
            // C `array_average` does NOT apply ILIL/IHIL filtering —
            // the skip loop lives only in `compress_array`.
            3 => self.push_array_average(input),
            // N-to-1 algorithms (Low/High/N_to_1_Average/Median):
            // C `compress_array` (compressRecord.c:154-221).
            _ => self.compress_array(input),
        }
    }

    /// C `compress_array` (compressRecord.c:154-221). Skips a
    /// **leading** run of out-of-limit samples (NOT a per-sample
    /// filter — an out-of-limit sample in the middle of the array is
    /// kept), then compresses consecutive N-element chunks. A trailing
    /// partial chunk (`< N`) is emitted only when `PBUF == YES`,
    /// otherwise it is dropped (C `break`s out of the loop).
    fn compress_array(&mut self, input: &[f64]) {
        // C: skip leading out-of-limit data.
        let mut start = 0usize;
        if self.ilil < self.ihil {
            while start < input.len() && (input[start] < self.ilil || input[start] > self.ihil) {
                start += 1;
            }
        }
        let source = &input[start..];
        let n = self.n.max(1) as usize;
        // C: nnew = min(no_elements, nsam * n).
        let nsam = self.nsam.max(1) as usize;
        let mut remaining = source.len().min(nsam.saturating_mul(n));
        let mut pos = 0usize;
        while remaining > 0 {
            // C: if (nnew < n && pbuf != YES) break;
            if remaining < n && self.pbuf == 0 {
                break;
            }
            let chunk_len = n.min(remaining);
            let chunk = &source[pos..pos + chunk_len];
            let value = match self.alg {
                // N_to_1_Low_Value
                0 => chunk.iter().cloned().fold(f64::INFINITY, f64::min),
                // N_to_1_High_Value
                1 => chunk.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
                // N_to_1_Average
                2 => chunk.iter().sum::<f64>() / chunk_len as f64,
                // N_to_1_Median: middle element after sort (C `psource[n/2]`).
                _ => {
                    let mut sorted = chunk.to_vec();
                    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                    sorted[sorted.len() / 2]
                }
            };
            self.put_one(value);
            pos += chunk_len;
            remaining -= chunk_len;
        }
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
    FieldDesc {
        name: "CVB",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
];

impl Record for CompressRecord {
    fn record_type(&self) -> &'static str {
        "compress"
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        if self.res != 0 {
            // C `reset` (compressRecord.c:85-99) clears the running
            // accumulator state too — `inx`, `cvb` and the summing
            // buffer — not just `off`/`nuse`.
            self.off = 0;
            self.nuse = 0;
            self.inx = 0;
            self.cvb = 0.0;
            self.accum.clear();
            for v in &mut self.val {
                *v = 0.0;
            }
            self.res = 0;
        }
        Ok(ProcessOutcome::complete())
    }

    // R52: CompressRecord missing egu/hopr/lopr/prec struct fields entirely;
    // DBR_GR display limits zeroed for compress PVs (compressRecord.c:478-479,455).
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
            "CVB" => Some(EpicsValue::Double(self.cvb)),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        match name {
            "VAL" => match value {
                // C `compressRecord.c` has NO raw-overwrite path for
                // VAL: VAL *is* the circular buffer and a CA put is
                // handled by `put_array_info`, which feeds data
                // through the algorithm and advances `off`/`nuse`.
                // Replacing `self.val` directly desynced `nuse`/`off`
                // and could shrink the backing buffer below `nsam`,
                // panicking `linearise_val` (out-of-bounds index).
                // Route the array through the normal `push_array`
                // ingestion so the buffer invariant is preserved.
                EpicsValue::DoubleArray(arr) => {
                    self.push_array(&arr);
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
                    self.inx = 0;
                    self.cvb = 0.0;
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
            "NSAM" | "OFF" | "NUSE" | "INX" | "CVB" => {
                Err(CaError::ReadOnlyField(name.to_string()))
            }
            _ => Err(CaError::FieldNotFound(name.to_string())),
        }
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        COMPRESS_FIELDS
    }

    fn primary_field(&self) -> &'static str {
        "VAL"
    }

    /// C `compressRecord.c::process` calls `dbGetLink(&prec->inp, ...)`
    /// every cycle and feeds the result to the algorithm. The Rust
    /// record cannot read links itself; it asks the framework to pull
    /// `INP` into `VAL` before `process()`. The `VAL` put routes
    /// through `push_array`, so the data is ingested by the configured
    /// compression algorithm (NOT a raw buffer overwrite — see C-1).
    fn pre_process_actions(&mut self) -> Vec<crate::server::record::ProcessAction> {
        if self.inp.is_empty() {
            return Vec::new();
        }
        vec![crate::server::record::ProcessAction::ReadDbLink {
            link_field: "INP",
            target_field: "VAL",
        }]
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
    fn pbuf_no_n_to_1_partial_tail_dropped_per_array() {
        // C `compress_array` is per-waveform: with PBUF=NO the
        // trailing partial chunk (`nnew < n`) hits the `break` and is
        // DROPPED — it does NOT persist into the next push_array.
        let mut rec = CompressRecord::new(8, 2);
        rec.n = 5;
        rec.pbuf = 0;
        let first: Vec<f64> = (1..=12).map(|i| i as f64).collect();
        rec.push_array(&first);
        // Chunks [1..5]=3, [6..10]=8; leftover [11,12] dropped.
        assert_eq!(rec.nuse, 2, "PBUF=NO drops the trailing partial chunk");
        // A second array of only 3 samples (< N=5) emits nothing —
        // there is no carried-over partial state.
        rec.push_array(&[13.0, 14.0, 15.0]);
        assert_eq!(rec.nuse, 2, "short array < N emits nothing with PBUF=NO");
        if let Some(EpicsValue::DoubleArray(v)) = rec.get_field("VAL") {
            assert_eq!(v[0], 3.0);
            assert_eq!(v[1], 8.0);
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

    /// ILIL/IHIL input range filter — C `compress_array`
    /// (compressRecord.c:163-170) skips only the *leading* run of
    /// out-of-limit samples; an out-of-limit sample in the middle of
    /// the array is NOT dropped.
    #[test]
    fn ilil_ihil_skips_only_leading_out_of_limit_run() {
        let mut rec = CompressRecord::new(8, 2);
        rec.n = 3;
        rec.ilil = 0.0;
        rec.ihil = 100.0;
        // First sample 10 is in range → nothing skipped. First chunk
        // [10,-5,50] keeps the mid-array out-of-limit -5: mean=55/3.
        rec.push_array(&[10.0, -5.0, 50.0, 200.0, 75.0]);
        assert_eq!(rec.nuse, 1, "one full chunk; trailing [200,75] dropped");
        if let Some(EpicsValue::DoubleArray(v)) = rec.get_field("VAL") {
            assert!(
                (v[0] - 55.0 / 3.0).abs() < 1e-9,
                "mid-array out-of-limit sample is kept"
            );
        } else {
            panic!("expected DoubleArray");
        }
    }

    /// Leading out-of-limit samples ARE skipped before compression.
    #[test]
    fn ilil_ihil_skips_leading_run() {
        let mut rec = CompressRecord::new(8, 2);
        rec.n = 2;
        rec.ilil = 0.0;
        rec.ihil = 100.0;
        // Leading -5, 200 skipped; chunk [10,20] mean=15.
        rec.push_array(&[-5.0, 200.0, 10.0, 20.0]);
        assert_eq!(rec.nuse, 1);
        if let Some(EpicsValue::DoubleArray(v)) = rec.get_field("VAL") {
            assert!((v[0] - 15.0).abs() < 1e-9);
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
