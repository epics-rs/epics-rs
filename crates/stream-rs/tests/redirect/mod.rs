//! An IOC of stream records whose protocols redirect formats to other
//! fields (`%(record.FIELD)`, StreamEpics.cc), on a bus that keeps every
//! write and answers a request with the reply set for it: from a thread of
//! its own on bus `R`, inline, on the requesting thread, on bus `RI`. Bus
//! `RA` is `R` whose asynchronous (I/O Intr) reads are fed from a thread.

#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;
use stream_rs::bus::{self, Client, ReadInput, StreamBusInterface, StreamIoStatus};

/// Every write the bus took, its terminator cut off.
pub static WRITES: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// The reply to each request; a request with none gets no reply.
pub static REPLIES: Mutex<Vec<(&str, &str)>> = Mutex::new(Vec::new());
/// Replies taken by the first request they answer, before [`REPLIES`].
pub static REPLIES_ONCE: Mutex<Vec<(&str, &str)>> = Mutex::new(Vec::new());
/// Bus `RI`'s asynchronous (I/O Intr) reads: each answered by the next
/// reply set for its record, and left waiting once they run out.
pub static ASYNC_REPLIES: Mutex<VecDeque<(&str, &str)>> = Mutex::new(VecDeque::new());
/// How many more asynchronous reads bus `RA` answers, each with its count
/// `n` as "n n"; once none are left they wait.
pub static ASYNC_FEED: AtomicUsize = AtomicUsize::new(0);
/// The asynchronous reads bus `RA` has answered.
pub static ASYNC_FED: AtomicUsize = AtomicUsize::new(0);
/// Each record's stream, as its bus was given it.
pub static CLIENTS: Mutex<Vec<(String, Weak<dyn Client>)>> = Mutex::new(Vec::new());

struct Dev {
    client: Weak<dyn Client>,
    last: Mutex<String>,
    inline: bool,
    feed: bool,
}

impl Dev {
    fn answer(&self, f: impl FnOnce(&dyn Client) + Send + 'static) -> bool {
        let client = self.client.clone();
        let run = move || {
            if let Some(client) = client.upgrade() {
                f(&*client);
            }
        };
        if self.inline {
            run();
        } else {
            std::thread::spawn(run);
        }
        true
    }

    fn reply(&self, reply: Option<String>) -> bool {
        self.answer(move |c| {
            match reply {
                Some(r) => {
                    c.read_callback(StreamIoStatus::StreamIoEnd, ReadInput::Bytes(r.as_bytes()))
                }
                None => c.read_callback(StreamIoStatus::StreamIoNoReply, ReadInput::Bytes(b"")),
            };
        })
    }
}

impl StreamBusInterface for Dev {
    fn lock_request(&self, _: u64) -> bool {
        self.answer(|c| c.lock_callback(StreamIoStatus::StreamIoSuccess))
    }
    fn unlock(&self) -> bool {
        true
    }
    fn write_request(&self, output: &[u8], _: u64) -> bool {
        let out = String::from_utf8_lossy(output).trim_end().to_string();
        WRITES.lock().push(out.clone());
        *self.last.lock() = out;
        self.answer(|c| c.write_callback(StreamIoStatus::StreamIoSuccess))
    }
    fn read_request(&self, _: u64, _: u64, _: isize, async_: bool) -> bool {
        if async_ && self.inline {
            let Some(record) = self.client.upgrade().map(|c| c.name()) else {
                return false;
            };
            let mut replies = ASYNC_REPLIES.lock();
            let Some(at) = replies.iter().position(|(r, _)| *r == record) else {
                return true;
            };
            let (_, reply) = replies.remove(at).unwrap();
            drop(replies);
            return self.reply(Some(format!("{reply}\n")));
        }
        if async_ && self.feed {
            let left =
                ASYNC_FEED.try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1));
            if left.is_err() {
                return true;
            }
            let n = ASYNC_FED.fetch_add(1, Ordering::AcqRel) + 1;
            return self.reply(Some(format!("{n} {n}\n")));
        }
        let last = std::mem::take(&mut *self.last.lock());
        let once = {
            let mut once = REPLIES_ONCE.lock();
            let at = once.iter().position(|(q, _)| *q == last);
            at.map(|at| once.remove(at).1)
        };
        let reply = once
            .or_else(|| {
                REPLIES
                    .lock()
                    .iter()
                    .find(|(q, _)| *q == last)
                    .map(|(_, r)| *r)
            })
            .map(|r| format!("{r}\n"));
        self.reply(reply)
    }
    fn supports_async_read(&self) -> bool {
        self.inline || self.feed
    }
}

fn dev(
    client: Weak<dyn Client>,
    busname: &str,
    _: i32,
    _: &str,
) -> Option<Box<dyn StreamBusInterface>> {
    let (inline, feed) = match busname {
        "R" => (false, false),
        "RI" => (true, false),
        "RA" => (false, true),
        _ => return None,
    };
    if let Some(c) = client.upgrade() {
        CLIENTS.lock().push((c.name(), client.clone()));
    }
    Some(Box::new(Dev {
        client,
        last: Mutex::new(String::new()),
        inline,
        feed,
    }))
}

/// An IOC running `db_text`, its protocols from `tests/redirect`.
pub async fn ioc(db_text: &str) -> Arc<PvDatabase> {
    bus::register_stream_bus_interface("Redirect", dev);
    stream_rs::error::set_stream_msg_time_stamped(0);
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/redirect");
    // SAFETY: set before any thread of this test binary reads it.
    unsafe { std::env::set_var("STREAM_PROTOCOL_PATH", dir) };
    let ioc = stream_rs::dev::register(IocBuilder::new())
        .db_string(db_text, &HashMap::new())
        .unwrap()
        .build_ioc()
        .await
        .unwrap();
    let db = ioc.database().clone();
    assert!(ioc.run().await);
    db
}

/// Process `record` and wait for its protocol to have written `n` more
/// lines (or two seconds), giving back what it wrote.
pub async fn process(db: &PvDatabase, record: &str, n: usize) -> Vec<String> {
    let before = WRITES.lock().len();
    db.put_record_field_from_ca_no_notify(record, "PROC", EpicsValue::Long(1))
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while WRITES.lock().len() < before + n && Instant::now() < deadline {
        epics_base_rs::runtime::task::sleep(Duration::from_millis(10)).await;
    }
    // let a reply land
    epics_base_rs::runtime::task::sleep(Duration::from_millis(200)).await;
    WRITES.lock()[before..].to_vec()
}

/// `record`'s stream.
pub fn client(record: &str) -> Arc<dyn Client> {
    let clients = CLIENTS.lock();
    let (_, client) = clients.iter().find(|(name, _)| name == record).unwrap();
    client.upgrade().unwrap()
}

pub fn get(db: &PvDatabase, pv: &str) -> EpicsValue {
    db.get_pv(pv).unwrap_or_else(|e| panic!("{pv}: {e}"))
}
