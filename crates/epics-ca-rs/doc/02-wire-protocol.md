# 02 — Wire protocol reference

This document is a quick reference for the CA wire format as
implemented in `epics-ca-rs`. The canonical spec is
`epics-base/modules/ca/src/client/CAref.html` plus the libca / rsrv
sources; this is a working summary of what we actually parse.

## Transport

| Layer | Use |
|-------|-----|
| UDP   | Search request (`SEARCH`), search reply, beacon (`RSRV_IS_UP`), repeater registration. |
| TCP   | All other operations (CREATE_CHAN, READ/WRITE/EVENT_ADD, etc.). One virtual circuit per `(server_addr, priority)`. |

Default ports:

| Port | Purpose | Override |
|------|---------|----------|
| 5064 | Server (UDP search + TCP) | `EPICS_CA_SERVER_PORT` |
| 5065 | Repeater (UDP fan-out)    | `EPICS_CA_REPEATER_PORT` |

## CaHeader

Every CA frame begins with a fixed 16-byte big-endian header. Source:
`src/protocol.rs::CaHeader`.

```
Offset  Field         Size  Meaning
------  ------------  ----  -----------------------------------------------
0       cmmd          2     Command (opcode) — see opcode table
2       postsize      2     Payload size in bytes (0xFFFF → extended form)
4       data_type     2     DBR type or context-specific tag
6       count         2     Element count (0 → extended form when applicable)
8       cid           4     Channel id (or ECA status / sentinel)
12      available     4     Subscription/io id, sid, etc.
```

When `postsize == 0xFFFF && count == 0`, an **extended header** follows:

```
Offset  Field             Size
------  ----------------  ----
16      extended_postsize 4    actual payload size (>= 0x10000)
20      extended_count    4    actual element count (>= 0x10000)
```

Total header size is then 24 bytes.

### Field interpretation per opcode

The same physical fields carry different meanings depending on `cmmd`.
The most important reinterpretations:

| Field | In SEARCH (request) | In SEARCH (response) | In CREATE_CHAN (resp) | In READ_NOTIFY (resp) |
|-------|---------------------|----------------------|-----------------------|-----------------------|
| `data_type` | `CA_DO_REPLY` (10) | server TCP port | DBR native type | DBR type |
| `count`     | minor version | unused | element count | element count |
| `cid`       | client cid    | server IP (`0` or `~0` → use src addr) | client cid (echo) | ECA status |
| `available` | client cid (dup) | client cid (echo) | server sid | client ioid (echo) |

**Sentinel meanings of `cid` in SEARCH responses**:

- `0` (INADDR_ANY) — server doesn't know its routable address; client
  uses the UDP packet's source IP.
- `~0u32` (0xFFFFFFFF) — same meaning, libca's "address unknown"
  sentinel. Both must be handled. See
  `client/search.rs::handle_udp_response`.

## Opcode table

Source: `src/protocol.rs`. All values are u16 big-endian.

| Code | Constant                     | Direction       | Description |
|------|------------------------------|-----------------|-------------|
| 0    | `CA_PROTO_VERSION`           | both            | Protocol version + sequence number prefix |
| 1    | `CA_PROTO_EVENT_ADD`         | C→S, S→C        | Subscribe / monitor delivery |
| 2    | `CA_PROTO_EVENT_CANCEL`      | C→S             | Cancel subscription |
| 3    | `CA_PROTO_READ`              | C→S (deprecated)| Read without notification (legacy) |
| 4    | `CA_PROTO_WRITE`             | C→S             | Fire-and-forget write |
| 6    | `CA_PROTO_SEARCH`            | both            | UDP/TCP PV search |
| 8    | `CA_PROTO_EVENTS_OFF`        | C→S             | Pause monitor delivery (flow control) |
| 9    | `CA_PROTO_EVENTS_ON`         | C→S             | Resume monitor delivery |
| 10   | `CA_PROTO_READ_SYNC`         | C→S             | Legacy echo for pre-v4.3 servers |
| 11   | `CA_PROTO_ERROR`             | S→C             | Error response with original header echoed |
| 12   | `CA_PROTO_CLEAR_CHANNEL`     | C→S             | Drop a channel |
| 13   | `CA_PROTO_RSRV_IS_UP`        | UDP broadcast   | Beacon |
| 14   | `CA_PROTO_NOT_FOUND`         | S→C (UDP/TCP)   | Negative search reply (only when CA_DO_REPLY) |
| 15   | `CA_PROTO_READ_NOTIFY`       | C→S, S→C        | Read with completion |
| 17   | `CA_PROTO_REPEATER_CONFIRM`  | rep→C           | Repeater registration ack |
| 18   | `CA_PROTO_CREATE_CHAN`       | C→S, S→C        | Create channel + claim native type |
| 19   | `CA_PROTO_WRITE_NOTIFY`      | C→S, S→C        | Write with completion callback |
| 20   | `CA_PROTO_CLIENT_NAME`       | C→S             | Client process owner name (for ACF) |
| 21   | `CA_PROTO_HOST_NAME`         | C→S             | Client hostname (trusted by default; ignored under `asCheckClientIP` / mTLS) |
| 22   | `CA_PROTO_ACCESS_RIGHTS`     | S→C             | Access rights bitmap (read=1, write=2) |
| 23   | `CA_PROTO_ECHO`              | both            | Liveness probe |
| 24   | `CA_PROTO_REPEATER_REGISTER` | C→rep           | Register with local repeater |
| 26   | `CA_PROTO_CREATE_CH_FAIL`    | S→C             | Channel creation rejected |
| 27   | `CA_PROTO_SERVER_DISCONN`    | S→C             | Single-channel disconnect |

Codes not implemented here: `READ_BUILD` (16), `SIGNAL` (25). Modern
clients/servers do not emit these.

## Common opcodes — wire layout

### SEARCH request (UDP, broadcast)

Sent in batches: a single VERSION header, followed by one or more
SEARCH headers each with the PV name as payload.

```
[VERSION header][SEARCH header][PV name (null-terminated, padded to 8)]
                [SEARCH header][PV name ...]
                ...
```

Fields:

```
VERSION:   cmmd=0, count=CA_MINOR_VERSION (13), data_type=0x8000 (sequenceNoIsValid), cid=seq_no
SEARCH:    cmmd=6, postsize=padded_len, data_type=CA_DO_REPLY (10), count=CA_MINOR_VERSION,
           cid=client_cid, available=client_cid
```

We always set `data_type = CA_DO_REPLY` so we get an explicit
NOT_FOUND on miss when the server supports it.

### SEARCH response (UDP unicast or TCP)

```
VERSION:   cmmd=0, count=server minor version, data_type=0x8000 if seq_no echoed, cid=our seq_no
SEARCH:    cmmd=6, postsize=8, data_type=server TCP port,
           cid=server IP (0 or ~0 → use src addr),
           available=client_cid (echo)
```

The 8-byte payload after the SEARCH header carries the server's minor
version as a u16 in the first 2 bytes (rest zero-padded to 8).

### NOT_FOUND (UDP unicast)

```
NOT_FOUND: cmmd=14, data_type=CA_DO_REPLY, count=CA_MINOR_VERSION,
           cid=client_cid, available=client_cid
```

Only sent when the original SEARCH header had `data_type == CA_DO_REPLY`.

### CREATE_CHAN

Client → server:
```
CREATE_CHAN: cmmd=18, postsize=padded PV name len, available=CA_MINOR_VERSION,
             cid=client_cid, payload=PV name
```

Server → client (success path emits ACCESS_RIGHTS first, then
CREATE_CHAN):
```
ACCESS_RIGHTS: cmmd=22, cid=client_cid, available=access bitmap (1=read,2=write,3=both)
CREATE_CHAN:   cmmd=18, data_type=native DBR type, count=element count,
               cid=client_cid (echo), available=server sid
```

Server → client (failure):
```
CREATE_CH_FAIL: cmmd=26, cid=client_cid
```

### READ_NOTIFY

Client → server:
```
READ_NOTIFY: cmmd=15, data_type=DBR type, count=requested element count,
             cid=server sid, available=ioid
```

Server → client (success):
```
READ_NOTIFY: cmmd=15, postsize=padded payload size, data_type=DBR type,
             count=actual count, cid=ECA_NORMAL (1), available=ioid (echo)
             payload=encoded DBR
```

Server → client (failure):
```
READ_NOTIFY: cmmd=15, count=0, data_type=DBR type, cid=ECA error code,
             available=ioid (echo), no payload
```

### WRITE_NOTIFY

Client → server:
```
WRITE_NOTIFY: cmmd=19, postsize=padded payload, data_type=DBR type,
              count=element count, cid=server sid, available=ioid,
              payload=encoded DBR
```

Server → client:
```
WRITE_NOTIFY: cmmd=19, count=client_count (echo), data_type=DBR type,
              cid=ECA status, available=ioid (echo)
```

### CA_PROTO_WRITE (fire-and-forget)

Same as WRITE_NOTIFY but `cmmd=4`. Server does **not** send a reply;
client does not allocate a write_waiter.

### EVENT_ADD (subscribe)

Client → server:
```
EVENT_ADD: cmmd=1, postsize=16, data_type=DBR type, count=element count,
           cid=server sid, available=subscription_id
           payload (16 bytes): [low(4)][high(4)][to(4)][mask(2)][pad(2)]
                               (low/high/to are doubles; only mask is honoured)
```

Server → client (one or more times):
```
EVENT_ADD: cmmd=1, postsize=padded payload, data_type=DBR type,
           count=element count, cid=ECA_NORMAL (1), available=sub_id,
           payload=encoded DBR
```

### EVENT_CANCEL

Client → server:
```
EVENT_CANCEL: cmmd=2, data_type=DBR type, cid=server sid, available=sub_id
```

Server → client (final reply per spec):
```
EVENT_ADD: cmmd=1, count=0, data_type=DBR type,
           cid=ECA_NORMAL, available=sub_id
```

### ECHO

Either direction sends an empty `cmmd=23` header. The receiver
**echoes a fresh `cmmd=23` back**. The sender uses the round trip as
liveness proof.

For pre-v4.3 servers (which do not understand ECHO), the client falls
back to `READ_SYNC` (`cmmd=10`) — handled in
`client/transport.rs::read_loop`.

### Beacon (CA_PROTO_RSRV_IS_UP)

Broadcast UDP datagram. No payload.

```
RSRV_IS_UP: cmmd=13, data_type=CA_MINOR_VERSION, count=server TCP port,
            cid=monotonically increasing beacon id (resets on restart),
            available=server IP (or 0 → repeater fills in)
```

Receivers track `(server_addr, last_id)`. Anomaly when:

- `beacon_id != last_id + 1` (sequence break → IOC restarted)
- `actual_interval < period_estimate / 3` (fast ramp → IOC restarted)

Detection: `client/beacon_monitor.rs::handle_beacon`.

### Repeater protocol

A trivial UDP fan-out. Source: `src/repeater.rs`.

```
REPEATER_REGISTER: cmmd=24, available=client local IP
                   (sometimes a zero-length datagram for pre-3.12 compat)
REPEATER_CONFIRM:  cmmd=17, available=client IP (echo)
```

Once registered, the repeater forwards every UDP datagram received on
port 5065 (typically beacons sent by IOCs) to every registered client.

## DBR encoding

DBR (Database Request) types are documented in detail in
[`06-dbr-types.md`](06-dbr-types.md). Briefly:

- Native (0–6): just the value, no metadata
- STS (7–13): status(2) + severity(2) + value
- TIME (14–20): status + severity + 8-byte EPICS timestamp + value
- GR (21–27): status + display metadata (units, precision, limits) + value
- CTRL (28–34): GR + 2 extra control limits + value
- PUT_ACKT (35), PUT_ACKS (36): write-only u16, routed to record ACKT/ACKS
- STSACK_STRING (37): status + severity + ackt + acks + 40-byte string

All multi-byte fields are big-endian. All payloads are padded to 8-byte
alignment on the wire (the codec computes `align8(len)` before
`set_payload_size`).

## ECA status codes

61 codes defined in `protocol.rs`, mirroring `caerr.h`. Encoded as
`((msg_no << 3) & 0xFFF8) | (severity & 0x7)`.

Severities: `CA_K_SUCCESS=1`, `CA_K_INFO=3`, `CA_K_WARNING=0`,
`CA_K_ERROR=2`, `CA_K_SEVERE=4`, `CA_K_FATAL=6`.

The most commonly seen codes:

| Constant | Decimal | Hex | Notes |
|----------|---------|-----|-------|
| `ECA_NORMAL` | 1 | 0x01 | Success |
| `ECA_BADTYPE` | 114 | 0x72 | Unsupported DBR type |
| `ECA_TIMEOUT` | 80 | 0x50 | Operation timed out |
| `ECA_BADCHID` | 410 | 0x19A | Unknown sid |
| `ECA_BADCOUNT` | 176 | 0xB0 | Element count out of range |
| `ECA_PUTFAIL` | 160 | 0xA0 | Write rejected by record |
| `ECA_NOWTACCESS` | 376 | 0x178 | Write access denied |
| `ECA_DISCONN` | 192 | 0xC0 | Channel disconnected |
| `ECA_ALLOCMEM` | 48 | 0x30 | Server resource exhausted (we use this for max-channels) |

`eca_message(status)` (in `protocol.rs`) returns the human-readable
text matching libca's `ca_message_text[]`.
