# item-7 stage G — scoping `rtems-pva-ioc`

Read-only investigation. No source file was edited, no commit made. One
`cargo check` was run (mutates only `target/`).

**Measured at:** worktree `/home/stevek/work/epics-rs/.caucus/worktrees/integration`,
branch `integration/rtems-scope-b`, HEAD **`1c27465c`** ("feat(pva):
BlockingPvaServer, the RTEMS accept loop (item 7 stage C)") at start **and**
end. Working tree **clean** at both. Stage C confirmed as HEAD itself.

**Measured baseline:** `cargo +nightly check -p epics-pva-rs --lib
--no-default-features -Zbuild-std=std,panic_abort --target armv7-rtems-eabihf`
→ **exit 0, 2 warnings.** Stage C retired the two `peers.rs` warnings exactly
as `doc/pva-rtems-stage-cd-design.md` §3.1 predicted. The residue is now W1
(`fetch_update` deprecation, `tcp.rs:1404`, unrelated) and W4 (`SearchRequest`'s
five dead fields, `search.rs:53`, stage D's). The completion criterion is
holding on its own terms.

---

## 0. The five findings that decide stage G

1. **Stage G does not need stage D.** Our server answers SEARCH **over the TCP
   circuit** (`tcp.rs:5866`, `:5908`, `:5913`), and both pvxs
   (`config.cpp:589-590`, `client.cpp:621-633`) and our own client
   (`client_native/context.rs:168`, `:218-225`) support
   `EPICS_PVA_NAME_SERVERS`. So an `rtems-pva-ioc` with **no UDP at all** is
   fully reachable by a host client today. §5. This also makes PVA *easier*
   than CA under QEMU SLIRP, not harder — it needs one TCP forward and no
   datagram path whatsoever.
2. **The GUID is broken right now, not merely at risk.** `PvaServerConfig`
   defaults `guid: [0u8; 12]` (`config.rs:343`); the only assignment anywhere
   in the crate is `runtime.rs:231` (`config.guid = random_guid()`), and
   `runtime.rs` is host-only. `BlockingPvaServer::bind` never touches it
   (`blocking.rs:829-853` — verified by `rg guid blocking.rs`: zero hits). So
   **a stage-G IOC built on stage C serves SEARCH replies with an all-zero
   GUID**, on host and on RTEMS alike. §3.
3. **The Cargo.toml problem is worse than "add a `[[bin]]`".**
   `epics-ca-rs` has `default = []` (`Cargo.toml:2`), which is *why* the
   no-`required-features` trick works there. `epics-pva-rs` has
   `default = ["tls", "pkcs12", "client"]` (`Cargo.toml`, `[features]` head),
   and `tls` pulls `getrandom 0.2`, which the manifest's own comment calls
   "the crate that does not build for RTEMS". A bare
   `cargo check --bin rtems-pva-ioc --target armv7-rtems-eabihf` therefore
   fails on dependencies, not on our code. §2.
4. **The zero-bridge claim holds, walked in code.** `PvDatabaseSource` →
   `DbSubscription::subscribe_with_mask` → `UpstreamMonitor::from_db` →
   `recv_event().await`, with the source comment stating "The subscription IS
   the stream: `marked_update` runs as the server pulls, so no task stands
   between the two" (`native_source.rs:877-882`). Nothing in that chain is
   gated or bridge-dependent. §4.
5. **Two corrections to my own earlier docs.** (a) `epics-pva-rs` declares
   **8** bins, not 6, and `mshim-rs` (`Cargo.toml:228-230`) has **no**
   `required-features` — so the "all bins are client-gated" statement in
   `doc/pva-rtems-stage-cd-design.md` §5.2 item 5 is wrong. (b) `mshim-rs`
   imports `server_native::udp::ForwardableDatagram` and `tokio::net::UdpSocket`
   (`mshim-rs.rs:35-37`), both RTEMS-gated, so the RTEMS command must stay
   `--bin rtems-pva-ioc` and must never become `--bins`.

---

## 1. `rtems-ca-ioc.rs` step by step, and the PVA equivalent

Template read in full: `crates/epics-ca-rs/src/bin/rtems-ca-ioc.rs`, 245 lines.

| # | CA step | CA `file:line` | PVA equivalent | Status |
|---|---|---|---|---|
| 1 | `background_init()` — callback bands, delayed timer, scanOnce worker (C `callbackInit`) | `:112` | **identical** — same `epics_base_rs::runtime::task::background_init` | exists |
| 2 | `load_database()` — argv are `.db` paths, else `DEMO_DB`; `block_on_sync(IocBuilder::build())` | `:80-84`, `:92-105`, `:115` | **identical, verbatim reusable** — `IocBuilder` is base-side and PVA-agnostic | exists |
| 3 | `db.all_record_names()`, sorted | `:122-124` | **identical** | exists |
| 4 | port + TCP bind: `cas_server_port()` → `BlockingCaServer::bind((0.0.0.0, port), db, acf)` | `:129-137` | `PvaServerConfig::default().with_env()` (`config.rs:409`) then `BlockingPvaServer::bind((0.0.0.0, port), source, config)` (`blocking.rs:829`) | **differs — 3 args, and the source must be built first** |
| 5 | UDP search bind: `bind_udp_search(0.0.0.0:port)` | `:138-144` | **no equivalent exists** — stage D | **missing; §5 shows it is not required** |
| 6 | thread `CAS-TCP` → `server.serve()` | `:146-155` | thread `PVAS-accept` → `server.serve()` (`blocking.rs:880`). Note `serve()` **applies `PVA_SERVER_PRIORITY` itself** (`:881`), where CA applies priority inside the per-client spawn (`server/blocking.rs:200`) | exists, slightly different shape |
| 7 | thread `CAS-UDP` → `server.serve_udp_search(udp)` | `:157-166` | **no equivalent** — stage D | **missing; skip for stage G** |
| 8 | banner + one line per record | `:168-176` | same shape, different text | trivial |
| 9 | `tcp_thread.join()` then `udp_thread.join()`; runs until killed | `:182-194` | one thread to join; `shutdown()` exists (`blocking.rs:971-978`) but, as in CA, nothing calls it | simpler |

### 1.1 What PVA needs that CA does not

| Need | Why CA has no analogue | Where it comes from |
|---|---|---|
| **A `PvaServerConfig`** | `BlockingCaServer::bind` takes `(addr, db, acf)` — no config object at all | `config.rs`, un-gated (`mod.rs:39`); `Default` `:337`, `with_env()` `:409`, `isolated()` `:385` |
| **A `DynSource`** | CA's server takes the `PvDatabase` directly | `DynSource = Arc<dyn ChannelSourceObj>` (`source.rs:2081`); the blanket `impl<T: ChannelSource + 'static> ChannelSourceObj for T` (`source.rs:2352`) is what lets `Arc::new(PvDatabaseSource::new(db))` coerce. Host precedent: `pva_server.rs:210` |
| **A GUID** | CA has no server-identity concept on the wire | **nothing supplies one on this path today** — §3 |
| **A protocol string** for SEARCH replies (`"tcp"`) | CA's reply carries no protocol field | passed into `build_search_response_proto` at `tcp.rs:5918` |
| **`--no-default-features` on every RTEMS command** | `epics-ca-rs` `default = []` | §2 |

### 1.2 What CA needs that PVA does not

- **The ACF cell.** CA threads `Arc<RwLock<Option<AccessSecurityConfig>>>`
  through `bind` (`rtems-ca-ioc.rs:130`). PVA's `PvDatabaseSource::new(db)`
  (`native_source.rs:46`) omits ACF entirely; `new_with_acf` (`:55`) is the
  opt-in. Stage G should use `new`, matching CA's permissive default, and say
  so in the banner.
- **A second socket.** With §5's name-server path, stage G binds exactly one
  socket. CA needs two (TCP + UDP) because CA has no TCP-search fallback.

### 1.3 What the binary does *not* have to do

`BlockingPvaServer::bind` already does inside itself what `accept.rs:69-70`
does on the host: constructs the `ChannelInvalidator` and calls
`source.set_channel_invalidator(...)` (`blocking.rs:836-840`). It also
constructs the `PeerRegistry` (`:846`) and the `ConnRegistry` (`:848`). So the
binary assembles **none** of stage 4's machinery — a real simplification
relative to what `doc/pva-rtems-stage-cd-design.md` §1.2 anticipated.

---

## 2. The `[[bin]]` entry and the `rtems-exec-model` feature

### 2.1 What it must look like

```toml
# The RTEMS PVA IOC entry point (design doc §9.5 / item 7 stage G).
# Deliberately NOT given `required-features`, for the reason
# epics-ca-rs/Cargo.toml:226-231 gives: a required-features gate makes cargo
# silently *skip* the target instead of building it, turning the RTEMS gate
# into a vacuous pass.
#
# UNLIKE epics-ca-rs (default = []), this crate's default features include
# `tls` -> getrandom 0.2, which does not build for RTEMS. So the RTEMS command
# MUST carry --no-default-features:
#   cargo +nightly check -p epics-pva-rs --bin rtems-pva-ioc \
#     --no-default-features -Zbuild-std=std,panic_abort \
#     --target armv7-rtems-eabihf
[[bin]]
name = "rtems-pva-ioc"
path = "src/bin/rtems-pva-ioc.rs"
```

and in `[features]`:

```toml
# Routes the `runtime::task` seam to the RTEMS executor backend on a host, so
# the RTEMS entry point is runnable and testable off-target. Mirrors
# epics-ca-rs/Cargo.toml:106. NOT in `default`.
rtems-exec-model = ["epics-base-rs/rtems-exec-model"]
```

### 2.2 What it must NOT do — five specific traps

1. **Must not carry `required-features = [...]`.** That is the whole point of
   the CA precedent (`epics-ca-rs/Cargo.toml:226-231`): cargo *skips*
   unsatisfied bins silently, so the RTEMS check would pass while building
   nothing. A loud dependency failure is strictly better than a silent skip.
2. **Must not be added to `default`.** `rtems-exec-model` in `default` would
   route the *hosted* task seam to the executor backend for every consumer of
   the crate.
3. **Must not rely on `--bins`.** `mshim-rs` has no `required-features`
   (`Cargo.toml:228-230`) and imports `server_native::udp` +
   `tokio::net::UdpSocket` (`mshim-rs.rs:35-37`), both RTEMS-gated. `--bins`
   on the RTEMS target fails on mshim, not on us. Always name the bin.
4. **Must not try to solve the default-features problem by narrowing
   `default`.** Cargo has no per-target feature defaults, and dropping `tls`
   from `default` would change every existing consumer's build. The honest fix
   is the documented `--no-default-features` command, and a CI line that runs
   exactly that command so it cannot rot.
5. **Must not gate the binary body on `target_os = "rtems"` alone.** Copy CA's
   predicate verbatim — `#[cfg(any(target_os = "rtems", feature =
   "rtems-exec-model"))]` (`rtems-ca-ioc.rs:60`, `:197`) with the refusing stub
   `main` for the hosted default (`:202-211`). That predicate is what makes
   §5's rung −1 possible at all.

### 2.3 Carry the guard test across

`rtems-ca-ioc.rs:214-245` is a source-inspection test asserting the file never
names `tokio::main` / `tokio::net` / `tokio::time` / `tokio::spawn` /
`Runtime::new` / `Builder::new_multi_thread` / `block_in_place` / `block_on(`,
assembled with `concat!` so the test body cannot self-match. Its rationale
(`:220-226`) applies identically to PVA: tokio's `rt` features survive on the
RTEMS target, so `cargo check` alone cannot catch a runtime constructor. Copy
it. `blocking.rs:993-999` already carries the same `production_scope` helper
idiom, so the crate has the pattern.

---

## 3. The GUID — traced end to end, and it is already broken

### 3.1 The chain

```
PvaServerConfig::default()          config.rs:343   guid: [0u8; 12]
  └─ with_env()                     config.rs:409   does NOT touch guid
BlockingPvaServer::bind(...)        blocking.rs:829 does NOT touch guid
  └─ (rg guid blocking.rs → 0 hits)
tcp.rs SEARCH handler               tcp.rs:5913-5919
  └─ build_search_response_proto(config.guid, ...)   ← ships [0u8;12]
```

The only assignment in the crate is `runtime.rs:224-231`
(`let guid = random_guid(); ... config.guid = guid;`) and `runtime.rs` is
`#[cfg(not(target_os = "rtems"))]` (`mod.rs:45-46`). The doc comment on the
field even says "The runtime fills this from `random_guid()`"
(`config.rs:47-49`) — which is precisely the assumption the blocking driver
breaks.

**So this is not a future RTEMS risk. A `BlockingPvaServer` built today, on
any platform, advertises GUID `000000000000`.**

### 3.2 Second defect: `random_guid` is unreachable on RTEMS anyway

`random_guid` lives at `udp.rs:47`, inside the RTEMS-gated module
(`mod.rs:58-59`). Its entropy helper is `try_fill_secure`, which under
`#[cfg(unix)]` reads `/dev/urandom` (`udp.rs:63-69`) and under
`#[cfg(not(unix))]` returns `false` (`udp.rs:70-72`).

**RTEMS is `target_family = ["unix"]`** (verified from
`rustc --print target-spec-json --target armv7-rtems-eabihf`), so it takes the
`/dev/urandom` arm — and if that file does not exist on the BSP,
`File::open(...).is_ok()` is simply `false` and the code falls **silently**
into the time+PID fallback (`udp.rs:52-59`):

```rust
let now = SystemTime::now().duration_since(UNIX_EPOCH)...;  // 8 bytes
let pid = std::process::id().to_le_bytes();                  // 4 bytes
```

On RTEMS/QEMU both inputs are near-constant: EPICS base sets the clock to a
**fixed** `1397460606` at boot when there is no RTC, and its comment says the
RTC "seems to be missing with libbsd and qemu"
(`epics-base/modules/libcom/RTEMS/posix/rtems_init.c:958-966`); and RTEMS is
single-process, so `process::id()` is a constant. **Two boots of the same image
would produce GUIDs differing only by elapsed ticks since boot** — and with a
10 ms tick (`rtems_config.c:33-35`) that is a handful of low bits.

So the GUID chain has **three** distinct problems, in increasing subtlety:

| # | Problem | Where | Detectable by |
|---|---|---|---|
| G1 | blocking driver never sets a GUID → all-zero | `blocking.rs:829-853` vs `runtime.rs:231` | a host unit test (§3.4) |
| G2 | `random_guid` is inside the RTEMS-gated module | `udp.rs:47` | `cargo check --target rtems` once G1 is fixed |
| G3 | the fallback is near-deterministic on RTEMS | `udp.rs:52-59` + fixed boot clock | only a two-boot comparison, or a host test that forces the fallback |

### 3.3 What a degenerate GUID actually does to a client

Not hypothetical — three concrete behaviours, read from pvxs:

1. **It silences the duplicate-PV-name diagnostic.** `client.cpp:938-943`: when
   a second SEARCH reply arrives for an already-connected channel, pvxs
   compares `chan->guid != guid` and logs
   `Duplicate PV name %s from %s and %s`. Two different servers both reporting
   GUID 0 compare *equal*, so the warning never fires and the operator never
   learns two IOCs are serving the same record.
2. **It defeats server-restart detection.** `client.cpp:807`: a beacon whose
   GUID differs from the cached one triggers the server-change path
   (`cur.guid != msg.guid || cur.peerVersion != msg.peerVersion`,
   `:807-824`). A constant GUID across reboots (G3) makes a restarted IOC look
   like the same server that never went away.
3. **It breaks gateway loop-avoidance.** `Context::ignoreServerGUIDs`
   (`client.cpp:454-460`, checked at `:881-883`) is how a PVA gateway refuses
   to search its own upstream. Ignoring one all-zero-GUID server ignores every
   other one too.

Our own client has the same semantics — `expected_guid` on the channel
(`client_native/channel.rs:57`, set from the reply at `:960-962` with a comment
citing pvxs `procSearchReply` parity) and a beacon tracker keyed on GUID
(`beacon_throttle.rs:33`, `:136`).

**All three failures are silent.** Nothing logs, nothing errors, and rungs 1–5
of an acceptance ladder would pass with an all-zero GUID.

### 3.4 The cheapest in-tree test — and it beats a reboot rung

A host test under `--features rtems-exec-model`, in the stage-G binary's own
`#[cfg(test)]` module, closes G1 outright and needs no guest:

```rust
#[test]
fn the_served_config_carries_a_nonzero_guid() {
    let config = build_server_config();          // the binary's own helper
    assert_ne!(config.guid, [0u8; 12],
               "SEARCH replies would advertise a null server identity");
}

#[test]
fn two_servers_in_one_process_get_distinct_guids() {
    assert_ne!(build_server_config().guid, build_server_config().guid);
}
```

The second one also catches G3's *mechanism* — a fallback keyed on a coarse
clock would collide when two configs are built in the same tick. That is
exactly the RTEMS failure, reproduced on a host in microseconds.

**But the test is the guard, not the fix.** Per the structural-over-patch rule,
the fix is to make the null GUID **unrepresentable**:

- **Preferred:** move `random_guid` + `try_fill_secure` out of the gated
  `udp.rs` into the un-gated `config.rs` — where the field they exist to fill
  already lives — and have `PvaServerConfig::default()` call it. Then no
  construction path can produce a zero GUID, `runtime.rs:231`'s manual
  assignment becomes redundant, and G1 and G2 close together. This is the same
  extract-don't-copy move `mod.rs:24-27` records for `tcp.rs`.
- **Weaker alternative:** have `BlockingPvaServer::bind` fill the GUID when it
  is zero. That is a runtime check on an illegal state rather than a
  construction that cannot produce it, and it leaves `PvaServerConfig` still
  able to represent a null identity. Mention it only as a fallback.

G3 remains open regardless and is `[VERIFY-ON-BOX]`: does the
`xilinx_zynq_a9_qemu` BSP present `/dev/urandom`? If not, the fallback needs a
better entropy source on RTEMS — `libc` declares `getentropy`
(`newlib/rtems/mod.rs:143`) and `arc4random_buf` (`:145`) for this target, so
the symbols exist to build one.

---

## 4. Does `PvDatabaseSource` really give a zero-bridge db-backed server?

Walked, not asserted.

**Gating.** `crates/epics-pva-rs/src/server/mod.rs:10` declares
`pub mod native_source;` with **no `cfg`**, where its neighbours `iocsh`
(`:8-9`) and `pva_server` (`:11-12`) are both `#[cfg(not(target_os = "rtems"))]`.
Confirmed again at this HEAD.

**Construction.** `PvDatabaseSource::new(db: Arc<PvDatabase>)`
(`native_source.rs:46`). `impl ChannelSource for PvDatabaseSource`
(`:583`). `Arc::new(...)` coerces to `DynSource` through the blanket impl at
`source.rs:2352`.

**The six operations a `pvxget`/`pvxinfo`/`pvxmonitor` needs, each traced:**

| Client op | `ChannelSource` method | `native_source.rs` | Reaches |
|---|---|---|---|
| SEARCH match | `searchable` | `:642` | `db.has_name…` |
| channel create | `has_pv` | `:626` | `db.find_entry` |
| `pvxinfo` (type only) | `get_introspection` | `:648` | record snapshot → `FieldDesc` |
| `pvxget` | `get_value` / `read_checked` | `:660` / `:687` | `snapshot_for(db, name)` `:572` |
| `pvxput` | `put_value` | `:708` | record processing |
| `pvxmonitor` | `subscribe_checked_opts_marked` | `:842` | see below |
| `pvxlist` | `list_pvs` | `:588` | `all_record_names` + `all_simple_pv_names` + `all_alias_names` |

**The monitor path in full** (`native_source.rs:865-882`):

```rust
let sub = DbSubscription::subscribe_with_mask(&db, &name, 0,
                                              crate::nt::monitor_mask().bits()).await?;
// The subscription IS the stream: `marked_update` runs as the server pulls,
// so no task stands between the two.
Some(MonitorStream::Upstream(UpstreamMonitor::from_db(sub, marked_update)))
```

`UpstreamMonitor::from_db` (`source.rs:1912-1921`) stores
`UpstreamSub::Db(sub)` and a plain `fn` mapper; `recv` (`:1945-1955`) awaits
`s.recv_event()` directly. **No spawned task, no channel, no bridge type**, and
the comment at `:866-870` records that the mask is the union of pvxs's two
subscriptions so DBE_PROPERTY events arrive.

**Against `DEMO_DB`'s three records** (`rtems-ca-ioc.rs:80-84` — `ao`,
`longout`, `stringout`): all three are ordinary records reached through
`db.find_entry` → the record arm, i.e. the `DbSubscription` path above, not the
`PvEntry::Simple` arm at `:886+`. The NT mapping is exercised by existing tests
in the same file — `numeric_nt_declares_display_form_enum` (`:1535`),
`integer_nt_limits_take_the_value_type` (`:1581`),
`string_nt_omits_control_value_alarm_and_numeric_display` (`:1616`) — which
between them cover exactly the double / integer / string shapes `DEMO_DB`
produces.

**Verdict:** the §5 claim from the stage-C/D doc holds. Zero bridge dependency,
zero gated module, and the RTEMS `--lib` check compiles all of it (exit 0
above). The `bridge-rtems-walls.md` finding
(`epics-bridge-rs/Cargo.toml:93`'s unconditional `tokio/full`) does not touch
this path.

**One caveat worth stating:** `AcfCell = Arc<RwLock<…>>` at
`native_source.rs:30` is a **tokio** `RwLock` (`use tokio::sync::RwLock` at
`:21`). It is lock L23 in `doc/pi-lock-evaluation.md` §8 — the sole
park_on-invisible row in that sweep. Using `PvDatabaseSource::new` (no ACF)
rather than `new_with_acf` keeps it out of the stage-G path entirely.

---

## 5. The rung −1 equivalent for PVA

### 5.1 Discovery: no UDP needed

Our TCP SEARCH handler is at `tcp.rs:5860-5920` — `parse_search_request`
(`:5866`), `matched_cids_for_requester` (`:5908`), reply gated on
`!matched.is_empty() || req.must_reply` (`:5912`, pvxs `serverchan.cpp:240-249`
parity), `build_search_response_proto` (`:5913`). It is in the un-gated `tcp`
module (`mod.rs:57`).

pvxs reads `EPICS_PVA_NAME_SERVERS` into `Config::nameServers`
(`config.cpp:589-590`), and `client.cpp:621-633` / `:676-681` establishes a TCP
connection to each and searches over it. Our own client does the same —
`config::env::name_servers()` (`context.rs:168`), builder
`name_servers(...)` (`:218-225`), fed to the SearchEngine as persistent search
peers (`:385-386`).

**Consequence:** the whole PVA ladder runs over one TCP port. Under QEMU that is
`hostfwd=tcp:127.0.0.1:5075-:5075` and nothing else — no UDP forward, no
broadcast question, and §0 finding 4 of the acceptance plan is not even needed
for PVA.

### 5.2 The golden capture, once the binary exists

Mirror `doc/rtems-acceptance-golden.txt`'s structure. Use a non-standard port —
never bind 5075/5076 in a test.

```
# terminal 1 — the IOC under test, on a host, exec-model backend
EPICS_PVAS_SERVER_PORT=15075 \
  cargo run -p epics-pva-rs --bin rtems-pva-ioc \
    --no-default-features --features rtems-exec-model

# terminal 2 — every host-side command, all TCP-only
export EPICS_PVA_AUTO_ADDR_LIST=NO
export EPICS_PVA_NAME_SERVERS=127.0.0.1:15075

pvxinfo   RTEMS:AO RTEMS:LO RTEMS:MSG        # (a) type + GUID
pvxget    RTEMS:AO RTEMS:LO RTEMS:MSG        # (b) values
pvxget -F tree RTEMS:AO                      # (c) full structure WITH type id
pvxmonitor RTEMS:LO &                        # (d) baseline update
pvxput    RTEMS:LO 42                        # (e) update propagates
pvxget    RTEMS:LO                           # (f) readback negative control
```

**Capture (a)–(f) verbatim into `doc/pva-acceptance-golden.txt`**, then have
every guest-side rung assert equality modulo the port. Repeat the same six with
our own tools (`pvinfo-rs`, `pvget-rs`, `pvmonitor-rs`, `pvput-rs`, which need
the `client` feature, so a **second** host build) so the golden file records
both client implementations — a divergence between them on the same server is
itself a finding.

### 5.3 Three things the CA golden run taught us that apply here

1. **Do not predict the formatting.** The CA capture retired an UNFIXED item by
   showing `caget` prints `1.5`, not `1.500` — `PREC=3` does not apply to a
   default `DBR_DOUBLE` request. The PVA analogue is `display.precision`, and
   whether `pvxget`'s default output honours it is exactly the kind of thing to
   *capture*, not assert. (Relevant history: `pvxs` PR #196 fixed a
   `display.precision` drop upstream — see the CBUG-G1 note.)
2. **The first monitor update is `<undefined> … UDF NO_ALARM`, and that is
   correct.** A record that has never been processed is UDF. The PVA analogue
   is an initial monitor whose `alarm.status` reflects UDF/INVALID and whose
   timestamp is the zero/boot time. **Assert it as the baseline; do not chase
   it.** This is the single most likely thing to be misread as a stage-G bug.
3. **Keep the readback negative control.** `pvxput` then `pvxget` proves the
   monitor update came from record processing, not from a client-side cache.

### 5.4 The type-id trap — mandatory, per [[pvxget-delta-omits-top-struct-id]]

`pvxget`'s **default Delta output omits the top-level structure id**: a GET
reply never sets the root bit, so `epics:nt/NTScalar:1.0` does not appear.
Any rung that asserts the NT type **must** use `pvxget -F tree` or `pvxinfo` —
line (c) above exists solely for this. Asserting the type from default `pvxget`
output produces a false negative that looks exactly like a server-side NT
mapping bug, and would cost a day on the guest where it is hardest to debug.

### 5.5 One extra PVA-only rung

**GUID distinctness across restarts.** Kill and restart the IOC, re-run
`pvxinfo`, assert the reported GUID **differs**. On a host this is seconds; on
the guest it is a reboot. Given §3, run it on the host first — and note that
with G1 unfixed this rung fails immediately and correctly, which makes it the
cheapest possible proof that G1 is real.

---

## 6. Ordered plan

1. **Fix the GUID structurally** (§3.4): move `random_guid`/`try_fill_secure`
   into un-gated `config.rs`, fill it in `PvaServerConfig::default()`, add the
   two host tests. Closes G1 + G2. Do this **before** the binary, so the binary
   is never written against a broken default.
2. **Add `rtems-exec-model` to `epics-pva-rs`** (§2.1) and confirm
   `cargo nextest run -p epics-pva-rs --features rtems-exec-model` is green.
3. **Write `src/bin/rtems-pva-ioc.rs`** as CA's steps 1–4, 6, 8, 9 with step 5
   and 7 omitted (§1), plus the copied source-inspection guard test (§2.3).
4. **Add the `[[bin]]` entry** with the manifest comment spelling out the
   `--no-default-features` asymmetry (§2.1), and a CI line running exactly that
   RTEMS command.
5. **Rung −1 on the host** (§5.2) → `doc/pva-acceptance-golden.txt`, including
   the restart-GUID rung (§5.5) and the `-F tree` type assertion (§5.4).
6. **Only then** stage D (UDP search responder), which upgrades discovery from
   name-server-only to broadcast, retires W4, and needs its own priority
   constant (pvxs `udp_collector.cpp:93` = `CAServerLow-4` = **16**; the
   existing `PVA_SERVER_PRIORITY` is `Custom(18)` at `blocking.rs:144`, so 16
   is a *new* constant, not a reuse).

---

## 7. Report

**Tested:**

- `git rev-parse HEAD` at start and end — pass (`1c27465c`, unchanged)
- `git status --porcelain` at start and end — pass (clean both times)
- `git merge-base --is-ancestor 1c27465c HEAD` — pass (stage C is HEAD)
- `cargo +nightly check -p epics-pva-rs --lib --no-default-features -Zbuild-std=std,panic_abort --target armv7-rtems-eabihf` — **pass, exit 0, 2 warnings** (W1 `tcp.rs:1404`, W4 `search.rs:53`). Stage C retired the two `peers.rs` warnings as predicted
- Read `crates/epics-ca-rs/src/bin/rtems-ca-ioc.rs` in full (245 lines) — pass
- `BlockingPvaServer` surface read (`blocking.rs:810-978`) — pass
- `rg guid crates/epics-pva-rs/src/server_native/blocking.rs` — pass, **zero hits** (the G1 evidence)
- GUID assignment audit workspace-wide — pass (sole production assignment `runtime.rs:231`, host-only; default `[0u8;12]` `config.rs:343`)
- `random_guid`/`try_fill_secure` read (`udp.rs:47-72`) — pass
- pvxs GUID semantics read (`client.cpp:807-824`, `:881-883`, `:918-943`, `:454-460`) — pass
- Our client GUID semantics read (`client_native/channel.rs:950-962`, `beacon_throttle.rs:33`,`:136`) — pass
- `PvDatabaseSource` → `DbSubscription` → `UpstreamMonitor::from_db` walked (`native_source.rs:842-882`, `source.rs:1912-1955`) — pass
- `server/mod.rs` gate audit — pass (`native_source` un-gated at `:10`)
- `DynSource` coercion path — pass (`source.rs:2081`, blanket impl `:2352`, host precedent `pva_server.rs:210`)
- `epics-pva-rs` feature/bin audit — pass (`default = ["tls","pkcs12","client"]`; **8** bins; `mshim-rs` has no `required-features`)
- `epics-ca-rs` feature audit — pass (`default = []` — the asymmetry that breaks the CA precedent)
- TCP SEARCH path — pass (`tcp.rs:5860-5920`)
- Name-server support both clients — pass (pvxs `config.cpp:589-590`, `client.cpp:621-633`; ours `context.rs:168`,`:218-225`)
- `PVA_SERVER_PRIORITY` — pass (`blocking.rs:144` = `Custom(18)`, matching the corrected pvxs parity)
- EPICS base fixed-boot-clock evidence for G3 — pass (`rtems_init.c:958-966`, `rtems_config.c:33-35`)

**Failed:** none.

**UNFIXED:**

- **G1 — `BlockingPvaServer` serves an all-zero GUID.** Present on every
  platform at this HEAD, not only RTEMS. Not fixed: read-only task. §3.4 gives
  the structural fix and the two host tests.
- **G2 — `random_guid` is unreachable from the RTEMS build** (gated `udp.rs:47`).
- **G3 — the RTEMS entropy fallback is near-deterministic** (fixed boot clock +
  constant PID). `[VERIFY-ON-BOX]`: whether `/dev/urandom` exists on
  `xilinx_zynq_a9_qemu`.
- **W4 remains** (`search.rs:53`, five dead `SearchRequest` fields) — stage D's,
  unchanged and expected.
- **Correction owed to `doc/pva-rtems-stage-cd-design.md` §5.2 item 5**: it says
  all six bins are `required-features = ["client"]`. There are **8** bins and
  `mshim-rs` has none. That doc is on `main`, outside this read-only scope.
- **Stage D still needs a second priority constant** (16, per
  `udp_collector.cpp:93`); `PVA_SERVER_PRIORITY` = 18 is not it.

**Fixed:** none — no source file was edited, no commit was made, as instructed.
