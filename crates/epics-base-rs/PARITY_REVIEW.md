# epics-base-rs — EPICS base C Parity Review

Date: 2026-05-16
Scope: `crates/epics-base-rs` only.
Reference C source: `~/codes/epics-base` (EPICS 7 — `libcom`, `database`, `ca`).
Method: 10 parallel review agents, one per functional area, each comparing the
Rust source against the corresponding C source file-by-file. Read-only review;
no code was modified.

Per-area detail reports are in [`parity-review/`](parity-review/) — this file is
the consolidated index. Each finding ID below (`01-C-1`, `07-H-3`, …) is
`<area>-<severity>-<n>` and resolves to a section in the matching detail file.

---

## 1. Summary

| Area | File | Crit | High | Med | Low |
|------|------|-----:|-----:|----:|----:|
| 01 CALC engine | [01-calc.md](parity-review/01-calc.md) | 1 | 7 | 6 | 5 |
| 02 runtime + net | [02-runtime-net.md](parity-review/02-runtime-net.md) | 0 | 4 | 6 | 5 |
| 03 types + db loader | [03-types-dbloader.md](parity-review/03-types-dbloader.md) | 1 | 4 | 6 | 7 |
| 04 server/database | [04-database.md](parity-review/04-database.md) | 0 | 4 | 5 | 4 |
| 05 record infra + scan | [05-record-infra.md](parity-review/05-record-infra.md) | 0 | 2 | 2 | 4 |
| 06 records: analog/calc | [06-records-analog.md](parity-review/06-records-analog.md) | 0 | 2 | 6 | 6 |
| 07 records: binary/mbb | [07-records-binary.md](parity-review/07-records-binary.md) | 3 | 9 | 9 | 6 |
| 08 records: string/array/seq | [08-records-string.md](parity-review/08-records-string.md) | 2 | 11 | 8 | 8 |
| 09 iocsh + access security | [09-iocsh-as.md](parity-review/09-iocsh-as.md) | 3 | 6 | 4 | 5 |
| 10 iocInit + autosave | [10-iocinit-autosave.md](parity-review/10-iocinit-autosave.md) | 0 | 4 | 6 | 5 |
| **Total** | | **10** | **53** | **58** | **55** |

**Total findings: 176.**

A handful of record-level findings (`06-H-2`, `07-H-5/H-7/H-8/H-9/M-9`) are
flagged by the reviewing agent as *needs framework verification*: alarm
evaluation, UDF clearing and IVOA dispatch are partly centralised in
`record_instance.rs` / `processing.rs`. The agents inspected those files and
found no consuming code path, but a maintainer should confirm before treating
each as a confirmed defect. They are marked **[verify]** below.

Autosave findings in area 10 are marked **[unverified-ref]** where they depend
on exact synApps autosave C semantics — no synApps `autosave` C tree is present
under `~/codes`, so those were reviewed for internal consistency only.

---

## 2. Cross-cutting structural themes

These are root causes, not isolated bugs. Fixing the theme fixes many findings.

### T1 — Security / validation fails *open* instead of *closed*
The single most serious theme. The Rust port repeatedly omits C's defensive
validation and defaults to the permissive branch:
- **Access security** returns `ReadWrite` for an empty ASG, an unknown ASG with
  no `DEFAULT`, and an empty/rule-less ACF (`09-C-1/C-2/C-3`). C fails closed
  (`asNOACCESS`) in every one of these. **Any ACF misconfiguration makes the
  Rust IOC world-writable.**
- **CALC engine** has neither C's compile-time `runtime_depth == 1` check nor
  the runtime residual-stack check (`01-C-1`, `01-H-1`) — malformed postfix
  yields a silent wrong value instead of an error.
- **Port env-vars** are not range-checked against `IPPORT_USERRESERVED`
  (`02-H-2`); bad values are obeyed instead of falling back to the documented
  default.

### T2 — Record field models built to an old / reduced spec
Multiple records model fewer fields than current EPICS 7:
- `fanout` is missing `LNK0` (`04-H-1`, `07-C-1`).
- `seq` has 10 link groups, C has 16; 1-based `DLY1` vs C `DLY0` (`04-M-3`, `08-H-2`).
- `sub`/`aSub` expose `A..L` (12), C has `A..U` (21) (`08-H-3`, `08-H-4`).
- `mbbiDirect`/`mbboDirect` model 16 bits, C has 32 `B0..B1F` (`07-H-3`).
- `event` record `VAL` is `i16`, C is `DBF_STRING` event name (`05-H-2`, `07-H-1`).
- `EVNT` field is `i16`, C is `DBF_STRING` (`05-H-2`).
- `aSub` has no per-channel `FTx/FTVx` type fields — all forced DOUBLE (`08-H-5`).
- `dbCommon` is missing `UTAG` (`05-L-1`).

Effect: standard EPICS `.db` files referencing the missing fields fail to load,
or load with a silently shifted/truncated layout.

### T3 — Shared `select_link_indices` helper is wrong for every caller
`database/mod.rs::select_link_indices` drives `fanout`/`dfanout`/`seq` link
selection and is wrong three ways at once (`04-H-2`, `04-H-3`, `04-M-5`,
`07-C-2`, `07-C-3`):
- 0-based, but `dfanout` `SELN` is **1-based** in C with `SELN==0` = no output.
- Ignores `OFFS` (Specified-mode bias) and `SHFT` (Mask-mode shift) entirely —
  the fields exist on the record structs but are never read.
- Never raises C's `SOFT_ALARM/INVALID` for an out-of-range selector.
One helper fix closes all five findings.

### T4 — Integer conversion uses native Rust `as` casts, not C 32-bit wrap
C uses `epicsInt32` truncation and the `d2i`/`d2ui` macros (wrap-on-overflow);
Rust uses `as i32`/`as i64`/`as u32` (saturating, NaN→0):
- CALC `nint`, `%`, all bitwise/shift ops (`01-H-3/H-4/H-5`, repeated across
  `numeric.rs`, `string.rs`, `array.rs`).
- `histogram` bucket counts: `i32` with `+= 1` vs C `epicsUInt32` with explicit
  `UINT_MAX` wrap → overflow panic / negative counts (`08-C-2`).
- `ts` filter emits signed `Long` where C emits `DBF_ULONG` (`04-L-1`).

### T5 — Record alarm evaluation is incomplete  **[verify]**
State/COS/limit alarms appear not to be evaluated for several records:
- `bi/bo/mbbi/mbbo` never compute `ZSV/OSV`, `ZRSV..FFSV`, `UNSV`, COS alarms
  (`07-H-5/H-7/H-8`); the `AFTC/AFVL` alarm filter is dead code.
- `sel` and `dfanout` have no limit-alarm fields/logic at all (`07-M-2/M-3`).
- A calc/input record producing NaN reports `NO_ALARM` instead of `UDF_ALARM`
  because UDF is cleared unconditionally (`06-H-2`).
A maintainer must confirm whether `record_instance.rs`/`processing.rs`
re-implement any of this; the agents found no such path.

### T6 — `initHooks` subsystem is absent
The C IOC fires 13 `initHookAnnounce()` states across `iocBuild`/`iocRun`; the
Rust port fires none and has no `initHookRegister` API (`10-H-1`). Autosave works
only because pass0/pass1 restore is hard-coded into `ioc_app.rs`; any other hook
consumer (areaDetector, sequencer, caPutLog, devIocStats) has no entry point.
Related: PINI ordering vs CA-server start is not guaranteed (`10-H-2`).

### T7 — String / escape handling diverges from the C lexers
- DB lexer: Rust translates `\n`/`\"` in quoted field values; the C dbStatic
  lexer keeps escape bytes raw (`03-H-2`). Rust accepts a newline inside a
  quoted string; C aborts the parse (`03-H-3`).
- iocsh tokenizer does not support single-quote strings (`09-M-1`).
- No 40-char `MAX_STRING_SIZE` truncation on the `DBR_STRING` path for
  `stringin/stringout/lsi/lso` (`08-H-10`).

---

## 3. Critical findings (10)

| ID | Area | Finding | Impact |
|----|------|---------|--------|
| 09-C-1 | access sec | Empty-rule ASG returns `ReadWrite`; C returns `asNOACCESS` | `ASG(X){}` makes every channel world-writable |
| 09-C-2 | access sec | Unknown ASG + no `DEFAULT` returns `ReadWrite`; C synthesises empty `DEFAULT` ⇒ deny | `field(ASG,"TYPO")` ⇒ fully writable |
| 09-C-3 | access sec | Empty / rule-less / comment-only ACF yields a permissive config | Loading a half-finished ACF inverts the fail-safe posture |
| 07-C-1 | fanout rec | `fanout` record has no `LNK0` (15 links, C has 16) | First forward link never processed; all SELM indices shifted |
| 07-C-2 | fanout/dfanout | SELM=Specified wrong index base; `OFFS` ignored | dfanout drives wrong output link; `SELN=0` drives OUTA instead of nothing |
| 07-C-3 | fanout rec | SELM=Mask ignores `SHFT`, no range check | Wrong subset of forward links fires; no INVALID alarm |
| 08-C-1 | compress rec | `put_field("VAL")` overwrites circular buffer, desyncs `nuse`/`off` | `linearise_val` indexes out of bounds → **panic** / garbage |
| 08-C-2 | histogram rec | Bucket counts `i32` `+= 1` vs C wrapping `epicsUInt32` | Overflow panic (debug) / negative counts; wrong CA field type |
| 03-C-1 | types/codec | `DBR_STS_DOUBLE` RISC pad is 2 bytes; C struct uses 4 | Every `DBR_STS_DOUBLE` CA reply is 2 bytes short, payload shifted |
| 01-C-1 | CALC engine | `calcPerform` final-result stack-depth check missing | Malformed postfix returns a silent wrong scalar instead of an error |

The three access-security criticals (`09-C-1/2/3`) are one root cause (T1) and
should be fixed together: initialise access to `NoAccess`, always synthesise a
`DEFAULT` ASG, and never return `ReadWrite` on a lookup miss.

---

## 4. High findings (53)

### 4.1 CALC engine (`01`)
- `01-H-1` — compiler omits end-of-expression `runtime_depth == 1`
  (`CALC_ERR_INCOMPLETE`) and per-`;` `> 1` (`CALC_ERR_TOOMANY`) checks;
  `1 2` and `A;B` compile in Rust, rejected by C. `CalcError::TooMany` is dead code.
- `01-H-2` — `max()`/`min()` NaN propagation: C adopts NaN from the *incoming*
  operand, Rust checks the *accumulator*. `max(nan,1)` is `nan` in C, `1` in Rust.
- `01-H-3` — `nint()` uses `i64` truncation; C uses `epicsInt32` wrap.
- `01-H-4` — `MODULO` uses 64-bit operands + `i64` zero-test; C uses 32-bit.
- `01-H-5` — bitwise/shift ops use `as i32`/`as u32` (saturating); C uses `d2i`/`d2ui`.
- `01-H-6` — string evaluator `==` uses a `1e-11` epsilon; C uses exact IEEE compare.
- `01-H-7` — string evaluator forces divide-by-zero to NaN; C uses IEEE (`1/0 = +Inf`).

### 4.2 runtime + net (`02`)
- `02-H-1` — `env::get_bool` accepts `1`/`true`/`TRUE` (C rejects) and matches
  `yes` case-*sensitively* (C uses `epicsStrCaseCmp`). `EPICS_CA_AUTO_ADDR_LIST=Yes`
  gets opposite interpretation in the two IOCs.
- `02-H-2` — port env-vars not range-checked against `IPPORT_USERRESERVED` (5000);
  `EPICS_CA_SERVER_PORT=80` is obeyed by Rust, defaulted by C.
- `02-H-3` — `env::get_u16` silently swallows bad/out-of-range values; no
  diagnostic, and stricter parse than C's lenient `sscanf("%ld")`.
- `02-H-4` — no `epicsThread` priority / stack-size mapping; CA-server, scan and
  callback threads all run as undifferentiated tokio tasks. The `linux-rt`
  PI-mutex in `sync.rs` is dead infrastructure without thread priorities.

### 4.3 types + db loader (`03`)
- `03-H-1` — `dbr_buffer_size` hard-codes STS meta as flat 4 bytes; wrong for
  `STS_DOUBLE` (8) and `STS_CHAR` (5). Independent of `03-C-1`.
- `03-H-2` — quoted-string escape translation: Rust translates `\n`/`\"`; the C
  dbStatic lexer keeps escape bytes raw for `.db` field values.
- `03-H-3` — newline inside a quoted string is accepted; C aborts with
  "Newline in string, closing quote missing".
- `03-H-4` — cross-type array GET collapses the whole array to a single scalar
  zero (`to_f64` has no array arms).

### 4.4 server/database (`04`)
- `04-H-1` — `fanout` omits `LNK0`; forward-link layout shifted by one (= `07-C-1`).
- `04-H-2` — `dfanout` SELM=Specified off-by-one (`SELN` is 1-based in C) (= `07-C-2`).
- `04-H-3` — `fanout`/`seq` SELM ignores `OFFS`/`SHFT` (= `07-C-3`).
- `04-H-4` — MS-class alarm: `rec_gbl_set_sevr` does not clear the pending
  `namsg`; record reports the right severity with a stale/wrong `AMSG` string.

### 4.5 record infra + scan (`05`)
- `05-H-1` — event scan does not route by event number: `post_event()` processes
  *every* `SCAN=Event` record regardless of `EVNT`. The `pevent_list[]` /
  `eventNameToHandle` routing layer is absent.
- `05-H-2` — `EVNT` field typed `i16`; C is `DBF_STRING` (40-char event name).
  Named-event databases do not work; `.EVNT` wire type mismatches a C IOC.

### 4.6 records: analog/calc (`06`)
- `06-H-1` — `longout`/`int64out` never clamp `VAL` to `DRVH/DRVL`; C clamps every
  cycle. Hardware can be commanded past its configured drive limits.
- `06-H-2` **[verify]** — UDF cleared unconditionally every successful cycle;
  a calc producing NaN, or an input whose link read failed, reports `NO_ALARM`
  instead of `UDF_ALARM`.

### 4.7 records: binary/mbb (`07`)
- `07-H-1` — `event` record `VAL` is `i16`, C is `DBF_STRING`; cannot store
  named events.
- `07-H-2` — `event` record has no `process()` / `postEvent`; it never fires the
  EPICS event mechanism — non-functional as an event source.
- `07-H-3` — `mbbiDirect`/`mbboDirect` model 16 bits; C uses 32 (`B0..B1F`).
  Upper 16 bits unreachable; `NOBT>16` mis-clamped.
- `07-H-4` — `mbboDirect` forces `RBV = RVAL` every process, destroying the
  device-support read-back value.
- `07-H-5` **[verify]** — `bo` has no state/COS alarm; the HIGH timer reprocesses
  without setting `VAL=0`, so a momentary/pulsed `bo` never resets.
- `07-H-6` — `busy` IVOA "Don't drive" is a no-op (OUT still written on INVALID);
  HIGH unimplemented.
- `07-H-7` **[verify]** — `bi/mbbi` never evaluate STATE/COS alarms; `AFTC/AFVL`
  alarm filter is dead code.
- `07-H-8` **[verify]** — `mbbo` has no state/COS alarm and no `SOFT_ALARM` on an
  illegal `VAL`.
- `07-H-9` — `sel` SELM=Specified: `seln` is `i16` (C: `epicsUInt16`); a
  negative/out-of-range `SELN` silently keeps the previous `VAL`, no INVALID alarm.

### 4.8 records: string/array/seq (`08`)
- `08-H-1` — `seq` record has **no process logic at all** — SELM/SELN/DLY/DOL/LNK
  inert; seq records never drive their output links.
- `08-H-2` — `seq` has 10 link groups, C has 16; 1-based `DLY1..DLYA` vs C
  `DLY0..DLYF`. Standard databases fail to link.
- `08-H-3` — `sub` record exposes 12 inputs (`A..L`); C `subRecord` has 21 (`A..U`).
- `08-H-4` — `aSub` exposes 12 of 21 channels; `VALM..VALU`, `NOM..NOU` etc. absent.
- `08-H-5` — `aSub` has no per-channel `FTx/FTVx` type fields; all channels forced
  to DOUBLE — non-double aSub channels behave incorrectly.
- `08-H-6` — `printf` `%s` formats the numeric `f64` input instead of the link's
  string value; defeats the printf record's primary purpose.
- `08-H-7` — `printf` lacks `%ls`, `%c`, `*` variable width, and `h`/`l` length
  modifiers.
- `08-H-8` — `lsi/lso` `SIZV` minimum not enforced; C clamps to `[16, 0x7fff]`.
- `08-H-9` — `lsi/lso` `process()` recomputes `len`/`oval` every cycle; C only on
  change. `OLEN` reports the wrong value.
- `08-H-10` — `stringin/stringout/lsi/lso` lack 40-char (`MAX_STRING_SIZE`)
  truncation on the `DBR_STRING` path.
- `08-H-11` — `histogram` has no `CSTA`/`CMD` start-stop; counting cannot be paused.

### 4.9 iocsh + access security (`09`)
- `09-H-1` — HAG hostname matching is case-sensitive; C lowercases both sides.
  A host rule silently fails to match — or is evaded by different-cased hostname.
- `09-H-2` — `TRAPWRITE` / `asTrapWrite` write-auditing entirely missing; the
  `TRAPWRITE` rule option is parsed-but-dropped.
- `09-H-3` — `CALC` rule clause unsupported and silently dropped — a conditional
  WRITE rule becomes unconditional, granting access C would deny.
- `09-H-4` — `INP(A..U)` ASG input links unsupported; calc-based access security
  is non-functional.
- `09-H-5` — the whole `as*` iocsh command family plus dozens of standard
  `db*`/core commands (`iocshCmd`, `on`, `var`, `dbl`-family, `postEvent`, …) are
  unregistered; a stock `st.cmd` errors on the first unknown command.
- `09-H-6` — `dbsr` is implemented as a record-name glob search; C `dbsr` is the
  "Database Server Report" (CA server status).

### 4.10 iocInit + autosave (`10`)
- `10-H-1` — no `initHooks` subsystem (see theme T6).
- `10-H-2` — PINI records process inside the scan task after the database is
  handed to the protocol runner; no ordering guarantee against the CA listener
  accepting connections. A client can `caget` an unprocessed UDF value.
- `10-H-3` — `after_init_hooks` are collected but `IocApplication::run` never
  executes them; whether they fire depends on the external runner draining a vector.
- `10-H-4` — `verify.rs` does `read_save_file(...).unwrap_or_default()`,
  collapsing a corrupt save file into an empty list — `asVerify` reports
  "all match" for exactly the corruption it exists to detect.

---

## 5. Medium and Low findings (113)

Full detail per finding is in the area files. One-line index of the Medium set;
Low findings are in the detail files only.

**01 CALC** — `M-1/M-2` fmod/atan2 arg order (verified correct, do-not-touch
notes); `M-3` `0x` hex literal accepts 64-bit, C is 32-bit; `M-4` literals not
classed int vs double; `M-5` no `calcErrorStr`/numeric error codes; `M-6`
`cond_search` nesting model unproven vs C.

**02 runtime/net** — `M-1` general-time ratchet always applied (C bypasses with
only the OS clock); `M-2` `generalTimeGetExceptPriority` / interrupt providers
missing; `M-3` `generalTimeReport` output format mismatch; `M-4`
`notify_clock_sync` is a Rust-only API presented as parity; `M-5` `IfaceMap`
keeps down-but-addressed interfaces (no `IFF_UP` check); `M-6`
`bind_ephemeral_same_port` silently degrades to single-NIC.

**03 types/dbloader** — `M-1` macro values not re-expanded (no chained
expansion); `M-2` `$(name,sub=val)` scoped-macro syntax unsupported; `M-3`
macros expanded inside single quotes (C suppresses); `M-4` `substitute`
directive splitting not quote-aware; `M-5` macro reference name not expanded
before lookup; `M-6` `DBR_STSACK_STRING` encodes but cannot decode.

**04 database** — `M-1` RPRO reprocessed inline, not via queued `scanOnce`;
`M-2` channel filters run on single-read context (C bypasses `dec`/`sync`);
`M-3` `seq` models 10 of 16 link groups, ignores per-group `DLY`; `M-4` `arr`
filter ignores circular-buffer offset; `M-5` SELM out-of-range raises no alarm.

**05 record infra** — `M-1` same-PHAS records scanned alphabetically, not in
database load order; `M-2` `recGblResetAlarms` monitor masks collapsed — SEVR
over-posted on stat-only transitions.

**06 records analog** — `M-1` `calcout` has no `ODLY/DLYA` output delay; `M-2`
`calcout` `OEVT` event posting missing; `M-3` `ao` IVOA=2 does not re-convert
`RVAL`; `M-4` `ao` `OVAL/RVAL` archive monitors not force-posted; `M-5`
`calc/calcout` `LA..LU` advanced in `process()` instead of on monitor post.
(`scalcout`/`transform` are synApps — `S-1/S-2` swallowed eval errors, `S-4`
`COPT=Conditional` not enforced — no epics-base C reference.)

**07 records binary** — `M-1` `sel` Specified does not propagate NaN→UDF; `M-2`
`sel` limit alarms entirely missing; `M-3` `dfanout` limit alarms / deadband /
UDF missing (its IVOA branch is therefore unreachable); `M-4` `sel`
High/Low/Median do not write the winning `SELN`; `M-5` `sel` Median zero-input
handling; `M-6` `mbbi/mbbo` `SDEF` not recomputed on runtime field write;
`M-7` `bo` `DOL` constant parse rejects negative/hex/float; `M-8` `bi` raw soft
read does not clear UDF; `M-9` **[verify]** input records' `process()` never
clears UDF.

**08 records string** — `M-1` `compress` scalar N-to-1 uses an N-element
accumulator instead of C's running `cvb` (`CVB`/`INX` wrong mid-cycle); `M-2`
`compress` `ILIL/IHIL` filters every sample, C only skips a leading run; `M-3`
`histogram` bin-index formula differs on bucket boundaries; `M-4` `histogram`
`SDEL`/`MDEL` throttling not implemented; `M-5` `compress` `process()` does not
read `INP`; `M-6` `waveform` `NELM` put reallocates and zeros the buffer; `M-7`
`stringout` `DOL`/IVOA not applied; `M-8` `lsi/lso` `LEN` initialised to 1, C to 0.

**09 iocsh/as** — `M-1` single-quote quoting unsupported in the tokenizer;
`M-2` `RULE` level parse silently defaults a garbage level to 1; `M-3` `RULE`
access keyword: anything not `WRITE` treated as READ (incl. `NONE`); `M-4` ACF
unknown top-level block is a hard error (C warns and continues); `M-6` iocsh
`on error` semantics absent.

**10 iocInit/autosave** — `M-1` `wire_device_support` runs `dev.init()` /
`set_record_info` in opposite orders in the two build paths; `M-2` `IocBuilder`
ignores `dev.init()` failures; `M-3` **[unverified-ref]** triggered save sets
built as `OnChange` polling, not a trigger-PV watcher; `M-4`
**[unverified-ref]** restore uses `put_pv_no_process` — no link/PINI
propagation; `M-5` no `.savB` written for the first save; `M-6`
**[unverified-ref]** Rust-written `.sav` is not C-readable (`CompatMode` enum
defined but unused).

Low findings (55): see the detail files. Notable ones — `04-L-1` `ts` filter
signed vs `DBF_ULONG`; `05-L-2` `TSEL` link parsed but never read; `05-L-3`
periodic scan threads have no priority ordering; `07-L-2` `busy` enum-string put
coerces garbage to 0; `08-L-4` `sub` `process()` is a no-op (subroutine
invocation external — needs framework confirmation); `10-L-1/L-2` `getenv`
device support inconsistent unset-var handling.

---

## 6. Verified-correct (spot-checked, no divergence)

The agents explicitly confirmed these match C — recorded so a future review does
not re-flag them:

- Alarm severity/status enums, `menuScan` rate values & numbering, `recGblSetSevr`
  maximize logic, `recGblResetAlarms` transfer + `ackt/acks`, `recGblInheritSevrMsg`
  MS/MSI/MSS/NMS, TSE constants `0/-1/-2`, FLNK forward-link semantics (`05`).
- `processing.rs` PACT entry guard, FLNK `processTarget` PUTF propagation, the
  `dbnd`/`decimate`/`sync`/`arr` filter cores, `apply_timestamp` TSE handling (`04`).
- TIME-layer RISC pad, GR/CTRL struct layouts, `DBF_CHAR` signedness,
  `DBR_CLASS_NAME`, empty-array handling, backslash-escape level-0 semantics (`03`).
- `cas_server_port` precedence, `ca_mcast_ttl` clamping, general-time provider
  FIFO insertion order, per-event ratchet, `SO_REUSEADDR`/`SO_REUSEPORT` policy,
  Linux `IP_MULTICAST_ALL=0` (`02`).
- CALC `fmod`/`atan2` argument order, `checksum` known-answer tests (`01`).
- Autosave atomic save-file write (open→write→`sync_all`→rename→dir-sync),
  include depth/cycle limits, `macEnvExpand` semantics, last-wins dedup (`10`).

---

## 7. Recommended fix order

1. **`09-C-1/C-2/C-3`** — access-security fail-open. Security defect; one
   coherent fix (theme T1). Init `access = NoAccess`, always synthesise
   `DEFAULT`, never return `ReadWrite` on a miss.
2. **`08-C-1`, `08-C-2`** — panic / memory-safety in `compress` and `histogram`.
3. **`03-C-1` + `03-H-1`** — `DBR_STS_DOUBLE` wire layout; breaks CA interop with
   real IOCs for every status-double reply.
4. **`07-C-1/C-2/C-3` + `04-H-1/H-2/H-3`** — `fanout`/`dfanout`/`seq` link
   selection; one shared helper + the `fanout` `LNK0` field (theme T3).
5. **`01-C-1` + `01-H-1`** — CALC stack-depth validation.
6. **Theme T2** — bring the reduced record field models up to EPICS 7 spec
   (`seq`, `sub`, `aSub`, `mbbiDirect`/`mbboDirect`, `event`/`EVNT`).
7. **`05-H-1/H-2`, `08-H-1`, `07-H-2`** — event routing and the inert `seq`/`event`
   records.
8. Confirm the **[verify]** findings (T5) against `record_instance.rs` /
   `processing.rs`, then fix the confirmed alarm-evaluation gaps.
9. **Theme T6** — add an `initHooks` subsystem; remove the hard-coded autosave
   pass0/pass1 wiring once hooks exist.
