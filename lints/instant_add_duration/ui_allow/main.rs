use std::time::{Duration, Instant};

fn owner(d: Duration) -> Instant {
    let saturate = || Instant::now() + d;
    Instant::now().checked_add(d).unwrap_or_else(saturate)
}

fn caller(d: Duration) -> Instant {
    Instant::now() + d
}

fn main() {
    let _ = owner(Duration::from_secs(1));
    let _ = caller(Duration::from_secs(1));
}
