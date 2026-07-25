//! Emits the RTEMS link arguments for `realtime-ca-ioc`.
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
//! two implementations: `tokio::net::TcpStream` on `tokio_backend`, and
//! `runtime::blocking_io`'s two-thread pump on `exec_backend`, which gives a
//! spawned future no reactor. This `--cfg` forces the second one on a build
//! that would otherwise take the first; it is the only way to reach that arm
//! without also moving the whole crate onto the exec backend.
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

//! # `exec_backend` / `tokio_backend` — does a spawned future get a reactor?
//!
//! `epics-base-rs`'s `build.rs` defines this rule and this script repeats it,
//! for this crate's own compilation:
//!
//! ```text
//! exec_backend  ⟺  target_os == "rtems"  ||  feature "rtems-exec-model"
//! tokio_backend ⟺  otherwise
//! ```
//!
//! Why the CA client needs it. Every client task is started through
//! `runtime::task::spawn`. On `exec_backend` that lands on a callback-pool
//! worker with **no tokio reactor entered**, so any `tokio::net` socket the
//! task opens panics — including in a hosted process that has a tokio runtime
//! elsewhere, because the runtime is not entered on that worker. The client's
//! UDP SEARCH transport was gated on `not(target_os = "rtems")`, which names
//! the target when the fact it needs is the *backend*; a host build with
//! `--features rtems-exec-model` compiled the UDP transport in and panicked on
//! it at `realtime-ca-ioc`'s first search (measured, `doc/calink-rtems-design.md`
//! §10.10 item 2). `tokio_backend` is the predicate that means "a reactor
//! exists", so the transport takes that one and `SearchTransport` has the
//! single `NameServersOnly` variant on `exec_backend` — the target's shape,
//! now reached by the host build that models the target.
//!
//! This is a third copy of a four-line rule, so it is pinned rather than
//! trusted: a `const` assertion in `src/lib.rs` checks it against
//! `epics_base_rs::runtime::task::HAS_TOKIO_REACTOR` at compile time. A build
//! that enables `epics-base-rs/rtems-exec-model` without this crate's own
//! `rtems-exec-model` fails to compile instead of panicking at boot.

fn main() {
    // Declared, never emitted here. See the note above.
    println!("cargo::rustc-check-cfg=cfg(ca_blocking_client)");
    println!("cargo::rustc-check-cfg=cfg(exec_backend)");
    println!("cargo::rustc-check-cfg=cfg(tokio_backend)");
    println!("cargo::rustc-check-cfg=cfg(ca_beacon_monitor)");

    let rtems = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("rtems");
    let host_exec_model = std::env::var_os("CARGO_FEATURE_RTEMS_EXEC_MODEL").is_some();
    let tokio_backend = !(rtems || host_exec_model);
    if tokio_backend {
        println!("cargo::rustc-cfg=tokio_backend");
    } else {
        println!("cargo::rustc-cfg=exec_backend");
    }

    // `ca_beacon_monitor` — this build has a UDP beacon listener.
    //
    // One name for one fact, because it is consumed at fifteen sites in
    // `client/mod.rs` (the task handle, the coordinator's anomaly message, the
    // control channel, the abort on shutdown) and a two-term conjunction
    // restated fifteen times is fifteen chances to restate it differently.
    //
    // Both terms are load-bearing. `feature = "client"` because the beacon
    // monitor is an optimisation over the UDP discovery stack that the
    // `client-core` split leaves out (`doc/calink-rtems-design.md` §2.1).
    // `tokio_backend` because the monitor's own socket and its repeater
    // registration are `tokio::net` UDP sockets opened inside a future started
    // through `runtime::task::spawn` — on `exec_backend` that future runs on a
    // callback-pool worker with no reactor entered and both sockets panic.
    if tokio_backend && std::env::var_os("CARGO_FEATURE_CLIENT").is_some() {
        println!("cargo::rustc-cfg=ca_beacon_monitor");
    }

    epics_rtems_boot::contract::emit_link_args();
}
