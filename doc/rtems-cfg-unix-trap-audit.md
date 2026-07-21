# The `cfg(unix)` trap — workspace audit

**Measured at** integration worktree `/home/stevek/work/epics-rs/.caucus/worktrees/integration`,
HEAD **`8660074d`**, working tree **clean** (`git status --porcelain` empty).
Read-only: no source file was edited, nothing was committed.

**The trap.** `armv7-rtems-eabihf` declares `target-family: ["unix"]`. So
`#[cfg(unix)]` is **true** on RTEMS. A bare `cfg(unix)` arm therefore hands
RTEMS a Linux/BSD-shaped path that (a) compiles clean under
`cargo check --target armv7-rtems-eabihf`, (b) is never executed on the host
CI, and (c) on target either fails or — worse — succeeds degenerately. The
GUID defect (`/dev/urandom` under bare `cfg(unix)`) was one instance. This
document is the population.

---

## 0. Scoping — which crates can reach an RTEMS image

The RTEMS dependency closure is exactly three workspace crates:

| crate | why in scope |
|---|---|
| `epics-base-rs` | `epics-ca-rs/Cargo.toml:11`, `epics-pva-rs/Cargo.toml:11` |
| `epics-ca-rs` | hosts `src/bin/rtems-ca-ioc.rs`, the only RTEMS bin today |
| `epics-pva-rs` | stage C `BlockingPvaServer`; stage G bin is planned |

`epics-macros-rs` is a proc-macro crate — it runs on the **host** compiler by
nature and never enters the target image.

**Out of scope by crate (never compiles for RTEMS, so every arm inside is
DISTINCT):** `ad-core-rs`, `ad-plugins-rs`, `asyn-rs`, `epics-bridge-rs`,
`epics-rs`, `motor-rs`, `std-rs`, `scaler-rs`, `optics-rs`, `mca-rs`,
`mqtt-rs`, `modbus-rs`, `epics-tools-rs`, plus `examples/*`,
`tools/dbd-codegen`, `tools/env-codegen`. (Anchor-4 hits exist in
`epics-tools-rs/src/procserv/*` and `asyn-rs/src/drivers/serial_port.rs`;
they are excluded here on that basis, not because they were judged portable.)

### 0.1 Module-level RTEMS gates inside the three in-scope crates

These are the gates that make most hits unreachable. Each was read, not
inferred:

| gate site | modules cut out on RTEMS |
|---|---|
| `epics-ca-rs/src/lib.rs:29,34,37,39,43,45,50,55` | `calink`, `chaos`, `cli`, `client`, `copt`, `discovery`, `hostname`, `repeater` |
| `epics-ca-rs/src/server/mod.rs:11,14,16,23` | `beacon`, `ca_server`, `introspection`, `signed_beacon` |
| `epics-base-rs/src/net/mod.rs:28,30,32` | `async_udp_v4`, `iface_map`, `loopback_mcast` |
| `epics-pva-rs/src/server_native/mod.rs:29,45,64` | `accept`, `runtime`, `udp` |
| `epics-pva-rs/src/server/mod.rs:8,11` | `iocsh`, `pva_server` |
| `epics-pva-rs/src/lib.rs:22,24` (feature, **not** target) | `client`, `client_native` |

**Un-gated and therefore live on RTEMS:** all of `epics-base-rs/src/server/*`
and `epics-base-rs/src/runtime/*`; `epics-ca-rs::server::{addr_list, blocking,
ioc_app, iocsh, monitor, outbox, tcp, udp, stats, rate_limit, access_token}`;
`epics-pva-rs::{auth, cli, codec, config, decode, format, log, nt, proto,
pv_request, pvdata, service, util}`, `server::native_source`,
`server_native::{blocking, composite, config, monitor_control, op_handle,
peers, search, search_engine, server_info, shared_pv, source, tcp}`.

**A caveat the reader should weigh.** `client`/`client_native` are cut by a
**Cargo feature**, not by `target_os`. `client` is a *default* feature
(`epics-pva-rs/Cargo.toml` `default = ["tls","pkcs12","client"]`), so the
RTEMS build is only safe because it passes `--no-default-features`. Cargo
feature unioning means any future workspace member that depends on
`epics-pva-rs` with `client` on, and is co-built for RTEMS, silently pulls
those seven arms back into scope. That is weaker protection than a
`cfg(not(target_os = "rtems"))` and is noted per-hit below.

---

## 1. Findings — SAME DEFECT, ranked SILENT first

Three hits. Two are live on the RTEMS path today.

### S1 — `osd_get_roles` runs the local passwd/group DB on RTEMS, and the arm that says it doesn't is unreachable. **SILENT. LIVE. Security-relevant.**

* **File:line:** `crates/epics-pva-rs/src/auth/plain.rs:202` (the bare
  `#[cfg(unix)]` arm) and `:261-265` (the `#[cfg(not(unix))]` arm).
* **What the arm does:** `:202` opens `#[cfg(unix)] { ... }` around
  `libc::getpwnam(account)` (`:219`), then `getgrouplist_ids` (`:240`), then
  `libc::getgrgid(gid)` (`:248`) per gid, returning the account's POSIX group
  **names**. RTEMS takes this arm.
* **Why it is the defect, in the code's own words.** The `#[cfg(not(unix))]`
  arm at `:261` carries this comment verbatim:

  > `// No local group DB available (Windows LANMAN handled elsewhere;`
  > `// RTEMS / vxWorks): pvxs reports the account as its only role.`
  > `vec![account.to_string()]`

  The author intended RTEMS to land there. RTEMS is `cfg(unix)`, so it never
  can. This is the trap with a written record of the intent it violates.
* **The asymmetry that proves it was an oversight, not a decision.** The
  helper this very function calls **does** carve RTEMS out, 100 lines above:
  `getgrouplist_ids` exists twice, `#[cfg(all(unix, not(target_os = "rtems")))]`
  at `:53` and `#[cfg(all(unix, target_os = "rtems"))]` at `:103`, with a doc
  at `:97` that opens *"RTEMS is `cfg(unix)` but its newlib has no
  `getgrouplist(3)`"*. So the supplementary-group call is RTEMS-aware while
  its two callers are not.
* **Why LIVE, and why it matters.** `osd_get_roles` has exactly one production
  caller: `crates/epics-pva-rs/src/server_native/tcp.rs:2466`,
  `ClientCredentials::with_server_roles()` — described at `:2459-2463` as
  *"The single funnel every constructor / parse path passes through, so
  `roles` is server-derived by construction."* `server_native::tcp` is
  **un-gated** (`server_native/mod.rs:63`) and is the module stage C's
  `serve_connection_blocking` drives, so **every accepted PVA connection on
  RTEMS runs this**. The resulting `roles` is then passed to the ACF gate at
  `tcp.rs:1993, 4381, 4406, 4704, 5071, 6441, 6697, 6845, …` (`&ctx.roles`),
  i.e. it decides whether a PUT/GET is authorized against `member group:`
  rules.
* **Why SILENT.** Three outcomes, none of which logs or errors:
  1. RTEMS `getpwnam` misses → the fallback at `:212-215` returns
     `vec![account]` — which is coincidentally the *right* answer, so the
     defect is invisible when it is harmless.
  2. RTEMS `getpwnam` hits a synthetic/default passwd entry → roles are
     whatever that entry's `pw_gid` maps to. An ACF rule
     `member group:<that name>` then matches **every** client.
  3. `getgrgid` returns NULL for the gid → the group is skipped, roles come
     back shorter than intended, and a legitimate client is denied with an
     ordinary authorization refusal indistinguishable from a correct one.
* **What is not established here.** Which of 1/2/3 RTEMS actually does depends
  on RTEMS libcsupport's passwd/group behaviour when `/etc/passwd` is absent
  from the IMFS image. **The RTEMS kernel source is not on this machine**
  (searched; per `rtems-qemu-box` it lives on the remote build box), so I am
  not stating what it does — that is the target-run question. What *is*
  established without it: the arm selection is wrong relative to its own
  documented intent, and all three possible outcomes are silent.

### S2 — `thread_sleep_quantum()` hardcodes 100 Hz on RTEMS, decoupled from the tick the boot shim actually configures. **SILENT. LIVE.**

* **File:line:** `crates/epics-base-rs/src/runtime/time.rs:43-52`.
* **What the arm does:** `#[cfg(target_os = "linux")]` calls
  `libc::sysconf(libc::_SC_CLK_TCK)` and returns `1.0/hz`; the
  `#[cfg(not(target_os = "linux"))]` else at `:49-51` returns the literal
  `0.01`. RTEMS falls into the else. This is the anchor shape *"an else that
  was written as 'some other unix'"* — the doc at `:38-40` states the premise
  as *"the 0.01 s fallback on the targets where `libc` is not linked (it is a
  Linux-only dependency of this crate)"*.
* **The premise is half true and that is the problem.** `libc` really is
  Linux-only for this crate — `epics-base-rs/Cargo.toml:66-67` is
  `[target.'cfg(target_os = "linux")'.dependencies] libc = "0.2"` — so the
  `sysconf` call genuinely cannot be made on RTEMS from here. But the chosen
  constant is a *Linux* value (`_SC_CLK_TCK == 100`), while on RTEMS the tick
  is a build-time confdefs choice: `CONFIGURE_MICROSECONDS_PER_TICK`.
* **Why LIVE.** `quantize_to_sleep_quantum` (`time.rs:83`) is called from
  `epics-base-rs/src/server/records/sseq.rs:1052` and `:1231`; `records::sseq`
  is un-gated (`server/records/mod.rs:110`), so it is compiled into the RTEMS
  image and runs for any `sseq` record in the loaded database.
* **Why SILENT.** It is a numeric quantization of `DLYn`, C parity
  `sseqRecord.c:197-200`. A wrong quantum produces delays rounded to the wrong
  grid — no error, no log, and no acceptance rung asserts sub-second timing.
* **Current status: correct by coincidence, and that coincidence is ours to
  break.** EPICS base's RTEMS posix config sets
  `CONFIGURE_MICROSECONDS_PER_TICK 10000` (`epics-base/modules/libcom/RTEMS/
  posix/rtems_config.c:33-35`) = 100 Hz = 0.01 s, and the boot-shim design
  (`doc/rtems-boot-shim-design.md`) keeps that directive. So today the literal
  matches. Base's **score** config uses 20 ms
  (`score/rtems_config.c:40`) — proof the value is a choice, not a constant.
  The moment our shim picks a different tick, `time.rs:50` is silently wrong
  with nothing tying the two together.

### S3 — `posix_groups` has the same defect as S1, currently unreachable on RTEMS only because a *feature* is off. **SILENT. LATENT.**

* **File:line:** `crates/epics-pva-rs/src/auth/plain.rs:119` (bare
  `#[cfg(unix)]`) and `:175` (the `#[cfg(not(unix))]` arm returning
  `Vec::new()`).
* **What the arm does:** `libc::getuid()` → `libc::getpwuid` (`:131`) →
  `getgrouplist_ids` (`:156`) → `libc::getgrgid` (`:161`), returning the
  *current process's* group names. RTEMS takes it, exactly as in S1.
* **Why LATENT not LIVE:** the only production caller is
  `crates/epics-pva-rs/src/client_native/server_conn.rs:1534`, and
  `client_native` is `#[cfg(feature = "client")]` (`lib.rs:24-25`) which the
  RTEMS build turns off via `--no-default-features`. That is a feature gate,
  not a target gate — see the caveat in §0.1. If any RTEMS-co-built crate ever
  unions `client` back on, S3 becomes live with the same three silent
  outcomes as S1.

---

## 2. UNKNOWN-NEEDS-TARGET-RUN — all fail LOUDLY

These are deliberate, RTEMS-aware arms whose *runtime* behaviour on
RTEMS/libbsd I cannot establish from this machine. They are listed separately
from §1 because their failure mode is an error return, not a wrong answer.

### U1 — `SO_REUSEPORT` before bind, CA blocking driver

* `crates/epics-ca-rs/src/server/blocking.rs:281` (`set_reuse_opt`) and `:301`
  (`bind_udp_search_socket`). Raw `libc::socket` + two `libc::setsockopt`
  (`SO_REUSEADDR`, `SO_REUSEPORT`) + `libc::bind`, only when the port is
  non-zero (`:311`). Compiles for RTEMS, so the `libc` crate does define both
  constants for `armv7-rtems-eabihf`.
* **Open question for the target run:** does RTEMS libbsd's `setsockopt`
  accept `SO_REUSEPORT` on an `AF_INET`/`SOCK_DGRAM` socket? libbsd is
  FreeBSD-derived, where it exists — but I have not read the libbsd tree
  (**not on this machine**) and will not assert it.
* **LOUD:** a rejected option returns `Err(last_os_error())` from
  `set_reuse_opt`, propagated by `?` out of `bind_udp_search` → the server's
  `bind()` fails → rung 2 of the acceptance ladder ("binds 5064") fails with
  an errno. Nothing degrades quietly.
* The sibling `#[cfg(not(unix))]` arm at `:340` is DISTINCT and says so:
  *"RTEMS is Unix-family, so the shared-port path above is the one that
  matters for the target."*

### U2 — same, PVA blocking driver

* `crates/epics-pva-rs/src/server_native/blocking.rs:1092`, `:1112`.
  Byte-for-byte the same construction as U1 (the doc at `:1075-1077` states it
  is hand-rolled for the same reason: `socket2` does not cross to RTEMS,
  `Cargo.toml:139-141`). Same open question, same LOUD failure. `:1151` is the
  `#[cfg(not(unix))]` twin — DISTINCT.

### U3 — the RTEMS `FIONREAD` request value

* `crates/epics-ca-rs/src/server/blocking.rs:373` supplies
  `FIONREAD_REQUEST = 0x4004_667F` under `#[cfg(target_os = "rtems")]`,
  because the `libc` crate omits `FIONREAD` for this target. The value is
  derived in the doc at `:346-368` from newlib's `sys/filio.h` +
  `sys/ioccom.h` encoding.
* **Already handled correctly, and safe by construction:** the doc states, and
  `pending_bytes` (`:376`) implements, that any `ioctl` error returns `Err`
  and every caller treats that as "flush now" (C's own `status < 0` branch).
  So a wrong constant costs batching, never correctness. Listed here only
  because the *value* is unverified on target; the *design* is not a defect.

---

## 3. DISTINCT — full enumeration with the reason for each

Grouped by why they cannot bite. Every one was opened and read.

### 3.1 Unreachable on RTEMS — module cut by `cfg(not(target_os = "rtems"))`

| file:line | what the arm does | gate |
|---|---|---|
| `epics-ca-rs/src/hostname.rs:66` | `#[cfg(unix)] resolve_ptr` — `getnameinfo(NI_NAMEREQD)` reverse lookup | `lib.rs:50` |
| `epics-ca-rs/src/hostname.rs:199` | `#[cfg(unix)] #[tokio::test]` warm-cache test; comment relies on `/etc/hosts` loopback PTR | `lib.rs:50` + test |
| `epics-ca-rs/src/hostname.rs:226` | `#[cfg(unix)] #[test]` loopback-resolves-to-a-name | `lib.rs:50` + test |
| `epics-ca-rs/src/client/transport.rs:106` | `#[cfg(unix)] fd_recv_queue_probe` — `libc::ioctl(FIONREAD)` occupancy probe | `lib.rs:39` |
| `epics-ca-rs/src/client/transport.rs:118` | `#[cfg(not(unix))]` twin returning "always drained" | `lib.rs:39` |
| `epics-ca-rs/src/client/transport.rs:1003` | `#[cfg(unix)]` capture `as_raw_fd()` before the stream split | `lib.rs:39` |
| `epics-ca-rs/src/client/transport.rs:1008` | `#[cfg(not(unix))]` twin passing fd `0` | `lib.rs:39` |
| `epics-ca-rs/src/server/ca_server.rs:1095` | `#[cfg(unix)]` SIGTERM drain task via `tokio::signal::unix` | `server/mod.rs:14` |
| `epics-ca-rs/src/server/ca_server.rs:1119` | `#[cfg(not(unix))]` twin: `signal_handle = None` | `server/mod.rs:14` |
| `epics-ca-rs/src/repeater.rs:135` | `#[cfg(not(windows))] set_reuse_address` after exclusive bind | `lib.rs:55` |
| `epics-base-rs/src/net/loopback_mcast.rs:73` | `#[cfg(unix)] sock.set_reuse_port(true)` | `net/mod.rs:32` |
| `epics-base-rs/src/net/loopback_mcast.rs:79` | `#[cfg(target_os = "linux")]` `IP_MULTICAST_ALL` clear | `net/mod.rs:32` |
| `epics-base-rs/src/net/loopback_mcast.rs:152` | `#[cfg(unix)] #[tokio::test]` two-listeners test | `net/mod.rs:32` + test |
| `epics-base-rs/src/net/async_udp_v4.rs:215` | `#[cfg(not(target_os = "windows"))]` extra per-NIC broadcast bind | `net/mod.rs:28` |
| `epics-base-rs/src/net/async_udp_v4.rs:1257` | `#[cfg(unix)] set_reuse_port` on a fixed port | `net/mod.rs:28` |

### 3.2 Unreachable on RTEMS — enclosing **function** is RTEMS-gated

| file:line | what the arm does | enclosing gate |
|---|---|---|
| `epics-ca-rs/src/server/udp.rs:360` | `#[cfg(unix)] sock.set_reuse_port(true)` for a fixed port | `bind_responder_socket` is `#[cfg(not(target_os = "rtems"))]` at `:329` |
| `epics-ca-rs/src/server/udp.rs:367` | `#[cfg(target_os = "linux")] set_multicast_all_v4(false)` | same, `:329` |
| `epics-ca-rs/src/server/addr_list.rs:480` | `#[cfg(unix)]` `ifa_dstaddr` walk for point-to-point links | `broadcast_for_ip` is `#[cfg(not(target_os = "rtems"))]` at `:453`; RTEMS stub at `:493` returns `None` |
| `epics-ca-rs/src/server/addr_list.rs:538,540` | `#[cfg(target_os="linux")]` / `#[cfg(not(...))]` `sa_family` width split | `ifa_dstaddr_for_ipv4` is `#[cfg(all(unix, not(target_os = "rtems")))]` at `:507` |

Note the sibling stubs in this file are the correct pattern and worth citing
as such: `discover_broadcast_addrs` (`:395`, returns `Vec::new()`) and
`osi_local_addr` (`:431`, returns `Ipv4Addr::LOCALHOST`) both have explicit
`#[cfg(target_os = "rtems")]` bodies with a documented C-parity rationale.

### 3.3 Unreachable on RTEMS — feature-gated (`client`, `tls`)

`epics-pva-rs/src/client_native/udp.rs:273, 416, 500, 592, 610, 668`;
`client_native/udp.rs:423, 634` (`#[cfg(any(target_os="linux", target_os="android"))]`);
`client_native/search_engine.rs:820`; `auth/tls.rs:1561, 1628`
(`std::process::Command::new("openssl")`, `#[cfg(feature = "tls")]`).

Weaker than a target gate — see §0.1 caveat.

### 3.4 Unreachable on RTEMS — the binary cannot compile for the target

`cargo` will attempt these under `--bins`, which is why the RTEMS build
command must name its bin (`--bin rtems-ca-ioc`). All fail at *compile*, so
they are loud by construction.

| bin | hits | why it cannot compile for RTEMS |
|---|---|---|
| `epics-ca-rs/src/bin/ca-repeater-rs.rs` | `:30` `#[cfg(unix)] detach_stdio` (`libc::open("/dev/null")` + `dup2` onto 0/1/2), `:36` the `/dev/null` literal, `:53` `#[cfg(not(unix))]` twin | imports `epics_ca_rs::repeater`, RTEMS-gated at `lib.rs:55` |
| `epics-ca-rs/src/bin/softioc-rs.rs` | `:419` `std::fs::read_to_string("/etc/hostname")` | imports `epics_ca_rs::discovery::TsigKey` (`:427`), RTEMS-gated at `lib.rs:45`. The read is also un-cfg'd and already falls back to `"localhost"` |
| `epics-pva-rs/src/bin/mshim-rs.rs` | `:140` `#[cfg(unix)] set_reuse_port` | uses `socket2` (`Cargo.toml:139-140`, `cfg(not(rtems))`) and `tokio::net::UdpSocket` |
| `epics-pva-rs/src/bin/pvxvct-rs.rs` | `:648` `#[cfg(unix)] #[test]` iface-name test | test code; bin also uses the `cli` iface resolver |

**One latent comment-lie worth recording** (not a defect today, because the
bin cannot build for RTEMS): `ca-repeater-rs.rs:53-56` says
*"C `caRepeater` skips detach on Windows / RTEMS / VxWorks via
`CAN_DETACH_STDINOUT`. Match that — leave stdio inherited."* — placed on the
`#[cfg(not(unix))]` arm RTEMS can never reach. Identical in shape to S1. If
the repeater is ever ported to RTEMS, that comment is already wrong.

### 3.5 Test-only arms in otherwise-live modules

`epics-pva-rs/src/auth/plain.rs:324, 355` (`#[cfg(unix)] #[test]`, inside
`#[cfg(test)] mod tests` at `:279`); `epics-pva-rs/src/cli.rs:730`
(`#[cfg(unix)]` inside a `#[test]`); `epics-ca-rs/src/server/udp.rs:1146,
1184` (inside `#[cfg(test)]` at `:1119`).

### 3.6 Already carved out for RTEMS — the correct pattern

These are the exemplars. A reviewer checking a new arm should compare against
these, not against §3.1.

| file:line | how RTEMS is excluded |
|---|---|
| `epics-pva-rs/src/server_native/search_engine.rs:86` | `#[cfg(target_os = "rtems")] fill_entropy` → `libc::getentropy` (chosen over `arc4random_buf` **because it can report failure**); `/dev/urandom` arm is `#[cfg(all(unix, not(target_os = "rtems")))]` at `:99`. The GUID fix. |
| `epics-pva-rs/src/server_native/search_engine.rs:565-588` | source-guard test `rtems_selects_entropy_by_target_not_by_family` — asserts the RTEMS arm exists, calls `getentropy`, and that `#[cfg(unix)]` appears **zero** times in production scope. This is the only mechanical defence against the family in the whole workspace. |
| `epics-pva-rs/src/auth/plain.rs:53,103` | dual `getgrouplist_ids`, `cfg(all(unix, not(rtems)))` / `cfg(all(unix, rtems))`, doc at `:97` names the trap |
| `epics-pva-rs/src/cli.rs:410,414` | `resolve_iface_ipv4`: `cfg(all(unix, not(rtems)))` → `getifaddrs`; `cfg(any(not(unix), target_os="rtems"))` → **`Err` naming the workaround**. Loud, not degenerate. |
| `epics-base-rs/src/server/ioc_app.rs:1088-1099` | comment states the rule outright — *"RTEMS is `cfg(unix)` too, so the guard is `all(unix, not(target_os = "rtems"))`, not `unix` alone"* — then applies it to the SIGTERM arm |
| `epics-ca-rs/src/server/blocking.rs:373` | `#[cfg(target_os = "rtems")] FIONREAD_REQUEST` (see U3) |
| `epics-ca-rs/src/bin/rtems-ca-ioc.rs:61,197` | `#[cfg(any(target_os = "rtems", feature = "rtems-exec-model"))]` — the pattern that makes the RTEMS path *host-testable*, which is what let rung −1 run at all |

### 3.7 Portable degradation, no RTEMS-specific arm needed

`epics-base-rs/src/server/iocsh/commands.rs:956` — `#[cfg(target_os="linux")]`
around `read_to_string("/proc/self/status")` for the `iocStats` RSS line. RTEMS
is not linux, the block is skipped, and the command simply prints no `RSS:`
line. `:966` `available_parallelism().unwrap_or(1)` degrades to 1 on any target
that cannot answer.

---

## 4. Two anchors the brief's `rg` patterns cannot see

Both were found by widening past `#[cfg(unix)]` in `src/`. Neither is a defect
today; both are places a future one would hide.

### 4.1 `cfg(unix)` in a **Cargo dependency table**

`crates/epics-pva-rs/Cargo.toml:168-169`:

```toml
[target.'cfg(unix)'.dependencies]
libc = "0.2"
```

Correct as written — RTEMS is unix and genuinely needs `libc` (used by
`auth/plain.rs`, `server_native/blocking.rs`). But it is the same predicate,
in a file no `src/` grep reaches. Contrast `epics-base-rs/Cargo.toml:66-67`,
which gates `libc` to `cfg(target_os = "linux")` — that asymmetry is precisely
what forces `time.rs`'s else-arm (S2). `epics-ca-rs/Cargo.toml:14` takes a
third position: `libc` unconditional.

Three crates, three different answers to "when do we have libc". None is
wrong; nobody reading one would predict the others.

### 4.2 Host-shaped crates that are **un-gated** in the RTEMS closure

`hostname = "0.4"` is a plain dependency in both `epics-base-rs/Cargo.toml:15`
and `epics-pva-rs/Cargo.toml:19` — no target gate — so it is compiled into the
RTEMS image. Its two call sites:

* `epics-base-rs/src/runtime/env.rs:348` `pub fn hostname()` — falls back to
  `"localhost"`. Its three callers (`epics-ca-rs` `discovery/mdns.rs:188`,
  `client/search.rs:775`, `client/types.rs:532`) are **all** in RTEMS-gated
  modules, so it is dead on target today.
* `epics-pva-rs/src/auth/plain.rs:276` `authnz_default_host()` — callers are
  `client_native/context.rs:341,344`, feature-gated off.

**Classification: DISTINCT (no live RTEMS caller), latent.** Recorded because
the *dependency* crossing to RTEMS un-gated is what would make a future caller
silently degrade rather than fail to compile — `host_or_ca_fallback` turns a
failed `gethostname` into the literal `"invalidhost."` (`plain.rs:14`, pvxs
`buildCAMethod` parity), which then flows into PVA ACF **host** matching with
no log line.

---

## 5. Counts

| classification | count | of which SILENT |
|---|---|---|
| SAME DEFECT | **3** (S1, S2, S3) | **3** |
| UNKNOWN-NEEDS-TARGET-RUN | 3 (U1, U2, U3) | 0 — all LOUD |
| DISTINCT | 43 | — |

DISTINCT breakdown: 15 module-gated, 4 function-gated, 9 feature-gated,
7 in non-compiling bins, 6 test-only, 2 portable-degradation
(+ the §3.6 exemplars, counted where their gate places them).

Anchor totals swept at `8660074d`, in-scope crates only:
`cfg(unix)`/`cfg(not(unix))` — 49 hits across 18 files;
`target_family = "unix"` — 1 (a comment in the GUID guard);
`cfg(not(windows))`-family — 2;
`cfg(any(target_os = …))` — 4;
Linux-shaped literal paths (`/dev`, `/proc`, `/sys`, `/etc`) — 4;
newlib-questionable syscalls (`getifaddrs`, `getgrouplist`, `getpwnam`,
`getpwuid`, `getgrgid`, `sysconf`) — all resolved into the rows above.

---

## 6. What this audit says about the acceptance ladder

The lesson from the GUID, applied rather than restated:

* **The ladder in `doc/rtems-runtime-acceptance-plan.md` catches U1, U2 and
  every §3.4 bin — none of S1, S2, S3.** U1/U2 fail rung 2 ("binds 5064") with
  an errno; §3.4 fails before rung 0 (link). S1 produces an authorization
  decision, S2 produces a rounded number, S3 produces nothing at all until a
  feature flips. A fully green ladder is compatible with all three being
  wrong.
* **The only mechanical defence that exists in the tree is
  `search_engine.rs:565` — one source-guard test, in one module.** It asserts
  that module has zero bare `#[cfg(unix)]`. Nothing asserts that for
  `auth/plain.rs`, which is where S1 lives.
* **Cheapest host-side catch for S1** (in the spirit of "a host test beats a
  reboot rung"): `osd_get_roles` is target-neutral in signature, so a test
  under a `rtems-exec-model`-style feature that forces the RTEMS arm — or, far
  cheaper, a source guard on `auth/plain.rs` mirroring
  `search_engine.rs:565` — would fail *today*, on Linux, with no toolchain.
  The `#[cfg(not(unix))]` arm at `:261` already contains the intended RTEMS
  answer; the fix is arm selection, not new logic.
* **S2 has no test that can catch it** without knowing the shim's tick, which
  is the point: it is a cross-language coupling between `time.rs:50` and a C
  `#define` in a file that does not exist yet
  (`doc/rtems-boot-shim-design.md`). The place to close it is the shim's
  authoring, while both halves are being written.

## 7. What I did not establish

Stated plainly rather than inferred:

* **RTEMS libcsupport's passwd/group behaviour** when `/etc/passwd` is absent
  from the image — decides which of S1's three silent outcomes occurs. RTEMS
  kernel source is not on this machine.
* **RTEMS libbsd's `SO_REUSEPORT` support** (U1, U2). libbsd source is not on
  this machine.
* **The RTEMS `FIONREAD` value** (U3) — derived from newlib header encoding in
  the code's own doc, not verified against a newlib tree here.
* **Whether `hostname::get()` succeeds on RTEMS** (§4.2) — no live caller
  today, so it was not chased further.

No source file was edited. No commit was made. HEAD is `8660074d`, tree clean.
