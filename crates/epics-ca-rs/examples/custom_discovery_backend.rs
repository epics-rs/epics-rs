//! Custom CA discovery backend example.
//!
//! Built-in backends cover mDNS (link-local) and DNS-SD over unicast
//! DNS. Sites running their own service registry (Consul, etcd, an
//! HTTP CMDB, a Kafka topic, …) can implement `discovery::Backend` and
//! plug it in via `CaClientConfig::extra_backends`.
//!
//! This example walks through three template backends, each
//! illustrating a different shape of registry, and then wires them
//! into a `CaClient`. Pick whichever pattern matches your environment;
//! the trait surface is intentionally narrow.
//!
//! Run with:
//! ```bash
//! cargo run -p epics-ca-rs --example custom_discovery_backend -- MY:PV
//! ```
//!
//! Note: the HTTP example uses `reqwest` if you have it on your
//! workspace, but we keep this example dependency-free by stubbing
//! the request out — drop in your real client where marked.

// `epics_ca_rs::client::CaClient` is `tokio_backend`-only — it awaits on the
// UDP search transport — so every item here follows it, and the `exec_backend`
// arm is a `main` that says why rather than a build error. Not the discovery
// module: only its `mdns`/`dnssd`/`dns_update` submodules need a reactor, and
// the `Backend` trait this example implements does not.
// Cargo's `required-features` cannot name a build-script cfg, so the gate is
// in this file; the example is still built and linted in that configuration.
#[cfg(tokio_backend)]
use std::net::SocketAddr;

#[cfg(tokio_backend)]
use async_trait::async_trait;
#[cfg(tokio_backend)]
use epics_ca_rs::client::{CaClient, CaClientConfig};
#[cfg(tokio_backend)]
use epics_ca_rs::discovery::{Backend, DiscoveryEvent};
#[cfg(tokio_backend)]
use tokio::sync::mpsc;

// ─── 1. Static-from-file backend ────────────────────────────────────
//
// Reads a newline-separated list of `host:port` from a file on disk.
// Useful as a thin wrapper around an existing site config or to test
// the wiring without any extra infrastructure.

#[cfg(tokio_backend)]
struct FileBackend {
    path: std::path::PathBuf,
}

#[cfg(tokio_backend)]
#[async_trait]
impl Backend for FileBackend {
    async fn discover(&self) -> Vec<SocketAddr> {
        let Ok(content) = tokio::fs::read_to_string(&self.path).await else {
            tracing::warn!(path = %self.path.display(), "FileBackend: cannot read file");
            return Vec::new();
        };
        content
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.starts_with('#'))
            .filter_map(|s| s.parse::<SocketAddr>().ok())
            .collect()
    }
}

// ─── 2. HTTP-API backend ────────────────────────────────────────────
//
// Queries a site CMDB or a custom registry that exposes a JSON list of
// IOCs at a known URL. Drop in any HTTP client (`reqwest`, `ureq`,
// `hyper`); we leave the call as a placeholder.

#[cfg(tokio_backend)]
struct HttpRegistryBackend {
    endpoint: String,
}

#[cfg(tokio_backend)]
#[async_trait]
impl Backend for HttpRegistryBackend {
    async fn discover(&self) -> Vec<SocketAddr> {
        // Replace this stub with a real GET to `self.endpoint`. Expect
        // a JSON body like `[{"host": "10.0.0.5", "port": 5064}, …]`
        // and parse with serde. For the example we return an empty
        // list so this builds without a network call.
        tracing::info!(endpoint = %self.endpoint, "HttpRegistryBackend would query here");
        Vec::new()
    }
}

// ─── 3. Push-style backend ──────────────────────────────────────────
//
// Some registries notify the client when IOCs come and go (Consul
// blocking queries, Kubernetes watches, an internal AMQP topic).
// Implement `subscribe()` to feed those updates into the search engine
// in real time. The trait still requires `discover()` for the initial
// fetch, but afterwards the client tracks live changes from the
// channel.

#[cfg(tokio_backend)]
struct PushBackend {
    initial: Vec<SocketAddr>,
    rx: std::sync::Mutex<Option<mpsc::UnboundedReceiver<DiscoveryEvent>>>,
}

#[cfg(tokio_backend)]
impl PushBackend {
    fn new(initial: Vec<SocketAddr>) -> (Self, mpsc::UnboundedSender<DiscoveryEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let backend = Self {
            initial,
            rx: std::sync::Mutex::new(Some(rx)),
        };
        (backend, tx)
    }
}

#[cfg(tokio_backend)]
#[async_trait]
impl Backend for PushBackend {
    async fn discover(&self) -> Vec<SocketAddr> {
        self.initial.clone()
    }

    fn subscribe(&self) -> Option<mpsc::UnboundedReceiver<DiscoveryEvent>> {
        self.rx.lock().ok().and_then(|mut g| g.take())
    }
}

// ─── Wiring ─────────────────────────────────────────────────────────

#[cfg(tokio_backend)]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let pv_name = std::env::args().nth(1).unwrap_or_else(|| "TEST:PV".into());

    // Build a config with all three custom backends installed.
    let (push_backend, push_tx) = PushBackend::new(vec!["10.0.0.10:5064".parse()?]);

    let config = CaClientConfig {
        extra_backends: vec![
            Box::new(FileBackend {
                path: "/etc/epics/iocs.list".into(),
            }),
            Box::new(HttpRegistryBackend {
                endpoint: "http://cmdb.facility.local/api/iocs".into(),
            }),
            Box::new(push_backend),
        ],
        ..CaClientConfig::default()
    };

    // Demonstrate that nothing stops you from sending events into the
    // push channel from elsewhere — usually this would be a watch
    // task spawned by your `Backend` impl itself.
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let _ = push_tx.send(DiscoveryEvent::Added {
            instance: "late-comer".into(),
            addr: "10.0.0.99:5064".parse().unwrap(),
        });
    });

    let client = CaClient::new_with_config(config).await?;
    let (_dbf, value) = client.caget(&pv_name).await?;
    println!("{pv_name} = {value}");
    Ok(())
}

/// The `exec_backend` arm. Discovery backends need a reactor to await their
/// registry sockets on, and this build has none, so there is nothing for the
/// example to demonstrate.
#[cfg(exec_backend)]
fn main() {
    eprintln!(
        "custom_discovery_backend: needs the tokio backend; this build selects \
         EPICS_RS_BUILD_EXEC_BACKEND=thread, which has no reactor for a discovery backend to \
         await on."
    );
}
