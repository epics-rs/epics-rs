# epics-ca-rs C Parity Review - 2026-05-18

Scope: `crates/epics-ca-rs` only. Reference implementation is
`~/codes/epics-base`, primarily libca under `modules/ca/src/client` and
rsrv under `modules/database/src/ioc/rsrv`.

This file records compatibility defects found during review. It excludes
intentional Rust-only behavior unless it breaks a libca/rsrv wire contract.

## Open Findings

### R2-1: Access-rights changes incorrectly tear down subscriptions

Severity: High

Rust: `crates/epics-ca-rs/src/server/tcp.rs`, `reeval_access_rights()`
removes every subscription whose channel becomes `NoAccess`.

C reference: `rsrv/camessage.c::casAccessRightsCB()` keeps the event in the
channel event queue. On read access loss it posts one event, then disables
future delivery; on read access restore it enables the event and posts one
fresh event.

Impact: after an ACF reload or identity change that temporarily revokes read
access, an existing Rust-server subscription is permanently removed. If access
is later restored, the original `camonitor` does not resume. libca/rsrv keeps
the subscription alive across the access transition.

### R2-3: TCP nameserver parser can wedge on malformed frames

Severity: Medium

Rust: `crates/epics-ca-rs/src/client/search.rs`,
`run_nameserver_connection()` breaks only the inner frame loop on malformed
extended headers or misaligned `postsize`. The bad prefix remains in
`accumulated`, so every later read reparses the same bytes. There is no
accumulation cap in this nameserver path.

C reference: `libca/tcpiiu.cpp::processIncoming()` returns false on
misaligned payloads, closing the TCP circuit and allowing reconnect.

Impact: a broken or hostile TCP name server can stall all searches routed
through that nameserver connection until process restart.

### R2-4: TCP VERSION priority is not range-checked

Severity: Low

Rust: `crates/epics-ca-rs/src/server/tcp.rs`, the `CA_PROTO_VERSION` handler
checks only the minor version before accepting the frame.

C reference: `rsrv/camessage.c::tcp_version_action()` rejects
`m_dataType > CA_PROTO_PRIORITY_MAX` with `RSRV_ERROR`.

Impact: malformed clients with impossible priority values are accepted by the
Rust server while rsrv drops the virtual circuit. The current Rust server does
not use the priority for scheduling, so this is mostly strict wire-contract
parity and input validation.

### R2-5: EVENT_ADD rejects no-read-access subscriptions instead of installing them disabled

Severity: High

Rust: `crates/epics-ca-rs/src/server/tcp.rs`, the `CA_PROTO_EVENT_ADD` handler
checks `state.lookup_access(sid).require_read()` before installing the
subscription. On denial it sends a command-specific error and returns without
adding any subscriber state.

C reference: `rsrv/camessage.c::event_add_action()` allocates `event_ext`,
adds it to the channel event queue, calls `db_add_event()`, and always posts
the initial event. It then enables future event delivery only when
`asCheckGet()` allows reads; otherwise the event remains installed but disabled.
The denied initial/update frame is produced through `no_read_access_event()`.

Impact: a subscription opened while read access is denied is permanently absent
on the Rust server. If access is later granted, the C IOC enables the existing
subscription and posts a fresh event, while the Rust server has no subscription
to resume.

### R2-6: READ_NOTIFY bad DBR type sends a reply that rsrv does not send

Severity: Medium

Rust: `crates/epics-ca-rs/src/server/tcp.rs`, the combined
`CA_PROTO_READ | CA_PROTO_READ_NOTIFY` branch treats `encode_dbr()` failure as
`ECA_BADTYPE`; for `READ_NOTIFY` it emits a `CA_PROTO_READ_NOTIFY` error frame
and then disconnects.

C reference: `rsrv/camessage.c::read_notify_action()` checks
`INVALID_DB_REQ(mp->m_dataType)` and returns `RSRV_ERROR` immediately. It does
not call `send_err()` or emit a `CA_PROTO_READ_NOTIFY` error frame. The
command that sends `send_err(ECA_BADTYPE)` is the deprecated
`read_action()`, not `read_notify_action()`.

Impact: malformed `READ_NOTIFY` receives an extra wire frame from the Rust
server before EOF. The regression test
`server_read_notify_bad_type_replies_error_and_disconnects()` also cites
`read_action()` for a `READ_NOTIFY` case, so the current test encodes the wrong
C reference behavior.

### R2-7: READ_NOTIFY read-denied response has the wrong payload shape

Severity: Medium

Rust: `crates/epics-ca-rs/src/server/tcp.rs`, read-access denial in the
`READ_NOTIFY` path calls `send_cmd_error()`, which sends a zero-payload response
with `count = 0`.

C reference: `rsrv/camessage.c::read_notify_action()` routes through
`read_reply()`. On read denial, `read_reply()` calls
`no_read_access_event()`, which sends the original command with
`m_dataType = request type`, `m_count = request count`, `m_cid =
ECA_NORDACCESS`, `m_available = ioid`, and a zero-filled payload sized as
`dbr_size_n(type, count)`.

Impact: libca-style clients see different callback metadata and wire framing
for the same no-read-access `caget` callback path: Rust reports count zero and
no payload, while rsrv reports the requested count and a full zeroed DBR body.

### R2-8: WRITE_NOTIFY error replies lose the request count

Severity: Low

Rust: `crates/epics-ca-rs/src/server/tcp.rs`, `send_cmd_error()` is used for
some `CA_PROTO_WRITE_NOTIFY` failures, including bad DBR type and payload
conversion errors. The helper always sets `count = 0`.

C reference: `rsrv/camessage.c::putNotifyErrorReply()` sends
`CA_PROTO_WRITE_NOTIFY` with `m_dataType = mp->m_dataType`,
`m_count = mp->m_count`, `m_cid = statusCA`, and `m_available =
mp->m_available`.

Impact: strict wire parity is broken for failed put-callback replies. The most
visible user-facing status code is still in `m_cid`, but exception metadata and
raw frame consumers get `count = 0` from Rust where rsrv preserves the request
count.

### R2-9: WRITE_NOTIFY callbacks are not serialized per channel

Severity: Medium

Rust: `crates/epics-ca-rs/src/server/tcp.rs`, async `WRITE_NOTIFY` completion
spawns one task per request and records only abort handles in
`state.write_notify_tasks`. There is no per-channel busy slot that blocks or
cancels a later `WRITE_NOTIFY` while an earlier one is still in progress.

C reference: `rsrv/camessage.c::write_notify_action()` stores one
`pciu->pPutNotify` per channel. If it is busy, rsrv waits, cancels on timeout,
and can report `ECA_PUTCBINPROG` before accepting the next put-notify for that
channel.

Impact: Rust can run multiple put-callback operations concurrently on the same
channel. Completion callbacks can overlap or reorder relative to rsrv, and a
record implementation that relies on rsrv's per-channel serialization can see
reentrant writes from the Rust server.

### R2-10: UDP search parsing continues after unsupported-version frames

Severity: Low

Rust: `crates/epics-ca-rs/src/server/udp.rs`, the UDP responder accepts any
`CA_PROTO_VERSION` frame without checking `CA_VSUPPORTED`, and for
`CA_PROTO_SEARCH` with `count < 4` it skips that one search and continues
parsing later messages in the same datagram.

C reference: `rsrv/camessage.c::udp_version_action()` and
`search_reply_udp()` return `RSRV_ERROR` when the minor version is unsupported.
The UDP dispatcher breaks out of the current datagram on any non-OK status.

Impact: a malformed datagram can place an old/unsupported VERSION or SEARCH
before a valid SEARCH and still receive a Rust reply for the later message.
rsrv drops the rest of the datagram after the unsupported-version frame.

### R2-11: UDP search response VERSION state leaks across datagrams

Severity: Low

Rust: `crates/epics-ca-rs/src/client/search.rs`, `handle_udp_response()` stores
the last `CA_PROTO_VERSION` marker in `state.last_valid_seq` and never resets
it at the start of a new UDP datagram. A `CA_PROTO_SEARCH` response with no
VERSION is dropped before any VERSION has ever been seen, but the same shape is
accepted after an unrelated earlier datagram set `last_valid_seq`.

C reference: `libca/udpiiu.cpp::postMsg()` resets
`lastReceivedSeqNoIsValid` and `lastReceivedSeqNo` at the start of every UDP
datagram. `versionAction()` can only mark the current datagram as carrying a
valid sequence.

Impact: search response handling depends on prior packets instead of only the
current datagram. This breaks libca's datagram-local VERSION semantics and can
make legacy or third-party search replies inconsistent: first packet dropped,
later equivalent packet accepted after an unrelated VERSION-bearing response.

### R2-12: EVENT_ADD ignores the requested element count

Severity: Medium

Rust: `crates/epics-ca-rs/src/server/tcp.rs`, the `CA_PROTO_EVENT_ADD` handler
records only `requested_type` in `SubscriptionEntry`. Initial monitor delivery
and later monitor tasks call `send_monitor_snapshot()` or build an
`EVENT_ADD` frame using `snapshot.value.count()` for the response count. The
request's `hdr.actual_count()` is not stored or applied.

C reference: `rsrv/camessage.c::event_add_action()` stores the original request
header in `pevext->msg`. Monitor delivery uses `read_reply()`, which derives
`item_count` from `pevext->msg.m_count` unless the request count is zero
autosize. For non-autosize requests, the response header count remains the
requested count; data past the real element count is zero-filled.

Impact: `ca_create_subscription(type, count=1, ...)` on a waveform should
receive one element from rsrv, but the Rust server sends the full waveform.
For requested counts larger than the PV's current element count, rsrv preserves
the requested count and pads zeros, while Rust reports only the actual count.

### R2-13: READ_NOTIFY does not pad short arrays up to the requested count

Severity: Medium

Rust: `crates/epics-ca-rs/src/server/tcp.rs`, the
`CA_PROTO_READ | CA_PROTO_READ_NOTIFY` path truncates when
`requested_count < snapshot.value.count()`, but when the client requests more
elements than exist it leaves the snapshot unchanged and sends
`snapshot.value.count()` in the response header.

C reference: `rsrv/camessage.c::read_reply()` allocates and headers the reply
with `item_count = mp->m_count` for non-zero request counts. After
`dbChannel_get_count()`, if fewer elements are returned than requested,
`read_reply()` zero-fills the remaining payload bytes and keeps the original
request count in the header.

Impact: `ca_array_get_callback(type, count > native_count, ...)` sees a shorter
Rust response than it would from rsrv. Clients that allocate or validate based
on requested count observe different callback metadata and payload length.

### R2-14: CA_PROTO_WRITE put failures are silent

Severity: High

Rust: `crates/epics-ca-rs/src/server/tcp.rs`, after a regular
`CA_PROTO_WRITE` passes access checks and payload conversion, `write_result` is
only used for audit unless the command is `CA_PROTO_WRITE_NOTIFY`. For
non-notify writes, a failed record write or failed `SimplePv` write hook
produces no wire error. The same silent result applies to the non-notify
`DBR_PUT_ACKT` / `DBR_PUT_ACKS` branch.

C reference: `rsrv/camessage.c::write_action()` is fire-and-forget only on
success. If `dbChannel_put()` returns failure, rsrv sends `send_err(...,
ECA_PUTFAIL, ...)` and keeps the connection open. It also sends errors for
access denial and conversion failure.

Impact: a failed `caput` without callback can look successful against the Rust
server: no exception frame reaches the client even though the value was not
stored. C libca clients receive the normal `CA_PROTO_ERROR` exception path from
rsrv.

### R2-15: Channel-scoped CA_PROTO_ERROR replies use SID where C uses client CID

Severity: Medium

Rust: `crates/epics-ca-rs/src/server/tcp.rs`, several `send_ca_error()` calls
in READ/WRITE paths pass `hdr.cid` as the outer `CA_PROTO_ERROR.m_cid`.
For these request opcodes, `hdr.cid` is the server-side channel id (SID). The
client-side channel id is stored on the channel entry as `entry.cid`.

C reference: `rsrv/camessage.c::vsend_err()` handles
`CA_PROTO_EVENT_ADD`, `CA_PROTO_EVENT_CANCEL`, `CA_PROTO_READ`,
`CA_PROTO_READ_NOTIFY`, `CA_PROTO_WRITE`, and `CA_PROTO_WRITE_NOTIFY` by
looking up the `channel_in_use` from request `m_cid`, then putting `pciu->cid`
in the outer error header. Only commands without a channel use
`0xffffffff`.

Impact: libca exception handling receives a channel identifier that it did not
allocate. Error callbacks for denied or malformed READ/WRITE requests can be
misattributed or fail channel lookup in clients that validate the outer
`m_cid`.

### R2-16: WRITE bad-type handling runs before the channel-id check

Severity: Medium

Rust: `crates/epics-ca-rs/src/server/tcp.rs`, the regular WRITE branch calls
`DbFieldType::from_u16(hdr.data_type)` before looking up `state.channels` for
`hdr.cid`. If both the SID and DBR type are bad, Rust emits a BADTYPE error
reply and disconnects.

C reference: `rsrv/camessage.c::write_action()` and
`write_notify_action()` call `MPTOPCIU(mp)` first. A bad SID goes through
`logBadId()` and returns `RSRV_ERROR` with no wire reply. The DBR type check is
only reached after the channel id resolves.

Impact: malformed clients can observe an error frame from Rust for a request
where rsrv would silently close. This also feeds into R2-15 because the bad-type
error path has no resolved client CID when it builds the `CA_PROTO_ERROR`.

### R2-17: Client CA_PROTO_ERROR handling does not wake operation callbacks

Severity: Medium

Rust: `crates/epics-ca-rs/src/client/transport.rs` parses
`CA_PROTO_ERROR`, extracts only the ECA status, original command, and diagnostic
string, then sends `TransportEvent::ServerError`. The echoed request header's
`m_available`, `m_dataType`, and `m_count` are not preserved, so the
coordinator cannot resolve an in-flight read, write, or subscription operation.

C reference: `libca/cac.cpp::exceptionRespAction()` reconstructs the echoed
request header and dispatches by original command through
`tcpExcepJumpTableCAC`. `readNotifyExcep()` and `writeNotifyExcep()` use
`hdr.m_available` to complete and uninstall the pending IO callback;
`eventAddExcep()` uses the subscription id to notify the monitor callback.

Impact: when a C IOC reports an operation failure through `CA_PROTO_ERROR`
instead of a command-specific status frame, Rust users get only the global
exception hook. The specific `get()` / `put()` / monitor receiver can remain
pending until timeout or miss the per-operation error that libca would deliver.

### R2-18: Client channel names are not bounded before SEARCH/CREATE_CHAN framing

Severity: Medium

Rust: `crates/epics-ca-rs/src/client/mod.rs`, `CaClient::create_channel()`
accepts any expanded string and immediately registers/schedules it.
`client/search.rs::build_search_payload()` and
`client/transport.rs::process_command(CreateChannel)` both put
`pad_string(pv_name).len() as u16` into the CA header while appending the full
payload.

C reference: `libca/cac.cpp::createChannel()` rejects null or empty names with
`ECA_BADSTR`. `libca/nciu.cpp::nciu()` rejects oversized channel names before
allocating the channel, and `libca/tcpiiu.cpp::createChannelRequest()` rejects
`postCnt >= 0xffff` with `ECA_UNAVAILINSERV` before sending
`CA_PROTO_CREATE_CHAN`.

Impact: an empty Rust channel starts a background search that can never match
instead of failing like libca. A very long PV name truncates the header size but
still appends the full body, producing malformed UDP SEARCH or TCP
CREATE_CHAN frames and potentially desynchronizing the peer's CA parser. libca
returns a client-side error before anything is put on the wire.

### R2-19: Client put paths do not enforce libca count/string bounds

Severity: High

Rust: `crates/epics-ca-rs/src/client/mod.rs`, `put()`,
`put_with_timeout()`, `put_nowait()`, `put_string()`,
`put_string_nowait()`, `put_string_array()`, and
`put_string_array_nowait()` serialize the supplied `EpicsValue` and send its
`value.count()` without checking `count <= snap.element_count`.
`epics-base-rs/src/types/value.rs::EpicsValue::to_bytes()` silently truncates
DBR_STRING elements to 39 bytes.

C reference: `libca/nciu.cpp::write()` and the write-callback overload reject
`countIn > this->count` before queueing the request, and
`nciu::stringVerify()` rejects DBR_STRING elements that exceed
`MAX_STRING_SIZE`. `cadef.h` documents `ECA_BADCOUNT` and `ECA_STRTOBIG` as
client-side outcomes.

Impact: Rust can send writes that libca refuses synchronously. The callback
variants surface a delayed server failure or timeout instead of the libca
status; the nowait variants can return `Ok(())` after putting a request on the
wire that C would reject. Long strings are silently truncated, so a Rust
`put_string()` can write a different value than the caller supplied.

### R2-20: Zero-length repeater registration bypasses the local-client gate

Severity: High

Rust: `crates/epics-ca-rs/src/repeater.rs`, `run_repeater_with_debug()`
registers `len == 0` datagrams before applying the loopback check used for
`CA_PROTO_REPEATER_REGISTER`. `fan_out()` then keeps clients by UDP port and
skips reflection by port only.

C reference: `repeater.cpp::ca_repeater()` sends both zero-length registration
and `REPEATER_REGISTER` through `register_new_client()`.
`register_new_client()` rejects non-AF_INET peers and requires loopback or a
bind test proving the source address is local.

Impact: a remote host that can reach the UDP repeater port can send a
zero-length datagram and become a registered fan-out recipient. That exposes
beacon traffic/PV presence outside the local host and can turn local beacon
traffic into repeated outbound sends to the remote address. C applies the same
locality rule to the legacy zero-length form.

### R2-21: Client CID/IOID/subscription IDs can wrap into live identifiers

Severity: Medium

Rust: `crates/epics-ca-rs/src/channel.rs` uses process-global
`AtomicU32::fetch_add()` counters for CIDs, IOIDs, and subscription IDs, with
no zero skip, no free list, and no check against the live channel, read, write,
or subscription maps.

C reference: libca assigns network IO identifiers through owned tables such as
`ioTable.idAssignAdd()` in `libca/cac.cpp::writeNotifyRequest()` and
`readNotifyRequest()`, and channels through the CA context channel table
rather than a raw wrapping global counter.

Impact: long-running high-rate clients can reuse a live identifier after
2^32 allocations. Reusing an IOID can overwrite an in-flight waiter and route a
later READ_NOTIFY/WRITE_NOTIFY to the wrong operation or leave the original
caller pending. Reusing a subscription ID can collide with an active monitor.
This is reachable for process-lifetime clients; 100,000 operations/s wraps in
about 11.9 hours.

### R2-22: Send backpressure accounting can undercount pending frames

Severity: Medium

Rust: `crates/epics-ca-rs/src/client/transport.rs`, `send_frame()` increments
`pending_frames` with `fetch_add()`, but `write_loop()` decrements by `load()`
plus `store(prev.saturating_sub(drained))`.

C reference: libca's send watchdog treats a stalled virtual circuit as a
connection failure; the Rust counter is the local guard that decides when to
close a circuit whose unbounded write queue is no longer draining.

Impact: a concurrent `send_frame()` can increment between the write loop's
`load()` and `store()`, and the store overwrites that increment. Under
sustained producer activity the counter can drift below the real queued-frame
count, so the `SEND_BACKPRESSURE_FRAMES` disconnect threshold is not reliable.
A stalled TCP circuit can retain an unbounded queue longer than the guard
intends.

### R2-23: EVENT_CANCEL request/ack headers lose the subscription count

Severity: Low

Rust client: `crates/epics-ca-rs/src/client/transport.rs`,
`TransportCommand::Unsubscribe` sends `CA_PROTO_EVENT_CANCEL` with
`data_type`, `sid`, and `subid`, but leaves `count = 0`.

Rust server: `crates/epics-ca-rs/src/server/tcp.rs`, the successful
`CA_PROTO_EVENT_CANCEL` branch replies with `CA_PROTO_EVENT_ADD`,
`count = 0`, and `cid = ECA_NORMAL`. `SubscriptionEntry` stores only the
requested DBR type, so the server no longer has the original EVENT_ADD count
or original channel header fields needed to echo the stored monitor request.

C reference: `libca/tcpiiu.cpp::subscriptionCancelRequest()` includes the
subscription count in the cancel request. `rsrv/camessage.c::event_cancel_reply()`
sends the stored `pevext->msg` fields back with zero payload: original event
DBR type, original event count, original event `m_cid`, and original
subscription id. `libca/cac.cpp::eventAddRespAction()` ignores zero-payload
cancel confirmations, but the frame is still byte-visible on the wire.

Impact: strict CA clients or trace/replay tooling see a different cancel
confirmation from the Rust server, and strict servers see a different cancel
request from the Rust client. This shares the same missing-state root as
R2-12, but affects the explicit cancel handshake instead of monitor delivery.

### R2-24: EVENT_CANCEL with a bad channel id sends an error frame that rsrv does not send

Severity: Medium

Rust: `crates/epics-ca-rs/src/server/tcp.rs`, the `CA_PROTO_EVENT_CANCEL`
handler checks the flat subscription map first. If the subscription id does not
belong to the requested SID, it sends `CA_PROTO_ERROR/ECA_BADMONID` and then
disconnects. That branch also covers an unknown SID, because the SID lookup is
only used to choose the diagnostic CID/string after the BADMONID decision has
already been made.

C reference: `rsrv/camessage.c::event_cancel_reply()` calls `MPTOPCIU(mp)`
first. If the request's channel id is unknown or belongs to another client,
rsrv calls `logBadId()` and returns `RSRV_ERROR` without sending a wire error.
Only after a valid channel is resolved does rsrv search that channel's event
queue and send `ECA_BADMONID` for an unknown monitor id.

Impact: malformed peers can distinguish an invalid SID from a valid-SID/
invalid-monitor request against rsrv by whether an error frame is sent. The
Rust server sends the BADMONID error in both cases, so strict clients and
protocol tests observe an extra frame before EOF for the bad-SID case.

## Cleared During Review

### R2-2: Monitor status errors are already delivered

The initial candidate was that non-`ECA_NORMAL` `CA_PROTO_EVENT_ADD` frames were
warn-and-dropped. Current code has `TransportEvent::MonitorStatusError` in
`client/types.rs`, sends it from `client/transport.rs`, and routes it through
`SubscriptionRegistry::on_monitor_error()` in `client/mod.rs`.

## Review Log

- 2026-05-18: Started continuation pass after the initial wire-contract review.
  Findings R2-1, R2-3, and R2-4 recorded. R2-2 cleared by current code.
- 2026-05-18: Continued server command parity pass over READ_NOTIFY,
  WRITE_NOTIFY, and EVENT_ADD. Findings R2-5 through R2-9 recorded.
- 2026-05-18: Continued UDP search/client response pass. Findings R2-10 and
  R2-11 recorded.
- 2026-05-18: Continued TCP array count-contract pass. Findings R2-12 and
  R2-13 recorded.
- 2026-05-18: Continued WRITE error-surface pass. Finding R2-14 recorded.
- 2026-05-18: Continued CA_PROTO_ERROR channel-id audit. Findings R2-15 and
  R2-16 recorded.
- 2026-05-18: Continued client CA_PROTO_ERROR dispatch audit. Finding R2-17
  recorded.
- 2026-05-18: Continued client validation, repeater registration, identifier
  allocation, and transport backpressure pass. Findings R2-18 through R2-22
  recorded.
- 2026-05-18: Continued EVENT_CANCEL request/reply parity pass. Findings R2-23
  and R2-24 recorded.
