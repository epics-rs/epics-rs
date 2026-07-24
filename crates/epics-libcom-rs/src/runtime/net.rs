use std::net::SocketAddr;

use super::env_table::{
    EPICS_CA_MCAST_TTL, EPICS_CA_REPEATER_PORT, EPICS_CA_SERVER_PORT, EPICS_CAS_BEACON_PORT,
    EPICS_CAS_SERVER_PORT,
};

/// C `CA_SERVER_PORT` (`caProto.h`) — const-derived from the generated
/// `ENV_PARAM` table, so it cannot drift from `configure/CONFIG_ENV`. A default
/// that stopped being a valid port would fail const-evaluation.
pub const CA_SERVER_PORT: u16 = EPICS_CA_SERVER_PORT.default_port();
/// C `CA_REPEATER_PORT` — see [`CA_SERVER_PORT`].
pub const CA_REPEATER_PORT: u16 = EPICS_CA_REPEATER_PORT.default_port();
// PVA port constants (originally from pva/protocol.rs, now in epics-pva-rs).
// NOT const-derived: PVA has no `ENV_PARAM` in EPICS Base — pvxs owns its own
// config vocabulary, and `EPICS_PVA_SERVER_PORT` is not in `env_param_list[]`.
pub const PVA_SERVER_PORT: u16 = 5075;
pub const PVA_BROADCAST_PORT: u16 = 5076;

/// Client-side CA server-port reader — where to **send** SEARCH.
/// Honours `EPICS_CA_SERVER_PORT` only; the server-only
/// `EPICS_CAS_SERVER_PORT` does **not** affect client behaviour.
///
/// C parity: matches the client path's `envGetInetPortConfigParam
/// (&EPICS_CA_SERVER_PORT, CA_SERVER_PORT)` — the server-specific
/// variable is intentionally invisible to clients so a process that
/// hosts both a client and a server (e.g. a gateway) does not get
/// its client routing redirected by a server-side override.
pub fn ca_server_port() -> u16 {
    EPICS_CA_SERVER_PORT.inet_port(CA_SERVER_PORT)
}

/// Server-side CA bind-port reader — where the server **binds** the
/// UDP discovery socket and the TCP listener (same value for both,
/// matching `caservertask.c:491-499` —
/// `ca_udp_port = ca_server_port`).
///
/// Precedence: `EPICS_CAS_SERVER_PORT` > `EPICS_CA_SERVER_PORT` >
/// [`CA_SERVER_PORT`] (5064). Mirrors C
/// `caservertask.c::ca_initialize` exactly — the CAS-specific
/// variable lets a gateway-style process override the *server-side*
/// port without disturbing its client routing.
///
/// For the multi-IOC "shared UDP search port, distinct TCP ports"
/// deployment, callers should use `epics_base_rs::server::ioc_app::
/// IocApplication::tcp_port` (or the equivalent `CaServerBuilder::tcp_port`,
/// in `epics-ca-rs`) to override the
/// TCP port *only*. The env-var path keeps strict C parity so a
/// startup script that sets `EPICS_CAS_SERVER_PORT=6064` lands UDP
/// and TCP on the same port, as it would under a C IOC.
///
/// The selection between the two variables is a *presence* test, as in
/// C: whichever parameter is configured is then resolved against the
/// **compiled** [`CA_SERVER_PORT`] default, so a rejected
/// `EPICS_CAS_SERVER_PORT` lands on 5064 — it does not fall through to
/// `EPICS_CA_SERVER_PORT`.
pub fn cas_server_port() -> u16 {
    if EPICS_CAS_SERVER_PORT.get().is_some() {
        EPICS_CAS_SERVER_PORT.inet_port(CA_SERVER_PORT)
    } else {
        EPICS_CA_SERVER_PORT.inet_port(CA_SERVER_PORT)
    }
}

/// Returns the CA repeater port, allowing override via `EPICS_CA_REPEATER_PORT`.
pub fn ca_repeater_port() -> u16 {
    EPICS_CA_REPEATER_PORT.inet_port(CA_REPEATER_PORT)
}

/// Server-side beacon port — C `caservertask.c:501-508`.
///
/// Presence-gated exactly like [`cas_server_port`]: `EPICS_CAS_BEACON_PORT`
/// when it is configured, else `EPICS_CA_REPEATER_PORT`, each resolved
/// against the compiled [`CA_REPEATER_PORT`] default.
pub fn cas_beacon_port() -> u16 {
    if EPICS_CAS_BEACON_PORT.get().is_some() {
        EPICS_CAS_BEACON_PORT.inet_port(CA_REPEATER_PORT)
    } else {
        EPICS_CA_REPEATER_PORT.inet_port(CA_REPEATER_PORT)
    }
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
    // C `udpiiu.cpp:198-200` / `caservertask.c:324-326`:
    //     long val;
    //     if (envGetLongConfigParam(&EPICS_CA_MCAST_TTL, &val)) val = 1;
    // — the `1` being a hand-copy of the table default, which here comes from
    // the table.
    let ttl = EPICS_CA_MCAST_TTL.long_or_default();
    u32::try_from(ttl)
        .ok()
        .filter(|v| (1..=255).contains(v))
        .unwrap_or(1)
}

// There is deliberately NO `pva_server_port()` / `pva_broadcast_port()` here.
//
// A PVA port is pvxs's to define, and pvxs's rules are not C's: `PickOne` reads
// `EPICS_PVAS_SERVER_PORT` before `EPICS_PVA_SERVER_PORT` (`config.cpp:402-408`)
// and `EPICS_PVA_SERVER_PORT=0` is a legitimate ephemeral-bind request
// (`config.cpp:624-632`), where `envGetInetPortConfigParam` — the function every
// helper in THIS module implements — rejects anything at or below 5000 and
// prints two CA diagnostics saying so. Helpers that looked like the CA ones and
// silently applied the CA rule to a PVA variable are a trap for the next caller;
// `epics_pva_rs::config::env` is the owner, and the only owner, of these two.

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

    /// The CAS/CA selection is a *presence* test against the compiled
    /// default (`caservertask.c:491-508`): a configured-but-rejected
    /// `EPICS_CAS_SERVER_PORT` lands on 5064, it does NOT fall through to
    /// `EPICS_CA_SERVER_PORT`. Same rule for the beacon port.
    #[test]
    #[serial(epics_env)]
    fn test_cas_port_selection_is_presence_gated() {
        unsafe {
            std::env::set_var("EPICS_CA_SERVER_PORT", "6064");
            std::env::set_var("EPICS_CAS_SERVER_PORT", "3000");
        }
        assert_eq!(
            cas_server_port(),
            CA_SERVER_PORT,
            "rejected EPICS_CAS_SERVER_PORT falls back to the compiled default, not to EPICS_CA_SERVER_PORT"
        );
        // Empty is "not configured" — the generic variable is resolved.
        unsafe { std::env::set_var("EPICS_CAS_SERVER_PORT", "") };
        assert_eq!(cas_server_port(), 6064);

        unsafe {
            std::env::set_var("EPICS_CA_REPEATER_PORT", "6065");
            std::env::set_var("EPICS_CAS_BEACON_PORT", "70000");
        }
        assert_eq!(
            cas_beacon_port(),
            CA_REPEATER_PORT,
            "rejected EPICS_CAS_BEACON_PORT falls back to the compiled default"
        );
        unsafe { std::env::set_var("EPICS_CAS_BEACON_PORT", "6165") };
        assert_eq!(cas_beacon_port(), 6165);
        unsafe { std::env::remove_var("EPICS_CAS_BEACON_PORT") };
        assert_eq!(cas_beacon_port(), 6065);

        unsafe {
            std::env::remove_var("EPICS_CA_SERVER_PORT");
            std::env::remove_var("EPICS_CAS_SERVER_PORT");
            std::env::remove_var("EPICS_CA_REPEATER_PORT");
        }
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
        // C parity regression (caservertask.c:491-499):
        //   ca_server_port = EPICS_CAS_SERVER_PORT (if set) else
        //                    EPICS_CA_SERVER_PORT else default 5064
        //   ca_udp_port = ca_server_port    <-- UDP follows the same value
        //
        // Earlier this fn was named cas_server_port() but documented as
        // a "TCP-only" override with UDP staying at 5064 — that
        // diverged from C and broke startup scripts that set
        // EPICS_CAS_SERVER_PORT expecting both ports to follow.
        unsafe {
            std::env::set_var("EPICS_CA_SERVER_PORT", "5064");
            std::env::set_var("EPICS_CAS_SERVER_PORT", "9064");
        }
        // C parity: client semantic only honours EPICS_CA_SERVER_PORT
        // (`envGetInetPortConfigParam(&EPICS_CA_SERVER_PORT, ...)`).
        assert_eq!(
            ca_server_port(),
            5064,
            "client semantic ignores EPICS_CAS_SERVER_PORT"
        );
        // C parity: server-side bind port honours EPICS_CAS_SERVER_PORT
        // first (caservertask.c:491). Both UDP and TCP follow this
        // value unless the caller splits via `.tcp_port(...)`.
        assert_eq!(
            cas_server_port(),
            9064,
            "server-side bind picks up EPICS_CAS_SERVER_PORT"
        );
        unsafe {
            std::env::remove_var("EPICS_CAS_SERVER_PORT");
            std::env::remove_var("EPICS_CA_SERVER_PORT");
        }
    }

    #[test]
    #[serial(epics_env)]
    fn test_ca_server_port_rejects_privileged_port() {
        // H2 C-parity: a port <= IPPORT_USERRESERVED (5000) is rejected
        // by `envGetInetPortConfigParam` and falls back to the
        // compiled default — `EPICS_CA_SERVER_PORT=80` must NOT be
        // honoured.
        unsafe { std::env::set_var("EPICS_CA_SERVER_PORT", "80") };
        assert_eq!(
            ca_server_port(),
            CA_SERVER_PORT,
            "privileged port 80 must fall back to the default 5064"
        );
        unsafe { std::env::set_var("EPICS_CA_SERVER_PORT", "5000") };
        assert_eq!(
            ca_server_port(),
            CA_SERVER_PORT,
            "port 5000 (== IPPORT_USERRESERVED) must fall back to default"
        );
        unsafe { std::env::set_var("EPICS_CA_SERVER_PORT", "0") };
        assert_eq!(ca_server_port(), CA_SERVER_PORT, "port 0 must fall back");
        unsafe { std::env::remove_var("EPICS_CA_SERVER_PORT") };
    }

    #[test]
    #[serial(epics_env)]
    fn test_ca_server_port_accepts_valid_high_port() {
        // 5001 is the first acceptable port (> IPPORT_USERRESERVED).
        unsafe { std::env::set_var("EPICS_CA_SERVER_PORT", "5001") };
        assert_eq!(ca_server_port(), 5001);
        unsafe { std::env::set_var("EPICS_CA_SERVER_PORT", "6064") };
        assert_eq!(ca_server_port(), 6064);
        unsafe { std::env::remove_var("EPICS_CA_SERVER_PORT") };
    }

    #[test]
    #[serial(epics_env)]
    fn test_ca_repeater_port_rejects_out_of_range() {
        // H2 applies to every port reader, not just CA server.
        unsafe { std::env::set_var("EPICS_CA_REPEATER_PORT", "443") };
        assert_eq!(ca_repeater_port(), CA_REPEATER_PORT);
        unsafe { std::env::remove_var("EPICS_CA_REPEATER_PORT") };
    }

    #[test]
    #[serial(epics_env)]
    fn test_port_reader_lenient_parse() {
        // H3 C-parity: `sscanf("%ld")` tolerates leading whitespace and
        // a trailing garbage suffix that `u16::parse` would reject.
        unsafe { std::env::set_var("EPICS_CA_SERVER_PORT", " 6064") };
        assert_eq!(ca_server_port(), 6064);
        unsafe { std::env::set_var("EPICS_CA_SERVER_PORT", "6064xyz") };
        assert_eq!(ca_server_port(), 6064);
        unsafe { std::env::remove_var("EPICS_CA_SERVER_PORT") };
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
