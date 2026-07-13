//! libca `iocinf.cpp` — the helpers every CA address-list environment variable
//! is built with, and the single owner of their diagnostics in the port.
//!
//! EVERY address list in EPICS goes through the same two calls:
//! `addAddrToChannelAccessAddressList` to tokenize, then
//! `removeDuplicateAddresses(…, silent=0)` to dedup and report what it dropped.
//! `EPICS_CA_ADDR_LIST` (`iocinf.cpp:225-227`), `EPICS_CA_NAME_SERVERS`
//! (`cac.cpp:259-260`), `EPICS_CAS_INTF_ADDR_LIST` (`caservertask.c:341-343`),
//! `EPICS_CAS_BEACON_ADDR_LIST` (`caservertask.c:413-438`),
//! `EPICS_CAS_IGNORE_ADDR_LIST` (`caservertask.c:450-451`) and the repeater's
//! merged beacon list (`repeater.cpp:545-550`) are the same code path with a
//! different `envDefs` entry passed in — so neither the dedup nor its warning
//! is a property of any one variable.
//!
//! The port hand-rolled a parser per variable, and every one of them lost the
//! dedup: a duplicated token multiplied that destination's search or beacon
//! traffic for the life of the process, silently. This module is the missing C
//! function, and the only place the port builds one of these lists.

use std::net::SocketAddr;

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
/// (`caservertask.c:450`).
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
