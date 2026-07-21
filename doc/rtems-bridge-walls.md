# `epics-bridge-rs` RTEMS walls — settling §9's "status undeclared"

Measurement, not a change. No files edited, nothing reverted (nothing was
modified). Format follows §8.1.1/§8.1.2 of `doc/rtems-runtime-portability-design.md`.

**Provenance.** Integration worktree
`/home/stevek/work/epics-rs/.caucus/worktrees/integration` @ **`af2d5d16`**,
branch `integration/rtems-scope-b`. HEAD `af2d5d16` and a **clean** working tree
at start *and* end — the concurrent panel did not touch the tree during this
run, so no citation needed re-verification. Toolchain: `nightly-x86_64-unknown-linux-gnu`
with `rust-src` installed; `armv7-rtems-eabihf` is a known nightly target.

---

## §0 Answer up front

**The bridge does not build for RTEMS, and it is nobody's blocker today.**
Three runs, all exit 101:

| run | features | failing crates | errors |
|---|---|---|---|
| 1 | default (`qsrv`, `qsrv-bin`, `ca-gateway`, `ca-gateway-bin`) | getrandom, signal-hook-registry, socket2, mio | 53 |
| 2 | `--no-default-features` | signal-hook-registry, socket2, mio | 56 |
| 3 | `--no-default-features --features qsrv` | getrandom, signal-hook-registry, socket2, mio | 53 |

**Every wall is a dependency wall. Not one is in bridge source.** And the root
cause is a single line.

---

## §1 The wall table

| Crate (ver) | Error class | Dependency path (from `epics-bridge-rs`) | Remediation class |
|---|---|---|---|
| **mio 1.2.0** | 29 errs — `cannot find Selector in sys`, `cannot find event in sys`, `&mut sys::Events` (`mio-1.2.0/src/poll.rs:323`, `src/event/event.rs:24`, `src/event/events.rs:189`) — no epoll/kqueue selector | `mio ← tokio (net) ← ` **`epics-bridge-rs` directly** (`Cargo.toml:93`) | **target-gate the dep** — split `tokio` per-target exactly as base/ca/pva already do |
| **socket2 0.6.3** | 20 errs — `cannot find IovLen in this scope` (`sys/unix.rs:743`), `recvmsg`/`sendmsg` not in `libc` (`:1128`, `:1181`), `ip_mreqn` missing (`:1405`, `:1407`, `:1412`) | `socket2 ← tokio (net) ←` **`epics-bridge-rs`** (`Cargo.toml:93`) — note this is socket2 **0.6**, pulled by tokio, *not* the 0.5 that base/ca pin directly | **target-gate the dep** — same split; disappears with `net` |
| **signal-hook-registry 1.4.8** | 4 errs — `unresolved import libc::siginfo_t` (`lib.rs:83`), `cannot find SA_RESTART` (`:187`), plus `:181`, `:267` | `signal-hook-registry ← tokio (signal) ←` **`epics-bridge-rs`** (`Cargo.toml:93`) | **target-gate the dep** — same split; disappears with `signal` |
| **getrandom 0.2.17** | 1 err — `target is not supported` (`lib.rs:351`, an explicit `compile_error!` for unknown targets) | `getrandom ← {p12, ring} ← {rustls, tokio-rustls} ← epics-pva-rs` **with default features** (`Cargo.toml:102` `epics-pva-rs = { workspace = true, optional = true }` — inherits pva's `default = ["tls", "pkcs12", "client"]`) | **target-gate the dep** — `default-features = false` for pva on RTEMS; identical in kind to phase-6 item 1's TLS gate (`1d5476df`) |

**No in-crate walls.** §8.1.2's bridge-equivalent rows (`getifaddrs`,
`getgrouplist`) have no counterpart here: the bridge calls no raw libc.

---

## §2 Root cause — one line, proved

`crates/epics-bridge-rs/Cargo.toml:93`:

```toml
tokio = { version = "1", features = ["full"] }
```

It sits in the **plain `[dependencies]`** table. Every sibling crate splits the
same dependency per target:

* `epics-base-rs/Cargo.toml:43-44` host `full`, `:49-` RTEMS `fs, io-util, io-std, macros, parking_lot, rt, …` (no `net`/`signal`/`process`)
* `epics-ca-rs/Cargo.toml:69-70` / `:74-`
* `epics-pva-rs/Cargo.toml:139-142` / `:144-`, whose comment states the reason verbatim: *"RTEMS gets tokio without `net`/`signal`/`process`: `net` pulls mio (29 errors …) and `signal` pulls signal-hook-registry (4 errors …)"*

Because **Cargo unions features across the whole graph**, one unconditional
`features = ["full"]` re-enables `net`, `signal` and `process` for every crate in
the build — silently undoing `bc7c8f53`'s per-target split.

Proved directly, not inferred (`cargo tree -p epics-bridge-rs
--no-default-features --target armv7-rtems-eabihf -i tokio -e features`):

```
├── tokio feature "libc"
│   ├── tokio feature "net"
│   │   └── tokio feature "full"
│   │       └── epics-bridge-rs          ← sole enabler
│   ├── tokio feature "process" …
│   └── tokio feature "signal" …
```

`epics-base-rs` appears in that tree contributing only `io-util`, `fs`,
`io-std` — the correct RTEMS-safe subset. **`epics-bridge-rs` is the only crate
in the workspace that enables `tokio/full` unconditionally.**

---

## §3 Is dep-gating alone enough? — measured, and yes

The question §9 could not answer without building. The decisive measurement is
whether bridge *source* uses the APIs the RTEMS tokio split removes:

| module | `tokio::net` / TcpListener / TcpStream / UdpSocket | `tokio::signal` | `tokio::process` | src lines |
|---|---|---|---|---|
| **qsrv** | **0** | **0** | **0** | 20,993 |
| **pvalink** | **0** | **0** | **0** | 10,664 |
| **pva_gateway** | **0** | **0** | **0** | 11,032 |
| **ca_gateway** | 5 | 2 | 0 | 12,838 |

Only `ca_gateway` touches them, at three production sites (test boundaries
`pvlist.rs:925`, `server.rs:1366` — all three are below theirs, i.e. production):

* `ca_gateway/pvlist.rs:244` `tokio::net::lookup_host(...)`
* `ca_gateway/server.rs:1219` `tokio::signal::ctrl_c()`
* `ca_gateway/server.rs:1285` `use tokio::signal::unix::{SignalKind, signal}`

(The other four hits are a doc comment at `upstream.rs:2036` and two test helpers
at `:2050`-`:2051` using `std::net`, plus a doc comment at `pvlist.rs:179`.)

**And the module gating already exists.** `lib.rs` puts every subsystem behind
its own feature:

```
:69  #[cfg(feature = "qsrv")]        pub mod qsrv;
:72  #[cfg(feature = "ca-gateway")]  pub mod ca_gateway;
:75  #[cfg(feature = "pvalink")]     pub mod pvalink;
:82  #[cfg(feature = "pva-gateway")] pub mod pva_gateway;
```

So excluding the one offending module needs **no new `cfg` work** — only not
selecting its feature.

**Conclusion: qsrv-on-RTEMS is reachable by dep-gating alone, exactly as
§8.1.1 was for base.** Three Cargo.toml edits, zero source changes:

1. Split `tokio` per-target in `epics-bridge-rs/Cargo.toml:93`, copying the
   `epics-pva-rs:139-146` block verbatim.
2. Give `epics-pva-rs` (`:102`) `default-features = false` on RTEMS — drops
   `tls`/`pkcs12` (→ getrandom) and `client` (→ the 23,853-line client surface
   §8.1.2 already measured as feature-gated).
3. Select `qsrv` without `ca-gateway`/`ca-gateway-bin` for the RTEMS build.

I did **not** verify this reaches exit 0 — that needs the edits, which are out of
scope here. What is measured is that (a) all four walls are dep-only, (b) the
qsrv+pvalink path uses none of the removed APIs, and (c) the module gates are
already in place. The residual risk is a wall that only appears *after* these
four are cleared, which is exactly how §8.1.2's own walk proceeded.

---

## §4 Minimal RTEMS bridge subset

| keep | why |
|---|---|
| `qsrv` (20,993 lines) | the point of the exercise — db-backed PVA with group support; 0 dropped-API uses |
| `pvalink` (10,664) | `qsrv` pulls it unconditionally (`Cargo.toml:28-29`), and pvxs ties pvalink to QSRV2 the same way; 0 dropped-API uses |
| `convert`, `error` (root, 1,130) | `#[cfg(any(feature = "qsrv", feature = "pvalink"))]` (`lib.rs:66`) |

| gate | why |
|---|---|
| `ca_gateway` (12,838) | the only module using `tokio::net`/`signal`; a CA fan-out gateway on an RTEMS IOC is not a use case |
| `pva_gateway` (11,032) | same rationale; 8 of §9's locks live here and none has an RTEMS story |
| `qsrv-bin`, `ca-gateway-bin`, bins (2,723) | `clap`/`tracing-subscriber` daemons; RTEMS uses the `rtems-*-ioc` entry-point shape instead |

**Compile surface:** 32,787 of 59,380 src lines (**55%**) would compile for
RTEMS — comparable to `epics-pva-rs`'s 46% in §8.1.2, and higher because the
bridge has no 21k-line protocol engine to gate.

---

## §5 Who needs this, and when — nobody yet

`rg "epics-bridge-rs" --glob 'Cargo.toml'` across the workspace gives every
inbound edge:

* **examples** — `qsrv-ioc`, `mini-beamline`, `xrt-beamline`, `mqtt-ioc`,
  `modbus-ioc`, `scope-ioc` (all behind each example's `ioc` feature)
* **`crates/epics-rs`** — facade, optional `bridge` feature (`Cargo.toml:14`, `:27`)
* **`crates/ad-plugins-rs`** — optional, `pva` feature (`Cargo.toml:30`, `:40`)

**Not** `epics-base-rs`, **not** `epics-ca-rs`, **not** `epics-pva-rs`.

The only RTEMS binary that exists today is `rtems-ca-ioc`
(`epics-ca-rs/Cargo.toml:233-234`), and `epics-ca-rs` does not depend on the
bridge. `rtems-pva-ioc` — item-7 stage G — **does not exist yet**
(`rg "rtems-pva-ioc" --glob 'Cargo.toml'` → nothing).

So the honest status line, replacing §9's "undeclared":

> `epics-bridge-rs` does not compile for RTEMS and no RTEMS target requires it.
> It becomes a blocker only if item-7 stage G's `rtems-pva-ioc` chooses
> **group-aware** db-backed PVA (Q:group, ASG) over plain single-record PVA.

**And stage G may not need it at all.** `epics-pva-rs` already ships db-backed
PVA without the bridge: `server/native_source.rs` `PvDatabaseSource` +
`UpstreamMonitor::from_db` (`server_native/source.rs:1912-1921`) serve records
straight off `epics_base_rs` `DbSubscription`, and `server/` is not RTEMS-gated
(only `server_native::{accept,peers,runtime,tcp,udp}` are). A first
`rtems-pva-ioc` can therefore serve records with **zero** bridge dependency; the
bridge is the increment that adds groups.

---

## §6 Recommendation

1. **Do the three Cargo.toml gates now anyway** (§3). They are cheap, they are
   pure parity with what base/ca/pva already did, and item 2 in particular
   (`tokio/full`) is a latent hazard for *any* target-restricted build, not just
   RTEMS — today the bridge silently re-enables `net`/`signal`/`process` for the
   entire workspace graph whenever it is in the build.
2. **Sequence stage G as plain-PVA-first**: `rtems-pva-ioc` on
   `PvDatabaseSource`, no bridge. That keeps stage G off this dependency
   entirely and defers the bridge question until groups are actually wanted.
3. **Close §9's other finding at the same time.** With the bridge declared
   RTEMS-buildable for the `qsrv` subset, its 40+ raw `tokio::spawn` /
   `tokio::runtime::Handle` sites (`pvalink/integration.rs` alone: 42, against
   one `runtime::task` seam use at `qsrv/pva_adapter.rs:1436`) stop being
   hypothetical and become a real seam audit — and `pvalink` is in the *keep*
   column, so that audit is on the critical path for a group-aware
   `rtems-pva-ioc`, not optional.

Ordering: (1) is independent and can land any time; (3) gates (2)'s group-aware
variant, not its plain variant.

---

## §7 What this measurement does not establish

* **No exit-0 proof.** I measured the walls and the source-level readiness; I did
  not apply the gates, so "dep-gating alone suffices" is a well-evidenced
  prediction, not a verified build.
* **`--lib` only**, per the brief. The bins (`qsrv-rs`, `ca-gateway-rs`) pull
  `clap` + `tracing-subscriber` and were not probed.
* **The seam audit (§6 item 3) was not performed** — §9 counted the sites; the
  question of whether each is reachable on RTEMS is open.
* **Feature flags that change the outcome, for the record:** `--no-default-features`
  removes the getrandom wall (drops the `epics-pva-rs` edge entirely);
  `--features qsrv` restores it (pva returns with default `tls`/`pkcs12`). The
  mio/socket2/signal-hook-registry trio is present in **all three** runs because
  `Cargo.toml:93` is unconditional and no feature flag can reach it.

**Logs:** `rtems-default.log`, `rtems-nodefault.log`, `rtems-qsrv.log` in this
scratchpad directory.
