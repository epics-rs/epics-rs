# mca-rs

Rust port of the synApps [mca](https://github.com/epics-modules/mca) record — a
multichannel analyzer spectrum, its 32 regions of interest, and its acquisition
presets. Ported from `mca/mcaApp/mcaSrc` at upstream `687d563`; `doc/reference-pin.md`
is where an auditor checks the crate's C citations.

No C dependencies. `cargo build -p mca-rs`.

## The `.dbd` is the declaration

The field table is not written in Rust. `dbd/mcaRecord.dbd` is vendored
byte-identical from upstream, and `tools/dbd-codegen` generates
`src/record/dbd_generated.rs` from it on exactly the terms every base record
type's table is generated, held current by the same drift gate
(`dbd_codegen::tests::generated_files_are_not_stale`). 320 fields, six menus
(`mcaSTRT`, `mcaREAD`, `mcaERAS`, `mcaMODE`, `mcaCHAS`, `mcaR0IP`), one
declaration — `one_declaration_per_record_type` is the test that says so.

`VAL` (the spectrum) and `BG` (the ROI background curve) are `DBF_NOACCESS` in
the `.dbd`, because there the field is C's `void *`. Both are allocated `NMAX`
deep in the `FTVL` element width and re-typed from `FTVL` on every name
resolution, so `dbd/cvt_dbaddr.types` records the served type and the
`SPC_DBADDR` that the `.dbd` cannot carry.

## Modules

| module | C counterpart | what it is |
|---|---|---|
| `record` | `mcaRecord.c` | the record type: state, field get/put, `VERS = 6.01`, the `NMAX`/`NUSE` clamp |
| `record::cycle` | `mcaRecord.c::process` (`:491-845`) | the acquisition cycle, split at the two points where C's control flow crosses into the driver |
| `record::roi` | `sum_ROIs` + the `PROCESS_ROI` macro | the region sums, net counts and preset-reached edge |
| `record::dbd_generated` | `mcaRecord.dbd` | the generated field table |
| `soft` | `devMCA_soft.c` | the `"Soft Channel"` device support |

## Three places the port is structurally different

Each one is a C shape that cannot be reproduced without reproducing its defect:

- **An ROI is one value.** C splits each region across `struct roi` (the four
  fields a client sets) and `struct roiSum` (the three the record computes),
  each reached by casting the first `.dbd` field and walking — two layouts that
  must match the `.dbd` order exactly, where one mis-ordered field silently
  mis-reads every region. Here a region is one `Roi`, and the `.dbd` order is
  the generated table's business alone.
- **Every element type computes its sums.** C writes the ROI arithmetic once
  per element type by macro, dispatched on `FTVL`, and the `DBF_CHAR`/`DBF_UCHAR`
  arm is a bare `break` — a char spectrum gets no region sums at all, silently.
  Here the arithmetic is written once in doubles and every element type reaches
  it.
- **The cycle is the record's, the device calls are the driver's.** C calls
  `pdset->send_msg()` from the middle of `process()`. A port record cannot call
  its device support, so the cycle is split in two: the record applies its own
  state and hands over what it wants sent (`take_device_requests`), the driver
  sends it and reads status and spectrum back, and the record lands the result.
  C's order is load-bearing and is kept — `ACQG` is forced to 1 when the start
  command goes out, so a device that finishes before the first status read still
  shows the 1 → 0 transition that triggers the spectrum read.

Simulation mode is C's, including its order: `mca` raises `SIMM_ALARM` *after*
the `SIOL` read, unlike every base record, so a broken `SIOL` under
`SIMS = INVALID` publishes `LINK_ALARM`/`AMSG = "field SIOL"` and not
`SIMM_ALARM`. `simm_alarm_is_raised_after_the_siol_read` pins that.

## Soft device support

`device(mca, CONSTANT, devMCA_soft, "Soft Channel")`. There is no hardware
behind it: every command is a no-op, the status struct is never written, and
`read_array` only moves `NORD`. So a soft mca never acquires — the spectrum it
serves is whatever a `.db` or a client put into `VAL`, which is the point: it is
the ROI machinery over a spectrum you supply.

One Tier-3 deviation (a driver's tier is correctness, not parity): C's zeroed
status carries `dwellTime = 0` and the record copies it, so the first process of
a soft mca destroys `DWEL` — measured on the C IOC built for this port, `DWEL`
reads 1 at iocInit and 0 after a single `caput PROC 1` on a record whose `.db`
never mentions it. A device that cannot correct the dwell time reports the
dwell it was given.

## Usage

```rust
use epics_base_rs::server::ioc_app::IocApplication;
use epics_ca_rs::server::run_ca_ioc_app;

#[epics_base_rs::epics_main]
async fn main() -> epics_base_rs::error::CaResult<()> {
    let (name, factory) = mca_rs::mca_record_factory();
    // The database loads through the shell, as in C: `db_file` is
    // `IocBuilder`'s, and an `IocApplication` has no second loader.
    run_ca_ioc_app(
        IocApplication::new()
            .register_record_type(name, factory)
            .startup_line(r#"dbLoadRecords("my-mca.db")"#),
    )
    .await
}
```

`mca_rs::MCA_DBD_DIR` is the bundled `.dbd` directory, for a tool that needs the
spec. `register_mca_record_type()` is the legacy global-registry path; prefer
the factory.

```
record(mca, "MCA1") {
    field(DTYP, "Soft Channel")
    field(NMAX, "2048")
    field(FTVL, "LONG")
    field(R0LO, "100")
    field(R0HI, "200")
}
```

## Testing

```bash
cargo nextest run -p mca-rs
```

63 tests. The invariants each file pins are in its name: the buffer width is the
capacity, the served spectrum length, `NORD` on a background write, `NMAX` as
the channel capacity a client sees before acquisition, the `rset` metadata
reaching the record, the simulation order and its menu carrier, and one
declaration per record type.

## Requirements

- Rust 1.94.0 (`rust-toolchain.toml`), edition 2024

## License

[EPICS Open License](LICENSE)
