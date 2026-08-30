# dbd-codegen

Generates `crates/epics-base-rs/src/server/record/dbd_generated.rs` — every
record type's `FieldDesc` table — from the EPICS `.dbd` record declarations
vendored in `crates/epics-base-rs/dbd/`.

```sh
cargo run -p dbd-codegen -- --write    # regenerate the checked-in table
cargo run -p dbd-codegen -- --check    # fail if it has drifted (CI)
```

The port used to hand-copy the `.dbd` into 1,174 `FieldDesc` literals. A wrong
`dbf_type` or a missed `special(SPC_NOMOD)` was then a *finding*, discovered by
eye, one audit round at a time. Deriving the table from the spec makes that
whole family unrepresentable.

## What is vendored, and why

`dbd/*.dbd` are the upstream declarations, copied verbatim with their EPICS Open
License headers intact:

* EPICS Base 7 (`aiRecord.dbd`, ..., `dbCommon.dbd`, `menu*.dbd`)
* synApps `calc` (`aCalcoutRecord.dbd`, `sCalcoutRecord.dbd`, `sseqRecord.dbd`,
  `swaitRecord.dbd`, `transformRecord.dbd`)
* `busy` (`busyRecord.dbd`), `asyn` (`asynRecord.dbd`)

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
   `DBF_NOACCESS` placeholder — even for `waveform.VAL`. Those 82 fields are
   resolved from `dbd/cvt_dbaddr.types`, and the generator **refuses to build**
   if one is missing or if that file names a field that is not SPC_DBADDR. The
   exception is declared and closed, not silent.

Both rules are pinned against compiled C: `tests/fixtures/c_native_types.tsv`
records the native type the real `softIoc` serves for all 2,558 CA-visible
fields of the 34 base record types, and `dbd_generated_matches_c_oracle` asserts
the generated tables reproduce it.

## Regenerating the C oracle fixture

Needs a built EPICS Base (not required for the normal build or for CI):

```sh
# 1. one record of every base type, arrays instantiated (see the fixture header)
softIoc -S -d all.db
# 2. cainfo every field the .dbd declares; record `Native data type`
```

A field whose row changes means the C IOC now serves it differently — that is a
finding, not a fixture to update blindly.
