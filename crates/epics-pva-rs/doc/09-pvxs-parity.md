# 09 — pvxs API parity

This page tracks the public surface of `epics-pva-rs` against the
upstream pvxs C++ classes it mirrors. Keep it in sync when adding
public methods.

Conventions:
- ✅ matches pvxs at the method level
- ⚠️ partial (subset of pvxs functionality, or different signature)
- ❌ not implemented yet
- — N/A (pvxs concept that doesn't translate to Rust idioms)

## `pvxs::client::Context` ↔ `pva-rs::client::PvaClient`

| pvxs | pva-rs | Status |
|------|--------|--------|
| `Context::Context(Config)` | `PvaClient::builder().build()` | ✅ |
| `Context::config()` | `PvaClientBuilder` getters | ✅ |
| `Context::close()` | `close()` | ✅ |
| `Context::hurryUp()` | `hurry_up()` | ✅ |
| `Context::cacheClear(name)` | `cache_clear(name)` | ✅ |
| `Context::ignoreServerGUIDs(...)` | `ignore_server_guids(...)` | ✅ |
| `Context::nameServers(...)` | `PvaClientBuilder::name_servers(...)` | ✅ |
| `Context::report(level)` | `report()` | ⚠️ summary only |
| `Context::get(name).exec()` | `pvget(name)` / `pvget_full` / `pvget_fields` | ✅ |
| `Context::get(name).server(addr).exec()` | `pvget_from(name, addr)` | ✅ |
| `Context::put(name).set("k",v).exec()` | `pvput(name, value_str)` | ⚠️ string-form only |
| `Context::put(name).server(addr).exec()` | `pvput_to(name, addr, ...)` | ✅ |
| `Context::monitor(name).exec()` | `pvmonitor(...)` / `pvmonitor_typed` / `pvmonitor_handle` | ✅ |
| `Context::monitor(name).server(addr).exec()` | `pvmonitor_handle_from(name, addr, cb)` | ✅ |
| `MonitorBuilder::maskConnected(b)` | `MonitorEventMask::mask_connected` | ✅ |
| `MonitorBuilder::maskDisconnected(b)` | `MonitorEventMask::mask_disconnected` | ✅ |
| `Context::rpc(name, args).exec()` | `pvrpc(name, args)` (when `rpc` enabled) | ✅ |
| `Context::discover()` | `PvaClient::discover()` (via SearchEngine) | ✅ |
| `DiscoverBuilder::pingAll(true)` | `ping_all()` | ✅ |
| `Subscription::pause()` / `resume()` | `SubscriptionHandle::pause()` / `resume()` | ✅ |
| `Subscription::stop(syncCancel=true)` | `SubscriptionHandle::stop_sync()` | ✅ |
| `Subscription::stats()` | `SubscriptionHandle::stats()` | ✅ |
| `Channel::operations()` (live op enumeration) | — | ❌ (rare; not on roadmap) |

## `pvxs::server::Server` ↔ `pva-rs::server_native::PvaServer`

| pvxs | pva-rs | Status |
|------|--------|--------|
| `Server(Config)` | `PvaServer::start(source, cfg)` | ✅ |
| `Config::isolated()` | `PvaServerConfig::isolated()` | ✅ |
| `Server::isolated(source)` | `PvaServer::isolated(source)` | ✅ |
| `Server::clientConfig()` | `PvaServer::client_config()` | ✅ |
| `Server::config()` | `PvaServer::config()` | ✅ |
| `Server::run()` | `PvaServer::run()` | ✅ |
| `Server::interrupt()` | `PvaServer::interrupt()` | ✅ |
| `Server::stop()` | `PvaServer::stop()` | ✅ |
| `Server::report()` | `PvaServer::report()` | ⚠️ summary level |
| `Server::start()` / `stop()` granular control | — | ⚠️ stop() exits accept loop only |
| `auth_complete` post-validation hook | `PvaServerConfig::auth_complete` | ✅ |
| `ignore_addrs` ACL | `PvaServerConfig::ignore_addrs` | ✅ |
| `monitor_*_watermark` knobs | `PvaServerConfig::monitor_queue_depth` / `monitor_high_watermark` | ✅ |

## `pvxs::server::SharedPV` ↔ `pva-rs::server_native::SharedPV`

| pvxs | pva-rs | Status |
|------|--------|--------|
| `SharedPV()` (bare) | `SharedPV::new()` | ⚠️ default no-handler PUT differs (see note) |
| `SharedPV::buildMailbox()` | `SharedPV::new()` (default behaviour) | ✅ |
| `SharedPV::buildReadonly()` | `new()` + `on_put` returning an error | ⚠️ no built-in ctor |
| `open(...)` / `close()` | `open` / `close` | ✅ |
| `post(value)` | `try_post(value)` / `force_post(value)` | ✅ |
| `current()` | `current()` | ✅ |
| `onPut(handler)` | `on_put(...)` | ✅ |
| `onRPC(handler)` | `on_rpc(...)` | ✅ |
| `onFirstConnect(handler)` | `on_first_connect(...)` | ✅ |
| `onLastDisconnect(handler)` | `on_last_disconnect(...)` | ✅ |
| `attach()` / `fetch()` / `prune_subscribers()` | `attach()` / `fetch()` / `prune_subscribers()` | ✅ |

> **Deviation — default no-handler PUT semantics.** `SharedPV::new()`
> defaults to *mailbox* behaviour: a PUT received with no `on_put` handler
> installed stores the value, posts it to subscribers, and replies success
> (`shared_pv.rs:480-490`; the BitSet-delta path `put_delta` behaves the
> same, `:555-579`). This matches pvxs `SharedPV::buildMailbox()`
> (`sharedpv.cpp:106-133`), **not** a bare pvxs `SharedPV()`, whose default
> `onPut` is empty and rejects the PUT with `"RPC not implemented by this
> PV"` (`sharedpv.cpp:209-227`). The pva-rs default is intentional — the
> common server-side use is a writable mailbox — and installing
> `on_put(...)` overrides it exactly as pvxs `onPut(...)` does (return an
> error from the handler for reject-by-default or read-only, the analog of
> pvxs `buildReadonly()`). pva-rs has no separate reject-by-default
> constructor.

## `pvxs::Value` ↔ `pva-rs::pvdata::Value`

| pvxs | pva-rs | Status |
|------|--------|--------|
| `Value::cloneEmpty()` | `Value::clone_empty()` | ✅ |
| `Value::copyIn` / `copyOut` | `copy_in` / `copy_out` | ✅ |
| `Value::tryCopyIn` / `tryCopyOut` | `try_copy_in` / `try_copy_out` | ✅ |
| `Value::operator=` | `Value::set` / `set_with_coercion` | ✅ |
| `TypeDef` builder | `pva-rs::pvdata::TypeDef` + `Member` | ✅ |
| `Value::iterAll` / `iterChildren` / `iterMarked` | `iter_all` / `iter_children` / `iter_marked` | ✅ |

## `pvxs::nt` ↔ `pva-rs::nt`

| pvxs | pva-rs | Status |
|------|--------|--------|
| `nt::NTScalar` | `nt::scalar::NTScalar` | ✅ |
| `nt::NTTable` | `nt::table::NTTable` | ✅ |
| `nt::NTURI` | `nt::uri::NTURI` | ✅ |
| `nt::NTEnum` | `nt::enum::NTEnum` | ⚠️ partial — basic shape only |
| `nt::NTNDArray` | — | ❌ (pvxs imageJ helper; not on roadmap) |

## tools / CLIs

| pvxs binary | pva-rs binary | Status |
|-------------|---------------|--------|
| `pvxs/pvget` | `pvget-rs` | ✅ |
| `pvxs/pvput` | `pvput-rs` | ✅ |
| `pvxs/pvmonitor` | `pvmonitor-rs` | ✅ |
| `pvxs/pvinfo` | `pvinfo-rs` | ✅ |
| `pvxs/pvcall` | `pvcall-rs` | ✅ |
| `pvxs/pvlist` | `pvlist-rs` | ✅ |
| `pvxs/pvxvct` | `pvxvct-rs` | ✅ |
| `pvxs/mshim` | `mshim-rs` | ✅ |

## Gateway

| upstream | pva-rs | Status |
|----------|--------|--------|
| `pva2pva/p2pApp` (legacy gw) | `epics-bridge-rs::pva_gateway` | ⚠️ initial impl — see `11-gateway.md` for the gap list |
| `pvxs/gateway` (newer, in progress upstream) | — | ❌ |
