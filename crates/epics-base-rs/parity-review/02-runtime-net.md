# Parity Review 02 — runtime/ and net/

Rust port of EPICS base `libcom` runtime + networking, reviewed against
`/Users/stevek/codes/epics-base`.

Scope reviewed: `src/runtime/{env,general_time,log,net,supervise,sync,task,time,mod}.rs`,
`src/net/{async_udp_v4,iface_map,loopback_mcast,mod}.rs`.

---

## Severity counts

| Severity | Count |
|----------|-------|
| Critical | 0 |
| High     | 4 |
| Medium   | 6 |
| Low      | 5 |

---

## HIGH

### H1 — `env::get_bool` accepts values C rejects, and is case-sensitive for "yes"
- **Rust:** `src/runtime/env.rs:16-21`
- **C:** `modules/libcom/src/env/envSubr.c:324-333` `envGetBoolConfigParam`
- **Diverges:** C's `envGetBoolConfigParam` does `*pBool = epicsStrCaseCmp(text, "yes")==0;`
  — *only* the string `yes` (case-insensitive) is true, everything else
  (`"1"`, `"true"`, `"on"`, `"YES "` with trailing space, …) is false.
  The Rust `get_bool` returns true for `matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "YES")`.
  Two separate bugs:
  1. Rust accepts `"1"`, `"true"`, `"TRUE"` as true — C does not.
  2. Rust's `"yes"`/`"YES"` match is case-*sensitive* (literal alternatives),
     so `Yes`, `yEs`, `yes\n` are false in Rust but `Yes`/`yEs` are **true**
     in C (`epicsStrCaseCmp`).
- **Impact:** A startup script that sets `EPICS_CA_AUTO_ADDR_LIST=Yes` (a
  common operator habit) is honored by a C IOC but ignored by the Rust
  port — auto address-list discovery silently disabled. Conversely a var
  set to `1` is treated as enabled by Rust but disabled by C. Wrong
  feature gating on real deployments. Note `EPICS_CA_AUTO_ADDR_LIST`,
  `EPICS_CA_AUTO_ARRAY_BYTES`, `EPICS_ALLOW_POSIX_THREAD_PRIORITY_SCHEDULING`
  all default to `YES` in `configure/CONFIG_ENV` and are boolean params.

### H2 — Port env-vars are not range-checked against `IPPORT_USERRESERVED`
- **Rust:** `src/runtime/net.rs:21-49` (`ca_server_port`, `cas_server_port`,
  `ca_repeater_port`, `pva_*_port`) via `env::get_u16` at `env.rs:9-14`
- **C:** `modules/libcom/src/env/envSubr.c:397-424` `envGetInetPortConfigParam`
- **Diverges:** C's port reader explicitly rejects out-of-range values:
  `if (epicsParam<=IPPORT_USERRESERVED || epicsParam>USHRT_MAX)` — and
  `IPPORT_USERRESERVED` is `5000` on every platform
  (`osi/os/Linux/osdSock.h` etc., `#define ... 5000`). C logs an error and
  falls back to the compiled default for any port ≤ 5000. The Rust
  `get_u16` accepts *any* `u16` that parses, including `0`, `1`, `80`,
  `443`, `5000`.
- **Impact:** `EPICS_CA_SERVER_PORT=80` is rejected→default-5064 by a C IOC
  but honored by the Rust port, causing a bind to a privileged/foreign
  port (or `EADDRINUSE`/`EACCES`) instead of the documented fallback.
  Also `EPICS_CA_SERVER_PORT=0` would make the Rust server bind an
  ephemeral port. Behavioral divergence on misconfiguration; C's contract
  is "bad port ⇒ documented default", Rust's is "bad port ⇒ obey it".
  Additionally C emits `errlogPrintf` diagnostics on the failure; Rust is
  silent. (`ca_mcast_ttl` *is* correctly clamped — only the port readers
  miss this.)

### H3 — `env::get_u16` silently swallows non-numeric / out-of-range values without C's diagnostic
- **Rust:** `src/runtime/env.rs:9-14`
- **C:** `envSubr.c:303-321` `envGetLongConfigParam` + `:397-424`
  `envGetInetPortConfigParam`
- **Diverges:** When the env value fails to parse, C prints
  `"Unable to find an integer in %s=%s"` to stderr and
  `envGetInetPortConfigParam` additionally prints
  `"EPICS Environment \"%s\" integer fetch failed"` /
  `"setting \"%s\" = %ld"`. The Rust `get_u16` does
  `.and_then(|v| v.parse().ok()).unwrap_or(default)` — completely silent.
  Also: C parses with `sscanf("%ld")`, which accepts leading whitespace,
  `+`/`-` signs, and a trailing garbage suffix (`"5064abc"` parses as
  `5064`). Rust `u16::parse` rejects all of those. So `EPICS_CA_SERVER_PORT=" 5064"`
  works in C, fails→default in Rust (here harmless because the value
  equals the default, but `EPICS_CA_SERVER_PORT=" 6064"` diverges).
- **Impact:** Misconfiguration is invisible to operators on the Rust port;
  values that a C IOC would parse leniently (whitespace, trailing junk)
  are dropped to the default. Wrong port with no log line.

### H4 — `runtime/task.rs` has no `epicsThread` priority / stack-size mapping
- **Rust:** `src/runtime/task.rs:8-22` (`spawn`, `spawn_blocking`)
- **C:** `modules/libcom/src/osi/epicsThread.h:73-92` (priority constants
  `epicsThreadPriorityMin..Max` 0..99, named levels
  `Low=10/Medium=50/High=90`, `CAServerLow=20/CAServerHigh=40`,
  `ScanLow=60/ScanHigh=70`, `Iocsh=91`; stack sizes
  `epicsThreadStackSmall/Medium/Big`)
- **Diverges:** The runtime task façade exposes only bare `tokio::spawn` /
  `tokio::task::spawn_blocking`. There is no concept of EPICS thread
  priority, no priority→OS-scheduler mapping, and no stack-size class.
  The CA server, scan tasks, and callback threads in a C IOC run at
  distinct SCHED priorities (CA server below scan, etc.); the Rust port
  runs everything as undifferentiated tokio tasks on one thread pool.
- **Impact:** Feature gap with real-time consequences. On a loaded IOC,
  C's priority bands guarantee scan threads preempt CA-server threads;
  the Rust port gives no such guarantee. `runtime/sync.rs` documents a
  `linux-rt` PI-mutex feature that is meaningless without per-thread
  priorities to invert — the PI-mutex is dead infrastructure until task
  priorities exist. Classified High because it is wrong real-time
  behavior, not merely a missing convenience API.

---

## MEDIUM

### M1 — General-time monotonic ratchet always applied; C bypasses it when only the OS clock is registered
- **Rust:** `src/runtime/general_time.rs:151-170` `get_current`
- **C:** `epicsGeneralTime.c:84` (`useOsdGetCurrent`), `:111-112` and
  `:159-160` (`if(useOsdGetCurrent) return osdTimeGetCurrent(pDest);`),
  `:392-395` (flag cleared in `insertProvider`)
- **Diverges:** In C, until a *non-default* current-time provider is
  registered, `epicsTimeGetCurrent` short-circuits straight to
  `osdTimeGetCurrent` and the monotonic ratchet (`lastProvidedTime`,
  `ErrorCounts`) is **never consulted**. The Rust port pre-registers an
  "OS Clock" provider at priority 999 (`general_time.rs:57-61`) and runs
  *every* `get_current` through the ratchet at `:156`. With only the OS
  clock present, a real wall-clock step backwards (NTP slew, manual
  `date` change) is silently clamped by the Rust port and increments
  `ERROR_COUNTS`, whereas a C IOC returns the stepped-back time verbatim.
- **Impact:** Edge case but observable: on the common "no time provider
  configured" IOC, Rust freezes time on a backward NTP correction where C
  follows it. Records timestamped during the freeze get a stale time.
  Also `error_counts()` reports phantom errors a C IOC would never count.

### M2 — `generalTimeGetExceptPriority` / interrupt-callable provider variant not ported
- **Rust:** `src/runtime/general_time.rs` (whole module)
- **C:** `epicsGeneralTime.c:106-151` `generalTimeGetExceptPriority`,
  `:226-238` `epicsTimeGetCurrentInt`, `:351-367` `epicsTimeGetEventInt`,
  `:445-459`/`:488-502` `generalTimeAdd*IntProvider`
- **Diverges:** The Rust port has no equivalent of the
  "get time except from priority N" query (used by NTP/clock providers to
  avoid recursive self-query), and no interrupt-context (`*Int`) provider
  registration. `generalTimeHighestCurrentName` is also absent.
- **Impact:** Feature gap. Any time-provider implementation that needs to
  read "the best time *other than mine*" (the standard pattern for an NTP
  provider validating its own sync) cannot be ported faithfully. ISR-time
  paths (`epicsTimeGetCurrentInt`) have no Rust analogue.

### M3 — `report()` output format does not match `generalTimeReport`
- **Rust:** `src/runtime/general_time.rs:259-284`
- **C:** `epicsGeneralTime.c:530-618` `generalTimeReport`
- **Diverges:** C prints `Backwards time errors prevented %u times.` as the
  *first* line, indents providers with a tab + `"%s", priority = %d`,
  and at `level>0` prints each provider's *current time sample*. The Rust
  `report` prints `Current Time Providers:` first, uses two-space indent
  and `"%s" priority %d` (no `= ` , no comma), and at `level>0` prints
  error count + last-provider names instead of per-provider time samples.
- **Impact:** Low-functional but any tooling/test that scrapes
  `generalTimeReport` output (iocsh `generalTimeReport` command) sees a
  different format. Cosmetic divergence; flagged Medium only because
  iocsh command output is a documented interface.

### M4 — `notify_clock_sync` / `register_clock_sync_hook` have no C counterpart
- **Rust:** `src/runtime/general_time.rs:111-144`
- **C:** no equivalent in `epicsGeneralTime.c` / `osiClockTime.c`
- **Diverges:** The doc-comments cite "epics-base 8-D `5cfff383`
  `osiClockTime` sync hooks" but no such API exists in the C tree
  (`osiClockTime.c` has `ClockTimeSync` internal logic, not a public
  registerable hook). This is an invented extension, not a port.
- **Impact:** Not a regression, but it is presented as parity ("Mirrors
  epics-base …") when it is a Rust-only addition. Reviewers tracking
  parity will be misled. If the commit hash is fabricated this should be
  corrected in the doc-comment. Flagged Medium for the false-parity claim.

### M5 — `IfaceMap` includes interfaces C's `osiSockDiscoverBroadcastAddresses` filters out
- **Rust:** `src/net/iface_map.rs:188-213` `enumerate_v4`
- **C:** `modules/libcom/src/osi/osdNetIfConf.c:68-237`
  `osiSockDiscoverBroadcastAddresses`
- **Diverges:** C's discovery explicitly skips interfaces that are
  **down** (`!(flags & IFF_UP)` → "net intf was down", `:170-173`) and
  loopback (`:178-181`), and only keeps interfaces with a valid
  broadcast or point-to-point peer address. The Rust `enumerate_v4` keeps
  every `if-addrs` IPv4 entry and only records `up_non_loopback =
  !iface.is_loopback()` — it never consults an actual UP/RUNNING flag.
  An administratively-down interface that still has an IPv4 address
  configured will be reported by Rust with `up_non_loopback = true`.
- **Impact:** `AsyncUdpV4::bind_with_map_filtered`
  (`async_udp_v4.rs:155-218`) and `fanout_to` (`:396-426`) will attempt
  to bind / send on a down interface. Bind may succeed (address still
  assigned) but sends silently go nowhere; SEARCH fanout wastes a socket
  and may log spurious debug errors. C avoids this entirely. `up_non_loopback`
  is a misnomer — it means "not loopback", not "up".

### M6 — `bind_ephemeral_same_port` chosen-port bind failures are silently tolerated; no loopback guarantee
- **Rust:** `src/net/async_udp_v4.rs:238-284`
- **C:** libca `caServerDestroy`/`addAddrToChannelAccessAddressList` per-NIC
  binding (`rsrv/caservertask.c`)
- **Diverges:** `bind_ephemeral_same_port_with_map` picks the port from the
  first NIC, then for every other NIC a failed bind to that port is logged
  at `debug` and skipped (`:271-281`). If the first (port-owning) NIC is a
  transient interface that later disappears, or if every *other* NIC
  fails the same-port bind, the bundle silently degrades to a single
  socket with no error returned — unlike `bind_with_map_filtered` which at
  least errors when `sockets.is_empty()`. Here `sockets` is never empty
  (the first bind succeeded) so a fully-degraded bundle returns `Ok`.
- **Impact:** Edge case: PVA SEARCH on a multi-NIC host can end up
  single-NIC with no diagnostic. Caller believes fanout works. Medium.

---

## LOW

### L1 — `runtime/log.rs` macros write to stderr only; no errlog severity / buffering / listeners
- **Rust:** `src/runtime/log.rs:1-27`
- **C:** `modules/libcom/src/error/errlog.c` (whole), severity enum
  `errlog.h:49-53` (`errlogInfo/Minor/Major/Fatal`)
- **Diverges:** The four `rt_*` macros are bare `eprintln!` wrappers. There
  is no severity-to-log threshold (`errlogSetSevToLog`/`errlogGetSevToLog`),
  no `MIN_BUFFER_SIZE=1280` ring buffer (`errlog.c:44`), no listener
  registration (`errlogAddListener`), no `errlogSevEnumString` mapping,
  and no IOC log-client forwarding (`EPICS_IOC_LOG_PORT=7004`). `tracing`
  is used elsewhere in the crate but the `rt_*` macros bypass it.
- **Impact:** Feature gap. An IOC port using these macros cannot route
  records' error messages to the central log server, and cannot suppress
  by severity. Low because `tracing` is the de-facto path; `rt_*` looks
  vestigial.

### L2 — `general_time.rs` ratchet uses `SystemTime::UNIX_EPOCH` as the initial value
- **Rust:** `src/runtime/general_time.rs:49-51`
- **C:** `epicsGeneralTime.c:66` `lastProvidedTime` is zero-initialized
  (epicsTimeStamp {0,0} = EPICS epoch 1990-01-01)
- **Diverges:** Rust seeds the ratchet at the Unix epoch (1970); C's
  `epicsTimeStamp` zero is the EPICS epoch (1990). Functionally both are
  "far in the past" so the first real time always wins the ratchet — no
  observable bug — but the constant is conceptually wrong for an EPICS
  port (EPICS time is seconds-past-1990).
- **Impact:** None at runtime (first sample always ratchets forward).
  Flagged Low as a latent correctness smell if anyone diffs the raw
  `last_provided_time` against an EPICS timestamp.

### L3 — `subnet_contains` rejects a `/32` host route only via the zero-mask guard; `/32` works but `0.0.0.0/0` default route is unmatchable
- **Rust:** `src/net/async_udp_v4.rs:984-990`, `iface_map.rs:182-186`
- **C:** routing is delegated to the kernel; libca binds per-NIC and lets
  the OS route.
- **Diverges:** Both `subnet_contains` impls return `false` when
  `mask == 0`. That is deliberate (a 0.0.0.0 netmask would match every
  destination). But it also means an interface legitimately configured
  with a `0.0.0.0` mask is never selected by `pick_nic` subnet matching,
  silently falling through to "first non-loopback NIC" (`:497`). C never
  has this problem because it does not do user-space subnet matching.
- **Impact:** Very rare config; falls back gracefully. Low.

### L4 — `loopback_mcast` hard-codes TTL=1 and ignores `EPICS_CA_MCAST_TTL`
- **Rust:** `src/net/loopback_mcast.rs:84` `set_multicast_ttl_v4(1)`
- **C:** pvxs `udp_collector.cpp` ORIGIN_TAG channel (TTL 1 by design)
- **Diverges:** Not a divergence from C base — the ORIGIN_TAG loopback
  channel is intentionally TTL=1 (host-local). Noting it only because
  `runtime/net.rs:64` `ca_mcast_ttl()` exists and a reader might expect
  it to apply here. It correctly does not.
- **Impact:** None. Documented for completeness; no action needed.

### L5 — `supervise` `SuperviseError::Inner` variant is dead; inner errors never abandon supervision
- **Rust:** `src/runtime/supervise.rs:90-98`, `:140-161`
- **C:** ca-gateway master `gateway.cc` restart loop
- **Diverges:** The `Inner(E)` variant is constructed nowhere — `supervise`
  always retries on inner `Err` until the rate-limit cap, then returns
  `TooManyRestarts`. The doc-comment at `:94-97` admits this. Not a C
  parity issue (C's sliding-window restart matches), just dead API
  surface.
- **Impact:** None functional. Low — a caller pattern-matching on
  `Inner` will get a warning-free unreachable arm.

---

## Notes / verified-correct

- `cas_server_port` precedence (`EPICS_CAS_SERVER_PORT` >
  `EPICS_CA_SERVER_PORT` > 5064) and the client/server split match C's
  `caservertask.c` semantics — `net.rs:21-44` is correct.
- `ca_mcast_ttl` clamping to `1..=255` with fallback-to-1 matches the
  `EPICS_CA_MCAST_TTL=1` default in `configure/CONFIG_ENV:40` —
  `net.rs:64-75` is correct.
- General-time provider insertion order: C `insertProvider`
  (`epicsGeneralTime.c:372-398`) inserts before the first
  strictly-higher-priority provider, so equal-priority providers retain
  registration (FIFO) order. Rust `position(|p| p.priority > priority)`
  (`general_time.rs:83-88`, `:103-108`) does the same. Match.
- Per-event ratchet: C uses `eventTime[NUM_TIME_EVENTS]` with
  `NUM_TIME_EVENTS=256`, ratchets events `1..255` and `BestTime(-1)`,
  no ratchet for `>=256` — Rust `general_time.rs:206-228` matches.
  `S_time_badEvent` for `event < -1` (C `:254-255`) is **not** reproduced:
  Rust `get_event` with a negative event other than -1 would fall through
  to the positive-event path and hit the event providers — minor, callers
  do not pass such values, not separately flagged.
- `SO_REUSEADDR`/`SO_REUSEPORT` policy (Unix both, Windows neither) in
  `bind_one_at` (`async_udp_v4.rs:955-958`) and `loopback_mcast.rs:59-62`
  matches `epicsSocketEnableAddressUseForDatagramFanout` intent.
- Linux `IP_MULTICAST_ALL=0` (`async_udp_v4.rs:964-967`,
  `loopback_mcast.rs:67-70`) matches `osi/os/posix/osdSock.c:116-125`.
</content>
</invoke>
