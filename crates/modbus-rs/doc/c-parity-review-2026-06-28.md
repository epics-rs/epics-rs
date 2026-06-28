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

### R16 — Acknowledge exception (code 5) wrongly increments READ_OK/WRITE_OK — CLEARED (f2361013 do_modbus_io control-flow rewrite, closes R16+R17+R18+R33)
- **Severity:** DEFECT (wrong statistic value) — corroborated by R33 and the
  protocol reviewer's out-of-category aside (3 independent finds)
- **Rust:** `driver.rs:502-509`
- **C:** `drvModbusAsyn.cpp:2231-2245` then OK-switch at 2251-2343
- On a Modbus exception 5 (command-will-take-long), C `status=asynSuccess; goto
  done;` jumps past the `readOK_++`/`writeOK_++` switch — neither counter moves.
  Rust's Acknowledge arm bumps both, and also resets `current_io_errors`. READ_OK/
  WRITE_OK over-count vs C on every Acknowledge.

### R17 — IO_ERRORS / currentIOErrors over-incremented on exception + malformed frames — CLEARED (with R16)
- **Severity:** DEFECT (wrong statistic value; false error-rate can trip alarms)
- **Rust:** `driver.rs:511-515` and `520-524`
- **C:** `drvModbusAsyn.cpp:2204-2209` (the only `IOErrors_++` site, gated on the
  `writeRead` transport status)
- C bumps `IOErrors_`/`currentIOErrors_` only on transport `writeRead` failure. A
  Modbus exception response (`:2239-2245`) and a register / report-slave-id
  word-count mismatch (`:2284-2290/2306-2312`) set `asynError; goto done` without
  touching the counters. Rust increments on both paths.

### R18 — READ_OK pre-increment ordering on count-mismatch frames — CLEARED (with R16)
- **Severity:** DEFECT (wrong statistic value, diverges opposite to R17)
- **Rust:** `driver.rs:518-530` (`read_ok`/`write_ok` only after `parse_response` Ok)
- **C:** `drvModbusAsyn.cpp:2278` (`readOK_++`) before the `:2284` mismatch check;
  `:2300` before `:2306` for report-slave-id
- C increments `readOK_` at the top of each read case, *before* validating
  `nread == len`, so a count-mismatch frame still reports `readOK_+1` then returns
  `asynError` (no `IOErrors_` bump). Rust leaves `read_ok` unchanged and bumps
  `io_errors`. The two records diverge from C in opposite directions per bad frame.

### R31 — read_string drops the trailing NUL from the char count — CLEARED (intentional divergence, asyn-rs octet contract)
- **Severity:** NOTE — intentional-divergence aside, **not a defect**
- **Rust:** `datatype.rs` `read_string` feeding `ioc.rs` `read_octet` (:636/:644)
- **C:** `drvModbusAsyn.cpp:3050` (`*bufferLen = strlen(data)+1`) consumed at `:1479`
- C reports the char count **including** the terminating NUL because the C
  consumer (`devAsynOctet`) treats the read buffer as a NUL-terminated C-string —
  the extra count is the terminator the consumer then strips. The asyn-rs octet
  interface contract is different: `asyn-rs/src/adapter.rs:1228-1231`
  (`result_to_value` for `"asynOctet"`) builds
  `EpicsValue::String(String::from_utf8_lossy(&d[..n]))`, so the returned count
  `n` is the **exact payload byte length** with no separate NUL-terminator
  convention. Reporting C's `strlen+1` in Rust would feed the framework one extra
  byte and **embed a spurious NUL** into the delivered String (corrupting every
  `stringin`/CHAR-waveform VAL). modbus-rs correctly returns `strlen`; the
  record-visible string content is byte-identical to C. The only C-observable
  effect of the `+1` is a trailing zero element in a CHAR-waveform `NORD`, which
  cannot be replicated under the asyn-rs contract without that NUL corruption — a
  C convention deliberately not copied. (The separate "C always sets
  `ASYN_EOM_CNT`, asyn-rs synthesises CNT only when the buffer fills" gap is a
  distinct EOM-reason concern, not this count off-by-one — see R55.)

### R32 — float→integer saturates instead of C's truncating cast — CLEARED (bf98cffa, intentional divergence)
- **Severity:** NOTE — intentional-divergence aside, **not a defect**
- **Rust:** `datatype.rs:379` (`f as i32 as i64`; also `write_float` `:504`)
- **C:** `drvModbusAsyn.cpp:2572` (`(epicsInt32)fValue`)
- `f as i32` saturates (NaN→0, overflow→INT32_MAX); the C truncating cast yields
  INT_MIN (0x80000000) on x86 for NaN / out-of-range — UB the standard does not pin.
  Per "do not copy C's bugs" the saturating result is kept; in-range values are
  byte-identical. The original disposition was a CONCERN only because the doc comments
  overclaimed bit-exactness ("exactly as the C code does", "matching C"); bf98cffa
  rewrites those comments to state the deliberate boundary difference. No behaviour change.

### R33 — (DUPLICATE of R16) Acknowledge bumps READ_OK/WRITE_OK — CLEARED (with R16)
- Folded into **R16**. Same defect, found independently by the datatype reviewer
  (`datatype.rs`/`driver.rs:503-507` ↔ `drvModbusAsyn.cpp:2237/2245`). Not counted
  separately; listed so the cross-reference is on record.

### R34 — per-record drvUser string length cap (`LEN=N`) dropped — STRUCTURAL BLOCK (asyn-rs contract; sign-off, sibling of R54)
- **Severity:** NOTE → CONCERN (changes wire register count for LEN-configured records)
- **Rust:** module docs (the `=N` length *value* is dropped; length comes from the record
  buffer `NELM` only)
- **C:** `drvModbusAsyn.cpp:2367-2377` (`getStringLen` caps the asyn `maxLen` to the per-record
  `drvUser->len`), applied at `:1456` (readOctet) and `:1521/1524/1531` (writeOctet)
- A string record with explicit `LEN=N` smaller than its buffer reads/writes more chars (more/
  fewer wire registers) in Rust than C caps to.
- **Blocker:** C stores the per-record cap in `pasynUser->drvUser` (`modbusDrvUser_t.len`), set in
  `drvUserCreate` per record. The asyn-rs `PortDriver::drv_user_create(&self, drv_info) -> usize`
  contract returns only a **shared reason index** with no per-record state slot — two records both
  `STRING_HIGH` with different `LEN` resolve to the same reason, and the accessor sees only the
  record buffer length (`buf.len()`), never the cap. Honouring `LEN=N` needs an asyn-rs mechanism
  to carry per-record driver data (C's `drvUser`) — a framework change spanning `modbus-rs` ↔
  `asyn-rs`, the same class as R54. The *validation* half is closed (R51); the *value* application
  is **surfaced for design sign-off**, not patched. (`AsynUser.user_data: Option<Box<dyn Any>>`
  exists but `drv_user_create` neither receives the `AsynUser` nor returns per-user data, so there
  is no wiring path today.)

### R35 — BCD encode masks each digit to a nibble (intentional; aside)
- **Severity:** NOTE — intentional-divergence aside, **not a defect**
- **Rust:** `datatype.rs:460` (`out |= digit & 0xF`)
- **C:** `drvModbusAsyn.cpp:2638` (`ui16Value |= digit;` no mask)
- For a magnitude beyond valid BCD (≥16000) C lets overflow bleed into the adjacent
  nibble; Rust drops it. Valid 0–9999 byte-identical. Rust declining a C overflow
  quirk on invalid input — keep.

### R46 — POLL_DELAY write errors out; poll period can never change at runtime — CLEARED (53d126b7)
- **Severity:** CONCERN (control param broken; valid write returns alarm)
- **Rust:** `ioc.rs:804-810` (`write_float64` does `datatype_of(reason)?` first, so the
  non-data POLL_DELAY reason → `Err`); poller uses a fixed `poll_delay` (`ioc.rs:1167-1177`)
- **C:** `drvModbusAsyn.cpp:1094-1099` (`writeFloat64` sets `pollDelay_=value` and signals
  the poller event)
- `poll_delay.template` binds an `ao` to POLL_DELAY; C retunes the poll period live,
  Rust fails the write (WRITE/INVALID alarm) and the period stays frozen.

### R47 — ENABLE_HISTOGRAM rising edge does not clear the histogram — CLEARED (c8345ecb)
- **Severity:** CONCERN (stale diagnostic counts carried across re-enable)
- **Rust:** `ioc.rs:819-822` (only sets `histogram_enabled`)
- **C:** `drvModbusAsyn.cpp:633-641` (on OFF→ON, zeros `timeHistogram_` before enabling)

### R48 — HISTOGRAM_BIN_TIME change does not clear the histogram (R47 family) — CLEARED (e6b106d7)
- **Severity:** CONCERN (stale counts misattributed to new bins)
- **Rust:** `ioc.rs:785-788` (sets `histogram_ms_per_bin` only)
- **C:** `drvModbusAsyn.cpp:794-803` (sets, clamps `<1→1`, then erases `timeHistogram_`)
- The axis rebuild is unneeded in Rust (axis computed on demand `ioc.rs:629-636`); the
  count erase is the missing part.

### R49 — READ_HISTOGRAM / HISTOGRAM_TIME_AXIS not served on Float64Array — CLEARED (0067a437)
- **Severity:** CONCERN (missing route; aai/waveform FTVL=DOUBLE binding errors)
- **Rust:** `ioc.rs:670-671` (`read_float64_array` does `datatype_of(reason)?` with no
  histogram case; only `read_int32_array` handles them at `ioc.rs:621-636`)
- **C:** `drvModbusAsyn.cpp:1181-1191` (`readFloat64Array` serves both, like `readInt32Array`
  at `:1350-1360`)
- Shipped `statistics.template` uses FTVL=LONG so the default path works; a Float64
  binding diverges.

### R50 — statistics counters never published in absolute-addressing mode (frozen at 0) — CLEARED (bb081061)
- **Severity:** CONCERN → wrong value (diagnostics read 0 vs live)
- **Rust:** `ioc.rs:394-411` (`publish_stats`, only writer) called only from `poll_cycle`
  (`ioc.rs:378`), which early-returns in absolute mode (`ioc.rs:343-345`); constructor
  seeds 0 (`ioc.rs:234-242`)
- **C:** `drvModbusAsyn.cpp:2205-2206/2213-2218/2254-2255/2300-2301/2340-2341` (`doModbusIO`
  itself `setIntegerParam`s the stats on every I/O)
- In absolute mode every per-record read runs `doModbusIO` (`ioc.rs:321-326`) and updates
  `engine.stats`, but they are never copied to the params → the `statistics.template`
  longins read 0 forever; C shows real counts.

### R51 — drvUser `=N` suffix validation dropped (C error routes missing) — CLEARED (903efea0)
- **Severity:** CONCERN (negative space; invalid drvInfo silently accepted)
- **Rust:** `ioc.rs` `drv_user_create` (splits on `=`, keeps prefix, drops the rest unvalidated)
- **C:** `drvModbusAsyn.cpp:387-413` (`=` valid only for the 8 string types → `asynError`
  for non-string; for string types `strtol` base-0 with `asynError` on garbage/negative)
- `INT16=5`, `STRING_HIGH=abc`, `STRING_HIGH=-3` all resolved in Rust where C rejects.
- **Fix:** `drv_user_create` now requires a suffix's resolved reason to be a string type and the
  suffix to parse as `strtol`-base-0 non-negative (`parse_drvuser_string_len`: 0x/0X hex, leading-0
  octal, else decimal; empty = 0 as C accepts), else `AsynError::Status{Error}` fails record init.
  The length *value* is still discarded — no asyn-rs home (R34). Tests
  `drv_user_create_validates_string_length_suffix`, `parse_drvuser_string_len_matches_strtol_base0`.

### R52 — drvUser bind does not reject an out-of-range offset (error deferred to first I/O) — CLEARED (e633e601)
- **Severity:** CONCERN (error at I/O time instead of init)
- **Rust:** `drv_user_create` runs no `checkOffset` (it has no `addr` — the asyn-rs
  `drv_user_create -> usize` contract resolves a shared reason, not a per-record bind); offsets
  were validated only per accessor.
- **C:** `drvModbusAsyn.cpp:378-384` (`drvUserCreate` `getAddr`+`checkOffset`) **and** `connect`
  (`:455-467`, the per-pasynUser offset gate) both reject an over-range offset with `asynError`.
- An over-range `addr` failed record init in C; in Rust it initialized and alarmed on every I/O.
- **Fix:** modbus is a multi-device port (`ASYN_MULTIDEVICE`), so the framework drives the per-`addr`
  connect through `connect_addr`. The driver overrides it to run `check_offset` and reject before
  marking the address connected — the faithful analogue of C's `connect` offset gate (the asyn-rs
  `drv_user_create` has no `addr`, so the connect path is where the per-record offset lives). Test
  `connect_addr_rejects_out_of_range_offset`.

### R53 — modbusInterposeConfig accepts timeoutMsec + writeDelayMsec but silently drops both — CLEARED (c17c1c2c)
- **Severity:** CONCERN (configured timeout + inter-frame write delay ignored)
- **Rust:** `modbus_interpose_config_command` read only `args[0]` port, `args[1]` link; `args[2]`
  timeout, `args[3]` writeDelay never read; transport used fixed `READ_TIMEOUT`, no write delay
- **C:** `modbusInterpose.c:122-135` (`timeout=timeoutMsec/1000`, `writeDelay=writeDelayMsec/1000`);
  write delay is a pre-write `epicsThreadSleep` at `:246`
- A user-set read timeout and the inter-frame write delay (needed by slow serial PLCs) were
  discarded; the arg slots were accepted so there was no error signalling the loss.
- **Fix:** new `InterposeSettings {link, timeout, write_delay}` carries all three per octet port
  (`record_interpose`/`take_interpose`). `parse_interpose_args` reads `timeoutMsec` (default
  `READ_TIMEOUT`=2 s when 0/unset, C `DEFAULT_TIMEOUT`) and `writeDelayMsec` (zero when 0/unset).
  `SyncIoTransport` sleeps `write_delay` before each `write_frame` (blocking sync-I/O thread =
  C `epicsThreadSleep`), and the `SyncIOHandle` is built with the configured timeout. Test
  `parse_interpose_args_reads_timeout_and_write_delay`.

### R54 — single Float64/Octet param per data reason collapses C's per-interface I/O-Intr callbacks (CONFIRMED DEFECT)
- **Severity:** DEFECT (wrong VAL on I/O-Intr integer-interface records) — structural, spans the
  `modbus-rs` ↔ `asyn-rs` boundary
- **Rust:** `ioc.rs:186-213` registers each data reason as ONE param — numeric→Float64, string→Octet.
  `poll_cycle` (`ioc.rs:371-374`) decodes the registers with the **float64** decode
  (`datatype::read_float`) and stores that single Float64, then `call_param_callbacks(addr)`
  (`ioc.rs:384-388`) fires one `InterruptValue{ value: Float64(v) }`.
- **C:** the read poller decodes the SAME register data **once per interface** and fires each
  interrupt user's interface-correct callback: UInt32Digital → masked `uInt32Value`
  (`drvModbusAsyn.cpp:1705-1706`), Int32 → `readPlcInt32`-decoded `int32Value` (`:1736-1743`),
  Int64 → `readPlcInt64`-decoded `int64Value` (`:1772-1779`), Float64 → `float64Value`
  (`:1814-1815`), Int32Array (`:1841-1850`), Float64Array (`:1884-1885`), Octet (`:1920-1921`).
  So an asynInt32 longin gets the exact int32 decode; an asynUInt32Digital bi/mbbi gets the
  masked value; an asynInt64 record gets the exact 64-bit value.
- **Verified routing (asyn-rs):** delivery is NOT the problem — `InterruptFilter::matches`
  (`asyn-rs/src/interrupt.rs:27-44`) keys on `(reason, addr, uint32_mask)` only, with NO interface
  field, so the Float64 `InterruptValue` DOES reach asynInt32/asynInt64/asynUInt32Digital
  subscribers. The "never updates" hypothesis is **refuted**. The defect is the VALUE, not delivery:
  the I/O-Intr read path (`asyn-rs/src/adapter.rs:2135-2138`) consumes the cached `InterruptValue`
  directly via `param_value_to_epics_value(Float64(v)) → EpicsValue::Double(v)`, and
  `store_read_value` (`adapter.rs:1405-1438`) only applies the interface-correct decode
  (`apply_raw_readback` mask/shift/state-table, the `ESLO` linear RVAL store) under
  `if let EpicsValue::Long(raw) = val`. A `Double` fails that gate and falls through to
  `set_val(Double)` (`adapter.rs:1440`).
- **Impact (I/O-Intr only; the polled path is correct — `read_int32`/`read_int64`/
  `read_uint32_digital` at `ioc.rs:505+` re-decode the raw registers per interface):**
  - asynUInt32Digital `bi`/`mbbi`/`mbbiDirect` — `apply_raw_readback` mask/shift is bypassed →
    wrong VAL for any masked/multi-bit field.
  - asynInt32 `ai` with `ESLO` linearization — the RVAL store + forward convert is bypassed →
    VAL holds the raw float, unconverted.
  - asynInt64 `int64in` — not handled in `store_read_value` at all → `set_val(Double)`; values
    `> 2^53` lose precision the polled `read_int64` keeps exact.
  - Plain `longin` (no ESLO/readback) and asynFloat64 `ai` are unaffected (the Double coerces to
    the right value).
- **Fix level:** structural, and larger than a modbus-rs patch — asyn-rs's interrupt layer is
  single-valued per `(reason, addr)` (one `ParamValue`, no per-interface routing), so faithfully
  mirroring C's per-interface callbacks needs either (a) an asyn-rs mechanism to fire multiple
  interface-typed callbacks for one reason, or (b) modbus-rs registering per-interface reasons.
  **Surface for design sign-off before fixing** (per the structural-fix-needs-sign-off rule); do
  not collapse it into an unrelated commit.

### R55 — readOctet always reports `ASYN_EOM_CNT`; the asyn-rs default only synthesises CNT when the buffer fills — CLEARED (8f15ba0d)
- **Severity:** CONCERN (EOMR/eom-flag divergence; record string content unaffected)
- **Rust:** `ioc.rs` `read_octet` returns only a count; the modbus driver does NOT override
  `io_read_octet_eom`, so it inherits the generic synthesis in `asyn-rs/src/port.rs:1184-1191`
  which sets `EomReason::CNT` only when `n >= cap` (buffer full) and `empty` otherwise.
- **C:** `drvModbusAsyn.cpp:1480` sets `*eomReason = ASYN_EOM_CNT` **unconditionally** on every
  successful P_Data octet read (and the poller callback at `:1921` passes `ASYN_EOM_CNT` too).
  Modbus reads are register-snapshot reads (always a complete logical message), so C always
  flags the read complete.
- **Impact:** a modbus string shorter than the record buffer (the common case) → C reports
  `ASYN_EOM_CNT`, Rust reports no EOM flag. Observable on `asynRecord.EOMR` and anything keying on
  the EOM reason. Record VAL/string content is identical. Found while grounding R31.
- **Fix:** `ModbusPortDriver::io_read_octet_eom` override returns `EomReason::CNT` for every
  successful P_Data octet read, mirroring C. Distinct from R31's count semantics. Test
  `read_octet_eom_always_flags_cnt_for_short_string` pins the not-full-buffer case (8f15ba0d).

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
6. **R54 single-param vs per-interface callbacks** — VERIFIED CONFIRMED DEFECT (was "verify-first
   routing risk"). Routing is fine (`InterruptFilter` keys on reason+addr only, no interface), so
   the "never updates" hypothesis is refuted; the defect is the VALUE — modbus-rs fires one Float64
   callback where C fires per-interface-decoded callbacks, and asyn-rs's `store_read_value` only
   applies the interface-correct decode for a `Long`, so I/O-Intr asynUInt32Digital/asynInt32-ESLO/
   asynInt64 records get the wrong VAL. Structural, spans the modbus-rs↔asyn-rs boundary →
   surfaced for design sign-off (asyn-rs interrupt layer is single-valued per reason).

Wire-byte path verified clean end-to-end (PDU, MBAP, CRC/LRC, ASCII, function codes, conversions
in-range). All defects are in statistics/diagnostics/config/lifecycle, not the data path. No DEFECT
in transmitted bytes.

Fix phase: per-finding commits, `cleared` marked here as each lands; convergence rounds after each
cluster.
