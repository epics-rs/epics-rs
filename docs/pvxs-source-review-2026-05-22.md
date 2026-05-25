# pvxs Source Review - 2026-05-22

Scope:

- Local pvxs checkout: `/Users/stevek/codes/pvxs`
- Areas reviewed: IOC qgroup get/put, access-security logging, IOC scalar/array put conversion, pvaLink callback/lifecycle paths, UDP search decode spot checks.
- This is a source review only. No pvxs source files were changed.

## Findings

### PVXS-SR-1 - `SecurityLogger` never restores `dbChannel::addr.pfield` after `asTrapWrite` callbacks

Severity: High

Status: Open

Evidence:

- `ioc/securitylogger.h:24-26` stores `pfieldsave`, `pchan`, and `pvt`, with `pchan` defaulting to `nullptr`.
- `ioc/securitylogger.h:47-55` saves `pDbChannel->addr.pfield` and calls `asTrapWriteWithData(...)`, but the constructor never assigns `pchan = pDbChannel`.
- `ioc/securitylogger.h:31-32` and `ioc/securitylogger.h:62-63` restore `pchan->addr.pfield` only when `pchan` is non-null, so both restore sites are unreachable for constructed loggers.
- `ioc/iocsource.cpp:363-374` installs this logger during put preprocessing, before the later DB put uses the same channel.

Impact:

The comments in `SecurityLogger` state that `asTrapWrite` callbacks may clobber `dbChannel::addr.pfield`. Because `pchan` is never set, pvxs does not restore the saved field pointer after the callback. A put with `TRAPWRITE` logging can continue with a clobbered `pfield`, which can corrupt the field used by the actual write or leave the channel state corrupted for later use.

Fix direction:

Initialize `pchan` from `pDbChannel` in the `SecurityLogger` constructor before any restore path can run, then add a regression test using an `ASG(... TRAPWRITE ...)` path that verifies the dbChannel field pointer is restored around a put.

### PVXS-SR-2 - qgroup non-atomic put uses the wrong `SecurityClient` after a channel-less field

Severity: High

Status: Open

Evidence:

- `ioc/groupsource.cpp:171-180` sizes `securityClients` to `group.fields.size()` and populates entries using the full field index, incrementing for every field.
- `ioc/fieldconfig.h:37` gives fields without `+putorder` the minimum put order. `ioc/groupconfigprocessor.cpp:253-257` sorts fields by that value.
- `ioc/groupconfigprocessor.cpp:148-154` explicitly permits `Structure` and `Const` fields without a channel. `test/ntenum.db:9-10` has a channel-less `value` structure before the puttable `value.index` field.
- `ioc/groupsource.cpp:573-580` increments `fieldIndex` for every field in the atomic put branch.
- `ioc/groupsource.cpp:589-600` skips channel-less fields with `continue` in the non-atomic put branch, and increments `fieldIndex` only after a channel-backed field is processed.

Impact:

After the first channel-less `Structure` or `Const` field, the non-atomic put branch indexes `groupSecurityCache.securityClients` with a value that no longer matches the current `group.fields` entry. The actual write can be authorized, denied, and logged using the access-security client for a different field. This is both an authorization bug and an audit-log integrity bug. The atomic put path does not have the same index drift.

Fix direction:

Make non-atomic put preserve the field-to-security-client index. The smallest structural fix is to iterate with an explicit field index and skip only the DB operation when `field.value` is null, not the index advancement. A stronger fix is to attach the security client to the field iteration record so the index cannot diverge across branches.

### PVXS-SR-3 - qgroup non-atomic get omits `Const` fields

Severity: Medium

Status: Open

Evidence:

- `ioc/iocsource.cpp:319-325` supports `MappingInfo::Const` by assigning `info.cval` into the output node.
- `ioc/groupsource.cpp:448-455` atomic get calls `getGroupField()` for every field except `Proc` and `Structure`, so `Const` fields are included.
- `ioc/groupsource.cpp:369-373` monitor setup also populates `Const` fields once.
- `ioc/groupsource.cpp:466-478` non-atomic get only calls `getGroupField()` inside `if (pDbChannel && leafNode)`.
- `ioc/groupconfigprocessor.cpp:148-154` clears/forbids channels for `Const`, so `pDbChannel` is null for `Const` fields by construction.

Impact:

A client requesting non-atomic get with `record[atomic=false]`, or a group configured for non-atomic access, receives a different value than atomic get/monitor for the same group: `Const` fields are left unassigned instead of being populated from `info.cval`. Existing tests cover atomic const get and monitor (`test/testqgroup.cpp:687-717`), but not non-atomic const get.

Fix direction:

Handle `MappingInfo::Const` before the `pDbChannel` guard in the non-atomic get branch, or make both get branches share one field-read helper that treats channel-less readable fields consistently.

### PVXS-SR-4 - server monitor stats report `nSquash` in `nQueue`

Severity: Medium

Status: Open

Evidence:

- `src/pvxs/source.h:57-67` defines `MonitorStat::nQueue` as the current unsent queue depth and `MonitorStat::nSquash` as the number of squashed updates.
- `src/servermon.cpp:311-315` assigns `stat.nQueue = mon->queue.size()`, then assigns `stat.nQueue = mon->nSquash`.
- `src/servermon.cpp:312-314` correctly fills the adjacent `maxQueue`, `limitQueue`, and `window` fields, but `nSquash` is never assigned.

Impact:

Server-side callers of `MonitorControlOp::stats()` cannot observe the real queue depth when squashing has occurred, and never receive the squash count in the intended `nSquash` field. Code that uses `nQueue` for backpressure or diagnostics gets a counter with different semantics.

Fix direction:

Change the second assignment to `stat.nSquash = mon->nSquash` and add a regression that forces queue squashing, then verifies both `nQueue` and `nSquash`.

### PVXS-SR-5 - `ackAny` percentage is interpreted as a multiplier, not a percentage

Severity: Medium

Status: Open

Evidence:

- `src/servermon.cpp:561-565` parses strings ending in `%`, clamps the parsed number to `[0, 100]`, then computes `op->ackAt = percent * op->limit`.
- `src/clientmon.cpp:774-803` repeats the same percentage handling on the client side with `op->ackAt = uint32_t(percent * op->queueSize)`.
- `test/testmonpipe.cpp:167` exercises `"50%"`, but only verifies completion and does not assert the negotiated ack threshold.

Impact:

`ackAny="50%"` with queue size 4 computes 200, then clamps to the queue size. That makes the effective threshold 100%, not 50%. Any practical percentage at or above 1% becomes "ack at full queue" after clamping, defeating the documented percentage-style control.

Fix direction:

Compute `ackAt` from `percent / 100.0 * queueSize` on both client and server parsing paths. Add a test that inspects the actual ack threshold or observable ACK cadence for `"50%"`.

### PVXS-SR-6 - pvaLink captures `timeStamp.userTag` but never publishes it to the record link API

Severity: Medium

Status: Open

Evidence:

- `ioc/pvalink_link.cpp:105-107` caches `timeStamp.secondsPastEpoch`, `timeStamp.nanoseconds`, and `timeStamp.userTag` into `fld_seconds`, `fld_nanoseconds`, and `fld_usertag`.
- `ioc/pvalink_lset.cpp:394-404` updates `snap_time` from `fld_seconds` and `fld_nanoseconds`.
- `ioc/pvalink_lset.cpp:571-582` returns `snap_tag` through `pvaGetTimeStampTag()`.
- `ioc/pvalink.h:248-249` initializes `snap_tag` to 0, and `rg snap_tag` finds no assignment from `fld_usertag`.

Impact:

On EPICS versions using `getTimeStampTag`, a PVA link always returns the initial user tag instead of the remote PV's `timeStamp.userTag`. Records using user tags through PVA links lose that metadata even though the monitor cache already captures it.

Fix direction:

Update `snap_tag` from `fld_usertag` in the same snapshot block that updates `snap_time`, with a zero/default fallback when the field is absent. Add a pvaLink regression with a non-zero remote `timeStamp.userTag`.

### PVXS-SR-7 - pvaLink type-change cleanup leaves `fld_seconds` stale

Severity: Medium

Status: Open

Evidence:

- `ioc/pvalink.h:237-243` lists cached metadata fields: `fld_value`, `fld_severity`, `fld_message`, `fld_seconds`, `fld_nanoseconds`, `fld_usertag`, and `fld_meta`.
- `ioc/pvalink_link.cpp:87-88` clears `fld_value`, `fld_severity`, `fld_nanoseconds`, `fld_usertag`, `fld_message`, and `fld_meta`, but omits `fld_seconds`. It also assigns `fld_severity` twice.
- `ioc/pvalink_link.cpp:99-102` handles a non-structure root by setting only `fld_value = root`, leaving any previous `fld_seconds` untouched.
- `ioc/pvalink_lset.cpp:394-404` uses `fld_seconds` to update `snap_time`.

Impact:

If a PVA link changes from a structure carrying `timeStamp.secondsPastEpoch` to a non-structure value, `pvaGetValue()` can keep using the old seconds field while the current value has no timestamp metadata. That produces stale record timestamps after a remote type change.

Fix direction:

Clear every cached metadata `Value` in one helper or explicit list, including `fld_seconds`, before rebuilding the cache in `onTypeChange()`. Add a type-change regression from timestamped NTScalar to a plain scalar.

## Second Pass - core wire / framing / client decode (2026-05-22)

SR-1 through SR-7 were fixed on branch `fix/source-review-2026-05-22` (upstream PR epics-base/pvxs#179). The findings below are a follow-up critical pass over the network-facing core that those did not touch: the wire (de)serializer, the TCP message framing layer, and the client monitor decode path. They are review-only and remain Open. Scope was Critical/High defects on peer/attacker-controlled input (out-of-bounds, unbounded allocation, null dereference); sub-threshold observations are listed under "Not Recorded As Findings".

### PVXS-SR-8 - wire decoder allocates from an unvalidated peer-supplied count before reading any payload

Severity: High

Status: Open

Evidence:

- `src/pvaproto.h:284-304` - `from_wire(Buffer&, Size&)` decodes a count up to `0xffffffff` (the `s==254` path reads a 4-byte `uint32`); the value is not bounded against the buffer.
- `src/dataencode.cpp:143-150` - decoding a `Struct`/`Union` type description reads the child count `nfld` and then `fld.miter.reserve(nfld.size)` *before* the child loop (`:155-164`) reads or validates a single child byte.
- `src/dataencode.cpp:607-610`, `:624-627`, `:656-659` - decoding a `StructA`/`UnionA`/`AnyA` field value does `shared_array<Value> arr(alen.size)` before the per-element validity-byte loop. `shared_array(size_t c)` (`src/pvxs/sharedArray.h:300-302`) is `new _E_non_const[c]`, which default-constructs (and therefore touches) every element.
- `src/pvaproto.h:523-525` - decoding a POD scalar array (`Int32A`, `Float64A`, ...) does `shared_array<E> arr(slen.size)` before the `buf.ensure(sizeof(C))` fill loop (`:537`).

Impact:

A peer can put a small frame on the wire whose embedded count field is up to ~4.29e9 while the actual body is a few bytes. Each site allocates that many elements/bytes before consuming the corresponding wire bytes, so the count is never checked against the remaining frame. On most hosts the oversized `reserve`/`new[]` (tens to hundreds of GiB) throws `bad_alloc`, which is caught at `src/conn.cpp:277` and disconnects; but the `StructA`/`UnionA`/`AnyA` path default-constructs every `Value`, committing and touching real memory before the short-read fault, so a moderate count (e.g. 100M) produces a real multi-GiB RSS spike per connection. This is reachable pre-authentication: `ServerConn::handle_CONNECTION_VALIDATION` (`src/serverconn.cpp:208`) decodes a peer type+value during the auth handshake. Repeated across connections this is an unauthenticated remote memory-exhaustion / OOM DoS. A frame-size cap (SR-9) does not mitigate it, because the count is independent of the body length.

Fix direction:

Clamp the wire count to the bytes that could possibly remain before allocating - every child/element consumes at least one wire byte, so `std::min(count, buf.size())` (POD: `buf.size()/sizeof(C)`) is a sound upper bound - or grow incrementally (`emplace_back` per successfully-decoded element) instead of pre-sizing. Apply at all four sites.

### PVXS-SR-9 - TCP framing has no maximum message or segment size

Severity: High

Status: UNFIXED (deferred, intentional, 2026-05-22) - pvxs intentionally imposes no default ceiling on inbound application-message or segment size; the advertised `serverReceiveBufferSize` is a flow-control hint, not an RX limit. This unbounded-by-default behavior is the parity baseline that downstream ports (epics-pva-rs) deliberately match: a hard cap is opt-in only and must not be re-added by default, because it would reject currently-accepted large messages (e.g. large arrays) and diverge from the protocol's negotiated-buffer model. A deployment that needs to bound inbound size should opt in (e.g. a configurable max enforced before arming the read watermark at `src/conn.cpp:206` and before the per-segment `evbuffer_remove_buffer` at `:218`). This is the root precedent SR-19 defers to. Re-open only if upstream decides to add a default ceiling for the message-size family.

Evidence:

- `src/conn.cpp:201-214` - the application-message body length `len` is the peer's 32-bit header field; when the full body is not yet buffered, the read watermark is set to `8 + len` (up to ~4 GiB) with no maximum.
- `src/conn.cpp:216-220` - the body is moved into `segBuf` via `evbuffer_remove_buffer(rx, segBuf.get(), len)`.
- `src/conn.cpp:244-289` - `segBuf` is drained only when `!seg || seg==SegLast`; a `SegFirst` followed by an unbounded run of `SegMiddle` frames accumulates into `segBuf` without limit (the continuation/`segCmd` checks at `:231` are satisfied by a well-formed stream).
- No maximum receive message size constant exists in `src/` or `include/`; the connection advertises `serverReceiveBufferSize = 0x10000` (`src/serverconn.cpp:104`) but never enforces it on RX.

Impact:

An unauthenticated peer can (1) declare a single frame with `len` near `0xffffffff` and let libevent accumulate up to ~4 GiB in the connection input buffer, or (2) send `SegFirst` then an endless stream of `SegMiddle` frames so `segBuf` grows without bound. The 40 s `tcpTimeout` is a read-inactivity timer and is reset by a slow byte trickle, so neither vector trips it. Repeated across connections this is a remote memory-exhaustion DoS. The code lives in the shared `ConnBase`, so both the server and the client are affected.

Fix direction:

Define a maximum accepted message/body size (a few MiB, consistent with the advertised receive-buffer size) and force-disconnect at `src/conn.cpp:206` when `len` exceeds it, before arming the watermark. Separately, track the running `segBuf` total across segments and force-disconnect when it exceeds the cap, before the `evbuffer_remove_buffer` at `:218`.

### PVXS-SR-10 - client MONITOR data message before the INIT reply dereferences a null `info->fl`

Severity: High

Status: Open

Evidence:

- `src/clientmon.cpp:481-496` - `info` is resolved from `opByIOID`, where an operation is present from MONITOR-INIT send time while its `info->fl` is still null (`RequestInfo` does not initialize `fl`).
- `src/clientmon.cpp:504-509` - the data branch (`else if(!final || !M.empty())`) takes `Guard G(info->fl->lock);` with no null or state check.
- `src/clientmon.cpp:654-658` - `info->fl` is assigned only in the `else if(init)` (successful INIT-reply) branch, under `mon->lock`.
- `src/clientmon.cpp:563-594` - the subcmd-vs-state validation that would reject a data message while `state==Creating` runs only after the decode block, i.e. after the `:509` dereference.

Impact:

A server (or man-in-the-middle) that has acknowledged CREATE_CHANNEL - so the client has sent MONITOR INIT and the op sits in `opByIOID` with `state==Creating` and `fl==nullptr` - can send a MONITOR data message before the INIT reply: either a non-init/non-final subcmd, or a final (`0x10`) message carrying trailing bytes (`!M.empty()`), with a success status. Decode reaches `info->fl->lock` while `fl` is null, producing a null-pointer dereference and crashing the client process. This is a remote-triggerable client DoS.

Fix direction:

Move the operation-state validation (`info->handle.lock()`, `op->op==CMD_MONITOR`, and the state-vs-subcmd check) ahead of the payload-decode block so a data message in the `Creating` state is rejected with `M.fault()` before any access to `info->fl`; this makes "payload is decoded only after a successful INIT" hold by construction. A minimal guard is `if(!info->fl) M.fault(__FILE__, __LINE__);` at the top of the data branch.

### PVXS-SR-11 - value decoder recurses without bound through nested `Any`, bypassing the descriptor depth guard (stack overflow / remote crash)

Severity: Critical

Status: Open

Evidence:

- `src/dataencode.cpp:69-74` - the type-description builder caps nesting with `if(!buf.good() || depth>20)` and threads `depth+1` through each recursive descriptor call (`:87`, `:132`, `:160`).
- `src/dataimpl.h:102` - that builder's depth parameter defaults to 0: `from_wire(Buffer&, std::vector<FieldDesc>&, TypeStore&, unsigned depth=0)`.
- `src/dataencode.cpp:451` (`from_wire_field`) and `:691` (`from_wire_full`) - the *value* decoders carry no depth counter at all.
- `src/dataencode.cpp:542-561` - decoding an `Any` field value calls `from_wire(buf, *descs, ctxt)` (line 545) with the default `depth=0`, then `from_wire_full(buf, ctxt, fld)` (line 557), which re-enters `from_wire_field`. The `AnyA` array-element loop (`:665`) does the same.
- `src/dataencode.cpp:202-203` - `TypeCode::Any` is accepted as a single-byte leaf in the descriptor builder, so each nested `Any` costs exactly one wire byte (`0x82`).

Impact:

The `depth>20` guard is the only nesting bound, and it is reset to 0 every time an `Any` *value* is decoded (line 545 reads the inner type with `depth` defaulting to 0). The value-decode call chain `from_wire_field -> from_wire(depth 0) -> from_wire_full -> from_wire_field -> ...` therefore has no global bound. Because an `Any` descriptor is a single byte, a message body that is just a run of `0x82` bytes drives one recursion level per byte; a sub-megabyte payload (tens of thousands of levels) exhausts the thread stack. A stack overflow is a hard `SIGSEGV`, not a `std::exception`, so it is NOT caught by the `catch(std::exception&)` around message dispatch (`src/conn.cpp:277`) and crashes the whole process. It is reachable pre-authentication on the server: `ServerConn::handle_CONNECTION_VALIDATION` decodes a peer-supplied type+value via `from_wire_type_value` (`src/serverconn.cpp:208`), whose top type may be `Any`. It is also reachable post-channel via a PUT value or pvRequest (`src/serverget.cpp:369,446`, `src/servermon.cpp:492`), and on the client via any GET/MONITOR/RPC reply value from a malicious server. Unauthenticated remote crash of either endpoint.

Fix direction:

Thread a depth counter through the value decoders (`from_wire_field`, `from_wire_full`, `from_wire_valid`), increment it on every recursive descent (the `Any`/`AnyA` element, `Union`, sub-struct, and array-of-compound branches), and `buf.fault(...)` once it exceeds the bound the descriptor builder already uses (20). In the `Any`/`AnyA` cases, pass the current value-decode depth into the nested `from_wire(buf, *descs, ctxt, depth+1)` so a chain of nested `Any` accumulates depth globally instead of resetting to 0 at each level.

## TLS Branch Pass - secure transport (origin/tls @ 9beba6b, 2026-05-22)

The findings below are on the `tls` branch only (worktree `/Users/stevek/codes/pvxs-tls`, `origin/tls` @ `9beba6b`), **not** on `master` and not in PR epics-base/pvxs#179. TLS (OpenSSL secure transport, peer-certificate access-security) is an in-development branch. Scope was Critical/High security defects in the new transport: handshake/verify bypass, identity confusion, transport downgrade, and secret handling. The core verify path (`SSL_VERIFY_PEER` always set, honest verify callback, RAII context ownership) was reviewed and found sound (see "Not Recorded As Findings").

### PVXS-SR-12 - TLS-capable client silently accepts a plaintext (`tcp`) search reply; no require-TLS mode (downgrade)

Severity: High

Status: Fixed (@6547f25, tls branch) — added an opt-in require-TLS mode exposed as `EPICS_PVA_TLS_OPTIONS=disable_plaintext=true` (new `ConfigCommon::tls_disable_plaintext`, parsed/serialized alongside `client_cert` in `config.cpp`). When set and the client has a TLS context, `procSearchReply` discards any `proto=="tcp"` reply (`if(isTCP && self.effective.tls_disable_plaintext) return;`) and only commits a `"tls"` reply, so a downgrade attempt fails to connect rather than silently building a plaintext channel. Two corrections to the original fix direction, both forced by the wire protocol: (1) the "advertise only `tls`" suggestion is **wrong** — a pvxs server only registers a search's PV names when `"tcp"` is offered (`udp_collector.cpp:443` gates `names.push_back` on `protoTCP`), so a tls-only search is never answered and discovery breaks entirely; the require-TLS client therefore still advertises `tcp`+`tls` and the protection lives solely in *which reply it accepts*. A TLS server still answers with `"tls"` (`server.cpp:850` prefers tls when the client offered it), which the require client accepts; only a plaintext server's `"tcp"` reply is refused. (2) The flag-less "prefer `tls` over `tcp` within a search round" default was implemented (one-round `tcpDeferred` re-solicit) then **dropped by decision**: the server already performs prefer-tls selection so a default client already gets `"tls"` from any TLS server, and a one-round client deferral is a non-binding heuristic (a persistent attacker re-injects `"tcp"` on the second round) that only affected dual-homed-PV races while adding a re-search wart — require mode is the structural protection. Tests: `testconfig` round-trips `disable_plaintext` through `EPICS_PVA_TLS_OPTIONS` (43/43); `testtls` `testRequireTLS` (require client connects to a TLS server over TLS) and `testNoDowngrade` (ordinary client reaches a plaintext-only server, require client refuses its `"tcp"` reply and times out) — fail-before verified for `testNoDowngrade` (without the tcp-reject the require client connects via plaintext); `testtls` 34/34, plus `testget` 67/67 and `testdiscover` 8/8 confirm default-mode search is unchanged.

Evidence:

- `src/client.cpp:1139-1144` - when `tls_context` is set, the client SEARCH request advertises **both** `"tls"` and `"tcp"` (protocol count 2); a non-TLS client advertises only `"tcp"` (`:1146-1149`).
- `src/client.cpp:938-948` - the search-reply handler computes `isTCP = proto=="tcp"` and `isTLS = proto=="tls"`, and the only rejection is `if(!self.tls_context && isTLS) return;` (a TLS reply with no local TLS context). A `tcp` reply is accepted unconditionally via `if(!found || !(isTCP || isTLS)) return;`.
- `src/client.cpp:971-975` - on a reply for a searching channel, `chan->conn = Connection::build(self.shared_from_this(), serv, false, isTLS)` builds the connection using the reply's `proto` verbatim; a `tcp` reply yields `isTLS=false`, i.e. a plaintext connection.
- `src/config.cpp:447` - the only TLS "require"-style knob is `tls_client_cert = ConfigCommon::Require`, which is the *server* requiring a *client* certificate during the TLS handshake. No client-side / transport-level "require TLS" option exists in the config; `rg` finds no `tls_required`-style enforcement.

Impact:

PVA UDP search replies are unauthenticated. A man-in-the-middle, or any host that can inject or race a UDP search reply (spoofing the source address), can answer a TLS-capable client's search with `proto="tcp"` and a server endpoint it controls; the client silently builds a plaintext connection. Because there is no require-TLS mode, an operator cannot configure the client to reject the plaintext answer, so an active attacker can strip TLS (classic downgrade) and read/modify all PVA traffic, and a misconfigured or compromised server can silently serve sensitive PVs in cleartext. Reachable before any TLS handshake, so peer-certificate access-security never engages.

Fix direction:

Add a client-side transport-requirement mode (e.g. an `EPICS_PVA_TLS_REQUIRED`-style flag) that, when set, makes the search-reply handler treat a `proto=="tcp"` reply as not-found and only `Connection::build(..., isTLS=true)`, and advertise only `"tls"` in the search request. Even without an explicit flag, prefer a `tls` reply over a `tcp` reply for the same GUID within a search round so an honest TLS server is not downgraded by a racing plaintext answer.

### PVXS-SR-13 - peer-certificate CN with an embedded NUL truncates the access-security account (identity confusion)

Severity: High

Status: Fixed (@b16b945, tls branch) — both the peer-account and root-CA-authority CN reads in `fill_credentials()` now route through a single `SSLContext::commonName()` helper that uses the length reported by `X509_NAME_get_text_by_NID` and rejects (returns false, leaving `method`/`account` at default) any CN whose length does not round-trip through `strlen()` — i.e. any embedded NUL — and treats `len<=0` as no usable CN (closing the prior `-1`-is-truthy uninitialized-buffer path). The single helper makes the truncated-identity state unrepresentable by construction. New `testtls` `testCommonName`: normal CN round-trips, embedded-NUL CN rejected and output untouched. Fail-before verified by removing the guard (tests 7-8 fail, account becomes the confused `admin\x00.evil`); restored fix passes `testtls` 30/30 including the existing end-to-end `account=="ioc1"/"server1"/"client2"` handshake cases.

Evidence:

- `src/ossl.cpp:385-426` - `fill_credentials` runs only for a peer certificate obtained via `SSL_get0_peer_certificate(ctx)` (`:390`), i.e. after the handshake verified the peer (`SSL_VERIFY_PEER` is always set and the verify callback is honest).
- `src/ossl.cpp:393-398` - the common name is read with `X509_NAME_get_text_by_NID(subj, NID_commonName, name, sizeof(name)-1)` into a fixed `char name[64]`; on success it sets `temp.method = "x509"` and `temp.account = name`. The return value (the CN length) is used only as a truthiness check; `temp.account` is constructed from `name` as a C string.

Impact:

`X509_NAME_get_text_by_NID` copies the CN's raw `ASN1_STRING` bytes, which may contain an embedded NUL, into the buffer. Constructing `std::string temp.account` from the `char*` stops at the first NUL, so a CN such as `admin\0.attacker.example` is silently truncated to `admin`. The bytes after the NUL — part of the certificate's true subject and what a CA's name-constraint checks see — are dropped. A CA that issues (or is tricked into issuing) a certificate whose CN embeds a NUL, or any deployment trusting a CA that does not reject embedded NULs in CN, lets the holder authenticate (method `x509`) as a different, typically higher-privileged, access-security account than the certificate's real subject. This is the classic NUL-prefix identity-confusion class (cf. CVE-2009-2408) applied to the PVA peer-cert account mapping; potential privilege escalation.

Fix direction:

Use the length returned by `X509_NAME_get_text_by_NID` to construct the account as `std::string(name, len)` and reject (fault the credential, leave `method` default) any CN whose returned length does not match `strlen(name)` — i.e. any CN containing an embedded NUL. Equivalently, read the CN's `ASN1_STRING` directly and reject non-printable / embedded-NUL content before mapping it to an account.

### PVXS-SR-14 - inline PKCS12 keychain password is serialized into the effective-config defs and printed in cleartext

Severity: Medium

Status: Fixed (@a162ffb, tls branch) — both the server and client `Config::updateDefs()` now route `tls_keychain_file` through a single `redactKeychain()` helper that emits only the path portion (everything before the first `;`), so the inline PKCS12 password no longer reaches the printable defs (the one channel to `pvxinfo -D`/`-v` effective-config output). The field still carries the inline form for the PKCS12 loader in `ossl.cpp` (its only other consumer, which logs just the path), so the field contract is unchanged and only the display boundary is redacted — a separate password field would be over-engineering for a display-redaction defect. `testconfig` `testDefs` asserts a `path;password` keychain serializes to just the path for both client and server, and a plain path is unchanged. Fail-before verified by disabling the redaction (tests 24/26/27 leak `;s3cret`); restored fix passes `testconfig` 38/38, `testtls` 30/30 unaffected.

Evidence:

- `src/ossl.cpp:230-237` - the keychain config string is parsed as `path;password`: `auto sep(conf.tls_keychain_file.find_first_of(';')); keychain = conf.tls_keychain_file.substr(0, sep); if(sep != std::string::npos) password = conf.tls_keychain_file.substr(sep + 1);` - the substring after `;` is the PKCS12 decryption password.
- `src/config.cpp:479-480` - `EPICS_PVAS_TLS_KEYCHAIN` / `EPICS_PVA_TLS_KEYCHAIN` are read verbatim into `tls_keychain_file`, including any `;password` suffix.
- `src/config.cpp:572` (server) and `:724` (client) - `updateDefs` serializes `defs["EPICS_PVAS_TLS_KEYCHAIN"] = defs["EPICS_PVA_TLS_KEYCHAIN"] = SB() << tls_keychain_file;` with no redaction of the password portion. These defs are emitted by the `Config` ostream operator and printed by `pvxinfo -v` / `-D` (effective-config dump).

Impact:

When an operator uses the inline `path;password` keychain format, the PKCS12 password — which protects the TLS private key — is stored unredacted in the effective-config defs and printed in cleartext by the standard `pvxinfo` diagnostic (and any tool dumping effective config). Such output routinely lands in terminals, support pastes, and CI logs. An attacker who obtains the leaked password together with the (commonly co-located, backed-up, or group-readable) `.p12` file can decrypt the private key and impersonate the endpoint. Credential exposure that weakens private-key protection.

Fix direction:

Redact the password when serializing keychain defs (emit only the path, or `path;<redacted>`), or store the password in a separate field that is never round-tripped through the printable defs map. The secret must not appear in any effective-config dump.

## Third Pass - pvaLink put lifecycle (fix/source-review-2026-05-22 @ 00f669d, 2026-05-22)

The findings below are on the current local pvxs worktree (`/Users/stevek/codes/pvxs`, branch `fix/source-review-2026-05-22` @ `00f669d`). They do not overlap SR-6/SR-7, which cover timestamp metadata caching. Scope was output-link put lifecycle: queued writes, async completion, disconnect handling, and schema-mismatch handling.

### PVXS-SR-15 - pvaLink disconnect cancels an in-flight async put without completing waiting records

Severity: High

Status: Fixed (pvxs fix/source-review-core-2026-05-22 @ b8fcc87) - disconnect now routes through `pvaLinkChannel::putAbort()`, which completes every in-flight/pending waiter with a LINK_ALARM instead of a bare `op_put.reset()`. A `retry:true` link also preserves its most-recent incomplete value and run() resends it on reconnect. Grouped with SR-16/SR-18 in one structural redesign. Regression tests in testpvalink (disconnect-in-flight + retry-resend).

Evidence:

- `ioc/pvalink_lset.cpp:650-653` - `pvaPutValueAsync()` inserts the record into `self->lchan->after_put` before starting the channel put.
- `ioc/pvalink_channel.cpp:186-217` - the only normal completion path is `linkPutDone()`: it checks `after_put`, resets `op_put`, and pushes `self->AP` so `AfterPut::run()` can complete the waiting records.
- `ioc/pvalink_channel.cpp:282-307` - `AfterPut::run()` swaps `after_put` and calls each record's `rset->process()` only if the record is still `PACT`.
- `ioc/pvalink_channel.cpp:360-371` - on monitor disconnect, the channel sets `connected=false`, then does `op_put.reset()` and calls `link->onDisconnect()` for every link. This path does not call `linkPutDone()`, does not push `self->AP`, and does not clear or complete `after_put`.
- `ioc/pvalink_link.cpp:75-81` - `onDisconnect()` clears `used_queue` and `used_scratch`, so queued data for retry is discarded at the same time.
- `src/clientget.cpp:188-203` and `:591-603` - destroying the client `Operation` performs `_cancel(true)`, erases the IOID, sets the operation state to `Done`, and does not invoke the result callback. Therefore `linkPutDone()` is not reached through cancellation.
- `documentation/pvalink.rst:99-100` documents `retry:true` as retrying the most recent incomplete PUT after reconnect, but the disconnect path clears the queued write and cancels the operation without preserving a retry item.

Impact:

If a remote server disconnects while an output pvaLink async put is in flight, the local record has already entered the async wait set, but no completion scan is scheduled. The record can remain `PACT` indefinitely. At the same time, `retry:true` does not preserve the documented "most recent incomplete PUT"; `onDisconnect()` drops both scratch and queued data. This is a control-system liveness and data-loss bug: a network disconnect can leave record processing wedged and the last commanded output value lost.

Fix direction:

Make disconnect/cancel go through one put-finalization path. On disconnect, atomically classify the in-flight put as failed, swap and schedule all `after_put` records for completion, and preserve the most recent queued value when `retry:true` is set. Do not reset `op_put` or clear queue/scratch state through a path that bypasses the after-put finalizer.

### PVXS-SR-16 - pvaLink silently reports output-link put success when the target field is absent

Severity: High

Status: Fixed (pvxs fix/source-review-core-2026-05-22 @ b8fcc87) - an unbuildable link (absent target field, empty/unsupported source) is recorded in `put_inflight_failed` and its record completes with recGblSetSevrMsg(LINK_ALARM, INVALID_ALARM) even when the network op succeeds, instead of being silently `continue`d. Grouped with SR-15/SR-18. Regression test in testpvalink (absent-field).

Evidence:

- `ioc/pvalink_link.cpp:69-72` - `pvaLink::valid()` checks only that the channel is connected and has a root value; it does not require the configured `fieldName` to resolve to `fld_value`.
- `ioc/pvalink_link.cpp:91-98` - `onTypeChange()` logs a warning when `fieldName` is absent, but leaves the link otherwise valid.
- `ioc/pvalink_lset.cpp:609-612` - output puts are rejected only when `!retry && !self->valid()`. A connected target with a missing field passes this guard.
- `ioc/pvalink_lset.cpp:648-653` - the put value is marked in `used_scratch`, async records are inserted into `after_put`, and `lchan->put()` is started.
- `ioc/pvalink_channel.cpp:133-147` - `linkBuildPut()` sees `used_queue`, clears `used_queue=false`, resolves `top[link->fieldName]`, then `continue`s when the field is absent. The queued write is neither applied nor reported as an error.
- `ioc/pvalink_channel.cpp:186-217` - if the network put itself succeeds, `linkPutDone()` treats the operation as OK and completes any `after_put` records; it has no record that a requested field was skipped.

Impact:

A configured output link can accept a record write, launch a network PUT, and complete an async record as successful while the requested remote field was not written at all. This can happen after a remote type/schema change, a typo in `fieldName`, or an NT structure losing a member. Because the queue flag is cleared before the absent-field check, the value is discarded and no retry remains. Operators see local put completion while the remote process variable remains unchanged.

Fix direction:

Make `linkBuildPut()` return a structured per-link result instead of silently skipping unresolved fields. An absent target field or impossible scalar assignment must fail the put operation, leave enough state for `linkPutDone()` to complete async records with an error/alarm, and avoid clearing queued data as successful. Consider making output-link validity include `fld_value` resolution so invalid schema is rejected before queueing.

## Fourth Pass - server GPR invalid-command state transition (fix/source-review-2026-05-22 @ 00f669d, 2026-05-22)

### PVXS-SR-17 - malformed RPC EXEC leaves a server operation stuck in `Executing`

Severity: Medium

Status: Fixed (pvxs fix/source-review-core-2026-05-22 @ 36925b3) - the EXEC dispatch in `handle_GPR()` is now total. The fall-through `else` (the only branch that could be reached after `op->state = Executing` was set without producing a terminal action) now calls `ctrl->error(...)` like its sibling not-implemented branches instead of only logging. `error()` routes through `doReply()`, which sends the error reply and transitions the state from `Executing` back to `Idle` (`src/serverget.cpp:89-90`), so no command/subcommand combination can leave an operation wedged. Verified fail-before/pass-after with a new wire-level regression test in `test/testclientconn.cpp` (`testRpcExecBadSubcmd`): a hand-rolled client peer drives a real server through the handshake to an RPC INIT, then sends an EXEC with the GET bit set; the buggy server sent nothing (the test's bounded `recv` times out -> `not ok`), the fixed server replies with an error for that ioid (`ok`). Non-regression: `testget` 67/67, `testput` 52/52, `testrpc` 23/23, `testmon` 43/43, `testclientconn` 2/2.

Evidence:

- `src/serverget.cpp:359-365` - the GPR EXEC handler derives `isput` from the peer-supplied subcommand bit. For `CMD_RPC`, setting bit `0x40` makes `isput=false`.
- `src/serverget.cpp:443-447` - `CMD_RPC` still decodes a full RPC value before validating that the subcommand combination is meaningful.
- `src/serverget.cpp:467-476` - once the operation is `Idle`, the handler creates `ServerGPRExec`, stores the peer subcommand, and sets `op->state = ServerOp::Executing` before dispatching on the command/subcommand combination.
- `src/serverget.cpp:481-503` - the valid dispatch cases are `(CMD_RPC && isput)`, `(CMD_PUT && isput)`, and `(cmd != CMD_RPC && !isput)`. The malformed `CMD_RPC && !isput` case only logs "Get exec in incorrect command" and does not call `ctrl->error()`, `op->cleanup()`, or reset the operation to `Idle`.
- `src/serverget.cpp:511-514` - later EXEC messages for the same IOID only log "incorrect state" while the operation remains `Executing`.
- `src/serverconn.cpp:297-321` can remove the operation on a later `DESTROY_REQUEST`, and `src/serverconn.cpp:487-510` can clean it up when the connection/channel is torn down, but there is no local cleanup on the malformed EXEC itself.

Impact:

A peer that has created an RPC operation can send an EXEC with the GET bit set (`subcmd & 0x40`) and no destroy. The server leaves that operation in `Executing`, sends no reply, and keeps the IOID in the channel/connection operation maps until the peer later destroys it or the connection is closed. This is a remote-triggered per-connection resource/liveness leak. It is not in the same class as SR-8/SR-11 crash/OOM defects, but it is a broken protocol-state transition that lets malformed input bypass the normal reply/cleanup path.

Fix direction:

Validate the command/subcommand combination before setting `Executing`. For malformed combinations, either send an error and return the operation to `Idle`, or disconnect/fault the peer. The invalid branch must not just log after the operation has already entered `Executing`.

## Fifth Pass - broader Medium+ lifecycle/backpressure sweep (fix/source-review-2026-05-22 @ 00f669d, 2026-05-22)

This pass widened the threshold from Critical/High network crashes to Medium+ lifecycle, backpressure, and data-integrity defects. It covers pvaLink operation generation accounting, monitor queue/window negotiation, SharedPV callback exception paths, and IOC SingleSource blocking put concurrency.

### PVXS-SR-18 - pvaLink overwrites an in-flight put and can complete async waiters with the wrong operation result

Severity: High

Status: Fixed (pvxs fix/source-review-core-2026-05-22 @ b8fcc87) - waiters are split into `after_put` (next generation) and `put_inflight` (current op, captured when put() starts); `put()` refuses to start a second op while one is in flight (overlap impossible by construction), staging the new value in scratch and remembering a forced put in `put_force_pending`. `completeInflight()` drains only the current generation. Grouped with SR-15/SR-16. Regression test in testpvalink (two overlapping puts on one channel).

Evidence:

- `ioc/pvalink_lset.cpp:650-653` - every async output put inserts only the record pointer into the channel-wide `after_put` set, then calls `lchan->put()` unless `defer` is set.
- `ioc/pvalink_channel.cpp:220-249` - `pvaLinkChannel::put()` moves each link's latest scratch value into `put_queue` and sets `used_queue=true`; it does not check whether `op_put` is already in flight.
- `ioc/pvalink_channel.cpp:265-278` - when `doit` is true, the channel unconditionally assigns a new operation into `op_put`; the in-code comment states this "cancels in-progress put".
- `src/clientget.cpp:188-203` and `:591-603` - replacing the last external `Operation` handle performs implicit `_cancel(true)`, erases the IOID, sets the operation state to `Done`, and does not call the result callback.
- `ioc/pvalink_channel.cpp:186-217` - `linkPutDone()` is the only path that drains `after_put`, and it drains the whole channel-wide set based on whichever `op_put` happens to complete.

Impact:

Two output puts to the same pvaLink channel can overlap. The second put cancels the first network operation without running the first result callback. Any record waiting in `after_put` for the first operation remains in the channel-wide wait set and is later completed by the second operation's result, or remains wedged if the replacement operation is also canceled. That can report success for a value that was never delivered, report the wrong error to the wrong record, or leave async record processing stuck.

Fix direction:

Tie `after_put` waiters and queued values to a specific put generation. A new write while `op_put` is active should either queue behind the active operation or explicitly finalize the canceled generation as failed before replacing it. `linkPutDone()` should complete only the waiters associated with the operation whose result arrived.

### PVXS-SR-19 - server monitor accepts unbounded peer-supplied `queueSize`

Severity: High

Status: UNFIXED (deferred, intentional per SR-9 precedent, 2026-05-22) - peer-negotiated `queueSize` is treated as intentional pvxs behavior, the same decision taken for SR-9's inbound message-size cap: no server-side default ceiling is added. A deployment that needs to bound this should opt in. Re-open only if a default cap is later decided for the message-size family too.

Evidence:

- `src/servermon.cpp:533-536` - the server reads `record._options.queueSize` from the client's pvRequest and assigns any `uint32_t >= 2` directly to `op->limit`; there is no server-side maximum.
- `src/servermon.cpp:273-287` - `MonitorControlOp::post()` appends to the per-monitor queue until `queue.size() < limit`; squashing starts only after that limit is reached.
- `include/pvxs/source.h:86-104` - the public monitor API exposes this queue limit through the normal `post()` / `tryPost()` / `forcePost()` backpressure contract.
- `src/servermon.cpp:311-314` reports the negotiated `limitQueue` and current queue depth, but no cap is applied before the negotiated limit is used.
- `src/servermon.cpp:120-129` can defer replies to the connection backlog when the TCP output buffer is full; it does not reduce the monitor's accepted queue limit.

Impact:

A remote client can create a monitor with a very large `queueSize` and then stop consuming updates. On an active PV, the server will retain updates until that client-selected limit rather than squashing at the intended small queue depth. This turns monitor queueing into a remote-controlled memory budget. The default queue size is 4, but a malicious client can negotiate a limit orders of magnitude larger.

Fix direction:

Add a server-side maximum monitor queue size independent of the client's requested `queueSize`, reject or clamp values above it, and expose the clamped value in `MonitorStat::limitQueue`. Apply the same cap before computing percentage `ackAny` thresholds.

### PVXS-SR-20 - client pipeline monitor ACK debt survives reconnect and is sent to the next server generation

Severity: Medium

Status: Fixed (@992a952) - `SubscriptionImpl::disconnected()` is the sole generation-end transition (active -> Connecting + requeue to `chan->pending`). Under the subscription lock it now resets `window = 0; unack = 0; ackPending = false;` and `event_del(ackTick)`, scoping pipeline flow control to one server monitor generation. The reset runs under the lock because `_pop()` schedules `ackTick` from user threads, so a concurrent pop must not leave `ackPending` set with no timer (ACK stall). `createOp()` still re-inits `window = queueSize` for the new generation; the initial-connect requeue (`:851`) starts from zero accounting, so neither needs the reset. Anchor audit: other `window`/`unack`/`ackPending` writers (`_pop` accrual, `tickAck` ACK owner, `handle_MONITOR` per-update `window--`, `_cancel` terminal `event_del`) are distinct, not generation-end. Regression test `testPipelineAckReconnect` (test/testclientconn.cpp): real client + hand-rolled two-generation server; bug emits an ACK count=4 on the wire after a single gen-2 update, fix emits none (fail-before verified by disabling the reset).

Evidence:

- `src/clientmon.cpp:64-65` stores pipeline `window` and `unack` on the long-lived `SubscriptionImpl`, not on a specific connection/monitor generation.
- `src/clientmon.cpp:156-181` increments `unack` every time a queued update is popped and schedules an ACK only when `unack >= ackAt`.
- `src/clientmon.cpp:373-403` handles disconnect by pushing the operation back to `chan->pending` and setting `state = Connecting`, but it does not reset `unack`, `window`, or `ackPending`, and it does not delete any pending ACK timer.
- `src/clientmon.cpp:319-367` reconnects by sending a fresh MONITOR INIT and resetting only `window = queueSize` for pipeline monitors.
- `src/clientmon.cpp:412-445` later sends the accumulated `unack` count as a monitor ACK whenever the state is `Idle` or `Running`, even if those popped updates belonged to the previous server generation.
- `src/clientmon.cpp:662-673` logs when a server exceeds the client window, but pipeline mode does not squash queued values at `queueSize` (`:679` squashes only when `!mon->pipeline`).

Impact:

If a pipeline monitor disconnects after the client has popped updates but before the ACK is sent, the ACK debt is carried into the next connection/monitor generation. The client can then grant the new server credit for updates that came from the old server. That breaks the negotiated flow-control invariant and can let an honest server overrun the client-side queue after reconnect; a malicious server can amplify the effect by timing disconnects around the ACK threshold.

Fix direction:

Reset pipeline accounting (`unack`, `window`, `ackPending`) and cancel the ACK timer on disconnect before the operation is requeued. ACK credit should be scoped to one server monitor generation, not to the long-lived `SubscriptionImpl`.

### PVXS-SR-21 - SharedPV callback exceptions can leave PUT/RPC operations without reply or cleanup

Severity: Medium

Status: Fixed (pvxs fix/source-review-core-2026-05-22 @ 5c9df11) - both SharedPV catch blocks (`src/sharedpv.cpp` RPC ~176, PUT ~223) now finalize the op when it is still locally owned: `if(op) op->error("error in {RPC,PUT} callback: " + e.what())`. `op->error()` routes through `doReply()`, which sends the error reply and returns the state from `Executing` to `Idle`; it is a no-op if the callback already replied (doReply's `Idle` guard, `src/serverget.cpp:47`), and the message is prefixed to stay non-empty as `ExecOp::error()` requires. A callback that moves the op away before throwing remains responsible for completing it (the unrecoverable case the finding notes). Verified fail-before/pass-after with two new SharedPV regression tests: `testrpc` `throwingCb()` (throwing onRPC) and `testput` `putThrows()` (throwing onPut) - the buggy server left the op in `Executing` and the client's bounded wait timed out (`not ok`), the fixed server replies `RemoteError` promptly (`ok`). Note `testput`'s pre-existing `testError(OnPut)` uses a raw `server::Source` (`ErrorSource`) that calls `op->error()` directly, so it did NOT cover the SharedPV catch path. Non-regression: `testrpc` 25/25, `testput` 54/54, `testget` 67/67, `testmon` 43/43.

Evidence:

- `include/pvxs/sharedpv.h:58-62` exposes user callbacks for SharedPV PUT and RPC operations, both receiving a `std::unique_ptr<ExecOp>`.
- `src/sharedpv.cpp:162-180` invokes the user RPC callback inside a `try` block, but the `catch` only logs `"error in RPC cb"` and does not call `op->error(...)` if the operation is still owned.
- `src/sharedpv.cpp:209-227` repeats the same pattern for the PUT callback: the `catch` only logs `"error in Put cb"`.
- `src/serverget.cpp:504-508` has the generic server GPR safeguard for handler exceptions: if a `ctrl` still exists after an exception, it sends `ctrl->error(e.what())`. SharedPV catches inside its handler, so this outer safeguard never sees the exception.
- `src/serverget.cpp:266-310` shows `ServerGPRExec::~ServerGPRExec()` is empty; dropping an unreplied `ExecOp` after an exception does not send an implicit error.

Impact:

A server application callback that throws before replying can leave the client PUT/RPC waiting for a response until timeout, while the server operation remains in `Executing` until the client destroys it or the connection closes. This is a server-side liveness leak and poor failure isolation: SharedPV logs the callback error but does not convert it into the protocol error reply that the surrounding GPR machinery would otherwise send.

Fix direction:

In the SharedPV PUT/RPC catch blocks, if the local `op` still owns the operation, send `op->error(e.what())`. If the callback moved ownership away, document that the callback is responsible for completing it; otherwise keep ownership with a small RAII guard that errors on exception unless explicitly released.

### PVXS-SR-22 - SingleSource reuses one `processNotify` for overlapping blocking PUTs

Severity: High

Status: NOT A BUG (premise does not hold for this pvxs version, verified 2026-05-22). The finding assumed `PutOperationCache` (with its single `notify`/`valueToSet`/`putOperation`) is created once **per connected channel** and reused for all PUT operations on that channel. It is not: `ioc/singlesource.cpp` registers `onOp` via `ChannelControl::onOp`, and `src/serverget.cpp:388-408` invokes `chan->onOp(ctrl)` inside the **GET/PUT INIT** handler — i.e. once **per operation (per ioid)**. Each PUT operation therefore gets its own `PutOperationCache`/`notify`. Verified empirically: tracing `onOp`/`onPut` on a held async record (`test:slowmo` ODLY=30) shows three puts on one channel produce three separate caches (one address is malloc-reused only after the prior put's cache is freed; an overlapping put gets a distinct address). Two channels never share the slot, so the "second request overwrites the cache fields the first `doneCallback` will use" path cannot occur. Additionally `src/serverget.cpp:467,511` drops a second EXEC on the same ioid while the op is `Executing`, so one operation's `onPut` is never re-entered concurrently. Two concurrent blocking PUTs land on two ioids → two `notify` structs → EPICS Base serializes them via the dbNotify restart list (no assert; the second simply queues and completes after the first). A `notifyBusy` guard was prototyped and then reverted because it would defend against a state the server already makes unreachable. The `done`-flag security-cache reuse the original code comments describe is itself effectively per-operation now (separate, pre-existing, not a security defect).

Evidence:

- `ioc/securityclient.h:65-69` stores one `processNotify`, one `valueToSet`, and one `putOperation` in `PutOperationCache`.
- `ioc/singlesource.cpp:322-337` creates a single `PutOperationCache` per connected channel and reuses its `processNotify` for all PUT operations on that channel.
- `ioc/singlesource.cpp:355-363` handles `record._options.block=true` by overwriting `valueToSet`, assigning `putOperation = std::move(putOperation)`, and calling `dbProcessNotify(&putOperationCache->notify)` with no busy guard; the local TODO at `:357` explicitly notes the missing concurrent-put guard.
- `ioc/singlesource.cpp:242-268` later completes whichever `putOperation` is currently stored in the cache, not necessarily the operation that initiated the `processNotify` callback.
- EPICS Base documents the `processNotify` contract in `dbNotify.h:121-123`: a client can issue a new `dbProcessNotify` request from `doneCallback` or after it returns. Reusing the same `processNotify` before that point violates the API contract.
- `dbNotify.c:362-374` handles reuse of an already-active `processNotify` by asserting the old state is `notifyUserCallbackActive`, waiting for the previous user callback, and cleaning it up before reinitializing. Calling it while a previous asynchronous process is still active is not a supported concurrent path.

Impact:

A remote client can issue two blocking PUTs to the same SingleSource PV before the first asynchronous record processing completes. The second request overwrites the cache fields that the first `doneCallback` will use. Outcomes include replying to the wrong operation, leaving the first operation stuck, dereferencing a null `putOperation` on the later callback, or tripping EPICS Base's active-notify assertions. This can become a remote-triggered IOC liveness failure or crash for records that process asynchronously.

Fix direction:

Track one outstanding blocking put per channel or per IOID. Reject/queue concurrent blocking PUTs while `processNotify` is active, and do not reuse `valueToSet`, `putOperation`, or the `processNotify` storage until its `doneCallback` has completed. If a second request is rejected, reply with a protocol error instead of overwriting the active state.

## Sixth Pass - wider discovery/config/server-monitor sweep (fix/source-review-2026-05-22 @ 00f669d, 2026-05-22)

The findings below widen the review beyond the previous pvaLink/GPR/monitor-queue pass into client discovery, search filtering, and server monitor producer concurrency. They are review-only and remain Open.

### PVXS-SR-23 - client discovery dispatch races `discoverers` across UDP and TCP workers

Severity: High

Status: Fixed (pvxs fix/source-review-core-2026-05-22 @ c2fb3dd) - the `tcp_loop` is now the sole owner of `discoverers` iteration and discovery-callback dispatch. `serverEvent()` copies the `Discovered` payload and dispatches the iteration plus user callbacks to `tcp_loop` (so all three off-loop callers - real beacons via `onBeacon`, the beacon-clean timer, and discovery pongs - become safe transitively), and `procSearchReply()`'s discovery-pong branch tests `discoverers` and calls `onBeacon()` inside a `tcp_loop` dispatch instead of reading the map on the UDP worker. User callbacks therefore run only on `tcp_loop` and never under `pokeLock`. Verified: `testdiscover` 8/8 (Online + Timeout events still delivered through the deferred dispatch); search-path non-regression `testget` 67/67, `testclientconn` 1/1, `testnamesrv` 5/5. No deterministic data-race regression test is feasible (pvxs build has no TSan/sanitizer target and macOS arm64 lacks Helgrind), so a fail-before stress test would pass on both old and new code; verification is the single-owner invariant (every `discoverers` access enumerated and routed through `tcp_loop`) plus functional non-regression.

Evidence:

- `src/clientimpl.h:328` stores active discovery operations in `std::map<Discovery*, std::weak_ptr<Discovery>> discoverers` with no lock.
- `src/clientdiscover.cpp:83-91` inserts a new `Discovery` into `discoverers` on `context->tcp_loop`.
- `src/clientdiscover.cpp:40-46` and `src/clientdiscover.cpp:70-78` erase from `discoverers` on the same TCP loop during explicit or implicit cancel.
- `src/client.cpp:613-619` registers the beacon callback with `UDPManager::onBeacon()`. `src/udp_collector.cpp:482-486` invokes that callback on the UDP manager worker.
- `src/client.cpp:750-815` handles real beacons on the UDP worker and calls `serverEvent(...)` while still holding `pokeLock`; `src/client.cpp:1254-1281` does the same from the beacon cleaner timer.
- `src/clientdiscover.cpp:103-113` iterates `discoverers` and invokes user callbacks without any `discoverers` lock.
- `src/client.cpp:866-876` also routes UDP discovery search replies through `onBeacon()` from the TCP loop, so the same `serverEvent()` path can run from both loop threads.

Impact:

Remote beacons and discovery replies can make the UDP worker iterate `discoverers` while the TCP worker inserts or erases entries for `DiscoverBuilder::exec()` or `Discovery::cancel()`. That is an unsynchronized C++ container access and can corrupt the map or crash the client process. The same path invokes user discovery callbacks while `pokeLock` is held; a callback that re-enters APIs such as `Context::hurryUp()` can call back into `poke()` on the manager loop and deadlock on the same mutex.

Fix direction:

Make one loop own `discoverers` and discovery callback dispatch. A low-risk shape is to have UDP beacon/search events copy the `Discovered` payload and dispatch it to `tcp_loop`, then iterate `discoverers` and call user callbacks only there. Do not hold `pokeLock` while invoking user callbacks; update `beaconTrack` under the lock, release it, then dispatch notifications.

### PVXS-SR-24 - `ignoreServerGUIDs()` mutates the search-reply ignore list on the wrong loop

Severity: Medium

Status: Fixed (@dbc5edb) - `Context::ignoreServerGUIDs()` now assigns the vector on `tcp_loop` instead of `manager.loop()`. The sole reader, `procSearchReply()`, runs on `tcp_loop` for both UDP replies (`searchRx4`/`searchRx6` events bind to `tcp_loop.base`, client.cpp:530-532) and TCP replies (clientconn.cpp:993), so `tcp_loop` becomes the single owner of both the write and every read; no mutex needed. Anchor audit: the only `ignoreServerGUIDs` sites are the member (clientimpl.h:268), the writer (client.cpp:459, now tcp_loop), and the reader (client.cpp:857). No deterministic regression test - cross-thread data race with no behavioural divergence absent a thread sanitizer (SR-23 precedent); verified by the loop-ownership invariant and client search/connect non-regression (testget, testmon, testnamesrv). Note: a follow-up (@9fb59a3) corrected two C++14 initialized lambda captures introduced by the SR-23 fix (client.cpp:881, clientdiscover.cpp:111) that warned under `-std=c++11`.

Evidence:

- `src/clientimpl.h:268` stores the ignore list as `std::vector<ServerGUID> ignoreServerGUIDs` with no mutex.
- `src/client.cpp:453-460` implements `Context::ignoreServerGUIDs()` by running the assignment on `pvt->impl->manager.loop()`.
- `src/client.cpp:827-923` decodes search replies in `procSearchReply()`. Both `src/client.cpp:925-981` (UDP search replies) and `src/clientconn.cpp:984-989` (TCP search replies) call it from the client TCP loop.
- `src/client.cpp:856-863` iterates `ignoreServerGUIDs` during search-reply handling with no lock.

Impact:

A client can update the ignored GUID list while search replies are arriving. The assignment on the UDP manager loop can reallocate the vector while the TCP loop is iterating it, producing a data race and possible process crash. It can also apply the new ignore policy late relative to already queued TCP-loop search replies, because the data is owned by neither the reader loop nor a mutex.

Fix direction:

Move `ignoreServerGUIDs` ownership to `tcp_loop`, since all search-reply consumers are on that loop, or guard it with a dedicated mutex and copy before iteration. The API call itself can still be synchronous; the important invariant is that writes and reads use the same owner.

### PVXS-SR-25 - IPv6 `ignoreAddrs` matching reads IPv4 fields from IPv6 addresses

Severity: Medium

Status: Fixed (@4ad9a9f) - `Server::Pvt::onSearch()` now calls a new `matchesAddrList()` helper (declared next to `SockAddr` in osiSockExt.h, defined in util.cpp) that compares family-aware via `evutil_sockaddr_cmp` (the comparator behind `SockAddr::operator==`/`compare()`). A list entry with port 0 ignores the port; otherwise ports must also match - preserving the wildcard-port semantics while comparing `sin6_addr` for AF_INET6 instead of the aliased `sin_addr.s_addr`/`sin6_flowinfo`. Anchor audit (`sin_addr.s_addr`/`->in.sin_port` reads): the cited `onSearch` matching was the only address-equality site; the GUID-seed XOR at server.cpp:533/536 hashes IPv4 bytes (not a match, not security-critical) and the rest are IPv4-only domains (IPv4 multicast joins, WIN32 broadcast calc, IPv4-mapped wire encoding) - all distinct. Regression test `test_matchaddrlist` in testsock: IPv4 exact/wildcard-port matches plus the fail-before IPv6 cases (::1 vs ::2, ::1 vs 2001:db8::1 not matched); the old field comparison fails those three (aliased flowinfo equal). Verified pass-after + non-regression (testsock 118, testnamesrv 5, testget 67, testmon 43).

Evidence:

- `src/pvxs/server.h:158-162` documents `Config::ignoreAddrs` as IP addresses with optional ports.
- `src/config.cpp:422-423` parses `EPICS_PVAS_IGNORE_ADDR_LIST` into `Config::ignoreAddrs`.
- `src/server.cpp:482-485` converts those strings into `ignoreList`.
- `src/server.cpp:669-685` applies the list in `Server::Pvt::onSearch()`.
- `src/server.cpp:673-681` checks only that the address families match, then compares `msg.origSrc->in.sin_addr.s_addr` and `addr->in.sin_addr.s_addr`, and checks `addr->in.sin_port`, even when the matched family is `AF_INET6`.

Impact:

For IPv6 entries, the code reads the `sockaddr_in` view of a `sockaddr_in6`. The compared `sin_addr.s_addr` bytes are not the IPv6 address; they overlap the IPv6 flowinfo field. An IPv6 ignore entry with default port zero can therefore match far more than the configured address, commonly all IPv6 searches with zero flowinfo, causing the server to ignore legitimate IPv6 clients. With a nonzero port, the match is still based on the wrong address bytes.

Fix direction:

Use `SockAddr::compare()` or explicit family-specific comparisons. For `AF_INET6`, compare `sin6_addr` and then apply the optional port wildcard using `SockAddr::port()` rather than reading through the IPv4 union member.

### PVXS-SR-26 - server monitor first-post flag is outside the monitor lock

Severity: Medium

Status: Fixed (@d743585) — moved the `mon->first` read+clear inside the existing `Guard G(mon->lock)` scope in `doPost()`, ahead of the `finished` check, so the flag is committed exactly once while locked. The `type` guard and `testmask(val, pvMask)` decision stay correct off-lock (both const after setup) and are now simply covered by the same lock with no behavior change. testmon 43/43 and testmonpipe 111/111 still pass; no deterministic fail-before/pass-after race test is feasible without TSan (same as SR-23/SR-24).

Evidence:

- `src/servermon.cpp:56-72` says the monitor queue/accounting fields after `lock` are guarded by that mutex. `first` is one of those fields.
- `src/servermon.cpp:251-297` implements `ServerMonitorControl::doPost()`, the method behind `MonitorControlOp::post()`, `tryPost()`, `forcePost()`, and `finish()`.
- `src/servermon.cpp:260-265` reads and writes `mon->first` before acquiring `Guard G(mon->lock)` at `src/servermon.cpp:267`.
- The queue mutation, `finished` check, and `maybeReply()` scheduling are protected by the lock at `src/servermon.cpp:267-294`, so `first` is the unprotected outlier in the same state transition.

Impact:

`MonitorControlOp` handles are exposed to server-side application code and can be posted from arbitrary producer threads. Concurrent first posts race on `mon->first`: two threads can both observe the initial value, both force a "first update" through the mask check, and the unsynchronized bool access is C++ data-race UB. This undermines the monitor queue's intended thread-safe producer boundary.

Fix direction:

Move the `first` read/write inside `mon->lock`, next to the queue insertion and `finished` check. Keep the mask decision under the same lock or split it into a two-step helper that copies the needed immutable mask/type state but commits `first` exactly once while locked.

### PVXS-SR-27 - server invokes `Source` callbacks while holding `sourcesLock`

Severity: Medium

Status: Fixed (@21778f9, core branch) — added `Server::Pvt::sourceSnapshot()` (copies the priority-ordered source map under the read lock, returns it with the lock released) and routed all five callback-invoking iterations through it: UDP `onSearch` (`server.cpp`), TCP `onSearch` and `onCreate` (`serverchan.cpp`, one snapshot per CREATE_CHANNEL message for a consistent source set across names), the "channels" RPC `onList` (`serversource.cpp`), and `show()` (`server.cpp`). Lock-held sites that only read/mutate the map without a user callback (add/remove/get/listSource, builtin registration) left as-is — they cannot re-enter. Regression: `testget` `testSourceReentry` (onSearch re-enters add/removeSource) times out via deadlock before the fix, passes after.

Evidence:

- `src/server.cpp:80-96` and `src/server.cpp:100-115` mutate the server source map under `sourcesLock.lockWriter()` for `Server::addSource()` and `Server::removeSource()`.
- `src/server.cpp:669-719` handles UDP search requests on the UDP manager worker, takes `sourcesLock.lockReader()` at `src/server.cpp:709-710`, and calls each `Source::onSearch()` at `src/server.cpp:711-713` before releasing the lock.
- `src/serverchan.cpp:173-222` repeats the same pattern for TCP SEARCH: it takes `iface->server->sourcesLock.lockReader()` at `src/serverchan.cpp:212-213` and invokes `Source::onSearch()` at `src/serverchan.cpp:214-216` under the lock.
- `src/serverchan.cpp:258-326` handles CREATE_CHANNEL with the same read lock held from `src/serverchan.cpp:264` through the per-source `Source::onCreate()` calls at `src/serverchan.cpp:298-300`.
- `src/serversource.cpp:55-69` also holds `serv->sourcesLock.lockReader()` while invoking `Source::onList()` for the builtin `server` RPC.
- `src/utilpvt.h:181-208` wraps `pthread_rwlock_t` or Windows `SRWLOCK`; there is no recursive-read-to-write upgrade path or reentrancy guard.
- `src/pvxs/server.h:106-129` exposes `addSource()`, `removeSource()`, `getSource()`, and `listSource()` as normal public `Server` operations and does not document that `Source` callbacks must not re-enter them.

Impact:

`Source` is an extension point. A source that reacts to `onSearch()`, `onCreate()`, or `onList()` by adding/removing/listing sources can attempt to acquire the same RW lock while the callback path still holds a read lock. A remote SEARCH, CREATE_CHANNEL, or builtin `server` RPC can then wedge the UDP worker or acceptor loop on a self-deadlock. Even without reentry, arbitrary source code running under `sourcesLock` blocks concurrent source-list mutation for the callback duration.

Fix direction:

Snapshot the ordered `std::shared_ptr<Source>` list under `sourcesLock`, release the lock, then invoke `onSearch()`, `onCreate()`, and `onList()` callbacks from the snapshot. For `onCreate()`, preserve the existing source order and stop after the first accepted/rejected/discarded result; the invariant is that user-provided `Source` code must not run while the server source-map lock is held.

### PVXS-SR-28 - failed async `ConnectBuilder::exec()` setup can crash during handle cleanup

Severity: Medium

Status: Fixed (@040806b, core branch) — wrapped `Channel::build()` in the `ConnectBuilder::exec()` setup lambda with `try/catch` (matching GET/PUT/RPC, INFO, MONITOR): on failure it reports disconnected via `_onDis` (leaving `_connected` false) without registering a connector, and logs the reason (the callback carries no error detail). The cleanup deleter now guards `if(op->chan)` instead of `assert(op->chan)`, so a handle whose setup never built a channel destroys cleanly. Anchor audit (`op->chan = Channel::build` setup sites): the other three builders already had both the `try/catch` and the `if(op->chan)` deleter guard; ConnectBuilder was the only one missing both, so the family is now closed. Regression: `testget` `testConnectSetupError` (malformed `.server()` address → `setAddress()` throws in setup) aborts via the cleanup deleter before the fix, passes after.

Evidence:

- `src/client.cpp:242-285` creates a `ConnectImpl` handle and returns an external `shared_ptr` whose custom deleter queues cleanup on the client TCP loop.
- The cleanup lambda at `src/client.cpp:260-266` asserts `op->chan` and then immediately dereferences it with `op->chan->connectors.remove(op.get())`.
- The setup lambda at `src/client.cpp:270-282` assigns `op->chan = Channel::build(...)` and then pushes the connector into `op->chan->connectors`, but unlike GET/PUT/INFO/MONITOR setup (`src/clientget.cpp:606-617`, `src/clientintrospect.cpp:220-238`, `src/clientmon.cpp:822-836`) it has no `try/catch`.
- `src/client.cpp:346-358` shows `Channel::build()` can throw before returning a channel: it throws when the context is no longer running (`src/client.cpp:350-351`) and can also throw while parsing the optional forced server address (`src/client.cpp:356-357`, `src/util.cpp:444-549`).
- `src/evhelper.cpp:226-238` catches exceptions from dispatched work items without a synchronous result target, logs them, and continues processing later work. That leaves `ConnectImpl::chan` unset if the setup lambda throws.

Impact:

If the context closes during the async setup window, or a caller supplies an invalid `.server(...)` address, the setup work can fail before `op->chan` is assigned. The returned `Connect` handle still exists. When it is later destroyed, the deleter queues the cleanup lambda, which hits the debug `assert(op->chan)` or dereferences null in release builds. Other operation builders convert the same setup failure class into an operation result or queued exception; `ConnectBuilder` turns it into a client process abort/crash.

Fix direction:

Make `ConnectBuilder::exec()` setup match the other operation builders: catch exceptions from `Channel::build()`, record a failed/disconnected state, and invoke `_onDis` or otherwise notify the user without registering a connector. The cleanup lambda must tolerate `!op->chan` and should remove from `connectors` only after setup has successfully inserted the connector.

## Reviewed Anchors

- `SecurityLogger`, `asTrapWriteWithData`, `pfieldsave`, `pchan`
- `GroupSource::putGroup`, `securityClients`, `fieldIndex`, `MappingInfo::Structure`, `MappingInfo::Const`
- `GroupSource::get`, `getGroupField`, `MappingInfo::Const`
- `ServerMonitorControl::stats`, `MonitorStat::nQueue`, `MonitorStat::nSquash`
- `record._options.ackAny`, client/server monitor pipeline parsing
- `IOCSource::put`, `putLongString`, `doDbPut`
- `pvaLinkChannel` monitor/put callbacks, `shared_from_this()`, operation cancellation paths
- `pvaLink::onTypeChange`, `fld_seconds`, `fld_usertag`, `snap_tag`
- `UDPCollector::process_one`, search message length/name parsing
- `from_wire(Buffer&, Size&)`, `shared_array(size_t)`, `dataencode.cpp` Struct/StructA/UnionA/AnyA value decode (SR-8)
- `ConnBase::bevRead`, `segBuf`, message `len` / segmentation watermark (SR-9)
- `clientmon.cpp` `handle_MONITOR`, `RequestInfo::fl`, subcmd/state validation order (SR-10)
- `from_wire_field`/`from_wire_full`/`from_wire_type_value`, nested `Any`/`AnyA` value-decode recursion vs the descriptor `depth>20` guard (SR-11)
- UDP beacon / `procSearchReply` / ORIGIN_TAG / `recvfromx` decode; `evhelper` loop+timer lifetime; `serverconn`/`serverchan` SID/CID/IOID handlers; `sharedpv` re-typing/locking; IOC scalar/array conversion (`copyIn`, void-array reinterpret)
- Value conversion engine (`copyIn`/`copyAs`/`convertArr`/`castTo`/`as<T>`, FieldStorage union access, aliasing `shared_ptr` lifetime); type registry / FieldDesc index+offset walk (union selector, `from_wire_valid`, `0xfd` cache fetch)
- `clientreq.cpp` PVRParser; `clientintrospect` GET_FIELD; `clientdiscover`; `describe`
- address/socket/config parsing (`util.cpp` `SockAddr`/`SockEndpoint`/`parseToAddr`, `config.cpp`, `os/default/osdSockExt.cpp`, `osgroups.cpp`)
- server SEARCH lookup + reply assembly (`server.cpp`, `serverchan.cpp`), beacon sender / GUID, `BitMask` decode bound vs descriptor
- (tls branch) `ossl.cpp` `SSL_CTX` setup / verify callback / `SSL_VERIFY_PEER`, `fill_credentials` peer-cert CN→account, PKCS12 keychain parse; `client.cpp` search-reply `proto` selection + `Connection::build` TLS flag + search-request advertise; `config.cpp` TLS keychain/cert env handling and `updateDefs` serialization (SR-12, SR-13, SR-14)
- `pvaLinkChannel::put`, `linkBuildPut`, `linkPutDone`, `AfterPut::run`, `pvaPutValueAsync`, `pvaLink::onDisconnect`, `pvaLink::valid`, client `Operation` implicit cancel (`GPROp::_cancel`) (SR-15, SR-16)
- `ServerConn::handle_GPR`, `ServerGPRExec`, GPR `subcmd`/`isput` dispatch, `ServerOp::Executing`, `DESTROY_REQUEST` cleanup (SR-17)
- pvaLink put generation accounting: `op_put`, `used_scratch`, `used_queue`, `put_queue`, channel-wide `after_put` (SR-18)
- monitor queue/window accounting: server `MonitorOp::limit/window/ackAt`, client `SubscriptionImpl::unack/window/ackPending`, reconnect path, `MonitorControlOp::post()` backpressure (SR-19, SR-20)
- SharedPV PUT/RPC callback exception paths and GPR outer exception handling (SR-21)
- IOC SingleSource blocking PUT path: `PutOperationCache`, `processNotify`, `dbProcessNotify`, `doneCallback`, EPICS Base `dbNotify` contract (SR-22)
- client discovery dispatch: `Discovery`, `discoverers`, `serverEvent()`, UDP manager beacon callbacks, discovery search replies, `pokeLock` (SR-23)
- client search ignore policy: `ignoreServerGUIDs`, `procSearchReply()`, UDP/TCP search-reply loop ownership (SR-24)
- server ignore address matching: `Config::ignoreAddrs`, `EPICS_PVAS_IGNORE_ADDR_LIST`, `Server::Pvt::onSearch()`, IPv4/IPv6 `SockAddr` field access (SR-25)
- server monitor producer concurrency: `ServerMonitorControl::doPost()`, `MonitorOp::first`, `MonitorOp::lock`, `MonitorControlOp::post()`/`tryPost()`/`forcePost()` (SR-26)
- server source registry locking: `sourcesLock`, `Server::addSource()`/`removeSource()`, `Source::onSearch()`/`onCreate()`/`onList()` callback dispatch (SR-27)
- client connect setup lifecycle: `ConnectBuilder::exec()`, `Channel::build()`, `ConnectImpl::chan`, `connectors`, `evbase::dispatch()` exception handling (SR-28)

## Not Recorded As Findings

- `IOCSource::putLongString()` passes `str.size() + 1` to `dbChannelPut()` for DBR_CHAR arrays. Re-examined against EPICS base: `dbPut` clamps `if (no_elements < nRequest) nRequest = no_elements;` (`dbAccess.c:1361-1362`) before conversion, so an oversized count is truncated to the field's element capacity - no target overflow, and `c_str()` guarantees the `+1` source bytes. Not a defect.
- `GroupSource` callbacks capture `Group&` from `IOCGroupConfig::groupMap`. Not recorded as a confirmed lifetime defect because local shutdown/reset paths stop the server before clearing the group map (`ioc/iochooks.cpp:107-110`, `ioc/groupsourcehooks.cpp:222-230`).
- `BitMask::from_wire` (`src/bitmask.cpp`): the `bits*8` and resize are length-guarded and the decode loop is bounded by the local descriptor size; the `_size = uint16_t(bits)` truncation does not produce an OOB read. Below the Critical/High bar.
- Client monitor pipeline `unack` not reset on reconnect was promoted to PVXS-SR-20 after the review threshold was widened to Medium+.
- `serverconn.cpp:310` DESTROY_REQUEST does not verify the ioid belongs to the looked-up sid's channel (unlike CANCEL_REQUEST at `:280`), but `op->cleanup()` re-erases from the correct map so the maps stay consistent. Not memory-unsafe.
- SharedPV `close()`+`open()` re-typing mid-operation throws in `doReply`, caught at the event-loop boundary (`evhelper.cpp:230`) leaving a stuck-Executing op, not a UAF or double-complete; `post()`/`doPost()` reject type changes.
- `udp_collector.cpp:495` `M.skip(head.len-16u)` underflows when `len<16`, but `Buffer::skip` is bounds-checked (faults via `refill`) and the following `M.good()` check returns. No OOB.
- Value conversion engine (`convertArr`/`copyAs`/`castTo`): every `Src`/`Dest` `convertCast` pair and the void->typed cast were checked size-consistent (mismatches are only signed/unsigned of identical width); destinations are freshly allocated at the correct element count. No OOB or type confusion.
- `BitMask::from_wire` re-verified: the decoded mask is re-clamped to the local descriptor via `valid.resize(top->members.size())` (`dataencode.cpp:714`), and every `word()`/`findSet` access is bounded by `min(wire_words, local_size)`. No OOB. (Supersedes the earlier `_size` truncation note.)
- pvRequest string parser (`clientreq.cpp` PVRParser) is iterative and only parses the local user's pvRequest string; a peer's pvRequest arrives as a binary `Value` through the bounded FieldDesc decoder, so the string parser is not network-reachable. No unbounded-recursion vector there.
- `os/WIN32/osdSockExt.cpp:114` `cbuf[WSA_CMSG_SPACE(sizeof(in6_pktinfo))]` is intentionally sized for the larger of in6_pktinfo/in_pktinfo (only one is received); the smaller fits. `:265` `htonl(0xffffffff<<(32u-p))` is guarded by `p<=0u || p>=32u` continue at `:261`, so the shift is 1..31 - no UB. Not defects.
- (tls branch) TLS verify-bypass: re-checked `src/ossl.cpp` - `SSL_VERIFY_PEER` is set unconditionally on both server and client contexts, the verify callback does not whitelist failures, and the `SSL_CTX`/`X509` objects are owned via RAII (no use-after-free). No verify-bypass finding (TLS-1 cleared).
- (tls branch) `$SSLKEYLOGFILE` TLS key-log support is the standard opt-in OpenSSL debug hook (off unless the env var is set by the operator); not a defect.
- (tls branch) CN buffer is `char name[64]`, so a CN of 64+ printable characters is length-truncated (distinct from the embedded-NUL truncation recorded as SR-13). This could in principle collapse two accounts sharing a 63-char prefix to the same account, but that requires a CA to issue such long, prefix-colliding CNs; hardening, below the High bar. Recommend constructing the account from the returned length and rejecting over-length CNs alongside the SR-13 fix.
- `StaticSource::close()` holds the StaticSource read lock while calling `SharedPV::close()` (`src/sharedpv.cpp:554-565`), while `StaticSource::remove()` copies the `SharedPV` out and closes it after releasing the writer lock (`src/sharedpv.cpp:584-603`). This can deadlock only if local close/disconnect callbacks re-enter the same `StaticSource`; recorded as a local API hardening candidate, not a Medium+ source finding in this pass.
