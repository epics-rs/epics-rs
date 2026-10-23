# epics-seq-rs

Rust port of the EPICS sequencer R2-2-9 run-time (`src/seq`, `src/pv`) that
programs compiled by `epics-snc-rs` run on. As in C, each state set runs on
its own OS thread and blocks where C's does (a `SYNC` get or put, `delay`,
the connect wait); generated code is plain synchronous Rust. pvs go through
the IOC's CA client context: a name the IOC serves gets an in-process
channel, any other goes out on the network client `epics-ca-rs` registers.

## Use

`build.rs` compiles `src/snl/*.st` with `epics_snc_rs::build::compile_dir`
(see the `epics-snc-rs` README). A hand-written companion module includes
each program and supplies what its escaped C supplied in C. std-rs's
`src/snl/femto.rs`:

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

There are no cargo features.

## Programs that run on it

- std-rs: `delayDo.st`, `femto.st`
- optics-rs: `filterDrive.st`, `flexCombinedMotion.st`, `hrCtl.st`, `Io.st`,
  `kohzuCtl.st`, `kohzuCtl_soft.st`, `ml_monoCtl.st`, `orient_st.st`,
  `pf4.st`, `sncqxbpm.st`, `xiahsc.st`, `xia_slit.st`
- sscan-rs: `scanProg.st`
- usb-ctr (epics-rs-iocs): `USBCTR_SNL.st`

## Not covered

- SNL constructs snc-rs refuses (struct definitions and variables,
  assignment through a pointer, and others): see the `epics-snc-rs` README.
- The sequencer's own `test/validate` programs are type-checked by
  `epics-snc-rs`'s reference test but not run on this crate.
