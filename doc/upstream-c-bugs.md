# Upstream C defects — catalogue for reporting upstream

Extracted 2026-07-13 from `doc/c-parity-review-2026-07-10.md`, where it grew as
a section since 2026-07-12. **New upstream findings accumulate HERE** — later
waves append batches to this file; nothing is deleted once filed.

> **Submission status lives in one place:** the *Filed upstream PRs — live
> GitHub status* table just below the Index (last reconciled 2026-07-18, 20
> upstream PRs by `physwkim`). Per-entry `Status:` prose predates it and may be
> stale; trust the table for PR number and state.

**What this catalogue is.** The parity inventory
(`doc/c-parity-review-2026-07-10.md`) catalogues divergences of *this port*
from its C/C++ reference. This file is the mirror image: defects **in the
reference itself**, found while porting.

Two kinds of entry, and the distinction is the whole point:

- **REPRODUCED** — the port carries the C defect *deliberately*, because
  bug-for-bug parity is the contract. Fixing it upstream would let us drop the
  reproduction; until then the port is wrong on purpose and says so in a comment.
- **NOT-REPRODUCED** — the port *refuses* the C behaviour, because it is
  undefined behaviour, a memory-safety violation, a data race, or a crash, and
  there is no defined contract to be faithful to. The port already deviates and
  the deviation is signed off. These are the entries most worth reporting: an
  IOC running the C today is exposed.

**Method.** Every entry names the C at `file:line`, the port site that either
reproduces or refuses it, a severity, the operational impact, and a proof. For
the calc engines and the optics/pvxs entries the proof is a **compiled-C driver**
run on this host (gcc 13.3 / g++ libstdc++, x86-64 Linux) linked against the real
upstream translation units — compiled C is ground truth, not a reading of it. For
the rest the proof is the decisive code path, quoted.

**Reference trees and versions read.**

| tree | version |
|---|---|
| `epics-base` | working tree at `/home/stevek/work/epics-base` |
| `asyn` | `R4-45-19-ge2a281e2` |
| `optics` | `R2-14-15-g3def19d` |
| `ADCore` | `R3-14-111-g6c53844e` |
| `pvxs` | `1.5.1-42-gb568e93` |
| `calc`, `motor`, `std`, `scaler`, `modbus`, `mqtt` | working trees under `/home/stevek/work/epics-modules` |

### Counts (wave 1 of the catalogue — 31 entries)

| bucket | n |
|---|---|
| REPRODUCED (port carries the C bug on purpose) | 12 |
| NOT-REPRODUCED (port refuses the C behaviour) | 19 |
| UNDECIDED | 0 |

> Corrected 2026-07-18: CBUG-B25 moved REPRODUCED → NOT-REPRODUCED. The port was
> flipped to divide-first (refuse) in `d8f27b88` on 2026-07-13, hours after this
> catalogue was extracted, so the original REPRODUCED count was stale by that
> evening; upstream then fixed it as ADCore #596 (merged 2026-07-16).

> **Mass reconciliation 2026-07-20 — REPRODUCED fully retired.** Per
> strategy-2026-07-13 §2 ("C's bugs are not the contract. Clean is the goal.") the
> port refuses C's defects rather than mirroring them. On 2026-07-20 the REPRODUCED
> bucket was reconciled against the actual port state, flipping **all 16** entries
> REPRODUCED → NOT-REPRODUCED: **A3, B1, B4, B6, B8, B10, B11, B12, B13, B18, B19,
> C2, D4, E2, F8, F12**. After this pass **no catalogue entry is REPRODUCED** — the
> port refuses every catalogued C upstream bug.
>
> Fourteen the port had *already* refused before this catalogue was even extracted
> (the fixes are ancestors of `main`, the B25/E2 precedent) — A3 `ed43b05f`,
> B1 `ba79fd0f`, B4 `d906ad45`, B6 `edaa41b7`, B8 `93f7baee`, B10 `06c05dbe`,
> B11 `aecc980a`, B12 code `972ac9b0`, B13 `7d2b48ac`, B18 `a0c723b9`,
> B19 `46be9a08`, C2 `67097734` (deleted `MonitorRequestFatal` — circuit teardown
> unrepresentable), F8 `25318fcd`; E2 already saturates (`651bf392`). Two needed a
> fresh fix on 2026-07-20: **F12** a structural code fix (`9a51ba4c` — raises
> SOFT/INVALID consistently on both the process and `.SGNL` paths via the single
> `nsta`/`nsev` owner) and **D4** a structural code fix (`09af6a24` — both
> `asyn-rs` escapers render NUL as `\0` through one parameter-free table,
> superseding the 2026-07-14 "keep both" adjudication). One needed a doc-line fix:
> **B12** (`b0704bff`). This historical wave-1 table is left as extracted; the
> per-entry `Bucket:` headers carry the current classification.
>
> **Caveats, not exceptions.** B19's flip is correct but *inert* until `do_alarm()`
> is ported (see the B19 body). D4's refusal carries a deliberate one-byte
> deviation on the display/trace path (`print_escaped` now emits `\0`, not C's
> `\x00`) — stated in the D4 body. CBUG-A2 is unchanged — **FIXED-UPSTREAM** (base
> half tracks PR #925), not REPRODUCED, so it is neither in the flip set nor a
> remaining REPRODUCED.

| severity | n |
|---|---|
| High | 8 |
| Medium | 14 |
| Low | 9 |

By upstream: epics-base/calc 4 · asyn 6 · ADCore 8 · optics 4 · std 4 ·
scaler 2 · pvxs 1 · motor 1 · modbus 1. `mqtt` was examined and produced no
proven defect (see "Leads rejected").

### Index

| id | upstream | one line | severity | bucket |
|---|---|---|---|---|
| CBUG-A1 | base calc | `MODULO` `INT_MIN % -1` — SIGFPE kills the IOC | High | NOT-REPRODUCED |
| CBUG-A2 | base calc | `NINT`/`MODULO` skip C's own `d2i` guard — out-of-range → `INT_MIN` | Medium | FIXED-UPSTREAM |
| CBUG-A3 | base calc | `ISINF` leaks glibc's *signed* isinf (±1) into the value | Low | NOT-REPRODUCED |
| CBUG-A4 | base calc | `RNDM` fixed seed + unsynchronised global RMW | Low | NOT-REPRODUCED |
| CBUG-B1 | optics | `pf4.st` interpolates on the interval *above* the energy — `frac < 0` always | Medium | NOT-REPRODUCED |
| CBUG-B2 | optics | `pf4.st` reads `keV[274]`/`mu[274]` out of bounds for Pb | Medium | NOT-REPRODUCED |
| CBUG-B3 | optics | `pf4.st` unguarded glass divide — all 16 transmissions NaN below 2 keV | High | NOT-REPRODUCED |
| CBUG-B4 | optics | `pf4.st` unknown material silently reports the blade fully opaque | Medium | NOT-REPRODUCED |
| CBUG-B5 | asyn | `asynInterposeCom setOption("ixon")` missing `return` — ships an uninitialized stack byte | High | NOT-REPRODUCED |
| CBUG-B6 | asyn | `asynInterposeCom` can enable flow control but never disable it | Medium | NOT-REPRODUCED |
| CBUG-B7 | asyn | `asynInterposeCom nextChar` ignores `nbytes` — uninitialized char on a 0-byte success | Low | NOT-REPRODUCED |
| CBUG-B8 | asyn | telnet subnegotiation payload is not IAC-stuffed | Low | NOT-REPRODUCED |
| CBUG-B9 | asyn | `drvAsynIPServerPort` UDP read returns stale heap + drops a byte | High | NOT-REPRODUCED |
| CBUG-B10 | asyn | every `asyn*Base.c` `readDefault` says "**write** is not supported" (6 files) | Low | NOT-REPRODUCED |
| CBUG-B11 | ADCore | `NDPluginCircularBuff` — writing **0** to `SoftTrigger` triggers | Medium | NOT-REPRODUCED |
| CBUG-B12 | pvxs | `ackAt == 0` sentinel makes the `ackAny` percentage mapping non-monotonic | Low | NOT-REPRODUCED |
| CBUG-B13 | motor | `motorRecord` publishes CDIR=forward for a reverse jog-stop backlash leg | Medium | NOT-REPRODUCED |
| CBUG-B14 | std | `throttleRecord` callback mutates the record with no `dbScanLock` | High | NOT-REPRODUCED |
| CBUG-B15 | std | `epidRecord` raises UDF then returns before committing it | Medium | NOT-REPRODUCED |
| CBUG-B16 | std | `devEpidSoft` "nothing to control" abort falls through when already INVALID | Medium | NOT-REPRODUCED |
| CBUG-B17 | std | `throttleRecord` writes CA-link status for the wrong link (2 sites) | Low | NOT-REPRODUCED |
| CBUG-B18 | scaler | `special(RATE)` posts `.TP` and never posts the clamped `.RATE` | Low | NOT-REPRODUCED |
| CBUG-B19 | scaler | `monitor()` builds the alarm mask, then posts a literal `DBE_LOG` and discards it | Low | NOT-REPRODUCED |
| CBUG-B20 | ADCore | `NDPluginROIStat` writes ROI geometry OOB for any RGB (3-D) array | High | NOT-REPRODUCED |
| CBUG-B21 | ADCore | `NDPluginAttrPlot` `<=` off-by-one → heap OOB write on the first frame | High | NOT-REPRODUCED |
| CBUG-B22 | ADCore | `NDPluginProcess` divides by `numFiltered` with `NumFilter == 0` | Medium | NOT-REPRODUCED |
| CBUG-B23 | ADCore | `NDPluginProcess` AutoOffsetScale divides by `(max−min)` on a uniform frame | Medium | NOT-REPRODUCED |
| CBUG-B24 | modbus | ASCII-serial LRC sums the LRC into itself and compares an undecoded byte | Medium | NOT-REPRODUCED |
| CBUG-B25 | ADCore | `NDPluginTimeSeries` narrows the sum *before* dividing — integer averaging corrupted | Medium | NOT-REPRODUCED (fixed upstream #596) |
| CBUG-B26 | ADCore | `NDPluginStats` broadcasts an uninitialized `NDStats_t` on dark frames | High | NOT-REPRODUCED |
| CBUG-B27 | ADCore | `NDPluginStats` histogram divides by `(histMax − histMin)` — `(int)NaN` UB | Medium | NOT-REPRODUCED |

---

### Filed upstream PRs — live GitHub status (as of 2026-07-20)

This is the authoritative submission-status table; it replaces the per-entry
`Status:` prose, which is stale (the catalogue was extracted 2026-07-13, before
most of these PRs were filed on 7/16–7/20). Author: `physwkim`. State is the live
GitHub PR state, not a claim about the catalogue. Total filed: **39** — 38
NOT-REPRODUCED (port refuses) + 1 FIXED-UPSTREAM. Of these, the 9 grouped in the
separate table after the main one were classified REPRODUCED when this catalogue
was extracted (2026-07-13), but all nine were flipped REPRODUCED →
NOT-REPRODUCED on 2026-07-20 (the port already refused them — see the 2026-07-20
reconciliation note); the second table is kept only as a grouped PR-status view.

**Catalogued CBUGs that are filed (30):**

Some PRs bundle several CBUGs into one PR (calc **#41** = D2/F5/F2/F3;
epics-base **#932** = D3/D5/F6; std **#28** = B15/B16; ADCore **#598** =
B22/B23/B27; optics **#27** = B1/B2/B4, spanning both tables) — each CBUG gets
its own row pointing at the shared PR.

| CBUG | PR | state | note |
|---|---|---|---|
| CBUG-A1 | epics-modules/calc **#38** | open | `MODULO INT_MIN % -1` SIGFPE guard |
| CBUG-A2 | epics-base/epics-base **#925** | open | `calcPerform` NINT/MODULO `d2i` narrowing |
| CBUG-B2 | epics-modules/optics **#27** | open | pf4 Pb top-bin `keV[j+1]`/`mu[j+1]` OOB read — removed by the `[j-1,j]` interval fix (supersedes closed #26) |
| CBUG-B3 | epics-modules/optics **#25** | open | pf4 glass-term guard |
| CBUG-B5 | epics-modules/asyn **#234** | merged | `asynInterposeCom` ixon `return` |
| CBUG-B7 | epics-modules/asyn **#238** | open | `asynInterposeCom` `nextChar` returns uninitialized `c` on a successful 0-byte read → guard `nbytes == 0` as EOF |
| CBUG-B9 | epics-modules/asyn **#233** | merged | `drvAsynIPServerPort` UDP read bound |
| CBUG-B14 | epics-modules/std **#27** | open | `throttleRecord` `dbScanLock` |
| CBUG-B15 | epics-modules/std **#28** | open | `epidRecord` commit UDF alarm before return |
| CBUG-B16 | epics-modules/std **#28** | open | `devEpidSoft` abort unconditionally when INP constant |
| CBUG-B17 | epics-modules/std **#29** | open | `throttleRecord` writes link-status for the wrong link (special hardcodes `outLinkStat`; checkLink `caLink`/`caLinkNc` stale across loop) |
| CBUG-B20 | areaDetector/ADCore **#594** | merged | `NDPluginROIStat` RGB heap OOB |
| CBUG-B21 | areaDetector/ADCore **#595** | merged | `NDPluginAttrPlot` `<=` off-by-one |
| CBUG-B22 | areaDetector/ADCore **#598** | open | `NDPluginProcess` `numFilter < 1` divide-by-zero guard |
| CBUG-B23 | areaDetector/ADCore **#598** | open | `NDPluginProcess` `autoOffsetScale` `maxValue > minValue` guard |
| CBUG-B25 | areaDetector/ADCore **#596** | merged | `NDPluginTimeSeries` narrow-before-divide |
| CBUG-B26 | areaDetector/ADCore **#597** | merged | `NDPluginStats` dark-frame value-init |
| CBUG-B27 | areaDetector/ADCore **#598** | open | `NDPluginStats` histogram `histMax <= histMin` guard |
| CBUG-C1 | epics-modules/calc **#39** | open | `sCalc lrc()` empty-operand OOB read |
| CBUG-D2 | epics-modules/calc **#41** | open | `sCalc` `<<`/`>>` negative shift-count OOB read/write |
| CBUG-D3 | epics-base/epics-base **#932** | open | `EPICS_CA_CONN_TMO` non-positive watchdog flood |
| CBUG-D5 | epics-base/epics-base **#932** | open | `EPICS_CA_MAX_SEARCH_PERIOD` non-finite crash |
| CBUG-E2 | epics-base/epics-base **#933** | open | `dbConvert`/`dbFastLinkConv` float→int cast UB → saturation; **port already saturates (refuses the bug)**; the entry's `Bucket:` header was flipped REPRODUCED → NOT-REPRODUCED on 2026-07-20 to match its body — see note below |
| CBUG-F1 | epics-modules/calc **#40** | open | `aCalc INC()` off-by-two |
| CBUG-F2 | epics-modules/calc **#41** | open | `aCalc SUBRANGE` inclusive upper bound OOB read |
| CBUG-F3 | epics-modules/calc **#41** | open | `aCalc DERIV`/`nderiv` fit-window > array OOB read |
| CBUG-F5 | epics-modules/calc **#41** | open | `sCalc LITERAL_STRING` copy bound never advances OOB write |
| CBUG-F6 | epics-base/epics-base **#932** | open | `calcRecord` drop unhandled `special(SPC_MOD)` from INPM..INPU |
| CBUG-F11 | epics-modules/asyn **#235** | merged | `asynManager` traceIO truncate hang |
| CBUG-G1 | epics-base/pvxs **#196** | open | QSRV2 `display.precision` |

> **CBUG-B25 — reclassified 2026-07-18 (was: classification conflict).** The
> catalogue extracted B25 as **REPRODUCED**, but that was stale within hours: the
> port was changed to divide-first (refuse the bug) in `d8f27b88` on 2026-07-13,
> so it has **not** carried the bug since. Upstream then fixed the same
> parenthesis error as ADCore **#596** (merged 2026-07-16). B25 is therefore
> **NOT-REPRODUCED** (port refuses) and upstream-agreeing — the port matched the
> #596 fix three days before it landed. No port behaviour change is owed; only
> the catalogue bucket and the `time_series_plugin.rs` comment framing were
> corrected.

> **CBUG-E2 — stale bucket (like B25), reconciled 2026-07-20.** The entry's
> `Bucket:` header read `REPRODUCED` until 2026-07-20, but its own adjudication
> records that the port **saturates** float/double → integer conversions
> (`epics-base-rs` `types/c_cast.rs`, Rust `as`), i.e. it **refuses** the C bug
> and is byte-identical to the aarch64 hardware result. The header was flipped to
> **NOT-REPRODUCED** (port refuses) on 2026-07-20 to match the body; E2 belongs in
> this table, not the (now-retired) REPRODUCED one. The upstream PR **#933** defines the C behaviour (saturate,
> NaN → 0) on both the network (`dbConvert.c`) and DB-link (`dbFastLinkConv.c`)
> paths, aligning x86-64 onto the aarch64/port semantics. No port behaviour
> change is owed; if #933 merges, the C side simply stops diverging from the
> port.

**Was REPRODUCED, now NOT-REPRODUCED — also reported upstream (9):**

These nine were catalogued as REPRODUCED at extraction (2026-07-13), but the
port had already been flipped to **refuse** each of them (the fixes predate the
catalogue snapshot; see the per-entry `Bucket:` headers and the 2026-07-20
reconciliation note), so on 2026-07-20 all nine were reclassified REPRODUCED →
NOT-REPRODUCED. The port no longer reproduces any of them; the note column below
already describes the corrected behaviour. Filing upstream still stands: the C
side stays wrong until a fix merges, at which point C simply stops diverging
from the already-correct port.

| CBUG | PR | state | note |
|---|---|---|---|
| CBUG-B1 | epics-modules/optics **#27** | open | `pf4` `OtherAbsorptionLength` interpolate on `[j-1,j]` (frac was always negative) |
| CBUG-B4 | epics-modules/optics **#27** | open | `pf4` unknown/out-of-range Other material: skip the blade + diagnose, not silently opaque |
| CBUG-B6 | epics-modules/asyn **#236** | open | `asynInterposeCom` disable flow control now sends NOFLOW (crtscts + ixon) |
| CBUG-B8 | epics-modules/asyn **#236** | open | `asynInterposeCom` IAC-stuff the COM-PORT-OPTION subnegotiation payload |
| CBUG-B10 | epics-modules/asyn **#237** | open | `asyn*Base.c` `readDefault` errorMessage "read", not "write", is not supported (6 files) |
| CBUG-B11 | areaDetector/ADCore **#599** | open | `NDPluginCircularBuff` writing `0` to `SoftTrigger` disarms, not fires (guard latch+flush behind `if (value)`) |
| CBUG-B13 | epics-modules/motor **#254** | open | `motorRecord` key CDIR on the commanded stroke after a jog-stop backlash |
| CBUG-B18 | epics-modules/scaler **#4** | open | `scalerRecord` `special(RATE)` posts `.RATE`, not `.TP` |
| CBUG-B19 | epics-modules/scaler **#4** | open | `scalerRecord` `monitor()` posts `monitor_mask`, not literal `DBE_LOG` |

**Filed upstream PRs with no catalogue CBUG entry yet (6):**

| PR | state | one line |
|---|---|---|
| epics-base/epics-base **#924** | open | `seqRecord` upper display limit of the DLYn fields |
| epics-base/epics-base **#926** | open | `selRecord` precision for the A-L / LA-LL fields |
| epics-base/pvxs **#179** | closed | qsrv & monitor: seven correctness issues (bundle, not 1:1) |
| epics-base/pvxs **#180** | open | codec/client: bound decode, reject pre-INIT monitor data |
| epics-base/pvxs **#181** | open | ossl/client/config: TLS peer-identity / downgrade / keychain |
| epics-base/pvxs **#195** | open | server: follow TCP port fallback on all interfaces |

These want back-fill CBUG entries (or an explicit "no entry — direct find" note)
so the catalogue and the filed set stay reconcilable.

**Catalogued NOT-REPRODUCED, still UNFILED (the remaining easy-to-accept set):**
none — the list is now empty. CBUG-B7 (asyn **#238**) and CBUG-B17
(std **#29**) were filed 2026-07-20; the 2026-07-19 batch had already cleared
B2, B22, B23, B27, D2, D3, D5 (all in the table above).

**Prepared but held (not filed):** the pvxs pair CBUG-F9 (process-only blocking
PUT silent no-op) and CBUG-F10 (UnionArray round-trip) — fixes committed on
`fix/blocking-put-and-unionarray` in a worktree, held pending re-verification of
F10 (the prepared fix is decode-side, contradicting F10's encode-side framing).
CBUG-B24 (modbus ASCII-serial LRC) is deferred on a `physwkim/modbus` fork
question.

---

### CBUG-A1: `MODULO` crashes the whole IOC on `INT_MIN % -1` (SIGFPE)
Bucket: NOT-REPRODUCED · Severity: High
C: `epics-base modules/libcom/src/calc/calcPerform.c:161-166` (`calcPerform`, `MODULO`):
```c
case MODULO:
    itop = (epicsInt32) *ptop--;
    if (itop)
        *ptop = (epicsInt32) *ptop % itop;   /* <-- no INT_MIN/-1 guard */
    else
        *ptop = epicsNAN;
```
The zero divisor is guarded; the one signed-remainder case that is *undefined* in
C — `INT_MIN % -1` — is not. On x86 `idiv` raises `#DE`, delivered as SIGFPE,
killing the process. Same statement in all three engines: sCalc
`calc/calcApp/src/sCalcPerform.c:1108` (`(long)ps->d % (long)ps1->d` — LP64, so
the crash is `INT64_MIN % -1`), aCalc `aCalcPerform.c:674` (array path) and
`:703` (scalar path).
Defect: unguarded UB — and not a corner input, because the dividend is produced by
an out-of-range/NaN double→int cast that *itself* yields `INT_MIN` (CBUG-A2). Any
dividend `≥ 2^31`, `≤ -2^31`, or NaN, with divisor `-1`, reaches the crash.
Port: `crates/epics-base-rs/src/calc/engine/numeric.rs:90-104`
(`c_int(a).wrapping_rem(den)`), `engine/string.rs:127-149`, `engine/array.rs:142-154`.
Rust defines `i32::MIN % -1 == 0`; the port returns 0 and never crashes.
Impact: a single `calc`/`calcout`/`scalcout`/`acalcout`/`swait`/`transform` record
whose expression contains `%` takes the **whole IOC** down with SIGFPE the moment a
large, negative-overflow, or NaN dividend meets divisor `-1`. Total IOC loss, from
a data-driven expression input.
Proof (compiled C, this host):
```
A%B  A=3e9         B=-1 -> Floating point exception (core dumped)
A%B  A=-nan        B=-1 -> Floating point exception (core dumped)
A%B  A=-2147483648 B=-1 -> Floating point exception (core dumped)
```

### CBUG-A2: `NINT` / `MODULO` narrow a double with a plain `(epicsInt32)` cast — out-of-range or NaN silently becomes `INT_MIN`
Bucket: FIXED-UPSTREAM (base half, PR #925) · Severity: Medium
Status: base/numeric closed — our PR #925 routes NINT/MODULO through `d2i` and the port
tracks the fix. sCalc/aCalc mirror pristine synApps per dialect (see Port below).
C: `calcPerform.c:290-293` (`NINT`): `*ptop = (epicsInt32)(top>=0 ? top+0.5 : top-0.5)`,
and `calcPerform.c:162-164` (`MODULO`'s dividend cast). Neither uses the `d2i`/`d2ui`
macros (`calcPerform.c:324-325`) that every sibling bitwise/shift op (`BIT_OR`,
`BIT_AND`, `RIGHT_SHIFT_*`, …) uses. The `d2i` comment (`:313-322`) says out-of-range
double→int conversions "give very different results on different systems" and exists
precisely to make those ops well-defined — `NINT` and `MODULO` were left on the raw
cast. On x86 an out-of-range `cvttsd2si` yields the "integer indefinite" value
`0x80000000` = `INT_MIN`; on other targets it differs. sCalc/aCalc carry the same via
`myNINT` / `(int)` / `(long)`.
Defect: platform-dependent wrong result, and the crash vector for CBUG-A1. The C team
half-fixed this family (bitwise via `d2i`) and left NINT/MODULO exposed.
Port (as of `91f9c327`, tracking base PR #925 `669a25697`): NINT/MODULO narrow
**per-engine, each mirroring its own C dialect**, through the single parameterized
owners `cast::nint`/`cast::imod`:
- **numeric (base)** → `d2i` — matches the **fixed** base C. Our upstream PR #925
  routes NINT/MODULO through `d2i` there, and the oracle ground truth is rebuilt on
  that fix, so this half is no longer a live deviation: fixed C and port both give
  `NINT(3e9) = -1294967296`, `3e9 % 7 = 0`.
- **sCalc (string)** → `c_long` (`(long)`, 64-bit) — mirrors pristine synApps
  `sCalcPerform.c`; `NINT(3e9) = 3e9`, `3e9 % 7 = 4` (the `(long)` path is actually
  *correct* for values below 2^63, so there is no base-style bug to fix there).
- **aCalc (array)** → `c_long` for **NINT** (`aCalcPerform.c:839,1096`, 64-bit) but
  `c_int` for **MODULO** (`:661,:685,:711`, 32-bit) — a verified **split-width
  inconsistency inside one engine**. The port mirrors it exactly. aCalc's out-of-range
  MODULO additionally rides CBUG-E2 (`c_int` saturates to `i32::MAX` where x86
  `cvttsd2si` gives `INT_MIN`) — pre-existing, separate.

Earlier catalogue text described the port as a reproduced-`cvttsd2si` state pinned to
`i32::MIN`; that was stale — the port was on a clean no-narrow deviation before
`91f9c327`, now replaced by the per-engine mirroring above.

Lead (unfiled): aCalc's split narrowing (NINT `(long)` vs MODULO `(int)`) is a
plausible upstream CBUG — align aCalc MODULO to `(long)`, or route both through a
`d2i`-style macro as base now does.
Impact: `NINT(3e9)` returns `-2147483648`; `3e9 % 7` returns `-2`. Any calc record
that rounds or takes a modulus of a value that can exceed 2^31 (counters, ns
timestamps, large ADC sums) writes a wrong number to `VAL`/its output link — and the
value is not portable across IOC CPU architectures.
Proof (compiled C):
```
NINT(A)  A=3e9     -> -2147483648   (true nearest int = 3000000000)
NINT(A)  A=2.5e9   -> -2147483648
A%B      A=3e9 B=7 -> -2            (A&B, the d2i-guarded op, gives 0)
A%B      A=-nan B=7 -> -2
```

### CBUG-A3: `ISINF` leaks glibc's *signed* isinf result (±1) into the expression value
Bucket: NOT-REPRODUCED (port refuses — flipped 2026-07-20; see the 2026-07-20 reconciliation note) · Severity: Low
C: `calcPerform.c:276-277`: `*ptop = isinf(*ptop);`. On glibc this resolves to the
GNU/BSD *function*, which returns `+1` for `+Inf` and **`-1` for `-Inf`** — not the
C99 *macro* (a plain boolean 1). Same in `sCalcPerform.c:703`/`:1407` and
`aCalcPerform.c:826`/`:1084`.
Defect: `calcRecord.dbd.pod:263` documents `ISINF (arg)` as "returns non-zero if any
argument is Inf" — a boolean predicate. `-1` satisfies "non-zero" but is neither the
documented boolean nor portable: an IOC where `isinf` resolves to the C99 macro gets
`+1` for `-Inf`.
Port: `numeric.rs:286-288` → `engine/mod.rs:118-124 c_isinf` returns `-1.0` for a
negative-signed infinity. Reproduced on purpose (glibc/Linux is the field target).
Impact: `A := ISINF(B)` stores `-1` when `B` is `-Inf`; a downstream `ISINF(B) == 1`
test misfires on `-Inf`; the numeric result differs between a glibc IOC and one
compiled against the C99 macro.
Proof (compiled C):
```
ISINF(A)     A=+inf -> 1
ISINF(A)     A=-inf -> -1
ISINF(-1/A)  A=0    -> -1
```

### CBUG-A4: `RNDM` uses a fixed-seed generator via an unsynchronised shared global
Bucket: NOT-REPRODUCED · Severity: Low
C: `calcPerform.c:514-524`:
```c
static unsigned short seed = 0xa3bf;              /* fixed seed */
static unsigned short multy = 191*8+5, addy = 0x3141;
static double calcRandom(void) {
    seed = (seed * multy) + addy;                 /* RMW on a shared global */
    return (double) seed / 65535.0;
}
```
Two defects in one function. (1) `seed` is the constant `0xa3bf` and is never
re-seeded, so **every IOC process emits the identical RNDM sequence from the same
starting point on every boot** — fully predictable. (2) `seed` is a file-scope global
mutated by a non-atomic read-modify-write with no lock, while `calcPerform` runs
concurrently on every scan thread (periodic / event / I/O-Intr) — a C11 data race
(torn/lost updates, UB; TSan flags it). aCalc's `local_random`
(`aCalcPerform.c:1662-1685`) is thread-private so it dodges the race but keeps the
same fixed seed `RAND_SEED 0xa3bf`. Third, minor: `(double)seed / 65535.0` reaches
exactly `1.0` at `seed == 65535`, so the "between 0 and 1" range includes 1.0.
Port: `numeric.rs:49-50` → `numeric.rs:435-452 simple_random`; aCalc `array.rs:1379`.
Seeds from `SystemTime::now()` nanoseconds, state in an `AtomicU64`. Deviation signed
off: reproducing this faithfully would mean shipping both the predictability and the
data race deliberately.
Impact: RNDM-based dithering/jitter/simulation is identical on every IOC in the field
and repeats exactly after each restart; concurrent RNDM on multiple scan threads is a
data race.
Proof (compiled C, two independent process runs):
```
run1: RNDM = 0.7500724804, 0.03596551461, 0.3266956588, 0.009201190204, 0.2976119631
run2: RNDM = 0.7500724804, 0.03596551461, 0.3266956588, 0.009201190204, 0.2976119631
```

---

### CBUG-B1: `pf4.st` `OtherAbsorptionLength` interpolates on the wrong interval — every "Other" filter transmission is wrong
Bucket: NOT-REPRODUCED (port refuses — flipped 2026-07-20; see the 2026-07-20 reconciliation note) · Severity: Medium
C: `optics/opticsApp/src/pf4.st:641-643`. The bracketing loop
`for (j=0; j<numEntries; j++) if (keV < filtermat[i].keV[j]) break;` leaves `j` as
the first node **strictly above** `keV`. C then interpolates on `[j, j+1]`:
`frac = (keV - keV[j]) / (keV[j+1] - keV[j])`. Since `keV < keV[j]` by construction,
`frac` is always **negative** — a backwards extrapolation off the interval *above*
the energy, not an interpolation on the interval containing it.
Defect: not a design choice — the same module contains the correct version of the
same computation over the same table. `optics/opticsApp/src/filterDrive.st:288-298`
(`calcTrans`) uses the identical bracketing loop, then `if ((j < 1) | (j >= numEntries))
return 0.;` and interpolates on `[j-1, j]`. `pf4.st` has neither the `j-1` indexing
nor the `j < 1` guard. One of the two is wrong, and the one with `frac < 0` is it.
Port: `crates/optics-rs/src/data/chantler.rs:1258-1281` (`other_absorption_length_um`)
reproduces `[j, j+1]` and the negative `frac` deliberately (comment at `:1263-1270`).
The correct `[j-1, j]` form is kept separately as `interpolate_mu` (`:1231-1252`) for
the `filterDrive` consumer.
Impact: every `pf4` "Other"-material blade reports a wrong absorption length at every
energy that is not exactly a table node, so the published transmissions `xmit[i]` and
the ranked filter recommendation `bits[i]` are wrong. Against the shipped Chantler
table: **+0.7% to +3.5%** at ordinary energies, **+7.4%** just below a Pb absorption
edge. Al/Ti/Glass are unaffected (analytic fits, not the table) — so the bug is
confined to exactly the path an operator uses for any material the beamline actually
installed.
Proof — `proof_pb_real.c`, linked against the **real** `optics/opticsApp/src/chantler.c`:
```
Al  @   8.5 keV: pf4=      92.894 um  filterDrive=      89.733 um  err=  +3.52%
Ti  @   8.5 keV: pf4=      13.261 um  filterDrive=      12.852 um  err=  +3.19%
Pb  @  20.7 keV: pf4=      11.539 um  filterDrive=      11.301 um  err=  +2.11%
Si  @   8.5 keV: pf4=      83.351 um  filterDrive=      80.512 um  err=  +3.53%
Pb edge near node j=154, keV[j]=2.4815 (mu jumps 757.57 -> 8497.4)
  pf4.st = 1.24862 um   filterDrive = 1.16291 um   err = +7.37%
```

### CBUG-B2: `pf4.st` `OtherAbsorptionLength` reads `keV[j+1]`/`mu[j+1]` out of bounds for Pb
Bucket: NOT-REPRODUCED · Severity: Medium
C: `pf4.st:642-643`. After the guard `if (j >= filtermat[i].numEntries) return(0.);`
(`:637-639`), `j` can still be `numEntries - 1`, and C dereferences
`filtermat[i].keV[j+1]` / `.mu[j+1]` — index `numEntries`. The arrays are
`float keV[NUM_ENTRIES]` / `float mu[NUM_ENTRIES]` with `NUM_ENTRIES 274`
(`chantler.h:4,14-15`).
Defect: `chantler.c:189` gives **Pb** `numEntries = 274 == NUM_ENTRIES`, and Pb is
`filtermat[21]`, the **last** element. So for any energy in Pb's top bin, `keV[274]`
and `mu[274]` are genuine out-of-bounds reads. The guard C wrote (`j >= numEntries`)
is off by one for the `j+1` it then performs. `filterDrive.st` never has this problem
because it indexes `[j-1, j]`.
Port: `crates/optics-rs/src/data/chantler.rs:1222-1229` (`table_cell`) — deviates:
returns `0.0` past the end rather than reading OOB; the deviation is signed off at
`:1218-1221`. Note this makes the port diverge from C *in the Pb top bin specifically*.
Impact: because the struct is `{int Z; char *name; float density; int numEntries;
float keV[274]; float mu[274];}`, `&keV[274] == &mu[0]` exactly — C silently reads
Pb's *mass-attenuation coefficient* `mu[0] = 3.9317e-06` and uses it as an *energy in
keV*, and `mu[274]` reads 4 bytes past the end of `filtermat[]` entirely. The value is
garbage, not merely imprecise; that it currently lands near the right answer is an
accident of adjacent memory. Any recompilation, reordering, or ASan build changes or
traps it.
Proof — `proof_pb_real.c`:
```
filtermat[21] name=Pb numEntries=274 (NUM_ENTRIES=274)  <-- last elem: reads past filtermat[]
Pb keV[272]=405 keV[273]=432.95   mu[272]=0.21702 mu[273]=0.19265
OOB: &keV[274]==&mu[0] ? YES   value read as keV[274] = 3.9317e-06  (this is mu[0])
OOB: mu[274] read = 0  (past the end of filtermat[])
```

### CBUG-B3: `pf4.st` `RecalcFilters` divides by the glass absorption length without the `> 0` guard it applies to every other term — all 16 transmissions become NaN below 2 keV
Bucket: NOT-REPRODUCED · Severity: High
C: `pf4.st:695`: `xmit[i] *= exp(-xGlass*1000./absLenGlass);` — **unconditional**,
where `GlassAbsorptionLength` (`:560-562`) returns `0` for `keV < 2` ("this routine
only good above 2 keV").
Defect: an omission the file proves against itself — the four "Other" terms four lines
below are each guarded:
```
:695   xmit[i] *= exp(-xGlass*1000./absLenGlass);                     <-- NO guard
:696   if (xOther1 > 0) xmit[i] *= exp(-xOther1*1000./absLenOther1);  <-- guarded
:697   if (xOther2 > 0) ...
```
With **no glass blade in the beam** (`xGlass == 0`, the ordinary case for any bank
without a glass filter) the expression is `exp(-(0.0/0.0))` = `exp(NaN)` = **NaN**.
Port: `crates/optics-rs/src/snl/pf4.rs:214` — deviates:
`if thickness_mm <= 0.0 || energy_kev <= 0.0 { return 1.0; }` short-circuits before
the divide. That guard is precisely the one C omits.
Impact: for any `pf4` bank driven below 2 keV, **every one of the 16** combination
transmissions is NaN — including combinations with nothing in the beam, and banks with
no glass blade configured at all. `sortDecreasing` (`:709-745`) is a bubble sort whose
comparison `arr[jj] < arr[jj+1]` is false for every NaN, so the array is left
**completely unsorted** and the "best filter combination" the record recommends is just
combination 0. NaN transmissions and a meaningless recommendation, with no error, no
alarm, no diagnostic.
Proof — `proof_nan.c`:
```
== RecalcFilters :693-695, NO glass blade inserted (xGlass == 0) ==
  after Al,Ti terms:               xmit = 1
  -xGlass*1000./absLenGlass  =  -0/0 = -nan
  after the UNGUARDED glass term:  xmit = -nan   <-- NaN, with NO glass in the beam
== all 16 combinations at keV = 1.5 (Al+Ti blades only) ==
  xmit[] = -nan (x16)
  bits[] after sortDecreasing = 0 1 2 ... 15   <-- unsorted
```

### CBUG-B4: `pf4.st` an unknown "Other" material name, or an energy above the table, silently reports the blade as fully opaque
Bucket: NOT-REPRODUCED (port refuses — flipped 2026-07-20; see the 2026-07-20 reconciliation note) · Severity: Medium
C: `pf4.st:629-631` and `:637-639` — `OtherAbsorptionLength` returns `0.` both when
`strcmp` matches no species and when `j >= numEntries`. Both `printf` diagnostics that
would have reported it are **commented out** in the shipped source:
```
:629    if (i >= NUM_SPECIES) {
:630        /* printf("pf4.st: Filter material '%s' not found\n", species);*/
:631        return(0.);
```
`RecalcFilters:696` then evaluates `exp(-xOther1*1000./0.)` = `exp(-inf)` = `0.0`.
Defect: `0.` is used as a "no data" sentinel and then consumed as a *divisor* on a path
that cannot distinguish it from a real absorption length. Not an error, not an alarm,
not a no-op — the maximally wrong answer (perfectly opaque), delivered silently. A typo
in a material name is an entirely ordinary operator error.
Port: `crates/optics-rs/src/snl/pf4.rs:225-228` + `chantler.rs:1274` — reproduces; the
comment at `pf4.rs:221-224` states the intent.
Impact: an operator who mistypes a filter material (or configures one outside the
Chantler table, or runs above 433 keV) gets `xmit = 0.0` for every combination
containing that blade; `sortDecreasing` ranks those *last* rather than flagging them,
so the record confidently recommends a filter set computed from a blade it knows
nothing about. Indistinguishable from a genuinely opaque filter.
Proof — `proof_optics.c`:
```
OtherAbsorptionLength(10 keV, "Unobtainium") = 0
OtherAbsorptionLength(1e6 keV, "Al")         = 0
xmit after 0.5mm blade with absLen==0: exp(-500/0) = 0  <-- fully OPAQUE, silently
```

### CBUG-B5: `asynInterposeCom` `setOption("ixon", …)` forgets its `return asynError` — an invalid value sends an **uninitialized** byte as a telnet SET-CONTROL command and reports success
Bucket: NOT-REPRODUCED · Severity: High
C: `asyn/asyn/miscellaneous/asynInterposeCom.c:593-597`:
```c
:591        if      (epicsStrCaseCmp(val, "n") == 0) xBuf[1] = pinterposePvt->flow;
:592        else if (epicsStrCaseCmp(val, "y") == 0) xBuf[1] = CPO_CONTROL_IXON;
:593        else {
:594            epicsSnprintf(pasynUser->errorMessage, pasynUser->errorMessageSize,
:595                                                                  "Bad option value");
:596        }                          /* <-- NO `return asynError;` */
:597        status = sbComPortOption(pinterposePvt, pasynUser, xBuf, 2, rBuf);
```
`xBuf` is `char xBuf[5]` (`:479`), a plain uninitialized local; only `xBuf[0]` is set
(`= CPO_SET_CONTROL`, `:586`). On the `else` path `xBuf[1]` is whatever was on the
stack, and `sbComPortOption` transmits it.
Defect: an unambiguous omission, proved by the file against itself — the two sibling
branches either side of it *do* return: `parity` (`:536-540`) and `crtscts`
(`:577-580`, the immediately preceding, structurally identical branch) both end their
`else` with `return asynError;`. Only `ixon` drops it.
Port: `crates/asyn-rs/src/interpose/com.rs:924-934` — refuses the value
(`Err(asyn_error("Bad option value"))`), marked DEVIATION.
Impact: `asynSetOption("<port>", 0, "ixon", "1")` — an ordinary mistake, since several
other asyn options do take numbers — transmits
`IAC SB COM-PORT-OPTION SET-CONTROL <stack garbage> IAC SE` to the terminal server. The
SET-CONTROL value space is not just flow control: `CPO_CONTROL_BREAK_ON = 5` and
`CPO_CONTROL_BREAK_OFF = 6` (`:57-58`) live in the same byte, so a stack byte that
happens to be 5 **asserts a BREAK on the physical serial line** to the attached
instrument. And because `setOption` ends with `return status;` (`:654`), the caller is
told the option was **set successfully** while `errorMessage` says "Bad option value".
Proof: `:593-596` has no `return`; `:597` unconditionally calls
`sbComPortOption(…, xBuf, 2, …)`; `sbComPortOption:427` does `memcpy(cbuf+3, xBuf, 2)`
and `:430` writes `cbuf` to the device. `xBuf[1]` is never assigned on that path.

### CBUG-B6: `asynInterposeCom` can turn flow control **on** but never **off**
Bucket: NOT-REPRODUCED (port refuses — flipped 2026-07-20; see the 2026-07-20 reconciliation note) · Severity: Medium
C: `asynInterposeCom.c:575` and `:591` — both the `crtscts` and the `ixon` branch
implement "n" as `if (epicsStrCaseCmp(val, "n") == 0) xBuf[1] = pinterposePvt->flow;`
— the value transmitted for "turn this off" is the port's **current** flow-control mode.
Defect: `CPO_CONTROL_NOFLOW = 1` ("No flow control", `:53`) is defined, and is decoded
in `getOption` (`:684`, `:695`), but is **never assigned to `xBuf[1]` anywhere in the
file**. If `flow` is currently `CPO_CONTROL_HWFLOW`, `asynSetOption(port,0,"crtscts","N")`
re-transmits `SET-CONTROL HWFLOW`, the server confirms HWFLOW, `:578` writes it back
into `pinterposePvt->flow`, and `getOption("crtscts")` still answers `"Y"`. The disable
is a silent no-op.
Port: `crates/asyn-rs/src/interpose/com.rs:906-907` — reproduces; `CPO_CONTROL_NOFLOW`
is declared, used as the initial state, decoded in `get_option`, and — exactly as in C —
never transmitted.
Impact: once RTS/CTS or XON/XOFF has been enabled on an RFC-2217 terminal-server port,
no `asynSetOption` call can disable it. The operator sets `crtscts N`, gets
`asynSuccess`, reads back `"Y"`, and the hardware keeps asserting handshaking.
Recovery requires restarting the IOC or power-cycling the terminal server.
Proof — exhaustive enumeration of every `xBuf[1]` assignment in `setOption` (`:474-655`):
```
:491 xBuf[1] = baud >> 24;              :531-535 xBuf[1] = CPO_PARITY_*;
:517 xBuf[1] = b;                       :557 xBuf[1] = (char)b;
:575 xBuf[1] = pinterposePvt->flow;     :576 xBuf[1] = CPO_CONTROL_HWFLOW;
:591 xBuf[1] = pinterposePvt->flow;     :592 xBuf[1] = CPO_CONTROL_IXON;
:625 xBuf[1] = CPO_CONTROL_BREAK_ON;    :637 xBuf[1] = CPO_CONTROL_BREAK_OFF;
```
`CPO_CONTROL_NOFLOW` appears nowhere in that list.

### CBUG-B7: `asynInterposeCom` `nextChar` ignores `nbytes` — a zero-length successful read returns an uninitialized character
Bucket: NOT-REPRODUCED · Severity: Low
C: `asynInterposeCom.c:95-107`:
```c
:97     char c;
:99     size_t        nbytes;
:103    status = poct->read(pinterposePvt->drvOctetPvt, pasynUser, &c, 1, &nbytes, &eom);
:104    if (status != asynSuccess)
:105        return EOF;
:106    return c & 0xFF;
```
`nbytes` is declared, passed by address, and **never examined**.
Defect: the `asynOctet::read` contract reports the transfer count in `nbytes` precisely
because a call can succeed having moved zero bytes. C tests the wrong variable; on
`asynSuccess` with `nbytes == 0`, `c` is never written and `c & 0xFF` reads an
uninitialized automatic.
Port: `crates/asyn-rs/src/interpose/com.rs:245-252` — treats a 0-byte success as EOF
(signed off at `:239-242`).
Impact: a garbage byte enters telnet negotiation parsing — `nextChar` is the sole byte
source for `sbComPortOption`'s reply loop (`:434-455`) and `readIt`'s IAC-partner fetch
(`:217`). Consequence: a spurious "Missing IAC", a mis-parsed subnegotiation reply, or
— if the garbage equals `IAC` — a negotiation that appears to succeed while the server
said something else. Graded Low on reachability: no shipped `asynOctet` driver was
found that returns `asynSuccess` with `nbytes == 0` on a 1-byte read. It is still an
uninitialized read and the fix is one line.
Proof: `:103` writes `&nbytes`; no read of `nbytes` exists in the function; `:106`
returns `c` whenever `status == asynSuccess`.

### CBUG-B8: `asynInterposeCom` telnet negotiation bypasses its own IAC-stuffing, so a payload byte of 0xFF corrupts the subnegotiation
Bucket: NOT-REPRODUCED (port refuses — flipped 2026-07-20; see the 2026-07-20 reconciliation note) · Severity: Low
C: `asynInterposeCom.c:430-431` — the negotiation frame is written straight to the
driver **below** the interpose (`pinterposePvt->pasynOctetDrv->write`), not through this
interpose's own `writeIt` (`:146-182`), which is the function that doubles `C_IAC`
bytes. So the `xBuf` payload copied at `:427` is never IAC-stuffed.
Defect: RFC 2217 requires a 0xFF byte inside a subnegotiation payload to be escaped as
`IAC IAC`. The payload is exactly where a 0xFF can occur: `CPO_SET_BAUDRATE` sends the
baud rate as 4 big-endian bytes (`:491`), so any baud with a 0xFF octet — e.g. `255`
(`0x000000FF`) — puts a raw `IAC` in the payload and a compliant terminal server reads
it as a command byte, desynchronising the negotiation.
Port: `crates/asyn-rs/src/interpose/com.rs:598-608` — reproduces; named in the module
doc (`com.rs:30-33`).
Impact: `asynSetOption(port, 0, "baud", "255")` (or any baud whose big-endian encoding
contains 0xFF) emits a malformed subnegotiation; a strict RFC-2217 server mis-frames it
and the negotiation hangs or errors. Real baud rates in use (9600, 19200,
115200 = `0x0001C200`) contain no 0xFF octet, which is why this has never bitten
anyone — hence Low. It is still a wire-protocol violation.
Proof: `:430` calls the layer below; the IAC-doubling loop lives in `writeIt` at
`:146-182` and is not on this path. Same for the `IAC DO/WILL` frames at `:336-339`.

### CBUG-B9: `drvAsynIPServerPort` UDP read drops one byte per read **and** returns uninitialised/stale buffer bytes as received data
Bucket: NOT-REPRODUCED · Severity: High
C: `asyn/asyn/drvAsynSerial/drvAsynIPServerPort.c:196-200` (`readIt`):
```c
:196        for (x = 0; x < (int)maxchars - 1; x++) {
:197            data[x] = tty->UDPbuffer[x + tty->UDPbufferPos];
:198        }
:199        thisRead = (int)maxchars - 1;
:200        tty->UDPbufferPos = tty->UDPbufferPos + (int)maxchars;
```
`tty->UDPbufferSize` is the datagram length from `recvfrom` (`:311`) into a
`malloc(65507)` buffer (`:456`, `:83`).
Defect: three errors in five lines, and the loop bound and the position advance
disagree with each other, so neither can be the intended one:
1. **The copy is not bounded by the datagram.** The loop runs to `maxchars - 1` with no
   reference to `UDPbufferSize`. `maxchars` is the *caller's buffer size*, not the bytes
   received. Ordinary case — device support reading with a 256-byte buffer, a 10-byte
   datagram arrives — C copies 255 bytes: 10 real, then 245 bytes of the previous
   datagram's tail or, on the first datagram after connect, never-written `malloc`
   memory. All 255 are reported as received via `*nbytesTransfered = thisRead` (`:230`).
2. **Off-by-one loss.** The copy takes `maxchars - 1` bytes but `:200` advances
   `UDPbufferPos` by `maxchars`. One byte of every datagram is skipped, never delivered.
3. **`maxchars == 1` returns nothing and consumes a byte.** The loop body never runs,
   `thisRead = 0`, `UDPbufferPos` still advances by 1 — a byte-at-a-time reader makes no
   progress.
Port: `crates/asyn-rs/src/drivers/ip_server_port.rs:997-1013` — copies
`min(maxchars, remaining)` from the datagram and advances by the amount actually copied;
deviation signed off in the doc comment.
Impact: every asyn UDP server port hands its device support a buffer in which only the
first `min(datagramLen, maxchars-1)` bytes are real and the remainder is stale heap —
reported as if received. Any device support that trusts `nbytesTransfered` (rather than
scanning for an EOS terminator) parses garbage. On the first datagram after connect that
garbage is uninitialised `malloc` memory — an **information leak** into a
waveform/stringin record. Separately, one byte of every datagram is silently dropped.
Proof: `UDPbufferSize` is written only at `:311` and reset at `:202-203`/`:244-245`; in
`readIt` it is read **only** at `:201`, *after* the copy, purely to decide the EOM
reason. It never bounds the loop at `:196`.

### CBUG-B10: every `asyn*Base.c` `readDefault` reports "**write** is not supported" for a failed **read** (6 files)
Bucket: NOT-REPRODUCED (port refuses — flipped 2026-07-20; see the 2026-07-20 reconciliation note) · Severity: Low
C: `asyn/asyn/interfaces/asynInt32Base.c:81-84` (`readDefault`):
```c
:81     epicsSnprintf(pasynUser->errorMessage,pasynUser->errorMessageSize,
:82         "write is not supported");
:83     asynPrint(pasynUser,ASYN_TRACE_ERROR,
:84         "%s %d read is not supported\n",portName,addr);
```
The trace correctly says "read"; the `errorMessage` — the string the *caller* receives —
says "write". A copy-paste from `writeDefault` directly above (`:63-66`).
Defect: the two adjacent lines contradict each other, which is what makes it a slip.
Same shape in six files:

| file | `readDefault` errorMessage line |
|---|---|
| `asyn/asyn/interfaces/asynEnumBase.c` | `:79` |
| `asyn/asyn/interfaces/asynFloat64Base.c` | `:78` |
| `asyn/asyn/interfaces/asynGenericPointerBase.c` | `:77` |
| `asyn/asyn/interfaces/asynInt32Base.c` | `:82` |
| `asyn/asyn/interfaces/asynInt64Base.c` | `:82` |
| `asyn/asyn/interfaces/asynUInt32DigitalBase.c` | `:91` |

Port: `crates/asyn-rs/src/interfaces/gpib.rs:238-245` — reproduces, named in the comment.
Impact: a port that registers an interface with a NULL read method (the normal way to say
"this port is write-only") makes every failed read report `write is not supported`. That
string lands in `asynRecord`'s `ERRS` field and in device-support errors, so an operator
debugging a read failure is told the *write* is unsupported. Purely diagnostic — but it
actively misdirects the person debugging.
Proof: read both functions in any of the six files.

### CBUG-B11: `NDPluginCircularBuff` — writing **0** to `SoftTrigger` triggers the capture exactly like writing 1
Bucket: NOT-REPRODUCED (port refuses — flipped 2026-07-20; see the 2026-07-20 reconciliation note) · Severity: Medium
C: `ADCore/ADApp/pluginSrc/NDPluginCircularBuff.cpp:266-278` (`writeInt32`):
```c
:266    }  else if (function == NDCircBuffSoftTrigger){
:268        status = (asynStatus) setIntegerParam(function, value);
:271        setIntegerParam(NDCircBuffTriggered, 1);
:273        epicsInt32 flushOn;
:274        getIntegerParam(NDCircBuffFlushOnSoftTrig, &flushOn);
:276        if (flushOn > 0){
:277            flushPreBuffer();
:278        }
```
`value` is stored (`:268`) and then **never tested**. `NDCircBuffTriggered` is latched to
1 and the pre-buffer flushed on *every* write, whatever was written.
Defect: the plugin itself treats 0 as "not triggered" everywhere else — `:255-257`
explicitly clears both parameters as the way to *disarm*. So 0 unambiguously means "off"
in this plugin's own vocabulary, and the one place a user can write it is the one place
that ignores it. Writing 0 to disarm instead arms.
Port: `crates/ad-plugins-rs/src/circular_buff.rs:800-812` — reproduces, stated at
`:801-806`.
Impact: `caput $(P)$(R)SoftTrigger 0` — the natural way for an operator or a sequencer to
disarm between acquisitions — fires the trigger instead: latches `Triggered`, flushes the
pre-trigger ring downstream, starts post-trigger capture. The dataset is triggered at the
wrong moment, and an autosave/`PINI` restore of `SoftTrigger=0` **arms the plugin on IOC
boot**.
Proof: `:268` is the only use of `value` in the branch; `:271` and `:276-278` are
unconditional on it. (The `flushOn > 0` gate at `:276` is *correct* C — see the
correction to R11-63 below.)

### CBUG-B12: pvxs `ackAt == 0` is overloaded as "caller said nothing", so a small `ackAny` percentage acks **later** than a larger one
Bucket: NOT-REPRODUCED (port refuses — flipped 2026-07-20; see the 2026-07-20 reconciliation note) · Severity: Low
C: `pvxs/src/servermon.cpp:564` and `:577-578` (`ServerMonitorSetup::onSetup`, pipeline branch):
```c++
:564            op->ackAt = std::max(0.0, std::min(percent, 100.0)) / 100.0 * op->limit;
:577    if(op->ackAt==0u){
:578        op->ackAt = op->limit/2u;
```
`op->ackAt` is `uint32_t`; `op->limit` defaults to `4u` (`servermon.cpp:66`). `:564`
truncates toward zero; `:577` then treats the resulting 0 as "the client supplied no
`ackAny`" and overwrites it with `limit/2`.
Defect: the sentinel cannot distinguish "no `ackAny` in the pvRequest" from "`ackAny`
computed to 0", and after `:564` the latter is the *common* case — with the default limit
of 4, every percentage below 25% truncates to 0. The mapping from requested percentage to
ack threshold is therefore **non-monotonic**: `ackAny="25%"` acks at 1, `ackAny="10%"`
acks at 2 — a client asking to acknowledge *more* eagerly gets a *lazier* threshold.
`ackAny="0%"` is not expressible at all.
Port: `crates/epics-pva-rs/src/server_native/tcp.rs:202` + `:219-221` — reproduces.
Impact: a pipelined PVA monitor client asking for a fine-grained ACK cadence
(`ackAny = "10%"`, or any percentage under `100/limit`) silently gets `limit/2` — coarser
than it asked for, and coarser than it would have got by asking for a *larger*
percentage. The flow-control window errs toward *less* back-pressure, the unsafe direction
for a slow client.
Note for the record: an earlier internal note filed this as a **NaN** issue. It is not.
Compiled C++ confirms libstdc++'s `std::max(0.0, std::min(NaN, 100.0))` is **0.0** (both
`std::min`/`std::max` return their first argument when the comparison is false, and every
NaN comparison is false). pvxs and the port agree on NaN. The real defect is the `== 0`
sentinel, reachable from ordinary inputs.
Proof — `proof_ackany.cpp` (g++/libstdc++):
```
std::max(0.0, std::min(NaN,100)) = 0   <-- 0.0, NOT 100.0
percent=25   limit=4 -> ackAt=1  -> final=1
percent=24   limit=4 -> ackAt=0  -> final=2    <-- CLOBBERED by the ==0 default
percent=10   limit=4 -> ackAt=0  -> final=2    <-- CLOBBERED
percent=0    limit=4 -> ackAt=0  -> final=2    <-- CLOBBERED
NON-MONOTONIC: ackAny="25%" -> 1 ; ackAny="10%" -> 2
```

### CBUG-B13: `motorRecord` publishes CDIR=forward after a jog-stop backlash take-out that actually commands the reverse direction
Bucket: NOT-REPRODUCED (port refuses — flipped 2026-07-20; see the 2026-07-20 reconciliation note) · Severity: Medium
C: `motor/motorApp/MotorSrc/motorRecord.cc:827-829`, `:845`, `:973` (`postProcess`). The
"sync drive to readback" block runs for every MIP except `{MOVE, MOVE_BL, JOG_BL1,
JOG_BL2}` — the predicate does **not** exclude `MIP_JOG_STOP`. Inside it,
`pmr->diff = 0.;` (`:845`). The JOG_STOP arm then computes `relpos = pmr->diff / pmr->mres`
(`:923`, now 0), dispatches the take-out leg via `WRITE_MSG(MOVE_REL, &relbpos)` /
`WRITE_MSG(MOVE_ABS, &bpos)` (`:943`/`:945`, toward `dval - bdst`), and finally sets
`pmr->cdir = (relpos < 0.0) ? 0 : 1;` (`:973`).
Defect: CDIR is derived from `relpos`, which `:845` has just forced to 0, so `(0 < 0.0)`
is false and `cdir = 1` **unconditionally** — regardless of the sign of the stroke
actually commanded. Not a convention: the sibling arms are self-consistent (the `MIP_MOVE`
arm is *excluded* from the `:827` sync so its `relpos` is live at `:973`; the
fractional-retry arm re-derives `relpos` at `:960`). Only JOG_STOP zeroes the value it then
keys CDIR on.
Port: `crates/motor-rs/src/record/state_machine.rs:813-817` — reproduces verbatim.
Impact: jog an axis in reverse, release, with `BDST > 0` and `|BDST| >= |MRES|`. The record
commands the backlash take-out in the negative direction but publishes `CDIR = 1`
(forward). Downstream: `:3731` `ls_active = (rhls && cdir) || (rlls && !cdir)` fails to
recognise the *minus* limit switch as active during the move; `:1368`/`:1405` miss the
limit-switch re-arm and the skip-retry gate, so the record **retries into a pressed reverse
limit** until `RCNT > RTRY` and latches `MISS = 1`; `:1047` `maybeRetry` takes the wrong
no-retry branch.
Proof: `:827` predicate omits `MIP_JOG_STOP` → `:845` `diff = 0` → `:923` `relpos = 0` →
`:973` `(0 < 0.0)` false → `cdir = 1`, independent of `bdst`'s sign.

### CBUG-B14: `throttleRecord` `delayFuncCallback` mutates the record and fires OUT/FLNK links with no `dbScanLock`
Bucket: NOT-REPRODUCED · Severity: High
C: `std/stdApp/src/throttleRecord.c:530-538` — `callbackGetUser(prec, pcallback);
valuePut(prec);` with **no** `dbScanLock`. `valuePut` (`:540-613`) writes `wait_flag`,
`delay_flag`, `prec->sts/sent/wait`, calls `dbPutLink(&prec->out,…)` (`:562`),
`recGblFwdLink(prec)` (`:580`), `recGblResetAlarms` (`:605`) and `db_post_events`. It is
armed from `process()` via `callbackRequestDelayed`, so it runs on a callback thread while
`process()` runs on a scan thread holding the record lock.
Defect: EPICS requires the record lock for field mutation, `dbPutLink` and `recGblFwdLink`.
The *same file* proves the omission is a slip: its other callback, `checkLinkCallback`
(`:675-678`), does `dbScanLock(...); checkLink(prec); dbScanUnlock(...)`. Only
`delayFuncCallback` skips it.
Port: `crates/std-rs/src/records/throttle.rs:509-560` — structurally avoids it: the timer
is a `ProcessAction::ReprocessAfter`, so the drain re-enters `process()` under the
framework's record lock.
Impact: a throttle whose DLY window overlaps an incoming write is a data race with
observable loss: `process()→enterValue()` reads `delay_flag` (`:525`) while the callback's
`valuePut()` is concurrently clearing it (`:597`). `enterValue` sees the stale
`delay_flag == 1` and returns without writing; the callback already passed its `wait_flag`
test — so the value sits in `prec->val` with `wait_flag = 1`, **no OUT write, no FLNK, no
timer re-armed**, and the throttle stalls until the next unrelated process. Torn writes to
`sts`/`sent` and an unlocked `dbPutLink`/`recGblFwdLink` into another record's lockset are
the same defect's other faces (memory-unsafe on SMP).
Proof: `:530-538` has no lock; `:675-678` in the same file locks its callback.
`callbackRequestDelayed` schedules on the general callback thread pool, distinct from the
scan thread that runs `process()`.

### CBUG-B15: `epidRecord` raises the UDF alarm but returns before committing it — STAT/SEVR stay NO_ALARM, then leak INVALID one cycle late
Bucket: NOT-REPRODUCED · Severity: Medium
C: `std/stdApp/src/epidRecord.c:195-202` (`process`) — `if (pepid->udf == TRUE) {
recGblSetSevr(pepid,UDF_ALARM,pepid->udfs); return(0); }`. This early return is above
`checkAlarms` (`:210`) and `monitor` (`:211`), and `monitor` (`:351`) holds the file's
**only** call to `recGblResetAlarms`.
Defect: `recGblSetSevr` writes only `nsta`/`nsev` (the *pending* alarm). `recGblResetAlarms`
(base `recGbl.c:178-210`) is the sole owner that copies `nsta/nsev → stat/sevr`, posts them,
and clears the pending pair. Returning before it means the UDF alarm the record just raised
is never published, and the pending INVALID severity latches (a second `recGblSetSevr` with
the same severity is a no-op).
Port: `crates/std-rs/src/records/epid.rs:200-207` — the framework's centralised
`rec_gbl_check_udf` runs after `process()` and commits/posts the severity, so the Rust
record has no inverted commit.
Impact: an epid that becomes UDF (unconnected/`MS` STPL, or SMSL supervisory with an
unwritten VAL) advertises **`.SEVR = NO_ALARM / .STAT = NO_ALARM`** to CA clients and alarm
handlers on every cycle it is undefined — an undefined controller reads as healthy. When
STPL finally connects and `udf` clears, the first `monitor()` commits the stale latched
INVALID, so the record reports UDF/INVALID for exactly one cycle *after* it became valid.
Proof: `epidRecord.c` contains exactly one `recGblResetAlarms` (`:351`, inside `monitor`);
the `:201 return(0)` is above both `checkAlarms` and `monitor`.

### CBUG-B16: `devEpidSoft` "nothing to control" abort falls through when the severity is already INVALID — the PID runs and drives the output on stale input
Bucket: NOT-REPRODUCED · Severity: Medium
C: `std/stdApp/src/devEpidSoft.c:110-116` (`do_pid`) — `if (pepid->inp.type == CONSTANT) {
if (recGblSetSevr(pepid,SOFT_ALARM,INVALID_ALARM)) return(0); }`.
Defect: the `return(0)` is gated on `recGblSetSevr`'s return, which is nonzero only when the
severity is *raised*. If `nsev` is already `INVALID_ALARM` on entry — reachable when an `MS`
STPL link's source is INVALID, since `epidRecord.c:192`'s `dbGetLink` propagates that into
`nsev` *before* `do_pid` is called — `recGblSetSevr` returns 0, the `return(0)` is skipped,
and control falls into the PID body despite the CONSTANT test and the comment ("nothing to
control") saying the abort should be unconditional. `dbGetLink` on a CONSTANT link succeeds
writing nothing, leaving `cval` stale.
Port: `crates/std-rs/src/device_support/epid_soft.rs:54-59` — unconditional return.
Impact: an epid with a constant/unconnected INP, entered already-INVALID, computes
`e = setp - cval` against a **stale** `cval`, integrates it, and does
`dbPutLink(&pepid->outl,…)` (`:220-224`) — **driving the real output** from a phantom error
signal — where the intended behaviour is to flag SOFT/INVALID and write nothing.
Proof: `recGblSetSevr` returns nonzero only on a severity *increase*; `nsev ==
INVALID_ALARM` on entry → returns 0 → no `return(0)` → `:117-224` execute, including the
OUTL put.

### CBUG-B17: `throttleRecord` writes CA-link status flags for the wrong link (two sites)
Bucket: NOT-REPRODUCED · Severity: Low
C: two sites in `std/stdApp/src/throttleRecord.c`. (1) `:364-373` (`special`, shared
OUT/SINP case) — the "PV not on this IOC" branch always does `prpvt->outLinkStat =
CA_LINK_NOT_OK;`, even when the field being written is **SINP**; `sinpLinkStat` is never set
here. (2) `:687-743` (`checkLink`) — `int caLink = 0, caLinkNc = 0;` are declared **outside**
the `for (i=0; i<2; i++)` loop (`:698`) and never reset per iteration, so the `i==1` (SINP)
pass inherits the `i==0` (OUT) pass's state and `:734-739` writes SINP's `*plinkStat` from
OUT's connection state.
Defect: wrong-variable / stale-variable writes, not policy — the loop deliberately re-points
`plinkStat` per link and `special()` deliberately re-points `plink`/`plinkValid` per field,
then hard-codes `outLinkStat`.
Port: no equivalent — the Rust throttle has no `outLinkStat`/`sinpLinkStat` pair; link
connection state is framework-owned.
Impact: a `caput` to `.SINP` naming an off-IOC PV marks the **OUT** link "not connected"
instead of SINP; and a disconnected OUT link makes `checkLink` report SINP as
`CA_LINK_NOT_OK` even when SINP is connected or not a CA link. Both corruptions land in the
diagnostic link-status flags; `outLinkStat` self-heals next process.
Proof: `:371` is inside the branch reached for `fieldIndex == throttleRecordSINP`;
`:687-688` declarations are outside the loop opened at `:698`.

### CBUG-B18: `scalerRecord` `special(RATE)` posts `.TP` — a field the write never touched — and never posts the clamped RATE
Bucket: NOT-REPRODUCED (port refuses — flipped 2026-07-20; see the 2026-07-20 reconciliation note) · Severity: Low
C: `scaler/scalerApp/src/scalerRecord.c:690-693` (`special`) — `case scalerRecordRATE:
pscal->rate = MIN(60.,MAX(0.,pscal->rate)); db_post_events(pscal,&(pscal->tp),DBE_VALUE);
break;` The clamp writes `rate`; the post passes `&pscal->tp`. Second site of the same
copy-paste at `:320-323` (`init_record`): sets `pscal->tp = 1.0;` then posts `&pscal->pr1`.
Defect: the RATE case posts a field it did not modify and never posts the field it did.
Every other `special()` case in the file posts exactly the fields it changes (`:672-676`,
`:681-686`, `:703-706`, `:717-719`) — a slip, not a convention.
Port: `crates/scaler-rs/src/records/scaler.rs:1019-1027` — reproduces deliberately.
Impact: `caput scaler.RATE 100` clamps the internal value to 60, but every CA client
subscribed to `.RATE` keeps displaying **100** until something else posts it, while every
`.TP` subscriber gets a spurious no-change event. The `init_record` site is benign (runs
before any monitor exists).
Proof: `:691` writes `pscal->rate`; `:692` passes `&(pscal->tp)` to `db_post_events`.

### CBUG-B19: `scalerRecord` `monitor()` computes the alarm monitor mask, then posts with a hard-coded `DBE_LOG` and discards it
Bucket: NOT-REPRODUCED (port refuses — flipped 2026-07-20; see the 2026-07-20 reconciliation note; the fix is inert until `do_alarm()` is ported — see body) · Severity: Low
C: `scalerRecord.c:758-773` — `monitor_mask = recGblResetAlarms(pscal); monitor_mask |=
(DBE_VALUE|DBE_LOG);` then the only post in the function is
`for (i=0;i<pscal->nch;i++) db_post_events(pscal,&(pscaler[i]),DBE_LOG);` — a **literal**
`DBE_LOG`, not `monitor_mask`. `monitor_mask` is assigned, OR-ed, and never read.
Defect: the two lines building the mask are dead; their only plausible use was as the third
`db_post_events` argument. `recGblResetAlarms` returns the alarm-transition mask
(`DBE_ALARM`) that every other record OR-s into its value posts; discarding it drops the
alarm bit.
Port: `crates/scaler-rs/src/records/scaler.rs:1296-1320` — reproduces C's literal
`DBE_LOG`-only sweep deliberately.
Impact: a client subscribed to `scaler.S1..Sn` with a `DBE_ALARM` mask receives **nothing**
on an alarm-severity transition of the scaler record. Only archivers (`DBE_LOG`) see this
sweep; the value path is separately served by `updateCounts` (`:580-583`), which is why it
is Low.
Proof: `:764`/`:766` assign `monitor_mask`; the sole `db_post_events` (`:771`) passes literal
`DBE_LOG`; `monitor_mask` has no other use in the function.

### CBUG-B20: `NDPluginROIStat` writes ROI geometry out of bounds for any array with more than 2 dimensions (RGB)
Bucket: NOT-REPRODUCED · Severity: High
C: `ADCore/ADApp/pluginSrc/NDPluginROIStat.cpp:216-220` and `:241-245`
(`processCallbacks`) — the rank guard `if ((pArray->ndims < 1) || (pArray->ndims > 2)) {
asynPrint(...); }` only prints, with **no `return`**. Execution continues to
`for (dim=0; dim<pArray->ndims; dim++) { pROI->offset[dim] = …; pROI->size[dim] = …;
pROI->arraySize[dim] = …; }`.
Defect: `NDROI_t` (`NDPluginROIStat.h:72,73,80`) declares `size_t offset[2]; size_t size[2];
size_t arraySize[2];`. For a 3-dimensional array (any `NDColorModeRGB1/2/3` frame) `dim`
reaches 2, so index 2 of every 2-element array is written. `arraySize` is the last member of
the struct, so `arraySize[2]` is 8 bytes past the `NDROI` object, and for `roi ==
maxROIs_-1` past the `new NDROI[maxROIs_]` allocation (`:209`). **The guard the author wrote
diagnoses the exact case it then fails to prevent.**
Port: `crates/ad-plugins-rs/src/roi_stat.rs:222` + `:366` — `clamp_roi_geometry` iterates
`0..ndims.min(2)` and `process_array` gates on `ndims == 1 || ndims == 2`. The OOB write is
unrepresentable.
Impact: enabling any ROI on a colour (RGB1/2/3) detector — an entirely ordinary
configuration — corrupts the adjacent `NDROI` (offset[2] aliases size[0], size[2] aliases
bgdWidth) and, on the last ROI, heap metadata past the array: wrong stats and a likely crash
in `delete[] pROIs` (`:325`). Memory-unsafe, reachable from a normal detector setup.
Proof: `:209` `new NDROI[maxROIs_]` → a 3-D array → `:216` guard prints only, no return →
`:241` `for (dim=0; dim<3; dim++)` writes index 2 of every `[2]` array.

### CBUG-B21: `NDPluginAttrPlot` off-by-one (`<=`) lets the attribute list grow one past its buffer count, then writes a circular buffer out of bounds
Bucket: NOT-REPRODUCED · Severity: High
C: `ADCore/ADApp/pluginSrc/NDPluginAttrPlot.cpp:162-164` (`rebuild_attributes`) — the
discovery loop condition is `attr != NULL && attributes_.size() <= n_attributes_`.
`push_data` (`:244`, `:262-263`) then does `size_t length = attributes_.size(); for (i <
length) data_[i].push_back(...)`.
Defect: the guard tests size **before** appending, so when `attributes_.size() ==
n_attributes_` the condition `n <= n` is still true, the body runs, and another attribute is
pushed — leaving `attributes_.size() == n_attributes_ + 1`. But `data_`
(`NDPluginAttrPlot.h:207`, `std::vector<CB>`) is filled with exactly `n_attributes_`
circular buffers in the ctor (`:72-75`). `length` therefore reaches `n_attributes_ + 1`, and
`data_[n_attributes_]` is an out-of-bounds `operator[]` on the vector — a `push_back`
through a `CircularBuffer` constructed from wild memory. The condition should be `<`.
Port: `crates/ad-plugins-rs/src/attr_plot.rs:206-207` — `names.truncate(self.n_attributes);`
caps the tracked list at exactly `n_attributes`, and `buffers` is sized to match.
Impact: any IOC whose NDArrays carry more numeric attributes than the plugin's configured
`n_attributes` — a routine mismatch the moment a detector adds an attribute — takes a heap
out-of-bounds write on the **first frame**, through a `std::vector` living in uninitialised
memory: corruption or crash.
Proof: `n_attributes_ = 4` → `data_` holds 4 CBs → a frame with ≥5 numeric attrs → `:163`
keeps looping while `size() <= 4`, reaching `size() == 5` → `push_data` `length = 5` →
`:263` `data_[4].push_back(...)` on a 4-element vector.

### CBUG-B22: `NDPluginProcess` divides by `numFiltered` without guarding `NumFilter == 0`
Bucket: NOT-REPRODUCED · Severity: Medium
C: `ADCore/ADApp/pluginSrc/NDPluginProcess.cpp:213-218` (`doProcess`) — `if
(this->numFiltered < numFilter) this->numFiltered++;` then `O1 = oScale*(oc1 +
oc2/this->numFiltered); … F2 = fScale*(fc3 + fc4/this->numFiltered);`.
Defect: `numFiltered` is incremented only while `< numFilter`. `NumFilter` is a
user-writable PV (`Db/NDProcess.template`, **no DRVL**). With `numFilter == 0`, the reset
path sets `numFiltered = 0` (`:210`), the guard `0 < 0` is false, and `:215-218` divide by
zero on every frame.
Port: `crates/ad-plugins-rs/src/process.rs:929` — the `NUM_FILTER` write is clamped
(`.max(1)`), so the divide never sees a zero denominator.
Impact: setting `NumFilter = 0` (reachable from any CA/autosave write) makes `O1/O2/F1/F2`
inf/NaN, so every processed output element **and the persistent filter buffer** become NaN —
every output frame is garbage and the filter stays poisoned until a manual reset.
Proof: `NumFilter = 0` → `:200` auto-reset arms → `:210` `numFiltered = 0` → `:213` no
increment → `:215` `oc2/0`.

### CBUG-B23: `NDPluginProcess` AutoOffsetScale divides by (max−min) with no guard against a uniform frame
Bucket: NOT-REPRODUCED · Severity: Medium
C: `NDPluginProcess.cpp:238-241` (`doProcess`) — `double maxScale =
pow(2.,bytesPerElement*8)-1; scale = maxScale/(maxValue-minValue);`
Defect: the `nElements == 0` case is handled (`:160-163`), but a **uniform frame**
(`minValue == maxValue`, every pixel identical) is not. Its denominator is 0.
Port: `crates/ad-plugins-rs/src/process.rs:238-239` — `if range > 0.0 { … }` skips the whole
scale/offset arm for a uniform frame.
Impact: a dark, saturated, or shutter-closed frame (all pixels equal — common at start-up or
on a closed shutter) makes `scale = +inf`, which is **latched into the `Scale` PV** (`:243`)
with `EnableOffsetScale` forced on (`:247`). Every subsequent frame is then multiplied by
inf and clipped: the auto-scale is permanently ruined until an operator intervenes.
Proof: a uniform image leaves `minValue == maxValue` after the min/max scan (`:167-168`) →
`:241` `maxScale/0`.

### CBUG-B24: `modbus` ASCII-serial LRC check sums the LRC byte into itself and compares against an undecoded byte one past the frame
Bucket: NOT-REPRODUCED · Severity: Medium
C: `modbus/modbusApp/src/modbusInterpose.c:423-434` (`readIt`, ASCII branch) —
`for (i=0; i<(nbytesActual-1)/2; i++) { decodeASCII(pin, &data[i]); pin+=2; }` decodes
**every** hex pair — slave + PDU + the trailing LRC byte — into `data[0..i-1]`. Then
`nRead = i;` `computeLRC(data, (int)nRead, &LRC);` `if (LRC != data[i]) { … return
asynError; }`.
Defect: two errors compound. (1) `computeLRC(data, nRead, …)` sums over `data[0..nRead-1]`,
which **includes** the received LRC byte; the LRC must be computed over slave + PDU only.
(2) the comparison reads `data[i]` where `i == nRead`, but the decode loop only wrote
`data[0..nRead-1]` — `data[nRead]` was never written by this call. Both operands of the
integrity check are wrong: the computed value folds in the byte it should exclude, and the
"received" value is an undecoded buffer byte past the frame. The RTU and TCP/UDP paths
(CRC / MBAP) are correct; only the ASCII path is broken.
Port: `crates/modbus-rs/src/interpose.rs:245-260` — LRC computed over slave + data only,
compared against the frame's actual last byte.
Impact: on any Modbus **ASCII-over-serial** link the LRC frame-integrity check is
meaningless — a mis-computed checksum against a garbage byte. Valid frames can be spuriously
rejected (`asynError`, retry/timeout churn) and **corrupt frames can pass undetected** into
the record, which is the worse direction. Reachable on every ASCII-serial Modbus device.
Proof: `:428 nRead = i` (i counts the LRC byte); `:429 computeLRC(data, nRead, …)` sums index
`nRead-1`, the LRC; `:430 if (LRC != data[i])` with `i == nRead` reads one past the decoded
range.

### CBUG-B25: `NDPluginTimeSeries` truncates the accumulated sum to the narrow element type **before** dividing — integer averaging is corrupted
Bucket: NOT-REPRODUCED (fixed upstream ADCore #596, merged 2026-07-16) · Severity: Medium
C: `ADCore/ADApp/pluginSrc/NDPluginTimeSeries.cpp:191` (`doTimeSeriesT<epicsType>`) —
`pTimeCircular[signal*numTimePoints_ + currentTimePoint_] =
(epicsType)averageStore_[signal]/numAveraged_;`
Defect: C++ precedence binds the cast tighter than the divide, so this parses as
`((epicsType)averageStore_[signal]) / numAveraged_`. `averageStore_` is a `double`
accumulator holding the **sum** of `numAveraged_` samples; casting that sum to the narrow
element type truncates and wraps it *before* the division. The intended computation is
`(epicsType)(averageStore_[signal] / numAveraged_)` — divide first, then narrow. The
parentheses are simply in the wrong place.
Port: `crates/ad-plugins-rs/src/time_series_plugin.rs` (`averaged_value`) — **refuses**
the bug: `sum / num_averaged` divides *first*, then narrows, so an in-range mean
narrows by an ordinary truncation. Changed to divide-first in `d8f27b88`
(2026-07-13), which pre-dated the upstream fix; ADCore **#596** (merged
2026-07-16) applies the same divide-then-narrow, so the port now matches current
upstream C. (The 2026-07-13 catalogue extraction recorded this entry as
REPRODUCED because the port still narrowed-before-dividing at extraction time,
hours before `d8f27b88`; that classification was stale by that evening.)
Impact: any integer-typed signal source with averaging enabled (`TSAveragingTime >
TSTimePerPoint`, so `numAveraged_ > 1`) produces wrong averaged points whenever the running
sum exceeds the element type's range — which it routinely does: three UInt8 samples of 200
sum to 600, wrap to 88, divide to **29 instead of 200**. Every averaged integer TS point is
wrong; float signals are unaffected.
Proof: `:191` — the cast `(epicsType)averageStore_[signal]` is a complete operand;
`/numAveraged_` applies to the already-narrowed value.

### CBUG-B26: `NDPluginStats` broadcasts an uninitialized `NDStats_t` — dark frames publish stack garbage to Sigma/Skew/Kurtosis/Eccentricity PVs and the time-series waveform
Bucket: NOT-REPRODUCED · Severity: High
C: `ADCore/ADApp/pluginSrc/NDPluginStats.cpp:430` — `NDStats_t stats, *pStats=&stats, …;` is
a plain POD local (`NDPluginStats.h`, no constructor), never `memset`. `:555-576` then copies
**every** field of `pStats` — including `sigmaXY`, `skewX/Y`, `kurtosisX/Y`, `eccentricity`,
`orientation` — into the broadcast time-series NDArray **unconditionally**.
Defect: those central-moment fields are assigned only inside `if (M00 > 0.)` (`:243-285`). A
frame whose every pixel is below `CentroidThreshold` (a dark frame, closed shutter, or
below-threshold illumination) leaves `M00 == 0`, the block is skipped, and the fields are read
uninitialized at `:570-576` (and again at the RBV-parameter copies around `:604-609`). With
`ComputeCentroid = 0` the centroid basics are never computed either.
Port: `crates/ad-plugins-rs/src/stats.rs:19,75,104` — the stats structs derive `Default` and
the centroid path yields `CentroidResult::default()` when `M00 == 0`. A field never assigned
is its `Default`; the garbage broadcast is unrepresentable.
Impact: `SigmaXY_RBV`, `SkewX/Y_RBV`, `KurtosisX/Y_RBV`, `Eccentricity_RBV`,
`Orientation_RBV` and the corresponding time-series waveforms carry **stack garbage** —
run-to-run varying, possibly NaN/inf — on any dark or below-threshold frame, or whenever
centroid computation is disabled. Archives are corrupted and alarm thresholds on those PVs
fire spuriously. Dark frames are routine (shutter closed, between exposures), so this is
reachable in normal operation.
Proof: `:430` no initializer; `:243` gates the moment writes on `M00 > 0.`; `:570-576` read
them unconditionally.

### CBUG-B27: `NDPluginStats` histogram divides by `(histMax − histMin)` with no guard — equal limits give `(int)NaN`, undefined behaviour
Bucket: NOT-REPRODUCED · Severity: Medium
C: `NDPluginStats.cpp:42,48` (`doComputeHistogram`) — `scale = (pStats->histSize - 1) /
(pStats->histMax - pStats->histMin);` then `bin = (int)(((value - pStats->histMin) * scale) +
0.5);`
Defect: `histMin`/`histMax` are user-writable PVs with no enforcement that `histMax >
histMin`. When equal, the denominator is 0, so `scale` is `±inf`; then for a pixel equal to
`histMin`, `(value - histMin) * scale` is `0 * inf = NaN`, and `(int)NaN` is **undefined
behaviour**. The sibling `computeHistX` clamps its divisor (`:657`); this routine does not.
Port: `crates/ad-plugins-rs/src/stats.rs:697` — `if hist_size == 0 || hist_max <= hist_min {
return …; }` guards the equal and inverted cases before any divide. (Even without the guard,
Rust's `f64 as usize` saturates rather than invoking UB.)
Impact: an operator (or an autosave restore) setting `HIST_MIN == HIST_MAX` — a natural
mistake when configuring a narrow histogram window — routes every pixel through
`(int)NaN`/`(int)inf` UB. In practice the histogram silently comes out empty or garbage, and
the behaviour is compiler- and optimization-dependent. No error, no alarm.
Proof: `:42` divides with no prior guard; `:48` feeds the `inf`/`NaN` `scale` into `(int)(…)`.

---

### Leads examined and REJECTED (not filed)

Recorded so a later pass does not re-litigate them.

- **`ATAN2` argument order** (`calcPerform.c:224`, `atan2(top, *ptop)`, with C's own comment
  `/* Ouch!: Args backwards! */`). NOT a defect: `calcRecord.dbd.pod:230` documents it
  exactly — `ATAN2 (den, num)`, "Arg's are reversed to ANSI C". A documented quirk. The port
  reproduces it faithfully.
- **`-2**2 == 4` precedence.** Unary minus binds tighter than `**` in the calc grammar. That
  is the documented grammar, a design choice.
- **sCalc `strncpy(dst, src, SCALC_STRING_SIZE)` non-termination** (`sCalcPerform.c:872,931`,
  `local_string[40]`). A real overflow *shape*, but a 40-char non-null-terminated input could
  not be shown reachable through the record layer (DBF string fields terminate at 39). Left
  out rather than invented; flagged for a future pass with the record layer built.
- **`NDPluginOverlay::setPixel` float→int cast** (`NDPluginOverlay.cpp:49-53`). Out-of-range
  float→int *is* UB, but XOR draw mode on a float image is nonsense by construction and no
  bounded pixel value reaches the UB on any real target. The "UB that happens to work" case;
  no reachable IOC configuration was constructed.
- **mqtt `stringWrite` "drops the last character of every string payload".** REJECTED as
  unproven. `drvMqtt.cpp:714-715` builds `std::vector<char> stringData(value.maxSize())` and
  Autoparam's `Octet::writeTo` (`autoparamHandler.h:271-274`) copies `min(size(),
  maxSize()-1)` bytes then NUL-terminates — which drops a character only if `size() ==
  maxSize()`. No write path was found that delivers an Octet with `size() == maxSize()`.
- **Unchecked `pNDArrayPool->alloc()`/`convert()` derefs** (`NDPluginStats.cpp:549-550`,
  `NDPluginProcess.cpp:294-295,306-307`) — crash only on pool exhaustion at a user
  `maxMemory` limit. Medium latent; the port's alloc-failure classification was not confirmed,
  so held rather than filed.
- **`NDPluginAttrPlot.cpp:117-119`** `std::fill(…, *(tmp_arr + n_copied - 1))` reads
  `tmp_arr[-1]` when the cache is empty (startup / post-`AP_Reset`) — a real underflow, but
  benign on real targets and the port side was not closed. Named for a follow-up pass.
- **`NDPluginFFT.cpp:249,290,309-314`** (freqStep divide-by-zero on the default
  `timePerPoint_ == 0`; `fftPvt_t` leak on the ndims-not-1/2 early return) — plausible, port
  guard status unverified. Named for a follow-up pass, not asserted as proven.

### Corrections this catalogue forces on findings recorded above

Three entries in this document's port-side inventory were **wrong about the C**, and the
compiled-C work behind this catalogue overturns them. They are corrected here rather than
silently edited above.

- **R10-49** ("asynRecord passes QUEUE_TIMEOUT=10.0 where its own comment implies otherwise")
  is **not** a C defect. `asynManager.c:1590-1595` rejects a `queueRequest` with `timeout >
  0.0` and no `timeoutUser`, and all four of asynRecord's asynUsers register one
  (`asynRecord.c:307-308`, `:531-533`, `:1274-1275`, `:1291-1292`). The C is self-consistent.
  R10-49 was a genuine *port* gap (no queue-timeout mechanism existed), since fixed. Nothing
  to report upstream.
- **R11-63** ("NDPluginCircularBuff gates on `flushOnSoftTrig != 0`, so a negative value arms
  the trigger") has its **premise inverted**. The C is `if (flushOn > 0)`
  (`NDPluginCircularBuff.cpp:276`), so a negative value does *not* flush — there is no defect
  at that gate. Re-reading the function surfaced the real defect two lines above it, filed
  here as **CBUG-B11**.
- **R6-62** (`polint` / `tableRecord.c` 1-based `ns`) — the C's Neville tableau
  (`optics/opticsApp/src/tableRecord.c:1918,1934,1945`) is the standard Numerical Recipes
  1-based formulation and is **correct**. The divergence was in the port's 0-based
  translation (`saturating_sub` clamping at 0), since fixed. Nothing to report upstream.

### Batch C (appended 2026-07-13, from the Round-13 candidate list — 6 entries: 1 REPRODUCED, 5 NOT-REPRODUCED)

### CBUG-C1: sCalc `LRC`/`AMODBUS` on an empty operand is an unbounded read — segfaults the IOC
Bucket: NOT-REPRODUCED · Severity: High
C: `sCalcPerform.c:247` — the LRC loop bound is `i < strlen(rawInput)-1` with `strlen` returning
`size_t`: for an empty operand `strlen-1` wraps to `SIZE_MAX` and the loop reads two bytes per
step past the end of a zero-length string.
Defect: no emptiness guard anywhere on the LRC path (`LRC(...)`, and `AMODBUS(...)` which
prepends `":"` *after* the LRC is computed). The read runs until it faults.
Port: `crates/epics-base-rs/src/calc/engine/checksum.rs:47-49` — an empty operand returns
`None` and the checksum owner yields the empty string; the site's doc block records that this
is a refusal of C UB, not a divergence.
Impact: any scalcout whose string input can be momentarily empty — a fresh record before its
first input fetch, a cleared field, an operator typo — crashes the **whole IOC** from a
reachable record state.
Proof: compiled upstream (Round-13 category-A harness) SEGFAULTS on `LRC("")`, `AMODBUS("")`,
and `LRC(AA)` with an empty `AA`.

---

### CBUG-C2: pvxs QSRV resets the whole TCP circuit when one channel's request options fail to parse
Bucket: NOT-REPRODUCED (port refuses — flipped 2026-07-20; already refused since commit `67097734` (2026-07-13, ancestor of main): `MonitorRequestFatal` was deleted, so a per-op request-option parse failure returns an ordinary per-op `OpError` and cannot tear down the circuit — teardown is unrepresentable by construction; see the 2026-07-20 reconciliation note) · Severity: Medium
C: `pvxs/ioc/singlesource.cpp:147` / `pvxs/ioc/groupsource.cpp:399` — `onSubscribe` calls a
bare `connect()`; the `NoConvert` its DBE/options parse can throw propagates uncaught into the
connection layer, which tears the circuit down.
Defect: a per-operation failure (one client's malformed `record._options`, e.g. a DBE
selector naming a non-array element kind) is escalated to a transport-level reset, killing
every other channel multiplexed on that TCP connection.
Port: `crates/epics-bridge-rs/src/qsrv/pva_adapter.rs:389-420` (`check_monitor_request`)
reproduces the reset for exactly pvxs's DBE `NoConvert` case (bug-for-bug, W10-C1/R10-37);
`crates/epics-pva-rs/src/server_native/tcp.rs:9997`
(`init_empty_selector_descriptor_only_registers_op`) pins that other malformed INITs degrade
per-op instead of resetting.
Impact: through a gateway, one downstream user's field typo drops every downstream user's
channels on that gateway connection — the blast radius is the shared circuit, not the
offending op.
Proof: W10-C1 adjudicated REAL in the Round-13 re-audit (pinned line re-read); the port's
reproduction and its scope are tested at the two sites above.

---

### CBUG-C3: sCalc `FETCH_AA` leaves the 40-byte local string unterminated when the source is exactly `SCALC_STRING_SIZE` long
Bucket: NOT-REPRODUCED · Severity: Low
C: `sCalcPerform.c:866-872` — `ps->s = &(ps->local_string[0]); strncpy(ps->s, psarg[op -
FETCH_AA], SCALC_STRING_SIZE);` — `strncpy` writes no terminator when the source length is ≥
`SCALC_STRING_SIZE` (40), and every later reader of `ps->s` (`atof`, `strlen`, string ops)
runs past the 40-byte `local_string` into adjacent stack-cell memory.
Defect: missing forced termination after the bounded copy (the idiomatic `s[SIZE-1]='\0'` is
absent here, though present in other paths).
Port: `crates/epics-base-rs/src/calc/engine/string.rs:47-50` — the string evaluator's fetch
clones the length-carrying `PvString`; there is no fixed buffer and no terminator to lose.
Impact: LATENT — a real scalcout supplies `char[40]` fields whose own copy paths terminate, so
the ≥40-byte psarg cannot arise from record state; only a device-support caller handing longer
strings to `sCalcPerform` directly is exposed.
Proof: the copy site quoted; not compiled-driven (unreachable from record state — the reason
it is Low and latent).

---

### CBUG-C4: `caget -w nan` waits forever — a NaN timeout never expires
Bucket: NOT-REPRODUCED · Severity: Low
C: `tool_lib.c:628` (`connect_pvs` → `ca_pend_io(caTimeout)`) — `epicsScanDouble` at caget's
`-w` case accepts `"nan"`, and inside libca every deadline comparison against a NaN timeout is
false, so the pend never times out.
Defect: no finiteness check between the lenient scanner and the pend deadline.
Port: `crates/epics-ca-rs/src/cli.rs:100-104` — a non-finite `-w` resolves to
`DEFAULT_CLI_TIMEOUT_SECS` (C's 1 s default); a negative `-w` is an already-expired deadline
(W10-B1).
Impact: a scripted `caget -w $computed` whose arithmetic goes NaN blocks the script forever on
any unanswered search instead of failing after the timeout.
Proof: decisive path quoted; surfaced during the Round-13 category-B compiled head-to-head
runs.

---

### CBUG-C5: sCalc `PRINTF` with more conversions than arguments reads a missing vararg — undefined behaviour
Bucket: NOT-REPRODUCED · Severity: Low
C: `sCalcPerform.c:1546-1564` — `PRINTF` pops exactly ONE operand and calls `snprintf` with
exactly one vararg; a format containing a second conversion makes `snprintf` fetch a variadic
argument that was never passed (undefined behaviour; in practice it reads whatever the
register/stack slot holds).
Defect: the conversion count in the user-supplied format is never validated against the fixed
one-argument call shape.
Port: `crates/epics-base-rs/src/calc/engine/string.rs:578-583` → `simple_printf` (`:1050-1058`)
— renders the single popped value through the port's own formatter; there is no vararg
machinery to over-read.
Impact: `PRINTF("%d %d", A)` in any scalcout prints A followed by garbage (content
compiler/ABI-dependent), silently corrupting the string result.
Proof: the one-vararg call shape quoted; UB by C99 7.19.6.1p2 (too few variadic arguments).

---

### CBUG-C6: sCalc `UNTIL` with a string condition tests an uninitialised double
Bucket: NOT-REPRODUCED · Severity: Low
C: `sCalcPerform.c:1999` — `if (ps->d == 0)` with no `toDouble(ps)` in front, while
`LITERAL_STRING`'s push (`:1493-1499`) sets `ps->s` and never touches `ps->d`: a string-valued
loop condition tests whatever double the stack cell last held.
Defect: the condition read skips the type settle every other numeric consumer performs.
Port: `crates/epics-base-rs/src/calc/engine/string.rs:796-800` — the condition is read through
`to_double` (the `atof` coercion every other numeric context applies); the site's doc block
records this as the adopted R13-8 disposition (do not port UB), and aCalc's `UNTIL_END`
carries the same documented deviation for an array condition.
Impact: `UNTIL(...;"0")` exits or loops depending on unrelated stack history — the same
expression behaves differently under a different evaluation prefix.
Proof: compiled upstream exits after ONE iteration for both `UNTIL(A:=A+1;"0")` and
`UNTIL(A:=A+1;"1")` (stale `d` non-zero both times); probes quoted in the port's doc block.

---

### Batch D (appended 2026-07-13, from the wave-13/14 fix reports and the Round-14/16 deviation dispositions — 5 entries: 1 REPRODUCED, 4 NOT-REPRODUCED)

| id | upstream | one line | severity | bucket |
|---|---|---|---|---|
| CBUG-D1 | calc | out-of-count `FETCH` leaves the stack cell stale (sCalc scalar) / the array tail stale (aCalc) | Low | NOT-REPRODUCED |
| CBUG-D2 | calc | sCalc string `<<`/`>>` with a negative count writes past the 40-byte `local_string` | Medium | NOT-REPRODUCED |
| CBUG-D3 | base ca | non-positive `EPICS_CA_CONN_TMO` accepted — watchdog flood, 177k stderr lines / 3 s | Medium | NOT-REPRODUCED |
| CBUG-D4 | base libCom | the two escape printers render NUL differently — `\0` vs `\x00` | Low | NOT-REPRODUCED |
| CBUG-D5 | base ca | `EPICS_CA_MAX_SEARCH_PERIOD=inf` aborts the client in malloc; `=nan` NaN-drives the timer wheel | Medium | NOT-REPRODUCED |

### CBUG-D1: sCalc's string engine leaves the stack cell stale on an out-of-count `FETCH`; aCalc's array `FETCH` zeroes only element 0
Bucket: NOT-REPRODUCED · Severity: Low
C: `sCalcPerform.c:857-863` — the string engine's scalar-FETCH else branch is a bare
`INC(ps);` (comment at `:861`): the "fetched" cell keeps whatever `s` pointer and `d` the
stack slot last held. The settle this branch skips is performed by the function's own
siblings — the double-only engine writes `*++pd = 0.` (`:421-426`) and aCalc's scalar FETCH
writes `ps->a = NULL; ps->d = 0.` (`aCalcPerform.c:433-438`). aCalc's *array* FETCH
(`aCalcPerform.c:442-455`) has the same family defect one level down: `toArray(ps,0)`
allocates a freelist block without initialising it (`to_array`, `:124-143`, `setValues=0`),
then only `ps->a[0] = 0.` — an out-of-count array fetch hands the expression `a[1..]` from a
recycled block's previous contents.
Defect: an out-of-count fetch yields evaluation-history-dependent garbage instead of a
defined value; the guard's else branch exists but forgets the write.
Port: `crates/epics-base-rs/src/calc/engine/mod.rs:301-321` — `with_counts` clamps
`num_args` by construction and the accessors return nothing past it; the fetch pushes a
defined 0 / empty (R15-1 structural fix, commit `927bd592`).
Impact: LATENT from record state — scalcout/acalcout always pass full argument arrays, so
only device support calling `sCalcPerform`/`aCalcPerform` directly with a short argument
array is exposed.
Proof: the bare `INC(ps)` and the `setValues=0` allocation path quoted; not compiled-driven
(unreachable from record state — the reason it is Low and latent).

---

### CBUG-D2: sCalc `<<`/`>>` with a negative count — OOB write through the 40-byte `local_string`, UB numeric shift
Bucket: NOT-REPRODUCED · Severity: Medium
C: `sCalcPerform.c:1263-1294` — the character-shift count is `j = myNINT(ps1->d);
j = myMIN(j, SCALC_STRING_SIZE);` (`:1266-1267`): clamped ABOVE at 40, never below. For a
negative `j`, RIGHT_SHIFT reads `ps->s[i-j]` past the end of the 40-byte `local_string`
(`:1280-1283`), and LEFT_SHIFT runs `i` up to `SCALC_STRING_SIZE - j` (> 40) and writes
`ps->s[i]` out to `s[40+|j|-1]` — an out-of-bounds WRITE into the adjacent stack cell
(`:1288-1291`). The numeric branch `(int)(ps->d) >> (int)(ps1->d)` (`:1272-1276`, and the
double-only engine at `:623-628`) is UB for a negative count by C99 6.5.7p3.
Defect: no lower clamp on the shift count anywhere on either branch.
Port: `crates/epics-base-rs/src/calc/engine/string.rs:1677-1680` — `shift_chars` applies
`clamp(0, 40)`, refusing the OOB (R14-5 documented deviation, recorded at the site); the
numeric branch (`:342-360`) pins the x86-64 hardware count-masking the compiled C exhibits
(`wrapping_shr`/`wrapping_shl`).
Impact: `AA<<B` in any scalcout whose `B` goes negative at runtime corrupts the evaluation
stack (string case) or produces compiler-dependent values (numeric case).
Proof: the clamp line and both loop bodies quoted; not compiled-driven for the negative
count (the OOB write corrupts C's own stack — the reason the port refuses it).

---

### CBUG-D3: a non-positive `EPICS_CA_CONN_TMO` is accepted — the connection-verify watchdog floods stderr and spins a core
Bucket: NOT-REPRODUCED · Severity: Medium
C: `epics-base modules/ca/src/client/cac.cpp:188-194` — the 30 s default
(`CA_CONN_VERIFY_PERIOD`) applies only when `envGetDoubleConfigParam` FAILS; a successful
parse of `-5` or `0` is stored into `connTMO` unchecked, leaving the circuit-verify deadline
permanently in the past.
Defect: no positivity gate between parse success and the watchdog period.
Port: `crates/epics-ca-rs/src/client/transport.rs:200-215` — a non-positive value is
refused out loud (one warning line) and the 30 s default used (R16-19 documented deviation,
commit `994a2f94`).
Impact: `EPICS_CA_CONN_TMO=-5` — one operator typo — makes every C CA client flood
"Virtual circuit unresponsive" while pinning a core; updates still arrive, so it presents
as an unexplained CPU/log leak rather than an obvious failure.
Proof: measured on the compiled camonitor on this host during the R16-19 fix wave —
**177,182 stderr lines in 3 seconds** with `EPICS_CA_CONN_TMO=-5`.

---

### CBUG-D4: libCom's two escape printers render NUL differently — `\0` vs `\x00`
Bucket: NOT-REPRODUCED (port refuses — fixed 2026-07-20, `09af6a24`: both `asyn-rs` escapers now render NUL as `\0` through one parameter-free table, so the `\0`-vs-`\x00` divergence is unrepresentable by construction; this supersedes the 2026-07-14 "keep both" adjudication — see the D4 body and the 2026-07-20 reconciliation note) · Severity: Low

**Adjudication (revised 2026-07-20 — the earlier "keep both" is superseded).** The C
divergence is real: `epicsStrnEscapedFromRaw` has an explicit `case '\0': OUT('\\');
OUT('0');` at `:145`, while `epicsStrPrintEscaped` (`:230-262`) has **no `'\0'` arm**, so a
NUL falls through to the `fprintf(fp, "\\x%02x", ...)` default and prints `\x00`. The C bug
is therefore a **missing case in the display escaper**, not two deliberately-different
renderings: C's round-trippable escaper renders NUL as the deliberate `\0`; the display
escaper merely forgot the case.

The earlier adjudication kept both renderings on a byte-parity-with-C argument. That is
rejected under strategy-2026-07-13 §2 (clean is the goal), and two of its premises are wrong:
(1) `\0` is **round-trip-safe** — `epicsStrnRawFromEscaped` decodes `\0` via `case '0'`,
consuming only the `0` with no octal continuation (the port decoder matches at
`iocsh.rs`), so `\0` followed by a digit round-trips as NUL-then-digit; and (2) the port is
**not** parameter-free of a shared table — `escape.rs` had ONE table whose only per-form
knob was the NUL rendering, i.e. exactly the shared table the argument claimed did not
exist. The port also already renders NUL as `\0` in its calc escaper
(`epics-base-rs/src/calc/engine/string.rs`, `ESC$`), so `\x00` on the asyn path was an
internal inconsistency too.
C: `epics-base modules/libcom/src/misc/epicsString.c` — `epicsStrnEscapedFromRaw` has an
explicit `case '\0': OUT('\\'); OUT('0');` (`:145`), while `epicsStrPrintEscaped`
(`:230-262`) has no `'\0'` case, so a NUL falls to the `fprintf(fp, "\\x%02x", ...)` default arm and
prints `\x00` (`:256-259`). Asyn traffic tracing escapes through both, chosen by output path, so
the same traced byte renders differently in a trace file than in an errlog capture.
Defect: two implementations of the same escape table diverge on one byte.
Port (**refuses** — fixed 2026-07-20, `09af6a24`): `crates/asyn-rs/src/escape.rs` — the
`nul` parameter was removed from the shared `escape()`; the NUL arm is now the constant
`0 => "\\0"`, so **both** `escaped_from_raw` and `print_escaped` render NUL as `\0` and the
`\0`-vs-`\x00` divergence is unrepresentable by construction. Converging onto `\0` matches
C's deliberate round-trippable case and the port's calc escaper.
Deliberate deviation (stated, not hidden): the port's `print_escaped` now emits `\0` where
compiled C's `epicsStrPrintEscaped` still emits `\x00`. On the display/trace path the port
diverges from an unfixed C IOC by exactly this one byte — the port owns that deviation,
choosing internal consistency (and agreement with C's own deliberate `\0` case) over
mirroring C's omission. Filing upstream (supply the missing `'\0'` case to
`epicsStrPrintEscaped`) would remove the deviation.
Impact: on the asyn trace/display path a NUL now prints `\0` (was `\x00`); a NUL captured
via asynTrace and via errlog now render identically, and consistently with the
round-trippable escaper. Diffs of the same NUL byte against an *unfixed* C IOC's display
escaper differ by this one deliberate byte.
Proof: both C switch arms quoted; the port's convergence is pinned by tests
(`escape::the_table_is_c_s_and_both_forms_now_agree_on_nul`, plus a NUL and
NUL-then-digit round-trip through the port decoder). Fix `09af6a24`.

---

### CBUG-D5: `EPICS_CA_MAX_SEARCH_PERIOD=inf` aborts the client in malloc; `=nan` drives the search timer wheel off a NaN period
Bucket: NOT-REPRODUCED · Severity: Medium
C: `epics-base modules/ca/src/client/udpiiu.cpp:68-94` (`getMaxPeriod`) — the only value
gate is `maxPeriod < maxSearchPeriodLowerLimit` (`:77`), which NaN fails, so NaN survives
resolution untouched. `getNTimers` (`:97-99`) then computes
`static_cast<unsigned>(1.0 + log(maxPeriod/minRoundTripEstimate)/log(2.0))` — for `inf`
(which `epicsScanDouble` happily accepts) the float→unsigned conversion is UB
(C++ [conv.fpint]) and the garbage `nTimers` sizes the search-timer allocation.
Defect: a lenient scanner feeding NaN-blind range gates and an unguarded float→unsigned
conversion.
Port: `crates/epics-ca-rs/src/client/search.rs:229-256` — NaN is routed to the 60 s lower
clamp with C's "(low)" diagnostics; `inf` saturates under Rust's `as u32` and takes the
"(high)" clamp to 4194.304 s; both recorded as deviations in the resolver's doc block
(R15-17 wave).
Impact: `EPICS_CA_MAX_SEARCH_PERIOD=inf` crashes every C CA client at startup;
`=nan` leaves the search back-off ladder driven by NaN arithmetic, where every deadline
comparison is false — resolver cadence undefined.
Proof: the compiled caget on this host aborts in malloc for `inf` (recorded at the port
site during the R15 wave); the NaN-blind gate (`:77`) and the UB cast (`:99`) quoted.

---

### Batch E (appended 2026-07-13, from the Round-17 dispositions — 2 entries: 1 REPRODUCED, 1 NOT-REPRODUCED)

| id | upstream | one line | severity | bucket |
|---|---|---|---|---|
| CBUG-E1 | base db | a scalar `dbPut` into a FIFO compress VAL writes through `get_array_info`'s READ offset, rewriting one slot instead of appending | Medium | NOT-REPRODUCED |
| CBUG-E2 | base db | `dbConvert`'s double→integer PUT/GET is a bare C cast — UB out of range, x86-64 gives `INT_MIN`/truncation garbage | Medium | NOT-REPRODUCED |

### CBUG-E1: scalar `dbPut` into a FIFO compress VAL writes at the READ start — every put during initial fill rewrites the same slot
Bucket: NOT-REPRODUCED · Severity: Medium
C: `epics-base modules/database/src/ioc/db/dbAccess.c:1350-1362` — for a field with
`special == SPC_DBADDR`, even a scalar put (`nRequest == 1`) fetches its write offset from
`prset->get_array_info`. `compressRecord.c:409-431`'s `get_array_info` returns the *read*
start — in FIFO mode `(off + nsam - nuse) % nsam`, "the index of the first valid element"
per its own comment — because it exists to serve `dbGet`. `dbPut` then hands that offset to
the put-convert routine, so a client put lands on the oldest element, not at the write
cursor `off`.
Defect: one `get_array_info` serves both `dbGet` (wants the read start) and `dbPut` (wants
the write cursor); `dbPut` consumes the read answer.
Port: `crates/epics-base-rs/src/server/records/compress.rs:283` (`push_value`) — a client
VAL put routes through the same ingest as INP data (`:784-787`), appending at the write
cursor and bumping `nuse`, so three puts into an empty NSAM=3 FIFO give `[1,2,3]`.
Impact: on a C IOC, `caput CMP.VAL 1; caput CMP.VAL 2; caput CMP.VAL 3` into an empty
NSAM=3 FIFO compress leaves `[3,0,0]` with `NUSE` still 0 — the puts silently overwrite one
another in a slot `dbGet` may not even serve — where the port yields `[1,2,3]`.
Proof: the offset hand-off (`dbAccess.c:1350-1362`) and the read-start computation
(`compressRecord.c:420-426`, with its "first valid element" comment) quoted; port behavior
pinned by the compress ingest tests. Filed as the R17-85 documented deviation
(`doc/c-parity-review-2026-07-10.md`), flagged for user sign-off.

### CBUG-E2: `dbConvert`'s double→integer conversion is a bare C cast — undefined behaviour out of range; compiled x86-64 yields `INT_MIN` / truncation garbage
Bucket: NOT-REPRODUCED (port saturates — refuses; the bucket label was flipped 2026-07-20 to match the body, which already recorded the saturating behaviour; see the 2026-07-20 reconciliation note) · Severity: Medium
C: `epics-base modules/database/src/ioc/db/dbConvert.c:96-113` — the PUT macro body is
`*pdst = (typeb) *psrc;`, instantiated for every integer destination at `:1631-1638`
(`putDoubleShort`, `putDoubleLong`, `putDoubleUlong`, ...); the GET twin (`:63-70`) is the
same cast in the read direction. A double outside the destination's range is undefined
behaviour (C17 6.3.1.4p1) — no diagnostic, no clamp.
Defect: an unguarded float→integer cast on client-supplied data, on every numeric put/get
path in the IOC database.
Port: `crates/epics-base-rs/src/types/c_cast.rs` — the single owner of the cast. It
**SATURATES**: out-of-range clamps to the destination's range, NaN → 0. That is Rust's
native `as`. Adjudicated 2026-07-14 (commit `651bf392`).

**Adjudication — the port deliberately does NOT reproduce compiled C, because compiled C
is not single-valued.** The cast is UB, and the two targets EPICS actually ships on
disagree. Compiling the macro body for each:

```text
x86_64    cvttsd2si   out of range -> INT_MIN;   NaN -> INT_MIN
aarch64   fcvtzs      out of range -> SATURATES; NaN -> 0
```

`3.0e9 → DBF_LONG` is `-2147483648` on an x86-64 IOC and `2147483647` on a Raspberry Pi
or Zynq one — both ordinary EPICS platforms. There is no "what C does" here to be
bug-for-bug faithful *to*. The earlier revision (R17-79, `51435dc8`) reproduced the x86-64
value and pinned it as `c_cast::matches_compiled_c_x86_64`; that did not close a divergence
so much as trade agreement-with-ARM for agreement-with-x86 while adopting the undefined
value. Per `doc/strategy-2026-07-13.md` §2 (Tier 2 — *fix C where it is wrong*), the port
now saturates, which is byte-identical to a compiled aarch64 IOC.

No alarm is raised: `dbConvert` runs on the put path, outside the record's process cycle,
so an alarm raised there is erased by the next `recGblResetAlarms` unless routed through
`nsta`/`nsev` — and even routed correctly it would flag a record whose stored value is now
in range and valid.

Tier 1 (wire) is unaffected: a `DBF_LONG` field carries 32 bits regardless of which 32 bits
were chosen.

**The `epicsEnum16` (DBF_MENU / DBF_ENUM) destination is the same cast.** `putDoubleEnum`
(`dbConvert.c:1631-1638`, same PUT macro body) casts the double straight to `epicsEnum16`,
so a numeric ordinal put out of the u16 range is the identical UB. It reaches that routine
because `caput` sends a numeric value that is not one of the enum's choice strings as
`DBR_DOUBLE`, not `DBR_ENUM` (`caput.c:498-507`): `caput SCAN -1` arrives as `-1.0`. The
x86-64 softIoc stores the indefinite `65535`; the port saturates the negative double to `0`,
which for a menu is a *valid* ordinal — `SCAN → Passive`, `PINI → NO`, a severity field →
`NO_ALARM`. This is the differential harness's `enum-negative-ordinal` class, allowlisted
under E2 on the `value_string`/`value_numeric` surface. Two neighbours are deliberately NOT
under E2: a negative ordinal into a *signed raw* field (`PRIO`'s `epicsInt16` holds `-1`
exactly, so C and the port agree and there is no diff to justify), and an ordinal that is in
u16 range but past the record's menu count (`DISS 4`, `PINI 6`) — that cast is exact, not UB,
and the port's clamp-to-menu-range is a separate decision adjudicated on its own.

**Scope — what moves with the owner, and what does not.** `c_cast` is also the calc
engines' narrowing owner, so `INT()`, `NINT()`, the bitwise/shift operands, `PRINTF` and
`BIN_WRITE` saturate too. Two families are DEFINED C, not UB, and deliberately do not move:

* base calc's `d2i`/`d2ui` (`calcPerform.c:324-325`) — `(epicsInt32)(epicsUInt32)d`, a
  modular reinterpretation rather than a narrowing cast. `3e9 → -1294967296` still.
* every **integer-source** conversion — C's `dbFastGetConvertRoutine` picks a different
  routine per source type, and integer→unsigned is defined modular arithmetic
  (C17 6.3.1.3p2). Those route through `EpicsValue::convert_to` (commit `0299eb37`), so a
  `DBF_LONG` `-1` read into a `DBF_USHORT` `SELN` is still `65535`, not a saturated `0`.

Impact (as filed, still true of C): any CA/PVA client writing an out-of-range double to an
integer field of a **C** IOC gets a compiler- and target-dependent value.
Proof: compiled softIoc on this host (x86-64): `calcout` 3.0e9 → `-2147483648`; `aao` DOL
`[1.7, 2.2, -3.9, 70000, 5, 6]` into an `FTVL=SHORT` waveform → `[1, 2, -3, 4464]` (fixer-f
oracle probes, wave 15). Those are recorded beside each corrected test as the values the
port knowingly declines to produce. Port pinned by
`c_cast::out_of_range_saturates_and_nan_is_zero`.

---

## Batch F — filed 2026-07-13 (Round 18 candidates, citations re-verified against the local trees)

Twelve entries: the synApps calc engines (aCalc/sCalc, 7), base calc's dbd
(1), pvxs (2), asyn (1), base histogram (1). Every `file:line` below was
re-read in the local reference tree before filing; two of the Round-18
candidate citations were corrected in the process (CBUG-F11's dispatch is
`asynRecord.c:450-451`, not `:470`; CBUG-F4/F7 both live in
`sCalcPerform.c`, not aCalc). Compiled proof where stated; otherwise the
decisive code path is quoted.

| id | upstream | one line | severity | bucket |
|---|---|---|---|---|
| CBUG-F1 | calc (aCalc) | `INC` bounds check admits writes two elements past the runtime stack — SIGSEGV at legal compile depth | High | NOT-REPRODUCED |
| CBUG-F2 | calc (aCalc) | `SUBRANGE`/`SUBARRAY` clamp the upper bound to `arraySize` *inclusive* — reads one past the buffer | Medium | NOT-REPRODUCED |
| CBUG-F3 | calc (aCalc) | `DERIV` hard-codes a 5-point fit — fewer than 5 points reads out of bounds | Medium | NOT-REPRODUCED |
| CBUG-F4 | calc (sCalc) | `SUBLAST` backward scan stops before offset 0 — a match at the string head is never found | Low | NOT-REPRODUCED |
| CBUG-F5 | calc (sCalc) | `LITERAL_STRING` copy guard never increments its counter — a >39-char literal overruns the 40-byte cell | Medium | NOT-REPRODUCED |
| CBUG-F6 | base calc | `INPM`..`INPU` declare `special(SPC_MOD)` that `special()` rejects — nine documented fields unwritable via CA | Medium | NOT-REPRODUCED |
| CBUG-F7 | calc (sCalc) | unconditional debug `printf` in the `SUBLAST` scan loop | Low | NOT-REPRODUCED |
| CBUG-F8 | calc (sCalc) | `CRC16`/`MODBUS` sign-extends payload bytes ≥ 0x80 — wire-incompatible with the Modbus standard | Medium | NOT-REPRODUCED |
| CBUG-F9 | pvxs | process-only blocking PUT selects a `requestType` dbNotify never matches — silent no-op success | Medium | NOT-REPRODUCED |
| CBUG-F10 | pvxs | UnionArray encode omits the selector its own decoder requires — a decoded UnionA cannot be re-serialized | Medium | NOT-REPRODUCED |
| CBUG-F11 | asyn | `caput REC.TSIZ -1` suspends the put thread forever inside `callocMustSucceed`, holding the global trace mutex | High | NOT-REPRODUCED |
| CBUG-F12 | base histogram | the `LLIM >= ULIM` alarm writes `stat`/`sevr` directly — erased on the process path (NO_ALARM) but STICKS on the `.SGNL` special path (SOFT); port now refuses — raises SOFT/INVALID consistently on both paths via `nsta`/`nsev` (`9a51ba4c`), on `LLIM > ULIM` only, so an unconfigured `0 == 0` record stays quiet | Low | NOT-REPRODUCED |

### CBUG-F1: aCalc's `INC` bounds check admits writes two elements past the runtime stack — SIGSEGV at legal compile depth
Bucket: NOT-REPRODUCED · Severity: High
C: `calc/calcApp/src/aCalcPerform.c:85-96` (the `INC` macro), `:328`
(`stack = calloc(ACALC_STACKSIZE, sizeof(stackElement))`), `:418`
(`top = ps = &stack[1]`):
```c
#define INC(ps) {                           \
    ++ps;                                   \
    ...                                     \
    if ((ps-top)>ACALC_STACKSIZE) {         \
        printf("aCalcPerform:stack overflow\n"); ... return(-1); \
    } else {                                \
        (ps)->numEl = -1;                   \
        (ps)->sourceDouble=-1;              \
```
`top` starts at `&stack[1]`, so element index = `(ps-top) + 1` and the last
valid index is `ACALC_STACKSIZE-1`. The guard fails only at
`ps-top > ACALC_STACKSIZE`, so it admits `ps-top == ACALC_STACKSIZE-1`
(index `ACALC_STACKSIZE`, one past) and `ps-top == ACALC_STACKSIZE` (two
past) — and the `else` arm *writes* `numEl`/`sourceDouble` through the
out-of-bounds pointer before any later check can fire.
Defect: off-by-two in the only stack bound the evaluator has. Multi-slot
operators (`DERIV`/`NDERIV`/`FITPOLY` do two `INC`s per operand,
`:976-985`) reach the overrun at expression depths the compiler accepts.
Port: `crates/epics-base-rs/src/calc/engine/array.rs:1631-1634` — the
stack is a growable `Vec<Cell>`; the compile-time ceiling lives in the
flavour's `ElementTable` (`token.rs:651-653`, R18-3) and an accepted
program cannot overrun at run time.
Impact: an aCalc expression at legal compile depth corrupts the heap
adjacent to the stack allocation or crashes the IOC. Round 18's
category-A compiled harness confirmed the crash under ASAN (heap
overflow at the `INC` write).
Proof: guard arithmetic above; ASAN run recorded in the Round 18 filing
(`doc/c-parity-review-2026-07-10.md`, category A).

### CBUG-F2: aCalc `SUBRANGE` clamps its upper bound to `arraySize` inclusive — the copy reads one element past the buffer
Bucket: NOT-REPRODUCED · Severity: Medium
C: `aCalcPerform.c:1528-1540`:
```c
i = myMAX(myMIN(i,arraySize),0);
j = myMIN(j,arraySize);              /* <-- arraySize itself is admitted */
if (op == SUBRANGE) {
    ...
    for (k=0; i<=j; k++, i++) ps->a[k] = ps->a[i];   /* reads a[arraySize] */
```
`ps->a` holds `arraySize` doubles; a subscript reaching `arraySize` (e.g.
`AA[3,N]` where `N` is the element count) reads one past the end.
Defect: the clamp is off by one for the inclusive loop it feeds — `j`
should cap at `arraySize-1`.
Port: `crates/epics-base-rs/src/calc/engine/array.rs:794-812` — takes C's
clamp verbatim (`subrange_bounds`, `mod.rs:205-208`) but the copy loop
stops at the buffer edge (`src.get(s)` + `break`, deviation documented at
`array.rs:800-805`); the window count still takes C's `1+j-i`.
Impact: an out-of-bounds heap read on an ordinary subscript; under ASAN
(or an unlucky allocation) the IOC crashes on a legal expression.
Proof: clamp and loop quoted; the port comment marks the exact deviation.

### CBUG-F3: `DERIV` hard-codes a 5-point fit — fewer than 5 array points reads out of bounds
Bucket: NOT-REPRODUCED · Severity: Medium
C: `calc/calcApp/src/calcUtil.c:71-75` — `deriv()` calls
`nderiv(x, y, n, d, 2, work)`; `nderiv` (`:27-70`) sets `m = 2*npts+1 = 5`
and immediately runs `fitpoly(x, y, m, ...)`, whose accumulation loop
(`:279-289`, the cited `:281` is `beta[0] += y[i]`) reads `x[0..4]`/
`y[0..4]` **regardless of `n`**. `nderiv` never compares `m` with `n`;
`fitpoly`'s only guard is `n<3` on its *own* argument, which is always the
constant 5 here. aCalc reaches it with `1+lastEl-firstEl` points
(`aCalcPerform.c:976-985`), which a 2-element array makes 2.
Defect: the window size is fixed at 5 while the caller's array can be
smaller; the tail loop `lx[j] = x[(n-m)+j]` (`:60`) even indexes with a
*negative* offset when `n < 5`.
Port: `crates/epics-base-rs/src/calc/engine/array.rs:1198-1204` — the
port's `nderiv` returns `None` for a window it cannot fit and the operator
raises `CalcError::FitFailed` instead of reading out of bounds.
Impact: `DERIV(AA)` on an array (or window) of fewer than 5 elements reads
before/past the operand buffer — garbage derivative or a crash.
Proof: call chain quoted; `n` is never checked against `m` anywhere in
`nderiv`.

### CBUG-F4: sCalc `SUBLAST` never matches at offset 0
Bucket: NOT-REPRODUCED · Severity: Low
C: `sCalcPerform.c:996-1003` (the both-strings arm of `case SUB`):
```c
s1 = ps->s + strlen(ps->s) - strlen(ps1->s);
for (s = NULL; (s == NULL) && (s1 > ps->s); s1--) {   /* stops BEFORE s1 == ps->s */
    if (strncmp(s1, ps1->s, strlen(ps1->s))==0) s = s1;
```
The backward scan's condition `s1 > ps->s` excludes the string head, so a
needle whose *last* (or only) occurrence starts at offset 0 is reported
absent.
Defect: boundary condition; `>=` was intended (the forward-scan `SUB` arm
above has no such exclusion).
Port: `crates/epics-base-rs/src/calc/engine/string.rs:789-810` +
`rfind_sub` (`:1638-1646`) — `rposition` over all windows, offset 0
included.
Impact: `"abc" |- "abc"` (delete last occurrence) deletes nothing in C;
any SUBLAST whose match sits at the head silently no-ops.
Proof: loop condition quoted.

### CBUG-F5: sCalc `LITERAL_STRING`'s copy guard never increments its counter — a >39-char literal overruns the 40-byte stack cell
Bucket: NOT-REPRODUCED · Severity: Medium
C: `sCalcPerform.c:1493-1498`:
```c
case LITERAL_STRING:
    INC(ps);
    ps->s = &(ps->local_string[0]);
    s = ps->s;
    for (i=0; (i<SCALC_STRING_SIZE-1) && *post; )
        *s++ = (char)*post++;                /* <-- i is NEVER incremented */
    *s = '\0';
```
`i` stays 0, so the bound `i < 39` is always true and the copy runs to the end
of the literal — a literal longer than 39 characters writes past
`local_string[40]` inside the stack element.
Defect: the loop was written as a bounded copy and the bound is inert.
Port: `crates/epics-base-rs/src/calc/engine/string.rs:618-625` →
`StackValue::str` → `ScalcString::from_c` (`value.rs:155-162`) — the
39-byte bound is structural at the one place string cells are built.
Impact: a `.db`-authored expression with a long quoted literal corrupts
the adjacent stack element (or, at the last slot, the allocation tail) on
every evaluation. Reachable by anyone who can load a database.
Proof: loop quoted — no `i++` exists on any path through it.

### CBUG-F6: calc `INPM`..`INPU` declare `special(SPC_MOD)` that `special()` rejects — nine documented fields are unwritable via CA
Bucket: NOT-REPRODUCED · Severity: Medium
C: `epics-base modules/database/src/std/rec/calcRecord.dbd.pod:637-687` —
`INPM`..`INPU` (nine link fields) each carry `special(SPC_MOD)`.
`calcRecord.c`'s `special()` (the `static long special(DBADDR*, int)` body)
handles `SPC_CALC` only and ends `recGblDbaddrError(S_db_badChoice, paddr,
"calc::special - bad special value!"); return S_db_badChoice;`. `SPC_MOD`
is `2` and is not `SPC_CALC`, so any `dbPutField` to those fields runs
`special(after=TRUE)`, hits the fall-through, and returns
`S_db_badChoice`.
Defect: the dbd declares a `special` the record's `special()` does not
implement. `INPA`..`INPL` (the first twelve) carry no `special` and write
fine; only M..U were given the attribute.
Port: `crates/epics-base-rs/src/server/records/calc.rs:758-770` — the
port's `special()` acts only on `CALC` and returns `Ok(())` for every
other field, so INPM..INPU accept writes. (The port declines to reproduce
a self-inflicted `S_db_badChoice`: refusing a documented, ordinarily
writable link field is a usability regression with no contract behind it.)
Impact: on a C IOC, `caput CALC.INPM "other.VAL CP"` fails with
`S_db_badChoice` ("bad special value"); the nine inputs M..U can only be
set at `.db` load time. A parity-faithful port would inherit the same
inability to re-point half a calc record's inputs at run time.
Proof: dbd `special(SPC_MOD)` on M..U quoted; `special()` handles only
`SPC_CALC` and returns `S_db_badChoice` otherwise.

### CBUG-F7: sCalc `SUBLAST` scan contains an unconditional debug `printf`
Bucket: NOT-REPRODUCED · Severity: Low
C: `sCalcPerform.c:999` — inside the CBUG-F4 backward-scan loop:
```c
for (s = NULL; (s == NULL) && (s1 > ps->s); s1--) {
    printf("comparing '%s' with '%s'\n", s1, ps1->s);   /* <-- unconditional */
    if (strncmp(s1, ps1->s, strlen(ps1->s))==0) s = s1;
```
Every other diagnostic in the file is gated on `sCalcPerformDebug`; this
one is not.
Defect: a debug `printf` left in the shipped evaluation path — no debug
gate, runs on every SUBLAST of two strings.
Port: `crates/epics-base-rs/src/calc/engine/string.rs:789-810` — the
SUBLAST arm emits nothing.
Impact: an sCalc/scalcout record using `|-` on strings spams the IOC
console once per compared suffix, per evaluation — console noise
proportional to string length, on a normal scan.
Proof: the `printf` at `:999` is inside the loop with no `if
(sCalcPerformDebug)` guard.

### CBUG-F8: sCalc `CRC16`/`MODBUS` sign-extends payload bytes ≥ 0x80 — wire-incompatible with the Modbus CRC standard
Bucket: NOT-REPRODUCED (port refuses — emits the standard Modbus digest, XORs the unsigned byte; flipped 2026-07-20, code fix `25318fcd`; the "Adjudicated REPRODUCE" line below is superseded; see the 2026-07-20 reconciliation note) · Severity: Medium
C: `sCalcPerform.c:193-212` (the `CRC16` case). Payload is
`char tranInput[40]` (signed on x86-64) folded into `unsigned int crc` via
`crc ^= (unsigned int)tranInput[i]`; a byte ≥ 0x80 sign-extends to
`0xFFFFFF80..`, polluting bits 16-31, and the eight `crc >>= 1` steps shift
that back down. The commented-out predecessor at `:211` masks to the low
byte, showing the low-byte XOR was intended.
Defect: the standard Modbus CRC-16 XORs the *unsigned* byte; C's signed
`char` makes every high byte wrong.
Port: `crates/epics-base-rs/src/calc/engine/checksum.rs:34` (`crc16`) —
**refuses** the C bug. It XORs the *unsigned* byte (`crc ^= byte as u16`),
the standard Modbus CRC-16, so a byte ≥ 0x80 no longer sign-extends.
History: R18-6/`ee4f6bff` first reproduced C's 32-bit-accumulator
signed-`char` sign-extension deliberately; commit `25318fcd`
("CBUG-F8 — sCalc CRC16 emits the STANDARD Modbus digest") flipped that to
the standard digest, superseding the earlier "Adjudicated REPRODUCE".
Refused per strategy-2026-07-13 §2 (clean is the goal): the port emits the
correct Modbus digest rather than mirroring the compiled IOC's wrong one.
Test `test_crc16_high_bytes_are_standard_not_c` pins the standard values
(`0x80` → `0xE0BE`) and asserts C's `0x1F41` is refused.
Impact: `CRC16` of a payload containing a byte ≥ 0x80 now matches the
Modbus standard and diverges from an unfixed C IOC (which is wrong); this
is a deliberate deviation the port owns. ASCII-only payloads were never
affected (`XOR8`/`LRC`/`AMODBUS` unaffected entirely). Filing upstream is
still open (the C IOC stays wire-wrong until a fix merges).
Proof (compiled C, this host): `CRC16("\x80")` → C `\x41\x1f` (= 0x1F41,
wrong), standard-correct → `\xbe\xe0` (= 0xE0BE); the port now emits the
standard `\xbe\xe0`, refusing C.

### CBUG-F9: pvxs process-only blocking PUT selects a `requestType` `dbNotify` never matches — silent no-op success
Bucket: NOT-REPRODUCED · Severity: Medium
C: `pvxs/ioc/singlesource.cpp:364-368` sets
`putOperationCache->notify.requestType = value["value"].isMarked(true,true)
? putProcessRequest : processRequest;` — i.e. `processRequest` when the
client marked no `value`. `epics-base modules/database/src/ioc/db/dbNotify.c`
`processNotifyCommon` never acts on `processRequest`: `:207` inits
`didPut=0`; the put branch (`:232-239`) tests only `putProcessRequest` /
`putProcessGetRequest`; the process branch (`:242-250`) requires
`processGetRequest`. So `didPut` stays 0, `doProcess` stays 0 — no put, no
process, and the callback reports success.
Defect: pvxs asks base to do a plain process via a request type base's
notify state machine does not recognise; the operation silently does
nothing.
Port: the port's QSRV blocking PUT with an unmarked `value` (R18-56)
**writes and processes** rather than no-opping — a deliberate deviation:
the port declines to reproduce a silent no-op that looks like success.
`crates/epics-bridge-rs/src/qsrv/channel.rs:360` and the group PUT
classifier (`group.rs`, R17-37 `b831f62e`) own the marked/unmarked
decision.
Impact: on pvxs+base, a blocking PUT that marks no `value` field (a
process-only request) returns success having done nothing — no record
processing, no error. A parity-faithful port would swallow process-only
PUTs the same way.
Proof: `singlesource.cpp:364-368` quoted; the three `dbNotify.c` branches
that never match `processRequest` quoted (`:207`, `:232-239`, `:242-250`).

### CBUG-F10: pvxs UnionArray encode omits the selector its own decoder requires — a decoded UnionArray cannot be re-serialized
Bucket: NOT-REPRODUCED · Severity: Medium
C: `pvxs/src/dataencode.cpp:370-382` (encode `UnionA`) writes, per element,
a presence byte (`uint8_t(elem ? 1 : 0)`) then `to_wire_full(buf, elem)` —
**no selector index**. The decoder (`:630-659`) reads the presence byte
then `from_wire(buf, select)` (a Selector) and rebuilds the element as the
selected member's type. Encode writes each element through its own
FieldDesc without emitting the selector the decoder then tries to read.
Defect: encode and decode disagree on the wire shape of a UnionArray
element — a value decoded from the wire cannot be re-encoded to the same
bytes (the selector is lost / mis-framed on the round trip).
Port: `crates/epics-pva-rs/src/pvdata/encode.rs:1049-1074` — the port emits
`presence(0x01) + selector index + value` per element (and collapses an
absent/invalid selector to `0x00`), which is a **self-consistent**
encode/decode pair and the shape the port's decoder expects. The port is
**correct**; do **not** "fix" it toward pvxs.
Impact: a pvxs peer that decodes a UnionArray and re-serializes it emits a
frame that does not round-trip; interop with the port on UnionArray fields
depends on the port's (correct) framing, not pvxs's.
Proof: pvxs encode `:370-382` (presence + `to_wire_full`, no selector) vs
decode `:630-659` (presence + `Selector` read) quoted.

### CBUG-F11: `caput REC.TSIZ -1` suspends the put thread forever inside `callocMustSucceed`, holding the global trace mutex
Bucket: NOT-REPRODUCED · Severity: High
C: `asyn/asyn/asynRecord/asynRecord.c:450-451` — the TSIZ special arm calls
`pasynTrace->setTraceIOTruncateSize(pasynUser, pasynRec->tsiz)` with `tsiz`
an `epicsInt32` (`:196`). `asynManager.c:2943-2959`
(`setTraceIOTruncateSize`) takes `size_t size`; `-1` becomes
`18446744073709551615`, passes `size > traceBufferSize`, and calls
`callocMustSucceed(size, 1, ...)`. `libcom/src/misc/cantProceed.c:22-36`:
`calloc` returns NULL for that size, so the `while ((mem = calloc(...)) ==
NULL)` loop `errlogPrintf`s and calls `epicsThreadSuspendSelf()` — **inside
the loop**, forever. The call happens under
`epicsMutexMustLock(pasynBase->lockTrace)` (`:2948`), which is never
released.
Defect: a signed record field is passed to a `size_t` allocator with no
range check, and the "must succeed" allocator's failure mode is permanent
thread suspension while holding a global lock.
Port: `crates/asyn-rs/src/...` (asynRecord port, R18-78) does
`self.tsiz as usize` → `usize::MAX` → interpreted as "unlimited tracing";
no allocation of that size is attempted, no thread suspends.
Impact: `caput <asynRecord>.TSIZ -1` — a plausible operator typo — hangs
the CA/dbPutField thread that serviced it *forever* and permanently holds
`lockTrace`, so every subsequent trace-config operation on any asyn port
blocks. Effective IOC-wide denial of the trace subsystem from one bad put.
Proof: `epicsInt32 tsiz` (`:196`) → `size_t` param (`asynManager.c:2943`) →
`callocMustSucceed(size,...)` (`:2949-2951`) → suspend-in-`while`-loop
(`cantProceed.c:26-33`), all under `lockTrace` taken at `:2948` and never
unlocked on this path.

### CBUG-F12: base `histogram` `LLIM >= ULIM` alarm writes `stat`/`sevr` directly and `recGblResetAlarms` erases it in the same cycle — dead code
Bucket: NOT-REPRODUCED (port refuses — raises SOFT/INVALID consistently on BOTH the process and `.SGNL` paths via the single `nsta`/`nsev` owner; fixed 2026-07-20, code fix `9a51ba4c`; see the 2026-07-20 reconciliation note) · Severity: Low
C: `epics-base modules/database/src/std/rec/histogramRecord.c:328-334`
(`add_count`):
```c
if (prec->llim >= prec->ulim) {
    if (prec->nsev < INVALID_ALARM) {
        prec->stat = SOFT_ALARM;       /* writes stat/sevr, NOT nsta/nsev */
        prec->sevr = INVALID_ALARM;
        return -1;
```
It writes `stat`/`sevr` directly rather than `nsta`/`nsev` via
`recGblSetSevr`. Its observability then depends on **which trigger ran
`add_count`**, and the two triggers disagree — the reason the original
"dead code" reading was only half right:
- **Process path** (`process()` → `add_count` → `monitor()`): `monitor()`
  runs `recGblResetAlarms()` in the same cycle, copying `nsta/nsev →
  stat/sevr` and overwriting the direct write before any client observes
  it. Here the alarm IS dead code — C's *intent* is to alarm, C's
  *behavior* is NO_ALARM.
- **SGNL special path** (`caput .SGNL` → SPC_MOD `special()` → `add_count`,
  no full process cycle): nothing runs `recGblResetAlarms` afterward, so
  the direct `stat=SOFT_ALARM/sevr=INVALID` write **STICKS** and a later
  `caget` observes `STAT=SOFT`. Observable, not dead.
Port (as of `932291d9`): reproduces C on **both** paths. `check_alarms`
writes `stat`/`sevr` directly (not `nsta`/`nsev`); the record declares
`special_checks_alarms` for `SGNL`, so the put owner runs `check_alarms`
after the store on the SGNL special path (success + rejected-conversion),
where it persists → SOFT; on the process path `recGblResetAlarms` erases it
→ NO_ALARM. The earlier "no port change warranted" note was a
**process-path-only** view — correct for that path, but it missed the SGNL
special-path observability that the differential oracle (`caput .SGNL`)
exercises.
Impact: a histogram with inverted limits reports NO_ALARM when reached by a
process cycle, but SOFT/INVALID when reached by a `.SGNL` special-path put —
on both the C IOC and the port.
Proof (compiled softIoc, this host, and differential oracle):
`LLIM=10 ULIM=5`, process → C and port both `STAT=NO_ALARM`; `caput .SGNL`
with inverted limits → C and port both `STAT=SOFT SEVR=INVALID`.

**Reconciliation 2026-08-03 — the refusal's condition narrowed to `>`.** The
`9a51ba4c` refusal took C's condition verbatim (`llim >= ulim`), and the first
full `--phase all` run since it landed showed what that costs: LLIM and ULIM
carry no `initial(...)`, so a bare `record(histogram, "…") {}` already satisfies
`>=` at `0 == 0`, and the port alarmed on the process path of every UNCONFIGURED
histogram against C's NO_ALARM — 14 defects on `SCAN`/`PROC`/`UDF`. The alarm
test is now `llim > ulim`: an INVERSION is a pair only an operator can create,
an EMPTY range is the state every histogram loads in. C's path-dependence means
no single condition agrees with C on both paths, so the residue moved rather
than vanished — with `>` the port is quiet on a default record's `.SGNL` path
where C's sticky write shows SOFT/INVALID (10 cases). That residue is C's bug
showing through, is what the CBUG-F12 allowlist row now covers
(`fields = ["SGNL"]`, `surface = ["stat","sevr"]`, `enabled = true`; the row had
been unmatchable — its `fields` named the compared surfaces, not the case
field), and histogram reconciles at 417 ran / 406 agreed / 11 expected /
0 defect. A genuinely inverted histogram still alarms on BOTH paths, which is
the consistency the refusal was for.

## Batch G — filed 2026-07-17 (PVA differential-oracle campaign, pvxs QSRV2 metadata)

One entry so far. Found not by static audit but by the `epics-oracle-rs`
PVA differential harness (fat `softIocPVX` QSRV2 ground truth vs
`oracle-ioc --pva` on the same `.db`): while closing the port's NT
metadata-routing families, the C ground truth itself was seen to drop a
metadata leaf a record's rset explicitly supplies. Citation re-read in the
local pvxs tree (`1.5.2-20-g4070775`, git head `4070775`, current on
master) before filing.

| id | upstream | one line | severity | bucket |
|---|---|---|---|---|
| CBUG-G1 | pvxs (QSRV2) | `getProperties` nests `display.precision` inside the `DBR_GR_DOUBLE` branch — a field that supplies `get_precision` but NULLs `get_graphic_double` (e.g. `bo.HIGH`) loses its precision over PVA | Low | NOT-REPRODUCED |

### CBUG-G1: pvxs QSRV2 serves no `display.precision` for a field whose rset supplies `get_precision` but NULLs `get_graphic_double`
Bucket: NOT-REPRODUCED · Severity: Low
C: `pvxs/ioc/iocsource.cpp:254` `getProperties()`. The one `options`
mask carries every requested DBR slot; `dbChannelGet` clears each bit
whose rset method is NULL, so `DBR_PRECISION` and `DBR_GR_DOUBLE` arrive
as **independent** flags (a field can have one without the other).
`:274` serves `display.units` under its own `DBR_UNITS` gate — correctly
independent. But the numeric block at `:287-294`:
```cpp
if(auto dlL = node["display.limitLow"]) {   // :287  if numeric
    if(options & DBR_GR_DOUBLE) {           // :288  gate = graphic-limits slot
        dlL = meta.lower_disp_limit;
        node["display.limitHigh"] = meta.upper_disp_limit;
        if(options & DBR_PRECISION) {       // :291  precision's own gate…
            node["display.precision"] = int32_t(meta.precision.dp);  // :292  …but nested inside DBR_GR_DOUBLE
        }
    }
    if(options & DBR_CTRL_DOUBLE) { … }     // :295  control served independently
    if(options & DBR_AL_DOUBLE)   { … }     // :299  valueAlarm served independently
}
```
nests the `display.precision` assignment inside `if(options &
DBR_GR_DOUBLE)`. `display.precision` is gated by `DBR_PRECISION`
(`get_precision`), a slot with no dependence on `DBR_GR_DOUBLE`
(`get_graphic_double`). Nesting the two makes precision reachable only
when the record *also* supplies graphic limits.
Defect: any field that supplies `get_precision` but NULLs
`get_graphic_double` has its precision silently dropped over PVA, even
though the rset answers it and `caget -d DBR_PRECISION` (CA) serves it.
`std/rec/boRecord.c` is the clean witness: `:55` declares `get_precision`
(supplied; `:301-308` returns `boHIGHprecision = 2` for `HIGH`), `:59`
`#define get_graphic_double NULL`, `:60` declares `get_control_double`
(supplied; `:310-318` returns `0 .. boHIGHlimit` for `HIGH`). So `bo.HIGH`
— bo's only `DBF_DOUBLE` field — reaches QSRV2 with `DBR_PRECISION` and
`DBR_CTRL_DOUBLE` set and `DBR_GR_DOUBLE` clear: units and control limits
are served, precision (2) is dropped. Measured on `softIocPVX`:
`display.units "s"`, `control.limitHigh 100000`, and **no**
`display.precision` for `bo.HIGH`.
Port: **deviates — does not reproduce.** The record layer was always
faithful (`bo`'s `field_metadata_override`,
`crates/epics-base-rs/src/server/records/bo.rs:151-160`, transcribes the
rset answer `precision: Some(2)`); the drop lived only in the wire-marking
model. `crates/epics-pva-rs/src/nt/qsrv_marks.rs` `property_leaves` now
gates `display.precision` on its own `DBR_PRECISION` slot, independent of
`DBR_GR_DOUBLE`, so the served precision reaches the wire. Measured across
the fat PVA dbd, this is the whole family — every field of the seven types
whose rset supplies `get_precision` but NULLs `get_graphic_double`
(`bo`/`mbbiDirect`/`mbboDirect` in base; `busy`/`asyn`/`transform`/`sseq`
in the modules), 62 channels, of which **two** carry a meaningful value:
`bo.HIGH` (2 = `boHIGHprecision`) and `busy.HIGH` (2, a literal at
`busyRecord.c:280` — `if(paddr->pfield == (void *)&prec->high)
*precision=2;`). This paragraph previously named `bo.HIGH` as the only one
and counted `busy.HIGH` among the zeros; that reading of `busyRecord.c` was
wrong, and the port served 0 for `busy.HIGH` until
`crates/epics-base-rs/src/server/records/busy.rs` grew the matching
`field_metadata_override`. The other 60 serve `0`, the `recGblGetPrec`
default — the same value CA serves, so that part is PVA-vs-CA consistency,
not a new number. The deviation is carried on the oracle allowlist as CBUG-G1
(`crates/epics-oracle-rs/allowlist/expected-deviations.toml`,
`port_adds_leaves = ["display.precision"]`, content-constrained so it
justifies only the precision add and launders no other marking diff); the
PVA read phase classifies all 62 as EXPECTED DEVIATION.
Impact: over PVA (pvxs QSRV2 and the parity-faithful port alike), the
`HIGH` field of a `bo` record — and any field of any type whose rset
supplies `get_precision` but not `get_graphic_double` — carries no
`display.precision`, so a PVA client renders it at default precision while
the same field over CA (`caget -d DBR_PRECISION`) reports the rset's
value.
Proof: `iocsource.cpp:274` (units, independent gate) vs `:287-294`
(precision nested under `DBR_GR_DOUBLE`) quoted; `boRecord.c:55/59/60`
(precision + control supplied, graphic NULL) and `:301-308` (`HIGH` →
`boHIGHprecision`) quoted; `softIocPVX` GET of `bo.HIGH` shows units +
control, no precision.

## Batch H — filed 2026-08-23 (transform expression conversion, found while porting `convertExpression`)

One entry. Found while closing the port's transform-CLCx conversion gap (the
port compiled the raw infix where C compiles the CONVERTED text): reading
`convertShortcuts`'s replacement table against `sCalcPostfix`'s element table
showed the replacements name a token the parser does not carry. Proof is a
compiled-C driver on this host (gcc 13.3, x86-64 Linux) linked against the real
`libcalc` (`/home/stevek/work/epics-modules/calc/lib/linux-x86_64`).

| id | upstream | one line | severity | bucket |
|---|---|---|---|---|
| CBUG-H1 | synApps calc (`transformRecord`) | `convertShortcuts` expands `$P(` to `$PRINTF(`, a spelling `sCalcPostfix` cannot lex — every `transform` CLCx using one of the six shortcuts silently fails to compile and its channel is never evaluated | Medium | NOT-REPRODUCED |

### CBUG-H1: `transformRecord`'s shortcut expansion produces a token `sCalcPostfix` does not carry
Bucket: NOT-REPRODUCED · Severity: Medium
C: `transformRecord.c:333-341` `shortcuts[]`, applied by `convertShortcuts`
(`:368-380`) inside `convertExpression` (`:384-389`), which both compile sites
run before `sCalcPostfix` (`:481-482` in `init_record`, `:682-684` in
`special`):
```c
struct shortcut { char target[4]; char replace[MAXSHORTCUT]; }
shortcuts[NUMSHORTCUTS] = {
    {"$P(", "$PRINTF("}, {"$T(", "$TR_ESC("}, {"$W(", "$WRITE("},
    {"$S(", "$SSCANF("}, {"$R(", "$READ("},  {"$E(", "$ESC("}
};
```
Each replacement is the `$` short spelling's long name with the `$` KEPT.
`sCalcPostfix.c` carries the two spellings as separate elements — `{"$P", …,
PRINTF}` at `:173` and `{"PRINTF", …, PRINTF}` at `:174`, and the same pairing
for `$E`/`ESC`, `$R`/`READ`, `$S`/`SSCANF`, `$T`/`TR_ESC`, `$W`/`WRITE` — but
there is no `$PRINTF` element, and none of the other five `$`-prefixed long
names exists either. `get_element` (`:255-283`) walks the table backwards and
takes the first entry that prefixes the text, i.e. the longest table symbol, so
`$PRINTF(` lexes as `$P` followed by the unknown operand `RINTF`.
Defect: the conversion turns a valid expression into an invalid one. A
`transform` record whose CLCx uses any of the six shortcuts gets
`CALC_ERR_SYNTAX` from `sCalcPostfix`, `init_record` reports "Illegal CALC
field" once to errlog, `CxV` goes non-zero, and `process` then skips that
channel forever (`:585` `postfix_ok`) — the channel silently keeps its old
value. The same expression compiles in every other sCalc record, because only
`transform` runs `convertExpression`.
Proof: compiled-C driver calling `sCalcPostfix` directly:
```text
$P("%d",3)         -> status=0  err=0  (OK)
$PRINTF("%d",3)    -> status=-1 err=11 (Syntax error, unknown operator/operand)
PRINTF("%d",3)     -> status=0  err=0  (OK)
$E("a")            -> status=0  err=0  (OK)
$ESC("a")          -> status=-1 err=11 (Syntax error, unknown operator/operand)
$S("7","%d")       -> status=0  err=0  (OK)
$SSCANF("7","%d")  -> status=-1 err=11 (Syntax error, unknown operator/operand)
$W("%d",3)         -> status=0  err=0  (OK)
$WRITE("%d",3)     -> status=-1 err=11 (Syntax error, unknown operator/operand)
```
Port: **deviates — does not reproduce.**
`crates/epics-base-rs/src/server/records/transform.rs` `SHORTCUTS` drops the
leading `$` from the replacement (`$P(` -> `PRINTF(`). `PRINTF` and `$P` are the
same element, so a working expression keeps working and keeps its value, while
the ordering the table exists for still holds: the shortcut is consumed before
the macro pass and the result carries no `$` for a macro to match — in fact the
port is stricter here than C, whose `$PRINTF(` still contains a `$P` for a
user macro named `$P` to eat. Pinned by
`crates/epics-base-rs/tests/transform_clcx_macro_expansion.rs`
(`the_shortcut_expansion_keeps_the_expression_compilable`,
`a_shortcut_is_expanded_before_the_macro_pass`).
Impact: on a C IOC, `field(CLCB,"$S($A,\"%d\")")` in a `transform` record is a
dead channel — no value, one errlog line at init, and `CBV` non-zero as the only
running indication.
