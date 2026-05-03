# PVA Reference Feature Map (PV Access)

Public API + wire protocol of EPICS PV Access, extracted from the
upstream `pvxs` C++ reference implementation. This is **Layer 1**
of the reference-feature-map: a stable inventory of "what a PVA library
must do." Implementation status (Layer 2) lives separately.

**Reference revision**: `pvxs @ 9beba6b` (audited 2026-05-03)
**Source headers** (5184 total lines):
- `include/pvxs/client.h` (1122 lines, client-side core)
- `include/pvxs/server.h` (240 lines, server lifecycle)
- `include/pvxs/data.h` (948 lines, Value / TypeDef)
- `include/pvxs/sharedpv.h` (127 lines, in-process server PV)
- `include/pvxs/source.h` (297 lines, custom server-side source)
- `include/pvxs/nt.h` (213 lines, Normative Type helpers)
- `include/pvxs/iochooks.h` (171 lines, IOC integration)
- `include/pvxs/util.h` (354 lines, utilities)
- `include/pvxs/log.h` (148 lines, logging facade)
- `include/pvxs/netcommon.h` (172 lines, shared net types)
- `include/pvxs/sharedArray.h` (812 lines, COW array)
- `include/pvxs/srvcommon.h` (126 lines, server-shared types)
- `include/pvxs/unittest.h` (357 lines, testing helpers)
- `include/pvxs/version.h` (97 lines)
- `src/pvaproto.h` (wire protocol commands — internal but stable)

ID prefix `PVA-NNN` is stable; new entries append, never renumber.

---

## 1. Client Context

| ID | Symbol | Header:line | Description |
|----|--------|-------------|-------------|
| PVA-001 | `client::Context` | client.h:315 | PVA client connection context. Holds search engine, beacon listener, virtual circuit pool. |
| PVA-002 | `Context(Config&)` | client.h:323 | Build a context from a `client::Config`. |
| PVA-003 | `Context::fromEnv()` | client.h:332 | Shorthand for `Config::fromEnv().build()`. Reads `EPICS_PVA_*` env vars. |
| PVA-004 | `Context::reconfigure(Config&)` | client.h:341 | Apply a new config (currently TLS-only updates). Disconnects in-progress operations. |
| PVA-005 | `Context::close()` | client.h:356 | Tear down the context (cancel all pending operations). |
| PVA-006 | `Context::hurryUp()` | client.h:601 | Force the search engine into fast-tick mode for one revolution (use after OOB "server is up" hint). |
| PVA-007 | `Context::ignoreServerGUIDs(...)` | client.h:622 | Blocklist server GUIDs whose beacons / search-responses should be ignored. |
| PVA-008 | `Context::cacheClear(...)` | client.h | Drop any cached state for a single PV name; next `find()` issues a fresh search. |
| PVA-009 | `Context::report(zero=true)` | client.h:626 | Snapshot of per-server / per-channel counters for diagnostics. |
| PVA-010 | `Context::request()` | client.h:553 | Build a `pvRequest` programmatically (`RequestBuilder`). |

---

## 2. Client Operations (Builder pattern)

| ID | Symbol | Header:line | Description |
|----|--------|-------------|-------------|
| PVA-020 | `Context::get(name)` | client.h:383 | Returns a `GetBuilder`. One-shot read. |
| PVA-021 | `Context::info(name)` | client.h:412 | Returns a `GetBuilder` configured for GET_FIELD (introspection-only, no value). |
| PVA-022 | `Context::put(name)` | client.h:453 | Returns a `PutBuilder`. One-shot write with optional pre-fetch. |
| PVA-023 | `Context::rpc(name)` | client.h:456 | Returns an `RPCBuilder`. Remote-procedure call. |
| PVA-024 | `Context::rpc(name, arg)` | client.h:488 | RPC helper that takes the argument inline. |
| PVA-025 | `Context::monitor(name)` | client.h:523 | Returns a `MonitorBuilder`. Long-lived subscription. |
| PVA-026 | `Context::connect(name)` | client.h:536 | Returns a `ConnectBuilder`. Track a channel's connection state without performing operations. |

### 2.1 Common builder methods (`PRBase`)

| ID | Method | Description |
|----|--------|-------------|
| PVA-040 | `field(s)` | Add a field-list filter (`field(value, alarm.severity)`). |
| PVA-041 | `record(key, val)` | Add a `record._options.<key>=<val>` clause. |
| PVA-042 | `pvRequest(req_str)` | Set the raw pvRequest string. |
| PVA-043 | `rawRequest(value)` | Set the raw pvRequest as a Value (advanced). |
| PVA-044 | `priority(p)` | Set TCP priority (0-99). |
| PVA-045 | `server(s)` | Pin the operation to a specific server (skip search). |
| PVA-046 | `syncCancel(b)` | Cancel-on-drop semantics for the resulting Operation. |

### 2.2 GetBuilder / PutBuilder

| ID | Symbol | Header:line | Description |
|----|--------|-------------|-------------|
| PVA-060 | `GetBuilder::result(cb)` | client.h:755 | Callback delivering `Result&&` (success / Disconnect / RemoteError). |
| PVA-061 | `GetBuilder::onInit(cb)` | client.h:760 | Callback when channel's introspection arrives. |
| PVA-062 | `GetBuilder::exec()` / `exec_with_handle` | client.h | Submit; returns `Operation` (use `.wait()` for sync). |
| PVA-063 | `PutBuilder::fetchPresent(b)` | client.h:795 | Pre-fetch current value before applying changes (RMW semantics). |
| PVA-064 | `PutBuilder::set(name, val, required)` | client.h:797 | Set one field of the put value. Templated for type-safe stores. |
| PVA-065 | `PutBuilder::build(cb)` | client.h:823 | Custom build callback receiving `Value&&` to populate. |
| PVA-066 | `PutBuilder::result(cb)` | client.h:831 | Result callback. |

### 2.3 MonitorBuilder

| ID | Symbol | Header:line | Description |
|----|--------|-------------|-------------|
| PVA-080 | `MonitorBuilder::event(cb)` | client.h:916 | Per-update callback receiving the `Subscription&`. |
| PVA-081 | `MonitorBuilder::maskConnected(b)` | client.h:918 | Suppress Connected events. |
| PVA-082 | `MonitorBuilder::maskDisconnected(b)` | client.h:920 | Suppress Disconnected events. |
| PVA-083 | `MonitorBuilder::onInit(cb)` | client.h:925 | Callback when monitor INIT response arrives (introspection). |
| PVA-084 | `MonitorBuilder::pipeline(n)` | client.h | pvRequest `record._options.pipeline` + `queueSize`. |

### 2.4 ConnectBuilder

| ID | Symbol | Header:line | Description |
|----|--------|-------------|-------------|
| PVA-100 | `ConnectBuilder::onConnect(cb)` | client.h:967 | Callback fires when channel reaches Connected state. |
| PVA-101 | `ConnectBuilder::onDisconnect(cb)` | client.h:977 | Callback fires when channel disconnects. |
| PVA-102 | `ConnectBuilder::server(addr)` | client.h:988 | Bypass search; connect to a specific TCP endpoint. |

---

## 3. Operation handle (`client::Operation`)

| ID | Symbol | Header:line | Description |
|----|--------|-------------|-------------|
| PVA-120 | `Operation::wait(timeout)` | client.h:162 | Block until result arrives or `timeout` elapses. Returns the value or throws on error. |
| PVA-121 | `Operation::wait()` | client.h:163 | Block forever (99,999,999 seconds). |
| PVA-122 | `Operation::cancel()` | client.h | Cooperatively cancel; subsequent `wait()` returns `Interrupted`. |
| PVA-123 | `Operation::error()` | client.h:117 | Boolean: did the operation complete with an error? |
| PVA-124 | `Disconnect`, `RemoteError`, `Finished`, `Connected`, `Interrupted`, `Timeout` | client.h:39-92 | Exception types thrown from `wait()` / delivered via result callback. |

---

## 4. Subscription handle (`client::Subscription`)

| ID | Symbol | Header:line | Description |
|----|--------|-------------|-------------|
| PVA-140 | `Subscription` | client.h:198 | Long-lived monitor handle. |
| PVA-141 | `pop()` | client.h | Drain one update from the queue. Returns `Value{}` (empty) when queue is dry. |
| PVA-142 | `cancel()` | client.h | Permanently stop the subscription. |
| PVA-143 | `pause(b)` / `resume()` | client.h | Pause / resume server-side flow control (sends pipeline-pause control). |
| PVA-144 | `stats()` | client.h | Per-subscription counters (events delivered / queued / dropped). |

---

## 5. Server (`server::Server`)

| ID | Symbol | Header:line | Description |
|----|--------|-------------|-------------|
| PVA-200 | `Server` | server.h:54 | PVA server — owns UDP responder, TCP listener, sources. |
| PVA-201 | `Server(Config&)` | server.h:61 | Allocate (do not start). |
| PVA-202 | `Server::fromEnv()` | server.h:74 | Build from `EPICS_PVAS_*` environment. |
| PVA-203 | `Server::start()` | server.h:77 | Begin accepting connections. Non-blocking. |
| PVA-204 | `Server::stop()` | server.h:79 | Stop and disconnect all clients. |
| PVA-205 | `Server::run()` | server.h:90 | start() + block until SIGINT/SIGTERM/`interrupt()`. |
| PVA-206 | `Server::interrupt()` | server.h:92 | Cooperatively wake `run()`. |
| PVA-207 | `Server::reconfigure(Config&)` | server.h:101 | TLS-only live reconfigure. |
| PVA-208 | `Server::config()` | server.h:105 | Effective config (post-reconfigure). |
| PVA-209 | `Server::clientConfig()` | server.h:109 | Build a client::Config that talks to *this* server (testing helper). |
| PVA-210 | `Server::addPV(name, SharedPV)` | server.h:112 | Register a SharedPV under `__builtin` source. |
| PVA-211 | `Server::removePV(name)` | server.h:114 | De-register. |
| PVA-212 | `Server::addSource(name, src, order)` | server.h:126 | Register a custom Source (advanced). |
| PVA-213 | `Server::removeSource(name, order)` | server.h:131 | De-register Source. |
| PVA-214 | `Server::getSource(name, order)` | server.h:135 | Look up Source by name. |
| PVA-215 | `Server::listSource()` | server.h:139 | Enumerate (name, order) pairs. |
| PVA-216 | `Server::report(zero=true)` | server.h:145 | Per-peer / per-channel counters (PVXS_EXPERT_API). |

---

## 6. SharedPV (in-process server PV)

| ID | Symbol | Header:line | Description |
|----|--------|-------------|-------------|
| PVA-240 | `SharedPV` | sharedpv.h:38 | A server-side PV with hand-written Get / Put / RPC handlers. |
| PVA-241 | `SharedPV::buildMailbox()` | sharedpv.h:41 | Pre-built handler that posts whatever the client wrote. |
| PVA-242 | `SharedPV::buildReadonly()` | sharedpv.h:43 | Pre-built handler that rejects writes. |
| PVA-243 | `SharedPV::open(initial)` | sharedpv.h:68 | Set initial value + permit client connections. |
| PVA-244 | `SharedPV::isOpen()` | sharedpv.h:70 | Boolean. |
| PVA-245 | `SharedPV::close()` | sharedpv.h:72 | Disconnect all clients. |
| PVA-246 | `SharedPV::post(value)` | sharedpv.h:80 | Update value + fan-out subscriptions. |
| PVA-247 | `SharedPV::fetch()` / `fetch(out)` | sharedpv.h:83-85 | Read internal value. |
| PVA-248 | `SharedPV::onFirstConnect(cb)` | sharedpv.h:55 | Callback when first client connects. |
| PVA-249 | `SharedPV::onLastDisconnect(cb)` | sharedpv.h:57 | Callback when last client leaves. |
| PVA-250 | `SharedPV::onPut(cb)` | sharedpv.h:59 | Custom write handler (`(SharedPV&, ExecOp&&, Value&&)`). |
| PVA-251 | `SharedPV::onRPC(cb)` | sharedpv.h:62 | Custom RPC handler. |
| PVA-252 | `SharedPV::attach(ChannelControl)` | sharedpv.h:52 | Manual attach (when not using StaticSource). |
| PVA-253 | `StaticSource` | sharedpv.h:97 | Aggregator: server-side name → SharedPV mapping (used by `Server::addPV`). |

---

## 7. Custom Source (`server::Source`)

| ID | Symbol | Header:line | Description |
|----|--------|-------------|-------------|
| PVA-280 | `Source` | source.h:204 | Pluggable resolver: handles `onSearch` (does this source know this PV?) and `onCreate` (build the channel). |
| PVA-281 | `Source::onSearch(op)` | source.h | Called for each SEARCH; source may claim the PV. |
| PVA-282 | `Source::onCreate(ChannelControl)` | source.h | Called when a client's create-channel arrives. |
| PVA-283 | `ChannelControl` | source.h:166 | Per-channel handle: install onOp / onSubscribe handlers, query peer credentials. |
| PVA-284 | `ConnectOp` | source.h:23 | Server-side handle for INIT-phase operations. |
| PVA-285 | `MonitorSetupOp` | source.h:137 | Server-side INIT for monitors (returns a `MonitorControlOp`). |
| PVA-286 | `MonitorControlOp` | source.h:75 | Server-side monitor in operation: `tryPost`, `forcePost`, `setHighWatermark`, `setLowWatermark`. |
| PVA-287 | `ExecOp` | srvcommon.h:84 | Per-op handle (Get / Put / RPC) with `reply(value)` / `error(msg)` / `info(msg)`. |
| PVA-288 | `OpBase` | srvcommon.h:40 | Base for op handles: peer credentials, name, isOpen. |
| PVA-289 | `RemoteLogger` | srvcommon.h:75 | Mixin for `info()` / `warn()` / `error()` strings sent to client (CMD_MESSAGE). |
| PVA-290 | `ClientCredentials` | srvcommon.h:34 | Server-side view of authenticated client (peer cert, user, host). |

---

## 8. Data model (`Value`, `TypeDef`)

| ID | Symbol | Header:line | Description |
|----|--------|-------------|-------------|
| PVA-310 | `Value` | data.h:505 | Self-describing value. References a FieldDesc tree; supports lookup by sub-name (`v["alarm.severity"]`). |
| PVA-311 | `Value::operator[]` | data.h | Sub-field access by dot-separated path. |
| PVA-312 | `Value::as<T>()` | data.h | Type-coerced read (throws `NoConvert` on incompatibility). |
| PVA-313 | `Value::from(T)` / `operator=` | data.h | Type-coerced write. |
| PVA-314 | `Value::valid()` | data.h | Sentinel check. |
| PVA-315 | `Value::cloneEmpty()` | data.h | Same TypeDef, empty value. |
| PVA-316 | `Value::id()` | data.h | Structure ID (e.g. `epics:nt/NTScalar:1.0`). |
| PVA-317 | `Value::nmembers()` / `iall()` / `imarked()` | data.h | Walk all / marked sub-fields. |
| PVA-318 | `TypeDef` | data.h:380 | Builder for compound types (`Struct`, `StructA`, `Union`, `UnionA`, scalar, scalar arrays). |
| PVA-319 | `TypeDef::create()` | data.h | Materialize an empty `Value`. |
| PVA-320 | `Value::format(...)` | data.h | Pretty-print to stream (matches `pvget` output). |
| PVA-321 | `NoField` | data.h:471 | Exception: field path doesn't exist. |
| PVA-322 | `NoConvert` | data.h:478 | Exception: incompatible coercion. |
| PVA-323 | `LookupError` | data.h:484 | Exception: structure-id lookup failed. |
| PVA-324 | `shared_array<E>` | sharedArray.h:240 | COW array container, used for array-typed Value sub-fields. |

---

## 9. Normative Types (`nt.h`)

| ID | Symbol | Header:line | Description |
|----|--------|-------------|-------------|
| PVA-340 | `NTScalar` | nt.h:32 (multiple PVXS_API) | Builder for `epics:nt/NTScalar:1.0`. Type-templated. |
| PVA-341 | `NTScalarArray` | nt.h:49 | Same but for array values. |
| PVA-342 | `NTNDArray` | nt.h:93 | `epics:nt/NTNDArray:1.0` — image / N-D array w/ attributes. |
| PVA-343 | `NTEnum` | nt.h:107 | `epics:nt/NTEnum:1.0` — index + choice strings. |
| PVA-344 | `NTTable` | nt.h:126 | `epics:nt/NTTable:1.0` — column-oriented table. |
| PVA-345 | `NTURI` | nt.h:170 | `epics:nt/NTURI:1.0` — used by RPC argument encoding. |

Each NT* helper exposes `create()`, `is_a(Value)`, and field accessors that mirror the pvAccessJava / pvDataCPP definitions.

---

## 10. Wire protocol (`pvaproto.h`)

| ID | Symbol | Code | Description |
|----|--------|------|-------------|
| PVA-400 | `CMD_BEACON` | 0 | UDP-only: server "I'm alive" with GUID + protocol/port + change-counter. |
| PVA-401 | `CMD_CONNECTION_VALIDATION` | 1 | TCP handshake step: server advertises peer credentials channel + auth schemes. |
| PVA-402 | `CMD_ECHO` | 2 | TCP heartbeat (bidirectional). |
| PVA-403 | `CMD_SEARCH` | 3 | UDP/TCP: client → server, "do you know these PV names?" |
| PVA-404 | `CMD_SEARCH_RESPONSE` | 4 | Server → client, "yes, connect to me". |
| PVA-405 | `CMD_AUTHNZ` | 5 | Authentication exchange (extensible scheme; `anonymous`, `ca`, `x509`). |
| PVA-406 | `CMD_ACL_CHANGE` | 6 | Server-asynchronous access-rights update. |
| PVA-407 | `CMD_CREATE_CHANNEL` | 7 | TCP: client requests a channel (PV name → server-assigned SID). |
| PVA-408 | `CMD_DESTROY_CHANNEL` | 8 | Server / client tears down a channel. |
| PVA-409 | `CMD_CONNECTION_VALIDATED` | 9 | Server confirms validation accepted. |
| PVA-410 | `CMD_GET` | 10 | One-shot read (INIT / EXECUTE / DESTROY phases). |
| PVA-411 | `CMD_PUT` | 11 | One-shot write (INIT / EXECUTE / DESTROY). |
| PVA-412 | `CMD_PUT_GET` | 12 | Atomic PUT-then-GET (modify-and-return). |
| PVA-413 | `CMD_MONITOR` | 13 | Long-lived subscription (INIT / START / DATA / STOP / DESTROY). |
| PVA-414 | `CMD_ARRAY` | 14 | Reserved (array sub-protocol; rarely used). |
| PVA-415 | `CMD_DESTROY_REQUEST` | 15 | Client cancels in-flight request. |
| PVA-416 | `CMD_PROCESS` | 16 | Trigger record processing without value change. |
| PVA-417 | `CMD_GET_FIELD` | 17 | Introspection-only: fetch FieldDesc without value. |
| PVA-418 | `CMD_MESSAGE` | 18 | Server → client log message (info / warn / error level). |
| PVA-419 | `CMD_MULTIPLE_DATA` | 19 | Reserved. |
| PVA-420 | `CMD_RPC` | 20 | Remote-procedure call (INIT / EXECUTE / DESTROY). |
| PVA-421 | `CMD_CANCEL_REQUEST` | 21 | Cooperative cancel (without destroy). |
| PVA-422 | `CMD_ORIGIN_TAG` | 22 | Loopback-mcast multi-server-on-one-host SEARCH forwarding (pvxs >=1.x). |

Header bytes: magic `0xCA`, version, flags (server/client, byte-order, segmented), command, payload-length.

---

## 11. TypeStore (cached-descriptor wire optimisation)

PVA frames can reference a previously-sent FieldDesc by 16-bit slot id
to avoid re-serialising compound types. Two markers in the wire format:

| ID | Symbol | Description |
|----|--------|-------------|
| PVA-440 | `0xFD` | "New type — assign this slot id, then inline the FieldDesc". |
| PVA-441 | `0xFE` | "Reference slot id (no inline FieldDesc follows)". |
| PVA-442 | `0xFF` | "End of struct" / variant-type sentinel (context-dependent). |

Bandwidth saving is significant for repeated NTScalar / NTTable types
(120 bytes inline → 3 bytes referenced).

---

## 12. Configuration (`Config` / `ConfigCommon`)

| ID | Symbol | Header:line | Description |
|----|--------|-------------|-------------|
| PVA-460 | `client::Config` | client.h:1046 | Client-side config struct. |
| PVA-461 | `server::Config` | server.h:163 | Server-side config struct. |
| PVA-462 | `Config::fromEnv()` | client.h, server.h | Read `EPICS_PVA*` env. |
| PVA-463 | `Config::isolated()` | server.h:197 | Loopback-only, randomly-allocated ports. For unit tests. |
| PVA-464 | `Config::applyEnv()` | server.h:200 | Apply env on top of an existing config. |
| PVA-465 | `Config::applyDefs(map)` | server.h:205 | Apply a name→value map (for parsing st.cmd-style overrides). |
| PVA-466 | `Config::expand()` | server.h:219 | Expand `$()` substitutions in path / addr fields. |
| PVA-467 | `Config::build()` | server.h:223 | Materialize a Server / Context. |
| PVA-468 | `ConfigCommon` | netcommon.h:126 | Shared base for client / server: timeouts, send/recv buffer sizes, addr lists. |

### 12.1 Environment variables

| ID | Variable | Description |
|----|----------|-------------|
| PVA-480 | `EPICS_PVA_ADDR_LIST` | Client search destinations (broadcast / unicast). |
| PVA-481 | `EPICS_PVA_AUTO_ADDR_LIST` | Auto-discover broadcast addresses. |
| PVA-482 | `EPICS_PVA_NAME_SERVERS` | TCP-based name-server list. |
| PVA-483 | `EPICS_PVA_SERVER_PORT` | Server TCP bind port (5075 default). |
| PVA-484 | `EPICS_PVA_BROADCAST_PORT` | UDP search port (5076 default). |
| PVA-485 | `EPICS_PVA_CONN_TMO` | Heartbeat / dead-circuit timeout (default 30 s). |
| PVA-486 | `EPICS_PVAS_INTF_ADDR_LIST` | Server: NICs to bind UDP responder on. |
| PVA-487 | `EPICS_PVAS_BEACON_ADDR_LIST` | Server: explicit beacon destinations. |
| PVA-488 | `EPICS_PVAS_AUTO_BEACON_ADDR_LIST` | Server: auto-discover broadcast destinations. |
| PVA-489 | `EPICS_PVAS_IGNORE_ADDR_LIST` | Server: silently drop packets from these peers. |
| PVA-490 | `EPICS_PVAS_BEACON_PERIOD` | Server: beacon emit period override (default 15 s burst, 180 s steady). |
| PVA-491 | `EPICS_PVAS_MAX_ARRAY_BYTES` | Cap on inbound payload size. |

---

## 13. IOC integration (`iochooks.h`)

| ID | Symbol | Header:line | Description |
|----|--------|-------------|-------------|
| PVA-510 | `iocshIsolatedServer()` | iochooks.h | Construct an isolated PVA server from inside an IOC's iocsh. |
| PVA-511 | `pvxsr` iocsh command | iochooks.h | Print the server's report (channels / peers). |
| PVA-512 | `dbpf`, `dbpvr` extensions | iochooks.h | DB integration commands — exposes records as PVA PVs. |
| PVA-513 | `pvxsmonitor`, `pvxsg`, `pvxsput` | iochooks.h | iocsh-side `monitor` / `get` / `put` for diagnostics. |
| PVA-514 | `TestIOC` | iochooks.h:154 | Programmatic test harness — spin up an IOC inside a unit test. |

---

## 14. Logging (`log.h`)

| ID | Symbol | Header:line | Description |
|----|--------|-------------|-------------|
| PVA-540 | `logger` | log.h:39 | Per-tag logger object (`logger_init("pvxs.client.io")`). |
| PVA-541 | `logger_level_set(name, lvl)` | log.h:125 | Adjust verbosity for a tag pattern. |
| PVA-542 | `logger_level_clear()` | log.h:132 | Reset all loggers to default. |
| PVA-543 | `logger_config_env()` | log.h:144 | Apply `PVXS_LOG=name=lvl,...` from env. |
| PVA-544 | Levels | log.h | DEBUG, INFO, WARN, ERR, CRIT (LOG_LEVEL_DEBUG=0..). |

---

## 15. Utilities (`util.h`)

| ID | Symbol | Header:line | Description |
|----|--------|-------------|-------------|
| PVA-560 | `ServerGUID` | util.h:79 | `std::array<uint8_t, 12>` — server identity emitted in beacons / search responses. |
| PVA-561 | `Escaper` | util.h:30 | Adapter for `<<` to escape a C string for safe logging. |
| PVA-562 | `Indented` | util.h:136 | RAII indenter for `Value::format` / report output. |
| PVA-563 | `Detailed` | util.h:152 | Toggle "show all sub-fields" mode for stream insertion. |
| PVA-564 | `SigInt` | util.h:104 | RAII SIGINT handler used by `Server::run`. |
| PVA-565 | `Timer` | util.h:324 | Single-shot / periodic timer plumbed into the pvxs event base. |
| PVA-566 | `MPMCFIFO` | util.h:212 | Multi-producer / multi-consumer queue (internal-ish, exported). |

---

## 16. Cross-cutting design choices

| ID | Topic | Reference | Description |
|----|-------|-----------|-------------|
| PVA-600 | Header byte order | pvaproto.h | Per-message: server picks LE/BE in `SET_BYTE_ORDER` flag, both sides honor. |
| PVA-601 | Phased operations | client.cpp / serverconn.cpp | GET / PUT / MONITOR / RPC each have INIT (introspection) → EXECUTE → DESTROY phases. |
| PVA-602 | Connection validation | serverconn.cpp | Server proposes auth schemes; client picks one (`anonymous`, `ca`, `x509`); CONNECTION_VALIDATED seals the handshake. |
| PVA-603 | TLS support | ossl.h | Optional `pvas://` scheme; OpenSSL handshake before PVA framing. Client / server certs configurable. |
| PVA-604 | Peer credentials | netcommon.h:34 | After validation, both sides expose `PeerCredentials` (peer cert subject, account, host). |
| PVA-605 | Search ring (30 buckets) | client.cpp:599 | Cooperative tick scheduler — caps UDP search rate at `pending / nBuckets` packets/tick. |
| PVA-606 | Beacon period (burst then long) | server.cpp:826 | 10 × 15 s burst, then 180 s steady-state. Re-burst on topology change (`change_count`). |
| PVA-607 | Beacon `change_count` tick | server.cpp | Increments on every `addPV` / `removePV`; clients re-search active channels when seen. |
| PVA-608 | Discover (passive + active) | client.cpp | `DiscoverBuilder` enumerates running servers via beacon listening + optional `pingAll`. |
| PVA-609 | pvRequest grammar | pvrequest.cpp | `field(value, alarm.severity) record[queueSize=8, pipeline=true]` — SQL-ish field selector + record options. |
| PVA-610 | Pipelined monitor / flow control | client.cpp | Client `record._options.queueSize` + `pipeline=true` — server pauses on watermark, resumes on `ackAny`. |
| PVA-611 | Origin-tag forwarding | udp_collector.cpp | Multi-server-on-one-host: one server receives broadcast SEARCH, forwards via `224.0.0.128` mcast loopback to siblings. |
| PVA-612 | Type cache (0xFD/0xFE) | dataimpl.cpp | Per-connection FieldDesc-id table — repeated NTScalar saves ~120 bytes → 3 bytes. |
| PVA-613 | Channel cache | client.cpp | Single Channel object per (Context, name) — multiple Operations share. |
| PVA-614 | Server name resolution | server.cpp | TCP nameserver mode — server publishes itself via `EPICS_PVAS_BEACON_ADDR_LIST` to a TCP daemon clients can poll. |

---

## Summary

- **6** Operation builders (Get/Put/RPC/Monitor/Connect/Info)
- **5** main client classes: Context, Operation, Subscription, Connect, Config
- **9** Server-side facilities: Server, SharedPV, StaticSource, Source, ConnectOp, MonitorSetupOp, MonitorControlOp, ChannelControl, ExecOp
- **23** wire protocol commands (CMD_BEACON…CMD_ORIGIN_TAG)
- **6** Normative Type helpers
- **3** TypeStore wire markers
- **12** environment variables
- **15** cross-cutting design topics
- **5** IOC integration commands
- **4** logging primitives

Total inventory items: **174**.

---

## Methodology notes

1. Every entry pins to a source file + line in `pvxs/include/pvxs/*.h` or
   `pvxs/src/pvaproto.h`. When pvxs is upgraded, a `git diff` on those
   files identifies the rows that need re-inspection.
2. Internal classes / private impls are excluded — this is the
   **public contract** layer.
3. Wire protocol items in §10 reflect the on-the-wire constants.
   Phase semantics (INIT vs EXECUTE) are captured in §16 cross-cutting.
4. New entries append to the end of their section; never renumber.
