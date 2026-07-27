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
// VxWorks takes the POSIX backend. Its `libc` binds `struct termios` field
// for field against the VxWorks 7 RTP header (`NCCS == 20`, `VMIN == 16`,
// `c_ispeed`/`c_ospeed`, no `c_line`), so the only differences are individual
// facilities, and those live in `serial_port::platform` rather than here.
// Note this is *not* the transport C uses there — C predates VxWorks 7's
// POSIX termios and drives the line through `ioctl(SIO_HW_OPTS_SET)` on a
// fake one-field struct (`drvAsynSerialPort.c:43-62`); the reasoning for
// taking the newer path is on `serial_port::platform`.
//
// RTEMS still gets NEITHER, and that is an unported capability rather than a
// decision: C asyn builds a serial driver there too — `drvAsynSerialPort.c`
// carries no `__rtems__` branch at all, so RTEMS takes the plain POSIX path.
// What blocks it is `libc`, not RTEMS, in two separate ways:
//
//   * ABI. RTEMS's real `struct termios` (`sys/_termios.h:228-236`) is
//     `c_iflag`/`c_oflag`/`c_cflag`/`c_lflag`/`c_cc[NCCS]`/`c_ispeed`/
//     `c_ospeed` with `NCCS == 20` (`:78`). `libc`'s newlib binding — marked
//     "Unverified" in its own source — inserts a `c_line: cc_t` the target
//     does not have and gates `c_ispeed`/`c_ospeed` to espidf. The four flag
//     words still land at 0/4/8/12, so the damage is not obvious: `c_cc` is
//     displaced by one byte, and `cfsetispeed`/`cfsetospeed` write speeds
//     past the end of the Rust struct. (`NCCS` itself is right — libc has an
//     explicit RTEMS arm at 20 — which is what makes the shift so quiet.)
//   * Constants. Newlib binds not one of the termios flags, so the 102
//     errors a forced mount reports are all `E0425`/`E0531` on `CSIZE`,
//     `CS5`..`CS8`, `PARENB`, `CLOCAL`, `VMIN`, `TCSANOW` and the `B*`
//     ladder. That is the benign half: the build fails loudly instead of
//     silently taking Linux values for a target whose numbering is BSD
//     (`CS8 == 0x300`, `CSTOPB == 0x400`, `PARENB == 0x1000`,
//     `CLOCAL == 0x8000`, `B9600 == 9600` — `_termios.h:128-141`, `:193-208`).
//
// Note the termios *functions* are bound (they come from the shared
// `libc::unix` block, not newlib's), so a mount fails on constants and would
// then mis-execute on the struct. Closing this means asyn-rs declaring its
// own ABI-correct struct, constants and `extern "C"` block, or fixing the
// binding upstream — a second backend either way, so the module stays absent
// and a caller that wants it fails to compile rather than silently getting a
// driver that writes to the wrong offsets.
#[cfg(all(unix, not(target_os = "rtems")))]
pub mod serial_port;

#[cfg(windows)]
#[path = "serial_port_win32.rs"]
pub mod serial_port;
