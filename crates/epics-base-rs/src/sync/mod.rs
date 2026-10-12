//! Shared-value primitives for values cycle threads read.

mod retiring_swap;

pub use retiring_swap::{Freed, RetireGuard, RetiringSwap, prime_current_thread};
