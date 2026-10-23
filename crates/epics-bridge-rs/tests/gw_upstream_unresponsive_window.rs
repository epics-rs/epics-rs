//! The CA gateway's access hook after an unresponsive upstream circuit,
//! against softIoc. Its own binary, not `upstream.rs`'s tests: it spawns an
//! IOC, and tools/ioc-spawn-gate charges a spawn under `src/` to every binary
//! that links the library. Tokio backend only: `UpstreamManager`,
//! `CacheMode` and `resolve_event_mask` do not exist on the exec backend,
//! where the gateway's reactor-bound CA server is compiled out (build.rs).
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
        // No deadline: the IOC answers, so `ensure_subscribed` waits on the
        // connect itself rather than racing the 1 s policy clock on a loaded
        // host. `bound` below is the hang guard.
        connect_timeout: Duration::MAX,
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
    /// What the proxy has carried, for the waits that replace timing.
    wire: Arc<std::sync::Mutex<Wire>>,
    /// Connections the proxy has accepted: a disconnect is redialled, an
    /// unresponsive circuit is not.
    accepted: Arc<std::sync::atomic::AtomicUsize>,
    proxy: tokio::task::JoinHandle<()>,
}

/// The CA frames the proxy has read, from either side, frozen or not.
#[derive(Default)]
struct Wire {
    /// Subscription ids of the gateway's `EVENT_ADD`s.
    subscribed: std::collections::BTreeSet<u32>,
    /// Subscription ids the IOC has sent an event for.
    evented: std::collections::BTreeSet<u32>,
    /// Events the IOC has sent.
    events: usize,
    /// The bits of the IOC's last `ACCESS_RIGHTS`.
    rights: Option<u32>,
}

impl Wire {
    const EVENT_ADD: u16 = 1;
    const ACCESS_RIGHTS: u16 = 22;

    /// Note every whole frame at the head of `acc` and drop it, leaving a
    /// partial one for the next read.
    fn note(&mut self, acc: &mut Vec<u8>, from_ioc: bool) {
        fn be16(b: &[u8], at: usize) -> u16 {
            u16::from_be_bytes([b[at], b[at + 1]])
        }
        fn be32(b: &[u8], at: usize) -> u32 {
            u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
        }
        while acc.len() >= 16 {
            // `caHdrLargeArray`: postsize 0xffff with count 0 means the
            // real sizes follow the header.
            let (head, body) = if be16(acc, 2) == 0xffff && be16(acc, 6) == 0 {
                if acc.len() < 24 {
                    return;
                }
                (24, be32(acc, 16) as usize)
            } else {
                (16, be16(acc, 2) as usize)
            };
            if acc.len() < head + body {
                return;
            }
            let p2 = be32(acc, 12);
            match (from_ioc, be16(acc, 0)) {
                (false, Self::EVENT_ADD) => {
                    self.subscribed.insert(p2);
                }
                (true, Self::EVENT_ADD) => {
                    self.evented.insert(p2);
                    self.events += 1;
                }
                (true, Self::ACCESS_RIGHTS) => self.rights = Some(p2),
                _ => {}
            }
            acc.drain(..head + body);
        }
    }
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
        let wire = Arc::new(std::sync::Mutex::new(Wire::default()));
        let seen = wire.clone();
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
                tokio::spawn(Self::pipe(dr, uw, frozen_rx.clone(), seen.clone(), false));
                tokio::spawn(Self::pipe(ur, dw, frozen_rx.clone(), seen.clone(), true));
            }
        });
        let rig = Self {
            ioc,
            said,
            port,
            frozen,
            wire,
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

    /// Forward `from` to `to`, holding what it reads while frozen; every
    /// frame is noted as it is read, so a held one is already seen.
    async fn pipe(
        mut from: tokio::net::tcp::OwnedReadHalf,
        mut to: tokio::net::tcp::OwnedWriteHalf,
        mut frozen: tokio::sync::watch::Receiver<bool>,
        wire: Arc<std::sync::Mutex<Wire>>,
        from_ioc: bool,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut buf = vec![0u8; 16 * 1024];
        let mut acc = Vec::new();
        loop {
            let n = match from.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            acc.extend_from_slice(&buf[..n]);
            wire.lock().unwrap().note(&mut acc, from_ioc);
            if frozen.wait_for(|f| !*f).await.is_err() || to.write_all(&buf[..n]).await.is_err() {
                return;
            }
        }
    }

    /// Both subscriptions a cached-mode connect makes, value and
    /// `DBE_PROPERTY`, are on the wire and have had their first event.
    fn quiet(&self) -> bool {
        let wire = self.wire.lock().unwrap();
        wire.subscribed.len() == 2 && wire.subscribed.is_subset(&wire.evented)
    }

    /// The `(read, write)` of the IOC's last `ACCESS_RIGHTS`.
    fn rights(&self) -> Option<(bool, bool)> {
        let bits = self.wire.lock().unwrap().rights?;
        Some((bits & 1 != 0, bits & 2 != 0))
    }

    fn subscriptions(&self) -> usize {
        self.wire.lock().unwrap().subscribed.len()
    }

    fn events(&self) -> usize {
        self.wire.lock().unwrap().events
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
/// appearance. The window opens once nothing from before it is in flight
/// and closes once the IOC's `ACCESS_RIGHTS` for `after` waits in the
/// proxy: caput returns before rsrv sends it. `held`, when given, is put
/// to the PV right after the freeze, and its event waits in the proxy
/// across the death.
async fn after_an_unresponsive_window(
    acf: &str,
    before: (bool, bool),
    held: Option<&str>,
    during: impl FnOnce(&StallRig),
    after: (bool, bool),
) -> Back {
    let bound = Duration::from_secs(20);
    let name = "GW:stall";
    let rig = StallRig::start(
        "record(ao, \"GW:stall\") {}\nrecord(ao, \"GW:gate\") { field(VAL, \"1\") }\n",
        acf,
    )
    .await;
    let db = Arc::new(PvDatabase::new());
    let mgr = manager(db.clone()).await;
    tokio::time::timeout(
        bound,
        mgr.ensure_subscribed(name.as_bytes(), name.as_bytes(), None, 0),
    )
    .await
    .expect("ensure_subscribed returns")
    .unwrap_or_else(|e| panic!("subscribe through the proxy: {e}\n{}", rig.console()));
    let rights = |hook: &epics_base_rs::server::pv::AccessHook| {
        let r = hook(b"alice", b"host1");
        (r.read, r.write)
    };
    let hook = db
        .find_pv(name)
        .expect("shadow PV")
        .access_hook()
        .expect("access hook");
    until(bound, "the connect's rights never reached the hook", || {
        std::future::ready(rights(&hook) == before)
    })
    .await;
    until(bound, "a subscription never had its first event", || {
        std::future::ready(rig.quiet())
    })
    .await;

    let dialled = rig.accepted();
    let subscribed = rig.subscriptions();
    rig.freeze(true);
    if let Some(value) = held {
        let sent = rig.events();
        rig.caput(name, value);
        until(
            bound,
            "the held put's event never reached the proxy",
            || std::future::ready(rig.events() > sent),
        )
        .await;
    }
    until(
        bound,
        "the window never took the shadow PV down",
        || async { db.find_pv(name).is_none() },
    )
    .await;
    during(&rig);
    until(bound, "the IOC never sent the window's rights", || {
        std::future::ready(rig.rights() == Some(after))
    })
    .await;
    rig.freeze(false);
    let deadline = std::time::Instant::now() + bound;
    // No sleep: the first appearance is what is asserted, and a PV that
    // shows stale rights for a moment must not be missed between polls.
    let back = loop {
        if let Some(pv) = db.find_pv(name) {
            break pv;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the shadow PV never came back:\n{}",
            rig.console()
        );
        tokio::task::yield_now().await;
    };
    let hook = back.access_hook().expect("access hook");
    let first = rights(&hook);
    // The PV is back at the reconnect, pending; a GET is answered with the
    // first event of its new monitors (gateVc.cc:1323-1326).
    let value = tokio::time::timeout(bound, back.read_snapshot())
        .await
        .expect("the revived PV never took a first event")
        .expect("the revived PV died again")
        .value
        .to_f64()
        .expect("numeric value");
    // A disconnect would have cleared the rights and passed for that reason.
    assert_eq!(
        rig.accepted(),
        dialled,
        "the window was a disconnect: the gateway redialled"
    );
    let back = Back {
        rights: first,
        value,
        resubscribed: rig.subscriptions() > subscribed,
        console: rig.console(),
    };
    mgr.shutdown().await;
    back
}

/// What the gateway serves when the PV first reappears after the window.
struct Back {
    rights: (bool, bool),
    value: f64,
    /// The gateway sent a new `EVENT_ADD` after the freeze.
    resubscribed: bool,
    console: String,
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns the EPICS base softIoc from $EPICS_BASE; runs in --profile interop"]
async fn a_write_right_is_held_through_an_unresponsive_window() {
    let granted = (true, true);
    let acf = "ASG(DEFAULT) {\n RULE(1, READ)\n RULE(1, WRITE)\n}\n";
    let back = after_an_unresponsive_window(acf, granted, None, |_| {}, granted).await;
    assert_eq!(back.rights, granted, "{}", back.console);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns the EPICS base softIoc from $EPICS_BASE; runs in --profile interop"]
async fn a_read_only_right_is_held_through_an_unresponsive_window() {
    let read_only = (true, false);
    let acf = "ASG(DEFAULT) {\n RULE(1, READ)\n}\n";
    let back = after_an_unresponsive_window(acf, read_only, None, |_| {}, read_only).await;
    assert_eq!(back.rights, read_only, "{}", back.console);
}

/// WRITE follows `GW:gate` through `INPA`; the gate drops inside the
/// window, and rsrv's ACCESS_RIGHTS waits in the frozen proxy until the
/// circuit recovers, ahead of anything that revives the PV.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns the EPICS base softIoc from $EPICS_BASE; runs in --profile interop"]
async fn a_rights_change_inside_an_unresponsive_window_is_applied_after_it() {
    let acf = "ASG(DEFAULT) {\n INPA(\"GW:gate\")\n RULE(1, READ)\n \
               RULE(1, WRITE) {\n  CALC(\"A=1\")\n }\n}\n";
    let (granted, read_only) = ((true, true), (true, false));
    let back = after_an_unresponsive_window(
        acf,
        granted,
        None,
        |rig| rig.caput("GW:gate", "0"),
        read_only,
    )
    .await;
    assert_eq!(back.rights, read_only, "{}", back.console);
}

/// An event rsrv sent just before the window waits in the proxy across the
/// death, and the value changes again inside it. C's `death` unmonitors
/// (gatePv.cc:658-661), so the held event reaches a cancelled subscription
/// and the PV comes back with the value it has after the window.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns the EPICS base softIoc from $EPICS_BASE; runs in --profile interop"]
async fn an_event_held_across_an_unresponsive_window_does_not_revive_the_pv() {
    let granted = (true, true);
    let acf = "ASG(DEFAULT) {\n RULE(1, READ)\n RULE(1, WRITE)\n}\n";
    let back = after_an_unresponsive_window(
        acf,
        granted,
        Some("5"),
        |rig| rig.caput("GW:stall", "7"),
        granted,
    )
    .await;
    assert!(
        back.resubscribed,
        "the death left the subscription installed"
    );
    assert_eq!(back.value, 7.0, "{}", back.console);
}
