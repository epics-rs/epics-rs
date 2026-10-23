# c-ca-tools

Test-only, never published: helpers that run the EPICS C reference tools
(`softIoc`, `caget`, `caput`) against `epics-ca-rs` in its interop and soak
tests. It ports no C module; it drives the installed EPICS base binaries, which
must be on `PATH`. A missing tool fails the test, never skips it.

`epics-ca-rs` takes it as a `path`-only dev-dependency, which `cargo package`
drops from the uploaded manifest.

## Usage

From `epics-ca-rs/tests/interop_rust_client_c_ioc.rs`:

```rust
use c_ca_tools::{require_tool, spawn_softioc};

require_tool("softIoc");
let ioc = spawn_softioc(TEST_DB); // .db text; killed when `ioc` drops
// point the client at it: ioc.ca_addr_list(), ioc.udp_port
```

- `spawn_softioc`, `spawn_softioc_with_acf` (`softIoc -a`) and
  `spawn_softioc_on` (one fixed port) start `softIoc -S` bound to loopback and
  return once it reports it is serving. The first two retry on a fresh port
  when another process took the candidate (`named-port`); every other failure
  panics with the IOC's console.
- `run_caget` / `run_caput` run one `caget -w 3` / `caput -w 3`.
- `LineCollector` waits on a child's output line instead of a fixed sleep.

The `interop_*`, `differential_libca` and `stress_load` suites are filtered
out of the default nextest profile; run them with
`cargo nextest run --profile interop`.

## Not covered

No `camonitor`, `cainfo` or `caRepeater` wrapper; a test that needs one spawns
it itself.

## License

[EPICS Open License](../../LICENSE)
