//! C has no unregister: a driver whose constructor fails after `registerPort`
//! keeps its port. `drvAsynDG645` (drvAsynDG645.cpp:525-594) returns from its
//! `*IDN?` failure with every interface registered and `init` still 0, so a
//! second Configure under the name is refused and a record attaching to the
//! port binds, then reads `asynError` from every transfer (:1071-1072).

use asyn_rs::error::{AsynError, AsynResult, AsynStatus};
use asyn_rs::manager::claim_port_driver;
use asyn_rs::param::ParamType;
use asyn_rs::port::InterfaceMask;
use asyn_rs::port::{PortDriver, PortDriverBase, PortFlags};
use asyn_rs::registry::{get_port, open_owner};
use asyn_rs::runtime::{RuntimeConfig, create_port_runtime};
use asyn_rs::user::AsynUser;
use epics_base_rs::server::ioc_app::DeviceSupportContext;
use epics_base_rs::server::recgbl::alarm_status;
use epics_base_rs::server::record::{AlarmSeverity, ScanType};
use epics_base_rs::server::records::longin::LonginRecord;

/// The driver as DG645's constructor leaves it when the device never answers.
struct HalfBuilt {
    base: PortDriverBase,
    init: bool,
}

impl PortDriver for HalfBuilt {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }
    fn read_int32(&mut self, user: &AsynUser) -> AsynResult<i32> {
        if !self.init {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: String::new(),
            });
        }
        self.base.get_int32_param(user.reason, 0)
    }
}

/// A Configure whose device never answers `*IDN?` after the port is
/// registered.
fn configure(port: &str) -> Result<(), AsynError> {
    let claim = claim_port_driver(port).map_err(|e| AsynError::Status {
        status: AsynStatus::Error,
        message: e.to_string(),
    })?;
    let mut base = PortDriverBase::new(
        port,
        1,
        PortFlags::default(),
        InterfaceMask::UNMIGRATED_ALL,
        InterfaceMask::UNMIGRATED_INTERRUPTS,
    );
    base.create_param("VAL", ParamType::Int32)?;
    let driver = HalfBuilt { base, init: false };
    let (runtime, _actor) = create_port_runtime(&claim, driver, RuntimeConfig::default())?;
    let identification = AsynError::Status {
        status: AsynStatus::Error,
        message: format!("{port} failed to acquire identification"),
    };
    Err(claim.publish_failed(runtime.port_handle().clone(), &open_owner(), identification))
}

#[test]
fn a_second_configure_of_a_half_built_port_is_refused() {
    const CHILD: &str = "ASYN_RS_HALF_BUILT_CHILD";
    const TEST: &str = "a_second_configure_of_a_half_built_port_is_refused";
    if std::env::var_os(CHILD).is_some() {
        assert!(configure("HALFDUP").is_err());
        assert!(
            get_port("HALFDUP").is_some(),
            "the failed port stays registered"
        );
        println!("SECOND");
        assert!(configure("HALFDUP").is_err());
        return;
    }
    let out = std::process::Command::new(std::env::current_exe().expect("the test binary"))
        .args(["--exact", TEST, "--nocapture"])
        .env(CHILD, "1")
        .output()
        .expect("re-exec the test binary");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let second = stdout.split_once("SECOND\n").expect("the child ran").1;
    let lines: Vec<&str> = second.lines().collect();
    assert_eq!(
        lines[..2],
        [
            "asynCommon:registerDriver HALFDUP already registered",
            "asynPortDriver:asynPortDriver ERROR: Can't register port: HALFDUP",
        ],
        "{stdout}"
    );
}

#[test]
fn a_record_attaches_to_a_half_built_port_and_reads_its_refusal() {
    assert!(configure("HALFREC").is_err());
    let ctx = DeviceSupportContext {
        record_type: "longin",
        dtyp: "asynInt32",
        inp: "@asyn(HALFREC,0,1)VAL",
        out: "",
    };
    let mut dev = asyn_rs::adapter::universal_asyn_factory(&ctx).expect("the factory binds");
    dev.set_record_info("HALF:LI", ScanType::Passive);
    let mut rec = LonginRecord::new(0);
    dev.init(&mut rec)
        .expect("drvUserCreate resolves VAL on the half-built port");
    let _ = dev.read(&mut rec);
    let alarm = dev.last_alarm().expect("the read left an alarm");
    assert_eq!(
        (alarm.stat, alarm.sevr),
        (alarm_status::READ_ALARM, AlarmSeverity::Invalid as u16)
    );
}
