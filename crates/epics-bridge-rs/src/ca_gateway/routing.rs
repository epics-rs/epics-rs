//! Split downstream/upstream network routing for the CA gateway.
//!
//! A CA gateway sits between two CA broadcast domains: it *listens*
//! downstream (the server side) and *searches* upstream (the client
//! side). C ca-gateway exposes this split through five command-line
//! options whose only effect is to `epicsEnvSet` the matching EPICS
//! environment variables at startup, before the CA client and server
//! are created (`gateway.cc:359-402`, `startEverything`):
//!
//! | C option   | EPICS env var                            | side       |
//! |------------|------------------------------------------|------------|
//! | `-sip`     | `EPICS_CAS_INTF_ADDR_LIST`               | downstream |
//! | `-signore` | `EPICS_CAS_IGNORE_ADDR_LIST`             | downstream |
//! | `-cip`     | `EPICS_CA_ADDR_LIST`                     | upstream   |
//! | `-cip`     | `EPICS_CA_AUTO_ADDR_LIST=NO`             | upstream   |
//! | `-cip`     | `EPICS_CAS_AUTO_BEACON_ADDR_LIST=YES`†   | downstream |
//! | `-cport`   | `EPICS_CA_SERVER_PORT`                   | upstream   |
//!
//! † conditional: C's `-cip` branch rewrites `EPICS_CAS_AUTO_BEACON_ADDR_LIST`
//! to `YES` only when the variable is already *present* and not
//! case-insensitive `NO` (`gateway.cc:369-372`,
//! `if(strcasecmp(tempBuff,"NO")) setEnv(...,"YES")`). An unset variable
//! and an explicit `NO` are left untouched.
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
//!
//! `-cip` further normalizes the *server-beacon* auto-addressing knob.
//! Because that rewrite depends on the current value of
//! `EPICS_CAS_AUTO_BEACON_ADDR_LIST`, the caller reads it (mirroring C's
//! `getenv`) and passes it in as `beacon_auto`, keeping the mapping pure.

/// Map the gateway's split-routing knobs to the EPICS environment
/// variables C's `startEverything` sets.
///
/// Each knob is applied only when present, so unset knobs leave the
/// ambient environment untouched (matching C, which calls `epicsEnvSet`
/// only for options that were actually passed). `cip` expands to the
/// upstream address list, the paired auto-address suppression, and —
/// conditionally — a server-beacon auto-addressing rewrite.
///
/// `beacon_auto` is the current value of `EPICS_CAS_AUTO_BEACON_ADDR_LIST`
/// (the caller reads it, mirroring C's `getenv`). When `-cip` is given and
/// that value is present and not case-insensitive `NO`, C promotes it to
/// `YES` so RSRV emits auto server beacons (`gateway.cc:369-372`); an unset
/// variable or an explicit `NO` is left untouched. The downstream CA server
/// then treats only `YES` as true (`envGetBoolConfigParam`, `envSubr.c`),
/// so the rewrite is what makes `EPICS_CAS_AUTO_BEACON_ADDR_LIST=0
/// ca-gateway -cip ...` actually beacon under C.
///
/// The values are returned as `(name, value)` pairs rather than being
/// set here so the mapping stays pure and unit-testable; the caller sets
/// them before spawning runtime threads.
pub fn routing_env_pairs(
    sip: Option<&str>,
    signore: Option<&str>,
    cip: Option<&str>,
    cport: Option<u16>,
    beacon_auto: Option<&str>,
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
        // Server-beacon auto-addressing normalization: C's `-cip` branch
        // reads EPICS_CAS_AUTO_BEACON_ADDR_LIST and, if it is *present* and
        // not case-insensitive "NO", rewrites it to "YES"
        // (gateway.cc:369-372, `if(strcasecmp(tempBuff,"NO")) setEnv(...)`).
        // This promotes any present non-NO value ("0", "garbage", "YES")
        // to "YES"; an unset variable and an explicit "NO" stay as-is.
        if let Some(beacon) = beacon_auto
            && !beacon.eq_ignore_ascii_case("NO")
        {
            pairs.push(("EPICS_CAS_AUTO_BEACON_ADDR_LIST", "YES".to_string()));
        }
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
        assert!(routing_env_pairs(None, None, None, None, None).is_empty());
    }

    #[test]
    fn cip_sets_addr_list_and_disables_auto_search() {
        // No ambient beacon var → no beacon rewrite (matches C: absent
        // EPICS_CAS_AUTO_BEACON_ADDR_LIST leaves the var untouched).
        let pairs = routing_env_pairs(None, None, Some("10.0.0.1 10.0.0.2"), None, None);
        assert_eq!(
            pairs,
            vec![
                ("EPICS_CA_ADDR_LIST", "10.0.0.1 10.0.0.2".to_string()),
                ("EPICS_CA_AUTO_ADDR_LIST", "NO".to_string()),
            ],
            "-cip must set the upstream list AND suppress auto-address search"
        );
    }

    /// C's `-cip` beacon-auto rewrite: a *present* `EPICS_CAS_AUTO_BEACON_ADDR_LIST`
    /// that is not case-insensitive `NO` is promoted to `YES`
    /// (gateway.cc:369-372). `0` and `garbage` are present-non-NO, so both
    /// become `YES`; an already-`YES` value stays `YES`; an explicit `NO`
    /// (any case) and an absent variable are left untouched.
    /// Regression R0604-BRCAGW-ROUTING-CIP-AUTOBEACON-1.
    #[test]
    fn cip_rewrites_present_non_no_beacon_auto_to_yes() {
        let beacon_pair = |beacon: Option<&str>| {
            routing_env_pairs(None, None, Some("10.0.0.1"), None, beacon)
                .into_iter()
                .find(|(k, _)| *k == "EPICS_CAS_AUTO_BEACON_ADDR_LIST")
                .map(|(_, v)| v)
        };

        // "0" → "YES" (the regression: pre-fix the gateway left "0" so the
        // CA server read auto_on=false and emitted no beacons).
        assert_eq!(beacon_pair(Some("0")).as_deref(), Some("YES"));
        // "garbage" → "YES" (any present non-NO value).
        assert_eq!(beacon_pair(Some("garbage")).as_deref(), Some("YES"));
        // "YES" → "YES" (idempotent).
        assert_eq!(beacon_pair(Some("YES")).as_deref(), Some("YES"));

        // Explicit "NO" (any case) is left untouched → no pair emitted.
        assert_eq!(beacon_pair(Some("NO")), None);
        assert_eq!(beacon_pair(Some("no")), None);
        assert_eq!(beacon_pair(Some("No")), None);
        // Absent variable → no rewrite.
        assert_eq!(beacon_pair(None), None);
    }

    /// The beacon-auto rewrite is gated on `-cip`: without `-cip` a present
    /// `EPICS_CAS_AUTO_BEACON_ADDR_LIST` is never touched (C only runs the
    /// rewrite inside the `client_ip_addr` branch).
    /// Regression R0604-BRCAGW-ROUTING-CIP-AUTOBEACON-1.
    #[test]
    fn beacon_auto_rewrite_requires_cip() {
        let pairs = routing_env_pairs(None, None, None, None, Some("0"));
        assert!(
            !pairs
                .iter()
                .any(|(k, _)| *k == "EPICS_CAS_AUTO_BEACON_ADDR_LIST"),
            "no -cip => no beacon-auto rewrite, got {pairs:?}"
        );
    }

    #[test]
    fn downstream_and_upstream_use_distinct_namespaces() {
        let pairs = routing_env_pairs(
            Some("192.168.1.10"),
            Some("192.168.9.0"),
            Some("10.0.0.1"),
            Some(5066),
            None,
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
