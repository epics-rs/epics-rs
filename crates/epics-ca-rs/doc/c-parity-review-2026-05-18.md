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

### R2-37: Subscription callbacks never receive `ECA_DISCONN` on disconnect

Severity: High

Rust: `crates/epics-ca-rs/src/client/subscription.rs` `SubscriptionRegistry::mark_disconnected()` only flips `needs_restore = true` and clears `pending_deliveries`; it never invokes `rec.callback_tx` with an error. `client/mod.rs` disconnect paths (`TcpClosed`, `ServerDisconnect`) call `drain_waiters_for_cids` for reads/writes but route subscriptions only through `mark_disconnected`.

C reference: `libca/cac.cpp:678-698` `cac::disconnectAllIO()` iterates every in-flight IO on the channel (including subscriptions) and calls `pNetIO->exception(guard, *this, ECA_DISCONN, hostName)`. `libca/netSubscription.cpp:86-107` routes through `notify.exception(...ECA_DISCONN...)`. Fires on every disconnect class — SERVER_DISCONN, unresponsive circuit, removeAllChannels.

Impact: a Rust `MonitorHandle::recv()` against a C IOC observes silence when the circuit dies — no `Err(ServerError(ECA_DISCONN))`. C contract is one ECA_DISCONN per active monitor on disconnect.

### R2-38: CircuitUnresponsive skips ECA_UNRESPTMO, per-channel access clear, per-IO ECA_DISCONN

Severity: High

Rust: `crates/epics-ca-rs/src/client/mod.rs` `TransportEvent::CircuitUnresponsive` only flips `ch.state = Unresponsive` and sends `ConnectionEvent::Unresponsive`. No global exception hook with ECA_UNRESPTMO, no `AccessRightsChanged{read:false,write:false}` per channel, no in-flight read/write/subscription failure.

C reference: `libca/tcpiiu.cpp:899-941` `unresponsiveCircuitNotify` calls `genLocalExcep(... ECA_UNRESPTMO ...)`, walks `connectedList`, calls `pChan->unresponsiveCircuitNotify` → `disconnectAllIO()` + `accessRightsNotify(noRights)`.

Impact: `on_exception` handler never sees ECA_UNRESPTMO; in-flight ops keep waiting their full timeout; access-rights subscribers don't get notified that writes/reads are refused.

### R2-39: Responsive-recovery does not resend per-subscription READ_NOTIFY

Severity: Medium

Rust: `crates/epics-ca-rs/src/client/mod.rs` `TransportEvent::CircuitResponsive` only flips state back to `Connected`. No call to send a fresh READ_NOTIFY per active subscription on the recovered channel.

C reference: `libca/tcpRecvWatchdog.cpp:131-158` `probeResponseNotify` → `iiu.responsiveCircuitNotify` → `tcpiiu.cpp:861-877` walks `unrespCircuit` and calls `pChan->connect()`, moving channels to `subscripUpdateReqPend`. Send thread then calls `sendSubscriptionUpdateRequests` (`tcpiiu.cpp:1610-1644`) — a forced READ_NOTIFY per subscription.

Impact: a Rust monitor against a C IOC whose circuit briefly went unresponsive sees no value update at recovery — values changed during the gap are invisible until the next natural update.

### R2-40: Send-side stall closes circuit at 10 s; libca only marks unresponsive

Severity: Medium

Rust: `client/transport.rs:826` sets `send_timeout = ECHO_TIMEOUT_SECS * 2` (10 s); `transport.rs:869-872` converts any timeout/error into `TcpClosed`, tearing the circuit down.

C reference: `libca/tcpSendWatchdog.cpp:43-64` fires `iiu.sendTimeoutNotify` after `connTMO` (default 30 s); `tcpiiu.cpp:879-888` calls `unresponsiveCircuitNotify` and starts a recv probe. TCP socket is kept; only closed if probe also fails.

Impact: a slow but live server is torn down by Rust after 10 s, forcing full search/connect. Drops all subscriptions/reads/writes in flight.

### R2-41: TCP dispatcher misses SEARCH / READ / CLEAR_CHANNEL opcodes

Severity: Medium

Rust: `client/transport.rs:1181-1503` match arms cover VERSION, ACCESS_RIGHTS, CREATE_CHAN, READ_NOTIFY, WRITE_NOTIFY, EVENT_ADD, ECHO, READ_SYNC, CREATE_CH_FAIL, ERROR, SERVER_DISCONN. Everything else hits R2-29's `unknown` branch and closes the circuit.

C reference: `libca/cac.cpp:60-89` TCP jump table includes `CA_PROTO_SEARCH` (`searchRespNotify`, used when server doubles as nameserver), `CA_PROTO_READ` (`readRespAction`, deprecated sync read), `CA_PROTO_CLEAR_CHANNEL` (no-op). `cac.cpp:1208-1218` `executeResponse` routes only truly out-of-range opcodes to `badTCPRespAction`.

Impact: a CA name server or gateway emitting these frames kills the Rust circuit per occurrence. R2-29 made unknown lethal — gap is misclassifying known-valid opcodes as unknown.

### R2-45: Oversized TCP payload kills circuit; libca skips message

Severity: Low

Rust: `protocol.rs:405-407` `from_bytes_extended` returns `Err("payload too large")` when `ext_post > max_payload_size()`. `transport.rs:1138-1145` converts to `TcpClosed`.

C reference: `libca/tcpiiu.cpp:1269-1284` — when `m_postsize > curDataMax` and realloc fails, logs ONCE then drains via `recvQue.removeBytes` and continues. Circuit kept.

Impact: a single oversized monitor frame tears down the Rust circuit and forces full reconnect; libca tolerates and continues.

### R2-46: WRITE_NOTIFY regular-write denied path bypasses `send_put_notify_response` helper

Severity: Low

Rust: `server/tcp.rs:2351-2358` regular WRITE_NOTIFY write-denied early-out builds the reply inline with `resp.count = hdr.count` (16-bit, 0 for extended) and `to_bytes()`. The ACKT/ACKS-denied branch at `tcp.rs:2191` correctly routes through `send_put_notify_response` (R2-8 refinement helper).

C reference: `rsrv/camessage.c::write_notify_action:1653-1656` routes `!rsrvCheckPut` through `putNotifyErrorReply` → `cas_copy_in_header(..., mp->m_dataType, mp->m_count, ECA_NOWTACCESS, ...)`. `m_count` is decoded 32-bit; `cas_copy_in_header` re-emits extended annex when `nElem >= 0xffff`.

Impact: large-array put-callback refused by ACF-denied client gets normal-form Rust reply with `count = 0`; rsrv preserves count via extended header. Same defect class as R2-8.

### R2-47: TCP server omits unsolicited VERSION on accept

Severity: Medium

Rust: `server/tcp.rs:1000-1006` after accept/audit/TLS handshake enters read loop with no proactive write. VERSION emitted only as response to client's VERSION.

C reference: `rsrv/caservertask.c:1525` `create_tcp_client` calls `rsrv_version_reply` immediately after `db_start_events`. Next flush ships VERSION as server's first wire frame.

Impact: libca `tcpRecvWatchdog::messageArrivalNotify` resets recv timer on every frame; server-side unsolicited VERSION is the first liveness beat. Without it, slow handshake (client batching VERSION+HOST_NAME+CLIENT_NAME with Nagle) can drift toward CA_ECHO_TIMEOUT. Wire-trace replay against rsrv breaks on first byte.

### R2-49: UDP SEARCH reply ships unsolicited VERSION to pre-V411 clients

Severity: Low

Rust: `server/udp.rs:248-417` always prepends fresh VERSION to reply; when `client_seq.is_none()` VERSION still ships with `cid=0, data_type=0, count=CA_MINOR_VERSION`.

C reference: `rsrv/caserverio.c:193-201` keeps VERSION on wire only when `CA_V411(minor_version)`; for older peers bytes are stripped.

Impact: pre-V4.11 libca client receives extra VERSION header where rsrv would send none. Wire-trace divergence.

### R2-50: Duplicate-sub_id EVENT_ADD refusal uses cancel-ack wire shape

Severity: Medium

Rust: `server/tcp.rs:2675-2689` when second EVENT_ADD with duplicate sub_id arrives, calls `send_cmd_error(CA_PROTO_EVENT_ADD, ..., ECA_BADMONID, sub_id)` — zero-payload EVENT_ADD with `m_cid = ECA_BADMONID`.

C reference: `rsrv/camessage.c::event_add_action:1762-1866` performs no per-client sub_id dedup. Two EVENT_ADDs with identical `m_available` both install; later cancel cancels first match. No C path returns ECA_BADMONID on duplicate-add.

Impact: Rust dedup itself is unexpected (rsrv accepts duplicates). Worse: zero-payload EVENT_ADD reply is exactly the R2-36/R2-27 cancel-ack shape — libca `eventRespAction` returns immediately for `!hdr.m_postsize`. Refusal silently swallowed; application waits forever. Use `send_ca_error(CA_PROTO_ERROR, ECA_BADMONID, ...)`.

### R2-51: ACCESS_RIGHTS pushed for every channel on reload even when unchanged

Severity: Medium

Rust: `server/tcp.rs:3514-3534` `reeval_access_rights` iterates every entry in `state.channels` and unconditionally writes ACCESS_RIGHTS before computing transitions. Cache update via `insert`, but wire emission not gated on `old_level != new_level`.

C reference: `libcom/src/as/asLibRoutines.c:1047-1051` `asComputePvt` fires `pclient->pcallback(... asClientCOAR)` only when `oldaccess != access`. ACF reload that leaves every channel at same level emits zero ACCESS_RIGHTS frames.

Impact: routine ACF reload triggers `O(channels-per-client)` ACCESS_RIGHTS burst per connection. Gateway-front IOC with thousands of channels sees visible network blip; strict clients keying off ACCESS_RIGHTS as a "re-read" hint mass-refresh on every reload.

### R2-52: Cap-token verification doesn't update `auth_method`/`auth_authority` for ACF METHOD/AUTHORITY

Severity: Medium

Rust: `server/tcp.rs:1631-1653` on successful `TokenVerifier::verify` only `state.username = claims.sub` is set. `auth_method` and `auth_authority` (consumed by `compute_access` and `AccessSecurityConfig::check_access_method`) never updated to `"cap-token"`/`claims.iss`.

C reference: epics-base doesn't implement cap-tokens but PR #563/#618 defines METHOD/AUTHORITY as scoping by authenticator subsystem. Rust's own `TokenClaims` carries `iss` precisely for this scoping.

Impact: ACF rule `RULE(1, WRITE) { METHOD("cap-token") AUTHORITY("ops-issuer-1") }` never matches authenticated cap-token peer. Operators can't express "writes only via cap-token"; a stolen plain CLIENT_NAME and a verified cap-token for same subject are indistinguishable to rule engine.

### R2-55: CREATE_CHAN's `m_available` minor-version override not honoured

Severity: Low

Rust: `server/tcp.rs:1473` `state.client_minor_version` set only from `CA_PROTO_VERSION` handler. CREATE_CHAN handler reads but never writes `client_minor_version`.

C reference: `rsrv/camessage.c:1190-1199` `claim_ciu_action` unconditionally executes `client->minor_version_number = mp->m_available;` before VSUPPORTED gate. CA protocol comment: "The available field is used (abused) here to communicate the minor version number starting with CA 4.1". Client that handshakes v4.4 then CREATE_CHANs v4.13 gets the upgrade — including `CA_V49` extended-form decision.

Impact: client using "negotiate v4.4 then upgrade on first CREATE_CHAN" pattern works against rsrv, loses upgrade against Rust. nelem cap stays in normal-form for peer whose CREATE_CHAN minor was 13 — peer sees truncated count on large arrays.

### R2-57: Client `EPICS_CA_AUTO_ADDR_LIST` parser diverges from C's substring "no" check

Severity: Medium

Rust: `client/mod.rs:3470-3475` `auto_addr.eq_ignore_ascii_case("YES")`. Anything not equal to `"YES"` (e.g. `"1"`, `"true"`, `"on"`, `"bogus"`) disables auto-discovery.

C reference: `ca/src/client/iocinf.cpp:186-193` `yes = true; if (strstr(pstr,"no") || strstr(pstr,"NO")) yes = false;`. Substring (not equality) check: any value not containing `"no"`/`"NO"` keeps `yes = true`.

Impact: site setting `EPICS_CA_AUTO_ADDR_LIST=1` or `=true` gets auto-discovery ON on C, OFF on Rust. Silent loss of per-NIC broadcast SEARCH coverage. Server-side parser is correctly strict-YES (matches C server); only client-side var has the strstr quirk.

### R2-59: `EPICS_CA_NAME_SERVERS` bare hostnames default to 5064, ignore `EPICS_CA_SERVER_PORT`

Severity: Medium

Rust: `client/mod.rs:3824-3835` `parse_nameserver_list` bare-hostname branch uses hardcoded `CA_SERVER_PORT = 5064`. The sibling `parse_addr_list_with_hostnames:3413` correctly reads `EPICS_CA_SERVER_PORT`.

C reference: `ca/src/client/cac.cpp:259` `addAddrToChannelAccessAddressList(..., this->_serverPort, ...)`, `_serverPort` from `envGetInetPortConfigParam` at `cac.cpp:185-186`.

Impact: site with `EPICS_CA_SERVER_PORT=5066` and `EPICS_CA_NAME_SERVERS="ioc.example.com"` has Rust try port 5064, C try 5066. Silent connection refused or wrong service. Two related env parsers in same file disagree.

### R2-62: `EPICS_CA_MAX_SEARCH_PERIOD` recognised by lint but never read

Severity: Low

Rust: `client/search.rs` hard-fixed `N_SEARCH_BUCKETS = 30` and `NORMAL_TICK = 1s`. `EPICS_CA_MAX_SEARCH_PERIOD` only in `bin/ca-lint-rs.rs:68` whitelist — no production reader.

C reference: `ca/src/client/udpiiu.cpp:71-89` reads via `envGetDoubleConfigParam`, default 300 s, clamp 60 s. Sizes `nTimers` in searchTimer; bounds exponential backoff.

Impact: site setting `EPICS_CA_MAX_SEARCH_PERIOD=600` gets effect on C, zero on Rust. Rust default cap (~30 s) already 10× more aggressive than C default (300 s) — slow nameserver gets 10× more search traffic from Rust.

### R2-63: `EPICS_CA_CONN_TMO` clamps sub-second values and treats 0 as 1 s

Severity: Low

Rust: `client/transport.rs:112-117` `.map(|v| v.max(1.0) as u64)` fallback 30 only on absent/unparseable. (1) positive <1.0 rounds up to 1; (2) fractional truncated via `as u64`; (3) explicit 0 or negative clamped to 1.

C reference: `ca/src/client/cac.cpp:186-194` `envGetDoubleConfigParam`, then `if (status || connTMO <= 0.0) connTMO = CA_CONN_VERIFY_PERIOD;` (default 30). Kept as `double`; sub-second honoured; 0/negative falls back to 30.

Impact: `EPICS_CA_CONN_TMO=0.5` gets 0.5 on C, 1 on Rust. `=0` (operator sentinel for "use default") gets 30 on C but 1 on Rust — Rust pumps ECHO every second on every circuit, multiplying inbound TCP load on each server 30×.

### R2-74: Multicast responder errors are silently swallowed; only the first per-intf result propagates

Severity: Medium

Rust: `server/udp.rs:71-87` — `run_udp_search_responder` awaits ONLY `handles_iter.next()` (the first interface responder) and unconditionally `.abort()`s every remaining handle, including all `run_multicast_responder` handles pushed after the per-interface ones. A `run_multicast_responder` returning `Err(...)` because `any_joined == false` is dropped on the floor.

C reference: `caservertask.c:633-668` calls `setsockopt(IP_ADD_MEMBERSHIP, ...)` inline during `rsrv_init`, and on failure calls `errlogPrintf("CAS: Socket mcast join %s to %s failed: %s\n", ...)`. The error is surfaced per-(interface,group) pair, every time.

Impact: Operator configures `EPICS_CAS_INTF_ADDR_LIST="239.10.0.1"` on a host whose interfaces all reject IP_ADD_MEMBERSHIP. Server boots successfully, logs only the per-intf `tracing::warn!`, and PVs are invisible to multicast SEARCH.

### R2-75: Multicast SEARCH replies use kernel-chosen source IP; C uses the per-interface bound socket's IP

Severity: Medium

Rust: `server/udp.rs:219-252` — `run_multicast_responder` binds a single wildcard `0.0.0.0:port` socket and joins the group on every supplied interface. On reply (`socket.send_to(reply, src)`), the kernel selects the outbound interface + source IP via routing, with no `IP_MULTICAST_IF` or per-NIC binding.

C reference: `caservertask.c:621-668` creates one socket per `casIntfAddrList` entry, binds it to `conf->udpAddr.ia.sin_addr`, and joins each group with `imr_interface = conf->udpAddr.ia.sin_addr.s_addr`. Replies source from the bound interface IP deterministically.

Impact: Multi-homed IOC: C reply carries a specific NIC's IP as source; Rust reply may carry a different NIC's. Clients matching `SEARCH_REPLY` source against the `EPICS_CA_ADDR_LIST` entry that sent the SEARCH (R2-26 surface) dedup inconsistently; per-NIC firewall rules see different traffic.

### R2-76: AUTO_BEACON mixed `0.0.0.0`+specific misconfig escapes detection when `EPICS_CAS_AUTO_BEACON_ADDR_LIST=NO`

Severity: High

Rust: `server/addr_list.rs:137-162` — the warning for `intf_has_wildcard && !intf_specific.is_empty()` is nested inside `if auto_on { ... }`. With `EPICS_CAS_AUTO_BEACON_ADDR_LIST=NO` the block is skipped and the operator gets neither warning nor abort.

C reference: `caservertask.c:390-392` — `cantProceed("CAS interface address list can not contain 0.0.0.0 and other interface addresses.\n")` is OUTSIDE the `if(!doautobeacon) continue;` check. IOC aborts at startup regardless of `autobeaconlist`. `cantProceed` is fatal (libcom kills the process), not `errlogPrintf`.

Impact: Operator sets `EPICS_CAS_INTF_ADDR_LIST="0.0.0.0 10.0.0.5" EPICS_CAS_AUTO_BEACON_ADDR_LIST=NO`. C IOC refuses to start; Rust IOC starts silently with the misconfig.

### R2-77: AUTO_BEACON expansion drops point-to-point interfaces C populates via `ifa_dstaddr`

Severity: Medium

Rust: `server/addr_list.rs:328-350` — `broadcast_for_ip` only consults `if_addrs::IfAddr::V4 { broadcast, .. }`. For `IFF_POINTOPOINT` interfaces (VPN tun, PPP, WireGuard) the `broadcast` field is `None` and the helper returns `None`. The R2-61 filter then drops the interface from `bcast_iter` entirely.

C reference: `osdNetIfAddrs.c:130-151` — `osiSockDiscoverBroadcastAddresses` checks `IFF_BROADCAST` first, then `IFF_POINTOPOINT`, substituting `ifa->ifa_dstaddr` so beacons go to the remote tunnel endpoint.

Impact: IOC reachable to a remote subnet only via a VPN tun, configured with `EPICS_CAS_INTF_ADDR_LIST="<tun-ip>"`, emits no beacons toward the tunnel peer. C IOC sends beacons to the tunnel's `dstaddr`.

### R2-78: `IP_MULTICAST_LOOP=1` not set on the beacon-sending socket

Severity: Low

Rust: `server/beacon.rs:41-47` — beacon sender socket sets `set_broadcast(true)` and `set_multicast_ttl_v4(...)`, but never `set_multicast_loop_v4(true)`.

C reference: `caservertask.c:307-318` — `rsrv_init` explicitly `setsockopt(beaconSocket, IPPROTO_IP, IP_MULTICAST_LOOP, &flag=1, ...)` on the beacon socket. Linux default happens to match, but non-Linux and Linux kernels with site policy disabling default loop diverge.

Impact: For a beacon group fan-out where the IOC runs a local CA repeater or a co-located client subscribed to multicast beacons (self-test), loopback delivery depends on platform default rather than the explicit-1 C contract.

### R2-79: UDP SEARCH-reply batch flushes per-inbound; C batches across inbounds until recv queue drains

Severity: Medium

Rust: `server/udp.rs:545-550` — at end-of-inbound, `if !send_buf.is_empty() { socket.send_to(&send_buf, src).await }` then drops `send_buf` (new Vec each outer iteration). No `FIONREAD`/peek to defer flush when more inbound is queued.

C reference: `cast_server.c:266-281` — recv loop calls `socket_ioctl(recv_sock, FIONREAD, &nchars)` after each `camessage()` and ONLY flushes via `cas_send_dg_msg(client)` when `nchars == 0`. Peer-change detection at `205-215` flushes early on src change. C accumulates SEARCH replies across multiple inbound datagrams from the same client into ONE outbound until the recv queue drains.

Impact: Client search storm of 10 datagrams × 5 PVs gets 10 reply datagrams from Rust vs (typically) 1-2 from C. R2-48 only achieved within-datagram amortization; the bulk of search-storm reduction is the cross-datagram path.

### R2-80: VERSION placeholder skipped when SEARCH precedes VERSION inside the same inbound datagram

Severity: Medium

Rust: `server/udp.rs:502-529` — `include_version = client_seq.is_some()` evaluated at each SEARCH match. Prepend VERSION only on FIRST append. If first match arrives BEFORE a `CA_PROTO_VERSION` header (legal chained inbound), `client_seq.is_none()` → no VERSION prepended → send_buf non-empty → subsequent VERSION-after-SEARCH never inserts placeholder. Datagram flushes without VERSION.

C reference: `cast_server.c:154-156` calls `rsrv_version_reply(client)` BEFORE entering the loop, seeding VERSION at byte 0. `cas_send_dg_msg` re-seeds after every flush. Placeholder always present; `cas_send_dg_msg:185-201` decides at send time whether to strip, gated on `CA_V411(pclient->minor_version_number)` which is set in `udp_version_action:2096` whenever the inbound carries VERSION at any position.

Impact: V4.13 client with SEARCH-then-VERSION chained inbound (legal) gets a Rust reply with no VERSION header — cannot bind seqNoOfReq to discard stale replies; every reply passes the freshness check unconditionally.

### R2-81: Silent `send_to` failure on batched reply drops all N replies, not one

Severity: Medium

Rust: `server/udp.rs:522, 549` — both flush sites use `let _ = socket.send_to(&send_buf, src).await;`, discarding the result. Pre-R2-48 code dropped one reply on failure; batched code drops the whole batch.

C reference: `caserverio.c:214-222` — `cas_send_dg_msg` on negative `sendto` calls `errlogPrintf("CAS: UDP send to %s failed: %s\n", ...)`. C logs every drop.

Impact: Under EMFILE/ENOBUFS pressure during a search storm (R2-48's target scenario), operator gets no signal. Diagnostic regression — batching enlarged the blast radius per failed send.

### R2-82: `UDP_FLUSH_THRESHOLD = 1472` exceeds C's `MAX_UDP_SEND = 1024`

Severity: Low

Rust: `server/udp.rs:334` — `const UDP_FLUSH_THRESHOLD: usize = 1472;` (IPv4 Ethernet payload max).

C reference: `caProto.h:66` — `#define MAX_UDP_SEND 1024u`. `client->send.maxstk = MAX_UDP_SEND`; `cas_copy_in_header` calls `cas_send_dg_msg` when next message would push `stk > maxstk`. C never builds a UDP datagram larger than ~1024 bytes.

Impact: Third-party CA implementations (Java CAJ, asyncio-ca, embedded) may assume the 1024-byte contract and truncate larger replies. libca peers unaffected (`recvBuf[MAX_UDP_RECV]`).

### R2-83: Multicast responder's wildcard socket also receives unicast/broadcast — duplicate replies

Severity: Low

Rust: `server/udp.rs:227` — `run_multicast_responder` calls `bind_responder_socket(Ipv4Addr::UNSPECIFIED, port)`. After joining groups, this socket also receives broadcast/unicast SEARCHes targeting local interfaces (`IP_MULTICAST_ALL=0` only filters multicast). Result: SEARCH to specific interface gets handled by both the unicast responder AND the wildcard-bound mcast responder.

C reference: C creates a SEPARATE socket for multicast joins (`conf->udp` bound to specific interface IP). Wildcard recv socket and per-interface mcast-joining socket have non-overlapping recv scopes.

Impact: Multi-NIC+mcast configs: TWO `send_to(reply, src)` outbound per SEARCH. C fires once.

### R2-84: WRITE_NOTIFY AfterWrite fires before async record completion

Severity: High

Rust: `server/tcp.rs:2551-2590` — for `ChannelTarget::RecordField`, `db.put_record_field_from_ca` returns `Ok(Some(rx))` synchronously when an async record starts processing. `dispatch_trap_write` fires `AfterWrite { status: Some("ok") }` at `2577-2590` before `rx.await` resolves at `2655`. The actual completion is delivered later via the background WRITE_NOTIFY task.

C reference: `rsrv/camessage.c:1745-1752` captures `asWritePvt` from `asTrapWriteWithData` (Before fires here), then dispatches `dbProcessNotify`. Matching `asTrapWriteAfter(asWritePvtTmp)` lives in `write_notify_reply:1400`, executing only from the extra-labor task after `dbProcessNotify` invokes `write_notify_done_callback`. C's Before→After span covers the entire async round trip and the AfterWrite status reflects the real `ppnb->dbPutNotify.status`.

Impact: caPutLog and put-loggers record every WRITE_NOTIFY on an async record as "completed=ok" the moment `dbProcessNotify` was kicked off — never observing real outcome. AfterWrite latency = 0 instead of actual device-side duration; genuine PUTFAIL logged as "ok".

### R2-85: TrapWriteMessage drops C's `dbrType`, `no_elements`, raw `data`, and `userPvt` continuation slot

Severity: Medium

Rust: `access_security.rs:602-621` — `TrapWriteMessage` carries `pv_name`, `user`, `host`, `peer`, `value_str` (pre-rendered, truncated to 64 chars), `status`, `rule_was_trap`. No `dbrType`, no `no_elements`, no raw `data`, and no per-event `userPvt` for listeners to stash state across Before/After.

C reference: `libcom/src/as/asTrapWrite.h:34-56` `asTrapWriteMessage` exposes `dbrType`, `no_elements`, `data`, `serverSpecific` (the dbChannel pointer), and `userPvt` — explicitly documented at `:45-51` as "When the listener is called before the write, this has the value 0. The listener can give it any value it desires and it will have the same value when the listener gets called again after the write." `asTrapWrite.c:144-147` initialises `userPvt=0` in Before, snapshots whatever the listener wrote, restores it in After.

Impact: Port of caPutLog or any logger that times the put or wants old-vs-new dual values cannot work — no way to stash start-time in Before and read in After; no array-length/dbrType for richer audit lines.

### R2-86: Listener panic propagates out of dispatch_trap_write and kills the per-connection task

Severity: Medium

Rust: `access_security.rs:660-672` — `dispatch_trap_write` calls `listener(msg)` directly with no `catch_unwind`. Call sites in `tcp.rs:2536-2549` / `:2577-2590` run on the per-circuit `handle_client` task. A panic unwinds dispatch_message → handle_client; the connection (and every subscription on it) is dropped.

C reference: `libcom/src/as/asTrapWrite.c:135-151` invokes `plistener->func(...)` under `pasTrapWritePvt->lock`. C has no unwind concept; a panicking listener brings the whole IOC down uniformly.

Impact: A buggy listener terminates exactly one CA connection. Symptom: "client randomly disconnects mid-session" with no audit trail. Remaining listeners for that BeforeWrite never fire; paired AfterWrite never dispatched.

### R2-87: dispatch_trap_write deadlocks if a listener registers or drops a TrapWriteListenerHandle

Severity: Medium

Rust: `access_security.rs:660-672` holds `reg.read()` (`std::sync::RwLock` reader) for the entire iteration. If the listener calls `register_trap_write_listener` (acquires `reg.write()` at `:638-645`) or drops a handle (acquires `reg.write()` in Drop at `:618-623`), same thread requests writer while holding reader. `std::sync::RwLock` is not re-entrant → hard deadlock on POSIX.

C reference: `asTrapWrite.c:135-151` invokes listener while `pasTrapWritePvt->lock` held; register/unregister `epicsMutexMustLock` same mutex. `epicsMutex` on POSIX uses `PTHREAD_MUTEX_RECURSIVE`; re-entry safe.

Impact: Listener that auto-rotates registration, or any test harness that drops a handle from inside a callback, deadlocks the connection. No timeout — task stuck until OS tears the socket down.

### R2-88: ASG forwarder task leaks per server restart and ASG event delivery never quiesces

Severity: Medium

Rust: `server/tcp.rs:602-626` — ASG → acf_reload forwarder is `tokio::spawn(async move { loop { match asg_rx.recv().await { ... } } })`; the `JoinHandle` is dropped (not on `accept_tasks` or any abort handle). Receiver loop only exits on `RecvError::Closed`, which only fires when the global `ASG_CHANGE_BROADCAST` Sender drops — but that Sender lives in a process-lifetime `OnceLock` static. When `run_tcp_listener` is cancelled, forwarder keeps running, holding its `acf_reload_tx_t` clone. A subsequent `run_tcp_listener` spawns *another* forwarder; every ASG put fans out into N stale `acf_reload_tx` clones whose receivers are gone.

C reference: `asInitCommon:144` registers SPC_AS callback exactly once via `dbSpcAsRegisterCallback`, `epicsThreadOnce`-guarded. C has no "restart rsrv listener" operation.

Impact: Long-running processes with intentional restart cycles (test fixtures, fault-tolerant supervisors, in-process gateway with reconfiguration) accumulate one zombie forwarder per restart. Each ASG put wakes N tasks; memory grows.

### R2-89: Listener registry holds std::sync::RwLock::write inside Drop on a tokio worker

Severity: Low

Rust: `access_security.rs:618-623` — `Drop for TrapWriteListenerHandle` calls `reg.write()`, blocking OS primitive. Handle is `Send` so owned by tokio tasks; Drop runs on whatever worker schedules the future. Register-Drop racing dispatch reader on a different worker blocks the worker for the dispatch duration — and dispatch holds the reader for the entire `listener(msg)` chain (unbounded per the doc).

C reference: `asTrapWrite.c:87-111` `asTrapWriteUnregisterListener` `epicsMutexMustLock`s on a single-process-IOC thread; blocking acceptable.

Impact: Worker stuck waiting for writer cannot progress other futures. With `worker_threads=1`, runtime stalls until dispatch completes. Symptom: "test occasionally hangs".

### R2-90: BeforeWrite fires for write_hook rejections that C would never have logged

Severity: Low

Rust: `server/tcp.rs:2536-2569` — BeforeWrite dispatched unconditionally before `entry.target` matches and `write_result` is computed. For `ChannelTarget::SimplePv`, a registered `write_hook` may reject; the put never reaches storage but BeforeWrite has already been logged. AfterWrite at `:2577` fires with `status=Some("fail")`. Same pattern for any `put_record_field_from_ca` error from `instance.record.put_field` (SPC_NOMOD, type-coerce rejection).

C reference: `rsrv/camessage.c:741-779` — `rsrvCheckPut` is the only gate before `asTrapWriteWithData`; non-AS rejections (`caNetConvert` type mismatch at `:753-766`) return RSRV_ERROR before reaching `:768`. Once `asTrapWriteWithData` fires, `dbChannel_put` runs unconditionally and `asTrapWriteAfter:779` always follows.

Impact: caPutLog-faithful consumers expect "Before always paired with an attempt that reached storage". Rust generates Before + After=fail pairs for write-hook-rejected puts that C silently drops.

### R2-91: `put_pv` / `put_pv_no_process` bypass the ASG-field notifier

Severity: Low

Rust: `database/field_io.rs:53-150` `put_pv` and `:683-...` `put_pv_no_process` both write via `instance.put_common_field(&field, value)`, which at `record_instance.rs:925-929` handles `"ASG"` by assigning `self.common.asg = s`. Neither path contains the `if field == "ASG" { notify_asg_field_changed() }` block added to `put_record_field_from_ca:559-561`. Callers include gateway shadow PV writes, IOCsh sequencer scripts, autosave-style restore on startup, internal admin tools.

C reference: `dbAccess.c:113-145` `dbPutSpecial` invoked from `dbPut` (lowest-level put primitive) for every field whose `paddr->special == SPC_AS`, regardless of caller entry path. SPC_AS callback chain fires for any put to `.ASG` including restore/admin paths.

Impact: Restore script writing `.ASG` at IOC startup via `put_pv` (autosave parity), or gateway mirroring `.ASG` via `put_pv_and_post`, mutates `common.asg` without firing the re-eval notifier. Live CA clients see stale cached access-level until next ACF reload, CLIENT_NAME, or asg-field write via `put_record_field_from_ca`. Exact mirror of the bug R2-54 just fixed for the CA-write path.

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

### R2-42: `pending_access` map now bounded and evicted oldest-first

`client/transport.rs::handle_access_rights_response` caps the per-circuit
`pending_access` map at `PENDING_ACCESS_CAP = 1024` and evicts the oldest
entry (FIFO via `keys().next()`) when full, with a
`ca_client_pending_access_evictions_total` metric for diagnostics. A
misbehaving server's stray ACCESS_RIGHTS frames no longer grow without
bound; `cac.cpp`-style silent-drop semantics for unknown cids is preserved
by leaving the event-emit path intact.

### R2-60: CA server UDP responders now join multicast groups from `EPICS_CAS_INTF_ADDR_LIST`

`server/addr_list.rs::from_env` partitions parsed entries on `is_multicast()`
into `CasUdpConfig::intf_addrs` (unicast) and `CasUdpConfig::mcast_addrs`
(`224.0.0.0/4`). `server/udp.rs::run_udp_search_responder` spawns one
`run_multicast_responder` task per group: it binds wildcard `0.0.0.0:port`,
calls `join_multicast_v4` for every non-wildcard interface in `intf_addrs`,
then runs the standard `recv_loop`. Mirrors `rsrv/caservertask.c:367-371,
633-668` (`casMCastAddrList` + per-NIC `IP_ADD_MEMBERSHIP`). Multicast
SEARCH topologies now receive replies from a Rust IOC configured with
multicast intf entries.

### R2-43: Multiply-defined PV diagnostic now emitted (IP-only variant)

`client/search.rs::SearchEngineState` gained a `resolved:
HashMap<u32, (String, SocketAddr)>` map (FIFO-evicted at 1024 entries,
dropped on `remove_channel`). The Found path inserts the resolution;
a *second* SEARCH reply for the same cid hits the new
`else if let Some((pv_name, prev_addr)) = state.resolved.get(&cid)`
branch and — when the new server differs — logs at warn:
`Channel multiply defined: PV is also hosted on a second server`
with `pv`/`cid`/`connected_to`/`but_also_on` fields, plus a
`ca_client_multiply_defined_pv_total` metric. Partial vs. libca: no
async DNS lookup, so we emit IPs instead of hostnames — adding a
resolver to the search hot path would be a heavier change than the
diagnostic warrants.

### R2-53: TRAPWRITE listener subsystem wired

`epics-base-rs::server::access_security` gained
`register_trap_write_listener` / `dispatch_trap_write` /
`TrapWriteMessage` / `TrapWriteListenerHandle` (RAII unregister) /
`has_trap_write_listeners` (cheap probe). `ca-rs::server::tcp.rs`
WRITE/WRITE_NOTIFY arms call `dispatch_trap_write` with
`BeforeWrite` / `AfterWrite` ops around every `dbChannel_put`-
equivalent. Zero-cost when no listener is registered (single
`RwLock::read` + `is_empty` probe). Partial vs. libca: the matched
ACF rule's `TRAPWRITE` mask is forced to `true` at the dispatch
sites (loggers see every write; over-reports rather than misses).
Surfacing the per-rule mask through `AccessChecked` is a follow-up
— caPutLog-style audit prefers over-reporting to silent gaps.

### R2-54: ASG-field puts now re-evaluate access rights for live clients

`epics-base-rs::server::access_security` gained a process-wide
`tokio::sync::broadcast` channel exposed via
`notify_asg_field_changed` (producer) and `subscribe_asg_changes`
(consumer). `database/field_io.rs::put_record_field_from_ca` fires
the notifier whenever `field == "ASG"`. `ca-rs::server::tcp.rs::
run_tcp_listener` spawns a forwarder task at startup that pumps
each notification into the existing `acf_reload_tx`, which
re-enters the per-client `reeval_access_rights` path. Coarser than
libca's per-ASGCLIENT dispatch (we re-eval every connection), but
the downstream `oldaccess != access` filter already gates wire
ACCESS_RIGHTS to only the channels whose level actually changed —
matching C's bounded-traffic property.

### R2-48: UDP SEARCH replies now batched into a single outbound datagram

`server/udp.rs::run_single_responder` per-datagram loop now accumulates
SEARCH match replies into `send_buf` and flushes once at end-of-datagram
(or earlier when the buffer would exceed a 1472-byte MTU threshold, with
VERSION placeholder re-seeded after each flush — mirroring
`cas_send_dg_msg`). N matches in one inbound datagram yield one outbound
datagram instead of N (was N× IP overhead / N× sendto cost / search-
storm amplification). Mirrors `rsrv/cast_server.c:163-281` +
`caserverio.c:185-201`.

### R2-44: VERSION priority field is wire-equivalent at default

`client/transport.rs` builds VERSION with `data_type = 0`. The Rust client
does not expose a per-context priority parameter, and the C wire payload
for the default priority is also `0` (priLev=0). Default operation
produces byte-identical frames to libca. Recorded as a divergence in
exposed API surface, not a wire bug — closing without an API change.
Re-opening this requires a new public `epics_ca_rs::Context::priority`
knob; deferred until a caller asks for non-default priority.

### R2-56: ACF reload race is bounded by next op trigger

Narrow window: ACF reload while no channels exist leaves no per-client
signal, and the next op uses cached `lookup_access` for one cycle until
`reeval_access_rights` re-fires. C behaviour relies on
`asAddClient`/`asAddMemberPvt` re-attaching freshly-rebuilt member
lists on the *next* claim path, so C has the same one-cycle window
when the reload races a CLIENT_NAME parse. Wire-equivalent in practice;
closing without further change.

### R2-58: REPEATER_REGISTER 16-byte frame is accepted by all modern repeaters

Wontfix. Rust `client/beacon_monitor.rs::register_with_repeater` and
`client/repeater.rs` send a full 16-byte CaHeader. libca's zero-length
default plus `attemptNumber & 1` source toggle exists only for pre-3.12
(1998-era) C repeater compatibility, which no longer ships. Modern
repeaters accept the 16-byte shape. Divergence in emitted shape only,
no functional impact on any supported deployment.

### R2-64: pending_access cap-hit now warns once per circuit

`client/transport.rs::read_loop` adds a `cap_warned` boolean and
fires one `tracing::warn!` (with the `epics_ca_rs::client::transport`
target) the first time `PENDING_ACCESS_CAP` is exceeded; subsequent
evictions stay silent but the `ca_client_pending_access_evictions_total`
metric continues to climb. Operators see the misbehaving-server
signal at warn level instead of needing to scrape Prometheus.

### R2-65: AccessRightsChanged event gated on known cid

`client/transport.rs::read_loop` now tracks a `known_cids:
HashSet<u32>` populated on CREATE_CHAN reply (and cleared on
SERVER_DISCONN). An ACCESS_RIGHTS frame for a known cid is a
post-create ACF update and fires `TransportEvent::AccessRightsChanged`
directly. An ACCESS_RIGHTS frame for an unknown cid is stashed (and
later consumed by the matching CREATE_CHAN, which carries the
access via `ChannelCreated`) but no spurious event is emitted.
Under R2-42 stray-frame stress the unbounded `event_tx` no longer
carries one message per stray frame.

### R2-66: Multiply-defined detection window extends to connected-channel lifetime

`client/search.rs::ConnectResult{success:true}` no longer calls
`remove_channel(cid)` (which dropped the `resolved` entry); it now
takes `cid` out of `pending`/`buckets`/`attempts` directly and
calls the new `mark_connected` helper, leaving `resolved` intact.
The map is only cleared on `Cancel` / channel-drop. A late SEARCH
reply from a second IOC announcing the same PV — arriving seconds
after the connect handshake completed — now fires the duplicate-
detect path, matching libca `cac.cpp:621-641`'s connected-lifetime
detection window.

### R2-67: ECA_DBLCHNL delivered via the exception handler

`client/types.rs` `SearchResponse::MultiplyDefined { pv_name,
prev_addr, new_addr }` carries the duplicate-detection event to
the coordinator (`client/mod.rs`), which calls
`types::dispatch_exception` with `kind=ServerError`,
`status=ECA_DBLCHNL`, and a libca-shape message `"Channel: \"<pv>\",
Connecting to: <prev>, Ignored: <new>"`. Library users that
registered `CaClient::set_exception_handler` (the documented analog
of `ca_add_exception_event`) now see the condition — matching
libca's `pvMultiplyDefinedNotify` → `this->exception(...
ECA_DBLCHNL, ...)` path.

### R2-68: Duplicate-detect lifted above penalty / circuit-breaker gates

`client/search.rs::handle_search_response` moves the
`state.resolved.get(&cid)` duplicate-check branch ABOVE the
penalty-box and `state.breakers.is_blocking(server_addr)` filters.
Emit does not consume any reply state, so firing on a
penalized/breaker-open server's reply is safe. A flaky duplicate
server's SEARCH reply now triggers the multiply-defined warn even
when we then decline to attempt connect — matching libca's
unconditional duplicate-detect.

### R2-69: Duplicate-detect lifted above the `last_valid_seq` gate

Same restructuring as R2-68: the duplicate-detect branch runs
before the `if state.last_valid_seq.is_none() { continue }` stale-
datagram gate. A SEARCH-only datagram (no preceding VERSION;
non-conformant but observed on older fan-outs) now triggers the
warn for a duplicate server. Matches libca
`cac.cpp::transferChanToVirtCircuit` which has no seq-number gate
between datagram receipt and the multiply-defined emit.

### R2-61: AUTO_BEACON expansion now honours `EPICS_CAS_INTF_ADDR_LIST` filter

`server/addr_list.rs` `expand_auto_beacon` (the AUTO sentinel expansion
path) now filters discovered broadcast addresses by `cfg.intf_addrs`: when
non-wildcard interface IPs are present, only those NICs' broadcast addrs
are emitted (via the existing `broadcast_for_ip` helper). A wildcard
(`0.0.0.0`) entry mixed with specific IPs is treated as the C
`cantProceed`-equivalent and logged at warn. Mirrors
`rsrv/caservertask.c:374-388, 390-392`. Multi-NIC IOC bound to one
isolated subnet no longer leaks beacons onto unrelated networks.

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
- 2026-05-18: Ran Codex-style multi-agent audit (4 parallel sub-agents
  covering libca client wire/lifecycle, rsrv non-RWE opcodes, access
  security / ACF, beacon / repeater / discovery). 27 new divergences
  recorded as R2-37 through R2-63. Theme summary: (a) disconnect /
  unresponsive lifecycle does not fan exception out to per-IO/per-sub
  waiters (R2-37/38/39/40), (b) TCP dispatcher misclassifies known-valid
  opcodes as unknown after R2-29 (R2-41), (c) ACF state model gaps —
  COAR no-change suppression missing, TRAPWRITE not wired, runtime
  ASG change has no re-eval trigger, cap-token METHOD/AUTHORITY not
  propagated (R2-51/52/53/54), (d) env-var parser divergences across
  AUTO_ADDR_LIST / NAME_SERVERS / MAX_SEARCH_PERIOD / CONN_TMO
  (R2-57/59/62/63), (e) multicast and per-interface beacon scoping
  missing (R2-60/61), (f) UDP search batching, accept-time VERSION,
  duplicate-sub_id wire shape (R2-47/48/49/50), (g) miscellaneous wire
  parity (R2-42/43/44/45/46/55/56/58).
- 2026-05-18: Ran a focused Codex-style audit on the 6 commits that
  fixed R2-42 / R2-43 / R2-48 / R2-53 / R2-54 / R2-60 / R2-61. Three
  parallel sub-agents covered (i) client R2-42/43 surface
  (`cac.cpp::accessRightsRespAction` + `transferChanToVirtCircuit`),
  (ii) server UDP/mcast/beacon R2-48/60/61 surface (`cast_server.c`
  + `caservertask.c` + `caserverio.c`), (iii) ACF/listener R2-53/54
  surface (`asLib.h` + `asTrapWrite.c` + `asDbLib.c` + `rsrv/camessage.c`
  write paths). 24 NEW divergences recorded as R2-64..R2-69 (client),
  R2-74..R2-83 (server UDP/mcast/beacon), R2-84..R2-91 (ACF/ASG/listeners).
  Theme summary:
  (a) the new fixes added cap-eviction / detection-window / dispatcher
  patterns that under-deliver vs. C's silent-success-or-loud-fail model
  (R2-64/65/66/67/68/69 — multiply-defined PV not via ECA_DBLCHNL, post-
  connect detection window closed, penalty filter swallows the warn);
  (b) multicast responder error/source-IP/socket-scope divergences
  that R2-60's wildcard-bind shortcut introduced (R2-74/75/83);
  (c) AUTO_BEACON misconfig escape and P2P interface drop (R2-76/77 —
  R2-76 is severity HIGH because C `cantProceed` is fatal and Rust
  silently misconfigs); (d) UDP batch parity is within-datagram only
  (R2-79), VERSION ordering assumption brittle (R2-80), batched send
  errors swallowed (R2-81), MTU constant 1472 vs C's 1024 (R2-82);
  (e) TRAPWRITE listener API parity gaps — AfterWrite premature on async
  records (R2-84 severity HIGH), `userPvt` / `dbrType` / `data` /
  `no_elements` dropped from message (R2-85), no panic isolation (R2-86),
  re-entrant deadlock (R2-87), blocking Drop on tokio worker (R2-89),
  Before-without-After-success for write_hook rejections (R2-90);
  (f) ASG forwarder task leaks per server restart (R2-88) and
  `put_pv` / `put_pv_no_process` bypass the ASG notifier the way
  CA-side R2-54 was just fixed (R2-91).
  Two HIGH-severity items: R2-76 (silent invalid-config startup) and
  R2-84 (caPutLog AfterWrite fires before async completion). The audit
  found zero re-reports of R2-1..R2-63 findings.
