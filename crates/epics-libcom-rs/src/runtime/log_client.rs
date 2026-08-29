//! The IOC log client — C `modules/libcom/src/log/{logClient,iocLog}.c`
//! @`R7.0.10`.
//!
//! A site runs one `iocLogServer` and every IOC forwards its `errlog` stream
//! to it over TCP. The client is nothing more than an errlog listener with a
//! socket: [`ioc_log_init`] registers one with
//! [`crate::runtime::log::errlog_add_listener`], and from then on every
//! message the errlog worker drains is appended to a 16 KiB buffer and pushed
//! to the server by a reconnecting background thread.
//!
//! Everything a site can observe is reproduced: the buffer size and its
//! overflow message, the 5-second reconnect period, the `iocLogPrefix`
//! write-once rule and its warning, and the exact `iocLog:` diagnostics for a
//! missing or out-of-range environment variable.

use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

use crate::runtime::env_table::{EPICS_IOC_LOG_INET, EPICS_IOC_LOG_PORT};

/// Why [`ioc_log_init`] declined. C returns a bare `iocLogError` (-1) for
/// both; naming them keeps the caller from having to re-read stderr to find
/// out which happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IocLogError {
    /// `EPICS_IOC_LOG_INET` / `EPICS_IOC_LOG_PORT` do not name a log server.
    NoServerConfigured,
    /// The reconnection thread could not be created.
    NoRestartThread,
}

impl std::fmt::Display for IocLogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoServerConfigured => {
                f.write_str("no log server configured (EPICS_IOC_LOG_INET / EPICS_IOC_LOG_PORT)")
            }
            Self::NoRestartThread => f.write_str("could not start the log client thread"),
        }
    }
}

impl std::error::Error for IocLogError {}

/// C `logClient::msgBuf[0x4000]` (`logClient.c:42`).
const MSG_BUF_SIZE: usize = 0x4000;
/// C `LOG_RESTART_DELAY` (`logClient.c:60`).
const RESTART_DELAY: Duration = Duration::from_secs(5);

/// C `iocLogDisable` (`iocLog.c:26`) — an exported iocsh variable, so it can
/// be flipped before *or* after `iocLogInit`.
static IOC_LOG_DISABLE: AtomicBool = AtomicBool::new(false);

/// C `logClientPrefix` (`logClient.c:66`): file-scope, shared by every client,
/// and prepended to every message.
static PREFIX: Mutex<Option<String>> = Mutex::new(None);

/// C's single `iocLogClient` (`iocLog.c:31`) — `iocLogInit` is a no-op once
/// this is set, however many times it is called.
static CLIENT: OnceLock<Arc<LogClient>> = OnceLock::new();

struct LogClientState {
    sock: Option<TcpStream>,
    /// C `nextMsgIndex` bytes of `msgBuf`.
    msg_buf: Vec<u8>,
    connect_count: u32,
    shutdown: bool,
}

struct LogClient {
    addr: SocketAddr,
    /// C `pClient->name`, the dotted address the diagnostics quote.
    name: String,
    state: Mutex<LogClientState>,
    /// C `shutdownNotify` — what cuts the restart thread's wait short.
    wake: Condvar,
}

impl LogClient {
    /// C `sendMessageChunk` (`logClient.c:171-196`): fill the buffer, flushing
    /// when it is full, and report the overflow exactly once per chunk.
    fn send_chunk(&self, state: &mut LogClientState, text: &[u8]) {
        let mut rest = text;
        while !rest.is_empty() {
            let mut left = MSG_BUF_SIZE - state.msg_buf.len();
            if left < rest.len() && !state.msg_buf.is_empty() && state.sock.is_some() {
                self.flush_locked(state);
                left = MSG_BUF_SIZE - state.msg_buf.len();
            }
            if left == 0 {
                eprintln!("log client: messages to \"{}\" are lost", self.name);
                break;
            }
            let take = left.min(rest.len());
            state.msg_buf.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
        }
    }

    /// C `logClientSend` (`logClient.c:202-221`) — the prefix, then the
    /// message, under one lock so the two cannot interleave with another
    /// thread's pair.
    fn send(&self, message: &str) {
        let mut state = self.state.lock().expect("log client");
        if let Some(prefix) = PREFIX.lock().expect("log client prefix").as_deref() {
            self.send_chunk(&mut state, prefix.as_bytes());
        }
        self.send_chunk(&mut state, message.as_bytes());
    }

    /// C `logClientFlush` (`logClient.c:222-273`): push what is buffered and
    /// close on any write error, so the restart thread reconnects.
    fn flush_locked(&self, state: &mut LogClientState) {
        let Some(sock) = state.sock.as_mut() else {
            return;
        };
        match sock.write_all(&state.msg_buf) {
            Ok(()) => {
                let _ = sock.flush();
                state.msg_buf.clear();
            }
            Err(e) => {
                eprintln!(
                    "log client: lost contact with log server at '{}'\n because \"{e}\"",
                    self.name
                );
                state.sock = None;
            }
        }
    }

    /// C `logClientConnect` (`logClient.c:308-424`), minus the non-blocking
    /// dance: a blocking `connect` with a timeout reaches the same two states,
    /// and the restart thread retries either way.
    fn connect(&self) {
        let sock = TcpStream::connect_timeout(&self.addr, RESTART_DELAY);
        let mut state = self.state.lock().expect("log client");
        match sock {
            Ok(sock) => {
                let _ = sock.set_nodelay(true);
                state.sock = Some(sock);
                state.connect_count += 1;
                eprintln!("log client: connected to log server at '{}'", self.name);
            }
            Err(_) => {
                // C prints its connect failure only once per distinct errno
                // (`connFailStatus`), so a log server that is simply not
                // running does not fill the console every 5 seconds. Silence
                // here is the same choice made whole.
                state.sock = None;
            }
        }
    }
}

/// C `logClientRestart` (`logClient.c:426-449`): reconnect if down, flush,
/// wait 5 s, repeat.
fn restart_thread(client: Arc<LogClient>) {
    loop {
        let (connected, shutdown) = {
            let state = client.state.lock().expect("log client");
            (state.sock.is_some(), state.shutdown)
        };
        if shutdown {
            return;
        }
        if !connected {
            client.connect();
        }
        {
            let mut state = client.state.lock().expect("log client");
            client.flush_locked(&mut state);
        }
        let state = client.state.lock().expect("log client");
        let _ = client
            .wake
            .wait_timeout_while(state, RESTART_DELAY, |s| !s.shutdown);
    }
}

/// C `getConfig` (`iocLog.c:37-66`) — both variables, both diagnostics.
fn get_config() -> Result<SocketAddr, IocLogError> {
    let Some(port) = EPICS_IOC_LOG_PORT.long() else {
        eprintln!(
            "iocLog: EPICS environment variable \"{}\" undefined",
            EPICS_IOC_LOG_PORT.name()
        );
        return Err(IocLogError::NoServerConfigured);
    };
    if !(0..=i64::from(u16::MAX)).contains(&port) {
        eprintln!(
            "iocLog: EPICS environment variable \"{}\" out of range",
            EPICS_IOC_LOG_PORT.name()
        );
        return Err(IocLogError::NoServerConfigured);
    }
    // C `envGetInetAddrConfigParam` fails on an unset or unparsable value, and
    // `EPICS_IOC_LOG_INET`'s compiled default is empty — so an IOC that was
    // never told where its log server is reports the variable undefined rather
    // than connecting somewhere.
    let inet = EPICS_IOC_LOG_INET.get().unwrap_or_default();
    let Ok(addr) = inet.trim().parse::<Ipv4Addr>() else {
        eprintln!(
            "iocLog: EPICS environment variable \"{}\" undefined",
            EPICS_IOC_LOG_INET.name()
        );
        return Err(IocLogError::NoServerConfigured);
    };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Ok(SocketAddr::new(IpAddr::V4(addr), port as u16))
}

/// C `iocLogInit` (`iocLog.c:121-142`).
///
/// A no-op when logging is disabled or a client already exists, so a startup
/// script may call it more than once. On success the client is registered as
/// an errlog listener and every subsequent message reaches the log server.
///
/// # Errors
/// Returns [`IocLogError`] when `EPICS_IOC_LOG_INET`/`EPICS_IOC_LOG_PORT` do
/// not name a server, or when the reconnection thread cannot be created —
/// exactly the two paths on which C returns `iocLogError`.
pub fn ioc_log_init() -> Result<(), IocLogError> {
    if IOC_LOG_DISABLE.load(Ordering::Relaxed) {
        return Ok(());
    }
    if CLIENT.get().is_some() {
        return Ok(());
    }
    let addr = get_config()?;
    let client = Arc::new(LogClient {
        addr,
        name: addr.to_string(),
        state: Mutex::new(LogClientState {
            sock: None,
            msg_buf: Vec::with_capacity(MSG_BUF_SIZE),
            connect_count: 0,
            shutdown: false,
        }),
        wake: Condvar::new(),
    });
    if CLIENT.set(Arc::clone(&client)).is_err() {
        // Another thread won the race; its client is the one registered.
        return Ok(());
    }

    let worker = Arc::clone(&client);
    if crate::runtime::task::spawn_dedicated_thread(
        "logRestart".to_string(),
        crate::runtime::task::ThreadPriority::Low,
        crate::runtime::task::StackSizeClass::Small,
        move || restart_thread(worker),
    )
    .is_err()
    {
        eprintln!("log client: unable to start reconnection thread");
        return Err(IocLogError::NoServerConfigured);
    }

    // C `logClientSendMessage` (`iocLog.c:79-84`) reads `iocLogDisable` on
    // every message, not once at init, so `setIocLogDisable 1` silences a
    // client that is already running.
    let sender = Arc::clone(&client);
    crate::runtime::log::errlog_add_listener(move |message| {
        if !IOC_LOG_DISABLE.load(Ordering::Relaxed) {
            sender.send(message);
        }
    });
    Ok(())
}

/// C `iocLogPrefix` (`logClient.c:551-576`) — write-once.
///
/// The prefix is prepended to every message by every client, so C refuses to
/// change one that is already in use and warns when the new value differs.
/// A repeat of the SAME prefix is silent, which is what makes a startup script
/// that is sourced twice harmless.
pub fn ioc_log_prefix(prefix: &str) {
    let mut current = PREFIX.lock().expect("log client prefix");
    match current.as_deref() {
        Some(existing) => {
            if existing != prefix {
                println!(
                    "{} iocLogPrefix: The prefix was already set to \"{existing}\" and can't be changed.",
                    crate::runtime::log::erl_warning()
                );
            }
        }
        None => *current = Some(prefix.to_string()),
    }
}

/// The prefix in force, if one was set.
#[must_use]
pub fn ioc_log_prefix_get() -> Option<String> {
    PREFIX.lock().expect("log client prefix").clone()
}

/// C `setIocLogDisable` (`libComRegister.c:226-229`).
pub fn set_ioc_log_disable(disable: bool) {
    IOC_LOG_DISABLE.store(disable, Ordering::Relaxed);
}

/// Whether forwarding is currently disabled.
#[must_use]
pub fn ioc_log_disabled() -> bool {
    IOC_LOG_DISABLE.load(Ordering::Relaxed)
}

/// C `iocLogFlush` (`iocLog.c:70-75`) — push whatever is buffered now.
pub fn ioc_log_flush() {
    if let Some(client) = CLIENT.get() {
        let mut state = client.state.lock().expect("log client");
        client.flush_locked(&mut state);
    }
}

/// C `iocLogShow`/`logClientShow` (`iocLog.c:145-152`, `logClient.c:513-545`).
/// Returns the lines rather than printing them, so the iocsh command can send
/// them through its own redirected output.
#[must_use]
pub fn ioc_log_show(level: u32) -> Vec<String> {
    let Some(client) = CLIENT.get() else {
        return Vec::new();
    };
    let state = client.state.lock().expect("log client");
    let mut out = Vec::new();
    if state.sock.is_some() {
        out.push(format!(
            "log client: connected to log server at '{}'",
            client.name
        ));
    } else {
        out.push(format!(
            "log client: disconnected from log server at '{}'",
            client.name
        ));
    }
    if let Some(prefix) = PREFIX.lock().expect("log client prefix").as_deref() {
        out.push(format!("log client: prefix is \"{prefix}\""));
    }
    if level > 0 {
        out.push(format!(
            "log client: sock {}, connect cycles = {}",
            if state.sock.is_some() {
                "OK"
            } else {
                "INVALID"
            },
            state.connect_count
        ));
    }
    if level > 1 {
        out.push(format!(
            "log client: {} bytes in buffer",
            state.msg_buf.len()
        ));
        if !state.msg_buf.is_empty() {
            out.push("-------------------------".to_string());
            out.push(String::from_utf8_lossy(&state.msg_buf).into_owned());
            out.push("-------------------------".to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::io::Read;
    use std::net::TcpListener;

    /// Boundary: no log server configured. `EPICS_IOC_LOG_INET`'s compiled
    /// default is empty, so an IOC that was never told where to log must
    /// report the variable undefined and connect nowhere — C's `getConfig`
    /// returns `iocLogError` before `logClientCreate` is reached
    /// (`iocLog.c:56-63`).
    #[test]
    #[serial(ioc_log)]
    fn with_no_inet_configured_init_declines_instead_of_connecting() {
        unsafe {
            std::env::remove_var("EPICS_IOC_LOG_INET");
        }
        assert_eq!(get_config(), Err(IocLogError::NoServerConfigured));
    }

    /// Boundary: the port must come from `EPICS_IOC_LOG_PORT`, whose
    /// compiled default is 7004 (`envDefs`/`env_table`), and a value outside
    /// `0..=65535` is refused rather than truncated.
    #[test]
    #[serial(ioc_log)]
    fn the_port_defaults_to_7004_and_is_range_checked() {
        unsafe {
            std::env::set_var("EPICS_IOC_LOG_INET", "127.0.0.1");
            std::env::remove_var("EPICS_IOC_LOG_PORT");
        }
        assert_eq!(get_config().expect("configured").port(), 7004);

        unsafe {
            std::env::set_var("EPICS_IOC_LOG_PORT", "70000");
        }
        assert_eq!(get_config(), Err(IocLogError::NoServerConfigured));
        unsafe {
            std::env::remove_var("EPICS_IOC_LOG_PORT");
        }
    }

    /// Boundary: `iocLogPrefix` is write-once. C keeps the first value and
    /// warns only when a LATER call differs (`logClient.c:560-573`), so a
    /// startup script sourced twice is silent while a genuine conflict is
    /// reported.
    #[test]
    #[serial(ioc_log)]
    fn the_prefix_is_write_once_and_a_repeat_of_the_same_value_is_silent() {
        *PREFIX.lock().expect("prefix") = None;
        ioc_log_prefix("fac=SR ");
        assert_eq!(ioc_log_prefix_get().as_deref(), Some("fac=SR "));
        ioc_log_prefix("fac=SR ");
        assert_eq!(ioc_log_prefix_get().as_deref(), Some("fac=SR "));
        ioc_log_prefix("fac=BTS ");
        assert_eq!(
            ioc_log_prefix_get().as_deref(),
            Some("fac=SR "),
            "the first prefix stands; C refuses to change one already in use"
        );
        *PREFIX.lock().expect("prefix") = None;
    }

    /// The row's own observable: a log server receives the IOC's messages.
    /// A real `TcpListener` stands in for `iocLogServer`, and the bytes it
    /// reads must be the prefix followed by the errlog text.
    #[test]
    #[serial(ioc_log)]
    fn a_log_server_receives_the_ioc_messages_with_the_prefix_prepended() {
        let server = TcpListener::bind("127.0.0.1:0").expect("log server");
        let port = server.local_addr().expect("addr").port();
        unsafe {
            std::env::set_var("EPICS_IOC_LOG_INET", "127.0.0.1");
            std::env::set_var("EPICS_IOC_LOG_PORT", port.to_string());
        }
        *PREFIX.lock().expect("prefix") = None;
        ioc_log_prefix("ioc=TEST ");
        set_ioc_log_disable(false);
        ioc_log_init().expect("the client must start");

        let (mut peer, _) = server.accept().expect("the client must connect");
        peer.set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");

        crate::runtime::log::errlog_printf("bind failed\n");
        crate::runtime::log::errlog_flush();

        // The restart thread flushes every 5 s; push now so the test does not
        // have to wait for it.
        let mut buf = [0u8; 256];
        let mut got = String::new();
        for _ in 0..50 {
            ioc_log_flush();
            match peer.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    got.push_str(&String::from_utf8_lossy(&buf[..n]));
                    break;
                }
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        assert_eq!(
            got, "ioc=TEST bind failed\n",
            "the server must see the prefix then the message"
        );

        unsafe {
            std::env::remove_var("EPICS_IOC_LOG_INET");
            std::env::remove_var("EPICS_IOC_LOG_PORT");
        }
    }

    /// Boundary: `setIocLogDisable 1` on a client that is ALREADY running.
    /// C reads `iocLogDisable` inside `logClientSendMessage`, per message
    /// (`iocLog.c:79-84`), so the switch takes effect without tearing the
    /// connection down.
    #[test]
    #[serial(ioc_log)]
    fn disabling_forwarding_silences_a_client_that_is_already_connected() {
        let server = TcpListener::bind("127.0.0.1:0").expect("log server");
        let port = server.local_addr().expect("addr").port();
        unsafe {
            std::env::set_var("EPICS_IOC_LOG_INET", "127.0.0.1");
            std::env::set_var("EPICS_IOC_LOG_PORT", port.to_string());
        }
        *PREFIX.lock().expect("prefix") = None;
        set_ioc_log_disable(false);
        ioc_log_init().expect("the client must start");
        let (mut peer, _) = server.accept().expect("the client must connect");
        peer.set_read_timeout(Some(Duration::from_millis(300)))
            .expect("read timeout");

        set_ioc_log_disable(true);
        crate::runtime::log::errlog_printf("suppressed\n");
        crate::runtime::log::errlog_flush();
        ioc_log_flush();

        let mut buf = [0u8; 64];
        let n = peer.read(&mut buf).unwrap_or(0);
        assert_eq!(
            n,
            0,
            "nothing may reach the server while iocLogDisable is set: {:?}",
            String::from_utf8_lossy(&buf[..n])
        );
        set_ioc_log_disable(false);
        unsafe {
            std::env::remove_var("EPICS_IOC_LOG_INET");
            std::env::remove_var("EPICS_IOC_LOG_PORT");
        }
    }
}
