//! Rust port of PSI StreamDevice (562dd5a).
//!
//! One module per C source, so each remaining C file ports on its own:
//!
//! | C source                                 | module                 |
//! |------------------------------------------|------------------------|
//! | `StreamBuffer.cc/.h`                     | [`buffer`]             |
//! | `StreamError.cc/.h`                      | [`error`]              |
//! | `StreamProtocol.cc/.h`                   | [`protocol`]           |
//! | `StreamFormat.h`                         | [`format`](mod@format) |
//! | `StreamFormatConverter.h` (+ base class) | [`converter`]          |
//! | `StreamBusInterface.h/.cc`               | [`bus`]                |
//! | `StreamCore.h`                           | [`core`]               |
//! | `StreamEpics.cc`, `devStream.h`          | [`epics`]              |
//!
//! [`cstr`] holds the C string and `<stdlib.h>` behaviour the sources
//! depend on.

pub mod buffer;
pub mod bus;
pub mod converter;
pub mod core;
pub mod cstr;
pub mod epics;
pub mod error;
pub mod format;
pub mod protocol;

pub use buffer::StreamBuffer;
