# First oracle run — 2026-07-13

Measured, not asserted. Every number below was reproducible with the commands
given on 2026-07-13; the ones that no longer are carry an UNVERIFIED note in
place, naming the commit or the upstream change that moved them. Nothing has
been deleted or re-estimated. Every difference still carries a two-line `.db`
you can paste.

Ground truth: `softIoc` from `/home/stevek/work/epics-base/bin/linux-x86_64`.
Both sides driven by the same C CA tools.

That is no longer the ground truth. `CTools::ioc_bin` defaults to the fat
`/home/stevek/work/oracle-ioc/bin/linux-x86_64/softIoc` (`e45a697f`), which
links busy/transform/sseq/acalcout/scalcout/asyn on top of base; `CTools::bin`
keeps the path above for the CA client tools only. Every figure below was
produced by the stock binary, so a re-run is not diffing against the same C.

**Booted and measured 2026-08-25.** The fat IOC runs, and the default
configuration — fat `softIoc` + fat `dbd/softIoc.dbd` + the stock CA client
tools — enumerates **40 record types, all 40 implemented by the port, 3388
CA-observable fields, 736 `DBF_NOACCESS` excluded**, and its read phase is
`ran 3388 = agreed 3388 + expected-deviation 0 + DEFECT 0 + ERROR 0`.

The two C binaries are **not interchangeable**, and the difference is
observable. The fat tree predates base `2ab1bb14c`, so its `calc` still carries
`special(SPC_MOD)` on `INPM`..`INPU` (as do its `calcout` and `transform`);
measured directly on `record(calc, "T:CALC") {}`:

```sh
caput -c -t -w 2 T:CALC.INPM 1
# ~/work/epics-base/bin/linux-x86_64/softIoc   -> 1                       (accepted)
# ~/work/oracle-ioc/bin/linux-x86_64/softIoc   -> Channel write request failed
```

`EPICS_ORACLE_DBD` changes the denominator only; the IOC binary is
`EPICS_ORACLE_IOC_BIN`, and a run that does not pin both has not pinned its
ground truth.

**Superseded 2026-08-30.** The fat tree has since been rebuilt past
`2ab1bb14c`: the same eighteen `calc`/`calcout` `INPM`..`INPU` put cases now
report `{"outcome": "completed"}` on BOTH sides. The two binaries no longer
differ here, and CBUG-F6's allowlist row was retired for that reason.

## The denominator

Enumerated from `/home/stevek/work/epics-base/dbd/softIoc.dbd` (the expanded
dbd — `dbCommon` inlined), not hand-listed.

| | | status |
|---|---|---|
| record types in the dbd | 34 | recomputed 2026-08-23 from the file named above |
| record types the port instantiates | **34** (measured by booting each; 0 unimplemented) | CONFIRMED 2026-08-25 — re-measured through `port_ioc_builder`, the configuration `74096a5b` moved the probe onto: still 34 of 34 on this dbd, and 40 of 40 on the fat one |
| CA-observable fields | **2551** at the run; **2553** today | SUPERSEDED 2026-08-25 — 2553 re-measured, and every count below that says `ran 2551` should read 2553. The run's copy of the file is gone, so the +2 is attributed, not measured: `c9817fa59` (`bi.AFTC`, `bi.AFVL`) is base's only field-adding commit since 2025-10, and `dbd/softIoc.dbd` was rebuilt 2026-08-11 |
| `DBF_NOACCESS` fields excluded | 594 (raw C pointers; no CA client can reach them) | recomputed 2026-08-23 |

The harness no longer defaults to this file. `CTools::DEFAULT_DBD` is now the fat
`/home/stevek/work/oracle-ioc/dbd/softIoc.dbd` (`e45a697f`), which enumerates 40
record types and 3388 CA-observable fields (736 `DBF_NOACCESS` excluded) —
re-measured 2026-08-25, and reported above. A re-run with defaults therefore
measures a different surface than everything below; set `EPICS_ORACLE_DBD` back
to the path above to reproduce this one.

## Coverage

**2462 / 2551 = 96.5 %** of the observable surface produced a reading on both
sides and was diffed. The remaining **89 fields (3.5 %) are ERRORED, not
covered** — see below. Errors are never counted as coverage and never as
agreement.

SUPERSEDED — re-measured 2026-08-25 as **2553 / 2553 = 100 %**, no ERRORs. The
`74096a5b` caveat (a dark record type's fields stay in the denominator while
`select_types` drives only the implemented ones) cannot bite here because
nothing is dark: the probe reports 0 unimplemented on both dbds. `d683e817`
never invalidated the old figure either — the filter it fixed counted any case
carrying no boundary class as a read case, and this is `--phase read`, whose
case list holds nothing else.

Coverage is claimed from the **read** phase only. The put and monitor phases do
not visit every field, so quoting them as coverage would inflate the number.

## Read phase — all 34 record types

```
ran 2551 = agreed 2149 + expected-deviation 0 + DEFECT 313 + ERROR 89
```

SUPERSEDED — re-measured 2026-08-25:

```
ran 2553 = agreed 2553 + expected-deviation 0 + DEFECT 0 + ERROR 0
```

Every one of the 313 DEFECTs and 89 ERRORs below is closed. The 89 were the
port not serving fields C serves; the 313 were the FieldDesc regeneration this
document was written ahead of. The lists are kept as the record of what was
fixed, not as an outstanding finding.

The re-run is **chunked**, and that is not cosmetic: on a 40-type sweep at load
100+ one record type's Rust IOC intermittently never answers a search, and the
whole type comes back ERROR. It reproduces on no particular type — `stringin`
(47 fields) on the fat sweep, `int64out` (66) on the stock one — and each
re-runs clean on its own three times over. The cause is not the probe budget
(`REACHABLE_ATTEMPTS` is 12 independent tries at C's own 1 s `-w`); it is
unidentified, and it is the one instrument defect these figures rest on.

### The 89 ERRORs were all one-sided: C served the field, the port did not

Not a harness failure — `caget`/`cainfo` connected on C and timed out on the
port. A C client could see these fields on the C IOC and not on the port, so
each was also a Tier-1 divergence; the harness scored them ERROR because no
reading was obtained, and named every one rather than rounding them into a pass.
**All 89 now read on both sides** (2026-08-25).

- `sel` — 21 fields missing (`PREC EGU HOPR LOPR ADEL MDEL LA..LL ALST MLST NLST`)
- `sub` — 25 fields missing (`EGU HOPR LOPR PREC LA..LU`)
- `aSub` — 22 (`OVAL ONVA..ONVU`)
- singletons: `ai.LBRK ao.LBRK ao.OMOD bi.SVAL calcout.POVL event.SVAL`
  `mbbi.SDEF mbbi.SVAL mbbiDirect.SVAL mbbo.SDEF mbboDirect.OBIT seq.OLDN`
  `seq.PREC stringin.SVAL`
- `dfanout` (`EGU PREC HOPR LOPR`), `histogram` (`PREC HOPR LOPR`)

Repro (any of them):

```sh
printf 'record(ai, "V:AI") {}\n' > repro.db
softIoc -S -d repro.db            # caget V:AI.LBRK -> 0
oracle-ioc --db repro.db          # caget V:AI.LBRK -> not found
```

### DEFECT clusters (313 cases, 559 differences) — all closed 2026-08-25

| n | surface | C | port | example |
|---|---|---|---|---|
| 137 | access rights | `read, no write` | `read, write` | `aSub.FTA` |
| 86 | native type | `DBF_DOUBLE` | `DBF_LONG` | `aSub.NOA`, `ai.ROFF` |
| 41 | native type | `DBF_ENUM` | `DBF_SHORT` | `aSub.FTA` |
| 41 | value | `DOUBLE` | `10` | `aSub.FTA` — ordinal instead of the menu string |
| 34 | native type | `DBF_ENUM` | `DBF_STRING` | `.DTYP` on every record type |
| 34 | value | `Soft Channel` / `` | `` / `DEFAULT` | `.DTYP`, `.ASG` |
| 34 | native type | `DBF_CHAR` | `DBF_SHORT` | `.LCNT` |
| 13 | native type | `DBF_LONG` | `DBF_DOUBLE` | `longin.HYST`, `longin.ADEL` |
| 4 | value | `STRING` | `DOUBLE` | `aai.FTVL` |
| 3 | value | `41` | `256` | `lsi.SIZV` |
| 1 | element count | `1` | `0` | `lso.IVOV` |

Three findings that are **independent of the FieldDesc regeneration** now in
flight on another branch (i.e. they will not be fixed by getting the types right):

1. **`.ASG` reads `DEFAULT` on the port, `""` on C — on all 34 record types.**
   An unset access-security group is the empty string in C; the port materialises
   the name of the default group.
2. **`.DTYP` reads `""` on the port, `Soft Channel` on C.** The port also
   advertises it as `DBF_STRING` where C advertises `DBF_ENUM` (a device-type
   menu). A client that enumerates DTYP choices gets nothing.
3. **Menu/enum fields return ordinals, not strings.** `aSub.FTA` is `DOUBLE` on
   C and `10` on the port — a `DBF_SHORT` carrying the ordinal. This is the same
   class as the `bi.VAL` monitor finding below.

`ai.ROFF` deserves its own line: the dbd says `DBF_ULONG`; CA has no unsigned
32-bit type, so C promotes it to `DBR_DOUBLE`. The port advertises `DBF_LONG`,
so any `ROFF > 2^31` comes back **negative** to a CA client.

```sh
printf 'record(ai, "V:AI") {}\n' > repro.db
# C:    cainfo V:AI.ROFF -> DBF_DOUBLE ;  caget V:AI.ASG -> ''
# port: cainfo V:AI.ROFF -> DBF_LONG   ;  caget V:AI.ASG -> 'DEFAULT'
```

## Put phase — `calc`, every observable field × boundary values

```
ran 842 = agreed 469 + expected-deviation 9 + DEFECT 364 + ERROR 0
```

`ran 842` is recomputed: `is_put_candidate` over `calc`'s 119 CA-observable
fields, expanded by `boundary_cases`, is exactly 842 cases on the dbd above as
of 2026-08-23, and 842 again on 2026-08-25. The four-way split is SUPERSEDED —
re-measured 2026-08-25, identically on both dbds and twice each:

```
ran 842 = agreed 833 + expected-deviation 9 + DEFECT 0 + ERROR 0
```

The nine are CBUG-F6 on `INPM`..`INPU`, unchanged. The reasons the old split was
suspect are settled rather than merely noted: `27d9c135` read STAT and SEVR
through `caget_batch(..).ok()`, so one unconnectable `.STAT` removed the alarm
surface from every case of the record type while they all stayed AGREED, and
`30f5b1b9` collapsed every `caput` failure into `accepted: false` on both sides
— both push AGREED up. The re-run has `ERROR 0` with those two fixed in place,
so the zero is measured, not inherited.

**CBUG-F6 fires as an EXPECTED DEVIATION on exactly the nine fields it names**
(`calc.INPM`..`INPU`): C's `special()` rejects the `SPC_MOD` put, the port
accepts it. That is the allowlist working as designed: `calcRecord.dbd` declares
a `special` that `calcRecord.c`'s `special()` never implements, so it falls
through to `S_db_badChoice` and nine documented link fields cannot be written
over CA at all. The port declines to reproduce that, so the difference is
justified and is not counted as a defect.

It still does — but only against the ground truth the harness actually uses.
Base `2ab1bb14c` (2026-07-21) dropped `special(SPC_MOD)` from `calcRecord.dbd`'s
`INPM`..`INPU`, and a `softIoc` built from that tree accepts the nine puts,
which makes the row STALE and fails the run. The default `EPICS_ORACLE_IOC_BIN`
was the fat `softIoc`, built before that commit, and it still refused them when
this was written — measured 2026-08-25, see the ground-truth section above. So
CBUG-F6 fired on 9 of 842, exactly as recorded here, and it would go STALE the
moment the fat tree was rebuilt.

**That happened.** A `--phase all` run on 2026-08-30 reported CBUG-F6 STALE:
C now completes all eighteen `calc`/`calcout` `INPM`..`INPU` puts, the port
always did, and the two agree. The row is retired. Nothing in the port moved —
the same run at the preceding commit reports it identically, and the row was
byte-identical to its state at `24756f664`.

### The dominant put defect: an empty CALC expression alarms on the port

537 of the differences reported by the 364 defect cases (272 on `SEVR`, 265 on
`STAT`) reduce to one root cause. Any put that causes a default `calc` record to
process:

```sh
printf 'record(calc, "V:CALC") {}\n' > repro.db
# both:  caget V:CALC.VAL .STAT .SEVR  ->  0 UDF INVALID      (never processed)
caput -c V:CALC.SCAN ".1 second"
# C:     0 NO_ALARM NO_ALARM
# port:  0 CALC     INVALID     <-- empty expression treated as an error
```

C's `postfix("")` is a valid empty program and `calcPerform` returns 0; the port
raises `CALC_ALARM`. Every subsequent read of `STAT`/`SEVR` on that record then
disagrees, which is why one bug produces hundreds of differences.

The 537 (272 + 265) is CLOSED — the re-run of 2026-08-25 reports 0 DEFECT on
`calc`, so the empty-CALC alarm no longer happens on either side. The figure was
never more than a floor anyway: under `27d9c135` a dropped STAT/SEVR batch could
only lose differences, never invent them.

Other put clusters, after removing that one:

| n | surface | C | port | example |
|---|---|---|---|---|
| 88 | put accepted | `false` | `true` | `PHAS` over-max — C refuses out-of-range puts the port takes |
| 32 | value | `0` | `inf` | `VAL` over-double-max — C clamps to 0, port stores `inf` |
| 12 | value (numeric) | `-1` | `0` | `SCAN` negative ordinal — C keeps `-1`, port clamps |
| 10 | STAT/SEVR | `UDF`/`INVALID` | `NO_ALARM` | put to `VAL` clears UDF on the port without processing |
| 8 | value | `0` | `-32768` / `32767` | `PHAS` over/under — port wraps, C rejects |
| 5 | value | `4` | `INVALID` | `UDFS` past-the-end ordinal — port saturates, C stores 4 |

Every `n` there is a measured outcome of the 2026-07-13 run and every one is now
**0** — the 2026-08-25 re-run has no DEFECT cases on `calc` at all. What each row
was, and why it moved, is below; three had a known direction even before the
re-run. The **88 put accepted** cases are over-counted by however many
had a C-side `caput` that timed out rather than being refused: `30f5b1b9` makes
those ERROR. The **32 VAL over-double-max** is a difference count, not a case
count — the plan drives 57 `over-double-max` cases on `calc`, one per DBF_FLOAT
or DBF_DOUBLE put candidate — and `ae481936` bounds `CBUG-E2` by destination
type, so a DBF_DOUBLE takes no integer cast and these stay DEFECT rather than
being absorbed. The **12 SCAN negative ordinal** and **8 PHAS over/under** rows
fall inside that bounded `CBUG-E2` scope (`DBF_MENU`
`enum-negative-ordinal`, `DBF_SHORT` `over-max`/`under-min`), and the row is
enabled at HEAD, so the prediction was that up to 20 would move from DEFECT to
EXPECTED DEVIATION. **They did not.** The re-run absorbs nothing under CBUG-E2
and reports it STALE — the port now agrees with the x86-64 C softIoc on all 34
cases in that scope, so the row has no deviation left to justify and fails the
run on its own.

## Monitor phase — event sequence and count

Six cases, driven `1, 2, 2, 3` (the repeated `2` tests no-change suppression):
**3 agreed, 3 DEFECT, 0 errors.**

SUPERSEDED, and the published "six" was never reconcilable with the probe.
`probe_monitor` emits one case per record type whose `VAL` is present, is a put
candidate and is not a link; that gate admits **25** of the stock dbd's 34 types
and **30** of the fat dbd's 40, and it is byte-identical at the run commit, so
six was not a smaller run — it was unexplained. Re-measured 2026-08-25 on the
default (fat) configuration:

```
ran 30 = agreed 27 + expected-deviation 0 + DEFECT 0 + ERROR 3
```

(26 agreed in the sweep, plus `state.VAL` which agreed on its own re-run after a
boot flake.) All three original DEFECTs — `bi.VAL` ordinals, `calc.VAL` missing
alarm, `mbbi.VAL` event count — are closed.

The **3 ERRORs are permanent, not flakes**: the stimulus put fails on both sides
and there is nothing to compare. They reproduce on every run.

| case | why it cannot run |
|---|---|
| `sel.VAL` | `caput` refused: `Write access denied` — `sel.VAL` is not writable with a default `SELM` |
| `sub.VAL` | `Write callback operation timed out` on **both** sides |
| `busy.VAL` | `Write callback operation timed out` on the C side |

`ba80e575` is what makes them visible: before it, a refused or timed-out drive
left both traces empty and `compare` called that agreement. Three cases the
harness cannot drive is a coverage gap in the monitor probe, and it is named
here rather than counted as a pass.

SUPERSEDED 2026-08-30 — the gap is closed, and the three rows split two ways.
`sel.VAL` was never a measurement failure: the `.dbd` declares it
`special(SPC_NOMOD)`, so no client can drive it and it leaves the drive
denominator, which is what the PVA monitor phase had always done and the CA
probe had not (`surface::val_status` now owns the rule for both). The other two
were the harness calling a reading an absence: a `caput -c` that times out on
the *callback* over a connected channel and a landed write is the server
declining to finish, not a write that did not happen, so it is now
`PutOutcome::NeverCompleted` and the drive counts as a stimulus. Re-measured on
`--phase all`: `sub.VAL` and `busy.VAL` monitor cases both AGREE (identical
event streams on both sides), and the monitor phase reports 0 ERROR.

The three original DEFECTs, kept as the record of what was fixed:

1. **`bi.VAL` — enum ordinals instead of strings.** C posts
   `['Illegal_Value','Illegal_Value']`, the port posts `['2','3']`.
2. **`calc.VAL` — alarm missing from the event.** C posts all four events
   carrying `[UDF INVALID]`; the port posts them with **no alarm**.
3. **`mbbi.VAL` — event count differs: C posts 0, the port posts 3.** C
   suppresses monitors for a value it will not accept; the port posts every put.

## Allowlist reconciliation

- **fired:** `CBUG-F6` (9 cases, put phase)
- **STALE:** `CBUG-E1` — the compress-FIFO deviation needs *three successive
  puts into one record*, and the put probe drives exactly one put per record
  instance (that is what keeps each case isolated and its reproducer minimal).
  The harness cannot fire it; it is reported STALE rather than silently dropped.
- **disabled (REPRODUCED, must not fire):** `CBUG-E2`, `CBUG-F12` — neither
  fired, as required.

The stale-row check earned its keep on day one: it flagged `CBUG-F6` as stale,
which turned out to be a bug in the **harness**, not the port — `caput` without
`-c` is fire-and-forget, so the server's rejection never reached the client and
every put looked accepted. Fixed in `9d824c3d`.

### Recomputed against the shipped allowlist, 2026-08-23

The file this reconciliation describes carried four rows at the run commit; it
carries eight now, all of them enabled — `enabled` defaults true and neither
`CBUG-E2` nor `CBUG-F12` sets it false any more. Note also that the three
commands under Reproducing are three processes with three separate ledgers, so
the merged list above is not an artifact the harness emits.

**now-stale — 1, and it is not the one predicted.** Measured 2026-08-25 on
`--phase put --record-types calc`, twice on each dbd:

- `CBUG-F6` **FIRES**, 9 cases, `INPM`..`INPU`. The prediction that base
  `2ab1bb14c` had killed it is right about a stock `softIoc` and wrong about
  this harness, whose `EPICS_ORACLE_IOC_BIN` is the older fat binary — see the
  ground-truth section.
- `CBUG-E2` is **STALE**: bounded by destination type (`ae481936`) it should
  have absorbed the `enum-negative-ordinal` and integer `over-max`/`under-min`
  cases, and it absorbs none, because the port now agrees with C on all of them.
  This is the one row that fails the run.
- `CBUG-E1` is **UNEXERCISED**, not stale — the label was wrong rather than the
  row. `5e205087` (2026-07-14, the day after this run) split UNEXERCISED out of
  STALE, and `compress.VAL` is `DBF_NOACCESS`, so no phase ever enumerates it.
  UNEXERCISED is coverage; STALE would fail the run.

**newly-required — 6.** `CBUG-E2` is enabled and bounded by destination type
(`ae481936`): it absorbs no `over-double-max` case and does absorb
`enum-negative-ordinal` and `over-max`/`under-min` on integer and enum
destinations, which the put table above counts as DEFECT. `CBUG-F12` was enabled
2026-08-03 with its scope corrected to `histogram.SGNL` `stat`/`sevr`; it was
disabled here, so nothing above counts it. `CBUG-G1`, `DESIGN-ASYN-BOUT`,
`INSTR-QSRV2-LONGIN-UTAG` and `INSTR-QSRV2-WAVEFORM-DEMO` are the four PVA rows:
this document has no PVA phase, so they change nothing above and nothing above
vouches for them.

**still-valid — 0.**

The `87999c3a..ae481936` window overstates nothing here. `87999c3a` (2026-07-14)
is what deleted `enabled = false` from `CBUG-E2`, one day *after* this run, so
the row could not fire in it; and the 32 is a case count from the put table, not
a row count. What the window does expose is any run made inside it: the
unbounded row would additionally have absorbed the `nan`, `over-float-max` and
`over-double-max` classes on every float and double destination — 171 driven
cases on `calc` alone, of which at least the 32 above actually differed.

## Whole-surface run — 2026-08-25, re-measured

The figures above are per-phase. `--phase all` on the fat default configuration,
run twice back to back on the same binaries, gave the **same numbers both
times**:

```
ran                : 23936
agreed             : 23446
expected deviation : 20   (CBUG-F6 x9, CBUG-E2 x1, CBUG-F12 x10)
DEFECT             : 0
ERROR              : 470
```

Identical case-for-case across the two runs (the 470 errored cases are the same
470, by record type, field, phase and class), at 1-minute load averages of 10.20
and 9.85 at the start and 11.75 and 541.23 at the end. So the 470 are not a load
flake: they are deterministic, and 465 of them are one record type.

**Why 465 `sub` cases cannot be measured, on either side.** The reproducer is
`record(sub, "ORACLE:SUB:6") {}` — no `SNAM`. C `subRecord.c:119-122` prints
`%s.SNAM is empty` and sets `prec->pact = TRUE`, permanently; `dbProcess`
returns early for a record already in PACT (`dbAccess.c:537`), so the completion
a `ca_put_callback` waits for never comes. Measured on a stock fat `softIoc`
with `record(sub,"T:SUB"){}` beside `record(ai,"T:AI"){}`:

```sh
caget -t T:SUB.PACT T:AI.PACT        # -> 1   0
caput -c -t -w 2 T:SUB.DESC hello    # -> Write callback operation timed out
caput -c -t -w 2 T:AI.DESC  hello    # -> hello
caput    -t -w 2 T:SUB.DESC bye      # -> bye   (a put with no callback lands)
```

The port reproduces this exactly — both sides time out on the identical set —
so the two IOCs *agree*, and the harness still cannot say so: `caput -c` is the
only mode in which "did the put succeed" is observable, and this record answers
it to nobody. The 465 are charged as ERROR, which is honest but leaves 1.94% of
the surface unadjudicated. Closing them needs a decision, not a patch: either
the `sub` reproducer stops being the bare record (no `function()` is registered
in a stock `softIoc`, so no `SNAM` resolves), or a case whose *both* sides time
out the write callback on a PACT-latched record gets its own bucket with this
citation attached. Neither is taken here.

The remaining 5: 4 `busy` and 1 `sel`, also identical across both runs.

SUPERSEDED 2026-08-30 — all 5 are closed, and none of them was a flake or an
instrument failure. `sel.VAL` left the monitor drive denominator (above). The 4
`busy` were C's `busy` record doing the one thing it exists to do: it declines
`recGblFwdLink()` while `VAL` is non-zero (busy `docs/busyRecord.md`), and
`recGblFwdLink()` is the only caller of `dbNotifyCompletion()`, so a `caput -c`
that leaves the record Busy is *meant* never to complete. Measured at the same
commit: the three put cases drove `VAL` to `1`, `2` and `-1` and timed out; the
`0` case completed normally. With the non-completion carried as a reading, the
monitor case AGREES and the three put cases become DEFECTs on `put_accepted` —
C answers "accepted, never completed", the port answers "completed". That is a
port divergence, not a C bug and not a harness failure. Re-measured
`--phase all`: `ran 23935 = agreed 23448 + expected-deviation 20 + DEFECT 3 +
no-completion 464 + ERROR 0`.

FIXED 2026-08-30 in the port, and the divergence was structural rather than
busy's own. `recGblFwdLink` is where `dbNotifyCompletion` lives, so a record
type that gates the forward link gates the put-callback with it; the port had
split that into two trait methods and honoured only one at the cycle tail. With
both read in `complete_put_notify`, the three cases agree on
"accepted, never completed" with identical readbacks (`Busy`,
`Illegal_Value`, `Illegal_Value`) and no differences at all:
`ran 23935 = agreed 23448 + expected-deviation 20 + DEFECT 0 +
no-completion 467 + ERROR 0`. `--phase pva-monitor` unchanged: 57 ran, 0 DEFECT,
0 ERROR.

## Reproducing

```sh
cargo build -p epics-oracle-rs
ORACLE_IOC_BIN=target/debug/oracle-ioc ./target/debug/oracle --phase read  --json read.json
ORACLE_IOC_BIN=target/debug/oracle-ioc ./target/debug/oracle --phase put   --record-types calc --json put.json
ORACLE_IOC_BIN=target/debug/oracle-ioc ./target/debug/oracle --phase monitor --json mon.json
```

Exit status is non-zero on any DEFECT, any ERROR, **any record type the port
does not implement, and any STALE allowlist row** — `report::run_failures`,
recomputed 2026-08-23. The last two conditions arrived with `74096a5b` and
`35762dbe`, after this run, so a re-run of the put command above also fails on
`CBUG-F6` going stale, for a reason the exit code reported here could not carry.
