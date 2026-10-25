# epics-snc-rs

Rust port of `snc`, the SNL compiler of the EPICS sequencer R2-2-9
(`src/snc`), together with the C preprocessor the EPICS build runs before it
(ported from GCC 13's cpplib). It turns an unchanged upstream `.st` program
into Rust at build time. The generated code runs on `epics-seq-rs`.

## Use from build.rs

```toml
[dependencies]
epics-seq-rs = "0.32"

[build-dependencies]
epics-snc-rs = "0.32"
```

```rust
// build.rs
fn main() {
    epics_snc_rs::build::compile_dir("src/snl");
}
```

`compile_dir` runs cpp and snc on every `*.st` directly in the directory, in
file-name order, and writes `$OUT_DIR/<stem>.rs`. Headers a program includes
sit beside it (std-rs keeps `seqPVmacros.h` in `src/snl`). Cargo reruns the
script when the directory, a program or an included file changes.
`compile` takes one file; a `.stt` skips cpp, as in the EPICS build.

Programs compile with snc's default options, and `option` lines inside a
program apply as in C. A program snc refuses fails the build with snc's
diagnostics; warnings of one that compiles become cargo warnings. A companion
module includes the output with `epics_seq_rs::include_program!("<stem>")`
(see the `epics-seq-rs` README).

`snc-rs <options> <infile>` is C snc's command line. It reads an already
preprocessed program and writes Rust, to `<infile>` with a `.rs` extension
unless `-o` is given.

## Programs built with it

The 16 programs v0.32.0 moved onto the sequencer (`IN_SCOPE` in
`tests/reference_gen_code.rs`):

- std-rs: `delayDo.st`, `femto.st`
- optics-rs: `filterDrive.st`, `flexCombinedMotion.st`, `hrCtl.st`, `Io.st`,
  `kohzuCtl.st`, `kohzuCtl_soft.st`, `ml_monoCtl.st`, `orient_st.st`,
  `pf4.st`, `sncqxbpm.st`, `xiahsc.st`, `xia_slit.st`
- sscan-rs: `scanProg.st`
- usb-ctr (epics-rs-iocs): `USBCTR_SNL.st`

## Not supported

snc-rs stops with an error naming the line where C snc would emit C that has
no Rust form yet: a non-`char` pointer, pointer arithmetic or assignment
through a pointer, a `char *` into a variable, a struct definition, a
struct, enum or union variable, a `typename` variable, a member of a seq
run-time value, and an assignment to a foreign variable. Each needs a
run-time type `epics-seq-rs` does not have yet. An array or `string`
parameter of a function definition, which C passes as a pointer, is refused
the same way.

`EXPECTED_FAILURES` in `tests/reference_gen_code.rs` pins every upstream
program that does not build, with its first error; every other program
type-checks or is rejected by analysis as C snc rejects it. Besides the
refusals, sequencer `examples/parallel/control.st` calls
`pvPutCompleteArray`, which is defined nowhere, so C cannot link it either.

There are no cargo features.

## Tests

The `reference_*` tests compare against C snc R2-2-9 and need the synApps
module collection with a built `sequencer` (`EPICS_MODULES`, or an
`epics-modules` directory beside the checkout); without it they fail rather
than skip. `reference_gen_code` generates the sequencer's 96 programs and 30
module programs, holds `delayDo.st` to `epics-seq-rs`'s
`tests/golden/delayDo.rs` byte for byte, and type-checks the output against
`epics-seq-rs`.
