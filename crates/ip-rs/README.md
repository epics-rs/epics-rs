# ip-rs

Rust port of the synApps [ip](https://github.com/epics-modules/ip) module's
SNL programs and of `devXxStrParm.c`, the generic octet device support their
templates use. Each `src/snl/*.st` is the upstream file with its C defects
fixed in place, each under a comment; `build.rs` compiles it with
epics-snc-rs and it runs on epics-seq-rs.

| Program | Rust module | Template |
|---|---|---|
| Federal ERO-1071 encoder reader | `snl::federal` | `db/Federal.db` |
| Keithley 2000/2700 DMM, 22-channel scanner | `snl::keithley2k_dmm` | `db/Keithley2kDMM_mf.db` |
| Keithley 2000/2700 DMM, 40-channel scanner | `snl::keithley2k_dmm_mf40` | `db/Keithley2kDMM_mf40.db` |
| Keithley 6517A electrometer, 10-channel scanner | `snl::keithley65xx_em` | `db/Keithley65xxEM.db` |
| Oxford Cryostream 800 controller | `snl::oxford_cs800` | `db/Oxford_CS800.db`, `db/Oxford_CS800_status.db` |

`db/` is vendored from `ipApp/Db` unchanged, so an `st.cmd` that loads
`$(IP)/db/Federal.db` resolves when `IP` is set to [`IP_DIR`](src/lib.rs).

## Usage

[`register_ip_dbd`](src/lib.rs) does what `ipSupport.dbd`'s `registrar()`
lines do: it registers each program and the shell's `seq` command, so the
upstream `st.cmd` lines run as in C:

```
drvAsynIPPortConfigure("serial1", "<host>:<port>", 0, 0, 0)
asynOctetSetOutputEos("serial1", 0, "\x02")
dbLoadRecords("$(IP)/db/Federal.db", "P=xxx:,S=s1,PORT=serial1")
iocInit
seq &Federal, "P=xxx:,S=s1"
```

The templates' records come from base, asyn (`asyn`) and calc
(`scalcout`); the IOC registers those record types itself.
`Oxford_CS800.db`'s command records are StreamDevice's (`stream`, from
epics-stream-rs), reading `db/Oxford_CS800.proto` through
`STREAM_PROTOCOL_PATH`.

`devXxStrParm` (`asyn <xx> stringParm`) is [`strparm`](src/strparm.rs): its
`device()` lines of `ipSupport.dbd` are `strparm::DEVICE_ROWS`, served by
`strparm::device_support_factory()` through
`register_dynamic_device_support`.

No cargo features.

## Not ported

- The other device supports in `ipApp/src/dev*.c`; the serial ones are in
  epics-rs-iocs's `ip-devices`.
- `ipApp/op` display screens and the `iocs/ipExample` IOC.

## Testing

```bash
cargo nextest run -p ip-rs
```

Each program runs on its template against a device mock over a
`drvAsynIPPort`; the expected values are the ones the C IOC posted against
the same mock and template.

## License

The synApps software license: [LICENSE](LICENSE).
