//! An XIA HSC-1 chain on a TCP socket, for `xiahsc.st` and `xia_slit.st`
//! behind a `drvAsynIPPort` asyn record. The C oracle IOC runs against this
//! same file, so a case's expected values are C's.
//!
//! A command is `!<id> <cmd>` ended by `\r`, by the `!` of the next command,
//! or by 30 ms of quiet: C's asyn record writes `xiahsc.st`'s commands with no
//! `\r`, because the `getEos` readback of its queued IEOS special overwrites
//! the OEOS the program has just put. `<id>` `ALL` addresses every
//! module in order. A reply is `%<id> ...` ended by `\r\n`: `OK;` to `K`, `M`,
//! `W` and `0 I`, `<a> <b> DONE;` (or `BUSY;` while a move polls busy) to
//! `P`, `OK <value>` to `R <n>`, and `ERROR; 1` to anything else. An
//! unknown id gets nothing. A module set to [`Module::echo`] answers `P`
//! with `P <a> <b> DONE;` and `P BUSY;`, the form `xiahsc.st` scans
//! (`"%*s %*s %d %d"`, then `"%*s %*s %s"` for `BUSY;`).

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Copy)]
pub struct Module {
    pub a: i32,
    pub b: i32,
    /// `R 1`.
    pub outer: i32,
    /// `R 2`.
    pub origin: i32,
    /// `R 5`.
    pub step_delay: i32,
    /// `R 6`.
    pub backlash: i32,
    /// `R 7`.
    pub csw: i32,
    /// `P` inquiries answered `BUSY;` after the next `M`.
    pub busy_polls: u32,
    busy_left: u32,
    /// `P` replies repeat the command letter after the id.
    pub echo: bool,
}

impl Module {
    /// The controller defaults `xiahsc.st` assumes until it has read them,
    /// with both blades at the origin.
    pub fn new() -> Self {
        Module {
            a: 400,
            b: 400,
            outer: 4400,
            origin: 400,
            step_delay: 0,
            backlash: 0,
            csw: 0,
            busy_polls: 0,
            busy_left: 0,
            echo: false,
        }
    }
}

#[derive(Default)]
struct State {
    modules: Vec<(String, Module)>,
    /// Exact command text → the reply sent in place of the module's.
    replies: HashMap<String, String>,
    seen: Vec<String>,
    silent: bool,
    link: Option<TcpStream>,
    /// Unasked lines sent before a client connected.
    held: Vec<String>,
}

#[derive(Clone)]
pub struct Hsc1 {
    state: Arc<Mutex<State>>,
    port: u16,
}

impl Hsc1 {
    /// Modules `ids`, each [`Module::new`], listening on a free local port.
    pub fn start(ids: &[&str]) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(State {
            modules: ids
                .iter()
                .map(|id| (id.to_string(), Module::new()))
                .collect(),
            ..State::default()
        }));
        let hsc = Hsc1 { state, port };
        let served = hsc.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let served = served.clone();
                std::thread::spawn(move || served.serve(stream));
            }
        });
        hsc
    }

    /// `host:port` for `drvAsynIPPortConfigure`.
    pub fn host_info(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    pub fn set(&self, id: &str, f: impl FnOnce(&mut Module)) {
        let mut st = self.state.lock().unwrap();
        let m = st.modules.iter_mut().find(|(i, _)| i == id).expect(id);
        f(&mut m.1);
    }

    pub fn module(&self, id: &str) -> Module {
        let st = self.state.lock().unwrap();
        st.modules.iter().find(|(i, _)| i == id).expect(id).1
    }

    /// Answer `cmd` (`!H-1 R 2`) with `reply` instead of the module's reply.
    pub fn reply_to(&self, cmd: &str, reply: &str) {
        let mut st = self.state.lock().unwrap();
        st.replies.insert(cmd.to_string(), reply.to_string());
    }

    /// Stop answering (`true`), or answer again.
    pub fn set_silent(&self, silent: bool) {
        self.state.lock().unwrap().silent = silent;
    }

    /// Every command received so far, without its `\r`.
    pub fn seen(&self) -> Vec<String> {
        self.state.lock().unwrap().seen.clone()
    }

    /// Send `line` unasked.
    pub fn send(&self, line: &str) {
        let mut st = self.state.lock().unwrap();
        match st.link.as_mut() {
            Some(link) => {
                let _ = link.write_all(format!("{line}\r\n").as_bytes());
            }
            None => st.held.push(line.to_string()),
        }
    }

    fn serve(&self, stream: TcpStream) {
        let mut out = stream.try_clone().unwrap();
        {
            let mut st = self.state.lock().unwrap();
            for line in std::mem::take(&mut st.held) {
                let _ = out.write_all(format!("{line}\r\n").as_bytes());
            }
            st.link = Some(stream.try_clone().unwrap());
        }
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
            while let Some(end) = command_end(&buf, quiet) {
                let cmd: Vec<u8> = buf.drain(..end).collect();
                if buf.first() == Some(&b'\r') {
                    buf.remove(0);
                }
                let cmd = String::from_utf8_lossy(&cmd)
                    .trim_start_matches('\n')
                    .to_string();
                for reply in self.answer(&cmd) {
                    if out.write_all(format!("{reply}\r\n").as_bytes()).is_err() {
                        return;
                    }
                }
            }
        }
    }

    fn answer(&self, cmd: &str) -> Vec<String> {
        let mut st = self.state.lock().unwrap();
        st.seen.push(cmd.to_string());
        if st.silent {
            return Vec::new();
        }
        if let Some(reply) = st.replies.get(cmd) {
            return vec![reply.clone()];
        }
        let Some((id, op)) = cmd.strip_prefix('!').and_then(|c| c.split_once(' ')) else {
            return Vec::new();
        };
        let words: Vec<&str> = op.split_whitespace().collect();
        let mut out = Vec::new();
        for (mid, m) in st.modules.iter_mut() {
            if id != "ALL" && id != mid {
                continue;
            }
            let p = if m.echo { "P " } else { "" };
            let reply = match words.as_slice() {
                ["K"] => {
                    m.busy_left = 0;
                    "OK;".to_string()
                }
                ["P"] if m.busy_left > 0 => {
                    m.busy_left -= 1;
                    format!("{p}BUSY;")
                }
                ["P"] => format!("{p}{} {} DONE;", m.a, m.b),
                ["M", a, b] => match (a.parse(), b.parse()) {
                    (Ok(a), Ok(b)) => {
                        (m.a, m.b) = (a, b);
                        m.busy_left = m.busy_polls;
                        "OK;".to_string()
                    }
                    _ => "ERROR; 8".to_string(),
                },
                ["R", n] => match *n {
                    "1" => format!("OK {}", m.outer),
                    "2" => format!("OK {}", m.origin),
                    "3" => format!("OK {}", m.a),
                    "4" => format!("OK {}", m.b),
                    "5" => format!("OK {}", m.step_delay),
                    "6" => format!("OK {}", m.backlash),
                    "7" => format!("OK {}", m.csw),
                    _ => "ERROR; 5".to_string(),
                },
                ["W", n, v] => match (*n, v.parse()) {
                    ("1", Ok(v)) => {
                        m.outer = v;
                        "OK;".to_string()
                    }
                    ("2", Ok(v)) => {
                        m.origin = v;
                        "OK;".to_string()
                    }
                    _ => "ERROR; 5".to_string(),
                },
                ["0", "I"] => {
                    (m.a, m.b) = (m.origin, m.origin);
                    "OK;".to_string()
                }
                _ => "ERROR; 1".to_string(),
            };
            out.push(format!("%{mid} {reply}"));
        }
        out
    }
}

/// The end of the first command in `buf`: its `\r`, the next command's `!`,
/// or, once the line is `quiet`, the end of what arrived.
fn command_end(buf: &[u8], quiet: bool) -> Option<usize> {
    let end = (0..buf.len()).find(|&i| buf[i] == b'\r' || (i > 0 && buf[i] == b'!'));
    end.or((quiet && !buf.is_empty()).then_some(buf.len()))
}
