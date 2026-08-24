use crate::error::{CaError, CaResult};
use crate::server::record::{FieldMetadataOverride, ProcessOutcome, Record};
use crate::server::records::count_put;
use crate::types::{EpicsValue, PvString};

/// Compress record — circular buffer with compression algorithms.
///
/// `alg` follows C `menuCompressALG` (compressRecord.dbd.pod):
///   0 = N to 1 Low Value
///   1 = N to 1 High Value
///   2 = N to 1 Average
///   3 = Average (rolling element-wise mean of N waveforms — `array_average`,
///       C `array_average` compressRecord.c:223)
///   4 = Circular Buffer
///   5 = N to 1 Median (sort each N-element chunk, take the middle
///       `psource[n/2]` — `compress_array`, C compressRecord.c:209-211; a
///       scalar input degrades to Average, matching C `compress_scalar`)
///
/// The numeric values are CA-wire-visible (DBR_SHORT) and must match
/// `menuCompressALG` so a C client setting `ALG=4` reaches the
/// Circular-Buffer code path.
pub struct CompressRecord {
    pub val: Vec<f64>,
    pub nsam: i32, // Number of samples (buffer size)
    pub alg: i16,  // See top-of-struct doc — matches menuCompressALG
    pub n: i32,    // Number of values to compress
    pub nuse: i32, // Number of elements used
    pub off: i32,  // Current write offset (see `put_one` for BALG-dependent role)
    pub res: i16,  // Reset flag
    pub balg: i16, // 0=FIFO, 1=LIFO — `menuBufferingALG`
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
    /// `OUSE` (old number used) — C `prec->ouse`, `special(SPC_NOMOD)`
    /// (compressRecord.dbd.pod:481-484). The latch behind C `monitor()`'s
    /// "post NUSE only when it changed" rule (compressRecord.c:104-108); see
    /// `Self::monitor`. Readable, never client-writable.
    pub ouse: i32,
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
    pub egu: PvString,
    pub hopr: f64,
    pub lopr: f64,
    pub prec: i16,
    // Internal element-wise summing buffer for the rolling-Average
    // algorithm (alg=3) — C `prec->sptr`. The N-to-1 algorithms keep
    // their running state in `cvb`/`inx` (`compress_scalar`) or work
    // a whole waveform in one call (`compress_array`).
    accum: Vec<f64>,
    // Per-cycle completion gate mirroring C `compressRecord.c::process`
    // `status`. The ingestion (`push_value`/`push_array`, run during the
    // pre-process INP read) sets `cycle_ingested`; `put_one` (the single emit
    // point) sets `cycle_emitted`. `process()` suppresses the publication
    // epilogue iff `cycle_ingested && !cycle_emitted` — C `status == 1`, the
    // record accumulated this cycle without emitting. No ingestion at all
    // (link error / empty read / no INP) leaves `cycle_ingested` false, so the
    // epilogue runs — C forces `status = 0` on those paths. Both are reset
    // each cycle in `pre_process_actions`.
    cycle_ingested: bool,
    cycle_emitted: bool,
    /// `INPN` — C `prec->inpn`, `DBF_LONG special(SPC_NOMOD)`
    /// (compressRecord.dbd.pod:503-506), "Number of elements in Working
    /// Buffer": the INP source's element count from the previous INP-driven
    /// ingest, and the trigger for C's WPTR reallocation — when it changes
    /// between cycles C frees the working buffer and `reset()`s
    /// (compressRecord.c:334-340).
    ///
    /// KNOWN DIVERGENCE in the value served. C latches
    /// `dbGetNelements(&prec->inp, &nelements)` — the source's element
    /// CAPACITY — before the read; the port latches the length the framework's
    /// `ReadDbLink` actually delivered. For a `waveform` source with NELM=3 and
    /// NORD=1, softIoc reads `CMP2.INPN = 3` where the port reads 1. Closing it
    /// needs the source capacity plumbed through `ReadDbLink`, which no other
    /// record needs; the reset trigger itself is unaffected in practice (a
    /// source's capacity and its delivered length both change exactly when the
    /// source array is re-shaped).
    inpn: usize,
    /// What C `monitor()` (:100-110) decided to post for the SPC_RESET put now
    /// in flight. Written by [`Self::monitor`] — the only place that makes the
    /// decision, and the only writer of `ouse` — and read back by
    /// `monitor_side_effect_fields`, which the put owner calls immediately
    /// after `special()`, for the same put. One meaning on every path: "the
    /// field set the last `monitor()` posted".
    monitor_posts: &'static [&'static str],
    // True only while applying this cycle's INP read (set in
    // `pre_process_actions`, consumed in `push_array`, cleared in `process`).
    // C's element-count reset lives in `process` and keys on `prec->wptr` (the
    // INP read buffer) — a CA put to VAL goes through `put_array_info`, never
    // `process`, so it must NOT reset. In Rust both the INP read and a direct
    // VAL put reach `push_array` via `put_field("VAL")`; this flag is how
    // `push_array` tells them apart so only the INP-driven ingest can reset.
    inp_read_pending: bool,
}

impl Default for CompressRecord {
    fn default() -> Self {
        Self {
            // C `compressRecord.dbd.pod` `field(NSAM,DBF_ULONG){ initial("1") }`:
            // an unset NSAM defaults to a 1-sample buffer, not 10. VAL is the
            // NSAM-length buffer; a NSAM put resizes it (put_field below), so
            // there is no load-order dependency.
            val: vec![0.0; 1],
            nsam: 1,
            // C `compressRecord.dbd.pod` `field(ALG,DBF_MENU){ menu(compressALG) }`
            // has no `initial(...)`, so an unset ALG defaults to menu index 0 =
            // `compressALG_N_to_1_Low_Value`, not Circular Buffer.
            alg: 0,
            n: 1,
            nuse: 0,
            ouse: 0,
            off: 0,
            res: 0,
            balg: 0,
            pbuf: 0,
            ilil: 0.0,
            ihil: 0.0,
            inx: 0,
            cvb: 0.0,
            egu: PvString::new(),
            hopr: 0.0,
            lopr: 0.0,
            prec: 0,
            accum: Vec::new(),
            cycle_ingested: false,
            cycle_emitted: false,
            inpn: 0,
            monitor_posts: &[],
            inp_read_pending: false,
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

    /// C `reset()` (compressRecord.c:85-99) — **the single owner of the
    /// compress buffer reset**.
    ///
    /// ```c
    /// prec->nuse = 0; prec->off = 0; prec->inx = 0;
    /// prec->cvb = 0.0; prec->res = 0;
    /// if (prec->alg == compressALG_Average && prec->sptr == NULL)
    ///     prec->sptr = calloc(prec->nsam, sizeof(double));
    /// if (prec->bptr && prec->nsam)
    ///     memset(prec->bptr, 0, prec->nsam * sizeof(double));
    /// ```
    ///
    /// INVARIANT: every C `SPC_RESET` write reaches exactly this body. C has
    /// one caller shape — `special()` (compressRecord.c:377-393) runs
    /// `reset(); monitor();` for the field index it was handed, whichever of
    /// the five `special(SPC_RESET)` fields it is (RES, ALG, PBUF, BALG, N;
    /// compressRecord.dbd.pod:396-437) — plus `init_record` pass 0 and the
    /// INP element-count change in `process()`. The port's owners are
    /// [`Record::special`], [`Record::init_record`] and `push_array`; no put
    /// arm resets on its own.
    ///
    /// `res = 0` is part of the reset, not of the RES put arm: C stores the
    /// written RES and `special()` then zeroes it, which is why `caput RES 1`
    /// reads back 0 (softIoc: `dbpf CMP.RES 1` → `DBF_SHORT: 0`).
    ///
    /// The `accum` clear is C's summing-buffer handling: C only *allocates*
    /// `sptr` (it does not zero an existing one), but it also sets `inx = 0`,
    /// and `push_array_average` overwrites the accumulator wholesale at
    /// `inx == 0`, so dropping the allocation is equivalent and re-allocates
    /// lazily at the next Average cycle.
    ///
    /// Does NOT touch the C-1 completion gate (`cycle_ingested`/
    /// `cycle_emitted`): a reset emits nothing, so the gate stays whatever the
    /// surrounding cycle set it to.
    fn reset(&mut self) {
        self.off = 0;
        self.nuse = 0;
        self.inx = 0;
        self.cvb = 0.0;
        self.res = 0;
        self.accum.clear();
        for v in &mut self.val {
            *v = 0.0;
        }
    }

    /// C `compressRecord.c::monitor` (:100-110) — **the single owner of the
    /// OUSE latch**:
    ///
    /// ```c
    /// if (alarm_mask || prec->nuse != prec->ouse) {
    ///     db_post_events(prec, &prec->nuse, monitor_mask);
    ///     prec->ouse = prec->nuse;
    /// }
    /// db_post_events(prec, &prec->val, monitor_mask);
    /// ```
    ///
    /// NUSE posts only when it CHANGED since the last post; VAL posts every
    /// time. OUSE is the "last posted NUSE" latch that makes that decision, and
    /// nothing else may write it.
    ///
    /// C calls `monitor()` from exactly two places, and so does the port: the
    /// SPC_RESET `special()` ([`Record::special`]) and the publication epilogue
    /// of `process()` (`:365-372`, skipped on an accumulate-only cycle — which
    /// is why an accumulating compress does not re-post NUSE).
    ///
    /// Returns the fields to post; `special()` hands them to the framework
    /// through [`Record::monitor_side_effect_fields`] (the alarm-mask term is
    /// the framework's — it posts on an alarm transition regardless).
    fn monitor(&mut self) -> &'static [&'static str] {
        if self.nuse != self.ouse {
            self.ouse = self.nuse;
            &["NUSE", "VAL"]
        } else {
            &["VAL"]
        }
    }

    /// Write one value into the circular buffer, advancing `off`
    /// and `nuse` per BALG. Mirrors C `put_value` (compressRecord.c).
    ///
    /// This is the single point at which a compressed sample is emitted, so it
    /// owns the per-cycle `cycle_emitted` flag that drives the completion gate
    /// (C `compressRecord.c::process` `status == 0`). Every algorithm path —
    /// circular, N-to-1 array/scalar, rolling average — emits through here.
    fn put_one(&mut self, value: f64) {
        self.cycle_emitted = true;
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
        // A scalar push ingests one sample this cycle; emission is recorded by
        // `put_one`. This covers the C `nelements == 1` → `compress_scalar`
        // path, which accumulates across cycles and emits only every Nth.
        self.cycle_ingested = true;
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
            // C `array_average` `return 1`: still accumulating, no emit
            // (no `put_one`, so the completion gate stays unset this cycle).
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
        if input.is_empty() {
            // C treats a zero-element read as a link error (status forced 0 →
            // completion epilogue runs). Mark no ingestion so `process()`
            // publishes rather than suppressing.
            return;
        }
        // A non-empty ingestion happened this cycle; whether it emitted is
        // recorded by `put_one`. See `cycle_ingested`/`cycle_emitted`.
        self.cycle_ingested = true;
        // C compressRecord.c:334-340: when the INP source's element count
        // changes between INP-driven cycles, C frees the buffer and `reset()`s,
        // restarting accumulation clean at the new length. Without this the
        // rolling Average (ALG=3) keeps stale per-element partial sums from the
        // old length and blends them with the new waveform → a corrupt average.
        // C gates the reset on `prec->wptr` already being allocated (a prior
        // INP read), so the FIRST INP read does not reset; and the reset lives
        // in `process` (the INP path), NOT in a CA put to VAL (`put_array_info`).
        // `inp_read_pending` confines the reset to the INP-driven ingest: a
        // direct VAL put / unit-test `push_array` is a raw buffer op and never
        // resets. The reset clears no completion-gate state, so C-1's emit gate
        // holds (a reset cycle still ingests, then the algorithm decides emit).
        if self.inp_read_pending {
            self.inp_read_pending = false;
            if self.inpn != 0 && input.len() != self.inpn {
                self.reset();
            }
            self.inpn = input.len();
        }
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

    /// C `compress_array`'s Low/High scan (`compressRecord.c:183-196`) — the
    /// shape a fold over a ±INF identity cannot express:
    ///
    /// ```c
    ///     value = *psource++;
    ///     for (j = 1; j < n; j++, psource++)
    ///         if (value > *psource) value = *psource;   /* High uses < */
    /// ```
    ///
    /// The seed is the chunk's FIRST sample and `value` moves only on a STRICT
    /// comparison, so NaN is not filtered but decided by position: a NaN seed
    /// survives every false comparison and the chunk answers NaN, while a NaN
    /// anywhere later loses every comparison and is skipped. `f64::min` /
    /// `f64::max` discard NaN in both positions, and the ±INF identity they
    /// need leaks out of an all-NaN chunk as ±inf. `compressRecord.c` has no
    /// `isnan` anywhere — unlike `selRecord.c:361-377`, which really does seed
    /// ±epicsINF and guard `!isnan`, and unlike `calcPerform.c:191-207`, whose
    /// `isnan(d)` clause lets a NaN ARGUMENT win.
    ///
    /// `reduce` seeds from the first element exactly as `*psource++` does; the
    /// `None` arm is C's own `double value = 0.0` initializer
    /// (`compressRecord.c:160`), which is what the switch would leave behind
    /// for a chunk the loop never entered.
    fn extremum(chunk: &[f64], replace: impl Fn(f64, f64) -> bool) -> f64 {
        chunk
            .iter()
            .copied()
            .reduce(|value, source| {
                if replace(value, source) {
                    source
                } else {
                    value
                }
            })
            .unwrap_or(0.0)
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
                // N_to_1_Low_Value: `if (value > *psource) value = *psource;`
                0 => Self::extremum(chunk, |value, source| value > source),
                // N_to_1_High_Value: `if (value < *psource) value = *psource;`
                1 => Self::extremum(chunk, |value, source| value < source),
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

/// Choice labels for the compression algorithm menu, in index order.
/// C `menu(compressALG)` (`compressRecord.dbd.pod:49-55`).
const COMPRESS_ALG_CHOICES: &[&str] = &[
    "N to 1 Low Value",
    "N to 1 High Value",
    "N to 1 Average",
    "Average",
    "Circular Buffer",
    "N to 1 Median",
];

/// Choice labels for the buffering algorithm menu, in index order.
/// C `menu(bufferingALG)` (`compressRecord.dbd.pod:57-59`).
const COMPRESS_BALG_CHOICES: &[&str] = &["FIFO Buffer", "LIFO Buffer"];

/// The five fields `compressRecord.dbd.pod` declares `special(SPC_RESET)`:
/// RES (:396-400), ALG (:402-408), PBUF (:409-416), BALG (:417-423) and
/// N (:431-437). C's `special()` (compressRecord.c:377-393) does not
/// discriminate between them — any SPC_RESET write runs `reset(); monitor();` —
/// so this list is both the [`Record::special`] trigger set and the
/// [`Record::monitor_side_effect_fields`] key set. None of the five is
/// `pp(TRUE)`, so the side-effect post is the only monitor the put produces.
const COMPRESS_SPC_RESET_FIELDS: &[&str] = &["RES", "ALG", "PBUF", "BALG", "N"];

impl Record for CompressRecord {
    fn record_type(&self) -> &'static str {
        "compress"
    }

    /// `compressRecord.c:341-366`: a failed `dbGetLink` on INP raises
    /// LINK/INVALID and then FOLDS the failure away — `status = 0` — so the
    /// tail's `if (status != 1) prec->udf = FALSE;` clears UDF on the broken
    /// cycle just as it does on a good one. compress is the one scalar-ish
    /// record that joins the array records here.
    fn derives_udf_on_read_failure(&self) -> bool {
        true
    }

    /// `compressRecord.c:479-493` `get_control_double` lists `VAL`, `IHIL` and
    /// `ILIL` — the two init-limit fields answer the record's `HOPR`/`LOPR`,
    /// like `VAL`. Note the list does NOT include the alarm bands: `compress`
    /// NULLs `get_alarm_double` entirely, so it has none to list.
    fn field_metadata_override(&self, field: &str) -> Option<FieldMetadataOverride> {
        ["IHIL", "ILIL"]
            .iter()
            .any(|f| field.eq_ignore_ascii_case(f))
            .then(|| FieldMetadataOverride {
                ctrl_limits: Some((self.hopr, self.lopr)),
                ..Default::default()
            })
    }

    /// C `compressRecord.c::special` (:377-393) — the SPC_RESET hook:
    ///
    /// ```c
    /// if (special_type == SPC_RESET) { reset(prec); monitor(prec); return 0; }
    /// ```
    ///
    /// The field index never enters the decision, so ALL FIVE SPC_RESET fields
    /// (`COMPRESS_SPC_RESET_FIELDS`) reset the buffer, not RES alone. The
    /// `monitor()` half is [`Record::monitor_side_effect_fields`].
    fn special(&mut self, field: &str, after: bool) -> CaResult<()> {
        if after && COMPRESS_SPC_RESET_FIELDS.contains(&field) {
            self.reset();
            self.monitor_posts = self.monitor();
        }
        Ok(())
    }

    fn monitor_side_effect_fields(&self, put_field: &str) -> &'static [&'static str] {
        // The `monitor(prec)` half of C's SPC_RESET `special()`. Which fields
        // that is, is `monitor()`'s decision, not this hook's: VAL always, NUSE
        // only when it changed against the OUSE latch (compressRecord.c:104-108)
        // — so a second RES put on an already-empty buffer re-posts VAL alone,
        // as C does. Keyed on the same five fields as the reset itself; none of
        // them is pp(TRUE), so this is the only monitor such a put produces.
        if COMPRESS_SPC_RESET_FIELDS.contains(&put_field) {
            self.monitor_posts
        } else {
            &[]
        }
    }

    /// The `recGblResetAlarms` half of C's SPC_RESET `special()`. C's `monitor()`
    /// (compressRecord.c:103) — which `special()` invokes for every SPC_RESET
    /// write — opens with `recGblResetAlarms(prec)`, so a put to any of the five
    /// reset fields commits the record's pending alarm (clearing the born-UDF of
    /// a never-processed compress) without a process cycle. Keyed on the same
    /// five fields as [`Record::monitor_side_effect_fields`].
    fn special_commits_alarms(&self, put_field: &str) -> bool {
        COMPRESS_SPC_RESET_FIELDS.contains(&put_field)
    }

    /// C `init_record` pass 0 (compressRecord.c:307-315): allocate the sample
    /// buffer, then `reset(prec)`. The reset is what clears a `.db`-loaded
    /// `field(RES,"1")` — the static loader bypasses `special()`, so without
    /// this pass the record would come up with RES stuck at 1.
    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 0 {
            if self.nsam < 1 {
                self.nsam = 1;
                self.val = vec![0.0; 1];
            }
            self.reset();
        }
        Ok(())
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        // Safety net: clear the INP-read marker for any cycle whose read never
        // reached `push_array` (no INP, empty/error read, scalar via
        // `push_value`), so a later direct VAL put cannot inherit a stale mark.
        self.inp_read_pending = false;
        // C's `process()` does NOT inspect RES: the reset is `special()`'s, and
        // `special()` zeroes RES as part of `reset()`. A record can never enter
        // `process()` with RES set.
        //
        // C `compressRecord.c:365` `if (status != 1)`: when this cycle's
        // ingestion (run during the pre-process INP read) accumulated without
        // emitting a compressed sample (C `status == 1`), the framework must
        // skip the value-publication epilogue (UDF clear / timestamp / monitor
        // / FLNK). An emit, or no ingestion at all (link error / no INP — C
        // forces `status = 0`), publishes.
        if self.cycle_ingested && !self.cycle_emitted {
            // C skips the epilogue entirely on `status == 1`, `monitor()`
            // included — so OUSE does NOT latch and NUSE is not posted.
            return Ok(ProcessOutcome::complete_no_emit());
        }
        // C `compressRecord.c:365-372`: the epilogue calls `monitor(prec)`,
        // which is where OUSE latches on a publishing cycle.
        self.monitor_posts = self.monitor();
        Ok(ProcessOutcome::complete())
    }

    // CompressRecord missing egu/hopr/lopr/prec struct fields entirely;
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
            // NSAM/N/OFF/NUSE/OUSE/INX are C's DBF_ULONG (INPN alone is
            // DBF_LONG, :503). The value variant is what CA and PVA project the
            // native type from — CA promotes DBF_ULONG to DBR_DOUBLE
            // (db_convert.h), PVA serves uint32 — so it must agree with the
            // `FieldDesc`. The counters are stored `i32` (the ring arithmetic is
            // signed) and every writer floors them at 0, so `as u32` is exact.
            "NSAM" => Some(EpicsValue::ULong(self.nsam as u32)),
            "NUSE" => Some(EpicsValue::ULong(self.nuse as u32)),
            "OUSE" => Some(EpicsValue::ULong(self.ouse as u32)),
            "INPN" => Some(EpicsValue::Long(self.inpn as i32)),
            "RES" => Some(EpicsValue::Short(self.res)),
            "BALG" => Some(EpicsValue::Short(self.balg)),
            "ALG" => Some(EpicsValue::Short(self.alg)),
            "N" => Some(EpicsValue::ULong(self.n as u32)),
            "OFF" => Some(EpicsValue::ULong(self.off as u32)),
            "PBUF" => Some(EpicsValue::Short(self.pbuf)),
            "ILIL" => Some(EpicsValue::Double(self.ilil)),
            "IHIL" => Some(EpicsValue::Double(self.ihil)),
            "INX" => Some(EpicsValue::ULong(self.inx as u32)),
            "CVB" => Some(EpicsValue::Double(self.cvb)),
            "EGU" => Some(EpicsValue::String(self.egu.clone())),
            "HOPR" => Some(EpicsValue::Double(self.hopr)),
            "LOPR" => Some(EpicsValue::Double(self.lopr)),
            "PREC" => Some(EpicsValue::Short(self.prec)),
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
            "N" => match count_put(&value) {
                Some(v) => {
                    self.n = v;
                    Ok(())
                }
                None => Err(CaError::TypeMismatch("N".into())),
            },
            // The four SPC_RESET arms below (RES/ALG/N/BALG/PBUF) STORE ONLY.
            // The reset is `special()`'s — C stores the value in `dbPut` and
            // then runs `dbPutSpecial(paddr, 1)`, which is where `reset()`
            // lives. That is also why RES reads back 0: `reset()` zeroes it
            // after this arm stored the 1.
            "RES" => match value {
                EpicsValue::Short(v) => {
                    self.res = v;
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
                    self.pbuf = match s.as_str_lossy().to_ascii_uppercase().as_str() {
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
            // C `field(NSAM,DBF_ULONG){ promptgroup special(SPC_NOMOD) }`:
            // settable at `.db` load, runtime-immutable (the field_io
            // `read_only` gate; the FieldDesc carries `read_only: true`). This
            // arm serves only the load path, sizing the sample buffer like
            // `new()`.
            "NSAM" => match count_put(&value) {
                Some(n) => {
                    let n = n.max(1);
                    self.nsam = n as i32;
                    self.val = vec![0.0; n as usize];
                    // Resizing the ring invalidates the cursor and used count.
                    self.nuse = 0;
                    self.off = 0;
                    Ok(())
                }
                None => Err(CaError::TypeMismatch("NSAM".into())),
            },
            // OFF/NUSE/OUSE/INPN/INX/CVB are `special(SPC_NOMOD)` runtime buffer
            // state — never client-writable, and (unlike NSAM) not settable at
            // load either: C gives them no promptgroup and the record owns each.
            "OFF" | "NUSE" | "OUSE" | "INPN" | "INX" | "CVB" => {
                Err(CaError::ReadOnlyField(name.to_string()))
            }
            "EGU" => {
                if let EpicsValue::String(s) = value {
                    self.egu = s;
                    Ok(())
                } else {
                    Err(CaError::TypeMismatch("EGU".into()))
                }
            }
            "HOPR" => {
                self.hopr = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("HOPR".into()))?;
                Ok(())
            }
            "LOPR" => {
                self.lopr = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("LOPR".into()))?;
                Ok(())
            }
            "PREC" => {
                self.prec = value
                    .to_f64()
                    .ok_or_else(|| CaError::TypeMismatch("PREC".into()))?
                    as i16;
                Ok(())
            }
            _ => Err(CaError::FieldNotFound(name.to_string())),
        }
    }

    /// C `compressRecord.c::cvt_dbaddr` (:398-407):
    ///
    /// ```c
    /// if (prec->balg == bufferingALG_LIFO)
    ///     paddr->special = SPC_NOMOD;
    /// ```
    ///
    /// A LIFO compress's VAL is not client-writable — `put_value` walks the
    /// ring backwards from `off`, so `put_array_info`'s forward write cursor
    /// (which is what a `dbPut` to VAL feeds) has no meaning there, and C
    /// refuses the put outright. FIFO leaves VAL writable. This is per-record
    /// STATE, not a `.dbd` declaration, so it cannot live in `field_list`'s
    /// static `read_only`; [`Record::field_no_mod`] is the dynamic half of the
    /// same gate.
    ///
    /// The INP-driven ingest is unaffected: it lands through
    /// `put_field_internal`, which is C's direct-to-memory `dbGetLink` write
    /// into `wptr`, not a `dbPut` on VAL.
    ///
    /// softIoc: with BALG="LIFO Buffer", `dbpf CMP.VAL 7` →
    /// `recGblDbaddrError: dbPut Attempt to modify noMod field PV: CMP.VAL`.
    /// `compressRecord.c` declares no `dset` — the record reads its own `INP`
    /// (`process`: `dbGetLink(&prec->inp, …)`), so there is no soft device
    /// support to run `recGblInitConstantLink` on it at init.
    fn input_read_by_device_support(&self) -> bool {
        false
    }

    fn field_no_mod(&self, field: &str) -> bool {
        field == "VAL" && self.balg == 1
    }

    /// The other half of the same `cvt_dbaddr`: `paddr->no_elements =
    /// prec->nsam` — the whole ring, not the `NUSE` that `get_array_info`
    /// serves out of it. Sizing the channel from the fill level stranded a
    /// client that connected before the ring filled, since `ca_element_count`
    /// is settled once at create-channel time.
    fn dbaddr_capacity(&self, _field: &str) -> Option<u32> {
        Some(self.nsam.max(0) as u32)
    }

    fn menu_field_choices(&self, field: &str) -> Option<&'static [&'static str]> {
        match field {
            "ALG" => Some(COMPRESS_ALG_CHOICES),
            "BALG" => Some(COMPRESS_BALG_CHOICES),
            _ => None,
        }
    }

    fn primary_field(&self) -> &'static str {
        "VAL"
    }

    /// C `compressRecord.c:326-343` — the INP ingest, and the one gate on it:
    ///
    /// ```c
    /// if (!dbIsLinkConnected(&prec->inp) || dbGetNelements(...) || nelements <= 0)
    ///     recGblSetSevr(prec, LINK_ALARM, INVALID_ALARM);   /* nothing ingested */
    /// else { ... dbGetLink(&prec->inp, DBF_DOUBLE, prec->wptr, ...) ... }
    /// ```
    ///
    /// The port does not read links from inside a record: the framework's
    /// soft-input stage performs the `dbGetLink` and delivers the value through
    /// [`Record::set_val`], which is why THAT is the ingest owner here. The
    /// per-cycle gates are therefore reset in `pre_input_link_actions` — the
    /// only hook that runs BEFORE that input stage. Resetting them in
    /// `pre_process_actions`, which runs after it, would clear the very flags
    /// the ingest had just set.
    ///
    /// compress emits no link action of its own. It used to emit a `ReadDbLink`
    /// reading a link string off a `CompressRecord::inp` field the loader never
    /// populates (a `.db`'s `field(INP,...)` lands in `common.inp`, since
    /// `COMPRESS_FIELDS` declares no INP, matching `compressRecord.dbd.pod`).
    /// That field was a second, always-empty INP: the action never fired for a
    /// loaded record, and a `caget CMP.INP` answered `""` where softIoc answers
    /// the link text. It is gone; INP has one source.
    fn pre_input_link_actions(&mut self) -> Vec<crate::server::record::ProcessAction> {
        // The per-cycle ingest/emit facts, cleared before the framework's input
        // stage can set them. If no value arrives (link not connected, or a
        // failed read), both stay false: `check_alarms` raises LINK/INVALID and
        // `process()` publishes — mirroring C, which forces `status = 0` on
        // those paths.
        self.cycle_ingested = false;
        self.cycle_emitted = false;
        Vec::new()
    }

    /// The framework's soft-input stage delivering this cycle's INP value —
    /// C's `dbGetLink(&prec->inp, DBF_DOUBLE, prec->wptr, 0, &nelements)`
    /// inside `process()` (:341).
    ///
    /// It is the ONLY INP-driven ingest, which is what `inp_read_pending`
    /// records: C's element-count reset (`nelements != prec->inpn` frees WPTR
    /// and `reset()`s, :334-340) lives in `process` and keys on WPTR, the INP
    /// read buffer — a CA put to VAL reaches `put_field` directly, never
    /// `set_val`, and must NOT reset. Marking the ingest here instead of in
    /// `pre_process_actions` is what makes the mark fire for a LOADED record:
    /// the old mark rode on the dead `ReadDbLink` above.
    fn set_val(&mut self, value: EpicsValue) -> CaResult<()> {
        self.inp_read_pending = true;
        self.put_field_internal("VAL", value)
    }

    /// C `compressRecord.c:328-343` — the record's only alarm. A cycle that
    /// took NO sample from INP latches LINK/INVALID, and C has three ways to
    /// take none: the link is unset, the link is CONSTANT (a literal has no
    /// lset, so `dbIsLinkConnected` is FALSE and C refuses to sample it), or
    /// the `dbGetLink` failed. All three land here as "nothing was ingested
    /// this cycle" — `cycle_ingested`, set by the `push_value`/`push_array`
    /// ingest owner and cleared by `pre_process_actions` — so ONE test covers
    /// them, and it cannot disagree with what the buffer actually took.
    ///
    /// Without it, an unset or dead INP left the record in NO_ALARM forever and
    /// operators lost the "my compression source is dead" signal entirely.
    fn check_alarms(&mut self, common: &mut crate::server::record::CommonFields) {
        if !self.cycle_ingested {
            crate::server::recgbl::rec_gbl_set_sevr(
                common,
                crate::server::recgbl::alarm_status::LINK_ALARM,
                crate::server::record::AlarmSeverity::Invalid,
            );
        }
    }
}

#[cfg(test)]
mod pbuf_tests {
    use super::*;

    // C `compressRecord.dbd.pod` `field(NSAM,DBF_ULONG){ initial("1") }` — a
    // compress built without an explicit NSAM defaults to a 1-sample buffer,
    // not the old hand-coded 10. VAL is the NSAM-length buffer.
    #[test]
    fn nsam_defaults_to_one_per_dbd_initial() {
        let rec = CompressRecord::default();
        assert_eq!(rec.nsam, 1);
        assert_eq!(rec.val.len(), 1, "VAL is the NSAM-length buffer");
        // NSAM is C's DBF_ULONG (compressRecord.dbd.pod:424).
        assert_eq!(rec.get_field("NSAM"), Some(EpicsValue::ULong(1)));
    }

    #[test]
    fn alg_defaults_to_zero_per_absent_dbd_initial() {
        // C `field(ALG,DBF_MENU){ menu(compressALG) }` has no `initial(...)`,
        // so an unset ALG is menu index 0 = N-to-1 Low Value, not Circular
        // Buffer (4).
        let rec = CompressRecord::default();
        assert_eq!(rec.alg, 0, "ALG has no C initial -> menu index 0");
    }

    /// `process_local` (the test-only record-body dispatch path) must honor the
    /// same emit-gate as the production engine path: a compress that ingested
    /// but did not emit (`CompleteNoEmit`, C `compressRecord.c:365` the
    /// `if (status != 1)` epilogue gate) publishes nothing. Construct the
    /// non-emit state by ingesting one sample of a 4-wide rolling Average, then
    /// dispatch via `process_local`. Without the `CompleteNoEmit` early-return
    /// in `process_local` this falls through to the publication epilogue and
    /// posts the buffer — the regression this pins.
    #[test]
    fn process_local_suppresses_publication_on_non_emit_cycle() {
        use crate::server::record::RecordInstance;

        // Rolling Average (ALG 3), N=4: one sample ingested, no emit yet.
        let mut cmp = CompressRecord::new(1, 3);
        cmp.n = 4;
        cmp.push_value(1.0);
        assert!(
            cmp.cycle_ingested && !cmp.cycle_emitted,
            "1/4 samples → accumulating (CompleteNoEmit), not emitting"
        );

        let mut inst = RecordInstance::new("CMP".into(), cmp);
        let (snap, actions) = inst.process_local().unwrap();

        assert!(
            snap.changed_fields.is_empty(),
            "a non-emit (CompleteNoEmit) cycle must publish no field changes via \
             process_local — got {:?}",
            snap.changed_fields
        );
        assert!(actions.is_empty(), "compress is soft → no process actions");
    }

    /// C compressRecord.c:334-340: a change in the INP source's element count
    /// between cycles must reset the accumulation buffer and restart clean.
    /// The rolling Average (ALG=3) is the victim — without the reset it keeps
    /// stale per-element partial sums from the old length and blends them with
    /// the new waveform. Feed a 4-element waveform mid-window, switch to 2
    /// elements; the emitted average must reflect only the post-resize data.
    #[test]
    fn average_resets_on_inp_element_count_change() {
        let mut rec = CompressRecord::new(4, 3); // NSAM=4, ALG=3 (Average)
        rec.n = 2; // average over 2 cycles

        // `inp_read_pending` marks each push as the INP-driven ingest (what
        // `pre_process_actions` sets in the real process path); only those may
        // trigger the element-count reset.
        // Cycle 1: length 4, mid-window (inx 0 -> 1, no emit).
        rec.inp_read_pending = true;
        rec.push_array(&[10.0, 20.0, 30.0, 40.0]);
        // Cycle 2: length CHANGES to 2 -> reset + restart (inx 0 -> 1, no emit).
        rec.inp_read_pending = true;
        rec.push_array(&[2.0, 2.0]);
        // Cycle 3: length 2 again -> inx 1 -> 2 -> emit mean([2,2],[4,4])=[3,3].
        rec.inp_read_pending = true;
        rec.push_array(&[4.0, 4.0]);

        // Clean restart: the average is the mean of the two post-resize
        // 2-element waveforms, NOT a blend with the stale length-4 cycle-1 data
        // (which would yield [6, 11] emitted a cycle early).
        assert_eq!(
            rec.val[0], 3.0,
            "element 0 must be mean(2,4)=3 from the clean post-resize window, \
             not a blend with the stale length-4 partial sum"
        );
        assert_eq!(rec.val[1], 3.0, "element 1 must be mean(2,4)=3");
    }

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

    /// C `compressRecord` reads its INP via `dbGetLink(&prec->inp,
    /// DBF_DOUBLE, …)` (compressRecord.c:342), so a linked `DBF_LONG`
    /// source is converted to double before the record sees it. The
    /// input-link delivery owner `put_field_internal` must coerce a
    /// `Long`/`LongArray` to the Double-only VAL arm; before the fix it
    /// reached the typed `put_field` as `Long`, returned `TypeMismatch`,
    /// and the caller discarded it so the buffer never advanced.
    #[test]
    fn input_link_coerces_long_source_into_double_buffer() {
        let mut rec = CompressRecord::new(10, 4); // circular buffer NSAM=10

        // Scalar Long delivery (e.g. INP from a longin/calc VAL).
        rec.put_field_internal("VAL", EpicsValue::Long(42)).unwrap();
        // Array Long delivery (e.g. INP from a waveform FTVL=LONG).
        rec.put_field_internal("VAL", EpicsValue::LongArray(vec![10, 20, 30]))
            .unwrap();

        match rec.get_field("VAL").unwrap() {
            EpicsValue::DoubleArray(v) => assert_eq!(v, vec![42.0, 10.0, 20.0, 30.0]),
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

    /// N-to-1 Median (alg=5) array path — C `compress_array`
    /// (compressRecord.c:209-211): sort each N-element chunk and emit the
    /// middle element `psource[n/2]`. This pins the algorithm the struct
    /// doc once wrongly described as "not implemented; falls through to
    /// 0.0" — it has always been the `_ =>` arm of `compress_array`.
    #[test]
    fn n_to_1_median_array_takes_sorted_middle_of_each_chunk() {
        // ALG=5, N=3 (odd): unambiguous middle of each chunk.
        let mut rec = CompressRecord::new(8, 5);
        rec.n = 3;
        // [3,1,2] -> sorted [1,2,3] -> middle 2; [6,5,4] -> [4,5,6] -> 5.
        rec.push_array(&[3.0, 1.0, 2.0, 6.0, 5.0, 4.0]);
        if let Some(EpicsValue::DoubleArray(v)) = rec.get_field("VAL") {
            assert_eq!(v, vec![2.0, 5.0]);
        } else {
            panic!("expected DoubleArray");
        }

        // Even N: C takes `psource[n/2]` = the UPPER of the two middles
        // (index 2 of 4), NOT their average. [10,40,30,20] -> sorted
        // [10,20,30,40] -> index 2 -> 30 (not the interpolated 25).
        let mut rec = CompressRecord::new(8, 5);
        rec.n = 4;
        rec.push_array(&[10.0, 40.0, 30.0, 20.0]);
        if let Some(EpicsValue::DoubleArray(v)) = rec.get_field("VAL") {
            assert_eq!(
                v,
                vec![30.0],
                "even-N median is the upper-middle (C psource[n/2]), not a mean"
            );
        } else {
            panic!("expected DoubleArray");
        }
    }
}
