# epics-seq-rs

Rust port of the EPICS sequencer R2-2-9: the run-time (`src/seq`, `src/pv`)
and `snc`, the SNL compiler (`src/snc`), together with the C preprocessor
the EPICS build runs before it (ported from GCC 13's cpplib). The compiler
turns an unchanged upstream `.st` program into Rust at build time, and the
generated code runs on the run-time. As in C, each state set runs on its own
OS thread and blocks where C's does (a `SYNC` get or put, `delay`, the
connect wait); generated code is plain synchronous Rust. pvs go through the
IOC's CA client context: a name the IOC serves gets an in-process channel,
any other goes out on the network client `epics-ca-rs` registers.

## Features

- `runtime` (default): the run-time, on `epics-base-rs`.
- `snc`: the compiler, `epics_seq_rs::snc`, and the `snc-rs` binary. A
  build-dependency with `default-features = false` compiles the compiler
  and not `epics-base-rs`.

## Use

```toml
[dependencies]
epics-seq-rs = "0.33"

[build-dependencies]
epics-seq-rs = { version = "0.33", default-features = false, features = ["snc"] }
```

```rust
// build.rs
fn main() {
    epics_seq_rs::snc::build::compile_dir("src/snl");
}
```

`compile_dir` runs cpp and snc on every `*.st` directly in the directory, in
file-name order, and writes `$OUT_DIR/<stem>.rs`. Headers a program includes
sit beside it (std-rs keeps `seqPVmacros.h` in `src/snl`). Cargo reruns the
script when the directory, a program or an included file changes.
`compile` takes one file; a `.stt` skips cpp, as in the EPICS build.

Programs compile with snc's default options, and `option` lines inside a
program apply as in C. A program snc refuses fails the build with snc's
diagnostics; warnings of one that compiles become cargo warnings.

`snc-rs <options> <infile>` is C snc's command line. It reads an already
preprocessed program and writes Rust, to `<infile>` with a `.rs` extension
unless `-o` is given.

A hand-written companion module includes each program and supplies what its
escaped C supplied in C. std-rs's `src/snl/femto.rs`:

```rust
use epics_seq_rs::c::*;

macro_rules! seqg_escaped {
    (r#"#include <stdio.h>"#) => {};
    (r#"#include <math.h>"#) => {};
    (r#"static int powers[] = {5,6,7,8,9,10,11,0,3,4,5,6,7,8,9,0};"#) => {};
}

#[allow(non_upper_case_globals)] // the C name, which the generated code uses
static powers: [i32; 16] = [5, 6, 7, 8, 9, 10, 11, 0, 3, 4, 5, 6, 7, 8, 9, 0];

epics_seq_rs::include_program!("femto");
```

`seqg_escaped!` has one arm per escaped-C fragment, matched by its exact
text, and must be defined before `include_program!`. A fragment with no arm
fails the build, so a changed upstream `.st` cannot compile against a stale
companion. A program with no escaped C needs only the `use` and the
`include_program!` (std-rs `src/snl/delay_do.rs`).

The included code exports the program and its registrar (`femto`,
`femtoRegistrar`). `epics_seq_rs::seq(&femto, "P=xxx:", 0, &db)` starts an
instance. Once the registrar has run, the `seq` shell command starts it by
name; `seq_register_sequencer_commands`, which every registrar calls, adds
`seq`, `seqShow`, `seqChanShow`, `seqQueueShow`, `seqStop` and `seqcar`.

The crate docs give the rules the generated code follows (names, variable
types, the `+r`/`+s` variable models, escaped-C binding, lock order).
`tests/golden/delayDo.rs` is snc-rs's output for std's `delayDo.st`.

## Programs built and run with it

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
run-time type this crate does not have yet. An array or `string` parameter
of a function definition, which C passes as a pointer, is refused the same
way.

`EXPECTED_FAILURES` in `tests/reference_gen_code.rs` pins every upstream
program that does not build, with its first error; every other program
type-checks or is rejected by analysis as C snc rejects it. Besides the
refusals, sequencer `examples/parallel/control.st` calls
`pvPutCompleteArray`, which is defined nowhere, so C cannot link it either.

## Tests

The compiler's tests need `--features snc`. The `reference_*` tests compare
against C snc R2-2-9 and need the synApps module collection with a built
`sequencer` (`EPICS_MODULES`, or an `epics-modules` directory beside the
checkout); without it they fail rather than skip. `reference_gen_code`
generates the sequencer's 96 programs and 30 module programs, holds
`delayDo.st` to `tests/golden/delayDo.rs` byte for byte, and type-checks the
output against the run-time. The sequencer's own `test/validate` programs
run in `seq-validate`.
