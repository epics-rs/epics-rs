use std::ops::{Add, AddAssign};
use std::time::{Duration, Instant};

const FIXED: Duration = Duration::from_secs(1);

fn main() {
    let now = Instant::now();
    let timeout = Duration::from_secs_f64(1e300_f64.min(1.0));

    let _ = now + timeout;
    let _ = Instant::now() + FIXED;
    let mut next = now;
    next += timeout;
    let _ = now.add(timeout);
    let _ = Add::add(now, timeout);
    let _ = <Instant as Add<Duration>>::add(now, timeout);
    next.add_assign(timeout);
    let later = || now + timeout;
    let _ = later();

    // Not this lint: the owner's `checked_add`, `Duration + Duration`, `Sub`.
    let _ = now.checked_add(timeout);
    let _ = timeout + FIXED;
    let _ = now - FIXED;
    let mut total = timeout;
    total += FIXED;
    let _ = 1_u64 + 2;
}
