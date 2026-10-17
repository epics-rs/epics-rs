//! pvxs `osiSockAttachExt`'s once-only work (`osdSockExt.cpp:42-58`), which
//! every pvxs event loop, client and `pvxinfo -D` runs before its first
//! socket: the IPv6 probe, whose lines print then.

use std::io;
use std::net::Ipv6Addr;

use crate::pvxs_log::{self, Level, SOCK};

/// pvxs `evsocket::canIPv6` (`evhelper.cpp:696-726`), probed once:
/// `PVXS_ENABLE_IPV6=NO` disables it, otherwise a UDP socket must bind
/// `[::1]`.
pub fn can_ipv6() -> bool {
    static CAN: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CAN.get_or_init(probe_ipv6)
}

/// `init_canIPv6` itself, its lines on `pvxs.sock`.
pub(crate) fn probe_ipv6() -> bool {
    if let Some(ena6) = epics_base_rs::runtime::env::get_lossy("PVXS_ENABLE_IPV6") {
        if ena6.eq_ignore_ascii_case("NO") {
            pvxs_log::log(SOCK, Level::Info, format_args!("IPv6 support disabled"));
            return false;
        } else if !ena6.eq_ignore_ascii_case("YES") {
            pvxs_log::log(
                SOCK,
                Level::Warn,
                format_args!("PVXS_ENABLE_IPV6={ena6} ignoring unrecognized"),
            );
        }
    }
    match std::net::UdpSocket::bind((Ipv6Addr::LOCALHOST, 0)) {
        Ok(_) => true,
        Err(e) => {
            // EAFNOSUPPORT: no IPv6 in the kernel; EADDRNOTAVAIL: no [::1].
            // pvxs on Windows compares the WinSock code with errno.h's
            // EAFNOSUPPORT (102) and so warns here; not copied.
            #[cfg(unix)]
            let no_family = e.raw_os_error() == Some(libc::EAFNOSUPPORT);
            #[cfg(windows)]
            let no_family = e.raw_os_error() == Some(10047); // WSAEAFNOSUPPORT
            if !no_family && e.kind() != io::ErrorKind::AddrNotAvailable {
                pvxs_log::log(
                    SOCK,
                    Level::Warn,
                    format_args!(
                        "Unexpected errno {} while probing IPv6: {}",
                        e.raw_os_error().unwrap_or(0),
                        epics_base_rs::net::sock_err_string(&e)
                    ),
                );
            }
            false
        }
    }
}
