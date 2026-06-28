# modbus-rs — C-parity review (2026-06-28)

Codex-style C-parity audit of `crates/modbus-rs/src` against the upstream
EPICS **modbus** module (Mark Rivers) at
`/Users/stevek/codes/epics-modules/modbus/modbusApp/src`.

Round 1 (round id `01KW5XH4`) fanned out to 4 opus reviewers by category with
carved numbering ranges. Read-only sweep; this doc is the inventory. Fixes are a
separate phase (per-finding commits), each marked `cleared` here as it lands.

Reviewer principle 5 governs severity: where the Rust port intentionally
declines to reproduce a C **bug** (the user's standing steer: "find divergences
but do not copy C's bugs"), that is recorded as an **intentional-divergence
aside**, not a finding.

| Category | Rust | C reference | Range | Findings |
|---|---|---|---|---|
| 1 Protocol & framing | `protocol.rs`, `interpose.rs` | `modbus.h`, `modbusInterpose.c/.h` | R1–R15 | R1 |
| 2 Driver core & fcodes | `driver.rs` | `drvModbusAsyn.cpp` | R16–R30 | R16, R17, R18 |
| 3 Data-type & I/O | `datatype.rs`, `error.rs` | `drvModbusAsyn.cpp` (readPlc*/writePlc*) | R31–R45 | R31, R32, R33†, R34, R35 |
| 4 Records / IOC / stats | `ioc.rs` | `drvModbusAsyn.cpp` + `Db/*.template` | R46–R60 | R46–R54 |

† R33 is the same defect as R16 (Acknowledge-exception counter), found
independently by the datatype reviewer — folded into R16, not double-counted.

---

## Verified MATCHING (no finding — read both sides)

The wire path is byte-faithful to C. Confirmed against C lines actually read:

- **Function codes** (`protocol.rs:24-45`) ↔ `modbus.h:14-23` — all 10 identical.
- **PDU layouts** byte-for-byte: read / write-single / write-multiple-registers /
  write-multiple-coils (LSB-first pack) / read-write-multiple / report-slave-id
  (`protocol.rs:171-275`) ↔ `modbus.h:58-117` + `drvModbusAsyn.cpp:2010-2161`.
- **F23** read 11-byte frame (`numRead=len`, `numOutput=1`, `byteCount=2`, no data)
  and write (`numRead=1`, `numOutput=len`, `byteCount=2*len`)
  (`protocol.rs:262`, `driver.rs:376/397`) ↔ `drvModbusAsyn.cpp:2041-2056/2138-2161`.
- **MBAP header** (`protocol.rs:114-140`, `interpose.rs:146-147`)
  ↔ `modbusInterpose.c:254-257`: txid increments from 1, big-endian,
  `protocolType=0`, `cmdLength` incl. unit byte, `& 0xFFFF` wrap.
- **CRC-16/MODBUS** poly 0xA001 init 0xFFFF, low-byte-first append
  (`interpose.rs:62-75/159-160`) ↔ `modbusInterpose.c:174-188/280-281`
  (check vector `"123456789"==0x4B37`).
- **LRC** 8-bit two's-complement of the byte sum (`interpose.rs:78-81`)
  ↔ `modbusInterpose.c:190-199`.
- **ASCII framing** `':'` + uppercase hex over slave+data + LRC, CR/LF via output EOS
  (`interpose.rs:84-88/164-174`) ↔ `modbusInterpose.c:201-211/290-315`.
- **Exception detection** `fcode & 0x80` + exception byte; Acknowledge (5) non-fatal
  (`protocol.rs:321-328`, `driver.rs:502`) ↔ `drvModbusAsyn.cpp:2229-2238`.
- **fcode decode / max_length / check_offset** (`driver.rs:74-89/122-134/333-347`)
  ↔ `drvModbusAsyn.cpp:54-75/232-258/2395-2404`.
- **Constants** `MAX_READ_WORDS=125`, `MAX_WRITE_WORDS=123`, `READ_TIMEOUT=2s`,
  `WAGO_OFFSET=0x200` (`driver.rs:19-27`) ↔ `drvModbusAsyn.cpp:54-65`.
- **UDP retransmit** `++retries < 5` (`driver.rs:575-581`) ↔ `modbusInterpose.c:356`.
- **Numeric conversions** INT16 / UINT16 / INT16SM / BCD signed+unsigned /
  INT32 & INT64 (LE/BE ± byte-swap) / FLOAT32 & FLOAT64 word+byte order, register
  strides (`register_count` = C `bufferLen` 1/2/4), `ExceptionCode` 0x01–0x0B
  (`datatype.rs`, `error.rs`) ↔ `drvModbusAsyn.cpp` readPlc*/writePlc*. In-range exact.
- **readUInt32Digital** mask, **is_array_write_function**, **drvModbusAsynConfigure**
  arg parse, **LinkType** ordering (TCP0/RTU1/ASCII2/UDP3) all match
  (`ioc.rs` ↔ `drvModbusAsyn.cpp:375-430/542-551/3110-3173`, `modbusInterpose.h:14-17`).
- **drvUserCreate has no function-code override** in this C — only the 37 data-type
  strings + optional `=N`. Rust correctly parses no function override.

---

## Open Findings

### R1 — read loop aborts on a too-short reply where C re-reads
- **Severity:** CONCERN (no wrong wire bytes; noisy/fragmented links only)
- **Rust:** `driver.rs:550-551` (parse `interpose.rs:188-193`)
- **C:** `modbusInterpose.c:366-369`
- C's TCP `readIt` loop falls through a successful read with `nbytesActual < 2`
  and reads again; Rust calls `unwrap_response` on every `Ok(raw)` and propagates
  `FrameTooShort` via `?`, so a single spurious short read ends the transaction as
  an I/O error. The txid-mismatch half of the loop *is* reproduced
  (`stale_frames`, `driver.rs:558-564`); only the short-frame skip is missing.

### R16 — Acknowledge exception (code 5) wrongly increments READ_OK/WRITE_OK
- **Severity:** DEFECT (wrong statistic value) — corroborated by R33 and the
  protocol reviewer's out-of-category aside (3 independent finds)
- **Rust:** `driver.rs:502-509`
- **C:** `drvModbusAsyn.cpp:2231-2245` then OK-switch at 2251-2343
- On a Modbus exception 5 (command-will-take-long), C `status=asynSuccess; goto
  done;` jumps past the `readOK_++`/`writeOK_++` switch — neither counter moves.
  Rust's Acknowledge arm bumps both, and also resets `current_io_errors`. READ_OK/
  WRITE_OK over-count vs C on every Acknowledge.

### R17 — IO_ERRORS / currentIOErrors over-incremented on exception + malformed frames
- **Severity:** DEFECT (wrong statistic value; false error-rate can trip alarms)
- **Rust:** `driver.rs:511-515` and `520-524`
- **C:** `drvModbusAsyn.cpp:2204-2209` (the only `IOErrors_++` site, gated on the
  `writeRead` transport status)
- C bumps `IOErrors_`/`currentIOErrors_` only on transport `writeRead` failure. A
  Modbus exception response (`:2239-2245`) and a register / report-slave-id
  word-count mismatch (`:2284-2290/2306-2312`) set `asynError; goto done` without
  touching the counters. Rust increments on both paths.

### R18 — READ_OK pre-increment ordering on count-mismatch frames
- **Severity:** DEFECT (wrong statistic value, diverges opposite to R17)
- **Rust:** `driver.rs:518-530` (`read_ok`/`write_ok` only after `parse_response` Ok)
- **C:** `drvModbusAsyn.cpp:2278` (`readOK_++`) before the `:2284` mismatch check;
  `:2300` before `:2306` for report-slave-id
- C increments `readOK_` at the top of each read case, *before* validating
  `nread == len`, so a count-mismatch frame still reports `readOK_+1` then returns
  `asynError` (no `IOErrors_` bump). Rust leaves `read_ok` unchanged and bumps
  `io_errors`. The two records diverge from C in opposite directions per bad frame.

### R31 — read_string drops the trailing NUL from the char count (NORD off-by-one)
- **Severity:** CONCERN (content identical; count off by one)
- **Rust:** `datatype.rs:558-560` feeding `ioc.rs:606/614`
- **C:** `drvModbusAsyn.cpp:3050` (`*bufferLen = strlen(data)+1`) consumed at `:1479`
- C reports the char count **including** the terminating NUL; Rust reports `strlen`.
  A CHAR/UCHAR waveform gets `NORD=strlen` vs C `strlen+1`; asyn octet
  `ASYN_EOM_CNT` is one short.

### R32 — float→integer saturates instead of C's truncating cast
- **Severity:** CONCERN (boundary/NaN only; in-range exact)
- **Rust:** `datatype.rs:379` (`f as i32 as i64`; also `write_float` `:504`)
- **C:** `drvModbusAsyn.cpp:2572` (`(epicsInt32)fValue`)
- `f as i32` saturates (NaN→0, overflow→INT32_MAX); the C truncating cast yields
  INT_MIN (0x80000000) on x86 for NaN / out-of-range. The `read_int64` doc comment
  claiming "exactly as the C code does" is imprecise at this boundary.

### R33 — (DUPLICATE of R16) Acknowledge bumps READ_OK/WRITE_OK
- Folded into **R16**. Same defect, found independently by the datatype reviewer
  (`datatype.rs`/`driver.rs:503-507` ↔ `drvModbusAsyn.cpp:2237/2245`). Not counted
  separately; listed so the cross-reference is on record.

### R34 — per-record drvUser string length cap (`LEN=N`) dropped
- **Severity:** NOTE → CONCERN (changes wire register count for LEN-configured records)
- **Rust:** `ioc.rs:15-18` (the `=N` length is dropped; length comes from the record
  buffer only)
- **C:** `drvModbusAsyn.cpp:2367-2377` (`getStringLen` caps to `drvlen`), applied at
  `:1456` (readOctet) and `:1521/1524/1531` (writeOctet)
- A string record with explicit `LEN=N` smaller than its buffer reads/writes more
  chars (more/fewer wire registers) in Rust than C caps to. Tied to R51 (the
  validation half).

### R35 — BCD encode masks each digit to a nibble (intentional; aside)
- **Severity:** NOTE — intentional-divergence aside, **not a defect**
- **Rust:** `datatype.rs:460` (`out |= digit & 0xF`)
- **C:** `drvModbusAsyn.cpp:2638` (`ui16Value |= digit;` no mask)
- For a magnitude beyond valid BCD (≥16000) C lets overflow bleed into the adjacent
  nibble; Rust drops it. Valid 0–9999 byte-identical. Rust declining a C overflow
  quirk on invalid input — keep.

### R46 — POLL_DELAY write errors out; poll period can never change at runtime
- **Severity:** CONCERN (control param broken; valid write returns alarm)
- **Rust:** `ioc.rs:804-810` (`write_float64` does `datatype_of(reason)?` first, so the
  non-data POLL_DELAY reason → `Err`); poller uses a fixed `poll_delay` (`ioc.rs:1167-1177`)
- **C:** `drvModbusAsyn.cpp:1094-1099` (`writeFloat64` sets `pollDelay_=value` and signals
  the poller event)
- `poll_delay.template` binds an `ao` to POLL_DELAY; C retunes the poll period live,
  Rust fails the write (WRITE/INVALID alarm) and the period stays frozen.

### R47 — ENABLE_HISTOGRAM rising edge does not clear the histogram
- **Severity:** CONCERN (stale diagnostic counts carried across re-enable)
- **Rust:** `ioc.rs:819-822` (only sets `histogram_enabled`)
- **C:** `drvModbusAsyn.cpp:633-641` (on OFF→ON, zeros `timeHistogram_` before enabling)

### R48 — HISTOGRAM_BIN_TIME change does not clear the histogram (R47 family)
- **Severity:** CONCERN (stale counts misattributed to new bins)
- **Rust:** `ioc.rs:785-788` (sets `histogram_ms_per_bin` only)
- **C:** `drvModbusAsyn.cpp:794-803` (sets, clamps `<1→1`, then erases `timeHistogram_`)
- The axis rebuild is unneeded in Rust (axis computed on demand `ioc.rs:629-636`); the
  count erase is the missing part.

### R49 — READ_HISTOGRAM / HISTOGRAM_TIME_AXIS not served on Float64Array
- **Severity:** CONCERN (missing route; aai/waveform FTVL=DOUBLE binding errors)
- **Rust:** `ioc.rs:670-671` (`read_float64_array` does `datatype_of(reason)?` with no
  histogram case; only `read_int32_array` handles them at `ioc.rs:621-636`)
- **C:** `drvModbusAsyn.cpp:1181-1191` (`readFloat64Array` serves both, like `readInt32Array`
  at `:1350-1360`)
- Shipped `statistics.template` uses FTVL=LONG so the default path works; a Float64
  binding diverges.

### R50 — statistics counters never published in absolute-addressing mode (frozen at 0)
- **Severity:** CONCERN → wrong value (diagnostics read 0 vs live)
- **Rust:** `ioc.rs:394-411` (`publish_stats`, only writer) called only from `poll_cycle`
  (`ioc.rs:378`), which early-returns in absolute mode (`ioc.rs:343-345`); constructor
  seeds 0 (`ioc.rs:234-242`)
- **C:** `drvModbusAsyn.cpp:2205-2206/2213-2218/2254-2255/2300-2301/2340-2341` (`doModbusIO`
  itself `setIntegerParam`s the stats on every I/O)
- In absolute mode every per-record read runs `doModbusIO` (`ioc.rs:321-326`) and updates
  `engine.stats`, but they are never copied to the params → the `statistics.template`
  longins read 0 forever; C shows real counts.

### R51 — drvUser `=N` suffix validation dropped (C error routes missing)
- **Severity:** CONCERN (negative space; invalid drvInfo silently accepted)
- **Rust:** `ioc.rs:499-503` (splits on `=`, keeps prefix, drops the rest unvalidated)
- **C:** `drvModbusAsyn.cpp:387-413` (`=` valid only for the 8 string types → `asynError`
  for non-string; for string types `strtol` base-0 with `asynError` on garbage/negative)
- `INT16=5`, `STRING_HIGH=abc`, `STRING_HIGH=-3` all resolve in Rust where C rejects.
  (Dropping the length *value* is intentional, `ioc.rs:15-18` / R34; the missing piece is
  the *validation*.)

### R52 — drvUser bind does not reject an out-of-range offset (error deferred to first I/O)
- **Severity:** CONCERN (error at I/O time instead of init)
- **Rust:** `ioc.rs:496-503` (`drv_user_create` runs no `checkOffset`; offsets validated per
  accessor, e.g. `ioc.rs:513`)
- **C:** `drvModbusAsyn.cpp:378-384` (`drvUserCreate` does `getAddr`+`checkOffset`, returns
  `asynError`, failing record init)
- An over-range `addr` fails record init in C; in Rust it initializes and alarms on every
  I/O (never-connected vs alive-but-always-alarming).

### R53 — modbusInterposeConfig accepts timeoutMsec + writeDelayMsec but silently drops both
- **Severity:** CONCERN (configured timeout + inter-frame write delay ignored)
- **Rust:** `ioc.rs:1001-1015` (reads only `args[0]` port, `args[1]` link; `args[2]` timeout,
  `args[3]` writeDelay never read); transport uses fixed `READ_TIMEOUT` (`driver.rs:22-23`,
  `ioc.rs:1151`), no write delay
- **C:** `modbusInterpose.c:122-135` (`timeout=timeoutMsec/1000`, `writeDelay=writeDelayMsec/1000`);
  write delay is a pre-write `epicsThreadSleep` at `:246`
- A user-set read timeout and the inter-frame write delay (needed by slow serial PLCs) are
  discarded; the arg slots are accepted so there is no error signalling the loss.

### R54 — MODBUS_DATA modelled as Float64/Octet params, not C's single asynParamInt32 (routing risk UNVERIFIED)
- **Severity:** NOTE (intentional design divergence) — **but flags a routing risk to verify**
- **Rust:** `ioc.rs:202-209` (MODBUS_DATA + 37 type strings each their own param, numeric→Float64,
  string→Octet); poll fan-out sets the param then `call_param_callbacks(addr)` (`ioc.rs:366-389`)
- **C:** `drvModbusAsyn.cpp:213` (MODBUS_DATA is one `asynParamInt32` `P_Data`); `readPoller`
  manually fans `P_Data` to int32/int64/float64/octet/array/uint32digital interrupt clients
  (`:1674-1894`)
- **Risk:** whether asyn-rs delivers a Float64 param change to an asynInt32/asynInt64 I/O-Intr
  client (longin/bi/mbbi) is NOT verified. If it does not, those I/O-Intr records never update —
  a severe bug, not a NOTE. **Verify the asyn-rs param-routing before dispositioning.**

---

## Intentional-divergence asides (C bugs correctly NOT copied — do not "fix" back)

- **ASCII LRC verify off-by-one** — `interpose.rs:246-260` computes LRC over slave+data and
  compares the received LRC byte; fixes C `modbusInterpose.c:423-433` where `computeLRC` sums
  the received LRC byte into the LRC and compares one byte past the decoded region. Write-side
  bytes identical.
- **Frame-size bound** — `interpose.rs:270-276` rejects frames > 600; C has no runtime guard and
  can write past the 600-byte buffer (`modbusInterpose.c:277-281`).
- **MAX_STALE_FRAMES = 32** — `driver.rs:38/558-563` bounds the txid-mismatch re-read loop that is
  unbounded `for(;;)` in C (`modbusInterpose.c:346-370`) — a flooded-peer hang. → `Timeout`.
- **Histogram div-by-zero guard** — `driver.rs:251` `histogram_ms_per_bin.max(1)`; C
  `bin = msec/histogramMsPerBin_` (`drvModbusAsyn.cpp:2220`) has no zero guard.
- **drvModbusAsynConfigure `length < 0` guard** — `ioc.rs:1103` is stricter than C.

---

## Review Log

### Round 1 — 2026-06-28 (round `01KW5XH4`, 4 opus reviewers)

16 findings (R1, R16–R18, R31–R35, R46–R54); R33 folded into R16. Thematic clusters:

1. **Statistics-counter accounting** (R16, R17, R18, R50) — the dominant cluster. The Rust
   counters do not follow C's `goto done` control flow, so READ_OK / WRITE_OK / IO_ERRORS /
   LAST_IO_TIME / MAX_IO_TIME hold numerically wrong values (and in absolute mode, 0). Likely a
   single structural fix: route all stat updates through one owner that mirrors C's increment
   points + `goto done` skips, published on every I/O (not only poll_cycle).
2. **Histogram lifecycle** (R47, R48, R49) — enable rising-edge clear, bin-time-change clear,
   Float64Array serving all missing; one owner for "reset histogram" + add the Float64Array route.
3. **drvUser / config arg parsing** (R34, R51, R52, R53) — dropped `=N` validation, dropped
   init-time `checkOffset`, dropped interpose timeout/writeDelay. Negative-space: C error/validation
   routes the Rust port silently accepts.
4. **Control param** (R46) — POLL_DELAY write path broken end-to-end (write rejects + no runtime
   poll-period update).
5. **Conversion boundaries** (R31, R32) — string NUL count, float→int boundary cast.
6. **R54 routing risk** — verify FIRST: if asyn-rs Float64 params don't reach asynInt32 I/O-Intr
   clients, MODBUS_DATA for integer records is broken (severe), not a NOTE.

Wire-byte path verified clean end-to-end (PDU, MBAP, CRC/LRC, ASCII, function codes, conversions
in-range). All defects are in statistics/diagnostics/config/lifecycle, not the data path. No DEFECT
in transmitted bytes.

Fix phase: per-finding commits, `cleared` marked here as each lands; convergence rounds after each
cluster.
