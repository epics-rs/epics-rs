//! `pvlist-rs` — server discovery + (optional) PV name enumeration
//! mirroring pvxs `tools/list.cpp`.
//!
//! ```text
//! pvlist-rs                  # discover servers (passive beacon listen)
//! pvlist-rs -w 5             # discover for 5 seconds, then exit
//! pvlist-rs --ping           # actively ping (DiscoverBuilder::pingAll)
//! pvlist-rs --verbose        # include guid + proto + peer
//! ```
//!
//! Pass-through PV-name enumeration (pvxs `pvlist <ip>` form) would
//! require sending `serverInfo` RPCs to each discovered server; that's
//! a known follow-up. v1 surfaces the discovery stream.

use std::collections::HashMap;
use std::time::Duration;

use clap::Parser;
use epics_pva_rs::client_native::search_engine::{Discovered, SearchEngine};

#[derive(Parser)]
#[command(name = "pvlist-rs", version, about = "Discover PVA servers")]
struct Args {
    /// Wait time in seconds before exiting (0 = forever)
    #[arg(short = 'w', default_value = "5.0")]
    timeout: f64,

    /// Actively ping discoverable servers (DiscoverBuilder::pingAll).
    #[arg(short = 'p', long = "ping")]
    ping: bool,

    /// Verbose output — include GUID, proto, and beacon peer address.
    #[arg(short = 'v', long = "verbose")]
    verbose: bool,
}

fn fmt_guid(guid: &[u8; 12]) -> String {
    let mut s = String::with_capacity(24);
    for b in guid {
        use std::fmt::Write;
        write!(&mut s, "{b:02X}").unwrap();
    }
    s
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let engine = match SearchEngine::spawn(Vec::new()).await {
        Ok(e) => e,
        Err(e) => {
            eprintln!("pvlist-rs: failed to spawn search engine: {e}");
            std::process::exit(1);
        }
    };
    let mut rx = match engine.discover().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("pvlist-rs: failed to subscribe to discovery: {e}");
            std::process::exit(1);
        }
    };
    if args.ping {
        engine.ping_all().await;
    }

    // De-dup by (server, guid) so a chatty IOC doesn't spam.
    let mut seen: HashMap<(std::net::SocketAddr, [u8; 12]), bool> = HashMap::new();
    // `args.timeout <= 0` means "wait forever" by design. Non-finite
    // (NaN / ±Inf) also collapses to "no deadline" — closer to user
    // intent than the 5 s default, and avoids the
    // `Duration::from_secs_f64` panic on Inf.
    let deadline = if args.timeout.is_finite() && args.timeout > 0.0 {
        Some(tokio::time::Instant::now() + Duration::from_secs_f64(args.timeout))
    } else {
        None
    };

    loop {
        let recv_fut = rx.recv();
        let evt = match deadline {
            Some(d) => match tokio::time::timeout_at(d, recv_fut).await {
                Ok(opt) => opt,
                Err(_) => break,
            },
            None => recv_fut.await,
        };
        let Some(evt) = evt else {
            break;
        };
        match evt {
            Discovered::Online {
                server,
                guid,
                peer,
                proto,
            } => {
                if seen.insert((server, guid), true).is_some() {
                    continue;
                }
                if args.verbose {
                    println!(
                        "ONLINE   {server:24}  guid={}  proto={proto}  peer={peer}",
                        fmt_guid(&guid)
                    );
                } else {
                    println!("ONLINE   {server}");
                }
            }
            Discovered::Timeout { server, guid } => {
                if args.verbose {
                    println!("OFFLINE  {server:24}  guid={}", fmt_guid(&guid));
                } else {
                    println!("OFFLINE  {server}");
                }
            }
        }
    }

    if args.verbose && !seen.is_empty() {
        println!("\n{} server(s) seen.", seen.len());
    }
}
