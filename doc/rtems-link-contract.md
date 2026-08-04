# The RTEMS link contract — measured, not designed

`doc/rtems-boot-shim-design.md` assumed one crate could own the whole link
contract and emit it from a single `build.rs`. **It cannot.** This document
records the measurement that changed the design, so nobody re-derives it.

Landed as `crates/epics-rtems-boot` on `integration/rtems-scope-b`
(`684e5508`). Nothing in that commit has ever been linked — there is no
`arm-rtems6` toolchain on the development machine. §4 lists exactly what the
box must run to close that.

## 1. What a build script's link output actually reaches

Probed with a real dependent binary, not read from documentation:

| `cargo::` instruction | reaches a dependent binary? |
|---|---|
| `rustc-link-search` | **yes** — lands on the dependent's own rustc line |
| `rustc-link-lib` | **yes, indirectly** — rustc forwards it from the rlib's metadata to the linker, *but only if the binary actually references the crate* |
| `rustc-link-arg` | **no** — applies to the emitting package's own targets, and an rlib performs no link |

The middle row was proved twice over: `rust-lld: error: unable to find library
-lPROBE_LINK_LIB` appeared while the reference existed, and **disappeared when
the reference was removed**.

### Consequences

1. `-u POSIX_Init`, `-B<bsp>/lib` and the five ABI selectors **cannot** be
   pushed from the shim crate into the IOC's link.
2. An **unreferenced rlib is not linked at all**. Without a live reference the
   shim archive, `-lbsd -lm -lz` and `POSIX_Init` all silently vanish. Hence
   `epics_rtems_boot::link_anchor()` is called from `rtems-ca-ioc`'s `main` —
   it is load-bearing, not decorative.

So the contract is delivered in **two halves with one owner**: search paths and
libraries from `epics-rtems-boot/build.rs`; link arguments from a three-line
`epics-ca-rs/build.rs` calling `epics_rtems_boot::contract::emit_link_args()`.
The flag list is written once.

### The split buys the ordering constraint for free

`doc/rtems-qemu-bringup-artefacts.md` §(b) requires `-lbsd -lm -lz` to precede
the `-qrtems` group rather than land inside it. Measured on rustc's own line:
the dependency's `-l` at **arg 36**, the `-C link-arg` at **arg 60**. The
ordering is a consequence of the delivery split, not a flag anyone tuned.

`-B<bsp>/lib` supplies both the BSP `-L` and the `-T linkcmds`, so the linker
script is never named — a test fails if any emitted arg contains `-T` or
`linkcmds`.

## 2. Making Rust agree with the `thumb/armv7-a+simd/hard` multilib

The multilib string is the compiler's own answer, not inferred from `-L` paths:

```
$ arm-rtems6-gcc -print-multi-directory \
    -march=armv7-a -mthumb -mfpu=neon -mfloat-abi=hard -mtune=cortex-a9
thumb/armv7-a+simd/hard
```

`rustc --print target-spec-json --target armv7-rtems-eabihf` (needs
`-Z unstable-options`; the bare form prints nothing) gives `abi = "eabihf"`,
`llvm-floatabi = "hard"`, `features = "+thumb2,+neon,+vfp3"`. **Every
ABI-significant axis already matches.**

The axis that differs is not an ABI axis: rustc emits **A32**, not Thumb (no
`thumb-mode` feature; the triple is `armv7-`, not `thumbv7-`). Armv7-A
interworks and AAPCS is identical for both instruction sets, so `-mthumb` is a
multilib *selector* for the C side, not a constraint on Rust's output. This is
reasoned, not observed — only a real link proves the veneers resolve.

What *is* load-bearing: the five selectors must reach the **link**, because gcc
picks the multilib from its link-time flags. Omit them and it takes the default
and dies about VFP register arguments. They are emitted both as link args and
as the shim's `cc` compile flags, and `contract::check_abi` turns the rustc half
into a build-time assertion against `CARGO_CFG_TARGET_ABI` / `TARGET_FEATURE`.

## 3. One deliberate deviation from the design, and why

Design §3.2 says `build.rs` must hard-fail when `RTEMS_BSP_PREFIX` is unset.
**It does not.** Cargo resolves dependencies identically for `check` and
`build`, so that hard failure would delete the `rtems-check` gate — which §4.2
of the same document calls *"the only gate that works without the box"*. The
two requirements conflict; this resolved toward §4.2.

Instead the crate leaves **one undefined symbol whose name is the diagnosis**.
Verified on real artefacts:

- check-mode RTEMS rlib → `U epics_rtems_boot__RTEMS_BSP_PREFIX_was_not_set_at_build_time`
- linked-mode RTEMS rlib → `U POSIX_Init` (the `#[used]` static produced a
  genuine relocation, which is what pulls the shim archive in and survives
  `--gc-sections`)

`CONFIGURE_MAXIMUM_USER_EXTENSIONS 1` is present with a host test that fails if
it is removed, and a comment recording the exact boot failure it fixes
(`emerg: rtems_bsd_threads_init_early: cannot create extension`) and that base
reserves 5.

## 4. Unproven — what only the box can answer

The 17 host tests guard **structure** — entry-point agreement across
configuration / C / link, `confdefs` last, the user-extension reservation, no
dropped facility creeping back. They do not guard syntax, and they cannot link.

1. **Neither C file has been through a compiler.** →
   `cargo build -p epics-ca-rs --bin rtems-ca-ioc -Zbuild-std=std,panic_abort --target armv7-rtems-eabihf --release`
   with `RTEMS_BSP_PREFIX` set and `$RTEMS_BSP_PREFIX/bin` on `PATH`.
2. **The include path is a guess.** `bsp_include_dir` assumes
   `<prefix>/arm-rtems6/<bsp>/lib/include`. → take the real `-I` set from a BSP
   sample's *compile* line.
3. **`-lbsd -lm -lz` resolution.** Order is right by measurement, but rustc also
   emits `-Bdynamic` and RTEMS has no shared libraries. → does the link need
   `-static`, or reordering?
4. **The fd ceiling of 150.** Base's own score-arm value, and our three crates
   make no `select`/`poll` call — but libbsd's internals were not audited and
   RTEMS 6 may spell the macro `CONFIGURE_LIBIO_MAXIMUM_FILE_DESCRIPTORS`. →
   grep the toolchain headers for it and for `FD_SETSIZE`.
5. **`CONFIGURE_MAXIMUM_USER_EXTENSIONS 1` may be one too few** now that
   `CONFIGURE_STACK_CHECKER_ENABLED` is also on. → if the boot dies creating an
   extension, raise toward base's 5; it is `#ifndef`-overridable.
6. **Interworking is reasoned, not observed** (§2).
7. **`arm-rtems6-gcc` must be on `PATH` for non-interactive ssh.**
   `.cargo/config.toml` names it unqualified deliberately. → confirm a
   `BatchMode` shell sees it.
8. **Boot behaviour of the reduced `POSIX_Init`** — the DHCP timeout path,
   loopback bring-up, the stack-usage report on exit. → rungs 1–3 of
   `doc/rtems-runtime-acceptance-plan.md`; console markers are prefixed
   `rtems-boot:`.

`rtems-pva-ioc` does not exist yet, so `epics-pva-rs` was left untouched — its
measured 2-warning RTEMS budget stays usable as an instrument. When the stage-G
bin lands it needs exactly the two manifest lines and the one-line `build.rs`
that `epics-ca-rs` now has.
