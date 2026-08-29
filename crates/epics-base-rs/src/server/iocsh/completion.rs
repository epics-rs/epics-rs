// RTEMS-EXEC-MODEL-ALLOW(1): a sync test that hand-builds its own tokio runtime; runs and passes in the exec-backend suite.
//! iocsh TAB completion — C `iocsh_attempt_completion` (`iocsh.cpp:502-603`).
//!
//! C installs one readline completion hook and dispatches on the
//! *declared type* of the argument the cursor sits in
//! (`iocsh.cpp:574-598`):
//!
//! * argument 0 (the command name) → the command table
//!   (`iocsh_complete_command`, `iocsh.cpp:456-477`),
//! * [`ArgType::Record`] → `iocshCompleteRecord`, which the database
//!   layer points at `dbCompleteRecord` (`dbIocRegister.c:585`),
//! * [`ArgType::Path`] → readline's default filesystem completion
//!   (C clears `rl_attempted_completion_over`, `iocsh.cpp:582-584`),
//! * `help` / `var` at argument 1 → the command and variable tables
//!   (`iocsh.cpp:592-596`),
//! * anything else → no candidates at all; C sets
//!   `rl_attempted_completion_over = 1` up front (`iocsh.cpp:577`) so an
//!   unclassified argument does *not* fall back to filenames.
//!
//! Word boundaries are C's, not readline's defaults: `setup()` sets
//! `rl_completer_word_break_characters` to `"\t (),"` and
//! `rl_completer_quote_characters` to `"\""` (`iocsh.cpp:635-638`).
//!
//! One deliberate divergence: C's own comment at `iocsh.cpp:550-551`
//! marks its argument-index search as a BUG, because it re-splits a
//! line `Tokenize::split` has already rewritten in place and so cannot
//! locate the cursor inside `dbpr("X")`. This scanner works on the
//! untouched line, so the quoted-parenthesised form completes here.

use std::sync::{Arc, RwLock};

use rustyline::completion::{Completer, Pair};
use rustyline::{Context, Helper};

use super::registry::{ArgType, CommandRegistry};
use crate::runtime::task::BlockingBridge;
use crate::server::database::PvDatabase;

/// C `rl_completer_word_break_characters` for iocsh (`iocsh.cpp:636`).
const BREAK_CHARS: [char; 5] = ['\t', ' ', '(', ')', ','];
/// C `rl_completer_quote_characters` for iocsh (`iocsh.cpp:638`).
const QUOTE_CHAR: char = '"';

/// The words that precede the one under the cursor, plus the byte offset
/// where that in-progress word starts.
///
/// `words[0]` is the command name when it is complete; an empty `words`
/// means the cursor is still inside the command name, which is C's
/// `start == 0` arm (`iocsh.cpp:517-518`).
fn scan_to_cursor(line: &str, pos: usize) -> (Vec<&str>, usize) {
    let head = &line[..pos.min(line.len())];
    let mut words = Vec::new();
    let mut start = 0usize;
    let mut quoted = false;
    for (i, ch) in head.char_indices() {
        if ch == QUOTE_CHAR {
            if quoted {
                // A closing quote ends the word with it.
                words.push(&head[start..i]);
                start = i + ch.len_utf8();
            } else {
                // An opening quote is not part of the word's text.
                start = i + ch.len_utf8();
            }
            quoted = !quoted;
        } else if !quoted && BREAK_CHARS.contains(&ch) {
            if start < i {
                words.push(&head[start..i]);
            }
            start = i + ch.len_utf8();
        }
    }
    (words, start)
}

/// C `dbCompleteRecord` (`dbCompleteRecord.cpp:90-162`): find the longest
/// common prefix of every record name `word` is a prefix of, then offer
/// each matching name chopped at the first `:<>{}-` at or after that
/// prefix (the separator included). Long hierarchical names therefore
/// complete one path element at a time instead of dumping the whole
/// database.
///
/// C returns `[prefix, suggestions...]` because readline takes element 0
/// as the substitution; rustyline derives the same substitution itself
/// with `longest_common_prefix`, so only the suggestions are returned.
pub(crate) fn complete_record(names: &[String], word: &str) -> Vec<String> {
    let mut prefix: Option<&str> = None;
    for name in names.iter().filter(|n| n.starts_with(word)) {
        prefix = Some(match prefix {
            None => name.as_str(),
            Some(p) => {
                let end = p
                    .char_indices()
                    .zip(name.char_indices())
                    .skip_while(|((i, a), (_, b))| a == b || *i < word.len())
                    .map(|((i, _), _)| i)
                    .next()
                    .unwrap_or_else(|| p.len().min(name.len()));
                &p[..end]
            }
        });
    }
    let Some(prefix) = prefix else {
        return Vec::new();
    };

    let mut out: Vec<String> = names
        .iter()
        .filter(|n| n.starts_with(prefix))
        .map(|n| {
            let cut = n[prefix.len()..]
                .find([':', '<', '>', '{', '}', '-'])
                .map(|off| prefix.len() + off + 1);
            n[..cut.unwrap_or(n.len())].to_string()
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Directory listing in readline's default filesystem style: split the
/// word at its last `/`, list that directory, keep the entries the tail
/// is a prefix of, and mark directories with a trailing `/`.
fn complete_path(word: &str) -> Vec<String> {
    let (dir, tail) = match word.rfind('/') {
        Some(i) => (&word[..=i], &word[i + 1..]),
        None => ("", word),
    };
    let listing = if dir.is_empty() { "." } else { dir };
    let Ok(entries) = std::fs::read_dir(listing) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            if !name.starts_with(tail) {
                return None;
            }
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            Some(format!("{dir}{name}{}", if is_dir { "/" } else { "" }))
        })
        .collect();
    out.sort();
    out
}

/// The completion half of the interactive shell: everything C's
/// `iocsh_attempt_completion` reaches, as owned handles so the
/// rustyline editor can hold it while the shell runs.
pub(crate) struct IocshCompleter {
    registry: Arc<RwLock<CommandRegistry>>,
    db: Arc<PvDatabase>,
    bridge: BlockingBridge,
}

impl IocshCompleter {
    pub(crate) fn new(
        registry: Arc<RwLock<CommandRegistry>>,
        db: Arc<PvDatabase>,
        bridge: BlockingBridge,
    ) -> Self {
        Self {
            registry,
            db,
            bridge,
        }
    }

    /// Every name `dbCompleteRecord` walks: `dbFirstRecord` /
    /// `dbNextRecord` iterate `precordType->recList`
    /// (`dbStaticLib.c:1584-1609`), which holds the alias nodes too.
    fn record_names(&self) -> Vec<String> {
        let mut names = self.bridge.block_on(self.db.all_record_names());
        names.extend(self.db.all_alias_names());
        names
    }

    fn candidates(&self, line: &str, pos: usize) -> (usize, Vec<String>) {
        let (words, start) = scan_to_cursor(line, pos);
        let word = &line[start..pos.min(line.len())];

        let Some(cmd_name) = words.first() else {
            let registry = self.registry.read().unwrap();
            let names = registry
                .list()
                .into_iter()
                .filter(|n| n.starts_with(word))
                .map(str::to_string)
                .collect();
            return (start, names);
        };

        // C's `arg` is the argv index, so argv[1] is `arg[0]`
        // (`iocsh.cpp:574`).
        let index = words.len() - 1;
        let arg_type = {
            let registry = self.registry.read().unwrap();
            match registry.get(cmd_name) {
                Some(def) => def.args.get(index).map(|a| a.arg_type.clone()),
                None => return (start, Vec::new()),
            }
        };

        match arg_type {
            Some(ArgType::Record) => (start, complete_record(&self.record_names(), word)),
            Some(ArgType::Path) => (start, complete_path(word)),
            // C `iocsh.cpp:592-596`: `help` and `var` name a command and
            // a variable in their first argument, which no argument type
            // can express, so C matches them by command name.
            _ if index == 0 && *cmd_name == "help" => {
                let registry = self.registry.read().unwrap();
                let names = registry
                    .list()
                    .into_iter()
                    .filter(|n| n.starts_with(word))
                    .map(str::to_string)
                    .collect();
                (start, names)
            }
            _ if index == 0 && *cmd_name == "var" => (
                start,
                super::vars::variable_names()
                    .into_iter()
                    .filter(|n| n.starts_with(word))
                    .map(str::to_string)
                    .collect(),
            ),
            _ => (start, Vec::new()),
        }
    }
}

impl Completer for IocshCompleter {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let (start, names) = self.candidates(line, pos);
        Ok((
            start,
            names
                .into_iter()
                .map(|n| Pair {
                    display: n.clone(),
                    replacement: n,
                })
                .collect(),
        ))
    }
}

impl rustyline::hint::Hinter for IocshCompleter {
    type Hint = String;
}
impl rustyline::highlight::Highlighter for IocshCompleter {}
impl rustyline::validate::Validator for IocshCompleter {}
impl Helper for IocshCompleter {}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn completer() -> IocshCompleter {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let bridge = {
            let _guard = rt.enter();
            BlockingBridge::capture()
        };
        std::mem::forget(rt);
        let mut registry = CommandRegistry::new();
        super::super::commands::register_builtins(&mut registry);
        IocshCompleter::new(
            Arc::new(RwLock::new(registry)),
            Arc::new(PvDatabase::new()),
            bridge,
        )
    }

    /// The classification the completer dispatches on. Each entry is the
    /// C declaration the port is copying; a plain `String` here would
    /// silently disable completion for that argument.
    #[test]
    fn the_declared_argument_types_match_cs() {
        let mut registry = CommandRegistry::new();
        super::super::commands::register_builtins(&mut registry);
        for (cmd, index, want) in [
            ("dbgf", 0, "Record"),
            ("dbpf", 0, "Record"),
            ("dbpr", 0, "Record"),
            ("dbglob", 0, "Record"),
            ("dbgrep", 0, "Record"),
            ("astac", 0, "Record"),
            ("dbDeleteRecord", 0, "Record"),
            ("dbLoadRecords", 0, "Path"),
            ("dbLoadTemplate", 0, "Path"),
            ("asSetFilename", 0, "Path"),
            ("cd", 0, "Path"),
            ("pushd", 0, "Path"),
            // C `dblArg0` is a plain `iocshArgString`
            // (`dbIocRegister.c:195`) — a record TYPE, not a name.
            ("dbl", 0, "String"),
            // C `dbCreateRecordArg1` is a plain string
            // (`dbStaticIocRegister.c:265` at `f4ccf7bc8`, a command in no
            // release tag): the record does not exist
            // yet, so there is nothing to complete against.
            ("dbCreateRecord", 2, "String"),
            // C `dbLoadRecordsArg1` (`dbIocRegister.c:56`).
            ("dbLoadRecords", 1, "String"),
            // C `threadArg0` is `iocshArgArgv` (`libComRegister.c:330`):
            // the whole tail of the line, not one token.
            ("epicsThreadShow", 0, "Argv"),
        ] {
            let def = registry.get(cmd).unwrap_or_else(|| panic!("{cmd} missing"));
            let got = match def.args[index].arg_type {
                ArgType::Record => "Record",
                ArgType::Path => "Path",
                ArgType::String => "String",
                ArgType::Int => "Int",
                ArgType::Double => "Double",
                ArgType::Argv => "Argv",
            };
            assert_eq!(got, want, "{cmd} argument {index}");
        }
    }

    #[test]
    fn the_first_word_completes_command_names() {
        let c = completer();
        let (start, got) = c.candidates("dbg", 3);
        assert_eq!(start, 0);
        assert!(got.contains(&"dbgf".to_string()), "got {got:?}");
        assert!(got.contains(&"dbgrep".to_string()), "got {got:?}");
        assert!(!got.contains(&"dbpr".to_string()), "got {got:?}");
    }

    /// C sets `rl_attempted_completion_over = 1` before the type
    /// dispatch (`iocsh.cpp:577`), so an argument it has no hint for
    /// completes to nothing at all rather than falling back to
    /// filenames.
    #[test]
    fn an_unhinted_argument_offers_nothing() {
        let c = completer();
        // `dbpf`'s second argument is the value (`dbIocRegister.c:266`).
        assert_eq!(c.candidates("dbpf REC ", 9).1, Vec::<String>::new());
        // An argument past the end of the declared list.
        assert_eq!(c.candidates("dbl a b c ", 10).1, Vec::<String>::new());
        // An unregistered command.
        assert_eq!(c.candidates("nosuchcmd ", 10).1, Vec::<String>::new());
    }

    /// C `iocsh.cpp:592-593`: `help`'s first argument names a command,
    /// which no argument type expresses, so C matches by command name.
    #[test]
    fn help_completes_command_names_in_its_first_argument() {
        let c = completer();
        let (start, got) = c.candidates("help dbg", 8);
        assert_eq!(start, 5);
        assert!(got.contains(&"dbgf".to_string()), "got {got:?}");
    }

    /// A `Path` argument reaches the filesystem completer — the whole
    /// point of the hint (`iocsh.cpp:582-584`).
    #[test]
    fn a_path_argument_completes_filenames() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("some.db"), b"").unwrap();
        let word = format!("{}/so", dir.path().display());
        let line = format!("dbLoadRecords {word}");
        let c = completer();
        let (start, got) = c.candidates(&line, line.len());
        assert_eq!(start, "dbLoadRecords ".len());
        assert_eq!(
            got,
            names(&[&format!("{}/some.db", dir.path().display())]),
            "got {got:?}"
        );
    }

    #[test]
    fn the_cursor_word_is_delimited_by_cs_break_characters() {
        // Legacy space-separated form.
        assert_eq!(scan_to_cursor("dbpr REC", 8), (vec!["dbpr"], 5));
        // C++ paren/comma form: `(` and `,` are break characters too
        // (`iocsh.cpp:636`).
        assert_eq!(scan_to_cursor("dbpr(REC", 8), (vec!["dbpr"], 5));
        assert_eq!(scan_to_cursor("dbpr(REC,2", 10), (vec!["dbpr", "REC"], 9));
        // A quote is not part of the word — the case C's own BUG comment
        // (`iocsh.cpp:550-551`) says it cannot reach.
        assert_eq!(scan_to_cursor("dbpr(\"REC", 9), (vec!["dbpr"], 6));
        // Still inside the command name.
        assert_eq!(scan_to_cursor("db", 2), (Vec::new(), 0));
        assert_eq!(scan_to_cursor("", 0), (Vec::new(), 0));
    }

    #[test]
    fn record_completion_chops_at_the_first_separator_past_the_prefix() {
        // C chops each suggestion at the first of `:<>{}-` at or after
        // the common prefix, separator included
        // (`dbCompleteRecord.cpp:65-76`, `:132`).
        let db = names(&["X:a:1", "X:a:2", "X:b", "Y:c"]);
        // Common prefix of the three `X` names is `X:`, so each is
        // offered chopped one element further: `X:a:` and `X:b`.
        assert_eq!(complete_record(&db, "X"), names(&["X:a:", "X:b"]));
        assert_eq!(complete_record(&db, "X:"), names(&["X:a:", "X:b"]));
        // Under `X:a` the common prefix is `X:a:`, past which neither
        // remaining name holds another separator, so both come whole.
        assert_eq!(complete_record(&db, "X:a"), names(&["X:a:1", "X:a:2"]));
        assert_eq!(complete_record(&db, "X:a:"), names(&["X:a:1", "X:a:2"]));
    }

    #[test]
    fn record_completion_is_empty_when_nothing_matches() {
        assert!(complete_record(&names(&["X:a"]), "Z").is_empty());
        assert!(complete_record(&[], "").is_empty());
    }

    #[test]
    fn record_completion_offers_a_lone_match_whole() {
        assert_eq!(complete_record(&names(&["TEMP"]), "TE"), names(&["TEMP"]));
    }

    #[test]
    fn path_completion_lists_the_named_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("alpha.db"), b"").unwrap();
        std::fs::write(dir.path().join("beta.db"), b"").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();

        let base = format!("{}/", dir.path().display());
        let all = complete_path(&base);
        assert_eq!(
            all,
            names(&[
                &format!("{base}alpha.db"),
                &format!("{base}beta.db"),
                // A directory carries the trailing separator so the next
                // TAB descends into it.
                &format!("{base}sub/"),
            ])
        );
        assert_eq!(
            complete_path(&format!("{base}al")),
            names(&[&format!("{base}alpha.db")])
        );
        assert!(complete_path(&format!("{base}nope")).is_empty());
        assert!(complete_path("/no/such/directory/x").is_empty());
    }
}
