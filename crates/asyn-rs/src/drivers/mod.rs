pub mod ftdi;
pub mod ip_port;
pub mod ip_server_port;
pub mod null_port;
pub mod option_parse;
pub mod prologix;
pub mod serial_config;
pub mod usbtmc;
pub mod vxi11;

// Serial support has a POSIX termios backend (`serial_port.rs`) and a Win32
// DCB backend (`serial_port_win32.rs`), mirroring C asyn's split between
// `drvAsynSerialPort.c` and `drvAsynSerialPortWin32.c`. Both expose the same
// `serial_port` module path so callers (iocsh, the port registry) need no
// per-platform gating — the build selects one, like C's `OS_CLASS` Makefile
// switch.
//
// RTEMS and VxWorks get NEITHER, and this is an unported capability rather
// than a decision: C asyn builds a serial driver for both. `not(...)` is on
// the unix arm because `cfg(unix)` matches those two triples as well, and
// what it hands them is a Linux/BSD termios backend their `libc` bindings
// cannot satisfy — 109 missing items on armv7-rtems-eabihf (`CSIZE`, `CS5`..
// `CS8`, `PARENB`, the whole `B*` baud ladder: newlib declares `struct
// termios` but the Rust bindings expose none of the constants) and 17 on
// x86_64-wrs-vxworks. The VxWorks 17 are precisely the symbols C itself
// `#ifdef vxWorks`-branches around, because VxWorks drives a serial line
// through `ioctl` rather than termios: `drvAsynSerialPort.c:55-59` declares
// its own *fake* termios struct there, sets baud with
// `ioctl(FIOBAUDRATE/SIO_BAUD_SET)` (:107-114), reads flow control back with
// `ioctl(FIOGETOPTIONS) & OPT_TANDEM` (:183-186), and substitutes `CLOCAL`
// for the `CRTSCTS` the target lacks (:174-176). Porting that is a backend,
// not a gate, so the module is absent here until one exists — and a caller
// that wants it must fail to compile rather than silently get a driver whose
// `connect()` cannot work.
#[cfg(all(unix, not(epics_embedded_target)))]
pub mod serial_port;

#[cfg(windows)]
#[path = "serial_port_win32.rs"]
pub mod serial_port;
