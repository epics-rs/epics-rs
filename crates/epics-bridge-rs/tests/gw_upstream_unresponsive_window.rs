//! The CA gateway's access hook after an unresponsive upstream circuit,
//! against softIoc. Its own binary, not `upstream.rs`'s tests: it spawns an
//! IOC, and tools/ioc-spawn-gate charges a spawn under `src/` to every binary
//! that links the library.
#![cfg(all(feature = "ca-gateway", tokio_backend))]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use epics_base_rs::server::database::PvDatabase;
use epics_bridge_rs::ca_gateway::upstream::{UpstreamManager, UpstreamManagerConfig};
use epics_bridge_rs::ca_gateway::{
    AccessConfig, BeaconAnomaly, CacheMode, PutLogScope, PvCache, PvList, Stats, resolve_event_mask,
};
use epics_oracle_rs::ioc::{
    BootError, BootNames, LogPolicy, OutputTail, Side, boot_on_free_port_claiming,
};
use tokio::sync::RwLock;

async fn manager(db: Arc<PvDatabase>) -> UpstreamManager {
    UpstreamManager::new(UpstreamManagerConfig {
        cache: Arc::new(RwLock::new(PvCache::new())),
        shadow_db: db,
        access: Arc::new(ArcSwap::from_pointee(AccessConfig::allow_all())),
        pvlist: Arc::new(ArcSwap::from_pointee(PvList::new())),
        putlog: None,
        putlog_scope: PutLogScope::default(),
        stats: Arc::new(Stats::new("gw:".into())),
        read_only: false,
        cache_mode: CacheMode::Cached,
        connect_timeout: Duration::from_secs(1),
        event_mask: resolve_event_mask(None),
        beacon_anomaly: Arc::new(BeaconAnomaly::new()),
        #[cfg(feature = "ca-gateway-tls")]
        upstream_tls: None,
        #[cfg(feature = "ca-gateway-tls")]
        upstream_tls_server_name: None,
    })
    .await
    .expect("manager builds")
}

/// A base binary, from the reference tree: the gate's PATH has no
/// softIoc, and a missing tree fails rather than skips.
fn base_bin(name: &str) -> std::path::PathBuf {
    use epics_base_rs::reference::{ReferenceTree, reference_path};
    let arch = std::env::var("EPICS_HOST_ARCH").unwrap_or_else(|_| "linux-x86_64".into());
    reference_path(ReferenceTree::Base, &format!("bin/{arch}/{name}"))
}

/// A softIoc on `127.0.0.2:port` reached only through a proxy on
/// `127.0.0.1:port` that can stop forwarding. Frozen, the gateway's
/// circuit goes unresponsive by echo timeout while the IOC keeps running,
/// so its rights can change inside the window. rsrv answers a TCP search
/// with "this circuit's host" and its own port (`camessage.c:2330`), so a
/// name-server search through the proxy leads back to it.
struct StallRig {
    ioc: std::process::Child,
    /// What the softIoc has said, for a failure message.
    said: OutputTail,
    port: u16,
    frozen: tokio::sync::watch::Sender<bool>,
    /// Connections the proxy has accepted: a disconnect is redialled, an
    /// unresponsive circuit is not.
    accepted: Arc<std::sync::atomic::AtomicUsize>,
    proxy: tokio::task::JoinHandle<()>,
}

impl Drop for StallRig {
    fn drop(&mut self) {
        self.proxy.abort();
        let _ = self.ioc.kill();
        let _ = self.ioc.wait();
        for var in ["EPICS_CA_NAME_SERVERS", "EPICS_CA_CONN_TMO"] {
            // SAFETY: nextest runs each test in its own process.
            unsafe { std::env::remove_var(var) };
        }
    }
}

impl StallRig {
    const IOC: std::net::Ipv4Addr = std::net::Ipv4Addr::new(127, 0, 0, 2);

    /// The proxy and the softIoc need one number. The owner's candidate is
    /// handed to the claim hook first, which binds the proxy listener on
    /// 127.0.0.1, so a lost number is replaced with the proxy already bound
    /// to the one the softIoc then gets on 127.0.0.2.
    async fn start(db: &str, acf: &str) -> Self {
        let dir = tempfile::tempdir().expect("temp");
        std::fs::write(dir.path().join("t.db"), db).expect("write db");
        std::fs::write(dir.path().join("t.acf"), acf).expect("write acf");
        let mut bound = None;
        let booted = boot_on_free_port_claiming(
            &BootNames::new("softIoc", "port"),
            LogPolicy::All,
            |port| {
                // A taken number is a lost port; any other failure is not one
                // a fresh port can fix.
                let l = std::net::TcpListener::bind(("127.0.0.1", port)).map_err(|e| {
                    let msg = format!("proxy listener on 127.0.0.1:{port}: {e}");
                    if e.kind() == std::io::ErrorKind::AddrInUse {
                        BootError::retryable(Side::C, msg)
                    } else {
                        BootError::new(Side::C, msg)
                    }
                })?;
                l.set_nonblocking(true)
                    .map_err(|e| BootError::new(Side::C, e.to_string()))?;
                bound = Some(l);
                Ok(())
            },
            |port| {
                let mut cmd = std::process::Command::new(base_bin("softIoc"));
                cmd.arg("-S")
                    .arg("-a")
                    .arg(dir.path().join("t.acf"))
                    .arg("-d")
                    .arg(dir.path().join("t.db"))
                    .envs(Self::ioc_env(port))
                    .stdin(std::process::Stdio::null());
                Ok(cmd)
            },
        )
        .unwrap_or_else(|e| panic!("softIoc: {e:?}"));
        let (ioc, port, said) = (booted.child, booted.port, booted.tail);
        let listener =
            tokio::net::TcpListener::from_std(bound.expect("claimed")).expect("proxy listener");
        let (frozen, frozen_rx) = tokio::sync::watch::channel(false);
        let upstream = SocketAddr::from((Self::IOC, port));
        let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = accepted.clone();
        let proxy = tokio::spawn(async move {
            while let Ok((down, _)) = listener.accept().await {
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let Ok(up) = tokio::net::TcpStream::connect(upstream).await else {
                    continue;
                };
                let (dr, dw) = down.into_split();
                let (ur, uw) = up.into_split();
                tokio::spawn(Self::pipe(dr, uw, frozen_rx.clone()));
                tokio::spawn(Self::pipe(ur, dw, frozen_rx.clone()));
            }
        });
        let rig = Self {
            ioc,
            said,
            port,
            frozen,
            accepted,
            proxy,
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while tokio::net::TcpStream::connect(upstream).await.is_err() {
            assert!(
                std::time::Instant::now() < deadline,
                "softIoc did not listen on {upstream}:\n{}",
                rig.console()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // SAFETY: nextest runs each test in its own process.
        unsafe {
            std::env::set_var("EPICS_CA_NAME_SERVERS", format!("127.0.0.1:{port}"));
            std::env::set_var("EPICS_CA_ADDR_LIST", "");
            std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
            std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
            // The echo watchdog arms after this much silence.
            std::env::set_var("EPICS_CA_CONN_TMO", "1");
        }
        rig
    }

    fn ioc_env(port: u16) -> Vec<(&'static str, String)> {
        let ioc = Self::IOC.to_string();
        vec![
            ("EPICS_CAS_INTF_ADDR_LIST", ioc.clone()),
            ("EPICS_CAS_BEACON_ADDR_LIST", ioc.clone()),
            ("EPICS_CAS_AUTO_BEACON_ADDR_LIST", "NO".into()),
            ("EPICS_CA_ADDR_LIST", ioc),
            ("EPICS_CA_AUTO_ADDR_LIST", "NO".into()),
            ("EPICS_CAS_SERVER_PORT", port.to_string()),
            ("EPICS_CA_SERVER_PORT", port.to_string()),
        ]
    }

    async fn pipe(
        mut from: tokio::net::tcp::OwnedReadHalf,
        mut to: tokio::net::tcp::OwnedWriteHalf,
        mut frozen: tokio::sync::watch::Receiver<bool>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            let n = match from.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            if frozen.wait_for(|f| !*f).await.is_err() || to.write_all(&buf[..n]).await.is_err() {
                return;
            }
        }
    }

    fn accepted(&self) -> usize {
        self.accepted.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn freeze(&self, on: bool) {
        self.frozen.send_replace(on);
    }

    /// `caput` straight to the IOC, past the proxy (whose name-server
    /// circuit is the frozen one).
    fn caput(&self, pv: &str, value: &str) {
        let out = std::process::Command::new(base_bin("caput"))
            .args(["-w", "3", pv, value])
            .envs(Self::ioc_env(self.port))
            .env_remove("EPICS_CA_NAME_SERVERS")
            .env_remove("EPICS_CA_CONN_TMO")
            .output()
            .expect("run caput");
        assert!(out.status.success(), "caput {pv} {value}: {out:?}");
    }

    fn console(&self) -> String {
        self.said.all_lines().join("\n")
    }
}

/// Poll `check` every 10 ms until it holds, inside `bound`.
async fn until<F: std::future::Future<Output = bool>>(
    bound: Duration,
    what: &str,
    mut check: impl FnMut() -> F,
) {
    let deadline = std::time::Instant::now() + bound;
    while !check().await {
        assert!(std::time::Instant::now() < deadline, "{what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// One unresponsive window on the gateway's upstream circuit, `during`
/// run inside it. C deletes the VC on the window's `CA_OP_CONN_DOWN`
/// (`gatePvData::death`, gatePv.cc:1315-1319) and the recovered PV takes
/// `ca_read_access`/`ca_write_access` (gatePv.cc:395-396, :1851-1852),
/// which an unresponsive circuit never clears (`nciu.cpp:161-182`). So
/// the PV the gateway serves after recovery grants `after` from its first
/// appearance. Returns that first `(read, write)` and the IOC's console.
async fn rights_after_an_unresponsive_window(
    acf: &str,
    before: (bool, bool),
    during: impl FnOnce(&StallRig),
) -> ((bool, bool), String) {
    let bound = Duration::from_secs(20);
    let name = "GW:stall";
    let rig = StallRig::start(
        "record(ao, \"GW:stall\") {}\nrecord(ao, \"GW:gate\") { field(VAL, \"1\") }\n",
        acf,
    )
    .await;
    let db = Arc::new(PvDatabase::new());
    let mgr = manager(db.clone()).await;
    tokio::time::timeout(bound, mgr.ensure_subscribed(name, name, None, 0))
        .await
        .expect("ensure_subscribed returns")
        .unwrap_or_else(|e| panic!("subscribe through the proxy: {e}\n{}", rig.console()));
    let rights = |hook: &epics_base_rs::server::pv::AccessHook| {
        let r = hook("alice", "host1");
        (r.read, r.write)
    };
    let hook = db
        .find_pv(name)
        .await
        .expect("shadow PV")
        .access_hook()
        .expect("access hook");
    until(bound, "the connect's rights never reached the hook", || {
        std::future::ready(rights(&hook) == before)
    })
    .await;

    let dialled = rig.accepted();
    rig.freeze(true);
    until(
        bound,
        "the window never took the shadow PV down",
        || async { db.find_pv(name).await.is_none() },
    )
    .await;
    during(&rig);
    rig.freeze(false);
    let deadline = std::time::Instant::now() + bound;
    let back = loop {
        if let Some(pv) = db.find_pv(name).await {
            break pv;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the shadow PV never came back:\n{}",
            rig.console()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let hook = back.access_hook().expect("access hook");
    let first = rights(&hook);
    // A disconnect would have cleared the rights and passed for that reason.
    assert_eq!(
        rig.accepted(),
        dialled,
        "the window was a disconnect: the gateway redialled"
    );
    mgr.shutdown().await;
    (first, rig.console())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_write_right_is_held_through_an_unresponsive_window() {
    let granted = (true, true);
    let acf = "ASG(DEFAULT) {\n RULE(1, READ)\n RULE(1, WRITE)\n}\n";
    let (first, console) = rights_after_an_unresponsive_window(acf, granted, |_| {}).await;
    assert_eq!(first, granted, "{console}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_only_right_is_held_through_an_unresponsive_window() {
    let read_only = (true, false);
    let acf = "ASG(DEFAULT) {\n RULE(1, READ)\n}\n";
    let (first, console) = rights_after_an_unresponsive_window(acf, read_only, |_| {}).await;
    assert_eq!(first, read_only, "{console}");
}

/// WRITE follows `GW:gate` through `INPA`; the gate drops inside the
/// window, and rsrv's ACCESS_RIGHTS waits in the frozen proxy until the
/// circuit recovers, ahead of anything that revives the PV.
#[tokio::test(flavor = "multi_thread")]
async fn a_rights_change_inside_an_unresponsive_window_is_applied_after_it() {
    let acf = "ASG(DEFAULT) {\n INPA(\"GW:gate\")\n RULE(1, READ)\n \
               RULE(1, WRITE) {\n  CALC(\"A=1\")\n }\n}\n";
    let granted = (true, true);
    let (first, console) =
        rights_after_an_unresponsive_window(acf, granted, |rig| rig.caput("GW:gate", "0")).await;
    let read_only = (true, false);
    assert_eq!(first, read_only, "{console}");
}
