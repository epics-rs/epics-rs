# Upstream bugs found during RTEMS 6 bring-up — recovered evidence

Recovered **2026-07-22** from `coding-agent@192.168.2.128:~/rtems-cside/`, which
is not under version control and has no backup. Before this recovery the
evidence for `doc/rtems-scope-b-session-handoff.md` §6 existed as a single copy
on that desktop; §8.2's premise ("each already has its evidence; what is missing
is the prose") depended entirely on it surviving.

Nothing on the box was written, moved, or modified. Every file here was copied
out read-only and verified byte-identical by SHA-256 on both ends
([table below](#checksums-verified-on-both-ends)).

## Why this directory, and not `doc/upstream-c-bugs.md`

`doc/upstream-c-bugs.md` is the established catalogue of defects in the C/C++
reference found by *parity reading*, one section per `CBUG-` entry in a single
file. These four are a different shape: they were found by *running on a target*,
and their evidence is not quotable code paths but boot logs, syscall-interposer
counts, CPU-idle attribution, two buildable reproducer trees, and two build
patches. That does not fit as inline sections, so it lives here as a directory.
`doc/upstream-c-bugs.md` is the place to cross-reference it from if these are
ever folded into the same index — this recovery deliberately did not edit that
file.

Numbering follows handoff §6 (bugs 1-4). No new `CBUG-`-style IDs were invented.

## The four bugs and where each one lives

| # | bug | primary document |
|---|---|---|
| 1 | **rtems-libbsd** — `poll()` returns `POLLERR` on a valid IMFS FIFO. Root cause of the whole class; breaks every libevent-based program on RTEMS 6, not only pvxs. | [`evidence/FINDING-1-libevent-poll-spin.md`](evidence/FINDING-1-libevent-poll-spin.md) (recovered verbatim) |
| 2 | **pvxs** `src/evhelper.cpp:183` — the RTEMS kqueue avoidance is an RTEMS-5.1 leftover that steers pvxs into the broken poll backend. | [`bug-2-pvxs-evhelper-rtems-kqueue-avoidance.md`](bug-2-pvxs-evhelper-rtems-kqueue-avoidance.md) (**written during this recovery**; the bug had no document of its own) |
| 3 | **pvxs** `bundle/cmake/Platform/RTEMS.cmake:28` — `-specs bsp_specs`, which RTEMS 6 does not install. Nothing compiles. | [`bug-3-pvxs-rtems-cmake-bsp-specs.md`](bug-3-pvxs-rtems-cmake-bsp-specs.md) (**written during this recovery**; previously only `DEVIATIONS.md` lines 35-40) |
| 4 | **EPICS base** — `epicsRtemsFSImage = NULL` faults through `set_directory(NULL)` → `strrchr` before `main()`. | [`evidence/FINDING-2-base-rtems-fsimage-null.md`](evidence/FINDING-2-base-rtems-fsimage-null.md) (recovered verbatim) |

## Measurements taken on the box, which are **not** upstream bugs

* [`measurement-c-thread-priority-on-rtems-6.md`](measurement-c-thread-priority-on-rtems-6.md)
  — **added 2026-07-22, after the recovery.** Does EPICS base's own thread
  ladder take effect on RTEMS 6? Measured: **yes**, on one boot of
  `cioc-fd64.exe` (stock 64-descriptor base configuration, zero base source
  patches). Closes handoff §8.0 gap 1. It also establishes, by measurement
  rather than by reading `configure/toolchain.c`, that an RTEMS 6 build of base
  uses the **POSIX** arm's linear map, and records where libbsd's own threads
  sit relative to `CAS-TCP`. Its evidence is the complete unedited console
  transcript in `evidence/`, and its drivers are in `repro/priority/`. Unlike
  everything else in this directory, this file and its evidence were produced
  by *running* the box rather than by copying from it.

Also recovered, and **not** an upstream bug:

* [`evidence/FINDING-3-per-connection-heap.md`](evidence/FINDING-3-per-connection-heap.md)
  — a measurement write-up of where the C IOC's per-connection heap goes. It is
  kept for the same reason as the rest: **it opens by retracting an earlier
  invalid comparison of its own** (a per-connection residual compared across two
  different builds and two different boots), and a document that hides its own
  corrections is how the next session re-derives the wrong version. The
  retraction is section "Correction to my earlier report"; do not drop it if this
  file is ever edited.
* [`evidence/DEVIATIONS.md`](evidence/DEVIATIONS.md) — the declared-deviation log
  for everything done on the box: which upstream trees were patched (two lines,
  both pvxs), which were not (base: zero source patches), what was site
  configuration rather than source, and that no packages were installed and
  `sudo` was never used. It is also the only original home of bug 3.

Note that both `FINDING-1` and `FINDING-3` carry retractions of earlier claims —
FINDING-1's is in handoff §5.3's "Wrong v1 / Wrong v2" framing and is echoed in
its "Priority hypothesis: tested and rejected" section. The retractions are part
of the evidence.

## Layout

```
doc/upstream-rtems-bugs/
├── README.md                                   this file
├── bug-2-pvxs-evhelper-rtems-kqueue-avoidance.md   written here, sourced from §5.3 + FINDING-1 + DEVIATIONS
├── bug-3-pvxs-rtems-cmake-bsp-specs.md             written here, sourced from DEVIATIONS + RTEMS.cmake.orig
├── measurement-c-thread-priority-on-rtems-6.md     handoff §8.0 gap 1, measured on a boot 2026-07-22
├── evidence/     four markdown files recovered byte-for-byte, unedited, plus
│   │             DEVIATIONS.md (re-copied 2026-07-22 with its session-3 section)
│   └── c-thread-priority-boot-console-2026-07-22.log   the priority boot's whole console
├── patches/      the one-line change to each of the two modified upstream files
│   ├── RTEMS.cmake.orig            pristine upstream file (1,781 B), verbatim
│   ├── pvxs-RTEMS.cmake.diff       bug 3, one line
│   └── pvxs-evhelper.cpp.diff      bug 2, one line, ±8 lines of context
└── repro/
    ├── fsbug/    bug 4 — 12-line application that faults unpatched base at boot
    ├── probe/    bugs 1 and 2 — the libevent/poll spin probe and its --wrap=poll link flag
    └── priority/ the four host-side CA load drivers behind the priority measurement
```

### What was deliberately **not** recovered

* All `.exe` images (`cioc.exe` 37 MB, `pioc.exe` 70 MB, `fsbug.exe` 30 MB,
  `probe.exe`, `testev.exe`, …) and every other binary — `O.<arch>/` build
  directories, `bin/`, `.o`, `.d`.
* The EPICS base and pvxs source trees, and the copied RTEMS toolchain.
* `patches/evhelper.cpp.orig` — 31 KB of a pristine upstream pvxs source file.
  The diff in `patches/pvxs-evhelper.cpp.diff` shows the one-line change with 8
  lines of context on each side, which is what was asked for; the full original
  is a slice of the pvxs tree and stays out. Its checksum is recorded below so
  the diff can be re-verified against upstream `cc7bc72`.
* Boot logs, `ceiling*.py` drivers, `boot-*.sh` scripts, the `cioc`/`pioc`
  application trees. These back handoff §5.1/§5.6/§5.7 rather than the four
  upstream bugs, and were out of scope for this recovery — **they are still
  single-copy on the box.** (Partial exception since 2026-07-22: the *one* boot
  log and the *four* drivers behind the priority measurement are now in
  `evidence/` and `repro/priority/`. `ceiling*.py`, `boot-*.sh` and the
  application trees remain single-copy.)

### Beyond the requested set

`repro/probe/` was recovered although the recovery brief listed only the `fsbug`
sources. Reason: `FINDING-1`'s "Reproduction" section cites
`~/rtems-cside/probe/probeApp/src/probeMain.c` by path as the reproducer for the
root-cause bug, and `DEVIATIONS.md` line 67 cites its `Makefile` for the
`-Wl,--wrap=poll` interposer that produced the 148,081-call figure. Without them
the recovered FINDING-1 points at nothing. Caveat recorded in bug 2's
Reproduction section: the recovered `probeMain.c` is the *last* variant left on
the box (2 s loop, 10 × `nanosleep(100 ms)`), while FINDING-1's tables were taken
at 4 s and 20 × 100 ms. It was copied as found, not adjusted.

## Reproducers

**`repro/fsbug/`** (bug 4) — a `PROD_IOC` whose entire application content is
`const epicsMemFS *epicsRtemsFSImage = NULL;` plus a `printf` in `main()`. Built
against **unpatched** base for an RTEMS target, it faults before `main()` is
entered. `configure/RELEASE.local` carries the absolute paths from the box
(`/home/coding-agent/rtems-cside/...`) and must be re-pointed to build elsewhere.
Its `configure/RELEASE.local` also names `PVXS`, which this application does not
use.

**`repro/probe/`** (bugs 1, 2) — `probeMain.c` does two things: part A polls a
UDP socket, a `pipe()` read end and an `AF_UNIX` `socketpair()` for 1000 ms each
and reports which block and which return immediately; part B runs a libevent loop
with `event_config_avoid_method(conf,"kqueue")` on one thread while the main
thread sleeps, and reports the starvation plus the `__wrap_poll` call count and
the offending descriptor. Requires `USR_LDFLAGS += -Wl,--wrap=poll`, which is in
the recovered `probeApp/src/Makefile`.

Neither reproducer was rebuilt or re-run during this recovery — the box was read
only, no build and no qemu was started. What is verified here is that the sources
are byte-identical to the box, not that they still build.

## Checksums (verified on both ends)

SHA-256, computed on `192.168.2.128` under `~/rtems-cside/` and again on the
copy in this repository. All 23 verbatim files matched; the two generated diffs
were checksummed against the same `diff -u` run piped through `sha256sum` on the
box.

**One row changed on 2026-07-22 after the recovery.** `evidence/DEVIATIONS.md`
was re-copied from the box once the priority measurement had appended its
"Session 3 additions" section to the box's file, so the row below is
`bcd3a3ed…b00b44` and no longer the recovered `8c3d8109…fbb440`. The earlier
content is unchanged; the new section is appended after it. The files added by
that measurement carry their own checksum table, in
[`measurement-c-thread-priority-on-rtems-6.md`](measurement-c-thread-priority-on-rtems-6.md).

| file in this directory | source on the box | sha256 |
|---|---|---|
| `evidence/FINDING-1-libevent-poll-spin.md` | `FINDING-1-libevent-poll-spin.md` | `ed161ccb162b361e381ea85cbbb094b1d3d20866791e9935e20f7804af2255d6` |
| `evidence/FINDING-2-base-rtems-fsimage-null.md` | `FINDING-2-base-rtems-fsimage-null.md` | `2ad0e20893c5a45a75ed4b0e4a8c3f1d3726683cc3388a233de9c13788d73222` |
| `evidence/FINDING-3-per-connection-heap.md` | `FINDING-3-per-connection-heap.md` | `18c9fd69a48efc7123a959770fea6e48744bde3a7553a6c8235d5a31f21f02ff` |
| `evidence/DEVIATIONS.md` | `DEVIATIONS.md` | `bcd3a3ede844cda8ff8296d6e424e57b3f804d4d71f463340328f8769ab00b44` |
| `patches/RTEMS.cmake.orig` | `patches/RTEMS.cmake.orig` | `8c66cd3b8cd51d6153d37f00f95d4db7dd406a9d12bf908a87251620c365280b` |
| `patches/pvxs-RTEMS.cmake.diff` | generated: `diff -u patches/RTEMS.cmake.orig pvxs/bundle/cmake/Platform/RTEMS.cmake` | `3a9adff2382b1ca5a03a70e97a1d49e1e3f489a9414371712e2138b931836210` |
| `patches/pvxs-evhelper.cpp.diff` | generated: `diff -u -U8 patches/evhelper.cpp.orig pvxs/src/evhelper.cpp` | `90303375b1857a54c605edc6ab373f50834f25da6e050b9a73c10e0406053c0d` |
| `repro/fsbug/Makefile` | `fsbug/Makefile` | `453021b7fdbc413c8ea8b1fc5d87ba425140159f37fbdd0b78efa393c2e67405` |
| `repro/fsbug/fsbugApp/Makefile` | `fsbug/fsbugApp/Makefile` | `960535bc161314e71fb4007bbff708cb556647b9299e605ad8f6d66f41f1ed53` |
| `repro/fsbug/fsbugApp/src/Makefile` | `fsbug/fsbugApp/src/Makefile` | `eb8accc5f3358331ac6579216e6f358cf36c7cabc4d93e3eebedc2f089065287` |
| `repro/fsbug/fsbugApp/src/fsbugMain.c` | `fsbug/fsbugApp/src/fsbugMain.c` | `bd479a38b4d1f24a19910ff0b3f1f4072815378eca79787ef9df1c514d2b2bcf` |
| `repro/fsbug/configure/CONFIG` | `fsbug/configure/CONFIG` | `3635f21380223cd15382aeb0eb4d933c106154cbc31880545de6dce518da9798` |
| `repro/fsbug/configure/CONFIG_SITE` | `fsbug/configure/CONFIG_SITE` | `418af52fa673118c478f683d0c3be1f3d4fd6c931193856ccae796fcee45e818` |
| `repro/fsbug/configure/Makefile` | `fsbug/configure/Makefile` | `bc53a6ada3ae3b03f32937725c222302b6adb1b512a4143cd5df693648658a22` |
| `repro/fsbug/configure/RELEASE` | `fsbug/configure/RELEASE` | `695ce8a3e3b337835cc51afa05687845c689017bb281669f909b4ac3c5e21848` |
| `repro/fsbug/configure/RELEASE.local` | `fsbug/configure/RELEASE.local` | `53b45695c1a4d40efa3a4858308604a27b6dcfb52a49960797f767772858013f` |
| `repro/fsbug/configure/RULES` | `fsbug/configure/RULES` | `992ac6dd3999917753508325192fba69dc0ee6b90efbb944924643e50d8fef02` |
| `repro/fsbug/configure/RULES_DIRS` | `fsbug/configure/RULES_DIRS` | `17509b755fe4d3a24bb20116294211c8d1fafac5705e30adf8d93aac98759490` |
| `repro/fsbug/configure/RULES.ioc` | `fsbug/configure/RULES.ioc` | `83aa64637499c7a09508b8c4cfbcd4b9c0e6e280b151d47674603603ac65511a` |
| `repro/fsbug/configure/RULES_TOP` | `fsbug/configure/RULES_TOP` | `1152876c06c70ce615f8d769b13d80234b87acde2116c96c25a27d295a07e404` |
| `repro/probe/Makefile` | `probe/Makefile` | `c06b2429868f2e360f15edfc8cd7932b2a4b6b4ce4b06740cb905d478b08785f` |
| `repro/probe/configure/Makefile` | `probe/configure/Makefile` | `bc53a6ada3ae3b03f32937725c222302b6adb1b512a4143cd5df693648658a22` |
| `repro/probe/probeApp/Makefile` | `probe/probeApp/Makefile` | `960535bc161314e71fb4007bbff708cb556647b9299e605ad8f6d66f41f1ed53` |
| `repro/probe/probeApp/src/Makefile` | `probe/probeApp/src/Makefile` | `8c8bbc6238cc9994fb6e9de45f9c102e60fddc98b03660f2adcbf2d7bbcfb704` |
| `repro/probe/probeApp/src/probeMain.c` | `probe/probeApp/src/probeMain.c` | `5701b8cfb959544ebc12a3aca0fc7a1d3c94eafa05a3a913c0fa59a852ed5706` |

Recorded but **not** copied here, so the diffs can be re-verified later:

| file on the box | sha256 |
|---|---|
| `patches/evhelper.cpp.orig` (pristine pvxs `cc7bc72`) | `13dce3fbdedf279c3594323a4b5307e6df487ac8fa1daacc60a08b038f19b7fd` |
| `pvxs/src/evhelper.cpp` (as modified on the box) | `9b9d256ed86be2bba5264bc9e278302febe38e9c75489a71dcf6c6f4b7487328` |
| `pvxs/bundle/cmake/Platform/RTEMS.cmake` (as modified on the box) | `c49cb5d42ff3a274544e3c889db83f1f122c49f047034b6ff20f3f4e6750d5bd` |

## Two constraints this material is written under

1. **Nothing here is prose for another project's issue tracker.** These are
   evidence and reproduction steps for this repository. No document in this
   directory is, or should become, a drafted issue body, a PR description, or
   text addressed to a maintainer. Where a recovered file already contains
   someone's prose, it is preserved verbatim rather than rewritten.
2. **The claim about pvxs stays narrow.** Handoff §5.3/§8.1: *the reference
   implementation ships an RTEMS-5-era workaround that makes it unusable on
   RTEMS 6 today* — never *a reactor cannot run on RTEMS*, because a libevent
   reactor demonstrably does once steered to kqueue. Relatedly, the libevent
   **select** backend was never tested; every statement about it is labelled a
   hypothesis, in bug 2's document and here.
