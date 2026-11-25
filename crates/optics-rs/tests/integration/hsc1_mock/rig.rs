#![allow(dead_code)]

//! An HSC-1 program started on its shipped template, as the C oracle IOC
//! starts it: a `drvAsynIPPort` (`noProcessEos` 0) to the [`Hsc1`] mock, an
//! asyn record `T:asyn` on it, the template with `P=T:,HSC=hsc1:`, then
//! `seq <prog> "name=hsc1,P=T:,HSC=hsc1:,S=T:asyn"`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use asyn_rs::asyn_record::AsynRecord;
use asyn_rs::drivers::ip_port::DrvAsynIPPort;
use asyn_rs::manager::{PortManager, claim_port_driver};
use asyn_rs::registry::open_owner;
use asyn_rs::runtime::PortRuntimeHandle;
use epics_base_rs::runtime::task;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::records::busy::BusyRecord;
use epics_base_rs::server::records::transform::TransformRecord;
use epics_base_rs::server::scan::ScanOwner;
use epics_base_rs::types::EpicsValue;
use epics_seq_rs::{ProgramVars, SeqProgram, SeqThreadId, seq, seq_stop};

use super::hsc1_mock::Hsc1;

const DB: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/db/");

pub struct Rig {
    pub db: Arc<PvDatabase>,
    pub hsc: Hsc1,
    task: SeqThreadId,
    _scan: ScanOwner,
    _port: PortRuntimeHandle,
}

impl Drop for Rig {
    fn drop(&mut self) {
        seq_stop(self.task);
    }
}

/// `prog` on `template` against modules `ids`. `setup` runs on the mock
/// before the port connects; `pre` are the `dbpf`s between `iocInit` and
/// `seq`.
pub async fn start<V: ProgramVars>(
    prog: &'static SeqProgram<V>,
    template: &str,
    ids: &[&str],
    setup: impl FnOnce(&Hsc1),
    pre: &[(&str, &str)],
) -> Rig {
    static PORTS: AtomicUsize = AtomicUsize::new(0);
    let port = format!("HSC{}", PORTS.fetch_add(1, Ordering::Relaxed));
    let hsc = Hsc1::start(ids);
    setup(&hsc);
    let driver = DrvAsynIPPort::new_configured(&port, &hsc.host_info(), false, false).unwrap();
    let rt = PortManager::global()
        .register_port(claim_port_driver(&port).unwrap(), driver, &open_owner())
        .unwrap();

    let macros: HashMap<String, String> = [("P", "T:"), ("HSC", "hsc1:")]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let (db, _) = IocBuilder::new()
        .register_record_type("asyn", || Box::new(AsynRecord::default()))
        .register_record_type("busy", || Box::new(BusyRecord::default()))
        .register_record_type("transform", || Box::new(TransformRecord::default()))
        .db_string(
            &format!(
                "record(asyn, \"T:asyn\") {{\n  field(PORT, \"{port}\")\n  field(ADDR, \"0\")\n}}\n"
            ),
            &HashMap::new(),
        )
        .unwrap()
        .db_file(&format!("{DB}{template}"), &macros)
        .unwrap()
        .build_served()
        .await
        .unwrap();
    db.ioc_init().await;
    let scan = ScanOwner::start(db.clone());
    for (pv, v) in pre {
        put(&db, pv, v).await;
    }
    let task = seq(prog, "name=hsc1,P=T:,HSC=hsc1:,S=T:asyn", 0, &db).expect("program starts");
    Rig {
        db,
        hsc,
        task,
        _scan: scan,
        _port: rt,
    }
}

/// `dbpf pv v`: a CA client's put, which processes the record.
pub async fn put(db: &PvDatabase, pv: &str, v: &str) {
    let (rec, field) = pv.split_once('.').unwrap_or((pv, "VAL"));
    db.put_record_field_from_ca(rec, field, EpicsValue::String(v.into()))
        .await
        .unwrap();
}

pub fn f64_of(db: &PvDatabase, pv: &str) -> f64 {
    db.get_pv(pv).unwrap().to_f64().unwrap()
}

pub fn string_of(db: &PvDatabase, pv: &str) -> String {
    match db.get_pv(pv).unwrap() {
        EpicsValue::String(s) => s.to_string(),
        v => format!("{v:?}"),
    }
}

/// Poll `probe` for up to ten seconds: the program runs on its own threads,
/// so every check is of a value arriving.
pub async fn eventually(mut probe: impl FnMut() -> bool) -> bool {
    for _ in 0..1000 {
        if probe() {
            return true;
        }
        task::sleep(Duration::from_millis(10)).await;
    }
    false
}

/// Let a program that is not expected to act have the time it would take.
pub async fn quiet() {
    task::sleep(Duration::from_millis(1500)).await;
}
