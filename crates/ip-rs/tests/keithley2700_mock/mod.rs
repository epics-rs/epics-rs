//! A Keithley 2700 with a scanner card, as `Keithley2kDMM_mf40.db` drives it:
//! LF-terminated commands, and `read?` answered with `<chan>.<mode>`, the
//! relay last closed and the `dmm_modes` index last configured, so each
//! reading names both. `*rst` and `rout:open:all` open every relay.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

/// The `conf_*` records' commands, in `dmm_modes` order.
const MODES: [&str; 10] = [
    ":conf:volt:dc",
    ":conf:volt:ac",
    ":conf:res",
    ":conf:fres",
    ":conf:freq",
    "conf:temp;:temp:tc:type j",
    ":conf:temp;:temp:tc:type k",
    ":conf:temp;:temp:tc:type t",
    ":conf:curr:dc",
    ":conf:curr:ac",
];

pub struct Keithley2700 {
    host: String,
    commands: Arc<Mutex<Vec<String>>>,
}

impl Keithley2700 {
    pub fn start() -> Keithley2700 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host = listener.local_addr().unwrap().to_string();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let log = commands.clone();
        std::thread::spawn(move || {
            let (mut chan, mut mode) = (0, 0);
            for conn in listener.incoming().flatten() {
                let mut reply = conn.try_clone().unwrap();
                for line in BufReader::new(conn).split(b'\n').map_while(Result::ok) {
                    let cmd = String::from_utf8(line).unwrap();
                    if cmd == "*rst" {
                        (chan, mode) = (0, 0);
                    } else if cmd == "rout:open:all" {
                        chan = 0;
                    } else if let Some(n) = cmd.strip_prefix("rout:clos (@") {
                        chan = n.trim_end_matches(')').parse().unwrap();
                    } else if let Some(m) = MODES.iter().position(|m| *m == cmd) {
                        mode = m;
                    }
                    let read = cmd == "read?";
                    log.lock().unwrap().push(cmd);
                    if read
                        && reply
                            .write_all(format!("{chan}.{mode}\n").as_bytes())
                            .is_err()
                    {
                        break;
                    }
                }
            }
        });
        Keithley2700 { host, commands }
    }

    /// `host:port`, for `drvAsynIPPortConfigure`.
    pub fn host_info(&self) -> &str {
        &self.host
    }

    /// How many commands have arrived since the last [`take`](Self::take).
    pub fn pending(&self) -> usize {
        self.commands.lock().unwrap().len()
    }

    /// The commands since the last call, in arrival order.
    pub fn take(&self) -> Vec<String> {
        std::mem::take(&mut self.commands.lock().unwrap())
    }
}
