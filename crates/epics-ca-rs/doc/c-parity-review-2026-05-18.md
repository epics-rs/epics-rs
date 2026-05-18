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

### R2-23: EVENT_CANCEL request/ack headers truncate large subscription counts

Severity: Low

Rust client: `crates/epics-ca-rs/src/client/transport.rs`,
`TransportCommand::Unsubscribe` copies the original subscription count into
`hdr.count` and serializes with `hdr.to_bytes()`. Counts `>= 0xffff` are
therefore truncated into the 16-bit normal header instead of using the CA
extended header.

Rust server: `crates/epics-ca-rs/src/server/tcp.rs`, the successful
`CA_PROTO_EVENT_CANCEL` branch replies with `CA_PROTO_EVENT_ADD` and
`resp.count = sub.data_count as u16`, then serializes with `resp.to_bytes()`.
The normal-count case now echoes the stored subscription count, but the large
array case still cannot represent the count.

C reference: `libca/tcpiiu.cpp::subscriptionCancelRequest()` includes the
subscription count through `comQueSend::insertRequestHeader()`, which emits the
extended form when `nElem >= 0xffff`. `rsrv/camessage.c::event_cancel_reply()`
sends the stored `pevext->msg` fields back with zero payload, preserving the
original event count.

Impact: monitors on arrays with 65,535 or more requested elements can be
cancelled with a different count than the one libca sends and rsrv echoes.
Strict servers, trace/replay tooling, and protocol tests see a normal-form
cancel/ack where C uses extended framing.

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

### R2-25: Unknown CREATE_CHAN replies leak server-side channels

Severity: Medium

Rust: `crates/epics-ca-rs/src/client/transport.rs` converts every
`CA_PROTO_CREATE_CHAN` response into `TransportEvent::ChannelCreated`.
`crates/epics-ca-rs/src/client/mod.rs` handles that event only when the CID is
still present in `channels`; otherwise the response is ignored. If the user
drops a channel while it is `Connecting`, `CoordRequest::DropChannel` cancels
search and removes the channel without sending `CLEAR_CHANNEL` because no SID
is known yet.

C reference: `libca/cac.cpp::createChannelRespAction()` checks the client
channel table. If a V4.4+ `CREATE_CHAN` response arrives for an unknown client
CID, libca immediately sends `tcpiiu::clearChannelRequest(hdr.m_available,
hdr.m_cid)` to free the server SID it just learned.

Impact: a Rust client can drop a channel after sending `CREATE_CHAN` but before
the server response arrives. The server allocates the channel and sends the
SID; Rust ignores the response, so the server-side channel remains allocated
until the TCP circuit closes. libca cleans up that race as soon as the late
response is parsed.

### R2-26: Search response freshness handling rejects unsequenced replies and accepts stale sequenced replies

Severity: High

Rust: `crates/epics-ca-rs/src/client/search.rs`, `run_nameserver_connection()`
feeds TCP name-server responses into the same `handle_udp_response()` path used
for UDP. That parser resets `state.last_valid_seq` at the start of each buffer,
sets it for any `CA_PROTO_VERSION` in that same buffer, and drops every
`CA_PROTO_SEARCH` response when it is `None`. It does not compare the echoed
sequence number with the sent datagram sequence or any timer window.

C reference: `libca/tcpiiu.cpp::searchRespNotify()` accepts TCP search replies
directly; TCP search replies from `rsrv/camessage.c::search_reply_tcp()` carry
no per-reply VERSION. On UDP, `libca/udpiiu.cpp::searchRespAction()` transfers
the channel even when no VERSION sequence marker was present, and
`searchTimer::uninstallChanDueToSuccessfulSearchResponse()` applies the stale
sequence-window check only when `seqNumberIsValid` is true.

Impact: TCP name-server discovery in Rust depends on TCP segmentation: a
SEARCH reply is accepted only if a VERSION response happens to be parsed in the
same buffer. A normal C TCP search reply arriving alone is dropped. For UDP,
legacy or third-party unsequenced replies are dropped even though libca accepts
them, while stale sequenced replies are accepted because Rust records the
sequence but never validates it.

### R2-27: Zero-payload EVENT_ADD cancel confirmations are treated as monitor traffic

Severity: Low

Rust: `crates/epics-ca-rs/src/client/transport.rs`, the `CA_PROTO_EVENT_ADD`
branch checks `hdr.cid != ECA_NORMAL` before checking whether `postsize == 0`.
For a zero-payload cancel confirmation it can emit
`TransportEvent::MonitorStatusError`; if the CID happens to be `ECA_NORMAL`, it
emits `TransportEvent::MonitorData` with an empty payload.

C reference: `libca/cac.cpp::eventRespAction()` returns immediately when
`!hdr.m_postsize`, before status handling or payload conversion. rsrv's
`event_cancel_reply()` intentionally sends a zero-payload `CA_PROTO_EVENT_ADD`
confirmation using the stored subscription request header.

Impact: the normal unsubscribe path removes the subscription before the ack is
usually processed, so this often degrades to a dropped internal event. In races
where the subscription record is still present, Rust can surface a cancel
confirmation as a monitor exception or decode error. libca treats the frame as
a no-op acknowledgement.

### R2-28: CLIENT_NAME/HOST_NAME framing truncates long payload sizes

Severity: Medium

Rust: `crates/epics-ca-rs/src/client/transport.rs` and
`crates/epics-ca-rs/src/client/search.rs` build the TCP handshake by assigning
`user_hdr.postsize = user_payload.len() as u16` and
`host_hdr.postsize = host_payload.len() as u16`, then appending the full
payload.

C reference: `libca/tcpiiu.cpp::userNameSetRequest()` and
`hostNameSetRequest()` route through `comQueSend::insertRequestHeader()`, whose
serializer emits extended headers when the payload does not fit the 16-bit
field. On the server side, `rsrv/camessage.c::client_name_action()` and
`host_name_action()` reject over-511-byte names after consuming a well-framed
message.

Impact: a large `USER`/`USERNAME` environment value, or an unexpectedly large
local hostname, makes Rust advertise a truncated `postsize` while still writing
the full string. The peer parses the excess bytes as subsequent CA headers,
which can desynchronize an IOC or TCP name-server connection before channel
creation starts. libca either frames the message correctly or the server
rejects it as a framed protocol error.

### R2-29: Unknown TCP response opcodes are silently ignored

Severity: Medium

Rust: `crates/epics-ca-rs/src/client/transport.rs`, the read loop's response
dispatcher ends with `_ => {}`. Any complete TCP frame with an unknown or
client-invalid opcode is skipped and the virtual circuit stays open.

C reference: `libca/cac.cpp::executeResponse()` dispatches unknown response
opcodes to `badTCPRespAction()`. That function logs the bad response and
returns false; `tcpiiu.cpp` treats `processIncoming() == false` as a protocol
failure and calls `initiateAbortShutdown()`.

Impact: a broken or hostile server can inject response opcodes that libca uses
to tear down the TCP circuit, while Rust quietly advances past them. That hides
protocol corruption and can leave later client operations running on a circuit
that libca would have rebuilt.

### R2-30: Client SEARCH requests ask for NOT_FOUND replies that libca avoids

Severity: Low

Rust: `crates/epics-ca-rs/src/client/search.rs::build_search_payload()` sets
`search_hdr.data_type = CA_DO_REPLY` for every search request.

C reference: `libca/udpiiu.cpp::searchMsg()` sets `m_dataType = DONTREPLY`.
The C server's TCP search path sends `CA_PROTO_NOT_FOUND` only when
`m_dataType == DOREPLY`; UDP search does not send NOT_FOUND. libca's TCP
response table treats `CA_PROTO_NOT_FOUND` as a bad TCP response rather than a
normal discovery result.

Impact: Rust asks TCP name servers and TCP search endpoints to generate
negative replies that libca does not request. Rust then ignores
`CA_PROTO_NOT_FOUND` in the search parser, so the extra frames add traffic and
exercise a response shape the C client path treats as invalid, without
improving discovery correctness.

### R2-31: Client get/put paths ignore cached access rights before sending

Severity: High

Rust: `crates/epics-ca-rs/src/client/mod.rs`, `CaChannel::snapshot()` returns
`ChannelSnapshotPublic` with `access_rights`, but
`send_read_notify_fast()`, `send_write_notify_fast()`, and
`send_write_nowait_fast()` never check those bits. The public `get*()` and
`put*()` methods therefore send READ_NOTIFY, WRITE_NOTIFY, or WRITE even when
the last ACCESS_RIGHTS frame denied the operation.

C reference: `libca/nciu.cpp::read()` rejects before queueing when
`!accessRightState.readPermit()`, and `nciu::write()` / the write-callback
overload reject before queueing when `!accessRightState.writePermit()`.

Impact: Rust sends requests that libca refuses synchronously with
`ECA_NORDACCESS` or `ECA_NOWTACCESS`. Callback variants depend on a later
server reply or timeout, and fire-and-forget writes can return `Ok(())` even
though the cached access state already says the write is forbidden. This also
makes behavior race against server-side access checks instead of matching
libca's local access-rights gate.

### R2-32: Metadata reads silently clamp overlarge requested counts

Severity: Medium

Rust: `crates/epics-ca-rs/src/client/mod.rs`,
`CaChannel::get_with_metadata_count()` maps `count > 0` to
`count.min(snap.element_count)`. A caller asking for more elements than the
channel's native count therefore receives a shorter request instead of an
error.

C reference: `libca/nciu.cpp::read()` rejects `countIn > this->count` before
queueing the read. `cadef.h` documents `ECA_BADCOUNT` for
`ca_array_get_callback()` / `ca_array_get()` when the requested count is larger
than the native element count.

Impact: Rust hides caller bugs and sends a different wire request than libca.
For example, requesting metadata count 100 from a 10-element PV becomes a
10-element READ_NOTIFY in Rust, while C returns `ECA_BADCOUNT` without sending
anything.

### R2-33: Unterminated PV names are parsed differently from rsrv

Severity: Low

Rust: `crates/epics-ca-rs/src/server/tcp.rs` and
`crates/epics-ca-rs/src/server/udp.rs` parse CREATE_CHAN and SEARCH payloads by
searching for the first NUL byte and using the full payload when no NUL exists.

C reference: `rsrv/camessage.c::claim_ciu_action()`,
`search_reply_udp()`, and `search_reply_tcp()` all force
`pName[mp->m_postsize - 1] = '\0'` after rejecting `m_postsize <= 1`.

Impact: a malformed client that omits the NUL terminator can cause Rust to
look up a different PV name than rsrv would. For payload `ABCD` with
`postsize = 4`, rsrv searches `ABC`; Rust searches `ABCD`. That can turn a C
not-found path into a Rust channel creation or search reply for malformed
frames.

## Cleared During Review

### R2-2: Monitor status errors are already delivered

The initial candidate was that non-`ECA_NORMAL` `CA_PROTO_EVENT_ADD` frames were
warn-and-dropped. Current code has `TransportEvent::MonitorStatusError` in
`client/types.rs`, sends it from `client/transport.rs`, and routes it through
`SubscriptionRegistry::on_monitor_error()` in `client/mod.rs`.

### R2-11: UDP search response VERSION state no longer leaks across datagrams

The initial finding was that `handle_udp_response()` retained
`state.last_valid_seq` across UDP datagrams. Current code resets
`state.last_valid_seq = None` at the start of each parsed response buffer, so
the cross-datagram leak itself is no longer present. The remaining current
search-response defects are recorded separately in R2-26.

### R2-22: Send backpressure accounting no longer uses load/store decrement

The initial finding was that `pending_frames` decremented with `load()` plus
`store(prev.saturating_sub(drained))`, allowing concurrent increments to be
lost. Current `client/transport.rs::write_loop()` and
`client/types.rs::DirectServerWriter::send_frame()` use a saturating
compare-exchange loop for decrement/rollback, so the lost-increment defect is
not present in the current code.

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
- 2026-05-18: Rechecked the earlier UDP VERSION-state finding against current
  code. R2-11 moved to cleared; R2-26 records the remaining search-response
  freshness defect.
- 2026-05-18: Continued client response, TCP name-server, handshake framing,
  and search-request parity pass. Findings R2-25 through R2-30 recorded.
- 2026-05-18: Continued client read/write preflight pass. Finding R2-31
  recorded.
- 2026-05-18: Rechecked send backpressure accounting against current code.
  R2-22 moved to cleared.
- 2026-05-18: Continued client metadata-count and server malformed-PV-name
  pass. Findings R2-32 and R2-33 recorded. R2-23 narrowed to the remaining
  large-count extended-framing defect.
