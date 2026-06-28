//! Control/log endpoint specs — a port of C procServ's `acceptFactory`
//! spec parsing (`acceptFactory.cc:104-150,229-325`).
//!
//! One spec string (`-P <spec>` / `-l <spec>`) parses into one
//! [`Endpoint`]: a bare TCP port, an `A.B.C.D:port` interface bind, or a
//! `unix:` socket — filesystem, `user:grp:perm:`-access-controlled, or
//! an abstract `@name`. C collects control specs into a `ctlSpecs`
//! vector built from repeatable `-P` (`procServ.cc:386`), so several
//! endpoints can be requested at once.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;

/// One listening endpoint, fully resolved. The localhost-vs-any-interface
/// decision is already baked into the [`SocketAddr`], exactly as C bakes
/// its `local` flag into the `sockaddr` inside `acceptFactory`
/// (`acceptFactory.cc:116,129`).
#[derive(Debug, Clone)]
pub enum Endpoint {
    /// TCP listener at a resolved address.
    Tcp(SocketAddr),
    /// UNIX-domain listener.
    Unix(UnixEndpoint),
}

/// A UNIX-domain control/log socket (`acceptItemUNIX`,
/// `acceptFactory.cc:229-325`).
#[derive(Debug, Clone)]
pub struct UnixEndpoint {
    /// Socket name: a filesystem path, or — when [`Self::abstract_socket`]
    /// is set — the abstract name with C's leading `@` already stripped.
    pub name: PathBuf,
    /// Linux abstract socket (the spec's leading `@`). No filesystem
    /// presence and no permission check (`acceptFactory.cc:286-293`).
    pub abstract_socket: bool,
    /// `chown` target uid (the `user:` prefix resolved via `getpwnam`,
    /// `acceptFactory.cc:255-263`). `None` ⇒ leave the socket's owner as
    /// created — C's default of chowning to `getuid()` is a no-op.
    pub uid: Option<u32>,
    /// `chown` target gid (the `:grp:` prefix via `getgrnam`, or a named
    /// user's primary group, `acceptFactory.cc:261,265-272`). `None` ⇒
    /// leave as created.
    pub gid: Option<u32>,
    /// Filesystem mode applied via `chmod` after bind. C default `0o666`
    /// ("equivalent to tcp bind to localhost", `acceptFactory.cc:233`).
    pub perms: u32,
}

impl UnixEndpoint {
    /// A plain filesystem socket at `path` with C's default access
    /// (owner unchanged, mode `0o666`). The `--unixpath` convenience flag
    /// and the `unix:/path` spec form both land here.
    pub fn filesystem(path: PathBuf) -> Self {
        Self {
            name: path,
            abstract_socket: false,
            uid: None,
            gid: None,
            perms: 0o666,
        }
    }
}

/// Parse one endpoint spec with C `acceptFactory` semantics
/// (`acceptFactory.cc:104-150`).
///
/// `local` is C's `ctlPortLocal` / `logPortLocal`: when `true` a TCP
/// endpoint binds loopback regardless of any interface named in the spec
/// (the localhost-by-default security posture — `--allow` clears it for
/// the control port, `--restrict` sets it for the log port); when `false`
/// it binds the requested interface, or `0.0.0.0` for a bare port. The
/// flag is irrelevant for UNIX sockets.
pub fn parse_endpoint(spec: &str, local: bool) -> Result<Endpoint, String> {
    let spec = spec.trim();

    // 1. A bare unsigned integer is a TCP port (C `sscanf("%u %c")==1`,
    //    `acceptFactory.cc:113-124`).
    if let Ok(port) = spec.parse::<u32>() {
        return Ok(Endpoint::Tcp(tcp_addr(
            local_ip(local, Ipv4Addr::UNSPECIFIED),
            port,
        )?));
    }

    // 2. A `unix:` prefix is a UNIX-domain socket (`acceptFactory.cc:138`).
    if let Some(rest) = spec.strip_prefix("unix:") {
        return Ok(Endpoint::Unix(parse_unix(rest)?));
    }

    // 3. `A.B.C.D:port` binds a specific interface (C
    //    `sscanf("%u.%u.%u.%u:%u")==5`, `acceptFactory.cc:125-137`). When
    //    `local`, C ignores the interface and still binds loopback
    //    (`local ? INADDR_LOOPBACK : A.B.C.D`); reproduced faithfully.
    if let Some((host, port_s)) = spec.rsplit_once(':')
        && let (Ok(v4), Ok(port)) = (host.parse::<Ipv4Addr>(), port_s.parse::<u32>())
    {
        return Ok(Endpoint::Tcp(tcp_addr(local_ip(local, v4), port)?));
    }

    Err(format!("Invalid socket spec '{spec}'"))
}

/// Loopback when `local`, otherwise the requested interface — C's
/// `local ? INADDR_LOOPBACK : addr`.
fn local_ip(local: bool, addr: Ipv4Addr) -> Ipv4Addr {
    if local { Ipv4Addr::LOCALHOST } else { addr }
}

/// Build a TCP [`SocketAddr`], enforcing C's `port > 0xffff` rejection
/// (`acceptFactory.cc:118-122,131-135`).
fn tcp_addr(ip: Ipv4Addr, port: u32) -> Result<SocketAddr, String> {
    if port > 0xffff {
        return Err(format!("invalid control port {port} (must be <65536)"));
    }
    Ok(SocketAddr::new(IpAddr::V4(ip), port as u16))
}

/// Parse the part after `unix:` into a [`UnixEndpoint`], porting
/// `acceptItemUNIX`'s constructor (`acceptFactory.cc:229-325`). Accepts
/// `/path`, `user:grp:perm:/path`, and `@abstract`.
fn parse_unix(rest: &str) -> Result<UnixEndpoint, String> {
    let mut uid = None;
    let mut gid = None;
    let mut perms = 0o666u32;

    // A `:` triggers the access-control prefix, which REQUIRES exactly
    // three colons before the path (C `acceptFactory.cc:242-279`).
    let path_str = if let Some(sep) = rest.find(':') {
        let after = &rest[sep + 1..];
        let unparsable = || format!("Unix path+permissions spec unparsable '{rest}'");
        let sep2 = after.find(':').ok_or_else(unparsable)?;
        let after2 = &after[sep2 + 1..];
        let sep3 = after2.find(':').ok_or_else(unparsable)?;

        let user = &rest[..sep];
        let grp = &after[..sep2];
        let perm = &after2[..sep3];
        let path = &after2[sep3 + 1..];

        if !user.is_empty() {
            // C sets BOTH uid and the (default) gid from the user's passwd
            // entry; a later `:grp:` overrides the gid (`acceptFactory.cc:255-263`).
            let (u, g) = resolve_user(user)?;
            uid = Some(u);
            gid = Some(g);
        }
        if !grp.is_empty() {
            gid = Some(resolve_group(grp)?);
        }
        if !perm.is_empty() {
            // C `strtoul(perm, NULL, 8)`. Strict octal here (C's lenient
            // parse stopping at the first non-octal digit is declined).
            perms = u32::from_str_radix(perm, 8)
                .map_err(|_| format!("invalid octal unix socket perms '{perm}'"))?;
        }
        path.to_string()
    } else {
        rest.to_string()
    };

    if path_str.is_empty() {
        return Err("expected unix socket path spec.".to_string());
    }

    let abstract_socket = path_str.starts_with('@');

    // `sun_path` is 108 bytes including the trailing NUL; C rejects a spec
    // whose length is `>= sizeof(sun_path)` (`acceptFactory.cc:303-306`).
    // The check is on the full spec, before the leading `@` is stripped.
    if path_str.len() >= 108 {
        return Err("Unix path is too long (must be <108)".to_string());
    }

    // Abstract: drop the leading `@`; C replaces it with a NUL and binds
    // the remaining name (`acceptFactory.cc:316-325`).
    let name = if abstract_socket {
        PathBuf::from(&path_str[1..])
    } else {
        PathBuf::from(&path_str)
    };

    Ok(UnixEndpoint {
        name,
        abstract_socket,
        uid,
        gid,
        perms,
    })
}

/// Resolve a user name to `(uid, primary gid)` via `getpwnam`
/// (`acceptFactory.cc:256-262`). Unknown user → error, mirroring C's
/// `exit(1)`.
fn resolve_user(name: &str) -> Result<(u32, u32), String> {
    match nix::unistd::User::from_name(name) {
        Ok(Some(u)) => Ok((u.uid.as_raw(), u.gid.as_raw())),
        _ => Err(format!("Unknown user '{name}'")),
    }
}

/// Resolve a group name to a gid via `getgrnam` (`acceptFactory.cc:266-271`).
/// Unknown group → error.
fn resolve_group(name: &str) -> Result<u32, String> {
    match nix::unistd::Group::from_name(name) {
        Ok(Some(g)) => Ok(g.gid.as_raw()),
        _ => Err(format!("Unknown group '{name}'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tcp(ep: &Endpoint) -> SocketAddr {
        match ep {
            Endpoint::Tcp(a) => *a,
            _ => panic!("expected TCP endpoint, got {ep:?}"),
        }
    }

    fn unix(ep: &Endpoint) -> &UnixEndpoint {
        match ep {
            Endpoint::Unix(u) => u,
            _ => panic!("expected UNIX endpoint, got {ep:?}"),
        }
    }

    #[test]
    fn bare_port_binds_loopback_when_local_and_any_when_not() {
        // C: a bare integer → TCP; `local ? INADDR_LOOPBACK : INADDR_ANY`.
        let local = tcp(&parse_endpoint("4051", true).unwrap());
        assert_eq!(local, "127.0.0.1:4051".parse().unwrap());
        let any = tcp(&parse_endpoint("4051", false).unwrap());
        assert_eq!(any, "0.0.0.0:4051".parse().unwrap());
    }

    #[test]
    fn port_above_65535_is_rejected() {
        // C `port > 0xffff` → error + exit (acceptFactory.cc:118-122).
        let err = parse_endpoint("70000", false).unwrap_err();
        assert!(err.contains("must be <65536"), "{err}");
    }

    #[test]
    fn interface_bind_honored_only_when_not_local() {
        // `A.B.C.D:port`: bound interface honored with --allow (local=false),
        // forced to loopback without it (local=true), exactly as C.
        let honored = tcp(&parse_endpoint("192.168.1.5:4051", false).unwrap());
        assert_eq!(honored, "192.168.1.5:4051".parse().unwrap());
        let forced = tcp(&parse_endpoint("192.168.1.5:4051", true).unwrap());
        assert_eq!(forced, "127.0.0.1:4051".parse().unwrap());
    }

    #[test]
    fn unix_plain_path_defaults() {
        let u = unix(&parse_endpoint("unix:/run/ioc.sock", true).unwrap()).clone();
        assert_eq!(u.name, PathBuf::from("/run/ioc.sock"));
        assert!(!u.abstract_socket);
        assert_eq!(u.uid, None);
        assert_eq!(u.gid, None);
        assert_eq!(u.perms, 0o666);
    }

    #[test]
    fn unix_permission_prefix_sets_perms_without_owner_change() {
        // Empty user + empty group + perms only: owner stays as-created,
        // mode is the parsed octal.
        let u = unix(&parse_endpoint("unix:::0660:/run/ioc.sock", false).unwrap()).clone();
        assert_eq!(u.name, PathBuf::from("/run/ioc.sock"));
        assert_eq!(u.uid, None);
        assert_eq!(u.gid, None);
        assert_eq!(u.perms, 0o660);
    }

    #[test]
    fn unix_user_prefix_resolves_to_uid_gid() {
        // Use the running user so the lookup is deterministic in CI.
        let me = nix::unistd::User::from_uid(nix::unistd::getuid())
            .unwrap()
            .expect("current user has a passwd entry");
        let spec = format!("unix:{}:::/run/ioc.sock", me.name);
        let u = unix(&parse_endpoint(&spec, false).unwrap()).clone();
        assert_eq!(u.uid, Some(me.uid.as_raw()));
        // Naming a user defaults the gid to that user's primary group.
        assert_eq!(u.gid, Some(me.gid.as_raw()));
        assert_eq!(u.perms, 0o666);
    }

    #[test]
    fn unix_unknown_user_errors() {
        let err = parse_endpoint("unix:nosuchuser_zzqq:::/run/x.sock", false).unwrap_err();
        assert!(err.contains("Unknown user"), "{err}");
    }

    #[test]
    fn unix_prefix_requires_three_colons() {
        // One colon, no path-completing pair → C "unparsable" (exit 1).
        let err = parse_endpoint("unix:foo:bar", false).unwrap_err();
        assert!(err.contains("unparsable"), "{err}");
    }

    #[test]
    fn unix_empty_path_errors() {
        let err = parse_endpoint("unix:", false).unwrap_err();
        assert!(err.contains("expected unix socket path"), "{err}");
    }

    #[test]
    fn unix_abstract_strips_the_at_sign() {
        let u = unix(&parse_endpoint("unix:@ioc-console", true).unwrap()).clone();
        assert!(u.abstract_socket);
        assert_eq!(u.name, PathBuf::from("ioc-console"));
    }

    #[test]
    fn bare_word_is_an_invalid_spec() {
        let err = parse_endpoint("softIoc", false).unwrap_err();
        assert!(err.contains("Invalid socket spec"), "{err}");
    }
}
