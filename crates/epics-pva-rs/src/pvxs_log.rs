//! pvxs `src/log.cpp`: named loggers whose levels `PVXS_LOG` sets, and the
//! line `log_printf` hands to errlog.
//!
//! errlog is the only sink, as in pvxs (`_log_vprintf` is `errlogVprintf`),
//! so a head that installs its own tracing subscriber gets each line once,
//! through the errlog event, and nothing here installs a subscriber.
//!
//! Only the lines this port shares with pvxs go through here, under pvxs's
//! logger names; the port's own diagnostics stay on `tracing`.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, OnceLock};

use epics_base_rs::runtime::epics_string::epics_str_glob_match;
use epics_base_rs::runtime::log::{errlog_flush, errlog_vprintf};

/// pvxs's logger names for the lines this port shares (`DEFINE_LOGGER`).
pub const CONFIG: &str = "pvxs.config";
pub const SERVER_SETUP: &str = "pvxs.server.setup";
pub const TCP_SETUP: &str = "pvxs.tcp.setup";
pub const TCP_IO: &str = "pvxs.tcp.io";
pub const CLIENT_SETUP: &str = "pvxs.client.setup";
pub const CLIENT_IO: &str = "pvxs.client.io";
pub const CLIENT_DUP: &str = "pvxs.client.dup";
pub const SOCK: &str = "pvxs.sock";

/// pvxs `Level` (`pvxs/log.h`): lower is more severe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Crit = 10,
    Err = 20,
    Warn = 30,
    Info = 40,
    Debug = 50,
}

impl Level {
    /// pvxs `name2lvl` (`log.cpp:161-171`): exact upper-case names only.
    fn from_name(name: &str) -> Option<Level> {
        Some(match name {
            "DEBUG" => Level::Debug,
            "INFO" => Level::Info,
            "WARN" => Level::Warn,
            "ERR" => Level::Err,
            "CRIT" => Level::Crit,
            _ => return None,
        })
    }

    /// `log_prep`'s level word (`log.cpp:77-84`), painted as pvxs paints it;
    /// errlog strips the escapes for a console that is not a terminal.
    fn word(self) -> &'static str {
        match self {
            Level::Crit => "\x1b[31;1mCRIT\x1b[0m",
            Level::Err => "\x1b[31;1mERR\x1b[0m",
            Level::Warn => "\x1b[35;1mWARN\x1b[0m",
            Level::Info => "\x1b[34;1mINFO\x1b[0m",
            Level::Debug => "DEBUG",
        }
    }
}

/// pvxs `logger_gbl_t` (`log.cpp:175-262`).
#[derive(Default)]
struct Loggers {
    /// `(pattern, level)` in the order first configured.
    config: Vec<(String, Level)>,
    /// Every logger used so far, at its current level.
    levels: HashMap<&'static str, Level>,
}

impl Loggers {
    /// `logger_gbl_t::init`: a logger's first use takes Warn, then the last
    /// configured pattern that matches it.
    fn level(&mut self, name: &'static str) -> Level {
        let config = &self.config;
        *self.levels.entry(name).or_insert_with(|| {
            config
                .iter()
                .rev()
                .find(|(pattern, _)| epics_str_glob_match(name.as_bytes(), pattern.as_bytes()))
                .map_or(Level::Warn, |&(_, lvl)| lvl)
        })
    }

    /// `logger_gbl_t::set`: update the pattern in place or append it, and
    /// apply it to every logger already in use that it matches.
    fn set(&mut self, pattern: &str, lvl: Level) {
        match self.config.iter_mut().find(|(p, _)| p == pattern) {
            Some((_, old)) if *old == lvl => return,
            Some((_, old)) => *old = lvl,
            None => self.config.push((pattern.to_owned(), lvl)),
        }
        for (name, level) in &mut self.levels {
            if epics_str_glob_match(name.as_bytes(), pattern.as_bytes()) {
                *level = lvl;
            }
        }
    }
}

fn loggers() -> std::sync::MutexGuard<'static, Loggers> {
    static LOGGERS: OnceLock<Mutex<Loggers>> = OnceLock::new();
    LOGGERS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// pvxs `logger::test`: whether `logger` prints at `lvl`.
pub fn enabled(logger: &'static str, lvl: Level) -> bool {
    lvl <= loggers().level(logger)
}

/// pvxs `log_printf(logger, lvl, …)`: `log_prep`'s prefix (`log.cpp:58-95`)
/// and `message`, through errlog. A prefix over 60 bytes, escapes counted,
/// puts the message on the next line.
pub fn log(logger: &'static str, lvl: Level, message: fmt::Arguments<'_>) {
    if !enabled(logger, lvl) {
        return;
    }
    let time = chrono::Local::now()
        .format("%Y-%m-%dT%H:%M:%S%.9f")
        .to_string();
    let word = lvl.word();
    let wrap = if time.len() + 2 + word.len() + logger.len() > 60 {
        "\n    "
    } else {
        ""
    };
    errlog_vprintf(format_args!("{time} {word} {logger}{wrap} {message}\n"));
}

/// pvxs `logger_level_set` (`log.cpp:325-332`).
pub fn logger_level_set(pattern: &str, lvl: Level) {
    loggers().set(pattern, lvl);
}

/// pvxs `logger_level_clear` (`log.cpp:334-341`), which empties the
/// configuration but leaves loggers already in use at their configured
/// level although `pvxs_log_clear` says it resets to the defaults. Not
/// copied: every logger returns to Warn.
pub fn logger_level_clear() {
    let mut loggers = loggers();
    loggers.config.clear();
    for level in loggers.levels.values_mut() {
        *level = Level::Warn;
    }
}

/// pvxs `logger_config_str` (`log.cpp:343-386`): `KEY=LEVEL` entries
/// separated by commas. An entry without `=` is skipped silently, as pvxs
/// skips it.
pub fn logger_config_str(config: &str) {
    for entry in config.split(',') {
        let Some((key, val)) = entry.split_once('=') else {
            continue;
        };
        if key.is_empty() || val.is_empty() {
            errlog_vprintf(format_args!("PVXS_LOG ignore invalid: '{key}={val}'\n"));
        } else if let Some(lvl) = Level::from_name(val) {
            logger_level_set(key, lvl);
        } else {
            errlog_vprintf(format_args!(
                "PVXS_LOG ignore invalid level: '{key}={val}'\n"
            ));
        }
    }
    errlog_flush();
}

/// pvxs `logger_config_env` (`log.cpp:388-395`): [`logger_config_str`] of
/// `$PVXS_LOG`, unless it is unset or empty.
pub fn logger_config_env() {
    if let Some(env) = epics_base_rs::runtime::env::get_lossy("PVXS_LOG")
        && !env.is_empty()
    {
        logger_config_str(&env);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_logger_defaults_to_warn_and_takes_the_last_matching_pattern() {
        let mut l = Loggers::default();
        assert_eq!(l.level("pvxs.tcp.setup"), Level::Warn);
        l.set("pvxs.*", Level::Debug);
        l.set("pvxs.tcp.*", Level::Err);
        assert_eq!(l.level("pvxs.tcp.io"), Level::Err);
        assert_eq!(l.level("pvxs.client.setup"), Level::Debug);
        // `set` re-applies to loggers already in use, and an updated pattern
        // keeps its place in the list, so it now overrides the later one for
        // those loggers but not for one first used afterwards.
        assert_eq!(l.level("pvxs.tcp.setup"), Level::Err);
        l.set("pvxs.*", Level::Info);
        assert_eq!(l.level("pvxs.tcp.setup"), Level::Info);
        assert_eq!(l.level("pvxs.tcp.conn"), Level::Err);
    }

    /// What `f` sends to errlog, each pvxs line without its timestamp.
    fn logged(f: impl FnOnce()) -> Vec<String> {
        use epics_base_rs::runtime::log;
        let heard = std::sync::Arc::new(Mutex::new(Vec::new()));
        let sink = heard.clone();
        let id = log::errlog_add_listener(move |m| {
            let line = match m.split_once(' ') {
                Some((time, line)) if time.starts_with(|c: char| c.is_ascii_digit()) => line,
                _ => m,
            };
            sink.lock().unwrap().push(line.to_owned());
        });
        f();
        log::errlog_flush();
        log::errlog_remove_listener(id);
        heard.lock().unwrap().clone()
    }

    /// softIocPVX 1.5.3 in a loopback-only netns prints both: the 60-byte
    /// prefix stays on one line, the 62-byte one wraps.
    #[test]
    fn a_prefix_over_60_bytes_puts_the_message_on_the_next_line() {
        let lines = logged(|| {
            log(
                TCP_SETUP,
                Level::Warn,
                format_args!("Server unable to bind port 5075"),
            );
            log(
                CLIENT_SETUP,
                Level::Err,
                format_args!("Client context created"),
            );
        });
        assert_eq!(
            lines,
            [
                "WARN pvxs.tcp.setup Server unable to bind port 5075\n",
                "ERR pvxs.client.setup\n     Client context created\n",
            ]
        );
    }

    #[test]
    fn pvxs_log_sets_levels_and_reports_bad_entries() {
        let lines = logged(|| {
            logger_config_str("pvxs.tcp.*=CRIT,,noequals,=WARN,pvxs.client.*=LOUD");
            log(TCP_SETUP, Level::Err, format_args!("hidden"));
            log(CLIENT_SETUP, Level::Err, format_args!("shown"));
            logger_level_clear();
            log(TCP_SETUP, Level::Err, format_args!("shown again"));
        });
        assert_eq!(
            lines,
            [
                "PVXS_LOG ignore invalid: '=WARN'\n",
                "PVXS_LOG ignore invalid level: 'pvxs.client.*=LOUD'\n",
                "ERR pvxs.client.setup\n     shown\n",
                "ERR pvxs.tcp.setup shown again\n",
            ]
        );
    }

    #[test]
    fn pvxs_log_env_configures_the_loggers() {
        // SAFETY: nextest runs each test alone in its process.
        unsafe { std::env::set_var("PVXS_LOG", "pvxs.tcp.*=CRIT") };
        let lines = logged(|| {
            logger_config_env();
            log(TCP_SETUP, Level::Err, format_args!("hidden"));
        });
        assert!(lines.is_empty(), "{lines:?}");
    }

    #[test]
    fn name2lvl_takes_only_the_upper_case_names() {
        assert_eq!(Level::from_name("ERR"), Some(Level::Err));
        assert_eq!(Level::from_name("err"), None);
        assert_eq!(Level::from_name("ERROR"), None);
    }
}
