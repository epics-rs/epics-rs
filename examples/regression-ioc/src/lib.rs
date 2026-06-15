//! Regression IOC harness.
//!
//! Boots the [`REGRESSION_DB`] record set under real in-process servers — CA
//! (high-level, which spawns the periodic SCAN scheduler) and PVA (native,
//! ephemeral) — over one shared [`PvDatabase`]. Because both servers serve the
//! same `Arc<PvDatabase>`, a value driven through one protocol is visible
//! through the other, and the single CA-spawned scheduler drives periodic-SCAN
//! processing for both.
//!
//! The companion `tests/` pin recurring bug-fix behaviors (v0.15.x-v0.20.x)
//! against this IOC over the wire.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use asyn_rs::interfaces::motor::AsynMotor;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::device_support::DeviceSupport;
use epics_base_rs::server::ioc_app::DeviceSupportContext;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_ca_rs::client::CaClient;
use epics_ca_rs::server::CaServer;
use epics_pva_rs::client_native::context::PvaClient;
use epics_pva_rs::server::PvDatabaseSource;
use epics_pva_rs::server_native::{PvaServer, PvaServerConfig};
use motor_rs::builder::{MotorBuilder, MotorSetup};
use motor_rs::poll_loop::PollCommand;
use motor_rs::sim_motor::SimMotor;
use tokio::sync::mpsc;

/// The regression record database, embedded so the harness and the runnable
/// `regression_ioc` binary serve byte-identical records.
pub const REGRESSION_DB: &str = include_str!("../db/regression.db");

/// DTYP that the standard motor record (`REG:D:MTR`) carries; the harness
/// supplies the matching sim device support under this name.
const MOTOR_DTYP: &str = "simMotor";

/// A booted regression IOC: a shared database served by live CA + PVA servers.
///
/// Holds the native PVA server (dropping it aborts the server) and the CA
/// server task so the IOC stays up for the lifetime of this value.
pub struct RegressionIoc {
    /// The shared database both servers serve.
    pub db: Arc<PvDatabase>,
    /// The CA UDP+TCP port (a reserved free loopback port).
    pub ca_port: u16,
    /// The PVA TCP endpoint (an OS-assigned ephemeral loopback addr).
    pub pva_addr: SocketAddr,
    _ca_task: tokio::task::JoinHandle<epics_base_rs::error::CaResult<()>>,
    _pva_server: PvaServer,
    /// Kept alive so the motor poll loop's command channel stays open.
    _motor_poll_tx: mpsc::Sender<PollCommand>,
}

impl RegressionIoc {
    /// Boot the standard [`REGRESSION_DB`] record set.
    pub async fn boot() -> Result<Self, Box<dyn std::error::Error>> {
        Self::boot_from_db(REGRESSION_DB).await
    }

    /// Boot a custom db string (same server topology) — used by focused tests
    /// that need a record the standard set does not carry.
    pub async fn boot_from_db(db_text: &str) -> Result<Self, Box<dyn std::error::Error>> {
        // Bind the CA listeners to loopback only, so the IOC never answers a
        // real network search while a test runs.
        unsafe {
            std::env::set_var("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1");
            std::env::set_var("EPICS_CAS_BEACON_ADDR_LIST", "127.0.0.1");
        }

        let macros = std::collections::HashMap::new();

        // Motor device support: a sim driver + poll loop, handed to the db's
        // motor record (DTYP `simMotor`) through a take-once dynamic factory.
        // io_intr_scan_independent drives the record on every readback even
        // though SCAN=Passive, which is exactly the v0.20.0 path under test.
        let motor: Arc<Mutex<dyn AsynMotor>> =
            Arc::new(Mutex::new(SimMotor::new().with_limits(-100.0, 100.0)));
        let MotorSetup {
            record: _,
            device_support,
            poll_loop,
            poll_cmd_tx,
        } = MotorBuilder::new(motor)
            .poll_interval(Duration::from_millis(50))
            .build();
        let device_support = device_support.with_dtyp_name(MOTOR_DTYP.to_string());
        tokio::spawn(poll_loop.run());
        let motor_slot: Arc<Mutex<Option<Box<dyn DeviceSupport>>>> =
            Arc::new(Mutex::new(Some(Box::new(device_support))));
        let motor_factory = move |ctx: &DeviceSupportContext| -> Option<Box<dyn DeviceSupport>> {
            if ctx.dtyp == MOTOR_DTYP {
                motor_slot.lock().unwrap().take()
            } else {
                None
            }
        };
        let (motor_type, motor_record_factory) = motor_rs::motor_record_factory();

        let (db, _autosave) = IocBuilder::new()
            .register_record_type(motor_type, motor_record_factory)
            .register_dynamic_device_support(motor_factory)
            .db_string(db_text, &macros)?
            .build()
            .await?;

        // Arm motor polling after iocInit (C arms the poller post-PINI).
        let _ = poll_cmd_tx.try_send(PollCommand::StartPolling);

        // CA server on a reserved free port; run() spawns the SCAN scheduler.
        let ca_port = free_loopback_port();
        let ca_server = CaServer::from_parts(db.clone(), ca_port, None, None, None, None);
        let _ca_task = tokio::spawn(async move { ca_server.run().await });

        // Native PVA server on an ephemeral loopback port; serves the same db.
        let source = Arc::new(PvDatabaseSource::new(db.clone()));
        let pva_server = PvaServer::start(source, PvaServerConfig::isolated())?;
        let pva_addr = pva_server.tcp_addr();

        // Let the CA listener finish binding before clients connect.
        tokio::time::sleep(Duration::from_millis(250)).await;

        Ok(Self {
            db,
            ca_port,
            pva_addr,
            _ca_task,
            _pva_server: pva_server,
            _motor_poll_tx: poll_cmd_tx,
        })
    }

    /// A CA client pointed at this IOC (skips UDP broadcast search).
    ///
    /// Sets the process-global `EPICS_CA_*` env, so tests using this MUST be
    /// `#[serial]` under the libtest (`cargo test`) runner.
    pub async fn ca_client(&self) -> CaClient {
        point_ca_client_at(self.ca_port);
        CaClient::new().await.expect("CaClient::new")
    }

    /// A PVA client pinned to this IOC's TCP endpoint (no UDP discovery).
    pub fn pva_client(&self) -> PvaClient {
        PvaClient::builder()
            .timeout(Duration::from_secs(5))
            .server_addr(self.pva_addr)
            .build()
    }
}

/// Reserve then release a free loopback TCP port, returning its number.
///
/// `CaServer` cannot read back a kernel-assigned port-0 binding, so the
/// verified idiom is reserve-then-bind-that-number.
pub fn free_loopback_port() -> u16 {
    let probe = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("reserve free port");
    let port = probe.local_addr().expect("local_addr").port();
    drop(probe);
    port
}

/// Point the next `CaClient::new()` at exactly `127.0.0.1:port`.
fn point_ca_client_at(port: u16) {
    unsafe {
        std::env::set_var("EPICS_CA_ADDR_LIST", format!("127.0.0.1:{port}"));
        std::env::set_var("EPICS_CA_AUTO_ADDR_LIST", "NO");
        std::env::set_var("EPICS_CA_SERVER_PORT", port.to_string());
    }
}
