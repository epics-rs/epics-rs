//! An Oxford quad X-ray BPM on a TCP socket, for `sncqxbpm.st` behind a
//! `drvAsynIPPort` asyn record. The C oracle IOC runs against this same
//! file, so a case's expected values are C's.
//!
//! A command ends at `\n` or after 30 ms of quiet. `:READ<n>:CURRALL?` is
//! answered `<prefix><a> <b> <c> <d>\n`, the line `sncqxbpm.st` scans from
//! its second character on (`s_ainp+1`, then `"%u%u%u%u"`). Every other
//! command gets no reply, as the program reads none.

#![allow(dead_code)]

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct State {
    raw: [u32; 4],
    prefix: String,
    /// The whole `CURRALL?` reply line, in place of the raw counts.
    reply: Option<String>,
    silent: bool,
    /// How long a `CURRALL?` waits before its reply.
    delay: Duration,
    seen: Vec<String>,
}

#[derive(Clone)]
pub struct Qxbpm {
    state: Arc<Mutex<State>>,
    port: u16,
}

impl Qxbpm {
    /// Raw counts 0, prefix `>`, listening on a free local port.
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(State {
            raw: [0; 4],
            prefix: ">".to_string(),
            reply: None,
            silent: false,
            delay: Duration::ZERO,
            seen: Vec::new(),
        }));
        let bpm = Qxbpm { state, port };
        let served = bpm.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let served = served.clone();
                std::thread::spawn(move || served.serve(stream));
            }
        });
        bpm
    }

    /// `host:port` for `drvAsynIPPortConfigure`.
    pub fn host_info(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    /// The counts of diodes A, B, C and D.
    pub fn set_raw(&self, raw: [u32; 4]) {
        self.state.lock().unwrap().raw = raw;
    }

    /// The text before the counts.
    pub fn set_prefix(&self, prefix: &str) {
        self.state.lock().unwrap().prefix = prefix.to_string();
    }

    /// Answer `CURRALL?` with `line` (`None`: the counts again).
    pub fn set_reply(&self, line: Option<&str>) {
        self.state.lock().unwrap().reply = line.map(str::to_string);
    }

    /// Stop answering (`true`), or answer again.
    pub fn set_silent(&self, silent: bool) {
        self.state.lock().unwrap().silent = silent;
    }

    /// Hold each `CURRALL?` reply back by `delay`.
    pub fn set_delay(&self, delay: Duration) {
        self.state.lock().unwrap().delay = delay;
    }

    /// Every command received so far, without its `\n`.
    pub fn seen(&self) -> Vec<String> {
        self.state.lock().unwrap().seen.clone()
    }

    pub fn clear_seen(&self) {
        self.state.lock().unwrap().seen.clear();
    }

    fn serve(&self, stream: TcpStream) {
        let mut out = stream.try_clone().unwrap();
        let mut input = stream;
        input
            .set_read_timeout(Some(Duration::from_millis(30)))
            .unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 256];
        loop {
            let quiet = match input.read(&mut chunk) {
                Ok(0) => return,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    false
                }
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => true,
                Err(_) => return,
            };
            loop {
                let end = match buf.iter().position(|&b| b == b'\n') {
                    Some(i) => i,
                    None if quiet && !buf.is_empty() => buf.len(),
                    None => break,
                };
                let cmd: Vec<u8> = buf.drain(..end).collect();
                if buf.first() == Some(&b'\n') {
                    buf.remove(0);
                }
                let cmd = String::from_utf8_lossy(&cmd).to_string();
                if let Some((reply, delay)) = self.answer(&cmd) {
                    std::thread::sleep(delay);
                    if out.write_all(format!("{reply}\n").as_bytes()).is_err() {
                        return;
                    }
                }
            }
        }
    }

    fn answer(&self, cmd: &str) -> Option<(String, Duration)> {
        let mut st = self.state.lock().unwrap();
        st.seen.push(cmd.to_string());
        if st.silent || !(cmd.starts_with(":READ") && cmd.ends_with(":CURRALL?")) {
            return None;
        }
        let line = match &st.reply {
            Some(reply) => reply.clone(),
            None => {
                let [a, b, c, d] = st.raw;
                format!("{}{a} {b} {c} {d}", st.prefix)
            }
        };
        Some((line, st.delay))
    }
}
