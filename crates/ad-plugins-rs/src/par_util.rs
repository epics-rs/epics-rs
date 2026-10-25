#[cfg(feature = "parallel")]
use std::path::Path;
#[cfg(feature = "parallel")]
use std::sync::OnceLock;
#[cfg(feature = "parallel")]
use std::sync::atomic::{AtomicUsize, Ordering};

/// Minimum element count to justify rayon overhead.
#[deprecated(since = "0.32.1", note = "the pool gate is `pool_wins`")]
pub const PAR_THRESHOLD: usize = 4096;

/// Number of CPU cores reserved for driver threads, tokio runtime, etc.
/// No rayon pool uses more than `available_cores - RESERVED_CORES` threads (minimum 1).
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
/// loses more to waking a worker and claiming and merging chunks than the
/// workers win back. Always false without
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

/// `f(i)` for each `i` in `0..n`, returned in order of `i`, by the calling
/// thread and the [`thread_pool`] workers that start while indices remain.
///
/// The caller claims indices from the first, so the pass never waits for a
/// worker to wake: one that starts after the last claim does nothing, and
/// the caller waits only for indices already claimed. Between frames the
/// workers sleep, and waking one costs more than a short pass takes.
/// Without the `parallel` feature the caller runs them all.
pub fn map_indices<R: Send>(n: usize, f: impl Fn(usize) -> R + Sync) -> Vec<R> {
    let mut slots: Vec<Option<R>> = (0..n).map(|_| None).collect();
    let base = Disjoint(slots.as_mut_ptr());
    claim::run(n, &|claims| {
        for i in claims {
            // SAFETY: `i` is claimed once, so its slot is written once.
            unsafe { *base.at(i) = Some(f(i)) };
        }
    });
    (slots.into_iter())
        .map(|s| s.expect("every index ran"))
        .collect()
}

/// The chunks a pass over a buffer splits into: the fewest that keep each
/// within a maximum length, all a whole number of units and as equal as
/// that allows.
///
/// Equal, so no chunk is a sliver: a fixed length leaves the remainder as a
/// last chunk, and at 257x257 a 65536 split was 65536 elements and 513, a
/// worker woken for 0.8% of the pass, 1.04-1.27x serial back-to-back for
/// the histograms against 0.56-0.68x in halves.
#[derive(Clone, Copy, Debug)]
pub struct Split {
    len: usize,
    unit: usize,
    units: usize,
    chunks: usize,
}

impl Split {
    /// `len` elements in units of `unit`, the last unit possibly short, cut
    /// into chunks of at most `max_len` elements, or of one unit where a
    /// unit is longer.
    pub fn new(len: usize, unit: usize, max_len: usize) -> Self {
        let unit = unit.max(1);
        let units = len.div_ceil(unit);
        Self {
            len,
            unit,
            units,
            chunks: units.div_ceil((max_len / unit).max(1)),
        }
    }

    pub fn chunks(&self) -> usize {
        self.chunks
    }

    /// The elements of chunk `i`: the first `units % chunks` chunks take
    /// one unit more than the rest.
    fn range(&self, i: usize) -> std::ops::Range<usize> {
        let (q, r) = (self.units / self.chunks, self.units % self.chunks);
        let start = i * q + i.min(r);
        start * self.unit..self.len.min((start + q + usize::from(i < r)) * self.unit)
    }
}

/// `f` of each chunk of `v` that `split` cuts, in order, as [`map_indices`]
/// runs them.
pub fn map_chunks<T: Sync, R: Send>(v: &[T], split: Split, f: impl Fn(&[T]) -> R + Sync) -> Vec<R> {
    map_indices(split.chunks, |i| f(&v[split.range(i)]))
}

/// One `S` per thread that took chunks of `v`, each from `init()` and then
/// `f(&mut s, chunk)` for every chunk `split` cuts that thread claimed, as
/// [`map_indices`] claims them.
pub fn fold_chunks<T: Sync, S: Send>(
    v: &[T],
    split: Split,
    init: impl Fn() -> S + Sync,
    f: impl Fn(&mut S, &[T]) + Sync,
) -> Vec<S> {
    let states = parking_lot::Mutex::new(Vec::new());
    claim::run(split.chunks, &|claims| {
        let mut state = None;
        for i in claims {
            f(state.get_or_insert_with(&init), &v[split.range(i)]);
        }
        states.lock().extend(state);
    });
    states.into_inner()
}

/// `f(start, chunk)` for each chunk of `out` that `split` cuts, `start` its
/// first element's index in `out`, as [`map_indices`] claims them.
pub fn for_each_chunk_mut<T: Send>(
    out: &mut [T],
    split: Split,
    f: impl Fn(usize, &mut [T]) + Sync,
) {
    assert!(split.len <= out.len());
    let base = Disjoint(out.as_mut_ptr());
    claim::run(split.chunks, &|claims| {
        for i in claims {
            let range = split.range(i);
            // SAFETY: `i` is claimed once and the chunks of a split are
            // disjoint and inside `out`, so its chunk is borrowed once.
            f(range.start, unsafe {
                std::slice::from_raw_parts_mut(base.at(range.start), range.len())
            });
        }
    });
}

/// Elements that the claiming threads each reach at an index of their own.
struct Disjoint<T>(*mut T);

// SAFETY: a thread dereferences only the elements of the indices it claimed.
unsafe impl<T: Send> Sync for Disjoint<T> {}

impl<T> Disjoint<T> {
    /// # Safety
    /// `i` is inside the slice `self.0` starts.
    unsafe fn at(&self, i: usize) -> *mut T {
        unsafe { self.0.add(i) }
    }
}

/// The claiming behind [`map_indices`]: each participant, the caller first,
/// gets an iterator over the indices it claims.
#[cfg(feature = "parallel")]
mod claim {
    use std::any::Any;
    use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The indices one participant claims, until none are left.
    pub struct Claims<'a> {
        next: &'a AtomicUsize,
        n: usize,
    }

    impl Iterator for Claims<'_> {
        type Item = usize;

        fn next(&mut self) -> Option<usize> {
            let i = self.next.fetch_add(1, Ordering::Relaxed);
            (i < self.n).then_some(i)
        }
    }

    type Body<'a> = dyn Fn(Claims<'_>) + Sync + 'a;

    /// Set in `Job::inside` once the caller admits no more helpers.
    const CLOSED: usize = 1 << (usize::BITS - 1);

    struct Job {
        next: AtomicUsize,
        n: usize,
        /// Helpers admitted and not yet done, with `CLOSED` once the caller
        /// stops admitting them.
        inside: AtomicUsize,
        /// The caller's body, its lifetime erased: only an admitted helper
        /// calls it, and the caller does not return before every admitted
        /// helper is done.
        body: *const Body<'static>,
        panic: parking_lot::Mutex<Option<Box<dyn Any + Send>>>,
    }

    // SAFETY: `body` is `Sync`, and `Job` dereferences it only while the
    // caller that owns it waits (see `Job::body`).
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        fn claims(&self) -> Claims<'_> {
            Claims {
                next: &self.next,
                n: self.n,
            }
        }

        fn help(self: Arc<Self>) {
            if self.inside.fetch_add(1, Ordering::AcqRel) & CLOSED != 0 {
                self.inside.fetch_sub(1, Ordering::Release);
                return;
            }
            // SAFETY: admitted before `CLOSED`, so the caller waits for the
            // `fetch_sub` below before its body goes away.
            let body = unsafe { &*self.body };
            if let Err(payload) = catch_unwind(AssertUnwindSafe(|| body(self.claims()))) {
                self.next.fetch_max(self.n, Ordering::Relaxed);
                *self.panic.lock() = Some(payload);
            }
            self.inside.fetch_sub(1, Ordering::Release);
        }
    }

    /// Closes the job and waits out its admitted helpers, on return and on
    /// unwind alike.
    struct Close<'a>(&'a Job);

    impl Drop for Close<'_> {
        fn drop(&mut self) {
            let job = self.0;
            // Ends the helpers' claims when the caller unwinds early.
            job.next.fetch_max(job.n, Ordering::Relaxed);
            let mut inside = job.inside.fetch_or(CLOSED, Ordering::AcqRel);
            let mut spins = 0u32;
            while inside & !CLOSED != 0 {
                if spins < 1 << 10 {
                    std::hint::spin_loop();
                } else {
                    std::thread::yield_now();
                }
                spins += 1;
                inside = job.inside.load(Ordering::Acquire);
            }
        }
    }

    /// `body` once on the calling thread and once on each pool worker
    /// that starts while indices of `0..n` remain.
    pub fn run(n: usize, body: &Body<'_>) {
        let pool = super::thread_pool();
        let helpers = pool.current_num_threads().min(n.saturating_sub(1));
        if helpers == 0 {
            return body(Claims {
                next: &AtomicUsize::new(0),
                n,
            });
        }
        // SAFETY: only the lifetime changes; `Job::body` says why it holds.
        let erased = unsafe { std::mem::transmute::<*const Body<'_>, *const Body<'static>>(body) };
        let job = Arc::new(Job {
            next: AtomicUsize::new(0),
            n,
            inside: AtomicUsize::new(0),
            body: erased,
            panic: parking_lot::Mutex::new(None),
        });
        // Every helper from the caller: helpers that each spawned two more
        // kept node0 u8 histogram 2048x2048 back-to-back at 370-420 us, against
        // 230-240 spawned here and 260 for rayon's own split; spawning four,
        // eight or all from the first helper stayed at 370.
        for _ in 0..helpers {
            let job = job.clone();
            pool.spawn(move || job.help());
        }
        {
            let _close = Close(&job);
            body(job.claims());
        }
        if let Some(payload) = job.panic.lock().take() {
            resume_unwind(payload);
        }
    }
}

/// Without the pool the caller claims every index.
#[cfg(not(feature = "parallel"))]
mod claim {
    pub fn run(n: usize, body: &(dyn Fn(std::ops::Range<usize>) + Sync + '_)) {
        body(0..n)
    }
}

/// The shared rayon pools, one per L3 cache domain.
///
/// Plugins in non-blocking mode each have their own data thread, so multiple
/// plugins may submit rayon work concurrently; shared pools keep that from
/// over-subscribing the box. [`thread_pool`] hands a pass the pool of the
/// domain its caller runs in, whose cache holds the frame the caller just
/// touched: on a two-socket box fed new frames back-to-back or 1-10 ms
/// apart, 24 workers there beat 94 across both sockets in 29 of 39 cells
/// measured, by up to 5.8x (u16 histogram 4096x4096). The 94 won by 2x on
/// u16 bayer 2048x2048 back-to-back and by under 30% in the other nine.
#[cfg(feature = "parallel")]
static POOLS: OnceLock<Pools> = OnceLock::new();

/// User-specified thread count override. 0 means "use default formula".
#[cfg(feature = "parallel")]
static NUM_THREADS_OVERRIDE: AtomicUsize = AtomicUsize::new(0);

/// Set the number of rayon worker threads before the pool is first used.
/// An explicit count gives one unpinned pool of `n` workers instead of one
/// pool per L3 domain.
///
/// Must be called before any plugin processes an array. Has no effect if the
/// pool has already been initialized.
#[cfg(feature = "parallel")]
pub fn set_num_threads(n: usize) {
    NUM_THREADS_OVERRIDE.store(n, Ordering::Relaxed);
}

/// The pool of the L3 domain the calling thread runs on.
#[cfg(feature = "parallel")]
pub fn thread_pool() -> &'static rayon::ThreadPool {
    let pools = POOLS.get_or_init(Pools::new);
    let d = match &pools.domains[..] {
        [one] => one,
        all => {
            let cpu = affinity::current();
            &all[pools.domain_of.get(cpu).copied().unwrap_or(0)]
        }
    };
    d.pool.get_or_init(|| d.build())
}

#[cfg(feature = "parallel")]
struct Pools {
    /// The domain of each CPU by number; a CPU outside every domain takes
    /// domain 0.
    domain_of: Vec<usize>,
    domains: Vec<Domain>,
}

/// A pool and the CPUs it runs on, built on first use.
#[cfg(feature = "parallel")]
struct Domain {
    /// The CPUs its workers are pinned to: the domain's CPUs within the
    /// process's affinity mask, or none for an unpinned pool.
    cpus: Vec<usize>,
    workers: usize,
    pool: OnceLock<rayon::ThreadPool>,
}

#[cfg(feature = "parallel")]
impl Pools {
    /// One pool per L3 domain the process may run in, each one worker per
    /// physical core it may use there, at most `available_cores -
    /// RESERVED_CORES`. One domain, or no topology to read, gives one
    /// unpinned pool of that many.
    fn new() -> Self {
        let cap = std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .saturating_sub(RESERVED_CORES)
            .max(1);
        match NUM_THREADS_OVERRIDE.load(Ordering::Relaxed) {
            0 => match l3_domains(Path::new("/sys/devices/system/cpu"), &affinity::allowed()) {
                Some(found) => Self::split(found, cap),
                None => Self::single(cap),
            },
            n => Self::single(n),
        }
    }

    fn single(workers: usize) -> Self {
        Self {
            domain_of: Vec::new(),
            domains: vec![Domain {
                cpus: Vec::new(),
                workers,
                pool: OnceLock::new(),
            }],
        }
    }

    /// The pools of `found`, [`l3_domains`]' domains, each at most `cap`
    /// workers.
    fn split(found: Vec<(Vec<usize>, usize)>, cap: usize) -> Self {
        let mut domain_of = Vec::new();
        let domains = (found.into_iter().enumerate())
            .map(|(i, (cpus, cores))| {
                for &c in &cpus {
                    if domain_of.len() <= c {
                        domain_of.resize(c + 1, 0);
                    }
                    domain_of[c] = i;
                }
                Domain {
                    cpus,
                    workers: cores.min(cap),
                    pool: OnceLock::new(),
                }
            })
            .collect();
        Self { domain_of, domains }
    }
}

#[cfg(feature = "parallel")]
impl Domain {
    fn build(&self) -> rayon::ThreadPool {
        let mut builder = rayon::ThreadPoolBuilder::new().num_threads(self.workers);
        if !self.cpus.is_empty() {
            let cpus = self.cpus.clone();
            builder = builder.start_handler(move |_| affinity::pin(&cpus));
        }
        builder.build().expect("failed to create rayon thread pool")
    }
}

/// The L3 domains of the `allowed` CPUs, read from the sysfs CPU directory
/// `root`: each domain's allowed CPUs and the physical cores among them.
/// `None` unless every allowed CPU names its L3 and its core, and they fall
/// in two domains or more.
#[cfg(feature = "parallel")]
fn l3_domains(root: &Path, allowed: &[usize]) -> Option<Vec<(Vec<usize>, usize)>> {
    // Keyed by the L3's and the core's CPU lists as sysfs prints them, which
    // every CPU sharing them prints alike.
    let mut found: Vec<(String, Vec<usize>, Vec<String>)> = Vec::new();
    for &cpu in allowed {
        let dir = root.join(format!("cpu{cpu}"));
        let l3 = std::fs::read_dir(dir.join("cache"))
            .ok()?
            .flatten()
            .find(|e| read(&e.path().join("level")).as_deref() == Some("3"))
            .and_then(|e| read(&e.path().join("shared_cpu_list")))?;
        let core = read(&dir.join("topology/core_cpus_list"))
            .or_else(|| read(&dir.join("topology/thread_siblings_list")))?;
        let i = match found.iter().position(|d| d.0 == l3) {
            Some(i) => i,
            None => {
                found.push((l3, Vec::new(), Vec::new()));
                found.len() - 1
            }
        };
        let (_, cpus, cores) = &mut found[i];
        cpus.push(cpu);
        if !cores.contains(&core) {
            cores.push(core);
        }
    }
    (found.len() >= 2).then(|| {
        (found.into_iter())
            .map(|(_, cpus, cores)| (cpus, cores.len()))
            .collect()
    })
}

#[cfg(feature = "parallel")]
fn read(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_owned())
}

#[cfg(all(feature = "parallel", target_os = "linux"))]
mod affinity {
    use std::mem::{size_of, zeroed};

    /// The CPUs in the process's affinity mask, so taskset and cpusets
    /// bound the pools.
    pub fn allowed() -> Vec<usize> {
        // SAFETY: the mask is a `cpu_set_t` owned by this frame, and
        // `CPU_ISSET` reads it below `CPU_SETSIZE`.
        unsafe {
            let mut set: libc::cpu_set_t = zeroed();
            if libc::sched_getaffinity(libc::getpid(), size_of::<libc::cpu_set_t>(), &mut set) != 0
            {
                return Vec::new();
            }
            (0..libc::CPU_SETSIZE as usize)
                .filter(|&c| libc::CPU_ISSET(c, &set))
                .collect()
        }
    }

    /// The CPU the calling thread runs on, `usize::MAX` if unknown.
    pub fn current() -> usize {
        // SAFETY: no arguments, no memory of ours.
        usize::try_from(unsafe { libc::sched_getcpu() }).unwrap_or(usize::MAX)
    }

    /// The calling thread confined to `cpus`. A refusal, a cpuset narrowed
    /// since `cpus` was read, leaves it where it was spawned.
    pub fn pin(cpus: &[usize]) {
        // SAFETY: the mask is a `cpu_set_t` owned by this frame, set only
        // below `CPU_SETSIZE`, which bounds every CPU `allowed` returns.
        unsafe {
            let mut set: libc::cpu_set_t = zeroed();
            for &c in cpus {
                libc::CPU_SET(c, &mut set);
            }
            libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &set);
        }
    }
}

/// No affinity calls: no CPU is allowed by number, so [`Pools`] keeps one
/// unpinned pool.
#[cfg(all(feature = "parallel", not(target_os = "linux")))]
mod affinity {
    pub fn allowed() -> Vec<usize> {
        Vec::new()
    }

    pub fn current() -> usize {
        usize::MAX
    }

    pub fn pin(_: &[usize]) {}
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "parallel")]
    use super::{Pools, l3_domains};
    use super::{Split, pool_wins, takes_at_least};
    #[cfg(feature = "parallel")]
    use std::path::Path;

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

    /// A sysfs CPU directory under `root`: each `(cpu, l3, core)` gets an L1
    /// and an L3 cache entry and its core's CPU list, under the pre-5.x name
    /// when `core` starts with `old:`.
    #[cfg(feature = "parallel")]
    fn sysfs(root: &Path, cpus: &[(usize, &str, &str)]) {
        let put = |path: std::path::PathBuf, text: &str| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, format!("{text}\n")).unwrap();
        };
        for &(cpu, l3, core) in cpus {
            let dir = root.join(format!("cpu{cpu}"));
            put(dir.join("cache/index0/level"), "1");
            put(dir.join("cache/index0/shared_cpu_list"), core);
            put(dir.join("cache/index3/level"), "3");
            put(dir.join("cache/index3/shared_cpu_list"), l3);
            match core.strip_prefix("old:") {
                Some(core) => put(dir.join("topology/thread_siblings_list"), core),
                None => put(dir.join("topology/core_cpus_list"), core),
            }
        }
    }

    /// Two L3 domains of two cores with two threads each: cpus 0,1 and their
    /// siblings 4,5 in one, 2,3 and 6,7 in the other.
    #[cfg(feature = "parallel")]
    fn two_sockets(root: &Path) {
        let (a, b) = ("0-1,4-5", "2-3,6-7");
        sysfs(
            root,
            &[
                (0, a, "0,4"),
                (1, a, "1,5"),
                (2, b, "old:2,6"),
                (3, b, "old:3,7"),
                (4, a, "0,4"),
                (5, a, "1,5"),
                (6, b, "old:2,6"),
                (7, b, "old:3,7"),
            ],
        );
    }

    #[cfg(feature = "parallel")]
    #[test]
    fn a_domain_is_its_allowed_cpus_and_their_cores() {
        let root = tempfile::tempdir().unwrap();
        two_sockets(root.path());
        let all: Vec<usize> = (0..8).collect();
        assert_eq!(
            l3_domains(root.path(), &all),
            Some(vec![(vec![0, 1, 4, 5], 2), (vec![2, 3, 6, 7], 2)])
        );
        // One thread of each core allowed: the same cores, half the CPUs.
        assert_eq!(
            l3_domains(root.path(), &[0, 1, 2, 3]),
            Some(vec![(vec![0, 1], 2), (vec![2, 3], 2)])
        );
        assert_eq!(
            l3_domains(root.path(), &[0, 4, 2]),
            Some(vec![(vec![0, 4], 1), (vec![2], 1)])
        );
    }

    #[cfg(feature = "parallel")]
    #[test]
    fn one_domain_or_no_topology_gives_no_split() {
        let root = tempfile::tempdir().unwrap();
        two_sockets(root.path());
        assert_eq!(l3_domains(root.path(), &[0, 1, 4, 5]), None);
        assert_eq!(l3_domains(root.path(), &[]), None);
        assert_eq!(l3_domains(&root.path().join("absent"), &[0, 2]), None);
        // A CPU with no L3, or no core list, leaves the topology unread.
        sysfs(root.path(), &[(8, "8", "8")]);
        std::fs::remove_dir_all(root.path().join("cpu8/cache/index3")).unwrap();
        assert_eq!(l3_domains(root.path(), &[0, 2, 8]), None);
        sysfs(root.path(), &[(9, "0-1,4-5", "9")]);
        std::fs::remove_dir_all(root.path().join("cpu9/topology")).unwrap();
        assert_eq!(l3_domains(root.path(), &[0, 2, 9]), None);
    }

    #[cfg(feature = "parallel")]
    #[test]
    fn split_maps_each_cpu_to_its_domain_and_caps_the_workers() {
        let pools = Pools::split(vec![(vec![0, 1, 4, 5], 2), (vec![2, 3, 6, 9], 3)], 2);
        assert_eq!(pools.domain_of, [0, 0, 1, 1, 0, 0, 1, 0, 0, 1]);
        let workers: Vec<usize> = pools.domains.iter().map(|d| d.workers).collect();
        assert_eq!(workers, [2, 2]);
        assert_eq!(pools.domains[1].cpus, [2, 3, 6, 9]);
    }

    /// The chunk lengths of `split` over `len` elements in `unit`s, after
    /// checking that they tile `0..len` in order, each starting on a unit.
    fn lengths(split: Split, len: usize, unit: usize) -> Vec<usize> {
        let mut at = 0;
        let lens = (0..split.chunks())
            .map(|i| {
                let r = split.range(i);
                assert_eq!((r.start, r.start % unit), (at, 0));
                at = r.end;
                r.len()
            })
            .collect();
        assert_eq!(at, len);
        lens
    }

    #[test]
    fn a_split_cuts_the_fewest_equal_chunks_of_whole_units() {
        // 257x257 in chunks of at most 65536: halves, not 65536 and 513.
        assert_eq!(
            lengths(Split::new(66049, 1, 1 << 16), 66049, 1),
            [33025, 33024]
        );
        assert_eq!(
            lengths(Split::new(4 << 16, 1, 1 << 16), 4 << 16, 1),
            [1 << 16; 4]
        );
        assert_eq!(Split::new(1 << 16, 1, 1 << 16).chunks(), 1);
        // 504 rows of 504, at most 130 to a chunk: four of 126, not 130 x 3 + 114.
        assert_eq!(
            lengths(Split::new(504 * 504, 504, 1 << 16), 504 * 504, 504),
            [126 * 504; 4]
        );
        // A unit longer than the maximum goes alone, and a short last unit
        // ends the last chunk.
        assert_eq!(lengths(Split::new(10, 4, 3), 10, 4), [4, 4, 2]);
        assert!(lengths(Split::new(0, 1, 7), 0, 1).is_empty());
        for len in 0..120 {
            for unit in 1..6 {
                for max_len in 1..40 {
                    let lens = lengths(Split::new(len, unit, max_len), len, unit);
                    let per = (max_len / unit).max(1) * unit;
                    // Each within the maximum, and one chunk fewer would not be.
                    assert!(lens.iter().all(|&l| l <= per));
                    assert!(lens.len() <= 1 || len > (lens.len() - 1) * per);
                    // Within a unit of each other, a short last unit counted whole.
                    let units: Vec<usize> = lens.iter().map(|l| l.div_ceil(unit)).collect();
                    let (lo, hi) = (units.iter().min(), units.iter().max());
                    assert!(lo.zip(hi).is_none_or(|(lo, hi)| hi - lo <= 1));
                }
            }
        }
    }

    #[test]
    fn map_indices_returns_every_index_in_order() {
        for n in [0, 1, 2, 1000] {
            let got = super::map_indices(n, |i| i * 3);
            assert_eq!(got, (0..n).map(|i| i * 3).collect::<Vec<_>>());
        }
        let v: Vec<u32> = (0..1000).collect();
        let split = Split::new(v.len(), 1, 7);
        let sums = super::map_chunks(&v, split, |c| c.iter().sum::<u32>());
        let each = || (0..split.chunks()).map(|i| split.range(i));
        assert_eq!(
            sums,
            each().map(|r| v[r].iter().sum()).collect::<Vec<u32>>()
        );
        let counts = super::fold_chunks(&v, split, || 0usize, |n, c| *n += c.len());
        assert_eq!(counts.iter().sum::<usize>(), 1000);
        let mut out = vec![usize::MAX; 1000];
        super::for_each_chunk_mut(&mut out, split, |at, c| c.fill(at));
        assert!(each().all(|r| out[r.clone()].iter().all(|&at| at == r.start)));
        let nested = super::map_indices(8, |i| {
            super::map_indices(8, |j| i * j).iter().sum::<usize>()
        });
        assert_eq!(nested, (0..8).map(|i| i * 28).collect::<Vec<usize>>());
    }

    /// With every worker of every pool busy, the caller runs the whole pass
    /// itself instead of waiting for one.
    #[cfg(feature = "parallel")]
    #[test]
    fn the_caller_runs_the_pass_while_every_worker_is_busy() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let pools = super::POOLS.get_or_init(super::Pools::new);
        let busy = Arc::new((AtomicUsize::new(0), AtomicBool::new(false)));
        let workers: usize = pools.domains.iter().map(|d| d.workers).sum();
        for d in &pools.domains {
            let pool = d.pool.get_or_init(|| d.build());
            for _ in 0..d.workers {
                let busy = busy.clone();
                pool.spawn(move || {
                    busy.0.fetch_add(1, Ordering::SeqCst);
                    while !busy.1.load(Ordering::SeqCst) {
                        std::thread::yield_now();
                    }
                });
            }
        }
        while busy.0.load(Ordering::SeqCst) < workers {
            std::thread::yield_now();
        }
        let caller = std::thread::current().id();
        let on = super::map_indices(64, |_| std::thread::current().id());
        busy.1.store(true, Ordering::SeqCst);
        assert!(on.iter().all(|&t| t == caller));
    }

    #[cfg(feature = "parallel")]
    #[test]
    fn a_helpers_panic_reaches_the_caller() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let helped = AtomicBool::new(false);
        let r = std::panic::catch_unwind(|| {
            super::map_indices(64, |i| {
                if rayon::current_thread_index().is_some() {
                    helped.store(true, Ordering::SeqCst);
                    panic!("helper {i}");
                }
                while !helped.load(Ordering::SeqCst) {
                    std::thread::yield_now();
                }
            })
        });
        let payload = r.expect_err("the helper's panic");
        assert!(
            payload
                .downcast_ref::<String>()
                .unwrap()
                .starts_with("helper ")
        );
    }

    /// The caller's panic does not unwind past the data a helper still reads.
    #[cfg(feature = "parallel")]
    #[test]
    fn a_callers_panic_waits_for_its_helpers() {
        use std::sync::atomic::{AtomicU8, Ordering};
        // 0 no helper yet, 1 a helper inside, 2 that helper done.
        let helper = AtomicU8::new(0);
        let r = std::panic::catch_unwind(|| {
            super::map_indices(64, |_| {
                if rayon::current_thread_index().is_some() {
                    if helper
                        .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                    {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        helper.store(2, Ordering::SeqCst);
                    }
                    // Each helper holds its one index till then, so the
                    // caller, which spawns them all first, still gets one.
                    while helper.load(Ordering::SeqCst) != 2 {
                        std::thread::yield_now();
                    }
                    return;
                }
                while helper.load(Ordering::SeqCst) == 0 {
                    std::thread::yield_now();
                }
                panic!("caller");
            })
        });
        assert!(r.is_err());
        assert_eq!(helper.load(Ordering::SeqCst), 2);
    }

    /// Every worker of a pinned pool runs on the domain's CPUs.
    #[cfg(all(feature = "parallel", target_os = "linux"))]
    #[test]
    fn a_domains_workers_run_on_its_cpus() {
        use super::{Domain, affinity};
        let cpu = affinity::allowed()[0];
        let d = Domain {
            cpus: vec![cpu],
            workers: 2,
            pool: Default::default(),
        };
        let on = d.build().broadcast(|_| affinity::current());
        assert_eq!(on, [cpu, cpu]);
    }
}
