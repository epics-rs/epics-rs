# Parity Review 06 — Analog / calc records

Scope: `ai.rs ao.rs longin.rs longout.rs int64in.rs int64out.rs calc.rs
calcout.rs scalcout.rs transform.rs` against epics-base `std/rec/*.c`.

Architecture note: alarm checking (`checkAlarms`), monitor posting,
MDEL/ADEL deadband filtering, IVOA dispatch and `recGblResetAlarms` are
**not** in the per-record `process()` — they are centralised in
`server/record/record_instance.rs::process_local` /
`server/database/processing.rs`. Findings below account for that split.

`scalcout.rs` / `transform.rs` are synApps `calc`-module records; epics-base
ships no C equivalent, so they are reviewed only for internal correctness.

---

## HIGH

### H1 — longout / int64out never clamp VAL to DRVH/DRVL
- Rust: `longout.rs` (whole file — no `process()` override; uses default
  no-op `Record::process` at `record_trait.rs:164`). `int64out.rs` is a
  `#[derive(EpicsRecord)]` struct, also no `process()`.
- C: `longoutRecord.c::convert` lines 436-441; `int64outRecord.c::convert`
  lines 418-423: `if (drvh > drvl) { if (value > drvh) value = drvh;
  else if (value < drvl) value = drvl; }`.
- Diverges: C clamps the output VAL to the drive-limit window every
  process cycle. The Rust records expose `DRVH`/`DRVL` fields, but they
  are consumed only as `ctrl_limit` metadata (`record_instance.rs:472-487`);
  no clamp is applied anywhere in the framework or the record.
- Impact: an operator (or DOL link) writing a value outside `[DRVL,DRVH]`
  has it propagated unclamped to the OUT link / downstream record. The
  drive limit is silently ineffective — hardware can be commanded past
  its safe range. Wrong value driven to output.

### H2 — ai / calc / input records: UDF cleared unconditionally, killing NaN→UDF alarm
- Rust: `record_instance.rs:1566-1568` — `if self.record.clears_udf() {
  self.common.udf = false; }`. `clears_udf()` defaults to `true`
  (`record_trait.rs:367`) and is **not** overridden by `ai/ao/calc/calcout/
  longin/int64in/int64out`. Also `processing.rs:839-841` does the same on
  the linked path.
- C: `aiRecord.c::process:161` `if (status == 0) prec->udf = isnan(prec->val);`
  `calcRecord.c::process:124` `prec->udf = isnan(prec->val);`
  `calcoutRecord.c::process:241` same; `longinRecord.c:148` /
  `int64inRecord.c:144` clear UDF **only** `if (status==0)`.
- Diverges: C keeps `UDF` true when the computed/read value is NaN (calc
  `0/0`, `sqrt(-1)`, `log(0)`, failed input-link read). `checkAlarms`
  then raises `UDF_ALARM` at severity `UDFS`. The Rust framework clears
  UDF every successful cycle regardless of whether VAL is NaN or whether
  the input read succeeded.
- Impact: a calc record whose expression evaluates to NaN, or an input
  record whose link read failed, reports `NO_ALARM` instead of
  `UDF_ALARM`. Missed alarm; operators see a stale/garbage value with no
  invalid indication.

---

## MEDIUM

### M1 — calcout has no ODLY / DLYA output delay
- Rust: `calcout.rs` — no `ODLY`, `DLYA` fields; `process()` evaluates
  OCAL and returns `complete()` synchronously.
- C: `calcoutRecord.c::process:276-288` — when `doOutput` and
  `odly > 0.0`, sets `dlya=1`, posts it, schedules
  `callbackRequestProcessCallbackDelayed(..., odly)` and returns 0
  (async); the output is written on the delayed re-process.
- Diverges: the calcout output-delay feature is entirely absent.
- Impact: any calcout database that relies on `ODLY` to stagger or
  debounce its OUT-link write fires immediately. Feature gap; timing
  behaviour wrong for delayed-output configurations.

### M2 — calcout OEVT / event posting on output missing
- Rust: `calcout.rs` — no `OEVT`/`epvt`; output never posts an event.
- C: `calcoutRecord.c::execOutput:637,642,650` — `if (prec->epvt)
  postEvent(prec->epvt);` after every successful `writeValue`.
- Impact: event-record chains triggered by a calcout's `OEVT` never
  fire. Feature gap.

### M3 — ao IVOA=2 does not re-convert RVAL from IVOV
- Rust: `ao.rs::apply_invalid_output_value` (lines 311-314) sets
  `OVAL = ivov` and `VAL = ivov`, but does not re-run `convert()`, so
  `RVAL` keeps its pre-IVOA value. Invoked from `processing.rs:872-873`.
- C: `aoRecord.c::process:207-213` (IVOA = `Set_output_to_IVOV`):
  `prec->val=prec->ivov; value=prec->ivov; convert(prec,value);` — the
  full convert runs, so `RVAL` reflects the converted IVOV.
- Impact: harmless for the current soft-only ao path (the soft OUT
  writeback sends `OVAL`, which is correct). Becomes a wrong-value bug
  the moment ao gets hardware device support that writes `RVAL`. Edge
  case today, latent.

### M4 — ao OROC / OVAL: no `omod` flag, OVAL/RVAL archive monitors not forced
- Rust: `ao.rs` has no `omod` field; `process()` computes OROC and sets
  `oval`. Monitor posting for `OVAL`/`RVAL` relies on the generic
  subscribed-field diff (`record_instance.rs:1613-1633`).
- C: `aoRecord.c::convert:482` sets `prec->omod = (prec->oval!=value)`;
  `monitor:535-548` ORs `DBE_VALUE|DBE_LOG` into the mask when `omod`,
  and posts `RVAL` with `monitor_mask|DBE_VALUE|DBE_LOG` whenever
  `oraw != rval`.
- Diverges: the Rust path posts `OVAL`/`RVAL` only if subscribed and the
  generic diff fires, and never sets the `DBE_ARCHIVE/DBE_LOG` bit that C
  forces for these fields.
- Impact: archive (`DBE_LOG`) clients miss OVAL/RVAL updates that C would
  have logged; a CA monitor on OVAL with `DBE_LOG`-only mask sees nothing.
  Missed monitor on the archive channel.

### M5 — calc / calcout LA..LU updated unconditionally in process(), not on post
- Rust: `calc.rs::process:695-715` and `calcout.rs::process:685-705` copy
  `A..U → LA..LU` unconditionally at the end of every `process()`.
- C: `calcRecord.c::monitor:417-423` / `calcoutRecord.c::monitor:679-685`
  update `*pprev = *pnew` **only** inside the per-field change test
  `if (*pnew != *pprev || monitor_mask & DBE_ALARM)`, i.e. only for
  fields that actually changed (or when an alarm-change forces a repost).
- Diverges: in C, `LA..LU` is "value of input as of the last time a
  monitor was posted for it". In Rust it is "value of input as of the
  last process". When a calc record processes but the framework filters
  the input-field monitor (no subscriber / not changed enough), C still
  has the old `LA`, Rust has already advanced it.
- Impact: `LA..LU` reads diverge from C. Any CALC expression that
  references `LA..LU` to detect "did input change since last posted
  value" computes a different result. Wrong derived value for
  change-detection expressions.

---

## LOW

### L1 — calc AFVL not cleared when AFTC disabled / on UDF
- Rust: `record_instance.rs:1431-1435` — when `aftc <= 0` the AFTC branch
  explicitly leaves `AFVL` untouched. There is no `AFVL = 0` on the UDF
  path.
- C: `calcRecord.c::checkAlarms:302` sets `prec->afvl = 0` on UDF and
  `:337,382` assigns `prec->afvl = afvl` (which is 0 when `aftc <= 0`),
  i.e. C always drives AFVL to 0 when the filter is inactive.
- Impact: after AFTC is set back to 0 (or while UDF), a stale non-zero
  AFVL persists. If AFTC is later re-enabled, the filter resumes from a
  stale accumulator instead of re-seeding. Minor; only affects runtime
  AFTC retuning.

### L2 — ai SMOO filter: `aslo == 0` silently skips ASLO scaling (matches C, but no validation)
- Rust: `ai.rs::process:255-258` `if self.aslo != 0.0 { v *= self.aslo; }`
  — matches C `aiRecord.c::convert:420` exactly. Noted only because a
  zero ASLO is treated as "no scaling" rather than "scale by 0"; both C
  and Rust agree, so this is **not** a divergence. No action.

### L3 — ai/ao breakpoint-table linearization (LINR >= 3) unimplemented
- Rust: `ai.rs::process:267` / `ao.rs::process:398` — `_ => {}` ("breakpoint
  tables not yet supported"); the value passes through with no conversion.
- C: `aiRecord.c::convert:433-436` / `aoRecord.c::convert:494-499` call
  `cvtRawToEngBpt`/`cvtEngToRawBpt` and raise `SOFT_ALARM/MAJOR_ALARM` on
  failure.
- Impact: any ai/ao record configured with a breakpoint table (`LINR`
  naming a `.dbd` breakpoint menu choice) silently produces the raw,
  unconverted value and never raises the BPT error alarm. Feature gap.

### L4 — calcout `should_output` On-Change uses MDEL — correct, but first cycle differs from longout
- Rust: `calcout.rs::should_output` OOPT=1 → `(pval-val).abs() > mdel`;
  `init_record` seeds `pval = val`, so the first process with OOPT=1
  produces no output. This **matches** `calcoutRecord.c` (calcout has no
  first-cycle force-emit, unlike `longoutRecord.c`'s `outpvt`). Noted to
  contrast with longout, which *does* force the first cycle (handled
  correctly in `longout.rs::compute_should_output`). No action.

---

## scalcout.rs / transform.rs — internal-correctness notes (no C reference)

### S1 (Medium) — scalcout: invalid CALC silently keeps stale VAL
- `scalcout.rs::process:407-422` — when `scalc_eval` errors, IVOA=0
  ("Continue") falls through with `{}` leaving `val`/`sval` at the
  previous cycle's value, and no alarm is raised. There is no
  `CALC_ALARM`/`calc_alarm` field for scalcout, so a broken expression is
  invisible. synApps `sCalcoutRecord` raises `CALC_ALARM`. Internal
  correctness gap: a failing scalcout expression produces a stale value
  with no alarm.

### S2 (Medium) — scalcout: OCAL eval error swallowed
- `scalcout.rs::process:441` — `Err(_) => {}`. An OCAL evaluation failure
  is discarded; `oval`/`osv` keep stale values and no alarm is set
  (calcout.rs at least sets `calc_alarm = true` here). Inconsistent with
  the sibling calcout record and with synApps.

### S3 (Low) — scalcout: `WAIT`/`OUT` fields present but unused
- `scalcout.rs` stores `wait` and `out` but `process()` never consults
  them; the framework's `multi_output_links()` is not implemented for
  scalcout (only `multi_input_links`). The string OUT side (`OSV`/`OUT`)
  is never written anywhere. Feature gap.

### S4 (Medium) — transform: COPT=Conditional not enforced; OUT links written for empty calcs
- `transform.rs::multi_output_links:615-627` — for both `COPT=0`
  (Conditional) and `COPT=1` (Always) it returns the full 16-entry `ALL`
  slice. The doc comment claims `process()` clears the OUTx link for
  channels without a calc, but `process()` (lines 449-474) does **no
  such thing**. Result: under COPT=Conditional a channel with an empty
  CLCx still has its OUTx link written with the (unchanged) channel
  value. Diverges from synApps transform semantics where Conditional
  writes only channels whose CLCx is non-empty.

### S5 (Low) — transform: every CLCx re-evaluated even for directly-written channels
- `transform.rs::process:454-472` evaluates all 16 `CLCx` expressions
  every cycle. synApps `transformRecord` skips re-computing a channel
  whose value field (`A..P`) was just written by a `dbPut` this cycle
  (the "don't overwrite a fresh put" rule). The Rust port has no
  put-tracking, so a CA put to `transform.A` is immediately overwritten
  by `CLCA` on the next process. Behavioural gap vs synApps.

### S6 (Low) — transform: no IVLA per-channel scope
- `transform.rs::process:463-468` — IVLA=1 ("Do Nothing") on *any*
  channel's eval error restores **all** channels and aborts the whole
  process. synApps applies the no-op per failing channel. Coarser than
  the original. Minor.

---

## Severity counts
- Critical: 0
- High: 2  (H1, H2)
- Medium: 6  (M1, M2, M3, M4, M5, S1+S2+S4 are scalcout/transform-internal Medium)
- Low: 6  (L1, L3, L4 informational, S3, S5, S6)

(Counting epics-base-comparable findings: High 2, Medium 5, Low 2.
 synApps internal findings: Medium 3, Low 3.)
