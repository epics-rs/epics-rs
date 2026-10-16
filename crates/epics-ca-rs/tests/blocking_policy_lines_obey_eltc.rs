//! The blocking driver's own policy lines — the rate limit, the per-client
//! channel cap and the per-PV subscriber cap, none of which C has — are errlog
//! lines beside C's `log_header`, so `eltc 0` silences them on a console that
//! `runtime::log::install_console_subscriber` renders, as realtime-ca-ioc's
//! does. A `tracing` line there reaches the console whatever `eltc` says.
//!
//! Each test is its own process under nextest, which is what lets it set the
//! policy's environment before the server reads it.

use std::io::{Read as _, Write as _};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use epics_base_rs::runtime::log;
use epics_base_rs::runtime::task::block_on_sync;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::types::EpicsValue;
use epics_ca_rs::protocol::{CA_MINOR_VERSION, CA_PROTO_ECHO, CA_PROTO_VERSION, CaHeader};
use raw_ca::*;

/// The console this process renders to, as realtime-ca-ioc's is, but a file,
/// with `eltc 0` in force.
fn quiet_console() -> &'static PathBuf {
    static CONSOLE: OnceLock<PathBuf> = OnceLock::new();
    CONSOLE.get_or_init(|| {
        let path = std::env::temp_dir().join(format!(
            "blocking_policy_lines_obey_eltc-{}.console",
            std::process::id()
        ));
        log::install_console_subscriber();
        log::errlog_set_console(std::fs::File::create(&path).expect("create the console"));
        log::eltc(false);
        path
    })
}

/// What `send` provokes: the errlog lines naming `peer`, and every console
/// line written meanwhile, which `eltc 0` leaves empty.
fn lines_for(peer: SocketAddr, send: impl FnOnce()) -> (Vec<String>, Vec<String>) {
    let console = quiet_console();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let listener =
        log::errlog_add_listener(move |line| sink.lock().unwrap().push(line.to_string()));
    let before = std::fs::metadata(console).expect("the console").len() as usize;
    send();
    log::errlog_flush();
    log::errlog_remove_listener(listener);
    let peer = peer.to_string();
    let errlog = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|l| l.contains(&peer))
        .cloned()
        .collect();
    let console = std::fs::read_to_string(console).expect("read the console")[before..]
        .lines()
        .map(str::to_owned)
        .collect();
    (errlog, console)
}

fn serve(
    pvs: &[&str],
) -> (
    Arc<epics_ca_rs::server::blocking::BlockingCaServer>,
    SocketAddr,
) {
    let db = Arc::new(PvDatabase::new());
    for name in pvs {
        block_on_sync(db.add_pv(name, EpicsValue::Double(0.0)))
            .expect("no async runtime on this test thread")
            .expect("add_pv");
    }
    let (server, addr, _accept) = start_server(db);
    (server, addr)
}

fn set_env(name: &str, value: &str) {
    // SAFETY: before this process starts any thread that reads the
    // environment; each test is its own process.
    unsafe { std::env::set_var(name, value) };
}

#[test]
fn the_rate_limit_disconnect_is_an_errlog_line() {
    set_env("EPICS_CAS_RATE_LIMIT_MSGS_PER_SEC", "1");
    set_env("EPICS_CAS_RATE_LIMIT_BURST", "8");
    set_env("EPICS_CAS_RATE_LIMIT_STRIKES", "3");
    quiet_console();
    let (_server, addr) = serve(&[]);
    let mut c = std::net::TcpStream::connect(addr).expect("connect");
    c.set_read_timeout(Some(fact_budget::FACT_BUDGET)).unwrap();
    let peer = c.local_addr().unwrap();
    let echo = CaHeader::new(CA_PROTO_ECHO).to_bytes();
    let (errlog, console) = lines_for(peer, || {
        // Eight tokens for VERSION and seven ECHOs, then three strikes.
        let mut version = CaHeader::new(CA_PROTO_VERSION);
        version.count = CA_MINOR_VERSION;
        let mut frames = version.to_bytes().to_vec();
        frames.extend(echo.repeat(17));
        c.write_all(&frames).unwrap();
        let mut replies = Vec::new();
        c.read_to_end(&mut replies)
            .expect("the strikes close the circuit");
    });
    assert_eq!(console, Vec::<String>::new());
    assert_eq!(
        errlog,
        [format!(
            "CAS: request from {peer} => rate limit: 3 messages discarded in a row\n"
        )]
    );
}

#[test]
fn the_channel_cap_lines_are_errlog_lines() {
    set_env("EPICS_CAS_MAX_CHANNELS", "10");
    quiet_console();
    let (_server, addr) = serve(&["CAP:A"]);
    let mut c = connect_and_handshake(addr);
    let peer = c.local_addr();
    for cid in 1..=9 {
        create_channel(&mut c, cid, "CAP:A");
    }
    let (errlog, console) = lines_for(peer, || {
        // The tenth crosses 90%; the eleventh is refused and ends the circuit.
        create_channel(&mut c, 10, "CAP:A");
        c.send(&create_chan_frame(11, "CAP:A"));
        c.expect_error("ECA_ALLOCMEM");
        c.expect_closed("the cap ends the circuit");
    });
    assert_eq!(console, Vec::<String>::new());
    let contexts: Vec<&str> = errlog
        .iter()
        .filter_map(|l| l.strip_prefix(&format!("CAS: request from {peer} => ")))
        .collect();
    assert_eq!(
        contexts,
        [
            "approaching the channel limit: 9 of 10\n",
            "no memory to create new channel\n"
        ]
    );
}

#[test]
fn the_pv_subscriber_cap_has_only_its_log_header() {
    // The cap's floor (`server::pv::max_subscribers_per_pv`).
    set_env("EPICS_CAS_MAX_SUBSCRIBERS_PER_PV", "8");
    quiet_console();
    let (_server, addr) = serve(&["CAP:B"]);
    let mut c = connect_and_handshake(addr);
    let peer = c.local_addr();
    let sid = create_channel(&mut c, 1, "CAP:B");
    for sub_id in 1..=8 {
        c.send(&event_add_frame(sid, sub_id));
        c.expect(
            epics_ca_rs::protocol::CA_PROTO_EVENT_ADD,
            "an initial value",
        );
    }
    let (errlog, console) = lines_for(peer, || {
        c.send(&event_add_frame(sid, 9));
        assert_eq!(
            c.expect_error("the cap's refusal"),
            epics_ca_rs::protocol::ECA_ALLOCMEM
        );
        c.expect_closed("the cap ends the circuit");
    });
    assert_eq!(console, Vec::<String>::new());
    assert_eq!(
        errlog.first().map(String::as_str),
        Some(format!("CAS: request from {peer} => no memory to add subscription to db\n").as_str())
    );
    assert_eq!(
        errlog
            .iter()
            .filter(|l| l.starts_with("CAS: request from"))
            .count(),
        1,
        "{errlog:?}"
    );
}
