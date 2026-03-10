/// Macro expansion engine — port of macCore.c trans()/refer() and macUtil.c macParseDefns().
///
/// Key differences from C:
/// - No value cache or dirty flag — always expand from rawval
/// - Recursion detection via explicit stack parameter instead of visited field
/// - Vec with reverse scan instead of linked list

#[derive(Debug)]
pub struct MacHandle {
    entries: Vec<MacEntry>,
    level: usize,
    suppress_warnings: bool,
    pub(crate) had_warnings: bool,
}

#[derive(Debug, Clone)]
struct MacEntry {
    name: String,
    rawval: Option<String>, // None = deleted
    level: usize,
    special: bool, // scope marker
}

impl MacHandle {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            level: 0,
            suppress_warnings: false,
            had_warnings: false,
        }
    }

    pub fn put_value(&mut self, name: &str, value: Option<&str>) {
        if value.is_none() {
            // Delete: remove all entries with this name
            self.entries.retain(|e| e.name != name);
            return;
        }
        // Check if an entry at the current level exists
        if let Some(entry) = self
            .entries
            .iter_mut()
            .rev()
            .find(|e| e.name == name && !e.special)
        {
            if entry.level == self.level {
                entry.rawval = value.map(|s| s.to_string());
                return;
            }
        }
        // Create new entry at current level
        self.entries.push(MacEntry {
            name: name.to_string(),
            rawval: value.map(|s| s.to_string()),
            level: self.level,
            special: false,
        });
    }

    pub fn push_scope(&mut self) {
        self.level += 1;
        self.entries.push(MacEntry {
            name: "<scope>".to_string(),
            rawval: None,
            level: self.level,
            special: true,
        });
    }

    pub fn pop_scope(&mut self) {
        if self.level == 0 {
            return;
        }
        // Find the last scope marker and remove it and everything after
        if let Some(pos) = self
            .entries
            .iter()
            .rposition(|e| e.special && e.name == "<scope>" && e.level == self.level)
        {
            self.entries.truncate(pos);
        }
        self.level -= 1;
    }

    pub fn suppress_warnings(&mut self, suppress: bool) {
        self.suppress_warnings = suppress;
    }

    pub fn had_warnings(&self) -> bool {
        self.had_warnings
    }

    /// Expand all macro references in `src`, returning the result string.
    /// Undefined/recursive macros are indicated in the output string;
    /// `had_warnings` is set if any were encountered.
    pub fn expand_string(&mut self, src: &str) -> String {
        let mut stack = Vec::new();
        self.trans(src, &[], 0, &mut stack)
    }

    /// Parse a "A=val,B=val2" definitions string into (name, Option<value>) pairs.
    /// Names without `=` produce None (deletion). Quotes and escapes in names are stripped.
    pub fn parse_defns(defns: &str) -> Vec<(String, Option<String>)> {
        parse_defns_inner(defns)
    }

    /// Install parsed macro definitions.
    pub fn install_macros(&mut self, defs: &[(String, Option<String>)]) {
        for (name, value) in defs {
            self.put_value(name, value.as_deref());
        }
    }

    // --- Private ---

    fn find_entry(&self, name: &str) -> Option<&MacEntry> {
        self.entries
            .iter()
            .rev()
            .find(|e| !e.special && e.name == name && e.rawval.is_some())
    }

    /// Core translation — port of macCore.c trans().
    ///
    /// `terminators`: characters that stop scanning (in addition to end-of-string).
    /// `level`: 0 = top-level user string, >0 = inside macro definition.
    /// Returns (expanded_string, index_where_we_stopped_in_src).
    fn trans(
        &mut self,
        src: &str,
        terminators: &[char],
        level: usize,
        stack: &mut Vec<String>,
    ) -> String {
        let mut result = String::new();
        let chars: Vec<char> = src.chars().collect();
        let mut i = 0;
        let discard = level > 0;
        let mut quote: Option<char> = None;

        while i < chars.len() {
            let c = chars[i];

            // Check terminators
            if terminators.contains(&c) {
                break;
            }

            // Quote handling
            if let Some(q) = quote {
                if c == q {
                    // End quote
                    quote = None;
                    if !discard {
                        result.push(c);
                    }
                    i += 1;
                    continue;
                }
                // Inside single quotes: no macro expansion
                if q == '\'' {
                    // Check for macro ref but don't expand
                    if c == '\\' && i + 1 < chars.len() {
                        if !discard {
                            result.push(c);
                        }
                        i += 1;
                        result.push(chars[i]);
                        i += 1;
                        continue;
                    }
                    result.push(c);
                    i += 1;
                    continue;
                }
                // Inside double quotes: expand macros but copy other chars
                // fall through to macro detection below
            } else {
                // Not in a quote — check for opening quote
                if c == '\'' || c == '"' {
                    quote = Some(c);
                    if !discard {
                        result.push(c);
                    }
                    i += 1;
                    continue;
                }
            }

            // Macro reference detection: $(  or ${
            if c == '$' && i + 1 < chars.len() && (chars[i + 1] == '(' || chars[i + 1] == '{') {
                // Not inside single quotes (checked above — single quote continues early)
                let (expanded, consumed) = self.refer(&chars, i, level, stack);
                result.push_str(&expanded);
                i += consumed;
                continue;
            }

            // Backslash escaping
            if c == '\\' && i + 1 < chars.len() {
                if !discard {
                    result.push(c);
                }
                i += 1;
                result.push(chars[i]);
                i += 1;
                continue;
            }

            // Normal character
            result.push(c);
            i += 1;
        }

        result
    }

    /// Translate a substring starting at `start` with given terminators.
    /// Returns (result_string, number_of_chars_consumed_from_start).
    fn trans_sub(
        &mut self,
        chars: &[char],
        start: usize,
        terminators: &[char],
        level: usize,
        stack: &mut Vec<String>,
    ) -> (String, usize) {
        let mut result = String::new();
        let mut i = start;
        let discard = level > 0;
        let mut quote: Option<char> = None;

        while i < chars.len() {
            let c = chars[i];

            if terminators.contains(&c) && quote.is_none() {
                break;
            }

            // Quote handling
            if let Some(q) = quote {
                if c == q {
                    quote = None;
                    if !discard {
                        result.push(c);
                    }
                    i += 1;
                    continue;
                }
                if q == '\'' {
                    if c == '\\' && i + 1 < chars.len() {
                        if !discard {
                            result.push(c);
                        }
                        i += 1;
                        result.push(chars[i]);
                        i += 1;
                        continue;
                    }
                    result.push(c);
                    i += 1;
                    continue;
                }
            } else {
                if c == '\'' || c == '"' {
                    quote = Some(c);
                    if !discard {
                        result.push(c);
                    }
                    i += 1;
                    continue;
                }
            }

            // Macro reference
            if c == '$' && i + 1 < chars.len() && (chars[i + 1] == '(' || chars[i + 1] == '{') {
                let (expanded, consumed) = self.refer(chars, i, level, stack);
                result.push_str(&expanded);
                i += consumed;
                continue;
            }

            // Backslash
            if c == '\\' && i + 1 < chars.len() {
                if !discard {
                    result.push(c);
                }
                i += 1;
                result.push(chars[i]);
                i += 1;
                continue;
            }

            result.push(c);
            i += 1;
        }

        (result, i - start)
    }

    /// Macro reference expander — port of macCore.c refer().
    /// `chars` is the full char array, `pos` is the index of '$'.
    /// Returns (expanded_text, number_of_chars_consumed).
    fn refer(
        &mut self,
        chars: &[char],
        pos: usize,
        level: usize,
        stack: &mut Vec<String>,
    ) -> (String, usize) {
        let open = chars[pos + 1]; // '(' or '{'
        let close = if open == '(' { ')' } else { '}' };
        let name_terminators: Vec<char> = vec!['=', ',', close];

        let mut i = pos + 2; // skip '$' and '(' or '{'

        // Step 1: Parse the macro name (may contain nested refs)
        let (refname, name_consumed) =
            self.trans_sub(chars, i, &name_terminators, level + 1, stack);
        i += name_consumed;

        // Step 2: Handle default value
        let mut defval: Option<usize> = None; // start position of default value in chars
        if i < chars.len() && chars[i] == '=' {
            i += 1; // skip '='
            defval = Some(i);
            // Scan past the default value (don't keep result)
            let def_terminators: Vec<char> = vec![',', close];
            let (_, def_consumed) =
                self.trans_sub(chars, i, &def_terminators, level + 1, stack);
            i += def_consumed;
        }

        // Step 3: Handle scoped macros
        let mut pop = false;
        if i < chars.len() && chars[i] == ',' {
            self.push_scope();
            pop = true;

            while i < chars.len() && chars[i] == ',' {
                i += 1; // skip ','
                // Parse sub-macro name
                let sub_terminators: Vec<char> = vec!['=', ',', close];
                let (subname, sub_consumed) =
                    self.trans_sub(chars, i, &sub_terminators, level + 1, stack);
                i += sub_consumed;

                if i < chars.len() && chars[i] == '=' {
                    i += 1; // skip '='
                    let val_terminators: Vec<char> = vec![',', close];
                    let (subval, val_consumed) =
                        self.trans_sub(chars, i, &val_terminators, level + 1, stack);
                    i += val_consumed;
                    self.put_value(&subname, Some(&subval));
                }
                // If no '=', just skip (name without value in scoped context)
            }
        }

        // Step 4: Skip closing delimiter
        if i < chars.len() && chars[i] == close {
            i += 1;
        }

        let consumed = i - pos;

        // Step 5: Look up and expand
        let result = if stack.contains(&refname) {
            // Recursive reference
            self.had_warnings = true;
            if self.suppress_warnings {
                format!("$({})", refname)
            } else {
                format!("$({},recursive)", refname)
            }
        } else if let Some(entry) = self.find_entry(&refname).cloned() {
            // Found — expand rawval
            let rawval = entry.rawval.as_deref().unwrap_or("");
            stack.push(refname.clone());
            let expanded = self.trans(rawval, &[], level + 1, stack);
            stack.pop();
            expanded
        } else if let Some(def_start) = defval {
            // Not found but has default value — expand the default
            let def_terminators: Vec<char> = vec![',', close];
            let (expanded, _) =
                self.trans_sub(chars, def_start, &def_terminators, level + 1, stack);
            expanded
        } else {
            // Undefined, no default
            self.had_warnings = true;
            if self.suppress_warnings {
                format!("$({})", refname)
            } else {
                format!("$({},undefined)", refname)
            }
        };

        if pop {
            self.pop_scope();
        }

        (result, consumed)
    }
}

impl Default for MacHandle {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse "A=val,B=val2" into pairs. Port of macUtil.c macParseDefns().
fn parse_defns_inner(defns: &str) -> Vec<(String, Option<String>)> {
    let mut pairs = Vec::new();
    let chars: Vec<char> = defns.chars().collect();
    let len = chars.len();
    let mut i = 0;

    loop {
        // Skip whitespace and commas (preName)
        while i < len && (chars[i].is_whitespace() || chars[i] == ',') {
            i += 1;
        }
        if i >= len {
            break;
        }

        // Read name
        let name_start = i;
        let mut quote: Option<char> = None;
        while i < len {
            let c = chars[i];
            if let Some(q) = quote {
                if c == q {
                    quote = None;
                }
                i += 1;
                continue;
            }
            if c == '\'' || c == '"' {
                quote = Some(c);
                i += 1;
                continue;
            }
            if c == '\\' && i + 1 < len {
                i += 2;
                continue;
            }
            if c == '=' || c == ',' {
                break;
            }
            i += 1;
        }
        // Trim trailing whitespace from name
        let mut name_end = i;
        while name_end > name_start && chars[name_end - 1].is_whitespace() {
            name_end -= 1;
        }
        let raw_name: String = chars[name_start..name_end].iter().collect();
        // Strip quotes and escapes from name
        let name = strip_quotes_escapes(&raw_name);

        if name.is_empty() {
            if i < len {
                i += 1; // skip delimiter
            }
            continue;
        }

        if i >= len || chars[i] == ',' {
            // Name without =value → deletion
            pairs.push((name, None));
            if i < len {
                i += 1; // skip comma
            }
            continue;
        }

        // Must be '='
        i += 1;

        // Skip whitespace before value
        while i < len && chars[i].is_whitespace() {
            i += 1;
        }

        // Read value (values keep quotes — they'll be processed by trans())
        let value_start = i;
        let mut quote: Option<char> = None;
        while i < len {
            let c = chars[i];
            if let Some(q) = quote {
                if c == q {
                    quote = None;
                }
                i += 1;
                continue;
            }
            if c == '\'' || c == '"' {
                quote = Some(c);
                i += 1;
                continue;
            }
            if c == '\\' && i + 1 < len {
                i += 2;
                continue;
            }
            if c == ',' {
                break;
            }
            i += 1;
        }
        // Trim trailing whitespace from value
        let mut value_end = i;
        while value_end > value_start && chars[value_end - 1].is_whitespace() {
            value_end -= 1;
        }
        let value: String = chars[value_start..value_end].iter().collect();
        pairs.push((name, Some(value)));

        if i < len && chars[i] == ',' {
            i += 1;
        }
    }

    pairs
}

/// Strip quote characters and backslash escapes from a name string.
fn strip_quotes_escapes(s: &str) -> String {
    let mut result = String::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' || c == '"' {
            i += 1;
            continue;
        }
        if c == '\\' && i + 1 < chars.len() {
            i += 1;
            result.push(chars[i]);
            i += 1;
            continue;
        }
        result.push(c);
        i += 1;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_expansion() {
        let mut h = MacHandle::new();
        h.put_value("NAME", Some("hello"));
        assert_eq!(h.expand_string("$(NAME)"), "hello");
        assert_eq!(h.expand_string("${NAME}"), "hello");
    }

    #[test]
    fn default_value() {
        let mut h = MacHandle::new();
        assert_eq!(h.expand_string("$(NAME=world)"), "world");
        // If defined, default is ignored
        h.put_value("NAME", Some("hello"));
        assert_eq!(h.expand_string("$(NAME=world)"), "hello");
    }

    #[test]
    fn undefined_passthrough_suppress() {
        let mut h = MacHandle::new();
        h.suppress_warnings(true);
        assert_eq!(h.expand_string("$(UNDEF)"), "$(UNDEF)");
        assert!(h.had_warnings());
    }

    #[test]
    fn undefined_warning() {
        let mut h = MacHandle::new();
        assert_eq!(h.expand_string("$(UNDEF)"), "$(UNDEF,undefined)");
        assert!(h.had_warnings());
    }

    #[test]
    fn recursive_detection() {
        let mut h = MacHandle::new();
        h.put_value("A", Some("$(A)"));
        let result = h.expand_string("$(A)");
        assert!(result.contains("recursive"));
        assert!(h.had_warnings());
    }

    #[test]
    fn recursive_suppress() {
        let mut h = MacHandle::new();
        h.suppress_warnings(true);
        h.put_value("A", Some("$(A)"));
        assert_eq!(h.expand_string("$(A)"), "$(A)");
    }

    #[test]
    fn scoped_macro() {
        let mut h = MacHandle::new();
        h.put_value("GREETING", Some("hello $(WHO)"));
        let result = h.expand_string("$(GREETING,WHO=world)");
        assert_eq!(result, "hello world");
    }

    #[test]
    fn nested_reference() {
        let mut h = MacHandle::new();
        h.put_value("SUFFIX", Some("1"));
        h.put_value("VAR1", Some("result"));
        assert_eq!(h.expand_string("$(VAR$(SUFFIX))"), "result");
    }

    #[test]
    fn single_quote_suppression() {
        let mut h = MacHandle::new();
        h.put_value("X", Some("hello"));
        assert_eq!(h.expand_string("'$(X)'"), "'$(X)'");
    }

    #[test]
    fn backslash_escape() {
        let mut h = MacHandle::new();
        h.put_value("X", Some("hello"));
        assert_eq!(h.expand_string("\\$(X)"), "\\$(X)");
    }

    #[test]
    fn scope_push_pop() {
        let mut h = MacHandle::new();
        h.put_value("A", Some("outer"));
        h.push_scope();
        h.put_value("A", Some("inner"));
        assert_eq!(h.expand_string("$(A)"), "inner");
        h.pop_scope();
        assert_eq!(h.expand_string("$(A)"), "outer");
    }

    #[test]
    fn parse_defns_basic() {
        let pairs = MacHandle::parse_defns("A=hello,B=world");
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0], ("A".to_string(), Some("hello".to_string())));
        assert_eq!(pairs[1], ("B".to_string(), Some("world".to_string())));
    }

    #[test]
    fn parse_defns_whitespace() {
        let pairs = MacHandle::parse_defns("A = hello , B = world");
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0], ("A".to_string(), Some("hello".to_string())));
        assert_eq!(pairs[1], ("B".to_string(), Some("world".to_string())));
    }

    #[test]
    fn parse_defns_deletion() {
        let pairs = MacHandle::parse_defns("A,B=val");
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0], ("A".to_string(), None));
        assert_eq!(pairs[1], ("B".to_string(), Some("val".to_string())));
    }

    #[test]
    fn parse_defns_quoted_name() {
        let pairs = MacHandle::parse_defns("\"A\"=hello");
        assert_eq!(pairs[0].0, "A");
    }

    #[test]
    fn install_macros() {
        let mut h = MacHandle::new();
        let defs = MacHandle::parse_defns("X=1,Y=2");
        h.install_macros(&defs);
        assert_eq!(h.expand_string("$(X) $(Y)"), "1 2");
    }

    #[test]
    fn multiple_refs_in_string() {
        let mut h = MacHandle::new();
        h.put_value("P", Some("IOC:"));
        h.put_value("R", Some("ai1"));
        assert_eq!(h.expand_string("$(P)$(R)"), "IOC:ai1");
    }

    #[test]
    fn mixed_text_and_macros() {
        let mut h = MacHandle::new();
        h.put_value("TYPE", Some("ai"));
        assert_eq!(
            h.expand_string("record($(TYPE), \"$(P)$(R)\")"),
            "record(ai, \"$(P,undefined)$(R,undefined)\")"
        );
    }

    #[test]
    fn empty_value() {
        let mut h = MacHandle::new();
        h.put_value("EMPTY", Some(""));
        assert_eq!(h.expand_string("before$(EMPTY)after"), "beforeafter");
    }

    #[test]
    fn transitive_expansion() {
        let mut h = MacHandle::new();
        h.put_value("A", Some("$(B)"));
        h.put_value("B", Some("final"));
        assert_eq!(h.expand_string("$(A)"), "final");
    }

    #[test]
    fn mutual_recursion() {
        let mut h = MacHandle::new();
        h.put_value("A", Some("$(B)"));
        h.put_value("B", Some("$(A)"));
        let result = h.expand_string("$(A)");
        assert!(result.contains("recursive"));
    }

    #[test]
    fn default_with_macro() {
        let mut h = MacHandle::new();
        h.put_value("Y", Some("default_val"));
        assert_eq!(h.expand_string("$(X=$(Y))"), "default_val");
    }

    #[test]
    fn dollar_without_paren() {
        let mut h = MacHandle::new();
        assert_eq!(h.expand_string("$100"), "$100");
    }

    #[test]
    fn braces_expansion() {
        let mut h = MacHandle::new();
        h.put_value("X", Some("val"));
        assert_eq!(h.expand_string("${X}"), "val");
    }

    #[test]
    fn scoped_does_not_leak() {
        let mut h = MacHandle::new();
        h.suppress_warnings(true);
        h.put_value("TMPL", Some("$(LOCAL)"));
        let result = h.expand_string("$(TMPL,LOCAL=secret)");
        assert_eq!(result, "secret");
        // LOCAL should not be defined after scoped expansion
        assert_eq!(h.expand_string("$(LOCAL)"), "$(LOCAL)");
    }

    #[test]
    fn no_expansion_plain_text() {
        let mut h = MacHandle::new();
        assert_eq!(h.expand_string("plain text"), "plain text");
    }

    #[test]
    fn delete_macro() {
        let mut h = MacHandle::new();
        h.put_value("A", Some("val"));
        assert_eq!(h.expand_string("$(A)"), "val");
        h.put_value("A", None);
        h.had_warnings = false;
        let result = h.expand_string("$(A)");
        assert!(result.contains("undefined") || result == "$(A)");
    }
}
