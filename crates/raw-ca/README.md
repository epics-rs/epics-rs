# raw-ca

Test-only, never published: a raw-socket Channel Access client for
`epics-ca-rs`'s blocking front-end suites (`BlockingCaServer`). It writes CA
frames by hand over `std::net::TcpStream`, with no `CaClient`, no tokio runtime
and no `.await`, so a test can drive the server on a bare thread under either
execution backend. It ports no C module.

`epics-ca-rs` takes it as a `path`-only dev-dependency, which `cargo package`
drops from the uploaded manifest.

## Usage

From `epics-ca-rs/tests/blocking_raw_client_e2e.rs`:

```rust
use epics_ca_rs::protocol::CA_PROTO_READ_NOTIFY;
use raw_ca::*;

let (server, addr, accept) = start_server(db); // ephemeral loopback port
let mut c = connect_and_handshake(addr);
let sid = create_channel(&mut c, 0x1234, "RAW:E2E");

c.send(&read_notify_frame(sid, 0x01));
let r = c.expect(CA_PROTO_READ_NOTIFY, "READ_NOTIFY");
assert_eq!(payload_double(&r), 1.5);
```

`Circuit` never discards a frame: every read checks the frames that have
arrived but not been claimed, so a write's WRITE_NOTIFY reply and its monitor
update can arrive in either order. `Circuit::expect_absent_before` asserts
absence against a later barrier reply, not a timeout, and every reader fails
the test on a `CA_PROTO_ERROR` frame.

## Not covered

Scalars only: the frame builders encode a one-element `DBR_DOUBLE`. There is
no UDP search client.

## License

[EPICS Open License](../../LICENSE)
