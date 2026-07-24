//! The RTEMS blocking PVA driver serving QSRV groups, driven end-to-end on
//! the host — the coverage gap `doc/qsrv-rtems-design.md` §9.15 exposed.
//!
//! The first GroupPump landing passed every host gate and then wedged the
//! QEMU target server-wide the moment one group subscription saw sustained
//! posting: the drain was a Medium-band pool task whose poll returns only
//! when every member queue is simultaneously dry, so under load it held the
//! band's single cooperative worker forever — starving the per-PV monitor
//! forwarders that share the band and (at SCHED_FIFO 64 on one core) every
//! thread below it. The gates stayed green because nothing on the host drove
//! [`BlockingPvaServer`] with a group source under sustained posting; the
//! feature-ON qsrv tests exercised `GroupMonitor` directly and briefly.
//!
//! This test closes that gap with the measured target scenario itself: one
//! real client monitoring a 20-member group, one real client monitoring an
//! unrelated scalar (the §9.15 victim, `RTEMS:PVA:V0`'s role), a poster
//! thread flooding every member, and a fresh third client issuing a GET
//! mid-flood (the "fresh pvxget times out permanently" probe). Both monitors
//! must keep delivering DURING the flood and the GET must complete. On the
//! unfixed tree the scalar monitor stops dead once the flood starts —
//! exactly the target symptom, reproduced on the host.
//!
//! Exec-model only: this drives the blocking driver + background-executor
//! configuration the target runs; the hosted tokio driver is a different
//! server (`PvaServer::start`) with its own suites.
#![cfg(feature = "rtems-exec-model")]

// RTEMS-EXEC-MODEL-ALLOW(1): checked - runs and passes in the feature-ON suite.

use std::io::Write as _;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_app::GroupLoadRequest;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::records::ai::AiRecord;
use epics_bridge_rs::qsrv::build_qsrv_mount;
use epics_pva_rs::client::PvaClient;
use epics_pva_rs::server::PvDatabaseSource;
use epics_pva_rs::server_native::blocking::BlockingPvaServer;
use epics_pva_rs::server_native::composite::CompositeSource;
use epics_pva_rs::server_native::config::PvaServerConfig;

const MEMBERS: usize = 20;
const GROUP_PV: &str = "TESTB:BIG";
const SCALAR_PV: &str = "TESTB:V0";

fn member_name(i: usize) -> String {
    format!("TESTB:m{i:02}")
}

/// Await `cond` becoming true, polling; panic with `what` on deadline. The
/// counters are bumped from the client's callback threads, so polling from
/// the test task observes cross-thread progress without coupling to the
/// callback's execution context.
async fn wait_for(cond: impl Fn() -> bool, deadline: Duration, what: &str) {
    let end = Instant::now() + deadline;
    while !cond() {
        assert!(Instant::now() < end, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn client(addr: SocketAddr) -> PvaClient {
    PvaClient::builder()
        .server_addr(addr)
        .timeout(Duration::from_secs(2))
        .build()
}

#[tokio::test]
async fn group_flood_starves_neither_the_scalar_monitor_nor_a_fresh_get() {
    // ── The IOC side: 20 group members + the victim scalar ──────────────
    let db = Arc::new(PvDatabase::new());
    let mut fields = String::new();
    for i in 0..MEMBERS {
        db.add_record(
            &member_name(i),
            Box::new(AiRecord::new(f64::from(i as u32))),
        )
        .await
        .unwrap();
        if i > 0 {
            fields.push(',');
        }
        fields.push_str(&format!(
            r#""f{i:02}": {{"+type": "plain", "+channel": "{}.VAL"}}"#,
            member_name(i)
        ));
    }
    db.add_record(SCALAR_PV, Box::new(AiRecord::new(0.5)))
        .await
        .unwrap();

    // The group definition goes through the real load path (`dbLoadGroup`'s
    // file shape), so the store under test is the one the IOC binary mounts.
    let mut group_file = tempfile::NamedTempFile::new().expect("temp group file");
    write!(group_file, r#"{{ "{GROUP_PV}": {{ {fields} }} }}"#).expect("write group json");
    let mount = build_qsrv_mount(
        &db,
        None,
        &[GroupLoadRequest {
            filename: group_file.path().to_string_lossy().into_owned(),
            macros: String::new(),
        }],
    )
    .await;
    assert!(mount.enabled, "QSRV2 must be enabled by default");

    // The binary's exact composition: `qsrvSingle` at 0, `qsrvGroup` at 1.
    let composite = CompositeSource::new();
    composite
        .add_source("qsrvSingle", Arc::new(PvDatabaseSource::new(db.clone())), 0)
        .expect("mount the single-record source");
    composite
        .add_source("qsrvGroup", mount.store.clone(), 1)
        .expect("mount the group source");

    let server = Arc::new(
        BlockingPvaServer::bind(
            SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            composite,
            PvaServerConfig::isolated(),
        )
        .expect("bind the blocking PVA server"),
    );
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, server.tcp_port()));
    let srv = server.clone();
    let serve_thread = std::thread::Builder::new()
        .name("PVAS-TCP-test".into())
        .spawn(move || srv.serve())
        .expect("spawn the accept thread");

    // ── Two subscriptions on two circuits (the target evidence shape) ───
    let big_updates = Arc::new(AtomicU64::new(0));
    let v0_updates = Arc::new(AtomicU64::new(0));

    let bu = big_updates.clone();
    let c_big = client(addr);
    let h_big = tokio::spawn(async move {
        let _ = c_big
            .pvmonitor(GROUP_PV, move |_value| {
                bu.fetch_add(1, Ordering::SeqCst);
            })
            .await;
    });
    let vu = v0_updates.clone();
    let c_v0 = client(addr);
    let h_v0 = tokio::spawn(async move {
        let _ = c_v0
            .pvmonitor(SCALAR_PV, move |_value| {
                vu.fetch_add(1, Ordering::SeqCst);
            })
            .await;
    });

    // Both connect-time snapshots must land before the flood: on the target
    // the group delivered exactly its initial snapshot and nothing more, so
    // the discriminating phase is what happens AFTER this point.
    let big = big_updates.clone();
    wait_for(
        move || big.load(Ordering::SeqCst) >= 1,
        Duration::from_secs(10),
        "the group monitor's connect-time snapshot over the blocking driver",
    )
    .await;
    let v0 = v0_updates.clone();
    wait_for(
        move || v0.load(Ordering::SeqCst) >= 1,
        Duration::from_secs(10),
        "the scalar monitor's connect-time snapshot over the blocking driver",
    )
    .await;

    // ── The flood: every member + the scalar, continuously ──────────────
    let run = Arc::new(AtomicBool::new(true));
    let poster = {
        let db = db.clone();
        let run = run.clone();
        std::thread::Builder::new()
            .name("test-poster".into())
            .spawn(move || {
                let members: Vec<_> = (0..MEMBERS)
                    .map(|i| db.get_record(&member_name(i)).expect("member exists"))
                    .collect();
                let scalar = db.get_record(SCALAR_PV).expect("scalar exists");
                while run.load(Ordering::SeqCst) {
                    for rec in &members {
                        rec.write()
                            .notify_field("VAL", EventMask::VALUE | EventMask::ALARM);
                    }
                    scalar
                        .write()
                        .notify_field("VAL", EventMask::VALUE | EventMask::ALARM);
                    // Outpaces the drain (whose unit of work is a full
                    // 20-member atomic read per event) without a hard spin.
                    std::thread::yield_now();
                }
            })
            .expect("spawn the poster thread")
    };

    let big_before = big_updates.load(Ordering::SeqCst);
    let v0_before = v0_updates.load(Ordering::SeqCst);

    // A FRESH circuit mid-flood — the target probe that timed out
    // permanently. Client dial machinery shares the callback pool with the
    // server's forwarders, so a wedged band freezes this too.
    let c_get = client(addr);
    let get_result = tokio::time::timeout(Duration::from_secs(10), c_get.pvget(SCALAR_PV)).await;

    // The §9.15 victim: the unrelated scalar monitor must keep delivering
    // WHILE the group floods. On the band-task drain it stopped dead here.
    let v0 = v0_updates.clone();
    wait_for(
        move || v0.load(Ordering::SeqCst) >= v0_before + 3,
        Duration::from_secs(10),
        "scalar monitor updates during a group flood (§9.15: it delivered 3 \
         in-flight updates and then nothing, permanently)",
    )
    .await;
    // And the group's own subscriber must see assembled updates, not just
    // its connect-time snapshot.
    let big = big_updates.clone();
    wait_for(
        move || big.load(Ordering::SeqCst) >= big_before + 3,
        Duration::from_secs(10),
        "group monitor updates during its own flood (§9.15: snapshot only)",
    )
    .await;

    run.store(false, Ordering::SeqCst);
    poster.join().expect("poster thread exits cleanly");

    get_result
        .expect("a fresh GET must complete during the flood, not time out")
        .expect("the GET succeeds");

    // ── Teardown ─────────────────────────────────────────────────────────
    h_big.abort();
    h_v0.abort();
    server.shutdown();
    serve_thread
        .join()
        .expect("accept thread exits on shutdown");
}
