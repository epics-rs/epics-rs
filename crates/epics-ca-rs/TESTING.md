# epics-ca-rs Testing Guide

This document describes the verification infrastructure for `epics-ca-rs`.
Tests are organized in three layers:

1. **Unit / integration tests** — no external dependencies, runs under
   `cargo test`
2. **Interop tests** — require EPICS C tools (`softIoc`, `caget`, `caput`,
   `camonitor`, `cainfo`) on `PATH`; auto-skipped otherwise
3. **Soak / load tests** — long-running binary (`ca-soak`) and
   high-concurrency / write-burst scenarios

---

## 1. Quick verification (CI / per-commit)

```bash
cargo test -p epics-ca-rs -p epics-base-rs
```

On hosts without an EPICS install all interop tests skip silently, so
the suite passes regardless of environment.

---

## 2. Interop tests (libca wire-level compatibility)

### Prerequisites

- `softIoc`, `caget`, `caput`, `camonitor`, `cainfo` on `PATH`
- After building EPICS base, add `bin/<arch>/` to `PATH`

Verify:
```bash
which softIoc caget caput camonitor cainfo
```

### Running

```bash
# Rust client ↔ C softIoc
cargo test -p epics-ca-rs --test interop_rust_client_c_ioc -- --test-threads=1

# C tools (caget/caput/camonitor/cainfo) ↔ Rust softioc-rs
cargo test -p epics-ca-rs --test interop_c_client_rust_ioc -- --test-threads=1

# Both
cargo test -p epics-ca-rs --tests -- --test-threads=1
```

`--test-threads=1` is required because each test spawns a real IOC on
ephemeral ports — concurrent runs could compete for the same default
sockets.

### Coverage

| Test | What it verifies |
|------|------------------|
| `rust_client_can_caget_from_softioc` | Rust client → C IOC: search, connect, READ_NOTIFY |
| `rust_client_can_caput_to_softioc` | Rust client → C IOC: WRITE_NOTIFY round trip |
| `rust_client_monitors_softioc_changes` | EVENT_ADD subscription, multi-update receive |
| `rust_client_handles_softioc_restart` | Beacon-anomaly-driven reconnect after IOC restart |
| `c_caget_can_read_from_rust_ioc` | C tool → Rust IOC: READ_NOTIFY |
| `c_caput_can_write_to_rust_ioc` | C tool → Rust IOC: WRITE_NOTIFY |
| `c_camonitor_sees_rust_ioc_changes` | C `camonitor` receives Rust IOC monitor stream |
| `c_cainfo_describes_rust_ioc_channel` | `cainfo` output compatibility |
| `pyepics_caget_via_libca_against_rust_ioc` | pyepics (via libca) compatibility; skipped if pyepics absent |

---

## 3. Load / failure tests

```bash
cargo test -p epics-ca-rs --test stress_load -- --test-threads=1
```

`stress_load` covers:

- 10 channels connecting concurrently (`many_concurrent_channels_connect`)
- 50 create/drop cycles — guards against coordinator state leaks
  (`rapid_create_drop_cycles`)
- 100-write burst — exercises the write-waiter path
  (`burst_of_writes_completes`)
- 200 updates/sec monitor stream — verifies coalescing-based
  drop-oldest semantics (`monitor_keeps_up_with_high_update_rate`)

Runs in seconds. For real soak conditions use `ca-soak`.

---

## 4. Soak testing (long-running)

### Binary

`ca-soak` opens monitors on a list of PVs, periodically reads them,
optionally drives writes, and prints rolling diagnostics. On exit it
dumps cumulative counters and the `CaDiagnostics` snapshot.

```bash
cargo build --release -p epics-ca-rs --bin ca-soak

ca-soak \
    --pv MY:PV:1 --pv MY:PV:2 \
    --writes-per-sec 10 \
    --duration 3600 \
    --report-interval 60
```

`--duration 0` runs until `Ctrl+C`.

### Recommended profiles

| Goal | Suggested flags |
|------|------------------|
| 1-hour smoke | `--duration 3600 --writes-per-sec 5 --report-interval 60` |
| Overnight (8h) | `--duration 28800 --writes-per-sec 10 --report-interval 300` |
| Weekend (72h) | `--duration 259200 --writes-per-sec 1 --report-interval 600` |
| Write-burst stress | `--writes-per-sec 100` against a single PV |

### Sample output

```
[soak +  60.0s] mons=1180 reads=120 (err 0) writes=596 (err 0)
  diag: conns=4 disconns=0 reconns=0 unresp=0 drop_mon=0 beacon_anom=2

=== Soak summary ===
Duration:       60.0s
Monitor events: 1180
Reads:          120 (0 err)
Writes:         596 (0 err)

Connections:            4
Disconnections:         0
Reconnections:          0
...
```

### What to look for

After a soak run, inspect the `CaDiagnostics` summary:

- `Disconnections` climbing slowly → circuit instability
- `Reconnections >= Disconnections` → expected (every disconnect should
  be followed by a reconnect)
- `Dropped monitors` non-zero → consumer is too slow, or the monitor
  queue is too small (`EPICS_CA_MONITOR_QUEUE`)
- Frequent `Unresponsive events` → echo timeouts; check network or IOC
  load
- `Beacon anomalies` increase whenever any IOC on the LAN restarts
  (expected; informational only)

---

## 5. Debugging interop failures

### softIoc waiting on a TTY

If a test fails immediately with `Disconnected`, softIoc may be
blocking on stdin for an interactive shell. The test harness already
passes `-S` (no shell); when invoking softIoc manually do the same:

```bash
softIoc -S -d test.db
```

### Port collisions

Each interop test allocates ephemeral UDP/TCP ports dynamically.  When
in doubt:

```bash
lsof -i UDP -i TCP | grep softIoc
```

### Reproduce with libca only

If a test fails, first confirm the C reference works in the same
environment:

```bash
PORT=$(python3 -c "import socket; s=socket.socket(); s.bind(('',0)); print(s.getsockname()[1])")
EPICS_CAS_INTF_ADDR_LIST=127.0.0.1 EPICS_CAS_SERVER_PORT=$PORT \
    softIoc -S -d test.db &
EPICS_CA_ADDR_LIST=127.0.0.1 EPICS_CA_AUTO_ADDR_LIST=NO EPICS_CA_SERVER_PORT=$PORT \
    caget -w 3 TEST:AI
```

If the C tools work, the bug is on the Rust side. If they don't, fix
the environment first.

---

## 6. Environment variable reference

EPICS variables honoured by the test harness and by `ca-soak`:

| Variable | Purpose |
|----------|---------|
| `EPICS_CA_ADDR_LIST` | Server search destinations (UDP) |
| `EPICS_CA_AUTO_ADDR_LIST` | Auto-discover NIC broadcasts (`YES`/`NO`) |
| `EPICS_CA_SERVER_PORT` | Default server port (default 5064) |
| `EPICS_CA_MONITOR_QUEUE` | Monitor queue capacity (Rust-only, default 256) |
| `EPICS_CAS_INTF_ADDR_LIST` | Server bind interfaces |
| `EPICS_CAS_BEACON_ADDR_LIST` | Beacon destinations |
| `EPICS_CAS_BEACON_PERIOD` | Beacon period (seconds) |
| `EPICS_CAS_INACTIVITY_TMO` | Forced client disconnect after idle period |
| _(none)_ | Host identity is not env-configurable — use `var asCheckClientIP 1` |

See the main `README.md` "Environment variables" section for the full
list.
