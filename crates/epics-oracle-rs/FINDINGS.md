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

## The denominator

Enumerated from `/home/stevek/work/epics-base/dbd/softIoc.dbd` (the expanded
dbd — `dbCommon` inlined), not hand-listed.

| | | status |
|---|---|---|
| record types in the dbd | 34 | recomputed 2026-08-23 from the file named above |
| record types the port instantiates | **34** (measured by booting each; 0 unimplemented) | UNVERIFIED — `74096a5b` moved `probe_supported_record_types` from a bare `IocBuilder` onto `port_ioc_builder`, so this was measured against a different IOC configuration than the one under test |
| CA-observable fields | **2551** | UNVERIFIED — the same path enumerates **2553** today. The run's copy of the file is gone, so the delta is attributed, not measured: `c9817fa59` (`bi.AFTC`, `bi.AFVL`) is base's only field-adding commit since 2025-10, and `dbd/softIoc.dbd` was rebuilt 2026-08-11 |
| `DBF_NOACCESS` fields excluded | 594 (raw C pointers; no CA client can reach them) | recomputed 2026-08-23 |

The harness no longer defaults to this file. `CTools::DEFAULT_DBD` is now the fat
`/home/stevek/work/oracle-ioc/dbd/softIoc.dbd` (`e45a697f`), which enumerates 40
record types and 3388 CA-observable fields (736 `DBF_NOACCESS` excluded). A
re-run with defaults therefore measures a different surface than everything
below; set `EPICS_ORACLE_DBD` back to the path above to reproduce this one.

## Coverage

**2462 / 2551 = 96.5 %** of the observable surface produced a reading on both
sides and was diffed. The remaining **89 fields (3.5 %) are ERRORED, not
covered** — see below. Errors are never counted as coverage and never as
agreement.

UNVERIFIED. The denominator moved to 2553 (above), and `74096a5b` keeps a dark
record type's fields in the denominator while `select_types` still drives only
the implemented ones, so one unimplemented type now lowers this percent instead
of leaving it unchanged. `d683e817` does **not** invalidate it: the filter it
fixed counted any case carrying no boundary class as a read case, and the run
below was `--phase read`, whose case list holds nothing else.

Coverage is claimed from the **read** phase only. The put and monitor phases do
not visit every field, so quoting them as coverage would inflate the number.

## Read phase — all 34 record types

```
ran 2551 = agreed 2149 + expected-deviation 0 + DEFECT 313 + ERROR 89
```

UNVERIFIED. The read probe is untouched by the harness fixes on this branch, but
`ran` is the denominator and that is now 2553, and the C side is no longer the
binary that produced this split: base fixed `sel`, `seq`, `printf`, `histogram`
and `calc` between 2026-07-19 and 2026-07-21. The 89-field list and the cluster
table below are what a re-run must reproduce, not what it may assume.

### The 89 ERRORs are all one-sided: C serves the field, the port does not

Not a harness failure — `caget`/`cainfo` connect on C and time out on the port.
A C client can see these fields on the C IOC and cannot see them on the port, so
each is also a Tier-1 divergence; the harness scores them ERROR because no
reading was obtained, and names every one rather than rounding them into a pass.

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

### DEFECT clusters (313 cases, 559 differences)

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
of 2026-08-23. The four-way split is UNVERIFIED. `27d9c135` read STAT and SEVR
through `caget_batch(..).ok()`, so one unconnectable `.STAT` removed the alarm
surface from every case of the record type while they all stayed AGREED, and
`30f5b1b9` collapsed every `caput` failure into `accepted: false` on both sides,
which reads as agreement about a write neither IOC saw. Both push AGREED up and
DEFECT/ERROR down, so `ERROR 0` is the number most at risk.

**CBUG-F6 fires as an EXPECTED DEVIATION on exactly the nine fields it names**
(`calc.INPM`..`INPU`): C's `special()` rejects the `SPC_MOD` put, the port
accepts it. That is the allowlist working as designed — the difference is
justified by `doc/upstream-c-bugs.md` and is not counted as a defect.

That can no longer happen. Base `2ab1bb14c` (2026-07-21) dropped
`special(SPC_MOD)` from `calcRecord.dbd`'s `INPM`..`INPU`, so the C IOC now
accepts those puts and there is no deviation left to justify. `calcoutRecord.dbd`
still declares it, so the row's `calcout` half stands; on a `calc`-only re-run
the row is exercised, never fires, and reports STALE — which since `35762dbe`
fails the run on its own.

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

The 537 (272 + 265) is UNVERIFIED for the same reason as the split above: under
`27d9c135` a dropped STAT/SEVR batch could only lose differences, never invent
them, so this is a floor rather than a count.

Other put clusters, after removing that one:

| n | surface | C | port | example |
|---|---|---|---|---|
| 88 | put accepted | `false` | `true` | `PHAS` over-max — C refuses out-of-range puts the port takes |
| 32 | value | `0` | `inf` | `VAL` over-double-max — C clamps to 0, port stores `inf` |
| 12 | value (numeric) | `-1` | `0` | `SCAN` negative ordinal — C keeps `-1`, port clamps |
| 10 | STAT/SEVR | `UDF`/`INVALID` | `NO_ALARM` | put to `VAL` clears UDF on the port without processing |
| 8 | value | `0` | `-32768` / `32767` | `PHAS` over/under — port wraps, C rejects |
| 5 | value | `4` | `INVALID` | `UDFS` past-the-end ordinal — port saturates, C stores 4 |

Every `n` there is a measured outcome and UNVERIFIED, but three rows have a
known direction. The **88 put accepted** cases are over-counted by however many
had a C-side `caput` that timed out rather than being refused: `30f5b1b9` makes
those ERROR. The **32 VAL over-double-max** is a difference count, not a case
count — the plan drives 57 `over-double-max` cases on `calc`, one per DBF_FLOAT
or DBF_DOUBLE put candidate — and `ae481936` bounds `CBUG-E2` by destination
type, so a DBF_DOUBLE takes no integer cast and these stay DEFECT rather than
being absorbed. The **12 SCAN negative ordinal** and **8 PHAS over/under** rows
now fall inside that bounded `CBUG-E2` scope (`DBF_MENU`
`enum-negative-ordinal`, `DBF_SHORT` `over-max`/`under-min`) and the row is
enabled at HEAD, so up to 20 of these move from DEFECT to EXPECTED DEVIATION on
a re-run; `calc`'s plan drives 34 cases in that scope in all.

## Monitor phase — event sequence and count

Six cases, driven `1, 2, 2, 3` (the repeated `2` tests no-change suppression):
**3 agreed, 3 DEFECT, 0 errors.**

UNVERIFIED, and the case count does not reconcile with the probe.
`probe_monitor` emits one case per record type whose `VAL` is present, is a put
candidate and is not a link; on the dbd above that gate admits **25** of the 34
types, and the gate is byte-identical at the run commit, so six is unexplained
rather than superseded. The verdicts are separately in question: `ba80e575` made
the stimulus puts propagate their failure, where before a refused or timed-out
drive left both traces empty and `compare` called that agreement — so `3 agreed`
and `0 errors` are precisely the two numbers that fix moves.

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

**now-stale — 2.** `CBUG-F6`: the deviation stopped existing, because base
`2ab1bb14c` removed `special(SPC_MOD)` from `calcRecord.dbd` and C now accepts
the nine puts the row says it refuses; on this document's `--record-types calc`
run the row is exercised, never fires, and that is a run failure since
`35762dbe`. `CBUG-E1`: the label is wrong rather than the row — `5e205087`
(2026-07-14, the day after this run) split UNEXERCISED out of STALE, and
`compress.VAL` is `DBF_NOACCESS`, so no phase ever enumerates it. It is
UNEXERCISED, which is coverage; STALE would fail the run.

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
