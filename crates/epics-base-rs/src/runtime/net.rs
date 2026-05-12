use std::net::SocketAddr;

use super::env;

// CA port constants (originally from protocol.rs, now in epics-ca-rs)
pub const CA_SERVER_PORT: u16 = 5064;
pub const CA_REPEATER_PORT: u16 = 5065;
// PVA port constants (originally from pva/protocol.rs, now in epics-pva-rs)
pub const PVA_SERVER_PORT: u16 = 5075;
pub const PVA_BROADCAST_PORT: u16 = 5076;

/// Returns the CA server port, allowing override via `EPICS_CA_SERVER_PORT`.
///
/// This is the *UDP discovery port* — clients send SEARCH packets here.
/// On a multi-IOC host every IOC must agree on the same UDP port so that
/// one search reaches them all; the TCP port can vary per IOC and is
/// controlled by [`cas_server_port`].
pub fn ca_server_port() -> u16 {
    env::get_u16("EPICS_CA_SERVER_PORT", CA_SERVER_PORT)
}

/// Returns the CA server *TCP* port, allowing override via
/// `EPICS_CAS_SERVER_PORT`. Falls back to [`ca_server_port`] when unset.
///
/// Mirrors epics-base PR #69: lets multiple IOCs on one host bind unique
/// TCP ports while all sharing the canonical UDP search port (5064).
/// The UDP responder advertises this TCP port back in SEARCH_REPLY so
/// clients connect to the right listener.
pub fn cas_server_port() -> u16 {
    env::get_u16("EPICS_CAS_SERVER_PORT", ca_server_port())
}

/// Returns the CA repeater port, allowing override via `EPICS_CA_REPEATER_PORT`.
pub fn ca_repeater_port() -> u16 {
    env::get_u16("EPICS_CA_REPEATER_PORT", CA_REPEATER_PORT)
}

/// IP TTL applied to CA multicast traffic.
///
/// Reads `EPICS_CA_MCAST_TTL` (epics-base 3.16, commit f2a1834d) and
/// returns the value clamped to `1..=255` — the protocol field is one
/// byte and 0 would silently drop every packet at the source NIC.
/// Default 1 matches both the upstream default and the link-local
/// scope assumption EPICS clients rely on; raise it only when a
/// site uses multicast for beacons/search across routed segments.
///
/// Applied on a UDP socket via `socket.set_multicast_ttl_v4(value)`.
/// Has no effect on unicast or limited-broadcast destinations — the
/// OS only consults this field when the destination is in the
/// 224.0.0.0/4 range.
pub fn ca_mcast_ttl() -> u32 {
    const DEFAULT: u32 = 1;
    match env::get("EPICS_CA_MCAST_TTL") {
        Some(s) => s
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|v| *v >= 1 && *v <= 255)
            .unwrap_or(DEFAULT),
        None => DEFAULT,
    }
}

/// Returns the PVA broadcast port, allowing override via `EPICS_PVA_BROADCAST_PORT`.
pub fn pva_broadcast_port() -> u16 {
    env::get_u16("EPICS_PVA_BROADCAST_PORT", PVA_BROADCAST_PORT)
}

/// Returns the PVA server port, allowing override via `EPICS_PVA_SERVER_PORT`.
pub fn pva_server_port() -> u16 {
    env::get_u16("EPICS_PVA_SERVER_PORT", PVA_SERVER_PORT)
}

/// Parse a `"host:port"` string into a `SocketAddr`.
pub fn parse_socket_addr(s: &str) -> Result<SocketAddr, std::net::AddrParseError> {
    s.parse()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    #[serial(epics_env)]
    fn test_default_ca_server_port() {
        // Remove env var to ensure default
        unsafe { std::env::remove_var("EPICS_CA_SERVER_PORT") };
        assert_eq!(ca_server_port(), 5064);
    }

    #[test]
    #[serial(epics_env)]
    fn test_default_ca_repeater_port() {
        unsafe { std::env::remove_var("EPICS_CA_REPEATER_PORT") };
        assert_eq!(ca_repeater_port(), 5065);
    }

    #[test]
    #[serial(epics_env)]
    fn test_default_pva_broadcast_port() {
        unsafe { std::env::remove_var("EPICS_PVA_BROADCAST_PORT") };
        assert_eq!(pva_broadcast_port(), 5076);
    }

    #[test]
    #[serial(epics_env)]
    fn test_default_pva_server_port() {
        unsafe { std::env::remove_var("EPICS_PVA_SERVER_PORT") };
        assert_eq!(pva_server_port(), 5075);
    }

    #[test]
    #[serial(epics_env)]
    fn test_ca_server_port_env_override() {
        unsafe { std::env::set_var("EPICS_CA_SERVER_PORT", "9064") };
        assert_eq!(ca_server_port(), 9064);
        unsafe { std::env::remove_var("EPICS_CA_SERVER_PORT") };
    }

    #[test]
    #[serial(epics_env)]
    fn test_cas_server_port_defaults_to_ca_server_port() {
        unsafe {
            std::env::remove_var("EPICS_CAS_SERVER_PORT");
            std::env::remove_var("EPICS_CA_SERVER_PORT");
        }
        assert_eq!(cas_server_port(), CA_SERVER_PORT);

        unsafe { std::env::set_var("EPICS_CA_SERVER_PORT", "9064") };
        assert_eq!(
            cas_server_port(),
            9064,
            "cas_server_port falls back to EPICS_CA_SERVER_PORT when CAS-specific unset"
        );
        unsafe { std::env::remove_var("EPICS_CA_SERVER_PORT") };
    }

    #[test]
    #[serial(epics_env)]
    fn test_ca_mcast_ttl_default() {
        unsafe { std::env::remove_var("EPICS_CA_MCAST_TTL") };
        assert_eq!(ca_mcast_ttl(), 1);
    }

    #[test]
    #[serial(epics_env)]
    fn test_ca_mcast_ttl_override() {
        unsafe { std::env::set_var("EPICS_CA_MCAST_TTL", "32") };
        assert_eq!(ca_mcast_ttl(), 32);
        unsafe { std::env::remove_var("EPICS_CA_MCAST_TTL") };
    }

    #[test]
    #[serial(epics_env)]
    fn test_ca_mcast_ttl_clamps_invalid_to_default() {
        for bad in ["0", "256", "abc", ""] {
            unsafe { std::env::set_var("EPICS_CA_MCAST_TTL", bad) };
            assert_eq!(
                ca_mcast_ttl(),
                1,
                "invalid EPICS_CA_MCAST_TTL={bad:?} must fall back to default 1"
            );
        }
        unsafe { std::env::remove_var("EPICS_CA_MCAST_TTL") };
    }

    #[test]
    #[serial(epics_env)]
    fn test_cas_server_port_overrides_ca_server_port() {
        unsafe {
            std::env::set_var("EPICS_CA_SERVER_PORT", "5064");
            std::env::set_var("EPICS_CAS_SERVER_PORT", "9064");
        }
        assert_eq!(ca_server_port(), 5064, "UDP discovery port stays at 5064");
        assert_eq!(cas_server_port(), 9064, "TCP port follows CAS-specific var");
        unsafe {
            std::env::remove_var("EPICS_CAS_SERVER_PORT");
            std::env::remove_var("EPICS_CA_SERVER_PORT");
        }
    }

    #[test]
    fn test_parse_socket_addr_valid() {
        let addr = parse_socket_addr("127.0.0.1:5064").unwrap();
        assert_eq!(addr.port(), 5064);
        assert_eq!(addr.ip(), std::net::Ipv4Addr::LOCALHOST);
    }

    #[test]
    fn test_parse_socket_addr_invalid() {
        assert!(parse_socket_addr("not-an-address").is_err());
    }
}
