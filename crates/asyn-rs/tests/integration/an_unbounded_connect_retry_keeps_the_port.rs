//! A driver whose `seconds_between_port_connect` is past the clock's range has
//! a port that never retries on its own. C's
//! `epicsTimerStartDelay(connectTimer, secondsBetweenPortConnect)`
//! (asynManager.c:3312) retries at once instead, because
//! `epicsTimeAddSeconds` (epicsTime.cpp:434-442) wraps a delay that large.
//! Either way the port thread must outlive the failed connect that arms it.

use std::time::Duration;

use asyn_rs::error::{AsynError, AsynResult, AsynStatus};
use asyn_rs::port::{InterfaceMask, PortDriver, PortDriverBase, PortFlags};
use asyn_rs::runtime::{RuntimeConfig, create_port_runtime_unclaimed};
use asyn_rs::user::AsynUser;

struct Unreachable(PortDriverBase);

impl PortDriver for Unreachable {
    fn base(&self) -> &PortDriverBase {
        &self.0
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.0
    }
    fn connect(&mut self, _: &AsynUser) -> AsynResult<()> {
        Err(AsynError::Status {
            status: AsynStatus::Error,
            message: "link down".into(),
        })
    }
}

#[test]
fn an_unbounded_retry_interval_leaves_the_port_serving() {
    let mut base = PortDriverBase::new(
        "UNBOUNDEDRETRY",
        1,
        PortFlags::default(),
        InterfaceMask::NONE,
        InterfaceMask::NONE,
    );
    base.init_auto_connect(true);
    base.seconds_between_port_connect = Duration::MAX;
    // Registration makes the port's first connect attempt, which fails and
    // arms the retry, before the port takes any request.
    let (rt, _jh) = create_port_runtime_unclaimed(Unreachable(base), RuntimeConfig::default())
        .expect("the port runtime thread must start");

    let connected = rt
        .port_handle()
        .is_connected_blocking(-1)
        .expect("the port answers after its failed connect");
    assert!(!connected, "the device never came up");
}
