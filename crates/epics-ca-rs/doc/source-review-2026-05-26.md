# CA-RS Source Review - 2026-05-26

Scope:

- Crate: `crates/epics-ca-rs`
- Upstream reference (read-only): EPICS base C at `/Users/stevek/codes/epics-base`
  - CA server protocol: `modules/database/src/ioc/rsrv/camessage.c`
  - CA client: `modules/ca/src/client/cac.cpp`, `modules/ca/src/client/udpiiu.cpp`
  - Protocol header: `modules/ca/src/client/caProto.h`, `modules/ca/src/client/caerr.h`
- Areas reviewed: CA wire protocol framing, CA_PROTO_ERROR reply layout, DBR type
  conversions, monitor flow control, search/UDP/repeater, ECA status codes,
  extended-header handling, beacon, client transport, access rights.
- Finding-ID series: `R-N` (the global parity-round series; this document records the
  epics-ca-rs slice — R45 here). IDs are globally unique by prefix and never reused;
  see `docs/review-tagging-conventions.md`.

## References

- `caProto.h` – wire constants, `mon_info` struct, CA_V4x macros
- `camessage.c` – server-side command handlers (`vsend_err`, `search_reply_udp`,
  `event_add_action`, `clear_channel_reply`, `read_sync_reply`, etc.)
- `cac.cpp` – client-side response parsers (`exceptionRespAction`, `searchRespAction`,
  `eventAddRespAction`, etc.)
- `udpiiu.cpp` – UDP search client

## Method

For each focus area: read both the Rust source and the C reference, compare field
assignments, payload sizes, and control flow. A finding is recorded only when both
a Rust path:line and a C path:line are cited as bilateral evidence.

## Findings

### R45 — `send_ca_error` declares 8 bytes too few in outer header for extended-original requests

Severity: High

Status: Fixed

Evidence:

- **Rust**: `crates/epics-ca-rs/src/server/tcp.rs:4657` —
  `let payload_size = CaHeader::SIZE + error_msg_bytes.len();` always uses 16 regardless
  of whether `original_hdr.to_bytes_extended()` returns 16 or 24 bytes.
- **C**: `modules/database/src/ioc/rsrv/camessage.c:201-214` (`vsend_err`) —
  when `curp->m_postsize >= 0xffff || curp->m_count >= 0xffff`, C computes
  `size = sizeof(caHdr) + 2*sizeof(*pLW) = 24`; otherwise `size = sizeof(caHdr) = 16`.
  `cas_commit_msg(client, size)` uses the correct size to set `m_postsize` in the
  outer CA_PROTO_ERROR reply header.

Impact:

When a CA_PROTO_ERROR reply is sent in response to a large-array request (one that
used the extended 24-byte header — i.e. `m_postsize == 0xFFFF`), the outer
CA_PROTO_ERROR response header declares `m_postsize = 16 + N` (where N is the padded
diagnostic string length), but the actual payload sent on the wire is
`24 + N` bytes (the extended echoed request header plus the diagnostic). The TCP
receiver (C libca `exceptionRespAction`) advances by `align8(16 + N)` bytes after
reading the outer CA_PROTO_ERROR header, leaving 8 orphan bytes (the extended annex
of the echoed request header) in the TCP stream. These orphan bytes are then parsed
as the opcode of the next message, causing all subsequent messages on the connection
to be mis-framed. Affected commands: any CA_PROTO_ERROR sent in response to a
large-array READ_NOTIFY, WRITE_NOTIFY, or EVENT_ADD whose element count or payload
size was >= 0xFFFF.

Fix direction:

Move `let orig_bytes = original_hdr.to_bytes_extended()` before the `payload_size`
calculation and change `payload_size` to use `orig_bytes.len()` instead of
`CaHeader::SIZE`. This makes the declared `m_postsize` exactly equal to the actual
bytes sent (echo header length + padded diagnostic), covering both the 16-byte and
24-byte echo cases with one formula.

### R46 — EVENT_ADD with mask=0 silently installs dead subscription; C sends ECA_ALLOCMEM + disconnects

Severity: Medium

Status: Fixed

Evidence:

- **Rust**: `crates/epics-ca-rs/src/server/tcp.rs:3160-3164` — mask is extracted from
  `payload[12..13]` (the `mon_info.m_mask` field), but no guard checks for zero before
  passing it to `pv.add_subscriber(sub_id, native_type, mask)`. A zero-mask subscription
  installs in the subscriber Vec, the initial snapshot is delivered to the client, but
  `Subscriber::accepts(post)` always returns `false` (mask bits never intersect any event
  class), so no further events ever arrive.
- **C**: `modules/database/src/ioc/db/dbEvent.c:437-439` — `db_add_event` guards
  `if (select==0 || select > UCHAR_MAX) return NULL;`. `select > UCHAR_MAX` also covers
  masks with bits above bit 7 set (nonexistent DBE classes). Back in
  `modules/database/src/ioc/rsrv/camessage.c:1814-1822` — the NULL return triggers
  `send_err(mp, ECA_ALLOCMEM, ...)` (CA_PROTO_ERROR on wire) followed by
  `return RSRV_ERROR` which closes the connection.

Impact:

A client that sends EVENT_ADD with mask=0 (always a client bug; no standard CA library
does this) gets different treatment:
- C IOC: ECA_ALLOCMEM error reply (CA_PROTO_ERROR) + connection closed.
- Rust server: subscription installed (wastes a subscriber slot), initial snapshot
  delivered (CA_PROTO_EVENT_ADD with ECA_NORMAL status), then silence. The client thinks
  the monitor is live but never receives further events — a silent hang.

Fix direction:

After extracting `mask` at line 3164, add:
```rust
if mask == 0 {
    send_ca_error(writer, hdr, ECA_ALLOCMEM, entry.cid,
        "EVENT_ADD mask=0: no events would be triggered").await?;
    return Err(CaError::Protocol("EVENT_ADD mask=0".into()));
}
```
The CA_PROTO_ERROR reply matches C `send_err(ECA_ALLOCMEM)`; the `Err` return closes the
connection matching C `RSRV_ERROR`. A mask that only has bits above bit 7 set (> 0x0F) is
not guarded separately since DBE bits 4-7 are reserved and such a mask would never match
any C DBE category; the Rust `Subscriber::accepts` already yields false for them.

### R47 — `DBE_PROPERTY` events never posted on metadata-field write; subscribers silent after initial snapshot

Severity: Medium

Status: Fixed

Evidence:

- **Rust**: `crates/epics-base-rs/src/server/record/record_instance.rs:260-268` —
  `notify_field_written_if_changed` calls `invalidate_metadata_cache()` when a
  metadata field (EGU/HOPR/LOPR/PREC/HIHI/LOLO/enum strings) actually changes, but
  never calls `notify_field_with_origin(*, EventMask::PROPERTY)`. The four write-path
  callers in `crates/epics-base-rs/src/server/database/field_io.rs:151,326,630,835`
  all rely on this function for metadata invalidation. A second path at
  `record_instance.rs:1499-1500` (`took_metadata_change()` during record processing)
  also only invalidates the cache without posting PROPERTY.
- **C**: `modules/database/src/ioc/db/dbAccess.c:1396-1397` — `dbPutField` sets
  `propertyUpdate = paddr->pfldDes->prop && precord->mlis.count` (line 1329), then
  after the write and optional change-suppression (lines 1374-1383), calls
  `db_post_events(precord, NULL, DBE_PROPERTY)` when `propertyUpdate && !status`.
  The `NULL` field pointer broadcasts to all field monitors on the record.

Impact:

CA clients that subscribe with `DBE_PROPERTY` mask (0x08) to monitor EGU/HOPR/LOPR/
PREC/enum-string changes receive the initial snapshot delivered at EVENT_ADD time
but are silent thereafter. Every subsequent write to a metadata field that changes
its value goes undelivered to those subscribers. The PVA gateway (`epics-bridge-rs`)
subscribes with `EventMask::PROPERTY` for NTScalar `display.units`/`display.form`
propagation; an IOC record writing its own EGU will not propagate that change to
PVA clients.

Fix direction:

In `notify_field_written_if_changed`, after `self.invalidate_metadata_cache()`, add:
```rust
let fields: Vec<String> = self.subscribers.keys().cloned().collect();
for f in fields {
    self.notify_field_with_origin(&f, EventMask::PROPERTY, 0);
}
```
This broadcasts the PROPERTY event to all field subscribers (only those with the
PROPERTY bit in their mask receive it, via the `sub_mask.intersects(mask)` gate in
`notify_field_with_origin`), matching C's `db_post_events(precord, NULL, DBE_PROPERTY)`
broadcast. Also add the same broadcast in `process_local`'s `took_metadata_change()`
block so record-processing-driven metadata changes are covered.

### R48 — non-VAL field writes on Passive scan records suppress VALUE|LOG events

Severity: Low

Status: Fixed

Evidence:

- **Rust**: `crates/epics-base-rs/src/server/database/field_io.rs:652` —
  `if instance.common.scan != ScanType::Passive && field != "VAL"` gates the
  VALUE|LOG `notify_field` call on the record not being Passive. For Passive-scan
  records, all non-VAL field writes (DESC, SCAN, PHAS, EGU, HOPR, LOPR, etc.) skip
  the event post; monitors on those fields receive silence after the write.
- **C**: `modules/database/src/ioc/db/dbAccess.c:1409-1414` — `dbPut` calls
  `db_post_events(precord, pfieldsave, DBE_VALUE | DBE_LOG)` under the condition
  `!(isValueField && pfldDes->process_passive)`. For any field that is not the
  record's primary value field (`isValueField=false`), this condition is always true
  — VALUE|LOG events are posted for non-VAL field writes regardless of the record's
  scan type (`precord->scan`).

Impact:

CA clients monitoring non-VAL fields (DESC, SCAN, PHAS, EGU, HOPR, LOPR, or other
auxiliary fields) on Passive scan records do not receive VALUE|LOG monitor events
after a CA write. The initial EVENT_ADD snapshot is delivered, but subsequent writes
are silent. Records with Periodic or Event scan modes are unaffected (the existing
non-Passive branch posts correctly).

Fix direction:

Remove the `scan != ScanType::Passive` guard from the condition at `field_io.rs:652`.
Change:
```rust
if instance.common.scan != ScanType::Passive && field != "VAL" {
```
to:
```rust
if field != "VAL" {
```
This matches C's `!(isValueField && pfldDes->process_passive)` logic where VAL is the
only field suppressed (because Passive records process VAL through `dbProcess`, which
posts VALUE events during the processing cycle). Non-VAL fields have no equivalent
deferred-posting path and must be notified immediately after the write.

### R49 — `CaServer` has no public `notify_access_change()` method; INP*-driven re-notification impossible

Severity: Low

Status: Fixed

Evidence:

- **Rust**: `crates/epics-ca-rs/src/server/ca_server.rs:434` —
  `acf_reload_tx: broadcast::Sender<()>` is a private field. The only paths that
  send on it are `reload_acf_inner` (triggered by `reload_acf_from` — file-based)
  and the ASG-field-change forwarder task in `run_tcp_listener` (triggered by
  `notify_asg_field_changed` when the `ASG` record field is written). No public
  method exists on `CaServer` to fire the broadcast programmatically, so library
  code that monitors INP* link values for CALC-gated ACF rules has no way to
  trigger `reeval_access_rights` on connected clients.
- **C**: `modules/database/src/ioc/as/asCa.c:137,161` — `eventCallback` and
  `connectCallback` call `asComputeAsg(pasg)` when an INP* link value changes or
  disconnects. `modules/database/src/ioc/as/asCa.c:205` — `asComputeAllAsg()` is
  called after ACF initialisation. Both paths propagate through
  `asComputePvt` → `casAccessRightsCB(asClientCOAR)` →
  `camessage.c:1057-1101` → `access_rights_reply` → `CA_PROTO_ACCESS_RIGHTS` for
  every affected already-connected channel. C exposes `asComputeAsg` as a public
  subsystem hook; Rust has no equivalent at the `CaServer` API level.

Impact:

Library code that monitors INP* link values for CALC-gated ACF rules (or that
changes access-security state programmatically in other ways) cannot trigger
`CA_PROTO_ACCESS_RIGHTS` re-push for connected clients. The ACF-file-reload and
ASG-field-change paths are correctly wired; only the programmatic trigger is missing.

Fix direction:

Add `pub fn notify_access_change(&self)` to `CaServer` that sends `()` on the
`acf_reload_tx` broadcast. This is the Rust equivalent of calling
`asComputeAllAsg()` — it prompts every connected TCP client's select loop to run
`reeval_access_rights`, which re-pushes `CA_PROTO_ACCESS_RIGHTS` only when the
computed access level actually changes (`oldaccess != access` filter, R2-51 parity).

## Uncertain Candidates

None identified. All other areas checked (EVENT_ADD mask extraction at offset 12,
CREATE_CHAN response field layout, READ_NOTIFY field layout, WRITE_NOTIFY field
layout, beacon m_available = 0, repeater registration noop, client CA_PROTO_ERROR
parsing with extended-echo handling, search reply cid sentinel, ECA status code
table, CLEAR_CHANNEL reply field echo, READ_SYNC echo, ECHO full-payload round-trip,
monitor flow-control lost-wake-safe gate, CA access security / access-rights frame
layout, large-array / dynamic element count autosize + padding + truncation +
extended-header) were found to match C behavior or be intentional, documented
deviations.
