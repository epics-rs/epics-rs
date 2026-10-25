//! A Keithley meter with a scanner card, as the ip templates drive it:
//! LF-terminated commands, and the model's read command answered with
//! `<chan>.<mode>`, the relay last closed and the index into the model's
//! `dmm_modes` last configured, so each reading names both. `*rst` and
//! `rout:open:all` (or `route:open:all`) open every relay.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

pub struct Keithley {
    host: String,
    commands: Arc<Mutex<Vec<String>>>,
}

impl Keithley {
    /// A meter whose `conf_*` commands are `modes`, in `dmm_modes` order,
    /// and which answers `read`.
    pub fn start(modes: &'static [&'static str], read: &'static str) -> Keithley {
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
                    } else if cmd == "rout:open:all" || cmd == "route:open:all" {
                        chan = 0;
                    } else if let Some(n) = cmd.strip_prefix("rout:clos (@") {
                        chan = n.trim_end_matches(')').parse().unwrap();
                    } else if let Some(m) = modes.iter().position(|m| *m == cmd) {
                        mode = m;
                    }
                    let answer = cmd == read;
                    log.lock().unwrap().push(cmd);
                    if answer
                        && reply
                            .write_all(format!("{chan}.{mode}\n").as_bytes())
                            .is_err()
                    {
                        break;
                    }
                }
            }
        });
        Keithley { host, commands }
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
