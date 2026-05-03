# CA Reference Feature Map (Channel Access)

Public API + wire protocol of EPICS Channel Access, extracted from the
upstream `epics-base` C reference implementation. This is **Layer 1**
of the reference-feature-map: a stable inventory of "what a CA library
must do." Implementation status (Layer 2) lives separately.

**Reference revision**: `epics-base @ c9817fa59` (audited 2026-05-03)
**Source headers**:
- `include/cadef.h` (2031 lines, 68 public functions)
- `include/caProto.h` (190 lines, wire protocol commands)
- `include/caeventmask.h` (50 lines, DBE event flags)
- `include/db_access.h` (767 lines, DBR types)
- `include/caerr.h` (223 lines, ECA error codes)

ID prefix `CA-NNN` is stable; new entries append, never renumber.

---

## 1. Client context lifecycle

| ID | Symbol | Header:line | Description |
|----|--------|-------------|-------------|
| CA-001 | `ca_task_initialize` | cadef.h:301 | Legacy aliased entry point for `ca_context_create(ca_disable_preemptive_callback)`. |
| CA-002 | `ca_context_create` | cadef.h:355 | Create a per-thread CA client context. `select` chooses preemptive vs. non-preemptive callback dispatch. |
| CA-003 | `ca_context_destroy` | cadef.h:411 | Tear down the calling thread's CA context, closing all channels and circuits. |
| CA-004 | `ca_detach_context` | cadef.h:365 | Detach the calling thread from its current context without destroying it. |
| CA-005 | `ca_attach_context` | cadef.h:1938 | Attach the calling thread to an existing context (typically created by another thread). |
| CA-006 | `ca_current_context` | cadef.h:1920 | Return the calling thread's active context (or NULL). |
| CA-007 | `ca_task_exit` | cadef.h:377 | Legacy alias for `ca_context_destroy`. |
| CA-008 | `ca_preemtive_callback_is_enabled` | cadef.h:1904 | Query whether the current context dispatches user callbacks asynchronously. |
| CA-009 | `ca_modify_user_name` | cadef.h:2020 | Change the user-name string sent to servers (host-side ACL). |
| CA-010 | `ca_modify_host_name` | cadef.h:2021 | Change the host-name string sent to servers. |

---

## 2. Channel lifecycle

| ID | Symbol | Header:line | Description |
|----|--------|-------------|-------------|
| CA-020 | `ca_create_channel` | cadef.h:519 | Open a channel by PV name. Optional connection-state callback fires asynchronously. |
| CA-021 | `ca_clear_channel` | cadef.h:647 | Close a channel; cancel pending I/O / subscriptions. |
| CA-022 | `ca_change_connection_event` | cadef.h:534 | Replace the connection-state callback registered at `ca_create_channel`. |
| CA-023 | `ca_replace_access_rights_event` | cadef.h:561 | Register / replace the access-rights-changed callback for a channel. |
| CA-024 | `ca_state` | cadef.h:289 | Return the channel's `cs_never_conn` / `cs_prev_conn` / `cs_conn` / `cs_closed` state. |
| CA-025 | `ca_field_type` | cadef.h:219 | Server-side native DBR type of the connected channel. |
| CA-026 | `ca_element_count` | cadef.h:227 | Server-side element count of the connected channel. |
| CA-027 | `ca_name` | cadef.h:235 | Return the PV name passed to `ca_create_channel`. |
| CA-028 | `ca_set_puser` | cadef.h:243 | Stash a user pointer on the channel handle (per-channel user data). |
| CA-029 | `ca_puser` | cadef.h:251 | Retrieve the user pointer set by `ca_set_puser`. |
| CA-030 | `ca_read_access` | cadef.h:260 | Boolean: server granted read access. |
| CA-031 | `ca_write_access` | cadef.h:269 | Boolean: server granted write access. |
| CA-032 | `ca_host_name` | cadef.h:1449 | Static buffer pointer to the connected server's host name. |
| CA-033 | `ca_get_host_name` | cadef.h:1460 | Reentrant variant of `ca_host_name` — copies into caller buffer. |
| CA-034 | `ca_host_minor_protocol` | cadef.h:1470 | Minor protocol version of the server hosting this channel. |
| CA-035 | `ca_search_attempts` | cadef.h:1907 | Number of UDP SEARCH retransmits issued for a still-searching channel. |
| CA-036 | `ca_v42_ok` | cadef.h:1853 | Server supports CA v4.2+ (asynchronous access-rights events). |
| CA-037 | `ca_build_and_connect` | cadef.h:1970 | Legacy synchronous variant of `ca_create_channel` (combines build + pend_io). |
| CA-038 | `ca_search_and_connect` | cadef.h:1985 | Legacy alias for `ca_create_channel`. |

---

## 3. Read / Write (one-shot)

| ID | Symbol | Header:line | Description |
|----|--------|-------------|-------------|
| CA-050 | `ca_array_get` | cadef.h:971 | Issue a GET; result delivered into the caller's buffer after the next `ca_pend_io`. |
| CA-051 | `ca_array_get_callback` | cadef.h:1067 | Issue a GET with a user callback delivered via `ca_pend_event`. |
| CA-052 | `ca_array_put` | cadef.h:781 | Issue a fire-and-forget WRITE. |
| CA-053 | `ca_array_put_callback` | cadef.h:820 | Issue a WRITE with completion callback (server confirms record processing). |

> Convenience wrappers `ca_get`, `ca_get_callback`, `ca_put`, `ca_put_callback` call into the array variants with `count = 1`.

---

## 4. Subscriptions (monitors)

| ID | Symbol | Header:line | Description |
|----|--------|-------------|-------------|
| CA-070 | `ca_create_subscription` | cadef.h:1178 | Create a monitor; callback fires on every value change matching `mask`. |
| CA-071 | `ca_clear_subscription` | cadef.h:1215 | Cancel a subscription. |
| CA-072 | `ca_clear_event` | cadef.h:1995 | Legacy alias for `ca_clear_subscription`. |
| CA-073 | `ca_evid_to_chid` | cadef.h:1220 | Recover the channel handle from a subscription id. |
| CA-074 | `ca_add_masked_array_event` | cadef.h:2012 | Legacy variant of `ca_create_subscription` that returns chid via output ptr. |
| CA-075 | `ca_test_event` | cadef.h:118 | Synthesize a fake monitor callback (test helper). |

---

## 5. Synchronous group (batch)

| ID | Symbol | Header:line | Description |
|----|--------|-------------|-------------|
| CA-090 | `ca_sg_create` | cadef.h:1579 | Create an opaque handle that batches GETs/PUTs into a single block point. |
| CA-091 | `ca_sg_delete` | cadef.h:1598 | Free the group; cancel pending operations. |
| CA-092 | `ca_sg_array_get` | cadef.h:1717 | Add a GET to the group. |
| CA-093 | `ca_sg_array_put` | cadef.h:1789 | Add a PUT to the group. |
| CA-094 | `ca_sg_block` | cadef.h:1637 | Block the calling thread until all operations in the group complete or `timeout` elapses. |
| CA-095 | `ca_sg_test` | cadef.h:1654 | Non-blocking poll of group completion. |
| CA-096 | `ca_sg_reset` | cadef.h:1673 | Discard any in-flight operations on the group without blocking. |
| CA-097 | `ca_sg_stat` | cadef.h:1832 | Per-group diagnostic dump to stdout. |

---

## 6. Event loop / I/O scheduling

| ID | Symbol | Header:line | Description |
|----|--------|-------------|-------------|
| CA-110 | `ca_pend_event` | cadef.h:1293 | Run the event loop for `timeout` seconds (or forever with 0). Required when context is non-preemptive. |
| CA-111 | `ca_pend_io` | cadef.h:1351 | Block until all outstanding `ca_array_get` (no-callback variant) operations complete or timeout. |
| CA-112 | `ca_pend` | cadef.h:1354 | Combined `pend_event` + `pend_io` driver (legacy). |
| CA-113 | `ca_test_io` | cadef.h:1369 | Non-blocking poll: are any outstanding GETs still pending? |
| CA-114 | `ca_flush_io` | cadef.h:1385 | Force any buffered outbound CA frames to the wire immediately. |
| CA-115 | `ca_add_fd_registration` | cadef.h:1531 | Register a callback that receives the CA UDP/TCP socket fd at registration time (legacy event-loop integration hook). |

---

## 7. Diagnostics & exceptions

| ID | Symbol | Header:line | Description |
|----|--------|-------------|-------------|
| CA-130 | `ca_add_exception_event` | cadef.h:617 | Register a process-wide handler for unrecoverable / out-of-band errors. |
| CA-131 | `ca_signal` | cadef.h:1406 | Format-and-print an `ECA_xxx` status with the standard library prefix. |
| CA-132 | `ca_signal_with_file_and_lineno` | cadef.h:1420 | Variant with explicit file/line, used by `SEVCHK` macro. |
| CA-133 | `ca_signal_formated` | cadef.h:1436 | Variant with printf-style format args. |
| CA-134 | `ca_dump_dbr` | cadef.h:1841 | Pretty-print a DBR buffer to stdout (debugging). |
| CA-135 | `ca_replace_printf_handler` | cadef.h:1895 | Redirect library-internal `errlog` output to a custom callback. |
| CA-136 | `ca_self_test` | cadef.h:1905 | Library-internal sanity check (assertion-only). |
| CA-137 | `ca_client_status` | cadef.h:1949 | Diagnostic dump of the calling thread's context. |
| CA-138 | `ca_context_status` | cadef.h:1961 | Diagnostic dump of an arbitrary context (cross-thread). |
| CA-139 | `ca_channel_status` | cadef.h:1989 | Diagnostic dump of all channels owned by `tid`. |
| CA-140 | `ca_get_ioc_connection_count` | cadef.h:1903 | Number of distinct IOC virtual circuits the context holds. |
| CA-141 | `ca_beacon_anomaly_count` | cadef.h:1906 | Process-wide count of detected beacon anomalies (server restart hints). |
| CA-142 | `ca_beacon_period` | cadef.h:1908 | Estimated beacon period observed for a channel's server. |
| CA-143 | `ca_receive_watchdog_delay` | cadef.h:1909 | Time since the channel's circuit last received any frame (liveness). |
| CA-144 | `ca_version` | cadef.h:1860 | Runtime CA library version string. |

---

## 8. Wire protocol commands (`caProto.h`)

| ID | Symbol | Header:line | Description |
|----|--------|-------------|-------------|
| CA-200 | `CA_PROTO_VERSION` (0) | caProto.h:87 | Minor version + priority handshake (used to be NOOP). |
| CA-201 | `CA_PROTO_EVENT_ADD` (1) | caProto.h:88 | Server-bound: add a monitor. Server-emitted: monitor data delivery. |
| CA-202 | `CA_PROTO_EVENT_CANCEL` (2) | caProto.h:89 | Cancel a previously-added monitor. |
| CA-203 | `CA_PROTO_READ` (3) | caProto.h:90 | Synchronous read (legacy; clients use READ_NOTIFY now). |
| CA-204 | `CA_PROTO_WRITE` (4) | caProto.h:91 | Fire-and-forget write. |
| CA-205 | `CA_PROTO_SNAPSHOT` (5) | caProto.h:92 | Obsolete: snapshot of system state. |
| CA-206 | `CA_PROTO_SEARCH` (6) | caProto.h:93 | UDP broadcast PV-name → server lookup. |
| CA-207 | `CA_PROTO_BUILD` (7) | caProto.h:94 | Obsolete: build channel (predecessor to CREATE_CHAN). |
| CA-208 | `CA_PROTO_EVENTS_OFF` (8) | caProto.h:95 | Flow control: ask server to pause monitor delivery. |
| CA-209 | `CA_PROTO_EVENTS_ON` (9) | caProto.h:96 | Flow control: resume monitor delivery. |
| CA-210 | `CA_PROTO_READ_SYNC` (10) | caProto.h:97 | Pre-v4.3 echo-equivalent: purge old reads. |
| CA-211 | `CA_PROTO_ERROR` (11) | caProto.h:98 | Server-bound error report (failed operation). |
| CA-212 | `CA_PROTO_CLEAR_CHANNEL` (12) | caProto.h:99 | Free server-side resources for a channel. |
| CA-213 | `CA_PROTO_RSRV_IS_UP` (13) | caProto.h:100 | Server beacon: "I'm alive" announcement. |
| CA-214 | `CA_PROTO_NOT_FOUND` (14) | caProto.h:101 | Server response to SEARCH: "I don't host this PV". |
| CA-215 | `CA_PROTO_READ_NOTIFY` (15) | caProto.h:102 | Modern read with completion notification (replaces CA_PROTO_READ). |
| CA-216 | `CA_PROTO_READ_BUILD` (16) | caProto.h:103 | Obsolete: read + build combined. |
| CA-217 | `CA_PROTO_CREATE_CHAN` (18) | caProto.h:105 | Client→server channel creation request. |
| CA-218 | `CA_PROTO_WRITE_NOTIFY` (19) | caProto.h:106 | Write with completion notification. |
| CA-219 | `CA_PROTO_CLIENT_NAME` (20) | caProto.h:107 | v4.1 client identification: user name. |
| CA-220 | `CA_PROTO_HOST_NAME` (21) | caProto.h:108 | v4.1 client identification: host name. |
| CA-221 | `CA_PROTO_ACCESS_RIGHTS` (22) | caProto.h:109 | v4.2 server-asynchronous access-rights change event. |
| CA-222 | `CA_PROTO_ECHO` (23) | caProto.h:110 | v4.3 connection-liveness ping. |
| CA-223 | `CA_PROTO_REPEATER_REGISTER` (24) | caProto.h | Register a client with the per-host UDP repeater. |
| CA-224 | `CA_PROTO_REPEATER_CONFIRM` | caProto.h | Repeater→client acknowledgement of registration. |
| CA-225 | `CA_PROTO_SIGNAL` (25) | caProto.h:112 | "Wake the server out of `select()`" — internal scheduling. |
| CA-226 | `CA_PROTO_CREATE_CH_FAIL` (26) | caProto.h:113 | Server rejected channel creation (out of resources). |
| CA-227 | `CA_PROTO_SERVER_DISCONN` (27) | caProto.h:114 | Server-initiated channel close (PV deleted). |

Access-rights bit flags (caProto.h:147-148):

| ID | Symbol | Description |
|----|--------|-------------|
| CA-240 | `CA_PROTO_ACCESS_RIGHT_READ` | Server granted read access. |
| CA-241 | `CA_PROTO_ACCESS_RIGHT_WRITE` | Server granted write access. |

---

## 9. DBR type system (`db_access.h`)

DBR (DataBase Request) types layer status / time / graphic / control metadata
on top of the seven core scalar types `STRING / SHORT(=INT) / FLOAT / ENUM /
CHAR / LONG / DOUBLE`. The seven core types × five layers gives the canonical
DBR type set.

| ID | Symbol | Layer | Description |
|----|--------|-------|-------------|
| CA-260 | `DBR_STRING` … `DBR_DOUBLE` (0..6) | core | Bare scalar values. |
| CA-261 | `DBR_STS_STRING` … `DBR_STS_DOUBLE` (7..13) | + status | Status (alarm severity + status code) prepended to value. |
| CA-262 | `DBR_TIME_STRING` … `DBR_TIME_DOUBLE` (14..20) | + status + timestamp | Adds 64-bit EPICS timestamp. |
| CA-263 | `DBR_GR_STRING` … `DBR_GR_DOUBLE` (21..27) | + status + graphic | Adds units / display limits / precision (no timestamp). |
| CA-264 | `DBR_CTRL_STRING` … `DBR_CTRL_DOUBLE` (28..34) | + status + control | Graphic + control limits. |
| CA-265 | `DBR_PUT_ACKT` (35) | special | Acknowledgement-transient PUT. |
| CA-266 | `DBR_PUT_ACKS` (36) | special | Acknowledgement-severity PUT. |
| CA-267 | `DBR_STSACK_STRING` (37) | special | Status + ackt + acks composite. |
| CA-268 | `DBR_CLASS_NAME` (38) | special | Returns the IOC record-type class name. |

---

## 10. Subscription event masks (`caeventmask.h`)

| ID | Symbol | Bit | Description |
|----|--------|-----|-------------|
| CA-280 | `DBE_VALUE` | `1<<0` | Fire on value change exceeding MDEL. |
| CA-281 | `DBE_ARCHIVE` (alias `DBE_LOG`) | `1<<1` | Fire on archive deadband (ADEL). |
| CA-282 | `DBE_ALARM` | `1<<2` | Fire on alarm-severity / alarm-status change. |
| CA-283 | `DBE_PROPERTY` | `1<<3` | Fire on metadata change (units, limits, precision). |

---

## 11. Channel state enum (`cadef.h`)

| ID | Symbol | Description |
|----|--------|-------------|
| CA-300 | `cs_never_conn` | Has not yet connected after `ca_create_channel`. |
| CA-301 | `cs_prev_conn` | Was connected; server went away (reconnecting). |
| CA-302 | `cs_conn` | Currently connected (TCP virtual circuit + CREATE_CHAN OK). |
| CA-303 | `cs_closed` | Channel handle invalid (after `ca_clear_channel`). |

---

## 12. ECA error / status codes (`caerr.h`)

50+ error codes covering allocation, name resolution, type mismatch, server
errors, and timeout cases. Categories:

| ID | Range | Description |
|----|-------|-------------|
| CA-320 | `ECA_NORMAL` (0), `ECA_TIMEOUT` (10), `ECA_DISCONN`, … | Operation outcome codes returned from the public API. |
| CA-321 | `ECA_BADTYPE`, `ECA_BADCOUNT`, `ECA_BADCHID`, … | Validation errors (caller bug / programming mistake). |
| CA-322 | `ECA_NOWTACCESS`, `ECA_NORDACCESS`, `ECA_NOACCESS` | Access-rights denial. |
| CA-323 | `ECA_BADPRIORITY`, `ECA_NOTTHREADED`, `ECA_16KARRAYCLIENT` | Misc protocol-level constraints. |

> Individual code mappings live in `caerr.h:38..213`. The reference treats
> them as a closed enumeration; clients translate via `ca_message(status)`.

---

## 13. Environment-variable knobs

These are read by libca at startup; documented across `cadef.h`,
`epicsExport.h`, and the EPICS Application Developer's Guide.

| ID | Variable | Description |
|----|----------|-------------|
| CA-340 | `EPICS_CA_ADDR_LIST` | Space-separated list of `host[:port]` to use for unicast/broadcast SEARCH. |
| CA-341 | `EPICS_CA_AUTO_ADDR_LIST` | `YES`/`NO`: auto-discover broadcast addresses per NIC. |
| CA-342 | `EPICS_CA_SERVER_PORT` | Default IOC TCP port (5064). |
| CA-343 | `EPICS_CA_REPEATER_PORT` | UDP repeater port (5065). |
| CA-344 | `EPICS_CA_CONN_TMO` | Connection-verify ECHO interval (default 30 s). |
| CA-345 | `EPICS_CA_BEACON_PERIOD` | Server-side beacon emit interval (default 15 s). |
| CA-346 | `EPICS_CA_MAX_ARRAY_BYTES` | Cap on inbound array message size (default 16 KB; 0 disables cap). |
| CA-347 | `EPICS_CA_MAX_SEARCH_PERIOD` | Upper bound on SEARCH retransmit interval (default 5 min). |
| CA-348 | `EPICS_CA_NAME_SERVERS` | TCP-based name-server list (modern alternative to UDP SEARCH). |
| CA-349 | `EPICS_CAS_INTF_ADDR_LIST` | Server: NICs to bind for SEARCH reception. |
| CA-350 | `EPICS_CAS_BEACON_ADDR_LIST` | Server: explicit beacon destinations. |
| CA-351 | `EPICS_CAS_AUTO_BEACON_ADDR_LIST` | Server: auto-discover broadcast destinations. |
| CA-352 | `EPICS_CAS_IGNORE_ADDR_LIST` | Server: silently ignore SEARCH from these peers. |
| CA-353 | `EPICS_CAS_BEACON_PERIOD` | Server: beacon emission period override. |
| CA-354 | `EPICS_CAS_SERVER_PORT` | Server: bind port override. |

---

## 14. Server-side (rsrv) facilities

`rsrv` is the IOC's CA server. It exposes no formal public C API to
applications (its surface is internal to the IOC build), but the
operational features are part of the CA reference contract.

| ID | Feature | Description |
|----|---------|-------------|
| CA-400 | UDP search responder | Listens on `EPICS_CAS_INTF_ADDR_LIST`, replies to matching SEARCHes. |
| CA-401 | Beacon emitter (`online_notify_task`) | Initial 20 ms doubling ramp-up to `beacon_period`; "I'm alive" announcement. |
| CA-402 | Per-client TCP virtual circuit | `tcpiiu` per client; routes named-channel I/O. |
| CA-403 | Per-channel resource allocation | `casChannelI` / SID allocator. |
| CA-404 | Access-rights authorization | `asLoadFile` ACL evaluation on channel create. |
| CA-405 | DBR conversion | Native field type → requested DBR type marshalling. |
| CA-406 | Subscription delivery | Fan-out from record processing → matching `EVENT_ADD`. |
| CA-407 | Flow control | Honor `EVENTS_OFF` / `EVENTS_ON` per circuit. |
| CA-408 | Echo / receive watchdog | `tcpRecvWatchdog` half-open detection (CA v4.3). |
| CA-409 | Repeater process | Per-host UDP demux to multiple co-located clients. |

---

## 15. Cross-cutting design choices

These are protocol-level rather than feature-level, but matter for any
implementation auditing 1-to-1 correspondence:

| ID | Topic | Reference | Description |
|----|-------|-----------|-------------|
| CA-500 | Header alignment | caProto.h | All CA frames are 8-byte-aligned. Extended headers (postsize 0xFFFF) carry 32-bit count + payload. |
| CA-501 | Byte order | caProto.h | All wire fields are big-endian (network order). |
| CA-502 | UDP message coalescing | rsrv / repeater | Multiple search responses or beacons can be concatenated in one datagram. |
| CA-503 | Beacon anomaly detection | bhe.cpp:51,199 | Client tracks beacon period via EMA; period collapse / id reset signals server restart. |
| CA-504 | Search retry algorithm | searchTimer.cpp | N exponentially-spaced timers (`(1<<i) * minRTT`); channels promote up the ladder per failure. Beacon-anomaly fast-promote. |
| CA-505 | Sync-group atomicity | ca_sg_*.cpp | `pend_block` waits for ALL group operations; partial completions reported via per-op status. |
| CA-506 | Connection state machine | nciu.cpp | `cs_never_conn → cs_conn → cs_prev_conn → cs_conn` reconnect loop driven by SEARCH + CREATE_CHAN replies. |
| CA-507 | Channel lifetime and circuits | tcpiiu.cpp | Multiple channels share one virtual circuit (TCP) per server; circuit teardown disconnects all. |
| CA-508 | Authentication model | rsrv + asLib | Client supplies user/host strings (CA_PROTO_CLIENT_NAME / HOST_NAME); server applies AS rules. No cryptographic auth. |

---

## Summary

- **68** public C functions (cadef.h)
- **27** wire protocol commands (caProto.h)
- **40** DBR type variants (db_access.h)
- **4** subscription event masks (caeventmask.h)
- **4** channel state enum values
- **15** environment variables (client + server)
- **10** rsrv server-side feature areas
- **9** cross-cutting design topics

Total inventory items: **177**.
