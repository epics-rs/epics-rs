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
//! exec_backend  ⟺  epics_embedded_target (target_os in {"rtems", "vxworks"})
//!               ||  EPICS_RS_BUILD_EXEC_BACKEND=thread
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
//! `EPICS_RS_BUILD_EXEC_BACKEND=thread` compiled the UDP transport in and
//! panicked on it at `realtime-ca-ioc`'s first search (measured).
//! `tokio_backend` is the predicate that means "a reactor exists", so every
//! arm that needs one takes that predicate — the target's shape, now reached
//! by the host build that models the target.
//!
//! The SEARCH socket is no longer one of those arms. It binds on both backends
//! through `epics_base_rs::net::search_udp::SearchUdpSocket`, whose
//! `exec_backend` arm is a `std::net::UdpSocket` and a receive-pump thread and
//! names no reactor. `SearchTransport` still reduces to `NameServersOnly` when
//! the configuration asks for no UDP — that variant describes a configuration
//! now, not a target.
//!
//! This is a third copy of a four-line rule, so it is pinned rather than
//! trusted: a `const` assertion in `src/lib.rs` checks it against
//! `epics_base_rs::runtime::task::HAS_TOKIO_REACTOR` at compile time. A build
//! in which one of the two scripts missed `EPICS_RS_BUILD_EXEC_BACKEND` fails
//! to compile instead of panicking at boot.

fn main() {
    // Declared, never emitted here. See the note above.
    println!("cargo::rustc-check-cfg=cfg(ca_blocking_client)");
    println!("cargo::rustc-check-cfg=cfg(exec_backend)");
    println!("cargo::rustc-check-cfg=cfg(tokio_backend)");
    println!("cargo::rustc-check-cfg=cfg(ca_beacon_monitor)");
    println!("cargo::rustc-check-cfg=cfg(epics_embedded_target)");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    // The reactor-free targets: no tokio reactor exists on either, so every
    // module gate above this crate's async front-end (`server/mod.rs`,
    // `server/udp.rs`, `server/tcp.rs`, `server/addr_list.rs`, `lib.rs`'s
    // `discovery`/`repeater`/`hostname`/`cli`/`copt`) gates on this one
    // capability cfg instead of repeating `any(target_os = "rtems", target_os
    // = "vxworks")`.
    let embedded_target = matches!(target_os.as_str(), "rtems" | "vxworks");
    if embedded_target {
        println!("cargo::rustc-cfg=epics_embedded_target");
    }

    // Build-time backend selection, from the environment rather than from a
    // cargo feature: a feature that flips a backend is not additive, so
    // `--all-features` turned the reactor off and no single invocation meant
    // "everything on". `epics-libcom-rs`'s module docs carry the reasoning;
    // `tools/rtems-exec-gate` holds every copy of this block against that
    // crate's, so 23 derivations of one rule cannot drift apart.
    println!("cargo::rerun-if-env-changed=EPICS_RS_BUILD_EXEC_BACKEND");
    let requested = std::env::var_os("EPICS_RS_BUILD_EXEC_BACKEND").unwrap_or_default();
    let host_exec_backend = match requested.to_string_lossy().as_ref() {
        "thread" => true,
        "" | "tokio" => false,
        bad => panic!(
            "EPICS_RS_BUILD_EXEC_BACKEND={bad}: the exec backend is `thread` \
             (reactor-free std threads) or `tokio` (the host default, which an \
             unset or empty variable also selects)"
        ),
    };
    if embedded_target || host_exec_backend {
        println!("cargo::rustc-cfg=exec_backend");
    } else {
        println!("cargo::rustc-cfg=tokio_backend");
    }
    let tokio_backend = !(embedded_target || host_exec_backend);

    // `ca_beacon_monitor` — this build has a UDP beacon listener.
    //
    // One name for one fact, because `client/mod.rs` gates on it at every site
    // that touches the monitor — the task handle, the coordinator's anomaly
    // message, the control channel, the abort on shutdown — and a two-term
    // conjunction restated at each of them is one chance per site to restate it
    // differently. Deliberately no count: this sentence used to carry one, it
    // was wrong by four the first time anyone checked it against the file, and
    // the argument was never about how many.
    //
    // Both terms are load-bearing. `feature = "client"` because the beacon
    // monitor is an optimisation over the UDP discovery stack that the
    // `client-core` split leaves out. `tokio_backend` because the monitor's
    // own socket and its repeater registration are `tokio::net` UDP sockets
    // opened inside a future started through `runtime::task::spawn` — on
    // `exec_backend` that future runs on a callback-pool worker with no
    // reactor entered and both sockets panic.
    if tokio_backend && std::env::var_os("CARGO_FEATURE_CLIENT").is_some() {
        println!("cargo::rustc-cfg=ca_beacon_monitor");
    }

    epics_rtems_boot::contract::emit_link_args();
}
