//! A Keithley meter with a scanner card, as the ip templates drive it:
//! LF-terminated commands, and the model's read command answered with
//! `<chan>.<mode>`, the relay last closed and the index into the model's
//! `dmm_modes` last configured, so each reading names both. `*rst` and
//! `rout:open:all` (or `route:open:all`) open every relay. A test can hold
//! one reading back, and with it every command behind it.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Condvar, Mutex};

pub struct Keithley {
    host: String,
    commands: Arc<Mutex<Vec<String>>>,
    /// How many more readings go out before one is held; `None` holds none.
    hold: Arc<(Mutex<Option<usize>>, Condvar)>,
}

impl Keithley {
    /// A meter whose `conf_*` commands are `modes`, in `dmm_modes` order,
    /// and which answers `read`.
    pub fn start(modes: &'static [&'static str], read: &'static str) -> Keithley {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host = listener.local_addr().unwrap().to_string();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let log = commands.clone();
        let hold = Arc::new((Mutex::new(None), Condvar::new()));
        let gate = hold.clone();
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
                    if answer {
                        let (pass, released) = &*gate;
                        let mut pass = pass.lock().unwrap();
                        match *pass {
                            Some(0) => drop(released.wait_while(pass, |p| p.is_some()).unwrap()),
                            Some(n) => *pass = Some(n - 1),
                            None => {}
                        }
                    }
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
        Keithley {
            host,
            commands,
            hold,
        }
    }

    /// Answer the next `pass` readings, then hold the one after them until
    /// [`release_read`](Self::release_read).
    pub fn hold_read(&self, pass: usize) {
        *self.hold.0.lock().unwrap() = Some(pass);
    }

    pub fn release_read(&self) {
        *self.hold.0.lock().unwrap() = None;
        self.hold.1.notify_all();
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
