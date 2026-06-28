# scaler-rs C-parity review — 2026-06-28

Codex-methodology parity sweep of `crates/scaler-rs` against the synApps
`scaler` module C reference at
`/Users/stevek/codes/epics-modules/scaler/scalerApp/src`.

Single-reviewer sweep (caucus panels were unavailable this session). Every
"matches C XXX:YY" claim in the Rust source was opened against the cited C
line — call-graph followed, negative space checked, no green test trusted on
its comment alone.

This is the **second** scaler round; the first (workspace
`doc/c-parity-review-2026-06-15.md`) produced SCAL-1..SCAL-5. This round
re-verifies that surface and extends it to the COUT/COUTP edge timing, the
done-detection mechanism, the device-command alarm gate, and the unported
hardware driver. SCAL-N numbering continues from SCAL-6.

## Surface audited

| Category | Rust | C reference | Lines (Rust / C) |
|----------|------|-------------|------------------|
| A — record | `src/records/scaler.rs` | `scalerRecord.c` | 1287 / 777 |
| B — asyn device support | `src/device_support/scaler_asyn.rs` | `devScalerAsyn.c` | 377 / 404 |
| C — soft driver | `src/device_support/scaler_soft.rs` | `drvScalerSoft.c` | 155 / 668 |
| C — hardware driver | (none) | `drvScaler974.cpp` (VSC16 VME) | 0 / 285 |

## Prior findings — do NOT re-open

From `doc/c-parity-review-2026-06-15.md` (and the SCAL-1/2 monitor-mask pair
later closed structurally):

| # | Finding | Disposition |
|---|---------|-------------|
| SCAL-1 | idle `process()` re-posts `S1..Snch` with `DBE_LOG` | **Fixed** (3486badf) — `log_swept_fields` |
| SCAL-2 | value-change posts carry an extra `DBE_LOG`; C is `DBE_VALUE`-only | **Fixed** (56bb25de) — `value_only_change_fields` |
| SCAL-3 | `arm(0)` disarm must clear counts (C clears unconditionally) | **Fixed** (b83b8af1) |
| SCAL-4 | `reset()` zeroes counts; C reset leaves them | **Signoff / keep-Rust** (5a751bb4) — transient, repopulates next read |
| SCAL-5 | `special("RATE")` posts RATE; C posts TP (`db_post_events(&tp)` copy-paste bug) | **Signoff / keep-Rust** (5a751bb4) — C bug not copied |

The prior round also recorded these as **verified-equivalent**: count→done
sequence, CNT/US/SS transitions, preset reconciliation (NINT vs trunc),
COUT/COUTP, VAL=T-on-completion, FwdLink gating, UDF via clears_udf,
device-support read/done/preset-compare. This round re-confirmed each (see
highlights) and refined the COUT/COUTP edge to SCAL-6.

## Verification highlights (re-confirmed this round)

- **process() state machine** (`scaler.rs:626-815` ↔ `scalerRecord.c:341-546`):
  `check_done` polled unconditionally every cycle (C:367); CNT REQSTART/stop
  block, `updateCounts`, COUT-on-start||finish (C:455-460), VAL=T on
  `COUNTING→IDLE` (C:476-479), autocount block (C:484-541) all match.
- **Forward-link gate** (`scaler.rs:898-900`): `should_fire_forward_link`
  returns `ss==IDLE && us==IDLE && pcnt==0`, exactly C's nested
  `if (ss==IDLE) { ... if (pcnt==0 && us==IDLE) recGblFwdLink }`
  (C:471-481). Consumed by the framework at `processing.rs:2791/3040/3311/4031`
  — FLNK is NOT fired while counting.
- **REQSTART preset reconciliation** (`scaler_asyn.rs:236-303` ↔
  `scalerRecord.c:406-432`): pre-guard `old_pr1` capture (C:406),
  `NINT(tp*freq)` guard (C:409-410), per-channel `write_preset` with
  driver-adjusted channel-0 readback (C:413-419), `save_pr1!=pr1` re-write
  (C:420-423), `old_pr1!=pr1` TP recompute (C:424-428), `old_freq!=freq` FREQ
  post (C:429-430), arm (C:431) — including the truncating-vs-NINT distinction
  (`pr1_trunc` C:328/672 vs `pr1_nint` C:409).
- **Autocount** (`scaler_asyn.rs:317-376` ↔ `scalerRecord.c:508-535`):
  `tp1>=1ms` single-channel-0 path with driver-adjust re-write (C:512-522),
  user-PR1 restore (C:532), FREQ-only post (C:530), TP not recomputed.
- **updateCounts** (`scaler.rs:240-257` ↔ `scalerRecord.c:549-601`): zero
  display while WAITING (C:571-575), `T = s[0]/freq` (C:586-588), periodic
  reschedule at `1/rate` only when `rate>0.1` and `ss==COUNTING` (C:590-596).
- **Periodic refresh re-reads the device**: `ReprocessAfter` →
  `schedule_delayed_reprocess` → `process_record_continuation` →
  `process_record_with_links_inner` (`processing.rs:570-580`) re-runs
  device-support `read()` before `process()`, so the soft driver's
  `check_presets`-set `done_flag` is refreshed and the count completes.
- **Soft driver cited lines verified**: reset clears presets
  (`drvScalerSoft.c:303-313`), arm clears counts on arm AND disarm (`:315-329`,
  the SCAL-3 fix), preset store (`:331-336`), `checkAcquireDone` preset-reached
  detection (`:588-600`).

## Open findings (this round)

All findings below are **NOTE** or **DESIGN** — no open DEFECT or CONCERN. The
C-bug divergence (SCAL-6) is intentional (the global rule forbids copying C's
actual bugs); the design divergences are faithful at the semantic level. No fix
commits are warranted.

### SCAL-6 — COUTP fires once on user-stop (C fires twice) — NOTE (C-bug-avoided)
Refines the prior "COUT/COUTP verified-equivalent" by enumerating the stop edge.
- Rust: `scaler.rs:751-762` coalesces the `coutp_pending` trigger (set by
  `special("CNT")`, mirroring `scalerRecord.c:625`) and the
  `just_finished_user_count` trigger (`scalerRecord.c:461-463`) into a single
  `fire_coutp` `WriteDbLink` per process cycle.
- C: on a user **stop** (`CNT 1→0`), `special()` fires COUTP (C:625) AND
  `process()` fires it again under `justFinishedUserCount` (C:463) — the same
  value `0` is written to the COUTP link **twice**. On a **start** C fires it
  once (special only), which Rust also does.
- Impact: a downstream record driven by COUTP processes once per stop in Rust
  vs twice in C. The C double-fire is an incidental redundancy (identical
  value); the single fire is cleaner — same disposition class as SCAL-5.
  **Not copied.**

### SCAL-7 — done detection is poll-driven, not interrupt-driven — DESIGN
- Rust: the soft driver sets `done_flag` inside `read()` (`check_presets`); the
  record polls it via `check_done` every process cycle, and the periodic
  `ReprocessAfter` re-runs `read()` (verified above), so completion is detected
  within `~1/rate` of the preset being reached.
- C: `devScalerAsyn.c:391-403` `interruptCallback` is fired by the driver when
  acquisition completes (`value!=0 → done=1 → callbackRequest(pcallback)`),
  pushing a record process. `scaler_done` (C:292-301) is read-and-clear —
  matched exactly by `ScalerDriver::done` (`scaler_soft.rs:143-150`).
- Impact: equivalent count-completion latency; the trait has no asyn interrupt
  channel, so the record's existing periodic reprocess carries the poll.
  Faithful. (Context: the `scaler_soft.rs` 155-vs-668 size gap is this asyn /
  input-PV / interrupt plumbing — the C soft driver connects template input
  PVs and runs a sim thread; the Rust trait sources counts from a shared
  `Arc<Mutex<[u32;64]>>`. SCAL-3/SCAL-4 already dispositioned the count-clear
  specifics of that abstraction.)

### SCAL-8 — no COMM_ALARM short-circuit on device commands — NOTE
- C: `scaler_command` (`devScalerAsyn.c:316`) returns `-1` without queuing when
  `psr->nsta == COMM_ALARM || psr->stat == COMM_ALARM` (port already known
  unreachable).
- Rust: `handle_command` (`scaler_asyn.rs:131-204`) always dispatches to the
  driver; an unreachable real driver surfaces via the `CaResult` error path
  rather than a pre-check. No Rust scaler path currently sets `COMM_ALARM`, so
  the gate is moot. A future hardware `ScalerDriver` wanting the C pre-check
  would add it at the trait boundary.

### SCAL-9 — VSC16 hardware driver not ported — DESIGN-ABSENCE
- `drvScaler974.cpp` (285 lines) is device support for the Joerger VSC16/VSC8
  VME scaler — raw VME register pokes (`devLib` `devRegisterAddress`,
  bus-error probes, IRQ vector wiring). It has no Rust counterpart.
- Without the VME hardware this cannot be exercised; a Rust port would be
  untestable register-poke fake-parity. The portable/testable surface (record +
  asyn trait + soft driver) is complete. Legitimate design absence.

## Review log

### Round 2 — 2026-06-28 (single-reviewer sweep)

Re-verified the SCAL-1..SCAL-5 surface plus everything the round-1 doc marked
"verified-equivalent", opening every cited C line in the Rust source directly
(process/special/updateCounts/init in category A; the full `asynCallback`
command dispatch and `scaler_done` read-and-clear in category B;
reset/arm/preset/checkAcquireDone in category C). Confirmed the FLNK gate is
consumed by the framework and the periodic-refresh path re-reads the device so
the soft-driver completion wires end-to-end.

Result: **parity-clean.** Zero DEFECT, zero CONCERN. Four new dispositions
(SCAL-6..SCAL-9): one C bug intentionally not copied (SCAL-6 COUTP stop
double-fire, same class as SCAL-5), two faithful design abstractions of asyn /
interrupt / alarm plumbing (SCAL-7/8), one untestable-hardware design absence
(SCAL-9). No fix commits warranted.
