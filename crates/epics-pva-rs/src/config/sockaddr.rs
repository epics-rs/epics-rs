//! pvxs `SockAddr::setAddress` (`util.cpp:444-548`) and the `GetAddrInfo` it
//! falls back to (`util.cpp:699-707`): one address of an address-list entry,
//! failing with the `what()` text pvxs's lists print.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use crate::pvdata::fmt::escape;

/// `char scratch[INET6_ADDRSTRLEN+1]`.
const SCRATCH: usize = 46 + 1;

/// `name` as `[ipv6]:port`, `ipv6`, `[ipv6]`, `ipv4:port`, `ipv4` or a host
/// name in place of either address, `defport` when it names no port.
pub(crate) fn set_address(name: &str, defport: u16) -> Result<SocketAddr, String> {
    let firstc = name.find(':');
    let lastc = name.rfind(':');
    let openb = name.find('[');
    let closeb = name.rfind(']');
    let (addr, port, v6) = match (openb, closeb) {
        (None, None) => match firstc {
            None => (name, None, false),
            Some(f) if firstc == lastc => {
                if f >= SCRATCH {
                    return Err(format!("IPv4 address too long \"{}\"", escape(name)));
                }
                (&name[..f], Some(&name[f + 1..]), false)
            }
            Some(_) => (name, None, true),
        },
        (Some(o), Some(c)) => {
            // `closeb-openb-1u` is a size_t: `]` before `[` wraps to "too long".
            let len = c
                .checked_sub(o + 1)
                .filter(|len| *len < SCRATCH)
                .ok_or_else(|| format!("IPv6 address too long \"{}\"", escape(name)))?;
            let port = lastc.filter(|l| *l > c).map(|l| &name[l + 1..]);
            (&name[o + 1..o + 1 + len], port, true)
        }
        _ => {
            return Err(format!(
                "IPv6 with mismatched brackets \"{}\"",
                escape(name)
            ));
        }
    };
    let literal = match v6 {
        false => addr.parse::<Ipv4Addr>().ok().map(IpAddr::V4),
        true => addr.parse::<Ipv6Addr>().ok().map(IpAddr::V6),
    };
    let mut sa = match literal {
        Some(ip) => SocketAddr::new(ip, 0),
        // pvxs takes the first IPv4 answer, else an IPv6 one only while its
        // `temp` is AF_UNSPEC — which the form above already made it not, so
        // a v6-only name becomes `0.0.0.0`. Not copied: the v6 answer is used.
        None => {
            let found = get_addr_info(addr)?;
            found
                .iter()
                .find(|a| a.is_ipv4())
                .or_else(|| found.first())
                .copied()
                .ok_or_else(|| {
                    format!("Not a valid host name or IP address \"{}\"", escape(name))
                })?
        }
    };
    let port = match port {
        // `setPort(unsigned short)` keeps the low 16 bits.
        Some(p) => crate::pvdata::convert::parse_to_u64(p).map_err(|e| e.to_string())? as u16,
        None => defport,
    };
    sa.set_port(port);
    Ok(sa)
}

/// `evutil_getaddrinfo(name, nullptr, nullptr, &info)`.
#[cfg(unix)]
fn get_addr_info(name: &str) -> Result<Vec<SocketAddr>, String> {
    // A C string ends at its first NUL.
    let node = name.split('\0').next().unwrap_or_default();
    let node = std::ffi::CString::new(node).expect("no interior NUL");
    let mut result: *mut libc::addrinfo = std::ptr::null_mut();
    // SAFETY: `node` is NUL-terminated and `result` outlives the call; null
    // service and hints are allowed.
    let err = unsafe {
        libc::getaddrinfo(
            node.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            &mut result,
        )
    };
    if err != 0 {
        return Err(resolve_error(name, &gai_strerror(err)));
    }
    let mut out = Vec::new();
    let mut ai = result;
    while !ai.is_null() {
        // SAFETY: `ai` walks the list `getaddrinfo` returned, freed below.
        let entry = unsafe { &*ai };
        if !entry.ai_addr.is_null() {
            match entry.ai_family {
                libc::AF_INET => {
                    // SAFETY: an `AF_INET` entry's `ai_addr` is a `sockaddr_in`.
                    let sin = unsafe { &*(entry.ai_addr as *const libc::sockaddr_in) };
                    let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                    out.push(SocketAddr::new(ip.into(), 0));
                }
                libc::AF_INET6 => {
                    // SAFETY: an `AF_INET6` entry's `ai_addr` is a `sockaddr_in6`.
                    let sin6 = unsafe { &*(entry.ai_addr as *const libc::sockaddr_in6) };
                    out.push(SocketAddr::V6(std::net::SocketAddrV6::new(
                        Ipv6Addr::from(sin6.sin6_addr.s6_addr),
                        0,
                        sin6.sin6_flowinfo,
                        sin6.sin6_scope_id,
                    )));
                }
                _ => {}
            }
        }
        ai = entry.ai_next;
    }
    // SAFETY: `result` came from a successful `getaddrinfo` and is freed once.
    unsafe { libc::freeaddrinfo(result) };
    Ok(out)
}

/// std's lookup is `getaddrinfo`, and its error is the WinSock code.
#[cfg(windows)]
fn get_addr_info(name: &str) -> Result<Vec<SocketAddr>, String> {
    use std::net::ToSocketAddrs;
    (name, 0)
        .to_socket_addrs()
        .map(Iterator::collect)
        .map_err(|e| match e.raw_os_error() {
            Some(code) => resolve_error(name, &gai_strerror(code)),
            None => resolve_error(name, &e.to_string()),
        })
}

fn resolve_error(name: &str, text: &str) -> String {
    format!("Error resolving \"{}\" : {text}", escape(name))
}

/// libevent `evutil_gai_strerror` (`evutil.c:1872-1910`) over the codes this
/// target names, the native text for the rest as its default arm does.
#[cfg(unix)]
fn gai_strerror(err: libc::c_int) -> String {
    let table: &[(libc::c_int, &str)] = &[
        (libc::EAI_AGAIN, "temporary failure in name resolution"),
        (libc::EAI_BADFLAGS, "invalid value for ai_flags"),
        (libc::EAI_FAIL, "non-recoverable failure in name resolution"),
        (libc::EAI_FAMILY, "ai_family not supported"),
        (libc::EAI_MEMORY, "memory allocation failure"),
        #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
        (libc::EAI_NODATA, "no address associated with nodename"),
        (
            libc::EAI_NONAME,
            "nodename nor servname provided, or not known",
        ),
        (libc::EAI_SERVICE, "servname not supported for ai_socktype"),
        (libc::EAI_SOCKTYPE, "ai_socktype not supported"),
        (libc::EAI_SYSTEM, "system error"),
    ];
    if let Some((_, text)) = table.iter().find(|(code, _)| *code == err) {
        return (*text).to_string();
    }
    // SAFETY: `gai_strerror` returns a static NUL-terminated string.
    unsafe { std::ffi::CStr::from_ptr(libc::gai_strerror(err)) }
        .to_string_lossy()
        .into_owned()
}

/// The same table under the WinSock codes `EAI_*` alias; libevent's default
/// arm is `gai_strerrorA`, whose text is the system message std renders.
#[cfg(windows)]
fn gai_strerror(err: i32) -> String {
    use windows_sys::Win32::Networking::WinSock as ws;
    let text = match err {
        ws::WSATRY_AGAIN => "temporary failure in name resolution",
        ws::WSAEINVAL => "invalid value for ai_flags",
        ws::WSANO_RECOVERY => "non-recoverable failure in name resolution",
        ws::WSAEAFNOSUPPORT => "ai_family not supported",
        ws::WSA_NOT_ENOUGH_MEMORY => "memory allocation failure",
        ws::WSAHOST_NOT_FOUND => "nodename nor servname provided, or not known",
        ws::WSATYPE_NOT_FOUND => "servname not supported for ai_socktype",
        ws::WSAESOCKTNOSUPPORT => "ai_socktype not supported",
        _ => return std::io::Error::from_raw_os_error(err).to_string(),
    };
    text.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_forms_take_their_port_or_the_default() {
        for (name, want) in [
            ("10.1.2.3", "10.1.2.3:5076"),
            ("10.1.2.3:7", "10.1.2.3:7"),
            ("::1", "[::1]:5076"),
            ("[::1]", "[::1]:5076"),
            ("[::1]:7", "[::1]:7"),
            // parseTo<uint64_t> is base 0, and setPort keeps 16 bits.
            ("10.1.2.3:0x10", "10.1.2.3:16"),
            ("10.1.2.3:70000", "10.1.2.3:4464"),
        ] {
            assert_eq!(set_address(name, 5076), Ok(want.parse().unwrap()), "{name}");
        }
    }

    #[test]
    fn malformed_forms_carry_pvxs_text() {
        for (name, what) in [
            ("[::1", "IPv6 with mismatched brackets \"[::1\""),
            ("::1]", "IPv6 with mismatched brackets \"::1]\""),
            ("]::1[", "IPv6 address too long \"]::1[\""),
            ("10.1.2.3:", "Invalid input : \"\""),
            ("10.1.2.3:7x", "Extraneous characters after integer: \"7x\""),
        ] {
            assert_eq!(set_address(name, 5076), Err(what.to_string()), "{name}");
        }
        let long = format!("{}:1", "a".repeat(SCRATCH));
        assert_eq!(
            set_address(&long, 0),
            Err(format!("IPv4 address too long \"{long}\""))
        );
    }

    /// `.invalid` never resolves (RFC 6761); the text is libevent's, not the
    /// C library's.
    #[cfg(unix)]
    #[test]
    fn an_unresolvable_host_is_error_resolving() {
        let err = set_address("nosuchhost.invalid:5", 0).unwrap_err();
        let texts = [
            "nodename nor servname provided, or not known",
            "temporary failure in name resolution",
            "no address associated with nodename",
        ];
        assert!(
            texts
                .iter()
                .any(|t| err == format!("Error resolving \"nosuchhost.invalid\" : {t}")),
            "{err}"
        );
    }

    #[test]
    fn localhost_resolves_v4_first() {
        let sa = set_address("localhost:9", 0).unwrap();
        assert_eq!(sa.port(), 9);
        assert!(sa.ip().is_loopback(), "{sa}");
    }
}
