# QSRV + pvalink on the RTEMS execution model

**Status:** design; **stages 1 and 2 implemented and stage 2 verified on
the target** (see §7, §9.11 for the measured boot, and the rest of §9 for
every place reality deviated from what the probes predicted — in
particular §9.7, where stage 2 found §3.3's mount point unbuildable).
Stages 3–5 unimplemented.
**Base:** `integration/rtems-scope-b` tip `9965bbd6`, plus the landed
`pi-h7-h9-l33` round (`e8b6cd50` / `6db95afc` / `5d46ce3b`) — every count
below that touches `qsrv/group.rs` or `pvalink/integration.rs` is stated
against the **post-round** shape and marked POST-ROUND where it differs.
**C reference:** pvxs at `/home/stevek/work/epics-modules/pvxs`
(paths below are relative to that root).

---

## 0. What this is for, and the one measurement that reframes it

The RTEMS target IOC must serve `Q:group` PVs. Today `epics-bridge-rs` is
outside the RTEMS closure entirely: it is absent from
`scripts/rtems-check.sh`'s `CRATES`, and the crate carries **zero**
RTEMS predicates (measured: `rg -n 'target_os = "rtems"|rtems-exec-model|
rtems_boot_linked|exec_backend' crates/epics-bridge-rs` returns nothing;
the only `rtems` string anywhere in the crate is a doc-comment
cross-reference at `qsrv/group.rs:861`). `rtems-pva-ioc`
serves single-record PVs through `PvDatabaseSource`
(`crates/epics-pva-rs/src/bin/rtems-pva-ioc.rs`, step 3).

The obvious reading of "21k lines of qsrv, 151 tokio references, 189
async fns" is that this is a large port. **It is not.** Measured, by
running the real gate invocation against the real target triple:

| probe | configuration | result |
|---|---|---|
| A | `--no-default-features --features qsrv`, `epics-pva-rs`/`epics-ca-rs` default-features off, tokio split per-target | **5 errors**, in 2 files |
| B | probe A, minus `pvalink` from the `qsrv` feature list | **2 errors**, both in one function |
| C | probe B, plus `#[cfg(not(target_os = "rtems"))]` on `run_ca_pva_qsrv_ioc` and its re-export | **0 errors**, 3 warnings |
| C-image | probe C with `RUSTFLAGS=--cfg rtems_boot_linked` | **0 errors** |
| D | `-p epics-pva-rs --no-default-features --features client` | **47 errors** |
| E | probe C **without** the tokio per-target split (`features = ["full"]` left unconditional) | **58 errors** in `mio` / `socket2` / `signal-hook-registry` |

Command for A–C:

```
cargo +nightly check --no-default-features --features qsrv \
  -Zbuild-std=std,panic_abort --target armv7-rtems-eabihf \
  -p epics-bridge-rs --lib
```

The probes were made with a temporary manifest/source edit that was
**reverted**; the working tree is clean at the commit that carries this
document (`git status --porcelain` empty after restore). Nothing in
`crates/` changed.

So the shape of the work is:

* **QSRV itself type-checks for `armv7-rtems-eabihf` as it stands.**
  `group.rs`, `group_config.rs`, `pvif.rs`, `provider.rs`, `channel.rs`,
  `iocsh.rs`, `monitor.rs`, `trap_write.rs` — 17,842 of qsrv's 21,013
  lines — produce not one error, and neither does the rest of
  `pva_adapter.rs` outside one ~135-line function. The blockers are
  three manifest lines and that function.
* **pvalink is blocked behind a project that does not exist.** It needs
  `epics_pva_rs::client` / `client_native`, and that module tree does not
  compile for RTEMS: 47 errors, 20 of them in `client_native/udp.rs` and
  11 in `client_native/search_engine.rs` — `tokio::net`, `socket2`,
  `if-addrs`, `epics_base_rs::net::AsyncUdpV4`. `client_native` is
  23,881 lines. A blocking PVA **client** driver is a peer of the
  blocking PVA **server** driver, not a sub-task of this one.

That asymmetry is the single most important input to §3 and §7: QSRV
group serving can land on the target in one short stage; pvalink cannot
land at all until someone writes a sans-io PVA client.

Type-checking is not running. Everything §8 lists is still unmeasured on
hardware.

---

## 1. Inventory

### 1.1 Method

Every number below is a `rg` count over the file split at its **first
column-0 `#[cfg(test)]`**, so "prod" excludes in-file test modules. That
split matters more here than anywhere else in the workspace: **all**
152 (POST-ROUND 153) `#[tokio::test]` sites in `qsrv`+`pvalink` fall
below that line — the production splits contain zero — so counting the
raw whole-file figures as production work overstates the port by roughly
threefold.

Raw whole-file counts (the "~151 / ~140 tokio refs" figure in the brief)
are reproduced for cross-reference, but they are not the work list.

### 1.2 Size and role — the five biggest files

| file | lines | role |
|---|---:|---|
| `qsrv/group.rs` | 5,641 (POST-ROUND 5,781) | `GroupChannel` / `GroupMonitor` — the multi-record composite PV. C++ QSRV `PDBGroupPV`/`PDBGroupChannel`/`PDBGroupMonitor`. Per-member `DbSubscription` fan-in, atomic GET/PUT under `lock_records`, `+trigger` mark resolution. **This is the file that "serving Q:group on RTEMS" means.** |
| `pvalink/integration.rs` | 4,396 | `PvaLinkResolver` — wires pvalink into `PvDatabase::set_external_resolver`, owns the scan-on-update (`CP`/`CPP`) forwarder and the atomic scan epoch. Holds a `tokio::runtime::Handle` (see §1.4). pvxs `ioc/pvalink.cpp` + `ioc/pvalink_channel.cpp`. |
| `pvalink/link.rs` | 3,996 | `PvaLink` — one live PVA link. Owns the re-subscribe loop, the OUT staging map, disconnect/alarm bookkeeping. **The file that imports the PVA client.** |
| `qsrv/pva_adapter.rs` | 3,018 | `QsrvPvStore` — the adapter that exposes `BridgeProvider` through `epics_pva_rs::server_native::ChannelSource` (impl at `:444`). Also holds `run_ca_pva_qsrv_ioc`, the host dual-protocol runner, and the pvxs `enable2()` port. **The mount point for §3.** |
| `qsrv/group_config.rs` | 2,796 | `Q:group` JSON parser (`GroupPvDef`), C++ QSRV `configparse.cpp`. Carries L33 (`atomic_write_lock`) at `:64`. |

(`qsrv/pvif.rs`, 2,680 lines — record↔pvData conversion — has **zero**
tokio references, zero `async fn` and zero `.await` in production. It is
pure translation and is listed here only because its size otherwise
invites the assumption that it is part of the problem.)

### 1.3 Per-file tokio dependency census — production code only

`spawn` counts every `spawn`-shaped call site (including
`handle.spawn(..)`, which a `tokio::spawn|task::spawn` regex misses —
that omission is why the four `integration.rs` spawns are easy to
undercount). `sync` counts declared lock/notify types of **any**
provenance; the tokio-only subset is §5.

| file | prod lines | spawn | timer | sync decls | channels | `select!` | `async fn` | `.await` |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `qsrv/group.rs` | 2,918 | 2 | 0 | 5 | 3 | 0 | 27 (POST-ROUND **23**) | 55 (POST-ROUND **50**) |
| `qsrv/pva_adapter.rs` | 1,515 | 3 (+1 already on the seam) | 0 | 6 | 10 | 3 | 8 | 70 |
| `qsrv/provider.rs` | 1,487 | 0 | 0 | 9 | 0 | 0 | 45 | 51 |
| `qsrv/pvif.rs` | 1,519 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| `qsrv/group_config.rs` | 1,152 | 0 | 0 | 2 | 0 | 0 | 0 | 0 |
| `qsrv/channel.rs` | 1,024 | 0 | 0 | 3 | 0 | 0 | 8 | 15 |
| `qsrv/iocsh.rs` | 997 | 0 | 0 | 0 | 0 | 0 | 0 | 2 |
| `qsrv/monitor.rs` | 391 | 0 | 0 | 0 | 0 | 1 | 3 | 4 |
| `qsrv/trap_write.rs` | 125 | 0 | 0 | 0 | 0 | 0 | 1 | 3 |
| `qsrv/put_status.rs` | 101 | 0 | 0 | 0 | 0 | 0 | 1 | 1 |
| `qsrv/mod.rs` | 52 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| **qsrv total** | **11,281** | **5 (+1)** | **0** | **25** | **13** | **4** | **93 → 89** | **201 → 196** |
| `pvalink/integration.rs` | 2,064 | 4 | 1 | 12 | 1 | 0 | 21 (POST-ROUND **20**) | 48 (POST-ROUND **47**) |
| `pvalink/link.rs` | 2,016 | 2 | 2 | 9 | 5 | 0 | 15 | 25 |
| `pvalink/config.rs` | 698 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| `pvalink/registry.rs` | 444 | 0 | 0 | 11 | 0 | 0 | 1 | 5 |
| `pvalink/iocsh.rs` | 405 | 1 (`std::thread`) | 0 | 0 | 0 | 0 | 0 | 1 |
| `pvalink/mod.rs` | 33 | 0 | 0 | 0 | 0 | 0 | 0 | 2 |
| **pvalink total** | **5,660** | **7** | **3** | **32** | **6** | **0** | **37 → 36** | **81 → 80** |

Whole-file (production + test) reference figures, for cross-checking
against the brief: qsrv 152 lines containing `tokio`, 189 `async fn`
occurrences; pvalink 143 and 106. Total `.await`-bearing test sites:
`#[tokio::test]` × 84 in `qsrv` (POST-ROUND 85), × 68 in `pvalink` — see
§6.

### 1.4 The production work list, enumerated

This is the complete set of production sites that a `runtime::task`
seam conversion has to touch. Twelve spawns, three timers, four
`select!`s, one runtime `Handle`.

**Task spawns (11 to convert, 1 already converted):**

| site | what it drives |
|---|---|
| `qsrv/group.rs:2496` | per-member **value** `DbSubscription` → fan-in mpsc |
| `qsrv/group.rs:2532` | per-member **PROPERTY** `DbSubscription` → fan-in mpsc |
| `qsrv/pva_adapter.rs:404` | `spawn_db_monitor_updates` — cooked `MonitorUpdate` forwarder (the marked-set path) |
| `qsrv/pva_adapter.rs:838` | legacy `subscribe_checked` forwarder |
| `qsrv/pva_adapter.rs:1134` | legacy ctx-less `subscribe` forwarder |
| `qsrv/pva_adapter.rs:1436` | **already** `epics_base_rs::runtime::task::spawn` — the CA server task inside `run_ca_pva_qsrv_ioc` |
| `pvalink/integration.rs:419` | `run_notify_forwarder` — scan-on-update fan-out |
| `pvalink/integration.rs:804` | scan-target processing |
| `pvalink/integration.rs:1370`, `:1379` | resolver-side deferred work |
| `pvalink/link.rs:341` | INP monitor re-subscribe loop |
| `pvalink/link.rs:463` | OUT connection-tracking monitor loop |
| `pvalink/iocsh.rs:43` | `std::thread::spawn` + `handle.block_on` for the `pvxr` shell command — **host-only**, there is no iocsh on the target |

Measured: the whole crate contains exactly **one**
`epics_base_rs::runtime::task` reference (`pva_adapter.rs:1436`). The
brief's "8 runtime::task seam uses" is not reproducible at `9965bbd6`;
`rg -n 'epics_base_rs::runtime::task' crates/epics-bridge-rs/src` returns
one line, and the broader `rg -n 'epics_base_rs::runtime::[a-z_]+'`
returns six (task ×1, supervise ×2, net ×2, env ×2, log ×1 — the
non-`task` ones all in `ca_gateway`, which stays excluded).

**Timers (3):**

| site | what |
|---|---|
| `pvalink/link.rs:427` | INP re-subscribe exponential backoff, 250 ms → 30 s |
| `pvalink/link.rs:498` | OUT re-subscribe backoff, same ladder |
| `pvalink/integration.rs:733` | 50 ms poll inside a deadline loop (`Instant::now()` at `:725`, `:730`) |

Zero timers in qsrv production code.

**`select!` (4):** `qsrv/monitor.rs:333` (value-sub vs property-sub race),
`qsrv/pva_adapter.rs:415`, `:843`, `:1141` (each: `tx.closed()` vs
`monitor.poll()`, so a client cancel tears down a parked monitor). All
four are pure future combinators over runtime-agnostic primitives — see
§4, they need **no** conversion.

**Runtime handle (1, structural):** `pvalink/integration.rs:38` —
`PvaLinkResolver.handle: tokio::runtime::Handle`, constructed at `:158`
and `:826`, used at the four `handle.spawn` sites. `Handle::current()`
panics with no runtime entered, which is precisely the RTEMS entry
point's state. This is the one item on the list that is a redesign
rather than a call swap.

**Channels:** all `tokio::sync::mpsc`. Runtime-agnostic; see §4.

---

## 2. The crate split — compiling qsrv+pvalink without the gateways

### 2.1 Module gating: already correct, nothing to do

`crates/epics-bridge-rs/src/lib.rs` already gates every bridge behind its
own feature:

```
lib.rs:69   #[cfg(feature = "qsrv")]        pub mod qsrv;
lib.rs:72   #[cfg(feature = "ca-gateway")]  pub mod ca_gateway;
lib.rs:75   #[cfg(feature = "pvalink")]     pub mod pvalink;
lib.rs:82   #[cfg(feature = "pva-gateway")] pub mod pva_gateway;
```

So `--no-default-features --features qsrv` compiles neither gateway —
measured (probes A–C reached errors *only* in `qsrv`/`pvalink` files).
**Feature flags, not module `cfg`**: the split already exists and is
already the right one, and adding a second `#[cfg(not(target_os =
"rtems"))]` layer over the gateway modules would be a redundant guard
whose only effect is a second place for the rule to drift. The gateways
stay host-only by *not being selected*, which is what the RTEMS gate
invocation already does (`scripts/rtems-check.sh`'s `COMMON` carries
`--no-default-features`).

### 2.2 Three manifest defects, all measured

**(a) `epics-pva-rs` default features drag `ring` into the RTEMS build —
hard fail.**

```
crates/epics-bridge-rs/Cargo.toml:102
    epics-pva-rs = { workspace = true, optional = true }
```

`workspace = true` inherits the workspace table's `default-features`
(unset → true), so `tls` is on, so `rustls` → `ring` is in the graph, and
`ring`'s build script runs `cc` for the target:

```
error: failed to run custom build command for `ring v0.17.14`
  error occurred in cc-rs: failed to find tool "arm-rtems6-gcc"
```

This is the same `tls`/`ring`/`getrandom 0.2` trap
`scripts/rtems-check.sh`'s own comment documents for `epics-pva-rs`
("`--no-default-features` … is load-bearing for a different reason that
must not be undone"). The bridge inherits it through the workspace table
and `--no-default-features` on the *bridge* does not reach it.

The fix is not a one-word edit: cargo **rejects** `workspace = true`
combined with `default-features = false` —

```
error inheriting `epics-ca-rs` from workspace root manifest's
  `workspace.dependencies.epics-ca-rs`
Caused by: `default-features = false` cannot override workspace's
  `default-features`
```

so the entry must be spelled out (`path` + `version`), which is what the
probe did:

```
epics-pva-rs = { path = "../epics-pva-rs", version = "0.24.0",
                 optional = true, default-features = false }
epics-ca-rs  = { path = "../epics-ca-rs",  version = "0.24.0",
                 optional = true, default-features = false }
```

Host builds then need the dropped features restored through bridge
features (`qsrv-bin`/`dual-ioc`/gateways gain
`epics-pva-rs/client`, `epics-pva-rs/tls`, `epics-ca-rs/<...>` as
appropriate). Alternative considered and rejected: setting
`default-features = false` in the **workspace** table. That changes the
resolved feature set for every other member in one edit and would be a
much larger blast radius than the problem justifies.

**(b) `tokio = { features = ["full"] }` is unconditional.**

```
crates/epics-bridge-rs/Cargo.toml:93
    tokio = { version = "1", features = ["full"] }
```

Measured (probe E): with everything else at probe C's zero-error state
and only this left unsplit, the build produces **58 errors** — 31 in
`mio` (`error[E0583]: file not found for module `selector``,
`... module `waker``, `unresolved import `crate::sys::IoSourceState``),
the rest in `socket2` (`unresolved import `libc::SOCK_RAW``) and
`signal-hook-registry`. `full` includes `net` and `signal`; `net` pulls
`mio`, which has no selector for RTEMS.
`epics-base-rs` (`Cargo.toml:55-70`) and `epics-pva-rs`
(`Cargo.toml:150-173`) both solve this by splitting the dependency into
two `[target.'cfg(...)'.dependencies]` tables, and both carry the comment
explaining why a shared table cannot be used (**cargo unions a
dependency's features across every matching table**, so a shared `full`
silently re-adds what the RTEMS table drops). The bridge needs the same
split, with the same retained set:

```
[target.'cfg(not(target_os = "rtems"))'.dependencies]
tokio = { version = "1", features = ["full"] }

[target.'cfg(target_os = "rtems")'.dependencies]
tokio = { version = "1", default-features = false, features = [
    "fs", "io-util", "io-std", "macros", "parking_lot",
    "rt", "rt-multi-thread", "sync", "time" ] }
```

`macros` is load-bearing, not cargo-culted: the four `select!` sites
(§1.4) are `tokio::select!`. `sync` is what makes every channel and lock
in §5 legal on the target.

**(c) `qsrv` implies `pvalink`, and pvalink cannot build for RTEMS.**

```
crates/epics-bridge-rs/Cargo.toml:28-34
    qsrv = [ "pvalink", "dep:epics-pva-rs", "dep:epics-ca-rs",
             "dep:serde", "dep:serde_json" ]
```

The comment there is right about *why* — pvxs ties pvalink to QSRV2, both
enabled together inside `if(enableQ)` (`ioc/iochooks.cpp:493-495`:
`single_enable(); group_enable(); pvalink_enable();`). But on this target
the implication is unsatisfiable: probe A shows the three unresolved
imports, probe D shows the client behind them is 47 errors from
compiling.

The structural answer is **not** to delete the implication (that would
silently drop pvalink from every host build that asks for `qsrv`, a
behaviour change nobody asked for). It is to make the tie
target-conditional, so the *default* stays "qsrv brings pvalink" and only
RTEMS gets the reduced set. Two shapes, in preference order:

1. **A `qsrv-core` / `qsrv` pair.** `qsrv-core` = the serving path
   (`dep:epics-pva-rs`, `dep:serde`, `dep:serde_json`, no `pvalink`, no
   `dep:epics-ca-rs`); `qsrv = ["qsrv-core", "pvalink",
   "dep:epics-ca-rs", "epics-pva-rs/client", "epics-pva-rs/tls"]`. The
   RTEMS gate selects `qsrv-core`. This keeps every existing host
   selection byte-identical and makes the target's reduced set a named,
   testable thing rather than an absence.
2. A `pvalink` feature that is a no-op on RTEMS. Rejected: it makes
   `#[cfg(feature = "pvalink")]` mean two different things depending on
   target, which is exactly the dual meaning that produces the next round
   of edge cases.

### 2.3 Bins

Five `[[bin]]` targets, all `required-features`-gated, none reachable
under `--features qsrv-core`:

| bin | required-features | RTEMS |
|---|---|---|
| `ca-gateway-rs` | `ca-gateway-bin` | host-only |
| `qsrv-rs` | `qsrv-bin` | host-only (needs `clap`, `tracing-subscriber`, and `run_ca_pva_qsrv_ioc`) |
| `dual-ioc-rs` | `dual-ioc-bin` | host-only |
| `pva-gateway-rs` | `pva-gateway-bin` | host-only |
| `dual-gateway-rs` | `dual-gateway-bin` | host-only |

All five go in `HOST_ONLY` in `scripts/rtems-check.sh` — see §6, where
the census requirement makes "in neither list" impossible.

### 2.4 Dev-dependencies

`serial_test = "3"`, `tempfile = "3"`. Neither is reachable from
`--lib`, which is the only thing the RTEMS gate compiles, so neither
needs a target split. Worth stating explicitly because
`epics-base-rs`'s dev-dep block *does* carry an RTEMS-relevant entry
(`rtems-exec-gate`), and §6 adds one here for the same reason.

### 2.5 The one production function that must be gated

`qsrv/pva_adapter.rs:1328` `run_ca_pva_qsrv_ioc` — the host
dual-protocol runner. Measured, it is the *entire* remainder of probe B:

```
error[E0433]: cannot find `CaServer` in `server`
   --> qsrv/pva_adapter.rs:1427   (epics-ca-rs/src/server/mod.rs:30-31
                                   gates it on cfg(not(target_os="rtems")))
error[E0433]: cannot find `PvaServer` in `server`
   --> qsrv/pva_adapter.rs:1443   (epics-pva-rs/src/server/mod.rs:15-16
                                   gates it the same way)
```

Both callees are already RTEMS-gated **in their own crates**; the bridge
is simply the last caller that has not followed. Gating the function and
its `qsrv/mod.rs:48-51` re-export takes probe C to zero errors.

It also produces three dead-code warnings that are a design signal, not
noise:

```
warning: fields `error` and `info` are never read   (pva_adapter.rs:1168, Qsrv2Decision)
warning: function `qsrv2_enabled` is never used     (pva_adapter.rs:1255)
warning: function `load_qsrv_groups` is never used  (pva_adapter.rs:1481)
```

Those three are the port of pvxs `enable2()`
(`ioc/iochooks.cpp:401-448`) and `processGroups()`
(`ioc/groupsourcehooks.cpp:192-213`). They are dead on RTEMS **only
because their sole caller was the host runner** — the target mount in §3
must call them, or the target IOC would serve groups without ever having
consulted `PVXS_QSRV_ENABLE` or finalized the group set. Suppressing the
warnings with `#[allow(dead_code)]` would hide exactly the thing that
needs building.

---

## 3. The serving path

### 3.1 What qsrv already implements

`QsrvPvStore` implements `epics_pva_rs::server_native::ChannelSource`
directly:

```
crates/epics-bridge-rs/src/qsrv/pva_adapter.rs:444
    impl epics_pva_rs::server_native::ChannelSource for QsrvPvStore { … }
```

That trait (`server_native/source.rs:510`) is the one *every* native PVA
server in this workspace is generic over. Its methods return
`impl Future` rather than being `async fn`, and object safety is supplied
by a blanket impl:

```
crates/epics-pva-rs/src/server_native/source.rs:2361
    impl<T: ChannelSource + 'static> ChannelSourceObj for T { … }
crates/epics-pva-rs/src/server_native/source.rs:2090
    pub type DynSource = Arc<dyn ChannelSourceObj>;
```

So `Arc<QsrvPvStore>` **is** a `DynSource` with no adapter, no boxing and
no new trait.

`QsrvPvStore` serves single-record *and* group PVs through one
`BridgeProvider` — where pvxs splits them into two sources,
`addSource("qsrvSingle", …, 0)` (`ioc/singlesourcehooks.cpp:159`) and
`addSource("qsrvGroup", …, 1)` (`ioc/groupsourcehooks.cpp:219`). That
difference is pre-existing and orthogonal to RTEMS; it is noted here only
so §3.3's mount is not mistaken for a place where the split should be
introduced.

### 3.2 What `BlockingPvaServer` accepts

```
crates/epics-pva-rs/src/server_native/blocking.rs:1072-1076
    pub fn bind<A: ToSocketAddrs>(
        addr: A,
        source: DynSource,
        config: PvaServerConfig,
    ) -> PvaResult<Self>
```

`DynSource` — the same type. `rtems-pva-ioc` passes
`Arc::new(PvDatabaseSource::new(db))` there today; passing
`Arc::new(QsrvPvStore::new(provider))` or an `Arc<CompositeSource>`
carrying both is a change of one expression, not of a signature.

The decisive detail is how the blocking driver runs a source:

```
crates/epics-pva-rs/src/server_native/blocking.rs:976
    let outcome = block_on_sync(handle_connection_io(source, …));
```

`block_on_sync` on a plain thread with no runtime entered selects
`park_on` (`epics-base-rs/src/runtime/task.rs:114-122`), which drives the
future by polling and `std::thread::park`ing between polls
(`task.rs:68-84`). **The whole async `ChannelSource` surface — all 89
production `async fn`s and 196 `.await`s in qsrv — runs unmodified under
that driver**, provided it only awaits runtime-agnostic primitives.
`park_on`'s own contract states the condition: "The future must only
await runtime-agnostic primitives (`tokio::sync` locks/channels/
notifies): nothing here drives a reactor or a timer wheel."

qsrv satisfies it everywhere except the five spawns of §1.4, which need
a runtime to *create* a task and would panic on the target. It has zero
production timer sites, so the timer-wheel half of the condition is
already met.

### 3.3 Recommendation: grow `rtems-pva-ioc` an optional qsrv mount

**Recommended.** Not a new binary.

Reasons, in order of weight:

1. **The census in `scripts/rtems-check.sh` is per-binary, and every
   binary costs a full `-Zbuild-std` compile in two configurations.** The
   gate already runs `CRATES × CONFIGS + BINS × CONFIGS`; a third target
   binary adds two more full builds to every run of the portability gate
   for a binary that is 95% a copy of the one next to it. The gate's own
   header explains that it exists because coverage that is expensive gets
   skipped.
2. **Two near-identical entry points is how the `--lib`/`src/bin` defect
   repeated.** `rtems-pva-ioc.rs` carries four source-text guards
   (`entry_point_never_starts_a_runtime`,
   `every_thread_here_states_a_stack_size`,
   `a_udp_bind_failure_does_not_stop_the_server`,
   `the_entry_point_publishes_its_status`). Each is `include_str!` over
   *its own file*. A sibling binary either duplicates all four — a second
   copy of four rules that can then disagree — or ships without them.
3. **pvxs does not have a second IOC either.** `qsrvSingle` and
   `qsrvGroup` are *sources added to the one server*
   (`singlesourcehooks.cpp:159`, `groupsourcehooks.cpp:219`), inside the
   one registrar (`iochooks.cpp:461-497`). The C shape is one IOC with a
   source registry, and `CompositeSource` (`server_native/composite.rs`)
   is that registry's port — `add_source(name, source, order)` at
   `composite.rs:109`, "lower `order` first", the same convention.
4. **The status-PV vocabulary is already shared on purpose.**
   `rtems-pva-ioc` and `rtems-ca-ioc` deliberately use the same
   `STATUS_PREFIX = "RTEMS"` so "an operator's screens should not have to
   know which one booted". A third binary with a fourth spelling of the
   same names is the same problem again.

Concretely, `rtems-pva-ioc`'s step (3) becomes:

```
let composite = CompositeSource::new();
composite.add_source("qsrvSingle", Arc::new(PvDatabaseSource::new(db.clone())), 0)?;
if qsrv_enabled {                       // pvxs enable2(), iochooks.cpp:401-448
    let provider = build_qsrv_provider(&db, group_json)?;   // processGroups(),
                                                            // groupsourcehooks.cpp:192-213
    composite.add_source("qsrvGroup", Arc::new(QsrvPvStore::new(provider)), 1)?;
}
let server = BlockingPvaServer::bind(addr, composite, config)?;
```

**Optional, and gated on the same decision pvxs gates on.** `qsrv_enabled`
is the port of `enable2()` already sitting in `pva_adapter.rs:1255`
(`qsrv2_enabled`) — the function probe C reported as dead. Wiring it here
is what makes the RTEMS IOC answer to `PVXS_QSRV_ENABLE` the way a C IOC
does, and it is why §2.5 says gating the host runner must not be allowed
to bury these three items.

`serve_status_pvs` and the `PVA_CONN_CNT` status PV are untouched: they
key off the `PvDatabase` and the `BlockingPvaServer`, neither of which
changes shape.

Ordering constraint, from C: `processGroups()` runs at
`initHookAfterInitDatabase` and `addGroupSrc()` at
`initHookAfterIocBuilt`, i.e. **groups are finalized before the source is
registered, and the source is registered before the server starts**
(`ioc/iochooks.cpp:343-366`). In `rtems-pva-ioc` that maps to: build the
database (step 2) → build+finalize the provider → `add_source` →
`BlockingPvaServer::bind` → spawn the accept thread. The existing comment
at `pva_adapter.rs`'s group-loading site already states the same rule
("run here before the PVA server accepts connections so the first client
GET already sees finalized group PVs").

### 3.4 Where pvalink lands in this — nowhere, yet

`BlockingPvaServer` is a **server**. pvalink is a **client**: `link.rs`
opens `PvaClient` (`link.rs:11`, `:297`), runs `pvmonitor` re-subscribe
loops and issues PUTs. There is no blocking client driver, and probe D
measured what standing one up costs: 47 compile errors across
`client_native/udp.rs` (20), `client_native/search_engine.rs` (11),
`config/env.rs` (4), plus `tokio::net::{TcpStream,UdpSocket}`,
`socket2`, and `epics_base_rs::net::AsyncUdpV4` — over a 23,881-line
module tree.

That is a peer project to the PVA server's own sans-io work, not a
sub-task of mounting QSRV. §7 stages it accordingly and does not pretend
it is close.

---

## 4. Async → execution-model conversion, per dependency class

The seam is `epics_base_rs::runtime::task`
(`crates/epics-base-rs/src/runtime/task.rs`). On a hosted build it is
tokio (`cfg(tokio_backend)`); on RTEMS or under `--features
rtems-exec-model` it is the process-global `BackgroundExecutor`
(`cfg(exec_backend)`) — callback pool + delayed timer + scanOnce worker,
initialised by `background_init()` (C `callbackInit`, `callback.c:286`).

| class | sites | verdict | seam replacement | corresponding pvxs IOC thread |
|---|---|---|---|---|
| **`tokio::spawn` / `handle.spawn`** | 11 (§1.4) | **convert** — panics with no runtime entered | `runtime::task::spawn` → `spawn_future(callbacks().handle(), DEFAULT_SPAWN_PRIORITY, fut)` (`task.rs:201-213`). Returns `TaskHandle`/`TaskAbortHandle` aliases (`task.rs:135-149`), so `MemberTaskGuard(handle.abort_handle())` (`group.rs:2098`) and `MonitorAbort` (`link.rs:284`) keep their exact shape | The two qsrv event pumps: `db_start_events(…, "qsrvSingle", …, epicsThreadPriorityCAServerLow-1)` (`ioc/singlesource.cpp:416`) and `db_start_events(…, "qsrvGroup", …, CAServerLow-1)` (`ioc/groupsource.cpp:96`). C runs **one** pump thread per source and every member subscription is a callback on it; the Rust shape runs one task per member subscription on a shared pool. Same total concurrency budget, different granularity — see §8. |
| **`tokio::time::sleep`** | 3 (`link.rs:427`, `:498`, `integration.rs:733`) | **convert** — no timer wheel on the exec backend | `runtime::task::sleep` (`task.rs:246-249`) over `background().timer()`. `timeout` and `interval` have the same seam entries (`task.rs:340-354`, `task.rs:298-302`) if the shape changes | pvxs has no equivalent backoff loop; its reconnect is driven by the client `Context` loop `PVXCTCP` (`src/client.cpp:1386`, `CAServerLow` = 20). See §8 — the 250 ms→30 s ladder is ours, not a parity number. |
| **`std::time::Instant::now()`** | 2 (`integration.rs:725`, `:730`) | **keep, but flag** — compiles and runs, but the clock is 1-second-quantized on the target and `Instant` reads can be degenerate under the wrong `libc` `time_t` width | no seam change; the deadline loop's 50 ms poll becomes the coarsest granularity the target can express | — |
| **`tokio::sync::mpsc` channels** | 19 declarations (13 qsrv + 6 pvalink) | **keep** — runtime-agnostic | none. `tokio`'s `sync` feature is retained on the RTEMS target table (§2.2), and `park_on`'s contract names `tokio::sync` channels as explicitly allowed | pvxs `MPMCFIFO<std::weak_ptr<epicsThreadRunable>> queue` (`ioc/pvalink.h:109`) for pvalink; the group fan-in has no C counterpart because C's callbacks run *on* the pump thread rather than being forwarded to it. |
| **`tokio::sync::{Mutex,RwLock,Notify}`** | 4 (§5) | **case by case** — see §5 | — | `epicsMutex lock` on `linkGlobal_t` (`ioc/pvalink.h:111`) and `pvaLinkChannel` (`ioc/pvalink.h:154`, with the stated order "record lock(s) → channel lock"); `DBManyLocker` / `DBLocker` for group access (`ioc/groupsource.cpp:326, 375, 492, 521, 621, 645`). |
| **`tokio::select!`** | 4 (`monitor.rs:333`, `pva_adapter.rs:415`, `:843`, `:1141`) | **keep** — a pure future combinator, no reactor, no timer | none. Needs tokio's `macros` feature, retained on the RTEMS table (§2.2) | pvxs expresses the same "value event or property event, whichever first" as two `dbEventCtx` subscriptions delivering into the *same* callback on the pump thread (`ioc/fieldsubscriptionctx.cpp:25`, `ioc/subscriptionctx.h:34`) — the race is resolved by the queue, not by a combinator. |
| **`tokio::runtime::Handle`** | 1 field, 3 construction/use points (`integration.rs:38`, `:158`, `:826`) | **redesign** — `Handle::current()` panics with no runtime entered; there is no exec-backend `Handle` | Replace the field with nothing, and the four `handle.spawn(fut)` calls with `runtime::task::spawn(fut)`. The `PvaLinkResolver`'s *synchronous* callers (the record-path resolver closure) go through `block_on_sync` (`task.rs:114`), which already picks `park_on` on a plain thread, `block_in_place` on a multi-thread worker, and refuses a current-thread runtime | pvxs needs no handle at all: the record-path lset calls push work onto `linkGlobal->queue` and the `pvxlink` worker (`ioc/pvalink_channel.cpp:39-46`, `epicsThreadPriorityMedium`, `epicsThreadStackBig`) pops it (`:53-67`). The `Handle` is the Rust port's stand-in for that one worker thread, and `runtime::task::spawn` + the callback pool is the closer analogue. |
| **`std::thread::spawn` + `handle.block_on`** | 1 (`pvalink/iocsh.rs:43`) | **exclude** — host-only | none; the target has no iocsh (`rtems-pva-ioc` module docs: "The interactive iocsh is host-only, so it is not wired here") | pvxs's counterpart shell command `dbpvxr` is an `IOCShCommand` registered in `pvalink_enable()` (`ioc/pvalink.cpp:328-331`), equally shell-only. |

### 4.1 The pvxs IOC thread census, for reference

Every thread a C IOC linked against pvxs+QSRV2 runs, with the file:line
that creates it. This is the budget the Rust target IOC is measured
against.

| C thread | created at | EPICS priority | Rust counterpart today |
|---|---|---|---|
| `PVXTCP` — acceptor **and** the reactor for every connection | `src/server.cpp:388` | `CAServerLow-2` = 18 | `PVAS-TCP` accept thread + per-connection reader/operation/writer threads, all at `PVA_SERVER_PRIORITY` = 18 (`server_native/blocking.rs:189`) |
| `PVXUDP` — UDP search collector | `src/udp_collector.cpp:93` | `CAServerLow-4` = 16 | `PVAS-UDP` (`rtems-pva-ioc.rs`), `PVA_UDP_PRIORITY` = 16 (`blocking.rs:206`) |
| `qsrvSingle` — db event pump for `SingleSource` | `ioc/singlesource.cpp:416` | `CAServerLow-1` = 19 | **none** — the Rust port forwards each `DbSubscription` on a spawned task |
| `qsrvGroup` — db event pump for `GroupSource` | `ioc/groupsource.cpp:96` | `CAServerLow-1` = 19 | **none** — same |
| `pvxlink` — the single pvalink worker | `ioc/pvalink_channel.cpp:39-46` | `Medium` = 50, `epicsThreadStackBig` | **none** — `handle.spawn` onto the tokio pool |
| `PVXCTCP` — pvalink's client `Context` loop | `src/client.cpp:1386` | `CAServerLow` = 20 | none on RTEMS (no blocking PVA client) |
| `IfMapDaemon` — interface-map refresher | `src/evhelper.cpp:727` | `Min` = 0 | not ported |
| `SigInt` | `src/util.cpp:302` | `Max` = 99, `StackBig` | not applicable (no shell on target) |

Two observations that matter for §7 and §8:

* `pvxLinkNWorkers` is declared as an iocsh variable and initialised to
  **1** (`ioc/pvalink_channel.cpp:20`), and the constructor comment says
  `// TODO respect pvxLinkNWorkers?` (`:45`) — so upstream is
  single-worker in practice regardless of the knob. A Rust design that
  fans pvalink work across a callback pool is *more* parallel than C, not
  less, and the ordering assumptions that buys are unverified.
* The C group event pump is **one thread per source**. The Rust group
  monitor spawns **two tasks per member with a channel**
  (`group.rs:2496`, `:2532`) — for a 20-member group, 40 tasks against
  C's 1 thread. On the exec backend those land on the callback pool, and
  whether the pool's worker count survives that is §8's first question.

---

## 5. Lock disposition

Every `tokio::sync` lock/notify in production `qsrv` + `pvalink` code.
There are four. (The crate's other 26 declared lock sites are already `parking_lot`,
`std::sync` or `OnceLock` — enumerated below the table so the census is
complete rather than selective.)

| id | site | type | held across `.await`? | verdict |
|---|---|---|---|---|
| **L33** | **STALE — see §9.1.** At `32cc7847` this is already `Arc<PriorityInheritanceMutex<()>>` (`qsrv/group_config.rs:76`); the flip landed between this document's base `9965bbd6` and stage 1, and it is what produced stage 1's one unpredicted blocker. The row below describes the pre-flip shape. ~~`qsrv/group_config.rs:64` `GroupPvDef::atomic_write_lock: Arc<tokio::sync::Mutex<()>>` (constructed `:800`)~~ | `tokio::sync::Mutex` | **yes** — post-H9 it is acquired *before* `PvDatabase::lock_records` and therefore held across `lock_records(…).await` | **TBD → resolved by the landed round: stays `tokio::sync::Mutex` for now.** `5d46ce3b` states it directly: "It stays a plain `tokio::sync::Mutex`, **not** a `PriorityInheritanceMutex`: it is held for the whole atomic block, which means it is held across `lock_records(…).await` itself — genuinely async today, since L1 has not made the step-4 type flip. Converting L33 first would hold a `!Send` guard across that await and fail to compile at the connection-task spawn site… L33 becomes convertible once L1 does (step 4)." So: **blocked on L1's step-4 type flip, not on this work.** No action in any stage of §7. |
| **L-A** | **DONE — §9.12.** Now `parking_lot::RwLock` (`provider.rs:650`, ctor `:755`). The row below describes the pre-conversion shape. `qsrv/provider.rs:641` `BridgeProvider::record_cache: tokio::sync::RwLock<HashMap<String, (NtType, DbFieldType)>>` (constructed `:746`) | `tokio::sync::RwLock` | **needs checking** — it is a pure memo of `(NtType, DbFieldType)` per record name | **→ `parking_lot::RwLock`.** It guards a `HashMap` insert/lookup with no I/O inside the critical section, which is the exact profile the rest of `BridgeProvider` already uses (`groups`, `access_cell`, `base_group_defs`, `group_files` are all `parking_lot::RwLock` — `provider.rs:628, 647, 656, 668`). Being the one tokio lock in a struct whose four siblings are `parking_lot` is itself the tell. Converting removes a PI-invisible wait from the GET/PUT hot path and removes four `.await`s. **Precondition:** confirm no `.await` occurs while a guard is live (a `!Send` guard across an await fails to compile at the connection-task spawn site — the same trap L33 documents), which the compiler will state for us. |
| **L-B** | **DONE — §9.12.** Now `parking_lot::RwLock` (`pva_adapter.rs:203`, ctor `:210`); `arc-swap` was *not* taken. The row below describes the pre-conversion shape. `qsrv/pva_adapter.rs:194` `QsrvPvStore::pva_pvs: Arc<tokio::sync::RwLock<HashMap<String, PvaPvHandle>>>` (imported `:13`, threaded through `:287`, `:328`) | `tokio::sync::RwLock` | read-only on the serve path (`:337` `pva_pvs.read().await.get(..).cloned()`) | **→ `parking_lot::RwLock`, or `arc-swap`.** Same profile: a name→handle map, cloned out immediately, no I/O under the guard. Note `PvaPvHandle`'s own interior state is *already* `parking_lot::Mutex` (`:49`, `:50`), so the outer tokio lock is the odd one out. `arc-swap` (already a dependency, `Cargo.toml:107`) is the better fit if registration is rare and lookup is hot — but that is an optimisation, and `parking_lot` is the structural fix. |
| **L-C** | `pvalink/registry.rs:10` `use tokio::sync::Notify`; used at `:89-91` (`pending: RwLock<HashMap<RegistryKey, Arc<Notify>>>`), `:234`, `:257` (`tokio::pin!(notified)`), `:284` | `tokio::sync::Notify` | **yes, by construction** — the whole point is "park until another task finishes opening this link" | **keep.** `Notify` is runtime-agnostic (it is a waker list, not a reactor primitive) and `park_on` names it as allowed. The single-flight pattern it implements — one opener, N waiters — is the correct shape and has no cheaper synchronous equivalent. It is PI-invisible, which matters only if a high-priority thread can wait behind a low-priority opener; on RTEMS today it cannot, because **pvalink is not in the RTEMS closure at all** (§3.4). Revisit when it is. |

**Locks that are already synchronous (no action, listed for completeness):**

`pvalink/integration.rs` — `parking_lot::RwLock` ×6 (`:61`, `:68`, `:73`,
`:80`, `:97`, `:892`), `parking_lot::Mutex` ×1 (`:86`).
`pvalink/link.rs` — `parking_lot::Mutex` ×6 (`:153`, `:178`, `:205`,
`:222`, `:248`, `:300`).
`pvalink/registry.rs` — `parking_lot::RwLock` ×3 (`:81`, `:89`, `:98`).
`qsrv/provider.rs` — `parking_lot::RwLock` ×4 (`:628`, `:647`, `:656`,
`:668`) + the `AcfCell` wrapper at `:699`.
`qsrv/pva_adapter.rs` — `parking_lot::Mutex` ×2 (`:49`, `:50`),
`std::sync::Mutex` ×1 (`:143`, the process-global registered-PV map),
`OnceLock` ×1 (`:1233`, the `enable2()` decision).
`qsrv/group.rs` — the record instance is
`Arc<parking_lot::RwLock<RecordInstance>>` (`:613`), owned by
`epics-base-rs`.

**Not a bridge lock, but the one every group operation goes through:**
`PvDatabase::lock_records` (`epics-base-rs/src/server/database/record_lock.rs:491`)
is L1, and it is **already** the `PriorityGate` — a band-ordered,
cancel-safe gate over `std::sync::Mutex<GateState>` + `Waker`
(`record_lock.rs:225`), not a `tokio::sync::Mutex`. It is
runtime-agnostic and works under `park_on` unchanged. The C counterpart
for the group path is `DBManyLocker` (`ioc/groupsource.cpp:326`, `:492`,
`:621`) and `DBLocker` for the per-member fallback (`:375`, `:521`,
`:645`).

**Verdict summary:** 1 delete-the-tokio-ness (L-A), 1 delete-the-tokio-ness
(L-B), 1 keep (L-C), 1 blocked on L1 step 4 (L33). **No PI conversion is
warranted in the bridge today** — the two convertible locks are memo
caches that should be `parking_lot`, and the one lock that genuinely
needs priority awareness (L33) is explicitly deferred by the round that
just landed.

---

## 6. Gate and test additions

### 6.1 `scripts/rtems-check.sh`

**`CRATES`** gains `epics-bridge-rs`:

```
CRATES=(epics-base-rs epics-ca-rs epics-pva-rs epics-rtems-boot epics-bridge-rs)
```

This is what makes probe C a standing property instead of a one-off
measurement. Both `CONFIGS` (`portability`, `image`) run it; probe C-image
confirms the `rtems_boot_linked` configuration is green today, so adding
the crate does not redden the gate.

One caveat the script's own header predicts: `COMMON` carries
`--no-default-features`, and the bridge's default set is `["qsrv",
"qsrv-bin", "ca-gateway", "ca-gateway-bin"]`. With no features at all the
crate compiles to `error`/`convert`/`lib.rs` and nothing else — a green
that proves nothing. **The `CRATES` loop must pass the feature that
selects the target's actual configuration**, i.e. `--features qsrv-core`
(§2.2c). That means either a per-crate feature map in the script, or a
`rtems` feature on the bridge that means "the target's selection". A
per-crate map is the honest shape: it says out loud that this crate's
target configuration is a *choice*, which `--no-default-features` alone
would hide.

### 6.2 Binary census

`scripts/rtems-check.sh` requires **every** `src/bin/*.rs` in a listed
crate to be classified as `BINS` or `HOST_ONLY` — "Being in neither is
how a target binary lands outside this gate — which is how the last build
break reached the bring-up box." Adding `epics-bridge-rs` to `CRATES`
therefore *immediately* fails the script until all five of its binaries
are classified. All five are host-only (§2.3):

```
HOST_ONLY=( …
    epics-bridge-rs:ca-gateway-rs
    epics-bridge-rs:dual-gateway-rs
    epics-bridge-rs:dual-ioc-rs
    epics-bridge-rs:pva-gateway-rs
    epics-bridge-rs:qsrv-rs
)
```

Note the census walks `crates/$crate/src/bin/*.rs` by **filename**, while
the bridge's `[[bin]]` entries give explicit `path`s whose basenames use
underscores (`src/bin/ca_gateway_rs.rs` → bin name `ca-gateway-rs`). The
census computes `crate:$(basename "$src" .rs)`, so the pairs it will
produce are `epics-bridge-rs:ca_gateway_rs`, `:dual_gateway_rs`,
`:dual_ioc_rs`, `:pva_gateway_rs`, `:qsrv_rs` — **underscores, not
hyphens**. Getting this wrong makes the script fail with both an
"unclassified" and a "stale" complaint at once. UNVERIFIED which spelling
the maintainers want; the mechanical answer is the basename.

`BINS` gains nothing: no bridge binary is built for the target. The
target's qsrv mount lives in `epics-pva-rs:rtems-pva-ioc`, which is
already in `BINS` (§3.3).

### 6.3 Feature-ON (`rtems-exec-model`) census implications

This is the larger of the two test-side costs, and it is the one that
will bite at review time rather than at gate time.

`tools/rtems-exec-gate` requires every *reactor-dependent test site* in a
crate that declares `rtems-exec-model` to be accounted for by one of four
things: a file-level gate, a module-level gate, a per-test gate, or a
file-level census marker `// RTEMS-EXEC-MODEL-ALLOW(N): why` whose `N`
**equals** the number of ungated sites.

Measured today, `qsrv` + `pvalink` carry:

| location | `#[tokio::test]` sites |
|---|---:|
| `src/pvalink/integration.rs` | 38 |
| `src/qsrv/group.rs` | 26 (POST-ROUND **27**) |
| `src/pvalink/link.rs` | 23 |
| `src/qsrv/pva_adapter.rs` | 21 |
| `src/qsrv/provider.rs` | 20 |
| `src/qsrv/channel.rs` | 9 |
| `src/qsrv/monitor.rs` | 5 |
| `src/pvalink/registry.rs` | 5 |
| `src/pvalink/iocsh.rs` | 2 |
| `src/qsrv/trap_write.rs` | 2 |
| `src/qsrv/iocsh.rs` | 1 |
| **in-crate subtotal** | **152 (POST-ROUND 153)** |
| `tests/testqgroup.rs` | 31 |
| `tests/pva_gateway.rs` | 24 |
| `tests/testqsingle.rs` | 24 |
| `tests/qsrv_remote_log.rs` | 16 |
| `tests/acf_access_control_contexts.rs` | 2 |
| `tests/pvalink_seam.rs` | 1 |
| **integration subtotal** | **98** |

Measured: `rg -n 'RTEMS-EXEC-MODEL-ALLOW' crates/epics-bridge-rs` returns
**nothing**. So the moment `epics-bridge-rs` declares
`rtems-exec-model`, **250 sites** (POST-ROUND 251) are unaccounted for and the gate fails
closed — by design.

> **POST-STAGE-4 — this table is a subset, and the total is wrong.** The
> real bill is **392 sites across 33 files**, not 250. The table above
> counts only `qsrv` + `pvalink` and omits both gateway modules:
> `ca_gateway` carries 72 sites and `pva_gateway` 92 (68 in-module plus
> the 24 in `tests/pva_gateway.rs`, which the table did list but under
> the wrong subtotal). Of the qsrv+pvalink half the table is off by one
> — `src/pvalink/link.rs` measured **24**, not 23. The classification as
> built is in §9.13; the numbers here are left as written so the
> estimate and the measurement can be compared.

Three consequences:

1. **Declaring the feature is not free and must be its own stage.** The
   RTEMS *portability* gate (§6.1) does **not** require the feature —
   `armv7-rtems-eabihf` selects `exec_backend` through
   `target_os = "rtems"`, not through the feature. So §7 can land the
   target compile without touching a single test. The feature is what
   lets the exec backend be *exercised* on a host, and that is a separate
   decision with a 250-site bill attached.
2. **The census is per-file and must be honest.** Files whose tests are
   pure translation (`group_config.rs`, `pvif.rs` — zero `#[tokio::test]`
   between them) need nothing. Files whose tests spin real servers
   (`tests/testqgroup.rs`, `tests/pva_gateway.rs`) are candidates for a
   file-level `#![cfg(not(feature = "rtems-exec-model"))]`. Files with a
   mix need per-test gates or a checked count.
3. **`tests/pva_gateway.rs` (24 sites) is in a module that is
   feature-gated out of the RTEMS selection entirely**, but the census
   tool reads *source text*, not the resolved feature graph. Whether it
   counts a file whose contents never compile in the RTEMS selection is
   UNVERIFIED — the tool's option 4 explicitly allows "the file does not
   build or run in that configuration at all" as a census reason, which
   suggests it does count it and expects the marker to say so.

### 6.4 Dev-dependency

`epics-bridge-rs`'s `[dev-dependencies]` gains
`rtems-exec-gate = { path = "../../tools/rtems-exec-gate" }` plus a
ten-line `tests/rtems_exec_model_gate.rs` calling
`assert_crate_is_accounted_for` — **only in the stage that declares the
feature**, not before. Path-only (no `version`) so `cargo publish` strips
it, matching the other three crates.

---

## 7. Staged plan

Each stage names its own gate. No stage depends on a later one.

### Stage 1 — manifest: get the bridge into the RTEMS closure (small) — **DONE**

Landed as `c1456c0c` / `5680834f` / `a6ae5e9b` / `79cbcc81` on top of
`32cc7847`.
Read §9 before trusting the sub-steps below: step (2) was reduced and
one blocker the probes could not see was added.


*Size:* ~40 lines of `Cargo.toml`, 2 lines of `cfg` in `src/`, 6 lines of
`scripts/rtems-check.sh`.

1. Split `tokio` per-target (§2.2b) — measured as worth 58 errors.
2. Spell out `epics-pva-rs` / `epics-ca-rs` with `default-features =
   false`, and restore the dropped features through the host-facing
   bridge features (§2.2a).
3. Introduce `qsrv-core` and re-express `qsrv = ["qsrv-core", "pvalink",
   …]` (§2.2c).
4. `#[cfg(not(target_os = "rtems"))]` on `run_ca_pva_qsrv_ioc`
   (`pva_adapter.rs:1328`) and its `qsrv/mod.rs:48-51` re-export (§2.5).
   **Do not** `#[allow(dead_code)]` the three warnings this exposes —
   they are stage 2's work list.
5. `scripts/rtems-check.sh`: add `epics-bridge-rs` to `CRATES` with a
   per-crate feature selection, and all five binaries to `HOST_ONLY`
   (§6.1, §6.2).

*Gate:* `./scripts/rtems-check.sh` green in both configurations (probes C
and C-image say it will be); `cargo clippy -p epics-bridge-rs
--all-targets -- -D warnings` and `cargo nextest run -p epics-bridge-rs`
unchanged on the host; `cargo nextest run --workspace` for the
cross-crate manifest change.

*Risk:* the host feature restoration in (2). Getting it wrong silently
drops `tls` or `client` from a host build. The check that catches it is
the full-workspace test run, because `tests/pva_gateway.rs` and
`tests/pvalink_seam.rs` both exercise the client.

### Stage 2 — the target mount: serve groups from `rtems-pva-ioc` (medium) — **DONE**

Read §9.7–§9.10 before trusting the sub-steps below: step 3's stated mount
point is unbuildable (cargo package cycle) and the binary moved crates
instead.


*Size:* ~150–250 lines in `rtems-pva-ioc.rs` + a small builder in
`pva_adapter.rs`, plus the five spawn conversions in qsrv.

1. Convert the five qsrv production spawns to `runtime::task::spawn`
   (`group.rs:2496`, `:2532`; `pva_adapter.rs:404`, `:838`, `:1134`).
   `TaskHandle::abort_handle()` keeps `MemberTaskGuard` intact.
2. Give the qsrv provider an RTEMS-reachable construction path that runs
   `qsrv2_enabled()` (pvxs `enable2()`, `ioc/iochooks.cpp:401-448`) and
   `load_qsrv_groups()` (pvxs `processGroups()`,
   `ioc/groupsourcehooks.cpp:192-213`) — the two functions stage 1's
   gating left dead.
3. `rtems-pva-ioc`: build a `CompositeSource`, add `qsrvSingle` at order
   0 and `qsrvGroup` at order 1 (pvxs `singlesourcehooks.cpp:159`,
   `groupsourcehooks.cpp:219`), in the C init order
   (`iochooks.cpp:343-366`): groups finalized → source added → server
   bound → accept thread started.
4. Extend `rtems-pva-ioc`'s source-text guards to cover the new code —
   in particular `entry_point_never_starts_a_runtime`, which will now be
   scanning a file that references qsrv.

*Gate:* `./scripts/rtems-check.sh`; the four `rtems-pva-ioc` source-text
guards; `cargo nextest run -p epics-bridge-rs -p epics-pva-rs`. **Not
sufficient**: this stage's real gate is a boot on the QEMU box with
`pvxget`/`pvxinfo` against a `Q:group` PV, which is §8's first item.

*Risk:* the spawn-count asymmetry. C runs one `qsrvGroup` pump thread;
this runs 2 tasks per member. On the callback pool that is a concurrency
question, not a correctness one, until it is measured.

### Stage 3 — lock cleanup (small) — **DONE** (see §9.12)

Convert L-A (`provider.rs:641`) and L-B (`pva_adapter.rs:194`) to
`parking_lot::RwLock` (§5). Independent of stages 1–2 and of the RTEMS
work; it is a `!Send`-guard reduction that pays off on the host too.
Leave L33 alone — `5d46ce3b` already ruled, and it unblocks with L1's
step 4, not here.

*Gate:* `cargo clippy -p epics-bridge-rs --all-targets -- -D warnings`
(the compiler is what proves no guard crosses an `.await`), `cargo
nextest run -p epics-bridge-rs`.

### Stage 4 — `rtems-exec-model` feature + the 250-site census (large) — **DONE** (see §9.13)

Declare `rtems-exec-model = ["epics-base-rs/rtems-exec-model"]` on
`epics-bridge-rs`, add the `rtems-exec-gate` dev-dep and
`tests/rtems_exec_model_gate.rs`, and classify all 250 reactor-dependent
test sites (§6.3). Expect this to be the largest stage by diff size and
the smallest by risk: every classification is a reviewable one-line
decision, and the tool fails closed on any miss.

*Gate:* `cargo nextest run -p epics-bridge-rs --features rtems-exec-model`
green, and the census test itself.

As built: **392** sites, not 250 — §6.3's table omitted both gateway
modules. The "smallest by risk" prediction held: one classification
needed more than a marker (eight tests that host a real async CA server),
and no production defect surfaced. §9.8's owed restoration — the
host compile of `rtems-pva-ioc`'s `mod ioc` — was discharged in the same
stage.

### Stage 5 — pvalink on RTEMS (blocked, large, not scheduled)

Requires a blocking/sans-io PVA **client** driver: 47 measured compile
errors over a 23,881-line `client_native` tree, concentrated in UDP
search (`udp.rs` 20, `search_engine.rs` 11) — the same shape of work the
PVA *server*'s blocking driver already went through. Until that exists,
the target IOC serves `Q:group` PVs and resolves **no** `pva://` record
links. That is a real functional gap against a C IOC (which gets pvalink
through `pvalink_enable()`, `ioc/iochooks.cpp:495`) and it should be
stated in `rtems-pva-ioc`'s startup banner rather than discovered by an
operator whose `INP=pva://…` silently never connects.

---

## 8. Unverified — needs measurement on the target

Everything here is a claim this document could not settle from source.

1. **Nothing in stages 1–3 has been run on hardware.** Probes A–D are
   `cargo check`. "Type-checks for RTEMS" and "runs on RTEMS" are
   different claims and the workspace has been bitten by the gap before.
   The first real gate is a boot on the QEMU/BSP box with `pvxget` and
   `pvxinfo` against a `Q:group` PV, reached via
   `EPICS_PVA_NAME_SERVERS` (SLIRP forwards TCP; broadcast does not
   cross it).
2. ~~**Task count against the callback pool.**~~ **MEASURED — see
   §9.14.** A 20-member group spawns ~40 forwarder tasks
   (`group.rs:2496`, `:2532`); C runs one `qsrvGroup` thread
   (`ioc/groupsource.cpp:96`). On the target this is *not* a non-issue:
   one group subscriber collapses group delivery to ~0.35 Hz and starves
   an unrelated scalar monitor on its own TCP connection for up to
   16.9 s, while scanning itself stays at 10 Hz. The single drain loop
   C has is the structural candidate.
3. **Per-connection memory ceiling with a group source mounted.** The
   measured PVA ceiling is ~1.59 MB per connection, bounded before file
   descriptors. A group GET assembles a whole `PvStructure` from N
   members; UNVERIFIED how much of that lands on the connection thread's
   stack under `park_on` (which pins whole async state machines there via
   `std::pin::pin!`) versus the heap, and therefore whether a wide group
   moves the ceiling.
4. **Stack class for the connection thread serving groups.**
   `rtems-pva-ioc` starts `PVAS-TCP` at `StackSizeClass::Medium` because
   it "accepts and hands off". Group assembly happens on the
   per-connection operation thread, which `blocking.rs` spawns at
   Big/Medium. UNVERIFIED whether a deep group (nested `+id` structures,
   many members) needs Big.
5. **Priority of the group event forwarders.** C runs the group pump at
   `CAServerLow-1` = 19 (`ioc/groupsource.cpp:96`), i.e. *above* the
   server's own reactor at 18 (`src/server.cpp:388`). The Rust
   forwarders land on the callback pool at
   `DEFAULT_SPAWN_PRIORITY` (Medium band). §9.14 measured the *effect*
   under load (multi-second delivery stalls) but cannot yet attribute it
   between band priority and delivery-path saturation; the attribution
   experiment is listed there. Latency-only, load-only, as predicted.
6. **The 250-site census count.** Stage 4's bill is derived from
   `#[tokio::test]` occurrences. The tool also counts hand-built runtimes
   in test code, which this document did not grep for. The real number is
   ≥ 250. UNVERIFIED whether `tests/pva_gateway.rs`'s 24 sites count at
   all, given the module is feature-gated out of the RTEMS selection
   (§6.3.3).
7. ~~**`scripts/rtems-check.sh` census spelling.**~~ **VERIFIED (stage 1,
   `a6ae5e9b`): underscores.** The census pairs are
   `epics-bridge-rs:ca_gateway_rs`, `:dual_gateway_rs`, `:dual_ioc_rs`,
   `:pva_gateway_rs`, `:qsrv_rs`, exactly as the mechanical reading of
   `basename src/bin/*.rs .rs` predicted. Hyphens fail the run with both
   an "unclassified" and a "stale" complaint at once, as anticipated.
8. **The bridge's `Instant::now()` deadline loop
   (`pvalink/integration.rs:725`, `:730`, `:733`).** Only reachable once
   pvalink is on the target (stage 5), but worth recording now: the
   target clock is 1-second-quantized, which makes a 50 ms poll
   meaningless there. Whatever the pvalink stage does, it should not
   assume sub-second deadlines are expressible.
9. **The bridge's four `#[cfg(unix)]` sites.** RTEMS satisfies
   `cfg(unix)`, so a bare `#[cfg(unix)]` arm silently hands the target a
   Linux-shaped path. Checked for stage 1: all four are in
   `ca_gateway/server.rs` (`:37`, `:1273`, `:1362`) and the live one
   reaches `tokio::signal::unix` (`:1287`). None is in the target's
   graph — `ca-gateway` is not selected by `qsrv-core`. Recorded as a
   *bounded* trap rather than a silent one: the RTEMS tokio table drops
   the `signal` feature, so if `ca_gateway` were ever selected for the
   target this would be an unresolved-import error rather than a wrong
   path taken quietly. It is still the arm to fix first if that changes.
10. **The referenced design docs are not in this repository.**
   `doc/rtems-priority-locks-design.md` and `doc/pi-lock-evaluation.md`
   are cited by the H7/H9/L33 commit messages
   (`e8b6cd50`, `6db95afc`, `5d46ce3b`) and by
   `epics-base-rs/src/server/database/record_lock.rs`, but
   `git ls-tree -r` over **all 51 branches** finds neither file. This
   document therefore quotes L33's disposition from `5d46ce3b`'s own diff
   text (which reproduces it verbatim in `record_lock.rs`) rather than
   from §1.1/§5 of the design doc. If those docs exist elsewhere, §5's
   L33 row should be re-checked against them.

---

## 9. Stage 1 as built — where reality deviated from the probes

Stage 1 landed as `c1456c0c` (base) / `5680834f` (bridge) / `a6ae5e9b`
(gate) / `79cbcc81` (feature fix) on top of `32cc7847`. Two of §7 stage 1's
five sub-steps were byte-accurate against the probes. Two needed
correction, one blocker was invisible to every probe, and one whole
configuration — `qsrv-core` on a *host* — no probe could reach, because
every probe named the target triple.

### 9.1 A blocker the probes could not see: L33 is no longer a tokio mutex

**Predicted:** probe C, "0 errors, 3 warnings". **Measured at
`32cc7847`:** *1 error*, and not in any file §2.5 names:

```
error[E0277]: `epics_base_rs::runtime::sync::pi_mutex::PiMutex<()>`
              doesn't implement `Debug`
   --> crates/epics-bridge-rs/src/qsrv/group_config.rs:75
```

Cause: §5's L33 row is **stale**. Between this document's base
(`9965bbd6`) and `32cc7847`, `GroupPvDef::atomic_write_lock` was flipped
from `Arc<tokio::sync::Mutex<()>>` to
`Arc<PriorityInheritanceMutex<()>>`. Probe C measured a tree where it was
still tokio, so the error could not appear.

The defect it exposed is not in the bridge. `PriorityInheritanceMutex<T>`
is one type alias with three `cfg` arms: `parking_lot::Mutex<T>` on the
non-RT fallback, `pi_mutex::PiMutex<T>` on the two PI arms. The fallback
implements `Debug`; the PI arms did not. So **any** `#[derive(Debug)]`
struct holding one compiles on a developer's box and fails for
`armv7-rtems-eabihf` — and `GroupPvDef` (`#[derive(Debug, Clone)]`) is
simply the first such struct in the workspace. `server::pv`,
`database::mod`'s buckets and `record_lock`'s gate all hold a
`PriorityInheritanceMutex` *without* deriving `Debug`, which is why the
gap survived until the bridge entered the gate.

Fixed at the alias (`c1456c0c`), not at the derive site, so the next such
struct cannot reopen it — `PiMutex<T: Debug>: Debug`, via `try_lock` and
not `lock`, matching `parking_lot` (a blocking `Debug` deadlocks the
moment anything formats a structure while the lock is held). The file's
own guard comment already stated the rule for auto traits ("keeps the two
arms' auto traits identical"); this extends it to the named ones.
`PiMutexGuard` deliberately did **not** get one: no struct stores a guard
and derives `Debug`, so that would be speculative API.

With the base fix in place, probe C's prediction holds exactly: **0
errors, 3 warnings**, and the three warnings are the three §2.5 names
(`Qsrv2Decision`'s fields, `qsrv2_enabled`, `load_qsrv_groups` — the last
now at `pva_adapter.rs:1490`, moved by the gating comment).

### 9.2 `epics-ca-rs` was not spelled out — §2.2a reduced

§2.2a says to spell out **both** `epics-pva-rs` and `epics-ca-rs` with
`default-features = false`. Only `epics-pva-rs` was, and the reason is a
consequence of adopting §2.2c's option 1 properly rather than a shortcut.

The probes reached `epics-ca-rs` because they ran `--features qsrv`,
which carries `dep:epics-ca-rs`. `qsrv-core` does not: the crate's *only*
`epics_ca_rs` reference in `qsrv` is at `pva_adapter.rs:1427`, inside
`run_ca_pva_qsrv_ioc`, so the dependency belongs to the host runner and
moves to `qsrv` with it. Under the target selection `epics-ca-rs` is not
in the graph at all, and that — not its feature list — is the whole
reason. (This paragraph also claimed its `default` list was empty. That
was true when written and stopped being true at `274a734b`, which split
`client-core` out and made `default = ["client"]`; the argument never
rested on it, so only the sentence changed.)

Spelling it out anyway would have bought no coverage and cost a second
place where a hand-written `version = "0.24.0"` can drift from the
workspace table. Left inherited, with the reason stated at the entry.

### 9.3 §6.1's caveat is the whole story — the feature map is mandatory

§6.1 raised the featureless-green risk as a caveat. Measured, it is not a
caveat but the entry's central requirement: `COMMON` carries
`--no-default-features`, the `qsrv` module is behind
`#[cfg(feature = "qsrv-core")]`, and a featureless build of the bridge
type-checks `error`, `convert` and `lib.rs` — **0** of the 11,281
production lines. The per-crate `CRATE_FEATURES` map (§6.1's "honest
shape") is what makes the entry mean anything, and it is what shipped.

`qsrv-core` is also what §2.2c's option 1 requires the `#[cfg]`s to say:
every `#[cfg(feature = "qsrv")]` in `src/` was renamed to
`feature = "qsrv-core"` (14 sites across `lib.rs`, `qsrv/mod.rs`,
`pvalink/integration.rs`). Because `qsrv` is a strict superset, every
host selection resolves byte-identically. The two `#![cfg(feature =
"qsrv")]` *test* files were deliberately left alone: they drive
`PvaClient`, which only `qsrv` restores.

### 9.4 The gate the probes could not run: `qsrv-core` on a *host*

Every probe in §0 ran against `armv7-rtems-eabihf`, so none of them could
see that the first version of the runner gate —
`#[cfg(not(target_os = "rtems"))]`, exactly as §7 stage 1 step 4
specifies — left `qsrv-core` compiling **only** for RTEMS:

```
$ cargo check -p epics-bridge-rs --no-default-features --features qsrv-core
error[E0433]: failed to resolve: use of unresolved module or
              unlinked crate `epics_ca_rs`
   --> src/qsrv/pva_adapter.rs:1437
```

On a host the predicate is true, so the body compiles in a build where
`epics-ca-rs` is not linked. That is §2.2c's rejected option 2 reached by
accident: a feature whose validity depends on which target you point it
at. Fixed in `79cbcc81` by naming both requirements —
`#[cfg(all(feature = "qsrv", not(target_os = "rtems")))]`, since `qsrv`
is what carries `dep:epics-ca-rs` — on the definition and on the
`qsrv::mod` re-export, so the two cannot drift. One in-file test needed
the same treatment (`PvaServer::client_config` is behind
`epics-pva-rs/client`).

**§7 stage 1 step 4 is therefore wrong as written**: the predicate is not
`not(target_os = "rtems")` but the conjunction. Any later stage adding a
host-only item to `qsrv` must state the feature clause too.

Two consequences worth stating:

* `--features qsrv-core --all-targets` is now a real host configuration
  and compiles clean. It is **not** clean under `-D warnings`, and
  deliberately so: the three §2.5 dead-code warnings become errors there.
  This is why the gate runs `cargo check` and not clippy — the warnings
  are stage 2's work list, and §2.5 forbids `#[allow(dead_code)]`.
* Two **pre-existing** compile breaks in the crate were found while
  sweeping the other feature selections, both untouched by stage 1 and
  both left alone (verified byte-identical at `32cc7847`):
  `pva_gateway/{control,middleware}.rs` use `MonitorStream` without
  importing it, so `--features pva-gateway` (and `all-bridges`,
  `pva-gateway-bin`, `dual-gateway-bin`) do not compile at all; and
  `tests/acf_access_control_contexts.rs` + `tests/testqsingle.rs` carry
  no `#![cfg(feature = "qsrv")]` header, so any selection without the
  qsrv module fails on them. Neither is reachable from the default
  feature set, which is why `clippy --workspace --all-targets` is green
  over both.

### 9.5 Feature-ON census: unchanged, and the suite is green — **superseded by §9.13**

§6.3's 250-site bill is stage 4's and nothing in stage 1 touched it.
`epics-bridge-rs` still declares no `rtems-exec-model` feature, carries
no `rtems-exec-gate` dev-dep and no `RTEMS-EXEC-MODEL-ALLOW` markers, so
the census gates nothing for this crate yet. Recorded for stage 4's
baseline: at `79cbcc81` `cargo nextest run -p epics-bridge-rs` is
**674 tests, 674 passed, 0 skipped**.

All three "still" clauses were made false by stage 4 (§9.13): the feature
is declared, the dev-dep is in, and 33 files carry markers. The 250 is
wrong as well — it was 392.

### 9.6 What stage 1 did *not* do — settled by stage 2

No behaviour changed and no serving path was mounted. `rtems-pva-ioc`
still serves single-record PVs only; `CompositeSource` is untouched;
`qsrv2_enabled` and `load_qsrv_groups` are still dead on the target,
which is precisely §3.3's work list. The three dead-code warnings are
left standing on purpose.

**All four are closed by stage 2** (§9.7–§9.9): the mount exists, the
composite carries both sources, both functions have a target-reachable
caller, and the dead-code count is 3 → 0.

### 9.7 Stage 2's topology deviation: §3.3 and §6.2 are unimplementable

**Predicted:** §3.3 "grow `rtems-pva-ioc` an optional qsrv mount", and
§6.2 "the target's qsrv mount lives in `epics-pva-rs:rtems-pva-ioc`, which
is already in `BINS`". **Measured:** that arrangement cannot be built. It
requires `epics-pva-rs` to depend on `epics-bridge-rs`, which already
depends on `epics-pva-rs`, and cargo refuses at the package level — so
`optional = true` and feature gating do not help:

```
error: cyclic package dependency: package `epics-bridge-rs v0.24.3` depends
on itself. Cycle:
package `epics-bridge-rs`
    ... which satisfies path dependency `epics-bridge-rs` of package `epics-pva-rs`
    ... which satisfies path dependency `epics-pva-rs` of package `epics-bridge-rs`
```

§3.3 was right about the *shape* — one IOC with a source registry, not a
second binary — and wrong only about which crate can host it. The
resolution keeps every one of its four reasons and moves the binary
down-graph: `crates/epics-pva-rs/src/bin/rtems-pva-ioc.rs` →
`crates/epics-bridge-rs/src/bin/rtems-pva-ioc.rs`, with
`required-features = ["qsrv-core"]`. `epics-pva-rs` now produces no target
binary at all.

That is also the C layering, which is why it does not feel like a
concession: QSRV sits above pvxs and base, and `epics-bridge-rs` is that
layer. An entry point belongs at the top of the dependency stack, where
every source it composes is visible.

Rejected alternatives, both considered against §3.3's own criteria:

* **A second binary in `epics-bridge-rs`, leaving `rtems-pva-ioc` in
  place.** This is §3.3 reason 2 exactly — two near-identical entry points,
  either duplicating the (now seven) source-text guards so they can drift,
  or shipping without them — plus reason 1's two extra `-Zbuild-std`
  builds per gate run, in both configurations.
* **A new top crate holding the binary.** Same shape as the move, plus a
  new `CRATES` entry and its build cost, for no property the move lacks.

The published-surface objection (`epics-pva-rs` losing a binary) was
checked and is not real: `rtems-pva-ioc` exists only on unpushed scope-B
branches and has never shipped in a crates.io release.

Three consequences that are easy to miss, all measured:

1. **`build.rs` moves with the binary.** Link *arguments* — unlike
   `-L`/`-l` — do not propagate from a dependency's build script to a
   dependent's link, so `epics_rtems_boot::contract::emit_link_args()` must
   be called by the package owning the binary. `epics-bridge-rs` gains a
   `build.rs` and both the normal and build dependency on
   `epics-rtems-boot`; `epics-pva-rs` sheds all three, because emitting
   link args there would now decorate a link that never happens.
2. **`scripts/rtems-check.sh`'s `BINS` loop had no feature selection.**
   `COMMON` carries `--no-default-features`, and the `--lib` loop had
   already grown `CRATE_FEATURES` in stage 1 (§9.3) for exactly this
   reason; the `BINS` loop had not, because until now no target binary
   needed a feature. It does now.
3. **`required-features` on a target binary is safe here, and the comment
   the binary arrived with said otherwise.** That comment warned a
   `required-features` gate makes cargo silently *skip* the target,
   "turning the RTEMS gate into a vacuous pass". Measured on this
   toolchain, that is true only for the plural forms (`--bins`,
   `--all-targets`); the explicit `--bin NAME` this gate issues is a hard
   error:

   ```
   $ cargo check -p epics-bridge-rs --no-default-features --bin qsrv-rs
   error: target `qsrv-rs` in package `epics-bridge-rs` requires the features: `qsrv-bin`
   ```

   Recorded at both the manifest and the gate so it is not re-derived.

### 9.8 The accepted cost: the IOC body is not host-compiled until stage 4 — **DISCHARGED in stage 4**

The moved binary's `ioc` module was gated
`any(target_os = "rtems", feature = "rtems-exec-model")`. In its new home
that names a feature `epics-bridge-rs` does not declare — a dangling
predicate: three `unexpected_cfg` warnings and an arm no configuration can
select. Declaring the feature to make the warnings go away is precisely
§6.3's bill being dodged, since the feature is what pulls in the
~250-site `rtems-exec-gate` census. So the predicate narrowed to
`target_os = "rtems"` alone.

**The cost, recorded here so stage 4 picks it up rather than
rediscovering it:**

* The IOC body's only compile coverage today is
  `scripts/rtems-check.sh`, in both configurations. That is real coverage
  — it is the gate this stage's binary is inside — but it is not a host
  compile.
* The `mod ioc` unit tests (`search_status` ×3, `split_load_args` ×2) do
  not run on a host. Under `epics-pva-rs` they ran via
  `-p epics-pva-rs --features rtems-exec-model`.
* The source-text guards are **outside** `mod ioc` and are unaffected —
  all seven run in every host test pass.

Stage 4 restores both by declaring `rtems-exec-model` on
`epics-bridge-rs` (with the census it owes) and widening this predicate
back to the `any(...)` form. Until then this is the one place in the
workspace where an RTEMS entry point is not host-selectable.

**Done in `fdc4319c`.** The predicate is
`any(target_os = "rtems", feature = "rtems-exec-model")` again on both
`mod ioc` and `main`, and the five `mod ioc` unit tests run on a host
under `--features rtems-exec-model`. `mod demo_db` took the same widening
*plus* its existing `test` arm, so the built-in database stays checked by
the default host selection with no feature flag — that arm was the one
thing this section's cost did not take away, and it was not given up to
buy the rest back.

### 9.9 What stage 2 changed, against §7's four steps

| step | as designed | as built |
|---|---|---|
| 1 | convert 5 spawns at `group.rs:2496`,`:2532`, `pva_adapter.rs:404`,`:838`,`:1134` | done; the two `group.rs` lines had drifted to `:2569`/`:2605` POST-ROUND. `MemberTaskGuard`'s field became the seam alias `TaskAbortHandle` |
| 2 | give the provider an RTEMS-reachable construction path | done as `build_qsrv_mount` — and the **host runner was rewired through it**, so the two entry points share one owner instead of two copies. §2.5's three dead-code warnings: **3 → 0** |
| 3 | mount in `epics-pva-rs:rtems-pva-ioc` | mounted, but in `epics-bridge-rs:rtems-pva-ioc` — see §9.7 |
| 4 | extend the source-text guards | done; 4 guards → 8, each proved to fail on its own defect before being committed. The 8th is not a source-text guard: it runs `parse_db` + `parse_info_group` over the built-in database and asserts the group it defines (see §9.11) |

Two further notes:

* §9.4's closing note that `--features qsrv-core --all-targets` is "not
  clean under `-D warnings`, and deliberately so" is **retired**: the three
  warnings it referred to were stage 2's work list and are now consumed.
  That selection is clean.
* §9.4's other pre-existing break — `tests/acf_access_control_contexts.rs`
  and `tests/testqsingle.rs` carrying no feature header — was fixed, and
  the census found a **third** file with the identical defect,
  `tests/testqgroup.rs`, hidden behind the compiler's error cap. All three
  took `#![cfg(feature = "qsrv-core")]`, not `qsrv`: none reaches
  `PvaClient`, and the narrower predicate keeps them compiling under the
  target's own selection. The `pva_gateway/{control,middleware}.rs`
  `MonitorStream` break is a missing import rather than a missing gate and
  is still open.

### 9.10 What stage 2 did *not* do

Stage 3 (the L-A/L-B lock cleanup) and stage 4 (the `rtems-exec-model`
feature and its census) are untouched, and stage 5 remains blocked on a
blocking PVA client — which is why the target IOC now states the `pva://`
gap at boot instead of leaving it to be discovered.

Two things inside stage 2's own scope were deliberately left:

* **The spawn-count asymmetry is still unmeasured under load.** §7 stage
  2's stated risk — C runs one `qsrvGroup` pump thread
  (`ioc/groupsource.cpp:96`), this runs two tasks per member — is now
  reachable on the target, but only correctness was verified, not
  behaviour at saturation. §8 item 2 stands.
* **The `activation_handles` / per-op MONITOR START-STOP gate** was not
  re-examined against the callback pool. It is runtime-agnostic and needed
  no conversion, so it is outside this stage's diff — but it has never run
  on the exec backend either.

### 9.11 Stage 2 on the target — measured

Built for `armv7-rtems-eabihf` (`-Zbuild-std=std,panic_abort`,
`--no-default-features --features qsrv-core`, 8,700,984-byte ELF) and
booted under qemu `xilinx-zynq-a9` on the bring-up box, reached over
`EPICS_PVA_NAME_SERVERS=127.0.0.1:5075` alone (SLIRP `hostfwd`; no UDP
broadcast, `EPICS_PVA_AUTO_ADDR_LIST=NO`). Clients are the C++ pvxs
tools. The forward is proven live by protocol traffic — type descriptors
and values come back — not by a `connect()` that SLIRP would accept
regardless.

#### The group source a bare target can have

A `-kernel` boot has no populated filesystem, so no argument can name a
`.json` file and the `dbLoadGroup` route of §3.2 is unreachable on this
target. The record-info route (pvxs `loadConfigFromDb`, step 1 of
`load_qsrv_groups`) is the only group source it has, so the built-in
database declares the group with three `info(Q:group, …)` fragments, one
per record, naming one group. `+channel` values there are record-relative
by construction: info-group channels are prefixed with `"{record}."`
unconditionally (`groupconfigprocessor.cpp:810-818`), so an absolute PV
name is not expressible on this path at all.

That database moved **out** of the `target_os = "rtems"` module into a
`cfg(any(target_os = "rtems", test))` one. It is data, not RTEMS code,
and by §9.8 the IOC body has no host compile until stage 4 — so without
the move, a misplaced brace in a group fragment would have been
detectable only by a cross-build, an image copy and a qemu boot, and its
symptom on the console is *nothing at all*: a group that was never
defined is a name no client can find. The 8th guard now runs the same two
parsers the target runs and asserts the group id, its atomicity, and the
three members in put order with their resolved channels.

#### Boot console

```
rtems-boot: main() reached
epics-rs: lock protocol: PI is enabled, RT scheduling AllowRealtime
INFO: PVXS QSRV2 is loaded, permitted, and ENABLED.
INFO  epics_bridge_rs::qsrv::pva_adapter: qsrv: processGroups created 1 group(s)
rtems-pva-ioc: serving 3 records on PVA TCP port 5075 (UDP search on 5076), GUID de505deeb1cf08b9b680a0ca, RTEMS execution model, no tokio runtime
rtems-pva-ioc: QSRV2 ENABLED — sources: qsrvSingle(0), qsrvGroup(1)
rtems-pva-ioc: NOTE pva:// record links do NOT resolve on this target — pvalink needs a blocking PVA client, which does not exist yet (design stage 5). An INP/OUT of the form @pva://... will never connect. ca:// links are unaffected.
```

`qsrv2_enabled()` and `load_qsrv_groups()` — the two functions stage 1
left dead — are the second and third lines. Nothing panicked over the
whole session (`grep -icE "panic|panicked|FAILURE"` → 0).

#### Group introspection

Asserted with `pvxinfo`, not a default-format `pvxget`: a GET reply never
sets the top-level struct-id bit, so Delta output drops exactly the id
being checked here.

```
$ pvxinfo -w 8 RTEMS:PVA:GRP
RTEMS:PVA:GRP from 127.0.0.1:5075
struct "rtems:demo/Group:1.0" {
    struct "epics:nt/NTScalar:1.0" {
        double value
        ...
    } setpoint
    int32_t count
    string message
    struct { struct { int32_t queueSize; bool atomic } _options } record
}
```

The declared `+id` survives to the wire; `+type:"scalar"` composes a full
NTScalar substructure while the two `+type:"plain"` members are bare
scalars; and the `record._options` block pvxs adds to every group is
present, carrying `atomic` from the group's `+atomic`.

#### Group GET, group PUT, single-PV regression

```
$ pvxget -w 8 -F tree RTEMS:PVA:GRP        # values from the built-in database
            double value = 1.5             #   RTEMS:PVA:AO.VAL, units "V", precision 3
        int32_t count = 7                  #   RTEMS:PVA:LO.VAL
        string message = "rtems-pva-ioc"   #   RTEMS:PVA:MSG.VAL
              bool atomic = true

$ pvxput -w 8 RTEMS:PVA:GRP setpoint.value=4.25 count=42 message=from-group-put
$ pvxget -w 8 -F tree RTEMS:PVA:GRP
            double value = 4.25
        int32_t count = 42
        string message = "from-group-put"

$ pvxget -w 8 RTEMS:PVA:AO RTEMS:PVA:LO RTEMS:PVA:MSG    # the backing records
    value double = 4.25
    value int32_t = 42
    value string = "from-group-put"

$ pvxput -w 8 RTEMS:PVA:AO 9.75; pvxput -w 8 RTEMS:PVA:LO 123
$ pvxput -w 8 RTEMS:PVA:MSG single-put
$ pvxget -w 8 RTEMS:PVA:AO RTEMS:PVA:LO RTEMS:PVA:MSG
    value double = 9.75
    value int32_t = 123
    value string = "single-put"

$ pvxget -w 8 -F tree RTEMS:PVA:GRP        # the group sees the single PUTs
            double value = 9.75
        int32_t count = 123
        string message = "single-put"

$ pvxget -w 8 RTEMS:PVA_CONN_CNT RTEMS:FD_CNT RTEMS:FD_MAX
    value double = 0 / 8 / 150
```

Both directions are live: a group PUT reaches the backing records, and a
single PUT is visible through the group. `pvxinfo RTEMS:PVA:AO` still
reports `epics:nt/NTScalar:1.0`, and the status PVs still resolve — so
`qsrvSingle` at order 0 is not shadowed by the group source at order 1.

#### What this run did not measure

Only correctness. MONITOR on a group, the atomic-PUT interleave, and the
spawn-count asymmetry of §8 item 2 were not exercised under load, and the
`activation_handles` MONITOR START-STOP gate still has never run on the
exec backend (§9.10).

### 9.12 Stage 3 as built — the lock family, and how the property is proved

Stage 3 landed as `46c60b48` (L-A) / `b3ddb6e6` (L-B) on top of
`7f9a089d`. §7's stage-3 entry was accurate and needed no correction: both
locks converted to `parking_lot::RwLock`, L33 untouched, `arc-swap` not
taken (§5 offered it for L-B as an optimisation; `parking_lot` is the
structural fix and the registration/lookup ratio was never measured, so
taking it would have been speculative).

**The cited line numbers survived stage 2.** §5 cites `provider.rs:641`
and `pva_adapter.rs:194`; stage 2 grew `pva_adapter.rs` by ~176 lines but
entirely *below* `QsrvPvStore` (`load_qsrv_groups` moved to `:1490`,
§9.1), so both declarations were still exactly where §5 put them. Both
were nevertheless re-located structurally rather than by line, because
that could not be known in advance.

**The family, enumerated before editing.** Anchors: declarations
`rg -n 'tokio::sync::(Mutex|RwLock)|use tokio::sync'` and acquisitions
`rg -n '\.(read|write|lock)\(\)\s*\.await'`, both over
`crates/epics-bridge-rs/src/qsrv/` plus `src/bin/rtems-pva-ioc.rs` —
i.e. the whole `qsrv-core` module graph (`pvalink` is
`#[cfg(feature = "pvalink")]`, outside it, so L-C is not in this family).
That is 2 declarations and 14 acquisition sites:

| site | classification |
|---|---|
| `provider.rs:641` decl + `:746` ctor | L-A — converted |
| `provider.rs:1277, 1432, 1461` | L-A acquisitions — converted |
| `pva_adapter.rs:13` `use tokio::sync::{RwLock, mpsc}` | L-B import — reduced to `mpsc` |
| `pva_adapter.rs:194` decl + `:201` ctor + `:287`, `:328` param types | L-B — converted |
| `pva_adapter.rs:217, 291, 337, 489, 526, 787, 1001, 1016, 1034, 1054, 1109, 1127` | L-B acquisitions — converted |
| `group_config.rs:66` | **distinct** — comment naming L33, which is already `PriorityInheritanceMutex` (§9.1); the row is blocked on L1 step 4, not on this stage |
| `group.rs:1787`, `group.rs:5257` | **distinct** — comments, no lock |
| `group.rs:2195, 2209, 2519` | **distinct** — `tokio::sync::mpsc`, a channel not a lock; `park_on` allows it and it carries no guard |
| `bin/rtems-pva-ioc.rs:640` | **distinct** — comment stating the `tokio::sync` allowance |

After the two commits, `rg '\.(read\|write\|lock)\(\)\s*\.await'` over
`src/qsrv/` returns **only** the `group.rs:5257` comment, and the
`tokio::sync` lock-type anchor returns only the two comments. The family
is closed, not sampled.

**One site needed restructuring, not a bare `.await` deletion.**
`check_monitor_request` read

```rust
if pva_pvs.read().await.contains_key(&name) || provider.is_servable_group(&name).await {
```

An `if` condition keeps its temporaries alive to the end of the whole
condition, so simply dropping `.await` would have left a sync guard live
across the `is_servable_group` await on the right-hand side. Split into
two sequential `if`s. Every other site's critical section ends before the
next await without restructuring.

**Two functions lost their `async`,** because the lock was the only thing
they awaited: `BridgeProvider::clear_cache` (no callers) and
`QsrvPvStore::register_pva_pv` (one production call site at
`pva_adapter.rs:1498`, six in tests). Leaving a `pub async fn` with a
wholly synchronous body would have been residue.

**How "no await under either guard" is proved.** Not by inspection — by
construction, and then confirmed by negative control. `parking_lot`'s
guards are `!Send`; every `QsrvPvStore` `ChannelSource` method returns
`impl Future<..> + Send`, and those methods are the only route to both
locks (L-B directly, L-A through `provider.create_channel_with_creds`).
So a guard held across an await is a compile error. Confirmed by
deliberately introducing one in each file and observing the failure
before reverting:

* L-A — `self.db.has_name(..).await` inserted under the live
  `record_cache` read guard in `create_channel_with_creds`:
  `error: future cannot be sent between threads safely`, reported at
  `create_channel` (`provider.rs:1350`) *and* propagated out to
  `get_value_checked` (`pva_adapter.rs:517`) and `put_value_checked`
  (`:588`) — which is the transitive `+ Send` reach being demonstrated,
  since `ChannelProvider`'s own `async fn`s carry no `Send` bound of
  their own.
* L-B — `provider.channel_find(key).await` inserted inside `list_pvs`'s
  `for key in pva_pvs.read().keys()` loop: the same error, reported
  directly at the `impl Future<Output = Vec<String>> + Send` bound.

The `for`-loop shape was chosen for the L-B control on purpose: it is the
one site whose guard outlives a whole block rather than a single
expression, so it is where the property is weakest by inspection and the
compiler's answer matters most.

**Gates.** `cargo fmt --all`; `cargo clippy -p epics-bridge-rs
--all-targets -- -D warnings` and the same with `--no-default-features
--features qsrv-core` (the target's selection, §2.2c) both exit 0;
`cargo nextest run -p epics-bridge-rs` 683/683; `scripts/rtems-check.sh`
exit 0 in both configurations after each commit; full-workspace
`cargo clippy --workspace --all-targets -- -D warnings` exit 0 and
`cargo nextest run --workspace` 10103 passed / 2 skipped.

**Not done here.** No behaviour changed and nothing was re-measured on
the target: stage 3 is a `!Send`-guard reduction, so the RTEMS evidence in
§9.11 stands as-is and was not re-run on hardware. §8's open items are
untouched.

### 9.13 Stage 4 as built — the census is 392, not 250

Stage 4 landed as `fdc4319c` (feature + §9.8 restoration), `1ee76e6f`
(the eight gated tests) and `811112ee` (the 392-site census and the gate
that checks it), on top of the stage-3 merge `bbc43b54`.

#### The number

§6.3 estimated ~250. Measured by the tool itself — declare the feature,
add `tests/rtems_exec_model_gate.rs`, and read the failure, which
enumerates every unaccounted site with its line numbers:

| module | files | sites |
|---|---:|---:|
| `src/qsrv/` | 7 | 85 |
| `src/pvalink/` | 4 | 69 |
| `src/ca_gateway/` | 10 | 72 |
| `src/pva_gateway/` | 6 | 68 |
| **in-crate subtotal** | **27** | **294** |
| `tests/` (6 files) | 6 | 98 |
| **total** | **33** | **392** |

The estimate was not slightly low, it was scoped wrong: it enumerated
`qsrv` + `pvalink` only and left both gateway modules out entirely —
140 of the 392 sites. Within the half it did enumerate it was off by
one (`pvalink/link.rs` is 24, the table said 23), so the qsrv+pvalink
in-crate figure is 154 against the predicted 153.

The lesson is worth keeping because it will recur: §6.3's table was built
from a `#[tokio::test]` grep over the *modules the design was reasoning
about*, and the crate is bigger than the design's subject. Running the
tool is the measurement; a hand-scoped grep is a guess with a table
around it.

#### The classification

| class | sites | files | accounting |
|---|---:|---:|---|
| checked — run and pass feature-ON | 292 | 26 | option 4, reason `checked - these run and pass in the feature-ON suite.` |
| not built feature-ON by default | 92 | 7 | option 4, reason `not built feature-ON by default - ... behind the pva-gateway feature.` |
| reactor-dependent, gated out | 8 | 1 | option 3, per-test `#[cfg(not(feature = "rtems-exec-model"))]` |
| real defect, fixed at source | 0 | 0 | — |
| **total** | **392** | **33** | |

Per-file, the "checked" 292: `qsrv/` 85 (`group.rs` 27,
`pva_adapter.rs` 21, `provider.rs` 20, `channel.rs` 9, `monitor.rs` 5,
`trap_write.rs` 2, `iocsh.rs` 1); `pvalink/` 69 (`integration.rs` 38,
`link.rs` 24, `registry.rs` 5, `iocsh.rs` 2); `ca_gateway/` 64 of its 72
(`upstream.rs` 5 after gating, `stats.rs` 10, `downstream.rs` 9,
`server.rs` 9, `command.rs` 8, `control.rs` 7, `pvlist.rs` 6,
`cache.rs` 4, `putlog.rs` 4, `beacon.rs` 2); `tests/` 74
(`testqgroup.rs` 31, `testqsingle.rs` 24, `qsrv_remote_log.rs` 16,
`acf_access_control_contexts.rs` 2, `pvalink_seam.rs` 1).

"Checked" is a measurement, not a courtesy. `cargo nextest run -p
epics-bridge-rs --features rtems-exec-model` runs **681 tests, all
passing**, and the census test runs inside that same selection, so a
count bumped without running the test still fails there. Every `src` file
carrying a "checked" marker has tests in the feature-ON binary list, and
every integration file's feature-ON test count equals its declared count
exactly.

#### The one classification that needed more than a marker

`ca_gateway/upstream.rs` has 13 sites. Eight fail feature-ON, five pass,
and the split is not arbitrary: the eight are exactly the eight whose
bodies contain `CaServer::builder()`, i.e. that host a real in-process
**async** CA server and drive it with `server.run()`. That server reaches
the network through the `runtime::task` seam, which feature-ON is the
std-thread executor — no tokio reactor — so its first `tokio::net` call
panics on a `cbMedium` worker:

```
thread 'cbMedium' panicked at epics-ca-rs/src/server/tcp.rs:1273:22:
there is no reactor running, must be called from the context of a Tokio 1.x runtime
```

and the test then fails on `ensure_subscribed`'s
`upstream PV did not connect`. The five that pass use a *dead* port with
nothing bound (`dead_upstream()`), so no server is ever started.

This is the feature behaving as specified, not a defect. `ca-gateway` is
not in the RTEMS closure — `scripts/rtems-check.sh` builds `qsrv-core` —
and the target's CA front-end is `BlockingCaServer`, which takes no part
in this path. So the eight take option 3, the shape `epics-ca-rs`'s
`two_priorities_open_two_circuits` already uses for the identical cause.

Family closed before editing, per the fixes-from-reported-defects rule:
anchor `rg 'CaServer::builder|CaServer::new' crates/epics-bridge-rs/{src,tests}`
returns exactly those eight sites plus two prose mentions in
`ca_gateway/server.rs`. No other test in the crate hosts an async CA
server. The `use epics_ca_rs::server::CaServer` import took the same
predicate — feature-ON it would otherwise be an unused import under
`-D warnings`.

#### The 92 `pva-gateway` sites, and the break behind them

`src/pva_gateway/` (68) and `tests/pva_gateway.rs` (24) take option 4's
*second* permitted reason: the file does not build or run in that
configuration. `pva-gateway` is not in the crate's default feature set,
so the gate command does not compile it at all.

Independently — and this is §9.9's last open item, unchanged by this
stage — the feature does not compile in *any* selection right now:
`cargo check -p epics-bridge-rs --all-targets --features pva-gateway`
produces **72** `cannot find type MonitorStream` errors in
`pva_gateway/{control,middleware}.rs`. Measured at `fdc4319c` with and
without `rtems-exec-model`, identically, so it is neither caused nor
worsened here. The census markers deliberately state the *stable* reason
(behind a non-default feature) rather than the transient one, so they
stay true once that break is fixed — at which point whoever fixes it owes
a run of `--features rtems-exec-model,pva-gateway` and, if those 92 pass,
a promotion of the seven markers to `checked`.

#### Feature-ON suite arithmetic

681 tests feature-ON against 683 in the default selection. The delta is
exactly accounted for: **+1** census gate, **+5** `mod ioc` unit tests
that `fdc4319c` restored to the host, **−8** gated above.

#### No new red was treated as a flake

The eight failures reproduced identically across two independent runs,
fail in ~50 ms with a named panic rather than a timeout, and are
partitioned by a source-level property (`CaServer::builder` present or
absent) rather than by scheduling. Nothing here matches the
shared-fixture race shape the exec backend is known to expose — that
shape was watched for and did not appear.

#### Gates

`cargo fmt --all`. `cargo clippy -p epics-bridge-rs --all-targets --
-D warnings` in all three configurations — default, `--no-default-features
--features qsrv-core` (the target's selection), and
`--features rtems-exec-model` — each exit 0. `cargo nextest run -p
epics-bridge-rs` 683/683; `cargo nextest run -p epics-bridge-rs
--features rtems-exec-model` 681/681 including the census test.
`./scripts/rtems-check.sh` exit 0 in both configurations. Full workspace:
`cargo clippy --workspace --all-targets -- -D warnings` exit 0,
`cargo nextest run --workspace` 10103 passed / 2 skipped. The three
sibling census gates (`epics-base-rs`, `epics-ca-rs`, `epics-pva-rs`,
each `-p <crate> --features rtems-exec-model`) still pass, so the shared
tool was not perturbed.

Naming the feature-ON selections explicitly is not ceremony:
`--workspace --all-targets` compiles a `#![cfg(feature = "...")]` test
file *away*, so a green workspace run says nothing about the census.
`-p epics-bridge-rs --features rtems-exec-model` is the only invocation
that runs it.

#### Not done here

* The `pva-gateway` `MonitorStream` break (72 errors) is untouched —
  pre-existing, and out of this stage's scope by construction: it is not
  in the RTEMS closure and not reachable from any configuration this
  stage's gates build.
* Nothing was re-measured on the target. Stage 4 changes no production
  code path — the only production-side edit is a widened `cfg` predicate
  on an entry point whose RTEMS arm is byte-identical — so §9.11's
  hardware evidence stands and §8's open items are untouched.
* Stage 5 (pvalink on RTEMS) remains blocked on a blocking PVA client.

### 9.14 §8 items 2 and 5, measured on the target — one group subscriber starves monitor delivery

**Setup.** Probe commit `8e70b6f8` compiles a 20-member `Q:group`
`RTEMS:PVA:BIG` into `rtems-pva-ioc`'s demo DB: members `B00..B19` are
self-driven calcs (`SCAN ".1 second"`, `CALC "VAL+1"`), plus an
out-of-group victim `RTEMS:PVA:V0` with the same 10 Hz self-drive.
Scanning on the PVA-only target is alive via `51f60ed0` (the scan-owner
thread; without it SCAN was dead entirely — see the scan-ownership
note). Target: QEMU `xilinx_zynq_a9` on the build box, reached over
SLIRP hostfwd to `127.0.0.1:5075`. Instrument: host-side `pvxmonitor`
arrival timestamps in microseconds (the guest clock is
1-second-quantized, so host wire time is the only usable clock), one
`pvxmonitor` process — and therefore one TCP connection — per PV.
Phases: 90 s baseline (victim monitored alone), 90 s load (a monitor
opened on `BIG`; group forwarders only run while a subscription
exists), 30 s recovery (load monitor closed). Scripts and raw captures:
box `~/rtems-bringup/qsrv8/` (`q8-measure.sh`, `q8.victim`, `q8.big`,
`q8.phases`, run 2 — run 1 is invalid, `pvxmonitor` has no `-w` flag).

**Victim `V0` inter-arrival gaps (ms), per phase:**

| phase    | n   | median | mean    | p99     | max      |
|----------|-----|--------|---------|---------|----------|
| BASELINE | 900 | 100.01 | 100.00  | 104.51  | 140.62   |
| LOAD     | 41  | 102.99 | 2198.36 | 5995.12 | 16883.97 |
| RECOVERY | 301 | 99.99  | 99.90   | 103.44  | 107.73   |

Baseline delivers every 10 Hz tick with no value jumps. Under load the
victim receives **41 updates where ~900 were posted**; delivery is
bimodal — bursts at normal cadence, then stalls up to **16.9 s** — and
8 value jumps (deltas up to 224 ticks) show the rest were coalesced
away, not delayed. Recovery is complete from the first post-load
sample.

**Group `BIG` during load:** 31 delivered updates in 89.5 s (~0.35 Hz
against a 10 Hz posting rate); gap median 3009.64 ms, min 0.50 ms
(queue-drain bursts), max 5883.04 ms; the `f00` step histogram is 13×
step-1 (burst drains) plus steps of 57–105 ticks (latest-value
coalescing through `queueSize` 4).

**What is and is not starved.** `f00` advanced 840 ticks in 89.5 s and
the victim's counter kept incrementing through every stall: database
scanning ran at ~10 Hz throughout. The victim sat on its own TCP
connection, so socket backpressure from the group reply cannot explain
its stalls. The collapse is server-side, in the monitor delivery path
shared by both subscriptions.

**Verdict on §8 item 2:** not a non-issue. The cooperative executor
*holds* the ~40 forwarder tasks (nothing died), but delivery collapses
~30× under a single group subscriber and takes an unrelated PV down
with it. C's one-`qsrvGroup`-drain-thread shape
(`ioc/groupsource.cpp:96`) is the structural candidate; resizing the
pool or re-banding the forwarders is the patch-shaped alternative.

**On §8 item 5 (forwarder priority vs C):** this run measures the
combined effect only. Separating band-priority inversion from
delivery-path saturation needs an attribution experiment — e.g. rerun
with the forwarders spawned in the High band, or with a single drain
task — before concluding which lever closes it.
