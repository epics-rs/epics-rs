//! EPICS environment parameters — C's `envSubr.c`.
//!
//! The **only** declaration of an EPICS environment default in this workspace is
//! the generated [`super::env_table`], which `env-codegen` derives from the
//! vendored `configure/CONFIG_ENV` + `CONFIG_SITE_ENV` the same way C's
//! `bldEnvData.pl` derives `envData.c`. Everything else resolves *through* it.
//!
//! That is enforced by shape, not by convention: [`EnvParam`] has no public
//! constructor (only the generated table, inside this crate, can mint one), and
//! none of its accessors takes a `default` argument. A caller that could supply
//! a default would be a second declaration source, so no caller can.
//!
//! [`get`] remains for the handful of variables that are NOT EPICS Base
//! `ENV_PARAM`s — C reads those with a plain `getenv` and its caller owns the
//! fallback, so this port does too.

use super::stdlib::epics_parse_double;

/// One row of C's `ENV_PARAM` table (`envDefs.h:41-45`): a parameter name and
/// the default compiled in from `configure/CONFIG_ENV`.
///
/// Values of this type exist only in the generated [`super::env_table`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnvParam {
    name: &'static str,
    default: &'static str,
}

impl EnvParam {
    /// Crate-private: the generated table is the only declaration source.
    pub(crate) const fn new(name: &'static str, default: &'static str) -> Self {
        Self { name, default }
    }

    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// The compiled-in default string, exactly as it appears in C's
    /// `envData.c`.
    pub const fn default_str(&self) -> &'static str {
        self.default
    }

    /// C `envGetConfigParamPtr` (`envSubr.c:81-100`): the environment string,
    /// falling back to the compiled default — with an **empty** result mapped to
    /// `None`.
    ///
    /// The `None`/`Some` answer is a load-bearing *presence* test, not just an
    /// absence check: `caservertask.c:492-509` uses it to pick *which* of two
    /// parameters to resolve, so `FOO=` must read as "not configured".
    pub fn get(&self) -> Option<String> {
        let v = std::env::var(self.name).unwrap_or_else(|_| self.default.to_string());
        if v.is_empty() { None } else { Some(v) }
    }

    /// C `envGetLongConfigParam` (`envSubr.c:303-320`) — `sscanf("%ld")` over the
    /// resolved value, out of the first 127 bytes (all C's `char text[128]`
    /// keeps).
    ///
    /// `None` is C's non-zero status. A set-but-unparseable value also prints
    /// C's stderr line first; an *unresolvable* one (empty value and empty
    /// default) prints nothing, because `envGetConfigParam` returned NULL before
    /// `sscanf` ever ran.
    pub fn long(&self) -> Option<i64> {
        let text = self.get()?;
        let text = truncate_to_c_buffer(&text);
        match sscanf_long(text) {
            Some(v) => Some(v),
            None => {
                eprintln!("Unable to find an integer in {}={text}", self.name);
                None
            }
        }
    }

    /// The C caller idiom for a long-valued parameter:
    ///
    /// ```c
    /// long i = 50;                            /* == the table default */
    /// envGetLongConfigParam(&IOCSH_HISTSIZE, &i);
    /// ```
    ///
    /// i.e. a rejected value leaves the compiled default standing. The `50` in
    /// C's source is a hand-copy of the table; here it comes from the table.
    pub fn long_or_default(&self) -> i64 {
        self.long()
            .or_else(|| sscanf_long(self.default))
            .unwrap_or(0)
    }

    /// C `envGetDoubleConfigParam` (`envSubr.c:191-211`) — `epicsScanDouble` over
    /// the resolved value.
    ///
    /// An unset parameter resolves to its compiled default and parses, so
    /// `Ok(30.0)` for `EPICS_CA_CONN_TMO` on a clean shell. `Err` is C's non-zero
    /// status: the value (or the default) is empty, or it did not scan — and in
    /// the latter case C's stderr line has already been printed. C's callers then
    /// print their own named diagnostic on top of it.
    pub fn double(&self) -> Result<f64, EnvDoubleError> {
        let raw = self.get().ok_or(EnvDoubleError::Unresolvable)?;
        epics_parse_double(&raw).map_err(|_| {
            eprintln!("Unable to find a real number in {}={raw}", self.name);
            EnvDoubleError::Invalid
        })
    }

    /// C `envGetBoolConfigParam` (`envSubr.c:324-333`):
    /// `*pBool = epicsStrCaseCmp(text, "yes") == 0`, i.e. true **iff** the
    /// resolved value is the word `yes`, any case. `"1"`, `"true"`, `"on"`,
    /// `"yes "` are all false.
    ///
    /// `None` is C's -1 status — nothing to compare, so C leaves the caller's
    /// variable untouched. The caller's `unwrap_or(..)` is then C's own
    /// initializer, not a second copy of the table default.
    pub fn bool(&self) -> Option<bool> {
        Some(self.get()?.eq_ignore_ascii_case("yes"))
    }

    /// C `envGetInetPortConfigParam` (`envSubr.c:397-424`).
    ///
    /// `fallback` is the *compiled* port the C caller passes as `defaultPort`
    /// — for the `EPICS_CAS_*` overrides that is a **different** parameter's
    /// default (`caservertask.c:493-494` passes `CA_SERVER_PORT`), which is why
    /// it is an argument rather than `self.default`. Derive it with
    /// [`EnvParam::default_port`] so it still comes from the table.
    ///
    /// - Not resolvable, or no integer found: the "integer fetch failed"
    ///   diagnostics, then `fallback`.
    /// - `<= IPPORT_USERRESERVED` (5000) or `> USHRT_MAX`: the "out of range"
    ///   diagnostics, then `fallback`.
    ///
    /// The diagnostics are byte-identical to compiled C, once per resolution —
    /// there is no dedup in C:
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
    pub fn inet_port(&self, fallback: u16) -> u16 {
        let mut port = match self.long() {
            Some(v) => v,
            None => {
                eprintln!("EPICS Environment \"{}\" integer fetch failed", self.name);
                eprintln!("setting \"{}\" = {fallback}", self.name);
                i64::from(fallback)
            }
        };

        if port <= i64::from(IPPORT_USERRESERVED) || port > i64::from(u16::MAX) {
            eprintln!("EPICS Environment \"{}\" out of range", self.name);
            port = i64::from(fallback);
            eprintln!("Setting \"{}\" = {fallback}", self.name);
        }

        // Range-checked above, so the cast cannot truncate.
        port as u16
    }

    /// The compiled default read as a port, at compile time.
    ///
    /// This is how a `pub const CA_SERVER_PORT: u16` is derived from the table
    /// instead of hand-written next to it: a default that is not a valid port
    /// fails const-evaluation, so the constant cannot drift from `CONFIG_ENV`.
    pub const fn default_port(&self) -> u16 {
        let bytes = self.default.as_bytes();
        assert!(!bytes.is_empty(), "parameter has no compiled default port");
        let mut i = 0;
        let mut value: u32 = 0;
        while i < bytes.len() {
            let d = bytes[i];
            assert!(d.is_ascii_digit(), "compiled default is not a port number");
            value = value * 10 + (d - b'0') as u32;
            assert!(
                value <= u16::MAX as u32,
                "compiled default exceeds USHRT_MAX"
            );
            i += 1;
        }
        value as u16
    }

    /// C `envPrtConfigParam` (`envSubr.c:353-364`) — one line of
    /// `epicsPrtEnvParams`.
    pub fn describe(&self) -> String {
        match self.get() {
            Some(v) => format!("{}: {v}", self.name),
            None => format!("{} is undefined", self.name),
        }
    }
}

/// Why [`EnvParam::double`] did not yield a value — C's non-zero status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvDoubleError {
    /// The value and the compiled default are both empty, so
    /// `envGetConfigParam` returned NULL before `epicsScanDouble` ran. No
    /// diagnostic.
    Unresolvable,
    /// `epicsScanDouble` rejected the value. C's stderr line is already printed.
    Invalid,
}

/// A plain `getenv`, no default and no `ENV_PARAM` behind it.
///
/// Only for variables that are NOT in C's table (`EPICS_CA_USE_SHELL_VARS`, the
/// pvxs `EPICS_PVA*` set, the port's own `EPICS_RS_*`); C reads those with
/// `getenv` too and its caller owns the fallback. Every EPICS Base `ENV_PARAM`
/// must go through [`super::env_table`].
pub fn get(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

/// Lowest port number a configurable EPICS port may take.
///
/// C parity: `IPPORT_USERRESERVED` is `#define`d to `5000` on every
/// supported platform (`osi/os/*/osdSock.h`). `envGetInetPortConfigParam`
/// rejects any port `<= IPPORT_USERRESERVED` or `> USHRT_MAX`.
pub const IPPORT_USERRESERVED: u32 = 5000;

/// C `envGetConfigParam` copies into `char text[128]` with `strncpy`, so only
/// the first 127 bytes ever reach `sscanf`.
fn truncate_to_c_buffer(raw: &str) -> &str {
    let mut end = raw.len().min(127);
    while !raw.is_char_boundary(end) {
        end -= 1;
    }
    &raw[..end]
}

/// Parse the leading integer out of a string the way C `sscanf("%ld")`
/// does: skip leading whitespace, accept an optional `+`/`-` sign, read
/// the run of decimal digits, and ignore any trailing garbage suffix.
///
/// Returns `None` when no digit is found (C `sscanf` returns 0 matches).
fn sscanf_long(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    let mut i = 0;
    // Skip leading whitespace (C `isspace`: space, \t, \n, \v, \f, \r).
    while i < bytes.len() && crate::runtime::stdlib::c_isspace(bytes[i] as char) {
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

/// C `epicsPrtEnvParams` (`envSubr.c:383-392`) — every parameter EPICS Base
/// knows, with its effective value, in `env_param_list[]` order.
pub fn prt_env_params() -> impl Iterator<Item = String> {
    super::env_table::ENV_PARAM_LIST
        .iter()
        .map(EnvParam::describe)
}

/// C `iocshRegisterCommon` (`iocshRegisterCommon.c:54-67`) — the environment
/// variables an IOC shell publishes about *itself*, so a `.db`, a `.substitutions`
/// or an `st.cmd` can expand `$(EPICS_VERSION_FULL)` or `$(ARCH)`.
///
/// These are outputs, not inputs: they are not `ENV_PARAM`s, they have no
/// compiled default to fall back to, and C sets them with `epicsEnvSet`, which
/// overwrites whatever the shell had. So does this — but only once per process,
/// because a re-entrant `set_var` is what makes it unsound.
///
/// `ARCH` is C's `envGetConfigParam(&EPICS_BUILD_TARGET_ARCH)`, which is the
/// build that produced the binary; here that is [`super::build_info`], reached
/// through the generated table like every other parameter.
pub fn register_iocsh_env_vars() {
    use super::version as v;
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // C: `if (targetArch) epicsEnvSet("ARCH", targetArch)` — an unresolvable
        // parameter is a NULL, and NULL is not published as an empty `ARCH`.
        let mut vars: Vec<(&str, String)> = super::env_table::EPICS_BUILD_TARGET_ARCH
            .get()
            .map(|arch| ("ARCH", arch))
            .into_iter()
            .collect();
        vars.extend([
            ("EPICS_VERSION_MAJOR", v::EPICS_VERSION.to_string()),
            ("EPICS_VERSION_MIDDLE", v::EPICS_REVISION.to_string()),
            ("EPICS_VERSION_MINOR", v::EPICS_MODIFICATION.to_string()),
            ("EPICS_VERSION_PATCH", v::EPICS_PATCH_LEVEL.to_string()),
            ("EPICS_VERSION_SNAPSHOT", v::EPICS_DEV_SNAPSHOT.to_string()),
            ("EPICS_VERSION_SITE", v::EPICS_SITE_VERSION.to_string()),
            ("EPICS_VERSION_SHORT", v::EPICS_VERSION_SHORT.to_string()),
            ("EPICS_VERSION_FULL", v::EPICS_VERSION_FULL.to_string()),
        ]);
        for (name, value) in vars {
            // SAFETY: `Once`, and called from `IocShell::new` — C registers these
            // at the same point, before any script or record can read them.
            unsafe { std::env::set_var(name, value) };
        }
    });
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
/// crate exports (e.g. `ad_core_rs::AD_CORE_DIR`, `motor_rs::MOTOR_IOC_DIR`).
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
    use super::super::env_table::*;
    use super::*;
    use serial_test::serial;

    /// A scratch parameter: the accessors are the unit under test, and the
    /// generated table is not the place to add fixtures.
    const SCRATCH: EnvParam = EnvParam::new("_EPICS_RT_TEST_VAR", "");
    const SCRATCH_PORT: EnvParam = EnvParam::new("_EPICS_RT_TEST_PORT", "5064");

    fn clear(p: EnvParam) {
        // SAFETY: every test that mutates env is `#[serial(epics_env)]`.
        unsafe { std::env::remove_var(p.name()) };
    }
    fn set(p: EnvParam, v: &str) {
        // SAFETY: see `clear`.
        unsafe { std::env::set_var(p.name(), v) };
    }

    /// The state every compiled-default assertion needs: nothing the generated
    /// table knows about is set in the process environment. Restored on drop.
    ///
    /// Hand-picked `clear` calls cannot establish it, because the list that is
    /// cleared and the list that is asserted on are then maintained apart:
    /// `unset_resolves_to_the_compiled_default` cleared three parameters and
    /// asserted on five, so a developer shell exporting
    /// `EPICS_CA_AUTO_ADDR_LIST=NO` — which a shell that sets
    /// `EPICS_CA_ADDR_LIST` normally does — failed it, on a table the test
    /// never doubted. Walking `ENV_PARAM_LIST` deletes the second list:
    /// whatever a test asserts on, the table has already covered.
    #[must_use]
    struct SilentEnv(Vec<(&'static str, String)>);

    impl SilentEnv {
        fn take() -> Self {
            let saved = ENV_PARAM_LIST
                .iter()
                .filter_map(|p| std::env::var(p.name()).ok().map(|v| (p.name(), v)))
                .collect();
            for p in ENV_PARAM_LIST {
                clear(*p);
            }
            Self(saved)
        }
    }

    impl Drop for SilentEnv {
        fn drop(&mut self) {
            for (name, value) in &self.0 {
                // SAFETY: see `clear`.
                unsafe { std::env::set_var(name, value) };
            }
        }
    }

    #[test]
    fn test_get_missing() {
        assert_eq!(get("_EPICS_RT_NONEXISTENT_VAR_12345"), None);
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

    /// C `sscanf` skips leading whitespace by `isspace`, which includes the
    /// vertical tab (`envSubr.c:315`). `EPICS_CA_SERVER_PORT=$'\v5064'`
    /// therefore resolves to 5064 on a C IOC; reading no integer here would
    /// discard the operator's port and silently bind the compiled default.
    #[test]
    fn a_leading_vertical_tab_is_whitespace_as_it_is_to_c() {
        assert_eq!(sscanf_long("\u{0b}5064"), Some(5064));
        assert_eq!(sscanf_long("\u{0b}\t \u{0c}-1"), Some(-1));
        assert_eq!(sscanf_long("\u{0b}"), None);
    }

    /// The compiled default resolves when the environment is silent — the whole
    /// point of the table.
    #[test]
    #[serial(epics_env)]
    fn unset_resolves_to_the_compiled_default() {
        let _silent = SilentEnv::take();
        assert_eq!(EPICS_CA_CONN_TMO.double(), Ok(30.0));
        assert_eq!(EPICS_CA_SERVER_PORT.long(), Some(5064));
        assert_eq!(EPICS_CA_AUTO_ADDR_LIST.bool(), Some(true));
        // Empty compiled default: `envGetConfigParamPtr` answers NULL.
        assert_eq!(EPICS_CA_ADDR_LIST.get(), None);
        assert_eq!(EPICS_CAS_SERVER_PORT.get(), None);
    }

    /// `default_port()` is const-evaluated, so this is really a compile-time
    /// assertion that the table still holds C's ports.
    #[test]
    fn default_port_is_const_derived_from_the_table() {
        const SERVER: u16 = EPICS_CA_SERVER_PORT.default_port();
        const REPEATER: u16 = EPICS_CA_REPEATER_PORT.default_port();
        const LOG: u16 = EPICS_IOC_LOG_PORT.default_port();
        assert_eq!((SERVER, REPEATER, LOG), (5064, 5065, 7004));
    }

    #[test]
    #[serial(epics_env)]
    fn test_env_inet_port_valid() {
        set(SCRATCH_PORT, "8080");
        assert_eq!(SCRATCH_PORT.inet_port(5064), 8080);
        clear(SCRATCH_PORT);
    }

    #[test]
    #[serial(epics_env)]
    fn test_env_inet_port_invalid_and_missing() {
        set(SCRATCH_PORT, "not_a_number");
        assert_eq!(SCRATCH_PORT.inet_port(5064), 5064);
        clear(SCRATCH_PORT);
        // Unset: the compiled "5064" default parses, silently.
        assert_eq!(SCRATCH_PORT.inet_port(5064), 5064);
    }

    /// An empty value is "not configured" for `envGetConfigParamPtr`, so
    /// `envGetLongConfigParam` fails and the port falls back to `fallback`.
    #[test]
    #[serial(epics_env)]
    fn test_env_inet_port_empty_value_is_a_failed_fetch() {
        set(SCRATCH, "");
        assert_eq!(SCRATCH.get(), None);
        assert_eq!(SCRATCH.inet_port(5064), 5064);
        clear(SCRATCH);
    }

    #[test]
    #[serial(epics_env)]
    fn test_env_inet_port_lenient_whitespace_and_suffix() {
        // C `sscanf("%ld")` accepts leading whitespace + trailing junk;
        // `u16::parse` would reject both.
        set(SCRATCH_PORT, " 6064");
        assert_eq!(SCRATCH_PORT.inet_port(5064), 6064);
        set(SCRATCH_PORT, "6064abc");
        assert_eq!(SCRATCH_PORT.inet_port(5064), 6064);
        clear(SCRATCH_PORT);
    }

    /// C copies the value into `char text[128]`, so a digit run that only
    /// starts past byte 127 is invisible to `sscanf`.
    #[test]
    #[serial(epics_env)]
    fn test_env_inet_port_truncates_at_the_c_buffer_length() {
        set(SCRATCH_PORT, &format!("{}6064", " ".repeat(130)));
        assert_eq!(SCRATCH_PORT.inet_port(5064), 5064);
        // The same digits inside the first 127 bytes are seen.
        set(SCRATCH_PORT, &format!("{}6064", " ".repeat(100)));
        assert_eq!(SCRATCH_PORT.inet_port(5064), 6064);
        clear(SCRATCH_PORT);
    }

    #[test]
    #[serial(epics_env)]
    fn test_env_inet_port_rejects_reserved_and_out_of_range_ports() {
        // C `envGetInetPortConfigParam` rejects port <= IPPORT_USERRESERVED
        // (5000) or > USHRT_MAX, falling back to the compiled default.
        for bad in [
            "0", "1", "80", "443", "3000", "5000", "-1", "65536", "70000", "99999",
        ] {
            set(SCRATCH_PORT, bad);
            assert_eq!(
                SCRATCH_PORT.inet_port(5064),
                5064,
                "out-of-range port {bad:?} must fall back to default"
            );
        }
        // 5001 is the first acceptable port (> IPPORT_USERRESERVED).
        set(SCRATCH_PORT, "5001");
        assert_eq!(SCRATCH_PORT.inet_port(5064), 5001);
        set(SCRATCH_PORT, "65535");
        assert_eq!(SCRATCH_PORT.inet_port(5064), 65535);
        clear(SCRATCH_PORT);
    }

    #[test]
    #[serial(epics_env)]
    fn test_get_bool_only_yes_case_insensitive() {
        // C `envGetBoolConfigParam`: true iff epicsStrCaseCmp(text,"yes")==0.
        for truthy in &["yes", "YES", "Yes", "yEs", "yeS"] {
            set(SCRATCH, truthy);
            assert_eq!(SCRATCH.bool(), Some(true), "{truthy:?} must be true");
        }
        for falsy in &[
            "1", "true", "TRUE", "on", "yes ", " yes", "yes\n", "no", "0",
        ] {
            set(SCRATCH, falsy);
            assert_eq!(SCRATCH.bool(), Some(false), "{falsy:?} must be false");
        }
        // Empty value AND empty compiled default: C's -1 status, caller's
        // initializer survives.
        set(SCRATCH, "");
        assert_eq!(SCRATCH.bool(), None);
        clear(SCRATCH);
        assert_eq!(SCRATCH.bool(), None);
    }

    /// `long_or_default` is C's `long i = <default>; envGetLongConfigParam(...)`
    /// — a rejected value leaves the compiled default standing.
    #[test]
    #[serial(epics_env)]
    fn test_long_or_default_keeps_the_table_default_on_garbage() {
        clear(IOCSH_HISTSIZE);
        assert_eq!(IOCSH_HISTSIZE.long_or_default(), 50);
        set(IOCSH_HISTSIZE, "garbage");
        assert_eq!(IOCSH_HISTSIZE.long_or_default(), 50);
        set(IOCSH_HISTSIZE, "1000");
        assert_eq!(IOCSH_HISTSIZE.long_or_default(), 1000);
        clear(IOCSH_HISTSIZE);
    }

    /// `envPrtConfigParam`: `NAME: value`, or `NAME is undefined` when the
    /// resolved value is empty. Both forms verified against compiled C
    /// (`env -i softIoc <<< epicsPrtEnvParams`).
    #[test]
    #[serial(epics_env)]
    fn describe_matches_env_prt_config_param() {
        let _silent = SilentEnv::take();
        assert_eq!(
            EPICS_CA_ADDR_LIST.describe(),
            "EPICS_CA_ADDR_LIST is undefined"
        );
        assert_eq!(
            EPICS_CA_SERVER_PORT.describe(),
            "EPICS_CA_SERVER_PORT: 5064"
        );
        set(EPICS_CA_ADDR_LIST, "10.0.0.1");
        assert_eq!(
            EPICS_CA_ADDR_LIST.describe(),
            "EPICS_CA_ADDR_LIST: 10.0.0.1"
        );
    }

    #[test]
    fn test_hostname() {
        let h = hostname();
        assert!(!h.is_empty());
    }

    /// The row-by-row diff against the BUILT C.
    ///
    /// The fixture is the verbatim stdout of
    ///
    /// ```text
    /// printf 'epicsPrtEnvParams\nexit\n' | env -i epics-base/bin/linux-x86_64/softIoc
    /// ```
    ///
    /// i.e. C walking its generated `env_param_list[]` with nothing in the
    /// environment. Every name, every position, and every compiled default is
    /// pinned by it — a dropped parameter, a reordered one, or a default that
    /// drifts from `CONFIG_ENV` all fail here.
    ///
    /// This reads the TABLE, not the process environment, so it is independent
    /// of whatever the developer's shell exports (and needs no `#[serial]`).
    /// `describe()`'s env-override path is covered by
    /// `describe_matches_env_prt_config_param`.
    #[test]
    #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
    fn env_param_list_matches_compiled_c() {
        let expected = include_str!("testdata/epicsPrtEnvParams.txt");
        let ours: Vec<String> = ENV_PARAM_LIST
            .iter()
            .map(|p| {
                if p.default_str().is_empty() {
                    format!("{} is undefined", p.name())
                } else {
                    format!("{}: {}", p.name(), p.default_str())
                }
            })
            .collect();
        let theirs: Vec<&str> = expected.lines().collect();
        assert_eq!(
            ours.len(),
            theirs.len(),
            "C's env_param_list[] has {} rows, ours has {}",
            theirs.len(),
            ours.len()
        );
        for (i, (a, b)) in ours.iter().zip(&theirs).enumerate() {
            assert_eq!(a, b, "row {i} differs from compiled C");
        }
    }

    /// The IOC shell's own environment, diffed against compiled C.
    ///
    /// Fixture: `printf 'epicsEnvShow\nexit\n' | env -i softIoc` (7.0.10.1-DEV,
    /// linux-x86_64), keeping the variables `iocshRegisterCommon` sets and
    /// dropping the two the OS supplies (`PWD`) — nine names, nine values.
    ///
    /// A `.db` or an `st.cmd` that expands `$(EPICS_VERSION_FULL)` or `$(ARCH)`
    /// resolves under C; before this it silently did not resolve here. Note
    /// `EPICS_VERSION_SITE` is *set and empty*, not absent — an empty
    /// `epicsEnvSet` value is still a variable, and `$(EPICS_VERSION_SITE)`
    /// expands to nothing rather than failing.
    #[test]
    #[serial(epics_env)]
    #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
    fn iocsh_registers_the_version_env_vars_c_does() {
        let expected = include_str!("testdata/iocshRegisterCommon.txt");
        for name in expected.lines().filter_map(|l| l.split_once('=')) {
            // SAFETY: `#[serial(epics_env)]`.
            unsafe { std::env::remove_var(name.0) };
        }

        register_iocsh_env_vars();

        for (i, line) in expected.lines().enumerate() {
            let (name, value) = line.split_once('=').expect("NAME=value");
            assert_eq!(
                std::env::var(name).ok().as_deref(),
                Some(value),
                "line {i}: C's iocshRegisterCommon publishes `{line}`"
            );
        }
    }
}
