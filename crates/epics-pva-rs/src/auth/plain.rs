//! Plain (non-TLS) authentication: "anonymous" or "ca" with user+host AuthZ.
//!
//! The user/host pair is part of the CONNECTION_VALIDATION reply per pvxs
//! `clientconn.cpp::handle_validation`. The user is `osiGetUserName`'s, as in
//! pvxs `buildCAMethod`; callers can override via
//! `client_native::PvaClientBuilder`, as can the host.

/// pvxs `buildCAMethod` (src/client.cpp:519-532) sends these literals in the
/// `ca` credential when the OS user / hostname lookup fails — `nobody`
/// for an unresolved user, `invalidhost.` for an unresolved host. PVA ACF
/// rules can match on user and host, so the degraded identity must match
/// pvxs exactly rather than diverging to `anonymous` / `localhost`.
use epics_base_rs::types::PvString;

const CA_FALLBACK_USER: &str = "nobody";
const CA_FALLBACK_HOST: &str = "invalidhost.";

/// Fold a resolved user identity to the pvxs CA fallback when absent.
/// Split out so the fallback literal is unit-testable without depending
/// on the host's environment.
fn user_or_ca_fallback(resolved: Option<PvString>) -> PvString {
    resolved.unwrap_or_else(|| CA_FALLBACK_USER.into())
}

/// Fold a resolved host identity to the pvxs CA fallback when absent.
/// Split out because `gethostname()` cannot be forced to fail from a
/// test, so the fallback literal is exercised through this pure helper.
fn host_or_ca_fallback(resolved: Option<PvString>) -> PvString {
    resolved.unwrap_or_else(|| CA_FALLBACK_HOST.into())
}

/// Resolve the AuthZ user: pvxs `buildCAMethod` (`client.cpp:521`) takes
/// `osiGetUserName`'s bytes and `nobody` when it fails; no environment
/// variable is read.
pub fn authnz_default_user() -> PvString {
    user_or_ca_fallback(epics_base_rs::runtime::env::user_name().map(PvString::from_bytes))
}

/// Run `getgrouplist(3)` for `name`/`basegid`, returning `basegid` plus
/// every supplementary group id.
///
/// The required buffer size is not portably knowable up front: glibc
/// updates `*ngroups` to the real count on overflow, but Darwin does NOT
/// (the count stays equal to the buffer size). pvxs `osgroups.cpp:82-118`
/// handles both by looping — double the buffer when the count was not
/// updated (Darwin), resize to the reported count when it was (glibc), and
/// give up (keeping just `basegid`) on an invalid or decreasing count. A
/// single grow-and-retry loses the Darwin case for any account in more
/// groups than the initial guess, silently dropping its extra roles.
#[cfg(local_account_db)]
fn getgrouplist_ids(name: &std::ffi::CStr, basegid: libc::gid_t) -> Vec<libc::gid_t> {
    // Darwin binds the groups pointer as `*mut c_int`, Linux as
    // `*mut gid_t` (u32); use a raw `c_int` buffer and cast on the way
    // out so the signature matches both platforms. The buffer is seeded
    // with the pvxs initial guess of 16.
    let mut gids: Vec<libc::gid_t> = vec![basegid];
    let mut groups: Vec<libc::c_int> = vec![-1; 16];
    loop {
        let mut ngroups: libc::c_int = groups.len() as libc::c_int;
        let len = ngroups;
        // SAFETY: `name` is NUL-terminated; `groups` is a writable buffer of
        // `ngroups` elements; the kernel writes at most that many and (on
        // glibc) updates `ngroups`.
        let rc = unsafe {
            libc::getgrouplist(
                name.as_ptr(),
                basegid as _,
                groups.as_mut_ptr() as *mut _,
                &mut ngroups,
            )
        };
        if rc >= 0 && ngroups >= 0 && ngroups <= len {
            // Success: `ngroups` entries written.
            groups.truncate(ngroups as usize);
            gids.extend(groups.iter().map(|&g| g as libc::gid_t));
            break;
        } else if rc >= 0 {
            // Success but an invalid count — keep just `basegid`.
            break;
        } else if ngroups == len {
            // Too small, count not updated (Darwin): double and retry.
            groups.resize(groups.len().saturating_mul(2), -1);
        } else if ngroups > len {
            // Too small, count holds the real size: resize and retry.
            groups.resize(ngroups as usize, -1);
        } else {
            // Too small, but the count shrank — give up (keep `basegid`).
            break;
        }
    }
    gids
}

/// Enumerate the current process's POSIX group names. Mirrors pvxs
/// `osdGetRoles()` (osgroups.cpp:54). On targets with no local account
/// database returns an empty list. The `ca` auth method advertises these as
/// the user's "roles" claim so server-side ACF rules of the form
/// `R member group:engineers` can match against the actual group
/// membership of the requesting user.
///
/// Calls `getgrouplist(3)` directly via `libc` — same data libca
/// gathers on the C side. Result is sorted + deduped for stable
/// matching.
pub fn posix_groups() -> Vec<PvString> {
    #[cfg(local_account_db)]
    {
        use std::ffi::CStr;
        // Step 1: get this process's effective uid + login name.
        // SAFETY: getuid is always safe; geteuid returns the
        // current effective uid.
        let uid = unsafe { libc::getuid() };
        // Copy the login name and passwd primary group (`pw_gid`) out while
        // the returned pointer is valid; no passwd entry means no groups.
        let (user_cstr, primary) = unsafe {
            let pw = libc::getpwuid(uid);
            if pw.is_null() {
                return Vec::new();
            }
            (CStr::from_ptr((*pw).pw_name).to_owned(), (*pw).pw_gid)
        };
        if user_cstr.is_empty() {
            return Vec::new();
        }
        // Step 2: supplementary groups via the shared getgrouplist(3)
        // doubling loop, seeded with `pw_gid` as pvxs `osdGetRoles`
        // (osgroups.cpp:65,88) does.
        let gids = getgrouplist_ids(&user_cstr, primary);
        // Step 3: gid → group name via getgrgid.
        let mut names: Vec<PvString> = Vec::with_capacity(gids.len());
        for gid in gids {
            unsafe {
                let gr = libc::getgrgid(gid);
                if gr.is_null() {
                    continue;
                }
                // pvxs `osdGetRoles` keeps `gr_name` as the C bytes.
                names.push(PvString::from(CStr::from_ptr((*gr).gr_name).to_bytes()));
            }
        }
        names.sort();
        names.dedup();
        names
    }
    #[cfg(not(local_account_db))]
    {
        // No local account database to enumerate — Windows, and every
        // unix-family target without one (RTEMS, VxWorks). pvxs advertises no
        // roles rather than inventing them.
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
pub fn osd_get_roles(account: &[u8]) -> Vec<PvString> {
    #[cfg(local_account_db)]
    {
        use std::ffi::{CStr, CString};

        // `getpwnam(account.c_str())` (osgroups.cpp:55): the name ends at the
        // first NUL, while the unknown-account fallback keeps all of it.
        let len = account
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(account.len());
        let account_cstr = CString::new(&account[..len]).expect("NUL-free prefix");
        // getpwnam(account): the named account's passwd entry. A NULL
        // result means the account is unknown locally — pvxs returns
        // {account} ("don't know who this is").
        // SAFETY: `account_cstr` is a valid NUL-terminated C string. We
        // read the returned passwd's fields only while the pointer is
        // valid (before any further NSS call) and copy them out at once.
        let (name_cstr, pw_gid) = unsafe {
            let pw = libc::getpwnam(account_cstr.as_ptr());
            if pw.is_null() {
                return vec![PvString::from(account)];
            }
            (CStr::from_ptr((*pw).pw_name).to_owned(), (*pw).pw_gid)
        };

        // gids = primary `pw_gid` + supplementary groups, via the shared
        // getgrouplist(3) doubling loop (osgroups.cpp:82-118).
        let gids = getgrouplist_ids(&name_cstr, pw_gid);

        // gid -> group name via getgrgid, kept as the C bytes.
        let mut names: Vec<PvString> = Vec::with_capacity(gids.len());
        for gid in gids {
            // SAFETY: getgrgid returns a pointer valid until the next NSS
            // call; the name is copied out immediately.
            unsafe {
                let gr = libc::getgrgid(gid);
                if gr.is_null() {
                    continue;
                }
                names.push(PvString::from(CStr::from_ptr((*gr).gr_name).to_bytes()));
            }
        }
        names.sort();
        names.dedup();
        names
    }
    #[cfg(not(local_account_db))]
    {
        // No local group DB available (Windows LANMAN handled elsewhere;
        // RTEMS / vxWorks): pvxs reports the account as its only role.
        //
        // This arm is what RTEMS takes. Under the `cfg(not(unix))` it used to
        // carry it was unreachable there — RTEMS is `target_family = "unix"` —
        // so every accepted PVA connection ran the passwd/group path above and
        // derived ACF roles from whatever a BSP's synthetic passwd entry
        // happened to say. See `build.rs` for why the predicate is a named
        // capability and not `not(target_os = "rtems")`.
        vec![PvString::from(account)]
    }
}

/// Resolve the AuthZ host: pvxs `buildCAMethod` (`client.cpp:528-533`)
/// takes `gethostname`'s bytes from a 256-byte buffer and `invalidhost.`
/// when it fails; no environment variable is read.
pub fn authnz_default_host() -> PvString {
    host_or_ca_fallback(epics_base_rs::runtime::env::hostname().map(|mut h| {
        h.truncate(255);
        PvString::from_bytes(h)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use source_guard::{Comments, production};

    /// **The mechanical defence for this file.** Modelled on
    /// `server_native::search_engine`'s `rtems_selects_entropy_by_target_not_by_family`,
    /// and it fails *today*, on Linux, with no cross toolchain — which is the
    /// point. The acceptance ladder cannot catch this defect at all: a wrong
    /// role set is an authorization decision, not an error.
    ///
    /// `cfg(unix)` was doing two jobs here — "libc is linked" (true on RTEMS)
    /// and "there is a passwd/group database" (false on RTEMS). Every arm that
    /// needs the second now names it. A future merge back to `cfg(unix)`, or a
    /// new arm that reaches for `libc::getpw*` under the family predicate, must
    /// fail here rather than on a board nobody can attach a debugger to.
    #[test]
    fn the_account_database_arms_select_on_the_capability_not_the_family() {
        let prod = production(include_str!("plain.rs"), Comments::Strip);
        assert_eq!(
            prod.matches("#[cfg(unix)]").count(),
            0,
            "a bare cfg(unix) arm captures RTEMS and VxWorks, which have libc \
             but no passwd/group database — that is the defect"
        );
        assert_eq!(
            prod.matches("#[cfg(not(unix))]").count(),
            0,
            "a cfg(not(unix)) arm is unreachable on RTEMS, so an RTEMS-intended \
             fallback placed there never runs"
        );
        // Both halves of the capability must exist: the implementation and the
        // no-database fallback. A count check rather than a mere `contains`, so
        // deleting one arm and leaving the other cannot pass.
        assert_eq!(prod.matches("#[cfg(local_account_db)]").count(), 3);
        assert_eq!(prod.matches("#[cfg(not(local_account_db))]").count(), 2);
    }

    /// The capability is decided in one place, by allowlist. An exclusion list
    /// would fix RTEMS and inherit the same defect on the next unix-family
    /// target without an account database.
    ///
    /// # Scope: the predicate, not the file
    ///
    /// The excluded-target check reads `fn local_account_db`'s body alone.
    /// It used to read the whole (comment-stripped) build script, which made
    /// it assert something it does not mean: that *no rule anywhere* in the
    /// script may name RTEMS. `build.rs` has since acquired a second, unrelated
    /// rule — the `exec_backend` / `tokio_backend` task-backend decision, which
    /// is `epics_embedded_target` (`target_os` in `{"rtems", "vxworks"}`) or
    /// `EPICS_RS_BUILD_EXEC_BACKEND=thread`, and therefore *must* name the
    /// target,
    /// exactly as `epics-base-rs`'s own `build.rs` does. A whole-file scan
    /// rejected that correct rule.
    ///
    /// Narrowing it to the named predicate makes the guard stricter about the
    /// thing it is for, not laxer: an exclusion-list `os != "rtems"` inside
    /// `local_account_db` still fails, and now an exclusion list smuggled in
    /// *around* the allowlist call would fail too, because the guard also pins
    /// that `main` reaches the capability only through this function.
    #[test]
    fn the_capability_is_owned_by_an_allowlist_in_the_build_script() {
        // Comment lines are stripped: the build script's own docs quote the
        // rejected `not(target_os = "rtems")` predicate in order to explain why
        // it is rejected. The guard is about what the code does.
        let build_src = include_str!("../../build.rs");
        let build: String = build_src
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(build.contains("cargo::rustc-check-cfg=cfg(local_account_db)"));
        assert!(build.contains("    \"linux\","));
        // `main` must not decide the capability itself — it asks the predicate.
        assert!(
            build.contains("if local_account_db(unix, &os) {"),
            "the capability emission must go through `fn local_account_db`"
        );

        let decl = "fn local_account_db(unix: bool, os: &str) -> bool {";
        let start = build
            .find(decl)
            .expect("the capability predicate must be a named function");
        let body = &build[start + decl.len()..];
        let body = &body[..body.find("\n}").expect("predicate body must close")];

        // The decision must go through the allowlist...
        assert!(
            body.contains("LOCAL_ACCOUNT_DB_TARGETS.contains("),
            "the capability must be decided by consulting the allowlist"
        );
        // ...and this is what separates an allowlist from an exclusion list:
        // an exclusion list has to NAME the targets it excludes. An allowlist
        // never mentions them, which is precisely why the next unix-family
        // target without an account database is handled correctly by a rule
        // nobody had to remember to update.
        for excluded in ["rtems", "vxworks"] {
            assert!(
                !body.contains(&format!("{excluded:?}")),
                "the predicate names {excluded}, so it is an exclusion list; \
                 a target that is not on the allowlist must simply not be on it"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial(epics_env)]
    fn the_user_is_the_passwd_name_not_the_environment() {
        use std::os::unix::ffi::OsStrExt;
        let saved: Vec<_> = ["USER", "EPICS_PVA_AUTH_USER"]
            .map(|k| (k, std::env::var_os(k)))
            .into();
        for (k, _) in &saved {
            // SAFETY: single-threaded under the `epics_env` serial group.
            unsafe { std::env::set_var(k, std::ffi::OsStr::from_bytes(b"op\xff")) };
        }
        let user = authnz_default_user();
        for (k, v) in saved {
            // SAFETY: same serial group.
            unsafe {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
        let pw_name = epics_base_rs::runtime::env::user_name().expect("passwd entry");
        assert_eq!(user.as_bytes(), pw_name.as_slice());
    }

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
    fn user_fallback_is_pvxs_ca_literal() {
        // pvxs buildCAMethod (src/client.cpp:519-526) sends "nobody" when the
        // OS user lookup fails. A resolved name passes through unchanged;
        // an absent one degrades to exactly that literal (not "anonymous").
        assert_eq!(user_or_ca_fallback(Some("alice".into())), "alice");
        assert_eq!(user_or_ca_fallback(None), "nobody");
    }

    #[test]
    fn host_fallback_is_pvxs_ca_literal() {
        // pvxs buildCAMethod (src/client.cpp:528-532) sends "invalidhost."
        // when gethostname() fails. gethostname() cannot be forced to fail
        // from a test, so the fallback literal is exercised through the
        // pure helper; a resolved host passes through unchanged.
        assert_eq!(host_or_ca_fallback(Some("myhost".into())), "myhost");
        assert_eq!(host_or_ca_fallback(None), "invalidhost.");
    }

    #[test]
    fn osd_get_roles_unknown_account_is_self() {
        // An account absent from the local passwd DB → pvxs "don't know
        // who this is" fallback (osgroups.cpp:57-60): the account name is
        // its only role. Deterministic regardless of host.
        let acct = "pvxs_unknown_account_zzqq";
        assert_eq!(osd_get_roles(acct.as_bytes()), vec![acct.to_string()]);
    }

    /// pvxs `osdGetRoles` looks the account up by `c_str()` (osgroups.cpp:55)
    /// but returns the whole `std::string` when it is unknown: a NUL ends the
    /// lookup name, and non-UTF-8 bytes survive the fallback.
    #[cfg(local_account_db)]
    #[test]
    fn osd_get_roles_takes_the_account_bytes() {
        assert_eq!(
            osd_get_roles(b"no\xffsuch"),
            vec![PvString::from(&b"no\xffsuch"[..])]
        );
        assert_eq!(osd_get_roles(b"root\0x"), osd_get_roles(b"root"));
    }

    #[cfg(local_account_db)]
    #[test]
    fn posix_groups_includes_passwd_primary_group() {
        use std::ffi::CStr;
        // Derive the current user's passwd primary-group NAME the way
        // pvxs osdGetRoles seeds getgrouplist — from `pw_gid`, not the
        // process `getgid()`. getgrouplist returns the basegid in its
        // list, so that primary group must appear in posix_groups().
        // If this env has no passwd entry or the gid has no name
        // mapping, there is nothing to assert.
        let primary_name: Option<PvString> = unsafe {
            let pw = libc::getpwuid(libc::getuid());
            if pw.is_null() {
                None
            } else {
                let gr = libc::getgrgid((*pw).pw_gid);
                if gr.is_null() {
                    None
                } else {
                    Some(PvString::from(CStr::from_ptr((*gr).gr_name).to_bytes()))
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

    #[cfg(local_account_db)]
    #[test]
    fn osd_get_roles_known_account_is_nonempty() {
        // `root` exists on every Unix host; its primary group maps to a
        // name via getgrgid, so the server-derived role set is non-empty.
        assert!(
            !osd_get_roles(b"root").is_empty(),
            "root must derive at least one group role from the local DB"
        );
    }
}
