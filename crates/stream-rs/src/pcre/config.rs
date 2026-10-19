//! `pcre_config.c` for this build.

use super::internal::*;

/// A `pcre_config` answer: C writes an int, an unsigned long or a string
/// pointer depending on `what`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigValue {
    Int(i32),
    Long(u64),
    /// PCRE_CONFIG_JITTARGET: NULL without JIT.
    Str(Option<&'static str>),
}

/// `pcre_config(what, where)`: `Err(PCRE_ERROR_BADOPTION)` for an unknown
/// request, and for UTF16/UTF32 in the 8-bit library (which still store 0).
pub fn config(what: i32) -> Result<ConfigValue, (i32, ConfigValue)> {
    use ConfigValue::*;
    Ok(match what {
        PCRE_CONFIG_UTF8 => Int(0),
        PCRE_CONFIG_UTF16 | PCRE_CONFIG_UTF32 => return Err((PCRE_ERROR_BADOPTION, Int(0))),
        PCRE_CONFIG_UNICODE_PROPERTIES => Int(0),
        PCRE_CONFIG_JIT => Int(0),
        PCRE_CONFIG_JITTARGET => Str(None),
        PCRE_CONFIG_NEWLINE => Int(NEWLINE),
        PCRE_CONFIG_BSR => Int(0),
        PCRE_CONFIG_LINK_SIZE => Int(LINK_SIZE as i32),
        PCRE_CONFIG_POSIX_MALLOC_THRESHOLD => Int(POSIX_MALLOC_THRESHOLD),
        PCRE_CONFIG_PARENS_LIMIT => Long(PARENS_NEST_LIMIT as u64),
        PCRE_CONFIG_MATCH_LIMIT => Long(MATCH_LIMIT),
        PCRE_CONFIG_MATCH_LIMIT_RECURSION => Long(MATCH_LIMIT_RECURSION),
        PCRE_CONFIG_STACKRECURSE => Int(1),
        _ => return Err((PCRE_ERROR_BADOPTION, Int(0))),
    })
}
