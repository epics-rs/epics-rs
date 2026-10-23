# sscan-rs

Rust port of the synApps [sscan](https://github.com/epics-modules/sscan)
module, release R2-12: the `sscan` and `scanparm` record types, `saveData`
and the `scanProgress` sequence.

| C source | Rust |
|---|---|
| `sscanRecord.c` | `records::sscan` |
| `scanparmRecord.c` | `records::scanparm` |
| `saveData_writeXDR.c` | `save_data::task`, `save_data::mda` |
| `writeXDR.c` | `save_data::write_xdr` |
| `req_file.c` | `save_data::req_file` |
| `scanProg.st` | `scan_prog`, from `src/snl/scanProg.st` |
| `recDynLink.c` | `epics_base_rs::server::rec_dyn_link`, re-exported as `sscan_rs::rec_dyn_link` |

`recDynLink` lives in base because the swait record runs on it too.
`src/snl/scanProg.st` is the upstream file with its C defects fixed in
place, each under a comment; `build.rs` compiles it with epics-snc-rs and it
runs on epics-seq-rs.

`db/` and the `.dbd` files in `dbd/` are vendored from `sscanApp/Db` and
`sscanApp/src` unchanged, so an `st.cmd` that loads
`$(SSCAN)/db/standardScans.db` resolves when `SSCAN` is set to
[`SSCAN_DIR`](src/lib.rs).

## Usage

Register the record types; either factory also loads sscan's `.dbd` and
registers saveData's shell commands ([`register_sscan_dbd`](src/lib.rs)).
From `examples/sscan_ab_ioc.rs`:

```rust
let (name, factory) = sscan_rs::sscan_record_factory();
let ioc = IocBuilder::new()
    .register_record_type(name, factory)
    .db_file(&db_file, &HashMap::new())?
    .build_ioc()
    .await?;
```

`scanparm_record_factory()` registers the scanparm record the same way.
In `st.cmd`, as upstream's `sscan.iocsh` does:

```
dbLoadRecords("$(SSCAN)/db/standardScans.db", "P=xxx:,MAXPTS1=1000,MAXPTS2=1000,MAXPTS3=1000,MAXPTS4=1000,MAXPTSH=1000")
dbLoadRecords("$(SSCAN)/db/saveData.db", "P=xxx:")
iocInit
saveData_Init("saveData.req", "P=xxx:")
```

`scanProgress` starts like any compiled program (`tests/scan_progress.rs`):

```rust
epics_seq_rs::seq(&sscan_rs::scan_prog::scanProgress, "S=xxx:,P=xxx:scanProgress:", 0, &db);
```

`sscan_rs::scan_prog::scanProgressRegistrar()` instead registers it for the
shell's `seq` command, as C's `registrar(scanProgressRegistrar)`.

No cargo features.

## Not ported

- `sscanApp/op` display screens and `sscanApp/iocsh/sscan.iocsh`.
- The MDA reading utilities (`mdautils-src.tar.gz`).
- `saveData.c` and the `xdr_*.c` files, which sscan's own Makefile no
  longer builds.

## Testing

```bash
cargo nextest run -p sscan-rs
```

The MDA and XDR writers are compared byte for byte against files written
by the C writers (`tests/fixtures/save_data`). `examples/sscan_ab` drives
this crate and a C IOC with the same `.db` and CA client for an A/B of the
record's monitor streams.

## License

The synApps software license: [LICENSE](LICENSE).
