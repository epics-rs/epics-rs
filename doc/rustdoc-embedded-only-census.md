# Embedded-only rustdoc census

Which rustdoc citations in `epics-libcom-rs`, `epics-base-rs`, `epics-ca-rs`
and `epics-pva-rs` are invisible to every host doc run, so the owners of those
crates can size what the `rustdoc-embedded` CI job (107d9ed0) still costs them
after their host sweep is green.

Measurement only — no source in those four crates was edited to produce it.
Tree state: `1b076986`, 2026-07-27, rustc/cargo nightly `1.99.0-nightly
(59800466c 2026-07-07)`, host `x86_64-unknown-linux-gnu`.

## Method

A citation is keyed by `file:line:col` plus its message, so the same message at
two sites counts twice and one site reported by both targets counts once.

Host set `H` is the union of two runs, because "host" is not one configuration:

    RUSTDOCFLAGS="-D warnings" cargo doc -p <crate> --no-deps
    RUSTDOCFLAGS="-D warnings" cargo doc -p <crate> --no-deps --lib \
      --no-default-features [--features client-core for epics-ca-rs]

The second row matters: it is the feature selection the embedded job uses, run
on the host, so a citation that is really about *features* does not get
misfiled as target-gated. For `epics-pva-rs` that row is the whole difference
between "0 host errors" and 10.

Target sets `R` and `V` are the CI job's invocation verbatim:

    RTEMS_USE_STOCK_SPEC=1 RUSTDOCFLAGS="-D warnings" cargo +nightly doc \
      --no-deps --no-default-features -Zbuild-std=std,panic_abort \
      --target armv7-rtems-eabihf -p <crate> --lib [features]

    RUSTDOCFLAGS="-D warnings" cargo +nightly doc --no-deps \
      --no-default-features --config <libc-std-patch.sh lines> \
      -Zbuild-std=std,panic_abort --target x86_64-wrs-vxworks -p <crate> --lib [features]

`RTEMS_USE_STOCK_SPEC=1` is required, not cosmetic: `.cargo/config.toml`
installs `scripts/rtems-rustc-wrapper.sh`, which reroutes the builtin triple
through the generated has-thread-local spec. rustdoc is not wrapped, so
without the variable `-Zbuild-std` compiles std for the hashed spec triple
while rustdoc asks for the builtin one and the run dies in 1055 `E0461`s that
say nothing about documentation. The VxWorks row snapshots and restores
`Cargo.lock` around the config-level libc patch.

Embedded-only is `(R ∪ V) \ H`.

## Counts

| crate | host default | host gate-flags | RTEMS | VxWorks | **embedded-only** | of those, RTEMS-only / VxWorks-only |
|---|---|---|---|---|---|---|
| epics-libcom-rs | 6 | 6 | 14 | 13 | **11** | 2 / 1 |
| epics-base-rs | 14 | 14 | 14 | 14 | **0** | 0 / 0 |
| epics-ca-rs | 9 | 9 | 12 | 12 | **3** | 0 / 0 |
| epics-pva-rs | 0 | 10 | 50 | 50 | **40** | 0 / 0 |
| **total** | 29 | 39 | 90 | 89 | **54** | 2 / 1 |

`epics-base-rs` is the one crate whose target sets are exactly its host set:
closing its 14 host citations closes its embedded rows with them, at no extra
cost. `epics-pva-rs` is the opposite — 40 of its 50 target citations exist in
no host configuration, so a green host sweep there leaves 80% of the target
work untouched.

50 of the 54 are `unresolved link`; the remaining 4 are
`public documentation … links to private item`.

## Where they sit

| crate | file | embedded-only |
|---|---|---|
| epics-libcom-rs | `src/net/mod.rs` | 7 |
| epics-libcom-rs | `src/runtime/task.rs` | 3 |
| epics-libcom-rs | `src/runtime/sync.rs` | 1 |
| epics-ca-rs | `src/server/stats.rs` | 2 |
| epics-ca-rs | `src/server/mod.rs` | 1 |
| epics-pva-rs | `src/config/env.rs` | 6 |
| epics-pva-rs | `src/server_native/mod.rs` | 6 |
| epics-pva-rs | `src/server_native/config.rs` | 5 |
| epics-pva-rs | `src/server/mod.rs` | 4 |
| epics-pva-rs | `src/server_native/blocking.rs` | 4 |
| epics-pva-rs | `src/server_native/source.rs` | 3 |
| epics-pva-rs | `src/server_native/peers.rs` | 2 |
| epics-pva-rs | `src/server_native/search.rs` | 2 |
| epics-pva-rs | `src/server_native/search_engine.rs` | 2 |
| epics-pva-rs | `src/server_native/tcp.rs` | 2 |
| epics-pva-rs | `src/cli.rs`, `src/server_native/{composite,server_info,shared_pv}.rs` | 1 each |

## Why they are embedded-only

One cause covers nearly all of them: a module doc that is compiled on every
target links a sibling module that is not. The gated siblings are
`#[cfg(not(epics_embedded_target))]` — `net::{async_udp_v4, iface_map,
loopback_mcast}` in epics-libcom-rs (`net/mod.rs:31-36`), `server::ca_server`
in epics-ca-rs (`server/mod.rs:15-16`), and `server_native::{accept, udp,
runtime}` in epics-pva-rs (`server_native/mod.rs:30-66`) — while the doc that
names them lives in the ungated parent. `PvaServer`, `run_pva_ioc`, `iocsh`
and `PvaServerConfig` are re-exports of those same gated modules, which is why
`config/env.rs` and `server/mod.rs` carry six and four.

The three target-specific ones are the priority map: `map_epics_priority_rtems`
and `map_epics_priority_vxworks` are each private and each compiled only on its
own target, so `DEFAULT_POLICY`'s doc cites a different private item per
target; `PriorityInheritanceMutexGuard` in `runtime/sync.rs` is RTEMS-only for
the same reason.

## Rules that closed the same family elsewhere

From the sweep of the 29 crates outside these four (commits `9cad4c35`
… `1b076986`):

- target private, or exists only under a configuration the doc run does not
  compile → code span. A code span resolves in every configuration, which is
  what makes the fix hold across the feature and target matrix rather than in
  the one run that reported it.
- path is dead but a real item owns the behaviour → relink to that item.
- markdown reads `[mm]`, `[0]`, `[index]` as reference links and `<CONFIG>` as
  an HTML tag → code span (or a ```text fence for a whole formula).

A citation is a sample: in that sweep the strings behind 45 bridge citations
occurred 91 times, and `serve_connection_blocking` was cited 5 times and
occurred 8.
