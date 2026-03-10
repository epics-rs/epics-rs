//! Pure-data protocol types for port communication.
//!
//! All types in this module are serializable and contain no trait objects,
//! closures, or channels. They define the external contract for port I/O.

pub mod status;
pub mod value;
pub mod types;
pub mod command;
pub mod reply;
pub mod request;
pub mod event;
pub mod error;
pub mod convert;

pub use command::PortCommand;
pub use reply::{PortReply, ReplyPayload};
pub use request::{PortRequest, RequestMeta, ProtocolPriority};
pub use event::{PortEvent, EventPayload, EventFilter};
pub use error::ProtocolError;
pub use status::ReplyStatus;
pub use value::{AlarmMeta, ParamValue, Timestamp};
