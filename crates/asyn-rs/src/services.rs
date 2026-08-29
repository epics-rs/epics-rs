//! The services every asyn port is born with — C's `pasynBase`.
//!
//! In C a port cannot exist without a trace configuration and an exception
//! list: `registerPort` reaches `dpCommonInit` (asynManager.c:2066), which
//! calls `tracePvtInit(&pdpCommon->trace)` (:528) and
//! `ellInit(&pdpCommon->exceptionUserList)` (:524) on the
//! `dpCommon` it is allocating, so `announceExceptionOccurred`
//! (asynManager.c:611-637) and `asynPrint` always have a list to walk. There
//! is no reachable state in which a registered port has neither.
//!
//! The port here binds the same guarantee to the one function every port
//! creation passes through ([`crate::runtime::create_port_runtime`]): it
//! injects a [`PortServices`] into the driver's [`PortDriverBase`] before the
//! actor thread starts. No other site may assign `base.trace` or
//! `base.exception_sink` — that is what keeps "a running port has both" true
//! by construction rather than by every port creator remembering to do it.
//! Before this, injection lived in `PortManager::register_port` alone, and
//! `drvAsynIPPortConfigure` & co. — the way an st.cmd actually builds a port —
//! bypassed it, leaving trace and exceptions inert on every real IOC port.

use std::sync::{Arc, OnceLock};

use crate::exception::ExceptionManager;
use crate::port::PortDriverBase;
use crate::trace::TraceManager;

/// The trace configuration and exception list a port is created with.
///
/// Cloning shares — every port created from the same `PortServices` announces
/// on the same exception list and reads the same trace masks, which is what
/// makes `asynSetTraceMask MYPORT -1 0x9` from iocsh reach the port it names.
#[derive(Clone)]
pub struct PortServices {
    trace: Arc<TraceManager>,
    exceptions: Arc<ExceptionManager>,
}

impl PortServices {
    /// Build services around an existing [`TraceManager`].
    ///
    /// The trace manager is wired to the new exception list so the
    /// `asynSetTrace*` setters announce `asynExceptionTrace*`
    /// (asynManager.c:2790/2832/2874/2923/2956).
    pub fn new(trace: Arc<TraceManager>) -> Self {
        let exceptions = Arc::new(ExceptionManager::new());
        trace.set_exception_sink(exceptions.clone());
        Self { trace, exceptions }
    }

    /// The process-wide services — C's `pasynBase`, created once
    /// (asynManager.c:236-260) and shared by every port `asynInit` ever sees.
    /// This is what a port built with a default [`crate::runtime::RuntimeConfig`]
    /// gets.
    pub fn global() -> Self {
        static GLOBAL: OnceLock<PortServices> = OnceLock::new();
        GLOBAL
            .get_or_init(|| PortServices::new(Arc::new(TraceManager::new())))
            .clone()
    }

    pub fn trace(&self) -> &Arc<TraceManager> {
        &self.trace
    }

    pub fn exceptions(&self) -> &Arc<ExceptionManager> {
        &self.exceptions
    }

    /// Bind a driver to these services. The **only** site that may write
    /// `base.trace` / `base.exception_sink`; called by `create_port_runtime`
    /// on every port, so no port can run without them.
    pub(crate) fn bind(&self, base: &mut PortDriverBase) {
        base.trace = Some(self.trace.clone());
        base.bind_exception_sink(self.exceptions.clone());
    }
}

impl Default for PortServices {
    fn default() -> Self {
        Self::global()
    }
}

impl std::fmt::Debug for PortServices {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PortServices").finish_non_exhaustive()
    }
}
