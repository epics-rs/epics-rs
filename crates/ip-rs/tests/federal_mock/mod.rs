#![allow(dead_code)]

//! A Federal ERO-1071 encoder reader, as `Federal.db` reads it: each STX the
//! serial record writes gets the next reading, `<probe:02>` and eight blanks
//! (`probe` takes `INT(AA)`) then the position in `AA[10,17]` (`position`),
//! ended by CR. The readings cycle.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct Federal {
    host: String,
    requests: Arc<AtomicUsize>,
}

impl Federal {
    pub fn start(readings: &[(i32, f64)]) -> Federal {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host = listener.local_addr().unwrap().to_string();
        let requests = Arc::new(AtomicUsize::new(0));
        let replies: Vec<String> = readings
            .iter()
            .map(|(probe, pos)| format!("{probe:02}        {pos:+8.4}\r"))
            .collect();
        let count = requests.clone();
        std::thread::spawn(move || {
            for mut conn in listener.incoming().flatten() {
                let mut buf = [0u8; 256];
                while let Ok(n @ 1..) = conn.read(&mut buf) {
                    for _ in buf[..n].iter().filter(|&&b| b == 0x02) {
                        let i = count.fetch_add(1, Ordering::SeqCst);
                        let reply = &replies[i % replies.len()];
                        if conn.write_all(reply.as_bytes()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Federal { host, requests }
    }

    /// `host:port`, for `drvAsynIPPortConfigure`.
    pub fn host_info(&self) -> &str {
        &self.host
    }

    /// How many readings the program has asked for.
    pub fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}
