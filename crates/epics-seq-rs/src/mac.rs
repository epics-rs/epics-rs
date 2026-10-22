//! Program macros (`seq_mac.c`).

use crate::pvt::Program;
use crate::var::ProgramVars;

/// A program instance's macro list: the program's `params`, then `seq`'s
/// macro string, as `seqMacParse` builds it. A name without `=` has no
/// value.
#[derive(Debug, Default)]
pub struct SeqMacros(pub Vec<(String, Option<String>)>);

/// `seqMacEval`'s name buffer: a longer name keeps its first 49 bytes.
const MAX_NAME: usize = 49;

impl SeqMacros {
    /// `seqMacParse` of `params`, then of `macro_def`.
    pub fn new(params: &str, macro_def: &str) -> Self {
        let mut macros = Self::default();
        macros.parse(params);
        macros.parse(macro_def);
        macros
    }

    /// `seqMacParse`: add the `name=value` pairs of `mac_str`.
    ///
    /// A value ends at a blank or a comma, and parsing stops at the first
    /// thing that is neither a name nor a separator. A name given without
    /// a value keeps the value it had.
    pub fn parse(&mut self, mac_str: &str) {
        fn blanks(s: &str) -> &str {
            s.trim_start_matches(' ')
        }
        let mut s = mac_str;
        while !s.is_empty() {
            s = blanks(s);
            // `seqMacParseName`
            if !s.starts_with(|c: char| c.is_ascii_alphabetic()) {
                break;
            }
            let len = s
                .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .unwrap_or(s.len());
            let (name, rest) = s.split_at(len);
            let at = match self.0.iter().position(|(n, _)| n == name) {
                Some(at) => at,
                None => {
                    self.0.push((name.to_string(), None));
                    self.0.len() - 1
                }
            };
            s = blanks(rest);
            if let Some(rest) = s.strip_prefix(',') {
                s = rest;
                continue;
            }
            let Some(rest) = s.strip_prefix('=') else {
                break;
            };
            s = blanks(rest);
            // `seqMacParseValue`
            let len = s.find([' ', ',']).unwrap_or(s.len());
            let (value, rest) = s.split_at(len);
            self.0[at].1 = Some(value.to_string());
            let Some(rest) = blanks(rest).strip_prefix(',') else {
                break;
            };
            s = rest;
        }
    }

    /// `seqMacValGet`: `None` when `name` is undefined or has no value.
    pub fn value(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(n, _)| n == name)
            .and_then(|(_, v)| v.as_deref())
    }

    /// [`Program::mac_eval`] on this list, for `seq`'s channel names before
    /// the program exists.
    pub(crate) fn eval(&self, input: &str, max_char: usize) -> String {
        let max_len = max_char.saturating_sub(1);
        let mut input = input.as_bytes();
        let mut out = Vec::new();
        while let Some((&c, rest)) = input.split_first() {
            if out.len() >= max_len {
                break;
            }
            input = rest;
            if c != b'{' {
                out.push(c);
                continue;
            }
            let end = input.iter().position(|&b| b == b'}');
            let name = &input[..end.unwrap_or(input.len())];
            input = end.map_or(&[][..], |end| &input[end + 1..]);
            let name = String::from_utf8_lossy(&name[..name.len().min(MAX_NAME)]);
            if let Some(value) = self.value(&name) {
                let room = max_len - out.len();
                out.extend_from_slice(&value.as_bytes()[..value.len().min(room)]);
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }
}

impl<V: ProgramVars> Program<V> {
    /// `seqMacEval`: `s` with each `{name}` replaced by its value, cut to
    /// `max_char - 1` bytes so it fits a `max_char` buffer with its NUL.
    /// C writes up to `max_char` bytes and the NUL one past the buffer, an
    /// overflow not copied.
    pub fn mac_eval(&self, s: &str, max_char: usize) -> String {
        self.macros.eval(s, max_char)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_line_overrides_the_program_parameters() {
        let m = SeqMacros::new("P=xxx:, M=m9, Y_OFF=35", "P=a:,Y_OFF=40");
        assert_eq!(m.value("P"), Some("a:"));
        assert_eq!(m.value("M"), Some("m9"));
        assert_eq!(m.value("Y_OFF"), Some("40"));
        assert_eq!(m.value("Q"), None);
    }

    #[test]
    fn a_name_without_a_value_keeps_the_one_it_had() {
        let m = SeqMacros::new("A=1", "A, B");
        assert_eq!(m.value("A"), Some("1"));
        assert_eq!(m.value("B"), None);
    }

    #[test]
    fn an_empty_value_is_a_value() {
        let m = SeqMacros::new("P=xxx:", "P=,H= ");
        assert_eq!(m.value("P"), Some(""));
        assert_eq!(m.value("H"), Some(""));
    }

    /// A value ends at a blank; what follows it is not a comma, so
    /// `seqMacParse` stops there.
    #[test]
    fn parsing_stops_where_the_grammar_breaks() {
        let m = SeqMacros::new("", "P=a b,Q=1");
        assert_eq!(m.value("P"), Some("a"));
        assert_eq!(m.value("Q"), None);
        let m = SeqMacros::new("", "_X=1,Y=2");
        assert_eq!(m.value("_X"), None);
        assert_eq!(m.value("Y"), None);
        let m = SeqMacros::new("", "X:1,Y=2");
        assert_eq!(m.value("Y"), None);
        let m = SeqMacros::new("", " A = 1 , B_2=x=y");
        assert_eq!(m.value("A"), Some("1"));
        assert_eq!(m.value("B_2"), Some("x=y"));
    }

    #[test]
    fn eval_substitutes_each_braced_name() {
        let m = SeqMacros::new("name=delayDo,P=xxx:,R=delayDo1", "P=ioc:");
        assert_eq!(m.eval("{P}{R}:enable.VAL", 100), "ioc:delayDo1:enable.VAL");
    }

    /// `seqMacValGet` answers NULL for both, and `seqMacEval` then copies
    /// nothing for the reference.
    #[test]
    fn eval_drops_a_name_that_is_undefined_or_has_no_value() {
        let m = SeqMacros::new("A", "");
        assert_eq!(m.eval("x{A}y{B}z", 100), "xyz");
    }

    /// The name runs to the end of the input; nothing follows it.
    #[test]
    fn eval_takes_an_unclosed_brace_to_the_end() {
        let m = SeqMacros::new("P=p", "");
        assert_eq!(m.eval("a{P", 100), "ap");
        assert_eq!(m.eval("a{Q", 100), "a");
    }

    #[test]
    fn eval_looks_a_long_name_up_by_its_first_49_bytes() {
        let name = "N".repeat(49);
        let m = SeqMacros::new(&format!("{name}=v"), "");
        assert_eq!(m.eval(&format!("<{{{name}XYZ}}>"), 100), "<v>");
        let short = SeqMacros::new(&format!("{}=v", &name[1..]), "");
        assert_eq!(short.eval(&format!("<{{{name}}}>"), 100), "<>");
    }

    /// `init_chan`'s 100-byte buffer holds 99 bytes and the NUL, whether
    /// the cut falls in a literal or in a value.
    #[test]
    fn eval_keeps_one_byte_less_than_max_char() {
        let m = SeqMacros::new(&format!("V={}", "v".repeat(120)), "");
        assert_eq!(m.eval("ab{V}", 100), format!("ab{}", "v".repeat(97)));
        assert_eq!(m.eval(&"x".repeat(120), 100), "x".repeat(99));
        assert_eq!(
            m.eval(&format!("{}{{V}}", "x".repeat(98)), 100),
            format!("{}v", "x".repeat(98))
        );
        assert_eq!(m.eval(&"x".repeat(99), 100), "x".repeat(99));
        assert_eq!(m.eval("x", 1), "");
    }
}
