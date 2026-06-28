# std-rs C-parity review — round 2 — 2026-06-28

Codex-methodology parity sweep of `crates/std-rs` against the synApps `std`
module C reference at `/Users/stevek/codes/epics-modules/std/stdApp/src`.

Single-reviewer sweep (caucus panels were unavailable this session). Every
"matches C XXX:YY" claim in the Rust source was opened against the cited C
line — call-graph followed, negative space checked, no green test trusted on
its comment alone.

This is the **second** std round. The first
(`doc/c-parity-review-2026-06-15.md`, workspace-level) produced STD-1..STD-8,
covering the epid record (OUTL gating, MLST/ALST deadband), the timestamp
record (subsec round, VAL post), and time_of_day device support (TSE source,
`<undefined>` sentinel) — all Fixed. Round 1 did **not** line-audit the
throttle record, the epid callback/fast device supports, or the SNL programs;
it listed them only under a one-line "verified-equivalent (std)" claim
(round-1 doc line 553). This round opens those surfaces directly. STD-N
numbering continues from STD-9.

## Surface audited (this round)

| Category | Rust | C reference | Lines (Rust / C) |
|----------|------|-------------|------------------|
| A — throttle record | `src/records/throttle.rs` | `throttleRecord.c` | 823 / 753 |
| B — epid soft-callback dev | `src/device_support/epid_soft_callback.rs` | `devEpidSoftCallback.c` | 125 / 263 |
| C — epid fast dev | `src/device_support/epid_fast.rs` | `devEpidFast.c` | 400 / 483 |
| D — SNL delay/femto | `src/snl/delay_do.rs`, `src/snl/femto.rs` | `delayDo.st`, `femto.st` | 188+155 / ~5k+8k |

## Prior findings — do NOT re-open

From `doc/c-parity-review-2026-06-15.md`:

| # | Finding | Disposition |
|---|---------|-------------|
| STD-1/2/3 | epid OUTL-write gating (FBON / sub-MDT / CONSTANT-INP) | **Fixed** (818148c7) |
| STD-4 | epid MLST/ALST double-advance vs deadband | **Fixed** (1e79cc76) |
| STD-5 | timestamp `.%03f` round vs truncate | **Fixed** (4adf6ec8) |
| STD-6 | timestamp VAL posts every cycle | **Fixed** (4e3d4990) |
| STD-7/8 | time_of_day TSE source + `<undefined>` sentinel | **Fixed** (c079c35e, user: Match C) |

The STD-7/8 disposition is the precedent for STD-9 below: a deviation that was
documented and (for STD-8) tested as intentional, re-dispositioned to "Match C"
on the user's call. STD-9 is the same shape and is surfaced the same way.

## Open findings (this round)

### STD-9 — throttle WAIT held for the whole delay; C clears it after the immediate send — CONCERN — **FIXED 67cd226d (user: Match C)**

Round 1 listed throttle "WAIT" under "verified-equivalent (std)" (round-1 doc
line 553). A line-by-line re-read shows it is **not** equivalent.

- **C semantic — WAIT means "a value is entered but not yet written to OUT".**
  `process()` sets `prec->wait = TRUE` when a fresh value is accepted
  (`throttleRecord.c:287`). `valuePut()` clears `prec->wait = FALSE`
  immediately after the OUT write (`:575` non-CONSTANT, `:587` CONSTANT) and
  *then* re-arms the delay timer (`:592-593`). So:
  - Immediate send (no delay in progress): WAIT is TRUE for the duration of
    `process()` then **FALSE** for the whole post-send cooldown.
  - A value queued *during* a delay: `enterValue()` sets `wait_flag` but does
    NOT call `valuePut()` (delay in progress, `:525`), so `process()` leaves
    `prec->wait = TRUE` — WAIT is TRUE only while a value is backed up.
  - Drain (timer fires → `valuePut()`): WAIT cleared **FALSE** after the drain
    write (`:575`).
  Net: C's WAIT ⟺ "an un-written value is pending".

- **Rust semantic — WAIT ⟺ `delay_active` (a delay timer is running).**
  `throttle.rs:624` sets `wait = 1` after an immediate send with `DLY > 0`,
  `:538` sets `wait = 1` after a drain re-arm, `:593` sets `wait = 1` while a
  value is queued. So WAIT stays TRUE for the entire delay window even when
  nothing is queued — diverging from C in two states: the post-immediate-send
  cooldown and the post-drain cooldown.

- **This is a deliberate, documented, tested deviation, not an accidental
  defect.** The source comment `throttle.rs:617-621` explicitly notes C clears
  WAIT in `valuePut` but chooses to hold it ("the operator-visible post-cycle
  state is Busy until the drain completes"); `throttle_tests.rs:115,282,748`
  assert `wait == 1` during the cooldown.

- **Impact:** a CA client monitoring WAIT sees Rust report Busy for the whole
  throttle period; C reports Busy only when a value is actually backed up
  waiting for the next drain. `self.wait` is report-only (written at
  `:522/538/593/624/633`, read once at `:716` for the WAIT field) — no control
  flow depends on it, so the choice affects only the observable WAIT field.

- **Disposition: Match C — FIXED 67cd226d.** User chose Match C (same as the
  STD-7/8 precedent). `wait = 0` at `:624` (immediate send) and `:538` (drain
  re-arm), `wait = 1` kept at `:593` (value queued), so the invariant is now
  uniform: WAIT=1 ⟺ `pending_value.is_some()`. The `:617-621`/`:583-590`
  comments were rewritten and four asserts updated (`throttle_tests.rs`
  test_process_sends_value_with_delay / test_process_queues_during_delay /
  the clip-on drain test / test_dly_huge_finite…process, plus
  `integration_tests.rs` test_throttle_delayed_reprocess) — each immediate-send
  assert flipped to WAIT=0 with a positive WAIT=1 queued-state check added.
  std-rs nextest 183/183.

### STD-10 — throttle SYNC requested during a queued value is read immediately, not deferred to the drain — NOTE (keep-Rust, end-state equivalent)

- C `valueSync()` sets `sync_flag = 1` then `if (wait_flag) return;`
  (`throttleRecord.c:625`) — when a value is queued for the delay timer
  (`wait_flag` still set, the only cross-call state where it persists), the
  SINP→VAL read is **deferred**. The pending `valuePut()`, on a successful OUT
  write, re-invokes `valueSync()` (`:569-570`, after clearing `wait_flag` at
  `:554`), so the SINP read lands *after* the queued value is sent.
- Rust `special("SYNC")` → `spawn_value_sync()` (`throttle.rs:700-704`) reads
  SINP→VAL immediately regardless of `delay_active`/`pending_value`; the drain
  path (`:523-542`) sends `pending_value`, never re-touching VAL.
- **End state is identical** in the divergent case (SYNC during a queued
  value): OUT receives the queued value and VAL ends at SINP under both. Only
  the *intermediate* VAL monitor sequence and the SYNC→IDLE timing differ (C:
  SINP read at drain time; Rust: at SYNC-write time). The two non-queued cases
  (idle, or cooldown with nothing queued — `wait_flag` already clear in C)
  match exactly. The Rust separate-`pending_value` design removes the need for
  C's `sync_flag`/`wait_flag` deferral coupling. Faithful; no fix.

### STD-11 — epid_fast DT sourced from the last PID compute, not from `time_per_point_actual` directly — NOTE (low)

- C `update_params()` writes `pepid->dt = pPvt->timePerPointActual`
  unconditionally on every record process (`devEpidFast.c:330`), and seeds
  `callbackInterval` at init via `pfloat64Input->read(...)`
  (`devEpidFast.c:254-256`).
- Rust `update_record_from_params()` copies `epid.dt = pvt.dt`
  (`epid_fast.rs:528`), and `pvt.dt` is set only inside `do_pid`
  (`:182`, `self.dt = self.time_per_point_actual`); `callback_interval` starts
  at `0.0` and is set only by `interval_callback`.
- **Impact:** before the first data callback fires, the record's DT reads `0`
  in Rust where C would already show `timePerPointActual`. Self-corrects after
  the first callback. Entirely within the asyn-driver-wired fast path (needs a
  real/sim asyn Float64 port to exercise, like the unported scaler hardware
  driver in SCAL-9).
- **STD-11a — FIXED a6dc15f0:** `epid.dt` now sourced from
  `pvt.time_per_point_actual` in `update_record_from_params`, matching
  `update_params:330` directly (closes the post-interval / pre-data-callback
  window). **STD-11b (hardware-path, deferred):** seed `callback_interval`
  from the asyn input port at init like `devEpidFast.c:254-256`; requires the
  asyn driver wiring and is untestable without it, and it shadows STD-11a in
  the pre-first-interval-callback window (`time_per_point_actual` is itself 0
  until `callback_interval` is seeded).

### STD-12 — SNL programs ported as native Rust FSMs (intentional redesign) — DESIGN

- `delayDo.st` (9-state SNL sequencer) is ported as an explicit Rust state
  machine `DelayDoController` (`delay_do.rs`). The state set and transition
  table are faithful — init/disable/maybeStandby/idle/standby/maybeWait/active/
  waiting/action, the `waiting`-state `when`-clause priority
  (enable→disable, standby→standby+resumeWaiting, active→active,
  delay→action), and the `action`→`idle` `PVPUT(doSeq,1)` all match.
- The one approximation: SNL **event-flag accumulation** (`efTest(active_mon)`
  vs `efTestAndClear`, where `active_mon` accumulates any `active` monitor
  event during `standby`) is modeled with the `active_seen` flag, set only on
  `active_changed && active` (`delay_do.rs:156-158`). C's `active_mon` is set
  on *any* `active` monitor event; the Rust narrows it to active-going-high.
  Could differ on a rapid active toggle during standby. Obscure edge in an
  intentional sequencer→FSM redesign — DESIGN, not a record-parity defect.
- `femto.st` (femtowatt amplifier control SNL, ~8k) ↔ `femto.rs` was **not**
  deep-audited this round (same redesign class; deferred to a future SNL-focused
  pass).

## Verified-equivalent (this round)

- **epid_soft_callback.rs ↔ devEpidSoftCallback.c** — the TRIG/PACT
  re-architecture is faithful: a CA TRIG link fires the trigger,
  `set_ca_trig_pending()` + `async_pending()` skip the process tail and defer
  the PID compute to the post-callback re-process (C `pact=TRUE; return(0)`,
  `:143-145`); a DB TRIG link emits the synchronous trigger write via
  `pre_input_link_actions` *before* the INP fetch then runs PID in the same
  pass (C `dbPutLink` then fall-through, `:121-152`); a CONSTANT/empty link
  runs PID immediately. The PID body is shared with `EpidSoftDeviceSupport::
  do_pid` (round-1-verified term-by-term).
- **epid_fast.rs PID body + averaging ↔ devEpidFast.c do_PID + dataCallback** —
  `compute_num_average` (`0.5 + tpp/interval`, clamp ≥1, `tppActual = n*interval`),
  the count/accumulate averaging split (Rust `accumulated:f64` sum + `count:u32`
  counter ↔ C `averageStore` + `accumulated`), `dt = callbackInterval`, the
  integral sanity clamps, the bumpless OFF→ON output-port read, and the
  correct omission of the FMOD/MaxMin switch (the Fast support has no `fmod`
  field, unlike the Soft supports) all match. Only the DT-source transient
  (STD-11) and the init-interval seed (STD-11b) diverge.

## Review log

### Round 2 — 2026-06-28 (single-reviewer sweep)

Opened the throttle record (`process`/`enterValue`/`valuePut`/`valueSync`/
`special`), the epid callback + fast device supports, and the delayDo SNL FSM
directly against the C reference, following the call graph and checking negative
space. Result: **one CONCERN surfaced for sign-off** (STD-9, throttle WAIT
semantic — a documented+tested deviation that round 1 had marked
verified-equivalent), **two NOTEs** (STD-10 throttle SYNC deferral,
end-state-equivalent; STD-11 epid_fast DT-source transient on the
asyn-hardware path), and **one DESIGN** (STD-12 SNL sequencer→FSM redesign).
The epid_soft_callback TRIG/PACT path and the epid_fast PID/averaging body are
verified-equivalent. STD-9 was surfaced (not silently fixed) — user chose
Match C, fixed at 67cd226d. STD-11a fixed at a6dc15f0. STD-10 (keep-Rust),
STD-11b (hardware-deferred) and STD-12 (SNL redesign DESIGN; femto deferred)
remain as documented dispositions. Round-2 converged: one CONCERN fixed to
C, one NOTE fixed, the rest dispositioned.
