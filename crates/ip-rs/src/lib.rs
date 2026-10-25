//! Rust port of the synApps [ip](https://github.com/epics-modules/ip) module's
//! SNL programs, compiled by epics-snc-rs from `src/snl` and run on
//! epics-seq-rs, and of its generic octet device support `devXxStrParm.c`
//! ([`strparm`]), which the programs' templates read and write through.
//!
//! # C reference pins
//!
//! Every `file:NNN` citation in this crate resolves in ip at `d00f687`.

pub mod fmt;
pub mod snl;
pub mod strparm;
pub mod user_param;

use asyn_rs::interfaces::InterfaceType;
use asyn_rs::manager::DeviceUser;
use asyn_rs::port_handle::PortHandle;
use epics_base_rs::error::CaResult;
use epics_base_rs::server::record::Record;
use epics_base_rs::types::EpicsValue;

/// The module root, for an `st.cmd` that names `$(IP)/db/...`.
pub const IP_DIR: &str = env!("CARGO_MANIFEST_DIR");

/// Path to the bundled database template directory.
pub const IP_DB_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/db");

/// The `registrar()` lines of `ipSupport.dbd`: each program becomes
/// startable by the shell's `seq` command, which the first registration
/// adds. Idempotent.
pub fn register_ip_dbd() {
    // C defect fixed: ipSupport.dbd has no registrar(FederalRegistrar), so a
    // C IOC built on it answers `seq &Federal` with "Can't find sequencer".
    snl::federal::FederalRegistrar();
    snl::keithley2k_dmm_mf40::Keithley2kDMM_mf40Registrar();
}

/// The body `MPCConvert`, `TPG261Convert`, `TelevacConvert` and
/// `strParmConvert` share: `eslo = 1.0; roff = 0` on an ai. The device writes
/// VAL itself, so the slope only has to stay out of the way.
pub fn unit_slope(ai: &mut dyn Record) -> CaResult<()> {
    ai.put_field("ESLO", EpicsValue::Double(1.0))?;
    ai.put_field("ROFF", EpicsValue::ULong(0))
}

/// Why `initCommon` took its `bad:` arm at `connectDevice`/`findInterface`.
pub enum Unconnected {
    /// `connectDevice` failed, leaving this `errorMessage`.
    Device(String),
    /// The port has no `asynOctet`; C's status is still `asynSuccess`.
    NoOctet,
}

/// C `pasynManager->connectDevice(pasynUser, port, addr)`, then
/// `findInterface(pasynUser, asynOctetType, 1)`. `port` is never C's NULL:
/// `parseLink` mallocs it on every success (asynEpicsUtils.c:80, :101).
pub fn connect_octet(port: &str, addr: i32) -> Result<PortHandle, Unconnected> {
    let mut user = DeviceUser::default();
    let entry = user
        .connect_device(Some(port), addr)
        .map_err(|_| Unconnected::Device(user.error_message().to_owned()))?;
    if !entry.handle.has_interface(InterfaceType::Octet) {
        return Err(Unconnected::NoOctet);
    }
    Ok(entry.handle.clone())
}

/// A registered port without `asynOctet`, as `findInterface` sees it.
#[cfg(test)]
pub(crate) fn no_octet_port(name: &str) {
    use asyn_rs::interfaces::Capability;
    use asyn_rs::manager::{PortManager, claim_port_driver};
    use asyn_rs::port::{InterfaceMask, PortDriver, PortDriverBase, PortFlags};
    use asyn_rs::registry::open_owner;

    struct Int32Only(PortDriverBase);
    impl PortDriver for Int32Only {
        fn base(&self) -> &PortDriverBase {
            &self.0
        }
        fn base_mut(&mut self) -> &mut PortDriverBase {
            &mut self.0
        }
        fn capabilities(&self) -> Vec<Capability> {
            vec![Capability::Int32Read, Capability::Int32Write]
        }
    }
    let driver = Int32Only(PortDriverBase::new(
        name,
        1,
        PortFlags::default(),
        InterfaceMask::UNMIGRATED_ALL,
        InterfaceMask::UNMIGRATED_INTERRUPTS,
    ));
    let runtime = PortManager::new()
        .register_port(claim_port_driver(name).unwrap(), driver, &open_owner())
        .unwrap();
    std::mem::forget(runtime);
}
