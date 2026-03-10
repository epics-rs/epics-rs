use std::net::SocketAddr;

use super::env;

// Re-export canonical port constants
pub use crate::protocol::{CA_REPEATER_PORT, CA_SERVER_PORT};
pub use crate::pva::protocol::{PVA_BROADCAST_PORT, PVA_SERVER_PORT};

/// Returns the CA server port, allowing override via `EPICS_CA_SERVER_PORT`.
pub fn ca_server_port() -> u16 {
    env::get_u16("EPICS_CA_SERVER_PORT", CA_SERVER_PORT)
}

/// Returns the CA repeater port, allowing override via `EPICS_CA_REPEATER_PORT`.
pub fn ca_repeater_port() -> u16 {
    env::get_u16("EPICS_CA_REPEATER_PORT", CA_REPEATER_PORT)
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

    #[test]
    fn test_default_ca_server_port() {
        // Remove env var to ensure default
        std::env::remove_var("EPICS_CA_SERVER_PORT");
        assert_eq!(ca_server_port(), 5064);
    }

    #[test]
    fn test_default_ca_repeater_port() {
        std::env::remove_var("EPICS_CA_REPEATER_PORT");
        assert_eq!(ca_repeater_port(), 5065);
    }

    #[test]
    fn test_default_pva_broadcast_port() {
        std::env::remove_var("EPICS_PVA_BROADCAST_PORT");
        assert_eq!(pva_broadcast_port(), 5076);
    }

    #[test]
    fn test_default_pva_server_port() {
        std::env::remove_var("EPICS_PVA_SERVER_PORT");
        assert_eq!(pva_server_port(), 5075);
    }

    #[test]
    fn test_ca_server_port_env_override() {
        std::env::set_var("EPICS_CA_SERVER_PORT", "9064");
        assert_eq!(ca_server_port(), 9064);
        std::env::remove_var("EPICS_CA_SERVER_PORT");
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
