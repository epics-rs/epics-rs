use std::collections::HashMap;

use super::error::{AutosaveError, AutosaveResult};

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

    /// Parse inline macro definitions like `"P=IOC:,M=m1"`.
    pub fn parse_inline(s: &str) -> HashMap<String, String> {
        let mut map = HashMap::new();
        if s.trim().is_empty() {
            return map;
        }
        for pair in s.split(',') {
            let pair = pair.trim();
            if let Some(eq_pos) = pair.find('=') {
                let key = pair[..eq_pos].trim().to_string();
                let val = pair[eq_pos + 1..].trim().to_string();
                if !key.is_empty() {
                    map.insert(key, val);
                }
            }
        }
        map
    }

    /// Create a child context by merging additional macros (child overrides parent).
    pub fn with_overrides(&self, overrides: &HashMap<String, String>) -> Self {
        let mut merged = self.macros.clone();
        merged.extend(overrides.iter().map(|(k, v)| (k.clone(), v.clone())));
        Self { macros: merged }
    }

    /// Expand all `$(KEY)`, `${KEY}`, and `$(KEY=default)` patterns in `input`.
    /// `$$` is treated as a literal `$`.
    /// Returns error on undefined macro with no default.
    pub fn expand(&self, input: &str, source: &str, line: usize) -> AutosaveResult<String> {
        let mut result = String::with_capacity(input.len());
        let bytes = input.as_bytes();
        let len = bytes.len();
        let mut i = 0;

        while i < len {
            if bytes[i] == b'$' && i + 1 < len {
                if bytes[i + 1] == b'$' {
                    // $$ → literal $
                    result.push('$');
                    i += 2;
                    continue;
                }
                let (open, close) = if bytes[i + 1] == b'(' {
                    (b'(', b')')
                } else if bytes[i + 1] == b'{' {
                    (b'{', b'}')
                } else {
                    result.push('$');
                    i += 1;
                    continue;
                };

                // Find closing delimiter
                let start = i + 2;
                let mut depth = 1u32;
                let mut j = start;
                while j < len && depth > 0 {
                    if bytes[j] == open {
                        depth += 1;
                    } else if bytes[j] == close {
                        depth -= 1;
                    }
                    if depth > 0 {
                        j += 1;
                    }
                }

                if depth != 0 {
                    // Unmatched — pass through literally
                    result.push('$');
                    i += 1;
                    continue;
                }

                let inner = &input[start..j];
                // Check for default: $(KEY=default)
                let (key, default) = if let Some(eq_pos) = inner.find('=') {
                    (&inner[..eq_pos], Some(&inner[eq_pos + 1..]))
                } else {
                    (inner, None)
                };

                if let Some(val) = self.macros.get(key) {
                    result.push_str(val);
                } else if let Some(val) = crate::runtime::env::get(key) {
                    // Fall back to environment variable (matches C macEnvExpand)
                    result.push_str(&val);
                } else if let Some(def) = default {
                    result.push_str(def);
                } else {
                    return Err(AutosaveError::UndefinedMacro {
                        key: key.to_string(),
                        source: source.to_string(),
                        line,
                    });
                }

                i = j + 1; // skip closing delimiter
            } else {
                result.push(input[i..].chars().next().unwrap());
                i += input[i..].chars().next().unwrap().len_utf8();
            }
        }

        Ok(result)
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
}
