//! The asyn user a motor's diagnostics print through.

use std::sync::OnceLock;

use asyn_rs::manager::PortManager;
use asyn_rs::trace::TraceMask;

/// The asyn user an axis's device support prints through.
#[derive(Debug, Clone, PartialEq)]
pub enum TraceUser {
    /// C `devMotorAsyn`'s `pasynUser`, connected to the axis `(port, addr)`.
    Asyn { port: String, addr: i32 },
    /// A model-1 axis: C `motordevCom` keeps no asyn user and prints nothing,
    /// so neither the device support nor the poller has a line to print.
    Model1,
}

/// The device support and the poller share one [`TraceUser`], so every line
/// the C device support and `asynMotorController` print goes to that port's
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

    /// The asyn port; empty for a model-1 axis, which prints nothing.
    pub fn port(&self) -> &str {
        match &self.user {
            TraceUser::Asyn { port, .. } => port,
            TraceUser::Model1 => "",
        }
    }

    /// The axis number, C's `pAxis->axisNo_`.
    pub fn addr(&self) -> i32 {
        match self.user {
            TraceUser::Asyn { addr, .. } => addr,
            TraceUser::Model1 => 0,
        }
    }

    /// `asynPrint(pasynUser, mask, ...)` with `pasynUser->reason = reason`.
    pub fn print(&self, mask: TraceMask, reason: i32, args: std::fmt::Arguments<'_>) {
        if let TraceUser::Asyn { port, addr } = &self.user {
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
        if let TraceUser::Asyn { port, .. } = &self.user {
            PortManager::global()
                .trace_manager()
                .print(Some(port), Some(0), 0, mask, args);
        }
    }
}
