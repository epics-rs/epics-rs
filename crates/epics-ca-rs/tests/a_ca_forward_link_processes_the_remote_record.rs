//! A forward link over CA, between two IOCs. softIoc R7.0.10 with `tgt.db`
//! (`X`, `Y`, `Z`, `W`, `W2`: `calc` `VAL+1`) on one server and `src.db` on
//! another,
//!
//!     record(fanout, "FO") { field(LNK1, "X.PROC CA") field(LNK2, "Z CA")
//!                            field(LNK3, "W.PROC") field(LNK4, "W2") }
//!     record(calc, "FL") { field(CALC, "VAL+1") field(FLNK, "Y.PROC CA") }
//!
//! after `dbpf FO.PROC 1; dbpf FL.PROC 1` three times, reads X 3, Y 3, Z 0,
//! W 3, W2 0 and both sources NO_ALARM, iocInit having warned `FO.LNK2 => Z`
//! then `FO.LNK4 => W2`: `dbInitLink` makes every non-local link field CA
//! (`dbLink.c:118-130`), and dbCa's `scanForward` puts 1 only through a link
//! whose field is PROC (`dbLink.c:131-136`, `dbCa.c:643-648`). A runtime
//! `dbpf FO2.LNK1 "R2.PROC"` on an empty fanout is made CA by `dbAddLink`
//! (`dbLink.c:180-191`), without a warning, and `dbpf FR.FLNK "R1.PROC CA"`
//! on a calc opens its channel there (`dbCaAddLink`): two seconds later,
//! three processes of each source read R2 3 and R1 3.

#![cfg(tokio_backend)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// A port number nothing on the box holds for UDP or for TCP.
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
    /// What it printed up to `iocRun: All initialization complete`.
    boot: Vec<String>,
}

impl Ioc {
    fn start(db: &std::path::Path, port: u16, addr_list: &str) -> Ioc {
        let mut child = Command::new(env!("CARGO_BIN_EXE_softioc-rs"))
            .arg("-d")
            .arg(db)
            .env("EPICS_CA_SERVER_PORT", port.to_string())
            .env("EPICS_CA_ADDR_LIST", addr_list)
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
        let mut boot = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(30);
        while let Ok(line) = lines.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            let up = line.contains("iocRun: All initialization complete");
            boot.push(line);
            if up {
                return Ioc {
                    child,
                    stdin,
                    lines,
                    boot,
                };
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        panic!("softioc-rs on {db:?} never came up: {boot:#?}")
    }

    /// Lines up to the first one `done` accepts, or `None` at the deadline.
    fn until(&mut self, done: impl Fn(&str) -> bool) -> Option<String> {
        let deadline = Instant::now() + Duration::from_secs(30);
        while let Ok(line) = self
            .lines
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            if done(&line) {
                return Some(line);
            }
        }
        None
    }

    fn cmd(&mut self, line: &str) {
        writeln!(self.stdin, "{line}").unwrap();
    }

    /// `dbpf`, with its echo consumed so a later `dbgf` reads its own line.
    fn dbpf(&mut self, pv: &str, value: &str) {
        self.cmd(&format!("dbpf {pv} {value}"));
        assert!(self.until(|l| l.contains("DBF_")).is_some(), "dbpf {pv}");
    }

    /// The `dbgf` value of `pv`, after everything printed before it.
    fn dbgf(&mut self, pv: &str) -> String {
        self.cmd(&format!("dbgf {pv}"));
        let line = self
            .until(|l| l.contains("DBF_"))
            .unwrap_or_else(|| panic!("no dbgf reply for {pv}"));
        let value = line.split_once(':').unwrap().1;
        value.trim().trim_matches('"').trim().to_string()
    }
}

impl Drop for Ioc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn ca_forward_links_process_the_remote_record_once_per_source_process() {
    let dir = tempfile::tempdir().expect("tempdir");
    let tgt = dir.path().join("tgt.db");
    let src = dir.path().join("src.db");
    let calc = |n: &str| format!("record(calc, \"{n}\") {{ field(CALC, \"VAL+1\") }}\n");
    let tgt_db: String = ["X", "Y", "Z", "W", "W2", "R1", "R2"].map(calc).concat();
    std::fs::write(&tgt, tgt_db).unwrap();
    std::fs::write(
        &src,
        "record(fanout, \"FO\") { field(LNK1, \"X.PROC CA\") field(LNK2, \"Z CA\") \
         field(LNK3, \"W.PROC\") field(LNK4, \"W2\") }\n\
         record(calc, \"FL\") { field(CALC, \"VAL+1\") field(FLNK, \"Y.PROC CA\") }\n\
         record(fanout, \"FO2\") {}\n\
         record(calc, \"FR\") { field(CALC, \"VAL+1\") }\n",
    )
    .unwrap();

    let a_port = port_free_for_udp_and_tcp();
    let mut a = Ioc::start(&tgt, a_port, "127.0.0.1");
    let b_port = loop {
        let p = port_free_for_udp_and_tcp();
        if p != a_port {
            break p;
        }
    };
    let mut b = Ioc::start(&src, b_port, &format!("127.0.0.1:{a_port}"));
    // softIoc's iocInit, for LNK2 and LNK4; ERL_WARNING is uncoloured on a
    // pipe.
    let warning = "WARNING: Forward-link uses Channel Access without pointing to PROC field";
    let warned: Vec<&str> = b
        .boot
        .windows(2)
        .filter(|w| w[0] == warning)
        .map(|w| w[1].as_str())
        .collect();
    assert_eq!(
        warned,
        ["    FO.LNK2 => Z", "    FO.LNK4 => W2"],
        "{:#?}",
        b.boot
    );
    assert!(!a.boot.iter().any(|l| l.contains("Forward-link")));

    // dbPutFieldLink → dbAddLink: R2 is not in this IOC, so LNK1 is CA, and
    // dbCaAddLink opens both channels now; softIoc's first forward a moment
    // later already lands.
    b.dbpf("FO2.LNK1", "\"R2.PROC\"");
    b.dbpf("FR.FLNK", "\"R1.PROC CA\"");
    std::thread::sleep(Duration::from_secs(2));
    for _ in 0..3 {
        b.dbpf("FO2.PROC", "1");
        b.dbpf("FR.PROC", "1");
        std::thread::sleep(Duration::from_millis(300));
    }

    // softIoc's CA links connect while iocInit finishes; the port's open on
    // first use, so the first source process must not race the connect.
    // Process until the targets answer, then count three more from there.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        b.dbpf("FO.PROC", "1");
        b.dbpf("FL.PROC", "1");
        std::thread::sleep(Duration::from_millis(300));
        if ["X", "Y", "W"].iter().all(|r| a.dbgf(r) != "0") {
            break;
        }
        assert!(Instant::now() < deadline, "CA forward links never fired");
    }
    let (x0, y0): (f64, f64) = (a.dbgf("X").parse().unwrap(), a.dbgf("Y").parse().unwrap());
    let w0: f64 = a.dbgf("W").parse().unwrap();
    for _ in 0..3 {
        b.dbpf("FO.PROC", "1");
        b.dbpf("FL.PROC", "1");
        std::thread::sleep(Duration::from_millis(300));
    }
    std::thread::sleep(Duration::from_millis(500));
    let x: f64 = a.dbgf("X").parse().unwrap();
    let y: f64 = a.dbgf("Y").parse().unwrap();
    assert_eq!(
        x - x0,
        3.0,
        "fanout LNK1 \"X.PROC CA\": X processes once per FO"
    );
    assert_eq!(y - y0, 3.0, "FLNK \"Y.PROC CA\": Y processes once per FL");
    assert_eq!(a.dbgf("Z"), "0", "LNK2 \"Z CA\" is not a PROC link");
    let w: f64 = a.dbgf("W").parse().unwrap();
    assert_eq!(w - w0, 3.0, "fanout LNK3 \"W.PROC\", made CA at iocInit");
    assert_eq!(a.dbgf("W2"), "0", "LNK4 \"W2\" is not a PROC link");
    assert_eq!(b.dbgf("FO.SEVR"), "NO_ALARM");
    assert_eq!(b.dbgf("FL.SEVR"), "NO_ALARM");
    assert_eq!(
        a.dbgf("R2"),
        "3",
        "runtime FO2.LNK1 \"R2.PROC\", made CA by the put"
    );
    assert_eq!(
        a.dbgf("R1"),
        "3",
        "runtime FR.FLNK \"R1.PROC CA\", open from the put"
    );
    assert_eq!(b.dbgf("FO2.SEVR"), "NO_ALARM");
    assert_eq!(b.dbgf("FR.SEVR"), "NO_ALARM");
    b.cmd("exit");
    a.cmd("exit");
}
