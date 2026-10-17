//! The asyn user a motor's diagnostics print through.

use std::sync::OnceLock;

use asyn_rs::manager::PortManager;
use asyn_rs::trace::TraceMask;

/// The asyn user an axis's device support prints through.
#[derive(Debug, Clone, PartialEq)]
pub enum TraceUser {
    /// C `devMotorAsyn`'s `pasynUser`, connected to the axis `(port, addr)`
    /// of an `asynMotorController` (model 3).
    Asyn { port: String, addr: i32 },
    /// The same user on a model-2 axis: a `motorAxisDrvSET_t` behind
    /// `drvMotorAsyn.c`, which prints its own write lines in place of
    /// `asynMotorController`'s.
    Model2 { port: String, addr: i32 },
    /// A model-1 axis, signal `signal` of card `card` in the table of the
    /// driver whose DTYP is `device`. C `motordevCom` keeps no asyn user and
    /// prints nothing, so neither the device support nor the poller has a line
    /// to print.
    Model1 {
        device: &'static str,
        card: i32,
        signal: i32,
    },
    /// A `motor_interface.h` axis reached through devMotorSim (devMotorSim.c):
    /// card `card`, axis `signal` of the `drvet` named `driver`, which a
    /// record's VME_IO parm names, under DTYP `device`. The driver's own task
    /// calls the dset back with what it changed; there is no asyn user and no
    /// motordevCom, so nothing prints.
    MotorInterface {
        device: &'static str,
        driver: &'static str,
        card: i32,
        signal: i32,
    },
}

/// The device support and the poller share one [`TraceUser`], so every line
/// the C device support and its driver layer print goes to that port's
/// trace file under that port's trace mask. `record` is the `pmr->name` those
/// lines and the record's own printf's carry.
#[derive(Debug)]
pub struct MotorTrace {
    user: TraceUser,
    record: OnceLock<String>,
}

impl Default for MotorTrace {
    fn default() -> Self {
        Self::with_user(TraceUser::Asyn {
            port: String::new(),
            addr: 0,
        })
    }
}

impl MotorTrace {
    pub fn new(port: impl Into<String>, addr: i32) -> Self {
        Self::with_user(TraceUser::Asyn {
            port: port.into(),
            addr,
        })
    }

    pub fn with_user(user: TraceUser) -> Self {
        Self {
            user,
            record: OnceLock::new(),
        }
    }

    /// Name the record; the first name wins, as C binds `dpvt` to one record.
    pub fn set_record(&self, name: &str) {
        let _ = self.record.set(name.to_owned());
    }

    pub fn record(&self) -> &str {
        self.record.get().map_or("", String::as_str)
    }

    /// The axis this user is connected to, which is also its address.
    pub fn user(&self) -> &TraceUser {
        &self.user
    }

    /// The asyn port; empty for a model-1 axis, which prints nothing.
    pub fn port(&self) -> &str {
        match &self.user {
            TraceUser::Asyn { port, .. } | TraceUser::Model2 { port, .. } => port,
            TraceUser::Model1 { .. } | TraceUser::MotorInterface { .. } => "",
        }
    }

    /// The axis number, C's `pAxis->axisNo_`.
    pub fn addr(&self) -> i32 {
        match self.user {
            TraceUser::Asyn { addr, .. } | TraceUser::Model2 { addr, .. } => addr,
            TraceUser::Model1 { .. } | TraceUser::MotorInterface { .. } => 0,
        }
    }

    /// Whether the driver layer is `asynMotorController` (model 3), whose
    /// writes set status fields of their own.
    pub fn is_controller(&self) -> bool {
        matches!(self.user, TraceUser::Asyn { .. })
    }

    /// Whether the axis is a model-1 card signal, with no asyn driver layer.
    pub fn is_model1(&self) -> bool {
        matches!(self.user, TraceUser::Model1 { .. })
    }

    /// Whether the axis is a `motor_interface.h` driver's, bound through
    /// devMotorSim.
    pub fn is_motor_interface(&self) -> bool {
        matches!(self.user, TraceUser::MotorInterface { .. })
    }

    /// Whether the driver layer is `drvMotorAsyn.c` (model 2) rather than
    /// `asynMotorController`.
    pub fn is_model2(&self) -> bool {
        matches!(self.user, TraceUser::Model2 { .. })
    }

    /// `driverReasons[motorStatus]`, the reason devMotorAsyn's init-time user
    /// carries (devMotorAsyn.c:405): `asynMotorController`'s `motorStatus_`
    /// index, or drvMotorAsyn's `motorStatus` command (drvMotorAsyn.c:103).
    pub fn status_reason(&self) -> i32 {
        if self.is_model2() { 108 } else { 26 }
    }

    /// The reason `pPvt->pasynUser` keeps after init_record's last
    /// `findDrvInfo` (devMotorAsyn.c:345), `driverReasons[motorUpdateStatus]`:
    /// the user `update_values` and `build_trans` print through.
    pub fn update_status_reason(&self) -> i32 {
        if self.is_model2() { 109 } else { 27 }
    }

    /// `asynPrint(pasynUser, mask, ...)` with `pasynUser->reason = reason`.
    pub fn print(&self, mask: TraceMask, reason: i32, args: std::fmt::Arguments<'_>) {
        if let TraceUser::Asyn { port, addr } | TraceUser::Model2 { port, addr } = &self.user {
            PortManager::global().trace_manager().print(
                Some(port),
                Some(*addr),
                reason,
                mask,
                args,
            );
        }
    }

    /// `asynPrint(pasynUserSelf, mask, ...)`: the controller's own user,
    /// connected at address 0 (asynPortDriver.cpp:4099).
    pub fn print_self(&self, mask: TraceMask, args: std::fmt::Arguments<'_>) {
        if let TraceUser::Asyn { port, .. } | TraceUser::Model2 { port, .. } = &self.user {
            PortManager::global()
                .trace_manager()
                .print(Some(port), Some(0), 0, mask, args);
        }
    }
}
