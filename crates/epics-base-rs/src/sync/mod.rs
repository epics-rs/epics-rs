//! Shared-value primitives for values cycle threads read.

mod retiring_swap;

pub use retiring_swap::{RetireGuard, RetiringSwap, prime_current_thread};
