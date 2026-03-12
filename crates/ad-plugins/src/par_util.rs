#[cfg(feature = "parallel")]
use std::sync::OnceLock;
#[cfg(feature = "parallel")]
use std::sync::atomic::{AtomicUsize, Ordering};

/// Minimum element count to justify rayon overhead.
pub const PAR_THRESHOLD: usize = 4096;

/// Number of CPU cores reserved for driver threads, tokio runtime, etc.
/// The rayon pool will use `available_cores - RESERVED_CORES` threads (minimum 1).
const RESERVED_CORES: usize = 2;

/// Returns true if the data size warrants parallel processing.
pub fn should_parallelize(num_elements: usize) -> bool {
    num_elements >= PAR_THRESHOLD
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
