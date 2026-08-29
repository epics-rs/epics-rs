//! IPv4/IPv6 ↔ 16-byte PVA address conversion.
//!
//! Source: pvxs `pvaproto.h::to_wire(SocketAddress)`. PVA always carries
//! addresses as 16 bytes; IPv4 is encoded as IPv4-mapped IPv6 (`::ffff:a.b.c.d`).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Pack an `IpAddr` as 16 bytes (IPv4-mapped IPv6 for v4 inputs).
pub fn ip_to_bytes(ip: IpAddr) -> [u8; 16] {
    match ip {
        IpAddr::V4(v4) => {
            let mut out = [0u8; 16];
            out[10] = 0xFF;
            out[11] = 0xFF;
            out[12..16].copy_from_slice(&v4.octets());
            out
        }
        IpAddr::V6(v6) => v6.octets(),
    }
}

/// Decode a 16-byte PVA address.
///
/// - All-zeros → `None` (unspecified address).
/// - IPv4-mapped (`::ffff:a.b.c.d`) → `Some(IpAddr::V4)`.
/// - Anything else → `Some(IpAddr::V6)`.
pub fn ip_from_bytes(addr: &[u8; 16]) -> Option<IpAddr> {
    if addr.iter().all(|&b| b == 0) {
        return None;
    }
    if addr[0..10].iter().all(|&b| b == 0) && addr[10] == 0xFF && addr[11] == 0xFF {
        return Some(IpAddr::V4(Ipv4Addr::new(
            addr[12], addr[13], addr[14], addr[15],
        )));
    }
    Some(IpAddr::V6(Ipv6Addr::from(*addr)))
}

/// decode a 16-byte PVA address treating the all-zero
/// pattern as the unspecified IPv6 address (`::`) rather than as
/// "decode failed". pvxs `evhelper.cpp:911-938` accepts it as
/// `SockAddr::isAny()` and `util.cpp:552-558` classifies IPv6
/// unspecified as wildcard; downstream code substitutes the UDP
/// packet's source address for any wildcard SEARCH_RESPONSE /
/// BEACON (pvxs `src/client.cpp:841-843`, `udp_collector.cpp:471-476`).
/// Pre-fix Rust mapped all-zero to `None` and rejected the frame
/// — so an IPv6-capable peer advertising wildcard via the raw-zero
/// encoding was ignored by the Rust client.
pub fn ip_from_bytes_allow_unspec(addr: &[u8; 16]) -> IpAddr {
    if addr[0..10].iter().all(|&b| b == 0) && addr[10] == 0xFF && addr[11] == 0xFF {
        return IpAddr::V4(Ipv4Addr::new(addr[12], addr[13], addr[14], addr[15]));
    }
    IpAddr::V6(Ipv6Addr::from(*addr))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv4_round_trip() {
        let v4 = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100));
        let bytes = ip_to_bytes(v4);
        assert_eq!(&bytes[10..12], &[0xFF, 0xFF]);
        assert_eq!(&bytes[12..16], &[192, 168, 1, 100]);
        assert_eq!(ip_from_bytes(&bytes), Some(v4));
    }

    #[test]
    fn ipv6_round_trip() {
        let v6 = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1));
        let bytes = ip_to_bytes(v6);
        assert_eq!(ip_from_bytes(&bytes), Some(v6));
    }

    #[test]
    fn all_zeros_is_unspecified() {
        assert_eq!(ip_from_bytes(&[0u8; 16]), None);
    }

    #[test]
    fn matches_reference_wire() {
        // pvAccess wire: ip_to_bytes(192.168.1.1) → [0,0,0,0,0,0,0,0,0,0,0xFF,0xFF,192,168,1,1]
        let bytes = ip_to_bytes(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)));
        assert_eq!(
            bytes,
            [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xFF, 192, 168, 1, 1]
        );
    }
}
