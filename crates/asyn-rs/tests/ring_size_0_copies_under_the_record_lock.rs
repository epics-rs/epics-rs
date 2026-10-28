//! `ringSize` 0: where a driver callback's value goes, and when.
//!
//! C copies a good callback's value into the record itself, under
//! `dbScanLock`, inside the callback (devAsynOctet.c:490-509,
//! devAsynXXXArray.cpp:412-428), and the pass it requests then takes "data
//! already copied to the record" (devAsynOctet.c:772-773). A put landing
//! between the callback and that pass is what the pass then holds. An
//! `asyn:READBACK` output's callback copies the same way.
//!
//! Each test holds the record's callback band, so the pass cannot start,
//! fires the callback and puts on this thread, then lets the band go.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use asyn_rs::asyn_record::register_port;
use asyn_rs::interfaces::InterfaceType;
use asyn_rs::interrupt::InterruptValue;
use asyn_rs::param::{ParamType, ParamValue};
use asyn_rs::port::InterfaceMask;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::port_handle::PortHandle;
use asyn_rs::runtime::config::RuntimeConfig;
use asyn_rs::runtime::port::create_port_runtime_unclaimed;
use epics_base_rs::runtime::task::CallbackPriority;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::types::{DbFieldType, EpicsValue};

const REC: &str = "RS0:REC";

struct Port {
    base: PortDriverBase,
}

impl PortDriver for Port {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }
}

/// One I/O Intr record with `asyn:FIFO` 0 on a port whose reason 0 is `ty`.
async fn ioc(port: &str, ty: ParamType, record: &str) -> (Arc<PvDatabase>, PortHandle) {
    let mut base = PortDriverBase::new(
        port,
        1,
        PortFlags::default(),
        InterfaceMask::OCTET | InterfaceMask::INT32_ARRAY | InterfaceMask::DRV_USER,
        InterfaceMask::OCTET | InterfaceMask::INT32_ARRAY,
    );
    base.create_param("P", ty).unwrap();
    let (runtime, _join) = create_port_runtime_unclaimed(Port { base }, RuntimeConfig::default())
        .expect("port runtime starts");
    let handle = runtime.port_handle().clone();
    let owner = Arc::new(PvDatabase::new());
    register_port(port, handle.clone(), owner.scan_control().interrupt_gate())
        .expect("port name is free");
    drop(runtime);

    let (db, _) = asyn_rs::adapter::register_asyn_device_support_for_builder(IocBuilder::new())
        .with_database(owner)
        .db_string(&record.replace("PORT", port), &HashMap::new())
        .unwrap()
        .build()
        .await
        .unwrap();
    db.scan_control().run();
    for _ in 0..40 {
        epics_base_rs::runtime::task::sleep(Duration::from_millis(5)).await;
    }
    (db, handle)
}

/// Occupy the record's band (PRIO LOW) until the returned sender drops.
fn hold_band() -> mpsc::Sender<()> {
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let _band =
        epics_base_rs::runtime::task::spawn_blocking_background(CallbackPriority::Low, move |_| {
            started_tx.send(()).unwrap();
            let _ = release_rx.recv();
        });
    started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the band picked the hold up");
    release_tx
}

fn fire(handle: &PortHandle, value: ParamValue, iface: InterfaceType) {
    handle.interrupts().notify(InterruptValue {
        reason: 0,
        addr: 0,
        value,
        iface: Some(iface),
        ..Default::default()
    });
}

/// A synchronous dbPutField of VAL, made on this thread; I/O Intr, so it
/// processes nothing.
fn put_val(db: &PvDatabase, value: EpicsValue) {
    let held = db.lock_for_put(REC, "VAL", &value);
    db.put_record_field_from_ca_already_locked(held, value)
        .expect("put accepted");
}

/// A DBE_VALUE|DBE_LOG|DBE_ALARM monitor; MPST Always makes each pass post.
fn subscribe(db: &PvDatabase) -> EventReader {
    let rec = db.get_record(REC).unwrap();
    let mut inst = rec.write();
    inst.add_subscriber("VAL", 1, DbFieldType::String, 0x07)
        .expect("VAL is subscribable")
}

fn block_until_posted(reader: &mut EventReader) {
    for _ in 0..400 {
        if reader.try_recv().is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("the callback's pass within 2 s");
}

fn val(db: &PvDatabase) -> Option<EpicsValue> {
    db.get_record(REC).unwrap().read().record.get_field("VAL")
}

/// devAsynOctet.c:490-509, :772-773.
#[epics_base_rs::epics_test]
async fn a_stringin_put_after_the_callback_is_what_the_pass_keeps() {
    let (db, handle) = ioc(
        "rs0Octet",
        ParamType::Octet,
        r#"record(stringin, "RS0:REC") {
            field(DTYP, "asynOctetRead") field(INP, "@asyn(PORT,0)P")
            field(SCAN, "I/O Intr") field(MPST, "Always")
            info(asyn:FIFO, "0")
        }"#,
    )
    .await;
    let band = hold_band();
    fire(
        &handle,
        ParamValue::Octet(Arc::from(&b"OLD"[..])),
        InterfaceType::Octet,
    );
    put_val(&db, EpicsValue::String("NEW".into()));
    let mut posts = subscribe(&db);
    drop(band);
    block_until_posted(&mut posts);
    assert_eq!(val(&db), Some(EpicsValue::String("NEW".into())));
}

/// devAsynXXXArray.cpp:412-428.
#[epics_base_rs::epics_test]
async fn a_waveform_put_after_the_callback_is_what_the_pass_keeps() {
    let (db, handle) = ioc(
        "rs0Array",
        ParamType::Int32Array,
        r#"record(waveform, "RS0:REC") {
            field(DTYP, "asynInt32ArrayIn") field(INP, "@asyn(PORT,0)P")
            field(SCAN, "I/O Intr") field(FTVL, "LONG") field(NELM, "4")
            info(asyn:FIFO, "0")
        }"#,
    )
    .await;
    let band = hold_band();
    fire(
        &handle,
        ParamValue::Int32Array(Arc::from(&[1, 2, 3][..])),
        InterfaceType::Int32Array,
    );
    put_val(&db, EpicsValue::LongArray(vec![9, 9].into()));
    let mut posts = subscribe(&db);
    drop(band);
    block_until_posted(&mut posts);
    assert_eq!(val(&db), Some(EpicsValue::LongArray(vec![9, 9].into())));
}

/// The copy is posted by the pass the callback asks for, as C's
/// `monitor()` posts what `dbProcess` finds in VAL (stringinRecord.c,
/// MPST On Change).
#[epics_base_rs::epics_test]
async fn a_stringin_callbacks_copy_is_posted_by_its_pass() {
    let (db, handle) = ioc(
        "rs0OctetPost",
        ParamType::Octet,
        r#"record(stringin, "RS0:REC") {
            field(DTYP, "asynOctetRead") field(INP, "@asyn(PORT,0)P")
            field(SCAN, "I/O Intr")
            info(asyn:FIFO, "0")
        }"#,
    )
    .await;
    let mut posts = subscribe(&db);
    fire(
        &handle,
        ParamValue::Octet(Arc::from(&b"HELLO"[..])),
        InterfaceType::Octet,
    );
    block_until_posted(&mut posts);
    assert_eq!(val(&db), Some(EpicsValue::String("HELLO".into())));
}

/// devAsynXXXArray.cpp:412-428 then waveformRecord's `monitor()`.
#[epics_base_rs::epics_test]
async fn a_waveform_callbacks_copy_is_posted_by_its_pass() {
    let (db, handle) = ioc(
        "rs0ArrayPost",
        ParamType::Int32Array,
        r#"record(waveform, "RS0:REC") {
            field(DTYP, "asynInt32ArrayIn") field(INP, "@asyn(PORT,0)P")
            field(SCAN, "I/O Intr") field(FTVL, "LONG") field(NELM, "4")
            info(asyn:FIFO, "0")
        }"#,
    )
    .await;
    let mut posts = subscribe(&db);
    fire(
        &handle,
        ParamValue::Int32Array(Arc::from(&[4, 5, 6][..])),
        InterfaceType::Int32Array,
    );
    block_until_posted(&mut posts);
    assert_eq!(val(&db), Some(EpicsValue::LongArray(vec![4, 5, 6].into())));
    assert_eq!(
        db.get_record(REC).unwrap().read().record.get_field("NORD"),
        Some(EpicsValue::ULong(3))
    );
}

/// devAsynOctet.c:490-503 on an output: the value is in VAL before the
/// `outputCallbackCallback` pass runs.
#[epics_base_rs::epics_test]
async fn a_stringout_readback_is_in_val_before_its_pass() {
    let (db, handle) = ioc(
        "rs0OctetOut",
        ParamType::Octet,
        r#"record(stringout, "RS0:REC") {
            field(DTYP, "asynOctetWrite") field(OUT, "@asyn(PORT,0)P")
            info(asyn:READBACK, "1") info(asyn:FIFO, "0")
        }"#,
    )
    .await;
    let band = hold_band();
    fire(
        &handle,
        ParamValue::Octet(Arc::from(&b"BACK"[..])),
        InterfaceType::Octet,
    );
    assert_eq!(val(&db), Some(EpicsValue::String("BACK".into())));
    let mut posts = subscribe(&db);
    drop(band);
    block_until_posted(&mut posts);
    assert_eq!(val(&db), Some(EpicsValue::String("BACK".into())));
}

/// devAsynXXXArray.cpp:412-428 on an output, then its `scanOnce` pass.
#[epics_base_rs::epics_test]
async fn an_aao_readback_is_in_val_before_its_pass() {
    let (db, handle) = ioc(
        "rs0ArrayOut",
        ParamType::Int32Array,
        r#"record(aao, "RS0:REC") {
            field(DTYP, "asynInt32ArrayOut") field(OUT, "@asyn(PORT,0)P")
            field(FTVL, "LONG") field(NELM, "4")
            info(asyn:READBACK, "1")
        }"#,
    )
    .await;
    let band = hold_band();
    fire(
        &handle,
        ParamValue::Int32Array(Arc::from(&[7, 8][..])),
        InterfaceType::Int32Array,
    );
    assert_eq!(val(&db), Some(EpicsValue::LongArray(vec![7, 8].into())));
    let mut posts = subscribe(&db);
    drop(band);
    block_until_posted(&mut posts);
    assert_eq!(val(&db), Some(EpicsValue::LongArray(vec![7, 8].into())));
}
