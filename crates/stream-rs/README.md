# epics-stream-rs

Rust port of PSI [StreamDevice](https://github.com/paulscherrerinstitute/StreamDevice)
2.8.26 (at 562dd5a). An IOC built on it loads the same `.proto` files and
`DTYP "stream"` records as a C StreamDevice IOC.

> The crate is published on crates.io as `epics-stream-rs` (the bare
> `stream-rs` name was already taken by an unrelated project), but the
> Rust library name is `stream_rs`, so consumers write
> `use stream_rs::...`.

Every `.c`/`.cc` source of `StreamDevice/src` is ported, one module each; the tables
are in `src/lib.rs` and `src/dev/mod.rs`:

- protocol parser and engine, the `stream` driver and `STREAM_PROTOCOL_PATH`
  (`StreamProtocol`, `StreamCore`, `StreamBuffer`, `StreamError`,
  `StreamEpics`);
- the `asyn` (over `asyn-rs`), `debug` and `dummy` bus interfaces;
- every converter: the standard ones, Enum, BCD, Raw, RawFloat, Binary,
  Checksum, MantissaExponent, Timestamp and Regexp;
- device support for ai, ao, bi, bo, mbbi, mbbo, mbbiDirect, mbboDirect,
  longin, longout, int64in, int64out, stringin, stringout, lsi, lso,
  waveform, aai, aao, calcout and scalcout;
- the iocsh commands `streamReload`, `streamReportRecord`,
  `streamSetLogfile` and `streamReinit`.

`%/regexp/` runs on `stream_rs::pcre`, PCRE 8.45 ported to Rust, so no C
library is linked. The vendored C PCRE in
[`stream-pcre-sys`](../stream-pcre-sys) is only a dev-dependency the tests
compare the port against.

## Use

```toml
[dependencies]
epics-stream-rs = "0.33"
```

`dev::register_app` brings what C's `streamApp.dbd` does: asyn device
support and its iocsh commands (`drvAsynIPPortConfigure`, ...), the
`DTYP "stream"` rows, the bus interfaces and the stream commands.

```rust
use epics_base_rs::error::CaResult;
use epics_base_rs::server::ioc_app::IocApplication;

#[epics_base_rs::epics_main]
async fn main() -> CaResult<()> {
    let app = stream_rs::dev::register_app(IocApplication::new());
    epics_ca_rs::server::run_ca_ioc_app(app.startup_script("st.cmd")).await
}
```

```
epicsEnvSet("STREAM_PROTOCOL_PATH", "db")
drvAsynIPPortConfigure("L0", "10.6.28.17:23", 0, 0, 0)
dbLoadRecords("db/xxCapaNCDT6200.template", "dev=MMS:S27:MMD1,Link=L0")
```

```
record(mbbi, "$(dev):sampleTimeM") {
  field(DTYP, "stream")
  field(INP, "@capaNCDT6200.proto querySampleTime($(dev)) $(Link) 0")
}
```

## Feature flags

None. Every bus interface, converter and record type is always built. C
drops Regexp from `CONFIG_STREAM` when no PCRE is configured and scalcout
when calc is absent; here both are always present.

## Differences from C

- `epics::STREAM_VERSION` is `StreamDevice 2.8.26` with no commit date or hash.
- On Windows, aao and waveform print an int64 element as 64 bits; C's LLP64
  `long` truncates it.
