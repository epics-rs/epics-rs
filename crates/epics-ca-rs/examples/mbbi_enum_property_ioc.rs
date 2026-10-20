//! The IOC half of the two-process rig that measures an enum-string re-key
//! reaching a *C* CA client. An mbbi whose device support re-keys ZRST/ONST/
//! TWST after `iocInit` and posts `DBE_PROPERTY` on VAL — the asyn
//! `callbackEnum` shape (devAsynInt32.c:712-766, asyn `e2a281e2`), where
//! `setEnums` rewrites the state fields silently and one
//! `db_post_events(&pr->val, DBE_PROPERTY)` is what tells clients to re-read.
//!
//! A `nextest` case cannot close this: the claim is about what the C client
//! library does with our wire bytes, so the client has to be `libca`.
//!
//! ```text
//! # IOC (this file)
//! cargo run -p epics-ca-rs --example mbbi_enum_property_ioc
//!
//! # clients, against the same private port
//! export EPICS_CA_AUTO_ADDR_LIST=NO EPICS_CA_ADDR_LIST=127.0.0.1 \
//!        EPICS_CA_SERVER_PORT=15764 LD_LIBRARY_PATH=$EPICS_BASE/lib/linux-x86_64
//! $EPICS_BASE/bin/linux-x86_64/camonitor -m p RIG:MBBI
//! $EPICS_BASE/bin/linux-x86_64/camonitor -m va RIG:MBBI   # must NOT see it
//! $EPICS_BASE/bin/linux-x86_64/caget -d DBR_GR_ENUM RIG:MBBI
//! ```
//!
//! `camonitor` alone does not settle the DBR_GR_ENUM half: for a `DBF_ENUM`
//! channel it subscribes as `DBR_TIME_STRING` (camonitor.c:156-165), so the
//! label it prints was rendered by the server. The client-side half needs a
//! `DBR_GR_ENUM` + `DBE_PROPERTY` subscription, which is what base itself
//! does for attribute re-reads (`dbCa.c`), in ~40 lines against `cadef.h`.
//!
//! `RIG_PORT` (default 15764) and `RIG_DELAY` seconds (default 5) tune it.
// On `exec_backend` this program's `main` refuses instead of running, so
// everything below it is unreachable in that configuration by construction.
// The lint is reporting the intent, not dead code: the default build still
// lints this file in full.
#![cfg_attr(exec_backend, allow(dead_code, unused_imports))]

use epics_base_rs::error::CaResult;
use epics_base_rs::server::device_support::{DeviceInitOutcome, DeviceSupport, PropertyPost};
#[cfg(tokio_backend)]
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::record::{DbLinkType, DevSup, Dset, Record};
use epics_base_rs::types::EpicsValue;
#[cfg(tokio_backend)]
use epics_ca_rs::server::CaServerBuilder;
use std::collections::HashMap;

/// `device(mbbi, CONSTANT, devMbbiEnumProp, "enumProp")`, with the
/// `init_record` and `read_mbbi` a C dset for it would carry.
static ENUM_PROP: DevSup = DevSup {
    name: "devMbbiEnumProp",
    choice: "enumProp",
    link_type: DbLinkType::Constant,
    dset: Dset::new(5, &[2, 4], false),
};

/// Holds both ends until the framework claims the receiver: the device owns
/// the source subscription, the framework owns the post.
struct EnumProp {
    rx: Option<tokio::sync::mpsc::Receiver<PropertyPost>>,
    tx: Option<tokio::sync::mpsc::Sender<PropertyPost>>,
}

impl EnumProp {
    fn new() -> Self {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        Self {
            rx: Some(rx),
            tx: Some(tx),
        }
    }
}

impl DeviceSupport for EnumProp {
    fn init(&mut self, _record: &mut dyn Record) -> CaResult<DeviceInitOutcome> {
        let tx = self.tx.take().expect("init runs once");
        let delay: u64 = std::env::var("RIG_DELAY")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
            epics_libcom_rs::stderr_println!(
                "[ioc] re-keying enum strings, posting DBE_PROPERTY on VAL"
            );
            let _ = tx
                .send(PropertyPost {
                    writes: vec![
                        ("ZRST".into(), EpicsValue::String("OFF".into())),
                        ("ONST".into(), EpicsValue::String("ON".into())),
                        ("TWST".into(), EpicsValue::String("FAULT".into())),
                    ],
                    post_field: "VAL".into(),
                })
                .await;
        });
        Ok(DeviceInitOutcome::ok())
    }

    fn write(&mut self, _record: &mut dyn Record) -> CaResult<()> {
        Ok(())
    }

    fn dtyp(&self) -> &str {
        "enumProp"
    }

    fn property_post_receiver(&mut self) -> Option<tokio::sync::mpsc::Receiver<PropertyPost>> {
        self.rx.take()
    }
}

#[cfg(tokio_backend)]
#[tokio::main]
async fn main() -> CaResult<()> {
    let port: u16 = std::env::var("RIG_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(15764);
    let db = r#"
record(mbbi, "RIG:MBBI") {
    field(DTYP, "enumProp")
    field(ZRST, "Zero")
    field(ONST, "One")
    field(TWST, "Two")
    field(VAL,  "1")
    field(SCAN, "Passive")
}
"#;
    let args = epics_base_rs::server::ioc_app::ServerInitArgs {
        port,
        tcp_port: Some(port),
    };
    let mut ioc = IocBuilder::new()
        .db_string(db, &HashMap::new())?
        .register_device_support("mbbi", &ENUM_PROP, || Box::new(EnumProp::new()))
        .register_server_init(
            epics_ca_rs::server::RSRV_SERVER,
            args,
            epics_ca_rs::server::rsrv_init,
        )
        .build_ioc()
        .await?;
    let server = CaServerBuilder::serving(
        ioc.database().clone(),
        epics_base_rs::server::access_security::new_acf_cell(None),
        None,
        None,
    )
    .port(port)
    .tcp_port(port)
    .bound_at_init(ioc.servers());
    ioc.run().await;
    let server = server.build().await?;
    epics_libcom_rs::stderr_println!("[ioc] serving RIG:MBBI on CA port {port}");
    server.run().await
}

/// The `exec_backend` arm: the rig serves through the async CA front-end, which
/// this backend does not compile.
#[cfg(exec_backend)]
fn main() -> CaResult<()> {
    epics_base_rs::runtime::task::refuse_backend(
        "mbbi_enum_property_ioc: needs the tokio backend; this build selects \
         EPICS_RS_BUILD_EXEC_BACKEND=thread.",
    )
}
