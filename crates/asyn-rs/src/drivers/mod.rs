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
// Both embedded targets take the POSIX backend, and neither needs a gate here:
// what a target can differ about is the termios ABI and the individual
// facilities, and `serial_port::platform` owns both. C asyn agrees on the
// shape — `drvAsynSerialPort.c` carries no `__rtems__` branch at all, so RTEMS
// takes the plain POSIX path there too.
//
// VxWorks does *not* take the transport C uses: C predates VxWorks 7's POSIX
// termios and drives the line through `ioctl(SIO_HW_OPTS_SET)` on a fake
// one-field struct (`drvAsynSerialPort.c:43-62`). RTEMS's `libc` binding is
// wrong rather than merely incomplete, so the ABI is declared in asyn-rs. The
// evidence for both decisions is on `serial_port::platform`.
#[cfg(unix)]
pub mod serial_port;

#[cfg(windows)]
#[path = "serial_port_win32.rs"]
pub mod serial_port;
