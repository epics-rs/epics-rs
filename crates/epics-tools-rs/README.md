# epics-tools-rs

Operational tooling for EPICS deployments — process supervisors, log/audit
dumpers, config validators.

First tenant: **`procserv`** — a pure-Rust port of
[`epics-modules/procServ`](https://github.com/epics-modules/procServ).
PTY-based process supervisor with a multi-client telnet console. Drop-in
flag compatibility with C `procServ`, so existing wrapper scripts and
systemd units do not need to be rewritten.

Unix-only (depends on `forkpty(3)` and POSIX signals). On non-Unix
targets the crate compiles to an empty library so workspace builds keep
succeeding; the `procserv-rs` binary is unavailable there.

## Build

From the workspace root:

```bash
cargo build -p epics-tools-rs --release --bin procserv-rs
# binary: target/release/procserv-rs
```

The default features (`procserv`, `procserv-bin`) already pull in the
binary; no extra `--features` flag is needed. To use the library only
without the CLI dependencies (`clap`, `tracing-subscriber`):

```toml
[dependencies]
epics-tools-rs = { version = "*", default-features = false, features = ["procserv"] }
```

## Run

`procserv-rs` mirrors the C `procServ` flag surface. Common shapes:

```bash
# Foreground, port 4051, supervising a softIoc
procserv-rs -f -P 4051 -- softIoc -d my.db

# Daemonized with log/pid/info files
procserv-rs -P 4051 \
    --logfile /var/log/ioc.log \
    --pidfile /var/run/ioc.pid \
    --info-file /var/run/ioc.info \
    --name myioc \
    -- softIoc -d my.db

# Bind to all interfaces (default is localhost only)
procserv-rs -f -P 4051 --allow -- softIoc -d my.db

# UNIX domain socket instead of TCP
procserv-rs -f --unixpath /tmp/ioc.sock -- softIoc -d my.db
```

Everything after `--` is the child program and its argv.

### Flags

| Flag | Description | Default |
|---|---|---|
| `-P, --port <PORT>` | TCP control port (note: uppercase `-P`) | — |
| `--allow` | Bind the control port to 0.0.0.0 instead of 127.0.0.1 | off |
| `-l, --logport <PORT>` | Read-only viewer/log TCP port | — |
| `--restrict` | Restrict the log port to localhost | off |
| `--unixpath <PATH>` | UNIX-domain socket path | — |
| `-f, --foreground` | Do not daemonize | off |
| `-d, --debug` | Keep child in foreground + debug-level logging (also `PROCSERV_DEBUG` env) | off |
| `-q, --quiet` | Suppress the no-log-file warning when daemonizing | off |
| `-L, --logfile <PATH>` | Log file (`-` logs to stdout) | — |
| `-S, --logstamp[=FMT]` | Timestamp each log line (FMT attached with `=`) | off |
| `-F, --timefmt <FMT>` | strftime for banner / start-time lines | `%c` |
| `-p, --pidfile <PATH>` | PID file (note: lowercase `-p`) | — |
| `--info-file <PATH>` | `PROCSERV_INFO` info file | — |
| `--holdoff <SEC>` | Hold-off between restarts | 15 |
| `-w, --wait` | Do not start child until first console request | off |
| `-N, --noautorestart` | Start with auto-restart disabled (server stays up) | off |
| `-o, --oneshot` | Exit the supervisor when the child exits | off |
| `--chdir <DIR>` | `chdir` before exec'ing child | — |
| `--name <NAME>` | Display name in banners | command |
| `-e, --exec <PATH>` | Executable to run instead of the command | command |
| `--max-restarts <N>` | Max restarts inside `--restart-window` | unlimited |
| `--restart-window <SEC>` | Sliding window for `--max-restarts` | 600 |
| `-C, --coresize <BYTES>` | Child `RLIMIT_CORE` soft limit | inherit |
| `-K, --killsig <SIGNUM>` | Signal for the kill key / shutdown teardown | 9 (SIGKILL) |
| `-i, --ignore <CHARS>` | Extra bytes to strip from child stdin (caret notation) | — |
| `-k, --killcmd <CHAR>` | Kill key, caret notation (`^X`; empty disables) | `^X` |
| `-T, --autorestartcmd <CHAR>` | Toggle restart-mode key (caret notation) | `^T` |
| `-x, --logoutcmd <CHAR>` | Per-client logout key (caret notation; disabled by default) | none |

### Connecting to the console

```bash
telnet localhost 4051
# or, for UNIX-socket listeners:
nc -U /tmp/ioc.sock
```

Multiple clients may connect simultaneously and share the child's stdout
in a party-line fashion. Built-in keys:

- `Ctrl-X` — force-kill the child (sends `SIGKILL` by default).
- `Ctrl-T` — toggle restart mode (`OnExit` ↔ off).
- `Ctrl-R` — manually restart the child when it is dead.
- `Ctrl-]` — log this client out (child stays running).

### Logging

`procserv-rs` uses the `tracing` ecosystem with an `EnvFilter`:

```bash
RUST_LOG=debug procserv-rs -f -P 4051 -- softIoc -d my.db
RUST_LOG=epics_tools_rs::procserv=trace procserv-rs ...
```

## Use as a library

```rust
use epics_tools_rs::procserv::{ProcServ, ProcServConfig};

# async fn run(cfg: ProcServConfig) -> Result<(), Box<dyn std::error::Error>> {
let server = ProcServ::new(cfg)?;
server.run().await?;
# Ok(()) }
```

The end-to-end test at `tests/procserv_e2e.rs` shows a complete config
literal with TCP listener, key bindings, restart policy and a
`/bin/cat` child — useful as a copy-paste starting point.

## Architecture

The C → Rust mapping is documented at the crate root in
[`src/lib.rs`](src/lib.rs). High-level points:

- Hub-and-spoke fan-out via a single supervisor task forwarding
  per-connection mpsc messages, matching C `SendToAll`'s
  exclude-the-sender semantics without `tokio::sync::broadcast`
  re-delivery.
- Per-connection `readonly` flag instead of a master role.
- Stateless command-key dispatch (no menu FSM); keys are still echoed
  to other connections.
- Narrow telnet usage — only `IAC WILL ECHO` and `IAC DO LINEMODE` are
  negotiated, so the in-crate parser stays under ~80 LOC instead of
  vendoring `libtelnet.c`.

## License

See [`LICENSE`](LICENSE).
