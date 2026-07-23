//! Emits the RTEMS link arguments for `rtems-ca-ioc`.
//!
//! This crate produces an RTEMS IOC binary, and link *arguments* — unlike
//! `-L`/`-l` — do not propagate from a dependency's build script to a
//! dependent's link (measured; see `epics_rtems_boot::contract`). So the
//! package that owns the binary has to emit them, and it does so by calling
//! into the one crate that defines them rather than by repeating a flag list.
//!
//! No-ops on every non-RTEMS target.
//!
//! # `ca_blocking_client` — forcing the blocking client transport on a host
//!
//! The CA client dials through one seam ([`client::transport::dial_ca`]) with
//! two implementations: `tokio::net::TcpStream` on a hosted target, and
//! `runtime::blocking_io`'s two-thread pump on RTEMS, which has no reactor.
//! Showing that the second one leaves the frame pipeline untouched means
//! running the *whole* host client suite against it, including the integration
//! tests under `tests/`, which are separate crates and so cannot see anything
//! `#[cfg(test)]`.
//!
//! It is a bare `--cfg`, declared below and emitted by nobody:
//!
//! ```text
//! RUSTFLAGS="--cfg ca_blocking_client" cargo nextest run -p epics-ca-rs
//! ```
//!
//! A cargo feature was the obvious alternative and is the wrong tool: features
//! unify across the graph, so any crate in a workspace build enabling it would
//! silently move every other crate's CA client onto the blocking transport. A
//! runtime env var would ship the switch in release binaries, where an operator
//! setting it would change the transport of a production IOC. A `--cfg` that no
//! manifest can turn on cannot reach either place. Same mechanism, same
//! reasoning, as `epics-pva-rs`'s `pva_blocking_client`.

fn main() {
    // Declared, never emitted here. See the note above.
    println!("cargo::rustc-check-cfg=cfg(ca_blocking_client)");
    epics_rtems_boot::contract::emit_link_args();
}
