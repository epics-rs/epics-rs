//! Rust port of PSI StreamDevice (562dd5a).
//!
//! One module per C source, so each remaining C file ports on its own:
//!
//! | C source                                   | module          |
//! |--------------------------------------------|-----------------|
//! | `StreamBuffer.cc/.h`                       | [`buffer`]      |
//! | `StreamError.cc/.h`                        | [`error`]       |
//!
//! [`cstr`] holds the C string and `<stdlib.h>` behaviour the sources
//! depend on.

pub mod buffer;
pub mod cstr;
pub mod error;

pub use buffer::StreamBuffer;
