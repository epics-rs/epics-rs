//! pvxs `IfaceMap` (`evhelper.cpp:818-905`): the one table an endpoint's
//! `@iface` resolves through, for the SockEndpoint ctor's normalisation, every
//! group join and every multicast send.
//!
//! pvxs never refuses an interface: a name it does not know is warned about
//! once, at parse, and then joins and sends as the wildcard (`INADDR_ANY`,
//! index 0). So nothing here returns an error.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// One up interface, as pvxs `IfaceMap::Iface` holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Iface {
    pub name: String,
    /// 0 where the OS reports none.
    pub index: u32,
    /// In [`pvxs_v4_order`], pvxs's `addrs` order, so the first is
    /// `address_of`'s.
    pub v4: Vec<Ipv4Addr>,
    /// The v4 broadcast addresses; pvxs's `byAddr` names the interface by
    /// these too (`evhelper.cpp:823-829`).
    pub bcast: Vec<Ipv4Addr>,
    /// The lowest v6 address, `address_of`'s answer on a v6-only interface.
    pub v6: Option<Ipv6Addr>,
}

/// The key pvxs's `std::map<SockAddr>` orders v4 addresses by: `s_addr`
/// compared as a native integer (`evutil_sockaddr_cmp`), so on a
/// little-endian host the last octet ranks first.
pub(crate) fn pvxs_v4_order(ip: &Ipv4Addr) -> u32 {
    u32::from_ne_bytes(ip.octets())
}

/// A snapshot of this host's up interfaces.
#[derive(Debug, Clone, Default)]
pub struct IfaceMap {
    /// In name order, pvxs `all_external`'s.
    ifaces: Vec<Iface>,
}

impl IfaceMap {
    /// This host's interfaces now. A failed enumeration is an empty map, so
    /// every interface reads as unknown and resolves to the wildcard.
    pub fn current() -> Self {
        match enumerate() {
            Ok(ifaces) => Self::from_ifaces(ifaces),
            Err(e) => {
                tracing::debug!(error = %e, "interface enumeration failed");
                Self::default()
            }
        }
    }

    /// [`Self::current`] when some endpoint of `eps` names an interface,
    /// else the empty map, which none of them consults.
    pub fn for_endpoints<'a>(mut eps: impl Iterator<Item = &'a super::Endpoint>) -> Self {
        if eps.any(|ep| ep.iface.is_some()) {
            Self::current()
        } else {
            Self::default()
        }
    }

    pub fn from_ifaces(mut ifaces: Vec<Iface>) -> Self {
        ifaces.sort_by(|a, b| a.name.cmp(&b.name));
        for i in &mut ifaces {
            i.v4.sort_by_key(pvxs_v4_order);
        }
        Self { ifaces }
    }

    /// A map of v4 interfaces `n0`, `n1`, … holding `(address, index)`.
    #[cfg(test)]
    pub(crate) fn of_v4(nics: &[(Ipv4Addr, u32)]) -> Self {
        Self::from_ifaces(
            nics.iter()
                .enumerate()
                .map(|(n, &(ip, index))| Iface {
                    name: format!("n{n}"),
                    index,
                    v4: vec![ip],
                    bcast: Vec::new(),
                    v6: None,
                })
                .collect(),
        )
    }

    fn by_name(&self, name: &str) -> Option<&Iface> {
        self.ifaces.iter().find(|i| i.name == name)
    }

    /// `name_of(SockAddr)`: the interface owning `addr` as an address or a
    /// broadcast.
    pub fn name_of_v4(&self, addr: Ipv4Addr) -> Option<&str> {
        self.ifaces
            .iter()
            .filter(|i| i.v4.contains(&addr) || i.bcast.contains(&addr))
            .min_by_key(|i| i.index)
            .map(|i| i.name.as_str())
    }

    /// `address_of(name)`: the lowest v4 address, else the lowest v6.
    pub fn address_of(&self, name: &str) -> Option<IpAddr> {
        let i = self.by_name(name)?;
        i.v4.first().map(|&v4| v4.into()).or(i.v6.map(IpAddr::V6))
    }

    /// What pvxs puts in `IP_MULTICAST_IF` and `imr_interface` for `name`:
    /// its v4 address, `INADDR_ANY` when it is unknown or v6-only
    /// (`config.cpp:101-106`, `evhelper.cpp:584-592`).
    pub fn address_of_v4(&self, name: &str) -> Ipv4Addr {
        match self.address_of(name) {
            Some(IpAddr::V4(v4)) => v4,
            _ => Ipv4Addr::UNSPECIFIED,
        }
    }

    /// `index_of(name)`: 0 when there is none.
    pub fn index_of(&self, name: &str) -> u32 {
        self.by_name(name).map_or(0, |i| i.index)
    }

    /// `all_external()` resolved for a join: each interface's v4 join
    /// address and index.
    pub fn external(&self) -> Vec<(Ipv4Addr, u32)> {
        self.ifaces
            .iter()
            .map(|i| (self.address_of_v4(&i.name), i.index))
            .collect()
    }

    /// The `SockEndpoint` ctor's `@iface` (`config.cpp:64-85`): a v4
    /// multicast endpoint's IP literal becomes the name of the interface
    /// owning it, or none; a name nothing knows is kept and warned about.
    pub fn normalize(&self, addr: SocketAddr, iface: Option<String>) -> Option<String> {
        let mut iface = iface?;
        if let (true, Ok(lit)) = (
            addr.is_ipv4() && addr.ip().is_multicast(),
            iface.parse::<Ipv4Addr>(),
        ) {
            iface = self.name_of_v4(lit)?.to_string();
        }
        if self.by_name(&iface).is_none() {
            crate::pvxs_log::log(
                crate::pvxs_log::CONFIG,
                crate::pvxs_log::Level::Warn,
                format_args!("Invalid interface address or name: \"{iface}\""),
            );
        }
        Some(iface)
    }
}

#[cfg(not(epics_embedded_target))]
fn enumerate() -> std::io::Result<Vec<Iface>> {
    let mut out: Vec<Iface> = epics_base_rs::net::all_external()?
        .into_iter()
        .map(|e| Iface {
            name: e.name,
            index: e.index,
            v4: Vec::new(),
            bcast: Vec::new(),
            v6: e.v6,
        })
        .collect();
    add_v4(&mut out)?;
    Ok(out)
}

/// No v6 and no index enumeration on the embedded targets.
#[cfg(epics_embedded_target)]
fn enumerate() -> std::io::Result<Vec<Iface>> {
    let mut out = Vec::new();
    add_v4(&mut out)?;
    Ok(out)
}

fn add_v4(out: &mut Vec<Iface>) -> std::io::Result<()> {
    for v4 in epics_base_rs::net::iface_v4::enumerate()? {
        let pos = match out.iter().position(|i| i.name == v4.name) {
            Some(pos) => pos,
            None => {
                out.push(Iface {
                    name: v4.name.clone(),
                    index: 0,
                    v4: Vec::new(),
                    bcast: Vec::new(),
                    v6: None,
                });
                out.len() - 1
            }
        };
        let i = &mut out[pos];
        i.v4.push(v4.ip);
        if let Some(b) = v4.dest.filter(|_| v4.is_broadcast()) {
            i.bcast.push(b);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map() -> IfaceMap {
        IfaceMap::from_ifaces(vec![
            Iface {
                name: "d5".into(),
                index: 5,
                v4: vec![
                    Ipv4Addr::new(10, 10, 0, 9),
                    Ipv4Addr::new(10, 10, 1, 1),
                    Ipv4Addr::new(10, 10, 0, 1),
                ],
                bcast: vec![Ipv4Addr::new(10, 10, 0, 255)],
                v6: None,
            },
            Iface {
                name: "v6only".into(),
                index: 7,
                v4: Vec::new(),
                bcast: Vec::new(),
                v6: Some("fd00::7".parse().unwrap()),
            },
        ])
    }

    /// One case per `address_of` boundary: v4 present (lowest wins),
    /// v6-only, unknown.
    #[test]
    fn address_of_takes_the_lowest_v4_else_v6() {
        let m = map();
        // 10.10.1.1 is below 10.10.0.9 by native `s_addr` on little-endian.
        let lowest = Ipv4Addr::new(10, 10, 0, 1);
        assert_eq!(m.address_of("d5"), Some(lowest.into()));
        assert_eq!(m.address_of_v4("d5"), lowest);
        assert_eq!(
            m.by_name("d5").unwrap().v4[1],
            if cfg!(target_endian = "little") {
                Ipv4Addr::new(10, 10, 1, 1)
            } else {
                Ipv4Addr::new(10, 10, 0, 9)
            }
        );
        assert_eq!(m.address_of("v6only"), Some("fd00::7".parse().unwrap()));
        assert_eq!(m.address_of_v4("v6only"), Ipv4Addr::UNSPECIFIED);
        assert_eq!(m.address_of("nosuch"), None);
        assert_eq!(m.address_of_v4("nosuch"), Ipv4Addr::UNSPECIFIED);
        assert_eq!((m.index_of("v6only"), m.index_of("nosuch")), (7, 0));
    }

    /// The ctor's literal mapping applies to v4 multicast alone, and an
    /// address or broadcast names its interface; an unknown literal is no
    /// interface; a name is kept whether known or not.
    #[test]
    fn normalize_maps_a_multicast_literal_to_its_name() {
        let m = map();
        let mc: SocketAddr = "239.1.1.1:5076".parse().unwrap();
        let uc: SocketAddr = "10.10.0.1:5076".parse().unwrap();
        let n = |a, s: &str| m.normalize(a, Some(s.to_string()));
        assert_eq!(n(mc, "10.10.0.9").as_deref(), Some("d5"));
        assert_eq!(n(mc, "10.10.0.255").as_deref(), Some("d5"));
        assert_eq!(n(mc, "192.0.2.7"), None);
        assert_eq!(n(uc, "192.0.2.7").as_deref(), Some("192.0.2.7"));
        assert_eq!(n(mc, "nosuch").as_deref(), Some("nosuch"));
        assert_eq!(m.normalize(mc, None), None);
    }

    /// The host table holds loopback with its address and an index.
    #[cfg(target_os = "linux")]
    #[test]
    fn current_holds_loopback() {
        let m = IfaceMap::current();
        assert_eq!(m.name_of_v4(Ipv4Addr::LOCALHOST), Some("lo"));
        assert_eq!(m.address_of_v4("lo"), Ipv4Addr::LOCALHOST);
        assert_ne!(m.index_of("lo"), 0);
    }
}
