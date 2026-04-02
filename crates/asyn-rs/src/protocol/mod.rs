#![allow(dead_code)]
#![allow(unused_imports)]
//! Pure-data protocol types for port communication.
//!
//! All types in this module are serializable and contain no trait objects,
//! closures, or channels. They define the external contract for port I/O.

pub mod command;
pub mod convert;
pub mod error;
pub mod event;
pub mod reply;
pub mod request;
pub mod status;
pub mod types;
pub mod value;

pub use command::PortCommand;
pub use error::ProtocolError;
pub use event::{EventFilter, EventPayload, PortEvent};
pub use reply::{PortReply, ReplyPayload};
pub use request::{PortRequest, ProtocolPriority, RequestMeta};
pub use status::ReplyStatus;
pub use value::{AlarmMeta, ParamValue, Timestamp};
