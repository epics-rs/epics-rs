# 08 — Environment variables

Complete reference of every `EPICS_CA_*` and `EPICS_CAS_*` variable
the implementation honours. Variables marked **(libca)** are also
honoured by the C reference implementation; **(rust-only)** are
unique to `epics-ca-rs`.

## Client variables

### `EPICS_CA_ADDR_LIST` (libca)

Whitespace-separated list of `host[:port]` entries to send UDP
SEARCH datagrams to.

- Unqualified entries (`host` only) use the port from
  `EPICS_CA_SERVER_PORT`.
- IPv6 not supported.
- DNS resolution attempted on non-numeric hosts.

Default: empty (auto-discovery only, see next variable).

```bash
EPICS_CA_ADDR_LIST="10.0.0.1 10.0.0.2:5064 ioc.example.com"
```

### `EPICS_CA_AUTO_ADDR_LIST` (libca)

`YES` (default) → also include the broadcast address of every up,
non-loopback IPv4 interface, plus the limited broadcast
`255.255.255.255` as a final fallback.

`NO` → use **only** what's in `EPICS_CA_ADDR_LIST` (which must be
non-empty, otherwise the search engine will silently send no traffic).

### `EPICS_CA_SERVER_PORT` (libca)

UDP server port. Used as default when `EPICS_CA_ADDR_LIST` entries
omit a port. Default: 5064.

### `EPICS_CA_REPEATER_PORT` (libca)

UDP port the local CA repeater listens on. Default: 5065. The
repeater binary `ca-repeater-rs` reads this; the client beacon
monitor registers with `localhost:<EPICS_CA_REPEATER_PORT>`.

### `EPICS_CA_CONN_TMO` (libca)

Echo idle timeout in seconds. After this much idle time on a TCP
circuit, the client sends `CA_PROTO_ECHO` to verify liveness.
Default: 30. Minimum: 1.

The echo response timeout (5 s) is not configurable.

### `EPICS_CA_MAX_ARRAY_BYTES` (libca)

Maximum payload size for any single CA message, in bytes. Default:
16 MB. Both client and server respect this; messages larger than
this are rejected at parse time.

### `EPICS_CA_MAX_SEARCH_PERIOD` (libca)

Upper bound on the per-channel search retry period in seconds.
Default: 300. Minimum: 60.

The search engine starts at the base RTT estimate (clamped at 32 ms
floor) and exponentially increases the lane period on each unanswered
attempt, capped at this value. Sites with very long-lived disconnect
periods may want this lower to shorten reconnect time after the IOC
returns.

### `EPICS_CA_PUT_TIMEOUT` (libca)

Default `put` timeout in seconds when the caller doesn't pass one
explicitly. Default: 30.

### `EPICS_CA_NAME_SERVERS` (libca)

Whitespace-separated `host[:port]` entries for TCP-based PV name
resolution (libca's `registerSearchDest` mechanism). Each entry
spawns a long-lived TCP connection that:

- Runs the same VERSION + HOST_NAME + CLIENT_NAME handshake as a
  regular CA circuit
- Receives outgoing search payloads from the engine and forwards them
  to TCP
- Reads incoming bytes and feeds them back through
  `handle_udp_response` for parsing

Used to traverse firewalls or NAT where UDP broadcast can't reach
the IOC. Default: empty (no nameserver connections).

```bash
EPICS_CA_NAME_SERVERS="ns1.facility.org:5064 ns2.facility.org:5064"
```

### `EPICS_CA_MONITOR_QUEUE` (rust-only)

Per-subscription mpsc capacity on the **client** side. Default: 256.
Minimum: 8.

When the queue fills (the application is calling `MonitorHandle::recv`
slower than events arrive), `try_send` fails and
`CaDiagnostics::dropped_monitors` increments. Combined with
EVENTS_OFF flow control, the client should converge before this
matters; raise the queue if you're seeing dropped events.

### `EPICS_CA_USE_SHELL_VARS` (libca)

`YES` → expand `${VAR}` and `$(VAR)` references in PV names against
the process environment (`expand_shell_vars` in `client/mod.rs`).
Default: `NO`.

```bash
export DETECTOR=DET01
EPICS_CA_USE_SHELL_VARS=YES caget-rs '${DETECTOR}:VAL'
# resolves to "DET01:VAL"
```

## Server variables

### `EPICS_CAS_INTF_ADDR_LIST` (libca)

Whitespace-separated IPv4 addresses to bind UDP responder sockets on.
Default: `0.0.0.0` (one socket on all interfaces). Each entry spawns
a separate UDP responder task — use this on multi-NIC IOCs that need
to answer searches from multiple subnets reliably.

```bash
EPICS_CAS_INTF_ADDR_LIST="10.0.1.5 10.0.2.5"
```

### `EPICS_CAS_BEACON_ADDR_LIST` (libca)

Whitespace-separated `host[:port]` destinations for beacon
broadcasts. Default: empty.

When unset and `EPICS_CAS_AUTO_BEACON_ADDR_LIST=YES` (the default),
the beacon emitter falls back to:

1. `EPICS_CA_ADDR_LIST` (if set)
2. Auto-discovered per-NIC broadcasts via `if-addrs`
3. `255.255.255.255` as last resort

### `EPICS_CAS_AUTO_BEACON_ADDR_LIST` (libca)

`YES` (default) → also include auto-discovered NIC broadcasts.

### `EPICS_CAS_IGNORE_ADDR_LIST` (libca)

Source IPs whose UDP datagrams should be silently dropped by the
search responder. Useful for filtering test or rogue clients.

### `EPICS_CAS_BEACON_PERIOD` (libca)

Steady-state beacon period in seconds. Default: 15. Minimum: 0.1.

The beacon emitter starts at 20 ms after each TCP accept/disconnect
and doubles up to this value, mirroring libca's "fast restart"
behaviour.

### `EPICS_CAS_SERVER_PORT` (libca)

Used by some `softIoc` configurations. `epics-ca-rs` itself uses
`EPICS_CA_SERVER_PORT` for the `--port` default.

### `asCheckClientIP` — an iocsh knob, not an environment variable

There is **no** `EPICS_CAS_USE_HOST_NAMES` variable. Earlier revisions of
this document and `04-server.md` described one and claimed it matched C's
default; it exists nowhere in epics-base and the port no longer reads it
(R7-16).

What C actually has is the global `asCheckClientIP`
(`asLibRoutines.c:34`), default `0`, set from the shell:

* **`0` (default)** — rsrv stores the hostname the client sends in
  `CA_PROTO_HOST_NAME` unconditionally (`camessage.c:797-878`) and
  `HAG` entries are host *names* (`asHagAddHost` lowercases them). This
  is what a `HOST(node)` rule in a facility `.acf` is written against.
  rsrv does not verify the claim — the trust model assumes a controlled
  subnet.
* **`1`** — rsrv fills the client's host with the peer's dotted-quad IP
  at accept time (`caservertask.c:1425-1439`), ignores
  `CA_PROTO_HOST_NAME`, and resolves every `HAG` entry to an IP when the
  ACF is loaded. Use this when the network is not trusted.

C sets it with an iocsh *variable* (`var asCheckClientIP 1`), and so does
this port:

```
var asCheckClientIP 1 # before asInit — the HAG storage form is chosen at parse time
asSetFilename /path/to/facility.acf
asInit
```

`var asCheckClientIP` with no value prints `int asCheckClientIP = 0`, and
setting it prints nothing, exactly as in C. Both halves — HAG storage and
client identity — read the one flag, so they can never disagree.

Under mTLS the identity comes from the verified client certificate and
neither mode applies; see `11-tls-design.md`.

### `EPICS_CAS_INACTIVITY_TMO` (rust-only)

Force a TCP client disconnect after this many seconds of total read
silence. Default: 600. Minimum: 30.

This is a belt-and-suspenders cap — OS-level keepalive (15 s/5 s)
should detect half-open connections within ~30 s, but on
environments where keepalive is disabled or unreliable, this
guarantees clients don't pin server resources indefinitely.

### `EPICS_CAS_MAX_CHANNELS` (rust-only)

Maximum number of channels per client connection. **Default:
unbounded** (opt-in) — C rsrv `claim_ciu_action` imposes no per-client
channel count limit, refusing only on genuine memory exhaustion, so a
single legitimate client (e.g. `caget` over thousands of PVs on one
circuit) is never refused at a fixed boundary. A fixed default
diverged from this and produced a latency cliff at the cap boundary.
Minimum (when set): 1.

When the variable is set and a client tries to create the (N+1)st
channel, the server replies `ECA_ALLOCMEM` and closes the connection
(matching C `claim_ciu_action`'s `send_err(ECA_ALLOCMEM)` + RSRV_ERROR).

### `EPICS_CAS_MAX_SUBS_PER_CHAN` (rust-only)

Maximum number of subscriptions per channel. **Default: unbounded**
(opt-in) — C rsrv `event_add_action` imposes no per-channel
subscription count limit, refusing only on genuine memory exhaustion.
Minimum (when set): 1. When the variable is set, excess `EVENT_ADD`
requests get `ECA_ALLOCMEM`.

### `EPICS_CAS_AUDIT_FILE` (rust-only)

Path to a JSON-Lines audit log. When set, the server appends one
line per security-relevant event (connect, disconnect,
create_chan, caput, ACF deny). Pair with `logrotate` for retention.
Unset disables audit.

### `EPICS_CAS_AUDIT` (rust-only)

Alternative sink. `stderr` mirrors audit lines to standard error
(useful under systemd-journald). Ignored when
`EPICS_CAS_AUDIT_FILE` is also set.

### `EPICS_CAS_RATE_LIMIT_MSGS_PER_SEC` (rust-only)

Per-client steady-state rate cap (CA messages per second). Default:
0 (disabled). When set, every accepted CA message draws one token
from a bucket; when the bucket is empty the message is dropped and a
strike is recorded.

### `EPICS_CAS_RATE_LIMIT_BURST` (rust-only)

Per-client burst capacity. Default: `4 × MSGS_PER_SEC`. Sized to
absorb short bursts (channel-create storm at IOC startup) without
penalizing the well-behaved.

### `EPICS_CAS_RATE_LIMIT_STRIKES` (rust-only)

How many consecutive dropped messages cause the connection to be
torn down. Default: 100. Set to 0 to never disconnect (drop-only
mode).

### `EPICS_CAS_INTROSPECTION_ADDR` (rust-only)

`host:port` to bind the HTTP introspection endpoint. When set, the
server exposes `/healthz`, `/info`, `/clients`, `/queues` (GET) and
`/drain`, `/reload-acf` (POST) for external supervisors (Kubernetes
probes, oncall dashboards). Plain JSON, no auth — bind to
`127.0.0.1:<port>` for IOC-local access or to a private interface
for facility tooling.

### `EPICS_CAS_DRAIN_GRACE_SECS` (rust-only)

Seconds to keep existing CA connections alive after drain is
triggered (SIGTERM or POST /drain). After this elapses the process
exits cleanly. Default: 30. Set to bracket your Kubernetes
`terminationGracePeriodSeconds`.

### `EPICS_CAS_TLS_MODE` (rust-only)

Selects the wire-level TLS negotiation mode when TLS is enabled.

- `rust-only` (default): the entire TCP stream is wrapped in TLS
  from byte zero — interoperable with other `epics-ca-rs` peers, not
  with C tooling.
- `ca-secure-draft`: placeholder for upstream `epics-base` 7
  ca-secure spec interop. Currently falls back to `rust-only` with a
  debug log; see `crates/epics-ca-rs/doc/11-tls-design.md` and `src/tls/ca_secure.rs`
  for the design path.

### `EPICS_CA_RS_CHAOS` (rust-only)

Comma-separated fault-injection directives: `drop:5%`, `stall:50ms`,
`reorder:1%`, `seed:42`. Wired into the TCP server read path for
soak / chaos testing. Default: unset (no overhead). See
`src/chaos.rs` for the full grammar.

## Compatibility notes

The **rust-only** variables (`EPICS_CA_MONITOR_QUEUE`,
`EPICS_CAS_INACTIVITY_TMO`, `EPICS_CAS_MAX_CHANNELS`,
`EPICS_CAS_MAX_SUBS_PER_CHAN`, `EPICS_CAS_AUDIT_FILE`,
`EPICS_CAS_AUDIT`, `EPICS_CAS_RATE_LIMIT_*`,
`EPICS_CAS_INTROSPECTION_ADDR`, `EPICS_CAS_DRAIN_GRACE_SECS`,
`EPICS_CA_RS_CHAOS`, `EPICS_CAS_TLS_MODE`,
`EPICS_CAS_UDP_SEARCH_RATE_LIMIT`) are no-ops if observed by
libca/rsrv.
They were chosen to mirror libca's variable-naming convention so
operators don't need to learn a separate vocabulary.

The default values for all libca-compatible variables match the
documented libca defaults.

## Verification

You can confirm at runtime which addresses the search engine is
sending to by setting `CA_RS_DEBUG=1` (compile-time gated debug
print, currently disabled in release builds — see git history for
how to re-enable during local debugging).

For env-var observability without recompiling, the `ca-soak` binary
prints the parsed address list in its initial `CA server: UDP search
on port X, beacons → N address(es)` line (server side) or via
`CaDiagnostics::history` event records (client side, post-run).
