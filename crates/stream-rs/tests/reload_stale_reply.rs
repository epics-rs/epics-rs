//! A reply read for a run of protocol A that `streamReload` aborts must not
//! reach the next run, of the reloaded protocol B, on a can-block asyn
//! port: the port thread waits for the record with the chunk while the
//! reload and a PROC take it. Every reply lands at once, so a nonzero
//! sx.VAL (B's "%d" against B's reply "x") is a chunk of A's reply "k 7".

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use asyn_rs::interfaces::InterfaceMask;
use asyn_rs::manager::{PortManager, claim_port_driver};
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::request::RequestOp;
use asyn_rs::user::AsynUser;
use epics_base_rs::runtime::task::test_block_on;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::EpicsValue;

#[derive(Default)]
struct Wire {
    input: VecDeque<(Instant, Vec<u8>)>,
    seq: u64,
    za: u64,
    zb: u64,
}

struct Device {
    base: PortDriverBase,
    wire: Arc<Mutex<Wire>>,
}

impl Device {
    /// Up to `buf.len()` bytes of the landed input, or None at the deadline.
    fn take(&self, user: &AsynUser, buf: &mut [u8]) -> Option<usize> {
        let deadline = Instant::now() + user.timeout.unwrap_or(Duration::from_secs(1));
        loop {
            let now = Instant::now();
            {
                let mut wire = self.wire.lock().unwrap();
                if let Some((at, bytes)) = wire.input.front_mut()
                    && *at <= now
                {
                    let end = bytes.len().min(buf.len());
                    buf[..end].copy_from_slice(&bytes[..end]);
                    bytes.drain(..end);
                    if bytes.is_empty() {
                        wire.input.pop_front();
                    }
                    return Some(end);
                }
            }
            if now >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_micros(200));
        }
    }
}

fn timeout_error() -> asyn_rs::error::AsynError {
    asyn_rs::error::AsynError::Status {
        status: asyn_rs::error::AsynStatus::Timeout,
        message: "no input".into(),
    }
}

impl PortDriver for Device {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }
    fn connect(&mut self, user: &AsynUser) -> asyn_rs::error::AsynResult<()> {
        self.base_mut().set_addr_connected(user.addr, true);
        Ok(())
    }
    fn disconnect(&mut self, user: &AsynUser) -> asyn_rs::error::AsynResult<()> {
        self.base_mut().set_addr_connected(user.addr, false);
        Ok(())
    }
    fn io_read_octet(
        &mut self,
        user: &AsynUser,
        buf: &mut [u8],
    ) -> asyn_rs::interpose::OctetReadResult {
        use asyn_rs::interpose::{EomReason, OctetReadResult};
        match self.take(user, buf) {
            Some(n) => OctetReadResult::ok(n, EomReason::empty()),
            None => OctetReadResult {
                status: Err(timeout_error()),
                ..OctetReadResult::ok(0, EomReason::empty())
            },
        }
    }
    fn io_write_octet(
        &mut self,
        _: &mut AsynUser,
        data: &[u8],
    ) -> asyn_rs::error::AsynResult<usize> {
        let mut wire = self.wire.lock().unwrap();
        let at = Instant::now();
        match String::from_utf8_lossy(data).trim_end() {
            "ZA" => {
                wire.za += 1;
                wire.seq += 1;
                let k = wire.seq;
                wire.input.push_back((at, format!("{k} 7").into_bytes()));
            }
            "ZB" => {
                wire.zb += 1;
                wire.input.push_back((at, b"x".to_vec()));
            }
            _ => {}
        }
        Ok(data.len())
    }
}

fn proto(dir: &Path, body: &str) {
    let tmp = dir.join("v.proto.tmp");
    std::fs::write(
        &tmp,
        format!("ReadTimeout = 5;\nReplyTimeout = 50;\nExtraInput = Ignore;\n{body}\n"),
    )
    .unwrap();
    std::fs::rename(&tmp, dir.join("v.proto")).unwrap();
}

#[test]
fn a_reply_of_a_run_the_reload_aborted_does_not_reach_the_next_protocol() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("reload_stale_reply");
    std::fs::create_dir_all(&dir).unwrap();
    let a = "p { out \"ZA\"; in \"%*d %*d\"; }";
    let b = "p { out \"ZB\"; in \"%d\"; }";
    proto(&dir, a);
    // SAFETY: set before any thread of this test binary reads it.
    unsafe { std::env::set_var("STREAM_PROTOCOL_PATH", &dir) };
    stream_rs::asyn::register();
    let wire = Arc::new(Mutex::new(Wire::default()));
    let flags = PortFlags {
        can_block: true,
        ..PortFlags::default()
    };
    let base = PortDriverBase::new(
        "VPORT",
        1,
        flags,
        InterfaceMask::OCTET_TRANSPORT,
        InterfaceMask::OCTET,
    );
    let manager = PortManager::new();
    manager
        .register_port(
            claim_port_driver("VPORT").unwrap(),
            Device {
                base,
                wire: wire.clone(),
            },
            &asyn_rs::registry::open_owner(),
        )
        .unwrap();
    manager
        .find_port_handle("VPORT")
        .unwrap()
        .submit_blocking(RequestOp::Connect, AsynUser::new(0))
        .unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    test_block_on(async {
        let ioc = stream_rs::dev::register(IocBuilder::new())
            .db_string(
                "record(longin, \"sx\") { field(DTYP, \"stream\") field(INP, \"@v.proto p VPORT\") }\n",
                &HashMap::new(),
            )
            .unwrap()
            .build_ioc()
            .await
            .unwrap();
        let db = ioc.database().clone();
        assert!(ioc.run().await);
        let putter = {
            let (db, stop) = (db.clone(), stop.clone());
            std::thread::spawn(move || {
                test_block_on(async {
                    while !stop.load(Ordering::Acquire) {
                        let _ = db
                            .put_record_field_from_ca_no_notify("sx", "PROC", EpicsValue::Long(1))
                            .await;
                    }
                });
            })
        };
        let mut hit = None;
        for round in 0..200 {
            proto(&dir, if round % 2 == 0 { b } else { a });
            let mut out = Vec::new();
            assert_eq!(stream_rs::epics::stream_reload(Some("sx"), &mut out), 0);
            let v = db.get_pv("sx").unwrap().to_f64().unwrap();
            if v != 0.0 {
                hit = Some((round, v));
                break;
            }
            epics_base_rs::runtime::task::sleep(Duration::from_millis(10)).await;
        }
        stop.store(true, Ordering::Release);
        putter.join().unwrap();
        let w = wire.lock().unwrap();
        assert!(
            w.za > 0 && w.zb > 0,
            "both protocols ran: {} {}",
            w.za,
            w.zb
        );
        assert_eq!(hit, None, "a chunk of an aborted A run reached B's input");
    });
}
