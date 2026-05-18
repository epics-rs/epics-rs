# epics-ca-rs C Parity Review - 2026-05-18

Scope: `crates/epics-ca-rs` only. Reference implementation is
`~/codes/epics-base`, primarily libca under `modules/ca/src/client` and
rsrv under `modules/database/src/ioc/rsrv`.

This file records compatibility defects found during review. It excludes
intentional Rust-only behavior unless it breaks a libca/rsrv wire contract.

## Open Findings

### R2-1: Access-rights loss emits the wrong monitor error frame

Severity: Medium

Rust: `crates/epics-ca-rs/src/server/tcp.rs`, `reeval_access_rights()`
now keeps subscriptions installed across read-access changes, but the read-loss
path sends a header-only `CA_PROTO_EVENT_ADD` status frame with `count = 0` and
no DBR payload. The restore path calls `send_monitor_snapshot()`, which still
uses the live value count instead of the subscription's stored request count.

C reference: `rsrv/camessage.c::casAccessRightsCB()` keeps the event in the
channel event queue. On read access loss it calls `db_post_single_event()` and
then disables future delivery; that routes through `read_reply()` and
`no_read_access_event()`, preserving the stored `event_ext.msg` DBR type/count
and sending a zero-filled payload sized from the original subscription request.
On read access restore it enables the event and posts one fresh `read_reply()`
using the same stored count.

Impact: after an ACF reload or identity change that revokes read access, a
strict client sees different callback metadata from Rust than from rsrv:
`count = 0` and no payload instead of the requested count and zero DBR body.
When access is restored, Rust can send a full native-count snapshot even when
the monitor was created with a smaller explicit count.

### R2-8: WRITE_NOTIFY replies truncate large request counts

Severity: Low

Rust: `crates/epics-ca-rs/src/server/tcp.rs`, all `CA_PROTO_WRITE_NOTIFY`
reply paths copy `hdr.count` into the response and serialize with
`resp.to_bytes()`. For extended WRITE_NOTIFY requests, `hdr.count` is the
normal-header marker value rather than `hdr.actual_count()`, so large put
callbacks lose their element count in both success and error replies.

C reference: `rsrv/camessage.c::putNotifyErrorReply()` and
`write_notify_reply()` call `cas_copy_in_header()` with
`mp->m_count` / `msgtmp.m_count` from `caHdrLargeArray`, which is the decoded
32-bit count for extended requests and is re-emitted in extended form when
needed.

Impact: large array `ca_array_put_callback()` operations can receive a
normal-form Rust WRITE_NOTIFY response with `count = 0` where rsrv preserves
the requested count with an extended header. Most clients key completion on
`m_available`, but strict wire tests and replay tooling see a different frame.

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

### R2-12: EVENT_ADD still does not enforce the requested element count

Severity: Medium

Rust: `crates/epics-ca-rs/src/server/tcp.rs`, the `CA_PROTO_EVENT_ADD` handler
now stores `data_count` in `SubscriptionEntry`, but it is not applied
consistently. Initial monitor delivery and access-right restoration still call
`send_monitor_snapshot()`, which always uses `snapshot.value.count()`. Later
monitor delivery calls `pad_dbr_to_requested_count()`, but that helper only
pads when `requested_count > actual_count`; it does not truncate the encoded
payload when `requested_count < actual_count`, so the response can carry
`count = requested_count` with a payload sized for the full native array.

C reference: `rsrv/camessage.c::event_add_action()` stores the original request
header in `pevext->msg`. Monitor delivery uses `read_reply()`, which derives
`item_count` from `pevext->msg.m_count` unless the request count is zero
autosize. For non-autosize requests, the response header count remains the
requested count; the payload is sized for that requested count, and data past
the real element count is zero-filled.

Impact: `ca_create_subscription(type, count=1, ...)` on a waveform should
receive exactly one element from rsrv. Current Rust update frames can still
carry the full waveform payload, and initial/restore frames report the full
native count. Requested counts larger than the current value are closer to rsrv
now, but requested counts smaller than the current value are still not enforced
on every monitor path.

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

### R2-23: EVENT_CANCEL request/ack headers do not preserve the stored subscription header

Severity: Low

Rust client: `crates/epics-ca-rs/src/client/transport.rs`,
`TransportCommand::Unsubscribe` copies the original subscription count into
`hdr.count` and serializes with `hdr.to_bytes()`. Counts `>= 0xffff` are
therefore truncated into the 16-bit normal header instead of using the CA
extended header.

Rust server: `crates/epics-ca-rs/src/server/tcp.rs`, the successful
`CA_PROTO_EVENT_CANCEL` branch replies with `CA_PROTO_EVENT_ADD` and
`resp.count = sub.data_count as u16`, `resp.cid = ECA_NORMAL`, then serializes
with `resp.to_bytes()`. The normal-count case now echoes the stored
subscription count, but the large-array case still cannot represent the count,
and the CID field does not echo the original EVENT_ADD request's `m_cid`.

C reference: `libca/tcpiiu.cpp::subscriptionCancelRequest()` includes the
subscription count through `comQueSend::insertRequestHeader()`, which emits the
extended form when `nElem >= 0xffff`. `rsrv/camessage.c::event_cancel_reply()`
sends the stored `pevext->msg` fields back with zero payload: original command,
DBR type, count, channel SID in `m_cid`, and subscription id in `m_available`.

Impact: monitors on arrays with 65,535 or more requested elements can be
cancelled with a different count than the one libca sends and rsrv echoes. For
all non-first-channel subscriptions, Rust server cancel acknowledgements also
carry `ECA_NORMAL` in `m_cid` where rsrv echoes the SID. Strict servers,
trace/replay tooling, and protocol tests see a different zero-payload
EVENT_ADD acknowledgement.

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

### R2-34: READ access denial shadows invalid DBR type handling

Severity: Medium

Rust: `crates/epics-ca-rs/src/server/tcp.rs`, the
`CA_PROTO_READ | CA_PROTO_READ_NOTIFY` branch checks
`state.lookup_access(sid).require_read()` before validating the requested DBR
type. If a read-denied channel receives an invalid `m_dataType`, Rust reports
read access denial: `READ_NOTIFY` sends a no-read-access EVENT frame and keeps
the connection open, while deprecated `READ` sends a `CA_PROTO_ERROR` with
`ECA_NORDACCESS`.

C reference: `rsrv/camessage.c::read_notify_action()` checks
`INVALID_DB_REQ(mp->m_dataType)` before channel lookup or access checking and
returns `RSRV_ERROR` without emitting a wire frame. Deprecated
`read_action()` resolves the channel first, but still checks
`INVALID_DB_REQ()` before `readAccess`; bad DBR type sends `ECA_BADTYPE` and
returns `RSRV_ERROR`.

Impact: malformed clients can observe an access-denied status from Rust where
rsrv treats the request as a bad protocol frame and disconnects. For
`READ_NOTIFY`, Rust can even emit a no-read-access payload using an invalid DBR
type that rsrv would never encode.

### R2-35: Repeater rejects local non-loopback registrations that C accepts

Severity: Low

Rust: `crates/epics-ca-rs/src/repeater.rs`, the
`CA_PROTO_REPEATER_REGISTER` path accepts only loopback-source datagrams. A
client registering from a local interface address such as `192.168.x.y` is
rejected before `register_client_debug()` runs.

C reference: `modules/ca/src/client/repeater.cpp::register_new_client()` first
accepts loopback, but for non-loopback IPv4 sources it performs a bind test
with the source address and accepts the registration if the address belongs to
a local interface. The comment documents this as compatibility with clients
that alternate between loopback and the first non-loopback interface.

Impact: modern libca uses loopback for repeater registration, so ordinary
clients are not affected. Older or site-specific clients that still register
from a local non-loopback interface receive a confirmation from the C repeater
but are silently refused by the Rust repeater, so they can miss beacon fan-out.

### R2-36: EVENT_ADD admission failures use a zero-payload frame libca treats as a cancel ack

Severity: Medium

Rust: `crates/epics-ca-rs/src/server/tcp.rs`, subscription refusal paths such
as the per-channel cap and per-PV subscriber cap call `send_cmd_error()` with
`CA_PROTO_EVENT_ADD`. That emits a zero-payload `EVENT_ADD` frame whose `m_cid`
carries `ECA_ALLOCMEM`.

C reference: `rsrv/camessage.c::event_add_action()` sends allocation/install
failures through `send_err(mp, ECA_ALLOCMEM, ...)`, i.e. `CA_PROTO_ERROR`, and
returns `RSRV_ERROR`. On the client side, `libca/cac.cpp::eventRespAction()`
returns immediately for `m_postsize == 0` because zero-payload EVENT_ADD is the
historical cancel-confirmation no-op; EVENT_ADD exceptions are delivered via
`eventAddExcep()` from the `CA_PROTO_ERROR` path.

Impact: a libca client can request a subscription from Rust after a cap is
reached, receive the zero-payload EVENT_ADD, ignore it as a cancel
confirmation, and wait for monitor updates that will never arrive. rsrv
delivers an exception and tears down the virtual circuit for the same
allocation-failure class.

## Cleared During Review

### R2-2: Monitor status errors are already delivered

The initial candidate was that non-`ECA_NORMAL` `CA_PROTO_EVENT_ADD` frames were
warn-and-dropped. Current code has `TransportEvent::MonitorStatusError` in
`client/types.rs`, sends it from `client/transport.rs`, and routes it through
`SubscriptionRegistry::on_monitor_error()` in `client/mod.rs`.

### R2-3: TCP nameserver parser now closes on malformed frames

Current `client/search.rs::run_nameserver_connection()` distinguishes partial
headers from definitive parse errors, sets `bad_frame` for malformed extended
headers or misaligned payload sizes, and closes the nameserver circuit so the
outer reconnect loop can rebuild it. The original bad-prefix wedge is not
present in the current code.

### R2-4: TCP VERSION priority is now range-checked

Current `server/tcp.rs` rejects `CA_PROTO_VERSION` when `hdr.data_type >
CA_PROTO_PRIORITY_MAX` and drops the connection, matching
`rsrv/camessage.c::tcp_version_action()`.

### R2-5: EVENT_ADD no-read-access subscriptions are now installed disabled

Current `server/tcp.rs` records `access_denied`, installs the subscription, sends
`send_no_read_access_event()` for the initial denied update, and stores a
`denied` gate that `reeval_access_rights()` can flip later. The original
"denied subscriptions are permanently absent" defect is not present in the
current code.

### R2-6: READ_NOTIFY bad DBR type no longer sends a reply frame

Current `server/tcp.rs` skips the `send_ca_error()` path for invalid
`READ_NOTIFY` DBR types and returns a protocol error, producing the rsrv-style
silent close. R2-34 records the remaining ordering defect when read access is
also denied.

### R2-7: READ_NOTIFY read-denied response now uses no_read_access_event shape

Current `server/tcp.rs` routes read-denied `READ_NOTIFY` through
`send_no_read_access_event()` with the original DBR type, requested count, IOID,
and zero-filled payload. That clears the original zero-count `send_cmd_error()`
defect.

### R2-10: UDP unsupported-version parsing now stops the datagram

Current `server/udp.rs` checks the minimum supported CA minor version for both
UDP `VERSION` and UDP `SEARCH`; unsupported frames break out of the current
datagram parse instead of skipping only that one message. The original
continue-after-bad-version defect is not present in the current code.

### R2-11: UDP search response VERSION state no longer leaks across datagrams

The initial finding was that `handle_udp_response()` retained
`state.last_valid_seq` across UDP datagrams. Current code resets
`state.last_valid_seq = None` at the start of each parsed response buffer, so
the cross-datagram leak itself is no longer present. The remaining current
search-response defects are recorded separately in R2-26.

### R2-13: READ_NOTIFY now pads short arrays up to the requested count

Current `server/tcp.rs` truncates snapshots when `requested_count < actual`,
then calls `pad_dbr_to_requested_count()` before building the reply. The
READ_NOTIFY short-array response keeps the requested count and zero-pads the
payload like rsrv `read_reply()`.

### R2-14: CA_PROTO_WRITE put failures now surface CA_PROTO_ERROR

Current `server/tcp.rs` sends `send_ca_error()` for non-notify write failures,
including the `DBR_PUT_ACKT` / `DBR_PUT_ACKS` branch. The original silent
fire-and-forget failure is not present in the current code.

### R2-15: Channel-scoped CA_PROTO_ERROR replies now use client CID

Current READ/WRITE error paths capture `entry.cid` / `entry_cid` and pass that
client CID to `send_ca_error()`, matching rsrv `vsend_err()` for channel-scoped
commands.

### R2-16: WRITE bad-type handling now resolves the channel id first

Current `server/tcp.rs` looks up `state.channels` before
`DbFieldType::from_u16()` in the regular WRITE branch, so a bad SID follows the
rsrv silent-close path before DBR type validation.

### R2-22: Send backpressure accounting no longer uses load/store decrement

The initial finding was that `pending_frames` decremented with `load()` plus
`store(prev.saturating_sub(drained))`, allowing concurrent increments to be
lost. Current `client/transport.rs::write_loop()` and
`client/types.rs::DirectServerWriter::send_frame()` use a saturating
compare-exchange loop for decrement/rollback, so the lost-increment defect is
not present in the current code.

### R2-24: EVENT_CANCEL bad-SID path now closes without a wire error

Current `server/tcp.rs` resolves the requested SID before consulting the flat
subscription map. Unknown SID returns a protocol error without sending
`ECA_BADMONID`, while valid-SID/unknown-monitor still sends `ECA_BADMONID`,
matching `rsrv/camessage.c::event_cancel_reply()`.

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
- 2026-05-18: Rechecked current server TCP changes against earlier open
  findings. R2-3, R2-4, R2-5, R2-6, R2-7, R2-10, R2-13, R2-14, R2-15,
  R2-16, and R2-24 moved to cleared. R2-1, R2-8, R2-12, and R2-23 narrowed
  to the remaining current defects. Finding R2-34 recorded.
- 2026-05-18: Continued repeater registration and EVENT_ADD admission-failure
  pass. Findings R2-35 and R2-36 recorded.
