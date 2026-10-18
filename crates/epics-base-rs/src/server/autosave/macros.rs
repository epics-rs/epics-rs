use std::collections::HashMap;

/// Macro expansion context for `$(KEY)` and `${KEY}` patterns.
#[derive(Debug, Clone, Default)]
pub struct MacroContext {
    macros: HashMap<String, String>,
}

impl MacroContext {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_map(macros: HashMap<String, String>) -> Self {
        Self { macros }
    }

    /// Parse inline macro definitions like `"P=IOC:,M=m1"` into their RAW
    /// values — no `$(...)` expansion, matching C `macParseDefns`
    /// (`macUtil.c:74-196`), which keeps values verbatim because "unlike
    /// values, they will not be re-parsed". The caller expands each value
    /// afterwards, which is the order that keeps a comma arriving from
    /// inside a value from terminating the definition after it.
    ///
    /// The grammar comes from
    /// [`macro_defn_pairs`](crate::server::iocsh::macro_defn_pairs), the
    /// crate's one owner of it, so a quoted or escaped comma (`S='a,b'`)
    /// stays inside its value here exactly as it does for `dbLoadRecords`.
    /// A fragment with no `=` is C's macro DELETION; autosave has no
    /// delete path, so it is ignored rather than silently defining an
    /// empty macro.
    pub fn parse_inline(s: &str) -> HashMap<String, String> {
        crate::server::iocsh::macro_defn_pairs(s)
            .into_iter()
            .filter(|(name, _)| !name.is_empty())
            .filter_map(|(name, value)| value.map(|v| (name, v)))
            .collect()
    }

    /// Expand `input` as C `readReqFile` does (save_restore.c:3562-3605):
    /// with no definitions the line is used verbatim; otherwise through
    /// macLib with no environment, where an undefined name is written back
    /// as `$(name,undefined)` after macLib's own warning and the line still
    /// counts.
    pub fn expand(&self, input: &str) -> String {
        if self.macros.is_empty() {
            return input.to_string();
        }
        crate::server::db_loader::expand_macros(
            input,
            &self.macros,
            crate::server::db_loader::MacroExpandOptions {
                env_fallback: false,
                dollar_escape: false,
                suppress_warnings: false,
            },
        )
        .text
    }

    /// [`Self::expand`] without macLib's warnings, for a trace of a line
    /// whose expansion is reported elsewhere.
    pub(super) fn expand_quiet(&self, input: &str) -> String {
        if self.macros.is_empty() {
            return input.to_string();
        }
        crate::server::db_loader::expand_macros(
            input,
            &self.macros,
            crate::server::db_loader::MacroExpandOptions {
                env_fallback: false,
                dollar_escape: false,
                suppress_warnings: true,
            },
        )
        .text
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.macros.get(key).map(|s| s.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(pairs: &[(&str, &str)]) -> MacroContext {
        MacroContext::from_map(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    #[test]
    fn test_simple_expand() {
        assert_eq!(ctx(&[("P", "IOC:")]).expand("$(P)temp"), "IOC:temp");
    }

    #[test]
    fn test_default_value() {
        assert_eq!(ctx(&[("Q", "x")]).expand("$(P=DEFAULT)"), "DEFAULT");
    }

    /// No macro string, no handle: C reads the line as written.
    #[test]
    fn no_definitions_leave_the_line_verbatim() {
        assert_eq!(MacroContext::new().expand("$(P)temp"), "$(P)temp");
    }

    #[test]
    fn an_undefined_name_is_written_back_as_maclib_does() {
        assert_eq!(
            ctx(&[("Q", "x")]).expand("$(UNDEF)temp"),
            "$(UNDEF,undefined)temp"
        );
    }

    /// C's handle has no environment.
    #[test]
    fn the_environment_is_not_read() {
        let key = format!("AUTOSAVE_MACRO_ENV_{}", std::process::id());
        // SAFETY: a name no other test reads.
        unsafe { std::env::set_var(&key, "fromenv") };
        let text = ctx(&[("Q", "x")]).expand(&format!("$({key})"));
        assert_eq!(text, format!("$({key},undefined)"));
    }

    /// macLib leaves `$$` alone.
    #[test]
    fn a_doubled_dollar_is_text() {
        assert_eq!(ctx(&[("Q", "x")]).expand("$$100"), "$$100");
    }

    #[test]
    fn test_parse_inline() {
        let map = MacroContext::parse_inline("P=IOC:,M=m1");
        assert_eq!(map.get("P").unwrap(), "IOC:");
        assert_eq!(map.get("M").unwrap(), "m1");
    }

    #[test]
    fn test_both_pv_and_path() {
        let c = ctx(&[("P", "IOC:"), ("FILE", "settings")]);
        assert_eq!(c.expand("${FILE}/$(P)temp"), "settings/IOC:temp");
    }

    #[test]
    fn nested_default_is_expanded() {
        assert_eq!(
            ctx(&[("FOO", "fromfoo")]).expand("${BAR=${FOO}}"),
            "fromfoo"
        );
    }

    #[test]
    fn scoped_definition_is_honored() {
        let c = ctx(&[("INNER", "$(A)"), ("FOO", "scoped")]);
        assert_eq!(c.expand("$(INNER,A=$(FOO))"), "scoped");
    }

    #[test]
    fn resolved_value_is_chained() {
        let c = ctx(&[("P", "$(Q)"), ("Q", "IOC:")]);
        assert_eq!(c.expand("$(P)TEMP"), "IOC:TEMP");
    }
}
