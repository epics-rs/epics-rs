# named-port

Test-only. One rule for every test that has to **name** the port its subject
binds, and a retry for the race that rule exposes.

Never published (`publish = false`); every consumer takes it as a `path`-only
dev-dependency, which `cargo package` drops from the uploaded manifest.

## Why a test would name a port at all

Most tests do not have to. The process that binds asks the kernel for `0` and
reports back what it got, so no number was ever guessed — `epics-oracle-rs`'s
Rust side and the workspace's in-process servers work that way, and they need
nothing from this crate.

A few cannot, and they are the callers here: either the property under test
*is* that a named number is honoured (an st.cmd line, an
`EPICS_CAS_SERVER_PORT` boot assignment), or the child is C's `softIoc`, which
takes its port from the environment and says nothing about what it bound.

For those a candidate port is a hint and never a reservation. The probe that
found it has closed its socket by the time the subject binds, so in between the
number belongs to whoever asks the kernel next — measured as a 0.055 s failure
of a boot test while a second full-workspace run was on the same box.

## The rule

> A test that must name a port treats "the subject did not get the port it was
> told to use" as a retry with a fresh candidate, never as a failure.

The assertion is untouched: the boot line still has to be honoured, the st.cmd
line still has to bind. What changes is the verdict on losing a race the test
never meant to enter.

```rust
let server = named_port::on_a_named_port(|port| {
    // start the subject on `port`; return None ONLY on evidence that
    // this number was taken by somebody else
    boot(port).ok()
});
```

`free_for_tcp_and_udp()` is the candidate source, and probes both because a CA
server binds its TCP listener and its UDP search socket on the same number, so
probing one leaves the other free to collide. `ATTEMPTS` is 16: a steal is a
coincidence, sixteen in a row is a host where something is systematically
taking the numbers, and the panic prints every candidate it burned.

## The dangerous half

A retry is only safe on evidence specific to *somebody else owns this number*.
Retrying on a general failure — "the client could not connect", "no value came
back" — converts a real regression into 16 slow attempts and a confusing
panic, which is worse than the flake it replaces. So the closure returns `None`
only on evidence that names the port, and each caller names its own:

- the asyn port object is absent after `drvAsynIPServerPortConfigure`, which
  the handler unregisters *only* on a failed bind;
- `realtime-ca-ioc` prints `cannot start the CA TCP server on port <p>:
  Address already in use` and exits 1;
- C `softIoc` prints `cas WARNING: Configured TCP port was unavailable.` and
  then `CAS: No TCP server started` — measured against R7.0.10, where
  `rsrv_init` reaches `cantProceed` and the process *suspends* rather than
  exiting, so "the child is gone" is not the discriminator.

Anything else the subject does is a failure and must reach the test as one.

## Consumers

`epics-ca-rs` (`softioc_spawn_verdict`, `realtime_ca_ioc_boots`, and the shared
`tests/common`) and `asyn-rs` (`ip_server_port_accepts`).

## Testing

```bash
cargo nextest run -p named-port
```

Three tests, one per boundary of the search: a win ends it, every attempt gets
its own candidate, and the cap is reached loudly.

## License

[EPICS Open License](LICENSE)
