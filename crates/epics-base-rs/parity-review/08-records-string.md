# Parity Review 08 — String / Array / Subroutine / Sequence Records

Scope: `stringin.rs stringout.rs lsi.rs lso.rs waveform.rs compress.rs histogram.rs
printf.rs sub_record.rs asub_record.rs seq.rs sseq.rs swait.rs asyn_record.rs`
against `epics-base/modules/database/src/std/rec/*.c`.

`sseq.rs`, `swait.rs` (synApps `calc` module) and `asyn_record.rs` (asyn module)
have **no epics-base C equivalent** — reviewed for internal correctness only.

Rust dir: `crates/epics-base-rs/src/server/records/`
C dir: `epics-base/modules/database/src/std/rec/`

---

## CRITICAL

### C-1. compress: `put_field("VAL", DoubleArray)` overwrites the circular buffer, desyncs `nuse`/`off`
- Rust: `compress.rs:407-411`
- C: `compressRecord.c` — there is **no** writable VAL path; VAL is the circular
  buffer and clients read it via `get_array_info`. A CA put to VAL of a
  compress record is rejected/handled by put_array_info, never a raw overwrite.
- Divergence: `put_field("VAL", EpicsValue::DoubleArray(arr))` does
  `self.val = arr;` directly. This replaces the backing buffer **without
  touching `nuse`, `off`, or resizing to `nsam`**. A subsequent
  `linearise_val()` (compress.rs:276-295) indexes `self.val[(start+i)%nsam]`
  with `start` derived from the stale `off`/`nuse`. If the pushed array is
  shorter than `nsam`, `linearise_val` indexes out of bounds → **panic**.
  If longer, `nuse`/`off` no longer describe the buffer → garbage reads.
- Impact: panic (index out of bounds) or silent buffer corruption on any
  client write to a compress `VAL`.

### C-2. histogram: `VAL` is `i32`/Long but C uses `epicsUInt32`; overflow panics in debug
- Rust: `histogram.rs:7` (`val: Vec<i32>`), `histogram.rs:52` (`self.val[bucket] += 1`)
- C: `histogramRecord.c:304-306` `dbr_field_type = DBF_ULONG`,
  `histogramRecord.c:345-348` — `if (*pdest == UINT_MAX) *pdest = 0; (*pdest)++;`
- Divergence: C bucket counters are unsigned 32-bit and explicitly wrap at
  `UINT_MAX`. Rust uses signed `i32` with `+= 1` and no wrap handling. At
  2^31 counts the Rust add overflows: **panic in a debug build**, wrap to
  `i32::MIN` (a large negative count) in release. The CA field type is also
  wrong (Long vs ULong) so clients interpret high counts as negative.
- Impact: panic / negative counts on long-running histogram records.

---

## HIGH

### H-1. seq: record has **no process logic at all** — SELM/SELN/DLY/DOL/LNK unused
- Rust: `seq.rs` (entire file) — `#[derive(EpicsRecord)]` only generates field
  storage + accessors. There is no `process()`, no `should_execute`, no link
  group handling.
- C: `seqRecord.c:133-199` `process()` — evaluates `selm` (`seqSELM_All` /
  `Specified` / `Mask`), reads `SELN` via `SELL`, computes a link mask, then
  processes each selected link group (`DLYn`, `DOLn`, `DOVn`, `LNKn`) with
  per-group delays via callbacks.
- Divergence: the Rust `seq` record stores fields but never sequences
  anything. Processing it does nothing — no DOL reads, no LNK writes, no
  delays, no SELN selection.
- Impact: `seq` records are inert; every output link write that an EPICS
  database expects from a seq record silently never happens.

### H-2. seq: only 10 link groups (`DLY1..DLY9, DLYA`); C has 16 (`DLY0..DLYF`)
- Rust: `seq.rs:18-77` — fields `dly1..dly9, dlya`, `dol1..dola`, `lnk1..lnka`.
- C: `seqRecord.c:86` `#define NUM_LINKS 16`; groups are `DLY0..DLYF` (the
  `.dbd` declares `DLY0` through `DLYF`). `seqRecord.c:123` iterates
  `&prec->dly0` for 16 groups.
- Divergence: Rust is missing 6 link groups and uses 1-based `DLY1`/`DLYA`
  naming instead of C's 0-based `DLY0..DLYF`. A database referencing
  `seqrec.DLY0`, `seqrec.LNKB`, etc. will hit `FieldNotFound`.
- Impact: field-name mismatch + 6 missing groups; standard seq databases fail
  to load / link.

### H-3. sub_record: only 12 inputs (`A..L`); C subRecord has 21 (`A..U`)
- Rust: `sub_record.rs:23-34` (`a..l`), `:9-19` (`inpa..inpl`)
- C: `subRecord.c:89` `#define INP_ARG_MAX 21`; fields `A..U` and `INPA..INPU`.
- Divergence: Rust exposes only `A..L` / `INPA..INPL`. Inputs `M..U` and
  `INPM..INPU` (9 of each) do not exist. `fetch_values` in C reads all 21.
- Impact: any sub record using inputs beyond L fails to load those fields;
  subroutines that read M-U see nothing.

### H-4. asub_record: only 12 inputs/outputs; C aSubRecord has 21 (`A..U` / `VALA..VALU`)
- Rust: `asub_record.rs:28-52` (`a..l`, `vala..vall`), `:67-90` (`noa..nol`,
  `nova..novl`)
- C: `aSubRecord.c:103-105` — `VALA..VALU`; `aSubRecord.c:301-302`
  `fieldIndex >= indexof(VALA) && fieldIndex <= indexof(VALU)`. Inputs and
  outputs both run `A..U` (21).
- Divergence: 9 missing inputs (`M..U`), 9 missing output arrays
  (`VALM..VALU`), and the matching `NOM..NOU` / `NOVM..NOVU`,
  `INPM..INPU`, `OUTM..OUTU` size/link fields.
- Impact: aSub databases using channels past L fail to load those fields.

### H-5. asub_record: no per-channel field-type fields (FTA..FTU / FTVA..FTVU); all forced to DOUBLE
- Rust: `asub_record.rs` — every input is `f64`, every output `Vec<f64>`.
- C: `aSubRecord.c:118-120` `initFields(&prec->fta, ...)` — each channel has an
  `FTx`/`FTVx` menuFtype selecting CHAR/SHORT/LONG/FLOAT/DOUBLE/STRING/etc.,
  and `NEA`/`NEVA` track elements-used separately from `NOA`/`NOVA`.
- Divergence: Rust hard-codes DOUBLE for all channels. A non-double aSub
  channel (e.g. `FTVA=STRING`) cannot be represented; the subroutine sees
  wrong-typed data.
- Impact: aSub records with non-double channels behave incorrectly.

### H-6. printf: `%s` conversion formats the numeric value, not the link string
- Rust: `printf.rs:86` — `b's' => format!("{}", val)` where `val` is the `f64`
  in `num_vals[inp_idx]`.
- C: `printfRecord.c:282-294` — for `%s`, C reads the link with `DBR_STRING`
  (`char val[MAX_STRING_SIZE]`) and prints the **string** value. `printf.rs`
  only ever stores `f64` per input (`num_vals`), so the string content of
  `INPn` is lost entirely.
- Divergence: `%s` in FMT prints something like `0` or `3.14` instead of the
  string fetched from the input link.
- Impact: any printf record using `%s` produces wrong output. This is a core
  use case of the printf record (it exists to format strings).

### H-7. printf: no `%ls` (long string), `%c`, `*` variable width, or length modifiers
- Rust: `printf.rs:53-88` parser handles flags `-+# 0`, width, `.prec`, and
  conversions `d i u o x X e E f g G s`. It does **not** handle `h`/`hh`/`l`/
  `ll` length modifiers, `*` (variable width/precision from a link), `%c`, or
  `%ls` (long-string link read).
- C: `printfRecord.c:103-302` — handles `h`/`l` modifiers (selecting the DBR
  type read from the link), `*` (`printfRecord.c:118-140`, reads an
  `epicsInt16` from the next link), `%c`, and `%ls` (`printfRecord.c:230-281`,
  `dbGetLink(DBR_CHAR)` / `dbLoadLinkLS`).
- Divergence: `%*d`, `%ld`, `%ls`, `%c` either misparse (the `*`/`l`/`h` chars
  are not consumed and end up as literal text, or the conversion char is
  mis-detected) or silently produce wrong output.
- Impact: common printf format strings produce garbage.

### H-8. lsi/lso: SIZV minimum not enforced (C clamps to 16, max 0x7fff)
- Rust: `lsi.rs:172-177`, `lso.rs:208-213` — `self.sizv = v.max(1) as u16;`
- C: `lsiRecord.c:46-55` / `lsoRecord.c` init — `if (sizv < 16) sizv = 16;
  else if (sizv > 0x7fff) sizv = 0x7fff;`
- Divergence: Rust allows `SIZV` as low as 1 (so max string length 0) and up
  to 65535 (`u16`). C enforces `[16, 32767]`. With `SIZV=1` the Rust
  `clamped()` truncates every string to empty; with `SIZV` near `u16::MAX`
  the C field-size invariant (`dbAddr::field_size` is signed) is violated.
- Impact: out-of-range SIZV silently changes truncation behaviour vs C.

### H-9. lsi/lso: `process()` copies `oval` and recomputes `len` unconditionally; C only on change
- Rust: `lsi.rs:122-128`, `lso.rs:154-160` — `process()` always does
  `olen = len; len = clamped+1; oval = val`.
- C: `lsiRecord.c:202-224` `monitor()` — copies `oval` and bumps `olen`
  **only when `len != olen || memcmp(oval,val,len)`**, and posts
  `DBE_VALUE|DBE_LOG` on `val`/`len` only on that change. `process()` itself
  does not touch `oval`/`len` (C sets `len` in `special()` / `put_array_info`).
- Divergence: Rust `process()` mutates `olen` every cycle even when the value
  is unchanged, so `olen` after a no-op process equals the previous `len`
  rather than tracking the last *posted* length. Combined with the missing
  monitor-on-change gate, a downstream observer cannot tell whether the value
  actually changed. Also `len` should be recomputed when VAL is written
  (C `special`), which Rust does in `put_field` — but `process()` recomputing
  it again from `clamped()` can disagree if `sizv` shrank between write and
  process.
- Impact: `OLEN` reports the wrong value; monitor semantics diverge from C
  (every process looks like a change).

### H-10. stringin/stringout/lsi/lso: no 40-char (`MAX_STRING_SIZE`) truncation on the DBR_STRING path
- Rust: `stringin.rs` / `stringout.rs` use `String` (unbounded). `lsi.rs:151`/
  `lso.rs:187` `put_field("VAL")` with `EpicsValue::String(s)` stores `s`
  bounded only by `sizv`.
- C: `stringinRecord.c:117,176-178` `strncpy(..., sizeof(prec->val))` —
  `val`/`oval`/`sval` are fixed 40-byte (`MAX_STRING_SIZE`) buffers; every
  copy truncates at 40. For lsi/lso a DBR_STRING put is itself capped at 40
  by `dbConvert` before reaching the record.
- Divergence: a stringin/stringout VAL longer than 39 chars is kept whole in
  Rust; C truncates to 39 + NUL. For lsi/lso a `DBR_STRING`-typed put should
  be limited to 40 chars even when `SIZV` is larger.
- Impact: string values that C would truncate are stored/echoed at full
  length; clients expecting 40-char semantics see longer strings.

### H-11. histogram: missing CSTA / CMD start-stop semantics — counting cannot be paused
- Rust: `histogram.rs:99-107` `process()` only handles `cmd == 1` (clear).
  `histogram.rs:42-53` `add_sample` always increments.
- C: `histogramRecord.c:320-352` `add_count` returns early `if (csta == FALSE)`.
  `histogramRecord.c:245-259` `special` SPC_CALC: `cmd<=1` clear,
  `cmd==2` → `csta=TRUE` (start), `cmd==3` → `csta=FALSE` (stop). There is a
  `CSTA` field tracking the running state.
- Divergence: Rust has no `CSTA` field and only `CMD==1`→clear. `CMD` values
  2 (start) and 3 (stop) are not handled; the histogram always counts.
- Impact: clients cannot pause/resume histogram counting; `CMD=2/3` writes
  are no-ops or errors.

---

## MEDIUM

### M-1. compress: scalar N-to-1 uses an N-element accumulator instead of C's running `cvb`
- Rust: `compress.rs:128-134` (`push_value` default arm) — pushes into
  `accum` and `flush_accum`s when `accum.len() >= n`.
- C: `compressRecord.c:273-304` `compress_scalar` — keeps a single running
  scalar `cvb` and counter `inx`; Low/High update with `inx==0` reset,
  Average uses the incremental `(inx*cvb + value)/(inx+1)`. There is no
  N-element buffer for scalar input.
- Divergence: numerically the per-N results coincide for Low/High/Average,
  but C exposes `CVB` (the partial accumulator) as a readable field and the
  partial state is a scalar; Rust exposes only `accum`. The Rust `INX` field
  is also not incremented on the scalar N-to-1 path (only on alg=3) so
  `INX` reads 0 mid-accumulation, unlike C where `compress_scalar` keeps
  `prec->inx`.
- Impact: `CVB` unavailable; `INX` wrong for N-to-1 algorithms mid-cycle.

### M-2. compress: ILIL/IHIL filter applied per-sample, but C only skips a *leading* out-of-limit run
- Rust: `compress.rs:88-91, 113-114, 226-228` — `ilil_ihil_rejects` is checked
  for **every** sample and rejected samples are dropped.
- C: `compressRecord.c:163-170` — the skip loop only advances past
  **leading** out-of-limit samples (`while (out-of-limit && no_elements>0)
  {no_elements--; psource++;}`), then stops at the first in-limit sample. C
  does **not** filter out-of-limit samples in the middle of the array.
- Divergence: an array like `[5, -1, 7]` with `ILIL=0,IHIL=10`: C skips
  nothing (5 is in range) and compresses `[5,-1,7]` including the `-1`;
  Rust drops the `-1` and compresses `[5,7]`.
- Impact: different compressed values whenever an out-of-limit sample is not
  at the array head.

### M-3. histogram: bin index formula differs from C on bucket boundaries
- Rust: `histogram.rs:50-52` — `bucket = ((value-llim)/range*nelm) as usize`,
  clamped to `nelm-1`.
- C: `histogramRecord.c:340-345` — `temp = sgnl-llim; for(i=1;i<=nelm;i++)
  if(temp <= i*wdth) break; pdest = bptr + i-1;` where `wdth =
  (ulim-llim)/nelm`.
- Divergence: C uses `<=` on `i*wdth` (closed upper edge per bucket); Rust
  uses truncation of a multiply-then-divide. For a value exactly on an
  internal boundary the two pick adjacent buckets, and floating-point
  rounding of `value/range*nelm` vs `temp<=i*wdth` can disagree by one bin.
- Impact: off-by-one bin assignment for boundary values.

### M-4. histogram: SDEL (signal deadband) and MDEL/MCNT monitor throttling not implemented
- Rust: `histogram.rs:13` stores `sdel` but it is never read; no `mdel`/
  `mcnt`/`wdog` callback.
- C: `histogramRecord.c:103-148` `wdogCallback`/`wdogInit` — `SDEL>0` arms a
  periodic timer that posts VAL monitors; `histogramRecord.c:282-297`
  `monitor` posts only when `mcnt > mdel`.
- Divergence: Rust posts no SDEL-timed monitors and has no count-based
  monitor throttle.
- Impact: monitor cadence differs from C; SDEL has no effect.

### M-5. compress: `process()` ignores INP / does not pull values; C reads INP every process
- Rust: `compress.rs:366-376` `process()` only handles the RES reset; it never
  reads `inp`. Values arrive only via `push_value`/`push_array`/`put_field`.
- C: `compressRecord.c:320-362` `process()` — `dbGetLink(&prec->inp, ...)`
  fetches the input array every process and feeds it to the algorithm.
- Divergence: the Rust compress record does not self-populate from its INP
  link on process; it depends on an external caller invoking `push_*`.
- Impact: a compress record scanned periodically with an INP link will not
  acquire data on its own (depends on framework wiring not visible here).

### M-6. waveform: NELM put reallocates and **zeros** the buffer, dropping existing data; C keeps data
- Rust: `waveform.rs:342-365` `put_field("NELM")` → `reallocate_val()` which
  does `vec![0; n]` and `nord = 0`.
- C: `waveformRecord` — `NELM` is a `DCT`-only / special field; it is not
  normally writable at run time, and `init_record` allocates `bptr` once.
  Run-time NELM changes are not a supported C operation.
- Divergence: Rust treats NELM as a freely writable field that wipes VAL.
  A CA client writing NELM destroys the waveform contents.
- Impact: data loss on NELM write; C would not expose this.

### M-7. stringout: IVOA `Set_output_to_IVOV` semantics not visible / DOL closed-loop not applied
- Rust: `stringout.rs` — macro-derived; has `ivoa`, `ivov`, `omsl`, `dol`
  fields but the derived `Record` impl has no `process()` applying them.
- C: `stringoutRecord.c:138-174` — `process()` reads `DOL` when
  `omsl==closed_loop`, and on `INVALID_ALARM` applies IVOA (continue / don't
  drive / set VAL to IVOV).
- Divergence: unless the framework's generic output path replicates DOL/IVOA
  for stringout, the closed-loop fetch and IVOA-on-invalid behaviour is
  absent. (`lso.rs:145-148` *does* implement `apply_invalid_output_value`;
  `stringout.rs` has no equivalent hook.)
- Impact: stringout with `OMSL=closed_loop` or non-Continue IVOA diverges
  from C.

### M-8. lsi/lso: `LEN` initialised to 1 (empty string) but C initialises `len = 0`
- Rust: `lsi.rs:28` / `lso.rs:33` default `len: 1, olen: 1`.
- C: `lsiRecord.c:58-60` — `prec->len = 0; prec->olen = 0;` after allocation.
  `len` only becomes `strlen+1` once a value is present
  (`lsiRecord.c:85-89`, `:196`).
- Divergence: a freshly created lsi/lso reports `LEN=1` (one byte, the NUL)
  in Rust vs `LEN=0` in C. `get_array_info` returns `len` as the element
  count, so CA clients see 1 element vs 0.
- Impact: element-count off-by-one for an uninitialised lsi/lso.

---

## LOW

### L-1. compress: alg=3 "Average" rolling-average emits `nuse` separate `put_one` calls
- Rust: `compress.rs:183-185` — after averaging, loops `for v in out
  { self.put_one(v); }`, so an averaged waveform of `nuse` elements becomes
  `nuse` independent circular-buffer entries.
- C: `compressRecord.c:268` `put_value(prec, prec->sptr, nuse)` — one call
  copying the whole averaged array contiguously. Result is the same buffer
  contents, but C advances `off`/`nuse` as a block; the Rust loop is
  equivalent here because `put_one` is `put_value` with n=1. Noted as a
  structural difference only — verified equivalent for FIFO/LIFO.
- Impact: none observed; cosmetic.

### L-2. printf: `format_g_val` special-cases `0.0` → `"0"`, ignoring `#` flag and precision
- Rust: `printf.rs:212-214` — `if val == 0.0 { return "0".to_string(); }`.
- C / libc `%g`: `printf("%g", 0.0)` → `"0"`, `printf("%#g",0.0)` →
  `"0.00000"`. Rust never honours `#` for `%g`.
- Impact: minor formatting divergence for `%#g` of zero.

### L-3. printf: integer/float conversions silently truncate `f64`→`i64`/`u64`
- Rust: `printf.rs:80-84` — `val as i64`, `val as u64`. A negative `f64` fed
  to `%u`/`%x`/`%o` becomes a huge `u64` via the `as` cast.
- C: `printfRecord.c` reads the link with the *unsigned* DBR type, so the
  device delivers an already-unsigned value; semantics differ for negative
  inputs but the practical outcome (two's-complement reinterpret) is similar.
- Impact: edge-case divergence for negative values with unsigned conversions.

### L-4. sub_record: `process()` is a no-op; subroutine invocation is external
- Rust: `sub_record.rs:208-210` — `process()` returns `complete()` without
  calling the subroutine. Comment in `asub_record.rs:576` says the subroutine
  is invoked via `RecordInstance::subroutine`.
- C: `subRecord.c:136-168` — `process` calls `fetch_values` then `do_sub`
  (the subroutine), then `checkAlarms`/`monitor`.
- Impact: acceptable *if* the framework drives the subroutine; flagged so a
  reviewer confirms `RecordInstance` actually does. If not, sub records never
  compute. Could not verify the framework wiring from the reviewed files.

### L-5. asyn_record: minimal stub — only CNCT/PORT/TIB2 (no C base equivalent)
- Rust: `asyn_record.rs` — three fields, no processing.
- The asyn record (asyn module) has ~100 fields. This is an intentional stub
  per the file comment; noted as a feature gap, not a correctness bug.

### L-6. sseq: `process()` performs no delays/sequencing (synApps, no C base ref)
- Rust: `sseq.rs:436-444` — `process()` sets `busy=1` then `busy=0`
  immediately; DOL reads and LNK writes are delegated to the framework via
  `pre_process_actions` / `multi_output_links`. The per-step `DLYn` delays
  and `WAITn` (Wait / AfterN) ordering are not implemented.
- synApps `sseqRecord` sequences steps with real delays and wait modes. As a
  synApps record there is no epics-base C reference; flagged as an internal
  feature gap — `DLY`/`WAIT` fields are stored but inert.

### L-7. sseq: SELM=Specified uses 1-based SELN with ad-hoc `sel==10`→index 9 mapping
- Rust: `sseq.rs:70-81` `should_execute_step` — `Specified` treats `seln` as
  1..9 selecting index `seln-1`, and `seln==10` → index 9; any other value →
  no step. No C base reference (synApps record); flagged for an internal
  review of whether SELN's intended range is 1..10 or 0..9.

### L-8. swait: `oopt` condition table is unverified against synApps semantics
- Rust: `swait.rs:69-79` `eval_should_output` — 6 OOPT cases (Every/On Change/
  When Zero/When Non-zero/Transition to Zero/Transition to Non-zero). No C
  base reference. Internal correctness only; the transition cases use
  `prev_val` captured at the start of `process()` (`swait.rs:353`), which is
  correct for an in-process comparison.

---

## Summary of counts

- Critical: 2
- High: 11
- Medium: 8
- Low: 8

The two Critical issues are memory-safety / data-integrity bugs:
`compress.rs` lets a client write `VAL` directly, replacing the circular
buffer without updating `nuse`/`off`, so `linearise_val` can index out of
bounds (panic) or read garbage; and `histogram.rs` stores bucket counts as
signed `i32` instead of C's wrapping `epicsUInt32`, so a long-running
histogram overflows (debug panic / negative counts) and reports the wrong
CA field type. Among the High findings the most consequential are that the
`seq` record has **no process logic whatsoever** (SELM/SELN/DLY/DOL/LNK are
inert, so seq records never drive their outputs) and that `printf`'s `%s`
conversion formats the numeric input value instead of the link's string —
defeating the printf record's primary purpose. The `sub`/`aSub` records
expose only 12 of the 21 C channels, and `seq` has 10 of 16 link groups with
1-based naming, so standard EPICS databases referencing the missing fields
fail to load.
