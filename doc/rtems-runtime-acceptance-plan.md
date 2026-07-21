# §10 Runtime acceptance plan — QEMU `xilinx_zynq_a9_qemu`, RTEMS 6

Read-only investigation. No source file was edited. `cargo check` / `cargo
build` were run (they mutate only `target/`).

**Measured at:** worktree `/home/stevek/work/epics-rs/.caucus/worktrees/integration`,
branch `integration/rtems-scope-b`, HEAD **`ab97461f`** at start **and** end.
Working tree was clean at start; at end **one file is dirty** —
`crates/epics-base-rs/src/runtime/task.rs`, modified by the concurrent panel.
No claim below cites that file, so nothing needed re-verification.

**Reference trees used (all present locally, all read):**

- EPICS base C — `/home/stevek/work/epics-base`
- pvxs C++ — `/home/stevek/work/epics-modules/pvxs` @ `9348ebc`
- Rust `library/std` source — `~/.rustup/toolchains/nightly-x86_64-unknown-linux-gnu/lib/rustlib/src/rust/library`
- `libc` crate 0.2.188 — `~/.cargo/registry/src/index.crates.io-*/libc-0.2.188`

> ### ⛔ What is NOT on this machine — stated, not reconstructed
>
> Per the reference-source rule, I searched and found **nothing**, so I am
> naming the gap instead of writing from memory:
>
> | Missing | Searched | Result |
> |---|---|---|
> | RTEMS 6 kernel/BSP source tree | `/home/stevek/work/rtems*`, `/home/stevek/rtems*`, `/opt/rtems*`, `/home/stevek/codes/rtems*`, `find /home/stevek -maxdepth 3 -iname '*rtems*'` | **absent** |
> | `arm-rtems6-gcc` toolchain | `which arm-rtems6-gcc` | **absent** |
> | `qemu-system-arm` | `which qemu-system-arm` | **absent** |
> | RTEMS BSP documentation for `xilinx_zynq_a9_qemu` | as above | **absent** |
> | rtems-libbsd source | as above | **absent** |
>
> Consequence: **every claim in this doc about the RTEMS/BSP side is grounded
> in EPICS base's own RTEMS build files, which ARE present**, or is marked
> **`[VERIFY-ON-BOX]`** — a thing the bring-up panel must confirm against the
> real toolchain rather than take from me. There is exactly one exception I
> call out inline: the concrete `qemu-system-arm` command line for this BSP.
> Base's tree contains a QEMU line only for the **i386** BSP
> (`modules/libcom/RTEMS/posix/rtems_init.c:1027-1030`); it has **no** ARM/zynq
> equivalent, and `RTEMS_QEMU_FIXUPS = YES` appears only for `pc386`/`pc686`
> (`configure/os/CONFIG.Common.RTEMS-pc386-qemu:12`,
> `CONFIG.Common.RTEMS-pc686-qemu:11`) — **not** for
> `CONFIG.Common.RTEMS-xilinx_zynq_a9_qemu`, which I read in full (19 lines, no
> run rule). So base builds this BSP but does not itself run it under QEMU.

---

## 0. The four findings that decide the plan

1. **The Rust side already compiles all the way through codegen for
   `armv7-rtems-eabihf`; only the link fails, and it fails on the *host*
   linker.** Measured below (§1.3). This means the bring-up is a
   toolchain/link/boot exercise, not a porting exercise.
2. **We have no RTEMS `main` contract wired.** EPICS base's RTEMS entry is
   `POSIX_Init` (`configure/os/CONFIG.Common.RTEMS:154`, `-u POSIX_Init`),
   defined at `modules/libcom/RTEMS/posix/rtems_init.c:945`, which brings up
   libbsd + dhcpcd and **then calls `main(argc, argv)`**
   (`rtems_init.c:1180`). Our `rtems-ca-ioc` provides `main` and nothing else.
   A small C shim (confdefs + `POSIX_Init`) is the missing piece — §1.2.
3. **`std::net` on RTEMS is plain BSD sockets and needs libbsd underneath.**
   Rust's std has no RTEMS special-casing in the socket layer (§1.4), and
   `libc`'s newlib constants are FreeBSD-valued (`SO_REUSEPORT = 0x0200`,
   `SO_RCVTIMEO = 0x1006`, `newlib/mod.rs:623`,`:632`) — i.e. the bindings
   already assume the libbsd stack. libbsd is a **separate RTEMS package and a
   second bring-up**: base links it as `-lbsd` with `-DRTEMS_LIBBSD_STACK`
   (`CONFIG.Common.RTEMS:134-135`) and initialises it explicitly via
   `rtems_bsd_initialize()` (`rtems_init.c:1040`). Answer to the brief's
   question: **no, our `std::net` path does not have a stack by default; the
   stack is a deliberate second build+init step.**
4. **QEMU user-mode networking is very likely sufficient for BOTH CA and PVA —
   no tap device, no sudo.** Both protocols delegate the server IP to the
   datagram source: CA sets `cid = u32::MAX` "use UDP source address"
   (`epics-ca-rs/src/server/udp.rs:661`, C parity `camessage.c:2193-2207`) and
   PVA encodes `Ipv4Addr::UNSPECIFIED` as the server address
   (`epics-pva-rs/src/server_native/search.rs:236`). So a NAT'd reply through
   `hostfwd` still points the client at a reachable endpoint. §3. **This
   removes the sudo decision from the critical path** — which matters more than
   it first looks, because the sudo limits already set on the bring-up box
   (apt-get install only, no `/etc` edits, no `systemctl`) make a tap device
   *unavailable* today, not merely expensive. §3.3.

---

## 1. `rtems-ca-ioc` as it stands, and what it needs to become an image

### 1.1 What it does today

Whole file read: `crates/epics-ca-rs/src/bin/rtems-ca-ioc.rs` (245 lines).
Manifest entry `crates/epics-ca-rs/Cargo.toml:232-234`, deliberately **without**
`required-features` (`:226-231` explains: a `required-features` gate would make
cargo silently skip the target and turn the RTEMS gate vacuous).

Gate: `#[cfg(any(target_os = "rtems", feature = "rtems-exec-model"))]` on the
`ioc` module (`:60`) and on the real `main` (`:197-200`). The host default
build compiles a stub `main` that prints a refusal and returns
`ExitCode::FAILURE` (`:202-211`) — it does not silently start a runtime.

Execution, in order:

| Step | Line | What |
|---|---|---|
| 1 | `:112` | `background_init()` — callback pool (`cbLow`/`cbMedium`/`cbHigh`), delayed timer, scanOnce worker. C `callbackInit`, `callback.c:286` |
| 2 | `:92-105`, `:115` | `load_database()` — every argv entry is a `.db` path; with none, the built-in `DEMO_DB` (`:80-84`). Driven by `block_on_sync(builder.build())` |
| 3 | `:122-124` | `db.all_record_names()`, sorted |
| 4 | `:129-137` | `cas_server_port()` (`EPICS_CAS_SERVER_PORT` > `EPICS_CA_SERVER_PORT` > 5064, `epics-base-rs/src/runtime/net.rs:56-62`), then `BlockingCaServer::bind((0.0.0.0, port), db, acf)`. ACF is `None` → permissive |
| 5 | `:138-144` | `bind_udp_search(0.0.0.0:port)` — same port as TCP, C parity `caservertask.c:491-499` |
| 6 | `:146-155` | thread `CAS-TCP` → `server.serve()` |
| 7 | `:157-166` | thread `CAS-UDP` → `server.serve_udp_search(udp)` |
| 8 | `:168-176` | the banner + one line per record |
| 9 | `:182-194` | `tcp_thread.join()` then `udp_thread.join()` — **runs until killed**; there is no shutdown path (`:41-43`) |

`DEMO_DB` (`:80-84`) is exactly three records:

```
record(ao,        "RTEMS:AO")  { field(VAL,"1.5") field(PREC,"3") field(EGU,"V") }
record(longout,   "RTEMS:LO")  { field(VAL,"7")   field(EGU,"counts") }
record(stringout, "RTEMS:MSG") { field(VAL,"rtems-ca-ioc") }
```

There is a source-inspection guard test (`:214-245`) asserting this file never
references `tokio::main`/`tokio::net`/`tokio::time`/`tokio::spawn`/
`Runtime::new`/`Builder::new_multi_thread`/`block_in_place`/`block_on(` —
because tokio's `rt` features survive on the RTEMS target, so `cargo check`
alone cannot catch a runtime constructor.

### 1.2 What it needs to become a bootable image

**(a) An RTEMS configuration + Init task.** Base's is
`modules/libcom/RTEMS/posix/rtems_config.c` (read in full). The load-bearing
parts we must reproduce:

| Directive | base line | Why it matters to us |
|---|---|---|
| `CONFIGURE_POSIX_INIT_THREAD_ENTRY_POINT POSIX_Init`, stack `64*1024` | `rtems_config.c:26-28` | the entry symbol; linked via `-u POSIX_Init` (`CONFIG.Common.RTEMS:154`) |
| `RTEMS_BSD_CONFIG_INIT` + `#include <machine/rtems-bsd-config.h>`, guarded `#ifndef RTEMS_LEGACY_STACK` | `rtems_config.c:47-60` | this is what pulls libbsd into the image |
| `CONFIGURE_UNLIMITED_OBJECTS`, `CONFIGURE_UNLIMITED_ALLOCATION_SIZE 32` | `rtems_config.c:88-89` | **directly answers design-doc §11's open "RTEMS task-count budget"**: base does not cap tasks; objects are allocated unlimited, 32 at a time. Our thread-per-connection model is not fighting a fixed task table under base's own config |
| `CONFIGURE_MAXIMUM_FILE_DESCRIPTORS 64` + the `select()`/`FD_SETSIZE` warning | `rtems_config.c:70-84` | **this IS a hard cap for us.** 3 threads per connection is a thread cost, but each connection is 1 fd. 64 fds total, minus console/IMFS/libbsd internals, is the real concurrent-client ceiling to measure |
| `CONFIGURE_EXTRA_TASK_STACKS (4000 * RTEMS_MINIMUM_STACK_SIZE)` | `rtems_config.c:38` | ≈32 MB of extra task stack budget — relevant because we spawn far more threads than a C IOC |
| `CONFIGURE_MICROSECONDS_PER_TICK 10000` (default) | `rtems_config.c:33-35` | **10 ms tick.** Any timing assertion finer than 10 ms is meaningless without overriding this |
| `CONFIGURE_APPLICATION_NEEDS_CLOCK_DRIVER` / `CONSOLE_DRIVER` | `rtems_config.c:64-65` | console driver is what makes `printf`/Rust `println!` reach the serial port we scrape |
| `CONFIGURE_STACK_CHECKER_ENABLED` | `rtems_config.c:92` | keep it on for bring-up; it is how a Rust thread stack overflow becomes a diagnosable message rather than a silent corruption |

**(b) A `POSIX_Init` that calls our `main`.** Base's (`rtems_init.c:945-1191`)
does far more than we need (NFS, iocsh registration, telnetd, NTP, TFTP). The
minimal shim, grounded on the same file, is:

1. `initConsole()` equivalent / rely on the BSP console (`rtems_init.c:951`)
2. optional `clock_settime` so timestamps are not 1970 (`rtems_init.c:966`)
3. `epicsThreadSetPriority(self, iocsh)` — base drops the Init task off POSIX
   prio 2 (`rtems_init.c:1000-1002`); our equivalent is to leave the Init task
   at a *low* priority so our own threads are not starved
4. `rtems_bsd_setlogpriority("debug")` (`rtems_init.c:1034`)
5. `default_network_set_self_prio(RTEMS_MAXIMUM_PRIORITY - 1U)`
   (`rtems_init.c:1038`) — base lowers itself so libbsd's background work runs
6. `rtems_bsd_initialize()` (`rtems_init.c:1040`) — **the stack comes up here**
7. `rtems_task_wake_after(2)` for the callout timer (`rtems_init.c:1044`)
8. `rtems_bsd_ifconfig_lo0()` (`rtems_init.c:1049`)
9. `rtems_dhcpcd_add_hook(...)` + `rtems_dhcpcd_start(NULL)`
   (`rtems_init.c:1053`,`:874`), then wait on the hook event with a timeout
   (`rtems_init.c:1066`, base waits 600 s)
10. `main(argc, argv)` (`rtems_init.c:1180`)

Note base writes a default `/etc/dhcpcd.conf` if missing
(`rtems_init.c:839-873`) containing `nodhcp6 / ipv4only / timeout 0` — under
QEMU SLIRP the built-in DHCP server answers, so this path is what gives the
guest its address. `[VERIFY-ON-BOX]` that SLIRP's DHCP satisfies this dhcpcd
config.

**(c) A linker configuration.** There is **no `.cargo/config.toml` in the
repo** (verified: `ls -a` of the worktree root shows `.config`, `.github`,
`.git`, `.gitattributes`, `.gitignore` — no `.cargo`), and **no script or CI
file in the workspace references rtems** outside `doc/` and `archaeology/`
(`rg -ln rtems --glob '!doc/**' --glob '!crates/**'`). So this does not exist
yet at all.

### 1.3 The link failure, measured

`cargo +nightly check -p epics-ca-rs --bin rtems-ca-ioc -Zbuild-std=std,panic_abort --target armv7-rtems-eabihf`
→ **exit 0**.

`cargo +nightly build` (same flags) → **exit 101, and the failure is
instructive**: every crate compiled — including `tokio`, `clap`, `serde_json`,
`chrono`, `dashmap`, `parking_lot` — producing **124 ARM object files**. The
error is:

```
error: linking with `cc` failed: exit status: 1
  /usr/bin/ld: ...rcgu.o: relocations in generic ELF (EM: 40)
  /usr/bin/ld: ...rcgu.o: error adding symbols: file in wrong format
```

`EM: 40` is ARM; `/usr/bin/ld` is the x86-64 host linker. The observed link
line was:

```
"cc" <124 objects> -Wl,--as-needed -Wl,-Bstatic <58 rlibs> -Wl,-Bdynamic
  -lc -lm -L .../raw-dylibs -Wl,-z,noexecstack -o rtems_ca_ioc-...
  -Wl,--gc-sections -no-pie
```

Diffing that against base's grounded RTEMS link settings, **everything RTEMS is
missing**:

| Present in base | base line | In our link line? |
|---|---|---|
| cross linker `arm-rtems6-gcc` | `CONFIG.Common.RTEMS-xilinx_zynq_a9_qemu:11-13` (`GNU_TARGET = arm-rtems`) | **no** — plain host `cc` |
| `-L $(RTEMS_BASE)/arm-rtems6/xilinx_zynq_a9_qemu/lib/` | `CONFIG.Common.RTEMS-xilinx_zynq_a9_qemu:19` | **no** |
| `-lbsd` (+ `-DRTEMS_LIBBSD_STACK`) | `CONFIG.Common.RTEMS:134-135` | **no** |
| `-lrtemscpu -lc -lm`, `-lrtemsCom -lCom`, `-ltftpfs -lz -ltelnetd` | `CONFIG.Common.RTEMS:145-152` | only `-lc -lm`, and *dynamically* |
| `-u POSIX_Init` | `CONFIG.Common.RTEMS:154` | **no** |
| `-Wl,--gc-sections` | `CONFIG.Common.RTEMS-xilinx_zynq_a9_qemu:17` | **yes** (rustc adds it anyway) |

So the concrete deliverable is a `.cargo/config.toml`:

```toml
[target.armv7-rtems-eabihf]
linker = "arm-rtems6-gcc"
rustflags = ["-C", "link-arg=-B<RTEMS_BSP_LIB_DIR>", "-C", "link-arg=-u", ...]
```

`[VERIFY-ON-BOX]` — I will not invent the exact flag set. The authoritative
list is what `arm-rtems6-gcc` needs for this BSP (BSP linker script selection,
`-B`/`-qrtems`-style spec flags, whether `-lbsd` ordering matters against
`-lrtemscpu`). Take the flags from a working C link of any RTEMS 6 zynq
example on the remote box and mirror them; do not take them from this doc.

### 1.4 Does `std::net` work? Grounded answer

- The Rust target is real and tier 3: `os: rtems`, `env: newlib`,
  `target-family: ["unix"]`, `std: true`, `host_tools: false`
  (`rustc --print target-spec-json --target armv7-rtems-eabihf`).
- `rg rtems` across `library/std/src` returns hits only in `os/rtems/{mod,raw,fs}.rs`,
  `sys/pal/unix/mod.rs:87` (excluded from the `poll()` fast path for stdio fd
  sanity), `sys/random/mod.rs:23`, `sys/process/unix/unix.rs:1187-1194`
  (three missing signals). **No RTEMS gating anywhere in the socket layer** —
  `std::net` is compiled as ordinary unix sockets.
- `libc` provides the socket surface via `unix/newlib` (e.g. `bind` at
  `newlib/mod.rs:861`, `recvfrom` at `:870`) with **BSD-valued** constants
  (`SO_REUSEADDR 0x0004` `:616`, `SO_REUSEPORT 0x0200` `:623`,
  `SO_SNDTIMEO 0x1005` `:631`, `SO_RCVTIMEO 0x1006` `:632`). Those are FreeBSD
  numbers — the bindings presuppose libbsd.
- `libc`'s `unix/newlib/rtems/mod.rs` additionally declares `getentropy`
  (`:143`) and `arc4random_buf` (`:145`) — relevant because PVA's
  `random_guid()` needs an entropy source (`epics-pva-rs/src/server_native/udp.rs:47`,
  `try_fill_secure` `:62`/`:70`).
- `PTHREAD_STACK_MIN = 0` (`newlib/rtems/mod.rs:80`) — Rust's
  `thread::Builder::stack_size` therefore has no libc-side floor; the RTEMS
  floor comes from `CONFIGURE_*_STACK_SIZE`. `[VERIFY-ON-BOX]`: confirm our
  spawned threads get a usable default stack, because CA/PVA frames are not
  small.

**Conclusion:** `std::net` will *compile* regardless. It will only *work* if
the image links `-lbsd` and calls `rtems_bsd_initialize()`. Design-doc §11
already recorded the matching measurement — "std built for RTEMS **with no BSP
present**" (`doc/rtems-runtime-portability-design.md:583-598`) — which is
exactly the state my `cargo check` reproduces and my `cargo build` exposes.

### 1.5 Expected console output if it worked

From `:168-176`, with no argv (built-in `DEMO_DB`) and default port:

```
rtems-ca-ioc: serving 3 records on CA port 5064 (TCP + UDP search), RTEMS execution model, no tokio runtime
rtems-ca-ioc: RTEMS:AO
rtems-ca-ioc: RTEMS:LO
rtems-ca-ioc: RTEMS:MSG
```

(Names are `sort`ed at `:124`; `RTEMS:AO` < `RTEMS:LO` < `RTEMS:MSG` by byte
order.) Preceded by whatever our `POSIX_Init` shim prints; if the shim mirrors
base's, that includes `***** RTEMS Version: … *****`
(`rtems_init.c:1013-1014`), `***** Initializing network (libbsd, dhcpcd) *****`
(`:1033`), `***** ifconfig lo0 *****` (`:1048`), and the `IFCONFIG`/`NETSTAT`
dumps (`:1084-1087`) — those dumps are worth keeping, because rung 2's
diagnosis depends on knowing the guest's address.

---

## 2. The acceptance ladder

Each rung: the command, the exact line that constitutes proof, and — where the
rung can pass for the wrong reason — the negative control that closes it.

Two conventions that make this executable:

- **`$IOC` = the ELF image**, `$QEMU` = the invocation from §3.
- **Rung −1 first.** Establish the golden host output before trusting any guest
  output (below). Without it, rung 4/5's expected text is my prediction, not a
  measurement.

### Rung −1 — golden output on Linux (no QEMU, no toolchain)

The same binary runs on a host under the exec-model feature
(`epics-ca-rs/Cargo.toml:106` → `epics-base-rs/Cargo.toml:92`). Use a
non-standard port: never bind 5064 in a test.

```
EPICS_CAS_SERVER_PORT=15064 \
  cargo run -p epics-ca-rs --bin rtems-ca-ioc --features rtems-exec-model
# in another shell:
EPICS_CA_AUTO_ADDR_LIST=NO EPICS_CA_ADDR_LIST=127.0.0.1:15064 caget RTEMS:AO RTEMS:LO RTEMS:MSG
```

**Proof:** capture both outputs verbatim into
`doc/rtems-acceptance-golden.txt`. Every later rung asserts *equality with this
file*, modulo the port number. This converts "expected output" from prediction
into measurement and is the single highest-value cheap step in the plan.

### Rung 0 — it links

```
cargo +nightly build -p epics-ca-rs --bin rtems-ca-ioc \
  -Zbuild-std=std,panic_abort --target armv7-rtems-eabihf --release
```

**Proof:** exit 0 **and** `arm-rtems6-readelf -h $IOC` reports
`Machine: ARM`, `Type: EXEC`, and `arm-rtems6-nm $IOC | grep POSIX_Init`
resolves. **Negative control:** today this rung fails at
`/usr/bin/ld ... EM: 40` (§1.3) — that is the *current* measured state, so a
pass here is a real state change, not a tautology.

### Rung 1 — it boots and prints

```
$QEMU -kernel $IOC ... | tee boot.log
```

**Proof:** `boot.log` contains, in order:
`***** RTEMS Version:` … then
`rtems-ca-ioc: serving 3 records on CA port 5064 (TCP + UDP search), RTEMS execution model, no tokio runtime`
followed by the three `rtems-ca-ioc: RTEMS:…` lines (§1.5).
**Negative control:** the banner is printed at `:168` — *after* `bind()` at
`:131` and `:138` succeeded and both threads spawned at `:147`/`:158`. So the
banner appearing already proves the sockets bound. If any bind failed the log
shows instead `rtems-ca-ioc: cannot bind CA TCP port 5064: …` (`:134`) or
`… cannot bind CA UDP search port 5064: …` (`:141`) and the process exits
FAILURE. **This makes rung 2 partly redundant with rung 1 — deliberately, and
it is the strongest single line in the ladder.**

### Rung 2 — it holds port 5064, independently confirmed

From the guest side, via the RTEMS shell if the shim configures it
(base does: `CONFIGURE_SHELL_COMMAND_*`, `rtems_config.c:119-155`, incl.
`rtems_shell_NETSTAT_Command`):

```
netstat -an
```

**Proof:** a `tcp4 … *.5064 … LISTEN` row and a `udp4 … *.5064` row.
**Why it is not redundant with rung 1:** rung 1 proves *our* code thought the
bind succeeded; this proves the *stack* agrees, which is the thing libbsd
bring-up can get wrong.

### Rung 3 — it answers a CA search from the host

```
EPICS_CA_AUTO_ADDR_LIST=NO EPICS_CA_ADDR_LIST=127.0.0.1:5064 \
  cainfo RTEMS:AO
```

**Proof:** `cainfo` prints `State:            connected` and a
`Host: <addr>:5064`. **Negative control that this rung genuinely needs:** run
the identical command with the IOC killed and confirm
`Channel "RTEMS:AO" not found.` — otherwise a stale host-side CA repeater or a
second IOC on the LAN can make this rung pass with nothing running in the
guest. **Run this control every time**; it is the single most likely false
pass in the whole ladder.

### Rung 4 — it serves a real `caget`

```
EPICS_CA_AUTO_ADDR_LIST=NO EPICS_CA_ADDR_LIST=127.0.0.1:5064 \
  caget -t RTEMS:AO RTEMS:LO RTEMS:MSG
```

**Proof:** byte-identical to the rung −1 golden capture. With `-t` (terse) and
the `DEMO_DB` values at `:81-83` the three values are `1.5`, `7`,
`rtems-ca-ioc` — but **assert against the golden file, not against this
sentence**: default `caget` field width and float formatting are host-tool
behaviour I have not measured, and `PREC=3` on `RTEMS:AO` may or may not apply
to the default `DBR_DOUBLE` request. Rung −1 settles it; I will not guess.

### Rung 5 — it survives a `camonitor`, and a write propagates

```
EPICS_CA_AUTO_ADDR_LIST=NO EPICS_CA_ADDR_LIST=127.0.0.1:5064 \
  camonitor RTEMS:LO &
sleep 2
EPICS_CA_AUTO_ADDR_LIST=NO EPICS_CA_ADDR_LIST=127.0.0.1:5064 caput RTEMS:LO 42
```

**Proof:** the monitor prints an initial update with value `7`, then a second
update with value `42`, then keeps the connection open for ≥60 s with no
`Disconnected` line. This is the rung that exercises the parts nothing else
does: the per-connection thread (`server/blocking.rs:190-207`), the event
thread at `Custom(CaServerLow-1)` (`:669`), and the `background_init` callback
bands that carry the monitor tail (`rtems-ca-ioc.rs:112`).
**Negative control:** `caput` a value, confirm `caget` reads it back — proving
the update came from record processing and not from a cached client-side value.

### Rung 6 — the honest endurance rung

Leave rung 5's `camonitor` running while looping `caget`/`caput` for ≥10
minutes; then check the RTEMS shell's `stackuse` and `malloc_info` (both
configured in base at `rtems_config.c:130`,`:154`).

**Proof:** no stack-checker report of a blown thread stack, and no monotonic
heap growth across the window. **Why this rung exists:** rungs 0–5 all pass in
seconds and prove *function*. Thread-per-connection on a target with a 64-fd
cap and finite task stacks fails on *accumulation*, not on the first request,
and nothing above would catch it.

---

## 3. QEMU network plumbing

### 3.1 The problem the brief names, and why it does not bite

CA discovery is a UDP broadcast to the search port. Under QEMU user-mode
networking (SLIRP) the guest sits on a private NAT (`10.0.2.0/24` in the
default configuration); the host cannot broadcast into it, and `hostfwd` only
forwards specific host ports inward. So "host broadcasts, guest hears" is
indeed impossible.

**But CA does not require broadcast.** The client will unicast its search to
every entry in `EPICS_CA_ADDR_LIST` when `EPICS_CA_AUTO_ADDR_LIST=NO`. That is
what every rung above uses. The remaining question is whether the *reply* is
usable, and this is where the measured code settles it:

- **CA:** the search reply sets `cid = u32::MAX` — "use the UDP packet's source
  address as the server IP" — and carries the TCP port in `data_type`
  (`epics-ca-rs/src/server/udp.rs:658-665`; C parity `rsrv/camessage.c:2193-2207`,
  quoted in the comment at `:642-659`). The client therefore connects TCP to
  *wherever the reply appeared to come from*, which under SLIRP is the host-side
  forwarded endpoint — reachable by construction.
- Because the reply advertises the guest's TCP **port number** verbatim, the
  forward must preserve the number: `hostfwd=tcp::5064-:5064`, not a remapped
  host port. Our IOC uses the same value for TCP and UDP (`rtems-ca-ioc.rs:129`
  feeds both `:131` and `:138`), so both forwards use 5064.

### 3.2 The working configuration

```
-netdev user,id=net0,hostfwd=udp:127.0.0.1:5064-:5064,hostfwd=tcp:127.0.0.1:5064-:5064
-device <BSP NIC>,netdev=net0
```

`[VERIFY-ON-BOX]` — three things I am explicitly not asserting:

1. **The full `qemu-system-arm` command line for `xilinx_zynq_a9_qemu`** (the
   `-M` machine name, the NIC device name, the serial/console flags). Base has
   an i386 line only (`rtems_init.c:1027-1030`:
   `qemu-system-i386 -m 64 -no-reboot -serial stdio -display none -net nic,model=e1000 -net user,restrict=yes -append "--video=off --console=/dev/com1" -kernel libComTestHarness`)
   and **no ARM equivalent anywhere in the tree**. Take the ARM line from the
   RTEMS BSP documentation on the remote box.
2. **Do not copy base's `restrict=yes`** — it blocks guest-initiated outbound
   traffic. It is fine for a self-contained test harness; it is wrong here.
3. Whether SLIRP's UDP forwarding keeps the NAT binding alive long enough for
   CA's search/beacon cadence. UDP hostfwd mappings are timed out by SLIRP;
   long-idle monitors are TCP so they are unaffected, but a *reconnect* after a
   long idle re-searches over UDP.

### 3.3 What user-mode networking will NOT give you — and the tap fallback

| Lost under SLIRP | Consequence |
|---|---|
| CA beacons (`CA_PROTO_RSRV_IS_UP`) broadcast guest→host | host clients never learn "server came up"; they still connect via search, so rungs 3–5 are unaffected, but a *beacon-anomaly reconnect* test is impossible |
| Host-broadcast search (`EPICS_CA_AUTO_ADDR_LIST=YES`) | must be `NO` in every rung — that is why every command above sets it |
| Guest→host connection initiation to arbitrary host ports | irrelevant for an IOC; relevant if we later test CA *client* code on the guest |
| Realistic packet loss / MTU / multi-NIC behaviour | out of scope for functional acceptance either way |

**The decision to put to the user, not assume:** if beacon behaviour or
auto-address-list discovery must be in scope, the plumbing becomes a **tap
device** (`-netdev tap,...` with a host bridge), which requires either
`CAP_NET_ADMIN` on the qemu binary or a `sudo`-installed `qemu-bridge-helper`
with an `/etc/qemu/bridge.conf` entry — **a root-scoped change to the user's own
desktop.** I am flagging it, not assuming it.

**And note it is currently out of bounds anyway.** The recorded sudo limits on
the bring-up box (`gv100` / `192.168.2.128`, the user's own desktop) are: sudo
for `apt-get install` only, **no `/etc` edits**, no `systemctl`, nothing under
`/home/stevek`. A tap device needs exactly the two things that list forbids —
an `/etc/qemu/bridge.conf` entry and interface/bridge administration. So tap is
not a cheaper-or-costlier alternative today; it is **unavailable without the
user first widening those limits**, which is a separate conversation.

My recommendation: **run the whole ladder on SLIRP.** It costs nothing, needs
no privilege, stays inside the granted limits, and by §0 finding 4 it covers
rungs 0–6 in full. Raise tap only if a specific test demands broadcast, and
raise it as a request to widen the sudo scope — not as a plumbing detail.

---

## 4. The same ladder for PVA (after stage C + G)

Prerequisites, from `doc/pva-rtems-stage-cd-design.md` (my previous round,
preserved on main as `e6db56f9`): stage C (`BlockingPvaServer`), stage D (the
socket-free SEARCH core extracted out of the RTEMS-gated `udp.rs`, plus the
blocking responder), and stage G (an `rtems-pva-ioc` binary — which does not
exist: all six `epics-pva-rs` bins are `required-features = ["client"]`,
`Cargo.toml:193-221`).

### 4.1 What is the same

Rungs −1, 0, 1, 2, 6 transfer verbatim with `pvxinfo`/`pvxget`/`pvxmonitor` (or
`pvinfo-rs`/`pvget-rs`/`pvmonitor-rs`) substituted, and `5075`/`5076`
substituted for `5064`.

### 4.2 What differs — four things

1. **The unicast escape hatch exists and works the same way.** PVA's search
   response encodes the server address as `Ipv4Addr::UNSPECIFIED`
   (`epics-pva-rs/src/server_native/search.rs:236`) — the spec's "use the
   datagram source" sentinel, exactly parallel to CA's `u32::MAX`. So
   `EPICS_PVA_AUTO_ADDR_LIST=NO EPICS_PVA_ADDR_LIST=127.0.0.1:5076` over
   `hostfwd` should work for the same reason CA does. **This is the single most
   important thing to check early**, because if it were false the whole PVA
   ladder would need tap.
2. **Two ports, not one.** PVA uses a UDP broadcast/search port and a separate
   TCP server port (5076 / 5075 conventionally), so the forward set is larger
   and the "preserve the port number" constraint applies to the TCP port the
   SEARCH reply advertises (`build_search_response_proto`'s `tcp_port`,
   `search.rs:223`,`:238`).
3. **The GUID.** `random_guid()` lives in the RTEMS-gated `udp.rs:47` and its
   entropy helper `try_fill_secure` (`:62`/`:70`) must resolve on RTEMS —
   `libc` declares `getentropy` and `arc4random_buf` for RTEMS
   (`newlib/rtems/mod.rs:143`,`:145`), so the symbols exist, but **whether they
   return real entropy on this BSP is `[VERIFY-ON-BOX]`**. A degenerate GUID
   (all zeros, or identical across boots) is a *silent* failure: clients would
   treat two different servers as one. **Add a PVA-specific rung**: reboot the
   guest and assert `pvxinfo` reports a *different* GUID than the previous boot.
   Nothing in the CA ladder has an analogue, and no functional test would catch
   it.
4. **Beacons.** My stage-C/D doc recommended leaving PVA beacons gated for the
   minimal RTEMS IOC (a recorded deviation). Under SLIRP that recommendation
   costs nothing — beacons could not cross the NAT anyway. Under tap it would
   become visible. Keep the deviation, and note that it makes the SLIRP-vs-tap
   choice *less* consequential for PVA than for CA.

### 4.3 One extra PVA rung worth having

`pvxinfo RTEMS:AO` (or `-F tree`) to assert the **top-level NT type id**. Per
[[pvxget-delta-omits-top-struct-id]] the default Delta output drops it, so this
must use `-F tree`/`pvxinfo` — a trap that would otherwise produce a
false-negative on the guest and cost a day chasing a non-bug.

---

## 5. What this acceptance can NOT prove

Stated plainly, because a green ladder will be read as "RTEMS works".

1. **Nothing about real-time scheduling.** QEMU emulates a Cortex-A9
   functionally, not temporally. Instruction timing, cache, interrupt latency
   and preemption points bear no defensible relation to hardware. **Every
   priority-related conclusion — the 18/16 pvxs-parity assignment, the
   `CaServerLow`/`CaServerLow-1` CA values, the entire PI-lock evaluation in
   `doc/pi-lock-evaluation.md` — is untestable here.** A green ladder is
   evidence of *functional* correctness on RTEMS and of nothing else. It must
   not be cited in support of any hard-RT claim.
2. **Nothing about priority inheritance.** Additionally moot by construction:
   RT priority application is opt-in via `EPICS_RS_ALLOW_RT_PRIORITY` and on
   RTEMS `apply_priority_impl` is the non-Linux arm returning `Unsupported`
   ([[pi-locks-park-on-invisible]]). So the guest is not even applying
   priorities. Do not conclude "no inversion observed."
3. **Nothing about the 10 ms tick boundary.**
   `CONFIGURE_MICROSECONDS_PER_TICK 10000` (`rtems_config.c:33-35`) means any
   observed latency below 10 ms is quantisation, not measurement.
4. **Nothing about the task-count budget at scale.** Rungs 3–5 use one or two
   clients. The `CONFIGURE_MAXIMUM_FILE_DESCRIPTORS 64` cap
   (`rtems_config.c:70`) and 3-threads-per-PVA-connection are a *concurrency*
   risk; only a many-client soak (rung 6 extended) touches it, and QEMU's
   emulated NIC is a poor place to generate that load.
5. **Nothing about the real BSP.** `xilinx_zynq_a9_qemu` is a QEMU-targeted BSP.
   Driver behaviour, DMA, cache coherency and NIC errata on a physical Zynq are
   a different bring-up. Base itself distinguishes these
   (`xilinx_zynq_a9_qemu` vs `xilinx_zynq_zedboard` vs `xilinx_zynq_microzed`
   are three separate config files in `configure/os/`).
6. **Nothing about libbsd behaviour under stress.** We would be exercising a
   handful of sockets on a NAT'd virtual NIC.
7. **Nothing about the code paths still gated out.** The RTEMS build excludes
   `iocsh`, the async server, `pva_server`, `accept`, `runtime`, `udp` — a green
   ladder says the *included* subset runs, not that the crate runs.
8. **`cargo check` green ≠ links ≠ boots ≠ serves.** §1.3 is the proof: check
   is exit 0 today while build is exit 101. Each rung is a genuinely
   independent claim; do not let rung 0 stand in for rung 1.

---

## 6. Recommended execution order

1. **Rung −1 on Linux** — no toolchain needed, do it today, produces
   `doc/rtems-acceptance-golden.txt` that every later rung asserts against.
2. Write the C shim (`rtems_config.c` + minimal `POSIX_Init`) and the
   `.cargo/config.toml`, cribbing flags from a working C link on the remote box
   (§1.2, §1.3).
3. Rung 0 → 1 → 2. If rung 1 prints the banner, §1.2's whole libbsd chain is
   proven at once.
4. Rungs 3–5 on SLIRP with `EPICS_CA_AUTO_ADDR_LIST=NO`, **each with its
   negative control**.
5. Rung 6 before declaring §10's CA criterion met.
6. Only then revisit tap-vs-SLIRP, and only if a specific test demands
   broadcast (§3.3) — as a decision put to the user, not an assumption.

---

## 7. Report

**Tested:**

- `git rev-parse HEAD` at start and end — pass (`ab97461f`, unchanged)
- `git status --porcelain` at start — pass (clean)
- `git status --porcelain` at end — **dirty**: `crates/epics-base-rs/src/runtime/task.rs` (concurrent panel). No cited claim depends on that file
- `cargo +nightly check -p epics-ca-rs --bin rtems-ca-ioc -Zbuild-std=std,panic_abort --target armv7-rtems-eabihf` — **pass, exit 0**
- `cargo +nightly build` (same flags) — **fail, exit 101**, host `ld`, `EM: 40`, `file in wrong format`. This is the expected and informative result (§1.3), not a regression
- Locate RTEMS toolchain / kernel / BSP docs / qemu locally — **all absent**, enumerated in the ⛔ box; no claim reconstructed from memory
- `rustc --print target-spec-json --target armv7-rtems-eabihf` — pass (tier 3, `std: true`, `target-family: unix`, `env: newlib`)
- `rg rtems` over `library/std/src` — pass (no socket-layer gating; 5 hits, all listed §1.4)
- `libc` newlib socket surface + RTEMS submodule — pass (`bind` `:861`, `recvfrom` `:870`, BSD-valued SO_* `:616-632`, `getentropy`/`arc4random_buf` `rtems/mod.rs:143`/`:145`)
- Read `crates/epics-ca-rs/src/bin/rtems-ca-ioc.rs` in full (245 lines) — pass
- Read `epics-base/modules/libcom/RTEMS/posix/rtems_config.c` in full — pass
- Read `epics-base/modules/libcom/RTEMS/posix/rtems_init.c` `POSIX_Init` path (`:945-1191`) — pass
- Read `configure/os/CONFIG.Common.RTEMS-xilinx_zynq_a9_qemu` in full (19 lines) — pass; **no qemu run rule for this BSP**
- Read `configure/os/CONFIG.Common.RTEMS:110-175` (libbsd/link wiring) — pass
- CA search-reply address encoding — pass (`server/udp.rs:658-665`, `cid = u32::MAX`)
- PVA search-response address encoding — pass (`server_native/search.rs:236`, `Ipv4Addr::UNSPECIFIED`)
- Workspace audit for existing RTEMS build tooling — pass (**none**: no `.cargo/config.toml`, no script or CI reference outside `doc/`/`archaeology/`)
- `rtems-exec-model` feature exists on both crates — pass (`epics-ca-rs/Cargo.toml:106`, `epics-base-rs/Cargo.toml:92`)

**Failed:** none as an investigation result. The one red command
(`cargo build` → 101) is a measured property of the current state, reported as
such in §1.3 and used as rung 0's negative control.

**UNFIXED:**

- The exact `qemu-system-arm` command line for `xilinx_zynq_a9_qemu`, and the
  exact `arm-rtems6-gcc` link flags, are **not determined** — the sources that
  would ground them are not on this machine. Marked `[VERIFY-ON-BOX]` in §1.3
  and §3.2 rather than reconstructed.
- Rung 4/5's exact expected `caget` text is **not determined** — default field
  width and float formatting are host-tool behaviour I did not measure. Rung −1
  is the prescribed fix; the ladder asserts against a captured golden file, not
  against my prose.
- Whether SLIRP's UDP hostfwd NAT binding survives CA's search cadence — open,
  §3.2 item 3.
- Whether `getentropy`/`arc4random_buf` return real entropy on this BSP — open,
  §4.2 item 3, with a proposed reboot-GUID rung to catch it.
- Design-doc §11's "RTEMS task-count budget" is **partly closed, not closed**:
  base configures `CONFIGURE_UNLIMITED_OBJECTS` (`rtems_config.c:88-89`), so
  tasks are not the binding constraint, but `CONFIGURE_MAXIMUM_FILE_DESCRIPTORS 64`
  (`:70`) is a hard concurrent-connection ceiling nobody has measured against.

**Fixed:** none — no source file was edited, no commit was made, as instructed.
