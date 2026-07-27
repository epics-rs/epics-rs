//! Serial port driver (drvAsynSerialPort equivalent).
//!
//! Mounted on every unix. Everything a target can differ about — the termios
//! ABI itself, the flag values, and which facilities exist at all — lives in
//! the private `platform` module below, which is the only place in the file
//! that names a target.

use std::os::unix::io::RawFd;
use std::time::{Duration, Instant};

use crate::error::{AsynError, AsynResult, AsynStatus};
use crate::exception::AsynException;
use crate::interpose::{EomReason, OctetNext, OctetReadResult};
use crate::port::{PortDriver, PortDriverBase, PortFlags};
use crate::trace::TraceMask;
use crate::user::AsynUser;
use crate::{asyn_trace, asyn_trace_io};

use super::option_parse::{bad_number, parse_yn_option, sscanf_int, sscanf_uint};
use super::serial_config::{DataBits, FlowControl, Parity, SerialConfig, StopBits};

/// The termios facilities this driver needs, resolved once per target.
///
/// This module is the only place in the file that names a target, and it owns
/// the whole termios ABI: the struct, the flag constants, the `c_cc` indices
/// and the calls are all reached through here, never through `libc` directly.
/// Two kinds of difference live here and nowhere else:
///
/// * a name the platform *has* but `libc` does not bind — taken from the
///   platform's own header, with the citation;
/// * a facility the platform genuinely *lacks* — an [`Option`] (or an
///   `Option`-returning call), so every consumer refuses the corresponding
///   asyn option through [`option_unsupported_here`] instead of silently
///   doing nothing.
///
/// That second shape is the point. A missing bit is not an absent line of
/// code, it is a *refusal*, and making it an `Option` means the refusal is
/// the same statement at every site rather than a `#[cfg]` at each one that
/// the next platform has to be added to individually.
///
/// Every arm exports the same names, so one rule covers every target: a
/// facility is supported exactly where this platform's termios has the bit,
/// the index or the call. No arm gets an exception, and a target that is
/// missing a name fails to build rather than silently taking another
/// platform's value.
///
/// # VxWorks: why the POSIX path and not C's
///
/// C's vxWorks branch drives the line through `ioctl(SIO_HW_OPTS_SET)` on a
/// *fake* `struct termios { int c_cflag; }` (`drvAsynSerialPort.c:43-62`),
/// because it predates VxWorks 7's POSIX termios. Consequences C then has to
/// live with: `crtscts` is aliased onto `CLOCAL` ("vxWorks uses CLOCAL when
/// it should use CRTSCTS", `:425-431`) since `sioLibCommon.h` has no
/// flow-control bit, and `ixoff` is refused outright (`:488-490`).
///
/// This driver takes VxWorks 7's real `<termios.h>` instead, which the RTP
/// sysroot exposes and `libc` binds ABI-correctly (`struct termios` field for
/// field, `NCCS == 20`, `VMIN == 16`). There `CRTSCTS` (`CCTS_OFLOW |
/// CRTS_IFLOW`) and `IXOFF` are real, distinct bits, so honouring them costs
/// nothing and keeps asyn's documented option contract — whereas C's aliasing
/// would make `crtscts` and `clocal` silently overwrite each other. `ixany`
/// stays refused because VxWorks genuinely has no such bit: the same
/// conclusion C reaches, reached by the same test.
///
/// Note the `c_cflag` bits are *not* Linux's — VxWorks uses the `sioLib`
/// numbering (`CS8 == 0xc`, `CLOCAL == 0x1`, `PARENB == 0x40`). They are
/// taken from `libc`, which was checked field for field against
/// `wrsdk-vxworks7/vxsdk/sysroot/usr/h/published/UTILS_UNIX/termios.h`.
///
/// # RTEMS: why the ABI is declared here rather than taken from `libc`
///
/// RTEMS is the one target where `libc` cannot be the source. Its newlib
/// module declares a `struct termios` — commented "Unverified" in `libc`'s own
/// source (`src/unix/newlib/mod.rs:200-212`) — that inserts a `c_line: cc_t`
/// RTEMS does not have and gates `c_ispeed`/`c_ospeed` to espidf. The four
/// flag words still land at 0/4/8/12, so nothing *looks* wrong; what breaks is
/// everything after them. `c_cc` is displaced one byte, so `VMIN`, `VTIME` and
/// the flow characters address the wrong bytes, and `cfsetispeed` /
/// `cfsetospeed` write four bytes past the end of the Rust struct. None of
/// that is a compile error.
///
/// The loud half is that newlib binds not one termios *constant*: mounting
/// this file on `libc` fails with 102 errors over 42 names (`CSIZE`, `CS5`..
/// `CS8`, `PARENB`, `CLOCAL`, `VMIN`, `TCSANOW`, `O_NOCTTY`, the `B*` ladder)
/// before it can ever mis-execute.
///
/// Both halves close the same way, and it is the way the two earlier RTEMS ABI
/// breaks in this workspace closed — `sockaddr` without its `sin_len`,
/// `timespec` at 8 bytes where the target has 16: declare the ABI here, from
/// the BSP sysroot's own header, and pin every offset with a `const` assertion
/// so a layout that drifts is a build failure instead of a silent misread.
///
/// The facilities themselves are all present, so this is a complete platform
/// with an incomplete binding — which is why the seam's rule needs no RTEMS
/// case. `librtemscpu.a` defines all twelve termios entry points including
/// `tcdrain`, and `sys/_termios.h` carries `CRTSCTS` (`:141`), `IXANY`
/// (`:97`) and `VSTART`/`VSTOP` (`:66-67`), so RTEMS takes `Some(..)` at every
/// option site.
mod platform {
    #[cfg(not(any(target_os = "vxworks", target_os = "rtems")))]
    mod imp {
        pub use libc::termios;
        /// `speed_t`, the type of a termios speed code.
        pub type Speed = libc::speed_t;
        pub use libc::{
            B300, B9600, CLOCAL, CREAD, CS5, CS6, CS7, CS8, CSIZE, CSTOPB, IGNBRK, IGNPAR, IXOFF,
            IXON, PARENB, PARODD, TCIFLUSH, TCSANOW, VMIN, VTIME,
        };
        pub use libc::{
            cfmakeraw, cfsetispeed, cfsetospeed, tcflush, tcgetattr, tcsendbreak, tcsetattr,
        };

        /// Hardware flow control.
        pub const CRTSCTS: libc::tcflag_t = libc::CRTSCTS;
        /// Do not make the serial line this process's controlling terminal.
        pub const O_NOCTTY: libc::c_int = libc::O_NOCTTY;
        /// Discard both queues — C `connectIt` (`drvAsynSerialPort.c:729`).
        pub const FLUSH_IO: libc::c_int = libc::TCIOFLUSH;
        /// Any character restarts paused output.
        pub const IXANY: Option<libc::tcflag_t> = Some(libc::IXANY);
        /// `c_cc` indices of the XON/XOFF characters.
        pub const SOFT_FLOW_CHARS: Option<(usize, usize)> = Some((libc::VSTART, libc::VSTOP));

        /// Block until queued output has actually been transmitted.
        pub fn drain(fd: libc::c_int) -> Option<std::io::Result<()>> {
            Some(if unsafe { libc::tcdrain(fd) } < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            })
        }
    }

    /// RTEMS 6, declared against `arm-rtems6/include/sys/_termios.h` and
    /// `.../termios.h` in the BSP sysroot. Every line below cites the header
    /// it was read from; nothing here is inferred from another platform.
    #[cfg(target_os = "rtems")]
    mod imp {
        /// `_termios.h:226`: `typedef unsigned int speed_t;`. Its two
        /// siblings — `tcflag_t` and `cc_t`, `:224-225` — are spelled
        /// `libc::c_uint` and `libc::c_uchar` at each use below rather than
        /// aliased, so the C type is visible where the field is.
        pub type Speed = libc::c_uint;

        /// `_termios.h:78`. `libc` gets this one right (it has an explicit
        /// RTEMS arm at 20), which is exactly what makes the one-byte `c_cc`
        /// displacement in its struct so quiet.
        pub const NCCS: usize = 20;

        /// `_termios.h:228-236`, in the header's own field order. This is the
        /// declaration `libc`'s newlib module gets wrong.
        #[repr(C)]
        #[derive(Clone, Copy)]
        pub struct termios {
            pub c_iflag: libc::c_uint,
            pub c_oflag: libc::c_uint,
            pub c_cflag: libc::c_uint,
            pub c_lflag: libc::c_uint,
            pub c_cc: [libc::c_uchar; NCCS],
            pub c_ispeed: Speed,
            pub c_ospeed: Speed,
        }

        // The offsets the C header really produces, written out so the
        // declaration above cannot drift from it and so a `libc`-shaped
        // struct cannot be substituted by accident. Under newlib's binding
        // `c_cc` starts at 17 and there are no speed fields at all, so the
        // `c_cc` and `c_ispeed` lines below are the two that fail first.
        const _: () = {
            use std::mem::{align_of, offset_of, size_of};
            assert!(size_of::<libc::c_uint>() == 4, "tcflag_t is `unsigned int`");
            assert!(size_of::<libc::c_uchar>() == 1, "cc_t is `unsigned char`");
            assert!(size_of::<Speed>() == 4, "speed_t is `unsigned int`");
            assert!(offset_of!(termios, c_iflag) == 0, "c_iflag at 0");
            assert!(offset_of!(termios, c_oflag) == 4, "c_oflag at 4");
            assert!(offset_of!(termios, c_cflag) == 8, "c_cflag at 8");
            assert!(offset_of!(termios, c_lflag) == 12, "c_lflag at 12");
            assert!(offset_of!(termios, c_cc) == 16, "c_cc at 16, not 17");
            assert!(offset_of!(termios, c_ispeed) == 36, "c_ispeed at 36");
            assert!(offset_of!(termios, c_ospeed) == 40, "c_ospeed at 40");
            assert!(size_of::<termios>() == 44, "sizeof(struct termios) == 44");
            assert!(align_of::<termios>() == 4, "alignof(struct termios) == 4");
            assert!(
                VMIN < NCCS && VTIME < NCCS && VSTART < NCCS && VSTOP < NCCS,
                "every c_cc index used here must be inside the array"
            );
        };

        // `termios.h:78-95`. Declared here rather than used from `libc`
        // because `libc`'s declarations take *its* `struct termios`, and a
        // pointer to this one is not that. The symbols themselves are the
        // same: `librtemscpu.a` defines all twelve as `T`.
        unsafe extern "C" {
            pub fn tcgetattr(fd: libc::c_int, t: *mut termios) -> libc::c_int;
            pub fn tcsetattr(
                fd: libc::c_int,
                action: libc::c_int,
                t: *const termios,
            ) -> libc::c_int;
            pub fn tcflush(fd: libc::c_int, queue: libc::c_int) -> libc::c_int;
            pub fn tcdrain(fd: libc::c_int) -> libc::c_int;
            pub fn tcsendbreak(fd: libc::c_int, duration: libc::c_int) -> libc::c_int;
            pub fn cfmakeraw(t: *mut termios);
            pub fn cfsetispeed(t: *mut termios, speed: Speed) -> libc::c_int;
            pub fn cfsetospeed(t: *mut termios, speed: Speed) -> libc::c_int;
        }

        // Input flags, `_termios.h:85-97`.
        pub const IGNBRK: libc::c_uint = 0x0000_0001;
        pub const IGNPAR: libc::c_uint = 0x0000_0004;
        pub const IXON: libc::c_uint = 0x0000_0200;
        pub const IXOFF: libc::c_uint = 0x0000_0400;
        /// `_termios.h:97`. RTEMS has the bit, so the option is supported.
        pub const IXANY: Option<libc::c_uint> = Some(0x0000_0800);

        // Control flags, `_termios.h:128-142`. BSD numbering, not Linux's.
        pub const CSIZE: libc::c_uint = 0x0000_0300;
        pub const CS5: libc::c_uint = 0x0000_0000;
        pub const CS6: libc::c_uint = 0x0000_0100;
        pub const CS7: libc::c_uint = 0x0000_0200;
        pub const CS8: libc::c_uint = 0x0000_0300;
        pub const CSTOPB: libc::c_uint = 0x0000_0400;
        pub const CREAD: libc::c_uint = 0x0000_0800;
        pub const PARENB: libc::c_uint = 0x0000_1000;
        pub const PARODD: libc::c_uint = 0x0000_2000;
        pub const CLOCAL: libc::c_uint = 0x0000_8000;
        /// `CCTS_OFLOW | CRTS_IFLOW`, `_termios.h:140-142`.
        pub const CRTSCTS: libc::c_uint = 0x0001_0000 | 0x0002_0000;

        // `c_cc` indices, `_termios.h:66-73`.
        pub const VSTART: usize = 12;
        pub const VSTOP: usize = 13;
        pub const VMIN: usize = 16;
        pub const VTIME: usize = 17;
        /// RTEMS has both indices, so the ^Q/^S characters are ours to seed.
        pub const SOFT_FLOW_CHARS: Option<(usize, usize)> = Some((VSTART, VSTOP));

        // `tcsetattr` action and `tcflush` selectors, `termios.h:62,69-71`.
        pub const TCSANOW: libc::c_int = 0;
        pub const TCIFLUSH: libc::c_int = 1;
        /// `TCIOFLUSH` — RTEMS has the both-queues selector.
        pub const FLUSH_IO: libc::c_int = 3;

        /// `sys/_default_fcntl.h:25` via `:59` (`O_NOCTTY` is `_FNOCTTY`).
        /// Newlib binds `O_RDWR` and `O_NONBLOCK` but not this one.
        pub const O_NOCTTY: libc::c_int = 0x8000;

        /// Standard speeds, `_termios.h:186-221`. Only the two the branch
        /// assertion below needs are named: the codes *are* the rates here,
        /// so `baud_to_speed` never looks the rest up.
        pub const B300: Speed = 300;
        pub const B9600: Speed = 9600;

        /// RTEMS has `tcdrain` (`termios.h:84`, defined in `librtemscpu.a`).
        pub fn drain(fd: libc::c_int) -> Option<std::io::Result<()>> {
            Some(if unsafe { tcdrain(fd) } < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            })
        }
    }

    #[cfg(target_os = "vxworks")]
    mod imp {
        pub use libc::termios;
        /// `speed_t`, the type of a termios speed code.
        pub type Speed = libc::speed_t;
        pub use libc::{
            B300, B9600, CLOCAL, CREAD, CS5, CS6, CS7, CS8, CSIZE, CSTOPB, IGNBRK, IGNPAR, IXOFF,
            IXON, PARENB, PARODD, TCIFLUSH, TCSANOW, VMIN, VTIME,
        };
        pub use libc::{
            cfmakeraw, cfsetispeed, cfsetospeed, tcflush, tcgetattr, tcsendbreak, tcsetattr,
        };

        /// `CCTS_OFLOW | CRTS_IFLOW`, `termios.h:114-116`. Present in the SDK
        /// header, absent from `libc`'s VxWorks module, so it is named here.
        pub const CRTSCTS: libc::tcflag_t = 0x0001_0000 | 0x0002_0000;
        /// `_FNOCTTY`, `sys/fcntlcom.h:76` via `:109`. Same story: the SDK
        /// defines it, `libc` does not bind it. VxWorks has no controlling
        /// terminal to acquire, but `open` still accepts the flag, and C
        /// passes it unconditionally (`drvAsynSerialPort.c:707`).
        pub const O_NOCTTY: libc::c_int = 0x8000;
        /// VxWorks defines no both-queues selector — `termios.h:102` has
        /// `TCIFLUSH` alone. Discarding the input queue is the half that
        /// matters at connect (stale bytes from before the line was
        /// configured); C skips this flush entirely on vxWorks
        /// (`drvAsynSerialPort.c:728-740` is `#ifndef vxWorks`), so doing the
        /// input half is strictly closer to the hosted behaviour than C is.
        pub const FLUSH_IO: libc::c_int = libc::TCIFLUSH;
        /// No `IXANY` bit exists on VxWorks — the SDK's input-flag block
        /// (`termios.h:70-81`) goes straight from `IXON` to `IXOFF`. C refuses
        /// the option here too (`drvAsynSerialPort.c:469-471`).
        pub const IXANY: Option<libc::tcflag_t> = None;
        /// No `VSTART`/`VSTOP` indices exist: VxWorks `c_cc` runs `VINTR`,
        /// `VQUIT`, `VERASE`, `VKILL`, `VEOF`, `VMIN`, `VTIME` and nothing
        /// else (`termios.h:59-68`). The XON/XOFF characters are fixed in the
        /// tty layer rather than programmable.
        pub const SOFT_FLOW_CHARS: Option<(usize, usize)> = None;

        /// VxWorks has no `tcdrain` (nor any drain-shaped ioctl — `FIOWFLUSH`
        /// *discards* the write queue rather than waiting for it), so this
        /// reports the gap instead of pretending to have drained.
        pub fn drain(_fd: libc::c_int) -> Option<std::io::Result<()>> {
            None
        }
    }

    pub use imp::*;
}

/// C's refusal for an asyn option the platform's termios has no bit for:
/// `"Option ixany not supported on vxWorks"` (`drvAsynSerialPort.c:469-471`,
/// and `:488-490` for `ixoff`). Refusing is what C does, and it is the only
/// honest answer — accepting the key and dropping it would let `getOption`
/// report a line state the hardware was never told about.
fn option_unsupported_here(key: &str) -> AsynError {
    AsynError::Status {
        status: AsynStatus::Error,
        message: format!("Option {key} not supported on {}", std::env::consts::OS),
    }
}

/// Write `speed` into both directions of `t`, reporting the platform's own
/// refusal instead of discarding it. The single owner of that write: nothing
/// else may call `cfset[io]speed`, so a caller cannot forget the check.
///
/// C tests both returns and answers asynError carrying the `strerror` text
/// (`drvAsynSerialPort.c:346-355`). Dropping that is not cosmetic, because
/// `cfsetospeed` is where a platform enforces its speed table — and the table
/// is not implied by `B300 == 300`. Measured on RTEMS 6:
/// `rtems_termios_baud_to_number(31250)` is 0 and `cfsetospeed` refuses the
/// rate, leaving the previous one, while a `tcsetattr` carrying 31250 in
/// `c_ospeed` directly is accepted and reads back. Without the check
/// `set_option("baud", "31250")` answered `Ok` and `get_option("baud")` then
/// reported 31250 on a line still running at 9600.
fn set_termios_speed(t: &mut platform::termios, speed: platform::Speed) -> AsynResult<()> {
    if unsafe { platform::cfsetispeed(t, speed) } < 0 {
        return Err(AsynError::Status {
            status: AsynStatus::Error,
            message: format!("cfsetispeed returned {}", std::io::Error::last_os_error()),
        });
    }
    if unsafe { platform::cfsetospeed(t, speed) } < 0 {
        return Err(AsynError::Status {
            status: AsynStatus::Error,
            message: format!("cfsetospeed returned {}", std::io::Error::last_os_error()),
        });
    }
    Ok(())
}

impl SerialConfig {
    /// Apply this configuration to a raw termios struct.
    ///
    /// Errors if `self.baud` is not settable on this platform. `baud_to_speed`
    /// is the single validation owner; surfacing the error here (rather than a
    /// silent `B9600` fallback) means an unmappable rate cannot be applied even
    /// through a directly-built `SerialConfig`, not just via `set_option`.
    pub fn apply_to_termios(&self, t: &mut platform::termios) -> AsynResult<()> {
        let baud = baud_to_speed(self.baud).ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: format!("unsupported baud rate: {}", self.baud),
        })?;
        set_termios_speed(t, baud)?;

        // Data bits
        t.c_cflag &= !platform::CSIZE;
        t.c_cflag |= match self.data_bits {
            DataBits::Five => platform::CS5,
            DataBits::Six => platform::CS6,
            DataBits::Seven => platform::CS7,
            DataBits::Eight => platform::CS8,
        };

        // Parity
        match self.parity {
            Parity::None => {
                t.c_cflag &= !platform::PARENB;
            }
            Parity::Even => {
                t.c_cflag |= platform::PARENB;
                t.c_cflag &= !platform::PARODD;
            }
            Parity::Odd => {
                t.c_cflag |= platform::PARENB;
                t.c_cflag |= platform::PARODD;
            }
        }

        // Stop bits
        match self.stop_bits {
            StopBits::One => t.c_cflag &= !platform::CSTOPB,
            StopBits::Two => t.c_cflag |= platform::CSTOPB,
        }

        // Flow control. `IXANY` is only in the *clear* masks, so a platform
        // without the bit (`None`) needs no refusal here — there is nothing
        // to clear, and neither mode ever sets it.
        let ixany = platform::IXANY.unwrap_or(0);
        match self.flow_control {
            FlowControl::None => {
                t.c_cflag &= !platform::CRTSCTS;
                t.c_iflag &= !(platform::IXON | platform::IXOFF | ixany);
            }
            FlowControl::Hardware => {
                t.c_cflag |= platform::CRTSCTS;
                t.c_iflag &= !(platform::IXON | platform::IXOFF | ixany);
            }
            FlowControl::Software => {
                t.c_cflag &= !platform::CRTSCTS;
                t.c_iflag |= platform::IXON | platform::IXOFF;
            }
        }
        Ok(())
    }
}

/// Map a baud rate to its termios speed code, or `None` if the rate is not
/// settable on this platform.
///
/// C parity (drvAsynSerialPort.c:271-345): on systems where the termios `Bxxx`
/// constants equal the literal baud rate — macOS and the BSDs, where
/// `B9600 == 9600` — C uses the baud value itself as the speed code
/// (`baudCode = baud`, line 274), so *any* rate is accepted including
/// non-standard ones. Elsewhere (Linux, where the codes are small encoded
/// integers) C maps the known standard rates with a `switch` and returns
/// asynError ("Unsupported data rate", lines 340-343) for anything outside it.
///
/// Both embedded targets are in the first group — VxWorks at `termios.h:22-52`
/// and RTEMS at `sys/_termios.h:186-221` — so both take the passthrough branch
/// exactly as C does there. Which group a target is in is
/// `asyn_baud_code_is_rate`, named once in `build.rs` because this file asks
/// the question three times.
///
/// Passthrough here is not a promise that the *device* will take the rate, and
/// C makes no such promise either: it is only the statement that this platform
/// has no separate encoding to look the rate up in. Measured on RTEMS 6, where
/// `rtems_termios_baud_to_number(31250)` is 0 and `cfsetospeed` refuses the
/// rate outright — which is why [`set_termios_speed`], not this function, is
/// where a rate is finally accepted or refused.
fn baud_to_speed(baud: u32) -> Option<platform::Speed> {
    #[cfg(asyn_baud_code_is_rate)]
    {
        // Bxxx == literal rate: the baud value is itself a valid speed code.
        // `from` (not `as`) so this stays clean whether speed_t is u32 or u64.
        Some(platform::Speed::from(baud))
    }

    // The one place the file reads `libc::` termios names instead of going
    // through `platform`, here and in the same arm of `speed_to_baud`. It is
    // sound because the `cfg` *is* the guarantee: a target reaches this ladder
    // only if its codes are encoded integers, which is only true where `libc`
    // binds them. Selecting the wrong arm does not misbehave, it fails to
    // compile — on RTEMS with `cannot find value B50 in crate libc`.
    #[cfg(not(asyn_baud_code_is_rate))]
    {
        Some(match baud {
            // C parity (drvAsynSerialPort.c:276-344): the Linux switch starts at
            // `case 50` with no `case 0`, so baud 0 (which would program B0 — a
            // line hangup) falls to the `default` asynError. macOS/BSD differ:
            // there `baudCode = baud`, so 0 is accepted by the passthrough branch
            // above (C-macOS does the same). No `0 => B0` arm here.
            50 => libc::B50,
            75 => libc::B75,
            110 => libc::B110,
            134 => libc::B134,
            150 => libc::B150,
            200 => libc::B200,
            300 => libc::B300,
            600 => libc::B600,
            1200 => libc::B1200,
            1800 => libc::B1800,
            2400 => libc::B2400,
            4800 => libc::B4800,
            9600 => libc::B9600,
            19200 => libc::B19200,
            38400 => libc::B38400,
            57600 => libc::B57600,
            115200 => libc::B115200,
            230400 => libc::B230400,
            // High baud rates: C gates each with `#ifdef Bxxx`; on Linux the
            // codes exist, while the arbitrary-rate branch above covers
            // macOS/BSD. Linux defines no B28800, matching C's `#ifdef B28800`.
            #[cfg(any(target_os = "linux", target_os = "android"))]
            460800 => libc::B460800,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            500000 => libc::B500000,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            576000 => libc::B576000,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            921600 => libc::B921600,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            1000000 => libc::B1000000,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            1152000 => libc::B1152000,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            1500000 => libc::B1500000,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            2000000 => libc::B2000000,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            2500000 => libc::B2500000,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            3000000 => libc::B3000000,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            3500000 => libc::B3500000,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            4000000 => libc::B4000000,
            // C parity (drvAsynSerialPort.c:340-343): unknown rate -> asynError.
            _ => return None,
        })
    }
}

/// `asyn_baud_code_is_rate` selects *code*, but the question it answers is a
/// property of the platform's constants, and C asks it as exactly that — a
/// preprocessor test rather than a platform list
/// (`drvAsynSerialPort.c:272-273`):
///
/// ```c
/// #if (defined(B300) && (B300 == 300) && defined(B9600) && (B9600 == 9600))
/// ```
///
/// The two cannot be collapsed here: the arms reference different constant
/// sets, so which one compiles has to be a `cfg`. What can be removed is the
/// chance of them disagreeing. This fails the build on any target where
/// `build.rs` claims one thing and the constants say the other — which is what
/// silently mapping a rate through the wrong branch would otherwise cost:
/// arbitrary rates rejected where they are legal, or `9600` programmed as
/// speed code 13.
const _: () = assert!(
    (platform::B300 == 300 && platform::B9600 == 9600) == cfg!(asyn_baud_code_is_rate),
    "asyn_baud_code_is_rate disagrees with this platform's Bxxx codes: add or \
     remove this target in build.rs, do not leave the branch mismatched"
);

/// The inverse of [`baud_to_speed`], branching the same way so the two stay
/// inverse on every platform. `0` means "not a rate this platform expresses".
#[allow(dead_code)]
fn speed_to_baud(speed: platform::Speed) -> u32 {
    // Where the code *is* the rate, the ladder below would be a table of
    // `n => n` — and one that answered 0 for every rate outside it, so
    // `speed_to_baud(baud_to_speed(31250))` would lose a rate the platform
    // accepts. The identity is both shorter and the actual inverse.
    #[cfg(asyn_baud_code_is_rate)]
    {
        u32::try_from(speed).unwrap_or(0)
    }

    #[cfg(not(asyn_baud_code_is_rate))]
    {
        match speed {
            libc::B0 => 0,
            libc::B50 => 50,
            libc::B75 => 75,
            libc::B110 => 110,
            libc::B134 => 134,
            libc::B150 => 150,
            libc::B200 => 200,
            libc::B300 => 300,
            libc::B600 => 600,
            libc::B1200 => 1200,
            libc::B1800 => 1800,
            libc::B2400 => 2400,
            libc::B4800 => 4800,
            libc::B9600 => 9600,
            libc::B19200 => 19200,
            libc::B38400 => 38400,
            libc::B57600 => 57600,
            libc::B115200 => 115200,
            libc::B230400 => 230400,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            libc::B460800 => 460800,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            libc::B500000 => 500000,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            libc::B576000 => 576000,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            libc::B921600 => 921600,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            libc::B1000000 => 1000000,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            libc::B1152000 => 1152000,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            libc::B1500000 => 1500000,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            libc::B2000000 => 2000000,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            libc::B2500000 => 2500000,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            libc::B3000000 => 3000000,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            libc::B3500000 => 3500000,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            libc::B4000000 => 4000000,
            _ => 0,
        }
    }
}

// --- I/O state ---

struct SerialIoState {
    fd: Option<RawFd>,
    /// Cumulative bytes successfully read / written, for `report()` diagnostics
    /// (C tracks `tty->nRead` / `tty->nWritten`, drvAsynSerialPort.c).
    n_read: u64,
    n_written: u64,
}

impl SerialIoState {
    fn new() -> Self {
        Self {
            fd: None,
            n_read: 0,
            n_written: 0,
        }
    }

    fn fd_or_err(&self) -> AsynResult<RawFd> {
        self.fd.ok_or_else(|| AsynError::Status {
            status: AsynStatus::Disconnected,
            message: "serial port not open".into(),
        })
    }
}

fn duration_to_poll_ms(d: Duration) -> i32 {
    d.as_millis().min(i32::MAX as u128) as i32
}

impl OctetNext for SerialIoState {
    fn read(&mut self, user: &AsynUser, buf: &mut [u8]) -> AsynResult<OctetReadResult> {
        let fd = self.fd_or_err()?;
        // C readIt (drvAsynSerialPort.c:871-875): reject maxchars == 0 with
        // asynError right after the fd check and before touching the device.
        // An empty buffer would otherwise reach libc::read(fd, ptr, 0), which
        // returns 0 and is misclassified below (n == 0) as a disconnect (EOF)
        // — tearing down a live serial port. (Message matches the serial
        // driver's own wording, which omits the period the IP driver carries.)
        if buf.is_empty() {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: "maxchars 0 Why <=0?".into(),
            });
        }
        let timeout_ms = duration_to_poll_ms(user.timeout);

        // C parity (drvAsynSerialPort.c): retry poll/read on EINTR (a signal
        // interrupted the call) and EAGAIN/EWOULDBLOCK (spurious wakeup);
        // only a real error is fatal. Without this, a benign signal would be
        // surfaced as a fatal Io error and tear the connection down.
        loop {
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };

            let ret = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
            if ret < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(AsynError::Io(err));
            }
            if ret == 0 {
                return Err(AsynError::Status {
                    status: AsynStatus::Timeout,
                    message: "serial read timeout".into(),
                });
            }

            let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted
                    || err.kind() == std::io::ErrorKind::WouldBlock
                {
                    continue;
                }
                return Err(AsynError::Io(err));
            }
            if n == 0 {
                return Err(AsynError::Status {
                    status: AsynStatus::Disconnected,
                    message: "serial port EOF".into(),
                });
            }

            self.n_read += n as u64; // C parity: tty->nRead += thisRead
            return Ok(OctetReadResult {
                nbytes_transferred: n as usize,
                // C parity: CNT only when the requested count was reached.
                eom_reason: if n as usize >= buf.len() {
                    EomReason::CNT
                } else {
                    EomReason::empty()
                },
            });
        }
    }

    fn write(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
        let fd = self.fd_or_err()?;

        // No blocking-mode toggle here: the fd is non-blocking for its whole
        // life (see `connect`). A blocking `write(fd, all_remaining)` would not
        // return until the *entire* buffer is accepted by the kernel, so a
        // stalled or slow peer would block the write past the timeout
        // regardless of the poll below — C instead unblocks a stuck write from
        // its timeout timer via tcflush(TCOFLUSH) (drvAsynSerialPort.c:649).
        // This driver has no such timer; a permanently non-blocking fd is what
        // replaces it, so each `write` returns immediately with what fit (or
        // EAGAIN) and the poll/deadline loop bounds the whole write.

        // C parity (drvAsynSerialPort.c:815-842): writeIt arms a single timer
        // for the whole writeTimeout *before* the loop and breaks when it fires,
        // so the timeout bounds the TOTAL write, not each chunk. Bound total
        // time with one deadline and poll with the remaining budget each
        // iteration (the IP driver's write_with_retry total-deadline model).
        // The previous code reused the full per-call timeout on every poll, so
        // a slowly-draining peer could keep a multi-chunk write alive for up to
        // timeout x iterations.
        let deadline = Instant::now() + user.timeout;
        // C parity (drvAsynSerialPort.c:849): `*nbytesTransfered = numchars -
        // nleft` runs on the way out of the loop for *every* break — timeout
        // and fatal errno alike — so the caller always learns how much of its
        // message the device took. `total` therefore lives outside the loop
        // and rides out on the error via `with_partial_write`.
        let mut total = 0usize;
        let result: AsynResult<()> = loop {
            if total >= data.len() {
                break Ok(());
            }
            let poll_ms = duration_to_poll_ms(deadline.saturating_duration_since(Instant::now()));
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLOUT,
                revents: 0,
            };

            let ret = unsafe { libc::poll(&mut pfd, 1, poll_ms) };
            if ret < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                break Err(AsynError::Io(err));
            }
            if ret == 0 {
                break Err(AsynError::Status {
                    status: AsynStatus::Timeout,
                    message: "serial write timeout".into(),
                });
            }

            let n = unsafe {
                libc::write(
                    fd,
                    data[total..].as_ptr() as *const libc::c_void,
                    data.len() - total,
                )
            };
            if n < 0 {
                // C parity: retry on EINTR/EAGAIN; only a real error is fatal.
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted
                    || err.kind() == std::io::ErrorKind::WouldBlock
                {
                    continue;
                }
                break Err(AsynError::Io(err));
            }
            total += n as usize;
            self.n_written += n as u64; // C parity: tty->nWritten += thisWrite

            // C parity (drvAsynSerialPort.c:827): after each write, if the
            // total deadline has passed stop with asynTimeout even though
            // some bytes went out. A non-blocking poll that finds free space
            // (e.g. a slow peer that drains a little each gap) would
            // otherwise let the write keep going past the deadline, since
            // `poll(0)` returns POLLOUT instead of timing out. (timeout==0
            // collapses to a single write attempt then bail, matching
            // writeIt's `writeTimeout==0`.)
            if total < data.len() && Instant::now() >= deadline {
                break Err(AsynError::Status {
                    status: AsynStatus::Timeout,
                    message: "serial write timeout".into(),
                });
            }
        };

        match result {
            Ok(()) => Ok(total),
            Err(e) => Err(e.with_partial_write(total)),
        }
    }

    fn flush(&mut self, _user: &mut AsynUser) -> AsynResult<()> {
        if let Some(fd) = self.fd {
            // C parity: tcflush(TCIFLUSH) discards received-but-unread input data,
            // matching C drvAsynSerialPort's flush behavior. NOT tcdrain (output wait).
            let ret = unsafe { platform::tcflush(fd, platform::TCIFLUSH) };
            if ret < 0 {
                return Err(AsynError::Io(std::io::Error::last_os_error()));
            }
        }
        Ok(())
    }
}

// --- Driver ---

/// Serial port driver.
pub struct DrvAsynSerialPort {
    base: PortDriverBase,
    /// C `tty->serialDeviceName`.
    device: String,
    /// C `tty->baud` (drvAsynSerialPort.c:1078). Kept beside the termios
    /// cache because C reads the rate back from this field rather than
    /// reversing the platform speed code, and non-standard rates have no
    /// reverse mapping on Linux.
    baud: u32,
    /// C `tty->termios` — the cached line configuration and the **single
    /// owner** of every termios-expressible option (bits, parity, stop,
    /// clocal, crtscts, ixon/ixoff/ixany).
    ///
    /// C seeds it at configure (`CS8|CLOCAL|CREAD`, `IGNBRK|IGNPAR`, B9600 —
    /// :1077-1089), `setOption` mutates it unconditionally (whether or not
    /// the port is open, :350-592), `applyOptions` pushes it to the device on
    /// every connect and after every successful option change (:105-130), and
    /// `getOption` reads it back (:135-207). Options therefore survive a
    /// disconnect/reconnect and can be set while the port is down.
    ///
    /// The port previously had no cache: options were written straight to the
    /// live termios and rebuilt from `SerialConfig` at each connect, so
    /// anything not representable in `SerialConfig` (clocal, the ixon family)
    /// was silently dropped while disconnected and wiped at the next
    /// auto-reconnect.
    termios: platform::termios,
    io: SerialIoState,
    saved_termios: Option<platform::termios>,
}

/// Seed the cached termios the way C `drvAsynSerialPortConfigure` does
/// (drvAsynSerialPort.c:1077-1089): `CS8|CLOCAL|CREAD`, `IGNBRK|IGNPAR`,
/// raw output/local modes, the ^Q/^S flow characters and B9600 — then apply
/// the parsed `SerialConfig` (baud/bits/parity/stop/flow from the configure
/// string) on top, which is the port's equivalent of C's post-configure
/// `asynSetOption` calls in the startup script.
///
/// This runs **once**, at construction. From then on the cache is the owner:
/// `set_option` mutates it and `apply_options` pushes it. Nothing rebuilds
/// the line state from `SerialConfig` again, so an option that `SerialConfig`
/// cannot express (clocal, per-flag ixon/ixoff/ixany) is no longer erased at
/// the next connect.
fn seed_termios(config: &SerialConfig) -> AsynResult<platform::termios> {
    let mut t: platform::termios = unsafe { std::mem::zeroed() };
    unsafe { platform::cfmakeraw(&mut t) };
    // Enable receiver, local mode (C's CS8|CLOCAL|CREAD seed).
    t.c_cflag |= platform::CREAD | platform::CLOCAL;
    // C parity (drvAsynSerialPort.c:1080): the default input flags are
    // IGNBRK | IGNPAR. cfmakeraw clears IGNBRK (and never sets IGNPAR),
    // so without this a line BREAK or a framing/parity error reaches
    // the reader as a spurious 0x00 byte where C silently ignores it.
    t.c_iflag |= platform::IGNBRK | platform::IGNPAR;
    // VMIN=1, VTIME=0 — blocking read waits for at least 1 byte.
    // Deliberate divergence from C (drvAsynSerialPort.c:1083 seeds
    // VMIN=0 and reprograms VMIN/VTIME per read from the requested
    // timeout, :899-908): C drives the read timeout through VTIME plus
    // an epicsTimer, whereas this driver gates every read with
    // poll(POLLIN, timeout) and only reads when data is ready. With that
    // architecture VMIN=1 keeps `n == 0` meaning exactly EOF/hangup;
    // VMIN=0 would make a spurious poll-wake return 0 and be misread as a
    // disconnect. Every representable (non-negative) timeout is already
    // bounded by the poll.
    t.c_cc[platform::VMIN] = 1;
    t.c_cc[platform::VTIME] = 0;
    // C parity (drvAsynSerialPort.c:1085-1086): the XON/XOFF flow
    // characters default to ^Q (0x11, VSTART) and ^S (0x13, VSTOP).
    // `t` was zeroed before cfmakeraw and cfmakeraw leaves c_cc
    // untouched, so without this FlowControl::Software (IXON|IXOFF)
    // would drive flow with NUL bytes instead of ^Q/^S. Where the
    // platform has no such indices the characters are not ours to
    // choose — see `platform::SOFT_FLOW_CHARS`.
    if let Some((vstart, vstop)) = platform::SOFT_FLOW_CHARS {
        t.c_cc[vstart] = 0x11; // ^Q
        t.c_cc[vstop] = 0x13; // ^S
    }
    config.apply_to_termios(&mut t)?;
    Ok(t)
}

impl DrvAsynSerialPort {
    /// Close the fd and mark the port disconnected so the actor's
    /// auto-reconnect re-opens it on the next request. C parity:
    /// `drvAsynSerialPort.c::closeConnection` (close, fd=-1,
    /// `exceptionDisconnect`). Unlike the graceful `disconnect`, a
    /// fatal-error teardown does not restore termios — the device is gone
    /// and the fd is being closed.
    fn drop_connection(&mut self) {
        if let Some(fd) = self.io.fd.take() {
            unsafe { libc::close(fd) };
        }
        self.saved_termios = None;
        self.base.set_connected(false);
    }

    /// Create a new serial port driver.
    ///
    /// The driver starts disconnected with `auto_connect = true` and `can_block = true`.
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
        // (drvAsynSerialPort.c:1125): every successful octet read on this port fans out to
        // its octet interrupt users, which is what drives a SCAN="I/O Intr"
        // record. See `PortDriverBase::octet_interrupt_process`.
        base.octet_interrupt_process = true;

        Ok(Self {
            base,
            device: config.device.clone(),
            baud: config.baud,
            termios: seed_termios(&config)?,
            io: SerialIoState::new(),
            saved_termios: None,
        })
    }

    /// Configure a serial port the way C `drvAsynSerialPortConfigure`
    /// does (`drvAsynSerialPort.c:1031-1126`): parse the device, honor
    /// `noAutoConnect`, and enable EOS processing by default unless
    /// `noProcessEos`. C does this by passing `(noProcessEos ? 0 : 1)` to
    /// `pasynOctetBase->initialize`; the Rust octet stack expresses EOS
    /// through the interpose layer, so the equivalent is to auto-install
    /// an `EosInterpose` (empty terminator until `setInputEos`/`OEOS`).
    ///
    /// `new` stays the parse-only constructor (no EOS), matching the
    /// lower-level C octet init without the `Configure` wrapper.
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

    /// Send a serial line BREAK condition (RS-232 BREAK), mirroring
    /// asyn PR #188 ("auto serial break"). Duration is in tenths of
    /// a second per POSIX `tcsendbreak(fd, duration)` (Linux honors
    /// the value, BSD/macOS treats non-zero as ≥0.25s — match the
    /// platform semantic). `duration = 0` requests the minimum
    /// implementation-defined BREAK length (typically 250-500ms).
    ///
    /// Returns an error if the port is not currently connected.
    /// Operators driving break-reset protocols (e.g. some Tektronix
    /// scopes, certain Allen-Bradley PLCs) call this between
    /// commands to force the device's serial state machine to its
    /// initial state.
    pub fn send_break(&self, duration_tenths: i32) -> AsynResult<()> {
        let fd = self.io.fd_or_err()?;
        let ret = unsafe { platform::tcsendbreak(fd, duration_tenths) };
        if ret < 0 {
            return Err(AsynError::Io(std::io::Error::last_os_error()));
        }
        Ok(())
    }

    /// Drain any output queued on the serial port, blocking until
    /// every byte the kernel has accepted has actually been
    /// transmitted (POSIX `tcdrain`). Useful immediately before
    /// [`Self::send_break`] so the BREAK signal isn't preceded by
    /// unflushed user data.
    ///
    /// Errors where the platform has no drain at all (VxWorks), rather
    /// than returning `Ok(())` for an operation that did not happen.
    pub fn drain_output(&self) -> AsynResult<()> {
        let fd = self.io.fd_or_err()?;
        match platform::drain(fd) {
            Some(Ok(())) => Ok(()),
            Some(Err(e)) => Err(AsynError::Io(e)),
            None => Err(option_unsupported_here("drain")),
        }
    }

    fn get_current_termios(&self) -> AsynResult<platform::termios> {
        let fd = self.io.fd_or_err()?;
        let mut t: platform::termios = unsafe { std::mem::zeroed() };
        let ret = unsafe { platform::tcgetattr(fd, &mut t) };
        if ret < 0 {
            return Err(AsynError::Io(std::io::Error::last_os_error()));
        }
        Ok(t)
    }

    fn apply_termios(&self, t: &platform::termios) -> AsynResult<()> {
        let fd = self.io.fd_or_err()?;
        let ret = unsafe { platform::tcsetattr(fd, platform::TCSANOW, t) };
        if ret < 0 {
            return Err(AsynError::Io(std::io::Error::last_os_error()));
        }
        Ok(())
    }

    /// Push the cached termios to the device — C `applyOptions`
    /// (drvAsynSerialPort.c:105-130): force `CREAD` into the cache, then one
    /// `tcsetattr(TCSANOW)` of the whole cache. Note C forces only CREAD here,
    /// **not** CLOCAL: CLOCAL is a seeded default that `setOption` may clear
    /// and the clear must survive every subsequent connect.
    ///
    /// Called by `connect` (initial setup) and by `set_option` after every
    /// successful cache mutation, so the device state is always exactly the
    /// cache — never a rebuild from a partial config.
    fn apply_options(&mut self) -> AsynResult<()> {
        self.termios.c_cflag |= platform::CREAD;
        let t = self.termios;
        self.apply_termios(&t)
    }
}

impl PortDriver for DrvAsynSerialPort {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }

    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }

    /// C drvAsynSerialPort registers asynCommon, asynOption and asynOctet
    /// (drvAsynSerialPort.c:1090-1110).
    fn capabilities(&self) -> Vec<crate::interfaces::Capability> {
        crate::interfaces::octet_transport_capabilities()
    }

    fn connect(&mut self, _user: &AsynUser) -> AsynResult<()> {
        // C drvAsynSerialPort.c::connectIt (694-698): reject a connect on
        // an already-open link ("Link already open!") rather than opening a
        // second fd and leaking the first (along with its saved termios).
        if self.io.fd.is_some() {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: format!("{}: Link already open!", self.base.port_name),
            });
        }
        // 1. Open device
        let c_path =
            std::ffi::CString::new(self.device.as_str()).map_err(|_| AsynError::Status {
                status: AsynStatus::Error,
                message: "invalid device path (contains NUL)".into(),
            })?;

        let fd = unsafe {
            libc::open(
                c_path.as_ptr(),
                libc::O_RDWR | platform::O_NOCTTY | libc::O_NONBLOCK,
            )
        };
        if fd < 0 {
            return Err(AsynError::Io(std::io::Error::last_os_error()));
        }
        self.io.fd = Some(fd);

        // Steps 2-4 configure the just-opened fd. Any failure here must
        // close the fd: `base.connected` is still false, so the `Drop`
        // impl would skip `disconnect()` and leak the descriptor.
        let setup = (|| -> AsynResult<()> {
            // C parity (drvAsynSerialPort.c:713-722): set close-on-exec right
            // after open so the serial fd is not inherited by child processes
            // (e.g. an iocsh `system` call), which would otherwise hold the
            // device open after this driver closes it.
            if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
                return Err(AsynError::Io(std::io::Error::last_os_error()));
            }

            // 2. Save original termios
            let saved = self.get_current_termios()?;
            self.saved_termios = Some(saved);

            // 3. Configure: push the cached termios — C `connectIt` calls
            // `applyOptions(pasynUser, tty)` (drvAsynSerialPort.c:722), which
            // tcsetattr's `tty->termios` verbatim. The cache carries every
            // option set since configure, including ones set while the port
            // was down, so they take effect on this connect instead of being
            // rebuilt away.
            self.apply_options()?;

            // C parity (drvAsynSerialPort.c:729): discard any bytes that
            // accumulated in the kernel input/output buffers before the port
            // was configured, so the first read/write starts from a clean
            // device state.
            unsafe { platform::tcflush(fd, platform::FLUSH_IO) };

            // 4. The fd STAYS non-blocking, for its whole life.
            //
            // C turns blocking back on here (drvAsynSerialPort.c:731-739),
            // because its reads are bounded by termios VMIN/VTIME and its
            // writes by an epicsTimer that fires tcflush(TCOFLUSH) at a stuck
            // one. This driver has neither: every read and every write is
            // gated by `poll` with the caller's deadline (see `OctetNext for
            // SerialIoState`), which is the deviation `seed_termios` already
            // documents. Under that model blocking mode buys nothing — a
            // `read` only ever runs after `poll` reported POLLIN, and EAGAIN
            // is already a retry — while a blocking `write` would sit past
            // the deadline with no timer to break it.
            //
            // So the state is uniform rather than toggled: `write` used to
            // flip the fd non-blocking and back on every call, which meant
            // the fd's mode depended on where you looked from. One mode for
            // the fd's whole life removes that, and with it the
            // `fcntl(F_GETFL)` that VxWorks answers -1 to — measured on
            // target, and the reason C skips this very block there
            // (`#ifndef vxWorks`, :728). One rule, no platform branch.
            Ok(())
        })();
        if let Err(e) = setup {
            if let Some(fd) = self.io.fd.take() {
                unsafe { libc::close(fd) };
            }
            self.saved_termios = None;
            return Err(e);
        }

        self.base.set_connected(true);
        asyn_trace!(
            Some(self.base.trace),
            &self.base.port_name,
            TraceMask::FLOW,
            "connected to {} at {} baud",
            self.device,
            self.baud
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

        // Restore original termios if available
        if let (Some(fd), Some(saved)) = (self.io.fd, &self.saved_termios) {
            unsafe { platform::tcsetattr(fd, platform::TCSANOW, saved) };
        }

        // Close fd
        if let Some(fd) = self.io.fd.take() {
            unsafe { libc::close(fd) };
        }
        self.saved_termios = None;

        self.base.set_connected(false);
        Ok(())
    }

    fn report(&self, out: &mut dyn std::fmt::Write, level: i32) {
        use std::fmt::Write as _;
        // C parity (drvAsynSerialPort.c:666-680): report the connection state,
        // and at details>=1 the fd plus cumulative bytes written/read.
        let _ = writeln!(
            out,
            "Serial line {}: {}",
            self.device,
            if self.base.is_connected() {
                "Connected"
            } else {
                "Disconnected"
            }
        );
        if level >= 1 {
            let _ = writeln!(out, "                    fd: {}", self.io.fd.unwrap_or(-1));
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
    /// `drvAsynSerialPort.c::readIt` below the interposes the manager installed
    /// on top of it. The interpose chain is run by the port
    /// (`crate::port::octet_read_chain`), not from here: a driver that
    /// dispatched its own chain gave every other driver a chain that never ran.
    fn io_read_octet_eom(
        &mut self,
        user: &AsynUser,
        buf: &mut [u8],
    ) -> AsynResult<(usize, EomReason)> {
        self.base.check_ready()?;
        let result = match self.io.read(user, buf) {
            Ok(r) => r,
            Err(e) => {
                // C parity: drvAsynSerialPort.c::closeConnection on a fatal
                // read error / EOF so the actor's auto-reconnect re-opens the
                // device. EINTR/EAGAIN are already retried inside
                // SerialIoState::read, so an error reaching here is fatal.
                if e.is_fatal_transport() && self.base.is_connected() {
                    asyn_trace!(
                        Some(self.base.trace),
                        &self.base.port_name,
                        TraceMask::FLOW,
                        "read error, disconnecting: {e}"
                    );
                    self.drop_connection();
                }
                return Err(e);
            }
        };
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
                // C parity: closeConnection on a fatal write error so the
                // next request reconnects (symmetric with read; matches
                // ip_port DRV-5).
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

    /// C `setOption` (drvAsynSerialPort.c:335-618): every supported key
    /// mutates the **cached** termios (or `tty->baud`) unconditionally —
    /// whether or not the port is open — and the single tail then pushes the
    /// cache to the device when it is, restoring the previous cache if that
    /// push fails. An option set while the port is down is therefore held and
    /// applied at the next connect, exactly like one set while it is up.
    ///
    /// The port previously mutated the *live* termios (tcgetattr → modify →
    /// tcsetattr) and skipped the write entirely when disconnected, so
    /// `clocal`/`ixon`/`ixoff`/`ixany` — which have no `SerialConfig` field —
    /// were silently dropped while down and erased at the next connect by the
    /// rebuild from config.
    fn set_option(&mut self, _user: &mut AsynUser, key: &str, value: &str) -> AsynResult<()> {
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();

        // C keeps `baudPrev`/`termiosPrev` for the applyOptions rollback
        // (:341-346, :599-606). `platform::termios` is Copy, so this is the same
        // snapshot-and-restore.
        let baud_prev = self.baud;
        let termios_prev = self.termios;

        match key.as_str() {
            "baud" => {
                // C `sscanf(val, "%d", &baud) != 1` -> "Bad number"
                // (drvAsynSerialPort.c:262-266). A prefix parse: "9600x" is 9600
                // to C, where `str::parse` refused it.
                let baud = sscanf_int(value).ok_or_else(bad_number)?;
                // C parity (drvAsynSerialPort.c:340-343): an unsupported rate is
                // asynError "Unsupported data rate (%d baud)". baud_to_speed is
                // the single source of truth for what is settable on this
                // platform, so the validation and the speed lookup cannot
                // disagree.
                let speed = u32::try_from(baud)
                    .ok()
                    .and_then(baud_to_speed)
                    .ok_or_else(|| AsynError::Status {
                        status: AsynStatus::Error,
                        message: format!("Unsupported data rate ({baud} baud)"),
                    })?;
                set_termios_speed(&mut self.termios, speed)?;
                self.baud = baud as u32;
            }
            "bits" => {
                // C compares the value string (drvAsynSerialPort.c:359-377), so
                // 5/6/7/8 and nothing else; the miss is "Invalid number of bits."
                let bits = match value {
                    "5" => DataBits::Five,
                    "6" => DataBits::Six,
                    "7" => DataBits::Seven,
                    "8" => DataBits::Eight,
                    _ => {
                        return Err(AsynError::Status {
                            status: AsynStatus::Error,
                            message: "Invalid number of bits.".into(),
                        });
                    }
                };
                self.termios.c_cflag &= !platform::CSIZE;
                self.termios.c_cflag |= match bits {
                    DataBits::Five => platform::CS5,
                    DataBits::Six => platform::CS6,
                    DataBits::Seven => platform::CS7,
                    DataBits::Eight => platform::CS8,
                };
            }
            "parity" => {
                // C drvAsynSerialPort.c::setOption (379-395) accepts only
                // "none"/"even"/"odd" (case-insensitive); anything else is
                // asynError "Invalid parity." The single-char aliases n/e/o
                // were a Rust-only superset and are dropped to match C.
                let val_lower = value.to_ascii_lowercase();
                match val_lower.as_str() {
                    "none" => self.termios.c_cflag &= !platform::PARENB,
                    "even" => {
                        self.termios.c_cflag |= platform::PARENB;
                        self.termios.c_cflag &= !platform::PARODD;
                    }
                    "odd" => {
                        self.termios.c_cflag |= platform::PARENB;
                        self.termios.c_cflag |= platform::PARODD;
                    }
                    _ => {
                        // C's text for this key is not of the "Invalid <key>
                        // value." shape (drvAsynSerialPort.c:394-395).
                        return Err(AsynError::Status {
                            status: AsynStatus::Error,
                            message: "Invalid parity.".into(),
                        });
                    }
                }
            }
            "stop" => match value {
                "1" => self.termios.c_cflag &= !platform::CSTOPB,
                "2" => self.termios.c_cflag |= platform::CSTOPB,
                _ => {
                    return Err(AsynError::Status {
                        status: AsynStatus::Error,
                        message: "Invalid number of stop bits.".into(),
                    });
                }
            },
            "clocal" => {
                if parse_yn_option(&key, value)? {
                    self.termios.c_cflag |= platform::CLOCAL;
                } else {
                    self.termios.c_cflag &= !platform::CLOCAL;
                }
            }
            "crtscts" => {
                if parse_yn_option(&key, value)? {
                    self.termios.c_cflag |= platform::CRTSCTS;
                } else {
                    self.termios.c_cflag &= !platform::CRTSCTS;
                }
            }
            "ixon" => {
                if parse_yn_option(&key, value)? {
                    self.termios.c_iflag |= platform::IXON;
                } else {
                    self.termios.c_iflag &= !platform::IXON;
                }
            }
            "ixoff" => {
                if parse_yn_option(&key, value)? {
                    self.termios.c_iflag |= platform::IXOFF;
                } else {
                    self.termios.c_iflag &= !platform::IXOFF;
                }
            }
            "ixany" => {
                // C validates the value before the platform check on POSIX,
                // but refuses `ixany` on vxWorks without looking at it at all
                // (drvAsynSerialPort.c:466-471) — the refusal is about the
                // key, not the value, so it comes first.
                let bit = platform::IXANY.ok_or_else(|| option_unsupported_here("ixany"))?;
                if parse_yn_option(&key, value)? {
                    self.termios.c_iflag |= bit;
                } else {
                    self.termios.c_iflag &= !bit;
                }
            }
            "break" => {
                // C parity (drvAsynSerialPort.c:507-528): "off" = no-op (early
                // asynSuccess), "" or "on" = standard break (len 0), a number =
                // break duration; anything else is "Bad number" (asynError).
                // C validates the value and acts on the fd WITHOUT guarding on
                // it being open, so a break on a closed port fails (tcsendbreak
                // EBADF -> asynError) rather than silently succeeding. Mirror
                // that order: validate first, then require the fd.
                if value != "off" {
                    let duration = if value.is_empty() || value == "on" {
                        0 // standard break duration
                    } else {
                        // C `sscanf(val, "%u", &break_len) != 1` -> "Bad number"
                        // (drvAsynSerialPort.c:511-515).
                        sscanf_uint(value).ok_or_else(bad_number)? as i32
                    };
                    // Disconnected -> error (not a silent no-op); C reaches
                    // tcdrain/tcsendbreak on the dead fd and returns asynError.
                    let fd = self.io.fd_or_err()?;
                    // Drain output first (C parity: tcdrain before tcsendbreak).
                    // Where the platform has no drain the BREAK still goes out
                    // — only the guarantee that queued bytes precede it is
                    // lost. C weighs it the same way, ignoring tcdrain's return
                    // entirely (drvAsynSerialPort.c:526), so a missing drain is
                    // not grounds to refuse the break.
                    if let Some(res) = platform::drain(fd) {
                        res.map_err(AsynError::Io)?;
                    }
                    let ret = unsafe { platform::tcsendbreak(fd, duration) };
                    if ret < 0 {
                        return Err(AsynError::Io(std::io::Error::last_os_error()));
                    }
                }
            }
            #[cfg(target_os = "linux")]
            "rs485_enable"
            | "rs485_rts_on_send"
            | "rs485_rts_after_send"
            | "rs485_delay_rts_before_send"
            | "rs485_delay_rts_after_send" => {
                self.set_rs485_option(&key, value)?;
            }
            other => {
                // C drvAsynSerialPort.c::setOption (lines 594-616): any
                // unsupported non-empty key returns asynError "Unsupported
                // key" (the `epicsStrCaseCmp(key,"") != 0` guard at :594).
                // The real handlers above own every supported key, so there
                // is no generic option store.
                if !other.is_empty() {
                    return Err(AsynError::OptionNotFound(other.to_string()));
                }
                // The empty key is not an error: it means "re-apply", and the
                // tail below does exactly that (C :609-615 runs applyOptions
                // for it like any other key) — restoring the configured line
                // state if another process changed the port underneath us.
            }
        }

        // C :599-606 — the one place the cache reaches the device: push it
        // when the port is open, and roll the cache back if the device
        // rejects it, so getOption never reports a value the line refused.
        if self.io.fd.is_some() {
            if let Err(e) = self.apply_options() {
                self.baud = baud_prev;
                self.termios = termios_prev;
                return Err(e);
            }
        }
        Ok(())
    }

    fn get_option(&self, key: &str) -> AsynResult<String> {
        match key {
            // C `getOption` (drvAsynSerialPort.c:135-207) answers every key
            // from the cache — it never calls tcgetattr. So the readback is
            // the *configured* state whether or not the port is open, and it
            // agrees with what the next connect will push. Reading the live
            // termios (and hard-coding "N" while disconnected, as this did)
            // reported "N" for a `clocal N`-by-default port that C reports as
            // "Y", and lost every option set while the line was down.
            "baud" => Ok(self.baud.to_string()),
            "bits" => Ok(match self.termios.c_cflag & platform::CSIZE {
                platform::CS5 => "5",
                platform::CS6 => "6",
                platform::CS7 => "7",
                platform::CS8 => "8",
                _ => "?",
            }
            .to_string()),
            "parity" => Ok(if self.termios.c_cflag & platform::PARENB == 0 {
                "none"
            } else if self.termios.c_cflag & platform::PARODD != 0 {
                "odd"
            } else {
                "even"
            }
            .to_string()),
            "stop" => Ok(if self.termios.c_cflag & platform::CSTOPB != 0 {
                "2"
            } else {
                "1"
            }
            .to_string()),
            "clocal" => Ok(if self.termios.c_cflag & platform::CLOCAL != 0 {
                "Y"
            } else {
                "N"
            }
            .to_string()),
            "crtscts" => Ok(if self.termios.c_cflag & platform::CRTSCTS != 0 {
                "Y"
            } else {
                "N"
            }
            .to_string()),
            // The iflag family reads back from the same cache as the cflags —
            // C getOption (drvAsynSerialPort.c:181-201) answers ixon/ixany/
            // ixoff from `tty->termios.c_iflag` on every POSIX target; the
            // hard-coded 'N' for ixany/ixoff there is inside `#ifdef vxWorks`,
            // where the flags genuinely have no termios home.
            "ixon" => Ok(if self.termios.c_iflag & platform::IXON != 0 {
                "Y"
            } else {
                "N"
            }
            .to_string()),
            "ixoff" => Ok(if self.termios.c_iflag & platform::IXOFF != 0 {
                "Y"
            } else {
                "N"
            }
            .to_string()),
            // Asymmetric with `set_option` on purpose, and C is asymmetric the
            // same way: `setOption` refuses `ixany` on vxWorks
            // (drvAsynSerialPort.c:466-471) while `getOption` answers a
            // hard-coded 'N' (:189-190) rather than erroring. Both are true
            // statements about a line that has no such bit — the option cannot
            // be turned on, and it is not on.
            "ixany" => Ok(match platform::IXANY {
                Some(bit) if self.termios.c_iflag & bit != 0 => "Y",
                _ => "N",
            }
            .to_string()),
            // C getOption (drvAsynSerialPort.c:204-207): "break" is a momentary
            // line action, so a read always reports "off" rather than erroring.
            "break" => Ok("off".to_string()),
            #[cfg(target_os = "linux")]
            "rs485_enable"
            | "rs485_rts_on_send"
            | "rs485_rts_after_send"
            | "rs485_delay_rts_before_send"
            | "rs485_delay_rts_after_send" => self.get_rs485_option(key),
            _ => self
                .base
                .options
                .get(key)
                .cloned()
                .ok_or_else(|| AsynError::OptionNotFound(key.to_string())),
        }
    }
}

// --- RS485 support (Linux only) ---
//
// Mirror of `<linux/serial.h>` `struct serial_rs485` — same layout
// used by `drvAsynSerialPort.c:76-77` (`struct serial_rs485 rs485`).
// Layout: 4 + 4 + 4 + 5*4 = 32 bytes. Pre-Linux-4.20 kernels read the
// full 32-byte buffer in TIOCGRS485 / TIOCSRS485 even though only the
// first three u32 fields carry data; the 5-word padding tail MUST be
// present or the ioctl silently writes garbage on some drivers.
// (PR #22 originally tried to pass a single c_ulong — the kernel
// read the next 24 bytes of stack as "padding" and some PCIe UART
// drivers latched that as a multi-µs rts delay.)
#[cfg(target_os = "linux")]
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SerialRs485 {
    flags: u32,
    delay_rts_before_send: u32,
    delay_rts_after_send: u32,
    padding: [u32; 5],
}

#[cfg(target_os = "linux")]
mod rs485_flags {
    pub const SER_RS485_ENABLED: u32 = 1 << 0;
    pub const SER_RS485_RTS_ON_SEND: u32 = 1 << 1;
    pub const SER_RS485_RTS_AFTER_SEND: u32 = 1 << 2;
}

// TIOCGRS485 = 0x542E, TIOCSRS485 = 0x542F — asm-generic/ioctls.h.
#[cfg(target_os = "linux")]
const TIOCGRS485: libc::c_ulong = 0x542E;
#[cfg(target_os = "linux")]
const TIOCSRS485: libc::c_ulong = 0x542F;

#[cfg(target_os = "linux")]
impl DrvAsynSerialPort {
    fn rs485_get(&self, fd: RawFd) -> AsynResult<SerialRs485> {
        let mut r: SerialRs485 = SerialRs485::default();
        let ret = unsafe { libc::ioctl(fd, TIOCGRS485, &mut r as *mut SerialRs485) };
        if ret < 0 {
            return Err(AsynError::Io(std::io::Error::last_os_error()));
        }
        Ok(r)
    }

    fn rs485_set(&self, fd: RawFd, r: &SerialRs485) -> AsynResult<()> {
        let ret = unsafe { libc::ioctl(fd, TIOCSRS485, r as *const SerialRs485) };
        if ret < 0 {
            return Err(AsynError::Io(std::io::Error::last_os_error()));
        }
        Ok(())
    }

    fn set_rs485_option(&mut self, key: &str, value: &str) -> AsynResult<()> {
        let fd = self.io.fd.ok_or_else(|| AsynError::Status {
            status: AsynStatus::Disconnected,
            message: "not connected".into(),
        })?;

        let mut r = self.rs485_get(fd)?;
        let prev = r;

        use rs485_flags::*;
        match key {
            // C `drvAsynSerialPort.c:531-543`: "Y" sets ENABLED; "N"
            // clears the whole flags word (not just the bit) — match
            // that semantic exactly.
            "rs485_enable" => {
                if parse_yn_option(key, value)? {
                    r.flags |= SER_RS485_ENABLED;
                } else {
                    r.flags = 0;
                }
            }
            "rs485_rts_on_send" => {
                if parse_yn_option(key, value)? {
                    r.flags |= SER_RS485_RTS_ON_SEND;
                } else {
                    r.flags &= !SER_RS485_RTS_ON_SEND;
                }
            }
            // C reports *this* key's bad value as "Invalid rs485_rts_on_send
            // value." (drvAsynSerialPort.c:566) — a copy-paste of the previous
            // arm's text. Deliberate deviation: the key names itself here, since
            // an operator told "rs485_rts_on_send" while writing
            // rs485_rts_after_send is being misdirected by a C typo.
            "rs485_rts_after_send" => {
                if parse_yn_option(key, value)? {
                    r.flags |= SER_RS485_RTS_AFTER_SEND;
                } else {
                    r.flags &= !SER_RS485_RTS_AFTER_SEND;
                }
            }
            // C `sscanf(val, "%u", &delay) != 1` -> "Bad number"
            // (drvAsynSerialPort.c:574-578, :584-588).
            "rs485_delay_rts_before_send" => {
                r.delay_rts_before_send = sscanf_uint(value).ok_or_else(bad_number)?;
            }
            "rs485_delay_rts_after_send" => {
                r.delay_rts_after_send = sscanf_uint(value).ok_or_else(bad_number)?;
            }
            _ => {}
        }

        // C `drvAsynSerialPort.c:608-613`: on TIOCSRS485 failure
        // restore the previous struct state — note that an in-kernel
        // failure may already have applied the change, but the
        // userland copy must still reflect the last-known-good value.
        if let Err(e) = self.rs485_set(fd, &r) {
            let _ = self.rs485_set(fd, &prev);
            return Err(e);
        }
        Ok(())
    }

    fn get_rs485_option(&self, key: &str) -> AsynResult<String> {
        let fd = self.io.fd.ok_or_else(|| AsynError::Status {
            status: AsynStatus::Disconnected,
            message: "not connected".into(),
        })?;
        let r = self.rs485_get(fd)?;
        use rs485_flags::*;
        // Format matches C drvAsynSerialPort.c:210-224 — 'Y'/'N' for
        // flags, "%u" for the delay fields.
        let s = match key {
            "rs485_enable" => if r.flags & SER_RS485_ENABLED != 0 {
                "Y"
            } else {
                "N"
            }
            .to_string(),
            "rs485_rts_on_send" => if r.flags & SER_RS485_RTS_ON_SEND != 0 {
                "Y"
            } else {
                "N"
            }
            .to_string(),
            "rs485_rts_after_send" => if r.flags & SER_RS485_RTS_AFTER_SEND != 0 {
                "Y"
            } else {
                "N"
            }
            .to_string(),
            "rs485_delay_rts_before_send" => r.delay_rts_before_send.to_string(),
            "rs485_delay_rts_after_send" => r.delay_rts_after_send.to_string(),
            _ => {
                return Err(AsynError::OptionNotFound(key.to_string()));
            }
        };
        Ok(s)
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

    // --- Config parsing tests ---

    #[test]
    fn test_parse_device() {
        let cfg = SerialConfig::parse("/dev/ttyUSB0").unwrap();
        assert_eq!(cfg.device, "/dev/ttyUSB0");
        assert_eq!(cfg.baud, 9600);
        assert_eq!(cfg.data_bits, DataBits::Eight);
        assert_eq!(cfg.parity, Parity::None);
        assert_eq!(cfg.stop_bits, StopBits::One);
        assert_eq!(cfg.flow_control, FlowControl::None);
    }

    #[test]
    fn test_parse_empty_error() {
        assert!(SerialConfig::parse("").is_err());
        assert!(SerialConfig::parse("   ").is_err());
    }

    // --- Driver creation tests ---

    #[test]
    fn test_driver_initial_state() {
        let drv = DrvAsynSerialPort::new("serial1", "/dev/ttyUSB0").unwrap();
        assert!(!drv.base().is_connected());
        assert!(drv.base().auto_connect);
        assert!(drv.base().flags.can_block);
        // `new` is parse-only: no EOS interpose (DRV-45).
        assert_eq!(drv.base().interpose_octet.len(), 0);
    }

    /// C drvAsynSerialPort.c:1126 enables EOS by default in Configure
    /// unless noProcessEos; `configure` is the Rust analogue (DRV-45).
    #[test]
    fn test_configure_installs_eos_unless_suppressed_and_honors_no_auto_connect() {
        let default_port =
            DrvAsynSerialPort::configure("s_eos_default", "/dev/ttyS0", false, false).unwrap();
        assert_eq!(
            default_port.base().interpose_octet.len(),
            1,
            "default serial port must auto-install the EOS interpose"
        );
        assert!(default_port.base().auto_connect);

        let suppressed =
            DrvAsynSerialPort::configure("s_eos_off", "/dev/ttyS0", true, true).unwrap();
        assert_eq!(
            suppressed.base().interpose_octet.len(),
            0,
            "noProcessEos must suppress the EOS interpose"
        );
        assert!(!suppressed.base().auto_connect);
    }

    #[test]
    fn test_set_option_baud_disconnected() {
        let mut drv = DrvAsynSerialPort::new("s1", "/dev/ttyS0").unwrap();
        drv.set_option(&mut AsynUser::default(), "baud", "115200")
            .unwrap();
        assert_eq!(drv.baud, 115200);
        assert_eq!(drv.get_option("baud").unwrap(), "115200");
    }

    #[test]
    fn test_set_option_bits() {
        let mut drv = DrvAsynSerialPort::new("s1", "/dev/ttyS0").unwrap();
        drv.set_option(&mut AsynUser::default(), "bits", "7")
            .unwrap();
        assert_eq!(drv.termios.c_cflag & libc::CSIZE, libc::CS7);
        assert_eq!(drv.get_option("bits").unwrap(), "7");
    }

    #[test]
    fn test_set_option_parity() {
        let mut drv = DrvAsynSerialPort::new("s1", "/dev/ttyS0").unwrap();
        drv.set_option(&mut AsynUser::default(), "parity", "even")
            .unwrap();
        assert_ne!(drv.termios.c_cflag & libc::PARENB, 0);
        assert_eq!(drv.termios.c_cflag & libc::PARODD, 0);
        assert_eq!(drv.get_option("parity").unwrap(), "even");
        drv.set_option(&mut AsynUser::default(), "parity", "odd")
            .unwrap();
        assert_eq!(drv.get_option("parity").unwrap(), "odd");
    }

    #[test]
    fn test_set_option_stop() {
        let mut drv = DrvAsynSerialPort::new("s1", "/dev/ttyS0").unwrap();
        drv.set_option(&mut AsynUser::default(), "stop", "2")
            .unwrap();
        assert_ne!(drv.termios.c_cflag & libc::CSTOPB, 0);
        assert_eq!(drv.get_option("stop").unwrap(), "2");
    }

    #[test]
    fn test_set_option_invalid_baud() {
        let mut drv = DrvAsynSerialPort::new("s1", "/dev/ttyS0").unwrap();
        assert!(
            drv.set_option(&mut AsynUser::default(), "baud", "abc")
                .is_err()
        );
    }

    #[test]
    fn test_set_option_unsupported_baud() {
        // DRV-34: a non-standard rate (12345) is rejected only where the
        // platform uses encoded Bxxx codes (Linux), matching C's switch default
        // (drvAsynSerialPort.c:340-343). On macOS/BSD, where B9600 == 9600, C
        // accepts any rate via `baudCode = baud` (:273-274), so the same value
        // is settable there — see baud_arbitrary_on_bsd_mapped_set_on_linux.
        let mut drv = DrvAsynSerialPort::new("s1", "/dev/ttyS0").unwrap();
        #[cfg(not(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "dragonfly"
        )))]
        {
            let err = drv
                .set_option(&mut AsynUser::default(), "baud", "12345")
                .unwrap_err();
            // R11-49: C's text, verbatim (drvAsynSerialPort.c:341-343).
            assert_eq!(err.message(), "Unsupported data rate (12345 baud)");
        }
        #[cfg(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "dragonfly"
        ))]
        {
            drv.set_option(&mut AsynUser::default(), "baud", "12345")
                .unwrap();
            assert_eq!(drv.baud, 12345);
            assert_eq!(drv.get_option("baud").unwrap(), "12345");
        }
    }

    #[test]
    fn baud_arbitrary_on_bsd_mapped_set_on_linux() {
        // DRV-34: baud_to_speed is the single source of truth for which rates
        // are settable. C (drvAsynSerialPort.c:271-345) accepts arbitrary rates
        // where Bxxx == literal rate (macOS/BSD) and a fixed mapped set
        // elsewhere (Linux), erroring on the rest.
        assert!(baud_to_speed(9600).is_some(), "9600 is standard everywhere");

        #[cfg(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "dragonfly"
        ))]
        {
            // Arbitrary rates accepted where the code is the literal rate; the
            // speed code returned is the baud value itself (C: baudCode = baud).
            assert_eq!(baud_to_speed(28800), Some(28800 as libc::speed_t));
            assert_eq!(baud_to_speed(250000), Some(250000 as libc::speed_t));
            assert_eq!(baud_to_speed(115200), Some(115200 as libc::speed_t));
            // 0 (B0, line hangup) is accepted here too, matching C-macOS
            // (baudCode = baud = 0), unlike the Linux switch.
            assert_eq!(baud_to_speed(0), Some(0 as libc::speed_t));
        }

        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            // Linux maps the high standard rates that the old fixed list (which
            // capped at 230400) wrongly rejected, and still has no B28800.
            assert!(baud_to_speed(460800).is_some(), "Linux maps 460800");
            assert!(baud_to_speed(4000000).is_some(), "Linux maps 4000000");
            assert!(baud_to_speed(28800).is_none(), "Linux has no B28800");
            assert!(
                baud_to_speed(250000).is_none(),
                "Linux rejects non-standard"
            );
            // C's Linux switch starts at case 50 — no case 0, so baud 0 falls
            // to the default asynError (drvAsynSerialPort.c:276-344).
            assert!(
                baud_to_speed(0).is_none(),
                "Linux rejects baud 0 (C switch has no case 0)"
            );
        }
    }

    #[test]
    fn apply_to_termios_errors_on_unmappable_baud() {
        // Round drv-s5 CONCERN fix: apply_to_termios surfaces an unmappable rate
        // as an error rather than a silent B9600 fallback, even for a directly
        // built SerialConfig that bypassed set_option validation.
        let valid = SerialConfig::parse("/dev/ttyS0").unwrap();
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        // A normal (9600) config applies cleanly on every platform.
        assert!(valid.apply_to_termios(&mut t).is_ok());

        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            // 28800 is absent from C's Linux switch -> baud_to_speed None -> err.
            let bad = SerialConfig {
                baud: 28800,
                ..SerialConfig::parse("/dev/ttyS0").unwrap()
            };
            assert!(bad.apply_to_termios(&mut t).is_err());
        }
        // macOS/BSD accept any rate via literal passthrough, so there is no
        // unmappable baud to drive the error path there.
    }

    #[test]
    fn test_set_option_invalid_bits() {
        let mut drv = DrvAsynSerialPort::new("s1", "/dev/ttyS0").unwrap();
        assert!(
            drv.set_option(&mut AsynUser::default(), "bits", "9")
                .is_err()
        );
    }

    #[test]
    fn test_set_option_key_case_insensitive() {
        let mut drv = DrvAsynSerialPort::new("s1", "/dev/ttyS0").unwrap();
        drv.set_option(&mut AsynUser::default(), "BAUD", "115200")
            .unwrap();
        assert_eq!(drv.baud, 115200);
        drv.set_option(&mut AsynUser::default(), "Parity", "Even")
            .unwrap();
        assert_eq!(drv.get_option("parity").unwrap(), "even");
    }

    #[test]
    fn test_set_option_value_trimmed() {
        let mut drv = DrvAsynSerialPort::new("s1", "/dev/ttyS0").unwrap();
        drv.set_option(&mut AsynUser::default(), "baud", " 9600 ")
            .unwrap();
        assert_eq!(drv.baud, 9600);
    }

    #[test]
    fn test_set_option_parity_case_insensitive() {
        let mut drv = DrvAsynSerialPort::new("s1", "/dev/ttyS0").unwrap();
        drv.set_option(&mut AsynUser::default(), "parity", "EVEN")
            .unwrap();
        assert_eq!(drv.get_option("parity").unwrap(), "even");
        drv.set_option(&mut AsynUser::default(), "parity", "None")
            .unwrap();
        assert_eq!(drv.get_option("parity").unwrap(), "none");
        // Single-char aliases (n/e/o) are no longer accepted (C parity).
        assert!(
            drv.set_option(&mut AsynUser::default(), "parity", "n")
                .is_err()
        );
    }

    #[test]
    fn test_set_option_parity_mark_space_unsupported() {
        let mut drv = DrvAsynSerialPort::new("s1", "/dev/ttyS0").unwrap();
        let err = drv
            .set_option(&mut AsynUser::default(), "parity", "mark")
            .unwrap_err();
        // R11-49: C has one text for every unrecognised parity value —
        // "Invalid parity." (drvAsynSerialPort.c:394-395). Mark/space are not a
        // separate case in C, and were never a separate case here either.
        assert_eq!(err.message(), "Invalid parity.");
    }

    #[test]
    fn test_parse_bool_option() {
        // C drvAsynSerialPort.c validates these options strictly Y/N
        // (case-insensitive); the looser y/yes/1/true coercion is gone.
        assert!(parse_yn_option("clocal", "Y").unwrap());
        assert!(parse_yn_option("clocal", "y").unwrap());
        assert!(!parse_yn_option("clocal", "N").unwrap());
        assert!(!parse_yn_option("clocal", "n").unwrap());
        // Tokens C rejects now error instead of silently coercing.
        for v in &["yes", "1", "true", "no", "0", "false", "maybe", ""] {
            assert!(
                parse_yn_option("clocal", v).is_err(),
                "expected err for '{v}'"
            );
        }
    }

    #[test]
    fn get_option_break_returns_off() {
        // DRV-39: C getOption (drvAsynSerialPort.c:204-207) reports "break" as
        // "off" (a momentary line action), not an error.
        let drv = DrvAsynSerialPort::new("s1", "/dev/ttyS0").unwrap();
        assert_eq!(drv.get_option("break").unwrap(), "off");
    }

    #[test]
    fn set_option_break_on_disconnected_errors_but_off_is_noop() {
        // DRV-43: C setOption "break" (drvAsynSerialPort.c:507-528) does not
        // guard on the fd, so a real break on a closed port fails
        // (tcsendbreak EBADF -> asynError). The Rust arm previously skipped
        // silently when disconnected. "off" stays a no-op (C asynSuccess), a
        // bad duration is rejected before the fd is touched (C order).
        let mut drv = DrvAsynSerialPort::new("s1", "/dev/ttyS0").unwrap();

        // "off" is a no-op even on a disconnected port.
        drv.set_option(&mut AsynUser::default(), "break", "off")
            .unwrap();

        // A real break on a disconnected port must error, not silently succeed.
        let err = drv
            .set_option(&mut AsynUser::default(), "break", "on")
            .unwrap_err();
        assert!(
            matches!(
                err,
                AsynError::Status {
                    status: AsynStatus::Disconnected,
                    ..
                }
            ),
            "break on a disconnected port must error, got {err:?}"
        );

        // A bad duration is rejected (validated before the fd, matching C).
        let err = drv
            .set_option(&mut AsynUser::default(), "break", "notanumber")
            .unwrap_err();
        // R11-49: C's sscanf("%u") miss is "Bad number" (drvAsynSerialPort.c:512-514).
        assert_eq!(err.message(), "Bad number");
    }

    /// R11-49. Every invalid-value diagnostic C `setOption` can emit, at the key
    /// that emits it (drvAsynSerialPort.c:261-616). These strings are the
    /// operator's: they reach ERRS through `pasynUser->errorMessage`, so an OPI
    /// or a script that keys off them must see C's words, not the port's own.
    #[test]
    fn every_serial_option_reports_cs_text() {
        let mut drv = DrvAsynSerialPort::new("s1", "/dev/ttyS0").unwrap();
        let mut err = |key: &str, val: &str| {
            drv.set_option(&mut AsynUser::default(), key, val)
                .unwrap_err()
                .message()
        };

        assert_eq!(err("baud", "fast"), "Bad number", "C :264");
        assert_eq!(err("bits", "9"), "Invalid number of bits.", "C :374");
        assert_eq!(err("parity", "mark"), "Invalid parity.", "C :395");
        assert_eq!(err("stop", "3"), "Invalid number of stop bits.", "C :406");
        assert_eq!(err("clocal", "maybe"), "Invalid clocal value.", "C :419");
        assert_eq!(err("crtscts", "maybe"), "Invalid crtscts value.", "C :440");
        assert_eq!(err("ixon", "maybe"), "Invalid ixon value.", "C :464");
        assert_eq!(err("ixany", "maybe"), "Invalid ixany value.", "C :481");
        assert_eq!(err("ixoff", "maybe"), "Invalid ixoff value.", "C :502");
        assert_eq!(err("break", "soon"), "Bad number", "C :513");
        assert_eq!(err("nosuch", "x"), "Unsupported key \"nosuch\"", "C :595");
    }

    /// R11-49. C parses the numeric option values with `sscanf("%d")` /
    /// `sscanf("%u")` — a *prefix* parse, so trailing text is ignored, not
    /// rejected. `str::parse` rejected it, so `baud 9600x` (and a value with a
    /// stray unit suffix, which is how these reach an IOC from a substituted
    /// template) errored where C sets the rate.
    #[test]
    fn a_numeric_option_prefix_parses_like_c_sscanf() {
        let mut drv = DrvAsynSerialPort::new("s1", "/dev/ttyS0").unwrap();

        drv.set_option(&mut AsynUser::default(), "baud", "9600x")
            .unwrap();
        assert_eq!(drv.baud, 9600, "C's sscanf %d stops at the first non-digit");

        // Negative control: a value with no leading number at all is C's
        // `sscanf(...) != 1` — "Bad number", not a silent 0.
        let err = drv
            .set_option(&mut AsynUser::default(), "baud", "x9600")
            .unwrap_err();
        assert_eq!(err.message(), "Bad number");
        assert_eq!(drv.baud, 9600, "the rejected write left the cache alone");
    }

    #[test]
    fn test_set_option_unknown() {
        // C drvAsynSerialPort.c::setOption (594-598) rejects any non-empty
        // unsupported key (asynError "Unsupported key") and never stores it,
        // so a later getOption cannot echo it back.
        let mut drv = DrvAsynSerialPort::new("s1", "/dev/ttyS0").unwrap();

        let err = drv
            .set_option(&mut AsynUser::default(), "custom", "value")
            .unwrap_err();
        assert!(matches!(err, AsynError::OptionNotFound(_)));
        // R11-49: and it says so in C's words (drvAsynSerialPort.c:595-596).
        assert_eq!(err.message(), "Unsupported key \"custom\"");
        assert!(drv.get_option("custom").is_err());

        // The empty key is not an error. With no open fd there is nothing to
        // re-apply, so it is a no-op here; the connected re-apply path is
        // covered by pty_empty_key_reapplies_configured_termios.
        drv.set_option(&mut AsynUser::default(), "", "ignored")
            .unwrap();
    }

    #[test]
    fn test_get_option_not_found() {
        let drv = DrvAsynSerialPort::new("s1", "/dev/ttyS0").unwrap();
        assert!(drv.get_option("nonexistent").is_err());
    }

    #[test]
    fn test_read_write_when_disconnected() {
        let mut drv = DrvAsynSerialPort::new("s1", "/dev/ttyS0").unwrap();
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let mut buf = [0u8; 32];
        assert!(drv.read_octet(&user, &mut buf).is_err());
        let mut user = AsynUser::new(0);
        assert!(drv.write_octet(&mut user, b"hello").is_err());
    }

    #[test]
    fn test_baud_speed_roundtrip() {
        // 0 is intentionally excluded: it maps only on macOS/BSD (passthrough),
        // not on Linux, where C rejects it — see baud_arbitrary_on_bsd_mapped_set_on_linux.
        for baud in [
            50, 75, 110, 134, 150, 200, 300, 600, 1200, 1800, 2400, 4800, 9600, 19200, 38400,
            57600, 115200, 230400,
        ] {
            let speed = baud_to_speed(baud).expect("standard rate must map");
            assert_eq!(
                speed_to_baud(speed),
                baud,
                "roundtrip failed for baud={baud}"
            );
        }
    }

    // --- PTY integration tests ---

    fn create_pty_pair() -> Option<(RawFd, RawFd, String)> {
        let mut master: RawFd = 0;
        let mut slave: RawFd = 0;
        let mut name_buf = [0u8; 256];

        let ret = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                name_buf.as_mut_ptr() as *mut libc::c_char,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if ret < 0 {
            return None;
        }

        let name = unsafe {
            std::ffi::CStr::from_ptr(name_buf.as_ptr() as *const libc::c_char)
                .to_string_lossy()
                .into_owned()
        };

        Some((master, slave, name))
    }

    struct PtyGuard {
        master: RawFd,
        slave: RawFd,
    }

    impl Drop for PtyGuard {
        fn drop(&mut self) {
            unsafe {
                libc::close(self.master);
                libc::close(self.slave);
            }
        }
    }

    #[test]
    fn test_pty_connect_disconnect() {
        let (master, slave, slave_name) = match create_pty_pair() {
            Some(v) => v,
            None => {
                eprintln!("openpty not available, skipping test");
                return;
            }
        };
        // Close slave — driver will reopen it
        unsafe { libc::close(slave) };
        let _guard = PtyGuard { master, slave: -1 };

        let mut drv = DrvAsynSerialPort::new("pty_test", &slave_name).unwrap();
        let user = AsynUser::default();

        assert!(!drv.base().is_connected());
        drv.connect(&user).unwrap();
        assert!(drv.base().is_connected());

        drv.disconnect(&user).unwrap();
        assert!(!drv.base().is_connected());
    }

    /// DRV-57: a zero-length serial read request must be rejected with
    /// asynError (C drvAsynSerialPort.c:871-875), NOT fall through to
    /// libc::read(fd, ptr, 0) -> 0 -> EOF -> disconnect, which would tear
    /// down a live serial port. The connection must stay up.
    #[test]
    fn pty_zero_length_read_rejected_not_eof_teardown() {
        let (master, slave, slave_name) = match create_pty_pair() {
            Some(v) => v,
            None => {
                eprintln!("openpty not available, skipping test");
                return;
            }
        };
        unsafe { libc::close(slave) };
        let _guard = PtyGuard { master, slave: -1 };

        let mut drv = DrvAsynSerialPort::new("pty_maxchars", &slave_name).unwrap();
        let user = AsynUser::default();
        drv.connect(&user).unwrap();
        assert!(drv.base().is_connected());

        let ruser = AsynUser::new(0).with_timeout(Duration::from_millis(50));
        let mut empty: [u8; 0] = [];
        let res = drv.read_octet(&ruser, &mut empty);
        assert!(
            matches!(
                res,
                Err(AsynError::Status {
                    status: AsynStatus::Error,
                    ..
                })
            ),
            "zero-length serial read must be rejected with asynError, got {res:?}"
        );
        assert!(
            drv.base().is_connected(),
            "zero-length serial read must not tear down the connection"
        );
    }

    /// DRV-36: the empty `set_option` key is not an error and, when the port is
    /// open, re-applies the configured line state to the device (C
    /// drvAsynSerialPort.c setOption :609-615 → applyOptions :119-126, which
    /// re-pushes the cached termios). Simulate another process clobbering the
    /// port's line settings and confirm the empty key restores the driver's
    /// configured state. CSTOPB is used as the observable: a single c_cflag bit
    /// that pty termios stores faithfully (unlike baud, which hardware may
    /// reject).
    #[test]
    fn pty_empty_key_reapplies_configured_termios() {
        let (master, slave, slave_name) = match create_pty_pair() {
            Some(v) => v,
            None => {
                eprintln!("openpty not available, skipping test");
                return;
            }
        };
        unsafe { libc::close(slave) };
        let _guard = PtyGuard { master, slave: -1 };

        let mut drv = DrvAsynSerialPort::new("pty_reapply", &slave_name).unwrap();
        let user = AsynUser::default();
        drv.connect(&user).unwrap();
        let fd = drv.io.fd.expect("connected fd");

        let read_cstopb = |fd: RawFd| -> bool {
            let mut t: libc::termios = unsafe { std::mem::zeroed() };
            assert_eq!(unsafe { libc::tcgetattr(fd, &mut t) }, 0);
            (t.c_cflag & libc::CSTOPB) != 0
        };

        // The stop-bit state the driver pushed at connect (per config).
        let configured = read_cstopb(fd);

        // Externally flip the stop-bit width, as another process sharing the
        // port would, and confirm the clobber actually took effect.
        {
            let mut t: libc::termios = unsafe { std::mem::zeroed() };
            assert_eq!(unsafe { libc::tcgetattr(fd, &mut t) }, 0);
            if configured {
                t.c_cflag &= !libc::CSTOPB;
            } else {
                t.c_cflag |= libc::CSTOPB;
            }
            assert_eq!(unsafe { libc::tcsetattr(fd, libc::TCSANOW, &t) }, 0);
        }
        assert_eq!(
            read_cstopb(fd),
            !configured,
            "external clobber must take effect before the re-apply"
        );

        // The empty key re-applies the configured termios, overwriting the
        // clobber — C applyOptions re-pushes the cached config, not the
        // device's current state.
        drv.set_option(&mut AsynUser::default(), "", "").unwrap();
        assert_eq!(
            read_cstopb(fd),
            configured,
            "empty-key set_option must restore the configured line state"
        );

        drv.disconnect(&user).unwrap();
    }

    #[test]
    fn test_pty_write_read_roundtrip() {
        let (master, slave, slave_name) = match create_pty_pair() {
            Some(v) => v,
            None => {
                eprintln!("openpty not available, skipping test");
                return;
            }
        };
        unsafe { libc::close(slave) };
        let _guard = PtyGuard { master, slave: -1 };

        let mut drv = DrvAsynSerialPort::new("pty_test", &slave_name).unwrap();
        let user = AsynUser::default();
        drv.connect(&user).unwrap();

        // Write from driver, read from master
        let mut user = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        drv.write_octet(&mut user, b"hello").unwrap();

        let mut buf = [0u8; 32];
        let n = unsafe { libc::read(master, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        assert!(n > 0);
        assert_eq!(&buf[..n as usize], b"hello");

        // Write from master, read from driver
        let msg = b"world";
        unsafe { libc::write(master, msg.as_ptr() as *const libc::c_void, msg.len()) };

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        let mut rbuf = [0u8; 32];
        let n = drv.read_octet(&user, &mut rbuf).unwrap();
        assert_eq!(&rbuf[..n], b"world");
    }

    #[test]
    fn test_pty_read_timeout() {
        let (master, slave, slave_name) = match create_pty_pair() {
            Some(v) => v,
            None => {
                eprintln!("openpty not available, skipping test");
                return;
            }
        };
        unsafe { libc::close(slave) };
        let _guard = PtyGuard { master, slave: -1 };

        let mut drv = DrvAsynSerialPort::new("pty_test", &slave_name).unwrap();
        let user = AsynUser::default();
        drv.connect(&user).unwrap();

        // Don't write anything — read should timeout
        let user = AsynUser::new(0).with_timeout(Duration::from_millis(100));
        let mut buf = [0u8; 32];
        let err = drv.read_octet(&user, &mut buf).unwrap_err();
        match err {
            AsynError::Status {
                status: AsynStatus::Timeout,
                ..
            } => {}
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    #[test]
    fn test_pty_read_error_disconnects() {
        // DRV-31: a fatal read error / EOF must tear the connection down so
        // the actor's auto-reconnect re-opens the device. Without it the port
        // stays `connected` with a dead fd and never self-heals.
        let (master, slave, slave_name) = match create_pty_pair() {
            Some(v) => v,
            None => {
                eprintln!("openpty not available, skipping test");
                return;
            }
        };
        unsafe { libc::close(slave) };

        let mut drv = DrvAsynSerialPort::new("pty_test", &slave_name).unwrap();
        drv.connect(&AsynUser::default()).unwrap();
        assert!(drv.base().is_connected());

        // Break the link: closing the master makes the driver's slave fd
        // return EOF (macOS) or EIO (Linux) on the next read — both fatal.
        unsafe { libc::close(master) };

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let mut buf = [0u8; 32];
        let err = drv.read_octet(&user, &mut buf).unwrap_err();
        assert!(
            err.is_fatal_transport(),
            "expected a fatal transport error, got {err:?}"
        );
        assert!(
            !drv.base().is_connected(),
            "DRV-31: fatal read error must set connected=false"
        );
    }

    #[test]
    fn test_pty_write_error_disconnects() {
        // DRV-31 (write side, symmetric with read): a fatal write error must
        // tear the connection down so the actor's auto-reconnect re-opens the
        // device — same closeConnection contract C applies in writeIt
        // (drvAsynSerialPort.c:837).
        let (master, slave, slave_name) = match create_pty_pair() {
            Some(v) => v,
            None => {
                eprintln!("openpty not available, skipping test");
                return;
            }
        };
        unsafe { libc::close(slave) };

        let mut drv = DrvAsynSerialPort::new("pty_test", &slave_name).unwrap();
        drv.connect(&AsynUser::default()).unwrap();
        assert!(drv.base().is_connected());

        // Break the link: closing the master makes the driver's slave fd return
        // a fatal error (EIO) on the next write.
        unsafe { libc::close(master) };

        let mut user = AsynUser::new(0).with_timeout(Duration::from_secs(1));
        let err = drv.write_octet(&mut user, b"hello world").unwrap_err();
        assert!(
            err.is_fatal_transport(),
            "expected a fatal transport error, got {err:?}"
        );
        assert!(
            !drv.base().is_connected(),
            "DRV-31: fatal write error must set connected=false"
        );
    }

    /// R7-47 (same defect family as the IP port's `is_timeout`): reads and
    /// writes dispatch through the interpose chain, and `configure` installs
    /// the EOS interpose by default — which wraps a lower-layer hangup as
    /// `PartialRead` (C `asynInterposeEos.c:242-253` returns the lower status
    /// unchanged). Classifying by the error *variant* read that hangup as
    /// non-fatal and left a dead fd reporting `connected`; the status is the
    /// contract.
    #[test]
    fn fatal_transport_error_sees_through_the_eos_interpose_wrapper() {
        use crate::interpose::PartialOctetRead;

        let wrapped_hangup = AsynError::Status {
            status: AsynStatus::Disconnected,
            message: "hangup".into(),
        }
        .with_partial_read(PartialOctetRead {
            data: b"AB".to_vec(),
            eom_reason: EomReason::empty(),
        });
        assert!(
            wrapped_hangup.is_fatal_transport(),
            "a hangup wrapped by the EOS interpose is still fatal"
        );

        // Boundary: a timeout — wrapped the same way — must stay non-fatal, or
        // every EOS-port read timeout would tear the link down.
        let wrapped_timeout = AsynError::Status {
            status: AsynStatus::Timeout,
            message: "read timeout".into(),
        }
        .with_partial_read(PartialOctetRead {
            data: b"AB".to_vec(),
            eom_reason: EomReason::empty(),
        });
        assert!(
            !wrapped_timeout.is_fatal_transport(),
            "a partial-line timeout leaves the fd intact (C returns asynTimeout)"
        );

        // R8-48 boundary — the case the status-only rule still missed: a real
        // errno (`Io`, not a status variant) is what a mid-line hangup actually
        // looks like, and the carrier used to flatten it to a bare
        // status=Error. Both partial carriers must let the errno through.
        let wrapped_errno =
            AsynError::Io(std::io::Error::other("EIO")).with_partial_read(PartialOctetRead {
                data: b"AB".to_vec(),
                eom_reason: EomReason::empty(),
            });
        assert!(
            wrapped_errno.is_fatal_transport(),
            "an errno behind the read carrier is still fatal"
        );
        let half_written = AsynError::Io(std::io::Error::other("EIO")).with_partial_write(2);
        assert!(
            half_written.is_fatal_transport(),
            "an errno behind the write carrier is still fatal"
        );
    }

    #[test]
    fn test_pty_eos_interpose() {
        use crate::interpose::eos::{EosConfig, EosInterpose};

        let (master, slave, slave_name) = match create_pty_pair() {
            Some(v) => v,
            None => {
                eprintln!("openpty not available, skipping test");
                return;
            }
        };
        unsafe { libc::close(slave) };
        let _guard = PtyGuard { master, slave: -1 };

        let mut drv = DrvAsynSerialPort::new("pty_test", &slave_name).unwrap();
        let eos = EosInterpose::new(EosConfig {
            input_eos: vec![b'\r', b'\n'],
            output_eos: vec![],
        });
        drv.install_interpose(Box::new(eos));

        let user = AsynUser::default();
        drv.connect(&user).unwrap();

        // Master sends "OK\r\n"
        let msg = b"OK\r\n";
        unsafe { libc::write(master, msg.as_ptr() as *const libc::c_void, msg.len()) };

        let user = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        let mut buf = [0u8; 32];
        // The EOS layer is the port's, not the driver's: a caller enters the
        // chain at the top and the driver is its base (C findInterface,
        // asynManager.c:1493-1501). `PortActor` does exactly this.
        let (n, _eom) = crate::port::octet_read_chain(&mut drv, &user, &mut buf).unwrap();
        // EOS should strip the terminator
        assert_eq!(&buf[..n], b"OK");
    }

    #[test]
    fn test_pty_set_option_baud() {
        let (master, slave, slave_name) = match create_pty_pair() {
            Some(v) => v,
            None => {
                eprintln!("openpty not available, skipping test");
                return;
            }
        };
        unsafe { libc::close(slave) };
        let _guard = PtyGuard { master, slave: -1 };

        let mut drv = DrvAsynSerialPort::new("pty_test", &slave_name).unwrap();
        let user = AsynUser::default();
        drv.connect(&user).unwrap();

        drv.set_option(&mut AsynUser::default(), "baud", "115200")
            .unwrap();
        assert_eq!(drv.baud, 115200);

        // Verify via tcgetattr
        let t = drv.get_current_termios().unwrap();
        let actual_speed = unsafe { libc::cfgetospeed(&t) };
        assert_eq!(actual_speed, libc::B115200);
    }

    /// R7-48: `clocal` is cached state, not live-termios-only state.
    ///
    /// C seeds CLOCAL into `tty->termios` (drvAsynSerialPort.c:1077), lets
    /// `setOption` clear it in the cache unconditionally (:410-419), re-pushes
    /// the cache at every connect via `applyOptions` (:105-130, :722) and reads
    /// it back from the cache (:169-170). The port instead mutated the live
    /// termios only when connected, stored nothing, force-set CREAD|CLOCAL at
    /// every connect, and hard-coded "N" for a disconnected readback — so
    /// modem-control mode could not be held across the actor's auto-reconnect
    /// and could not be set at all while the line was down.
    #[test]
    fn clocal_survives_reconnect_and_can_be_set_while_disconnected() {
        let (master, slave, slave_name) = match create_pty_pair() {
            Some(v) => v,
            None => {
                eprintln!("openpty not available, skipping test");
                return;
            }
        };
        unsafe { libc::close(slave) };
        let _guard = PtyGuard { master, slave: -1 };

        let mut drv = DrvAsynSerialPort::new("pty_test", &slave_name).unwrap();

        // C's seed has CLOCAL on, and getOption answers from the cache even
        // while the port has never been opened — "Y", not "N".
        assert_eq!(
            drv.get_option("clocal").unwrap(),
            "Y",
            "C seeds CS8|CLOCAL|CREAD (:1077) and getOption reads the cache"
        );

        // Set while DISCONNECTED: C mutates the cache regardless of fd.
        drv.set_option(&mut AsynUser::default(), "clocal", "N")
            .unwrap();
        assert_eq!(
            drv.get_option("clocal").unwrap(),
            "N",
            "an option set while the line is down is held, not dropped"
        );

        // Connect: applyOptions pushes the cache, so CLOCAL must be OFF on the
        // device — the old force-set of CREAD|CLOCAL at connect re-enabled it.
        drv.connect(&AsynUser::default()).unwrap();
        let live = drv.get_current_termios().unwrap();
        assert_eq!(
            live.c_cflag & libc::CLOCAL,
            0,
            "connect must push the cached clocal=N, not force CLOCAL back on"
        );
        assert_ne!(
            live.c_cflag & libc::CREAD,
            0,
            "applyOptions still forces CREAD (C :119)"
        );

        // Disconnect/reconnect (the actor's auto-reconnect path): the cache is
        // the only source, so the setting must survive.
        drv.disconnect(&AsynUser::default()).unwrap();
        assert_eq!(drv.get_option("clocal").unwrap(), "N");
        drv.connect(&AsynUser::default()).unwrap();
        let live = drv.get_current_termios().unwrap();
        assert_eq!(
            live.c_cflag & libc::CLOCAL,
            0,
            "clocal=N must survive the reconnect (C re-pushes tty->termios)"
        );

        // And back on, while connected.
        drv.set_option(&mut AsynUser::default(), "clocal", "Y")
            .unwrap();
        let live = drv.get_current_termios().unwrap();
        assert_ne!(live.c_cflag & libc::CLOCAL, 0);
        assert_eq!(drv.get_option("clocal").unwrap(), "Y");
    }

    /// R7-49: the `ixon`/`ixoff`/`ixany` iflags read back from the cache, per
    /// flag, exactly like the cflag family.
    ///
    /// C `getOption` answers all three from `tty->termios.c_iflag`
    /// (drvAsynSerialPort.c:181-201) — the hard-coded 'N' for ixany/ixoff
    /// lives inside `#ifdef vxWorks`. This port read the *live* termios when
    /// connected and returned a flat "N" when not, so on a closed port every
    /// one of them read "N" no matter what had been set, and there was no
    /// per-flag state at all: the three flags shared `SerialConfig`'s single
    /// `FlowControl` enum, which cannot express "ixon without ixoff" or ixany.
    #[test]
    fn iflag_family_reads_back_per_flag_from_the_cache() {
        let (master, slave, slave_name) = match create_pty_pair() {
            Some(v) => v,
            None => {
                eprintln!("openpty not available, skipping test");
                return;
            }
        };
        unsafe { libc::close(slave) };
        let _guard = PtyGuard { master, slave: -1 };

        let mut drv = DrvAsynSerialPort::new("pty_test", &slave_name).unwrap();

        // Default (no flow control in the configure string): all three off.
        assert_eq!(drv.get_option("ixon").unwrap(), "N");
        assert_eq!(drv.get_option("ixoff").unwrap(), "N");
        assert_eq!(drv.get_option("ixany").unwrap(), "N");

        // Set two of the three while the line is DOWN. Each flag is
        // independent — ixon on, ixoff still off, ixany on.
        drv.set_option(&mut AsynUser::default(), "ixon", "Y")
            .unwrap();
        drv.set_option(&mut AsynUser::default(), "ixany", "Y")
            .unwrap();
        assert_eq!(
            drv.get_option("ixon").unwrap(),
            "Y",
            "a disconnected readback must report the cache, not a flat N"
        );
        assert_eq!(drv.get_option("ixoff").unwrap(), "N");
        assert_eq!(
            drv.get_option("ixany").unwrap(),
            "Y",
            "ixany is a real cached flag on POSIX (C hard-codes N only on vxWorks)"
        );

        // They land on the device at connect, still per flag.
        drv.connect(&AsynUser::default()).unwrap();
        let live = drv.get_current_termios().unwrap();
        assert_ne!(live.c_iflag & libc::IXON, 0);
        assert_eq!(live.c_iflag & libc::IXOFF, 0);
        assert_ne!(live.c_iflag & libc::IXANY, 0);

        // Set the third while connected, then take the line down and back up:
        // the cache is the owner, so the readback and the device both hold.
        drv.set_option(&mut AsynUser::default(), "ixoff", "Y")
            .unwrap();
        assert_eq!(drv.get_option("ixoff").unwrap(), "Y");
        drv.disconnect(&AsynUser::default()).unwrap();
        assert_eq!(drv.get_option("ixoff").unwrap(), "Y");
        assert_eq!(drv.get_option("ixon").unwrap(), "Y");
        drv.connect(&AsynUser::default()).unwrap();
        let live = drv.get_current_termios().unwrap();
        assert_ne!(live.c_iflag & libc::IXOFF, 0);
        assert_ne!(live.c_iflag & libc::IXON, 0);

        // And clearing one clears only that one.
        drv.set_option(&mut AsynUser::default(), "ixon", "N")
            .unwrap();
        assert_eq!(drv.get_option("ixon").unwrap(), "N");
        assert_eq!(drv.get_option("ixoff").unwrap(), "Y");
        assert_eq!(drv.get_option("ixany").unwrap(), "Y");
        let live = drv.get_current_termios().unwrap();
        assert_eq!(live.c_iflag & libc::IXON, 0);
        assert_ne!(live.c_iflag & libc::IXOFF, 0);
    }

    /// R7-48 (migration guard, not the regression pin): the rest of the cflag
    /// family set while disconnected must still be held, and still land on the
    /// device at the next connect, now that the owner is the `termios` cache
    /// instead of the old `SerialConfig` mirror. This one passes on the unfixed
    /// tree as well — it exists so the ownership move does not quietly lose
    /// what the old config path did get right. The failing-without-the-fix test
    /// is `clocal_survives_reconnect_and_can_be_set_while_disconnected`.
    #[test]
    fn cflag_options_set_while_disconnected_apply_at_connect() {
        let (master, slave, slave_name) = match create_pty_pair() {
            Some(v) => v,
            None => {
                eprintln!("openpty not available, skipping test");
                return;
            }
        };
        unsafe { libc::close(slave) };
        let _guard = PtyGuard { master, slave: -1 };

        let mut drv = DrvAsynSerialPort::new("pty_test", &slave_name).unwrap();
        drv.set_option(&mut AsynUser::default(), "bits", "7")
            .unwrap();
        drv.set_option(&mut AsynUser::default(), "parity", "odd")
            .unwrap();
        drv.set_option(&mut AsynUser::default(), "stop", "2")
            .unwrap();
        drv.set_option(&mut AsynUser::default(), "crtscts", "Y")
            .unwrap();

        // Readback comes from the cache, not the (still absent) device.
        assert_eq!(drv.get_option("bits").unwrap(), "7");
        assert_eq!(drv.get_option("parity").unwrap(), "odd");
        assert_eq!(drv.get_option("stop").unwrap(), "2");
        assert_eq!(drv.get_option("crtscts").unwrap(), "Y");

        drv.connect(&AsynUser::default()).unwrap();
        let live = drv.get_current_termios().unwrap();
        // The Linux pty driver rewrites `CSIZE`/`PARENB` on every tcsetattr
        // (`pty_set_termios` forces CS8 and clears PARENB), so those two cannot
        // be observed on this fixture — measured, not assumed. `CSTOPB` and
        // `CRTSCTS` do round-trip, and they are enough to show the cache (not a
        // rebuilt-from-defaults termios) is what connect pushes.
        assert_ne!(
            live.c_cflag & libc::CSTOPB,
            0,
            "stop=2 set while disconnected must land on the device at connect"
        );
        assert_ne!(
            live.c_cflag & libc::CRTSCTS,
            0,
            "crtscts=Y set while disconnected must land on the device at connect"
        );
    }

    #[test]
    fn test_pty_connect_rejects_double_open() {
        // C drvAsynSerialPort.c::connectIt (694-698) returns asynError
        // "Link already open!" on a connect to an already-open link,
        // rather than opening a second fd and leaking the first.
        let (master, slave, slave_name) = match create_pty_pair() {
            Some(v) => v,
            None => {
                eprintln!("openpty not available, skipping test");
                return;
            }
        };
        unsafe { libc::close(slave) };
        let _guard = PtyGuard { master, slave: -1 };

        let mut drv = DrvAsynSerialPort::new("pty_test", &slave_name).unwrap();
        let user = AsynUser::default();
        drv.connect(&user).unwrap();
        let first_fd = drv.io.fd;
        assert!(first_fd.is_some());

        let err = drv.connect(&user).unwrap_err();
        assert!(matches!(err, AsynError::Status { .. }));
        // The original fd (and its saved termios) is left intact.
        assert_eq!(drv.io.fd, first_fd);
        assert!(drv.saved_termios.is_some());
    }

    #[test]
    fn test_pty_runtime_integration() {
        use crate::runtime::{RuntimeConfig, create_port_runtime};

        let (master, slave, slave_name) = match create_pty_pair() {
            Some(v) => v,
            None => {
                eprintln!("openpty not available, skipping test");
                return;
            }
        };
        unsafe { libc::close(slave) };
        let _guard = PtyGuard { master, slave: -1 };

        let drv = DrvAsynSerialPort::new("pty_rt", &slave_name).unwrap();
        let (runtime_handle, _jh) = create_port_runtime(drv, RuntimeConfig::default())
            .expect("the port runtime thread must start");
        let ph = runtime_handle.port_handle();

        // Write via PortHandle
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        ph.submit_blocking(
            crate::request::RequestOp::OctetWrite {
                data: b"ping".to_vec(),
            },
            user,
        )
        .unwrap();

        // Read from master
        let mut buf = [0u8; 32];
        let n = unsafe { libc::read(master, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        assert!(n > 0);
        assert_eq!(&buf[..n as usize], b"ping");

        // Master sends response
        let resp = b"pong";
        unsafe { libc::write(master, resp.as_ptr() as *const libc::c_void, resp.len()) };

        // Read via PortHandle
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        let result = ph
            .submit_blocking(crate::request::RequestOp::OctetRead { buf_size: 32 }, user)
            .unwrap();
        assert_eq!(result.data.as_deref(), Some(b"pong".as_slice()));

        runtime_handle.shutdown_and_wait();
    }

    #[test]
    fn test_pty_termios_restored_on_disconnect() {
        let (master, slave, slave_name) = match create_pty_pair() {
            Some(v) => v,
            None => {
                eprintln!("openpty not available, skipping test");
                return;
            }
        };
        unsafe { libc::close(slave) };
        let _guard = PtyGuard { master, slave: -1 };

        // Read original termios before the driver touches it
        let mut drv = DrvAsynSerialPort::new("pty_test", &slave_name).unwrap();
        let user = AsynUser::default();
        drv.connect(&user).unwrap();

        // saved_termios should exist
        assert!(drv.saved_termios.is_some());
        let saved = drv.saved_termios.unwrap();

        // cfmakeraw changes key flags; verify they differ now
        let current = drv.get_current_termios().unwrap();
        // Raw mode typically clears ECHO, ICANON in c_lflag
        assert_ne!(
            current.c_lflag & libc::ECHO,
            saved.c_lflag & libc::ECHO,
            "raw mode should have changed ECHO flag"
        );

        // Re-set saved_termios (disconnect reads from it)
        drv.saved_termios = Some(saved);
        drv.disconnect(&user).unwrap();
        assert!(drv.saved_termios.is_none());
        assert!(!drv.base().is_connected());

        // Now reopen and verify key flags were restored by reading termios
        // from the same PTY slave path. Re-open to read the restored state.
        let c_path = std::ffi::CString::new(slave_name.as_str()).unwrap();
        let fd2 = unsafe {
            libc::open(
                c_path.as_ptr(),
                libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK,
            )
        };
        if fd2 >= 0 {
            let mut restored: libc::termios = unsafe { std::mem::zeroed() };
            if unsafe { libc::tcgetattr(fd2, &mut restored) } == 0 {
                // Compare key flags (kernel may adjust some bits, so check important ones)
                assert_eq!(
                    restored.c_lflag & libc::ECHO,
                    saved.c_lflag & libc::ECHO,
                    "ECHO flag should be restored"
                );
                assert_eq!(
                    restored.c_lflag & libc::ICANON,
                    saved.c_lflag & libc::ICANON,
                    "ICANON flag should be restored"
                );
                assert_eq!(
                    restored.c_cflag & libc::CSIZE,
                    saved.c_cflag & libc::CSIZE,
                    "CSIZE should be restored"
                );
            }
            unsafe { libc::close(fd2) };
        }
    }

    /// DRV-32: C (drvAsynSerialPort.c:1080) sets the default input flags to
    /// IGNBRK | IGNPAR so a line BREAK / framing-parity error is ignored
    /// rather than delivered as a spurious 0x00 byte. cfmakeraw clears IGNBRK
    /// and never sets IGNPAR, so the driver must restore them after cfmakeraw.
    #[test]
    fn pty_termios_sets_ignbrk_ignpar() {
        let (master, slave, slave_name) = match create_pty_pair() {
            Some(v) => v,
            None => {
                eprintln!("openpty not available, skipping test");
                return;
            }
        };
        unsafe { libc::close(slave) };
        let _guard = PtyGuard { master, slave: -1 };

        let mut drv = DrvAsynSerialPort::new("pty_ignbrk", &slave_name).unwrap();
        drv.connect(&AsynUser::default()).unwrap();

        let t = drv.get_current_termios().unwrap();
        assert_ne!(
            t.c_iflag & libc::IGNBRK,
            0,
            "IGNBRK must be set (C default, drvAsynSerialPort.c:1080)"
        );
        assert_ne!(
            t.c_iflag & libc::IGNPAR,
            0,
            "IGNPAR must be set (C default, drvAsynSerialPort.c:1080)"
        );
    }

    /// DRV-33: C (drvAsynSerialPort.c:1085-1086) seeds the XON/XOFF flow
    /// characters to ^Q (0x11, VSTART) and ^S (0x13, VSTOP). cfmakeraw leaves
    /// c_cc untouched and `t` is zeroed first, so the driver must set them or
    /// software flow control would use NUL instead of ^Q/^S.
    #[test]
    fn pty_termios_sets_xon_xoff_chars() {
        let (master, slave, slave_name) = match create_pty_pair() {
            Some(v) => v,
            None => {
                eprintln!("openpty not available, skipping test");
                return;
            }
        };
        unsafe { libc::close(slave) };
        let _guard = PtyGuard { master, slave: -1 };

        let mut drv = DrvAsynSerialPort::new("pty_xonxoff", &slave_name).unwrap();
        drv.connect(&AsynUser::default()).unwrap();

        let t = drv.get_current_termios().unwrap();
        assert_eq!(t.c_cc[libc::VSTART], 0x11, "VSTART must be ^Q (C default)");
        assert_eq!(t.c_cc[libc::VSTOP], 0x13, "VSTOP must be ^S (C default)");
    }

    /// Invariant: the serial fd is non-blocking for its whole life — at every
    /// point an observer can look, not just inside `write`.
    ///
    /// The three boundaries are right after `connect` (which used to clear
    /// O_NONBLOCK), after a `write` (which used to set it and restore it), and
    /// after a `read`. Checking all three is the point: the defect this
    /// replaces was not a wrong mode, it was a mode that *depended on where
    /// you looked from*, and only a per-boundary check catches that.
    #[test]
    fn pty_fd_is_non_blocking_at_every_boundary() {
        let (master, slave, slave_name) = match create_pty_pair() {
            Some(v) => v,
            None => {
                eprintln!("openpty not available, skipping test");
                return;
            }
        };
        unsafe { libc::close(slave) };
        let _guard = PtyGuard { master, slave: -1 };

        let mut drv = DrvAsynSerialPort::new("pty_nonblock", &slave_name).unwrap();
        drv.connect(&AsynUser::default()).unwrap();
        let fd = drv.io.fd.expect("connected");

        let nonblocking = |where_: &str| {
            let fl = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            assert!(fl >= 0, "F_GETFL failed at {where_}");
            assert_ne!(
                fl & libc::O_NONBLOCK,
                0,
                "fd must be non-blocking {where_}, flags=0x{fl:x}"
            );
        };

        nonblocking("after connect");

        let mut user = AsynUser {
            timeout: Duration::from_millis(200),
            ..Default::default()
        };
        drv.write_octet(&mut user, b"probe\n").unwrap();
        nonblocking("after write");

        // The read times out (nothing is driving the master end); what matters
        // is the fd's mode on the way out, not the outcome.
        let mut buf = [0u8; 8];
        let _ = drv.read_octet(&user, &mut buf);
        nonblocking("after read");
    }

    /// Boundary: this host HAS every termios facility, so `platform` must
    /// report every one of them present.
    ///
    /// The `None` arms exist for VxWorks and cannot be exercised here — but
    /// the failure this guards against is the opposite direction and is very
    /// much reachable: widen `platform`'s `cfg` by one target, or invert it,
    /// and a hosted build silently starts refusing `ixany`, stops seeding
    /// ^Q/^S, and answers `drain_output` with an error. Nothing else would
    /// fail, because every consumer handles `None` as a legitimate answer.
    /// So the assertion is that `None` is *not* legitimate here.
    #[test]
    fn platform_reports_every_facility_present_on_this_host() {
        assert_eq!(
            platform::IXANY,
            Some(libc::IXANY),
            "hosted unix has an IXANY bit"
        );
        assert_eq!(
            platform::SOFT_FLOW_CHARS,
            Some((libc::VSTART, libc::VSTOP)),
            "hosted unix has programmable XON/XOFF characters"
        );
        assert_eq!(
            platform::CRTSCTS,
            libc::CRTSCTS,
            "the seam must be transparent where libc has the bit"
        );
        assert_eq!(platform::O_NOCTTY, libc::O_NOCTTY);
        assert_eq!(
            platform::FLUSH_IO,
            libc::TCIOFLUSH,
            "hosted unix can discard both queues at once"
        );
        // A closed fd: `drain` must still answer `Some` (the platform *has*
        // tcdrain), reporting EBADF rather than the `None` that means "no
        // such call exists here".
        assert!(
            matches!(platform::drain(-1), Some(Err(_))),
            "hosted unix has tcdrain; -1 must fail as an fd, not as a facility"
        );
    }

    /// A speed the platform's own `cfsetospeed` refuses must come back as an
    /// error, not as a silently unchanged rate.
    ///
    /// `baud_to_speed` keeps an invalid code from reaching here through the
    /// public API on this host, so the owner is exercised directly — which is
    /// the point: it is the owner, not the caller, that has to refuse. The
    /// failure this reproduces was measured on RTEMS, where the rate reached
    /// `cfsetospeed` and was dropped.
    ///
    /// Runs where the speed argument is an encoded code, because that is where
    /// a refusable input exists. On the rate-valued hosts — macOS and the BSDs
    /// — `cfsetospeed` is an assignment to `c_ospeed` that validates nothing
    /// and returns 0 for any value, so no `bogus` can reach the error arm; C
    /// accepts every rate there too (`baudCode = baud`). RTEMS is nominally in
    /// that group but its `cfsetospeed` does enforce a rate table, which is
    /// what this reproduces — asserted on target, since RTEMS runs no
    /// `cargo test`.
    #[cfg(not(asyn_baud_code_is_rate))]
    #[test]
    fn a_speed_the_platform_refuses_is_an_error_not_a_silent_no_op() {
        let mut t: platform::termios = unsafe { std::mem::zeroed() };
        set_termios_speed(&mut t, platform::B9600).expect("9600 is settable everywhere");
        let before = unsafe { libc::cfgetospeed(&t) };
        let bogus = platform::Speed::MAX / 2;
        let r = set_termios_speed(&mut t, bogus);
        assert!(
            matches!(&r, Err(AsynError::Status { message, .. })
                     if message.starts_with("cfsetispeed returned")
                     || message.starts_with("cfsetospeed returned")),
            "must name the call C names, got {r:?}"
        );
        assert_eq!(
            unsafe { libc::cfgetospeed(&t) },
            before,
            "a refused speed must leave the previous rate in place"
        );
    }

    /// Every `c_cc` index the seam hands out has to be inside the array the
    /// platform's own `struct termios` declares — `seed_termios` writes
    /// through all four without a bounds question.
    ///
    /// On RTEMS this is a `const` assertion next to the struct, because the
    /// target cannot run these tests; here it is checked against the real
    /// array, so an arm that took an index from another platform's `c_cc`
    /// layout is caught rather than corrupting whatever follows the field.
    #[test]
    fn every_c_cc_index_the_seam_hands_out_is_inside_the_array() {
        let t: platform::termios = unsafe { std::mem::zeroed() };
        let n = t.c_cc.len();
        assert!(
            platform::VMIN < n,
            "VMIN {} outside c_cc[{n}]",
            platform::VMIN
        );
        assert!(
            platform::VTIME < n,
            "VTIME {} outside c_cc[{n}]",
            platform::VTIME
        );
        if let Some((vstart, vstop)) = platform::SOFT_FLOW_CHARS {
            assert!(vstart < n, "VSTART {vstart} outside c_cc[{n}]");
            assert!(vstop < n, "VSTOP {vstop} outside c_cc[{n}]");
        }
    }

    /// The refusal `platform`'s `None` arms lean on, checked for shape here
    /// since the arms themselves only compile on VxWorks. C's wording is
    /// "Option ixany not supported on vxWorks" (drvAsynSerialPort.c:469-471).
    #[test]
    fn unsupported_option_names_the_key_and_the_platform() {
        let msg = match option_unsupported_here("ixany") {
            AsynError::Status { status, message } => {
                assert_eq!(status, AsynStatus::Error);
                message
            }
            other => panic!("expected a Status error, got {other:?}"),
        };
        assert!(msg.contains("ixany"), "must name the option: {msg}");
        assert!(
            msg.contains(std::env::consts::OS),
            "must name the platform: {msg}"
        );
    }

    /// DRV-35: C setOption (drvAsynSerialPort.c:601-604) restores the previous
    /// baud/termios if applyOptions fails. The Rust driver must not leave the
    /// cached config reporting a value the device rejected. Point the driver at
    /// a non-tty fd so the apply path (tcgetattr/tcsetattr) fails, then assert
    /// the cached baud is unchanged.
    #[test]
    fn set_option_does_not_commit_cached_config_on_apply_failure() {
        let mut drv = DrvAsynSerialPort::new("rollback", "/dev/null").unwrap();
        // A /dev/null fd is open but not a terminal: tcgetattr -> ENOTTY.
        let badfd = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDWR) };
        assert!(badfd >= 0, "could not open /dev/null");
        drv.io.fd = Some(badfd);

        // Default cached baud is 9600.
        assert_eq!(drv.get_option("baud").unwrap(), "9600");

        // The apply path fails (tcgetattr ENOTTY on /dev/null) ...
        let r = drv.set_option(&mut AsynUser::default(), "baud", "115200");
        assert!(r.is_err(), "set_option must fail when apply fails");

        // ... and the cached config must not have been mutated.
        assert_eq!(
            drv.get_option("baud").unwrap(),
            "9600",
            "cached baud must stay 9600 when apply fails (C restores baudPrev)"
        );

        // Clean up the fd ourselves (the driver never owned a real connection).
        drv.io.fd = None;
        unsafe { libc::close(badfd) };
    }

    /// DRV-37: C writeIt (drvAsynSerialPort.c:815-842) arms one timer for the
    /// whole write, so the timeout bounds TOTAL write time. The old Rust write
    /// reused the full timeout on every poll, so a peer that drains a little at
    /// a time (each gap under the timeout) would never trip a per-poll timeout
    /// and the write would run to completion well past the deadline. A slow
    /// drain (4 KiB / 10 ms ~= 400 KiB/s) keeps each POLLOUT gap far under the
    /// 300 ms timeout while a 512 KiB payload needs ~1.3 s to drain: the
    /// total-deadline fix times out at ~300 ms; the per-poll bug would complete.
    #[test]
    fn pty_write_timeout_bounds_total_not_per_poll() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let (master, slave, slave_name) = match create_pty_pair() {
            Some(v) => v,
            None => {
                eprintln!("openpty not available, skipping test");
                return;
            }
        };
        unsafe { libc::close(slave) };
        let _guard = PtyGuard { master, slave: -1 };

        let mut drv = DrvAsynSerialPort::new("pty_write_total", &slave_name).unwrap();
        drv.connect(&AsynUser::default()).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let reader = std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while !stop2.load(Ordering::Relaxed) {
                let n =
                    unsafe { libc::read(master, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
                if n <= 0 {
                    break; // slave closed (EOF) or error
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });

        let payload = vec![0x5Au8; 512 * 1024];
        let mut user = AsynUser::new(0).with_timeout(Duration::from_millis(300));
        let start = Instant::now();
        let res = drv.write_octet(&mut user, &payload);
        let elapsed = start.elapsed();

        // Close the slave fd so the reader's blocking read returns EOF and the
        // thread exits, then join before the PtyGuard closes the master fd.
        stop.store(true, Ordering::Relaxed);
        drv.disconnect(&AsynUser::default()).ok();
        let _ = reader.join();

        let err = match res {
            Err(e) => e,
            other => panic!("expected total-deadline Timeout, got {other:?}"),
        };
        assert_eq!(err.status(), AsynStatus::Timeout, "got {err:?}");
        // R8-48: C `writeIt` publishes `*nbytesTransfered = numchars - nleft`
        // on the timeout break (drvAsynSerialPort.c:849) — the bytes the port
        // already took ride out *with* the timeout instead of being dropped.
        // This write drained part of the payload into the pty before the
        // deadline, so the count must be a real partial: neither 0 nor the
        // whole payload.
        let sent = err
            .partial_write()
            .expect("a timed-out serial write must report what it transferred");
        assert!(
            sent > 0 && sent < payload.len(),
            "expected a partial count in 1..{}, got {sent}",
            payload.len()
        );
        assert!(
            elapsed < Duration::from_millis(900),
            "total write time must be bounded by ~the timeout, took {elapsed:?}"
        );
    }

    /// DRV-41: C connectIt (drvAsynSerialPort.c:713-722) sets FD_CLOEXEC on the
    /// serial fd right after open so it is not inherited across exec.
    #[test]
    fn pty_connect_sets_cloexec() {
        let (master, slave, slave_name) = match create_pty_pair() {
            Some(v) => v,
            None => {
                eprintln!("openpty not available, skipping test");
                return;
            }
        };
        unsafe { libc::close(slave) };
        let _guard = PtyGuard { master, slave: -1 };

        let mut drv = DrvAsynSerialPort::new("pty_cloexec", &slave_name).unwrap();
        drv.connect(&AsynUser::default()).unwrap();

        let fd = drv.io.fd.expect("connected fd");
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(flags >= 0, "F_GETFD failed");
        assert!(
            flags & libc::FD_CLOEXEC != 0,
            "FD_CLOEXEC must be set after connect (C parity)"
        );
    }

    /// DRV-44: C report (drvAsynSerialPort.c:666-680) shows cumulative
    /// nWritten/nRead. Verify the counters track real I/O and report() runs at
    /// every level without panicking.
    #[test]
    fn pty_report_tracks_byte_counters() {
        let (master, slave, slave_name) = match create_pty_pair() {
            Some(v) => v,
            None => {
                eprintln!("openpty not available, skipping test");
                return;
            }
        };
        unsafe { libc::close(slave) };
        let _guard = PtyGuard { master, slave: -1 };

        let mut drv = DrvAsynSerialPort::new("pty_report", &slave_name).unwrap();
        drv.connect(&AsynUser::default()).unwrap();
        assert_eq!(drv.io.n_written, 0);
        assert_eq!(drv.io.n_read, 0);

        let mut user = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        drv.write_octet(&mut user, b"hello").unwrap();
        assert_eq!(drv.io.n_written, 5, "n_written must track bytes written");

        let msg = b"world";
        unsafe { libc::write(master, msg.as_ptr() as *const libc::c_void, msg.len()) };
        let user = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        let mut rbuf = [0u8; 32];
        let n = drv.read_octet(&user, &mut rbuf).unwrap();
        assert!(n > 0);
        assert_eq!(drv.io.n_read, n as u64, "n_read must track bytes read");

        // report() must not panic at any level.
        let mut out = String::new();
        drv.report(&mut out, 0);
        drv.report(&mut out, 2);
    }
}
