pub fn get(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

pub fn get_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

pub fn get_u16(key: &str, default: u16) -> u16 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

pub fn get_bool(key: &str, default: bool) -> bool {
    std::env::var(key)
        .ok()
        .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(default)
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
/// Usage:
/// ```ignore
/// // In a binary crate, set ADCORE pointing to the ad-core crate:
/// epics_base_rs::runtime::env::set_crate_path("ADCORE", env!("CARGO_MANIFEST_DIR"), "../../crates/ad-core-rs");
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
    fn test_get_u16_valid() {
        unsafe { std::env::set_var("_EPICS_RT_TEST_PORT", "8080") };
        assert_eq!(get_u16("_EPICS_RT_TEST_PORT", 5064), 8080);
        unsafe { std::env::remove_var("_EPICS_RT_TEST_PORT") };
    }

    #[test]
    fn test_get_u16_invalid() {
        unsafe { std::env::set_var("_EPICS_RT_TEST_PORT_BAD", "not_a_number") };
        assert_eq!(get_u16("_EPICS_RT_TEST_PORT_BAD", 5064), 5064);
        unsafe { std::env::remove_var("_EPICS_RT_TEST_PORT_BAD") };
    }

    #[test]
    fn test_get_u16_missing() {
        assert_eq!(get_u16("_EPICS_RT_NONEXISTENT_VAR_12345", 5064), 5064);
    }

    #[test]
    fn test_get_bool_true_values() {
        for val in &["1", "true", "TRUE", "yes", "YES"] {
            unsafe { std::env::set_var("_EPICS_RT_TEST_BOOL", val) };
            assert!(
                get_bool("_EPICS_RT_TEST_BOOL", false),
                "failed for value: {val}"
            );
        }
        unsafe { std::env::remove_var("_EPICS_RT_TEST_BOOL") };
    }

    #[test]
    fn test_get_bool_false_values() {
        unsafe { std::env::set_var("_EPICS_RT_TEST_BOOL_F", "no") };
        assert!(!get_bool("_EPICS_RT_TEST_BOOL_F", true));
        unsafe { std::env::remove_var("_EPICS_RT_TEST_BOOL_F") };
    }

    #[test]
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
