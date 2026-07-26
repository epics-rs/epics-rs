# Per-dial-attempt heap residue on VxWorks 7, both halves, attributed by call site

**Measured 2026-07-26 on the shared build box `192.168.2.128`**
(`qemu-system-x86_64` 8.2.2 Debian 1:8.2.2+ds-0ubuntu1.17, BSP
`itl_generic_3_0_0_5`, `wrsdk-vxworks7-qemu-1.17.0` — VxWorks 7 26.03 sources,
clang 18.1.8.2 — target `x86_64-wrs-vxworks`, nightly
`rustc 1.99.0-nightly (87e5904f5 2026-07-20)`).

The tree is `origin/main` @ `e971e26a`, verified by blob hash rather than by
claim: the four files the rig mutates were compared against
`git rev-parse e971e26a:<path>` and all four `.e10-orig` backups match

```
crates/epics-ca-rs/src/bin/realtime-ca-ioc.rs        abd52a6265b50dc1af8066d399db441e0c0a034e
crates/epics-bridge-rs/src/bin/realtime-pva-ioc.rs   cf68cdf95eaeec62179cebf6ade04274e5561dbc
crates/epics-pva-rs/src/client_native/search_engine.rs 0cb76b57420043a4c3fd8f59b26fe020922bc31e
crates/epics-libcom-rs/src/runtime/blocking_io.rs    7d2940ba7385432ef461dae039dc5152f03fb170
```

## Which halves were measured

**Both.** The RTEMS counterpart
(`doc/upstream-rtems-bugs/measurement-dial-attempt-residue-on-rtems-6.md`)
measured only the CA dial and listed the PVA half under "Not measured / open".
This run closes both:

| half | image | package / features |
|---|---|---|
| CA | `realtime-ca-ioc` | `epics-ca-rs`, `--no-default-features --features client-core,bringup-probes` |
| PVA | `realtime-pva-ioc` | `epics-bridge-rs`, `--no-default-features --features qsrv-core,pvalink,bringup-probes` |

Four images in total — each half built twice, once with the shipping pooled
dial and once with the pre-`9daff491` per-attempt dial.

## Answer

> On VxWorks 7 the pooled dial shape and the per-attempt dial shape grow the
> heap by the same amount, to within **one 184 B allocation group** in either
> direction, over ~230 dial attempts, in **both** halves. The per-attempt
> residue the `DialPool` removes is therefore **0 B/attempt** on this target,
> with a measurement resolution of **±0.8 B/attempt** — the same answer RTEMS
> gave, reached independently and on both halves.

The brief's hypothesis was that VxWorks might be non-zero where RTEMS was zero,
because `semMCreate`/`semBCreate` allocate from the heap on every call and
`std`'s `Mutex`/`Condvar`/thread all use them. That hypothesis is **measured
false, with the mechanism identified**: VxWorks 7 returns every per-thread
block *and* every per-thread semaphore on thread exit. In the per-attempt CA
image the three per-thread size classes go from 67 to 302 allocations — exactly
one per dial attempt — while their live count stays **flat at 18** and their
live bytes stay byte-identical. The PVA per-attempt image reproduces this: 80 →
304 allocations, live flat at 18. `semMCreate`'s own site takes 6,839 more
calls across the window and ends +2 blocks / +80 B.

## Result 0 — the static-linkage precondition, re-confirmed on these four images

`--wrap` is only complete if nothing in the image reaches libc through a
dynamic symbol the static link cannot interpose. Verbatim, all four RTPs:

```
$ readelfpentium -d ca-pooled-unstripped.vxe

There is no dynamic section in this file.
$ nmpentium ca-pooled-unstripped.vxe | grep " T __wrap_" | wc -l
7
$ readelfpentium -d ca-perattempt-unstripped.vxe

There is no dynamic section in this file.
$ nmpentium ca-perattempt-unstripped.vxe | grep " T __wrap_" | wc -l
7
$ readelfpentium -d pva-pooled-unstripped.vxe

There is no dynamic section in this file.
$ nmpentium pva-pooled-unstripped.vxe | grep " T __wrap_" | wc -l
7
$ readelfpentium -d pva-perattempt-unstripped.vxe

There is no dynamic section in this file.
$ nmpentium pva-perattempt-unstripped.vxe | grep " T __wrap_" | wc -l
7
```

No `NEEDED`, no `SONAME`, no dynamic section at all on any of the four RTPs, and
all seven `__wrap_*` interposers are defined `T` in each. `--wrap` is therefore
complete and the `#[global_allocator]` fallback is neither needed nor used.

## Method — `-Wl,--wrap` × 7, live-block accounting, `addr2line`

`doc/vxworks-e10-rig/heapresidue.c`, compiled once to `heapresidue.o` and
linked into each image via `RUSTFLAGS`:

```
-Clink-arg=$OBJ
-Clink-arg=-Wl,--wrap=malloc   -Clink-arg=-Wl,--wrap=free
-Clink-arg=-Wl,--wrap=calloc   -Clink-arg=-Wl,--wrap=realloc
-Clink-arg=-Wl,--wrap=memalign -Clink-arg=-Wl,--wrap=posix_memalign
-Clink-arg=-Wl,--wrap=aligned_alloc
```

Every live block is recorded in a 262144-slot open-addressed pointer table with
its requested size and its call site; `free` removes it with a tombstone. Two
incremental indexes (per requested size, per site) mean a report never walks
the big table. `heapresidue_report(seq, detail)` is called from each IOC's
existing 10 s probe, on the same line group as the `dialpool attempts=` counter
the residue is priced per; the per-size and per-site tables ride the census
cadence (every 6th pass).

Four things this target forces, each measured rather than assumed:

* **The wrap set inverts.** RTEMS deliberately does *not* wrap
  `calloc`/`realloc` because its libcsupport implements both over the public
  `malloc()`/`free()`, so an outer wrapper double-counts. VxWorks 7's libc is
  mimalloc-based and its public entry points dispatch to `mi_*` internals, never
  to each other — so here `calloc`/`realloc` **must** be wrapped or their blocks
  are untracked. `memalign` is a seventh entry point the RTEMS set omits, and is
  the one Rust's over-aligned allocations actually reach: the CA images record
  `memalign=42` and `memalign=325` against `pmemalign=0 alignedalloc=0`.
* **`__builtin_return_address(0)` is usable directly.** RTEMS could not use it —
  rustc emits A32 with LLVM's frame layout while its shim is `-mthumb` with
  gcc's, so the link inserts `____wrap_malloc_from_arm` veneers and the address
  names the veneer. x86_64 has no veneers; the return address resolves straight
  to Rust frames, so the RTEMS rig's conservative stack scan, `SCAN_WORDS`,
  `SITE_PCS` and `bsp_section_text` bounds are all deleted.
* **Locking must be lock-free CAS.** RTEMS used interrupt-off; an RTP cannot
  (`intLock` is kernel-only). A pthread mutex would allocate from inside the
  allocator, and a spinlock deadlocks a uniprocessor under `SCHED_FIFO`. Every
  table here claims slots with `__atomic_compare_exchange_n` and accumulates
  with `__atomic_fetch_add`.
* **The report must be re-callable.** The RTEMS report is a top-N pass that
  *consumes* entries, so it cannot be called twice — fatal for a differential
  sampled every 10 s. This one is a non-destructive threshold print, and `seq`
  is an integer parameter rather than a formatted tag so that no Rust-side
  `CString` allocates from the heap being reported.

**`llvm-addr2line` does not exist in this SDK.** The compiler bin directory
(`compilers/llvm-18.1.8.2/LINUX64/bin`) ships `readelfpentium`, `nmpentium`,
`objdumppentium` and the `wr-*` drivers, but no `addr2line` of any spelling.
The host `GNU addr2line (GNU Binutils for Ubuntu) 2.42` was substituted; it
resolves Rust, libc and VxWorks frames in these RTPs with file:line, e.g.
`0x755595` → `semMCreate` at
`.../vxworks/26.03/source/os/core/user/src/wind/semLib.c:423`. Images are built
with `profile.release.debug="line-tables-only"` and kept unstripped beside the
FTP-served copy.

### Accounting health — an exact identity, on all four images

Every report carries `untracked_free`, `blk_ovf`, `site_ovf` and `size_ovf`.
All four are **0 on every sample of every run**. Beyond that, the four images
satisfy an exact identity that no partial accounting could:

```
alloc − free − realloc == live_blocks

ca-pooled          217616 −   213203 − 3412 = 1001   (live_blocks=1001)
ca-perattempt      223073 −   218156 − 3934 =  983   (live_blocks=983)
pva-pooled       11434819 − 11430905 − 1984 = 1930   (live_blocks=1930)
pva-perattempt   11461756 − 11457328 − 2507 = 1921   (live_blocks=1921)
```

(`realloc` appears once in `alloc` while retiring one block without passing
through `free`; that is the whole of the gap.) This is also what rules out the
one way this shim could fabricate a leak. `HEAPCALL` reports a diagnostic
`in_realloc` counter — `__wrap_malloc` entered while a `realloc` was in flight —
and it is non-zero in the two per-attempt images (`in_realloc=2` for CA,
`in_realloc=800` for PVA). Real nesting would double-insert the pointer and
inflate `live_blocks` permanently by one per nesting. It does not: the PVA
per-attempt image ends at **1921** live blocks against the pooled image's
**1930**, nine *lower*, not 800 higher, and the identity above closes to the
block. `in_realloc` is a non-atomic global flag with no thread-local, so in an
image that creates ~300 threads it fires when a *different* thread is inside
`realloc`. 800 of 11,458,629 mallocs is 0.007%.

## Rig and kill discipline

The box is shared: another arm runs two long-lived `qemu-system-arm` RTEMS
guests (pids 3308690, 3309544 — both verified alive after every run, at 2 days
uptime), and other panels run their own VxWorks guests. **No `pkill` or
`killall` of any kind was run.** `stop-e10.sh` kills only pids this rig wrote to
its own pidfiles, and re-reads `/proc/<pid>/comm` before each kill.
`~/rtems-bringup/rigpid.sh` is unusable here because its `rig_is_qemu`
hardcodes `comm == "qemu-system-arm"` and would silently skip a VxWorks guest.

One trap worth recording: `/proc/<pid>/comm` truncates at 15 characters, so the
comm of a `qemu-system-x86_64` is `qemu-system-x86`. The first version of
`stop-e10.sh` compared against the untruncated name, printed
`SKIP 843212: comm=qemu-system-x86, expected qemu-system-x86_64` and left the
smoke-run guest alive. It was killed by hand after re-reading
`/proc/843212/cmdline`, and the expected string was corrected.

Resource block for this panel and no other: rig dir `~/vx-rig-e10`, hostfwd
21534/25064/25075, rig ftpd 2131 with passive 60010-60015, console socket
`/tmp/vxcon-e10.sock`.

## The four images

All four carry a compiled-in name server at `10.0.2.2:15076` where **nothing
listens**, so every dial fails immediately:

```
DIALPROBE resolve n=1 target=10.0.2.2:15076 elapsed_ms=33 outcome=error:connection refused (os error 61)
DIALPROBE resolve n=2 target=10.0.2.2:15076 elapsed_ms=0 outcome=error:connection refused (os error 61)
DIALPROBE resolve n=3 target=10.0.2.2:15076 elapsed_ms=0 outcome=error:connection refused (os error 61)
DIALPROBE resolve n=4 target=10.0.2.2:15076 elapsed_ms=0 outcome=error:connection refused (os error 61)
```

Both halves are driven at a 5 s redial cadence, so 1500 s yields ~285 attempts
where the shipping cadences would give ~50 (CA's 30 s `EPICS_CA_CONN_TMO`) and
~150 (PVA's 10 s `RECONNECT_INTERVAL`). CA is retuned through a compiled-in
`EPICS_CA_CONN_TMO=5` default; PVA has no environment variable for it, so
`search_engine.rs`'s
`const RECONNECT_INTERVAL: Duration = Duration::from_secs(10)` is edited to `5`.
Neither changes what one attempt allocates; both change how often one happens.

| image | dial shape | half |
|---|---|---|
| `ca-pooled.vxe` | pooled (`CA_DIAL_POOL`, the shipping shape) | CA |
| `ca-perattempt.vxe` | one transient thread per attempt (pre-`9daff491`) | CA |
| `pva-pooled.vxe` | pooled (`PVA_DIAL_POOL`, the shipping shape) | PVA |
| `pva-perattempt.vxe` | one transient thread per attempt | PVA |

**One mutation covers both halves.** The per-attempt arm replaces the body of
`DialPool::dial` in `crates/epics-libcom-rs/src/runtime/blocking_io.rs` — not
the two call sites — because `CA_DIAL_POOL` and `PVA_DIAL_POOL` are two
instances of that one type. The replacement spawns a dedicated thread per
attempt with the same name stem, band, `StackSizeClass::Small` and `oneshot`
reply channel as the pooled path.

**That the mutation was live is not assumed.** In the per-attempt arm `workers`
is incremented and never returned, so the IOC's own probe prints
`workers == attempts` on the same line as the attempt count:

```
ca-pooled
C6 seq=138 dialpool workers=1 attempts=284 queued=0 dialing=0 MEM_FREE=-1 MEM_USED=16998400
ca-perattempt
C6 seq=138 dialpool workers=284 attempts=284 queued=0 dialing=0 MEM_FREE=-1 MEM_USED=16998400
pva-pooled
STAGE5 seq=138 dialpool workers=1 attempts=285 MEM_FREE=-1 MEM_USED=16998400
pva-perattempt
STAGE5 seq=138 dialpool workers=286 attempts=286 MEM_FREE=-1 MEM_USED=16998400
```

The pooled images stay at `workers=1` for all 284/285 attempts — `min=1 max=1`
across every sample — because the dials never overlap, so `MAX_DIAL_WORKERS = 4`
is never approached and one worker serves the whole run.

## Result 1 — CA half: 0 B/attempt

Steady window seq 24 → 138 (115 reports, ~1150 s), both endpoints on the census
cadence so the per-size and per-site tables bracket it. Verbatim endpoints:

```
ca-pooled
C6 seq=24 dialpool workers=1 attempts=50 queued=0 dialing=0 MEM_FREE=-1 MEM_USED=16998400
HEAPLIVE seq=24 live_bytes=437149 live_blocks=978 alloc=39952 free=38290 untracked_free=0 blk_ovf=0 site_ovf=0 size_ovf=0
C6 seq=138 dialpool workers=1 attempts=284 queued=0 dialing=0 MEM_FREE=-1 MEM_USED=16998400
HEAPLIVE seq=138 live_bytes=440709 live_blocks=1001 alloc=217616 free=213203 untracked_free=0 blk_ovf=0 site_ovf=0 size_ovf=0
HEAPCALL seq=138 malloc=214142 free=213203 calloc=20 realloc=3412 pmemalign=0 alignedalloc=0 memalign=42 in_calloc=0 in_realloc=0

ca-perattempt
C6 seq=24 dialpool workers=49 attempts=49 queued=0 dialing=0 MEM_FREE=-1 MEM_USED=16998400
HEAPLIVE seq=24 live_bytes=433082 live_blocks=964 alloc=40857 free=39124 untracked_free=0 blk_ovf=0 site_ovf=0 size_ovf=0
C6 seq=138 dialpool workers=284 attempts=284 queued=0 dialing=0 MEM_FREE=-1 MEM_USED=16998400
HEAPLIVE seq=138 live_bytes=436458 live_blocks=983 alloc=223073 free=218156 untracked_free=0 blk_ovf=0 site_ovf=0 size_ovf=0
HEAPCALL seq=138 malloc=218511 free=218156 calloc=303 realloc=3934 pmemalign=0 alignedalloc=0 memalign=325 in_calloc=0 in_realloc=2
```

| | attempts | live bytes | Δ bytes | Δ blocks | lsq slope |
|---|---:|---:|---:|---:|---:|
| pooled | 50 → 284 (234) | 437149 → 440709 | **+3560 B** | +23 | 6.212 B/attempt |
| per-attempt | 49 → 284 (235) | 433082 → 436458 | **+3376 B** | +19 | 6.263 B/attempt |
| **difference** | | | **−184 B** | −4 | **+0.051 B/attempt** |

The growth decomposes into the *same six* size classes in both images, and the
two dominant classes are identical block-for-block:

| size | Δ count pooled | Δ count per-attempt | resolves to |
|---:|---:|---:|---|
| 1568 | +1 | +1 | `tokio::sync::mpsc::list::Tx<CoordRequest>::push` |
| 144 | **+10** | **+10** | `epics_ca_rs::client::search::fire_searches::{closure#0}` |
| 80 | +3 | +2 | `OnceBox<pthread Mutex>::initialize` |
| 56 | +3 | +2 | `epics_libcom_rs::runtime::background::timer_sleep::sleep_until` |
| 40 | +3 | +2 | `semMCreate` (`semLib.c:423`) |
| 8 | +3 | +2 | `<timer_sleep::Sleep as Future>::poll` |
| | **+3560 B** | **+3376 B** | |

The whole −184 B difference is **one group of those last four classes**
(80 + 56 + 40 + 8 = 184), i.e. one timer-sleep registration in flight at one
sampling instant and not the other. There is no size class present in one image
and absent from the other.

## Result 2 — CA: the per-thread classes, and why VxWorks answers zero

This is the measurement the brief's `semMCreate` hypothesis turns on. Three
size classes are allocated once per thread creation. Their sites, resolved from
the `ca-perattempt` run's own `seq=138` site table (`live` × block size picks
them out; `calls=302` is the run's thread count):

```
size= 2048 pc=0x563e22 calls=302 live=18 -> pthread_key_allocate_data
size=  208 pc=0x5293d2 calls=302 live=18 -> <std::thread::thread::Thread>::new
size= 1064 pc=0x5703b4 calls=302 live=18 -> _InitialiseModuleTLSArea
```

Verbatim, at both window endpoints:

```
ca-pooled
HEAPSIZE seq=24 size=208 live=19 bytes=3952 allocs=19
HEAPSIZE seq=24 size=2048 live=19 bytes=38912 allocs=19
HEAPSIZE seq=24 size=1064 live=19 bytes=20216 allocs=19
HEAPSIZE seq=138 size=208 live=19 bytes=3952 allocs=19
HEAPSIZE seq=138 size=2048 live=19 bytes=38912 allocs=19
HEAPSIZE seq=138 size=1064 live=19 bytes=20216 allocs=19

ca-perattempt
HEAPSIZE seq=24 size=208 live=18 bytes=3744 allocs=67
HEAPSIZE seq=24 size=2048 live=18 bytes=36864 allocs=67
HEAPSIZE seq=24 size=1064 live=18 bytes=19152 allocs=67
HEAPSIZE seq=138 size=208 live=18 bytes=3744 allocs=302
HEAPSIZE seq=138 size=2048 live=18 bytes=36864 allocs=302
HEAPSIZE seq=138 size=1064 live=18 bytes=19152 allocs=302
```

In the pooled image these never move: 19 allocations, 19 live, for the whole
run. In the per-attempt image the allocation count goes **67 → 302** — +235,
exactly one per dial attempt over the window's 235 attempts — while the live
count stays **flat at 18** and the live bytes stay byte-identical. **VxWorks 7
returns every per-thread block on thread exit.** That is the opposite of
stock-spec RTEMS, where the retained 136 B `Arc<Thread::Inner>` is what the
`has-thread-local: true` flip removes, and it is why the VxWorks per-attempt
residue is 0 B/attempt without any spec flip.

`semMCreate` behaves the same way. Its site across the same window:

```
ca-pooled
HEAPSITE seq=24 pc=0x567565 calls=1459 bytes=58360 live=115 livebytes=4600
HEAPSITE seq=138 pc=0x567565 calls=7818 bytes=312720 live=118 livebytes=4720
ca-perattempt
HEAPSITE seq=24 pc=0x566f05 calls=1556 bytes=62240 live=115 livebytes=4600
HEAPSITE seq=138 pc=0x566f05 calls=8395 bytes=335800 live=117 livebytes=4680
```

6,359 further calls (pooled) and 6,839 (per-attempt) land +3 and +2 live blocks,
+120 B and +80 B. The semaphores `std`'s `Mutex`/`Condvar`/thread create are
returned; the hypothesised per-dial `semMCreate` residue does not exist on this
target.

## Result 3 — CA: what does grow is the RTEMS finding, reproduced

The dominant CA growth term is 144 B × 10 blocks, and it is the same site the
RTEMS run found:

```
ca-pooled
HEAPSITE seq=24 pc=0x2f324b calls=8 bytes=1152 live=8 livebytes=1152
HEAPSITE seq=138 pc=0x2f324b calls=18 bytes=2592 live=18 livebytes=2592
ca-perattempt
HEAPSITE seq=24 pc=0x2f31db calls=8 bytes=1152 live=8 livebytes=1152
HEAPSITE seq=138 pc=0x2f31db calls=18 bytes=2592 live=18 livebytes=2592
```

`addr2line` resolves both to
`epics_ca_rs::client::search::fire_searches::{closure#0}` — the search datagram
cloned into the `EPICS_CA_NAME_SERVERS` channel, which nothing drains while the
name server is unreachable. Same site, same 144 B class, same +10 blocks over
the same span, identical in both dial shapes: byte-for-byte the RTEMS reading,
on a different architecture and a different libc.

The ceiling is `EPICS_CA_NAMESERVER_QUEUE_DEPTH` (default 256), i.e.
256 × 144 B = 36,864 B. **That ceiling was proven on RTEMS by pinning the depth
to 8 and observing growth stop dead; it was not re-proven on VxWorks** — see
"Not measured / open".

## Result 4 — PVA half: 0 B/attempt, on a sawtooth

The PVA images carry a superimposed ±23 kB sawtooth of period **18 reports
(180 s)**, present in *both* arms with the *same* phase — peaks at
seq 30/48/66/84/102/120/138, troughs at seq 36/54/72/90/108/126. A least-squares
fit across all samples is biased by it (a sawtooth correlates with a linear
ramp even over an integer number of periods), so the slope is taken
phase-locked: peak-to-peak and trough-to-trough over the same six cycles.

Verbatim endpoints, both at sawtooth peaks:

```
pva-pooled
STAGE5 seq=30 dialpool workers=1 attempts=62 MEM_FREE=-1 MEM_USED=16998400
HEAPLIVE seq=30 live_bytes=663608 live_blocks=1790 alloc=2482111 free=2479498 untracked_free=0 blk_ovf=0 site_ovf=0 size_ovf=0
STAGE5 seq=138 dialpool workers=1 attempts=285 MEM_FREE=-1 MEM_USED=16998400
HEAPLIVE seq=138 live_bytes=670048 live_blocks=1930 alloc=11434819 free=11430905 untracked_free=0 blk_ovf=0 site_ovf=0 size_ovf=0
HEAPCALL seq=138 malloc=11432785 free=11430905 calloc=24 realloc=1984 pmemalign=0 alignedalloc=0 memalign=26 in_calloc=0 in_realloc=0

pva-perattempt
STAGE5 seq=30 dialpool workers=62 attempts=62 MEM_FREE=-1 MEM_USED=16998400
HEAPLIVE seq=30 live_bytes=659600 live_blocks=1777 alloc=2484033 free=2481322 untracked_free=0 blk_ovf=0 site_ovf=0 size_ovf=0
STAGE5 seq=138 dialpool workers=286 attempts=286 MEM_FREE=-1 MEM_USED=16998400
HEAPLIVE seq=138 live_bytes=666224 live_blocks=1921 alloc=11461756 free=11457328 untracked_free=0 blk_ovf=0 site_ovf=0 size_ovf=0
HEAPCALL seq=138 malloc=11458629 free=11457328 calloc=309 realloc=2507 pmemalign=0 alignedalloc=0 memalign=311 in_calloc=0 in_realloc=800
```

| | attempts | live bytes | Δ bytes | Δ blocks | peak-locked | trough-locked |
|---|---:|---:|---:|---:|---:|---:|
| pooled | 62 → 285 (223) | 663608 → 670048 | **+6440 B** | +140 | 28.782 B/att | 29.838 B/att |
| per-attempt | 62 → 286 (224) | 659600 → 666224 | **+6624 B** | +144 | 30.137 B/att | 30.995 B/att |
| **difference** | | | **+184 B** | +4 | **+1.355** | **+1.157** |

Again the entire difference is **one 184 B group**, this time with the opposite
sign to the CA half. The decomposition is four classes and only four, with the
same count in both arms:

| size | Δ count pooled | Δ count per-attempt | resolves to |
|---:|---:|---:|---|
| 80 | +36 | +36 | `OnceBox<pthread Mutex>::initialize` |
| 56 | +35 | +36 | `timer_sleep::sleep_until` |
| 40 | +35 | +36 | `semMCreate` |
| 8 | +36 | +36 | `<timer_sleep::Sleep as Future>::poll` |
| | **+6440 B** | **+6624 B** | |

The per-thread classes reproduce Result 2 exactly. `pva-pooled` holds
`size=208 live=19 allocs=19` and `size=2048 live=19 allocs=19` unchanged from
seq 30 to seq 138; `pva-perattempt` holds `live=18` for both while their
allocation counts go **80 → 304** — +224 against the window's 224 attempts.

```
pva-pooled
HEAPSIZE seq=30 size=208 live=19 bytes=3952 allocs=19
HEAPSIZE seq=30 size=2048 live=19 bytes=38912 allocs=19
HEAPSIZE seq=138 size=208 live=19 bytes=3952 allocs=19
HEAPSIZE seq=138 size=2048 live=19 bytes=38912 allocs=19
pva-perattempt
HEAPSIZE seq=30 size=208 live=18 bytes=3744 allocs=80
HEAPSIZE seq=30 size=2048 live=18 bytes=36864 allocs=80
HEAPSIZE seq=138 size=208 live=18 bytes=3744 allocs=304
HEAPSIZE seq=138 size=2048 live=18 bytes=36864 allocs=304
```

(The PVA image has no 1064 B class — `size=1064` appears nowhere in either PVA
console — so its `_InitialiseModuleTLSArea` block falls in a different size
class. Its other two resolve identically:
`pc=0x751e72 calls=304 live=18 -> pthread_key_allocate_data` and
`pc=0x717012 calls=304 live=18 -> <std::thread::thread::Thread>::new`.)

## Result 5 — a PVA-only growth that the dial shape does not explain

The four classes above are not sampling noise: over the six cycles they add
**+6440 B / +6624 B**, and the sawtooth *envelope* drifts up by a consistent
1104 B per 180 s cycle in both arms. That is one retained 184 B timer-sleep
group per ~30 s, ~6.1 B/s, in `epics_libcom_rs::runtime::background::timer_sleep`
on the PVA name-server reconnect path.

It is **not** attributable to the dial shape — pooled and per-attempt differ by
one group out of 36 — and it is **not** the generic cost of a 5 s redial: the CA
images ran the *same* 5 s attempt cadence and grew those same four classes by
only +3 / +2 blocks each over a longer window, against PVA's +36. It is
specific to the PVA reconnect path's use of `timer_sleep::sleep_until`, whose
`Sleep` future carries a lazily-initialised `pthread` `Mutex` and hence one
`semMCreate` per retained instance.

This measurement cannot say whether it is bounded: both arms ran the same 5 s
cadence for the same ~25 minutes, so wall-clock pacing and attempt pacing are
not separable here, and no ceiling was reached within the run. It is recorded
as an open, not as a leak.

`MEM_USED` (mimalloc `current_commit`, page-granular) read **16998400** on every
sample of all four runs, so it neither confirms nor contradicts a sub-4 KB
delta; `MEM_FREE` is `-1` (NaN) on this target as already recorded in
`doc/vxworks-port.md`.

## What is therefore true

1. **The per-dial-attempt residue attributable to dial thread shape is
   0 B/attempt on VxWorks 7, in both the CA half and the PVA half**, measured
   absolutely by live-block accounting rather than by a slope differential. The
   resolution is one 184 B allocation group over ~230 attempts, i.e.
   ±0.8 B/attempt; the CA half lands at −184 B and the PVA half at +184 B.
2. **The `semMCreate`/`semBCreate` hypothesis is false on this target, with the
   mechanism identified.** VxWorks 7 returns per-thread blocks and per-thread
   semaphores on thread exit: 235 (CA) and 224 (PVA) extra thread creations add
   zero live blocks in all three per-thread size classes.
3. **`DialPool` buys nothing in heap residue on VxWorks.** Its value here is the
   thread-creation *rate* and the fd/stack ceiling, not retained bytes. The
   RTEMS justification (`MAX_DIAL_WORKERS` exists because creations cost on
   RTEMS) does not transfer to this target.
4. **The CA `fire_searches` warm-up reproduces byte-for-byte** across
   architecture and libc: same site, same 144 B class, +10 blocks over the same
   span, identical in both dial shapes.
5. **The PVA half carries a growth the CA half does not** — 184 B per ~30 s in
   `timer_sleep::sleep_until` — which is independent of the dial shape and is
   left open below.

## Not measured / open

* **The `EPICS_CA_NAMESERVER_QUEUE_DEPTH` ceiling was not re-proven here.** On
  RTEMS it was proven by pinning the depth to 8 and watching growth stop dead
  at +8 blocks. This run reproduced the site and the growth but ran no
  depth-pinned VxWorks image, so on this target the ceiling is inferred from
  the shared source, not observed.
* **The PVA `timer_sleep` growth (Result 5) has no measured bound.** Nothing in
  this run distinguishes wall-clock pacing from attempt pacing, and no ceiling
  was reached in ~25 minutes. Whether it is a bounded warm-up like the CA
  channel or an unbounded retention is unanswered.
* **The 1568 B `tokio::sync::mpsc::list::Tx<CoordRequest>::push` block** in the
  CA images (+1 over the window, both arms) was not chased further; it is one
  block per run, identical in both shapes.
* **`MEM_USED` gives no independent cross-check.** On RTEMS the wrapper's number
  was confirmed against `mem_usage()` to within one heap block. Here
  `current_commit` is page-granular and never moved, so the wrapper's number
  stands alone. Nothing else on this image can see the heap.
* **The measurement changes one compiled-in setting per half** — CA's
  `EPICS_CA_CONN_TMO` default to 5, PVA's `RECONNECT_INTERVAL` constant to 5 s.
  Neither changes what one attempt allocates; both change how often one happens.
* **Allocations made through an allocator other than libc's** are invisible to
  the wrapper. On RTEMS this was closed by agreement with `mem_usage()`; here
  `MEM_USED` never moved, so this remains an unclosed edge rather than a
  disproven one.
* **The runs are not load-matched.** Two other panels were building and booting
  on the same box throughout. The accounting is absolute block counting rather
  than a timing-sensitive slope, so load cannot move the byte counts, but the
  attempt *cadence* between runs differs by one attempt (284/284/285/286 over
  the same 1500 s).
* **The `in_realloc` false positive is characterised, not eliminated.** Making
  the flag thread-local would remove it; it was left alone because the exact
  `alloc − free − realloc == live_blocks` identity already proves no
  double-insert occurred.

## Artefacts

Console logs of all four boots, gzipped, in `doc/vxworks-e10-rig/evidence/`:
`console-ca-pooled.log.gz` (790,974 B raw), `console-ca-perattempt.log.gz`
(949,875 B), `console-pva-pooled.log.gz` (705,073 B),
`console-pva-perattempt.log.gz` (865,704 B). Every number in this document was
read from these files with `doc/vxworks-e10-rig/analyse-e10.py`.

Rig sources in `doc/vxworks-e10-rig/` — the box is not backed up:
`heapresidue.c` (the shim), `build-e10.sh` (the `--wrap` link and the two libc
`--config` lines), `boot-e10.sh` / `stop-e10.sh` (own-pid-only rig discipline),
`ftpd-e10.py` (rig ftpd on 2131 / passive 60010-60015), `apply-e10.py` (the
anchored source mutations, `probe` / `perattempt` / `revert`), `analyse-e10.py`,
`env.sh`. The mutations themselves are also carried as a flat patch in
`doc/vxworks-e10-dial-residue-probe.patch`.
