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

### R1 — read loop aborts on a too-short reply where C re-reads — CLEARED (6ed4c056)
- **Severity:** CONCERN (no wrong wire bytes; noisy/fragmented links only)
- **Rust:** `transact` (parse `interpose.rs:188-193`)
- **C:** `modbusInterpose.c:366-369`
- C's TCP `readIt` loop falls through a successful read with `nbytesActual < 2`
  and reads again; Rust called `unwrap_response` on every `Ok(raw)` and propagated
  `FrameTooShort` via `?`, so a single spurious short read ended the transaction as
  an I/O error. The txid-mismatch half of the loop *was* reproduced (`stale_frames`).
- **Fix:** the `Ok(raw)` arm now catches `FrameTooShort` and, for the MBAP-framed links
  (`Tcp`/`Udp`), re-reads (bounded by the same `MAX_STALE_FRAMES` guard) instead of aborting.
  RTU/ASCII read once in C, so their short/CRC failures still propagate. Tests
  `do_modbus_io_skips_too_short_tcp_frame`, `do_modbus_io_rtu_short_frame_errors_without_reread`.

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

### R52 — drvUser bind does not reject an out-of-range offset at connect — STRUCTURAL BLOCK (asyn-rs contract; sign-off, sibling of R34/R54)
- **Severity:** CONCERN (error at I/O time instead of connect time)
- **Rust:** `drv_user_create` runs no `checkOffset` (it has no `addr` — the asyn-rs
  `drv_user_create -> usize` contract resolves a shared reason, not a per-record bind); offsets are
  validated only per accessor.
- **C:** `drvModbusAsyn.cpp:378-384` (`drvUserCreate` `getAddr`+`checkOffset`) **and** `connect`
  (`:455-470`, the per-pasynUser offset gate) reject an over-range offset with `asynError`; an
  over-range `addr` fails record init in C.
- **e633e601 attempt — does NOT clear the finding (round-2 convergence, D-records + A-protocol
  panels, ground-verified):** the commit added a `connect_addr` override that runs `check_offset`
  before marking the address connected. The override's *internal* logic is correct, but it is
  **dormant** — nothing in the framework ever drives `connect_addr` for a normally-bound modbus
  record:
  - The only `RequestOp::ConnectAddr` emitter is `asyn-rs/port_handle.rs:693
    connect_addr_blocking`, which has **zero callers**; no record/adapter init seeds `device_states`
    or issues a per-addr connect.
  - Auto-connect (`port_actor.rs:305`) calls `connect_addr` only when the addr is already in
    `device_states` **and** disconnected; a missing addr is treated as connected (`map_or(true)`),
    so the block is skipped. `check_ready_addr` (`port.rs:429`) likewise only gates an existing,
    disconnected addr.
  - So a bad-offset record: addr absent → auto-connect skipped → I/O dispatches → the per-accessor
    `engine.check_offset` (14 sites: `ioc.rs:652,672,689,714,728,803,851,890,931,971,979,999,1024,
    1099`) returns `Err` → the record alarms on **every** I/O. That protective outcome already
    existed before e633e601; the override changed no observable behavior, and C's connect-time
    rejection is not reproduced.
  - The unit test `connect_addr_rejects_out_of_range_offset` calls `connect_addr` **directly**,
    bypassing the framework dispatch that never calls it — green test, no end-to-end coverage.
  - **Secondary divergence (A-protocol 3b):** the override rejects `offset < 0` (`check_offset`
    semantics) but C's **`connect`** allows `offset == -1` (`drvModbusAsyn.cpp:462`,
    `if (offset < -1) return asynError`, the connect-all port bind). The override therefore matches
    C's per-I/O `checkOffset`, not the C `connect` it cites. Latent today (asyn-rs routes a `-1`
    port connect through `RequestOp::Connect → driver.connect`, which modbus does not override, so
    `-1` never reaches `connect_addr`).
- **Why structural:** like R34/R54, the real gap is that asyn-rs has **no record-init per-addr
  connect path** (a `connectDevice` analogue that seeds `device_states` and routes through
  `connect_addr`). Without that, the connect-time offset rejection cannot fire. This is the *same*
  asyn-rs contract gap as R34/R54 — A-protocol's recommendation is to sign all three off as one
  framework work item. The dormant override + its `-1` bound are held pending that decision (do not
  patch the dormant path or revert e633e601 unilaterally).

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
- **Round-2 follow-up (A-protocol 3a, FIXED a649050d):** C applies `writeDelay` only in `writeIt`
  (`:246`); the UDP read-failure retransmit resends via the raw `pasynOctet->write` (`:358`),
  bypassing the delay. The first c17c1c2c implementation slept on *every* `write_frame`, so
  `transact`'s retransmit also slept. Fixed by routing the retransmit through a new no-delay
  `OctetTransport::resend_frame` (overridden in `SyncIoTransport` to skip the sleep); the initial
  send still paces. Tests `udp_retransmit_resends_via_no_delay_path` +
  `udp_retransmits_at_most_four_times_then_gives_up` (initial/retransmit split).

### R54 — single Float64/Octet param per data reason collapses C's per-interface I/O-Intr callbacks — CLEARED (a3e2fee6)
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
  - asynInt64 `int64in` — bridged through `EpicsValue::Double(v as f64)`, so values `> 2^53` lose
    precision. (Correction: this is NOT an I/O-Intr-vs-polled divergence — the polled path narrows
    identically at `result_to_value` (`adapter.rs:1226`, `asynInt64 => EpicsValue::Double(v as
    f64)`), and the post-fix typed `Int64` fire is collapsed the same way at
    `param_value_to_epics_value` (`adapter.rs:919`). The int64 `>2^53` narrowing is a uniform,
    pre-existing asyn-rs consumer-bridge limitation affecting every driver, tracked separately from
    R54's interface-routing scope.)
  - Plain `longin` (no ESLO/readback) and asynFloat64 `ai` are unaffected (the Double coerces to
    the right value).
- **Fix level:** structural, and larger than a modbus-rs patch — asyn-rs's interrupt layer is
  single-valued per `(reason, addr)` (one `ParamValue`, no per-interface routing), so faithfully
  mirroring C's per-interface callbacks needs either (a) an asyn-rs mechanism to fire multiple
  interface-typed callbacks for one reason, or (b) modbus-rs registering per-interface reasons.
  **Surface for design sign-off before fixing** (per the structural-fix-needs-sign-off rule); do
  not collapse it into an unrelated commit.
- **Fix (a3e2fee6, signed off — option (a)):** restored the interface dimension C keeps via
  per-interface interrupt lists. `InterruptValue`/`InterruptFilter` gained
  `iface: Option<InterfaceType>`; `matches` rejects only when both the value and the subscriber
  name an interface and they differ, so an untyped value (the `call_param_callbacks` path) still
  reaches every subscriber and single-value drivers are unaffected. New producer
  `PortDriverBase::notify_interface_value` fires one interface-typed value; the `AsynDeviceSupport`
  subscriptions tag their filter with the record's own interface. `poll_cycle` now decodes each
  active register block once per interface and fires int32/int64/float64/raw-uInt32Digital-word
  separately (the whole block decoded up front so a mid-decode error aborts before any partial
  fire). Tests: `interrupt::tests::notify_routes_typed_values_per_interface` and
  `ioc::tests::poll_cycle_fires_per_interface_typed_values`.

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

### R56 — I/O-Intr array waveforms (asynInt32Array/asynFloat64Array) never fired by the poller — CLEARED (514807a1 + 9c136c32)
- **Severity:** DEFECT (an I/O-Intr array waveform never updates) — pre-existing, surfaced by the
  R54 fix review. NOT introduced by R54.
- **Rust:** `poll_cycle` (`ioc.rs:409`) fires only the scalar interfaces
  (int32/int64/float64/uInt32Digital) + octet per active reason — it never fires `Int32Array` or
  `Float64Array`. An `intarray_in`/`floatarray_in` waveform (`db/intarray_in.template`
  asynInt32ArrayIn, `db/floatarray_in.template` asynFloat64ArrayIn) with `SCAN="I/O Intr"` registers
  a mailbox subscriber on iface `Int32Array`/`Float64Array` (`adapter.rs:2533/2543`,
  `from_asyn_name`), which after R54 correctly REJECTS the scalar fires → the record receives
  nothing and never updates.
- **C:** `readPoller` fires the int32Array (`drvModbusAsyn.cpp:1841-1858`, on change) and
  float64Array (`:1860-1894`, unconditional) interrupt lists, decoding the whole register block
  from the record's offset (`readPlcInt32`/`readPlcFloat` per element).
- **Pre-existing:** before R54 the array record's filter had no iface and received the untyped
  scalar `Float64` — also wrong (a scalar delivered to an array record). R54 made the failure clean
  (rejected) but did not add the array fan-out. Periodic-`SCAN` array reads via `read_int32_array`/
  `read_float64_array` (`ioc.rs:838/889`) are CORRECT and unaffected — only `SCAN="I/O Intr"`
  arrays are dead.
- **Fix part 1 (514807a1):** `poll_cycle` decodes the whole block per array interface and fires
  `ParamValue::Int32Array`/`Float64Array` via `notify_interface_value`, reusing the relative-mode
  decode shape of `read_int32_array` (`decode_block_int32`/`decode_block_float64`). The consumer path
  (mailbox → `convert_param_array_to_iface` → `store_read_value`, `adapter.rs:2135`) was already built
  for NDPluginStdArrays — purely the modbus producer side, no consumer change. A subscriber-presence
  gate (`InterruptManager::has_subscriber`) skips the whole-block decode when no array record is bound.
- **Fix part 2 — the REAL blocker (9c136c32):** part 1 was INERT on its own. `poll_cycle` iterated
  `self.active`, a HashSet seeded ONLY by `touch`-on-read in the five scalar read accessors. A
  `SCAN="I/O Intr"` record never reads on its own (inputs do not auto-readback, `adapter.rs:2904`;
  templates set no PINI; `setup_io_intr` registers a mailbox but issues no read), so its
  `(reason, addr)` never entered `active` and the poller fired NOTHING for it — the array fire (part 1)
  and EVERY scalar I/O-Intr fire alike. This was broader than the array fan-out: scalar I/O-Intr
  modbus records were dead too. Structural fix — drive the fire set from the interrupt subscriber
  registry (the single owner of "which records want a fire"), exactly C `readPoller` firing every
  registered interrupt user. New `InterruptManager::subscribed_bindings()` returns the distinct
  concrete bindings; `poll_cycle` iterates it; the `active` field + `touch` helper + its five call
  sites are removed. Tests: `subscribed_bindings_enumerates_distinct_concrete_pairs` (asyn-rs);
  `poll_cycle_fires_array_interfaces_only_when_a_record_is_bound` now fires with NO prior read (the
  real I/O-Intr path); `poll_cycle_fires_per_interface_typed_values` binds a mailbox subscriber;
  `poll_cycle_skips_out_of_range_subscriber_binding_without_panic`. Confirmed by both R56-review opus
  panels (consumer: active-gate false-negative; parity: standalone I/O-Intr array waveform never fires).
- **Fix part 3 — sync-callback bindings (3ac7db5b):** the fix-verification round caught that
  `subscribed_bindings()` was mailbox-ONLY. Averaging (`asynInt32Average`/`asynFloat64Average`) and
  time-series device support register via `register_sync_callback` (the C `registerInterruptUser`
  analogue) with NO mailbox — the average branch returns before `setup_io_intr`'s mailbox block — so
  their `(reason, addr)` was missed and modbus `poll_cycle` never fired them (empirical: 0 samples).
  C `readPoller` fires averaging via its registered interrupt user (`devAsynInt32.c:870-872`,
  `readPoller:1714/1750/1786`). `subscribed_bindings()` now enumerates BOTH mailbox subscriptions and
  sync callbacks; only the broadcast `subscribe_async` observer stays excluded. `has_subscriber`
  stays mailbox-only by design (it gates the array decode; averaging/time-series are scalar-only).
  CONVERGED — a fix-verification round (2 fresh opus panels) confirmed: the registry has exactly
  three registration entry points (`register_interrupt_user`/mailbox, `register_sync_callback`/sync,
  `subscribe_async`/broadcast), the two private subscriber structs forbid a fourth kind by
  construction, the snapshot-then-dedup is deadlock-free against `notify` (neither nests the two
  locks), and the empirical Scenario C now fires (Ci `0→1`, Cf `1`, array path no regression). No new
  findings; the invariant "`subscribed_bindings()` contains every concrete `(reason,addr)` any
  record-binding interrupt user wants polled" holds by construction.
- **Benign divergence (kept, NOT a C bug to copy):** the block decode tail uses `floor`
  (`while (n+1)*rc <= len`), dropping a trailing partial element; C uses `ceil` and may OOB-read a
  partial register past `modbusLength_`. Rust's floor is strictly safer (no OOB) and differs only on
  a non-`register_count`-aligned block (a malformed config). Per "do not copy C's bugs", the floor
  stays.
- **Residual (folded into R57, now CLEARED d78b01e0):** `int32Array` was fired UNCONDITIONALLY where C
  gates it on `forceCallback_ || anyChanged` (`:1824`); `float64Array` matches C (unconditional,
  `:1857`). The int32Array on-change cadence was the array sibling of R57 and closed with the same
  shared `prev_data`/`any_changed` primitive — see R57.

### R57 — on-change I/O-Intr fires (uInt32Digital + octet + int32Array) sent unconditionally vs C's change gate — CLEARED (d78b01e0)
- **Severity:** CONCERN (extra record processing; no wrong value, identical wire-visible monitor
  output).
- **Rust:** `poll_cycle` (`ioc.rs:489-501` uInt32Digital; `ioc.rs:441-447` octet) fires both
  interfaces UNCONDITIONALLY each poll (`uint32_changed_mask = !0` for uInt32Digital) and relies on
  the record's monitor deadband to suppress unchanged posts.
- **C:** `readPoller:1700` fires uInt32Digital only on `forceCallback_ || (masked newValue != masked
  prevValue)` (per-offset change check); `readPoller:1893` gates the asynOctet callback on
  `forceCallback_ || anyChanged` (port-wide change flag). (int32/int64/float64 are fired
  unconditionally by BOTH C and Rust — C comment at `:1858`: "called even if the data has not
  changed, because we could be doing ADC averaging"; only uInt32Digital, octet — and int32Array,
  see R56 — are on-change in C. float64Array `:1857-1889` is unconditional in C too.)
- **Octet sibling (surfaced in the R54-fix review, parity panel):** the string branch of `poll_cycle`
  fires `InterfaceType::Octet` every poll (`ioc.rs:441-447`); the existing comment there cites the C
  octet list (`:1894-1921`) but not its `forceCallback_ || anyChanged` gate at `:1893`. Same
  on-change-gating gap as the uInt32Digital case, same root cause: `poll_cycle` lacks a
  prev-data/`anyChanged` primitive.
- **Impact:** a uInt32Digital (bi/mbbi/mbbiDirect) or asynOctet (stringin/waveform) I/O-Intr record
  processes every poll in Rust vs only-on-change in C. CA monitor posts are identical (the record's
  deadband suppresses unchanged VAL); the divergence is extra record processing each poll (forward
  links / .PROC side effects fire every poll). Never a wrong value.
- **Fix (shared primitive, d78b01e0):** `poll_cycle` now holds a `prev_data` register-block snapshot
  and a one-shot `force_callback` flag (set for the first cycle per C `:331` and after an I/O error
  per C `:1654`; cleared each cycle end per C `:1928`). `any_changed` is the port-wide block compare
  (`:1658`); `port_gate = force || any_changed` gates octet (`:1893`) and int32Array (`:1824`).
  int32/int64/float64 + float64Array stay unconditional (C ADC-averaging, `:1714/1858`). uInt32Digital
  closes STRUCTURALLY, not with a coarse gate: the poller passes the actually-changed bits
  `word ^ prev_word` as `uint32_changed_mask` instead of `!0`, so the interrupt filter's existing mask
  gate (`uint32_changed_mask & @asynMask != 0`, asynPortDriver.cpp:720) reproduces C's exact
  per-subscriber `(new ^ prev) & mask` test (`:1700`); `force` still passes `!0`. Boundary tests:
  `poll_cycle_uint32_digital_fires_only_on_per_offset_masked_change`,
  `poll_cycle_int32_array_gated_on_change_scalars_unconditional`, `poll_cycle_octet_fires_only_on_change`,
  `poll_cycle_io_error_forces_next_unchanged_cycle`.

### R58 — R57's force re-arm covered only the engine-poll error, not a mid-loop decode abort — CLEARED (af8991bd)
- **Severity:** CONCERN (recoverable stale fire on a misconfigured/late-subscribing record; no
  wrong value). Introduced by R57's gating (d78b01e0); caught in the R57 review round
  (`01KWB56E`, consumer panel).
- **Rust (pre-fix):** `poll_cycle` cleared `force_callback` + advanced `prev_data` only at cycle
  end, and only the **engine-poll** error path re-armed `force_callback=true` on abort. A mid-loop
  per-offset decode `?` (e.g. an `INT32_LE` subscriber bound at the last register, where
  `regs[addr..]` is shorter than `need_regs`, `datatype.rs:357`) returned `Err` WITHOUT re-arming
  the force or advancing the baseline.
- **Impact:** if such an abort lands **after** a clean cycle (a late-subscribing or misconfigured
  record), `prev_data` freezes at the last-good snapshot and `force_callback` stays `false`, so the
  on-change-gated interfaces (octet / int32Array / uInt32Digital) at **other** offsets can miss a
  change or revert until a fully-clean cycle runs. The common case (error before any clean cycle)
  leaves the force stuck `true` — a safe over-fire. Never a wrong value; self-heals on the next
  clean cycle. The unconditional scalars (int32/int64/float64/float64Array) are unaffected.
- **Fix (structural single finalizer, af8991bd):** the fallible body is split into `run_poll_cycle`,
  which advances `prev_data` + clears `force_callback` only as its **last** statement (reached only
  on full success); `poll_cycle`'s `match` wrapper re-arms `force_callback` on **any** `Err` — engine
  poll, per-offset decode, or stats publish — through one owner, so no `?` can leave the baseline
  frozen with the force cleared. Mirrors C, which updates `prevData` + clears `forceCallback_` at
  cycle end on every completed cycle (`drvModbusAsyn.cpp:1928/1934`) and re-arms on an I/O-status
  transition (`:1654`). Boundary test
  `poll_cycle_mid_loop_decode_error_rearms_force_for_recovery`.

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
- **uInt32Digital `@asynMask(port,addr,0)` (zero mask) never fires** — the R57 on-change
  filter (`interrupt.rs matches`: `uint32_changed_mask & M != 0`) gates a `mask==0`
  subscriber out forever (`x & 0 == 0`). C `readPoller:1695-1707` treats `mask==0` as
  "no mask" and compares the **full word**, so it *fires* a zero-mask subscriber on any
  change. Rust deliberately follows the **asyn framework** convention instead: the generic
  `doCallbacksUInt32Digital` (asynPortDriver.cpp:720, `pInterrupt->mask & interruptMask`)
  also never fires `mask==0`. C's modbus `readPoller` full-word path is the idiosyncratic
  one, inconsistent with C's own framework. A zero-mask digital record is degenerate (it
  selects no bits — value is always 0) and `@asynMask` requires an explicit mask argument
  (the default is `0xFFFFFFFF`), so the case only arises from a meaningless explicit
  `@asynMask(port,addr,0)`. Matching the framework, not the readPoller quirk, is the
  correct decline. (R57 review, parity panel, round `01KWB56E`.)

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

### Round 2 — 2026-06-28 (convergence, 4 opus reviewers, adversarial cross-cut)

Re-verified the landed fixes against the cited C. Per-finding panels (B-driver, C-datatype,
D-records) plus one adversarial cross-cut (A-protocol) tasked to *break* the batch.

- **CONFIRMED converged (no change):** R1 (short-frame skip — 3 checks confirm), R31 (octet
  `strlen` vs C `strlen+1`), R32 (float→int saturation vs C UB cast), R51 (drvUser `=N` strtol
  base-0 accept/reject set, verified input-by-input), R53 (interpose timeout/writeDelay — defaults,
  dual-path timeout, dedicated-thread blocking-sleep safety), R55 (always-CNT EOM).
- **R34 / R54 STRUCTURAL BLOCKS — CONFIRMED sound.** A-protocol tried every named avenue
  (`AsynUser.user_data`, `reason_to_datatype`, synthetic per-`(type,len)` reason) and found no
  modbus-rs-local fix; both are the *same* asyn-rs gap viewed twice (per-record / per-interface
  driver data the `drv_user_create -> usize` + single-valued interrupt contract cannot carry). One
  asyn-rs contract change closes both → sign off as one work item.
- **R52 RECLASSIFIED** from CLEARED to STRUCTURAL BLOCK (sibling of R34/R54). Ground-verified: the
  `connect_addr` override is dormant (no framework caller seeds `device_states` / drives per-addr
  connect); the protective outcome already exists via 14 per-I/O `check_offset` sites; the override
  also uses the `checkOffset` bound where C `connect` allows `offset == -1` (3b). See the R52 entry.
- **NEW finding 3a (R53 retransmit delay) — FIXED a649050d.** A-protocol caught that the c17c1c2c
  `write_delay` slept on the UDP retransmit too, where C bypasses it (raw write at
  `modbusInterpose.c:358`). Fixed via a no-delay `OctetTransport::resend_frame`. See the R53 entry.
- **`clear_histogram` ioc gate (0e0c1875) — CONFIRMED correct AND complete.** It is the *only*
  `pub(crate)` fn in the four non-ioc files; no sibling helper is missing the cfg gate.

Net: data path remains wire-clean. Open after Round 2: R34, R52, R54 — one asyn-rs framework
work item (record-init per-addr connect + per-record/per-interface driver data), awaiting design
sign-off. No further modbus-rs-local fixes outstanding.

### Round 3 — 2026-06-30 (R54 fix landed + 2 opus reviewers, F1 convergence)

R54 fixed as **F1** (`a3e2fee6`, branch `f1-asyn-per-interface-iointr`): per-interface I/O-Intr
routing via an `iface: Option<InterfaceType>` tag on `InterruptValue`/`InterruptFilter`; modbus
`poll_cycle` decodes the register block once and fires Int32/Int64/Float64/UInt32Digital separately
through the new `PortDriverBase::notify_interface_value`. Two opus review panels (consumer-bridge +
C-parity) converged:

- **F1 — CONFIRMED CORRECT (both panels).** `matches()` rejects only when both sides are `Some` and
  differ (untyped fires still reach everyone; typed fires reach only their interface); the consumer
  bridge (Int32→Long, Int64→Double, UInt32Digital→Long) lands each value in the right
  `store_read_value` branch (apply_raw_readback mask/shift, asynInt32 ESLO); int64 is consistently
  `Double` on both the polled and I/O-Intr paths. **No regression:** no record/interface combination
  loses a value the old coalesced path delivered.
- **R56 (I/O-Intr array fan-out) — CONFIRMED defect, kept OPEN for its own sign-off (both panels).**
  Same C `readPoller` fan-out family as R54 but a genuinely distinct, larger work item (whole-block
  array decode, distinct on-change gating at `:1824`, subscriber-presence gate, an unexercised
  end-to-end array consumer path). NOT to be folded into R54.
- **R57 (on-change gating) — CONFIRMED CONCERN, broadened.** Real divergence, wire-observable only
  via forward-link/.PROC processing every poll; output-equivalent on the direct value monitor, never
  a wrong value. Parity panel surfaced the **asynOctet sibling** (`poll_cycle` fires octet
  unconditionally vs C `:1893` `forceCallback_ || anyChanged`) — appended to R57; both close with one
  shared `prevData`/`anyChanged` primitive in `poll_cycle` (also gates int32Array, the on-change half
  of R56).

Open after Round 3: R34, R52 (asyn-rs contract, sign-off); R56 (array fan-out, sign-off); R57
(on-change gating incl. octet, CONCERN). R54 CLEARED.

**Post-round:** R56 signed off and CLEARED in two parts. `514807a1` added the int32Array/float64Array
fire mechanism (with a subscriber-presence gate), but a second R56-review round (2 fresh opus panels,
2026-06-30) found it INERT: `poll_cycle` gated the fire on the read-seeded `active` set, which a
`SCAN="I/O Intr"` record never enters (it never reads on its own) — so the array fire, AND every
scalar I/O-Intr fire, were dead. Structural fix `9c136c32` (user-signed-off): drive the fire set from
the interrupt subscriber registry (`InterruptManager::subscribed_bindings()`), exactly C `readPoller`;
the `active`/`touch` primitive is removed. This also closed a broader latent bug — scalar I/O-Intr
modbus records were dead too. A THIRD R56-review round then found `subscribed_bindings()` was
mailbox-ONLY, missing averaging/time-series records (sync-callback bindings, no mailbox) that C
`readPoller` fires — closed by `3ac7db5b` (enumerate both mailbox and sync-callback bindings). A
fourth (fix-verification) round CONVERGED R56: both opus panels CONFIRMED-CORRECT with no new
findings (three registration entry points only, fourth kind impossible by construction, deadlock-free,
empirical Scenario C fires). int32Array on-change cadence folded into R57. R56 CLOSED.

**R57 CLEARED (d78b01e0).** A shared `prev_data`/`force_callback` primitive in `poll_cycle` now gates
the on-change interfaces exactly as C `readPoller`: octet (`:1893`) and int32Array (`:1824`) port-wide
on `force || any_changed`; uInt32Digital structurally via `word ^ prev_word` as the changed mask (the
interrupt filter's mask gate reproduces C's per-subscriber `(new ^ prev) & mask`, `:1700`);
int32/int64/float64 + float64Array stay unconditional (ADC averaging, `:1714/1858`). `force` covers the
first cycle (`:331`) and post-I/O-error recovery (`:1654`), cleared each cycle end (`:1928`). Four
boundary tests. Open: R34, R52 (asyn-rs contract).

**R57-fix review round (`01KWB56E`, 2 opus panels, 2026-06-30).** Both panels CONFIRMED the d78b01e0
gating C-faithful (parity: `any_changed` == C `anyChanged` memcmp `:1658`; `force_callback`
init/error/clear lifecycle; gate split octet+int32Array on-change vs scalars+float64Array
unconditional; uInt32Digital `(new^prev)&mask` exact for every sensible mask. consumer: `prev_data`
read-during/rewrite-after-loop, no panic, no double-fire, no averaging/TS starvation). Two NEW
low-severity findings, both dispositioned:
- **mask==0 (parity).** C `readPoller:1695` fires a zero-mask subscriber on full-word change; the Rust
  filter never fires it. DECLINED as an intentional divergence — Rust follows the asyn framework
  convention (`doCallbacksUInt32Digital`, asynPortDriver.cpp:720, also never fires `mask==0`); C's
  readPoller full-word path is the idiosyncrasy, inconsistent with C's own framework, and a zero-mask
  digital record is degenerate. See Intentional-divergence asides.
- **mid-loop decode abort (consumer) → R58.** A per-offset decode `?` aborted the cycle without
  re-arming `force_callback` (only the engine-poll error did), so an abort after a clean cycle could
  freeze the on-change baseline. FIXED structurally `af8991bd` (single finalizer: every Err re-arms
  through one owner). See R58.
