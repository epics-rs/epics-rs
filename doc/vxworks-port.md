# VxWorks 7 port — target contract, cfg architecture, and what was measured on the box

**Status:** ported, built, booted and served on target. The port spans three
branches (§2.5); this file describes the whole of it, so parts of what it
describes are forward references from this branch's point of view — each is
marked where it appears.
**Date:** 2026-07-25
**Target:** `x86_64-wrs-vxworks` (tier 3), Wind River `wrsdk-vxworks7-qemu-1.17.0`,
BSP `itl_generic_3_0_0_5`, RTP execution model
**Executable truth:** `scripts/vxworks-check.sh` — the gate is the statement,
this file is the explanation. Where the two disagree, the script is right.
**Measurement provenance:** the bring-up box `coding-agent@192.168.2.128`
(`gv100`), scratch tree `~/vx-phase1`. The box panel's raw capture lives in a
session-local scratchpad and will not outlive the session, so every line this
document relies on is **reproduced verbatim here** rather than linked.

This is the VxWorks counterpart of `doc/rtems-runtime-portability-design.md`
and its siblings. It exists because the RTEMS side has that documentation and
the VxWorks side had none — the port was three branches of measured work with
no single place that said what the target contract is, which arms compile, or
what a reader should expect to see on a console.

---

## 1. Target and toolchain contract

`x86_64-wrs-vxworks` is a **builtin** rustc triple, and that single fact
deletes the largest piece of RTEMS machinery: its `has-thread-local` is
already true, so there is no generated JSON spec, no `-Zjson-target-spec`, no
`CARGO_TARGET_…_LINKER` stem plumbing, and no retirement trip-wire arming any
of it. Contrast `doc/rtems-tls-spec-deviation.md`, which exists entirely to
explain the apparatus VxWorks does not need. `TARGET` is a literal string.

Tier 3 means no prebuilt std, so `-Zbuild-std=std,panic_abort` is required.
That is where the difficulty is.

### 1.1 Stock nightlies cannot build std for this target

Two independent upstream `libc` problems, both hit on the way in:

| problem | evidence |
|---|---|
| `pread`/`pwrite` removed from libc's vxworks module | Removed in **0.2.187** (2026-07-20) by PR **#5129**, collateral damage of deprecating kernel-mode `off64_t`; present ≤ 0.2.186. std still imports both (`library/std/src/sys/fd/unix.rs:32,406`), so std does not build. |
| `killpg` referenced by std, declared for vxworks nowhere | `library/std/src/sys/process/unix/vxworks.rs:179` references it; libc declares it for vxworks in no version. |

**Which nightly you are on decides which of the two you see**, because
`-Zbuild-std` resolves libc from rust-src's own lock: the 2026-07-09 nightly
pins 0.2.185 and shows only `killpg`, while a current one pins 0.2.188 and
shows both. `killpg` reproduces with no VxWorks SDK present at all:

```sh
cargo +nightly check -Zbuild-std=std,panic_abort --target x86_64-wrs-vxworks
```

The fix for all three symbols has to be **always-failing shims taking `off_t`,
never `extern` declarations**: `nm` over every SDK sysroot `.a`/`.so` and the
prebuilt kernel finds **0 definitions** of any of them — they appear only in
bundled Boost sources. The `.vxe` images linked with a hand `extern killpg`
present only because nothing ever referenced it (`nm` shows it neither defined
nor undefined). **A build succeeding is not evidence that a declared symbol
resolves** — which is why §3.2 measures every extern with `nm` before writing
it, rather than trusting a green link.

The upstream filing kit, including the traces and that conclusion, is
`doc/upstream-rust-targets/` — **a forward reference: that directory arrives
from another branch and is not present on this one.** It has not been filed.

### 1.2 Why the RTEMS fix does not transfer

RTEMS solves its equivalent problem with a workspace `[patch.crates-io]` libc
pin, which is why `rtems-check.sh` runs whole on a GitHub runner. That exact
lever does not reach here — **a MANIFEST `[patch]` is not part of rust-src's
`library/Cargo.lock`, which is what `-Zbuild-std` resolves std against — but a
CONFIG-LEVEL one is**, measured both ways: the box's RTEMS bring-up carries the
pin in `~/.cargo/config.toml` with the comment *"so `-Zbuild-std` also picks it
up"*, and all eleven VxWorks rows go green on a **stock** nightly under
`--config 'patch.crates-io.libc.path="…/libc-vx"'` (the bring-up-era local
checkout; its two commits now live on the fork branch below).

The VxWorks fixes were then pushed to the **same public fork branch the
manifest already pins for RTEMS** (`physwkim/libc`, branch `epics-rs-0.2`),
and `scripts/vxworks-check.sh` derives the config-level patch **from the
manifest pin** — same URL, same rev. Hence the three operator shapes it
implements:

* Nothing set — **the default, and what CI runs**: stock `nightly` plus a
  config-level patch prepared by `scripts/libc-std-patch.sh` — a clone of the
  manifest pin's exact rev whose `version` field is relabelled to the one the
  toolchain's own rust-src lock pins. The relabel is not cosmetic: a patch
  whose version differs from that pin is **silently dropped from the std unit
  graph** (measured four ways — git or path source, with or without
  `--locked`, each ending in `warning: patch … was not used` and the `killpg`
  build failure), while the same-version swap applies cleanly and leaves the
  sysroot lock untouched. The helper emits the patch as an **alias entry**
  (`patch.crates-io.libc-std`, `package = "libc"`) rather than a second
  `libc` key, because a bare same-key patch at the relabelled version
  supersedes the manifest pin and drops the WORKSPACE graph onto a floating
  stock crates-io libc (measured — the lock got rewritten to the registry
  release); under the alias the workspace keeps the committed fork
  resolution and only the std graph, whose requirement the relabelled
  version alone satisfies, takes the derived copy. Both graphs therefore
  compile the same content: the manifest pin's exact rev. `--locked` is
  dropped with a printed notice — any config-added patch entry needs a lock
  bookkeeping write, which the flag refuses outright (measured) — and
  `Cargo.lock` is snapshotted and restored on exit, so the tree is left as
  found. The remaining trip-wire is content drift: a nightly whose std needs
  libc API the pinned content lacks fails the gate loudly — the signal to
  rebase the fork branch and bump the manifest rev.
* `VXWORKS_TOOLCHAIN` alone, naming a prepared toolchain whose bundled
  rust-src already carries the fixes.
* `VXWORKS_TOOLCHAIN=nightly` plus `VXWORKS_CARGO_CONFIG` carrying an explicit
  config-level patch — the shape for developing the libc fixes themselves
  against a local checkout. It **drops `--locked`**, because a path override
  must change resolution and `--locked` exists to refuse exactly that
  (measured: `error: cannot update the lock file … because --locked was
  passed`); the script prints one line saying the flag is gone and that the
  rows therefore measure the patched resolution, and repeats it in the green
  summary.

With no nightly toolchain installed at all, the binary census still runs and
still fails loudly, the target rows **skip behind a banner `--quiet` cannot
suppress**, and the summary says `TARGET ROWS SKIPPED: no nightly toolchain`.
Never a bare success.

### 1.3 What stays box-only: linking

Since the patched libc became a public fork branch, the `check` half of this
gate runs anywhere `nightly` + `rust-src` exist — the `vxworks-census` job in
`.github/workflows/rust.yml` installs exactly that and type-checks the whole
VxWorks closure, the same job shape as `rtems-closure`. What no runner can do
is anything past `check`: producing or booting a `.vxe` needs the proprietary
Wind River SDK (`wr-cc`, the RTP crtbegin/libs, the QEMU BSP), so
`scripts/embedded-image.sh vxworks …` and the boot rig of §5 stay on the
bring-up box. Linking was never the check gate's claim, and the gap that
remains is stated as a fact rather than papered over.

---

## 2. The cfg architecture

### 2.1 `epics_embedded_target` — the capability, not the OS name

The port's central move. Before it, the target-specific surface split on a
**literal OS name**: `#[cfg(target_os = "rtems")]` selected the blocking
driver, the `NameServersOnly` search transport and the no-UDP arms, and
`#[cfg(not(target_os = "rtems"))]` selected the hosted tokio/UDP path. For
`target_os = "vxworks"` every one of those predicates answers the wrong way —
a VxWorks build would have taken the **hosted tokio path on a tier-3 target
with no tokio**, which is a different and almost certainly red compilation
surface.

The fix is a build-script cfg naming the capability the arms actually depend
on — emitted by **five** build scripts, one per package that needs it. From
`crates/epics-libcom-rs/build.rs` (**forward reference: this shape arrives with
the cfg-port branch**):

```rust
let embedded_target = matches!(target_os.as_str(), "rtems" | "vxworks");
if embedded_target {
    println!("cargo::rustc-cfg=epics_embedded_target");
}
```

with the comment that carries the reason: *"no tokio reactor exists on either,
so both the exec-model selection below and every dependency/socket-portability
seam above `epics-libcom-rs` gate on this one capability cfg instead of
repeating `any(target_os = "rtems", target_os = "vxworks")`."* 117 sites were
reclassified onto it and 10 `Cargo.toml` target tables widened; a port-seam
cross-check over the result reported **0 MISSED / 0 WRONG**.

### 2.2 `exec_backend` versus `epics_embedded_target`

Two cfgs, deliberately not one:

* `epics_embedded_target` — a **pure target predicate**. RTEMS or VxWorks. No
  feature can turn it on.
* `exec_backend` — the std-thread background executor is selected. True on
  `epics_embedded_target`, **or** on a hosted target with the
  `rtems-exec-model` feature, which is a real product mechanism (a PREEMPT_RT
  blocking front end wanting the runtime-free spawn/timer backend) and is also
  how the target entry points get host-compiled, host-linted and host-tested.

Collapsing them would make the host-selectable execution model imply
target-only dependency and socket decisions.

### 2.3 What deliberately stays `target_os = "rtems"`

Not everything widened, and the residue is not oversight:

* **`epics-rtems-boot`'s BSP boot glue and link contract.** `POSIX_Init`, the
  `rtems_config.c`/`rtems_init.c` shim, the `_RTEMS_LIBC_*_LAYOUT` refusals
  and the whole `rtems_boot_linked` apparatus are RTEMS-only by intent. Its
  build script returns early for any other target OS, so a VxWorks build
  compiles none of it. An RTP is loaded by `rtpSp`; there is no boot anchor to
  gate.
* **The RTEMS priority machinery.** `rtems_core_priority` and
  `RTEMS_MAXIMUM_PRIORITY` encode an RTEMS kernel constant measured on the
  RTEMS guest. VxWorks reaches the same POSIX number by its own route (§4) and
  must not import that constant to do it.
* **`libc::timespec` avoidance and the RTEMS newlib gaps.** Target-specific
  bugs in an RTEMS toolchain, with no VxWorks analogue.

### 2.4 The bins

`realtime-ca-ioc` and `realtime-pva-ioc` gate their real entry points on
`any(target_os = "rtems", target_os = "vxworks", feature = "rtems-exec-model")`
— the `exec_backend` predicate spelled out, because a binary crate has no
build script emitting it. `crates/epics-bridge-rs/src/bin/realtime-pva-ioc.rs:110`
carries the history of why the `any(...)` form is the right one.

**The bins keep their `rtems-` names on a VxWorks image.** They are the target
IOCs regardless of which RTOS runs them, the box rig already stages them under
different file names (`ca.vxe` / `pvaioc.vxe`), and a rename would touch every
doc and rig path for no compilation benefit.

### 2.5 Where the port lives

| branch | what it carries |
|---|---|
| `cfg-port` | `epics_embedded_target`, the 117 reclassified sites, 10 widened target tables, VxWorks entropy arm, VxWorks `sched_param` via libc, `map_epics_priority_vxworks` |
| `bin-hardening` | the bins' `any(...)` gates, the PVA `SO_SNDTIMEO` best-effort fix, peer-address logging |
| `probes-parity` (this branch) | the stats funnel and its VxWorks backend, `scripts/vxworks-check.sh`, the CI census job, this file |

Merged together on the box with **zero source edits** and `rtems-exec-model`
absent. `runtime/task.rs` auto-merged clean: the cfg-port edits and this
branch's one-line `register_task()` hook are disjoint regions.

---

## 3. The statistics funnel and the console census

`crates/epics-rtems-boot/src/stats/` is one portable funnel over a per-OS
backend: `mod.rs` holds the types and one entry point per reading with **zero
`#[cfg]` of its own**, and `rtems.rs` / `vxworks.rs` / `unsupported.rs` are
selected by a single `#[cfg]` + `#[path]` block. Consumers — the FD/MEM status
PVs, both target IOCs' probe blocks — call one function and never fork per OS.

It lives in `epics-rtems-boot` despite the crate name, and the crate's README
records why: `rtems_boot_linked` is a cfg only that package's build script can
emit, and `epics-libcom-rs` is deliberately the leaf a consumer takes *without*
the boot shim. `epics-libcom-rs` therefore takes `epics-rtems-boot` under
`[target.'cfg(target_os = "vxworks")'.dependencies]` — target-gated because on
RTEMS that package emits an image link contract that propagates to every
dependent binary, and on VxWorks it compiles no C and emits none.

### 3.1 Three deliberate absences

**No `vxworks_boot_linked`.** RTEMS needs `rtems_boot_linked` because its
build script compiles C against a BSP and the cfg says it did. The VxWorks
backend compiles no C of ours; every symbol it declares is one an RTP resolves
from the C library it links unconditionally. `target_os = "vxworks"` alone is
the whole selection, so a `vxworks_boot_linked` would be a configuration axis
no build can be in. `CONFIGS=(portability)` in the gate is therefore permanent
— a property of the port, not a gap awaiting work.

**`MEM_FREE`, `MEM_MAX` and `MEM_BLK` are `NaN`, by decision.** `MEM_USED` is
mimalloc's `current_commit` via `mi_process_info`, a real reading. The other
three have no source that is not a fabrication:

* `memPartInfoGet(memSysPartId, …)` — the shape devIocStats' vxWorks OSD uses
  — is rejected twice over. VxWorks 7's libc allocator is mimalloc, so an
  RTP's `malloc` does not come from the system partition at all, and
  `memSysPartId` is measured ABSENT from every RTP library, so the partition
  cannot even be named. Publishing the kernel partition's free bytes as this
  IOC's heap would be a confident number about the wrong heap — worse than
  `NaN`, because an operator would believe it.
* `free` is derivable only by walking `mi_heap_visit_blocks` over the default
  heap. Approximate, default-heap-only: rejected by the same rule.
* `largest_free` has no source at all. mimalloc exposes no free-run metric and
  its visitor reports allocated blocks only. `MEM_BLK` — the fragmentation
  signal an allocation actually fails on — has no VxWorks analogue.

This is why `MemUsage`'s **fields** are optional rather than the struct: a
backend that can measure one of three must not have to throw it away or invent
the other two, and an `Option<MemUsage>` over plain `u64` fields would give two
ways to say "no reading", the second indistinguishable from an exhausted heap.

**The task census is registry-scoped, and says so in-band.** Measured:
`taskIdListGet` and `taskEach` are **kernel-mode only and ABSENT from every RTP
library**. An RTP can describe a task it can name and cannot ask what tasks
exist. So the list is built as the threads start —
`runtime::task::enter_ioc_thread` calls `stats::register_task()`, which
captures `taskIdSelf()` into a 192-entry registry, plus one explicit
registration for `main`, which does not go through the prologue.

`enter_ioc_thread` is the seam because it is already the single owner of the
thread transition it rides on: every IOC thread passes through it to take its
scheduling band, so *every thread that bands itself registers itself* adds a
consequence to an invariant that already holds rather than a rule to remember
at each spawn. A thread starting outside it is invisible to this census — and
that limitation is **printed in the census output's own header**, not left in a
comment, because a reader who took the block for the RTP's thread table would
under-count and have no way to know.

`TASK_DESC` is mirrored as `#[repr(C)]` and pinned by `offset_of!` const
assertions (`td_priority` 68, `td_stack_size` 80, `td_stack_current` 88,
`td_stack_high` 96, `td_stack_margin` 104, `td_name` 128, size 208). This is
the one declaration where being wrong would not fail to link — a mis-declared
function is a link error, a mis-declared struct links clean and publishes a
plausible stack figure read out of the wrong eight bytes — so a drifting SDK
must stop the build.

### 3.2 Symbol provenance

`taskIdSelf` is declared by the patched libc (`src/vxworks/mod.rs:2337`).
`taskInfoGet`, `rtpIoTableSizeGet` and `mi_process_info` are this branch's own
`extern "C"` declarations; libc declares none of them. Every one was measured
DEFINED with `nm` over the SDK's RTP libraries before being declared, because
on this target **a declaration that is never called links clean whether or not
the symbol exists** — which is exactly how `killpg` got as far as it did.

`rtpIoTableSizeGet`'s header prototype, read on the box:

```
vxsdk/sysroot/usr/h/public/ioLib.h:533
    extern size_t   rtpIoTableSizeGet (RTP_ID rtpId);
```

`RTP_ID → OBJ_HANDLE → _Vx_OBJ_HANDLE → int` (`types/vxWindBase.h:31`,
`types/vxWind.h:36,43`), so the `c_int` argument is right; the kernel-mode
`struct wind_rtp *` typedef does not apply to an RTP. The **return** was
declared `c_int` and is now `size_t`. The width decides the failure return, not
exactness: `ERROR` is `-1`, and `-1` arriving as `size_t` is `SIZE_MAX`, which
a cast would turn into a four-billion-descriptor walk bound. `u32::try_from`
refuses it.

---

## 4. Priority model — the same formula, and *not* a deviation

On RTEMS this workspace takes a **deliberate deviation** from what base does.
Base compiles `os/posix/osdThread.c` on RTEMS 6 and applies the linear map
`oss = epics*(max-min)/100 + min`, which on the bring-up guest puts the CA
server band at core 24 — far above libbsd's network threads, with EPICS 63 the
crossover above the interrupt server. Reproducing that would reproduce the
hazard, so the RTEMS arm takes EPICS's *own* RTEMS answer
(`RTEMS-score/osdThread.c:94-102`, `core = 199 - epics`) and inverts it into
the POSIX space actually set:

```text
core = RTEMS_MAXIMUM_PRIORITY - posix   (measured, 255)
core = 199 - epics                      (RTEMS-score/osdThread.c:94-102)
⟹ posix = 56 + epics
```

**On VxWorks the identical POSIX value is not a deviation — it is base's own
vxWorks map, reached by a different route.** EPICS base's vxWorks port
computes the native priority directly
(`modules/libcom/src/osi/os/vxWorks/osdThread.c:99-106`, checked at
R7.0.10-142-g33f4d15ff):

```c
static int getOssPriorityValue(unsigned int osiPriority)
{
    if ( osiPriority > 99 ) { return 100; }
    else { return ( 199 - (signed int) osiPriority ); }
}
```

We set the POSIX value and let VxWorks's own POSIX layer invert it. Measured on
the box: `posix = 56 + epics` landed **11 of 11 threads at
`PriorityApplied::Realtime`, one scheduler call each**, and the observed native
priority was `vx = 199 - epics` — exact agreement with the C above.

`map_epics_priority_vxworks` restates the `56 + epics` arithmetic directly
rather than calling the RTEMS map, on purpose: the RTEMS path goes through
`RTEMS_MAXIMUM_PRIORITY`, a kernel constant VxWorks has no equivalent of, and a
change to the RTEMS core-priority mechanism must not silently move the VxWorks
value with it. The two happen to land on the same number; they do not share a
derivation.

Real-time priority defaults **ON** for `epics_embedded_target`, as on RTEMS:
neither `RLIMIT_RTPRIO` nor `CAP_SYS_NICE` gates exist there and there is no
desktop to wedge. `EPICS_RS_ALLOW_RT_PRIORITY=NO` still turns it off.

---

## 5. Measured on target

Every row below ran on `gv100` against the three branches merged, **with no
source edits and `rtems-exec-model` absent** — the feature's absence is the
point of the KEY rows: it proves the target selects the executor backend by
target predicate, not by someone remembering a flag.

The console transcripts below are reproduced as captured, so their banners
still read `rtems-ca-ioc` / `rtems-pva-ioc`: the binaries were renamed to
`realtime-ca-ioc` / `realtime-pva-ioc` after these runs, and a banner is a
line the binary printed, not a claim about what it is called today. Prose and
file names outside the fences use the current names.

### 5.1 Gate rows — 11/11 `EXIT=0`

The script's whole matrix: six libs — `epics-libcom-rs`, `epics-base-rs`,
`epics-ca-rs` (`client-core`), `epics-pva-rs`, `epics-rtems-boot`,
`epics-bridge-rs` (`qsrv-core,pvalink`) — both bins with and without
`bringup-probes`, and the ratchet row `epics-pva-rs --features client`, whose
extracted count is **0 target errors**. That is the same zero the RTEMS gate
reports, for the same reason (UDP search gated out, so `SearchTransport` has
its single `NameServersOnly` variant) but through `epics_embedded_target`
rather than `cfg(target_os = "rtems")`.

Run under shape 3 of §1.2 — stock `nightly` plus an explicit
`VXWORKS_CARGO_CONFIG` patch pointing at the bring-up checkout (content now
on the fork branch), no `--locked`.

Real links, feature absent: `realtime-ca-ioc.vxe` 116,440,216 B and
`realtime-pva-ioc.vxe` 158,996,752 B, both ELF x86-64 static RTP, `T main` present
at `0x224f90`. With `bringup-probes`: 118,207,216 B and 160,199,848 B.

Those are **dev** links, which is what the gate builds. Release, strip and LTO
figures for both targets are in §5.5.

The probes build was the **first-ever compile of every `target_os = "vxworks"`
line in `stats/vxworks.rs`** — 0 errors, first try.

### 5.2 CA — read and write round-trip

```
rtems-ca-ioc: serving 3 records on CA port 5064 (TCP + UDP search),
RTEMS execution model, no tokio runtime
```

That banner is the proof of §2.1: `exec_backend` on `target_os = "vxworks"`
with no feature. Records `RTEMS:AO` / `RTEMS:LO` / `RTEMS:MSG`; read
`RTEMS:AO → 1.5` (DBR_DOUBLE); write `42.5 → WRITE_NOTIFY status=1
(ECA_NORMAL)`, readback `42.5`.

### 5.3 PVA — NTScalar over the wire

```
rtems-pva-ioc: serving 3 records on PVA TCP port 5075 (UDP search on 5076),
GUID b1bc740ef1ea7bfaf4085362, RTEMS execution model, no tokio runtime
```

QSRV2 enabled. `pvxinfo RTEMS:PVA:AO` → struct `epics:nt/NTScalar:1.0`;
`pvxget RTEMS:PVA:AO` → `value double = 1.5`, full NTScalar.

### 5.4 The census blocks, verbatim

```
TASKDUMP begin tag=c6-6 count=19 capacity=192 dropped=0 source=registry
TASKDUMP scope tag=c6-6 lists only threads that called
runtime::task::enter_ioc_thread, plus main; VxWorks has no RTP task
enumerator (taskIdListGet is kernel-only), so a std::thread spawned
outside the runtime seam is invisible here
```

(the scope line is one physical `println!`, wrapped here.) `count=19` is
`main` (`iCaprobe`) plus 18 IOC threads — cbLow/Medium/High,
Timer, scanOnce, scan-owner, the seven periodic scan threads, CAC-dial 0,
status-pv, CAS-TCP, CAS-UDP, c6-probe. `dropped=0`.

The descriptor census, with the fields that were captured — `mode`, `rdev`,
`so_type` and `peer` are emitted too and are elided here as `…` rather than
guessed at:

```
FDCENSUS begin tag=c6-6
FDCENSUS tag=c6-6 fd=0 kind=chardev …
FDCENSUS tag=c6-6 fd=1 kind=chardev …
FDCENSUS tag=c6-6 fd=2 kind=chardev …
FDCENSUS tag=c6-6 fd=3 kind=tcp … listening=1 local=0.0.0.0:5064 …
FDCENSUS tag=c6-6 fd=4 kind=udp  … local=0.0.0.0:5064 …
FDCENSUS end tag=c6-6 open=5 max=1000
```

fd 0–2 are the serial chardev of §6; fd 3 and fd 4 are CAS-TCP and CAS-UDP,
the entire socket surface of a CA IOC serving three records. `max=1000` is
`rtpIoTableSizeGet`, the RTP's own descriptor table — not
`sysconf(_SC_OPEN_MAX)`, which is the POSIX limit rather than the table being
walked. Re-measured after the extern's return type was corrected to `size_t`:
`max=1000`, **unchanged**. The width change is unobservable on a healthy guest
by construction — it decides only what the `ERROR` return becomes — so compile
plus boot plus an unchanged reading is the whole bar it can clear. `STACKUSE` present, per-task size/current/high/margin from
`taskInfoGet`.

Status PVs read over CA, matching the census printed beside them:

```
RTEMS:MEM_USED = 17022976.0
RTEMS:MEM_FREE = nan
RTEMS:MEM_MAX  = nan
RTEMS:FD_CNT=5.0  FD_MAX=1000.0  FD_FREE=995.0
```

`MEM_USED` is `mi_process_info`'s `current_commit`; the two `nan`s are the
§3.1 decision arriving at the operator, and the console's own MEM line
(`MEM_FREE=-1 MEM_USED=16998400`, sampled moments earlier) is the same pair in
the C-side spelling.

The status-PV prefix is the compile-time constant `RTEMS` in
`realtime-ca-ioc.rs`, so it reads `RTEMS:` on a VxWorks image. No runtime config
surface; renaming it is the same decision as renaming the bins (§2.4).

### 5.5 Release image size: strip + LTO (measured)

The §5.1 figures are **dev** images — the gate builds `check`/`dev`, so those
are the numbers the rest of this file quotes. They are not what anyone ships.
Measured 2026-07-25/26 on `gv100`, five rows per target, both binaries each.

**Every row below was measured as a CLI `--config` row** — `[profile.release]`
itself carries no `strip`/`lto`/`codegen-units` override in any manifest, so a
plain `cargo build --release` still produces the row-2 sizes and host build
times are unaffected. The row-5 settings are adopted as the workspace profile
`release-embedded` (root `Cargo.toml`: inherits `release`, `strip = "symbols"`,
`lto = "fat"`, `codegen-units = 1`); substitute `--profile release-embedded`
for `--release` in the row-2 invocations below to build the deployable image.
`scripts/embedded-image.sh <rtems|vxworks> <ca|pva>` is the owned entry point
for exactly that build — it defaults to `release-embedded` (Cargo itself has
no target-conditional profile, so the default lives at the entry point;
override with `EMBEDDED_PROFILE=` for a comparison row).

#### `x86_64-wrs-vxworks` (`.vxe`)

| Row | CA bytes | Δ vs row3 (strip) | PVA bytes | Δ vs row3 | CA cold build¹ | CA MAXRSS |
|---|---:|---:|---:|---:|---:|---:|
| 1 dev | 116,768,688 | — | 159,248,592 | — | 85.69 s | 1,034,284 KB |
| 2 `--release` | 7,274,408 | — | 10,477,624 | — | 87.02 s | 1,072,120 KB |
| 3 `+strip=symbols` | 5,287,328 | (baseline) | 7,851,744 | (baseline) | 85.80 s | 1,073,000 KB |
| 4 `+lto=thin +cgu1` | **4,447,584** | **−839,744 (−15.9%)** | **6,418,080** | **−1,433,664 (−18.3%)** | 144.75 s (1.66×) | 1,071,716 KB |
| 5 `+lto=fat +cgu1` | **4,287,696** | **−999,632 (−18.9%)** | **6,077,968** | **−1,773,776 (−22.6%)** | 163.49 s (1.88×) | 1,071,788 KB |

fat beats thin: CA −159,888 B (−3.6%), PVA −340,112 B (−5.3%). dev→fat overall:
CA **27.2×**, PVA **26.2×** smaller.

(Row 1 differs by ~330 KB from §5.1's dev figures — a different build on a
different day, not a discrepancy to reconcile. Rows are comparable to each
other, which is what the matrix is for.)

#### `armv7-rtems-eabihf` (ELF)

| Row | CA bytes | Δ vs row3 (strip) | PVA bytes | Δ vs row3 | CA cold build¹ | CA MAXRSS |
|---|---:|---:|---:|---:|---:|---:|
| 1 dev | 122,884,636 | — | 159,408,272 | — | 80.44 s | 1,025,556 KB |
| 2 `--release` | 7,965,212 | — | 11,015,520 | — | 82.99 s | 1,147,632 KB |
| 3 `+strip=symbols` | 5,452,848 | (baseline) | 7,676,976 | (baseline) | 84.44 s | 1,127,740 KB |
| 4 `+lto=thin +cgu1` | 4,764,592 | −688,256 (−12.6%) | 6,517,680 | −1,159,296 (−15.1%) | 143.47 s (1.73×) | 924,340 KB |
| 5 `+lto=fat +cgu1` | **4,604,848** | **−848,000 (−15.6%)** | **6,165,424** | **−1,511,552 (−19.7%)** | 168.17 s (2.03×) | 934,348 KB |

fat beats thin: CA −159,744 B (−3.4%), PVA −352,256 B (−5.4%). dev→fat overall:
CA **26.7×**, PVA **25.9×**.

#### Cross-target summary

`fat+cgu1` versus `strip`: **vxworks CA −18.9% / PVA −22.6%; RTEMS CA −15.6% /
PVA −19.7%.** Both targets agree on every qualitative point — fat beats thin,
LTO roughly doubles the cold build, no linker memory trouble, and the smallest
image boots and round-trips with behaviour unchanged.

#### Methodology

¹ **Cold `CARGO_TARGET_DIR` per row** (`rm -rf` before each), so no row reuses
another's artefacts. **The CA column is the comparable build-time number**: it
is a full rebuild including `std` under `-Zbuild-std`. PVA times are *warm* —
built after that row's CA — and load-sensitive on this shared box, where RTEMS
guests and other work run concurrently; the vxworks release-PVA row came in at
267 s, a load outlier. PVA build times are therefore not quoted as a column and
should not be compared across rows.

**Linker memory: no trouble at any row.** `/usr/bin/time` MAXRSS for every CA
build — the tabulated column, fat LTO included — is **≈0.92–1.15 GB** across
both targets, against 309 GB free. No `collect2`/`ld`/`cannot allocate`/OOM
signature and no undefined-reference failure in any of the twenty builds.

**Boot-verified on the smallest image of both targets** — row 5, `fat+cgu1` CA:

* vxworks, 4,287,696 B, `rtpSp "/host.host/realtime-ca-ioc.vxe"`: boots at
  `-m 2048` **and** `-m 1024`; banner `realtime-ca-ioc: serving 3 records on CA
  port 5064`; all three record lines; `caget RTEMS:AO` 1.5 → `caput` 42.5 →
  `WRITE_NOTIFY ECA_NORMAL` → readback 42.5 (and 7.25 at `-m 1024`); no scan
  FATAL; `rtpShow` TASK CNT 17.
* RTEMS, 4,604,848 B, fresh `qemu-system-arm -M xilinx-zynq-a9 -m 256M`,
  hostfwd 15064: DHCP BOUND, same banner, three record lines, `caget` 1.5 →
  `caput` 42.5 `ECA_NORMAL` → readback 42.5.

#### Verbatim invocations

These are the invocations the table's numbers were **measured** under, kept
verbatim as the record; the `--config …libc.path="…/libc-vx"` in them is the
bring-up-era local checkout whose commits now live on the fork branch
`epics-rs-0.2` (§1.2). A reproduction today uses
`scripts/embedded-image.sh vxworks <ca|pva>`, which derives that patch from
the manifest pin.

VxWorks — common prefix for every row:

```
cargo +nightly build -Zbuild-std=std,panic_abort --target x86_64-wrs-vxworks \
  --config 'patch.crates-io.libc.path="/home/coding-agent/vx-bringup/libc-vx"'
```

```
# Row1 dev CA
cargo +nightly build -Zbuild-std=std,panic_abort --target x86_64-wrs-vxworks --config 'patch.crates-io.libc.path="/home/coding-agent/vx-bringup/libc-vx"' -p epics-ca-rs --bin realtime-ca-ioc --no-default-features --features client-core
# Row1 dev PVA
cargo +nightly build -Zbuild-std=std,panic_abort --target x86_64-wrs-vxworks --config 'patch.crates-io.libc.path="/home/coding-agent/vx-bringup/libc-vx"' -p epics-bridge-rs --bin realtime-pva-ioc --no-default-features --features qsrv-core,pvalink
# Row2 --release CA / PVA  (same as Row1 + --release, same -p/--bin/--features)
cargo +nightly build --release -Zbuild-std=std,panic_abort --target x86_64-wrs-vxworks --config 'patch.crates-io.libc.path="/home/coding-agent/vx-bringup/libc-vx"' -p epics-ca-rs     --bin realtime-ca-ioc  --no-default-features --features client-core
cargo +nightly build --release -Zbuild-std=std,panic_abort --target x86_64-wrs-vxworks --config 'patch.crates-io.libc.path="/home/coding-agent/vx-bringup/libc-vx"' -p epics-bridge-rs --bin realtime-pva-ioc --no-default-features --features qsrv-core,pvalink
# Row3 --release + strip CA / PVA  (Row2 + one extra --config, NO repo edits)
cargo +nightly build --release -Zbuild-std=std,panic_abort --target x86_64-wrs-vxworks --config 'patch.crates-io.libc.path="/home/coding-agent/vx-bringup/libc-vx"' --config 'profile.release.strip="symbols"' -p epics-ca-rs     --bin realtime-ca-ioc  --no-default-features --features client-core
cargo +nightly build --release -Zbuild-std=std,panic_abort --target x86_64-wrs-vxworks --config 'patch.crates-io.libc.path="/home/coding-agent/vx-bringup/libc-vx"' --config 'profile.release.strip="symbols"' -p epics-bridge-rs --bin realtime-pva-ioc --no-default-features --features qsrv-core,pvalink
# Row4 thin+cgu1 CA
cargo +nightly build --release -Zbuild-std=std,panic_abort --target x86_64-wrs-vxworks --config 'patch.crates-io.libc.path="/home/coding-agent/vx-bringup/libc-vx"' --config 'profile.release.strip="symbols"' --config 'profile.release.lto="thin"' --config profile.release.codegen-units=1 -p epics-ca-rs     --bin realtime-ca-ioc  --no-default-features --features client-core
# Row4 thin+cgu1 PVA  (…same configs…) -p epics-bridge-rs --bin realtime-pva-ioc --no-default-features --features qsrv-core,pvalink
# Row5 fat+cgu1 CA
cargo +nightly build --release -Zbuild-std=std,panic_abort --target x86_64-wrs-vxworks --config 'patch.crates-io.libc.path="/home/coding-agent/vx-bringup/libc-vx"' --config 'profile.release.strip="symbols"' --config 'profile.release.lto="fat"'  --config profile.release.codegen-units=1 -p epics-ca-rs     --bin realtime-ca-ioc  --no-default-features --features client-core
# Row5 fat+cgu1 PVA  (…same configs…) -p epics-bridge-rs --bin realtime-pva-ioc --no-default-features --features qsrv-core,pvalink
```

Per build: `source ~/vx-bringup/vxenv25.sh; export
CARGO_TARGET_DIR=$HOME/vx-phase1-target; rm -rf $CARGO_TARGET_DIR`. All ten
vxworks builds `Finished [optimized]`, EXIT=0.

RTEMS — same five rows over the RTEMS route: toolchain env, `~/.cargo/config.toml`
`[patch.crates-io]` `libc-bringup`, `cargo update -p libc --precise 0.2.188`, and
the TLS spec of `doc/rtems-tls-spec-deviation.md`
(`armv7-rtems-eabihf-tls.json`, `-Zjson-target-spec`, `arm-rtems6-gcc`).

```
# Row5 fat+cgu1 CA
cargo +nightly build --target "$TGT" -Zbuild-std=std,panic_abort -Zjson-target-spec --release --config 'profile.release.strip="symbols"' --config 'profile.release.lto="fat"' --config profile.release.codegen-units=1 -p epics-ca-rs --bin realtime-ca-ioc --no-default-features --features client-core
# Row5 fat+cgu1 PVA  (same configs) -p epics-bridge-rs --bin realtime-pva-ioc --no-default-features --features qsrv-core,pvalink
```

---

## 6. Boot rig

`cargo check` needs none of this. Booting does, and the path took several
attempts, so the working form is recorded verbatim.

```sh
qemu-system-x86_64 -m 1024M -kernel $SDK/vxsdk/bsps/itl_generic_3_0_0_5/vxWorks \
  -net nic -net "user,hostfwd=tcp:127.0.0.1:11534-:1534,hostfwd=tcp:127.0.0.1:15064-:5064,\
hostfwd=tcp:127.0.0.1:15075-:5075,guestfwd=tcp:10.0.2.100:21-cmd:python3 /tmp/pybridge.py 2121,\
guestfwd=tcp:10.0.2.100:60000-cmd:python3 /tmp/pybridge.py 60000, ...60001..60005 same..." \
  -display none -monitor none \
  -chardev socket,id=vcon,path=/tmp/vxcon.sock,server=on,wait=off -serial chardev:vcon \
  -append "bootline:fs(0,0)host:vxWorks h=10.0.2.100 e=10.0.2.15 u=target pw=vxTarget o=gei0"

# console:  nc -U /tmp/vxcon.sock < con.in > console.log &
# ftpd:     python3 /tmp/ftpd2.py root 127.0.0.1   (bind 2121, masq 10.0.2.100, pasv 60000-60005)
# launch:   echo 'rtpSp "/host.host/<bin>.vxe"' > con.in
```

Four constraints, each of which cost a debugging round:

* **The FTP bridge must propagate EOF.** `rtpSpawn` loads the RTP through
  netDrv's FTP client. An unprivileged bridge built on `cmd:nc` transfers every
  byte — ftpd logs `RETR completed=1` with the full count — but never closes
  the data socket, so netDrv never sees end-of-file, `rtpSpawn` blocks forever
  and the kernel shell wedges in `rtpSp` with no `value=`. `nc -N` does not fix
  it. The fix is a bridge that **exits the instant the host socket closes**
  (`/tmp/pybridge.py` on the box); ftpd then logs a clean `session closed` and
  the RTP runs.
  This was diagnosed by discriminator, not by inspection: the known-good
  milestone binary and an 882 KB trivial RTP wedge identically in the same
  environment, which exonerates the IOC code, the image size and the NIC stack.
* **Serial must be off-stdio.** `-serial stdio` collides with the `cmd:`
  chardevs. Console goes to a unix-socket chardev with an `nc -U` bridge.
* **Legacy `-net nic -net user`, with `o=gei0` in the bootline.** The bootline
  device name has to match the onboard NIC the BSP enumerates.
* **Two full IOCs contend on a 1 GB TCG guest.** Delete the CA IOC before
  running `pvxget` against the PVA one.

Only privileged-port availability distinguishes this from the earlier
milestone: with a native ftpd on `:21` and `h=10.0.2.2` the image booted in
seconds. The bridge above exists because the box has no `sudo`, no `authbind`,
no `setcap`, and `net.ipv4.ip_unprivileged_port_start=1024`.

---

## 7. Known opens

* **E9 — DONE 2026-07-26**, in
  [vxworks-circuit-wedge-on-target-measurement.md](vxworks-circuit-wedge-on-target-measurement.md).
  The `CircuitDeathGuard` A/B was run on target and the fix confirmed: the
  control image holds one descriptor (`peer=none(errno=57)`, the same local
  port) across seven consecutive censuses of a 480 s link cut, the fix image
  holds none. The redial ladder that had to be measured first is **75.0 s per
  rung, 74900–75000 ms over eight rungs**, ~30 s of client backoff between
  rungs — it coincides with the RTEMS `TCPTV_KEEP_INIT`, which was not knowable
  in advance. The errno does not transfer (BSD numbering, and it varies between
  `EHOSTUNREACH` 65 and `ETIMEDOUT` 60 across attempts of the same cut), nor
  does the RTEMS census signature (`mode` stays `0140666` on a dead VxWorks
  socket). Running it uncovered and fixed one shipped defect: `SO_SNDTIMEO` is
  unimplemented here and `drive_socket_blocking` propagated its `ENOPROTOOPT`
  fatally, so a CA client on this target established no circuits at all.
  The first fix for that made the option best-effort and was a workaround; the
  round that closed it moved the bound inside `write_frame_deadline`
  (`poll(POLLOUT, remaining)` over a socket the function itself puts in
  non-blocking mode), so the send deadline no longer depends on a socket option
  any target may refuse and `SO_SNDTIMEO` is now set nowhere in the write path.
  That first landed resting on `send(MSG_DONTWAIT)` rather than on the mode,
  which Darwin ignores and this target was never measured honouring; corrected
  in v0.25.2, `doc/darwin-send-dontwait-gap.md`. Measured on target in one boot against a
  peer that accepts and never reads: the bounded path returned at **2016 ms
  against a 2000 ms deadline**, the old path had not returned **127.5 s** later
  and still held its descriptor (§5.1). This removes `SEND_TICKS_PER_DEADLINE`
  and `send_tick_for` from the public API of `epics-libcom-rs`, so the next
  release is a **0.26.0 minor bump** — decided against a deprecated shim, since
  the two items exist only to split a deadline the caller no longer owns.
* **The same `SO_SNDTIMEO` defect in `asyn-rs`, closed — latent until the
  crate's port gap is closed.** `drivers::ip_port::IpIoState::write` armed the option on
  all three of its arms (TCP, UDP, Unix) and propagated the result with `?`, so
  the write path would fail before sending a byte on a target that refuses the
  option. C never sets it here either: `drvAsynIPPort.c` compiles the option
  only under `USE_SOCKTIMEOUT`, which is `#if defined(__rtems__)` (`:71-72`),
  while vxWorks takes `USE_POLL` through its own `FAKE_POLL` (`:75-76`) and
  bounds `writeIt` with `poll(POLLOUT, writePollmsec)` (`:667-686`) on a socket
  `connectIt` already made non-blocking (`:511`). `write_with_retry` now has
  C's shape, proven by a host regression test that fails without it.
  **On-target verification is not available until an open port gap is closed.**
  `asyn-rs` does not build for VxWorks or RTEMS: its `socket2` dependency is
  unconditional, in the shared `[dependencies]`
  (`crates/asyn-rs/Cargo.toml:16`), where `epics-ca-rs` keeps
  `socket2`/`if-addrs`/`tokio` full behind
  `[target.'cfg(not(any(target_os = "rtems", target_os = "vxworks")))'.dependencies]`
  (`crates/epics-ca-rs/Cargo.toml:82`); `socket2` 0.5.10 aliases `IovLen` under
  two `target_os` lists that omit vxworks, and the crate is absent from
  `vxworks-check.sh`'s `CRATES`, so nothing catches a regression here. That is
  a gap, not a decision: C `asyn` supports vxWorks explicitly —
  `drvAsynIPPort.c:63` disables `AF_UNIX` for vxWorks alone, `:75` selects
  `FAKE_POLL` for it, `setNonBlock` branches to `ioctl(fd, FIONBIO, &flags)`
  at `:178`, the socket `cleanup` comment at `:243` exists to "reduce problems
  with vxWorks when the IOC restarts", and
  `asynDriver/epicsInterruptibleSyscall.c:31` includes `ioLib.h` under
  `#ifdef vxWorks`. Closing that gap is separate work and is not attempted
  here; until then this fix is proven on the host only.
* **The CA name-service connection had no liveness rule — closed, measured
  both ways.** `run_nameserver_connection` sent `CA_PROTO_ECHO` on a hardcoded
  60 s tick and never checked that it was answered, so an
  `EPICS_CA_NAME_SERVERS` peer that accepts, keeps reading and never replies was
  held indefinitely: ten consecutive censuses on the same local port over
  ≈600 s, one accept, dial-pool `attempts=1`, against the data circuit's 35 s
  bound (`EPICS_CA_CONN_TMO` + `ECHO_TIMEOUT_SECS`). The link-cut A/B cannot
  reach this because a cut always ends in TCP giving up — which is also what
  retired the name-service socket 30 s after the data circuit in that run. C has
  no such asymmetry: `cac::setSearchDestinations` builds a name server through
  `findOrCreateVirtCircuit`, so it is a `tcpiiu` with the same watchdogs. The
  fix inherits the rule the same way rather than branching on the peer's kind:
  `CircuitWatchdog` is returned by `split_circuit` with the reader and writer
  halves and taken **by value** by `read_loop`, so a CA TCP reader without a
  liveness rule does not compile. Re-measured on the E9 rig with the fix
  compiled in: seven retirements at **35, 35, 35, 37, 35, 35, 35 s** against the
  35 s bound, eight dial/retire cycles in 8 minutes with `FD_CNT=6` and
  `MEM_USED=17022976` on all 47 censuses. One deliberate deviation: C's
  `unresponsiveCircuitNotify` keeps the socket, our name-service reader ends and
  redials after `CONN_TMO`
  ([§3.5](vxworks-circuit-wedge-on-target-measurement.md)).

  **The verdict and the descriptor now land together.** Retiring the reader is
  not retiring the circuit: a descriptor lives until *both* halves are dropped,
  and the reconnect backoff sat in the same scope as the send half, so a circuit
  the watchdog had just declared unresponsive stayed open a further
  `EPICS_CA_CONN_TMO` — 69.6 s of socket for a 35 s verdict, on the host test
  that now bounds it. `serve_nameserver_circuit` is the circuit's scope and the
  backoff is in its caller. Measured on the rig against a control image that
  differs by that one hunk: the descriptor goes at `seq=16` with the fix and at
  `seq=19` without it — 30 s apart — and only the control ever censuses it as
  `peer=none(errno=57)`, the VxWorks reading for a socket already shut but still
  allocated ([§3.6](vxworks-circuit-wedge-on-target-measurement.md)). The data
  circuit gets the same guarantee from a different owner, `CircuitDeathGuard`,
  and is the later of the two afterwards (138 s) because C keeps an unresponsive
  circuit's socket on purpose.
* **E8 is measured.** E8 (pool probe) was re-authored for
  this target as `doc/vx-ca-pool-probe.patch` and run under load:
  `doc/vxworks-ca-worker-pool-on-target-measurement.md`. Holding 141 concurrent
  CA clients, `POOLPROBE` gives `BUSY == SETS == CONNS` with
  `worker_count() == 2 × sets` on every sample and `sets` never past
  `CAS_CLIENT_POOL_CAPACITY = 141`, and all 282 pooled workers read
  `vx = 199 - epics` with zero mismatches over three independent instruments
  (`pthread_getschedparam`, `taskPriorityGet`, `taskInfoGet`). E9's ladder and
  E10's dial residue have since been measured too — see their bullets.
* **E10 is closed, both halves.** The pooled-vs-per-attempt dial residue is
  **0 B/attempt on VxWorks 7**, for the CA half *and* the PVA half, measured
  2026-07-26 by `-Wl,--wrap` live-block accounting over ~230 dial attempts per
  image: CA differs by −184 B, PVA by +184 B, in both cases exactly one 184 B
  timer-sleep group of sampling skew (resolution ±0.8 B/attempt). The brief's
  `semMCreate`/`semBCreate` hypothesis is measured false with the mechanism
  named — VxWorks 7 returns per-thread blocks *and* per-thread semaphores on
  thread exit, so 235 extra thread creations add zero live blocks in all three
  per-thread size classes. `DialPool` therefore buys no heap residue on this
  target; its value here is thread-creation rate and the fd/stack ceiling.
  `doc/vxworks-dial-attempt-residue-on-target-measurement.md` carries the
  transcripts and the rig (`doc/vxworks-e10-rig/`).
* **E10's `timer_sleep` growth was a leak, and it is fixed at source.** The
  184 B per ~30 s that round 1 left open is wall-clock paced, not attempt
  paced — halving the dial rate over the same wall clock left it unchanged
  (+6624 B against +6440 B) while the `sleep_until` trough climbed +6 entries
  per 180 s `BEACON_CLEAN_INTERVAL` cycle against a flat arm rate, i.e.
  ~530 KB/day on an unattended IOC. Cause: a `Sleep` dropped before its
  deadline could not take its queue entry back, because `DelayedTimer` held
  entries in a `BinaryHeap`, which addresses only its top; the entry kept the
  future's `Arc<Mutex<SleepState>>` — and the `pthread` mutex `std` creates
  inside it — alive for the whole remaining delay. The queue is now a
  `BTreeMap<WakeKey, _>`, `schedule_wake` returns the key and `Drop for Sleep`
  cancels it, so an entry is addressable by construction. Measured on target:
  `pva-fixed-5s` reads the same `live_bytes` at seq 30, 138 and 139 over 223
  dial attempts and 9.0 M allocations, with **no size class and no call site
  changed**; `pva-fixed-perattempt` repeats it on the other dial arm. This is
  `epics-libcom-rs` runtime code, so it applied to every `exec_backend`
  consumer, RTEMS included.
* **The CA name-server queue ceiling is now observed on VxWorks.** With
  `EPICS_CA_NAMESERVER_QUEUE_DEPTH` compiled in at 8, `fire_searches` holds
  exactly 8 live 144 B blocks for 172 further dial attempts while its call
  count runs 8 → 16 — `ns_try_send` drops rather than queues. Ceiling at the
  shipping default is 256 × 144 B = 36,864 B, reached only while a configured
  name server is unreachable.
* **Connection-wall sizing.** The wall is **not** a fixed client count. Measured
  on one image on two guests: 47 concurrent clients on `~958MB`, refused with
  `EAGAIN` from the worker spawn, and 141 on `~1982MB`, refused by
  `CAS_CLIENT_POOL_CAPACITY` itself. The earlier "44 concurrent clients" figure
  is retracted — it was reproduced at neither size. Per connection the port
  reserves 3,145,728 B (`CAS-client` Big + `CAS-event` Medium) and *uses*
  19,208 B of it, flat from n = 1 to n = 141; `cbMedium` at 206,800 B is the one
  class whose high-water justifies `Big`. Not a formula difference:
  `StackSizeClass::bytes` is `f * 0x10000 * size_of::<usize>()`, parameterised
  by pointer width exactly as C's `STACK_SIZE(f)` is, so a 64-bit target costs
  precisely 2× what `armv7-rtems-eabihf` costs, class for class. **Why `EAGAIN`
  lands at 47 is now answered** (§10 of the measurement): a three-arm
  `StackSizeClass` A/B at fixed RAM moved the wall 47 → 59 → 80 as the declared
  per-connection stack fell 3,145,728 → 2,097,152 → 1,048,576, which falsifies
  "the wall tracks declared stack" — that predicted 141, the pool's own
  capacity. Charging each thread its declared stack **plus a flat 1 MiB** makes
  the total at all three walls 246.4 / 247.5 / 251.7 MB, a 2.1 % spread. The
  binding resource is reserved address space, ceiling ~248 MB on this guest, not
  committed memory. Two thirds of the per-connection cost is that per-thread
  overhead, so the class is the minor lever and threads-per-connection the major
  one; `Big` is justified by no high-water measured here (13,240 B in all three
  arms) and costs a fifth of the wall.
* **The class decision, now measured against a real database** (§15 of the
  measurement). The 13,240 B above came from `READ_NOTIFY` on a single `ao`, so
  it is superseded, not confirmed: driven against waveform / `subArray` /
  `compress` / a FLNK chain / an 8-wide `dfanout` / six monitors per connection
  and the `DBR_TIME` and `DBR_CTRL` reply shapes, `CAS-client` measures
  **65,912 B** and `CAS-event` **7,120 B** — 5.0× and 1.19× the old figures — on
  all four threads over four census passes, single-instance proved with
  `rtpShow`. **The number is invariant**: payloads of 8 B, 32,768 B, 262,144 B
  and 1,048,576 B, every reply shape, and all three class arms leave it
  unchanged to the byte, because array payloads are on the heap. **The
  bullet above is wrong on one point and it matters:** C's stack table is not
  one table. `StackSizeClass::bytes` matches C's *POSIX* table
  (`posix/osdThread.c:506-509`), but C on VxWorks uses `{4000, 6000, 11000} *
  ARCH_STACK_FACTOR` (`vxWorks/osdThread.c:63-64`), so C's
  `epicsThreadStackBig` on x86_64 is **22,000 B** and our `Big` is 95.3× it —
  while our measured 65,912 B is 3.0× C's *entire* `Big`, because `park_on`
  pins each connection's async state machine onto the connection stack. So
  "match C's class" is not available as a decision rule here; only the measured
  bytes are. `Big` runs at 3.14 % utilisation and `Medium` would keep 15.9×
  margin while moving the measured wall 47 → 59. The change is **not made**:
  `client_roster` is shared with `armv7-rtems-eabihf`, where no `CAS-client`
  high-water has ever been measured.
* **`MAX_LINK_DEPTH = 16` silently truncates a legal FLNK chain** (§16). A put
  to a 32-deep chain processes the entry plus `L1..L15` and stops; C has no
  depth counter on that path (`processTarget`, `dbDbLink.c:427-436`, guards
  cycles via `pact`, and `MAX_LOCK 10` is an unrelated already-active alarm).
  The bail is `Ok(None)`, so the put returns success with no client notice and
  no record alarm, and the notice goes through `eprintln!` which never reaches
  `errlog` on this target. Its one upside: the cap makes the 65,912 B above
  depth-inclusive by construction.
* **A monitored 1 MiB waveform aborts the RTP at four clients** (§17).
  `EVENT_ADD` on a 131,072-element `DOUBLE` array drove `MEM_USED` 43,278,336 →
  211,804,160 B and killed the process with `memory allocation of 1048576 bytes
  failed` → signal 6. The same workload without that one monitor survives a
  480 s hold with eighty 1 MiB gets and eighty 1 MiB `WRITE_NOTIFY`s, so the
  reply path is sound and the per-subscriber event queues are what exhaust the
  heap.
* **A wall-abort with mutex `EINVAL` — now reproduced, and worse than filed.**
  Two of four wall events this round, in the Big and Small arms and not the
  Medium one, never below the wall, never on a guest where pool capacity binds
  first. The census registry is exonerated. In the Small arm the panic deleted
  the RTP with signal 6, so this is not "a worker is lost while the IOC looks
  healthy" — **a client that fills the pool can take the IOC down.**
  **Root cause now established, statically and on target** (§18). The SDK's
  `pthreadLib.o` shows the first `lock()` on a `PTHREAD_MUTEX_INITIALIZER`
  mutex reaching `pthreadMutexInitComplete` → `pthreadMutexInit` →
  `semMCreate`, which on NULL returns `0x16` — `EINVAL`, not `ENOMEM` —
  propagated verbatim so std panics "invalid argument (os error 22)".
  `pthread_mutex_init` only stamps the magic, so **every** VxWorks pthread mutex
  materialises its semaphore on first lock and eager init is no protection.
  Confirmed with `--wrap=semMCreate --wrap=pthread_mutex_lock`:
  `semMCreate=NULL nth_null=1 succeeded_before=588`, then `lock rc=22`, then the
  fatal panic on `CAS-client 48`. So **the failing allocation is a VxWorks
  semaphore object, not mimalloc heap** — which is why this arm reports no byte
  count while the aborts above do; two allocators fail at the same wall. There
  is no single guilty mutex: it is whichever `std::sync::Mutex` a freshly leased
  worker touches first. The budget number is **588 live semaphores** at 49 sets
  / 98 workers / 48 connections, and it is **transient** — creations passed
  1,024 afterwards — so a bound must throttle creation rate, not only cap a
  total.
* **A panicking worker leaks its pool set permanently.** `BUSY=2 SETS=50
  WORKERS=100 CONNS=0` — nothing returns a `SetLease` whose worker died, so
  capacity drops permanently by one set per panic. `WorkerPool` accounting,
  shared with `armv7-rtems-eabihf`, so outside this row's scope.
* ~~**Connection establishment knees 180× at ~80 concurrent clients.**~~
  **Root-caused and fixed** — same defect as the census truncation below; see
  that bullet.
* **Status PVs are lagging indicators under load — but my band-ordering
  explanation for it was wrong.** I attributed the 402 s reporter silence to
  pooled workers banding at 179/180 against `status-pv`/`scan-owner`/`c6-probe`
  at 189. That is contradicted: with the census registry fixed, the same 282
  thread creations take 10.4 s with **unbroken reporter sequence numbers** and
  no starvation at all. The reporter was blocked on the registry mutex the
  sweeps held, not starved by priority. What survives is the weaker and still
  load-bearing claim: the status PVs are scan-driven, so a burst that completes
  faster than the scan is sampled through pre-burst values — measured this
  round, a 0.9 s burst of 40 read `CONN_CNT=0.0`, and the same burst held 90 s
  read `CONN_CNT=40.0`. **Any row that counts something under load still needs
  a client-side derivation beside the PV**, for scan latency rather than for
  band starvation.
* ~~**The task census truncates before a saturated pool.**~~ **Fixed** — and it
  was the same defect as the 180× handshake knee above, not two. `MAX_TASKS =
  192` in `crates/epics-rtems-boot/src/stats/vxworks.rs` is reached at
  `19 + 2 × (n + 5) = 192`, n = 81.5, exactly the (80, 88] bracket the knee was
  measured in; past it every `insert()` ran a full `retain_live()` sweep — one
  `taskInfoGet` per entry under the registry mutex — reclaiming nothing, at
  3.674 s a sweep and two sweeps per connection. The registry now grows and
  `SWEEP_THRESHOLD_MIN` keeps only its sweep-trigger job. On target the
  141-client ramp fell **402 s → 10.4 s** and the saturated census reads
  `count=301 capacity=unbounded dropped=0`. The RTEMS shim's
  `EPICS_RTEMS_DUMP_MAX_TASKS = 192` (`csrc/rtems_stats.c:148`) has the same
  symptom and **cannot** take this fix: its arrays are filled inside a
  `rtems_task_iterate` visitor where allocating is unsafe. Open, on that target.
* **The CA client pool never shrinks — reclassified from "by design" to a
  defect.** C `rsrv` creates a `CAS-client` thread per accept
  (`caservertask.c:109`) and tears both threads down in `destroy_tcp_client`, so
  its steady-state retention is zero. Ours holds `SETS=45 WORKERS=90` with
  `BUSY=0 CONNS=0` after every client has left; descriptors, connections and
  heap all come back, the threads and their reservation do not. Against the
  ~248 MB ceiling above, 42 retained sets are 53 % of the guest's entire
  reservation held permanently at zero clients — a transient burst becomes a
  permanent exhaustion. Counterweight: the pool exists to bound the RTEMS 128 B
  per-`std::thread` leak, and the VxWorks thread-exit leak is unmeasured (it
  cannot be measured through a pool that never destroys a thread). Even granting
  the RTEMS figure, retaining a thread spends 3,145,728 B to avoid 128 B.
  **Proposed structural fix, not made, awaiting sign-off:** bound the pool by a
  reservation budget rather than by `CAS_CLIENT_POOL_CAPACITY = 141`, which on
  the `~958MB` guest is never the binding term — so the IOC walks past the real
  ceiling into a failing `pthread_create`, whose outcome ranges from a clean
  refusal to an RTP abort.
* **Refusal `errlog` records are dropped without a discard notice.** Both walls
  logged every refusal through `epics_ca_rs::server::blocking`'s `WARN` and
  counted every one in `POOLPROBE REFUSED=`, while the `errlog` leg printed 4 of
  8 on one guest and 3 of 4 on the other and emitted no `messages were
  discarded` line. The `errlog` counter itself is right (it labels the 8th
  refusal `#8`), so the loss is after the call.
* **`MEM_FREE`, `MEM_MAX`, `MEM_BLK` stay `NaN`** until either mimalloc grows a
  public free-bytes accessor or a defensible source appears. §3.1 is the
  standing rejection, not a TODO.
* **Threads that call `name_current_thread()` alone are invisible to the task
  census** — the iocsh script runners, which deliberately take no EPICS band
  and so never reach `enter_ioc_thread`. Pre-existing, and stated in-band in
  the census header.
* **The upstream libc filing has not been sent.**
* **There is no C counterpart to compare against, and there cannot be.** C base
  supports VxWorks 6.6–6.9 — `configure/os/CONFIG.Common.vxWorksCommon`'s
  `VX_GNU_VERSION` table stops at 6.9, so `VXWORKS_VERSION = 7` expands empty,
  and no `configure/os/*vxWorks*` arch file is x86_64. rustc supports 7 only.
  **So "parity" on this target means parity with the RTEMS port beside it** —
  same classification rules, same census format, same scraper — and every
  same-OS parity audit this workspace runs against `/home/stevek/work/epics-base`
  is unavailable here. Where an answer had to come from base, it came from
  base's *source* (the priority map in §4), not from a running C IOC.
