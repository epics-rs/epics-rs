//! Callback-band cost, the port's counterpart to the C bench the band's layout
//! and refill rule were measured with.
//!
//! `cargo run --release --example band_bench -- <workers> <n> <rounds> <producers> <spin>`
//!
//! Three numbers, because the band has three costs and they differ by three
//! orders of magnitude:
//!
//! - **push** — the requester path with every worker already busy, so no wake
//!   and no pop contend with it. This is the band's only hot path in C's
//!   `callbackRequest`, and what the submission-side atomics show up in.
//! - **drain** — pop and refill with no requester running: `n` entries already
//!   queued, every worker released at once. Note this is one refill of an
//!   `n`-entry batch followed by `n` pops, so it weighs a pop heavily and a
//!   refill barely; a change to the refill rule shows up in `trip`, where
//!   batches are the handful of entries a live band actually carries, or not at
//!   all.
//! - **trip** — end to end, requesters and workers concurrent. Dominated by one
//!   park/unpark per callback whenever a worker outruns its requester, which on
//!   an idle band it does; `spin` (iterations burnt inside each callback) is how
//!   far that can be pushed back.
//!
//! Lower is better everywhere. The port allocates one `Box<dyn FnOnce>` per
//! request where C hands over a caller-owned struct, so a given change in the
//! queue is a smaller fraction here.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

use epics_libcom_rs::runtime::background::CallbackPriority;
use epics_libcom_rs::runtime::background::callback_executor::CallbackPool;

const BAND: CallbackPriority = CallbackPriority::Medium;
const QUEUE: usize = 65_536;

/// Occupy every worker of the band, so a push neither wakes anyone nor races a
/// pop. Returns the gate the workers are spinning on.
fn occupy(pool: &CallbackPool, workers: usize) -> Arc<AtomicBool> {
    let gate = Arc::new(AtomicBool::new(false));
    let running = Arc::new(AtomicUsize::new(0));
    for _ in 0..workers {
        let gate = Arc::clone(&gate);
        let running = Arc::clone(&running);
        pool.request(
            BAND,
            Box::new(move || {
                running.fetch_add(1, Ordering::SeqCst);
                while !gate.load(Ordering::Acquire) {
                    std::hint::spin_loop();
                }
            }),
        )
        .unwrap();
    }
    while running.load(Ordering::SeqCst) < workers {
        std::hint::spin_loop();
    }
    gate
}

fn submit(pool: &CallbackPool, n: usize, done: &Arc<AtomicUsize>, spin: usize) -> usize {
    let mut full = 0;
    for _ in 0..n {
        loop {
            let done = Arc::clone(done);
            // A refused request drops its callback, so the box is rebuilt per
            // attempt. Only the accepted one is timed work.
            let cb = Box::new(move || {
                for _ in 0..spin {
                    std::hint::spin_loop();
                }
                done.fetch_add(1, Ordering::Relaxed);
            });
            if pool.request(BAND, cb).is_ok() {
                break;
            }
            full += 1;
            std::hint::spin_loop();
        }
    }
    full
}

fn stat(label: &str, mut v: Vec<f64>) {
    v.sort_by(f64::total_cmp);
    println!(
        "  {label:5} min={:8.1} med={:8.1} max={:8.1} ns/callback",
        v[0],
        v[v.len() / 2],
        v[v.len() - 1]
    );
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let workers: usize = a.first().map_or(1, |v| v.parse().unwrap());
    let n: usize = a.get(1).map_or(20_000, |v| v.parse().unwrap());
    let rounds: usize = a.get(2).map_or(7, |v| v.parse().unwrap());
    let producers: usize = a.get(3).map_or(1, |v| v.parse().unwrap());
    let spin: usize = a.get(4).map_or(0, |v| v.parse().unwrap());
    let total = n * producers;
    assert!(
        total + workers <= QUEUE,
        "{total} entries do not fit the {QUEUE}-slot band"
    );

    let (mut push, mut drain, mut trip) = (vec![], vec![], vec![]);
    for _ in 0..rounds {
        // push + drain: the two halves measured apart.
        let pool = Arc::new(CallbackPool::with_config(QUEUE, workers));
        let done = Arc::new(AtomicUsize::new(0));
        let gate = occupy(&pool, workers);

        let t0 = Instant::now();
        std::thread::scope(|s| {
            for _ in 0..producers {
                let (pool, done) = (Arc::clone(&pool), Arc::clone(&done));
                s.spawn(move || assert_eq!(submit(&pool, n, &done, spin), 0, "band overflowed"));
            }
        });
        push.push(t0.elapsed().as_nanos() as f64 / total as f64);

        let t0 = Instant::now();
        gate.store(true, Ordering::Release);
        while done.load(Ordering::Relaxed) < total {
            std::hint::spin_loop();
        }
        drain.push(t0.elapsed().as_nanos() as f64 / total as f64);
        drop(pool);

        // trip: the same work with nothing held back.
        let pool = Arc::new(CallbackPool::with_config(QUEUE, workers));
        let done = Arc::new(AtomicUsize::new(0));
        let t0 = Instant::now();
        std::thread::scope(|s| {
            for _ in 0..producers {
                let (pool, done) = (Arc::clone(&pool), Arc::clone(&done));
                s.spawn(move || {
                    submit(&pool, n, &done, spin);
                });
            }
        });
        while done.load(Ordering::Relaxed) < total {
            std::hint::spin_loop();
        }
        trip.push(t0.elapsed().as_nanos() as f64 / total as f64);
    }
    println!("p={producers} w={workers} n={n} spin={spin}");
    stat("push", push);
    stat("drain", drain);
    stat("trip", trip);
}
