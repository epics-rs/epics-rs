//! libca `iocinf.cpp` — the helpers every CA address-list environment variable
//! is built with, and the single owner of their diagnostics in the port.
//!
//! EVERY address list in EPICS goes through the same two calls:
//! `addAddrToChannelAccessAddressList` to tokenize, then
//! `removeDuplicateAddresses(…, silent=0)` to dedup and report what it dropped.
//! `EPICS_CA_ADDR_LIST` (`iocinf.cpp:225-227`), `EPICS_CA_NAME_SERVERS`
//! (`cac.cpp:259-260`), `EPICS_CAS_INTF_ADDR_LIST` (`caservertask.c:342-344`),
//! `EPICS_CAS_BEACON_ADDR_LIST` (`caservertask.c:414-439`),
//! `EPICS_CAS_IGNORE_ADDR_LIST` (`caservertask.c:451-452`) and the repeater's
//! merged beacon list (`repeater.cpp:533-538`) are the same code path with a
//! different `envDefs` entry passed in — so neither the dedup nor its warning
//! is a property of any one variable.
//!
//! The port hand-rolled a parser per variable, and every one of them lost the
//! dedup: a duplicated token multiplied that destination's search or beacon
//! traffic for the life of the process, silently. This module is the missing C
//! function, and the only place the port builds one of these lists.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, ToSocketAddrs};

/// One entry as `addAddrToChannelAccessAddressList` produced it: the resolved
/// address, plus the DNS name it came from when it was not an IP literal.
///
/// C keeps only the resolved `osiSockAddr`; the port keeps the name as well so
/// the search engine can re-resolve an entry whose IOC moved (epics-base#488).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AddrToken {
    pub sock: SocketAddr,
    /// `None` when the token was an IP literal — nothing to re-resolve.
    pub hostname: Option<String>,
    pub port: u16,
}

/// C `addAddrToChannelAccessAddressList` (`iocinf.cpp:45-100`): split the value
/// on whitespace, run each token through `aToIPAddr` with the list's default
/// port, and REPORT every token that does not resolve — then carry on with the
/// rest of the list.
///
/// The port dropped bad tokens silently (a `continue`, or a `tracing::debug!`
/// nobody sees), so a typo in `EPICS_CA_ADDR_LIST` left the client searching a
/// shorter list than the operator wrote, with nothing on the terminal to say
/// so. Every list C builds — client, name servers, server interface, beacon,
/// ignore — is tokenized here, so the diagnostic is not a property of one
/// variable and neither is this function.
pub(crate) fn add_addr_to_channel_access_address_list(
    list: &str,
    env_name: &str,
    default_port: u16,
) -> Vec<AddrToken> {
    let mut out = Vec::new();
    // C tokenizes on `" \t\n\r"` (`epicsStrtok_r`), which is `split_whitespace`
    // minus the Unicode spaces no `envDefs` value carries.
    for token in list.split_whitespace() {
        match a_to_ip_addr(token, default_port) {
            Some(entry) => out.push(entry),
            None => bad_address(env_name, token),
        }
    }
    out
}

/// `iocinf.cpp:71-74`, byte for byte — `__FILE__` is the C source path as the
/// build compiled it, `../iocinf.cpp`, and the second line is TAB-indented.
/// Captured from the compiled `caget`:
///
/// ```text
/// ../iocinf.cpp: Parsing 'EPICS_CA_ADDR_LIST'
/// <TAB>Bad internet address or host name: 'no.such.host.invalid'
/// ```
///
/// (`<TAB>` is a literal U+0009 in C's format string and in the output; it is
/// spelled out here only because a tab in a doc comment is a clippy error.)
fn bad_address(env_name: &str, token: &str) {
    eprintln!("../iocinf.cpp: Parsing '{env_name}'");
    eprintln!("\tBad internet address or host name: '{token}'");
}

/// libcom `aToIPAddr` (`aToIPAddr.c:75-194`): `<host>` or `<host>:<port>`, where
/// `<host>` is a dotted IPv4 literal or a name the resolver knows. Anything else
/// — an unresolvable name, a non-numeric or out-of-range port, an empty host —
/// is a failure, and C's caller reports it.
///
/// CA is IPv4-only (`sin_family = AF_INET`), so an IPv6-only name is a failure
/// here as it is in C.
fn a_to_ip_addr(token: &str, default_port: u16) -> Option<AddrToken> {
    let (host, port) = match token.rsplit_once(':') {
        Some((host, port)) => (host, port.parse::<u16>().ok()?),
        None => (token, default_port),
    };
    if host.is_empty() {
        return None;
    }
    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        return Some(AddrToken {
            sock: SocketAddr::V4(SocketAddrV4::new(ip, port)),
            hostname: None,
            port,
        });
    }
    let sock = format!("{host}:{port}")
        .to_socket_addrs()
        .ok()?
        .find(SocketAddr::is_ipv4)?;
    Some(AddrToken {
        sock,
        hostname: Some(host.to_string()),
        port,
    })
}

/// C `removeDuplicateAddresses(pDestList, pSrcList, silent=0)`
/// (`iocinf.cpp:104-140`): keep the first entry for each `(ip, port)`, drop
/// every later repeat, and print one warning line per dropped entry.
///
/// The `silent=1` pass libca runs over the auto-discovered broadcast addresses
/// alone (`iocinf.cpp:206`) is this same dedup with the warning suppressed; in
/// the port that pass is the `!addrs.iter().any(…)` guard at each append site,
/// which never had a warning to suppress.
///
/// The key is `(ip, port)` — C compares `sin_addr.s_addr` and `sin_port` — and
/// the warning names the *dotted address*, not the token: C dedups a list of
/// already-resolved `osiSockAddr`s and prints it through `ipAddrToDottedIP`, so
/// an entry written as a host name is reported by the IP it resolved to, and an
/// entry given without a port is reported with the port its list defaults to.
///
/// Verified head-to-head against the compiled tools:
/// `caget` with `EPICS_CA_ADDR_LIST="127.0.0.1:15099 127.0.0.1:15099"` warns
/// once, three copies warn twice, and `"127.0.0.1:15099 127.0.0.1:15098"` warns
/// not at all; `softIoc` with `EPICS_CAS_INTF_ADDR_LIST="127.0.0.1 127.0.0.1"`
/// warns `"127.0.0.1:5064"`, and with `EPICS_CAS_IGNORE_ADDR_LIST="10.1.2.3
/// 10.1.2.3"` warns `"10.1.2.3:0"` — the ignore list is built with `port = 0`
/// (`caservertask.c:451`).
pub(crate) fn remove_duplicate_addresses<T>(
    entries: Vec<T>,
    addr_of: impl Fn(&T) -> SocketAddr,
) -> Vec<T> {
    let mut kept: Vec<T> = Vec::with_capacity(entries.len());
    for entry in entries {
        let addr = addr_of(&entry);
        if kept.iter().any(|k| addr_of(k) == addr) {
            // `iocinf.cpp:123-126`, byte for byte.
            eprintln!("Warning: Duplicate EPICS CA Address list entry \"{addr}\" discarded");
            continue;
        }
        kept.push(entry);
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addrs(list: &[&str]) -> Vec<SocketAddr> {
        list.iter().map(|s| s.parse().unwrap()).collect()
    }

    /// The dedup key is `(ip, port)`, and the FIRST occurrence survives: C adds
    /// a node to the destination list only when it matched nothing already
    /// there, and frees it otherwise.
    #[test]
    fn duplicates_are_by_ip_and_port_and_the_first_one_wins() {
        let got = remove_duplicate_addresses(
            addrs(&["127.0.0.1:15099", "127.0.0.1:15099", "127.0.0.1:15098"]),
            |a| *a,
        );
        assert_eq!(got, addrs(&["127.0.0.1:15099", "127.0.0.1:15098"]));
    }

    /// Three copies of one address discard two — compiled `caget` prints the
    /// warning twice for that list.
    #[test]
    fn every_repeat_after_the_first_is_discarded() {
        let got = remove_duplicate_addresses(
            addrs(&["10.0.0.1:5064", "10.0.0.1:5064", "10.0.0.1:5064"]),
            |a| *a,
        );
        assert_eq!(got, addrs(&["10.0.0.1:5064"]));
    }

    /// A different port on the same host is a different entry.
    #[test]
    fn a_different_port_is_not_a_duplicate() {
        let got = remove_duplicate_addresses(addrs(&["127.0.0.1:5064", "127.0.0.1:5065"]), |a| *a);
        assert_eq!(got.len(), 2);
    }
}
