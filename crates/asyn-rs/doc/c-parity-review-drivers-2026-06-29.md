# asyn-rs Hardware-Driver Parity Review — epics-modules/asyn C↔Rust

분석일: 2026-06-29
대상: `asyn-rs` hardware transport drivers (`crates/asyn-rs/src/drivers/`)
upstream: `epics-modules/asyn` (drvAsynSerial / drvPrologixGPIB)
방법론: Codex-style C-first parity audit (read-only; fixes are a separate phase)

## Scope

The 2026-06-12 core-path review (`parity-review.md`) explicitly excluded the
hardware drivers. This audit covers them. Of the 7 driver modules, **3 are
intentional scaffolds** whose `connect()` returns "hardware path not yet
implemented" behind unwired feature flags — they have no behavior to compare
and are EXCLUDED (auditing a stub yields only "everything missing", not a
parity divergence):

- `ftdi.rs` (feature `ftdi-mpsse`, unwired)
- `usbtmc.rs` (feature `usbtmc`, unwired)
- `vxi11.rs` (feature `vxi11`, unwired)

The **4 implemented drivers** under audit, one category each:

| Cat | Rust | C upstream | C lines | Finding range |
|---|---|---|---|---|
| A | `drivers/ip_port.rs` | `drvAsynSerial/drvAsynIPPort.c` | 1115 | DRV-1..DRV-15 |
| B | `drivers/ip_server_port.rs` | `drvAsynSerial/drvAsynIPServerPort.c` | 759 | DRV-16..DRV-30 |
| C | `drivers/serial_port.rs` | `drvAsynSerial/drvAsynSerialPort.c` | 1175 | DRV-31..DRV-45 |
| D | `drivers/prologix.rs` | `drvPrologixGPIB/drvPrologixGPIB.c` | 628 | DRV-46..DRV-60 |

## Audit dimensions (Codex principles)

1. C call graph, not isolated bodies (who-calls-what, option dispatch tables).
2. Negative space (silent failures, missing error→asynStatus routing, missing
   option key, missing state transition, missing disconnect/cleanup).
3. Wire/transfer parity (exact option keys + defaults, EOS/break handling,
   nbytesTransfered on success AND error, eomReason bits, flush semantics).
4. Test skepticism (open every cited "matches C XXX:YY" line; don't trust
   comments or green tests).
5. Don't re-report findings already in this doc.

## Open Findings

Round 1 (4 opus panels, 2026-06-29). 48 real findings + 2 NON-GAPs. Full
impact paragraphs in the round report
`.caucus/.../rounds/01KW85XB3NBBQH96V4EAVZYBKY.md`; condensed below.

### Category A — `ip_port.rs` ↔ `drvAsynIPPort.c`

| id | sev | Rust | C | one-line |
|---|---|---|---|---|
| DRV-1 | CLEARED | ip_port.rs:596-638 | drvAsynIPPort.c:513-523,656,775-789 | UDP socket is `connect()`-ed → inbound datagrams filtered to one peer; `udp*` broadcast replies dropped; SO_BROADCAST set after connect — FIXED (F3, see Fix Log) |
| DRV-2 | CLEARED | iocsh.rs:441-505, ip_port.rs:471-493 | drvAsynIPPort.c:1065-1066 | EOS interpose never auto-installed; C installs `asynInterposeEos` by default → IEOS/OEOS dead on a fresh port — FIXED (F7, see Fix Log) |
| DRV-3 | CLEARED | ip_port.rs:266-269 | drvAsynIPPort.c:814-831 | TCP EOF returns Disconnected error; C returns success+`ASYN_EOM_END`; END bit never emitted anywhere — FIXED (F2, see Fix Log) |
| DRV-4 | CLEARED | ip_port.rs:296-299,782-804 | drvAsynIPPort.c:815 | empty UDP datagram treated as EOF → tears down socket (legit zero-len datagram kills the port) — FIXED (F2, see Fix Log) |
| DRV-5 | CLEARED | ip_port.rs:810-828,357-379 | drvAsynIPPort.c:692-699 | write errors never close connection (asymmetric w/ read path); port wedges, no reconnect — FIXED (F1, see Fix Log) |
| DRV-6 | CONCERN | ip_port.rs:121-138,32-51 | drvAsynIPPort.c:364-367,1061-1064 | `COM` protocol suffix unsupported → silently downgraded to plain TCP, RFC 2217 interpose omitted |
| DRV-7 | CLEARED | ip_port.rs:767-771,711-718 | drvAsynIPPort.c:214-217,557-558,815-819 | HTTP per-transaction disconnects after each read → truncates multi-segment responses + flaps Connect exception — FIXED 1629e3bb |
| DRV-8 | DOC | ip_port.rs:861-866,665-667 | drvAsynIPPort.c:525-534,915-947 | `noDelay` is a non-C option key (C has none, sets TCP_NODELAY unconditionally) — KEPT, intentional extension (see Fix Log) |
| DRV-9 | CLEARED | ip_port.rs:916-945 | drvAsynIPPort.c:902-906,941-945 | setOption/getOption accept+echo arbitrary unknown keys; C closes dispatch (asynError) — FIXED (F4, see Fix Log) |
| DRV-10 | CLEARED | ip_port.rs:867-869 | drvAsynIPPort.c:924-935 | `disconnectOnReadTimeout` parse accepts extra values, never errors (C: only Y/N) — FIXED (see Fix Log) |
| DRV-11 | CLEARED | ip_port.rs:264,792-794 | drvAsynIPPort.c:775-777,798 | `timeout==0` read → Duration::ZERO rejected by std → misclassified, disconnects (C floors to 1ms poll); missing `timeout>0` disconnect guard — FIXED a092a777; convergence bff0263d (read EINTR non-fatal + zero-timeout write deadline floor) |
| DRV-12 | CLEARED | ip_port.rs:661-731 | drvAsynIPPort.c:424-427 | `connect()` doesn't reject already-open link (C: "Link already open!") — FIXED (F6, see Fix Log) |
| DRV-13 | DOC | ip_port.rs:92,109,553,575 | drvAsynIPPort.c:513-523 | hardcoded 5s connect timeout where C connect is OS-default blocking — intentional divergence, documented b2938e11 |
| DRV-14 | CLEARED | ip_port.rs:368-371 | drvAsynIPPort.c:613-614 | zero-length write emits an empty UDP datagram; C returns before sending — FIXED (see Fix Log) |
| DRV-15 | CLEARED | ip_port.rs:810-828, port.rs:979 | drvAsynIPPort.c:678-705 | partial-write byte count dropped on error (framework `write_octet -> ()` contract) — FIXED (F8, see Fix Log) |

### Category B — `ip_server_port.rs` ↔ `drvAsynIPServerPort.c`

| id | sev | Rust | C | one-line |
|---|---|---|---|---|
| DRV-16 | CONCERN | ip_server_port.rs:529-535 | drvAsynIPServerPort.c:373-383 | new-TCP-connection octet-interrupt callback never delivered (the driver's documented purpose) |
| DRV-17 | CONCERN | ip_server_port.rs:887-893,683-689 | drvAsynIPServerPort.c:311-320 | UDP datagram octet-interrupt push delivery missing → I/O Intr record never fires |
| DRV-18 | CLEARED | ip_server_port.rs:408-415 | drvAsynIPServerPort.c:426-429 | UDP datagram-fanout SO_REUSEPORT not set; comment wrongly asserts parity — FIXED 4dc28663 |
| DRV-19 | CLEARED | ip_server_port.rs:451-457,391-397 | drvAsynIPServerPort.c:403-419 | host/iface not resolved — bare `SocketAddr::parse` rejects `localhost`/hostnames/empty-host (client driver resolves correctly) — FIXED e502ccfb |
| DRV-20 | CLEARED | ip_server_port.rs (no io_flush) | drvAsynIPServerPort.c:240-247,655 | UDP `flush` doesn't discard cached datagram → stale datagram on flush-then-read — FIXED d890bafa |
| DRV-21 | DOC | ip_server_port.rs:486 | drvAsynIPServerPort.c:447 | listen backlog hardcoded 128 not `maxClients` — intentional divergence, documented in source |
| DRV-22 | CLEARED | ip_server_port.rs:328 | drvAsynIPServerPort.c:545-548 | `maxClients==0` coerced to 1 instead of rejected — FIXED 526f3b17 |
| DRV-23 | LOW | ip_server_port.rs:683-689,823-836 | drvAsynIPServerPort.c:201-207,232-236 | UDP read drops `ASYN_EOM_END` at datagram boundary |
| DRV-24 | DOC | ip_server_port.rs:163-168 | drvAsynIPServerPort.c:582 | trailing tokens after protocol rejected (Rust stricter than C sscanf) — intentional divergence, documented in source |
| DRV-25 | NON-GAP | ip_server_port.rs:638-645 | drvAsynIPServerPort.c:462-483 | Rust returns bind error; C reports success on failed bind (C bug not reproduced) |
| DRV-26 | NON-GAP | ip_server_port.rs:823-836 | drvAsynIPServerPort.c:196-200 | C UDP readIt off-by-one (copy maxchars-1, advance maxchars) not reproduced |

### Category C — `serial_port.rs` ↔ `drvAsynSerialPort.c`

| id | sev | Rust | C | one-line |
|---|---|---|---|---|
| DRV-31 | CLEARED | serial_port.rs:566-574,868-934 | drvAsynSerialPort.c:837,959 | hard read/write error + EOF never closes connection → no auto-reconnect; also no EINTR/EAGAIN retry — resolved by later read-EINTR/EOF + DRV-37 write work; verified + write-test added e8b282af (see Fix Log) |
| DRV-32 | CLEARED | serial_port.rs:663 | drvAsynSerialPort.c:1080 | termios input flags: C `IGNBRK\|IGNPAR` vs Rust `cfmakeraw` → spurious 0x00 on BREAK/line errors — FIXED 2685bd6f (see Fix Log) |
| DRV-33 | CLEARED | serial_port.rs:681-682 | drvAsynSerialPort.c:1085-1086 | VSTART/VSTOP never set → XON/XOFF software flow control broken (NUL not ^Q/^S) — FIXED 117e612e (see Fix Log) |
| DRV-34 | CLEARED | serial_port.rs:159-208,263-270 | drvAsynSerialPort.c:271-345 | baud set narrower than C (no arbitrary baud on macOS/BSD; 28800 missing; Linux high rates rejected; silent 9600 fallback) — FIXED 7ef497b5 (see Fix Log) |
| DRV-35 | CLEARED | serial_port.rs:830-1000 | drvAsynSerialPort.c:254-256,599-605 | setOption mutates cached config before apply, no rollback on failure → get reports never-applied value — FIXED fdf7d488 (commit self.config only after successful apply, all 5 arms) |
| DRV-36 | CLEARED | serial_port.rs:846-848 | drvAsynSerialPort.c:594-616 | unknown option keys silently accepted (C: asynError); empty-key re-apply not honored — both halves now closed (4436d8a8) |
| DRV-37 | CLEARED | serial_port.rs:392-487 | drvAsynSerialPort.c:810-842 | write timeout applied per-poll-iteration, not single total deadline → write lives timeout×N (and a blocking write(fd,all) ignored the timeout entirely) — FIXED 92755a0f (single deadline + post-write check + non-blocking write loop) |
| DRV-38 | CLEARED | serial_port.rs:438-535,921 | drvAsynSerialPort.c:843 | write byte count dropped by `write_octet -> ()` — fixed via F8 `AsynResult<usize>` contract (see DRV-15/DRV-38 Fix Log); error-path partial count is a documented intentional divergence |
| DRV-39 | CLEARED | serial_port.rs:1131-1133 | drvAsynSerialPort.c:203-208 | getOption("break") errors instead of returning "off" — FIXED dbb9a127 ("break" arm returns "off") |
| DRV-40 | LOW | serial_port.rs:514-531 | drvAsynSerialPort.c:694-698 | connect() not guarded against double-open (fd + saved_termios leak) |
| DRV-41 | CLEARED | serial_port.rs:695-742 | drvAsynSerialPort.c:713-729 | connect() omits FD_CLOEXEC and connect-time tcflush(TCIOFLUSH) — FIXED 77930ec3 (FD_CLOEXEC after open + tcflush(TCIOFLUSH) before blocking restore) |
| DRV-42 | DOC | user.rs:15-22 | drvAsynSerialPort.c:906-909 | "wait-forever" (negative) timeout unrepresentable (framework `AsynUser.timeout` is an unsigned `Duration` vs C's `double`) — DOC, intentional framework-wide divergence (every blocking op bounded); documented at user.rs timeout field. Sibling of DRV-59 |
| DRV-43 | LOW | serial_port.rs:817-836,314-344 | drvAsynSerialPort.c:519-526,871-875 | disconnected `break` silently Ok; `maxchars==0` read misclassified as Disconnected |
| DRV-44 | LOW | serial_port.rs (no report) | drvAsynSerialPort.c:666-680 | report() lacks serial diagnostics (fd, nWritten, nRead) |
| DRV-45 | CLEARED | serial_port.rs:626-640; iocsh.rs:548-608 | drvAsynSerialPort.c:1032-1175 | drvAsynSerialPortConfigure registrar + default EOS + noAutoConnect/noProcessEos done (7115a8fb, 2ecb546c); priority accepted-but-ignored (documented, IP-command parity) |

### Category D — `prologix.rs` ↔ `drvPrologixGPIB.c`

| id | sev | Rust | C | one-line |
|---|---|---|---|---|
| DRV-46 | CLEARED | prologix.rs:385-441 | drvPrologixGPIB.c:439,422,534-535 | octet EOS interface now routes to driver `eos` state; output EOS rejected (asynGpib NULL slot) (01395338) |
| DRV-47 | CLEARED | prologix.rs:80,410,422 | drvPrologixGPIB.c:334-349 | eomReason (END/EOS/CNT) now reported via `io_read_octet_eom` owner + `read_eom` rule (see DRV-47 Fix Log) |
| DRV-48 | CLEARED | prologix.rs:133,392 | drvPrologixGPIB.c:386,409 | staged read remainder now discarded at `write_octet` via `clear_read_carry()` owner (F6 invariant; see DRV-48 Fix Log) |
| DRV-49 | CLEARED | iocsh.rs:628-680 | drvPrologixGPIB.c:547-628 | `prologixGPIBConfigure` iocsh command added (priority accepted-but-ignored, IP/serial-command parity) (f82f4537) |
| DRV-50 | CLEARED | prologix.rs:571-639 | drvPrologixGPIB.c:461-525 | `AsynGpib` now implemented for prologix (ifc/srqStatus real, rest unimplemented per C); devGpib discovery wiring is a separate gap (4732abce) |
| DRV-51 | LOW | prologix.rs:272-324 | drvPrologixGPIB.c:166-168 | connect() doesn't clear `read_carry` (reconnect-without-disconnect returns stale bytes) |
| DRV-52 | LOW | prologix.rs:322,333 | drvPrologixGPIB.c:213,231 | per-device connect/disconnect toggles port-level state + announces addr −1 (ASYN_MULTIDEVICE) |
| DRV-53 | LOW | prologix.rs:131-135 | drvPrologixGPIB.c:592-593 | port registered `destructible:true`; C registers no ASYN_DESTRUCTIBLE (over-grants shutdown rights) |
| DRV-54 | LOW | prologix.rs:396-400 | drvPrologixGPIB.c:253 | zero read timeout coerced to 1s (C passes 0 = poll verbatim) |
| DRV-55 | CLEARED | ip_port.rs:283-289, ip_server_port.rs:887-893,1169-1175,772-778,780-798 | drvAsynIPPort.c:736-740, drvAsynIPServerPort.c:180-184 | `maxchars==0` read guard unported → empty buffer reads Ok(0) → misread as peer EOF → healthy-connection teardown — FIXED 75654e2b (maxchars_zero_error at the 3 stream-read entries); UDP server read sibling FIXED efe9ae2a (read_octet + io_read_octet_eom UDP branches, C readIt:180-184, benign) |
| DRV-56 | CLEARED | ip_server_port.rs:1064-1080 | drvAsynIPServerPort.c:308-323 | UDP recv worker broke loop on EINTR → benign signal permanently killed reception; C also stops on EINTR (UDPbufferSize=-1) = C bug not copied — FIXED 789ecc2a (EINTR routed to non-fatal continue) |
| DRV-57 | CLEARED | serial_port.rs:321-333 | drvAsynSerialPort.c:871-875 | serial-driver sibling of DRV-55, ADVERSE: `SerialIoState::read` no `maxchars==0` guard → empty buf → `libc::read(fd,ptr,0)`==0 → misread as EOF → Disconnected → teardown of a live serial port — FIXED e83cc61d |
| DRV-58 | DOC | ip_port.rs:239-271 | drvAsynIPPort.c:631-674 | `write_with_retry` bounds total write time from write-start; C `writeRaw` only bounds time from the FIRST EWOULDBLOCK (`haveStartTime` set once at :631, never reset) and is unbounded while sends progress. Rust's bound is stricter/safer (bounded write latency), identical to C at `timeout==0`; intentional divergence, not a C-bug copy — DOC, no change |
| DRV-59 | DOC | serial_port.rs:674 | drvAsynSerialPort.c:1083,899-908 | termios-default sibling of DRV-32/33: C seeds `c_cc[VMIN]=0` and reprograms VMIN/VTIME per read from `pasynUser->timeout` (≥0 → VMIN=0 + VTIME/epicsTimer; <0 → VMIN=1). Rust uses a different read architecture — poll(POLLIN, timeout)-gated blocking read with static VMIN=1 — so `n==0 ⟺ EOF` is a clean invariant. Flipping to C's VMIN=0 would break that (spurious poll-wake → read 0 → false Disconnect). Architecture-coupled, every representable (non-negative) timeout is bounded by poll; intentional divergence, not a C-bug copy — DOC, no change |

NON-GAPs / parity-clean (Rust correctly declines C bugs or improves): ip_port
ASIDES (IPv6 superset, null-term, DNS caching); ip_server DRV-25/26; serial
termios-restore-on-disconnect, POLLHUP busy-loop declined, value leniency;
prologix ASIDE-A/B/C + the full verified-wire-exact connect-burst / addressing /
escaping list. See round report.

## Review Log

### drv-s2..s4 convergence — 2026-06-29 (2 opus panels: parity + adversarial)

ip_port singleton fixes DRV-7/11/13 reviewed, then driven to convergence.
The adversarial lens reopened DRV-11 (the readRaw/writeRaw zero/edge family
was not closed by the first commit) and then the maxchars==0 family:

- DRV-11 closed across client + both server reads (read EINTR non-fatal via
  the shared `is_nonfatal_read_timeout` owner) + zero-timeout write deadline
  floor. DRV-56 (UDP recv worker EINTR-death) split out as distinct.
- DRV-55 maxchars==0 grew from 3 IP stream reads → +UDP server read
  (`readIt` sibling) → +serial read (DRV-57, the only *adverse* one:
  empty-buf teardown). Final sweep across all 6 drivers: every syscall-backed
  octet read entry is guarded; prologix + ftdi/usbtmc/vxi11 are non-members
  (no C guard / no syscall, benign by construction).
- DRV-58 (write make-progress deadline: total in Rust vs from-first-block in
  C) and the client/server flush-drain EINTR asymmetry both dispositioned as
  documented intentional/benign divergences (no code change).

Both panels returned **converged: YES**. maxchars==0 family CLOSED; DRV-11
family CLOSED. Round artefacts: `.caucus/.../rounds/01KW8S86GCEE7197SXZSQJ9262.md`
(drv-s3) and `.../01KW8T7SCXT2VRN97YGQY4EEM6.md` (drv-s4).

### Round 1 — 2026-06-29 (4 opus panels, C-first)

48 real findings: 4 DEFECT + ~19 CONCERN + ~25 LOW; 2 NON-GAP. The findings
cluster into structural families (the high-leverage fix units):

- **F1 — fatal transport error must flip connection state (auto-reconnect):**
  DRV-5 (ip write), DRV-31 (serial read+write). ip_port READ already does this;
  the invariant ("a fatal transport error closes the fd + fires
  exceptionDisconnect so the actor's `!connected`-gated reconnect fires") is the
  established asyn-rs pattern, just not applied to ip write / serial. **DEFECT,
  local fix, highest value.**
- **F2 — EOF / empty-datagram semantics (ip_port):** DRV-3 (TCP EOF =
  success+END not error), DRV-4 (empty UDP datagram ≠ EOF). Local.
- **F3 — UDP unconnected-socket model (ip_port):** DRV-1 (don't connect() the
  UDP socket; sendto/recvfrom), DRV-14 (no empty datagram on zero-write). Local
  but a behavioral redesign of the UDP path.
- **F4 — close the option dispatch (reject unknown keys):** DRV-9 (ip),
  DRV-36 (serial); siblings DRV-8, DRV-10. Local, clear C contract.
- **F5 — eomReason END/EOS never reported (no io_read_octet_eom override):**
  DRV-3, DRV-23 (ip_server UDP), DRV-47 (prologix). Family.
- **F6 — connect()/write() must reject double-open + discard staged read
  state:** DRV-12 (ip), DRV-40 (serial), DRV-48 (prologix write, DEFECT),
  DRV-51 (prologix connect). Invariant family.
- **F7 — default EOS interpose + iocsh configure registrar:** DONE (see Fix
  Log). Family corrected after reading C: EOS auto-install = IP (DRV-2,
  `asynInterposeEos`), serial (DRV-45, octetBase built-in EOS), ftdi
  (`drvAsynFTDIPort.cpp:622-623`). prologix DRV-46 (own in-driver `eos`
  state, no interpose in C) and DRV-49 (prologix iocsh command) are DISTINCT
  — prologix manages EOS itself and is not part of the auto-install family;
  they remain open as prologix-specific items. ip_server is DISTINCT too
  (`drvAsynIPServerPort.c:659` passes `0,0,0`, vestigial `#include`, no
  auto-install). Sign-off obtained 2026-06-29: full structural wiring.
- **F8 — partial nbytesTransfered on error (framework `write_octet -> ()`):**
  DRV-15 (ip), DRV-38 (serial). **Framework-wide trait-signature change. Needs
  sign-off.**
- **F9 — ip_server interrupt push delivery (driver's documented purpose):**
  DRV-16, DRV-17. Plus DRV-18/19/20.
- Singletons: DRV-6/7/11/13 (ip), DRV-32/33/34/35/37/39/41/42/43/44 (serial;
  DRV-32/33/34/35/37/39/41/43/44 CLEARED, DRV-42/59 DOC; all serial singletons closed),
  DRV-50/52/53/54 (prologix), DRV-21/22/24 (ip_server).

Fix-phase plan: start with F1 (DEFECT, local, established invariant), then
F4/F5/F6 (local, clear contracts), then per-driver singletons. F7 (EOS+iocsh
architecture), F8 (write_octet count contract), and F3 (UDP redesign) are
surfaced for sign-off before implementation because they change framework
contracts / behavioral models, not just a driver site.

### Round 2 — 2026-06-29 (verification, 2 fresh opus panels) — CONVERGED

Verification round on the F5-remaining + F8 + DIV (DRV-61/62) batch. Both
panels returned **CLEARED on every item, zero DEFECT, zero CONCERN requiring
code change**:

- DRV-23 ip_server UDP eom (incl. the effc2766 exact-fit `END|CNT` refinement),
  DRV-47 prologix eom, F8 `write_octet -> usize` contract + per-driver and
  downstream counts, DRV-61 noDelay strict Y/N, DRV-62 serial bool/parity strict
  — all byte-exact against C.
- Confirmed Rust correctly declines three C bugs (not copying): the UDP readIt
  off-by-one (drvAsynIPServerPort.c:196-200) + its dead :235 CNT branch; the
  modbus Z-string `--*nActual` under-report / SIZE_MAX underflow
  (drvModbusAsyn.cpp:1552); and (documented, acceptable) the write-error
  partial-count omission.
- **Unverifiable-pending-source:** mqtt `write_octet` exact `*nbytesTransfered`
  for an embedded-NUL payload is owned by the `autoparamDriver` framework, which
  is not vendored locally (only drvMqtt.cpp/.h under epics-modules/mqtt/, and
  autoparamDriver.h is absent from ~/codes entirely). `raw.len()` is
  semantically correct (published, NUL-truncated length) but mid-string-NUL
  byte-exactness needs that source to close.

Note: the caucus driver-review panels wedged on a display/SIGWINCH event this
session; restart-resume left them untracked (briefs never landed). This round
ran on two freshly-spawned panels — the reliable path.

### Round 3 — 2026-06-29 (F7 EOS family, 2 opus panels + 1 verify pass) — CONVERGED

Review of the F7 EOS auto-install family (6 commits: routing enabler +
ip/serial/ftdi install + serial iocsh + doc).

- **drv-eos-install: CLEARED.** All four auto-install/suppression paths
  byte-checked against C (`drvAsynIPPort.c:1065-1066`,
  `drvAsynSerialPort.c:1126`, `drvAsynFTDIPort.cpp:616,622-623`); both DISTINCT
  no-install sites confirmed correct (prologix inner `_TCP` via
  `DrvAsynIPPort::new`; ip_server `0,0,0`).
- **drv-eos-routing: 1 DEFECT, then FIXED.** `EosInterpose::read`
  short-circuited to `next.read` on an empty terminator, conflating C's
  construction-time `processEosIn==0` ("never process") with `eosInLen==0`
  ("terminator cleared"). For an always-`processEosIn==1` installed interpose
  this stranded read-ahead bytes in `in_buf` when IEOS was cleared
  (reachable via `OctetReadBinary` / runtime IEOS clear after a line read).
  Fixed in **f921dafd**: remove the short-circuit so `read` always runs the
  buffering loop, gate only the *match* on a non-empty terminator (mirror C
  `readIt:191` + `:199`). Regression test
  `cleared_input_eos_still_drains_buffered_readahead`.
- **Verification pass (re-finder + fresh adversary): both CLEARED.** The
  adversary could not break the fix across read-ahead, END/CNT
  capture/override/propagation, zero-byte END survival, 2-byte-EOS straddle,
  `maxchars==0`, and UDP/TCP/serial/ftdi datagram semantics. It also confirmed
  two deliberate Rust divergences that *fix* real C bugs (not copied): the
  straddle `n_read -= eos.len().min(n_read)` guard vs C's `SIZE_MAX` underflow
  (`readIt:203`), and the `maxchars==0` early return vs C's one-byte write into
  a zero-length buffer (`readIt:197`).

asyn-rs `-p` clippy `--all-targets` + nextest (666) green per commit;
full-workspace pre-push pass still owed.

### Round 4 — 2026-06-29 (F9 ip_server local fixes, 2 fresh opus panels) — 1 DEFECT, fixed; reverify pending

Review of the three tractable F9 fixes (DRV-18 UDP SO_REUSEPORT, DRV-19
bind-host resolution, DRV-20 UDP flush). Ran on two freshly-spawned opus
panels (parity + adversary).

- **DRV-18 / DRV-19: CLEARED by both panels.** Byte-checked against C
  `createServerSocket` (drvAsynIPServerPort.c:403-419,426-430) and
  `osdSockAddrReuse.cpp:60-68`. Empty/`localhost`→`0.0.0.0` confirmed
  faithful (C comment :407-411 refuses loopback mapping); SO_REUSEPORT
  UDP-only confirmed; no stray bare `SocketAddr::parse` bind site remains.
  Adversary asides (not defects): a multi-record hostname `.next()` may
  bind IPv6 where C's IPv4-only `hostToIPAddr` binds IPv4 — consistent
  with the whole asyn-rs resolver, exotic for a bind interface; Rust
  `trim()`s host whitespace C would reject (more lenient); `#[cfg(unix)]`
  SO_REUSEPORT on Solaris/illumos is a non-target.
- **DRV-20: DEFECT found by the adversary, then FIXED (9522b61b).** The
  UDP-cache half was correct, but the TCP data path was left a no-op while
  C drains it: each accepted connection is a full `drvAsynIPPort` child
  (drvAsynIPServerPort.c:690) whose `flushIt` tosses all pending socket
  input (drvAsynIPPort.c:846-861). Closed via a shared
  `ClientSlot::drain_input` owner wired into both the child subport flush
  and the parent's addr-routed TCP flush (see Fix Log).
- **DRV-16 / DRV-17: untouched, OPEN for sign-off** (octet-interface
  interrupt subsystem). Adversary noted the polled-cache vs interrupt-
  callback delivery as a pre-existing design choice, consistent with this.

asyn-rs `-p` clippy `--all-targets` + nextest (673) green per commit;
full-workspace pre-push pass still owed.

### Round 5 — 2026-06-29 (DRV-20 TCP-sibling reverify, 2 fresh opus panels) — CONVERGED

Reverify of the DRV-20 TCP-flush-drain fix (9522b61b). Both fresh opus
panels (parity + adversary) returned **DRV-20: CLEARED**.

- `ClientSlot::drain_input` byte-matches C `drvAsynIPPort::flushIt`
  (drvAsynIPPort.c:846-861): unoccupied-slot guard ↔ `fd != INVALID_SOCKET`,
  non-blocking toggle/restore ↔ `setNonBlock(1)`/`setNonBlock(0)`,
  recv-until-empty with `Ok(0)`/`WouldBlock`/`Err` breaks ↔ `numRecv<=0`
  break, no error surfaced and no slot teardown on EOF during flush.
- Both TCP flush entry points reach the drain: parent `io_flush` TCP
  branch (addressed slot, or all slots on `addr<0`) and child
  `DrvAsynIPSubport::io_flush`; the actor routes both `RequestOp::Flush`
  and `OctetWriteRead{flush:true}` through `driver.io_flush`. No interpose
  bypass (the server reads raw, no EOS read-ahead). UDP unaffected.
- Adversary "could not break it." Three intentional, safe divergences:
  (a) the Rust parent is a data-bearing `multi_device` TCP octet path with
  no C counterpart (C parent TCP octet is all-NULL; data flows only
  through child `parent:N`), so its flush drain is required by *that* Rust
  design; (b) `addr<0` broadcast drain has no C equivalent but is
  symmetric with the existing Rust broadcast write; (c) Rust bails on a
  `set_nonblocking(true)` failure where C would risk a blocking-recv hang.
  `set_nonblocking` (O_NONBLOCK) is orthogonal to `read_timeout`
  (SO_RCVTIMEO), so the restore leaves no wrong mode behind.

**F9 tractable findings (DRV-18/19/20 + TCP sibling) CONVERGED.** DRV-16/17
remain OPEN for sign-off (octet-interface interrupt subsystem).

## Fix Log

- **DRV-5** (DEFECT, ip_port write-side teardown) — CLEARED. Added a single
  owner for the fatal-transport-error classification
  (`is_fatal_transport_error`) and teardown (`drop_connection`), shared by the
  read path (refactored to call them) and the write path (new). On a fatal
  write error (`ECONNRESET`/`EPIPE`) the port now closes the socket + sets
  `connected=false` so the actor's `!connected`-gated auto-reconnect
  (port_actor.rs:311-322) re-establishes it on the next request — symmetric
  with the read path. Tests: `test_is_fatal_transport_error_classification`,
  `test_write_error_disconnects`.
- **DRV-31** (DEFECT, serial error→disconnect + EINTR retry) — CLEARED. Sibling
  of DRV-5 in the F1 family. (a) `read_octet`/`write_octet` now tear down the
  connection (`drop_connection`: close fd + `connected=false`, matching C
  `closeConnection`) on a fatal read/write error or EOF, so the actor's
  auto-reconnect re-opens the device — previously the port stayed `connected`
  with a dead fd forever. (b) `SerialIoState::read`/`write` now retry
  `poll`/`read`/`write` on EINTR (signal) and EAGAIN/EWOULDBLOCK (spurious),
  matching C; this is required so a benign signal isn't misclassified as a fatal
  Io error and doesn't spuriously trip the new teardown. The classifier is
  duplicated from ip_port (a trivial pure predicate over exactly two raw-fd
  transports — promote to a shared owner only if a third caller appears). Test:
  `test_pty_read_error_disconnects` (close pty master → fatal read → connected
  flips false).
- **DRV-48** (DEFECT, prologix write leaves stale read_carry) — CLEARED. F6
  invariant: every transaction/session boundary must discard staged read data.
  Added a single owner `clear_read_carry()` and called it at the start of
  `write_octet` (C `prologixWrite` sets `bufCount=0` at the start of every
  write); routed the existing `io_flush`/`disconnect` clears through the same
  owner. Without it, when a reply overflowed the caller buffer the tail stayed
  in `read_carry` and the next plain write+read returned it as the response to
  the new command (cross-transaction stale-data leak; SyncIO/streamDevice were
  shielded by their flush-before-read, raw asynOctet consumers were not). Test:
  `write_discards_staged_read_carry`.
- **DRV-51** (LOW, prologix connect doesn't clear read_carry) — CLEARED. Sibling
  of DRV-48 in the F6 invariant. `connect` reset the address cache
  (last_primary/last_secondary) but not `read_carry`; C `prologixConnect` also
  resets `bufCount=0`. A reconnect driven without an intervening `disconnect`
  (e.g. the inner ip_port auto-disconnected on a read error, then reconnect)
  would leak the dead session's tail into the first read. Routed through the
  same `clear_read_carry()` owner. Test: `connect_discards_staged_read_carry`.
- **DRV-46** (CONCERN, prologix octet EOS interface not wired to driver `eos`) —
  CLEARED (01395338). C prologix is an `asynGpibPort` whose octet EOS interface
  maps to `prologixSetEos`/`prologixGetEos` over the single `pdpvt->eos` field
  (drvPrologixGPIB.c:422-459). The Rust prologix used the default
  `PortDriver::{set,get}_input_eos`, which write `base.input_eos` and forward to
  an (empty) interpose stack — bytes the read (`++read <eos>` vs `++read eoi`)
  and write (append-on-`eos>=0`) paths never consult, so `set_input_eos` had no
  protocol effect and `get_input_eos` echoed stored-but-ineffective bytes (C
  reports eoslen from `pdpvt->eos`, default 0). Defect family — the EOS
  interface must reflect the driver's real EOS state, never a dead base cache —
  fixed at both sites: input EOS routes to `State.eos` (eoslen 0→None, 1→Some,
  >1→asynError "Invalid EOS" per asynGpib.c:443 / prologixSetEos:449), and
  output EOS is rejected (asynGpib leaves the output-EOS vtable slots NULL,
  asynGpib.c:132 `...setInputEos, getInputEos, 0, 0`) rather than silently
  caching ineffective bytes. Test: `eos_interface_routes_to_driver_state`.
- **DRV-46(b)** (follow-up DEFECT surfaced by the convergence round, adv panel
  `converged: NO`) — CLEARED (b68d08d5). The wiring above made the driver's
  EOS-mode read (`++read <eos>`) reachable via the standard IEOS interface, but
  the bridge stayed pinned at `++eot_enable 1` (set once at connect). With EOT
  enabled, a GPIB instrument that asserts EOI together with its terminator makes
  the bridge append the EOT marker after the eos char (`...\n\xEF`); the
  eos-mode read loop breaks only on the eos byte, so it never sees its
  terminator and the read times out. C `prologixSetEos` *design* re-issues
  `++eot_enable (eos<0)` on every eos change (drvPrologixGPIB.c:456) — EOI mode
  enable=1, EOS mode enable=0 — but C's never-store bug leaves its driver eos
  dead so the path never runs (bug not copied; the command it sends is the
  design). Realized it: `eot_enable_arg(eos)` (None→1, Some→0); `set_eos` (now
  `&mut self`, single owner, commits `State.eos` only after a successful bridge
  write per the DRV-35 rule) re-issues it when connected; the connect handshake
  derives the init burst's `++eot_enable` from `State.eos` (so an eos set before
  connect is honored). Also replaced the false `set_eos` comment claiming the
  command was "punted to the next read/write path" (no such code existed).
  Tests: `eos_mode_toggles_bridge_eot_enable`,
  `eos_set_before_connect_seeds_eot_enable_off`.
- **DRV-46(c)** (eos terminator strip — semantic) — CLEARED (user sign-off to
  strip). The prologix read kept the matched eos terminator byte as data
  (prologix.rs:578-583), whereas the standard Rust `EosInterpose` strips the
  matched terminator (eos.rs:130-139), C's asynGpib read layer strips it
  (asynGpib.c:415-419), and streamDevice/asynRecord expect it stripped. C's
  prologix *driver* itself does not strip (its driver-level eos is dead; the
  asynGpib layer one level up does), so there is no direct driver-level C
  reference — but the *observable* C behavior a record sees is the stripped form.
  Now strips uniformly: `if acc.last() == Some(&terminator)` removes the matched
  terminator in BOTH modes (EOT marker in EOI mode — unchanged — and the eos byte
  in EOS mode — new). EOM still flags EOS in EOS mode (read_eom sees the
  shortened `remaining`). Flipped the deliberately-tested contract:
  `read_with_eos_keeps_terminator_byte` → `read_with_eos_strips_terminator_byte`
  (record now sees `OK`, not `OK\n`); the line-578 comment was rewritten. The
  EOI-mode test (`read_strips_eot_marker_in_eoi_mode`) is unaffected — its `\n`
  is payload, only the EOT marker strips.
- **DRV-46(c)-EOM** (EOM-flag sequencing on small/exact-fit buffers — NIT,
  intentional divergence) — DOCUMENTED. The DRV-46(b)+(c) convergence round
  (opus parity panel `converged: YES`; opus adversary raised this one item).
  Scenario: EOS mode, eos=`\n`, reply `OK\n`, caller buffer 2. Rust returns
  `OK`/`EOS|CNT` in ONE call (eager strip → `remaining`=2; `read_eom(2,2,true)`
  sets both). The full C stack returns `OK`/`CNT` then ``/`END|EOS` across TWO
  calls. Ground-traced against the real C: C's prologix *driver* eos is
  initialized to `-1` (drvPrologixGPIB.c:560) and `prologixSetEos` never stores
  `newEos` (drvPrologixGPIB.c:454-458 — writes `++eot_enable (pdpvt->eos<0)` off
  the *stale* value, no assignment), so the driver is **permanently EOI mode**;
  a record's IEOS sets the asynGpib **layer** eos (`pgpibPvt->eos` via
  `asynGpib::setInputEos`, :447-451). `asynGpib::readIt` (:411-422) calls the
  driver, then strips the layer-eos byte (`nt--`, ORs `ASYN_EOM_EOS`) and OR-s
  `CNT` from the **post-strip** `nt`; the driver itself OR-s its eom from the
  **pre-strip** count and flags `END` (not EOS) because its eos is `-1`. So C's
  extra `END` on the strip call is a direct artifact of the dead-eos bug, and
  the quirky `CNT` is C computing it on the pre-strip byte count in its
  two-layer architecture. KEPT Rust's folded single-layer eager strip (data is
  byte-identical; standard consumers — devAsynOctet, asynRecord, streamDevice —
  key on EOS/END for completion and accept `EOS|CNT`; no hang, no corruption).
  Copying C's sequencing would require relingering the eos byte and reproducing
  the dead-eos `END` — i.e. re-introducing the dual-meaning patch and copying
  the bug, both rejected. Also note the broader, accepted DRV-46 design choice:
  Rust drives the *bridge* into EOS mode (`++read <eos>`, `++eot_enable 0`) when
  IEOS is set — implementing what `prologixSetEos` *intended* (the
  mode-switch command it issues) — whereas buggy C keeps the bridge in EOI mode
  and strips at the layer. Same observable payload for terminator-emitting
  instruments; divergence only for an EOI-without-terminator instrument, where
  Rust realizes the intended design.
- **DRV-49** (CONCERN, no prologix iocsh registrar) — CLEARED (f82f4537). C
  `drvPrologixGPIB.c` registers `prologixGPIBConfigure(portName, host, priority,
  noAutoConnect)` (lines 547-628); the Rust prologix had no iocsh command, so
  `DrvAsynPrologixPort` was unreachable from st.cmd. Added
  `drv_asyn_prologix_port_configure_command` mirroring the IP/serial commands
  (portName + host required; `priority` accepted-but-ignored — the Rust runtime
  schedules port actors uniformly, same disposition as the IP/serial commands;
  `noAutoConnect` honored). No `noProcessEos` arg: the prologix driver owns EOS
  and passes `noProcessEos=1` to its inner `_TCP` IP port (C drvPrologixGPIB.c:
  575). Registered on both the `IocApplication` and direct-`IocShell` paths; the
  port lands in the `asyn_record` registry so `asynRecord` resolves it by name.
  Tests: `drv_asyn_prologix_port_configure_registers_port`,
  `drv_asyn_prologix_port_configure_rejects_missing_host`.
- **DRV-50** (CONCERN, GPIB command interface absent — `AsynGpib` scaffold with
  zero implementors) — CLEARED (4732abce). C prologix exposes an `asynGpibPort`
  vtable (`prologixMethods`, drvPrologixGPIB.c:527-545) whose GPIB bus-control
  operations are mostly unimplemented by the bridge; only `ifc` and `srqStatus`
  are real. Implemented `AsynGpib for DrvAsynPrologixPort` matching that vtable:
  `ifc` writes `++ifc\n` to the TCP transport (prologixIfc:476-484); `srq_status`
  reports not-asserted (`*srqStatus = 0`, :493-498); `srq_enable` is a no-op
  success (:500-504); `addressed_cmd` / `universal_cmd` / `ren` / `serial_poll`
  return asynError unimplemented (:461-491, :513-518). C's diagnostic printf in
  the serial-poll stubs is not copied (the error carries the same info). RESIDUAL
  (not closed here, separate larger gap): there is no Rust devGpib /
  `findInterface(asynGpibType)` discovery layer, so nothing in the stack reaches
  this interface — it provides the bridge's real GPIB behavior for a future GPIB
  device-support layer. Tests: `gpib_ifc_writes_bridge_command`,
  `gpib_command_interface_matches_c_methods`.
- **DRV-9 / DRV-36** (CONCERN, option dispatch leaks unknown keys) — CLEARED. F4
  family. Both `ip_port`/`serial_port` override `set_option` with a known-key
  chain, then a catch-all that inserted unknown keys into the generic
  `base.options` map and returned Ok, so a later `get_option` echoed the
  arbitrary value back. C `drvAsynIPPort.c::setOption` (941-945) / `getOption`
  (902-906) and `drvAsynSerialPort.c::setOption` (594-598) instead return
  asynError "Unsupported key" for any non-empty unsupported key; only the empty
  key is a silent no-op (`epicsStrCaseCmp(key,"") != 0` guard). The catch-all
  now applies that contract (empty → no-op, else → `OptionNotFound`); the real
  handlers own every supported key so nothing populates the generic map and
  `get_option` can no longer echo. Tests: `unsupported_option_key_is_rejected`
  (ip), `test_set_option_unknown` (serial, rewritten from asserting the leak).
  Distinct (not fixed here): the shared default trait `set_option`
  (port.rs:875) used by `ip_server`/`prologix`, whose C drivers register no
  `asynOption` interface at all — an interface-advertisement question for the
  F7/interface sign-off, not the known-key-chain leak.
- **DRV-36 empty-key re-apply** (CONCERN, second half of DRV-36) — CLEARED
  (4436d8a8). The F4 fix above closed only the unknown-key half; the empty key
  `""` was left a pure no-op even when connected. C `setOption` (:609-615) runs
  `applyOptions` on the empty key when `fd >= 0`, which forces CREAD and
  re-pushes the cached termios via `tcsetattr` (`applyOptions`, :119-126),
  re-applying the configured line state — a restore if another process changed
  the port out from under the driver. Closed structurally: extracted
  `build_configured_termios()` (serial_port.rs) as the single owner of the
  configured line state (cfmakeraw + fixed seeds + `self.config`), shared by
  `connect` and the empty-key re-apply; like C's `applyOptions` it rebuilds from
  the cached config (not a device read-back), so it overwrites an external
  clobber. Test: `pty_empty_key_reapplies_configured_termios` flips CSTOPB
  externally and confirms the empty key restores it; the disconnected no-op
  stays covered by `test_set_option_unknown`.
- **DRV-10** (LOW, disconnectOnReadTimeout value validation) — CLEARED. The
  `disconnectOnReadTimeout` branch coerced any value to a bool via a lenient
  truthy parse (`"Y"|"y"|"1"|"yes"` → true, everything else → false) and never
  errored. C `drvAsynIPPort.c::setOption` (924-935) accepts only "Y"/"N"
  (case-insensitive) and returns asynError "Invalid disconnectOnReadTimeout
  value." for anything else. The branch now validates strictly and errors on
  any non-Y/N value (including the empty string), matching C and the "Y"/"N"
  shape that `get_option` already reports. Test:
  `test_disconnect_on_read_timeout_value_validation`. Defect-family anchor: the
  same lenient parse also drove `noDelay` (ip_port.rs:980). That one has no C
  value-validation contract (C has no `noDelay` key, DRV-8), but a typo silently
  coercing to "off" is a latent footgun and validating one Y/N option strictly
  while another stayed loose is a non-uniform rule — tightened to strict Y/N in
  DRV-61 (the `noDelay` key itself remains the DRV-8 superset).
- **DRV-8** (CONCERN, noDelay is a non-C option key) — KEPT as an intentional
  Rust superset (documented divergence, no code change). C
  `drvAsynIPPort.c:525-534` sets TCP_NODELAY unconditionally on every TCP/INET
  socket with no option key. Rust's `no_delay` defaults to `true` for every
  TCP port built via `IpPortConfig::parse` (ip_port.rs:110), so the default
  behavior already matches C (NODELAY on); the `noDelay` option only adds the
  ability to turn it off, a superset feature like the IPv6/Unix-socket
  extensions. This is not a C-bug copy (C's lack of a toggle is not a bug, and
  the Rust default matches C's behavior), so it is recorded as an intentional
  divergence rather than fixed. The HTTP path (ip_port.rs:740) sets NODELAY
  unconditionally like C; the conditional only governs the configurable
  TCP/TcpReusePort path.
- **DRV-12** (LOW, ip connect double-open guard) — CLEARED. F6 "reject
  double-open" half. C `drvAsynIPPort.c::connectIt` (424-427) returns asynError
  "Link already open!" when `tty->fd != INVALID_SOCKET`; the Rust `connect`
  opened a second socket unconditionally, overwriting `io.inner` and leaking
  the first. Added the guard at the top of `connect` (mirror: `io.inner.is_some()`).
  Reconnect is unaffected — the F1 teardown clears `io.inner` before the actor's
  `!connected`-gated reconnect runs. Test: `connect_rejects_double_open`.
- **DRV-40** (LOW, serial connect double-open guard) — CLEARED. Sibling of
  DRV-12 in the F6 "reject double-open" half. C `drvAsynSerialPort.c::connectIt`
  (694-698) returns asynError "Link already open!" when `tty->fd >= 0`; the Rust
  `connect` called `libc::open` unconditionally, overwriting `io.fd` (leaking the
  old fd) and `saved_termios` (losing the original device settings). Added the
  guard at the top of `connect` (mirror: `io.fd.is_some()`). Test:
  `test_pty_connect_rejects_double_open`.
- **DRV-3** (CONCERN, TCP EOF = success+END; END never emitted) — CLEARED
  (sign-off, read-contract change). C `drvAsynIPPort.c::readRaw` (815-821)
  returns asynSuccess with zero bytes and ASYN_EOM_END on a TCP `recv()==0`
  and runs `closeConnection`; Rust returned `Err(Disconnected,"EOF")` and the
  END bit never reached a consumer (close-delimited protocols like HTTP/1.0
  saw a read error at message end). The fix spans three sites (one finding):
  (1) base `OctetNext::read` TCP + Unix-stream EOF now returns
  `Ok((0, END))` instead of an error; (2) the EOS interpose
  (`interpose/eos.rs`) now captures the lower read's `eom` BEFORE the
  zero-byte break so END survives the chain (C `asynInterposeEos.c:232,241,246,251`
  — `*eomReason = eom` after the loop); (3) ip_port factors `read_octet` and a
  new `io_read_octet_eom` override through a shared `read_octet_core` that
  returns the real `eom_reason` (closing the F5 propagation gap — `read_octet`
  alone dropped it) and runs `drop_connection` when END is seen (C
  closeConnection), so the F1 teardown moves from the error path to the END
  path while genuine fatal errors still tear down via the Err branch. Test:
  `test_server_disconnect_eof` (rewritten: EOF → `(0, END)` + `!connected`).
  Full asyn-rs suite (654 tests) green.
- **DRV-1** (DEFECT, UDP socket was connect()-ed) — CLEARED (sign-off, F3 UDP
  redesign). C `drvAsynIPPort.c::connectIt` (513) never `connect()`s a
  SOCK_DGRAM socket; it keeps the resolved remote (`tty->farAddr`) and uses
  `sendto` (656) / `recvfrom` (775-789). Rust `connect()`-ed the UDP socket,
  so inbound datagrams were filtered to the single connected peer — broadcast
  (`udp*`) replies and multi-peer answers were silently dropped, and a device
  that answers from a different source port was unreachable. Redesigned:
  `IpIoInner::Udp` now carries the resolved peer `SocketAddr`; `connect_udp`
  resolves the peer and binds an unconnected socket of the peer's address
  family (no `connect()`); reads use `recv_from`, writes use `send_to(peer)`,
  flush uses `recv_from`. Test: `test_udp_accepts_reply_from_any_peer` (a reply
  from a peer never sent to is received — a connected socket would drop it).
- **DRV-4** (DEFECT, empty UDP datagram treated as EOF) — CLEARED (sign-off,
  F3). C `drvAsynIPPort.c::readRaw` only treats `recv()==0` as a closed
  connection for SOCK_STREAM (the EOF/closeConnection branch at line 815 is
  `socketType == SOCK_STREAM`); a SOCK_DGRAM `recvfrom()==0` is a legitimate
  zero-length datagram. Rust returned `Err(Disconnected,"EOF")` on a 0-byte
  UDP read, which (post-F1) tore down the socket — a single empty datagram
  killed the port. The UDP read arm now reports a successful zero-byte read
  with an empty reason (no END, no teardown). Test:
  `test_udp_empty_datagram_is_not_eof` (0-byte datagram → `read==0`, still
  connected, next real datagram still read).
- **DRV-14** (LOW, zero-length write emits an empty UDP datagram) — CLEARED
  (sign-off, F3). C `drvAsynIPPort.c::writeRaw` (613-614) returns asynSuccess
  immediately on a zero-length write (after the connection check), sending
  nothing; Rust ran `send_to`/`write_with_retry` with the empty slice, which
  for UDP emits a spurious empty datagram. Added the early return in
  `OctetNext::write` after the connection check (so a disconnected port still
  errors, matching C order) — output EOS is appended by the interpose above,
  so reaching the base with empty data means there is genuinely nothing to
  send. Test: `test_udp_zero_length_write_sends_nothing`. F3 (DRV-1/4/14) now
  complete; full asyn-rs suite (657 tests) green.
- **DRV-61** (CONCERN, noDelay value parse looser than C strict Y/N; review
  round DIV-1) — CLEARED. Follow-up of DRV-8/DRV-10. The `noDelay` value used
  the lenient coercion (`"Y"|"y"|"1"|"yes"` → on, everything else silently →
  off) — the same shape DRV-10 removed from `disconnectOnReadTimeout`. C has no
  `noDelay` key so there is no parity contract for the value, but (a) silently
  coercing a typo to "off" is a latent footgun and (b) validating one Y/N
  option strictly while another stayed loose is a non-uniform rule. Tightened
  to strict Y/N (case-insensitive), erroring on anything else, matching the
  `disconnectOnReadTimeout` branch. The `noDelay` *key* itself remains the
  intentional Rust superset of DRV-8 (default on via `IpPortConfig::parse`,
  matching C's unconditional TCP_NODELAY; only the off-toggle is the
  extension). Test: `test_set_option_nodelay` (extended to assert `"1"`/typo
  now error).
- **DRV-62** (CONCERN, serial bool/parity option values looser than C; review
  round DIV-2) — CLEARED. The serial-side siblings of DRV-10/DRV-61 that the
  earlier strict-validation fix did not cover (the IP fix was applied only to
  `disconnectOnReadTimeout`). (a) `parse_bool_option` — the single owner of
  `clocal`/`crtscts`/`ixon`/`ixoff`/`ixany` — accepted `y/yes/1/true` /
  `n/no/0/false`, but C `drvAsynSerialPort.c::setOption` (410-504) accepts
  strictly `Y`/`N` (`epicsStrCaseCmp`), else asynError "Invalid <key> value.";
  tightened to strict Y/N (case-insensitive). (b) `parity` accepted single-char
  aliases `n`/`e`/`o`, but C (379-395) accepts only `none`/`even`/`odd`, else
  asynError "Invalid parity."; aliases dropped. These were accept-more-than-C
  leniency divergences (all C-valid inputs still work); tightened for parity and
  for uniformity with the IP Y/N options rather than recorded as intentional,
  since validating one Y/N option strictly while a sibling stays loose is the
  non-uniform rule the structural-fix guidance removes. Tests:
  `test_parse_bool_option` (rewritten to strict Y/N), `test_set_option_parity`
  / `test_set_option_parity_case_insensitive` (use full words, assert alias
  rejected). DISTINCT, not touched: `bits`/`stop` (exact-match-or-error =
  C-faithful), `baud` (numeric, narrower than C — separate pre-existing item),
  rs485 options (Linux-only Rust extension, no C `setOption` counterpart),
  prologix/ip_server (register no C `asynOption` interface).
- **DRV-15 / DRV-38** (F8 — write byte count dropped by the `write_octet -> ()`
  contract) — CLEARED (framework trait-signature change, sign-off). C
  `asynOctet::write` fills `*nbytesTransfered`; the interpose layer and base
  `OctetNext::write` already returned `usize`, but `PortDriver::write_octet`/
  `io_write_octet` (and the public `PortHandle`/`SyncIO::write_octet`,
  `AsynOctet::write_octet`) returned `()`, discarding the count
  `dispatch_write` produced (drvAsynIPPort.c writeRaw 678-705 accumulates
  `*nbytesTransfered`; drvAsynSerialPort.c:843). Changed the whole write
  contract to `AsynResult<usize>` (success carries bytes written), the
  write-side twin of the F5 read fix (`io_read_octet_eom`). The actor carries
  it via the existing `RequestResult.nbytes` (`RequestResult::write_n`); the
  public `PortHandle`/`SyncIO::write_octet` now return it. Per-driver counts:
  ip/serial return `dispatch_write`'s count; prologix returns the caller's
  `data.len()` (not the GPIB-framed `out` length); ip_server returns
  `data.len()` (`write_all`, full write). Downstream impls updated:
  `ad-core-rs` (`data.len()`), `mqtt-rs` (`raw.len()`, published payload),
  `epics-modbus-rs` (`consumed.min(data.len())` — register-budget-capped, NUL
  excluded, C `writeOctet` char count). DIVERGENCE (documented, not fixed):
  the **error-path partial count** is NOT carried — on a partial-write-then-
  error `Result<usize, AsynError>` returns `Err` with no count, where C
  returns `(asynError, *nbytesTransfered=partial)`. This matches the read-side
  contract (a read error carries no partial count either), the base
  `write_with_retry` already discards its partial `offset` on the error
  return, and a write error alarms the record regardless; carrying a partial
  count on error would require an `AsynError`-restructure that breaks the
  read/write symmetry. Workspace clippy `--all-targets` clean; full-workspace
  nextest (7311) + doctests green. Both `write_octet -> ()` findings closed.

- **DRV-23** (F5-remaining — UDP read drops `ASYN_EOM_END` at the datagram
  boundary) — CLEARED. C `readIt` (drvAsynIPServerPort.c:201-207) treats a UDP
  datagram as a message boundary: `ASYN_EOM_END` once the datagram is fully
  drained, `ASYN_EOM_CNT` when the caller buffer is too small and more of the
  datagram remains. The Rust UDP path had no `io_read_octet_eom` override, so
  the default actor synthesis (port.rs:1180) reported CNT-only and never END —
  the EOS interpose and `asynRecord::EOMR` never saw the boundary. Added an
  `io_read_octet_eom` override on `DrvAsynIPServerPort`: UDP routes through a
  new single-owner `udp_drain_into_eom` (drain + END/CNT decision; the old
  `udp_drain_into` now delegates, count-only) reporting END on full drain / CNT
  on a buffer-limited drain / `(0, empty)` on an empty-cache poll; TCP forwards
  the slot read's real `eom_reason` from `base_read_octet`. The C off-by-one
  (`maxchars - 1` copy with `+= maxchars` advance) is intentionally not
  reproduced (not C-bug copying). Self-review refinement (commit follows): END
  and CNT are modelled as two INDEPENDENT conditions (END = datagram fully
  drained, CNT = caller buffer filled), so an exact-fit datagram reports
  `END|CNT`. C's `:235` CNT branch is dead ONLY because the off-by-one short-
  reads by one byte and never fills the buffer; with the off-by-one removed the
  buffer-filled condition is live, and this also makes the rule uniform with the
  prologix `read_eom` exact-fit = both behaviour. DISTINCT, not
  fixed: serial_port.rs (C drvAsynSerialPort.c:974-977 sets CNT-only, no native
  END — byte stream, default synthesis is C-faithful) and the ip_server TCP
  subport (byte stream, base behaviour == default synthesis). Regression test
  `udp_server_read_eom_reports_end_at_datagram_boundary`. asyn-rs clippy
  `--all-targets` + nextest (658) green.

- **DRV-47** (F5-remaining — prologix read drops eomReason END/EOS/CNT) —
  CLEARED. C `readIt` (drvPrologixGPIB.c:334-349) buffers the whole device
  message, serves it in caller-sized chunks, and returns `eomReason` per chunk:
  the final chunk (caller buffer holds the rest) is `ASYN_EOM_EOS` when an EOS
  char is configured else `ASYN_EOM_END` (binary/EOI), a buffer-limited chunk
  is `ASYN_EOM_CNT`, and an exact fit sets both. The Rust prologix had no
  `io_read_octet_eom` override, so the default actor synthesis reported CNT-only
  and lost the GPIB EOI/EOS boundary. `io_read_octet_eom` is now the single
  owner of the read path (the old `read_octet` body moved into it; `read_octet`
  delegates and discards the EOM); both serve points (staged `read_carry`
  remainder and a fresh bridge read) compute the EOM through one free
  `read_eom(remaining, maxchars, eos_set)` helper that encodes the C rule.
  Tests: `read_eom_rule_matches_c_readit` (per-boundary unit test of the rule),
  `read_eom_carry_path_reports_boundary` (carry-drain CNT→END), plus END/EOS
  assertions added to the two end-to-end read tests. asyn-rs clippy
  `--all-targets` + nextest (660) green.

- **F7** (default EOS interpose + iocsh configure registrar) — DONE
  2026-06-29, 5 commits. Root cause found by reading C+Rust directly: EOS
  was fully non-functional in production. `setInputEos`/`setOutputEos` wrote
  `PortDriverBase::{input,output}_eos`, but the only EOS-matching layer
  (`EosInterpose`) reads its own config and was never installed outside
  tests, and the native read path falls through an empty interpose stack to
  the raw socket. So IEOS/OEOS on any Rust ip/serial port did nothing.
  - *a0c80be5 (enabler):* added `set_input_eos`/`set_output_eos` to the
    `OctetInterpose` trait (default no-op) + `OctetInterposeStack` (forward
    to every layer); `EosInterpose` overrides them + gained `Default`;
    `PortDriver::set_input_eos`/`set_output_eos` now cache in base AND
    forward to `base.interpose_octet` (one write owner), so runtime EOS and
    the binary-suppress save/restore both reach an installed interpose.
    C routes `setInputEos` down the interpose chain the same way.
  - *8d228d1a (DRV-2, ip):* `drvAsynIPPortConfigure` installs
    `EosInterpose::default()` unless `noProcessEos` (C
    `drvAsynIPPort.c:1065-1066`), via shared `build_configured_ip_port`.
    prologix's inner `_TCP` transport uses `DrvAsynIPPort::new` directly, so
    it correctly does not auto-install.
  - *7115a8fb (DRV-45, serial):* added `DrvAsynSerialPort::configure` (EOS by
    default unless `noProcessEos`, honors `noAutoConnect`) mirroring ftdi;
    `new` stays parse-only. C `drvAsynSerialPort.c:1126`.
  - *d7da13df (ftdi):* `configure` now installs `EosInterpose::default()`
    unless `no_process_eos` (C `drvAsynFTDIPort.cpp:616,622-623`); the stale
    "no octet-stack counterpart yet" comment removed.
  - *2ecb546c (DRV-45, serial iocsh):* added `drvAsynSerialPortConfigure`
    iocsh command (mirrors IP; registered on both registration paths);
    generalized `keep_ip_port_runtime` → `keep_port_runtime`.
  - Tests: enabler end-to-end (set IEOS via driver → interpose terminates
    read on EOS; clear IEOS → pass-through), per-driver install/suppress
    assertions (ip helper, serial configure, ftdi configure), serial iocsh
    registers-port. asyn-rs clippy `--all-targets` + nextest (665) green.
  - DISTINCT (C does not auto-install, not part of this family, still open):
    prologix DRV-46 (own in-driver `eos`, no interpose), DRV-49 (prologix
    iocsh command), ip_server (`drvAsynIPServerPort.c:659` `0,0,0`).

- **DRV-19** (CONCERN — ip_server bind host not resolved) — CLEARED
  e502ccfb. Both the TCP (`bind_with_options`) and UDP
  (`open_udp_listener`) paths parsed `host:port` with a bare
  `SocketAddr::parse`, which only accepts IP literals — empty host,
  `localhost`, and every DNS hostname were rejected, so a server could
  never bind a named interface. C `createServerSocket`
  (drvAsynIPServerPort.c:403-419) defaults the address to `INADDR_ANY`
  (:404) and overrides it only for a host that is non-empty AND not
  `localhost` (:412-413), resolving that name via `hostToIPAddr` (:414).
  C deliberately maps both empty and `localhost` to `INADDR_ANY` (its
  comment directs callers to `127.0.0.1` for loopback) — faithful parity,
  not a copied bug. Added one `resolve_bind_addr` owner shared by both
  paths: empty/`localhost` (case-insensitive) → `0.0.0.0`; IP literal →
  verbatim (preserves the explicit IPv4/IPv6 paths, no lookup); other
  name → resolved like the client driver. `bind_with_options` now takes
  the resolved `SocketAddr`. Tests:
  `resolve_bind_addr_maps_localhost_and_empty_to_inaddr_any`,
  `connect_localhost_named_host_binds`.
- **DRV-18** (CONCERN — UDP datagram-fanout SO_REUSEPORT not set) —
  CLEARED 4dc28663. The UDP server socket set only SO_REUSEADDR and a
  comment wrongly claimed upstream C never uses SO_REUSEPORT. C
  `createServerSocket` calls
  `epicsSocketEnableAddressUseForDatagramFanout(tty->fd)` for SOCK_DGRAM
  (drvAsynIPServerPort.c:426-429), which sets SO_REUSEPORT (where
  available) + SO_REUSEADDR so multiple IOCs can bind the same UDP port
  and the kernel fans each datagram out. The TCP listener keeps
  SO_REUSEADDR alone (:430) — the fanout helper is SOCK_DGRAM-only.
  Verified by reading `osdSockAddrReuse.cpp` (sets SO_REUSEPORT then
  SO_REUSEADDR). Set SO_REUSEPORT on the UDP socket before bind (unix);
  the "no SO_REUSEPORT *token*" doc note stays accurate (no config-string
  token; the option is enabled directly). Test (reads the option back off
  the bound socket): `reuse_port_set_for_udp_only_not_tcp`.
- **DRV-20** (CONCERN — UDP flush doesn't discard cached datagram) —
  CLEARED d890bafa. `DrvAsynIPServerPort` had no `io_flush` override, so
  the framework default no-op left the cached datagram in place — a
  flush-then-read re-returned the stale datagram. C registers `flushIt`
  only for the UDP server (drvAsynIPServerPort.c:655) and it resets
  `UDPbufferPos`/`UDPbufferSize` (flushIt:244-245). Added an `io_flush`
  override clearing `udp_cache` in UDP mode (which also lets the recv
  worker, refilling only when empty, fetch the next datagram). Test:
  `udp_server_flush_discards_cached_datagram`.
  - **TCP sibling** (9522b61b, found by adversarial caucus review of the
    UDP-only commit). The original commit wrongly claimed TCP server mode
    has no flush in C and left it a no-op. C serves each accepted
    connection through a full `drvAsynIPPort` child port
    (drvAsynIPServerPort.c:690) whose `flushIt` drains the socket
    (non-blocking `recv`-until-empty, drvAsynIPPort.c:846-861). Both Rust
    TCP data paths skipped it — the child `DrvAsynIPSubport` had no
    `io_flush` (default no-op) and the parent's addr-routed TCP `io_flush`
    was an explicit no-op — so a flush-then-read returned stale bytes.
    Added a single `ClientSlot::drain_input` owner (non-blocking
    recv-until-empty, blocking restored after, no-op on an unoccupied
    slot = C's `fd != INVALID_SOCKET` guard); routed the child flush and
    the parent's TCP flush (addressed slot, or every slot on broadcast
    `addr<0`) through it. Tests:
    `tcp_server_flush_drains_staged_socket_input`,
    `subport_flush_drains_staged_socket_input`,
    `tcp_server_flush_no_connection_is_harmless`. asyn-rs clippy
    `--all-targets` + nextest (673) green.

- **DRV-16 / DRV-17** (CONCERN — ip_server octet-interrupt push: new-TCP-
  connection name and UDP datagram bytes) — OPEN, surfaced for sign-off.
  This is the driver's documented purpose: C `drvAsynIPServerPort.c`
  issues an `asynOctet` *interface* interrupt to every `octetCallbackPvt`
  user with the new client's port name on TCP accept (:373-383) and with
  the datagram bytes on each UDP `recvfrom` (:311-320). asyn-rs models
  only **parameter** interrupts (`InterruptManager` /
  `InterruptValue{reason,…}`); there is no asynOctet-interface interrupt
  list, no `registerInterruptSource(octet)` analogue, and no consumer
  (devAsynOctet I/O Intr). Closing this is a new framework subsystem
  (interrupt source + push + a consuming device-support path), not a local
  driver fix — modelling only the push without a consumer would be fake
  parity. Deferred pending sign-off, same as F3/F8.

- **DRV-22** (LOW — `maxClients==0` coerced to 1) — CLEARED 526f3b17.
  `with_config` did `config.max_clients.max(1)`, silently accepting a
  useless zero-slot server. C `drvAsynIPServerPortConfigure` rejects
  `maxClients==0` with "No clients." and returns -1
  (drvAsynIPServerPort.c:545-548), unconditionally before the protocol is
  parsed. Now errors on `max_clients==0` (TCP and UDP). Test:
  `with_config_rejects_zero_max_clients`.
- **DRV-21** (LOW — listen backlog 128 not `maxClients`) — DOC, intentional
  divergence (no code change). C `listen(fd, maxClients)`
  (drvAsynIPServerPort.c:447) ties the kernel pending-connection queue to
  the slot cap; Rust uses a fixed backlog of 128 (≈ SOMAXCONN) because the
  slot cap bounds *concurrent accepted* clients, not the *pending* queue —
  a small backlog made third-party `connect()` block in tests. Rationale
  already in source (ip_server_port.rs `bind_with_options`).
- **DRV-24** (LOW — trailing tokens rejected) — DOC, intentional
  divergence (no code change). C `sscanf(":%u %5s", ...)`
  (drvAsynIPServerPort.c:582) reads the port + one protocol token and
  silently ignores trailing garbage; Rust rejects extra tokens to surface
  config typos. Not a C-bug copy (C's leniency is not a bug, but matching
  it would swallow typos); rationale added to source (`IpServerConfig::parse`).

- **DRV-11** (CONCERN — `timeout==0` socket timeout) — CLEARED a092a777.
  Two clauses, one defect family. (a) C `readRaw`/`writeRaw` floor
  `(int)(timeout*1000)==0` to a 1 ms poll (drvAsynIPPort.c:775-777 / 649-651);
  std rejects a zero `Duration` in `set_read_timeout`/`set_write_timeout`
  (`InvalidInput`), so a `timeout == 0` poll request hard-errored on the
  client read/write paths, and the server-mode reads skipped the setter
  (`if user.timeout > 0`) → blocked on the accept-time timeout instead of
  C's 1 ms poll. Added `socket_poll_timeout` (ip_port.rs) as the single owner
  of the asyn-timeout → socket-timeout mapping (zero → 1 ms; positive
  verbatim) and routed all 8 IP-family socket-timeout sites through it:
  ip_port read TCP/UDP/Unix + write TCP/UDP/Unix, ip_server_port base +
  subport reads (the divergent skip-guards removed). (b) Added the missing
  `timeout > 0` guard to the disconnect-on-read-timeout condition
  (C readRaw:799 `(disconnectOnReadTimeout) && (timeout > 0)`), so a
  zero-timeout poll that expires returns `asynTimeout` with the socket intact
  rather than torn down. Sub-ms positive timeouts pass through verbatim (Rust
  finer than C's whole-ms coarsening — strictly safer, documented aside); the
  negative "wait forever" timeout stays unrepresentable in the unsigned
  `Duration` (DRV-42, separate finding). Tests:
  `socket_poll_timeout_floors_zero_to_one_ms`,
  `zero_timeout_read_polls_without_disconnect`.
  CONVERGENCE bff0263d — the adversary review found the readRaw/writeRaw
  zero/edge-handling family was not yet closed by (a)/(b) above. Two more
  same-family sites fixed: (a') read-path EINTR teardown. C readRaw:798-800
  excludes BOTH `EWOULDBLOCK` *and* `EINTR` from the fatal disjunct, so a
  signal-interrupted read is a non-fatal timeout; the Rust read arms mapped
  only `TimedOut`/`WouldBlock` to `Timeout` and routed `Interrupted` into
  `Io(_)` (fatal → teardown of a healthy socket). Extracted
  `classify_read_error` as the single owner of read-error classification,
  mapping `Interrupted` onto the same non-fatal `Timeout` path as
  `WouldBlock`, and collapsed the three duplicated TCP/UDP/Unix Err arms.
  (b') zero-timeout write deadline defeated. C writeIt:649-651 floors the
  zero timeout to a 1 ms poll then attempts the send, so a `timeout == 0`
  write of a writable socket succeeds; the Rust write path floored
  `set_write_timeout` but built the retry deadline from the raw
  `user.timeout`, so the deadline was `now` and `write_with_retry`'s
  top-of-loop `now > deadline` check returned `Timeout` before ever calling
  `write()`. Floored the deadline with `socket_poll_timeout` too. Tests:
  `classify_read_error_eintr_and_wouldblock_are_nonfatal_timeout`,
  `zero_timeout_write_attempts_send_not_instant_timeout`.

- **DRV-7** (CONCERN — HTTP connect-per-transaction) — CLEARED 1629e3bb.
  Structural: `base.connected` conflated "logically connected" (exception /
  autoConnect state) with "has a live socket". C models HTTP via
  `FLAG_CONNECT_PER_TRANSACTION` — the response ends on the server's EOF
  (HTTP/1.0 close), the socket reopens lazily at the next write
  (`writeRaw` `connectIt` on `fd==INVALID`, drvAsynIPPort.c:590-606), and
  `closeConnection` suppresses `exceptionDisconnect` for cpt
  (drvAsynIPPort.c:214-216) so the Connect exception never flaps. Two bugs
  followed from the conflation: (1) read_octet_core dropped after EVERY HTTP
  read (`eof || Http`) → multi-chunk responses lost everything past chunk 1
  (next read reconnected to a fresh socket); (2) drop_connection set
  `connected=false` each transaction and the lazy reconnect gated on
  `!connected` → the Connect exception flapped per request. Fix separates the
  meanings: `drop_connection` is now the C `closeConnection` (HTTP releases
  only `io.inner`, keeps `connected` true; normal links still drop to false
  for the actor's auto-reconnect); EOF drops on `eof` only (removed the
  `|| Http`); both lazy reconnects gate on `io.inner.is_none()` not
  `!connected`, and `connect()` is edge-guarded so reopening no-ops
  `set_connected(true)` (no flap). Aside: Rust opens the HTTP socket eagerly
  at connect vs C's lazy-at-first-write — benign (idle socket reused by the
  first write). Test: `http_multi_segment_response_not_truncated`.

- **DRV-13** (LOW — hardcoded 5s connect timeout) — DOC, intentional
  divergence (no behavior change). C `connectIt` (drvAsynIPPort.c:513-523)
  does a plain blocking `connect()` (OS-default SYN timeout ~75-130s); Rust
  caps at 5s so the port actor fails fast into its 2s auto-reconnect cycle
  instead of parking ~75s inside `connect()` on an unreachable device. Not a
  C-bug copy (OS-default blocking isn't a bug); the trade-off (a device that
  genuinely needs >5s to accept — pathological on a control LAN — fails where
  C might eventually succeed) is documented at `DEFAULT_CONNECT_TIMEOUT`. The
  value is centralized into that const (was two magic `from_secs(5)`
  literals) and C exposes no connect-timeout option, so it is not
  runtime-settable.

- **DRV-55** (CONCERN — `maxchars==0` read guard unported) — CLEARED 75654e2b.
  C `readRaw` (drvAsynIPPort.c:736-740) rejects a `maxchars == 0` read
  request with `asynError` ("maxchars %d. Why <=0?") *after* the connect
  check and *before* touching the socket. The Rust IP reads had no guard: a
  `stream.read(&mut [])` returns `Ok(0)`, which the TCP read arm interprets
  as a peer EOF — the client read tears down the socket, the server reads
  clear the slot and announce a spurious disconnect. Added
  `maxchars_zero_error` (pub(crate), ip_port.rs) as the single owner of the
  empty-buffer rejection and applied `if buf.is_empty()` at all three IP
  octet read entry points (the C `readRaw` analogues): `IpIoState::read`,
  `DrvAsynIPServerPort::base_read_octet`, `DrvAsynIPSubport::read_octet` —
  each placed after the connect check and before the socket read, exactly as
  C orders it. Distinct/skipped: the flush drains (`Ok(0)=>break` is the
  drain's own end-of-data) and `write_octet`'s empty-data `Ok(0)` early
  return (zero-byte writes are valid; C `writeRaw` has no maxchars guard).
  Reachability through the framework's sized-buffer device support is
  unconfirmed, but the C guard is unconditional and the alternative (silent
  teardown of a live connection) is adverse, so the guard closes the parity
  gap regardless of caller. Test:
  `zero_length_read_request_rejected_not_eof_teardown` (empty buffer →
  asynError, socket stays connected). EXTENSION efe9ae2a — the drv-s3
  review round found a 4th IP read entry the first commit missed: the UDP
  server read bypasses `base_read_octet` (`read_octet`/`io_read_octet_eom`
  call `udp_drain_into[_eom]` directly, returning `Ok(0)` on an empty
  buffer). C's UDP read is a *separate* function, `readIt`
  (drvAsynIPServerPort.c:180-184), with its own `maxchars==0 → asynError`
  guard at the top (also what shields C from its own `(int)maxchars-1`
  underflow at :196). Added the guard to both UDP read entries, path-local
  so the maxchars-first order matches `readIt` (the TCP path keeps
  `readRaw`'s disconnect-first order in `base_read_octet`). Benign (cache
  poll-again, no teardown). Test: `udp_server_zero_length_read_rejected`.

- **DRV-56** (CONCERN — UDP recv worker dies on EINTR) — CLEARED 789ecc2a.
  The server-port UDP recv worker (`udp_recv_loop`) treated any recv error
  other than `WouldBlock`/`TimedOut` as fatal and broke out of the loop, so a
  single EINTR (a benign signal interrupting `recv`) permanently killed UDP
  reception for the port. C's UDP worker (drvAsynIPServerPort.c:308-323)
  assigns `UDPbufferSize = recvfrom(...)` with no error check at all — on
  EINTR it stores -1, fires the callback with a -1 size, and then never
  `recvfrom`s again (UDPbufferSize stuck at -1 routes every later iteration
  to the 1 ms sleep branch). That is a C bug (silent reception-death on a
  benign signal), not a contract to copy. Routed EINTR into the existing
  non-fatal `continue` arm via the shared `is_nonfatal_read_timeout` owner so
  the worker retries the recv after a signal; a genuine hard recv error still
  exits the thread cleanly (reception is dead either way — C would
  1 ms-busy-spin a worker that can never receive again). Distinct from
  DRV-11: different C function (recv worker, not `readRaw`) and a different
  consequence (worker-thread death, not read-status misclassification).

- **DRV-57** (DEFECT — serial `maxchars==0` guard, ADVERSE) — CLEARED
  e83cc61d. Serial-driver sibling of DRV-55, surfaced by the drv-s3
  adversarial reviewer. C `read` (drvAsynSerialPort.c:871-875) rejects
  `maxchars == 0` with `asynError` right after the fd check and before
  touching the device. `SerialIoState::read` had the fd check
  (`fd_or_err`) but no maxchars guard: an empty buffer reaches
  `libc::read(fd, ptr, 0)`, which returns 0, and the `n == 0` branch
  classifies that as a disconnect ("serial port EOF",
  `AsynStatus::Disconnected`) → `is_fatal_transport_error` →
  `drop_connection()` tears down a live serial port — the exact adverse
  DRV-55 consequence. Added the guard after the fd check, matching C order.
  Message uses the serial driver's own wording ("maxchars 0 Why <=0?", no
  period) per its C source rather than importing `ip_port::maxchars_zero
  _error` (whose ". Why" matches the IP drivers) — consistent with the
  DRV-31 decision to keep serial's read-error helpers local. Defect-family
  sweep (anchor = octet read entries where `maxchars==0` misreads):
  `SerialIoState::read` is the only same-defect site; serial write has no
  maxchars; the other `libc::read` calls are test code. Test:
  `pty_zero_length_read_rejected_not_eof_teardown`.
  PROLOGIX CLARIFICATION (drv-s4 review corrected my first rationale) —
  prologix `read_octet`/`io_read_octet_eom` do NOT inherit the inner
  `DrvAsynIPPort` guard: they read the bridge into their own 4096-byte
  `chunk` (prologix.rs:462), never the caller's `buf`, so an empty caller
  buffer never reaches the inner read. The empty-buf path returns
  `(0, eom)` with any device reply preserved in `read_carry` — benign (no
  syscall on the caller buf, no teardown, no data loss). There is no C
  guard to match either: C `drvPrologixGPIB.c` read (:240) has no
  `maxchars==0` guard, only buffer clamping (:336,:342-343), so
  `maxchars==0` → 0 bytes with no error — exactly the Rust behaviour. Not a
  family member; no change. ftdi/usbtmc/vxi11 have no octet-read impl and
  fall through to the default in-memory param-table `PortDriver::read_octet`
  (no fd/syscall/teardown) — also not members.

- **DRV-58** (CONCERN — write make-progress deadline vs C) — DOC,
  intentional divergence (no change). Raised by the drv-s3 adversarial
  reviewer. `write_with_retry` (ip_port.rs:239-271) checks `now > deadline`
  at the top of every loop iteration, so the deadline (write-start +
  `socket_poll_timeout(timeout)`) bounds the TOTAL write time. C `writeRaw`
  (drvAsynIPPort.c:631-674) sets `haveStartTime` on the FIRST EWOULDBLOCK
  only (line 631, never reset) and bounds time from there; while sends make
  progress without blocking it has no time bound at all. So on a large
  write to a slow/backpressured socket Rust can abort mid-write where C
  keeps going. Pre-existing (not introduced by the DRV-11(b) deadline
  floor), low severity, and at `timeout == 0` both behave nearly
  identically. Rust's total-time bound is stricter and safer (bounded write
  latency) — a defensible intentional divergence, not a C-bug copy, so left
  as-is and documented here.

- **Flush-drain EINTR (aside, no change)** — the drv-s3 parity reviewer
  noted the two flush drains handle EINTR differently: client
  `IpIoState::flush` (ip_port.rs:441/455/471) *continues* on `Interrupted`
  (more thorough than C), while server `ClientSlot::drain_input`
  (ip_server_port.rs:283/290) *breaks* on any error including EINTR
  (C-faithful — C `flushIt` drvAsynIPPort.c:853-857 is `if (numRecv <= 0)
  break`). Both are benign (DRV-20 flush family, already dispositioned);
  neither leaks bytes nor tears anything down. No defect — left unaligned.

- **DRV-32** (CONCERN — serial termios input flags) — CLEARED. FIXED
  2685bd6f. C `drvAsynSerialPort.c` (line 1080) seeds the default input
  flags `IGNBRK | IGNPAR`, so a line BREAK and framing/parity errors are
  silently dropped. The Rust `connect` configured the port with
  `cfmakeraw`, which clears `IGNBRK` (and `BRKINT`) and never sets
  `IGNPAR`, so a BREAK or a line error reached the reader as a spurious
  `0x00` byte where C delivers nothing. Set `t.c_iflag |= IGNBRK | IGNPAR`
  (serial_port.rs:663) after `cfmakeraw`, before the config layer.
  `apply_to_termios` only touches `c_iflag` for the XON/XOFF flow bits, so
  the flags survive it and coexist with `FlowControl::Software`'s
  `IXON|IXOFF`. Test: `pty_termios_sets_ignbrk_ignpar` (after connect,
  `get_current_termios` shows both flags set).

- **DRV-33** (CONCERN — serial XON/XOFF flow characters) — CLEARED. FIXED
  117e612e. C `drvAsynSerialPort.c` (lines 1085-1086) seeds the XON/XOFF
  flow characters `c_cc[VSTART]=0x11` (^Q) and `c_cc[VSTOP]=0x13` (^S). The
  Rust `connect` zeroes the termios struct before `cfmakeraw`, and
  `cfmakeraw` leaves `c_cc` untouched, so `VSTART`/`VSTOP` stayed `0`. With
  `FlowControl::Software` (IXON|IXOFF) the kernel would then drive flow
  control with NUL bytes instead of ^Q/^S, breaking interop with any peer
  expecting the standard control characters. Set `t.c_cc[VSTART]=0x11` and
  `t.c_cc[VSTOP]=0x13` (serial_port.rs:672-673) after the VMIN/VTIME seed,
  before the config layer; `apply_to_termios` only toggles the
  IXON/IXOFF/IXANY bits in `c_iflag`, never `c_cc`, so the seeded
  characters survive it. Test: `pty_termios_sets_xon_xoff_chars` (after
  connect, `get_current_termios` shows VSTART==0x11 and VSTOP==0x13).

- **DRV-59** (DOC — serial VMIN default vs C) — DOC, intentional
  architecture-coupled divergence (documented in source, no behavioral
  change). Found during the DRV-32/33 termios-default family sweep: it is
  the one remaining sibling of the C default-termios block
  (drvAsynSerialPort.c:1077-1089) that Rust does not match. C seeds
  `c_cc[VMIN]=0` (line 1083) and then *reprograms* VMIN/VTIME on every read
  from `pasynUser->timeout` (`readIt`, lines 899-908): `timeout>0` →
  VMIN=0/VTIME=(timeout*10)+1 capped 255, `timeout==0` → VMIN=0/VTIME=0 +
  `O_NONBLOCK`, `timeout<0` → VMIN=1/VTIME=0. So C uses VMIN=0 for every
  representable (non-negative) timeout and lets VTIME + an epicsTimer
  govern the wait. The Rust driver uses a *different read architecture*:
  every read is gated by `poll(POLLIN, timeout)` (serial_port.rs:341-361)
  on a blocking fd, and only calls `read()` when poll signals readable.
  With that gate, a static `VMIN=1` keeps the invariant `n == 0 ⟺
  EOF/hangup` clean — `read()` returns ≥1 byte when data is present (the
  normal post-poll case) and 0 only on a genuine hangup. Adopting C's
  `VMIN=0` would re-introduce a dual meaning for `n == 0` (EOF *or*
  spurious poll-wake with no data), turning a benign wake into a false
  Disconnect+teardown. C's `VMIN=0` is coupled to C's VTIME-timer design,
  not portable into the poll-gated design. The remaining termios defaults
  all match C: `c_cflag` CS8|CLOCAL|CREAD (cfmakeraw CS8 + explicit
  CREAD|CLOCAL + 8N1 config default), `c_oflag`/`c_lflag` 0 (zeroed +
  cfmakeraw), VTIME 0, baud 9600 — only VMIN diverges, by design. Left
  as-is, divergence documented at serial_port.rs:665-673.

- **DRV-35** (CONCERN — serial setOption cached-config dual state) —
  CLEARED. FIXED fdf7d488. C `setOption` (drvAsynSerialPort.c:601-604)
  saves `baudPrev`/`termiosPrev` and restores them if `applyOptions`
  fails, so `getOption` never reports a value the device rejected. The
  Rust `set_option` committed `self.config.X` *before* the fallible
  `apply_termios()?` at five arms (baud, bits, parity, stop, crtscts), so
  an apply failure (tcgetattr/tcsetattr error) left the cached config
  diverged from the device — a subsequent `get_option` returned a
  never-applied value. Structural fix: move the `self.config.X` commit to
  *after* the apply block in all five arms, so the cached value updates
  only on the success path (or when the port is not open yet — applied at
  the next connect). On any failure the cached config is untouched by
  construction; no rollback window, no dual meaning. clocal/ixon/ixoff/
  ixany do not persist to `self.config` (distinct, not members); break/
  rs485 take distinct paths. Test:
  `set_option_does_not_commit_cached_config_on_apply_failure` (non-tty fd
  → tcgetattr ENOTTY → apply fails → cached baud stays 9600).

- **DRV-37** (CONCERN — serial write timeout per-poll, not total) —
  CLEARED. FIXED 92755a0f. C `writeIt` (drvAsynSerialPort.c:815-842) arms
  one timer for the whole `writeTimeout` before the loop and breaks when it
  fires (`timeoutFlag`, :827), so the timeout bounds the TOTAL write. The
  Rust write was unbounded two ways: (1) it reused the full per-call
  `timeout_ms` on every `poll`, so a slowly-draining peer could keep a
  multi-chunk write alive for up to timeout×iterations; (2) the driver fd
  is blocking (connect clears O_NONBLOCK so reads block on poll), so
  `write(fd, all_remaining)` blocked in-kernel until the *entire* buffer
  was accepted — a stalled peer blocked the write far past the timeout and
  the poll never timed it out. C unblocks a stuck write from its timer via
  `tcflush(TCOFLUSH)` (:649); this driver has no timer. Fix: one `deadline`
  + poll with the remaining budget each iteration + a post-write deadline
  check (mirrors :827), and run the loop with the fd temporarily
  non-blocking so each `write` returns immediately with what fit (or
  EAGAIN) instead of blocking on the whole payload; blocking mode restored
  on every exit path. `timeout==0` collapses to a single write attempt then
  bail (matches `writeTimeout==0`). This is localized to `write()` — the
  read path and the DRV-59 VMIN analysis are untouched. Mechanism diverges
  from C (non-blocking poll loop vs blocking + timer/flush) but the
  end-behavior — a write bounded by the timeout — matches. Test:
  `pty_write_timeout_bounds_total_not_per_poll` (slow drain 4 KiB/10 ms;
  512 KiB payload; per-poll behavior runs to completion ~1.3 s, the fix
  times out at ~300 ms).

- **DRV-39** (LOW — serial getOption("break")) — CLEARED. FIXED dbb9a127.
  C `getOption` (drvAsynSerialPort.c:204-207) reports the "break" key as
  the literal "off" — a line break is a momentary action so a read always
  returns off. The Rust `get_option` had no "break" arm and fell through to
  the generic store lookup → `OptionNotFound`. Added the arm returning
  "off". Test: `get_option_break_returns_off`.

- **DRV-41** (LOW — serial connect FD_CLOEXEC + tcflush) — CLEARED. FIXED
  77930ec3. C `connectIt` hardens a freshly-opened port two ways the Rust
  connect omitted: (1) FD_CLOEXEC (drvAsynSerialPort.c:713-722) set right
  after open so a later child process (e.g. an iocsh `system` call) does
  not inherit the serial fd and hold the device open after this driver
  closes it — set via `fcntl(F_SETFD, FD_CLOEXEC)` at the top of the
  connect setup closure, so a failure routes through the existing
  fd-closing error handler; (2) `tcflush(TCIOFLUSH)` (drvAsynSerialPort.c:
  729) to discard bytes left in the kernel input/output buffers before the
  port was configured, so the first read/write starts clean — done right
  before restoring blocking mode, matching C's order. Test:
  `pty_connect_sets_cloexec` (F_GETFD shows FD_CLOEXEC after connect; the
  tcflush is on the same connect path, not separately asserted).

- **DRV-43** (LOW — serial "break" set when disconnected) — CLEARED. FIXED
  1d3db686. C `setOption` "break" (drvAsynSerialPort.c:507-528) validates
  the value then calls `tcdrain`/`tcsendbreak` WITHOUT guarding on the fd,
  so a break on a closed port fails (`tcsendbreak` EBADF → asynError). The
  Rust arm gated the whole action behind `else if let Some(fd) =
  self.io.fd`, so a break on a disconnected port silently returned Ok.
  Restructured to C's order: "off" is a no-op; otherwise validate the
  duration first ("" / "on" → 0, a number → that duration, else error),
  then require the fd via `fd_or_err()` (Disconnected error) before
  `tcdrain` + `tcsendbreak`. A bad duration is now rejected even when
  disconnected. The `maxchars==0` half of DRV-43 was already closed by
  DRV-57; this closes the "disconnected break silently Ok" half. Test:
  `set_option_break_on_disconnected_errors_but_off_is_noop`.

- **DRV-44** (LOW — serial report() diagnostics) — CLEARED. FIXED
  64a5d4c0. C `report` (drvAsynSerialPort.c:666-680) prints the connection
  state and, at details>=1, the fd plus cumulative characters written
  (nWritten) and read (nRead). The Rust serial driver used the generic
  default `report()` and tracked no byte counters. Added `n_read`/
  `n_written` to `SerialIoState`, incremented per successful chunk
  (mirroring C's `tty->nRead`/`nWritten += this{Read,Write}`, so partial
  bytes before a timeout count too), and overrode `report()` to print the
  "Serial line <dev>: Connected/Disconnected" header with fd + the two
  counters at details>=1. Test: `pty_report_tracks_byte_counters`.

- **DRV-42** (LOW — "wait forever" negative timeout) — DOC, intentional
  framework-wide divergence (documented at the `AsynUser.timeout` field, no
  behavioral change). C asyn's `pasynUser->timeout` is a `double` where a
  negative value means "wait forever" (read VMIN=1 with no timer, write no
  timer — drvAsynSerialPort.c:906-909). The Rust framework's
  `AsynUser.timeout` is an unsigned `Duration`, so that sentinel is
  unrepresentable and every blocking driver operation is supplied a finite
  timeout. This is a deliberate, safer default — a stuck device cannot
  wedge the port actor thread indefinitely — and it is the framework root
  of the DRV-59 "every representable (non-negative) timeout" note. Left
  as-is; documented in `crates/asyn-rs/src/user.rs`.

- **DRV-34** (CONCERN — baud range narrower than C) — CLEARED. FIXED
  7ef497b5. C `setOption` "baud" (drvAsynSerialPort.c:271-345) sets the
  termios speed platform-conditionally: where the `Bxxx` constants equal the
  literal rate (macOS/BSD, `B9600 == 9600`) it uses the baud value directly
  as the speed code (`baudCode = baud`, :273-274) so any rate is settable;
  on Linux (encoded codes) it maps a fixed `switch` set and returns
  asynError "Unsupported data rate" (:340-343) for the rest (`#ifdef B28800`
  is false on Linux, so 28800 is unsupported there in C too). The Rust port
  used one fixed table on every platform with a silent `_ => B9600`
  fallback, and the set_option gate (`is_supported_baud` over
  `SUPPORTED_BAUDS`) listed only 0..230400 — narrower than C on macOS/BSD (no
  arbitrary rate; 28800/250000 rejected) and narrower than the table itself
  on Linux (460800..4000000 mappable yet rejected). An unmapped rate that
  passed the gate was silently coerced to 9600. `baud_to_speed` now returns
  `Option<speed_t>` and is the single source of truth: arbitrary passthrough
  on macOS/BSD, the mapped standard set (incl. the high Linux rates)
  otherwise, `None` for an unknown rate. `set_option` derives validation and
  the speed lookup from one `ok_or` so they cannot disagree; `SUPPORTED_BAUDS`
  and the redundant `is_supported_baud` helper are removed; `apply_to_termios`
  falls back to 9600 only for a `SerialConfig` built directly with an
  unmappable rate. Tests: `baud_arbitrary_on_bsd_mapped_set_on_linux` plus
  the platform-aware `test_set_option_unsupported_baud`.
  - **Round drv-s5 follow-up (DEFECT, FIXED 34b22ebc):** the rewrite kept a
    `0 => libc::B0` arm in the Linux branch, so `setOption("baud","0")` on Linux
    succeeded and programmed B0 (line hangup) where C returns asynError — C's
    Linux switch starts at `case 50` with no `case 0` (drvAsynSerialPort.c:276-344).
    Dropped the arm so 0 falls to `_ => return None` on Linux; macOS/BSD still
    accept 0 via the literal passthrough (C-macOS `baudCode = baud = 0`). Tests
    extended (baud 0: Some on macOS/BSD, None on Linux; 0 removed from the
    roundtrip set).
  - **Round drv-s5 CONCERN (residual, NOT a C-parity divergence) — FIXED
    da0e0917.** The silent 9600 fallback was closed for the driver path (the
    only writers of `config.baud` are `SerialConfig::parse` → always 9600 and
    `set_option` → only after `baud_to_speed == Some`; `apply_to_termios` has
    one caller, `connect`, on the private `config`), but `SerialConfig` is `pub`
    with a `pub baud` field and `pub fn apply_to_termios`, so an external caller
    could build `SerialConfig { baud: 28800, .. }` on Linux and call
    `.apply_to_termios()` → silent B9600. No C equivalent (C has no public
    config-apply), so not a wire divergence; both review panels marked it
    non-blocking. Closed structurally per user sign-off: `apply_to_termios` now
    returns `AsynResult<()>` and errors (via `baud_to_speed`'s `ok_or`) instead
    of the silent fallback — an unmappable rate cannot be applied even through a
    directly built `SerialConfig`. Breaking public-API change (approved). The
    one caller, `connect`, propagates with `?`. Test:
    `apply_to_termios_errors_on_unmappable_baud`.

- **DRV-31** (DEFECT — hard read/write error + EOF no reconnect; no EINTR/EAGAIN
  retry) — CLEARED (already resolved by later work; verified + write-test added
  e8b282af). Ground-verified against current code rather than re-ported (the
  inventory row was stale): the fix had landed across the read-EINTR/EOF work
  and the DRV-37 write rewrite. `read_octet` (serial_port.rs:868-901) and
  `write_octet` (:903-934) both route a fatal error through
  `is_fatal_transport_error` (:566-574, `Disconnected | Io(_)`) →
  `drop_connection` (:583-589: close fd, fd=None, set_connected(false)), the
  closeConnection analogue C calls in `readIt`/`writeIt`
  (drvAsynSerialPort.c:837,959) — so the actor's auto-reconnect re-opens the
  device on the next request. EINTR/EWOULDBLOCK/EAGAIN are retried *inside*
  `SerialIoState::read` (:386-435) and `::write` (:468-530), matching C's errno
  exclusions, so anything reaching the wrapper is genuinely fatal. The read side
  was already covered by `test_pty_read_error_disconnects`; added the symmetric
  `test_pty_write_error_disconnects` for the write side DRV-31 also names. (The
  read `n == 0` → `Disconnected "serial port EOF"` nuance vs C's loop-until-
  timeout belongs to the DRV-57 family, already dispositioned, not DRV-31.)
