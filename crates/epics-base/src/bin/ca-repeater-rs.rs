use epics_base_rs::repeater::run_repeater;

#[tokio::main]
async fn main() {
    if let Err(e) = run_repeater().await {
        // Port already in use means another repeater is running — that's fine
        if e.kind() == std::io::ErrorKind::AddrInUse {
            return;
        }
        eprintln!("ca-repeater: {e}");
        std::process::exit(1);
    }
}
