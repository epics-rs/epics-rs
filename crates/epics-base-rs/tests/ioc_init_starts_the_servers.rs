//! **The `iocInit` line must leave the servers running, so the lines after it
//! run against an IOC that is on its port.**
//!
//! C `iocInit()` is `iocBuild() || iocRun()` (`iocInit.c:112`): `iocBuild`
//! binds the sockets in `dbInitServers()` (`:222`) and `iocRun` starts serving
//! in `dbRunServers()` (`:265-267`), which reaches RSRV's `rsrv_run`
//! (`caservertask.c:766-771`) and RETURNS. softMain then goes on to its
//! interactive `iocsh` (`softMain.cpp:250`) with the IOC already listening.
//!
//! This port awaited the protocol runner as the last phase of
//! `IocApplication::run`, so the servers could not exist until the WHOLE
//! startup script had finished. Measured before this change with
//! `softioc-rs st.cmd`, script `dbLoadRecords` / `iocInit` / `casr` /
//! `ss -ltn 'sport = :45064'`: no listening socket, where C `softIoc` on the
//! same script has one.
//!
//! What is proved is the ordering, not a race that happened to win: the probe
//! line asks for the bound address WITHOUT waiting for it, so it can only see
//! one if `iocInit` had already returned from `dbRunServers()`. That is what
//! `db_server::announce_serving` and `BuiltIoc::run`'s await of it buy — a
//! runner that binds after the script has moved on would fail this test rather
//! than pass it slowly.

use std::io::Read;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use epics_base_rs::server::ioc_app::IocApplication;
use epics_base_rs::server::iocsh::registry::{ArgValue, CommandDef, CommandOutcome};

/// The address the test's protocol runner is listening on, published the
/// moment it is bound. `None` still means "the runner has not got there".
///
/// Read without waiting, deliberately. A `Condvar` here would let a runner
/// that binds late still pass — the reader would simply block until it did,
/// which is the spawn-and-hope this file exists to rule out.
#[derive(Default)]
struct Bound(Mutex<Option<SocketAddr>>);

impl Bound {
    fn publish(&self, addr: SocketAddr) {
        *self.0.lock().unwrap() = Some(addr);
    }

    fn peek(&self) -> Option<SocketAddr> {
        *self.0.lock().unwrap()
    }
}

/// What the script line after `iocInit` saw.
#[derive(Debug, PartialEq, Eq)]
enum Probe {
    /// The runner never started while the script was running.
    NoServer,
    /// Its port was reachable from the script line.
    Connected,
    Refused(String),
}

fn probe(bound: Arc<Bound>, into: Arc<Mutex<Vec<Probe>>>) -> CommandDef {
    CommandDef::new(
        "probeServerPort",
        vec![],
        "probeServerPort - connect to the protocol runner's port from this line",
        move |_args: &[ArgValue], _ctx: &_| {
            let seen = match bound.peek() {
                None => Probe::NoServer,
                Some(addr) => match TcpStream::connect(addr) {
                    // Reading to EOF is what lets the runner's single `accept`
                    // return before this command does, so the line after this
                    // one meets a runner that has already finished.
                    Ok(mut sock) => {
                        let _ = sock.read(&mut [0u8; 1]);
                        Probe::Connected
                    }
                    Err(e) => Probe::Refused(e.to_string()),
                },
            };
            into.lock().unwrap().push(seen);
            Ok(CommandOutcome::Continue)
        },
    )
}

fn st_cmd(name: &str, lines: &[&str]) -> String {
    let path = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::write(&path, format!("{}\n", lines.join("\n"))).expect("write st.cmd");
    path.to_string_lossy().into_owned()
}

/// The transition, seen from the script line that follows it.
///
/// The runner here is the smallest thing that is still a server: it binds,
/// says so, and accepts once. `run` returns when that accept has been served,
/// which is the runner completing naturally — the `select!` arm that
/// propagates its result.
#[epics_macros_rs::epics_test]
async fn a_script_line_after_ioc_init_reaches_the_running_server() {
    let bound = Arc::new(Bound::default());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let runner_bound = bound.clone();

    IocApplication::new()
        .port(0)
        .register_startup_command(probe(bound.clone(), seen.clone()))
        .startup_script(&st_cmd(
            "ioc_init_starts_servers.cmd",
            &["on error break", "iocInit", "probeServerPort"],
        ))
        .run(move |_config| async move {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind the test server");
            runner_bound.publish(listener.local_addr().unwrap());
            // What a real protocol server's serve entry does at this point —
            // `CaServer::run` and the PVA bind callback both announce here.
            // Without it `iocInit` would have nothing to return on and would
            // wait for this runner to FINISH, which is a deadlock: the accept
            // below is served by the script line that cannot run yet.
            epics_base_rs::server::db_server::announce_serving();
            epics_base_rs::runtime::task::spawn_blocking(move || {
                let _ = listener.accept();
            })
            .await
            .expect("the test server's accept thread");
            Ok(())
        })
        .await
        .expect("the script runs to its end and the runner finishes");

    assert_eq!(
        seen.lock().unwrap().as_slice(),
        [Probe::Connected],
        "the line after `iocInit` must find the servers up without waiting for \
         them — C's `iocRun` has called `dbRunServers()` and returned by then"
    );

    // And the runner is gone with the `run` that owned it: `ProtocolServer` is
    // the only handle, and every way out of `run_to_completion` joins or
    // aborts it.
    let addr = bound.peek().expect("the bound address");
    assert!(
        TcpStream::connect(addr).is_err(),
        "the protocol runner must not outlive the `run` that started it"
    );
}
