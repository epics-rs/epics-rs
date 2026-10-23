use std::collections::HashMap;
use std::sync::Mutex;

use crate::server::db_loader::{MacroDefs, MacroExpandOptions, MacroTable};

/// C's `MAC_HANDLE` for one `readReqFile`, `do_manual_restore` or
/// `reboot_restore` call: one table every line is expanded through, so a
/// faulty definition is reported once per call rather than once per line.
#[derive(Default)]
pub struct MacroContext {
    /// The definitions, in C's `macInstallMacros` order.
    defs: MacroDefs,
    /// C `handle`, `None` for a call with no macro string, whose lines are
    /// read as written.
    table: Option<Mutex<MacroTable>>,
}

impl std::fmt::Debug for MacroContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MacroContext")
            .field("defs", &self.defs)
            .field("handle", &self.table.is_some())
            .finish()
    }
}

/// C `macCreateHandle(&handle, NULL)`: no environment, warnings on.
const OPTS: MacroExpandOptions = MacroExpandOptions {
    env_fallback: false,
    dollar_escape: false,
    suppress_warnings: false,
};

impl MacroContext {
    pub fn new() -> Self {
        Self::default()
    }

    /// A handle holding `macros`, or none when there are none.
    pub fn from_map(macros: HashMap<String, String>) -> Self {
        let handle = !macros.is_empty();
        Self::from_defs((&macros).into(), handle)
    }

    /// C `if (macrostring && macrostring[0])` `macCreateHandle` +
    /// `macParseDefns` + `macInstallMacros`.
    pub fn from_macrostring(macrostring: &str) -> Self {
        Self::from_defs(Self::parse_defs(macrostring), !macrostring.is_empty())
    }

    /// `defs` installed in a handle, or no handle.
    pub fn from_defs(defs: MacroDefs, handle: bool) -> Self {
        let table = handle.then(|| Mutex::new(MacroTable::new(&defs, OPTS)));
        Self { defs, table }
    }

    /// [`Self::parse_inline`] in definition order.
    pub fn parse_defs(s: &str) -> MacroDefs {
        Self::defn_pairs(s).collect()
    }

    /// `s`'s definitions in order, as [`Self::parse_inline`] keeps them.
    pub(super) fn defn_pairs(s: &str) -> impl Iterator<Item = (String, String)> {
        crate::server::iocsh::macro_defn_pairs(s)
            .into_iter()
            .filter(|(name, _)| !name.is_empty())
            .filter_map(|(name, value)| value.map(|v| (name, v)))
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
        Self::defn_pairs(s).collect()
    }

    /// Whether there is a handle, which C expands each line through.
    pub fn has_handle(&self) -> bool {
        self.table.is_some()
    }

    /// C `macExpandString(handle, line, ...)` (save_restore.c:3601): with
    /// no handle the line is used verbatim; otherwise an undefined name is
    /// written back as `$(name,undefined)` after macLib's own warning and
    /// the line still counts.
    pub fn expand(&self, input: &str) -> String {
        match &self.table {
            Some(table) => table.lock().unwrap().expand(input).text,
            None => input.to_string(),
        }
    }

    /// [`Self::expand`]'s text for part of a line whose own expansion
    /// already reported it, leaving the handle as that expansion did.
    pub(super) fn expand_part(&self, input: &str) -> String {
        if self.table.is_none() {
            return input.to_string();
        }
        MacroTable::new(&self.defs, OPTS)
            .expand_deferred(input)
            .0
            .text
    }

    /// The raw value `macros` defines for `key`.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.defs
            .iter()
            .find(|(name, _)| *name == key.as_bytes())
            .and_then(|(_, value)| std::str::from_utf8(value).ok())
    }

    /// C `macGetValue(handle, name, ...) > 0` (save_restore.c:3689-3699):
    /// `name`'s value, when the handle defines it and it expands to text
    /// with no fault.
    pub fn get_value(&self, name: &str) -> Option<String> {
        let table = self.table.as_ref()?;
        if !self.defs.iter().any(|(n, _)| n == name.as_bytes()) {
            return None;
        }
        let out = table.lock().unwrap().expand(&format!("$({name})"));
        (!out.errored() && !out.text.is_empty()).then_some(out.text)
    }

    /// C `macReportMacros(handle)` (`macCore.c:472-504`): the handle's
    /// expansion pass, then each definition with its expanded value, `*`
    /// marking one that expanded wrong.
    pub(super) fn report(&self) {
        let Some(table) = &self.table else {
            return;
        };
        let mut table = table.lock().unwrap();
        let _ = table.expand("");
        let row = |e: &str, name: &str, raw: &str, value: &str| {
            println!("{e:<1} {name:<16} {raw:<16} {value}");
        };
        row("e", "name", "rawval", "value");
        row("-", "----", "------", "-----");
        for (name, raw) in self.defs.iter() {
            let name = String::from_utf8_lossy(name);
            let (value, _) = table.expand_deferred(&format!("$({name})"));
            let e = if value.errored() { "*" } else { " " };
            row(e, &name, &String::from_utf8_lossy(raw), &value.text);
        }
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
    fn get_value_is_a_defined_name_expanding_cleanly_to_text() {
        let c = ctx(&[
            ("P", "N:"),
            ("E", ""),
            ("X", "$(P)x"),
            ("Y", "$(E)"),
            ("Z", "$(Q)z"),
        ]);
        assert_eq!(c.get_value("X").as_deref(), Some("N:x"));
        assert_eq!(c.get_value("Y"), None);
        assert_eq!(c.get_value("Z"), None);
        assert_eq!(c.get_value("Q"), None);
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
