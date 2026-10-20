//! Replay a recorded CA event log to stdout.
//!
//! Reads the JSON-Lines file produced by `epics_ca_rs::replay::EventRecorder`
//! and emits each event back. Two modes:
//!
//! - default: streams as fast as possible, useful for analysis pipes.
//! - `--paced`: honours wall-clock timing of the original recording,
//!   so a 1-hour log replays over 1 hour. Useful when feeding into a
//!   live dashboard.
//!
//! Usage:
//! ```bash
//! ca-replay-rs --file rec.jsonl
//! ca-replay-rs --file rec.jsonl --paced
//! ca-replay-rs --file rec.jsonl --filter beacon_recv
//! ```

use clap::Parser;
use epics_ca_rs::replay::{RecordedEvent, replay};

#[derive(Parser)]
#[command(name = "ca-replay-rs")]
struct Args {
    /// Path to the recording file.
    #[arg(long)]
    file: String,

    /// Honour wall-clock pacing of the original events.
    #[arg(long)]
    paced: bool,

    /// Only emit events of this kind (`beacon_recv`,
    /// `client_connect`, `client_disconnect`).
    #[arg(long)]
    filter: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let filter = args.filter;
    let count = replay(&args.file, args.paced, |ev| {
        let kind = match ev {
            RecordedEvent::BeaconRecv { .. } => "beacon_recv",
            RecordedEvent::ClientConnect { .. } => "client_connect",
            RecordedEvent::ClientDisconnect { .. } => "client_disconnect",
        };
        if let Some(ref f) = filter
            && f != kind
        {
            return;
        }
        println!("{}", ev.to_json());
    })
    .await?;
    epics_libcom_rs::stderr_println!("replayed {count} events");
    Ok(())
}
