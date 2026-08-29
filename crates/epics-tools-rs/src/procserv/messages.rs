//! Single owner of procServ's `@@@`-prefixed console text.
//!
//! Every string here is ported byte-for-byte from upstream C procServ so
//! that log scrapers and telnet front-ends written against it (e.g.
//! `manage-procs`, which greps for `Received a sigChild` and `The PID of
//! new child`) recognise the Rust port's output. Each builder cites its C
//! origin. `NL` is C's `NL` macro = `"\r\n"` (procServ.h:65); control keys
//! render via [`ctl_sc`], the port of C's `CTL_SC` macro (procServ.h:67).
//!
//! Keeping the text in one module (rather than scattered `format!`s in the
//! supervisor) makes it the lone place a divergence from C can be
//! introduced — and the lone place a parity test has to check.

use crate::procserv::child::ChildExit;
use crate::procserv::config::KeyBindings;
use crate::procserv::restart::RestartMode;

/// C `NL` macro (procServ.h:65) — telnet line ending.
pub const NL: &str = "\r\n";

/// The `@@@ @@@ @@@ @@@ @@@` separator C prints around child
/// birth/death (procServ.cc:789, processFactory.cc:195).
pub const RULER: &str = "@@@ @@@ @@@ @@@ @@@\r\n";

/// Render a control byte the way C's `CTL_SC` macro does
/// (procServ.h:67): a control char `0 < c < 32` becomes `^` followed by
/// `c + 64` (so `0x12` → `^R`); any other byte prints literally.
///
/// C's macro expands `CTL_SC(0)` to `"" + '\0'`, emitting a literal NUL
/// into the stream. The port returns nothing for byte 0 instead —
/// injecting a NUL is a C quirk, not a feature, and disabled keys are the
/// only callers that pass 0.
///
/// PS-46: the return is bytes, not a `String`, because C's `%c` writes the
/// key byte itself. A key bound to `0x80..=0xff` has no `char` that encodes
/// back to one byte, so no `String` can carry it and every builder that
/// embeds a rendered key builds bytes too.
pub fn ctl_sc(c: u8) -> Vec<u8> {
    if c > 0 && c < 32 {
        vec![b'^', c + 64]
    } else if c == 0 {
        Vec::new()
    } else {
        vec![c]
    }
}

/// Identity lines for a connecting client — C `infoMessage1`
/// (procServ.cc:572-595): server PID, the server + child startup
/// directories, how the child was launched, and the log file if any.
pub fn info_message1(
    pid: i32,
    startup_dir: &str,
    child_dir: &str,
    name: &str,
    command: &str,
    log_path: Option<&str>,
) -> String {
    let mut s = format!(
        "@@@ procServ server PID: {pid}{NL}\
         @@@ Server startup directory: {startup_dir}{NL}\
         @@@ Child startup directory: {child_dir}{NL}"
    );
    // C only names the child when `-n` gave it a name distinct from the
    // command (procServ.cc:579-584).
    if name != command {
        s.push_str(&format!("@@@ Child \"{name}\" started as: {command}{NL}"));
    } else {
        s.push_str(&format!("@@@ Child started as: {command}{NL}"));
    }
    // C also has an "unable to open log file" variant, but the port aborts
    // startup if the log can't open, so a running server only ever has a
    // good path to report here.
    if let Some(p) = log_path {
        s.push_str(&format!("@@@ Child log file: {p}{NL}"));
    }
    s
}

/// The child PID / shutdown state line — C `infoMessage2`
/// (procServ.cc:586, processFactory.cc:109,191). `Some(pid)` ⟹ child
/// alive; `None` ⟹ child shut down.
pub fn info_message2(name: &str, child_pid: Option<i32>) -> String {
    match child_pid {
        Some(pid) => format!("@@@ Child \"{name}\" PID: {pid}{NL}"),
        None => format!("@@@ Child \"{name}\" is SHUT DOWN{NL}"),
    }
}

/// The command-key menu — C `infoMessage3` (procServ.cc:442-450). Shown
/// at connect time while the child is dead, and again in the shutdown
/// block of every non-oneshot child death.
pub fn info_message3(keys: &KeyBindings) -> Vec<u8> {
    let mut s = Vec::new();
    s.extend_from_slice(b"@@@ ");
    s.extend_from_slice(&keys.restart.map(ctl_sc).unwrap_or_default());
    s.extend_from_slice(b" or ");
    s.extend_from_slice(&keys.kill.map(ctl_sc).unwrap_or_default());
    s.extend_from_slice(b" restarts the child, ");
    s.extend_from_slice(&keys.quit.map(ctl_sc).unwrap_or_default());
    s.extend_from_slice(b" quits the server");
    if let Some(l) = keys.logout {
        s.extend_from_slice(b", ");
        s.extend_from_slice(&ctl_sc(l));
        s.extend_from_slice(b" closes this connection");
    }
    s.extend_from_slice(NL.as_bytes());
    s
}

/// The SIGCHLD reaper block — C `OnPollTimeout` (procServ.cc:788-807): a
/// blank line, the ruler, then the `Received a sigChild` line whose tail
/// reports the normal exit status or the killing signal. A normal exit
/// reads `Normal exit status = K`; a signal death reads `The process was
/// killed by signal N`; an unknown status (neither `WIFEXITED` nor
/// `WIFSIGNALED`, as C tests them) appends nothing.
pub fn child_reaped(pid: i32, exit: ChildExit) -> String {
    let detail = match exit {
        ChildExit::Exited(code) => format!(" Normal exit status = {code}"),
        ChildExit::Signaled(sig) => format!(" The process was killed by signal {sig}"),
        ChildExit::Unknown => String::new(),
    };
    format!("{NL}{RULER}@@@ Received a sigChild for process {pid}.{detail}{NL}")
}

/// The child-shutdown block — C `~processClass` (processFactory.cc:82-114):
/// the current time, the mode-dependent shutdown reason, and (unless
/// oneshot) the command menu redisplayed. `now` is preformatted with the
/// banner time format; `info3` is [`info_message3`] for the active keys.
pub fn child_shutting_down(now: &str, mode: RestartMode, info3: &[u8]) -> Vec<u8> {
    let reason = match mode {
        RestartMode::OnExit => "a new one will be restarted shortly",
        RestartMode::Disabled => "auto restart is disabled",
        RestartMode::OneShot => "oneshot mode: server will exit",
    };
    let mut s =
        format!("@@@ Current time: {now}{NL}@@@ Child process is shutting down, {reason}{NL}")
            .into_bytes();
    if mode != RestartMode::OneShot {
        s.extend_from_slice(info3);
    }
    s
}

/// The restart announcement — C `processFactory` (processFactory.cc:66-72):
/// printed just before a (re)launch, naming the child and, when the
/// display name differs from the executable, the executable too. C prints
/// this even on the first launch, where it reaches only the log.
pub fn restarting_child(name: &str, command: &str) -> String {
    let mut s = format!("@@@ Restarting child \"{name}\"{NL}");
    if name != command {
        s.push_str(&format!("@@@    (as {command}){NL}"));
    }
    s
}

/// Post-fork birth announcement — C `processClass` constructor
/// (processFactory.cc:193-196): the new child's PID followed by the ruler.
pub fn new_child_pid(name: &str, pid: i32) -> String {
    format!("@@@ The PID of new child \"{name}\" is: {pid}{NL}{RULER}")
}

/// One child line for the welcome banner: its PID and wall-clock start
/// time. Absence ([`WelcomeCtx::child`] = `None`) means the child is dead.
pub struct ChildLine<'a> {
    pub pid: i32,
    pub started: &'a str,
}

/// Everything the connect-time welcome banner needs. Built by the
/// supervisor, which owns the live state; this module owns the text.
pub struct WelcomeCtx<'a> {
    pub readonly: bool,
    pub version: &'a str,
    pub keys: &'a KeyBindings,
    pub restart_mode: RestartMode,
    pub pid: i32,
    pub startup_dir: &'a str,
    pub child_dir: &'a str,
    pub name: &'a str,
    pub command: &'a str,
    pub log_path: Option<&'a str>,
    pub proc_started: &'a str,
    pub child: Option<ChildLine<'a>>,
    pub users: usize,
    pub loggers: usize,
}

/// The full connect-time welcome banner, composed in C's exact order and
/// with C's readonly / child-exists gating (clientFactory.cc:100-163):
/// greeting + key hints (interactive only) → infoMessage1 → infoMessage2
/// → start times → peer counts (interactive only) → command menu (only
/// while the child is dead).
pub fn welcome(ctx: &WelcomeCtx) -> Vec<u8> {
    let mut s = Vec::new();

    // greeting1 + greeting2 — C writes these only to non-readonly
    // (interactive) clients (clientFactory.cc:151-154).
    if !ctx.readonly {
        s.extend_from_slice(format!("@@@ Welcome to procServ ({}){NL}", ctx.version).as_bytes());
        s.extend_from_slice(&greeting2(ctx.keys, ctx.restart_mode));
    }

    // infoMessage1 + infoMessage2 — every client (clientFactory.cc:157-158).
    s.extend_from_slice(
        info_message1(
            ctx.pid,
            ctx.startup_dir,
            ctx.child_dir,
            ctx.name,
            ctx.command,
            ctx.log_path,
        )
        .as_bytes(),
    );
    s.extend_from_slice(info_message2(ctx.name, ctx.child.as_ref().map(|c| c.pid)).as_bytes());

    // buf1: start times — every client (clientFactory.cc:134-141,159).
    s.extend_from_slice(
        format!("@@@ procServ server started at: {}{NL}", ctx.proc_started).as_bytes(),
    );
    if let Some(c) = &ctx.child {
        s.extend_from_slice(
            format!("@@@ Child \"{}\" started at: {}{NL}", ctx.name, c.started).as_bytes(),
        );
    }

    // buf2: connected-peer counts — interactive clients only
    // (clientFactory.cc:143-144,160-161). Counted excluding the joining
    // client → C's "(plus you)".
    if !ctx.readonly {
        s.extend_from_slice(
            format!(
                "@@@ {} user(s) and {} logger(s) connected (plus you){NL}",
                ctx.users, ctx.loggers
            )
            .as_bytes(),
        );
    }

    // infoMessage3: the command menu — only when no child is running
    // (clientFactory.cc:162-163).
    if ctx.child.is_none() {
        s.extend_from_slice(&info_message3(ctx.keys));
    }

    s
}

/// The kill/restart-mode/toggle/logout hint block — C `greeting2`
/// (clientFactory.cc:108-124). Packed onto combined lines exactly as C
/// builds it: the kill hint and restart-mode share a line with the toggle
/// hint, then an optional logout line.
fn greeting2(keys: &KeyBindings, mode: RestartMode) -> Vec<u8> {
    let mut s = Vec::new();
    match keys.kill {
        Some(k) => {
            s.extend_from_slice(b"@@@ Use ");
            s.extend_from_slice(&ctl_sc(k));
            s.extend_from_slice(b" to kill the child, ");
        }
        None => s.extend_from_slice(b"@@@ Kill command disabled, "),
    }
    s.extend_from_slice(format!("auto restart mode is {}, ", mode.label()).as_bytes());
    match keys.toggle_restart {
        Some(t) => {
            s.extend_from_slice(b"use ");
            s.extend_from_slice(&ctl_sc(t));
            s.extend_from_slice(format!(" to toggle auto restart{NL}").as_bytes());
        }
        None => s.extend_from_slice(format!("auto restart toggle disabled{NL}").as_bytes()),
    }
    if let Some(l) = keys.logout {
        s.extend_from_slice(b"@@@ Use ");
        s.extend_from_slice(&ctl_sc(l));
        s.extend_from_slice(format!(" to logout from procServ server{NL}").as_bytes());
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Text view of a byte builder's output. Every assertion below is a
    /// default (ASCII) key set; the PS-46 case, whose whole point is a key
    /// byte no `str` can hold, asserts the bytes directly instead.
    fn text(bytes: Vec<u8>) -> String {
        String::from_utf8(bytes).expect("ASCII key bindings render as text")
    }

    fn keys() -> KeyBindings {
        // C defaults: kill ^X, toggle ^T, restart ^R, quit ^Q, logout off.
        KeyBindings {
            kill: Some(0x18),
            toggle_restart: Some(0x14),
            restart: Some(0x12),
            quit: Some(0x11),
            logout: None,
        }
    }

    #[test]
    fn ctl_sc_renders_control_chars_with_caret() {
        assert_eq!(text(ctl_sc(0x12)), "^R");
        assert_eq!(text(ctl_sc(0x18)), "^X");
        assert_eq!(text(ctl_sc(0x11)), "^Q");
        assert_eq!(text(ctl_sc(0x1d)), "^]");
    }

    #[test]
    fn ctl_sc_passes_printable_chars_literally_and_drops_nul() {
        assert_eq!(text(ctl_sc(b'a')), "a");
        // C would emit a NUL; the port emits nothing (don't copy the quirk).
        assert_eq!(text(ctl_sc(0)), "");
    }

    /// PS-46: `CTL_SC` is a printf `%c` (procServ.h:67), so a key bound to a
    /// byte >= 0x80 puts exactly that byte on the connection. Rendering it
    /// through `char` UTF-8-encoded it to two bytes, which is why the whole
    /// chain from `ctl_sc` to the banner carries bytes rather than `String`.
    #[test]
    fn ctl_sc_emits_the_raw_byte_for_a_high_key() {
        assert_eq!(ctl_sc(0x80), vec![0x80]);
        assert_eq!(ctl_sc(0xff), vec![0xff]);
        assert_eq!(ctl_sc(0xa9), vec![0xa9]);

        // ...and the byte survives the builders that embed it.
        let mut k = keys();
        k.kill = Some(0x80);
        for block in [info_message3(&k), greeting2(&k, RestartMode::OnExit)] {
            assert_eq!(
                block.iter().filter(|&&b| b == 0x80).count(),
                1,
                "the key byte reaches the wire once"
            );
            assert!(
                !block.windows(2).any(|w| w == [0xc2, 0x80]),
                "and never as its UTF-8 encoding"
            );
        }
    }

    #[test]
    fn info_message3_matches_c_menu_with_logout_off() {
        // C procServ.cc:443-444 with logout disabled.
        assert_eq!(
            text(info_message3(&keys())),
            "@@@ ^R or ^X restarts the child, ^Q quits the server\r\n"
        );
    }

    #[test]
    fn info_message3_appends_logout_clause_when_enabled() {
        let mut k = keys();
        k.logout = Some(0x1d);
        assert_eq!(
            text(info_message3(&k)),
            "@@@ ^R or ^X restarts the child, ^Q quits the server, ^] closes this connection\r\n"
        );
    }

    #[test]
    fn child_reaped_reports_normal_exit_and_signal() {
        // Normal exit (C procServ.cc:794-798): "Normal exit status = K".
        assert_eq!(
            child_reaped(4242, ChildExit::Exited(3)),
            "\r\n@@@ @@@ @@@ @@@ @@@\r\n\
             @@@ Received a sigChild for process 4242. Normal exit status = 3\r\n"
        );
        // Signal death (C procServ.cc:801-804): "killed by signal N".
        assert_eq!(
            child_reaped(4242, ChildExit::Signaled(9)),
            "\r\n@@@ @@@ @@@ @@@ @@@\r\n\
             @@@ Received a sigChild for process 4242. The process was killed by signal 9\r\n"
        );
        // Unknown status: C tests only WIFEXITED/WIFSIGNALED, so neither
        // tail clause is appended.
        assert_eq!(
            child_reaped(4242, ChildExit::Unknown),
            "\r\n@@@ @@@ @@@ @@@ @@@\r\n@@@ Received a sigChild for process 4242.\r\n"
        );
    }

    #[test]
    fn child_shutting_down_picks_reason_and_redisplays_menu_unless_oneshot() {
        let info3 = info_message3(&keys());
        let menu = text(info3.clone());
        // Auto-restart mode: reason + menu redisplayed.
        assert_eq!(
            text(child_shutting_down(
                "Mon Jun 28 10:00:00 2026",
                RestartMode::OnExit,
                &info3
            )),
            format!(
                "@@@ Current time: Mon Jun 28 10:00:00 2026\r\n\
                 @@@ Child process is shutting down, a new one will be restarted shortly\r\n{menu}"
            )
        );
        // norestart: different reason, menu still shown.
        assert_eq!(
            text(child_shutting_down("T", RestartMode::Disabled, &info3)),
            format!(
                "@@@ Current time: T\r\n\
                 @@@ Child process is shutting down, auto restart is disabled\r\n{menu}"
            )
        );
        // oneshot: reason line only, NO menu (C processFactory.cc:113).
        assert_eq!(
            text(child_shutting_down("T", RestartMode::OneShot, &info3)),
            "@@@ Current time: T\r\n@@@ Child process is shutting down, oneshot mode: server will exit\r\n"
        );
    }

    #[test]
    fn restarting_child_names_executable_only_when_distinct() {
        assert_eq!(
            restarting_child("ioc", "/bin/st.cmd"),
            "@@@ Restarting child \"ioc\"\r\n@@@    (as /bin/st.cmd)\r\n"
        );
        assert_eq!(
            restarting_child("/bin/st.cmd", "/bin/st.cmd"),
            "@@@ Restarting child \"/bin/st.cmd\"\r\n"
        );
    }

    #[test]
    fn new_child_pid_appends_ruler() {
        assert_eq!(
            new_child_pid("ioc", 200),
            "@@@ The PID of new child \"ioc\" is: 200\r\n@@@ @@@ @@@ @@@ @@@\r\n"
        );
    }

    #[test]
    fn info_message2_distinguishes_alive_from_shutdown() {
        assert_eq!(
            info_message2("ioc", Some(4242)),
            "@@@ Child \"ioc\" PID: 4242\r\n"
        );
        assert_eq!(
            info_message2("ioc", None),
            "@@@ Child \"ioc\" is SHUT DOWN\r\n"
        );
    }

    #[test]
    fn info_message1_names_the_child_only_when_distinct_from_command() {
        // Distinct name → "@@@ Child \"name\" started as: cmd".
        let named = info_message1(
            7,
            "/srv",
            "/srv",
            "ioc",
            "/bin/st.cmd",
            Some("/var/log/ioc"),
        );
        assert_eq!(
            named,
            "@@@ procServ server PID: 7\r\n\
             @@@ Server startup directory: /srv\r\n\
             @@@ Child startup directory: /srv\r\n\
             @@@ Child \"ioc\" started as: /bin/st.cmd\r\n\
             @@@ Child log file: /var/log/ioc\r\n"
        );
        // name == command → bare "@@@ Child started as: cmd", no log line.
        let bare = info_message1(7, "/srv", "/run", "/bin/st.cmd", "/bin/st.cmd", None);
        assert_eq!(
            bare,
            "@@@ procServ server PID: 7\r\n\
             @@@ Server startup directory: /srv\r\n\
             @@@ Child startup directory: /run\r\n\
             @@@ Child started as: /bin/st.cmd\r\n"
        );
    }

    #[test]
    fn welcome_interactive_with_live_child_matches_c_order() {
        let ctx = WelcomeCtx {
            readonly: false,
            version: "2.8.0",
            keys: &keys(),
            restart_mode: RestartMode::OnExit,
            pid: 100,
            startup_dir: "/srv",
            child_dir: "/srv",
            name: "/bin/st.cmd",
            command: "/bin/st.cmd",
            log_path: None,
            proc_started: "Mon Jun 28 10:00:00 2026",
            child: Some(ChildLine {
                pid: 200,
                started: "Mon Jun 28 10:00:01 2026",
            }),
            users: 1,
            loggers: 0,
        };
        assert_eq!(
            text(welcome(&ctx)),
            "@@@ Welcome to procServ (2.8.0)\r\n\
             @@@ Use ^X to kill the child, auto restart mode is ON, use ^T to toggle auto restart\r\n\
             @@@ procServ server PID: 100\r\n\
             @@@ Server startup directory: /srv\r\n\
             @@@ Child startup directory: /srv\r\n\
             @@@ Child started as: /bin/st.cmd\r\n\
             @@@ Child \"/bin/st.cmd\" PID: 200\r\n\
             @@@ procServ server started at: Mon Jun 28 10:00:00 2026\r\n\
             @@@ Child \"/bin/st.cmd\" started at: Mon Jun 28 10:00:01 2026\r\n\
             @@@ 1 user(s) and 0 logger(s) connected (plus you)\r\n"
        );
    }

    #[test]
    fn welcome_readonly_trims_greeting_counts_and_shows_menu_when_dead() {
        // A logger (readonly) with a dead child: no greeting/key hints, no
        // peer-count line, but the command menu IS shown because the child
        // is down (clientFactory.cc:151-163).
        let ctx = WelcomeCtx {
            readonly: true,
            version: "2.8.0",
            keys: &keys(),
            restart_mode: RestartMode::Disabled,
            pid: 100,
            startup_dir: "/srv",
            child_dir: "/srv",
            name: "ioc",
            command: "/bin/st.cmd",
            log_path: None,
            proc_started: "Mon Jun 28 10:00:00 2026",
            child: None,
            users: 2,
            loggers: 1,
        };
        assert_eq!(
            text(welcome(&ctx)),
            "@@@ procServ server PID: 100\r\n\
             @@@ Server startup directory: /srv\r\n\
             @@@ Child startup directory: /srv\r\n\
             @@@ Child \"ioc\" started as: /bin/st.cmd\r\n\
             @@@ Child \"ioc\" is SHUT DOWN\r\n\
             @@@ procServ server started at: Mon Jun 28 10:00:00 2026\r\n\
             @@@ ^R or ^X restarts the child, ^Q quits the server\r\n"
        );
    }
}
