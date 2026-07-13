pub fn get(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

/// The value of a configuration parameter whose compiled-in default is
/// the empty string (`EPICS_CAS_SERVER_PORT`, `EPICS_CAS_BEACON_PORT`,
/// … — see `configure/CONFIG_ENV`).
///
/// C parity: `envGetConfigParamPtr` (`envSubr.c:81-100`) returns the
/// environment string, falling back to the parameter's compiled default,
/// and maps an **empty** string to NULL. Callers such as
/// `rsrv::caservertask` (`caservertask.c:491-508`) use exactly that
/// NULL/non-NULL answer to pick *which* parameter to resolve, so the
/// presence test must treat `FOO=` as "not configured".
pub fn config_param_ptr(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
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

/// The single owner of every EPICS **inet port** environment parameter —
/// C `envGetInetPortConfigParam` (`envSubr.c:397-424`) on top of
/// `envGetLongConfigParam` (`envSubr.c:303-320`).
///
/// No caller may parse a port env var itself: the C semantics are not
/// `parse::<u16>()`.
///
/// - Unset: the compiled-in default string is parsed instead, which for
///   every port parameter is `default` itself — so the value is returned
///   silently (`configure/CONFIG_ENV`: `EPICS_CA_SERVER_PORT=5064`, …).
/// - Set: parsed with `sscanf("%ld")` leniency (leading whitespace, sign,
///   trailing garbage tolerated — see [`sscanf_long`]) out of the first
///   127 bytes, which is all C's `char text[128]` buffer keeps.
/// - Empty (`FOO=`) or no integer found: the "integer fetch failed"
///   diagnostics, then `default`.
/// - `<= IPPORT_USERRESERVED` (5000) or `> USHRT_MAX`: the "out of range"
///   diagnostics, then `default`.
///
/// The diagnostics are byte-identical to compiled C, which writes the
/// first via `fprintf(stderr)` and the other two via `errlogPrintf`
/// (console = stderr), once per resolution — there is no dedup in C:
///
/// ```text
/// Unable to find an integer in EPICS_CA_SERVER_PORT=abc
/// EPICS Environment "EPICS_CA_SERVER_PORT" integer fetch failed
/// setting "EPICS_CA_SERVER_PORT" = 5064
/// ```
/// ```text
/// EPICS Environment "EPICS_CA_SERVER_PORT" out of range
/// Setting "EPICS_CA_SERVER_PORT" = 5064
/// ```
pub fn env_inet_port(key: &str, default: u16) -> u16 {
    let Ok(raw) = std::env::var(key) else {
        // Not in the environment: C parses the compiled-in default
        // string, which round-trips to `default` with no diagnostic.
        return default;
    };

    // C `envGetConfigParam` copies into `char text[128]` with `strncpy`,
    // so only the first 127 bytes ever reach `sscanf`.
    let mut end = raw.len().min(127);
    while !raw.is_char_boundary(end) {
        end -= 1;
    }
    let text = &raw[..end];

    // Empty value: `envGetConfigParamPtr` returns NULL, so
    // `envGetLongConfigParam` fails *without* printing its own line.
    let parsed = if text.is_empty() {
        None
    } else {
        sscanf_long(text)
    };

    let mut port = match parsed {
        Some(v) => v,
        None => {
            if !text.is_empty() {
                eprintln!("Unable to find an integer in {key}={text}");
            }
            eprintln!("EPICS Environment \"{key}\" integer fetch failed");
            eprintln!("setting \"{key}\" = {default}");
            i64::from(default)
        }
    };

    if port <= i64::from(IPPORT_USERRESERVED) || port > i64::from(u16::MAX) {
        eprintln!("EPICS Environment \"{key}\" out of range");
        port = i64::from(default);
        eprintln!("Setting \"{key}\" = {default}");
    }

    // Range-checked above, so the cast cannot truncate.
    port as u16
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
    fn test_env_inet_port_valid() {
        unsafe { std::env::set_var("_EPICS_RT_TEST_PORT", "8080") };
        assert_eq!(env_inet_port("_EPICS_RT_TEST_PORT", 5064), 8080);
        unsafe { std::env::remove_var("_EPICS_RT_TEST_PORT") };
    }

    #[test]
    #[serial(epics_env)]
    fn test_env_inet_port_invalid() {
        unsafe { std::env::set_var("_EPICS_RT_TEST_PORT_BAD", "not_a_number") };
        assert_eq!(env_inet_port("_EPICS_RT_TEST_PORT_BAD", 5064), 5064);
        unsafe { std::env::remove_var("_EPICS_RT_TEST_PORT_BAD") };
    }

    #[test]
    #[serial(epics_env)]
    fn test_env_inet_port_missing() {
        assert_eq!(env_inet_port("_EPICS_RT_NONEXISTENT_VAR_12345", 5064), 5064);
    }

    /// An empty value is "not configured" for `envGetConfigParamPtr`, so
    /// `envGetLongConfigParam` fails and the port falls back to the default.
    #[test]
    #[serial(epics_env)]
    fn test_env_inet_port_empty_value_is_a_failed_fetch() {
        unsafe { std::env::set_var("_EPICS_RT_TEST_PORT_EMPTY", "") };
        assert_eq!(env_inet_port("_EPICS_RT_TEST_PORT_EMPTY", 5064), 5064);
        assert_eq!(config_param_ptr("_EPICS_RT_TEST_PORT_EMPTY"), None);
        unsafe { std::env::remove_var("_EPICS_RT_TEST_PORT_EMPTY") };
    }

    #[test]
    #[serial(epics_env)]
    fn test_env_inet_port_lenient_whitespace_and_suffix() {
        // C `sscanf("%ld")` accepts leading whitespace + trailing junk;
        // `u16::parse` would reject both.
        unsafe { std::env::set_var("_EPICS_RT_TEST_PORT_WS", " 6064") };
        assert_eq!(env_inet_port("_EPICS_RT_TEST_PORT_WS", 5064), 6064);
        unsafe { std::env::set_var("_EPICS_RT_TEST_PORT_WS", "6064abc") };
        assert_eq!(env_inet_port("_EPICS_RT_TEST_PORT_WS", 5064), 6064);
        unsafe { std::env::remove_var("_EPICS_RT_TEST_PORT_WS") };
    }

    /// C copies the value into `char text[128]`, so a digit run that only
    /// starts past byte 127 is invisible to `sscanf`.
    #[test]
    #[serial(epics_env)]
    fn test_env_inet_port_truncates_at_the_c_buffer_length() {
        let long = format!("{}6064", " ".repeat(130));
        unsafe { std::env::set_var("_EPICS_RT_TEST_PORT_LONG", &long) };
        assert_eq!(env_inet_port("_EPICS_RT_TEST_PORT_LONG", 5064), 5064);
        // The same digits inside the first 127 bytes are seen.
        let short = format!("{}6064", " ".repeat(100));
        unsafe { std::env::set_var("_EPICS_RT_TEST_PORT_LONG", &short) };
        assert_eq!(env_inet_port("_EPICS_RT_TEST_PORT_LONG", 5064), 6064);
        unsafe { std::env::remove_var("_EPICS_RT_TEST_PORT_LONG") };
    }

    #[test]
    #[serial(epics_env)]
    fn test_env_inet_port_rejects_reserved_and_out_of_range_ports() {
        // C `envGetInetPortConfigParam` rejects port <= IPPORT_USERRESERVED
        // (5000) or > USHRT_MAX, falling back to the compiled default.
        for bad in [
            "0", "1", "80", "443", "3000", "5000", "-1", "65536", "70000", "99999",
        ] {
            unsafe { std::env::set_var("_EPICS_RT_TEST_PORT_RANGE", bad) };
            assert_eq!(
                env_inet_port("_EPICS_RT_TEST_PORT_RANGE", 5064),
                5064,
                "out-of-range port {bad:?} must fall back to default"
            );
        }
        // 5001 is the first acceptable port (> IPPORT_USERRESERVED).
        unsafe { std::env::set_var("_EPICS_RT_TEST_PORT_RANGE", "5001") };
        assert_eq!(env_inet_port("_EPICS_RT_TEST_PORT_RANGE", 5064), 5001);
        unsafe { std::env::set_var("_EPICS_RT_TEST_PORT_RANGE", "65535") };
        assert_eq!(env_inet_port("_EPICS_RT_TEST_PORT_RANGE", 5064), 65535);
        unsafe { std::env::remove_var("_EPICS_RT_TEST_PORT_RANGE") };
    }

    /// `envGetConfigParamPtr` presence test — the gate `caservertask.c`
    /// uses to pick between the CAS-specific and the generic parameter.
    #[test]
    #[serial(epics_env)]
    fn test_config_param_ptr_presence() {
        assert_eq!(config_param_ptr("_EPICS_RT_NONEXISTENT_VAR_12345"), None);
        unsafe { std::env::set_var("_EPICS_RT_TEST_PTR", "") };
        assert_eq!(config_param_ptr("_EPICS_RT_TEST_PTR"), None);
        unsafe { std::env::set_var("_EPICS_RT_TEST_PTR", "abc") };
        assert_eq!(
            config_param_ptr("_EPICS_RT_TEST_PTR"),
            Some("abc".to_string())
        );
        unsafe { std::env::remove_var("_EPICS_RT_TEST_PTR") };
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
