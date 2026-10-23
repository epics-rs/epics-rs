#[cfg(feature = "parallel")]
use std::sync::OnceLock;
#[cfg(feature = "parallel")]
use std::sync::atomic::{AtomicUsize, Ordering};

/// Minimum element count to justify rayon overhead.
#[deprecated(since = "0.32.1", note = "the pool gate is `pool_wins`")]
pub const PAR_THRESHOLD: usize = 4096;

/// Number of CPU cores reserved for driver threads, tokio runtime, etc.
/// The rayon pool will use `available_cores - RESERVED_CORES` threads (minimum 1).
#[cfg(feature = "parallel")]
const RESERVED_CORES: usize = 2;

/// Returns true if the data size warrants parallel processing.
#[deprecated(
    since = "0.32.1",
    note = "an element count alone pools passes that lose to serial; use `pool_wins`"
)]
#[allow(deprecated)]
pub fn should_parallelize(num_elements: usize) -> bool {
    cfg!(feature = "parallel") && num_elements >= PAR_THRESHOLD
}

/// Whether to split a pass over `elements` elements, at `ps_per_element`
/// picoseconds each serially, in `chunks` chunks across the pool.
///
/// C runs these passes serially, so the split must never lose to that: a
/// lone chunk has nothing to split, and a pass shorter than `min_us`, the
/// serial time from which the split measured faster at the calling site,
/// loses to handing its chunks to sleeping workers. Always false without
/// the `parallel` feature, so a caller needs no `#[cfg]` fallback of its own.
///
/// Unit tests split every pass of two or more chunks, so test-sized frames
/// still reach the pooled paths; the `--no-default-features` run covers the
/// serial ones.
pub fn pool_wins(elements: usize, ps_per_element: usize, min_us: usize, chunks: usize) -> bool {
    cfg!(feature = "parallel")
        && chunks >= 2
        && (cfg!(test) || takes_at_least(elements, ps_per_element, min_us))
}

fn takes_at_least(elements: usize, ps_per_element: usize, min_us: usize) -> bool {
    (elements as u64).saturating_mul(ps_per_element as u64)
        >= (min_us as u64).saturating_mul(1_000_000)
}

/// Shared rayon ThreadPool.
///
/// Plugins in non-blocking mode each have their own data thread, so multiple
/// plugins may submit rayon work concurrently. A single shared pool ensures
/// work-stealing without over-subscription.
///
/// The pool is sized to `available_cores - RESERVED_CORES` to leave headroom
/// for port driver data threads, autoconnect tasks, and the tokio runtime.
/// Call [`set_num_threads`] before the first `thread_pool()` access to override.
#[cfg(feature = "parallel")]
static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();

/// User-specified thread count override. 0 means "use default formula".
#[cfg(feature = "parallel")]
static NUM_THREADS_OVERRIDE: AtomicUsize = AtomicUsize::new(0);

/// Set the number of rayon worker threads before the pool is first used.
///
/// Must be called before any plugin processes an array. Has no effect if the
/// pool has already been initialized.
#[cfg(feature = "parallel")]
pub fn set_num_threads(n: usize) {
    NUM_THREADS_OVERRIDE.store(n, Ordering::Relaxed);
}

#[cfg(feature = "parallel")]
pub fn thread_pool() -> &'static rayon::ThreadPool {
    POOL.get_or_init(|| {
        let user = NUM_THREADS_OVERRIDE.load(Ordering::Relaxed);
        let num_threads = if user > 0 {
            user
        } else {
            let available = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1);
            available.saturating_sub(RESERVED_CORES).max(1)
        };
        rayon::ThreadPoolBuilder::new()
            .num_threads(num_threads)
            .build()
            .expect("failed to create rayon thread pool")
    })
}

#[cfg(test)]
mod tests {
    use super::{pool_wins, takes_at_least};

    #[test]
    fn pool_wins_takes_two_chunks() {
        let parallel = cfg!(feature = "parallel");
        assert_eq!(pool_wins(1, 1, usize::MAX, 2), parallel);
        assert!(!pool_wins(usize::MAX, 1, 0, 1));
        assert!(!pool_wins(usize::MAX, 1, 0, 0));
    }

    #[test]
    fn takes_at_least_compares_the_serial_time() {
        // 1_000_000 elements at 100 ps: 100 us serially.
        assert!(takes_at_least(1_000_000, 100, 100));
        assert!(!takes_at_least(999_999, 100, 100));
        assert!(takes_at_least(usize::MAX, usize::MAX, 1 << 40));
    }
}
