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
#![cfg(feature = "client-core")]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const FILTER_LINE: &str = "dbChannelCreate: Channel filter 'bad' not found";

use named_port::verdict as softioc_verdict;

struct Ioc {
    child: Child,
    stdin: ChildStdin,
    lines: mpsc::Receiver<String>,
    port: u16,
    /// Every line read so far, the evidence a lost port is judged on.
    console: Vec<String>,
}

impl Drop for Ioc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Ioc {
    /// softioc-rs on `db` and `port`, returned once it printed `iocRun`.
    fn start(db: &str, port: u16) -> Ioc {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.db");
        std::fs::write(&path, db).expect("write db");
        // stdout and stderr share one pipe, as on C's console, so the order
        // lines are read in is the order they were written: errlog's line
        // is on stderr and the dbpf echo on stdout, and `errlogVprintf`
        // flushes the iocsh thread's line before returning (`errlog.c:186`).
        let (out, out_w) = std::io::pipe().expect("pipe");
        let mut child = Command::new(env!("CARGO_BIN_EXE_softioc-rs"))
            .arg("-d")
            .arg(&path)
            .env("EPICS_CA_SERVER_PORT", port.to_string())
            .env("EPICS_CA_ADDR_LIST", "127.0.0.1")
            .env("EPICS_CA_AUTO_ADDR_LIST", "NO")
            .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
            .stdin(Stdio::piped())
            .stdout(out_w.try_clone().expect("clone pipe"))
            .stderr(out_w)
            .spawn()
            .expect("spawn softioc-rs");
        let stdin = child.stdin.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        });
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut console = Vec::new();
        loop {
            let line = lines
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("softioc-rs never printed iocRun");
            let up = line.contains("iocRun: All initialization complete");
            console.push(line);
            if up {
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
            console,
        }
    }

    /// softioc-rs binds its CA server after `iocRun`, so its report of a
    /// lost port can arrive at any point of a run.
    fn lost_port(&self) -> bool {
        softioc_verdict::rs_ioc_port_was_taken(&self.console.join("\n"), self.port)
    }

    /// Run one shell command and return what it printed up to the line
    /// containing `until`, or `None` when the IOC lost its port meanwhile.
    fn command(&mut self, cmd: &str, until: &str) -> Option<Vec<String>> {
        let mut heard = Vec::new();
        if writeln!(self.stdin, "{cmd}").is_ok() {
            let deadline = Instant::now() + Duration::from_secs(10);
            while let Ok(line) = self
                .lines
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                let done = line.contains(until);
                self.console.push(line.clone());
                heard.push(line);
                if done {
                    return Some(heard);
                }
            }
        }
        // Whatever is still queued is evidence too.
        self.console.extend(self.lines.try_iter());
        if self.lost_port() {
            return None;
        }
        panic!("`{cmd}` never printed {until:?}; heard:\n{heard:#?}");
    }

    /// Everything printed until the shell exits, or `None` when the IOC lost
    /// its port.
    fn finish(mut self) -> Option<Vec<String>> {
        let _ = writeln!(self.stdin, "exit");
        let mut heard = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while let Ok(line) = self
            .lines
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            self.console.push(line.clone());
            heard.push(line);
        }
        (!self.lost_port()).then_some(heard)
    }
}

fn filter_lines(heard: &[String]) -> usize {
    heard
        .iter()
        .filter(|l| l.trim_start_matches(|c| c != 'd').starts_with(FILTER_LINE))
        .count()
}

#[test]
fn ca_create_with_an_unknown_filter_prints_the_line_once() {
    let heard = named_port::on_a_named_port(|port| {
        let ioc = Ioc::start("record(ai, \"Y\") { field(VAL, \"2\") }\n", port);
        // SAFETY: the only test in this binary that sets the environment,
        // before `CaClient::new` snapshots it.
        unsafe {
            std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
            std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        }
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async {
            let client = epics_ca_rs::client::CaClient::new()
                .await
                .expect("CA client");
            let ch = client.create_channel(r#"Y.VAL{"bad":{}}"#);
            assert!(
                ch.wait_connected(Duration::from_secs(1)).await.is_err(),
                "a channel with an unknown filter connected"
            );
        });
        drop(rt);
        ioc.finish()
    });
    assert_eq!(filter_lines(&heard), 1, "heard:\n{heard:#?}");
}

#[test]
fn a_link_put_with_an_unknown_filter_prints_the_line_before_its_echo() {
    let heard = named_port::on_a_named_port(|port| {
        let mut ioc = Ioc::start(
            "record(ai, \"Y\") { field(VAL, \"2\") }\n\
             record(ao, \"Z\") { field(DTYP, \"Soft Channel\") field(OUT, \"Y\") }\n",
            port,
        );
        let heard = ioc.command(r#"dbpf Z.OUT 'Y.VAL{"bad":{}}'"#, "DBF_STRING")?;
        ioc.finish().map(|_| heard)
    });
    assert_eq!(filter_lines(&heard), 1, "heard:\n{heard:#?}");
}

/// The first line is `dbPutFieldLink`'s lookup, the second `add_record`'s
/// (`dbAccess.c:1107`, `:1196`).
#[test]
fn an_async_inp_put_with_an_unknown_filter_prints_two_lines_and_the_refusal() {
    let heard = named_port::on_a_named_port(|port| {
        let mut ioc = Ioc::start(
            "record(ai, \"Y\") { field(VAL, \"2\") }\n\
             record(ai, \"X\") { field(DTYP, \"Async Soft Channel\") field(INP, \"Y\") }\n",
            port,
        );
        let heard = ioc.command(r#"dbpf X.INP 'Y.VAL{"bad":{}}'"#, "DBF_STRING")?;
        ioc.finish().map(|_| heard)
    });
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
