# epics-stream-rs

Rust port of PSI [StreamDevice](https://github.com/paulscherrerinstitute/StreamDevice)
(at 562dd5a): `StreamBuffer`, `StreamError`, the `.proto` parser of
`StreamProtocol`, and the trait boundaries of `StreamBusInterface`,
`StreamFormatConverter` and `StreamCore` that the bus, converter and record
ports plug into.

> The crate is published on crates.io as `epics-stream-rs` (the bare
> `stream-rs` name was already taken by an unrelated project), but the
> Rust library name is `stream_rs`, so consumers write
> `use stream_rs::...`.
