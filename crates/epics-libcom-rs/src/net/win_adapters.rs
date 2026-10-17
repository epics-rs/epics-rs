//! The Windows interface walk: pvxs `IfaceMap::_refresh`
//! (`os/WIN32/osdSockExt.cpp:211-300`), `GetAdaptersAddresses` with its flags,
//! skipping an adapter whose `OperStatus` is not up as C's
//! `osdNetIntf.c:90` skips one without `IFF_UP`. `if-addrs` walks the same
//! list but reports no status, so it handed out a down adapter's address.
//! An adapter is named by its `AdapterName` (the GUID) as pvxs names it, so
//! an `@iface` matches what it matches there.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use windows_sys::Win32::Foundation::ERROR_BUFFER_OVERFLOW;
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GAA_FLAG_INCLUDE_ALL_INTERFACES, GAA_FLAG_INCLUDE_PREFIX, GAA_FLAG_SKIP_ANYCAST,
    GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST, GetAdaptersAddresses,
    IF_TYPE_SOFTWARE_LOOPBACK, IP_ADAPTER_ADDRESSES_LH,
};
use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows_sys::Win32::Networking::WinSock::{
    AF_INET, AF_INET6, AF_UNSPEC, SOCKADDR_IN, SOCKADDR_IN6,
};

/// One unicast address of an up adapter.
pub(crate) struct AdapterAddr {
    /// `AdapterName`, pvxs's `ifName` (`osdSockExt.cpp:241-243`).
    pub name: String,
    /// `IfIndex` for an IPv4 address, `Ipv6IfIndex` for an IPv6 one.
    pub index: u32,
    pub ip: IpAddr,
    /// `OnLinkPrefixLength`.
    pub prefix_len: u8,
    /// `IfType == IF_TYPE_SOFTWARE_LOOPBACK`.
    pub loopback: bool,
}

impl AdapterAddr {
    /// The v4 netmask of [`Self::prefix_len`].
    pub fn netmask_v4(&self) -> Ipv4Addr {
        Ipv4Addr::from(
            u32::MAX
                .checked_shl(32 - u32::from(self.prefix_len.min(32)))
                .unwrap_or(0),
        )
    }

    /// pvxs's broadcast for a v4 address: `P | ~M` of its prefix, none for
    /// loopback or a prefix outside `1..32`.
    pub fn broadcast_v4(&self) -> Option<Ipv4Addr> {
        let IpAddr::V4(ip) = self.ip else {
            return None;
        };
        if self.loopback || self.prefix_len == 0 || self.prefix_len >= 32 {
            return None;
        }
        let mask = u32::from(self.netmask_v4());
        Some(Ipv4Addr::from((u32::from(ip) & mask) | !mask))
    }
}

/// Every IPv4 and IPv6 unicast address of every up adapter.
pub(crate) fn up_adapter_addrs() -> io::Result<Vec<AdapterAddr>> {
    let flags = GAA_FLAG_SKIP_ANYCAST
        | GAA_FLAG_SKIP_MULTICAST
        | GAA_FLAG_SKIP_DNS_SERVER
        | GAA_FLAG_INCLUDE_PREFIX
        | GAA_FLAG_INCLUDE_ALL_INTERFACES;
    // u64 elements keep the buffer aligned for the structs it holds.
    let mut buf: Vec<u64> = vec![0; 2048];
    let mut err = 0;
    for _ in 0..3 {
        let mut len = (buf.len() * 8) as u32;
        // SAFETY: `buf` is writable for `len` bytes; the call fills it or
        // reports the length it needs.
        err = unsafe {
            GetAdaptersAddresses(
                u32::from(AF_UNSPEC),
                flags,
                std::ptr::null(),
                buf.as_mut_ptr().cast(),
                &mut len,
            )
        };
        if err != ERROR_BUFFER_OVERFLOW {
            break;
        }
        buf.resize((len as usize).div_ceil(8), 0);
    }
    if err != 0 {
        return Err(io::Error::from_raw_os_error(err as i32));
    }
    let mut out = Vec::new();
    let mut adapter = buf.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
    // SAFETY: the list and every pointer in it live in `buf`, which the
    // successful call filled and which outlives the walk.
    unsafe {
        while !adapter.is_null() {
            let a = &*adapter;
            adapter = a.Next;
            if a.OperStatus != IfOperStatusUp {
                continue;
            }
            let name = if a.AdapterName.is_null() {
                "<AdapterName==NULL>".to_string()
            } else {
                // SAFETY: a non-null `AdapterName` is a NUL-terminated ANSI
                // string inside `buf`.
                std::ffi::CStr::from_ptr(a.AdapterName.cast())
                    .to_string_lossy()
                    .into_owned()
            };
            let loopback = a.IfType == IF_TYPE_SOFTWARE_LOOPBACK;
            let mut uni = a.FirstUnicastAddress;
            while !uni.is_null() {
                let u = &*uni;
                uni = u.Next;
                let sa = u.Address.lpSockaddr;
                if sa.is_null() {
                    continue;
                }
                let (ip, index) = match (*sa).sa_family {
                    AF_INET => {
                        let sin = &*sa.cast::<SOCKADDR_IN>();
                        let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.S_un.S_addr));
                        (IpAddr::V4(ip), a.Anonymous1.Anonymous.IfIndex)
                    }
                    AF_INET6 => {
                        let sin6 = &*sa.cast::<SOCKADDR_IN6>();
                        (
                            IpAddr::V6(Ipv6Addr::from(sin6.sin6_addr.u.Byte)),
                            a.Ipv6IfIndex,
                        )
                    }
                    _ => continue,
                };
                out.push(AdapterAddr {
                    name: name.clone(),
                    index,
                    ip,
                    prefix_len: u.OnLinkPrefixLength,
                    loopback,
                });
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(ip: [u8; 4], prefix_len: u8, loopback: bool) -> AdapterAddr {
        AdapterAddr {
            name: String::new(),
            index: 1,
            ip: IpAddr::V4(ip.into()),
            prefix_len,
            loopback,
        }
    }

    #[test]
    fn broadcast_is_the_prefix_broadcast_as_pvxs() {
        assert_eq!(
            v4([10, 9, 0, 1], 24, false).broadcast_v4(),
            Some([10, 9, 0, 255].into())
        );
        assert_eq!(
            v4([10, 9, 0, 1], 24, false).netmask_v4(),
            Ipv4Addr::new(255, 255, 255, 0)
        );
        assert_eq!(v4([10, 9, 0, 1], 32, false).broadcast_v4(), None);
        assert_eq!(v4([10, 9, 0, 1], 0, false).broadcast_v4(), None);
        assert_eq!(v4([127, 0, 0, 1], 8, true).broadcast_v4(), None);
    }
}
