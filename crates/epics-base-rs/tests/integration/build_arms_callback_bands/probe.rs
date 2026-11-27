//! A callback request is refused until the build arms the bands, as C's
//! are before `callbackInit` (`callback.c:360-363`).

use epics_base_rs::runtime::background::{CallbackBody, CallbackCell, CallbackError};
use epics_base_rs::runtime::task::{CallbackPriority, request_cell};

struct Body;

impl CallbackBody for Body {
    fn run(&self) {}
}

pub fn request() -> Result<(), CallbackError> {
    request_cell(CallbackPriority::Low, &CallbackCell::new(Body))
}
