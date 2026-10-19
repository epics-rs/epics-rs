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
//! | `StreamFormatConverter.cc` (Std*)        | [`std_converters`]     |
//! | `BCDConverter.cc`                        | [`bcd_converter`]      |
//! | `RegexpConverter.cc`                     | [`regexp_converter`]   |
//! | `TimestampConverter.cc`                  | [`timestamp_converter`] |
//! | `StreamBusInterface.h/.cc`               | [`bus`]                |
//! | `StreamCore.h`                           | [`core`]               |
//! | `StreamEpics.cc`, `devStream.h`          | [`epics`]              |
//! | `AsynDriverInterface.cc`                 | [`asyn`]               |
//! | `dev*Stream.c`, `stream-base.dbd`        | [`dev`]                |
//! | `EnumConverter.cc`                       | [`enum_converter`]     |
//! | `RawConverter.cc`                        | [`raw_converter`]      |
//! | `MantissaExponentConverter.cc`           | [`mantissa_exponent_converter`] |
//! | `ChecksumConverter.cc`                   | [`checksum`]           |
//! | `BinaryConverter.cc`                     | [`binary`]             |
//! | `RawFloatConverter.cc`                   | [`raw_float_converter`] |
//! | `DebugInterface.cc`                      | [`debug_interface`]    |
//! | `DummyInterface.cc`                      | [`dummy`]              |
//! | PCRE 8.45 (`pcre_*.c`)                   | [`pcre`]               |
//!
//! [`cstr`] holds the C string and `<stdlib.h>` behaviour the sources
//! depend on, [`cprintf`] the glibc printf of the standard converters.

pub mod asyn;
pub mod bcd_converter;
pub mod binary;
pub mod buffer;
pub mod bus;
pub mod checksum;
pub mod converter;
pub mod core;
pub mod cprintf;
pub mod cstr;
pub mod debug_interface;
pub mod dev;
pub mod dummy;
pub mod enum_converter;
pub mod epics;
pub mod error;
pub mod format;
pub mod mantissa_exponent_converter;
pub mod pcre;
pub mod protocol;
pub mod raw_converter;
pub mod raw_float_converter;
pub mod regexp_converter;
pub mod std_converters;
pub mod timestamp_converter;

pub use buffer::StreamBuffer;
