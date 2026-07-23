//! PREEMPT_RT measurement harness for the epics-rs workspace.
//!
//! Four subcommands, each emitting a distribution the caller can paste
//! verbatim:
//!
//!   * `scan-jitter` — boots an in-process IOC with a `.1 second` periodic
//!     calc record, CA-monitors it, and reports the deviation of the
//!     record's own timestamps from the nominal scan period.
//!   * `ca-latency` / `pva-latency` — a counter is driven into a record and
//!     the elapsed time until the matching monitor update is delivered is
//!     collected, optionally against a background CPU-hog load.
//!   * `pi-proof` — the classic three-priority inversion test built directly
//!     on `PvDatabase::lock_record` (the L1 record gate, a
//!     `PriorityInheritanceMutex`). A low-priority holder keeps the gate, a
//!     medium-priority hog burns the one pinned CPU, and a high-priority
//!     waiter times how long it takes to acquire the gate. Build with
//!     `--features linux-rt` for the PI-on arm and without it for the
//!     PI-off arm; the two runs are the comparison.
//!
//! Absolute numbers are QEMU/KVM guest numbers, not bare metal — only the
//! comparative results are load-bearing. Every subcommand prints its own
//! sample size.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::scan::ScanOwner;
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::client::CaClient;
use epics_ca_rs::server::CaServer;
use epics_pva_rs::client_native::context::PvaClient;
use epics_pva_rs::client_native::ops_v2::{MonitorEvent, MonitorEventMask};
use epics_pva_rs::server::PvDatabaseSource;
use epics_pva_rs::server_native::{PvaServer, PvaServerConfig};

#[derive(Parser)]
#[command(
    name = "rt-probe",
    about = "PREEMPT_RT measurement harness for epics-rs"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Periodic-scan jitter as seen in record timestamps.
    ScanJitter {
        /// Number of scan-period samples to collect.
        #[arg(long, default_value_t = 300)]
        samples: usize,
    },
    /// CA monitor delivery latency (put → monitor round-ish trip).
    CaLatency {
        #[arg(long, default_value_t = 500)]
        samples: usize,
        /// Background CPU-hog threads sharing the run.
        #[arg(long, default_value_t = 0)]
        hogs: usize,
    },
    /// PVA monitor delivery latency (put → monitor round-ish trip).
    PvaLatency {
        #[arg(long, default_value_t = 500)]
        samples: usize,
        #[arg(long, default_value_t = 0)]
        hogs: usize,
    },
    /// Observe whether an IOC thread actually gets SCHED_FIFO under the
    /// current `EPICS_RS_ALLOW_RT_PRIORITY` setting — the switch both
    /// directions. Reports `enter_ioc_thread`'s verdict and the kernel's
    /// own `sched_getscheduler`/priority for the thread.
    RtPolicy,
    /// Three-priority inversion proof on the record gate.
    PiProof {
        /// High-priority acquisition samples to collect.
        #[arg(long, default_value_t = 200)]
        samples: usize,
        /// SCHED_FIFO priority of the low holder.
        #[arg(long, default_value_t = 10)]
        low: i32,
        /// SCHED_FIFO priority of the medium hog.
        #[arg(long, default_value_t = 30)]
        med: i32,
        /// SCHED_FIFO priority of the high waiter.
        #[arg(long, default_value_t = 50)]
        high: i32,
        /// CPU to pin the three contenders to. Deliberately NOT 0: a runaway
        /// FIFO band on the boot CPU starves the housekeeping/IRQ threads that
        /// share it and can lock a remote operator out of new SSH sessions
        /// while the kernel itself stays up (observed on this box).
        #[arg(long, default_value_t = 6)]
        cpu: usize,
    },
}

// ─────────────────────────── distribution helper ───────────────────────────

fn report(label: &str, mut us: Vec<f64>) {
    if us.is_empty() {
        println!("{label}: NO SAMPLES");
        return;
    }
    us.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = us.len();
    let pct = |p: f64| us[((n as f64 * p).ceil() as usize).min(n).saturating_sub(1)];
    let mean = us.iter().sum::<f64>() / n as f64;
    println!(
        "{label}: n={n} min={:.1} mean={:.1} p50={:.1} p90={:.1} p99={:.1} p99.9={:.1} max={:.1} (us)",
        us[0],
        mean,
        pct(0.50),
        pct(0.90),
        pct(0.99),
        pct(0.999),
        us[n - 1],
    );
}

// ─────────────────────────── in-process IOC boot ───────────────────────────

struct Ioc {
    db: Arc<PvDatabase>,
    ca_port: u16,
    pva_addr: std::net::SocketAddr,
    _ca_task: tokio::task::JoinHandle<epics_base_rs::error::CaResult<()>>,
    _pva: PvaServer,
    _scan: ScanOwner,
}

const FAST_DB: &str = r#"
record(calc, "RT:SCAN") {
    field(SCAN, ".1 second")
    field(INPA, "RT:SCAN.VAL")
    field(CALC, "A+1")
    field(VAL, "0")
}
record(ao, "RT:AO") {
    field(VAL, "0")
}
"#;

async fn boot_ioc() -> Result<Ioc, Box<dyn std::error::Error>> {
    unsafe {
        std::env::set_var("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1");
        std::env::set_var("EPICS_CAS_BEACON_ADDR_LIST", "127.0.0.1");
    }
    let macros = std::collections::HashMap::new();
    let (db, _autosave) = IocBuilder::new()
        .db_string(FAST_DB, &macros)?
        .build()
        .await?;
    let scan = ScanOwner::start(db.clone());
    let ca_server = CaServer::from_parts(db.clone(), 0, None, None, None, None).await?;
    let ca_port = ca_server.udp_port();
    let _ca_task = tokio::spawn(async move { ca_server.run().await });
    let source = Arc::new(PvDatabaseSource::new(db.clone()));
    let pva = PvaServer::start(source, PvaServerConfig::isolated())?;
    let pva_addr = pva.tcp_addr();
    tokio::time::sleep(Duration::from_millis(300)).await;
    Ok(Ioc {
        db,
        ca_port,
        pva_addr,
        _ca_task,
        _pva: pva,
        _scan: scan,
    })
}

fn point_ca_at(port: u16) {
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }
}

// ─────────────────────────── CPU-hog background load ────────────────────────

fn spawn_hogs(n: usize, stop: Arc<AtomicBool>) -> Vec<std::thread::JoinHandle<()>> {
    (0..n)
        .map(|i| {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name(format!("hog{i}"))
                .spawn(move || {
                    let mut x: u64 = 0x9e3779b97f4a7c15;
                    while !stop.load(Ordering::Relaxed) {
                        // Un-elidable busy work.
                        for _ in 0..100_000 {
                            x ^= x << 13;
                            x ^= x >> 7;
                            x ^= x << 17;
                        }
                        std::hint::black_box(x);
                    }
                })
                .unwrap()
        })
        .collect()
}

// ─────────────────────────── subcommands ───────────────────────────

async fn scan_jitter(samples: usize) -> Result<(), Box<dyn std::error::Error>> {
    let ioc = boot_ioc().await?;
    point_ca_at(ioc.ca_port);
    let ca = CaClient::new().await?;
    let ch = ca.create_channel("RT:SCAN");
    let mut mon = ch.subscribe().await?;

    let period = Duration::from_millis(100);
    let mut prev: Option<Instant> = None;
    let mut devs = Vec::with_capacity(samples);
    let mut got = 0usize;
    // Skip a few warm-up events.
    let mut warm = 5i32;
    while got < samples {
        match tokio::time::timeout(Duration::from_secs(5), mon.recv()).await {
            Ok(Some(Ok(_snap))) => {
                let now = Instant::now();
                if warm > 0 {
                    warm -= 1;
                    prev = Some(now);
                    continue;
                }
                if let Some(p) = prev {
                    let dt = now.duration_since(p);
                    let dev = dt.as_secs_f64() - period.as_secs_f64();
                    devs.push(dev * 1e6); // us, signed deviation from 100 ms
                    got += 1;
                }
                prev = Some(now);
            }
            _ => {
                eprintln!("scan-jitter: monitor stalled, collected {got}");
                break;
            }
        }
    }
    // Report absolute jitter magnitude.
    let mag: Vec<f64> = devs.iter().map(|d| d.abs()).collect();
    report("scan-jitter |dev from 100ms|", mag);
    Ok(())
}

async fn ca_latency(samples: usize, hogs: usize) -> Result<(), Box<dyn std::error::Error>> {
    let ioc = boot_ioc().await?;
    point_ca_at(ioc.ca_port);
    let stop = Arc::new(AtomicBool::new(false));
    let handles = spawn_hogs(hogs, stop.clone());

    let ca = CaClient::new().await?;
    let ch = ca.create_channel("RT:AO");
    let mut mon = ch.subscribe().await?;
    // Drain the initial connect snapshot.
    let _ = tokio::time::timeout(Duration::from_millis(500), mon.recv()).await;

    let mut lats = Vec::with_capacity(samples);
    for i in 0..samples {
        let v = (i + 1) as f64;
        let t0 = Instant::now();
        ch.put(&EpicsValue::Double(v)).await?;
        // Wait for the monitor to deliver exactly this value.
        loop {
            match tokio::time::timeout(Duration::from_secs(2), mon.recv()).await {
                Ok(Some(Ok(snap))) => {
                    if let EpicsValue::Double(x) = snap.value
                        && (x - v).abs() < 1e-9
                    {
                        lats.push(t0.elapsed().as_secs_f64() * 1e6);
                        break;
                    }
                }
                _ => {
                    eprintln!("ca-latency: stalled at sample {i}");
                    break;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    stop.store(true, Ordering::Relaxed);
    for h in handles {
        let _ = h.join();
    }
    report(&format!("ca-latency hogs={hogs}"), lats);
    Ok(())
}

async fn pva_latency(samples: usize, hogs: usize) -> Result<(), Box<dyn std::error::Error>> {
    let ioc = boot_ioc().await?;
    let stop = Arc::new(AtomicBool::new(false));
    let handles = spawn_hogs(hogs, stop.clone());

    let client = PvaClient::builder()
        .timeout(Duration::from_secs(5))
        .server_addr(ioc.pva_addr)
        .build();

    // Monitor RT:AO in a background task; funnel the latest delivered VAL to
    // a shared cell keyed by the counter value.
    let seen = Arc::new(AtomicU64::new(0));
    let seen_cb = seen.clone();
    let mon_client = client.clone();
    tokio::spawn(async move {
        let _ = mon_client
            .pvmonitor_events("RT:AO", None, MonitorEventMask::default(), move |ev| {
                if let MonitorEvent::Data { value, .. } = ev
                    && let Some(v) = extract_pva_double(&value)
                {
                    seen_cb.store(v as u64, Ordering::SeqCst);
                }
            })
            .await;
    });
    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut lats = Vec::with_capacity(samples);
    for i in 0..samples {
        let v = (i + 1) as u64;
        let t0 = Instant::now();
        // Put through the in-process db so the put itself is not the network
        // variable under test; the monitor delivery is.
        ioc.db
            .put_record_field_from_ca("RT:AO", "VAL", EpicsValue::Double(v as f64))
            .await
            .ok();
        loop {
            if seen.load(Ordering::SeqCst) >= v {
                lats.push(t0.elapsed().as_secs_f64() * 1e6);
                break;
            }
            if t0.elapsed() > Duration::from_secs(2) {
                eprintln!("pva-latency: stalled at sample {i}");
                break;
            }
            tokio::time::sleep(Duration::from_micros(200)).await;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    stop.store(true, Ordering::Relaxed);
    for h in handles {
        let _ = h.join();
    }
    report(&format!("pva-latency hogs={hogs}"), lats);
    Ok(())
}

fn extract_pva_double(f: &epics_pva_rs::PvField) -> Option<f64> {
    use epics_pva_rs::{PvField, ScalarValue};
    match f {
        PvField::Structure(s) => {
            let v = s.get_field("value")?;
            extract_pva_double(v)
        }
        PvField::Scalar(sc) => match sc {
            ScalarValue::Double(x) => Some(*x),
            ScalarValue::Float(x) => Some(*x as f64),
            ScalarValue::Long(x) => Some(*x as f64),
            ScalarValue::Int(x) => Some(*x as f64),
            _ => None,
        },
        _ => None,
    }
}

// ─────────────────────────── RT-policy switch probe ───────────────────────────

/// Spawn one IOC thread through the real `enter_ioc_thread` path and report
/// both the runtime's own verdict and the kernel's ground truth. Run twice by
/// the caller — once with `EPICS_RS_ALLOW_RT_PRIORITY=YES`, once `=NO` — to see
/// the switch bite in both directions. Must run as root (or with CAP_SYS_NICE)
/// for the YES arm to actually obtain SCHED_FIFO.
fn rt_policy() {
    use epics_base_rs::runtime::task::{RtPolicy, ThreadPriority, enter_ioc_thread};
    let raw = std::env::var(epics_base_rs::runtime::task::RT_PRIORITY_ENV).ok();
    println!(
        "rt-policy: {}={:?} -> RtPolicy::{:?}",
        epics_base_rs::runtime::task::RT_PRIORITY_ENV,
        raw,
        RtPolicy::current()
    );
    let handle = std::thread::spawn(|| {
        let verdict = enter_ioc_thread(ThreadPriority::CaServerLow);
        // Kernel ground truth for THIS thread (tid 0 == caller).
        let policy = unsafe { libc::sched_getscheduler(0) };
        let mut param: libc::sched_param = unsafe { std::mem::zeroed() };
        unsafe { libc::sched_getparam(0, &mut param) };
        let policy_name = match policy {
            libc::SCHED_FIFO => "SCHED_FIFO",
            libc::SCHED_RR => "SCHED_RR",
            libc::SCHED_OTHER => "SCHED_OTHER",
            _ => "?",
        };
        println!(
            "rt-policy: enter_ioc_thread(CaServerLow) verdict={verdict:?}; kernel policy={policy_name} prio={}",
            param.sched_priority
        );
    });
    handle.join().unwrap();
}

// ─────────────────────────── PI proof ───────────────────────────

fn set_fifo(prio: i32) -> bool {
    let param = libc::sched_param {
        sched_priority: prio,
    };
    let r = unsafe { libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) };
    r == 0
}

fn pin_cpu(cpu: usize) -> bool {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(cpu, &mut set);
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) == 0
    }
}

async fn pi_proof(
    samples: usize,
    low: i32,
    med: i32,
    high: i32,
    cpu: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    println!(
        "pi-proof: PI mutex active in this build = {}",
        epics_base_rs::runtime::sync::is_pi_mutex_active()
    );
    // Boot only the database — we exercise the record gate directly, no
    // servers needed.
    let macros = std::collections::HashMap::new();
    // `build()` already returns an `Arc<PvDatabase>`.
    let (db, _autosave) = IocBuilder::new()
        .db_string(FAST_DB, &macros)?
        .build()
        .await?;

    // Everything pinned to one CPU so the three priorities genuinely
    // contend on a single runqueue (the only configuration in which an
    // inversion can occur).
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let stop = Arc::new(AtomicBool::new(false));
    let holder_has_lock = Arc::new(AtomicBool::new(false));
    let release_req = Arc::new(AtomicBool::new(false));

    // ---- low-priority holder ----
    let h_db = db.clone();
    let h_bar = barrier.clone();
    let h_stop = stop.clone();
    let h_has = holder_has_lock.clone();
    let h_rel = release_req.clone();
    let holder = std::thread::Builder::new()
        .name("pi-holder".into())
        .spawn(move || {
            pin_cpu(cpu);
            let ok = set_fifo(low);
            eprintln!("holder: SCHED_FIFO({low}) set={ok}");
            h_bar.wait();
            while !h_stop.load(Ordering::Relaxed) {
                let guard = h_db.lock_record("RT:AO");
                h_has.store(true, Ordering::SeqCst);
                // Hold until the waiter has asked for the lock, then do a
                // bounded chunk of work still holding it, then release.
                while !h_rel.load(Ordering::SeqCst) && !h_stop.load(Ordering::Relaxed) {
                    std::hint::spin_loop();
                }
                // Bounded critical-section work while holding the gate: spin
                // for ~10 ms of *CPU time* the holder must actually receive.
                // With PI the holder inherits the waiter's priority (above the
                // hog) and gets the CPU immediately, so this is ~10 ms of wall
                // time. Without PI the holder (below the hog) only advances in
                // the windows the hog yields (below), so the same 10 ms of CPU
                // is smeared across a much longer wall interval — that
                // stretch, measured by the waiter, is the inversion.
                let cs = Instant::now();
                let mut x: u64 = 1;
                while cs.elapsed() < Duration::from_millis(10) {
                    for _ in 0..2_000 {
                        x ^= x << 13;
                        x ^= x >> 7;
                        std::hint::black_box(x);
                    }
                }
                h_has.store(false, Ordering::SeqCst);
                h_rel.store(false, Ordering::SeqCst);
                drop(guard);
                // Let the waiter re-arm.
                std::thread::sleep(Duration::from_micros(200));
            }
        })?;

    // ---- medium-priority hog ----
    let m_bar = barrier.clone();
    let m_stop = stop.clone();
    let hog = std::thread::Builder::new()
        .name("pi-hog".into())
        .spawn(move || {
            pin_cpu(cpu);
            let ok = set_fifo(med);
            eprintln!("hog: SCHED_FIFO({med}) set={ok}");
            m_bar.wait();
            // Spin ~20 ms, then yield CPU0 for ~5 ms. The yield is what makes
            // the PI-OFF arm *terminate* instead of hanging in an unbounded
            // inversion: it lets the below-hog holder make slow progress. The
            // 4:1 spin:sleep ratio keeps the inversion large and clearly
            // separated from the PI-ON latency, while bounding it.
            let mut x: u64 = 0x1234;
            while !m_stop.load(Ordering::Relaxed) {
                let burst = Instant::now();
                while burst.elapsed() < Duration::from_millis(20) {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    std::hint::black_box(x);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        })?;

    // ---- high-priority waiter (this thread) ----
    let w_db = db.clone();
    let w_bar = barrier.clone();
    let w_stop = stop.clone();
    let w_has = holder_has_lock.clone();
    let w_rel = release_req.clone();
    let waiter =
        std::thread::Builder::new()
            .name("pi-waiter".into())
            .spawn(move || -> Vec<f64> {
                pin_cpu(cpu);
                let ok = set_fifo(high);
                eprintln!("waiter: SCHED_FIFO({high}) set={ok}");
                w_bar.wait();
                let mut lats = Vec::with_capacity(samples);
                let mut i = 0;
                while i < samples {
                    // Wait until the holder actually owns the gate. This MUST
                    // sleep, not spin: the waiter runs at the highest FIFO
                    // priority on the same pinned CPU as the low-priority
                    // holder, so a busy-wait here would starve the very holder
                    // it is waiting on — a priority inversion in the harness
                    // rather than in the gate under test. `sleep` relinquishes
                    // the CPU so the holder can run and take the gate.
                    let spin_start = Instant::now();
                    while !w_has.load(Ordering::SeqCst) {
                        if spin_start.elapsed() > Duration::from_secs(5) {
                            break;
                        }
                        std::thread::sleep(Duration::from_micros(200));
                    }
                    if !w_has.load(Ordering::SeqCst) {
                        continue;
                    }
                    // Ask the holder to enter its critical work + release, then
                    // time how long acquiring the gate takes.
                    w_rel.store(true, Ordering::SeqCst);
                    let t0 = Instant::now();
                    let guard = w_db.lock_record("RT:AO");
                    let dt = t0.elapsed().as_secs_f64() * 1e6;
                    drop(guard);
                    lats.push(dt);
                    i += 1;
                    if w_stop.load(Ordering::Relaxed) {
                        break;
                    }
                }
                lats
            })?;

    let lats = waiter.join().expect("waiter join");
    stop.store(true, Ordering::Relaxed);
    let _ = holder.join();
    let _ = hog.join();

    report(
        &format!(
            "pi-proof low={low} med={med} high={high} pi_active={}",
            epics_base_rs::runtime::sync::is_pi_mutex_active()
        ),
        lats,
    );
    Ok(())
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::ScanJitter { samples } => scan_jitter(samples).await,
        Cmd::CaLatency { samples, hogs } => ca_latency(samples, hogs).await,
        Cmd::PvaLatency { samples, hogs } => pva_latency(samples, hogs).await,
        Cmd::RtPolicy => {
            rt_policy();
            Ok(())
        }
        Cmd::PiProof {
            samples,
            low,
            med,
            high,
            cpu,
        } => pi_proof(samples, low, med, high, cpu).await,
    }
}
