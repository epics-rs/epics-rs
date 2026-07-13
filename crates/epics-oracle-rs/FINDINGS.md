# First oracle run — 2026-07-13

Measured, not asserted. Every number below is reproducible with the commands
given; every difference carries a two-line `.db` you can paste.

Ground truth: `softIoc` from `/home/stevek/work/epics-base/bin/linux-x86_64`.
Both sides driven by the same C CA tools.

## The denominator

Enumerated from `/home/stevek/work/epics-base/dbd/softIoc.dbd` (the expanded
dbd — `dbCommon` inlined), not hand-listed.

| | |
|---|---|
| record types in the dbd | 34 |
| record types the port instantiates | **34** (measured by booting each; 0 unimplemented) |
| CA-observable fields | **2551** |
| `DBF_NOACCESS` fields excluded | 594 (raw C pointers; no CA client can reach them) |

## Coverage

**2462 / 2551 = 96.5 %** of the observable surface produced a reading on both
sides and was diffed. The remaining **89 fields (3.5 %) are ERRORED, not
covered** — see below. Errors are never counted as coverage and never as
agreement.

Coverage is claimed from the **read** phase only. The put and monitor phases do
not visit every field, so quoting them as coverage would inflate the number.

## Read phase — all 34 record types

```
ran 2551 = agreed 2149 + expected-deviation 0 + DEFECT 313 + ERROR 89
```

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

**CBUG-F6 fires as an EXPECTED DEVIATION on exactly the nine fields it names**
(`calc.INPM`..`INPU`): C's `special()` rejects the `SPC_MOD` put, the port
accepts it. That is the allowlist working as designed — the difference is
justified by `doc/upstream-c-bugs.md` and is not counted as a defect.

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

Other put clusters, after removing that one:

| n | surface | C | port | example |
|---|---|---|---|---|
| 88 | put accepted | `false` | `true` | `PHAS` over-max — C refuses out-of-range puts the port takes |
| 32 | value | `0` | `inf` | `VAL` over-double-max — C clamps to 0, port stores `inf` |
| 12 | value (numeric) | `-1` | `0` | `SCAN` negative ordinal — C keeps `-1`, port clamps |
| 10 | STAT/SEVR | `UDF`/`INVALID` | `NO_ALARM` | put to `VAL` clears UDF on the port without processing |
| 8 | value | `0` | `-32768` / `32767` | `PHAS` over/under — port wraps, C rejects |
| 5 | value | `4` | `INVALID` | `UDFS` past-the-end ordinal — port saturates, C stores 4 |

## Monitor phase — event sequence and count

Six cases, driven `1, 2, 2, 3` (the repeated `2` tests no-change suppression):
**3 agreed, 3 DEFECT, 0 errors.**

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

## Reproducing

```sh
cargo build -p epics-oracle-rs
ORACLE_IOC_BIN=target/debug/oracle-ioc ./target/debug/oracle --phase read  --json read.json
ORACLE_IOC_BIN=target/debug/oracle-ioc ./target/debug/oracle --phase put   --record-types calc --json put.json
ORACLE_IOC_BIN=target/debug/oracle-ioc ./target/debug/oracle --phase monitor --json mon.json
```

Exit status is non-zero on any DEFECT **or any ERROR**.
