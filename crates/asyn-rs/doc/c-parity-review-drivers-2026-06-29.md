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
| DRV-1 | DEFECT | ip_port.rs:596-638 | drvAsynIPPort.c:513-523,656,775-789 | UDP socket is `connect()`-ed → inbound datagrams filtered to one peer; `udp*` broadcast replies dropped; SO_BROADCAST set after connect |
| DRV-2 | DEFECT | iocsh.rs:441-505, ip_port.rs:471-493 | drvAsynIPPort.c:1065-1066 | EOS interpose never auto-installed; C installs `asynInterposeEos` by default → IEOS/OEOS dead on a fresh port |
| DRV-3 | CONCERN | ip_port.rs:266-269 | drvAsynIPPort.c:814-831 | TCP EOF returns Disconnected error; C returns success+`ASYN_EOM_END`; END bit never emitted anywhere |
| DRV-4 | DEFECT | ip_port.rs:296-299,782-804 | drvAsynIPPort.c:815 | empty UDP datagram treated as EOF → tears down socket (legit zero-len datagram kills the port) |
| DRV-5 | DEFECT | ip_port.rs:810-828,357-379 | drvAsynIPPort.c:692-699 | write errors never close connection (asymmetric w/ read path); port wedges, no reconnect |
| DRV-6 | CONCERN | ip_port.rs:121-138,32-51 | drvAsynIPPort.c:364-367,1061-1064 | `COM` protocol suffix unsupported → silently downgraded to plain TCP, RFC 2217 interpose omitted |
| DRV-7 | CONCERN | ip_port.rs:767-771,711-718 | drvAsynIPPort.c:214-217,557-558,815-819 | HTTP per-transaction disconnects after each read → truncates multi-segment responses + flaps Connect exception |
| DRV-8 | CONCERN | ip_port.rs:861-866,665-667 | drvAsynIPPort.c:525-534,915-947 | `noDelay` is a non-C option key (C has none, sets TCP_NODELAY unconditionally) |
| DRV-9 | CONCERN | ip_port.rs:916-945 | drvAsynIPPort.c:902-906,941-945 | setOption/getOption accept+echo arbitrary unknown keys; C closes dispatch (asynError) |
| DRV-10 | LOW | ip_port.rs:867-869 | drvAsynIPPort.c:924-935 | `disconnectOnReadTimeout` parse accepts extra values, never errors (C: only Y/N) |
| DRV-11 | CONCERN | ip_port.rs:264,792-794 | drvAsynIPPort.c:741-743,799 | `timeout==0` read → Duration::ZERO rejected by std → misclassified, disconnects (C floors to 1ms poll); missing `timeout>0` disconnect guard |
| DRV-12 | LOW | ip_port.rs:661-731 | drvAsynIPPort.c:424-427 | `connect()` doesn't reject already-open link (C: "Link already open!") |
| DRV-13 | LOW | ip_port.rs:92,109,553,575 | drvAsynIPPort.c:513-523 | hardcoded 5s connect timeout where C connect is OS-default blocking |
| DRV-14 | LOW | ip_port.rs:368-371 | drvAsynIPPort.c:613-614 | zero-length write emits an empty UDP datagram; C returns before sending |
| DRV-15 | LOW | ip_port.rs:810-828, port.rs:979 | drvAsynIPPort.c:678-705 | partial-write byte count dropped on error (framework `write_octet -> ()` contract) |

### Category B — `ip_server_port.rs` ↔ `drvAsynIPServerPort.c`

| id | sev | Rust | C | one-line |
|---|---|---|---|---|
| DRV-16 | CONCERN | ip_server_port.rs:529-535 | drvAsynIPServerPort.c:373-383 | new-TCP-connection octet-interrupt callback never delivered (the driver's documented purpose) |
| DRV-17 | CONCERN | ip_server_port.rs:887-893,683-689 | drvAsynIPServerPort.c:311-320 | UDP datagram octet-interrupt push delivery missing → I/O Intr record never fires |
| DRV-18 | CONCERN | ip_server_port.rs:408-415 | drvAsynIPServerPort.c:426-429 | UDP datagram-fanout SO_REUSEPORT not set; comment wrongly asserts parity |
| DRV-19 | CONCERN | ip_server_port.rs:451-457,391-397 | drvAsynIPServerPort.c:403-419 | host/iface not resolved — bare `SocketAddr::parse` rejects `localhost`/hostnames/empty-host (client driver resolves correctly) |
| DRV-20 | CONCERN | ip_server_port.rs (no io_flush) | drvAsynIPServerPort.c:240-247,655 | UDP `flush` doesn't discard cached datagram → stale datagram on flush-then-read |
| DRV-21 | LOW | ip_server_port.rs:486 | drvAsynIPServerPort.c:447 | listen backlog hardcoded 128 not `maxClients` (intentional/documented) |
| DRV-22 | LOW | ip_server_port.rs:328 | drvAsynIPServerPort.c:545-548 | `maxClients==0` coerced to 1 instead of rejected |
| DRV-23 | LOW | ip_server_port.rs:683-689,823-836 | drvAsynIPServerPort.c:201-207,232-236 | UDP read drops `ASYN_EOM_END` at datagram boundary |
| DRV-24 | LOW | ip_server_port.rs:163-168 | drvAsynIPServerPort.c:582 | trailing tokens after protocol rejected (Rust stricter than C sscanf) |
| DRV-25 | NON-GAP | ip_server_port.rs:638-645 | drvAsynIPServerPort.c:462-483 | Rust returns bind error; C reports success on failed bind (C bug not reproduced) |
| DRV-26 | NON-GAP | ip_server_port.rs:823-836 | drvAsynIPServerPort.c:196-200 | C UDP readIt off-by-one (copy maxchars-1, advance maxchars) not reproduced |

### Category C — `serial_port.rs` ↔ `drvAsynSerialPort.c`

| id | sev | Rust | C | one-line |
|---|---|---|---|---|
| DRV-31 | DEFECT | serial_port.rs:336-344,387-389 | drvAsynSerialPort.c:837,959,625-635 | hard read/write error + EOF never closes connection → no auto-reconnect; also no EINTR/EAGAIN retry |
| DRV-32 | CONCERN | serial_port.rs:542-549 | drvAsynSerialPort.c:1080 | termios input flags: C `IGNBRK\|IGNPAR` vs Rust `cfmakeraw` → spurious 0x00 on BREAK/line errors |
| DRV-33 | CONCERN | serial_port.rs:542-548,151-154 | drvAsynSerialPort.c:1085-1086 | VSTART/VSTOP never set → XON/XOFF software flow control broken (NUL not ^Q/^S) |
| DRV-34 | CONCERN | serial_port.rs:263-270,645-658,159-208 | drvAsynSerialPort.c:271-345 | baud set narrower than C (no arbitrary baud on macOS/BSD; 28800 missing) |
| DRV-35 | CONCERN | serial_port.rs:659-747 | drvAsynSerialPort.c:254-256,599-605 | setOption mutates cached config before apply, no rollback on failure → get reports never-applied value |
| DRV-36 | CONCERN | serial_port.rs:846-848 | drvAsynSerialPort.c:594-598 | unknown option keys silently accepted (C: asynError); empty-key re-apply not honored |
| DRV-37 | CONCERN | serial_port.rs:357-391 | drvAsynSerialPort.c:810-842 | write timeout applied per-poll-iteration, not single total deadline → write lives timeout×N |
| DRV-38 | CONCERN | serial_port.rs:373-378,621-634 | drvAsynSerialPort.c:843 | partial byte count dropped on write timeout/error (framework `write_octet -> ()` contract) |
| DRV-39 | LOW | serial_port.rs:853-955 | drvAsynSerialPort.c:203-208 | getOption("break") errors instead of returning "off" |
| DRV-40 | LOW | serial_port.rs:514-531 | drvAsynSerialPort.c:694-698 | connect() not guarded against double-open (fd + saved_termios leak) |
| DRV-41 | LOW | serial_port.rs:522-560 | drvAsynSerialPort.c:713-729 | connect() omits FD_CLOEXEC and connect-time tcflush(TCIOFLUSH) |
| DRV-42 | LOW | user.rs:16, serial_port.rs:309-333 | drvAsynSerialPort.c:906-909 | "wait-forever" (negative) timeout unrepresentable (framework `Duration` is unsigned) |
| DRV-43 | LOW | serial_port.rs:817-836,314-344 | drvAsynSerialPort.c:519-526,871-875 | disconnected `break` silently Ok; `maxchars==0` read misclassified as Disconnected |
| DRV-44 | LOW | serial_port.rs (no report) | drvAsynSerialPort.c:666-680 | report() lacks serial diagnostics (fd, nWritten, nRead) |
| DRV-45 | CONCERN | serial_port.rs:423-448 | drvAsynSerialPort.c:1032-1175 | no drvAsynSerialPortConfigure registrar; default EOS not installed; no priority/noAutoConnect knob |

### Category D — `prologix.rs` ↔ `drvPrologixGPIB.c`

| id | sev | Rust | C | one-line |
|---|---|---|---|---|
| DRV-46 | CONCERN | prologix.rs:169 | drvPrologixGPIB.c:439,422,534-535 | EOS interface not wired to driver `eos` state; getInputEos echoes stored-but-ineffective bytes (C reports eoslen=0) |
| DRV-47 | CONCERN | prologix.rs:361 | drvPrologixGPIB.c:334-349 | eomReason (END/EOS/CNT) never reported (no io_read_octet_eom override) |
| DRV-48 | DEFECT | prologix.rs:346-359 | drvPrologixGPIB.c:386,409 | write_octet doesn't discard staged read remainder → next read returns stale bytes (cross-transaction leak) |
| DRV-49 | CONCERN | prologix.rs:108, iocsh.rs:446 | drvPrologixGPIB.c:547-628 | no iocsh `prologixGPIBConfigure` command; `priority` dropped; unreachable from st.cmd |
| DRV-50 | CONCERN | prologix.rs, interfaces/gpib.rs:58 | drvPrologixGPIB.c:461-525 | GPIB command interface absent (ifc/srqStatus real in C; AsynGpib has zero implementors) — scaffold |
| DRV-51 | LOW | prologix.rs:272-324 | drvPrologixGPIB.c:166-168 | connect() doesn't clear `read_carry` (reconnect-without-disconnect returns stale bytes) |
| DRV-52 | LOW | prologix.rs:322,333 | drvPrologixGPIB.c:213,231 | per-device connect/disconnect toggles port-level state + announces addr −1 (ASYN_MULTIDEVICE) |
| DRV-53 | LOW | prologix.rs:131-135 | drvPrologixGPIB.c:592-593 | port registered `destructible:true`; C registers no ASYN_DESTRUCTIBLE (over-grants shutdown rights) |
| DRV-54 | LOW | prologix.rs:396-400 | drvPrologixGPIB.c:253 | zero read timeout coerced to 1s (C passes 0 = poll verbatim) |

NON-GAPs / parity-clean (Rust correctly declines C bugs or improves): ip_port
ASIDES (IPv6 superset, null-term, DNS caching); ip_server DRV-25/26; serial
termios-restore-on-disconnect, POLLHUP busy-loop declined, value leniency;
prologix ASIDE-A/B/C + the full verified-wire-exact connect-burst / addressing /
escaping list. See round report.

## Review Log

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
- **F7 — default EOS interpose + iocsh configure registrar:** DRV-2 (ip),
  DRV-45 (serial), DRV-46/DRV-49 (prologix). **Larger architecture — how ports
  auto-install EOS and register iocsh commands. Needs design sign-off.**
- **F8 — partial nbytesTransfered on error (framework `write_octet -> ()`):**
  DRV-15 (ip), DRV-38 (serial). **Framework-wide trait-signature change. Needs
  sign-off.**
- **F9 — ip_server interrupt push delivery (driver's documented purpose):**
  DRV-16, DRV-17. Plus DRV-18/19/20.
- Singletons: DRV-6/7/11/13 (ip), DRV-32/33/34/35/37/39/41/42/43/44 (serial),
  DRV-50/52/53/54 (prologix), DRV-21/22/24 (ip_server).

Fix-phase plan: start with F1 (DEFECT, local, established invariant), then
F4/F5/F6 (local, clear contracts), then per-driver singletons. F7 (EOS+iocsh
architecture), F8 (write_octet count contract), and F3 (UDP redesign) are
surfaced for sign-off before implementation because they change framework
contracts / behavioral models, not just a driver site.

