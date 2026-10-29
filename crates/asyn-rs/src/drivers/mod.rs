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
// RTEMS takes the POSIX backend and needs no gate here: what a target can
// differ about is the termios ABI, and `serial_port::platform` owns it. C asyn
// agrees on the shape — `drvAsynSerialPort.c` carries no `__rtems__` branch at
// all, so RTEMS takes the plain POSIX path there too.
//
// RTEMS's `libc` binding is wrong rather than merely incomplete, so the ABI is
// declared in asyn-rs. The evidence is on `serial_port::platform`.
#[cfg(unix)]
pub mod serial_port;

#[cfg(windows)]
#[path = "serial_port_win32.rs"]
pub mod serial_port;

/// The whole-millisecond interval a wait budget maps to, for the serial drivers'
/// `poll` and `SetCommTimeouts`; a socket's `poll` takes the same rule from
/// `epics_libcom_rs::runtime::socket::poll_ready`.
///
/// Two different zeros arrive here and collapsing them is the bug this owner
/// exists to prevent:
///
/// * a **remaining** budget of zero is an expired deadline. It stays zero — a
///   non-blocking probe — so the retry loop above it can retire the call
///   instead of waiting another tick on a timeout that has already run out.
/// * a **positive** budget under a millisecond is a caller who asked to wait.
///   It rounds up to 1 ms, because truncating it to zero turns
///   `caput $(P)$(R).TMOT 0.0005` into a read that never waits at all and
///   alarms on every scan against a device answering in 300 us.
///
/// C reaches the same place from the other side: `readIt`/`writeIt` compute
/// `pollmsec = (int)(timeout * 1000.0)` and then `if (pollmsec == 0) pollmsec =
/// 1` (`drvAsynIPPort.c:792-794`, `:662-664`). Rounding every positive budget
/// up is that floor generalised — C only ever sees the caller's timeout here,
/// whereas the deadline loops also feed this a remainder. Each of those C
/// triples ends `if (pollmsec < 0) pollmsec = -1`, the wait-forever case a
/// `Duration` cannot carry and this function therefore never sees.
///
/// This is deliberately *not*
/// [`ip_port::socket_poll_timeout`], which owns
/// the other half: a `timeout == 0` *caller request* becoming C's 1 ms poll on
/// the IP transports. That one is about what the caller asked for and yields a
/// `Duration` for the socket-option sites; this one is about what is left to
/// wait and yields milliseconds. Merging them is exactly the collapse
/// described above — and the serial driver's own `timeout == 0` is C's
/// `VMIN=0, VTIME=0` non-blocking read (`drvAsynSerialPort.c:902-905`), not
/// the IP driver's 1 ms poll.
///
/// The `u128` return is `Duration::as_millis`'s own width; each caller clamps
/// it to whatever its API takes (`c_int` for `poll`, `DWORD` for
/// `SetCommTimeouts`).
pub(crate) fn wait_millis(budget: std::time::Duration) -> u128 {
    if budget.is_zero() {
        0
    } else {
        budget.as_millis().max(1)
    }
}

/// An OS error's own text — C's `strerror(errno)` and
/// `epicsSocketConvertErrnoToString` — without std's ` (os error N)`.
pub(crate) fn errno_text(e: &std::io::Error) -> String {
    let text = e.to_string();
    match e.raw_os_error() {
        Some(code) => match text.strip_suffix(&format!(" (os error {code})")) {
            Some(bare) => bare.to_owned(),
            None => text,
        },
        None => text,
    }
}

/// A serial transfer failure in C's words, `"<device> <op> error: <why>"` as
/// asynError, for an OS error a read, write or flush surfaced as
/// [`crate::error::AsynError::Io`]; a write keeps the count it moved first.
/// `why` is the backend's: POSIX prints `strerror(errno)`
/// (drvAsynSerialPort.c:835, :957), Win32 the `GetLastError()` number
/// (drvAsynSerialPortWin32.c:523, :621, :677).
pub(crate) fn transfer_failure(
    e: crate::error::AsynError,
    device: &str,
    op: &str,
    why: fn(&std::io::Error) -> String,
) -> crate::error::AsynError {
    use crate::error::{AsynError, AsynStatus};
    match e {
        AsynError::Io(err) => AsynError::Status {
            status: AsynStatus::Error,
            message: format!("{device} {op} error: {}", why(&err)),
        },
        AsynError::PartialWrite { source, nbytes } => {
            transfer_failure(*source, device, op, why).with_partial_write(nbytes)
        }
        other => other,
    }
}

/// C-Win32's `%ld` of `GetLastError()`.
#[cfg(any(windows, test))]
pub(crate) fn last_error_code(e: &std::io::Error) -> String {
    e.raw_os_error().unwrap_or(0).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::port::PortDriver;
    use std::time::Duration;

    /// The boundary the owner exists to hold: the two zeros stay apart, and
    /// nothing positive rounds down into the expired one.
    /// No production C asyn driver passes `ASYN_DESTRUCTIBLE` to `registerPort`
    /// at pin `e2a281e2`. The flag occurs in exactly six places there — its
    /// definition (asynDriver.h:97), its two consumers (asynManager.c:2098 in
    /// `registerPort`, :2273 in `shutdownPort`), the `asynPortDriver` base that
    /// forwards a caller's `asynFlags` (asynPortDriver.cpp:3996), and two *test*
    /// drivers (testAsynPortDriver.cpp:54, asynPortDriverTest.cpp:58) — plus
    /// documentation. Every real transport registers without it, so a port that
    /// grants `PortManager::shutdown_port` rights grants what C refuses.
    ///
    /// The attribute argument each C original actually passes:
    ///
    /// | port | C original | attributes |
    /// |---|---|---|
    /// | FTDI | drvAsynFTDIPort.cpp:586-587 | `ASYN_CANBLOCK` |
    /// | IP | drvAsynIPPort.c:1082-1083 | `ASYN_CANBLOCK` |
    /// | IP server (listener) | drvAsynIPServerPort.c:646-647 | `ASYN_CANBLOCK` |
    /// | IP server (child) | drvAsynIPServerPort.c:711 → drvAsynIPPortConfigure | `ASYN_CANBLOCK` |
    /// | serial | drvAsynSerialPort.c:1101-1102 | `ASYN_CANBLOCK` |
    /// | serial (win32) | drvAsynSerialPortWin32.c:774-775 | `ASYN_CANBLOCK` |
    /// | USBTMC | drvAsynUSBTMC.c:1273-1274 | `ASYN_CANBLOCK` |
    /// | VXI-11 | drvVxi11.c:1759-1762 | `ASYN_CANBLOCK` (`\| ASYN_MULTIDEVICE` unless single-link) |
    /// | Prologix GPIB | drvPrologixGPIB.c:592-593 | `ASYN_CANBLOCK \| ASYN_MULTIDEVICE` |
    ///
    /// `null_port` has no C original; it declares itself a mirror of
    /// drvAsynIPPort, so it inherits that row.
    ///
    /// The win32 serial port is not compiled on this host (`#[cfg(windows)]`,
    /// drivers/mod.rs:32-34), so it is fixed but not covered here.
    #[test]
    fn no_ported_driver_grants_the_shutdown_rights_c_withholds() {
        // Collect rather than assert per port: a first-failure panic would hide
        // how far the family reaches, and the family is the point.
        let mut granted: Vec<&str> = Vec::new();
        let mut check = |what: &'static str, d: &dyn PortDriver| {
            if d.base().flags.destructible {
                granted.push(what);
            }
        };

        let server =
            ip_server_port::DrvAsynIPServerPort::new("SRV", "127.0.0.1:0 tcp").expect("server");
        let child = server.make_subport(0).expect("subport 0");

        check(
            "ftdi",
            &ftdi::DrvAsynFtdiPort::configure("FTDI", 0x0403, 0x6001, 9600, 1, 0, true, false, 0)
                .expect("ftdi"),
        );
        check(
            "ip_port",
            &ip_port::DrvAsynIPPort::new("IP", "127.0.0.1:1234 TCP").expect("ip"),
        );
        check("ip_server_port (listener)", &server);
        check("ip_server_port (child)", &child);
        check("null_port", &null_port::NullOctetPort::new("NULLP"));
        check(
            "prologix",
            &prologix::DrvAsynPrologixPort::new("GPIB", "127.0.0.1:1234", true).expect("prologix"),
        );
        check(
            "serial_port",
            &serial_port::DrvAsynSerialPort::new("SER", "/dev/null").expect("serial"),
        );
        check(
            "usbtmc",
            &usbtmc::DrvAsynUsbtmcPort::configure("TMC", 0x0957, 0x1755, "", 0, 1).expect("usbtmc"),
        );
        check(
            "vxi11",
            &vxi11::DrvVxi11Port::configure("VXI", "127.0.0.1", 0, "", "inst0", 0, true)
                .expect("vxi11"),
        );

        assert!(
            granted.is_empty(),
            "these ports grant ASYN_DESTRUCTIBLE where their C original withholds it: {granted:?}"
        );
    }

    #[test]
    fn wait_millis_separates_an_expired_budget_from_a_sub_millisecond_one() {
        assert_eq!(wait_millis(Duration::ZERO), 0);
        assert_eq!(wait_millis(Duration::from_nanos(1)), 1);
        assert_eq!(wait_millis(Duration::from_micros(500)), 1);
        assert_eq!(wait_millis(Duration::from_millis(1)), 1);
        assert_eq!(wait_millis(Duration::from_micros(1500)), 1);
        assert_eq!(wait_millis(Duration::from_millis(250)), 250);
    }

    /// C-Win32 prints the bare `GetLastError()` number, `"%s read error: %ld"`
    /// / `"%s write error: %ld"` / `"%s flush error: %ld"`
    /// (drvAsynSerialPortWin32.c:621, :523, :677); a write keeps its count and
    /// a timeout passes through with no text (:535, :644).
    #[test]
    fn a_win32_transfer_failure_is_worded_by_its_last_error_number() {
        use crate::error::{AsynError, AsynStatus};
        let denied = || AsynError::Io(std::io::Error::from_raw_os_error(5));
        let e = transfer_failure(denied(), "COM1", "read", last_error_code);
        assert_eq!(e.status(), AsynStatus::Error);
        assert_eq!(e.message(), "COM1 read error: 5");
        let e = transfer_failure(
            denied().with_partial_write(3),
            "COM1",
            "write",
            last_error_code,
        );
        assert_eq!(e.message(), "COM1 write error: 5");
        assert_eq!(e.partial_write(), Some(3));
        let e = transfer_failure(denied(), "COM1", "flush", last_error_code);
        assert_eq!(e.message(), "COM1 flush error: 5");
        let timeout = AsynError::Status {
            status: AsynStatus::Timeout,
            message: String::new(),
        }
        .with_partial_write(2);
        let e = transfer_failure(timeout, "COM1", "write", last_error_code);
        assert_eq!(e.status(), AsynStatus::Timeout);
        assert_eq!(e.message(), "");
        assert_eq!(e.partial_write(), Some(2));
    }
}
