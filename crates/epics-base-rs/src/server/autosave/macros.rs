use std::collections::HashMap;

use super::error::{AutosaveError, AutosaveResult};
use crate::server::db_loader::MacroFault;

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

    /// Create a child context by merging additional macros (child overrides parent).
    pub fn with_overrides(&self, overrides: &HashMap<String, String>) -> Self {
        let mut merged = self.macros.clone();
        merged.extend(overrides.iter().map(|(k, v)| (k.clone(), v.clone())));
        Self { macros: merged }
    }

    /// Expand `$(...)` / `${...}` macro references in `input` using the
    /// crate's single macLib engine
    /// ([`crate::server::db_loader::expand_macros`]), so autosave `.req`
    /// expansion gets the full language — nested defaults
    /// (`$(BAR=$(FOO))`), scoped definitions (`$(X,X=$(Y))`),
    /// name-of-name (`$($(WHICH))`), chained expansion, single-quote
    /// suppression — not just the flat `$(KEY)` / `$(KEY=default)` subset.
    ///
    /// Autosave-specific options: `env_fallback` (C `macEnvExpand`) and
    /// `dollar_escape` (`$$` → literal `$`, a `.req` convenience).
    ///
    /// Every fault the engine can record is a hard error here — an
    /// undefined macro with no default, a macro that resolves into
    /// itself, and a reference whose closing delimiter never arrived —
    /// because all three leave text where the `.req` wanted a PV name,
    /// and a `.req` line built from a placeholder (or from a raw
    /// passed-through `$(`) names a PV that does not exist. Which one it
    /// was is read from
    /// [`MacroExpansion::fault`], never from one of its lists, so a new
    /// fault arm cannot be silently accepted here.
    ///
    /// [`MacroExpansion::fault`]: crate::server::db_loader::MacroExpansion::fault
    pub fn expand(&self, input: &str, source: &str, line: usize) -> AutosaveResult<String> {
        let result = crate::server::db_loader::expand_macros(
            input,
            &self.macros,
            crate::server::db_loader::MacroExpandOptions {
                env_fallback: true,
                dollar_escape: true,
                // Not a base C path, and this caller turns the first
                // fault into an error rather than accepting the
                // placeholder — so macLib's own warning would be noise
                // printed just before a hard failure that names the same
                // macro.
                suppress_warnings: true,
            },
        );
        let fault = result.fault().map(|fault| match fault {
            MacroFault::Undefined(key) => AutosaveError::UndefinedMacro {
                key: key.to_string(),
                source: source.to_string(),
                line,
            },
            MacroFault::Recursive(key) => AutosaveError::RecursiveMacro {
                key: key.to_string(),
                source: source.to_string(),
                line,
            },
            MacroFault::Unterminated(reference) => AutosaveError::UnterminatedMacro {
                reference: reference.to_string(),
                source: source.to_string(),
                line,
            },
        });
        match fault {
            Some(err) => Err(err),
            None => Ok(result.text),
        }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.macros.get(key).map(|s| s.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_expand() {
        let ctx = MacroContext::from_map([("P".into(), "IOC:".into())].into());
        assert_eq!(ctx.expand("$(P)temp", "test", 1).unwrap(), "IOC:temp");
    }

    #[test]
    fn test_default_value() {
        let ctx = MacroContext::new();
        assert_eq!(ctx.expand("$(P=DEFAULT)", "test", 1).unwrap(), "DEFAULT");
    }

    #[test]
    fn test_undefined_error() {
        let ctx = MacroContext::new();
        let err = ctx.expand("$(UNDEF)", "test.req", 5).unwrap_err();
        match err {
            AutosaveError::UndefinedMacro { key, source, line } => {
                assert_eq!(key, "UNDEF");
                assert_eq!(source, "test.req");
                assert_eq!(line, 5);
            }
            _ => panic!("expected UndefinedMacro"),
        }
    }

    #[test]
    fn test_parse_inline() {
        let map = MacroContext::parse_inline("P=IOC:,M=m1");
        assert_eq!(map.get("P").unwrap(), "IOC:");
        assert_eq!(map.get("M").unwrap(), "m1");
    }

    #[test]
    fn test_dollar_literal() {
        let ctx = MacroContext::new();
        assert_eq!(ctx.expand("$$100", "test", 1).unwrap(), "$100");
    }

    #[test]
    fn test_both_pv_and_path() {
        let ctx = MacroContext::from_map(
            [
                ("P".into(), "IOC:".into()),
                ("FILE".into(), "settings".into()),
            ]
            .into(),
        );
        assert_eq!(
            ctx.expand("${FILE}/$(P)temp", "test", 1).unwrap(),
            "settings/IOC:temp"
        );
    }

    // The full macLib language now reaches autosave .req expansion via
    // the shared engine — previously `expand` only did the flat
    // `$(KEY)` / `$(KEY=default)` subset.

    #[test]
    fn nested_default_is_expanded() {
        // `${BAR=${FOO}}`: BAR unset, default is itself a macro ref.
        let ctx = MacroContext::from_map([("FOO".into(), "fromfoo".into())].into());
        assert_eq!(ctx.expand("${BAR=${FOO}}", "test", 1).unwrap(), "fromfoo");
    }

    #[test]
    fn scoped_definition_is_honored() {
        // `$(INNER,A=$(FOO))`: A is defined only for this reference and
        // its value is itself expanded.
        let ctx = MacroContext::from_map(
            [
                ("INNER".into(), "$(A)".into()),
                ("FOO".into(), "scoped".into()),
            ]
            .into(),
        );
        assert_eq!(
            ctx.expand("$(INNER,A=$(FOO))", "test", 1).unwrap(),
            "scoped"
        );
    }

    #[test]
    fn resolved_value_is_chained() {
        // P=$(Q), Q=IOC: → $(P) expands through to IOC:.
        let ctx = MacroContext::from_map(
            [("P".into(), "$(Q)".into()), ("Q".into(), "IOC:".into())].into(),
        );
        assert_eq!(ctx.expand("$(P)TEMP", "test", 1).unwrap(), "IOC:TEMP");
    }

    #[test]
    fn undefined_without_default_still_errors() {
        let ctx = MacroContext::new();
        let err = ctx.expand("$(NOPE)", "f.req", 7).unwrap_err();
        match err {
            AutosaveError::UndefinedMacro { key, source, line } => {
                assert_eq!(key, "NOPE");
                assert_eq!(source, "f.req");
                assert_eq!(line, 7);
            }
            other => panic!("expected UndefinedMacro, got {other:?}"),
        }
    }

    /// A `.req` whose macros resolve into one another expanded to the
    /// recursion placeholder and returned `Ok`, because `expand` read
    /// the undefined list and a recursive name is never in it. The
    /// caller then built a PV name out of the placeholder.
    #[test]
    fn recursive_macro_is_an_error_not_placeholder_text() {
        let ctx = MacroContext::from_map(
            [("A".into(), "$(B)".into()), ("B".into(), "$(A)".into())].into(),
        );
        let err = ctx.expand("$(A)", "cycle.req", 7).unwrap_err();
        match err {
            AutosaveError::RecursiveMacro { key, source, line } => {
                assert_eq!(key, "A");
                assert_eq!(source, "cycle.req");
                assert_eq!(line, 7);
            }
            other => panic!("expected RecursiveMacro, got {other:?}"),
        }
    }
}
