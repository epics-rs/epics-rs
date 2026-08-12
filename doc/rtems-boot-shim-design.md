# The two artefacts our repo does not have — the RTEMS C boot shim and `.cargo/config.toml`

Read-only investigation. No source file was edited, no commit made.

**Measured at:** worktree `/home/stevek/work/epics-rs/.caucus/worktrees/integration`,
branch `integration/rtems-scope-b`, HEAD **`69164b67`**, working tree **clean**.

**G1/G2 confirmed closed at this HEAD** (independently re-verified, not taken on
report): `random_guid` is at `server_native/search_engine.rs:46` with
`try_fill_secure` at `:61`/`:69`, and `search_engine` is un-gated
(`mod.rs:55`); `blocking.rs:878` does `config.guid = random_guid();`, there is a
`guid()` accessor at `:909`, and a test `both_search_paths_advertise_one_guid`
at `:2708`. G3 (entropy quality on the BSP) remains open and is w1's task.

**Reference tree used (present locally, read):** EPICS base at
`/home/stevek/work/epics-base` — specifically
`modules/libcom/RTEMS/posix/rtems_config.c` (read in full),
`modules/libcom/RTEMS/posix/rtems_init.c:140-1191`,
`modules/libcom/RTEMS/score/rtems_config.c` (for contrast),
`modules/libcom/RTEMS/Makefile`, `configure/toolchain.c:29-44`,
`configure/os/CONFIG.Common.RTEMS`, and
`configure/os/CONFIG.Common.RTEMS-xilinx_zynq_a9_qemu`.

> **Still not on this machine** (searched again): the RTEMS 6 kernel/BSP tree,
> rtems-libbsd source, RTEMS headers, `arm-rtems6-gcc`, `qemu-system-arm`. The
> toolchain now exists **on the remote box**, not here. So every RTEMS
> *header-level* name below is cited from base's usage of it, and everything
> that needs a real header or a real link is left as a named hole
> (`[HOLE-n]`) with a capture instruction — not a guess.

---

## 0. Five decisions this investigation settles

1. **We must use the POSIX arm, not the score arm.** `configure/toolchain.c:31-35`
   selects `OS_API = posix` for `__RTEMS_MAJOR__ >= 5`. Two independent reasons
   bind us to it regardless: Rust std threads are pthreads (`libc` declares
   `pthread_create` for this target at `newlib/rtems/mod.rs:131`), and base's
   **score** `rtems_config.c` contains no libbsd configuration at all — the
   score link pulls only `-lnfs` (`CONFIG.Common.RTEMS:148`). Entry symbol is
   therefore `POSIX_Init`, forced with `-u POSIX_Init`
   (`CONFIG.Common.RTEMS:154`).
2. **The 64-fd ceiling is a base *choice*, not an RTEMS limit, and it does not
   bind us.** Base caps `CONFIGURE_MAXIMUM_FILE_DESCRIPTORS 64`
   (`posix/rtems_config.c:70`) purely to stay under newlib's `FD_SETSIZE`,
   and its own comment (`:71-84`) says the cap exists because raising it
   "will likely cause applications making `select()` calls to fault" —
   explicitly noting "IOC core components (libca and RSRV) do not make
   `select()` calls". Base's **score** config sets 150
   (`score/rtems_config.c:36`). **Measured for us:** `rg 'libc::select|libc::poll|FD_SET|fd_set'`
   across `epics-base-rs`, `epics-ca-rs`, `epics-pva-rs` returns **zero hits** —
   every match is Rust's `Future::poll`. We are a blocking thread-per-connection
   design with no reactor; that is the whole point. **So we may raise the fd
   ceiling, and we should, because it is the binding constraint on concurrent
   clients.** §1.2 row F.
3. **The shim belongs in its own crate with a `build.rs` + `cc`, not prebuilt
   and not duplicated per IOC crate.** §3.
4. **`.cargo/config.toml` can carry the whole RTEMS link configuration without
   touching the host build**, because `[target.<triple>]` sections apply only
   when that triple is selected. §2.
5. **Nothing in the shim may be `#[cfg]`-free.** A `.cargo/config.toml` in the
   repo root is read for *every* invocation; only its `[target.armv7-rtems-eabihf]`
   table is applied on the RTEMS target. The `[build]` table must stay empty of
   RTEMS content. §2.3.

---

## 1. Artefact 1 — the C shim

### 1.1 What base's `POSIX_Init` does that we do **not** need

`rtems_init.c:945-1191`, walked. Dropped, with the reason:

| Dropped | base site | Why we don't need it |
|---|---|---|
| NFS mount + `nfsMount` iocsh command | `:268-330`, `:594-616`, `:697` | our IOC loads its database from a compiled-in string or argv paths (`rtems-ca-ioc.rs:80-105`); there is no remote filesystem in the acceptance ladder |
| TFTP / `initialize_remote_filesystem` | `:1126` | same — no remote startup script |
| iocsh registration, `set_directory`, `IOCSH_PS1`/`IOC_NAME` | `:1160-1163`, `:1131-1141` | the interactive iocsh is host-only (`rustyline` does not build for RTEMS); `rtems-ca-ioc.rs:41-43` states there is no shutdown command by design |
| telnetd / ftpd / `telnet_pseudoIocsh` | `:797-...`, `rtems_config.c:157-161` | no remote shell in scope; each is a service that adds tasks and fds we would then have to budget |
| NTP (`epicsNtpGetTime`, `rtemsInit_NTP_server_ip`) | `:1088-1100` | the acceptance ladder asserts values and connections, not timestamps. Base itself prints "Until now no NTP support in RTEMS 5 with rtems-libbsd" (`:1087`) |
| `setBootConfigFromNVRAM` / bootp NVRAM path | `:987-995` | legacy-stack-only (`#ifdef RTEMS_LEGACY_STACK`), and the zynq BSP config sets `-DMY_DO_BOOTP=NULL` (`CONFIG.Common.RTEMS-xilinx_zynq_a9_qemu:12`) |
| `fixup_hosts`, `gethostname`-derived prompt | `:1127`, `:1133-1140` | cosmetic |
| the RTEMS shell and its ~25 `CONFIGURE_SHELL_COMMAND_*` | `rtems_config.c:117-155` | **keep a reduced subset** — see §1.2 row K; the full set is not needed but `netstat`/`ifconfig`/`stackuse` are rungs 2 and 6 of the acceptance ladder |
| `epicsRtemsInitHookPre/Post` | `:986`, `:993` | base's extension point for site code; we have no site code |
| `ne2kpci.c`, `QEMU_FIXUPS` e1000 NVM hack | `Makefile:38-40`, `:1017-1024` | both are `__i386__`-only (`#if defined(QEMU_FIXUPS) && defined(__i386__)`); irrelevant on ARM |

### 1.2 The minimal `rtems_config.c` — every directive kept, justified

Each row cites the base line it comes from. Rows **D, E, F, G** are the four
the brief calls out as load-bearing for us specifically; they are marked ★.

| | Directive | base line | Why we keep it |
|---|---|---|---|
| A | `CONFIGURE_POSIX_INIT_THREAD_TABLE` + `..._ENTRY_POINT POSIX_Init` + `..._STACK_SIZE (64*1024)` | `rtems_config.c:26-28` | this **is** the entry contract; without it there is no `POSIX_Init` and `-u POSIX_Init` has nothing to pull |
| B | `CONFIGURE_APPLICATION_NEEDS_CLOCK_DRIVER` | `:64` | no clock driver ⟹ no ticks ⟹ `rtems_task_wake_after`, every Rust `thread::sleep`, and our delayed-timer band never fire |
| C | `CONFIGURE_APPLICATION_NEEDS_CONSOLE_DRIVER` | `:65` | the console **is** the acceptance instrument — every rung scrapes `println!` off the serial line |
| D ★ | `CONFIGURE_MICROSECONDS_PER_TICK 10000` | `:33-35` | keep base's value **and record it**. 10 ms is the quantum for `thread::sleep`, our delayed-timer band, and every latency number the ladder could produce. Base makes it overridable from the build (`:30-32`); we should expose the same override rather than hard-code, so a timing experiment does not need a source edit |
| E ★ | `CONFIGURE_EXTRA_TASK_STACKS (4000 * RTEMS_MINIMUM_STACK_SIZE)` | `:38` | ≈32 MB of extra task stack. **This matters more to us than to a C IOC**: CA is 1 thread per client and PVA is 3 (`blocking.rs:795-800` states the 3N+2 budget explicitly), and every Rust thread's stack comes out of this pool. Keep base's generous value; shrinking it is an optimisation to make *after* rung 6 measures actual usage |
| F ★ | `CONFIGURE_MAXIMUM_FILE_DESCRIPTORS` — **raise above base's 64** | `:70`, caveat `:71-84`; contrast `score/rtems_config.c:36` = 150 | this is the real concurrent-client ceiling. Base capped it at 64 only to stay under newlib's `FD_SETSIZE` for `select()` users; **we make no `select`/`poll` call anywhere** (measured, §0 item 2). Each connection is one fd regardless of how many threads serve it, so raising this raises the client ceiling directly. `[HOLE-A]` |
| G ★ | `CONFIGURE_STACK_CHECKER_ENABLED` | `:92` | the single most valuable debugging directive for a Rust port. Rust's own stack-guard machinery is thin on a tier-3 target; without this a blown thread stack is silent memory corruption. Pair it with base's exit hook: `on_exit(default_network_on_exit, NULL)` (`:1035`) whose body is `rtems_stack_checker_report_usage_with_plugin(&printer)` (`:819-827`) — that is what turns rung 6's "check `stackuse`" into a real measurement |
| H | `CONFIGURE_UNLIMITED_OBJECTS` + `CONFIGURE_UNLIMITED_ALLOCATION_SIZE 32` | `:88-89` | so RTEMS task/semaphore/mutex counts are not a fixed table. This is what makes thread-per-connection viable at all, and it closes half of design-doc §11's "task-count budget" open item |
| I | `CONFIGURE_UNIFIED_WORK_AREAS` | `:90` | one heap for RTEMS objects and malloc, so the 32 MB stack pool and the Rust allocator are not two separately-sized pools we must both get right |
| J | `RTEMS_BSD_CONFIG_BSP_CONFIG` + `RTEMS_BSD_CONFIG_INIT` + `#include <machine/rtems-bsd-config.h>`, under `#ifndef RTEMS_LEGACY_STACK` | `:47-60` | **this is what puts libbsd in the image.** Without it `std::net` links against nothing |
| K | `CONFIGURE_USE_IMFS_AS_BASE_FILESYSTEM`, `CONFIGURE_FILESYSTEM_IMFS`, `CONFIGURE_FILESYSTEM_DEVFS` | `:48`, `:45`, `:42` | dhcpcd writes `/etc/dhcpcd.conf` (`rtems_init.c:839-873`) so a writable root is required; DEVFS is what gives `/dev/console` and — relevant to G3 — is where `/dev/urandom` would appear if it exists |
| L | a **reduced** shell: `CONFIGURE_SHELL_COMMANDS_INIT`, `rtems_shell_{NETSTAT,IFCONFIG}_Command`, `CONFIGURE_SHELL_COMMAND_STACKUSE`, `CONFIGURE_SHELL_COMMAND_MALLOC_INFO`, `#include <rtems/shellconfig.h>` | `:117-155` | exactly the four the ladder uses: `netstat -an` is rung 2, `stackuse` + `malloc_info` are rung 6, `ifconfig` is the diagnosis path when rung 3 fails. Everything else in base's list is dropped |
| M | `CONFIGURE_MAXIMUM_DRIVERS 40` | `:166` | cheap, and libbsd + console + shell register several |
| N | `CONFIGURE_INIT` then `#include <rtems/confdefs.h>` **last** | `:184-186` | confdefs is a single-translation-unit generator; this must be the final two lines |

**Explicitly not carried over:**

- `CONFIGURE_APPLICATION_NEEDS_RTC_DRIVER` — base guards it out for `__arm__`
  (`:169-172`), and its own comment says the RTC "seems to be missing with
  libbsd and qemu" (`rtems_init.c:958-961`). This is also the mechanism behind
  G3: no RTC ⟹ fixed boot clock ⟹ a time-seeded GUID fallback is deterministic.
- `RTEMS_BSD_CONFIG_DOMAIN_PAGE_MBUFS_SIZE` — base sets it only for
  `BSP_pc386`/`pc686`/`qoriq_e500` (`:176-180`), **not** for zynq, which
  therefore takes libbsd's default. Leave it unset to match base, and revisit
  only if rung 5/6 shows mbuf exhaustion. Note base defines `BSP_$(RTEMS_BSP)`
  via `Makefile:41` (`rtems_config_CPPFLAGS += -DBSP_$(RTEMS_BSP)`), so if we
  ever need a BSP conditional we must pass `-DBSP_xilinx_zynq_a9_qemu`
  ourselves.
- NFS / TFTP / libblock / BDBUF (`:41-44`, `:94-97`) — no block devices.
- `CONFIGURE_MAXIMUM_PERIODS 5`, `CONFIGURE_IMFS_ENABLE_MKFIFO 2`,
  `CONFIGURE_MAXIMUM_NFS_MOUNTS`, `CONFIGURE_MAXIMUM_USER_EXTENSIONS`
  (`:30`, `:85-87`) — tied to the dropped facilities.

**`[HOLE-A]` — the fd ceiling value.** Capture on the box: the value of
`FD_SETSIZE` in this toolchain's `<sys/select.h>`, and whether RTEMS 6's
confdefs spells it `CONFIGURE_MAXIMUM_FILE_DESCRIPTORS` (base posix) or
`CONFIGURE_LIBIO_MAXIMUM_FILE_DESCRIPTORS` (base score, `score/rtems_config.c:36`)
— base uses both names in different files and I have no RTEMS headers here to
tell which is current. Then set it to the client ceiling we want (start at 150,
matching base's own score value) and confirm the image still boots.

### 1.3 The minimal `POSIX_Init`

Ten steps, each citing base. Everything base does that is not in this list is
in §1.1's dropped table.

| # | Step | base site | Note |
|---|---|---|---|
| 1 | `initConsole()` — `tcgetattr`/`tcsetattr` clearing `IXOFF|IXON|IXANY` | `rtems_init.c:711-729`, called `:953` | keep the flow-control clear, drop base's four diagnostic `printf`s at `:715-718` |
| 2 | `clock_settime(CLOCK_REALTIME, &now)` with a fixed epoch | `:960-970` | base hard-codes `1397460606` because there is no RTC. **Keep it, and make the constant visible**, because it is G3's root cause; a build that later gains real entropy or NTP can drop it |
| 3 | `epicsThreadSetPriority(self, iocsh)` equivalent — get off POSIX prio 2 | `:1000-1002` | we have no `epicsThreadSetPriority`; the equivalent is a `pthread_setschedparam`/`rtems_task_set_priority` to a **low** priority so our own threads (PVA accept at `Custom(18)`, `blocking.rs:144`) are not starved by the init task |
| 4 | `rtems_bsd_setlogpriority("debug")` | `:1034` | keep for bring-up; make it a compile-time switch so a quiet image is possible later |
| 5 | `on_exit(default_network_on_exit, NULL)` → stack-checker report | `:1035`, body `:819-827` | this is directive G's payoff |
| 6 | `default_network_set_self_prio(RTEMS_MAXIMUM_PRIORITY - 1U)` | `:830-837`, called `:1038` | lower ourselves so libbsd's background work can run during init |
| 7 | `rtems_bsd_initialize()` + `assert` | `:1040-1041` | **the stack comes up here** |
| 8 | `rtems_task_wake_after(2)` — let the callout timer allocate | `:1043-1045` | base's comment; keep verbatim |
| 9 | `rtems_bsd_ifconfig_lo0()` | `:1049` | loopback, and it is what makes `EPICS_CA_ADDR_LIST=127.0.0.1` meaningful inside the guest |
| 10 | `rtems_dhcpcd_add_hook(&hook)` → `default_network_dhcpcd()` → `epicsEventWaitWithTimeout(dhcpDone, …)` | `:1051-1056`, `:1066`; hook `:742-812`; conf writer `:839-873` | **reduce the hook drastically**: base parses seven dhcp variables (NTP, TFTP, bootfile, cmdline) into globals; we need only `reason == BOUND` to signal, plus `new_host_name` for `sethostname`. Base waits 600 s (`:1066`); use a short timeout and **proceed anyway on timeout**, printing loudly — a dead network should still give us a booting image to diagnose, and rung 1 does not need an address |
| 11 | `main(argc, argv)` then `epicsExit`/`exit` | `:1180-1190` | this is the contract our Rust `fn main` satisfies |

Keep base's two `ifconfig`/`netstat` dumps (`:1084-1087`) — they cost nothing
and rung 3's diagnosis depends on knowing the guest's address.

Step 10 gained a second arm after this design was written: base #853 added a
libbsd static-IP path (`rtems_bsd_ifconfig` + a PF_ROUTE link-up wait, with
DHCP as the fallback), and the shim carries it as
`configure_static_network()`. Base sources the addresses from motload/PPCBUG
NVRAM at run time; the shim has no NVRAM contract (§1.1 drops that path), so
they arrive as the compile-time defines `EPICS_RTEMS_STATIC_IP` /
`EPICS_RTEMS_STATIC_NETMASK` / `EPICS_RTEMS_STATIC_GATEWAY` instead — see the
comment block in `rtems_init.c`.

`delayedPanic` (`:145-150`) is worth carrying: two 1-second waits before
`rtems_panic` so the console actually flushes the message. On a serial-scrape
acceptance ladder, a panic that loses its own message is a wasted boot.

---

## 2. Artefact 2 — `.cargo/config.toml`

### 2.1 Shape

```toml
# Repo-root .cargo/config.toml. Read on EVERY cargo invocation; only the
# [target.armv7-rtems-eabihf] table applies when that triple is selected, so
# the host build is untouched. Nothing RTEMS may go in [build].

[target.armv7-rtems-eabihf]
linker    = "<HOLE-1>"          # e.g. $HOME/rtems-bringup/tools/bin/arm-rtems6-gcc
rustflags = [
  "-C", "link-arg=<HOLE-2>",    # BSP spec / prefix selection
  "-C", "link-arg=-L<HOLE-3>",  # BSP lib dir
  "-C", "link-arg=-u", "-C", "link-arg=POSIX_Init",   # <HOLE-4>: confirm needed
  # <HOLE-5>: the RTEMS library list, in the order the C link uses
]

[unstable]
build-std = ["std", "panic_abort"]     # <HOLE-6>: only if we want it implicit
```

### 2.2 What belongs here vs in a build script

| Concern | Where | Why |
|---|---|---|
| linker binary, link-args, library list | **`.cargo/config.toml`** | they are properties of *the target*, identical for every crate in the workspace. Putting them in a build script would mean every crate that links an RTEMS binary repeats them |
| compiling the C shim (`rtems_config.c`, `rtems_init.c`) | **`build.rs` + `cc`** | it needs the cross compiler, the BSP include path, and `-DBSP_…`; cargo config cannot compile C |
| the BSP **prefix path** | **an environment variable read by both** | it appears in the link-args *and* in the shim's include path. Hard-coding a `$HOME`-relative path into a committed config file is wrong — the value differs per machine. Use `RTEMS_BSP_PREFIX` (or reuse base's `RTEMS_BASE` name), read by `build.rs` and injected via `rustc-link-search`/`rustc-link-arg` so `.cargo/config.toml` holds only machine-independent flags |
| `-Zbuild-std` | **the command line, not the config** | it is nightly-unstable; pinning it in `[unstable]` makes every RTEMS invocation implicitly nightly-only in a way that is easy to forget. Keep it explicit in the documented command (§4) — `[HOLE-6]` is a deliberate maybe, not a recommendation |

**Recommended split**, which shrinks the config file to almost nothing and
removes the machine-specific hole entirely:

```toml
[target.armv7-rtems-eabihf]
linker = "arm-rtems6-gcc"      # found on PATH; the box exports tools/bin
```

…and everything else emitted by the shim crate's `build.rs` as
`cargo::rustc-link-search=native=…`, `cargo::rustc-link-lib=…`,
`cargo::rustc-link-arg=…`, computed from `RTEMS_BSP_PREFIX`. That is the
structural version: **one owner for the RTEMS link contract**, and it is a
crate we can test, not a file we hope is right.

### 2.3 Coexistence with the host build — three rules

1. **Never put RTEMS content in `[build]`.** `[build] target = …` would
   redirect the default host build; `[build] rustflags = …` applies to every
   target. Both would break `cargo nextest run --workspace`.
2. **The shim crate must be a target-gated dependency**, not an unconditional
   one: `[target.'cfg(target_os = "rtems")'.dependencies]` on the IOC crates,
   the same pattern already used at `epics-pva-rs/Cargo.toml:139`/`:152` and
   `epics-base-rs`/`epics-ca-rs`. Otherwise a host build tries to run `cc` for
   a cross target and fails.
3. **The shim crate's `build.rs` must no-op unless
   `CARGO_CFG_TARGET_OS == "rtems"`.** Same predicate `epics-base-rs/build.rs`
   already uses (`build.rs:26`). A `cargo publish`/`cargo package` on a host
   must not need a cross compiler.

Verify rule 1 holds by running `cargo nextest run --workspace` after adding the
file — that is the whole regression test for "the host still builds".

### 2.4 The holes, and exactly what to capture

The bring-up panel has a working toolchain now, so each hole is filled by
**observing a real C link**, not by reading documentation. The single most
useful capture is: build any RTEMS 6 zynq C example (the BSP ships them) and
run its link with `make V=1` / `-n`, then `arm-rtems6-gcc -v <that link>`, and
save the full expanded command.

| Hole | What it is | Capture |
|---|---|---|
| **H1** | linker binary | absolute path of `arm-rtems6-gcc`; whether `tools/bin` is on PATH for non-interactive ssh (`BatchMode` shells often skip `.bashrc`) — if not, the config needs the absolute path |
| **H2** | BSP selection flag(s) | from the expanded C link: everything before `-o` that selects the BSP (linker script, `-B`, `-q…`, `-specs`). Copy verbatim |
| **H3** | BSP lib dir | the `-L` path(s). Base's zynq config uses `$(RTEMS_BASE)/$(GNU_TARGET)$(RTEMS_VERSION)/xilinx_zynq_a9_qemu/lib/` (`CONFIG.Common.RTEMS-xilinx_zynq_a9_qemu:19`) — confirm the actual installed path |
| **H4** | `-u POSIX_Init` | whether it is required when the shim comes from a Rust **staticlib/rlib**. `--gc-sections` is on (`CONFIG.Common.RTEMS-xilinx_zynq_a9_qemu:17`, and rustc adds it too — observed in the §1.3 link line of the acceptance-plan doc), so an unreferenced `POSIX_Init` inside an archive can be dropped. Capture: does `arm-rtems6-nm` find `POSIX_Init` in the linked image without `-u`? |
| **H5** | library list and order | from base: `-lbsd` (`CONFIG.Common.RTEMS:134`), `-ltftpfs -lz -ltelnetd` (`:145`), `-lrtemsCom -lCom` (`:147`), `-lrtemscpu -lc -lm` (`:151`). We drop `-lrtemsCom -lCom` (those are base's own C libraries — we are not linking EPICS base C) and probably `-ltelnetd`. Capture the minimal set that actually resolves |
| **H6** | `-Zbuild-std` | decide whether to pin it; see §2.2 |
| **H7** | static-vs-dynamic | rustc emits `-Wl,-Bdynamic -lc -lm` (observed). RTEMS has no shared libraries. Capture whether that resolves against the static libs anyway or whether an explicit `-static` / reordering is needed |
| **H8** | shim include path | the `-I` set needed for `<rtems.h>`, `<machine/rtems-bsd-config.h>`, `<rtems/netcmds-config.h>`, `<rtems/shellconfig.h>`. Take it from the C example's *compile* line, not its link line |
| **H9** | `RTEMS_MINIMUM_STACK_SIZE` | needed to reason about directive E's real byte count. `grep` the toolchain headers |
| **H10** | `FD_SETSIZE` + the confdefs macro name | `[HOLE-A]` from §1.2 |

Every hole above is a value the panel **measures**. None of them should be
filled from this document or from a model.

---

## 3. Where the files live, and how the shim is compiled

### 3.1 Layout

```
crates/epics-rtems-boot/
    Cargo.toml
    build.rs            # cc, gated on CARGO_CFG_TARGET_OS == "rtems"
    src/lib.rs          # empty on host; on RTEMS, the link anchor (§3.3)
    csrc/rtems_config.c # §1.2
    csrc/rtems_init.c   # §1.3
.cargo/config.toml      # §2.1
```

**One crate, not two copies.** `rtems-ca-ioc` and `rtems-pva-ioc` both need the
identical boot contract; duplicating a `build.rs` + C sources into
`epics-ca-rs` and `epics-pva-rs` would be two owners for one invariant, which
is the shape that produces divergence. Both IOC crates depend on it under
`[target.'cfg(target_os = "rtems")'.dependencies]`.

### 3.2 `build.rs` + `cc`, not prebuilt

Prebuilt `.o`/`.a` in-tree is rejected for three reasons: it is unreviewable in
a diff, it is locked to one toolchain build (RSB `5dbc1e08`, Newlib `1b3dcfd`)
so a toolchain bump silently mismatches, and it cannot pick up the BSP include
path which differs per machine.

`cc` is not currently a dependency anywhere in the workspace (`rg '^cc = '`
over all `Cargo.toml` → no hits), so this adds one build-dependency to one
target-gated crate. `cc` honours `CC_armv7_rtems_eabihf` and
`CFLAGS_armv7_rtems_eabihf`, which is the same environment the panel will
already have set. The `build.rs` reads `RTEMS_BSP_PREFIX` and emits the `-I`
set (`[HOLE-8]`), `-DBSP_xilinx_zynq_a9_qemu` (mirroring base's
`Makefile:41`), plus the `rustc-link-search`/`rustc-link-lib`/`rustc-link-arg`
lines from §2.2.

`build.rs` must fail with a **clear message** — not a `cc` panic — when
`RTEMS_BSP_PREFIX` is unset on an RTEMS target. That message is the first thing
a new developer will see.

### 3.3 The link-anchor problem

`--gc-sections` is on. If `POSIX_Init` sits in an archive member that nothing
references, it can be discarded and the image will have no entry task. Base
solves this with `-u POSIX_Init` (`CONFIG.Common.RTEMS:154`). We should do
both: emit `cargo::rustc-link-arg=-u` / `=POSIX_Init` from `build.rs`, **and**
give `src/lib.rs` an `extern "C"` declaration plus a `#[used]` static
referencing it, so the symbol survives even if the link-arg is ever dropped.
`[HOLE-4]` decides whether the belt is needed as well as the braces — but
having both costs nothing and the failure mode without them (an image that
boots to nothing) is expensive to diagnose over a serial line.

---

## 4. How to invoke the RTEMS build

### 4.1 The commands, with the traps encoded

```bash
export RTEMS_BSP_PREFIX=$HOME/rtems-bringup/tools        # [HOLE-3]/[HOLE-8]
export PATH=$RTEMS_BSP_PREFIX/bin:$PATH                  # [HOLE-1]

# CA — epics-ca-rs has `default = []`, so no feature flags are needed.
cargo +nightly build -p epics-ca-rs --bin rtems-ca-ioc \
  -Zbuild-std=std,panic_abort --target armv7-rtems-eabihf --release

# PVA — MUST carry --no-default-features and MUST name the bin.
cargo +nightly build -p epics-pva-rs --bin rtems-pva-ioc \
  --no-default-features \
  -Zbuild-std=std,panic_abort --target armv7-rtems-eabihf --release
```

Two traps, both measured:

1. **`--no-default-features` is mandatory for `epics-pva-rs`.** Its
   `default = ["tls", "pkcs12", "client"]`, and `tls` pulls `getrandom 0.2`,
   which the crate's own manifest comment calls "the crate that does not build
   for RTEMS". `epics-ca-rs` has `default = []` (`Cargo.toml:2`), which is why
   the CA command needs no flags — the asymmetry is real and easy to trip over.
2. **Never `--bins`.** `mshim-rs` (`epics-pva-rs/Cargo.toml:228-230`) has **no**
   `required-features` and imports `server_native::udp::ForwardableDatagram` and
   `tokio::net::UdpSocket` (`mshim-rs.rs:35-37`), both RTEMS-gated. `--bins`
   fails on mshim, not on us — a confusing failure that looks like our bug.

### 4.2 The CI shape

Three jobs, in increasing cost, so a regression is attributed precisely:

| Job | Command | Catches |
|---|---|---|
| `rtems-check` (no toolchain needed) | the two `cargo check` variants, `-Zbuild-std`, `--target armv7-rtems-eabihf` | Rust-side portability regressions. **This is what we have today** and it must keep running independently — it is the only gate that works without the box |
| `rtems-link` (toolchain required) | the two `cargo build` commands above | shim/config regressions. Today this is the step that fails at `/usr/bin/ld … EM: 40` |
| `rtems-boot` (toolchain + qemu) | rungs 1–5 of `doc/rtems-runtime-acceptance-plan.md` | behaviour |

Keep `rtems-check` as a separate job rather than folding it into `rtems-link` —
otherwise a broken toolchain on the box masks a real portability regression,
and vice versa. Also keep the warning budget assertion in `rtems-check`: the
`epics-pva-rs --lib` RTEMS check is currently **exit 0 with 2 warnings**, and
that number is a measured completion criterion for stage D, not noise.

---

## 5. Corrections owed to `doc/pva-rtems-stage-cd-design.md`

For folding into main, so the next reader is not misled. Both are in **§5.2,
item 5** of that document.

> **§5.2 item 5, as written:** "A `[[bin]]` target.
> `crates/epics-pva-rs/Cargo.toml:193-221` declares six binaries, *every one*
> `required-features = ["client"]`."

**Correction 1 — the count is 8, not 6.** `epics-pva-rs/Cargo.toml` declares
`pvget-rs` (`:193`), `pvput-rs` (`:198`), `pvmonitor-rs` (`:203`),
`pvinfo-rs` (`:208`), `pvcall-rs` (`:213`), `pvlist-rs` (`:218`),
`pvxvct-rs` (`:223`), and `mshim-rs` (`:228`). The cited line range `193-221`
stops before the last two.

**Correction 2 — "every one" is false; `mshim-rs` has no `required-features`.**
`Cargo.toml:228-230` is three lines — `name`, `path`, and nothing else. This
matters operationally, not just editorially: it is why the RTEMS command must
name the bin and must never use `--bins` (§4.1 trap 2), because `mshim-rs`
imports `server_native::udp::ForwardableDatagram` and `tokio::net::UdpSocket`
(`mshim-rs.rs:35-37`), both of which are `#[cfg(not(target_os = "rtems"))]`.

Suggested replacement text for that bullet:

> 5. **A `[[bin]]` target.** `crates/epics-pva-rs/Cargo.toml` declares eight
>    binaries (`:193`–`:230`). Seven carry `required-features = ["client"]`;
>    `mshim-rs` (`:228-230`) carries none, and imports RTEMS-gated modules — so
>    an RTEMS build must always name its bin and never use `--bins`. A
>    `rtems-pva-ioc` entry must be added with no `client` requirement, and —
>    unlike `epics-ca-rs`, whose `default = []` — the RTEMS command for this
>    crate must also carry `--no-default-features`, because `default` includes
>    `tls` → `getrandom 0.2`.

---

## 6. Report

**Tested:**

- `git rev-parse HEAD` at start and end — pass (`69164b67`, unchanged)
- `git status --porcelain` at start and end — pass (clean both times)
- G1/G2 closure independently re-verified — pass (`search_engine.rs:46`/`:61`/`:69`, un-gated at `mod.rs:55`; `blocking.rs:878` stamps the guid; accessor `:909`; test `both_search_paths_advertise_one_guid` `:2708`)
- Read `epics-base/modules/libcom/RTEMS/posix/rtems_config.c` in full — pass
- Read `posix/rtems_init.c` `:140-190`, `:705-830`, `:830-1191` — pass
- Read `posix/rtems_init.c` dhcpcd hook `:742-812` and conf writer `:839-873` — pass
- Read `score/rtems_config.c:21-44` for contrast — pass (150 fds, 20 ms tick, Classic `Init`, no libbsd)
- Read `modules/libcom/RTEMS/Makefile` — pass (`SRC_DIRS += ../$(OS_API)`, `-DBSP_$(RTEMS_BSP)` at `:41`, `LIBRARY_RTEMS = rtemsCom`)
- `configure/toolchain.c:29-35` — pass (`OS_API = posix` for RTEMS ≥ 5)
- **`select`/`poll`/`fd_set` audit across `epics-base-rs`, `epics-ca-rs`, `epics-pva-rs`** — pass, **zero libc hits** (every match is Rust `Future::poll`). This is the evidence for raising the fd ceiling
- Existing `build.rs` audit — pass (one file, `epics-base-rs/build.rs`, already uses the `CARGO_CFG_TARGET_OS == "rtems"` predicate)
- `cc` crate audit — pass (not a dependency anywhere in the workspace)
- `epics-pva-rs` bin/feature audit re-verified — pass (8 bins; `mshim-rs` `:228-230` has no `required-features`; `default = ["tls","pkcs12","client"]`)
- `epics-ca-rs` feature audit — pass (`default = []`)
- Local RTEMS header/BSP/toolchain search — **absent again**, as recorded in the header box; no header-level name asserted without a base citation

**Failed:** none.

**UNFIXED:**

- **Ten named holes (`H1`–`H10`, plus `HOLE-A`)** in the link and shim
  configuration. Each is left as a hole with a capture instruction (§2.4,
  §1.2) rather than a guessed value. None can be closed from this machine.
- **G3** (entropy quality on the BSP) — w1's task; §1.2's dropped-RTC row and
  §1.3 step 2 document the mechanism that causes it.
- **The two corrections in §5** are written but not applied —
  `doc/pva-rtems-stage-cd-design.md` is on `main`, outside this read-only
  scope.
- **The fd-ceiling raise is a recommendation, not a measurement.** The
  no-`select` evidence is solid for our three crates, but I did not audit
  transitive dependencies compiled into the RTEMS image for `select` use, nor
  libbsd's own internals. Validate on the box before treating a raised ceiling
  as safe.

**Fixed:** none — no source file was edited, no commit was made, as instructed.
