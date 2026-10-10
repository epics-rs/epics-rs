//! `dbChannelCreate`'s errlog line on an unknown filter (`dbChannel.c:179`),
//! as softIoc R7.0.10 prints it on this database:
//!
//! ```text
//! $ caget -w 1 'Y.VAL{"bad":{}}'      # softIoc stderr, one line per CREATE_CHAN
//! dbChannelCreate: Channel filter 'bad' not found
//!
//! epics> dbpf Z.OUT 'Y.VAL{"bad":{}}'   # Z: ao, Soft Channel; dbPutFieldLink
//! dbChannelCreate: Channel filter 'bad' not found
//! DBF_STRING:         "Y.VAL{\"bad\":{}} NPP NMS"
//!
//! epics> dbpf X.INP 'Y.VAL{"bad":{}}'   # X: ai, Async Soft Channel
//! dbChannelCreate: Channel filter 'bad' not found
//! dbChannelCreate: Channel filter 'bad' not found
//! recGblRecordError: devAiSoftCallback (add_record) link target not found Process Variable Not Found PV: X
//! DBF_STRING:         "Y.VAL{\"bad\":{}} NPP NMS"
//! ```

#![cfg(tokio_backend)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const FILTER_LINE: &str = "dbChannelCreate: Channel filter 'bad' not found";

fn port_free_for_udp_and_tcp() -> u16 {
    loop {
        let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = tcp.local_addr().unwrap().port();
        if std::net::UdpSocket::bind(("0.0.0.0", port)).is_ok() {
            return port;
        }
    }
}

struct Ioc {
    child: Child,
    stdin: ChildStdin,
    lines: mpsc::Receiver<String>,
    port: u16,
}

impl Ioc {
    /// softioc-rs on `db`, returned once it printed `iocRun`.
    fn start(db: &str) -> Ioc {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.db");
        std::fs::write(&path, db).expect("write db");
        let port = port_free_for_udp_and_tcp();
        let mut child = Command::new(env!("CARGO_BIN_EXE_softioc-rs"))
            .arg("-d")
            .arg(&path)
            .env("EPICS_CA_SERVER_PORT", port.to_string())
            .env("EPICS_CA_ADDR_LIST", "127.0.0.1")
            .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
            .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
            .env("EPICS_CAS_BEACON_ADDR_LIST", "127.0.0.1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn softioc-rs");
        let stdin = child.stdin.take().unwrap();
        let (tx, lines) = mpsc::channel();
        for out in [
            Box::new(child.stdout.take().unwrap()) as Box<dyn std::io::Read + Send>,
            Box::new(child.stderr.take().unwrap()),
        ] {
            let tx = tx.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(out).lines().map_while(Result::ok) {
                    let _ = tx.send(line);
                }
            });
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let line = lines
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("softioc-rs never printed iocRun");
            if line.contains("iocRun: All initialization complete") {
                break;
            }
        }
        // The database file must outlive nothing past the load.
        drop(dir);
        Ioc {
            child,
            stdin,
            lines,
            port,
        }
    }

    /// Run one shell command and return what it printed up to the line
    /// containing `until`.
    fn command(&mut self, cmd: &str, until: &str) -> Vec<String> {
        writeln!(self.stdin, "{cmd}").expect("write command");
        let mut heard = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while let Ok(line) = self
            .lines
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            let done = line.contains(until);
            heard.push(line);
            if done {
                return heard;
            }
        }
        panic!("`{cmd}` never printed {until:?}; heard:\n{heard:#?}");
    }

    /// Everything printed until the shell exits.
    fn finish(mut self) -> Vec<String> {
        let _ = writeln!(self.stdin, "exit");
        let mut heard = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while let Ok(line) = self
            .lines
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            heard.push(line);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        heard
    }
}

fn filter_lines(heard: &[String]) -> usize {
    heard
        .iter()
        .filter(|l| l.trim_start_matches(|c| c != 'd').starts_with(FILTER_LINE))
        .count()
}

#[tokio::test]
async fn ca_create_with_an_unknown_filter_prints_the_line_once() {
    let ioc = Ioc::start("record(ai, \"Y\") { field(VAL, \"2\") }\n");
    // SAFETY: the only test in this binary that sets the environment, before
    // `CaClient::new` snapshots it.
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{}", ioc.port));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
    }
    let client = epics_ca_rs::client::CaClient::new()
        .await
        .expect("CA client");
    let ch = client.create_channel(r#"Y.VAL{"bad":{}}"#);
    assert!(
        ch.wait_connected(Duration::from_secs(1)).await.is_err(),
        "a channel with an unknown filter connected"
    );
    drop(ch);
    drop(client);
    let heard = ioc.finish();
    assert_eq!(filter_lines(&heard), 1, "heard:\n{heard:#?}");
}

#[test]
fn a_link_put_with_an_unknown_filter_prints_the_line_before_its_echo() {
    let mut ioc = Ioc::start(
        "record(ai, \"Y\") { field(VAL, \"2\") }\n\
         record(ao, \"Z\") { field(DTYP, \"Soft Channel\") field(OUT, \"Y\") }\n",
    );
    let heard = ioc.command(r#"dbpf Z.OUT 'Y.VAL{"bad":{}}'"#, "DBF_STRING");
    ioc.finish();
    assert_eq!(filter_lines(&heard), 1, "heard:\n{heard:#?}");
}

/// The first line is `dbPutFieldLink`'s lookup, the second `add_record`'s
/// (`dbAccess.c:1107`, `:1196`).
#[test]
fn an_async_inp_put_with_an_unknown_filter_prints_two_lines_and_the_refusal() {
    let mut ioc = Ioc::start(
        "record(ai, \"Y\") { field(VAL, \"2\") }\n\
         record(ai, \"X\") { field(DTYP, \"Async Soft Channel\") field(INP, \"Y\") }\n",
    );
    let heard = ioc.command(r#"dbpf X.INP 'Y.VAL{"bad":{}}'"#, "DBF_STRING");
    ioc.finish();
    let refusal = heard.iter().position(|l| {
        l.contains(
            "recGblRecordError: devAiSoftCallback (add_record) link target not found \
             Process Variable Not Found PV: X",
        )
    });
    let echo = heard.iter().position(|l| l.contains("DBF_STRING"));
    assert_eq!(filter_lines(&heard), 2, "heard:\n{heard:#?}");
    assert!(refusal.is_some() && refusal < echo, "heard:\n{heard:#?}");
    let last_filter = heard.iter().rposition(|l| l.contains(FILTER_LINE)).unwrap();
    assert!(Some(last_filter) < refusal, "heard:\n{heard:#?}");
}
