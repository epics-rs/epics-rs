use crate::error::{CaError, CaResult};
use crate::server::record::{FieldDesc, ProcessOutcome, Record};
use crate::types::{DbFieldType, EpicsValue};

/// Compress record — circular buffer with compression algorithms.
pub struct CompressRecord {
    pub val: Vec<f64>,
    pub nsam: i32,   // Number of samples (buffer size)
    pub inp: String, // input link
    pub alg: i16,    // 0=N to 1 Low, 1=N to 1 High, 2=N to 1 Mean, 3=Circular Buffer
    pub n: i32,      // Number of values to compress
    pub nuse: i32,   // Number of elements used
    pub off: i32,    // Current write offset
    pub res: i16,    // Reset flag
    pub balg: i16,   // 0=FIFO, 1=LIFO
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
            alg: 3, // Circular Buffer by default
            n: 1,
            nuse: 0,
            off: 0,
            res: 0,
            balg: 0,
            pbuf: 0,
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

    /// Push a value into the compress record.
    pub fn push_value(&mut self, input: f64) {
        match self.alg {
            3 => {
                // Circular buffer
                let idx = self.off as usize % self.nsam as usize;
                self.val[idx] = input;
                self.off += 1;
                if (self.nuse as usize) < self.nsam as usize {
                    self.nuse += 1;
                }
            }
            _ => {
                // N-to-1 algorithms
                self.accum.push(input);
                if self.accum.len() >= self.n as usize {
                    self.flush_accum();
                }
            }
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
        // Circular Buffer (alg=3) treats every sample independently;
        // the partial-buffer question doesn't apply.
        if self.alg == 3 {
            for &v in input {
                self.push_value(v);
            }
            return;
        }
        for &v in input {
            self.accum.push(v);
            if self.accum.len() >= self.n as usize {
                self.flush_accum();
            }
        }
        // Tail handling: anything still in the accumulator is a
        // partial chunk. PBUF=YES emits it now (PR #84f4771).
        if self.pbuf != 0 && !self.accum.is_empty() {
            self.flush_accum();
        }
    }

    /// Compress `self.accum` via the configured algorithm and push
    /// the result into the circular VAL buffer. Clears `accum`
    /// regardless of partial vs full — callers decide *whether* to
    /// flush; this just executes it.
    fn flush_accum(&mut self) {
        if self.accum.is_empty() {
            return;
        }
        let compressed = match self.alg {
            0 => self.accum.iter().cloned().fold(f64::INFINITY, f64::min), // Low
            1 => self.accum.iter().cloned().fold(f64::NEG_INFINITY, f64::max), // High
            2 => self.accum.iter().sum::<f64>() / self.accum.len() as f64, // Mean
            _ => 0.0,
        };
        let idx = self.off as usize % self.nsam as usize;
        self.val[idx] = compressed;
        self.off += 1;
        if (self.nuse as usize) < self.nsam as usize {
            self.nuse += 1;
        }
        self.accum.clear();
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
                // epics-base 7.0.8 `PBUF` semantics: when YES, expose
                // only the valid leading prefix while the buffer is
                // still filling. Defaults (PBUF=NO) and full-buffer
                // case keep the historic behaviour — whole NSAM-sized
                // vector with trailing zeros for unused slots.
                let valid = self.nuse.max(0) as usize;
                if self.pbuf != 0 && valid < self.val.len() {
                    Some(EpicsValue::DoubleArray(self.val[..valid].to_vec()))
                } else {
                    Some(EpicsValue::DoubleArray(self.val.clone()))
                }
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
            "NSAM" | "OFF" | "NUSE" => Err(CaError::ReadOnlyField(name.to_string())),
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

    /// PBUF defaults to NO (0) — historic behaviour. VAL returns
    /// the full NSAM-sized vector with trailing zeros for unused slots.
    #[test]
    fn pbuf_default_no_returns_full_array() {
        let mut rec = CompressRecord::new(4, 3); // circular buffer NSAM=4
        rec.push_value(1.0);
        rec.push_value(2.0);
        // nuse=2 < nsam=4. PBUF=NO → full vec exposed.
        match rec.get_field("VAL").unwrap() {
            EpicsValue::DoubleArray(v) => {
                assert_eq!(v.len(), 4);
                assert_eq!(v[0], 1.0);
                assert_eq!(v[1], 2.0);
                assert_eq!(v[2], 0.0);
                assert_eq!(v[3], 0.0);
            }
            other => panic!("expected DoubleArray, got {other:?}"),
        }
    }

    /// PBUF=YES exposes only the valid leading prefix while the
    /// buffer is still filling. After it fills, VAL is the full
    /// NSAM vector again.
    #[test]
    fn pbuf_yes_truncates_to_nuse_while_filling() {
        let mut rec = CompressRecord::new(4, 3);
        rec.pbuf = 1;
        rec.push_value(10.0);
        rec.push_value(20.0);
        match rec.get_field("VAL").unwrap() {
            EpicsValue::DoubleArray(v) => assert_eq!(v, vec![10.0, 20.0]),
            other => panic!("expected DoubleArray, got {other:?}"),
        }
        rec.push_value(30.0);
        rec.push_value(40.0);
        // nuse == nsam → full array exposed.
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
        let mut rec = CompressRecord::new(4, 3);
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
        // alg=3 doesn't compress; every element lands in the circular
        // buffer directly. PBUF flag is irrelevant for this path.
        let mut rec = CompressRecord::new(4, 3);
        rec.push_array(&[1.0, 2.0, 3.0]);
        assert_eq!(rec.nuse, 3);
        if let Some(EpicsValue::DoubleArray(v)) = rec.get_field("VAL") {
            assert_eq!(v.len(), 4);
            assert_eq!(&v[..3], &[1.0, 2.0, 3.0]);
        } else {
            panic!("expected DoubleArray");
        }
    }
}
