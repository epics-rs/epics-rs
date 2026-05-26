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

### R50 — `int64in` missing from `populate_control_info`; DBR_CTRL_LONG/DOUBLE returns zeroed control limits

Severity: Low

Status: Fixed

Evidence:

- **Rust**: `crates/epics-base-rs/src/server/record/record_instance.rs` —
  `populate_control_info` match arm `"ai" | "longin" | "calc" | "calcout"` uses
  HOPR/LOPR as control limits. `int64in` is absent from every arm; the `_ => {}`
  wildcard leaves `snap.control = None`, so `encode_ctrl` encodes
  `upper_ctrl_limit = 0` / `lower_ctrl_limit = 0` for all `int64in` channels.
- **C**: `modules/database/src/std/rec/int64inRecord.c:226-227` —
  `int64inRecord::get_control_double` sets
  `pcd->upper_ctrl_limit = prec->hopr; pcd->lower_ctrl_limit = prec->lopr;`.
  Parity with `longinRecord.c:231-232` which also uses HOPR/LOPR.

Impact:

CA clients that request `DBR_CTRL_LONG` (or `DBR_CTRL_DOUBLE`) for an `int64in`
channel receive `upper_ctrl_limit = 0` / `lower_ctrl_limit = 0` regardless of the
record's HOPR/LOPR settings. Control-panel widgets that respect control limits
(sliders, spin boxes) display unconstrained ranges for `int64in` channels.

Fix direction:

Add `"int64in"` to the `"ai" | "longin" | "calc" | "calcout"` arm in
`populate_control_info`.

### R51 — `longout`/`int64out` control limits zeroed when DRVH=DRVL=0; HOPR/LOPR fallback missing

Severity: Low

Status: Fixed

Evidence:

- **Rust**: `crates/epics-base-rs/src/server/record/record_instance.rs` —
  `populate_control_info` arm `"ao" | "longout" | "int64out"` reads DRVH/DRVL
  with `.unwrap_or(hopr)` / `.unwrap_or(lopr)`. Because DRVH/DRVL default to
  `0.0` (always present), the `unwrap_or` fallback never fires; when DRVH=DRVL=0.0
  (the typical "not configured" state), the encoded control limits are 0/0.
- **C**: `modules/database/src/std/rec/longoutRecord.c:282-287` and
  `int64outRecord.c:265-270` — `get_control_double` uses
  `if(prec->drvh > prec->drvl) { drvh/drvl } else { hopr/lopr }`.
  `aoRecord.c:356-357` does NOT have this guard — it always uses DRVH/DRVL.

Impact:

`longout` and `int64out` channels whose DRVH/DRVL are left at their default
values (0.0/0.0) report `upper_ctrl_limit = 0` / `lower_ctrl_limit = 0` instead
of HOPR/LOPR. This is the common case for output records that rely on HOPR/LOPR
for both display and control range. `ao` is unaffected (C also uses unconditional
DRVH/DRVL for `ao`).

Fix direction:

Split the `"ao" | "longout" | "int64out"` arm into two arms:
- `"ao"`: unconditionally use DRVH/DRVL (matching `aoRecord.c:356-357`).
- `"longout" | "int64out"`: fetch both DRVH and DRVL; if `drvh > drvl` use them,
  else use HOPR/LOPR (matching `longoutRecord.c:282-287` /
  `int64outRecord.c:265-270`).

### R52 — `waveform`/`aai`/`aao`/`compress` missing from `populate_display_info`; DBR_GR_* returns zeroed display limits

Severity: Low

Status: Fixed

Evidence:

- **Rust**: `crates/epics-base-rs/src/server/record/record_instance.rs` —
  `populate_display_info` has arms for `ai|ao|calc|calcout`,
  `longin|longout|int64in|int64out`, and `motor` only. `waveform`, `aai`, `aao`,
  and `compress` all fall through to `_ => {}` so `snap.display` stays `None`;
  `encode_gr` / `encode_ctrl` encode zeroed limits for these PVs.
  Compound gap: `crates/epics-base-rs/src/server/records/waveform.rs` has
  `egu`/`hopr`/`lopr`/`prec` struct fields but `get_field` returns `None` for all
  four; `put_field` returns `FieldNotFound` for them. `CompressRecord` lacks those
  struct fields entirely.
- **C**: `modules/database/src/std/rec/waveformRecord.c:251-252` —
  `get_graphic_double` sets `pgd->upper_disp_limit = prec->hopr` /
  `pgd->lower_disp_limit = prec->lopr` for the VAL field; line 239 returns
  `*precision = prec->prec`.
  `aaiRecord.c:280-281`, `aaoRecord.c:283-284` — identical HOPR/LOPR assignment.
  `compressRecord.c:478-479` — same for VAL/IHIL/ILIL; line 464 returns
  `prec->prec`. All four also expose EGU via `get_units` (waveformRecord.c:230,
  compressRecord.c:455).

Impact:

CA clients that request `DBR_GR_DOUBLE` (or any `DBR_GR_*` / `DBR_CTRL_*` type)
for a `waveform`, `aai`, `aao`, or `compress` PV receive `upper_disp_limit = 0`,
`lower_disp_limit = 0`, `precision = 0`, and `units = ""` regardless of the
record's HOPR/LOPR/PREC/EGU settings. Control-panel widgets show unconstrained
ranges and no engineering units for all array PVs.

Fix direction:

1. `waveform.rs` — add EGU, HOPR, LOPR, PREC to `get_field` (returning the
   stored struct fields) and to `put_field` (storing into the struct).
2. `compress.rs` — add `egu`/`hopr`/`lopr`/`prec` struct fields + Default values,
   then expose them in `get_field` and `put_field`.
3. `record_instance.rs` — add `"waveform" | "aai" | "aao"` and `"compress"` arms
   to `populate_display_info`, sourcing EGU/PREC/HOPR/LOPR from `get_field`.
   Neither record type has analog alarm limits (no HIHI/HIGH/LOW/LOLO), so those
   fields remain 0.0.

### R53 — `bi`/`bo` and `mbbi`/`mbbo` enum-string `no_str` not trimmed; trailing empty strings inflated

Severity: Low

Status: Fixed

Evidence:

- **Rust**: `crates/epics-base-rs/src/server/record/record_instance.rs` —
  `populate_enum_info` `"bi" | "bo" | "busy"` arm unconditionally produces
  `strings: vec![znam, onam]` (length 2, no_str=2). `"mbbi" | "mbbo"` arm collects
  all 16 `*ST` fields unconditionally (length 16, no_str=16). The codec
  (`crates/epics-base-rs/src/types/codec.rs:524`) derives `no_str` directly as
  `ei.strings.len().min(16)`.
- **C** (bi/bo): `boRecord.c:342-352` — `get_enum_strs` starts at `no_str=2`,
  then applies: `if (*prec->znam != 0) no_str=1; if (*prec->onam != 0) no_str=2;`.
  Result: `no_str=1` when ZNAM is set and ONAM is empty. Comment reads
  "SETTING no_str=0 breaks channel access clients."
- **C** (mbbi/mbbo): `mbbiRecord.c:262-269` — `get_enum_strs` uses a highwater
  mark: `if (*pstate != 0) states = i+1; … pes->no_str = states;`. Result: `no_str`
  equals the index of the last non-empty string + 1; all-empty → `no_str=0`.

Impact:

`bi`/`bo` PVs whose ZNAM is set but ONAM is unset return `no_str=2` with an empty
string in slot 1; C returns `no_str=1`. `mbbi`/`mbbo` PVs with fewer than 16 states
configured return `no_str=16`; C returns the count of actually-defined states. CA
clients (control-panel widgets, camonitor) display extra blank enum entries beyond
the configured states.

Fix direction:

1. `record_instance.rs` `"bi" | "bo" | "busy"` arm — apply C's logic: evaluate
   `!znam.is_empty() && onam.is_empty()` before moving into the vec, then
   `truncate(1)` when true.
2. `record_instance.rs` `"mbbi" | "mbbo"` arm — apply C's highwater-mark:
   `rposition(|s| !s.is_empty()).map(|i| i+1).unwrap_or(0)` then `truncate(no_str)`.

### R55 — `$` long-string suffix not parsed; `RECORD.FIELD$` returns `CREATE_CH_FAIL`

Severity: Medium

Status: Fixed

Evidence:

- **Rust**: `crates/epics-ca-rs/src/server/tcp.rs` — `CA_PROTO_CREATE_CHAN` handler:
  `parse_pv_name(&record_path)` splits `RECORD.DESC$` into field `"DESC$"` (dollar
  included). `instance.resolve_field("DESC$")` returns `None` → the handler sends
  `CA_PROTO_CREATE_CH_FAIL`. `split_channel_name` only strips JSON `{...}` suffixes;
  `$` is never recognised. `spawn_monitor_sender` (`monitor.rs`) likewise has no
  long-string conversion path.
- **C**: `modules/database/src/ioc/db/dbChannel.c:483-507` — when `*pname == '$'`
  and the field is `DBF_STRING`: `paddr->no_elements = paddr->field_size` (= 40 =
  `MAX_STRING_SIZE`), `paddr->field_type = DBF_CHAR`, `paddr->dbr_field_type =
  DBR_CHAR`. The channel then behaves as a `DBR_CHAR` array of up to 40 elements.
  For link fields (`DBF_INLINK`/`OUTLINK`/`FWDLINK`): `paddr->no_elements =
  PVLINK_STRINGSZ` (= 1024). For all other field types: returns
  `S_dbLib_fieldNotFound` (wire equivalent: `CREATE_CH_FAIL`).

Impact:

CA clients that append `$` to a field name (e.g. `caget PV.DESC$`) to read the
string as a `DBR_CHAR` array — bypassing the 40-byte `DBR_STRING` truncation for
long-string records, or consuming string fields as raw bytes — receive
`CA_PROTO_CREATE_CH_FAIL` from the Rust server instead of a `DBR_CHAR` channel.
Tools like `caget -S PV.DESC` (which maps to a `$` channel under the hood) and any
CA client using the long-string convention fail silently.

Fix direction:

1. `tcp.rs` `CREATE_CHAN` handler: after `parse_pv_name`, detect trailing `$` on the
   field name, strip it, look up the bare field. Verify the value is
   `EpicsValue::String`; if not, send `CREATE_CH_FAIL` (C parity:
   `S_dbLib_fieldNotFound`). If it is a string field: override `dbr_type =
   DbFieldType::Char` and `element_count = 40` (C parity:
   `paddr->field_size = MAX_STRING_SIZE`). Store `long_string = true` in
   `ChannelEntry` and `SubscriptionEntry`.
2. `tcp.rs` READ handler: if `entry.long_string`, convert `snapshot.value` from
   `EpicsValue::String(s)` to `EpicsValue::CharArray(s_bytes + NUL)` before
   `encode_dbr`.
3. `tcp.rs` EVENT_ADD handler: capture `long_string`; apply the same conversion to
   the initial snapshot and inside the per-event monitor-task loop (both RecordField
   and SimplePv paths). Store `long_string` in `SubscriptionEntry` so the
   access-restore path can apply it too.
4. `monitor.rs` `spawn_monitor_sender` / `send_event`: add `long_string: bool`;
   apply the same conversion before `encode_dbr`.
5. Access-restore path: read `sub.long_string` from `SubscriptionEntry` and apply
   conversion before `send_monitor_snapshot`.

Note: link fields (`DBF_INLINK`/`OUTLINK`/`FWDLINK`) with `$` (C uses
`PVLINK_STRINGSZ = 1024`) are not covered — the Rust server does not distinguish
link-field types at the `get_field` level. The fix covers all `EpicsValue::String`
results, which corresponds to `DBF_STRING` fields in C.

### R56 — DBR_GR_*/DBR_CTRL_* alarm limits not severity-gated; ungated limits sent verbatim instead of NaN/0

Severity: Medium

Status: Fixed

Evidence:

- **Rust**: `crates/epics-base-rs/src/server/record/record_instance.rs:780-786` —
  `alarm_limits()` returns `(aa.hihi, aa.high, aa.low, aa.lolo)` UNCONDITIONALLY,
  and both `populate_display_info` arms feed it into `DisplayInfo`:
  `"ai" | "ao" | "calc" | "calcout"` (line 422) and
  `"longin" | "longout" | "int64in" | "int64out"` (line 457). The codec sources
  the four alarm-limit slots of every DBR_GR_*/DBR_CTRL_* response from
  `DisplayInfo` (`crates/epics-base-rs/src/types/codec.rs:429-432`, GR `limits[2..5]`
  / CTRL `limits[2..5]`).
- **C**: `get_alarm_double` is severity-gated for the analog records —
  `modules/database/src/std/rec/aiRecord.c:295-298`
  (`pad->upper_alarm_limit = prec->hhsv ? prec->hihi : epicsNAN`),
  `aoRecord.c:368-372`, `longinRecord.c:244-248`, `longoutRecord.c:300-304`,
  `calcRecord.c:263-267`, `calcoutRecord.c:538-542`. `int64inRecord.c:239-243`
  and `int64outRecord.c:283-287` are UNCONDITIONAL (no gating) — the asymmetry
  is real and must be preserved. `modules/database/src/ioc/db/dbAccess.c:294-326`
  shows the gated NaN is encoded as NaN for `DBR_AL_DOUBLE` (float/double DBR
  fields) and as `finite(ald) ? (epicsInt32)ald : 0` (i.e. **0**) for
  `DBR_AL_LONG` (integer DBR fields).

Impact:

CA clients that request DBR_GR_* / DBR_CTRL_* (control-panel widgets, `caget -a`)
for an analog record whose alarm severities are disabled (HHSV/HSV/LSV/LLSV =
NO_ALARM, the default) receive the raw HIHI/HIGH/LOW/LOLO field values (typically
0.0) where the C IOC returns NaN (float/double types) or 0 (integer types). A
widget that respects alarm limits draws an alarm marker at 0.0 instead of treating
the limit as "not configured" (NaN). Affects ai, ao, longin, longout, calc,
calcout. int64in/int64out are already correct (C does not gate them).

Fix direction:

Gate `alarm_limits()` with `f64::NAN` when the matching severity field is
`AlarmSeverity::NoAlarm` (and when `analog_alarm` is `None`, since C reads
prec->hhsv == 0 → NaN there too). Add `alarm_limits_unchecked()` returning the
raw limits for the int64in/int64out path, and select the source by record type
inside the `longin | longout | int64in | int64out` arm. The codec's existing
`f64 → i16/i32/i8` casts make `NaN as iN == 0` (matching C's
`finite()?cast:0`) and `f64 → f32` preserves NaN, so storing NaN in `DisplayInfo`
is byte-exact for every DBR variant — no codec change needed.

### R57 — numeric→DBR_STRING conversion ignores record precision (cvtDoubleToString parity)

Severity: Medium

Status: Fixed

Evidence:

- **Rust**: `crates/epics-base-rs/src/types/value.rs:680` — `convert_to`'s
  `DbFieldType::String => EpicsValue::String(format!("{self}"))` formats a
  `Double`/`Float` value with Rust's default `Display` (e.g. `3.14`), with no
  access to the record's precision. `crates/epics-base-rs/src/types/codec.rs:287`
  (`encode_dbr`) routes every `*_STRING` request through
  `convert_and_serialize(String, value)` → `value.convert_to(String)`.
- **C**: `modules/database/src/ioc/db/dbConvert.c:772-799` (`getDoubleString`,
  the `[DBF_DOUBLE][DBR_STRING]` table entry) formats via
  `cvtDoubleToString(*psrc, pdst, precision)` where `precision` comes from the
  record's `get_precision` RSET (PREC field; default 6 when absent).
  `getFloatString` (`:731`) is the parallel for `DBF_FLOAT`.
  `modules/libcom/src/cvtFast/cvtFast.c:111-190` (`cvtDoubleToString`): fast
  path for `|val| <= 1e7` and `precision <= 8` uses a **round-half-up**
  algorithm (`(fraction + 5) / 10`); `precision > 8` or `|val| > 1e16` →
  `sprintf("%*.*e", precision+7, precision, val)`; `1e7 < |val| <= 1e16` →
  `sprintf("%.*f", min(precision,3), val)`. `cvtFloatToString` (`:32-109`) is
  the f32 analogue (thresholds `1e8` / width `precision+6`).

Impact:

CA clients that request a string representation of a numeric record (`caget -d
DBR_STRING PV`, `caget -s`, StripTool, simple scripts, display managers reading
the value as text) receive the wrong string from the Rust server: precision is
not applied at all. For an `ai` with `PREC=3` and `VAL=3.14`, the C IOC returns
`"3.140"` while Rust returns `"3.14"`; for `VAL=1.0` C returns `"1.000"` while
Rust returns `"1"`. Also affects the round-half-up boundary (`0.125` at `PREC=2`
→ C `"0.13"`, Rust `format!`-based `"0.12"`). Applies to every `*_STRING` DBR
variant for Double/Float-valued records.

Fix direction:

In `encode_dbr`, when `native == DbFieldType::String`, route the value through a
precision-aware converter that ports `cvtDoubleToString` / `cvtFloatToString`
byte-for-byte (round-half-up fast path, `%.*f` and C-style `%*.*e` fallbacks,
glibc `"nan"`/`"inf"` spellings), sourcing precision from
`snapshot.display.precision` (default 6 when `display` is `None`). Integer
fields keep the existing `to_string` path (C `getLongString` etc. carry no
precision, so Rust already matches). Scalar Double/Float and DoubleArray/
FloatArray are all converted element-wise with the same precision.

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
