//! Serial port driver — Win32 backend (`drvAsynSerialPortWin32.c` equivalent).
//!
//! This is the Windows counterpart of the POSIX termios backend in
//! `serial_port.rs`. C asyn ships two device files selected by the Makefile's
//! `OS_CLASS` switch (`drvAsynSerialPort.c` for POSIX, `drvAsynSerialPortWin32.c`
//! for `WIN32`); this module mirrors that split. Both Rust files expose the
//! same [`DrvAsynSerialPort`] type through the `drivers::serial_port` module
//! path, so iocsh and the port registry need no per-platform gating.
//!
//! Line I/O uses the raw Win32 Comm API (`CreateFileW` + `DCB` +
//! `COMMTIMEOUTS` + `ReadFile`/`WriteFile`/`PurgeComm`), the Windows analogue
//! of the POSIX `open`/`termios`/`read`/`write`/`tcflush` the unix backend
//! uses. The shared spec grammar and option names live in
//! [`super::serial_config`] so the two backends cannot drift.

use std::time::{Duration, Instant};

use windows_sys::Win32::Devices::Communication::{
    COMMTIMEOUTS, ClearCommBreak, DCB, EVENPARITY, GetCommState, NOPARITY, ODDPARITY, ONESTOPBIT,
    PURGE_RXCLEAR, PurgeComm, SetCommBreak, SetCommState, SetCommTimeouts, TWOSTOPBITS,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FlushFileBuffers, OPEN_EXISTING, ReadFile, WriteFile,
};

use crate::error::{AsynError, AsynResult, AsynStatus};
use crate::interpose::{EomReason, OctetNext, OctetReadResult};
use crate::port::{PortDriver, PortDriverBase, PortFlags};
use crate::trace::TraceMask;
use crate::user::AsynUser;
use crate::{asyn_trace, asyn_trace_io};

use super::option_parse::{
    bad_number, break_action, option_key, option_value, parse_yn_option, sscanf_int,
};
use super::serial_config::{DataBits, FlowControl, Parity, SerialConfig, StopBits};
use super::wait_millis;

// --- DCB bitfield helpers ---
//
// windows-sys exposes the DCB flow-control flags as a single packed `u32`
// (`DCB._bitfield`) with no field accessors, so we set/get the bits by hand.
// Layout is fixed by `winbase.h` (`DCB`): fBinary:1, fParity:1, fOutxCtsFlow:1,
// fOutxDsrFlow:1, fDtrControl:2, fDsrSensitivity:1, fTXContinueOnXoff:1,
// fOutX:1, fInX:1, fErrorChar:1, fNull:1, fRtsControl:2, fAbortOnError:1.
mod dcb_bits {
    pub const F_BINARY: u32 = 1 << 0;
    pub const F_OUTX_CTS_FLOW: u32 = 1 << 2;
    pub const F_OUTX_DSR_FLOW: u32 = 1 << 3;
    pub const F_DTR_CONTROL_SHIFT: u32 = 4;
    pub const F_DTR_CONTROL_MASK: u32 = 0b11 << 4;
    pub const F_DSR_SENSITIVITY: u32 = 1 << 6;
    pub const F_OUT_X: u32 = 1 << 8;
    pub const F_IN_X: u32 = 1 << 9;
    pub const F_RTS_CONTROL_SHIFT: u32 = 12;
    pub const F_RTS_CONTROL_MASK: u32 = 0b11 << 12;

    // 2-bit fDtrControl / fRtsControl values (winbase.h DTR_CONTROL_* /
    // RTS_CONTROL_*).
    pub const DTR_CONTROL_ENABLE: u32 = 1;
    pub const DTR_CONTROL_HANDSHAKE: u32 = 2;
    pub const RTS_CONTROL_ENABLE: u32 = 1;
    pub const RTS_CONTROL_HANDSHAKE: u32 = 2;

    pub fn set_flag(bf: &mut u32, mask: u32, on: bool) {
        if on {
            *bf |= mask;
        } else {
            *bf &= !mask;
        }
    }

    pub fn get_flag(bf: u32, mask: u32) -> bool {
        bf & mask != 0
    }

    pub fn set_field(bf: &mut u32, shift: u32, mask: u32, val: u32) {
        *bf = (*bf & !mask) | ((val << shift) & mask);
    }
}

/// Build the wide (UTF-16, NUL-terminated) device name Win32 `CreateFileW`
/// wants. C `drvAsynSerialPortConfigure` prepends `\\.\` unless the caller
/// already gave a `\\.\`-prefixed device name (needed to reach `COM10` and
/// above). Mirror that exactly.
fn device_to_wide(device: &str) -> Vec<u16> {
    const PREFIX: &[u8] = br"\\.\";
    let already_prefixed = device.len() >= PREFIX.len()
        && device.as_bytes()[..PREFIX.len()].eq_ignore_ascii_case(PREFIX);
    let full = if already_prefixed {
        device.to_string()
    } else {
        format!(r"\\.\{device}")
    };
    full.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Apply the configured line state (baud/bits/parity/stop/flow) to a `DCB`.
/// Shared by `connect` (initial setup) and the empty-key `set_option` re-apply,
/// the single source of the configured state (analogue of the unix backend's
/// `build_configured_termios`).
fn apply_config_to_dcb(config: &SerialConfig, dcb: &mut DCB) {
    use dcb_bits::*;

    dcb.BaudRate = config.baud;
    dcb.ByteSize = match config.data_bits {
        DataBits::Five => 5,
        DataBits::Six => 6,
        DataBits::Seven => 7,
        DataBits::Eight => 8,
    };
    dcb.Parity = match config.parity {
        Parity::None => NOPARITY,
        Parity::Even => EVENPARITY,
        Parity::Odd => ODDPARITY,
    };
    dcb.StopBits = match config.stop_bits {
        StopBits::One => ONESTOPBIT,
        StopBits::Two => TWOSTOPBITS,
    };
    // fBinary must be TRUE on Windows — non-binary mode is unsupported and
    // SetCommState fails without it.
    set_flag(&mut dcb._bitfield, F_BINARY, true);
    // XON/XOFF flow characters default to ^Q (0x11) / ^S (0x13), matching the
    // unix backend's VSTART/VSTOP seeds.
    dcb.XonChar = 0x11;
    dcb.XoffChar = 0x13;

    let bf = &mut dcb._bitfield;
    match config.flow_control {
        FlowControl::None => {
            set_flag(bf, F_OUTX_CTS_FLOW, false);
            set_field(
                bf,
                F_RTS_CONTROL_SHIFT,
                F_RTS_CONTROL_MASK,
                RTS_CONTROL_ENABLE,
            );
            set_flag(bf, F_OUT_X, false);
            set_flag(bf, F_IN_X, false);
        }
        FlowControl::Hardware => {
            set_flag(bf, F_OUTX_CTS_FLOW, true);
            set_field(
                bf,
                F_RTS_CONTROL_SHIFT,
                F_RTS_CONTROL_MASK,
                RTS_CONTROL_HANDSHAKE,
            );
            set_flag(bf, F_OUT_X, false);
            set_flag(bf, F_IN_X, false);
        }
        FlowControl::Software => {
            set_flag(bf, F_OUTX_CTS_FLOW, false);
            set_field(
                bf,
                F_RTS_CONTROL_SHIFT,
                F_RTS_CONTROL_MASK,
                RTS_CONTROL_ENABLE,
            );
            set_flag(bf, F_OUT_X, true);
            set_flag(bf, F_IN_X, true);
        }
    }
}

/// The line state the next connect must push.
///
/// One rule, both connects: the cache when the port already has one, and
/// otherwise the device's own `DCB` with the configure string folded in. The
/// fold happens exactly once per port, which is what lets an option
/// [`SerialConfig`] cannot express — `clocal`, `ixon`, `ixoff`, or `crtscts`
/// and `ixon` at the same time — survive a disconnect/reconnect. Folding on
/// every connect is what reverted them.
fn line_state_for_connect(cache: Option<DCB>, device: DCB, config: &SerialConfig) -> DCB {
    match cache {
        Some(cached) => cached,
        None => {
            let mut seed = device;
            apply_config_to_dcb(config, &mut seed);
            seed
        }
    }
}

fn io_err() -> AsynError {
    // On Windows `std::io::Error::last_os_error()` reads GetLastError, matching
    // the unix backend's use of the same call after a failed syscall.
    AsynError::Io(std::io::Error::last_os_error())
}

// --- I/O state ---

struct SerialIoStateWin32 {
    /// Open comm handle stored as the numeric value (`isize`) rather than the
    /// raw `HANDLE` pointer so the driver stays `Send + Sync` (the `PortDriver`
    /// bound), mirroring the unix backend's `Option<RawFd>`.
    handle_val: Option<isize>,
    /// Cumulative bytes read / written, for `report()` diagnostics
    /// (C `tty->nRead` / `tty->nWritten`).
    n_read: u64,
    n_written: u64,
    /// Last timeout pushed via `SetCommTimeouts`; re-applied only when the
    /// per-request timeout changes (C readIt caches `tty->readTimeout`).
    last_timeout: Option<Duration>,
    /// C `tty->break_active` (`drvAsynSerialPortWin32.c:64`). A Win32 BREAK is
    /// LATCHED, not momentary: `setOption("break","on")` leaves the line
    /// asserted and only `"off"` or a timed form clears it. Both the setter
    /// and the `getOption` readback derive from this one flag, as they do in
    /// C — without it `"off"` had nothing to clear and the readback had
    /// nothing to report.
    break_active: bool,
}

impl SerialIoStateWin32 {
    fn new() -> Self {
        Self {
            handle_val: None,
            n_read: 0,
            n_written: 0,
            last_timeout: None,
            break_active: false,
        }
    }

    fn handle(&self) -> Option<HANDLE> {
        self.handle_val.map(|v| v as HANDLE)
    }

    fn handle_or_err(&self) -> AsynResult<HANDLE> {
        self.handle().ok_or_else(|| AsynError::Status {
            status: AsynStatus::Disconnected,
            message: "serial port not open".into(),
        })
    }

    /// Program the comm timeouts from the request timeout, matching C readIt's
    /// `SetCommTimeouts` policy. `timeout == 0` returns immediately with
    /// whatever is already buffered; `timeout > 0` bounds the total read/write
    /// by that many milliseconds.
    fn apply_timeouts(&mut self, handle: HANDLE, timeout: Duration) -> AsynResult<()> {
        if self.last_timeout == Some(timeout) {
            return Ok(());
        }
        let mut ct: COMMTIMEOUTS = unsafe { std::mem::zeroed() };
        if timeout.is_zero() {
            // MAXDWORD interval with zero totals = return immediately.
            ct.ReadIntervalTimeout = u32::MAX;
            ct.ReadTotalTimeoutMultiplier = 0;
            ct.ReadTotalTimeoutConstant = 0;
            ct.WriteTotalTimeoutMultiplier = 0;
            // 0 would mean "no write timeout" (block forever); 1ms keeps a
            // zero-timeout write near-immediate like the unix single-attempt
            // path.
            ct.WriteTotalTimeoutConstant = 1;
        } else {
            let ms = wait_millis(timeout).min(u32::MAX as u128 - 1) as u32;
            ct.ReadIntervalTimeout = ms;
            ct.ReadTotalTimeoutMultiplier = 1;
            ct.ReadTotalTimeoutConstant = ms;
            ct.WriteTotalTimeoutMultiplier = 1;
            ct.WriteTotalTimeoutConstant = ms;
        }
        if unsafe { SetCommTimeouts(handle, &ct) } == 0 {
            return Err(io_err());
        }
        self.last_timeout = Some(timeout);
        Ok(())
    }
}

impl OctetNext for SerialIoStateWin32 {
    fn read(&mut self, user: &AsynUser, buf: &mut [u8]) -> AsynResult<OctetReadResult> {
        let handle = self.handle_or_err()?;
        // C readIt (drvAsynSerialPortWin32.c): reject maxchars == 0 before
        // touching the device (same wording as the serial driver).
        if buf.is_empty() {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: "maxchars 0 Why <=0?".into(),
            });
        }
        self.apply_timeouts(handle, user.timeout)?;

        let want = buf.len().min(u32::MAX as usize) as u32;
        let mut n_read: u32 = 0;
        let ok = unsafe {
            ReadFile(
                handle,
                buf.as_mut_ptr(),
                want,
                &mut n_read,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            // A broken device surfaces as a ReadFile error rather than a
            // 0-byte return, but C-Win32 readIt reports it with the handle
            // still open — see `io_read_octet_eom` for why this backend does
            // not tear down on a failed read.
            return Err(io_err());
        }
        if n_read == 0 {
            // The comm timeout elapsed with no data. Unlike POSIX read()==0
            // (EOF/hangup) this is a timeout, so the connection stays open.
            return Err(AsynError::Status {
                status: AsynStatus::Timeout,
                message: "serial read timeout".into(),
            });
        }
        self.n_read += n_read as u64;
        Ok(OctetReadResult {
            nbytes_transferred: n_read as usize,
            // C parity: CNT only when the requested count was reached.
            eom_reason: if n_read as usize >= buf.len() {
                EomReason::CNT
            } else {
                EomReason::empty()
            },
        })
    }

    fn write(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
        let handle = self.handle_or_err()?;
        // C writeIt: numchars == 0 is a no-op success.
        if data.is_empty() {
            return Ok(0);
        }
        self.apply_timeouts(handle, user.timeout)?;

        // Bound the TOTAL write by one deadline (matching the unix backend and
        // C writeIt's single pre-loop timer), while WriteFile's own comm
        // timeout bounds each call.
        let deadline = Instant::now() + user.timeout;
        // C parity (drvAsynSerialPortWin32.c writeIt, drvAsynSerialPort.c:849):
        // the bytes the port accepted are reported alongside the failing status,
        // never dropped — carry `total` out on the error via
        // `with_partial_write` so `asynRecord`'s NAWT can show it.
        let mut total = 0usize;
        while total < data.len() {
            let chunk = &data[total..];
            let want = chunk.len().min(u32::MAX as usize) as u32;
            let mut n_written: u32 = 0;
            let ok = unsafe {
                WriteFile(
                    handle,
                    chunk.as_ptr(),
                    want,
                    &mut n_written,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(io_err().with_partial_write(total));
            }
            total += n_written as usize;
            self.n_written += n_written as u64;
            if total >= data.len() {
                break;
            }
            // WriteFile returned fewer than requested → its comm timeout fired,
            // or the deadline passed. C writeIt breaks with asynTimeout here.
            if n_written == 0 || Instant::now() >= deadline {
                return Err(AsynError::Status {
                    status: AsynStatus::Timeout,
                    message: "serial write timeout".into(),
                }
                .with_partial_write(total));
            }
        }
        Ok(total)
    }

    fn flush(&mut self, _user: &mut AsynUser) -> AsynResult<()> {
        // C-Win32 `flushIt` opens on the same `commHandle ==
        // INVALID_HANDLE_VALUE` test as `getOption` (:97), `setOption` (:178),
        // `writeIt` (:494) and `readIt` (:566), and REFUSES the flush
        // (`drvAsynSerialPortWin32.c:668-672`) rather than reporting success
        // over a device that is not there. Taking the same
        // `handle_or_err` as `read` and `write` above is what keeps the closed
        // handle from meaning "nothing to flush" here and "refuse" everywhere
        // else on the same port.
        //
        // This does NOT hold for the POSIX backend and must not be flattened
        // onto it: `drvAsynSerialPort.c::flushIt` (:986-1001) is
        // `if (tty->fd >= 0) tcflush(...)` and always returns `asynSuccess`.
        // The asymmetry between C's two device files is C's own.
        let handle = self.handle_or_err()?;
        // PurgeComm(PURGE_RXCLEAR) discards received-but-unread input — the
        // Win32 analogue of tcflush(TCIFLUSH).
        if unsafe { PurgeComm(handle, PURGE_RXCLEAR) } == 0 {
            return Err(io_err());
        }
        Ok(())
    }
}

// --- Driver ---

/// Serial port driver (Win32 backend).
pub struct DrvAsynSerialPort {
    base: PortDriverBase,
    config: SerialConfig,
    /// The configured line state, and the single owner of it.
    ///
    /// The POSIX backend states the rule this mirrors (`serial_port.rs`, the
    /// `termios` field): the cache owns every option the device representation
    /// can express — baud, bits, parity, stop, clocal, crtscts, ixon/ixoff —
    /// `set_option` mutates it, `connect` pushes it, `get_option` reads it, and
    /// nothing rebuilds the line state from [`SerialConfig`] again. That last
    /// clause is the fix: rebuilding from `SerialConfig` re-derived
    /// `fOutX`/`fInX` and the DSR/DTR handshake from a three-valued
    /// `flow_control`, so every reconnect silently reverted `clocal`, `ixon`
    /// and `ixoff` — a device pausing the host with XOFF was then overrun.
    ///
    /// `None` until the first connect. Unlike POSIX the seed is taken from the
    /// device rather than synthesised, because a `DCB` also carries fields no
    /// option of ours sets (`XonLim`/`XoffLim`, `ErrorChar`, `fAbortOnError`)
    /// whose defaults are the driver's to adopt, not to invent. Both option
    /// entry points are gated on the port being open (C-Win32 refuses every key
    /// on a closed handle), so nothing can observe the cache before it exists.
    dcb: Option<DCB>,
    io: SerialIoStateWin32,
}

impl DrvAsynSerialPort {
    /// Close the handle and mark the port disconnected so the actor's
    /// auto-reconnect re-opens it on the next request (C `closeConnection`).
    fn drop_connection(&mut self) {
        if let Some(v) = self.io.handle_val.take() {
            unsafe { CloseHandle(v as HANDLE) };
        }
        self.io.last_timeout = None;
        self.base.set_connected(false);
    }

    /// Create a new serial port driver. Starts disconnected with
    /// `auto_connect = true` and `can_block = true`.
    pub fn new(port_name: &str, config_str: &str) -> AsynResult<Self> {
        let config = SerialConfig::parse(config_str)?;
        let mut base = PortDriverBase::new(
            port_name,
            1,
            PortFlags {
                multi_device: false,
                can_block: true,
                destructible: true,
            },
        );
        base.init_connected(false);
        base.auto_connect = true;
        // C passes `interruptProcess = 1` to `pasynOctetBase->initialize`
        // (drvAsynSerialPortWin32.c:798): every successful octet read on this port fans out to
        // its octet interrupt users, which is what drives a SCAN="I/O Intr"
        // record. See `PortDriverBase::octet_interrupt_process`.
        base.octet_interrupt_process = true;

        Ok(Self {
            base,
            config,
            dcb: None,
            io: SerialIoStateWin32::new(),
        })
    }

    /// Configure a serial port the way C `drvAsynSerialPortConfigure` does:
    /// parse the device, honor `noAutoConnect`, and enable EOS processing by
    /// default unless `noProcessEos`. Identical policy to the unix backend.
    pub fn configure(
        port_name: &str,
        config_str: &str,
        no_auto_connect: bool,
        no_process_eos: bool,
    ) -> AsynResult<Self> {
        let mut driver = Self::new(port_name, config_str)?;
        if no_auto_connect {
            driver.base.auto_connect = false;
        }
        if !no_process_eos {
            driver.install_interpose(Box::new(crate::interpose::eos::EosInterpose::default()));
        }
        Ok(driver)
    }

    /// Push an interpose layer onto the octet I/O stack.
    pub fn install_interpose(&mut self, layer: Box<dyn crate::interpose::OctetInterpose>) {
        self.base.install_octet_interpose(layer);
    }

    /// Send a serial line BREAK condition. Win32 has no timed-break primitive,
    /// so assert `SetCommBreak`, sleep, then `ClearCommBreak` (the POSIX
    /// `tcsendbreak` analogue). `duration_tenths` is in tenths of a second;
    /// `<= 0` requests the conventional ~250 ms minimum.
    pub fn send_break(&self, duration_tenths: i32) -> AsynResult<()> {
        let handle = self.io.handle_or_err()?;
        if unsafe { SetCommBreak(handle) } == 0 {
            return Err(io_err());
        }
        let ms = if duration_tenths <= 0 {
            250
        } else {
            (duration_tenths as u64) * 100
        };
        std::thread::sleep(Duration::from_millis(ms));
        if unsafe { ClearCommBreak(handle) } == 0 {
            return Err(io_err());
        }
        Ok(())
    }

    /// Block until every byte the driver has queued has actually been
    /// transmitted (`FlushFileBuffers`, the Win32 `tcdrain` analogue).
    pub fn drain_output(&self) -> AsynResult<()> {
        let handle = self.io.handle_or_err()?;
        if unsafe { FlushFileBuffers(handle) } == 0 {
            return Err(io_err());
        }
        Ok(())
    }

    /// The disconnected gate both option entry points open with. C-Win32
    /// `getOption` (`drvAsynSerialPortWin32.c:96-101`) and `setOption`
    /// (`:180-185`) test `commHandle == INVALID_HANDLE_VALUE` *before* they
    /// look at the key, so on a closed port EVERY key — baud/bits/parity/stop/
    /// break as well as the flow-control set — reports `asynError` with the
    /// message `"<device> disconnected:"`, and no key may be cached, defaulted
    /// or stored. Calling this first is what makes "the handle is open" an
    /// invariant for the rest of both functions rather than a per-key check.
    fn check_option_connected(&self) -> AsynResult<()> {
        if self.io.handle_val.is_none() {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: format!("{} disconnected:", self.config.device),
            });
        }
        Ok(())
    }

    /// Read the device's DCB — used once per port, to seed [`Self::dcb`].
    fn read_dcb(handle: HANDLE) -> AsynResult<DCB> {
        let mut dcb: DCB = unsafe { std::mem::zeroed() };
        dcb.DCBlength = std::mem::size_of::<DCB>() as u32;
        if unsafe { GetCommState(handle, &mut dcb) } == 0 {
            return Err(io_err());
        }
        Ok(dcb)
    }

    /// The cache, which both option entry points may assume exists because
    /// their shared `check_option_connected` guard has already run.
    fn dcb_or_err(&self) -> AsynResult<&DCB> {
        self.dcb.as_ref().ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: format!("{} disconnected:", self.config.device),
        })
    }

    /// Push the cache to the device — the Win32 twin of the POSIX backend's
    /// `apply_options`, and the only place the cache reaches the line.
    fn apply_options(&self) -> AsynResult<()> {
        let handle = self.io.handle_or_err()?;
        let dcb = self.dcb_or_err()?;
        if unsafe { SetCommState(handle, dcb) } == 0 {
            return Err(io_err());
        }
        Ok(())
    }

    /// Mutate the cache and push it, the single owner of a DCB update used by
    /// every `set_option` key.
    ///
    /// The cache is rolled back when the device refuses the result, so
    /// `get_option` can never report a value the line rejected — C keeps the
    /// same `dcbPrev` snapshot (drvAsynSerialPortWin32.c:342-347) and the POSIX
    /// backend the same `termios_prev`.
    fn modify_dcb<F: FnOnce(&mut DCB)>(&mut self, f: F) -> AsynResult<()> {
        self.io.handle_or_err()?;
        let prev = *self.dcb_or_err()?;
        let mut next = prev;
        f(&mut next);
        self.dcb = Some(next);
        if let Err(e) = self.apply_options() {
            self.dcb = Some(prev);
            return Err(e);
        }
        Ok(())
    }
}

impl PortDriver for DrvAsynSerialPort {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }

    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }

    /// C drvAsynSerialPortWin32 registers the same set as the POSIX serial
    /// driver: asynCommon, asynOption, asynOctet.
    fn capabilities(&self) -> Vec<crate::interfaces::Capability> {
        crate::interfaces::octet_transport_capabilities()
    }

    fn connect(&mut self, _user: &AsynUser) -> AsynResult<()> {
        // C connectIt: reject a connect on an already-open link rather than
        // leaking the first handle.
        if self.io.handle_val.is_some() {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: format!("{}: Link already open!", self.base.port_name),
            });
        }

        let wide = device_to_wide(&self.config.device);
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0, // exclusive access (no sharing)
                std::ptr::null(),
                OPEN_EXISTING,
                0, // non-overlapped, no extra attributes
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io_err());
        }
        self.io.handle_val = Some(handle as isize);
        self.io.last_timeout = None;
        // The setup closure below clears any BREAK a prior IOC left asserted,
        // so the latch starts down on every fresh handle.
        self.io.break_active = false;

        // Any setup failure after CreateFile must close the handle:
        // base.connected is still false, so Drop would skip disconnect() and
        // leak it.
        let setup = (|| -> AsynResult<()> {
            // Clear a BREAK possibly left asserted by a prior ioc termination.
            unsafe { ClearCommBreak(handle) };
            if unsafe { FlushFileBuffers(handle) } == 0 {
                return Err(io_err());
            }
            // Apply the configured line state at connect (like the POSIX
            // backend), rather than leaving the DCB at its open default and
            // relying on later setOption calls as C-Win32 connectIt does.
            //
            // The configure string is folded in **once**, on the connect that
            // seeds the cache. Every later connect pushes the cache unchanged,
            // which is what lets an option the spec cannot express survive a
            // reconnect.
            let device = Self::read_dcb(handle)?;
            self.dcb = Some(line_state_for_connect(self.dcb, device, &self.config));
            self.apply_options()?;
            // Discard bytes buffered before the port was configured, so the
            // first read starts clean (unix tcflush(TCIFLUSH) analogue).
            unsafe { PurgeComm(handle, PURGE_RXCLEAR) };
            Ok(())
        })();
        if let Err(e) = setup {
            unsafe { CloseHandle(handle) };
            self.io.handle_val = None;
            return Err(e);
        }

        self.base.set_connected(true);
        asyn_trace!(
            Some(self.base.trace),
            &self.base.port_name,
            TraceMask::FLOW,
            "connected to {} at {} baud",
            self.config.device,
            self.dcb.map(|d| d.BaudRate).unwrap_or(self.config.baud)
        );
        Ok(())
    }

    fn disconnect(&mut self, _user: &AsynUser) -> AsynResult<()> {
        asyn_trace!(
            Some(self.base.trace),
            &self.base.port_name,
            TraceMask::FLOW,
            "disconnect"
        );
        if let Some(v) = self.io.handle_val.take() {
            unsafe { CloseHandle(v as HANDLE) };
        }
        self.io.last_timeout = None;
        self.base.set_connected(false);
        Ok(())
    }

    fn report(&self, out: &mut dyn std::fmt::Write, level: i32) {
        use std::fmt::Write as _;
        let _ = writeln!(
            out,
            "Serial line {}: {}",
            self.config.device,
            if self.base.is_connected() {
                "Connected"
            } else {
                "Disconnected"
            }
        );
        if level >= 1 {
            let _ = writeln!(
                out,
                "                commHandle: {:?}",
                self.io.handle().unwrap_or(std::ptr::null_mut())
            );
            let _ = writeln!(out, "    Characters written: {}", self.io.n_written);
            let _ = writeln!(out, "       Characters read: {}", self.io.n_read);
            // The level is passed through, as C's `asynPortDriver::report`
            // passes it to `reportParams` (asynPortDriver.cpp:3692).
            self.base.report_params(out, level);
        }
    }

    fn read_octet(&mut self, user: &AsynUser, buf: &mut [u8]) -> AsynResult<usize> {
        self.io_read_octet_eom(user, buf).map(|(n, _eom)| n)
    }

    /// The raw device read — the **bottom** of the port's octet chain, C
    /// `drvAsynSerialPortWin32.c::readIt` below the interposes the manager
    /// installed on top of it. The chain is run by the port
    /// ([`crate::port::octet_read_chain`]), not from here.
    ///
    /// A failed read leaves the handle open. C-Win32 `readIt` reaches no
    /// `closeConnection` on any exit — the file has exactly two call sites,
    /// the explicit `disconnect` (`drvAsynSerialPortWin32.c:470`) and `writeIt`
    /// (`:525`) — so a failing `ReadFile` reports `asynError` with the port
    /// still connected (`:596-640`). The asymmetry against `write_octet` below
    /// is the backend's, not an oversight: on Win32 a line error (framing,
    /// parity, overrun) also surfaces as a failed `ReadFile`, whereas the POSIX
    /// twin sees those as a short `read()` and reserves its errno branch for a
    /// dead device, which is why it does close on both directions
    /// (`drvAsynSerialPort.c:959`).
    fn io_read_octet_eom(
        &mut self,
        user: &AsynUser,
        buf: &mut [u8],
    ) -> AsynResult<(usize, EomReason)> {
        self.base.check_ready()?;
        let result = self.io.read(user, buf)?;
        asyn_trace_io!(
            Some(self.base.trace),
            &self.base.port_name,
            TraceMask::IO_DRIVER,
            &buf[..result.nbytes_transferred],
            "read"
        );
        Ok((result.nbytes_transferred, result.eom_reason))
    }

    fn write_octet(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
        self.base.check_ready()?;
        asyn_trace_io!(
            Some(self.base.trace),
            &self.base.port_name,
            TraceMask::IO_DRIVER,
            data,
            "write"
        );
        match self.io.write(user, data) {
            Ok(n) => Ok(n),
            Err(e) => {
                if e.is_fatal_transport() && self.base.is_connected() {
                    asyn_trace!(
                        Some(self.base.trace),
                        &self.base.port_name,
                        TraceMask::FLOW,
                        "write error, disconnecting: {e}"
                    );
                    self.drop_connection();
                }
                Err(e)
            }
        }
    }

    fn io_flush(&mut self, user: &mut AsynUser) -> AsynResult<()> {
        self.io.flush(user)
    }

    fn set_option(&mut self, _user: &mut AsynUser, key: &str, value: &str) -> AsynResult<()> {
        use dcb_bits::*;

        // C-Win32 setOption's opening guard (drvAsynSerialPortWin32.c:180-185):
        // a closed handle errors for every key, before any value is parsed or
        // stored. Unlike the POSIX backend — whose C twin mutates its cached
        // termios whether or not the port is open (R7-48) — Win32 refuses the
        // key outright, so the cache below is only ever reached with the port
        // open.
        self.check_option_connected()?;

        let key = option_key(key);
        // C-Win32 runs every option value through `epicsStrCaseCmp` too
        // (`drvAsynSerialPortWin32.c:211-308`); fold once here so no arm below
        // needs its own comparison.
        let value = option_value(value);
        let value = value.as_str();

        match key.as_str() {
            "baud" => {
                // C-Win32 `sscanf(val, "%d", &baud) != 1` -> "Bad number"
                // (drvAsynSerialPortWin32.c:192-198).
                let baud = sscanf_int(value).ok_or_else(bad_number)?;
                let baud = u32::try_from(baud).map_err(|_| bad_number())?;
                // Windows accepts an arbitrary BaudRate (no termios speed
                // table), matching C-Win32's direct dcb.BaudRate assignment.
                self.modify_dcb(|dcb| dcb.BaudRate = baud)?;
            }
            "bits" => {
                // C-Win32 parses this key with `sscanf("%d")` too
                // (drvAsynSerialPortWin32.c:201-208) — "Bad number" for a value
                // with no number in it. A number the DCB cannot carry (anything
                // but 5..8 here) is refused by SetCommConfig in C; refuse it up
                // front with the same text the POSIX backend uses.
                let byte_size: u8 = match sscanf_int(value).ok_or_else(bad_number)? {
                    5 => 5,
                    6 => 6,
                    7 => 7,
                    8 => 8,
                    _ => {
                        return Err(AsynError::Status {
                            status: AsynStatus::Error,
                            message: "Invalid number of bits.".into(),
                        });
                    }
                };
                self.modify_dcb(|dcb| dcb.ByteSize = byte_size)?;
            }
            "parity" => {
                // C-Win32 setOption accepts only none/odd/even.
                let dcb_parity = match value {
                    "none" => NOPARITY,
                    "even" => EVENPARITY,
                    "odd" => ODDPARITY,
                    _ => {
                        return Err(AsynError::Status {
                            status: AsynStatus::Error,
                            message: "Invalid parity.".into(),
                        });
                    }
                };
                self.modify_dcb(|dcb| dcb.Parity = dcb_parity)?;
            }
            "stop" => {
                let dcb_stop = match value {
                    "1" => ONESTOPBIT,
                    "2" => TWOSTOPBITS,
                    _ => {
                        return Err(AsynError::Status {
                            status: AsynStatus::Error,
                            message: "Invalid number of stop bits.".into(),
                        });
                    }
                };
                self.modify_dcb(|dcb| dcb.StopBits = dcb_stop)?;
            }
            "clocal" => {
                // C-Win32 setOption clocal: Y = ignore modem status (no DSR
                // flow, DTR asserted); N = honor it (DSR flow, DTR handshake).
                let enabled = parse_yn_option(&key, value)?;
                self.modify_dcb(|dcb| {
                    let bf = &mut dcb._bitfield;
                    if enabled {
                        set_flag(bf, F_OUTX_DSR_FLOW, false);
                        set_flag(bf, F_DSR_SENSITIVITY, false);
                        set_field(
                            bf,
                            F_DTR_CONTROL_SHIFT,
                            F_DTR_CONTROL_MASK,
                            DTR_CONTROL_ENABLE,
                        );
                    } else {
                        set_flag(bf, F_OUTX_DSR_FLOW, true);
                        set_flag(bf, F_DSR_SENSITIVITY, true);
                        set_field(
                            bf,
                            F_DTR_CONTROL_SHIFT,
                            F_DTR_CONTROL_MASK,
                            DTR_CONTROL_HANDSHAKE,
                        );
                    }
                })?;
            }
            "crtscts" => {
                let enabled = parse_yn_option(&key, value)?;
                self.modify_dcb(|dcb| {
                    let bf = &mut dcb._bitfield;
                    set_flag(bf, F_OUTX_CTS_FLOW, enabled);
                    set_field(
                        bf,
                        F_RTS_CONTROL_SHIFT,
                        F_RTS_CONTROL_MASK,
                        if enabled {
                            RTS_CONTROL_HANDSHAKE
                        } else {
                            RTS_CONTROL_ENABLE
                        },
                    );
                })?;
            }
            "ixon" => {
                let enabled = parse_yn_option(&key, value)?;
                self.modify_dcb(|dcb| set_flag(&mut dcb._bitfield, F_OUT_X, enabled))?;
            }
            "ixoff" => {
                let enabled = parse_yn_option(&key, value)?;
                self.modify_dcb(|dcb| set_flag(&mut dcb._bitfield, F_IN_X, enabled))?;
            }
            "ixany" => {
                // C-Win32 setOption: ixany is unsupported on Windows.
                return Err(AsynError::Status {
                    status: AsynStatus::Error,
                    message: "Option ixany not supported on Windows".into(),
                });
            }
            "break" => {
                // C-Win32 holds the break LATCHED: setOption "on" asserts the
                // line and returns with it still asserted, and only "off" or a
                // timed form releases it. The three-way rule and its 250 ms
                // default live in `option_parse::break_action`, which is also
                // where the tests reach it without a COM port.
                let action = break_action(value, self.io.break_active)?;
                let handle = self.io.handle_or_err()?;
                if action.assert_break {
                    // C: FlushFileBuffers before asserting the break so queued
                    // data is transmitted first.
                    if unsafe { FlushFileBuffers(handle) } == 0 {
                        return Err(io_err());
                    }
                    if unsafe { SetCommBreak(handle) } == 0 {
                        return Err(io_err());
                    }
                    self.io.break_active = true;
                }
                if let Some(ms) = action.hold_ms {
                    std::thread::sleep(Duration::from_millis(ms));
                }
                if action.clear_break {
                    if unsafe { ClearCommBreak(handle) } == 0 {
                        return Err(io_err());
                    }
                    self.io.break_active = false;
                }
            }
            other => {
                if !other.is_empty() {
                    return Err(AsynError::OptionNotFound(other.to_string()));
                }
                // Empty key: re-apply the configured line state (C
                // applyOptions re-pushes its cached config), the Win32 twin of
                // the unix empty-key re-apply. Pushing the cache rather than
                // rebuilding it from `SerialConfig` is what makes the re-apply
                // restore the line as configured instead of reverting clocal
                // and the ixon family the way the rebuild did.
                self.apply_options()?;
            }
        }
        Ok(())
    }

    fn get_option(&self, key: &str) -> AsynResult<String> {
        use dcb_bits::*;

        // C-Win32 getOption's opening guard (drvAsynSerialPortWin32.c:96-101):
        // a closed handle errors for every key before the key is inspected —
        // there is no disconnected readback, cached or defaulted.
        self.check_option_connected()?;

        // Every key is answered from the cache, never by re-reading the line —
        // the POSIX backend's rule (`getOption` never calls `tcgetattr`), and
        // what makes the readback agree with what the next connect will push.
        // A live read would disagree the moment another process touched the
        // port, and could not answer the keys `SerialConfig` never held.
        let dcb = *self.dcb_or_err()?;
        let yn = |on: bool| if on { "Y" } else { "N" }.to_string();

        // C-Win32 `getOption` dispatches with `epicsStrCaseCmp` (:110-155) just
        // as `setOption` does, so both halves fold the key through the same
        // owner and a key set as `BAUD` reads back.
        let key = option_key(key);
        match key.as_str() {
            "baud" => Ok(dcb.BaudRate.to_string()),
            "bits" => Ok(dcb.ByteSize.to_string()),
            "parity" => Ok(match dcb.Parity {
                EVENPARITY => "even",
                ODDPARITY => "odd",
                _ => "none",
            }
            .to_string()),
            "stop" => Ok(if dcb.StopBits == TWOSTOPBITS {
                "2"
            } else {
                "1"
            }
            .to_string()),
            // C getOption: clocal is 'N' when DSR flow is on, else 'Y'.
            "clocal" => Ok(yn(!get_flag(dcb._bitfield, F_OUTX_DSR_FLOW))),
            "crtscts" => Ok(yn(get_flag(dcb._bitfield, F_OUTX_CTS_FLOW))),
            "ixon" => Ok(yn(get_flag(dcb._bitfield, F_OUT_X))),
            "ixoff" => Ok(yn(get_flag(dcb._bitfield, F_IN_X))),
            // C-Win32 getOption reports ixany as 'N' (unsupported).
            "ixany" => Ok("N".to_string()),
            // C getOption reports the live latch, not a constant:
            // `tty->break_active ? "on" : "off"`
            // (drvAsynSerialPortWin32.c:147-150).
            "break" => Ok(if self.io.break_active { "on" } else { "off" }.to_string()),
            _ => self
                .base
                .options
                .get(&key)
                .cloned()
                .ok_or_else(|| AsynError::OptionNotFound(key.clone())),
        }
    }
}

impl Drop for DrvAsynSerialPort {
    fn drop(&mut self) {
        let user = AsynUser::default();
        if self.base.is_connected() {
            let _ = self.disconnect(&user);
        }
    }
}

#[cfg(test)]
#[allow(deprecated)]
mod tests {
    use super::*;

    #[test]
    fn parse_extracts_device() {
        let cfg = SerialConfig::parse(r"\\.\COM3").unwrap();
        assert_eq!(cfg.device, r"\\.\COM3");
        assert_eq!(cfg.baud, 9600);
    }

    #[test]
    fn parse_rejects_empty() {
        assert!(SerialConfig::parse("").is_err());
        assert!(SerialConfig::parse("   ").is_err());
    }

    #[test]
    fn device_to_wide_prepends_prefix_once() {
        // A bare COM name gets the \\.\ prefix (needed for COM10+).
        let w = device_to_wide("COM3");
        let s = String::from_utf16(&w[..w.len() - 1]).unwrap();
        assert_eq!(s, r"\\.\COM3");
        assert_eq!(*w.last().unwrap(), 0, "must be NUL-terminated");

        // An already-prefixed name is left as-is (no double prefix).
        let w2 = device_to_wide(r"\\.\COM10");
        let s2 = String::from_utf16(&w2[..w2.len() - 1]).unwrap();
        assert_eq!(s2, r"\\.\COM10");
    }

    #[test]
    fn apply_config_sets_dcb_fields() {
        let mut cfg = SerialConfig::parse("COM1").unwrap();
        cfg.baud = 115200;
        cfg.data_bits = DataBits::Seven;
        cfg.parity = Parity::Even;
        cfg.stop_bits = StopBits::Two;
        cfg.flow_control = FlowControl::Hardware;

        let mut dcb: DCB = unsafe { std::mem::zeroed() };
        apply_config_to_dcb(&cfg, &mut dcb);

        assert_eq!(dcb.BaudRate, 115200);
        assert_eq!(dcb.ByteSize, 7);
        assert_eq!(dcb.Parity, EVENPARITY);
        assert_eq!(dcb.StopBits, TWOSTOPBITS);
        // fBinary always set; hardware flow → CTS flow on + RTS handshake.
        assert!(dcb_bits::get_flag(dcb._bitfield, dcb_bits::F_BINARY));
        assert!(dcb_bits::get_flag(dcb._bitfield, dcb_bits::F_OUTX_CTS_FLOW));
        let rts = (dcb._bitfield & dcb_bits::F_RTS_CONTROL_MASK) >> dcb_bits::F_RTS_CONTROL_SHIFT;
        assert_eq!(rts, dcb_bits::RTS_CONTROL_HANDSHAKE);
        assert_eq!(dcb.XonChar, 0x11);
        assert_eq!(dcb.XoffChar, 0x13);
    }

    #[test]
    fn configure_installs_eos_unless_suppressed() {
        let default_port = DrvAsynSerialPort::configure("s_eos", "COM1", false, false).unwrap();
        assert_eq!(
            default_port.base().interpose_octet.len(),
            1,
            "default serial port must auto-install the EOS interpose"
        );
        let suppressed = DrvAsynSerialPort::configure("s_eos_off", "COM1", true, true).unwrap();
        assert_eq!(
            suppressed.base().interpose_octet.len(),
            0,
            "noProcessEos must suppress the EOS interpose"
        );
    }

    #[test]
    fn new_is_parse_only_no_eos() {
        let drv = DrvAsynSerialPort::new("s1", "COM1").unwrap();
        assert_eq!(drv.base().interpose_octet.len(), 0);
    }

    /// C-Win32 `flushIt` (`drvAsynSerialPortWin32.c:668-672`) guards on the
    /// closed handle exactly as the other four entry points do, so a flush over
    /// an absent device is `asynError`, not success. The port used to fall
    /// through to `Ok(())`, which made `TMOD=Flush` on a port whose `COM` device
    /// is gone indistinguishable from a flush that really ran.
    ///
    /// Deliberately not mirrored in `serial_port.rs`: the POSIX C twin
    /// (`drvAsynSerialPort.c:986-1001`) is lenient by design.
    #[test]
    fn flush_on_a_closed_handle_is_refused() {
        let mut drv = DrvAsynSerialPort::new("s_flush_disc", "COM1").unwrap();
        assert!(
            drv.io.handle_val.is_none(),
            "new() must not open the device"
        );

        let mut user = AsynUser::default();
        let err = drv
            .io_flush(&mut user)
            .expect_err("a flush with no handle must not report success");
        assert_eq!(err.status(), AsynStatus::Disconnected);
    }

    /// R7-50: C-Win32 guards getOption AND setOption on the closed handle
    /// (`drvAsynSerialPortWin32.c:96-101,180-185`) *before* the key is
    /// inspected, so every key — not just the flow-control subset — reports
    /// asynError "<device> disconnected:" while the port is closed, and no key
    /// may be stored to the cached config.
    #[test]
    fn options_on_a_closed_handle_report_disconnected_for_every_key() {
        let mut drv = DrvAsynSerialPort::new("s_disc", "COM1").unwrap();
        assert!(
            drv.io.handle_val.is_none(),
            "new() must not open the device"
        );

        for key in [
            "baud", "bits", "parity", "stop", "clocal", "crtscts", "ixon", "ixoff", "ixany",
            "break",
        ] {
            match drv.get_option(key) {
                Err(AsynError::Status { status, message }) => {
                    assert_eq!(status, AsynStatus::Error, "get_option({key}) status");
                    assert!(
                        message.contains("disconnected:"),
                        "get_option({key}) message: {message}"
                    );
                }
                other => panic!("get_option({key}) must error while closed, got {other:?}"),
            }
        }

        for (key, value) in [
            ("baud", "19200"),
            ("bits", "7"),
            ("parity", "even"),
            ("stop", "2"),
            ("clocal", "Y"),
            ("crtscts", "Y"),
            ("ixon", "Y"),
            ("ixoff", "Y"),
            ("break", "on"),
        ] {
            match drv.set_option(&mut AsynUser::default(), key, value) {
                Err(AsynError::Status { status, message }) => {
                    assert_eq!(status, AsynStatus::Error, "set_option({key}) status");
                    assert!(
                        message.contains("disconnected:"),
                        "set_option({key}) message: {message}"
                    );
                }
                other => panic!("set_option({key}) must error while closed, got {other:?}"),
            }
        }

        // The refused set_option calls must not have reached the cached config.
        assert_eq!(drv.config.baud, 9600);
        assert_eq!(drv.config.data_bits, DataBits::Eight);
        assert_eq!(drv.config.parity, Parity::None);
        assert_eq!(drv.config.stop_bits, StopBits::One);
        assert_eq!(drv.config.flow_control, FlowControl::None);
    }

    /// F6 regression: an option the configure string cannot express must
    /// survive a disconnect/reconnect.
    ///
    /// `ixon N` on a port configured for software flow control is the sharp
    /// case — the connect used to re-derive `fOutX` from the three-valued
    /// `flow_control`, so an unplugged cable or an auto-connect retry turned
    /// XON/XOFF back on behind the operator and the next device XOFF was
    /// ignored. `clocal` rides along: `SerialConfig` cannot hold it at all.
    #[test]
    fn options_the_configure_string_cannot_express_survive_a_reconnect() {
        use dcb_bits::*;

        let mut config = SerialConfig::parse("COM3").unwrap();
        config.flow_control = FlowControl::Software;

        // First connect: the device's DCB with the configure string folded in.
        let device: DCB = unsafe { std::mem::zeroed() };
        let mut cache = line_state_for_connect(None, device, &config);
        assert!(
            get_flag(cache._bitfield, F_OUT_X),
            "the configure string still seeds the line on the first connect"
        );

        // The operator turns XON off and clocal off on the live port; both go
        // to the cache, which is the only owner of the line state.
        set_flag(&mut cache._bitfield, F_OUT_X, false);
        set_flag(&mut cache._bitfield, F_OUTX_DSR_FLOW, true);

        // Cable unplugged, auto-connect re-opens: the device is back at its own
        // defaults and the cache, not the configure string, decides.
        let reopened: DCB = unsafe { std::mem::zeroed() };
        let after = line_state_for_connect(Some(cache), reopened, &config);
        assert!(
            !get_flag(after._bitfield, F_OUT_X),
            "ixon N must survive the reconnect"
        );
        assert!(
            get_flag(after._bitfield, F_OUTX_DSR_FLOW),
            "clocal N must survive the reconnect"
        );
    }

    /// C-Win32 `readIt` never closes the handle: the file's only two
    /// `closeConnection` call sites are the explicit disconnect
    /// (`drvAsynSerialPortWin32.c:470`) and `writeIt` (`:525`), and the read
    /// error branch at `:596-640` sets `errorMessage`, assigns `asynError` and
    /// breaks. `writeIt` does close, so the two directions are asserted
    /// together — the asymmetry is the point.
    ///
    /// The port publishes `connected` with no handle so the failure comes from
    /// the device layer rather than from `check_ready`, which is the shape a
    /// failing `ReadFile` has and needs no COM port.
    #[test]
    fn a_failed_read_keeps_the_port_open_while_a_failed_write_closes_it() {
        let mut drv = DrvAsynSerialPort::new("s_read_err", "COM1").unwrap();
        drv.base.set_connected(true);
        assert!(drv.io.handle_val.is_none());

        let user = AsynUser::default();
        let mut buf = [0u8; 8];
        let err = drv
            .io_read_octet_eom(&user, &mut buf)
            .expect_err("a read with no handle must fail");
        assert!(
            err.is_fatal_transport(),
            "the branch under test only ever ran for a fatal error"
        );
        assert!(
            drv.base.is_connected(),
            "C-Win32 readIt reports the error with the port still connected"
        );

        // Still reaching the device rather than the base gate: the second read
        // carries the device layer's message, not `port ... not connected`.
        let again = drv
            .io_read_octet_eom(&user, &mut buf)
            .expect_err("a second read must still reach the device");
        assert!(
            again.to_string().contains("serial port not open"),
            "expected the device-layer refusal, got {again}"
        );

        // The write half keeps C's teardown (`:525`).
        let mut wuser = AsynUser::default();
        drv.write_octet(&mut wuser, b"x")
            .expect_err("a write with no handle must fail");
        assert!(
            !drv.base.is_connected(),
            "C-Win32 writeIt closes the connection on a failed WriteFile"
        );
    }
}
