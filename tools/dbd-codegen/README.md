# dbd-codegen

Generates every record type's `FieldDesc` table from the EPICS `.dbd`
declarations vendored into the crate that owns the type. The targets are
`src/targets.rs`'s list, base first, since every other target takes `dbCommon`
and the shared menus from base's:

| `.dbd` directory | generated into |
|---|---|
| `crates/epics-base-rs/dbd` | `src/server/record/dbd_generated.rs`, and the breakpoint tables into `bpt_generated.rs` beside it |
| `crates/motor-rs/dbd` | `src/record/dbd_generated.rs` |
| `crates/optics-rs/dbd` | `src/records/dbd_generated.rs` |
| `crates/scaler-rs/dbd` | `src/records/dbd_generated.rs` |
| `crates/std-rs/dbd` | `src/records/dbd_generated.rs` |
| `crates/sscan-rs/dbd` | `src/records/dbd_generated.rs` |
| `crates/mca-rs/dbd` | `src/record/dbd_generated.rs` |
| `crates/asyn-rs/dbd` | `src/dbd_generated.rs` (device menus only; asyn declares no record type) |

```sh
cargo run -p dbd-codegen -- --write    # regenerate the checked-in tables
cargo run -p dbd-codegen -- --check    # fail if one has drifted
```

The same comparison runs as the test `generated_files_are_not_stale`, so
`cargo nextest run --workspace` catches drift.

A crate outside this workspace that declares its own record types names its
`.dbd` directory and output file, relative to where the tool runs; its table
reaches base through `epics_rs::base`:

```sh
cargo run --manifest-path <epics-rs>/Cargo.toml -p dbd-codegen -- \
    --write --external <dbd_dir> <out_file>
```

The port used to hand-copy the `.dbd` into 1,174 `FieldDesc` literals. A wrong
`dbf_type` or a missed `special(SPC_NOMOD)` was then a *finding*, discovered by
eye, one audit round at a time. Deriving the table from the spec makes that
whole family unrepresentable.

## What is vendored, and why

`dbd/*.dbd` are the upstream declarations, copied verbatim with their EPICS Open
License headers intact. Base's directory holds:

* EPICS Base 7 (`aiRecord.dbd`, ..., `dbCommon.dbd`, `menu*.dbd`,
  `bptType*.dbd`)
* synApps `calc` (`aCalcoutRecord.dbd`, `sCalcoutRecord.dbd`, `sseqRecord.dbd`,
  `swaitRecord.dbd`, `transformRecord.dbd`)
* `busy` (`busyRecord.dbd`), `asyn` (`asynRecord.dbd`)
* `devices.dbd`, the upstream `device()` lines for those types gathered in C's
  load order, which is wire-visible through `DTYP`

Each downstream target holds its own module's: `motorRecord.dbd`,
`tableRecord.dbd`, `scalerRecord.dbd`, std's `epid`, `throttle` and
`timestamp`, sscan's `sscan` and `scanparm`, `mcaRecord.dbd`, and asyn's
`devEpics.dbd`.

They are vendored rather than read from an EPICS install so that neither the
build nor CI depends on a file outside the repository. The generator is offline
and its output is checked in; nothing in the build graph runs it.

## The two things the `.dbd` does not tell you

1. **The CA wire type.** The `.dbd` declares the *field* type. CA has no
   unsigned or 64-bit types, so an IOC promotes: `DBF_ULONG`/`DBF_INT64` are
   served as `DBR_DOUBLE`, `DBF_USHORT` as `DBR_LONG`, `DBF_UCHAR` as
   `DBR_CHAR`. The generator does **not** fold that in — `DbFieldType::ca_wire_type`
   already owns it, and PVA serves the native width. Emitting the promoted type
   here would double-promote.

2. **The type of a `special(SPC_DBADDR)` field.** C takes it from the record's
   `cvt_dbaddr()` at name-resolution time, so the `.dbd` carries only a
   `DBF_NOACCESS` placeholder — even for `waveform.VAL`. Those fields are
   resolved from the target's `dbd/cvt_dbaddr.types`, and the generator
   **refuses to build** if one is missing or if that file names a field that is
   not SPC_DBADDR. The exception is declared and closed, not silent.

Both rules are pinned against compiled C:
`crates/epics-base-rs/tests/fixtures/c_native_types.tsv` records the native type
the real `softIoc` serves for every CA-visible field of the 34 base record
types, and `generated_types_match_the_c_oracle`
(`crates/epics-base-rs/tests/dbd_generated_c_oracle.rs`) asserts the generated
tables reproduce it.

## Regenerating the C oracle fixture

Needs a built EPICS Base (not required for the normal build or for CI):

```sh
# 1. one record of every base type, arrays instantiated (see the fixture header)
softIoc -S -d all.db
# 2. cainfo every field the .dbd declares; record `Native data type`
```

A field whose row changes means the C IOC now serves it differently — that is a
finding, not a fixture to update blindly.
