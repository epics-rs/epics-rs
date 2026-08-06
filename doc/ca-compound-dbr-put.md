# Compound DBR types on a CA write

**Status:** As-built
**Date:** 2026-08-06
**Code:** `crates/epics-ca-rs/src/server/tcp.rs`, `AcceptedWriteType` +
`serve_write_head`

A CA client may put using any buffer type the protocol defines, not just the
seven native ones. `ca_array_put` checks only `type < 0`
(`ca/src/client/oldChannelNotify.cpp:429`), so a `DBR_STS_*` / `DBR_TIME_*` /
`DBR_GR_*` / `DBR_CTRL_*` buffer — value preceded by status, severity,
timestamp, limits — goes on the wire unchallenged. It arises when a client
reuses its read type for the write, or echoes back a structure it got from a
get.

The metadata in such a put is unusable by definition: `.TIME` is
`DBF_NOACCESS` (`dbCommon.dbd:280`) and `STAT`/`SEVR`/`UTAG` are `SPC_NOMOD`
(`:117`, `:123`, `:286`), so no client can set them through any field path
either.

## What C does

The two write opcodes diverge.

| | `CA_PROTO_WRITE` | `CA_PROTO_WRITE_NOTIFY` |
|---|---|---|
| type bound | `caNetConvert` — `ECA_BADTYPE` above `LAST_BUFFER_TYPE` (38) → RSRV_ERROR | `INVALID_DB_REQ` — same bound, same drop (`camessage.c:1673`) |
| compound at or below the bound | **written**: `dbChannel_put` (`db/db_access.c:820`) skips the metadata header and puts `.value` | **fails**: `mapOldType` (`db_access.c:988`) maps only native types, returns -1, `db_put_process` → `notifyError` |
| answer on failure | `send_err(ECA_PUTFAIL)`, RSRV_OK — circuit kept (`camessage.c:806-816`) | `ECA_PUTFAIL` in the completion (`camessage.c:1412-1413`) — circuit kept |

epics-base PR #948 fixed one cell of `dbChannel_put`'s translation table: the
`oldDBR_TIME_STRING` arm passed `DBR_TIME`, which is the *get option mask*
`0x10` from `dbAccessDefs.h`, not a field type. `dbPut` rejected it through
`INVALID_DB_REQ(dbrType) = (x > DBR_ENUM)` with `DBR_ENUM` = 11, so that one
combination failed with `S_db_badDbrtype` while its siblings worked.

## What this server does

`AcceptedWriteType::classify` splits the wire type three ways, and the gate
rejects only what is above C's bound:

- `Native(0..=6)` — decoded and written as before.
- `Compound(7..=34)` — carries the base native type. On `CA_PROTO_WRITE` the
  metadata is skipped and the value written; on `CA_PROTO_WRITE_NOTIFY` it is
  `ECA_PUTFAIL`, as in C.
- `MetadataOnly(37, 38)` — `ECA_PUTFAIL` on both opcodes, C's `default:` arm.

Above `LAST_BUFFER_TYPE` the classifier returns `None` and the gate answers
`ECA_BADTYPE` and drops, as RSRV does. `DBR_PUT_ACKT`/`DBR_PUT_ACKS` (35/36)
never reach the classifier — the alarm-acknowledge branch takes them first.

Three things follow from where the failures land. The gates keep their
C-observable positions and the refusal happens at the put, so a compound
WRITE_NOTIFY to a channel the peer cannot write still reports
`ECA_NOWTACCESS`, and still supersedes the channel's in-flight put-callback.
The refusal is a put *result* (`PutPlan::Refuse`, answered with
`CaError::UnsupportedType` → `ECA_PUTFAIL`) rather than an early return, so it
runs inside the trap-write bracket exactly as C's unconditional
`asTrapWriteWithData` ahead of `dbChannel_put` / `dbProcessNotify` does — a
put-log records the attempt, not the outcome. And the header skip is not a
second table: it is `decode_dbr`, the same compound-layout owner the read and
monitor paths use, which bounds-checks where C casts the struct unchecked.

Both the put-log entry and the WRITE_NOTIFY completion echo `hdr.data_type` —
the type the client sent, as C's `asTrapWriteWithData` and `write_notify_reply`
(which frames from the saved request header) do. For a compound put that
differs from the base type the record took.

Boundary tests in `server/blocking.rs`:
`compound_dbr_plain_write_strips_metadata_and_writes_the_value` (value crosses,
status/severity/timestamp do not),
`compound_dbr_write_notify_is_putfail_and_keeps_the_circuit`,
`metadata_only_dbr_plain_write_is_putfail_and_keeps_the_circuit`,
`dbr_type_above_last_buffer_type_still_drops_the_circuit`,
`a_refused_dbr_type_is_still_bracketed_in_the_put_log` (both refusal shapes,
`BeforeWrite`/`AfterWrite` with status `dbr-type-not-puttable`).

## Measured against the C oracle

2026-08-06, `record(ao,"PROBE:AO")` served by base's `softIoc` on 127.0.0.1:5075
and by `softioc-rs --db probe.db` on :5076, driven by a raw-CA prober (libca
cannot emit these buffer types). Each probe ran on a fresh circuit with a
`READ_NOTIFY` pipelined behind it, so a drop in one cannot mask the next.
Both servers answered identically on all seven:

| probe | reply | circuit | readback |
|---|---|---|---|
| `DBR_TIME_DOUBLE` `WRITE` (24 B) | none | up | 7.5 — value written |
| `DBR_TIME_DOUBLE` `WRITE_NOTIFY` | `ECA_PUTFAIL`, type echo 20 | up | unchanged |
| `DBR_CLASS_NAME` `WRITE` (40 B) | `ERROR`/`ECA_PUTFAIL` | up | unchanged |
| `DBR_CLASS_NAME` `WRITE_NOTIFY` (40 B) | `ECA_PUTFAIL`, type echo 38 | up | unchanged |
| `DBR_CLASS_NAME` `WRITE` (8 B, short) | `ERROR`/`ECA_PUTFAIL` | up | unchanged |
| type 39 (`LAST_BUFFER_TYPE`+1) | `ERROR`/`ECA_BADTYPE` | **down** | — |
| `DBR_DOUBLE` `WRITE` (control) | none | up | 3.25 |

The short `DBR_CLASS_NAME` row answers the question this server has no size
table for: C does **not** drop the circuit on an undersized metadata-only
frame, so refusing it with `ECA_PUTFAIL` is what C does, not a shortcut.

A separate probe wrote `DBR_TIME_DOUBLE` 42.125 carrying status 3, severity 2
and seconds `0x11111111`, then read `DBR_TIME_DOUBLE` back. Both servers
returned value 42.125, status 0, severity 0, and a timestamp that is not the
injected one.
