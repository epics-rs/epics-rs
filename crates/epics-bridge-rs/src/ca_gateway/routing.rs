//! Split downstream/upstream network routing for the CA gateway.
//!
//! A CA gateway sits between two CA broadcast domains: it *listens*
//! downstream (the server side) and *searches* upstream (the client
//! side). C ca-gateway exposes this split through five command-line
//! options whose only effect is to `epicsEnvSet` the matching EPICS
//! environment variables at startup, before the CA client and server
//! are created (`gateway.cc:359-402`, `startEverything`):
//!
//! | C option   | EPICS env var                | side       |
//! |------------|------------------------------|------------|
//! | `-sip`     | `EPICS_CAS_INTF_ADDR_LIST`   | downstream |
//! | `-signore` | `EPICS_CAS_IGNORE_ADDR_LIST` | downstream |
//! | `-cip`     | `EPICS_CA_ADDR_LIST`         | upstream   |
//! | `-cip`     | `EPICS_CA_AUTO_ADDR_LIST=NO` | upstream   |
//! | `-cport`   | `EPICS_CA_SERVER_PORT`       | upstream   |
//!
//! The downstream server reads the `EPICS_CAS_*` namespace and the
//! upstream client reads the `EPICS_CA_*` namespace, so a single gateway
//! process drives both sides from one env block without the two
//! colliding — that namespace split is exactly why C's mechanism works.
//!
//! `epics-ca-rs` already reads every one of these variables (the
//! `CaServer` honours `EPICS_CAS_INTF_ADDR_LIST` / `EPICS_CAS_IGNORE_ADDR_LIST`
//! when binding its listeners; `CaClient::new` reads `EPICS_CA_ADDR_LIST`,
//! `EPICS_CA_AUTO_ADDR_LIST`, and `EPICS_CA_SERVER_PORT` at construction).
//! Neither `CaServer::from_parts` nor `CaClientConfig` accepts these as
//! explicit parameters, so mapping the gateway's own CLI/TOML knobs onto
//! the env block at startup — exactly as C does — is the faithful port
//! and needs no `epics-ca-rs` API change.
//!
//! [`routing_env_pairs`] is the pure mapping; the binary applies the
//! returned pairs with `std::env::set_var` *before* it builds the tokio
//! runtime, so the env is in place (and no other thread exists yet) by
//! the time the CA server/client read it.
//!
//! `-cip` deliberately also forces `EPICS_CA_AUTO_ADDR_LIST=NO`: an
//! explicit upstream address list without auto-address suppression would
//! let the gateway broadcast searches back onto its own downstream
//! segment, creating self-search loops (`gateway.cc:374-377`).

/// Map the gateway's split-routing knobs to the EPICS environment
/// variables C's `startEverything` sets.
///
/// Each knob is applied only when present, so unset knobs leave the
/// ambient environment untouched (matching C, which calls `epicsEnvSet`
/// only for options that were actually passed). `cip` expands to two
/// pairs: the address list and the paired auto-address suppression.
///
/// The values are returned as `(name, value)` pairs rather than being
/// set here so the mapping stays pure and unit-testable; the caller sets
/// them before spawning runtime threads.
pub fn routing_env_pairs(
    sip: Option<&str>,
    signore: Option<&str>,
    cip: Option<&str>,
    cport: Option<u16>,
) -> Vec<(&'static str, String)> {
    let mut pairs = Vec::new();
    if let Some(sip) = sip {
        pairs.push(("EPICS_CAS_INTF_ADDR_LIST", sip.to_string()));
    }
    if let Some(signore) = signore {
        pairs.push(("EPICS_CAS_IGNORE_ADDR_LIST", signore.to_string()));
    }
    if let Some(cip) = cip {
        pairs.push(("EPICS_CA_ADDR_LIST", cip.to_string()));
        // Paired auto-address suppression: an explicit upstream list must
        // disable auto search so the gateway does not broadcast back onto
        // its own downstream segment (gateway.cc:374-377).
        pairs.push(("EPICS_CA_AUTO_ADDR_LIST", "NO".to_string()));
    }
    if let Some(cport) = cport {
        pairs.push(("EPICS_CA_SERVER_PORT", cport.to_string()));
    }
    pairs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_knobs_set_nothing() {
        assert!(routing_env_pairs(None, None, None, None).is_empty());
    }

    #[test]
    fn cip_sets_addr_list_and_disables_auto_search() {
        let pairs = routing_env_pairs(None, None, Some("10.0.0.1 10.0.0.2"), None);
        assert_eq!(
            pairs,
            vec![
                ("EPICS_CA_ADDR_LIST", "10.0.0.1 10.0.0.2".to_string()),
                ("EPICS_CA_AUTO_ADDR_LIST", "NO".to_string()),
            ],
            "-cip must set the upstream list AND suppress auto-address search"
        );
    }

    #[test]
    fn downstream_and_upstream_use_distinct_namespaces() {
        let pairs = routing_env_pairs(
            Some("192.168.1.10"),
            Some("192.168.9.0"),
            Some("10.0.0.1"),
            Some(5066),
        );
        // Every downstream var is EPICS_CAS_*, every upstream var is
        // EPICS_CA_* — the split that lets one process drive both sides.
        let names: Vec<&str> = pairs.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            names,
            vec![
                "EPICS_CAS_INTF_ADDR_LIST",
                "EPICS_CAS_IGNORE_ADDR_LIST",
                "EPICS_CA_ADDR_LIST",
                "EPICS_CA_AUTO_ADDR_LIST",
                "EPICS_CA_SERVER_PORT",
            ]
        );
        assert_eq!(
            pairs
                .iter()
                .find(|(k, _)| *k == "EPICS_CA_SERVER_PORT")
                .unwrap()
                .1,
            "5066"
        );
    }
}
