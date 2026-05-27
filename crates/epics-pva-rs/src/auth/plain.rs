//! Plain (non-TLS) authentication: "anonymous" or "ca" with user+host AuthZ.
//!
//! The user/host pair is part of the CONNECTION_VALIDATION reply per pvxs
//! `clientconn.cpp::handle_validation`. We default to `$USER`/`hostname()`
//! but callers can override via [`crate::client_native::PvaClientBuilder`]
//! or the `EPICS_PVA_AUTH_USER` / `EPICS_PVA_AUTH_HOST` environment variables.

/// Resolve the AuthZ user. Honours `EPICS_PVA_AUTH_USER`, then `USER`,
/// then `USERNAME`, then falls back to `"anonymous"`.
pub fn authnz_default_user() -> String {
    std::env::var("EPICS_PVA_AUTH_USER")
        .or_else(|_| std::env::var("USER"))
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "anonymous".to_string())
}

/// Enumerate the current process's POSIX group names. Mirrors pvxs
/// `osdGetRoles()` (osgroups.cpp:54). On non-POSIX targets returns an
/// empty list. The `ca` auth method advertises these as the user's
/// "roles" claim so server-side ACF rules of the form
/// `R member group:engineers` can match against the actual group
/// membership of the requesting user.
///
/// Calls `getgrouplist(3)` directly via `nix` — same data libca
/// gathers on the C side. Result is sorted + deduped for stable
/// matching.
pub fn posix_groups() -> Vec<String> {
    #[cfg(unix)]
    {
        use std::ffi::CStr;
        // Step 1: get this process's effective uid + login name.
        // SAFETY: getuid is always safe; geteuid returns the
        // current effective uid.
        let uid = unsafe { libc::getuid() };
        // Look up the login name AND the passwd primary group
        // (`pw_gid`) via getpwuid. Both fields are read and copied out
        // while the returned pointer is still valid. Falls back to the
        // `$USER` env when the lookup misses (then no `pw_gid` exists).
        let (name, pw_gid) = unsafe {
            let pw = libc::getpwuid(uid);
            if pw.is_null() {
                (None, None)
            } else {
                let cs = CStr::from_ptr((*pw).pw_name);
                (cs.to_str().ok().map(|s| s.to_string()), Some((*pw).pw_gid))
            }
        };
        let user = name
            .or_else(|| std::env::var("USER").ok())
            .unwrap_or_default();
        if user.is_empty() {
            return Vec::new();
        }
        // Step 2: getgrouplist(3) — first call probes the size.
        // SAFETY: We pass cstr-terminated `user` and a writable
        // groups slice big enough for the kernel's reply.
        let user_cstr = match std::ffi::CString::new(user) {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        // Darwin's libc binding uses `*mut c_int` for the groups
        // pointer; Linux uses `*mut gid_t` (u32). Use a raw c_int
        // buffer and cast on the way out so the signature matches
        // both platforms.
        //
        // The basegid is the account's passwd primary group (`pw_gid`),
        // matching pvxs `osdGetRoles` (osgroups.cpp:65,88 seed
        // getgrouplist with `user->pw_gid`). Only when getpwuid missed
        // (the $USER-env fallback, no passwd entry) do we fall back to
        // the process `getgid()`.
        let primary = pw_gid.unwrap_or_else(|| unsafe { libc::getgid() }) as libc::c_int;
        let mut ngroups: libc::c_int = 64;
        let mut groups: Vec<libc::c_int> = vec![0; ngroups as usize];
        let rc = unsafe {
            libc::getgrouplist(
                user_cstr.as_ptr(),
                primary as _,
                groups.as_mut_ptr() as *mut _,
                &mut ngroups,
            )
        };
        if rc < 0 {
            groups.resize(ngroups as usize, 0);
            let rc2 = unsafe {
                libc::getgrouplist(
                    user_cstr.as_ptr(),
                    primary as _,
                    groups.as_mut_ptr() as *mut _,
                    &mut ngroups,
                )
            };
            if rc2 < 0 {
                return Vec::new();
            }
        }
        groups.truncate(ngroups as usize);
        // Step 3: gid → group name via getgrgid.
        let mut names: Vec<String> = Vec::with_capacity(groups.len());
        for gid in groups {
            unsafe {
                let gr = libc::getgrgid(gid as libc::gid_t);
                if gr.is_null() {
                    continue;
                }
                let cs = CStr::from_ptr((*gr).gr_name);
                if let Ok(s) = cs.to_str() {
                    names.push(s.to_string());
                }
            }
        }
        names.sort();
        names.dedup();
        names
    }
    #[cfg(not(unix))]
    {
        Vec::new()
    }
}

/// Re-derive the POSIX group "roles" of a NAMED account, server-side,
/// from the local passwd/group database. This is the exact source pvxs
/// `server::ClientCredentials::roles()` uses (serverconn.cpp:33-37 →
/// `osdGetRoles(account)`, osgroups.cpp:54): the account's passwd primary
/// group (`pw_gid`) plus every supplementary group from `getgrouplist(3)`,
/// each mapped to a name via `getgrgid(3)`. When the account is unknown to
/// the local passwd DB (`getpwnam` miss) the account name itself is the
/// only role — pvxs's "don't know who this is" fallback.
///
/// SECURITY: a PVA client advertises a `groups`/`roles` field in
/// CONNECTION_VALIDATION, but the server MUST NOT trust it — doing so lets
/// a crafted client (`account="nobody", roles=["admin"]`) satisfy any
/// `member group:admin` ACF rule. pvxs never reads wire roles; the server
/// re-derives them here from the account against its OWN group DB.
///
/// Distinct from [`posix_groups`]: that resolves the CURRENT process's
/// groups via `getpwuid(getuid())` (the client advertising its own
/// identity); this resolves an ARBITRARY named account via
/// `getpwnam(account)` and uses the account's `pw_gid` (not the process
/// `getgid()`).
pub fn osd_get_roles(account: &str) -> Vec<String> {
    #[cfg(unix)]
    {
        use std::ffi::{CStr, CString};

        let account_cstr = match CString::new(account) {
            Ok(c) => c,
            // An embedded NUL cannot name a real passwd entry — treat as
            // the getpwnam-miss fallback.
            Err(_) => return vec![account.to_string()],
        };
        // getpwnam(account): the named account's passwd entry. A NULL
        // result means the account is unknown locally — pvxs returns
        // {account} ("don't know who this is").
        // SAFETY: `account_cstr` is a valid NUL-terminated C string. We
        // read the returned passwd's fields only while the pointer is
        // valid (before any further NSS call) and copy them out at once.
        let (pw_name, pw_gid) = unsafe {
            let pw = libc::getpwnam(account_cstr.as_ptr());
            if pw.is_null() {
                return vec![account.to_string()];
            }
            let name = CStr::from_ptr((*pw).pw_name)
                .to_str()
                .ok()
                .map(|s| s.to_string());
            (name, (*pw).pw_gid)
        };
        let pw_name = match pw_name {
            Some(n) => n,
            None => return vec![account.to_string()],
        };
        let name_cstr = match CString::new(pw_name) {
            Ok(c) => c,
            Err(_) => return vec![account.to_string()],
        };

        // gids = primary `pw_gid` + supplementary groups. Darwin binds
        // the groups pointer as `*mut c_int`, Linux as `*mut gid_t`
        // (u32); use a raw `c_int` buffer and cast, matching
        // `posix_groups`. The first call probes the required size.
        let mut gids: Vec<libc::gid_t> = vec![pw_gid];
        let mut ngroups: libc::c_int = 64;
        let mut groups: Vec<libc::c_int> = vec![0; ngroups as usize];
        // SAFETY: `name_cstr` is NUL-terminated; `groups` is a writable
        // buffer of `ngroups` elements; the kernel writes at most that
        // many and updates `ngroups`.
        let rc = unsafe {
            libc::getgrouplist(
                name_cstr.as_ptr(),
                pw_gid as _,
                groups.as_mut_ptr() as *mut _,
                &mut ngroups,
            )
        };
        if rc < 0 {
            // Buffer too small — `ngroups` now holds the real count;
            // grow and retry once.
            groups.resize(ngroups.max(0) as usize, 0);
            // SAFETY: as above, with the now-correctly-sized buffer.
            let rc2 = unsafe {
                libc::getgrouplist(
                    name_cstr.as_ptr(),
                    pw_gid as _,
                    groups.as_mut_ptr() as *mut _,
                    &mut ngroups,
                )
            };
            if rc2 >= 0 {
                groups.truncate(ngroups.max(0) as usize);
                gids.extend(groups.into_iter().map(|g| g as libc::gid_t));
            }
            // rc2 < 0: keep just the primary `pw_gid` (pvxs gives up on
            // the supplementary list but still has the primary group).
        } else {
            groups.truncate(ngroups.max(0) as usize);
            gids.extend(groups.into_iter().map(|g| g as libc::gid_t));
        }

        // gid -> group name via getgrgid.
        let mut names: Vec<String> = Vec::with_capacity(gids.len());
        for gid in gids {
            // SAFETY: getgrgid returns a pointer valid until the next NSS
            // call; the name is copied out immediately.
            unsafe {
                let gr = libc::getgrgid(gid);
                if gr.is_null() {
                    continue;
                }
                if let Ok(s) = CStr::from_ptr((*gr).gr_name).to_str() {
                    names.push(s.to_string());
                }
            }
        }
        names.sort();
        names.dedup();
        names
    }
    #[cfg(not(unix))]
    {
        // No local group DB available (Windows LANMAN handled elsewhere;
        // RTEMS / vxWorks): pvxs reports the account as its only role.
        vec![account.to_string()]
    }
}

/// Resolve the AuthZ host. Honours `EPICS_PVA_AUTH_HOST`, then the OS
/// hostname, falling back to `"localhost"`.
pub fn authnz_default_host() -> String {
    if let Ok(h) = std::env::var("EPICS_PVA_AUTH_HOST") {
        return h;
    }
    hostname::get()
        .ok()
        .and_then(|s| s.into_string().ok())
        .unwrap_or_else(|| "localhost".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_falls_back() {
        // Just check it returns *something*.
        let u = authnz_default_user();
        assert!(!u.is_empty());
    }

    #[test]
    fn host_falls_back() {
        let h = authnz_default_host();
        assert!(!h.is_empty());
    }

    #[test]
    fn osd_get_roles_unknown_account_is_self() {
        // An account absent from the local passwd DB → pvxs "don't know
        // who this is" fallback (osgroups.cpp:57-60): the account name is
        // its only role. Deterministic regardless of host.
        let acct = "pvxs_unknown_account_zzqq";
        assert_eq!(osd_get_roles(acct), vec![acct.to_string()]);
    }

    #[cfg(unix)]
    #[test]
    fn posix_groups_includes_passwd_primary_group() {
        use std::ffi::CStr;
        // Derive the current user's passwd primary-group NAME the way
        // pvxs osdGetRoles seeds getgrouplist — from `pw_gid`, not the
        // process `getgid()`. getgrouplist returns the basegid in its
        // list, so that primary group must appear in posix_groups().
        // If this env has no passwd entry or the gid has no name
        // mapping, there is nothing to assert.
        let primary_name: Option<String> = unsafe {
            let pw = libc::getpwuid(libc::getuid());
            if pw.is_null() {
                None
            } else {
                let gr = libc::getgrgid((*pw).pw_gid);
                if gr.is_null() {
                    None
                } else {
                    Some(CStr::from_ptr((*gr).gr_name).to_string_lossy().into_owned())
                }
            }
        };
        if let Some(name) = primary_name {
            assert!(
                posix_groups().contains(&name),
                "posix_groups() must include the passwd primary group {name:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn osd_get_roles_known_account_is_nonempty() {
        // `root` exists on every Unix host; its primary group maps to a
        // name via getgrgid, so the server-derived role set is non-empty.
        assert!(
            !osd_get_roles("root").is_empty(),
            "root must derive at least one group role from the local DB"
        );
    }
}
