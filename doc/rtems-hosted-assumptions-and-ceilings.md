# Unguarded hosted assumptions, and the static-configuration ceiling

**Measured at** integration worktree
`/home/stevek/work/epics-rs/.caucus/worktrees/integration`, HEAD
**`684e550878afa284884eaefa6a3d9064e7733976`** (`684e5508`, *"feat(rtems): the
boot shim and the link contract, in one crate"*). Read-only: no file was created
in the repo, nothing edited, nothing committed.

**Tree state.** `git status --porcelain` was **empty** for the whole
investigation and went dirty only while this document was being written: the
sibling panel's S1 fix appeared as ` M crates/epics-pva-rs/src/auth/plain.rs`
plus a new untracked `crates/epics-pva-rs/build.rs` (it emits a
`local_account_db` cfg, splitting `cfg(unix)`'s two meanings — "libc is linked",
true on RTEMS, from "there is a passwd/group database", false there). **Every
citation below was taken from the `684e5508` blobs.** The one citation that
touches an edited file is A3's count of five `env::var` sites in
`auth/plain.rs`; re-verified after the edit landed — `git show
684e5508:…/auth/plain.rs` and the working tree both report 5, so that number
holds under both. Nothing else in this document reads a file the panel is
editing.

Part A is the surface no predicate audit can reach: live-on-RTEMS code with no
`cfg` anywhere that assumes a hosted OS. Part B is the arithmetic — what our
code can demand versus what `crates/epics-rtems-boot/csrc/rtems_config.c`
actually reserves.

Scope is §0.1 of `doc/rtems-cfg-unix-trap-audit.md`: the un-gated surface of
`epics-base-rs`, `epics-ca-rs`, `epics-pva-rs`, plus the shim.

---

# Part A — hosted assumptions with no guard

Ranked silent-first, because the silent ones are what a green ladder cannot
catch.

## A1 — SILENT. Every thread priority in the port is inert on RTEMS, twice over.

The design work behind priorities is real and cited: PVA at 18 / UDP at 16
(pvxs `server.cpp:388`, `udp_collector.cpp:93`), CA receiver at
`CaServerLow` = 20 and CA sender at 19
(`epics-ca-rs/src/server/blocking.rs:200`, `:669`). None of it takes effect on
the target, for two independent reasons, either of which alone is sufficient:

1. **The switch cannot be turned on.** `apply_to_current_thread`
   (`epics-base-rs/src/runtime/task.rs:583`) delegates to
   `apply_to_current_thread_under(RtPolicy::current(), …)`, and
   `RtPolicy::current()` (`:550`) reads `RT_PRIORITY_ENV` =
   `"EPICS_RS_ALLOW_RT_PRIORITY"` (`:511`), defaulting to `RtPolicy::Disabled`
   when unset (`:533-535`). `POSIX_Init`
   (`crates/epics-rtems-boot/csrc/rtems_init.c:190-292`) calls `setenv` **zero
   times** — see A3 — and there is no shell in the image to set it. So the
   policy is `Disabled` on every boot, and `apply_to_current_thread_under`
   returns `PriorityApplied::Disabled` at `:598` before any scheduler call is
   reachable.
2. **Even switched on, the call is not wired for this target.**
   `apply_priority_impl` has two arms; the non-Linux one
   (`task.rs:769-773`) returns `PriorityApplied::Unsupported` with the comment
   *"The crate links `libc` only on Linux"* — confirmed by
   `epics-base-rs/Cargo.toml:66-67`,
   `[target.'cfg(target_os = "linux")'.dependencies] libc = "0.2"`.

**What the threads actually run at instead.** `POSIX_Init` deliberately lowers
the init task to `RTEMS_MAXIMUM_PRIORITY - 1U` — *just above idle* —
(`rtems_init.c:222-223`), and `main()` runs on that task (`:286`). Every IOC
thread is created from it. What priority a pthread created there receives is an
RTEMS pthread-attribute-default question; **I cannot establish that from this
machine** (see Part C, C1).

**Why SILENT.** Nothing logs. `sched_calls_made()` (`task.rs:783`) staying at
`0` is the *documented correct* observation for the switch being off
(`:775-781`), so the one counter that could reveal this reads as healthy. Every
acceptance rung — bind, search, caget, camonitor — passes at any priority.

**Not proposing a fix here** (read-only inventory), but naming the shape: the
priority intent is expressed as a call that is a no-op on the only target that
has no other way to set priorities, and the target does have an API
(`rtems_task_set_priority`, which the shim itself uses at `rtems_init.c:222`).

## A2 — SILENT. Every timestamp the IOC serves is 2014-04-14 plus uptime.

`rtems_init.c:206-211` sets `CLOCK_REALTIME` from
`EPICS_RTEMS_BOOT_EPOCH`, default `1397460606` (`:69`), and the file says why
in its own words at `:57-64`: no RTC on this BSP, so *"the clock starts from a
compile-time constant and is identical on every boot of every board"*. The shim
drops NTP (its `:8` note, and `doc/rtems-boot-shim-design.md` §1.1), so nothing
ever corrects it — `general_time::notify_clock_sync`
(`epics-base-rs/src/runtime/general_time.rs:237`) is never called.

That clock reaches the wire through one funnel with seven live call sites:
`general_time::get_current()` (`general_time.rs:256`) is called from
`server/recgbl.rs:428`, `server/record/record_instance.rs:3315`, `:3891`,
`:3968`, `server/database/field_io.rs:980`, `server/records/bi.rs:202`,
`server/records/mbbi.rs:554`. It becomes the CA wire stamp at
`types/codec.rs:47-53` (`epics_timestamp_parts`).

**Why SILENT.** A client sees a well-formed timestamp twelve years stale. `caget
-t` prints it without complaint; `camonitor` deltas are correct because the
monotonic part advances. The golden capture in `doc/rtems-acceptance-golden.txt`
was taken on Linux, so a guest run's stamps differ from it by construction and
the difference reads as expected, not as a finding.

**One sharp edge nearby.** `codec.rs:49-52` computes
`unix.as_secs().saturating_sub(EPICS_UNIX_EPOCH_OFFSET_SECS)` where the offset
is `631_152_000` (1990-01-01, `:11`). The default boot epoch is safely above
that. But `EPICS_RTEMS_BOOT_EPOCH` is a build-time override
(`rtems_init.c:68-70`), and any value below 1990 makes **every** timestamp
saturate to EPICS second 0 — silently, because `saturating_sub` cannot fail.

## A3 — SILENT. The environment is empty, so every `EPICS_*` setting is its compiled-in default and none is reachable.

`POSIX_Init` never calls `setenv`/`putenv` (read in full, 292 lines), the shim
drops the iocsh and the startup script (`rtems_init.c:8`), and the reduced shell
(`rtems_config.c:143-159`) offers `netstat`, `ifconfig`, `stackuse`,
`malloc_info` — none of which sets a variable in the IOC's environment.

`runtime/env.rs` reads no files (verified: zero `std::fs`/`Path` hits), so there
is no `envPaths`-style fallback either. The env-var read surface that goes
compiled-in-default on this target, by file:
`epics-pva-rs/src/config/env.rs` (68 sites), `epics-ca-rs/src/server/addr_list.rs`
(10), `epics-ca-rs/src/server/tcp.rs` (5), `epics-ca-rs/src/protocol.rs` (5),
`epics-pva-rs/src/auth/plain.rs` (5), `epics-base-rs/src/runtime/env.rs` (4).

Consequences that matter, beyond A1: `EPICS_CA_ADDR_LIST` /
`EPICS_PVA_ADDR_LIST` / `EPICS_PVA_NAME_SERVERS` cannot be set on the guest, so
every discovery test must be driven from the host side; `EPICS_CAS_BEACON_*`,
queue sizes and every log-level switch are fixed at compile time.

**The one loud exception, worth recording as the contrast:** the `getenv` device
support (`epics-base-rs/src/server/builtin_devices/getenv.rs`) turns an unset
variable into *"an empty VAL with a UDF_ALARM"* (its `:18-20`). A `.db` using
`DTYP "getenv"` therefore fails visibly on RTEMS rather than silently — which is
what the rest of this list does not do.

## A4 — SILENT. `argv` is hard-coded to one element, so `rtems-ca-ioc` can only ever serve `DEMO_DB`.

`rtems_init.c:195` declares `char *argv[] = {"rtems-ioc", NULL};` and `:286`
calls `main(1, argv)`. `rtems-ca-ioc` builds its database list from
`std::env::args().skip(1)` (`crates/epics-ca-rs/src/bin/rtems-ca-ioc.rs:124`),
which is therefore **always empty**, so `load_database` always takes the
`db_string(DEMO_DB, …)` branch (`:96`). The three demo records are the only
records this target can serve today.

**Why SILENT.** The IOC boots, prints its three record names, answers `caget`,
and every ladder rung passes. Nothing distinguishes "serving the database you
asked for" from "serving the built-in fallback because the argument vector is a
constant" — the fallback is the designed behaviour for an empty argv, and argv
is empty by construction rather than by choice.

This also makes the `db_loader` filesystem surface
(`epics-base-rs/src/server/db_loader/mod.rs`, 24 `std::fs` sites,
`db_loader/include.rs`, `db_loader/substitution.rs`) unreachable on the target
as it stands — which is why it is not in the SILENT list, but is the first thing
that becomes reachable the moment argv is made configurable.

## A5 — SILENT. Two live DNS paths degrade into an authorization answer and a dropped address, both behind a log line no ladder reads.

`std::net::ToSocketAddrs` calls `getaddrinfo`, which on RTEMS needs libbsd's
resolver and a resolver configuration. `POSIX_Init` writes `/etc/dhcpcd.conf`
(`rtems_init.c:165-188`) but nothing in the shim writes or reads
`/etc/resolv.conf`; **whether dhcpcd populates it on this BSP is not
establishable from this machine** (Part C, C3).

Two live call sites, both read in full:

* `epics-base-rs/src/server/access_security.rs:1499-1526`, `hag_members` —
  resolves ACF host-group members when `as_check_client_ip()` is on. A resolve
  failure logs `tracing::warn!` *"ACF: Unable to resolve host"* and stores the
  member as `format!("unresolved:{m}")` (`:1516`), an entry no IPv4 peer can
  ever match. On a resolver-less guest **every named HAG member becomes a
  non-matching sentinel**, so a rule intended to grant access denies it — or, in
  a `DENY FROM` rule, fails to deny. The log is `warn`, the effect is an access
  decision.
* `epics-pva-rs/src/config/env.rs:367-390`, `resolve_token_addr` — a token that
  will not resolve is dropped with `tracing::debug!` and `None` (`:385-388`).
  The doc comment right above it (`:355-365`) records that this exact "silently
  dropping every DNS hostname" behaviour was a previous defect (P-6). It
  reappears on RTEMS not as a parser bug but as an environment property.

## A6 — SILENT on host, and it is the binding ceiling on target: 10 of 12 thread spawns set no stack size.

Full enumeration of `thread::Builder::new()` in the three crates:

| site | `.stack_size()`? |
|---|---|
| `epics-base-rs/src/runtime/background/scan_once.rs:172` | **yes** — `StackSizeClass::Big` (`:176`) |
| `epics-base-rs/src/runtime/background/callback_executor.rs:293` | **yes** — `Big` (`:296`) |
| `epics-base-rs/src/runtime/background/delayed_timer.rs:226` (`cbTimer`) | no |
| `epics-base-rs/src/runtime/task.rs:887` (`spawn_dedicated_thread`, tokio arm) | no |
| `epics-base-rs/src/runtime/task.rs:908` (`spawn_dedicated_thread`, **exec arm — the RTEMS one**) | no |
| `epics-base-rs/src/runtime/task.rs:651` (RT-range probe) | no |
| `epics-base-rs/src/server/ioc_app.rs:694`, `:1029` | no |
| `epics-ca-rs/src/server/blocking.rs:192` (**per client**) | no |
| `epics-ca-rs/src/server/blocking.rs:659` (**per client**) | no |
| `epics-ca-rs/src/bin/rtems-ca-ioc.rs:157` (`CAS-TCP`), `:168` (`CAS-UDP`) | no |

`StackSizeClass` (`task.rs:442-459`) exists precisely for this, is documented as
matching *"the POSIX `stackSizeTable` in `osdThread.c`"*, and is applied at two
sites out of twelve. **Every thread that scales with connection count is in the
"no" column.** Why that is silent on the host and decisive on the target is
Part B.

## A7 — LOUD, or unreachable. Recorded so the reader can see they were checked.

* `std::env::current_exe()` — on RTEMS, std reads the literal path `"sys:exe"`
  (`std/src/sys/paths/unix.rs:363-366`), which does not exist in our IMFS, so it
  returns `Err`. Sole caller is `epics-ca-rs/src/repeater.rs:599`, an
  RTEMS-gated module. **Not live.**
* Autosave — `create_dir_all` (`server/autosave/startup.rs:246`) and
  `canonicalize` (`autosave/request.rs:207`) would fail loudly, but autosave is
  opt-in (`ioc_builder.rs:42`, `:72` default `None`; `:401` only acts when
  configured) and `rtems-ca-ioc` never calls `.autosave(…)`. **Not live**,
  latent for stage G and any real IOC.
* `iocsh` — `epics-base-rs/src/server/iocsh/mod.rs` has 32 `std::fs` sites and
  `commands.rs:956` reads `/proc/self/status` under `cfg(target_os = "linux")`;
  `rtems-ca-ioc` never starts the shell, and the RSS branch is correctly
  linux-gated. `available_parallelism()` at `commands.rs:966` already degrades
  via `.unwrap_or(1)`.
* `std::process::Command` — two sites, both in `epics-pva-rs/src/auth/tls.rs`
  (`:1561`, `:1628`) invoking `openssl`, inside `cfg(feature = "tls")` which the
  RTEMS build turns off. `std::env::temp_dir()` and `tempfile` — test code only
  (`ioc_app.rs:1522`, `autosave/*.rs`, `replay.rs:286`).
* `std::random` — on RTEMS std selects the `arc4random` backend
  (`std/src/sys/random/mod.rs:23`). That is what `HashMap`'s `RandomState` draws
  on. It is *not* what the server GUID uses: `search_engine.rs:86` deliberately
  calls `libc::getentropy` instead, because arc4random returns `void` and cannot
  report failure. Consistent, and worth knowing the two paths differ.

---

# Part B — the static-configuration ceiling

## B.0 What the shim reserves

From `crates/epics-rtems-boot/csrc/rtems_config.c`, read in full:

| directive | value | line |
|---|---|---|
| `CONFIGURE_POSIX_INIT_THREAD_STACK_SIZE` | 64 KiB | `:35` |
| `CONFIGURE_MICROSECONDS_PER_TICK` | 10000 (10 ms) | `:44` |
| `CONFIGURE_EXTRA_TASK_STACKS` | `4000 * RTEMS_MINIMUM_STACK_SIZE` | `:54` |
| `CONFIGURE_MAXIMUM_FILE_DESCRIPTORS` | 150 | `:104` |
| `CONFIGURE_MAXIMUM_USER_EXTENSIONS` | 1 | `:121` |
| `CONFIGURE_UNLIMITED_ALLOCATION_SIZE` | 32 | `:130` |
| `CONFIGURE_UNLIMITED_OBJECTS` | — | `:131` |
| `CONFIGURE_UNIFIED_WORK_AREAS` | — | `:132` |
| `CONFIGURE_STACK_CHECKER_ENABLED` | — | `:141` |
| `CONFIGURE_MAXIMUM_DRIVERS` | 40 | `:165` |

Base's own comment gives the stack unit: *"MINIMUM_STACK_SIZE == 8K"*
(`epics-base/modules/libcom/RTEMS/posix/rtems_config.c:38`), so
`CONFIGURE_EXTRA_TASK_STACKS` ≈ **32 MiB**.

## B.1 The per-thread cost, from rust-src on this machine

`rust-src` is installed under the local `nightly` toolchain, so these are read
from the actual standard library rather than recalled. **Version caveat:** the
local nightly is `rustc 1.99.0-nightly (af3d95584 2026-07-09)`; the bring-up box
runs `(87e5904f5 2026-07-20)` per `rtems-qemu-box`. Eleven days apart, different
commits — the box should re-check these three facts against its own `rust-src`
before anything is sized on them.

1. **Default thread stack = 2 MiB on RTEMS.**
   `std/src/sys/thread/unix.rs:25-31` gates `DEFAULT_MIN_STACK_SIZE = 2 * 1024 *
   1024` on `not(any(l4re, vxworks, espidf, nuttx))`. VxWorks got a 256 KiB
   carve-out at `:34-35`; **RTEMS did not**, so it takes the generic 2 MiB. This
   is the previous audit's family — an else-arm written for hosted unix that
   RTEMS falls into — living inside std itself.
2. **`std::sync::Mutex` and `Condvar` are pthread-backed on RTEMS, not futex.**
   `std/src/sys/sync/mutex/mod.rs` lists the futex arm by explicit `target_os`
   (linux, android, freebsd, openbsd, dragonfly, …); RTEMS is absent, so it
   falls to `any(target_family = "unix", …)` → `pthread::Mutex`. Identical
   selection in `std/src/sys/sync/condvar/mod.rs`.
3. **Each `std::thread` carries a pthread mutex + condvar for its parker.**
   `std/src/sys/sync/thread_parking/mod.rs` — same shape, RTEMS falls to the
   `target_family = "unix"` arm → `pthread::Parker`.

Together: **on RTEMS one `std::thread` costs 1 pthread + 1 pthread\_mutex + 1
pthread\_cond, where on our Linux CI host it costs 1 pthread + 0.** Our
per-thread OS-object demand is three times what every host test exercises, and
nothing on the host can observe the difference.

## B.2 The demand census

`StackSizeClass::bytes()` (`task.rs:451-459`) is
`f * 0x10000 * size_of::<usize>()`. On `armv7-rtems-eabihf` `size_of::<usize>()`
is 4, so Small = 256 KiB, Medium = 512 KiB, **Big = 1 MiB** (on the x86-64 host
the same code yields 512 KiB / 1 MiB / 2 MiB — the classes are half as large on
the target).

**Fixed, before the first client** (`rtems-ca-ioc` as it stands):

| thread | n | stack |
|---|---|---|
| `POSIX_Init` / `main` | 1 | 64 KiB (confdefs) |
| callback pool — `NUM_CALLBACK_PRIORITIES` 3 × `DEFAULT_THREADS_PER_PRIORITY` 1 (`callback_executor.rs:44`, `:51`) | 3 | 3 × 1 MiB (Big) |
| `cbTimer` (`delayed_timer.rs:226`) | 1 | 2 MiB (default) |
| `scanOnce` (`scan_once.rs:172`) | 1 | 1 MiB (Big) |
| `CAS-TCP` (`rtems-ca-ioc.rs:157`) | 1 | 2 MiB (default) |
| `CAS-UDP` (`rtems-ca-ioc.rs:168`) | 1 | 2 MiB (default) |
| **total** | **8** | **≈ 10 MiB** |

Plus libbsd's own threads, whose count is not establishable here (Part C, C4).

**Per connection:**

| | threads | stack | fds | `std::sync` objects beyond parkers |
|---|---|---|---|---|
| CA client (`blocking.rs:192` + `:659`) | 2 | **4 MiB** | 1 | 1 — `send_lock: Arc<Mutex<TcpStream>>` (`:634`, `std::sync::Mutex` per `:102`) |
| PVA connection (reader + writer + connection thread, `blocking.rs:770-792`) | 3 | **6 MiB** | 1 | 0 (both queues are `tokio::sync::mpsc`, pure-Rust) |

In OS objects, one PVA connection is 3 pthreads + 3 mutexes + 3 condvars; one CA
client is 2 pthreads + 3 mutexes + 2 condvars.

## B.3 The first ceiling, and where it bites

Against the 32 MiB `CONFIGURE_EXTRA_TASK_STACKS` reserve, minus ≈10 MiB fixed,
**≈22 MiB is left**:

| load | consumes | verdict |
|---|---|---|
| 5 CA clients | 20 MiB | fits |
| **6 CA clients** | 24 MiB | **over** |
| 3 PVA connections | 18 MiB | fits |
| **4 PVA connections** | 24 MiB | **over** |

**The first ceiling is task-stack memory, and it is hit at roughly 5 concurrent
CA clients or 3 concurrent PVA connections.** It is not the file descriptors:
150 descriptors is ~140 clients, about **28×** further out. It is not any object
table: those are `CONFIGURE_UNLIMITED_OBJECTS` (B.4).

The cause is A6, not the shim. Every one of those threads takes 2 MiB because no
`.stack_size()` is set, while the two threads that *do* set one take 1 MiB. Had
the per-connection threads used `StackSizeClass::Small` (256 KiB), the same 22
MiB would hold **~44 CA clients or ~29 PVA connections** — at which point the
descriptor ceiling and the shim's reserve are finally in the same order of
magnitude, which is what a coherent configuration looks like. `epicsThreadStackSmall`
is what C `rsrv` uses for its per-client tasks, so this is also the parity
answer, not merely the cheap one.

**A second reading, which I cannot rule out.** `CONFIGURE_UNIFIED_WORK_AREAS`
(`rtems_config.c:132`) merges the RTEMS workspace and the C heap into one pool,
and the shim's own comment says that is the point (`:127-128`). If pthread
stacks are `malloc`ed from that unified pool rather than drawn against the
`CONFIGURE_EXTRA_TASK_STACKS` reserve, the ceiling is instead **total BSP RAM
divided by 2 MiB**, and `EXTRA_TASK_STACKS` is only an addend to the initial
size estimate. Which of the two readings holds is a confdefs question (Part C,
C2), and the BSP's default RAM size is a QEMU question (C4). **The remedy is the
same under both readings, and so is the ranking**: 2 MiB per connection thread
is 8× the class the code already defines for this purpose, on the only target
where it is scarce.

## B.4 What `CONFIGURE_UNLIMITED_OBJECTS` covers — what I can and cannot establish

**I cannot establish the confdefs semantics from this machine.** Searched:
`/opt/rtems`, `/usr/local/rtems`, `$HOME/rtems*`, `/usr/include/rtems`, and a
whole-filesystem `find` for `confdefs.h`, `rtems-bsd-config.h`, `shellconfig.h`
— **zero hits**; no `arm-rtems6-gcc` on `PATH`; the only `rtems*` files in the
repo are the two we wrote. Per the reference-source rule I am not reconstructing
`<rtems/confdefs.h>` from memory.

**What local evidence does establish** — an *exception list by base's own
practice*, not by reading confdefs. EPICS base sets `CONFIGURE_UNLIMITED_OBJECTS`
(`posix/rtems_config.c:90`) and *still* explicitly configures six other maxima
alongside it:

| directive base still sets | line | our shim |
|---|---|---|
| `CONFIGURE_MAXIMUM_FILE_DESCRIPTORS` 64 | `:83` | set, 150 (`:104`) |
| `CONFIGURE_MAXIMUM_USER_EXTENSIONS` 5 | `:87` | set, 1 (`:121`) — **already proven necessary**: the shim's `:110-113` records that libbsd dies with *"cannot create extension"* without it |
| `CONFIGURE_MAXIMUM_DRIVERS` 40 | `:173` | set, 40 (`:165`) |
| `CONFIGURE_MAXIMUM_PERIODS` 5 | `:28` | not set — we create no rate-monotonic periods |
| `CONFIGURE_IMFS_ENABLE_MKFIFO` 2 | `:84` | not set — we create no FIFOs |
| `CONFIGURE_MAXIMUM_NFS_MOUNTS` 3 | `:86` | not set — NFS is dropped |

So for the facilities the shim keeps, its exception coverage matches base's.
Two honest qualifications: base's `CONFIGURE_MAXIMUM_PERIODS` line predates
`CONFIGURE_UNLIMITED_OBJECTS` in the same file and may simply be legacy, so
inferring "periods are outside the unlimited set" from it is weak; and file
descriptors are a libio limit rather than an object class at all, so their
presence on this list proves nothing about object coverage.

**The question this leaves open is worth more to us than to base, and that is
the point.** By B.1(2) and B.1(3), every `std::sync::Mutex`, every `Condvar`,
and every thread's parker becomes a POSIX synchronization object on RTEMS. At 32
PVA connections that is 8 + 96 = 104 threads and on the order of 300 POSIX mutex
and condition-variable objects — created by Rust's standard library, invisibly,
where the equivalent C IOC creates a handful. If `CONFIGURE_UNLIMITED_OBJECTS`
does **not** extend to `CONFIGURE_MAXIMUM_POSIX_THREADS`,
`…_POSIX_MUTEXES`, `…_POSIX_CONDITION_VARIABLES` and `…_POSIX_KEYS`, the failure
will look exactly like the user-extension failure the panel already hit: a
creation error during early init or at the Nth connection, not a graceful
refusal. That is the first thing to check on the box, and B.5 says how.

## B.5 Ceilings the image can hit, ordered by how close they are

| # | ceiling | reserve | our demand | first hit at | loud? |
|---|---|---|---|---|---|
| 1 | **task-stack memory** | ≈22 MiB after fixed | 4 MiB/CA client, 6 MiB/PVA conn | **~5 CA / ~3 PVA** | thread spawn returns `Err`; CA logs `warn!` *"failed to spawn blocking CA client thread"* (`blocking.rs:207-211`) and **drops the client silently from the client's point of view** — it sees a TCP connect that dies |
| 2 | POSIX object classes (threads/mutexes/condvars/keys) | `UNLIMITED_OBJECTS` **if it covers them** | 1 pthread + 1 mutex + 1 cond per thread | unknown — see B.4 | loud (creation error), like the user-extension one |
| 3 | file descriptors | 150 (`:104`) | 1 per connection + listeners | ~140 clients | loud (`accept` returns `EMFILE`) |
| 4 | user extensions | 1 (`:121`) | libbsd 1 + stack checker ? | boot, if the checker also claims one | loud — the shim's `:116-118` already flags this |
| 5 | drivers | 40 (`:165`) | libbsd + console + shell | not close | loud |

Ceiling 1 is both the nearest and the only one on the list that does **not**
announce itself to the operator — the server logs a warning and keeps running,
the client just fails to connect. That combination is what makes it the one to
close first.

---

# Part C — what I cannot establish from this machine

Stated in those words, with what would settle each.

* **C1 — the priority a pthread created from `POSIX_Init` inherits.** A1 shows
  our code never sets one, and the shim lowers the init task to
  `RTEMS_MAXIMUM_PRIORITY - 1U`. Whether RTEMS's default pthread attribute is
  inherit-from-creator or an explicit default is an RTEMS POSIX-API question.
  **Settled by:** reading `cpukit/posix/src/pthreadcreate.c` and
  `pthread_attr_init` on the box, or by running `stackuse`/`rtems task` on a
  booted guest and reading the priority column.
* **C2 — whether pthread stacks are drawn against
  `CONFIGURE_EXTRA_TASK_STACKS` or `malloc`ed from the unified work area.**
  This selects between the two readings in B.3. **Settled by:** reading
  `<rtems/confdefs.h>` (`_CONFIGURE_STACK_SPACE_SIZE`) plus
  `cpukit/posix/src/pthreadcreate.c` on the box; or empirically, by booting with
  a client-count ramp and watching where thread creation starts failing.
* **C3 — whether a DNS resolver exists on the guest.** A5 depends on it. The
  shim writes `/etc/dhcpcd.conf` but nothing writes or reads
  `/etc/resolv.conf`. **Settled by:** `cat /etc/resolv.conf` from the guest
  shell after DHCP binds, or a `getaddrinfo` probe record.
* **C4 — libbsd's own thread and object count, and the BSP's default RAM.**
  Both are addends to every number in B.2/B.3. **Settled by:** the `stackuse`
  and `malloc_info` shell commands the shim already configures
  (`rtems_config.c:156-157`) on a booted guest — which is precisely the rung
  `doc/rtems-runtime-acceptance-plan.md` calls the resource rung.
* **C5 — whether `CONFIGURE_UNLIMITED_OBJECTS` covers the POSIX object
  classes.** B.4. **Settled by:** reading `<rtems/confdefs.h>` on the box —
  specifically which `CONFIGURE_MAXIMUM_POSIX_*` macros the unlimited branch
  assigns — which is a single grep once the toolchain tree is in hand, and is
  the same read that closes the `CONFIGURE_MAXIMUM_PERIODS` doubt in B.4.
* **C6 — the three rust-src facts in B.1 against the box's own toolchain.** Read
  here from `nightly (af3d95584 2026-07-09)`; the box runs
  `(87e5904f5 2026-07-20)`. **Settled by:** the same three greps in the box's
  `lib/rustlib/src/rust/library/std/src`.

No source file was edited, no commit was made. HEAD `684e5508`, tree clean at
both ends of this investigation.
