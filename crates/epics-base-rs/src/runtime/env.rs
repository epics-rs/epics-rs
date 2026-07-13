pub fn get(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

pub fn get_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Lowest port number a configurable EPICS port may take.
///
/// C parity: `IPPORT_USERRESERVED` is `#define`d to `5000` on every
/// supported platform (`osi/os/*/osdSock.h`). `envGetInetPortConfigParam`
/// rejects any port `<= IPPORT_USERRESERVED` or `> USHRT_MAX`.
pub const IPPORT_USERRESERVED: u32 = 5000;

/// Parse the leading integer out of a string the way C `sscanf("%ld")`
/// does: skip leading whitespace, accept an optional `+`/`-` sign, read
/// the run of decimal digits, and ignore any trailing garbage suffix.
///
/// Returns `None` when no digit is found (C `sscanf` returns 0 matches).
fn sscanf_long(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    let mut i = 0;
    // Skip leading whitespace (C `isspace`: space, \t, \n, \r, \v, \f).
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    let mut neg = false;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        neg = bytes[i] == b'-';
        i += 1;
    }
    let start = i;
    let mut value: i64 = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        value = value
            .saturating_mul(10)
            .saturating_add((bytes[i] - b'0') as i64);
        i += 1;
    }
    if i == start {
        // No digits — sscanf matched nothing.
        return None;
    }
    Some(if neg { -value } else { value })
}

/// Read an env var as a `u16`, matching C `envGetInetPortConfigParam`
/// (`envSubr.c:397-424`) — used for port parameters.
///
/// C parity:
/// - Parses with `sscanf("%ld")` semantics (leading whitespace, sign,
///   trailing garbage tolerated) — see [`sscanf_long`].
/// - When the value fails to parse, or is out of the legal port range
///   (`<= IPPORT_USERRESERVED` (5000) or `> USHRT_MAX`), it logs a
///   diagnostic and falls back to the compiled `default`.
/// - C emits these via `errlogPrintf`; here they go through `tracing`
///   at `warn`, which is the crate's logging path.
pub fn get_u16(key: &str, default: u16) -> u16 {
    let Some(raw) = std::env::var(key).ok() else {
        return default;
    };
    let parsed = match sscanf_long(&raw) {
        Some(v) => v,
        None => {
            // C `envGetLongConfigParam`: "Unable to find an integer in ..."
            // then `envGetInetPortConfigParam`: integer fetch failed.
            tracing::warn!(
                target: "epics_base_rs::env",
                var = key,
                value = %raw,
                default,
                "EPICS environment integer fetch failed; using default"
            );
            return default;
        }
    };
    if parsed <= IPPORT_USERRESERVED as i64 || parsed > u16::MAX as i64 {
        // C `envGetInetPortConfigParam`: "out of range" diagnostic +
        // fall back to the compiled default.
        tracing::warn!(
            target: "epics_base_rs::env",
            var = key,
            value = parsed,
            default,
            "EPICS environment port out of range (must be > {IPPORT_USERRESERVED} and <= {}); using default",
            u16::MAX
        );
        return default;
    }
    parsed as u16
}

/// Read an env var as a boolean, matching C `envGetBoolConfigParam`
/// (`envSubr.c:324-333`).
///
/// C parity: `*pBool = epicsStrCaseCmp(text, "yes") == 0;` — the value
/// is true if and only if it equals `"yes"` **case-insensitively**.
/// Every other value (`"1"`, `"true"`, `"on"`, `"yes "` with trailing
/// whitespace, …) is false. There is no whitespace trimming — C
/// compares the raw env string.
pub fn get_bool(key: &str, default: bool) -> bool {
    match std::env::var(key).ok() {
        Some(v) => v.eq_ignore_ascii_case("yes"),
        None => default,
    }
}

/// Set an environment variable only if it is not already set.
///
/// # Safety
/// Uses `std::env::set_var` which is unsafe in multi-threaded programs.
/// Call this early in main(), before spawning threads.
pub fn set_default(name: &str, value: &str) {
    if std::env::var_os(name).is_none() {
        // SAFETY: called during IOC startup, before multi-threaded operation.
        unsafe { std::env::set_var(name, value) };
    }
}

/// Set an environment variable to a path relative to a crate's `CARGO_MANIFEST_DIR`.
///
/// `manifest_dir` must be the manifest dir of the crate that *owns* the assets,
/// and `relative` a path beneath it. Reaching out of the crate (`../other-crate`)
/// produces a path that only exists in a sibling-checkout layout: under a
/// registry checkout the sibling is version-suffixed (`ad-core-rs-0.22.1`) and
/// the path never resolves. To point at another crate's assets, use the dir that
/// crate exports (e.g. [`ad_core_rs::AD_CORE_DIR`], `motor_rs::MOTOR_IOC_DIR`).
///
/// Usage:
/// ```ignore
/// // In a binary crate, expose that crate's own db/ directory:
/// epics_base_rs::runtime::env::set_crate_path("MYIOC", env!("CARGO_MANIFEST_DIR"), "db");
/// ```
pub fn set_crate_path(name: &str, manifest_dir: &str, relative: &str) {
    set_default(name, &format!("{manifest_dir}/{relative}"));
}

pub fn hostname() -> String {
    hostname::get()
        .ok()
        .and_then(|s| s.into_string().ok())
        .unwrap_or_else(|| "localhost".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn test_get_existing() {
        // SAFETY: test runs single-threaded
        unsafe { std::env::set_var("_EPICS_RT_TEST_VAR", "hello") };
        assert_eq!(get("_EPICS_RT_TEST_VAR"), Some("hello".to_string()));
        unsafe { std::env::remove_var("_EPICS_RT_TEST_VAR") };
    }

    #[test]
    fn test_get_missing() {
        assert_eq!(get("_EPICS_RT_NONEXISTENT_VAR_12345"), None);
    }

    #[test]
    fn test_get_or_default() {
        assert_eq!(
            get_or("_EPICS_RT_NONEXISTENT_VAR_12345", "fallback"),
            "fallback"
        );
    }

    #[test]
    fn test_sscanf_long_lenient_parsing() {
        // C `sscanf("%ld")` semantics: leading whitespace skipped,
        // sign accepted, trailing garbage tolerated.
        assert_eq!(sscanf_long("5064"), Some(5064));
        assert_eq!(sscanf_long("  6064"), Some(6064));
        assert_eq!(sscanf_long("\t6064\n"), Some(6064));
        assert_eq!(sscanf_long("5064abc"), Some(5064));
        assert_eq!(sscanf_long("+6064"), Some(6064));
        assert_eq!(sscanf_long("-1"), Some(-1));
        assert_eq!(sscanf_long("not_a_number"), None);
        assert_eq!(sscanf_long(""), None);
        assert_eq!(sscanf_long("   "), None);
    }

    #[test]
    #[serial(epics_env)]
    fn test_get_u16_valid() {
        unsafe { std::env::set_var("_EPICS_RT_TEST_PORT", "8080") };
        assert_eq!(get_u16("_EPICS_RT_TEST_PORT", 5064), 8080);
        unsafe { std::env::remove_var("_EPICS_RT_TEST_PORT") };
    }

    #[test]
    #[serial(epics_env)]
    fn test_get_u16_invalid() {
        unsafe { std::env::set_var("_EPICS_RT_TEST_PORT_BAD", "not_a_number") };
        assert_eq!(get_u16("_EPICS_RT_TEST_PORT_BAD", 5064), 5064);
        unsafe { std::env::remove_var("_EPICS_RT_TEST_PORT_BAD") };
    }

    #[test]
    #[serial(epics_env)]
    fn test_get_u16_missing() {
        assert_eq!(get_u16("_EPICS_RT_NONEXISTENT_VAR_12345", 5064), 5064);
    }

    #[test]
    #[serial(epics_env)]
    fn test_get_u16_lenient_whitespace_and_suffix() {
        // C `sscanf("%ld")` accepts leading whitespace + trailing junk;
        // `u16::parse` would reject both.
        unsafe { std::env::set_var("_EPICS_RT_TEST_PORT_WS", " 6064") };
        assert_eq!(get_u16("_EPICS_RT_TEST_PORT_WS", 5064), 6064);
        unsafe { std::env::set_var("_EPICS_RT_TEST_PORT_WS", "6064abc") };
        assert_eq!(get_u16("_EPICS_RT_TEST_PORT_WS", 5064), 6064);
        unsafe { std::env::remove_var("_EPICS_RT_TEST_PORT_WS") };
    }

    #[test]
    #[serial(epics_env)]
    fn test_get_u16_rejects_reserved_and_out_of_range_ports() {
        // C `envGetInetPortConfigParam` rejects port <= IPPORT_USERRESERVED
        // (5000) or > USHRT_MAX, falling back to the compiled default.
        for bad in ["0", "1", "80", "443", "5000", "-1", "65536", "99999"] {
            unsafe { std::env::set_var("_EPICS_RT_TEST_PORT_RANGE", bad) };
            assert_eq!(
                get_u16("_EPICS_RT_TEST_PORT_RANGE", 5064),
                5064,
                "out-of-range port {bad:?} must fall back to default"
            );
        }
        // 5001 is the first acceptable port (> IPPORT_USERRESERVED).
        unsafe { std::env::set_var("_EPICS_RT_TEST_PORT_RANGE", "5001") };
        assert_eq!(get_u16("_EPICS_RT_TEST_PORT_RANGE", 5064), 5001);
        unsafe { std::env::set_var("_EPICS_RT_TEST_PORT_RANGE", "65535") };
        assert_eq!(get_u16("_EPICS_RT_TEST_PORT_RANGE", 5064), 65535);
        unsafe { std::env::remove_var("_EPICS_RT_TEST_PORT_RANGE") };
    }

    #[test]
    #[serial(epics_env)]
    fn test_get_bool_only_yes_case_insensitive() {
        // C `envGetBoolConfigParam`: true iff epicsStrCaseCmp(text,"yes")==0.
        for truthy in &["yes", "YES", "Yes", "yEs", "yeS"] {
            unsafe { std::env::set_var("_EPICS_RT_TEST_BOOL", truthy) };
            assert!(
                get_bool("_EPICS_RT_TEST_BOOL", false),
                "case-insensitive 'yes' must be true: {truthy:?}"
            );
        }
        // Everything else is false — including values the old impl accepted.
        for falsy in &[
            "1", "true", "TRUE", "on", "yes ", " yes", "yes\n", "no", "0", "",
        ] {
            unsafe { std::env::set_var("_EPICS_RT_TEST_BOOL", falsy) };
            assert!(
                !get_bool("_EPICS_RT_TEST_BOOL", true),
                "non-'yes' value must be false: {falsy:?}"
            );
        }
        unsafe { std::env::remove_var("_EPICS_RT_TEST_BOOL") };
    }

    #[test]
    #[serial(epics_env)]
    fn test_get_bool_missing() {
        assert!(get_bool("_EPICS_RT_NONEXISTENT_VAR_12345", true));
        assert!(!get_bool("_EPICS_RT_NONEXISTENT_VAR_12345", false));
    }

    #[test]
    fn test_hostname() {
        let h = hostname();
        assert!(!h.is_empty());
    }
}
