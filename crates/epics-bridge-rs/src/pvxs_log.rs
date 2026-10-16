//! pvxs's `log_warn_printf` line, for the messages this crate ports from
//! pvxs's IOC side.

/// pvxs `log_warn_printf(<logger>, …)`: `log_prep`'s prefix and the message,
/// through errlog as pvxs's `_log_vprintf` sends it (`src/log.cpp:58-104`).
/// The prefix is over 60 bytes with the WARN escapes, so the message starts
/// a new line (log.cpp:86-92) even where errlog strips them.
pub(crate) fn warn(logger: &str, message: std::fmt::Arguments<'_>) {
    use epics_base_rs::runtime::log;
    let warn = if log::errlog_console_paints() {
        "\x1b[35;1mWARN\x1b[0m"
    } else {
        "WARN"
    };
    log::errlog_vprintf(format_args!(
        "{} {warn} {logger}\n     {message}\n",
        chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.9f"),
    ));
}
