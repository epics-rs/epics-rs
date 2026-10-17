//! SIGTERM and SIGINT: the one place an IOC waits for them, and how the
//! process leaves on one.
//!
//! C installs no handler for either, so softIoc dies at the default
//! disposition and its parent sees it killed by the signal (status 143 or
//! 130). The port catches them so it can stop its servers and run the exit
//! callbacks, then re-raises at the default disposition, so the status is
//! C's. The exit callbacks running are the one difference from C left (user
//! decision 2026-10-17).
//!
//! One owner answers each signal. An [`IocApplication`] answers both; a
//! server run without one answers them itself ([`application_owns`]). A
//! SIGTERM drain, when configured, takes SIGTERM from either through a
//! [`TerminateClaim`].
//!
//! [`IocApplication`]: crate::server::ioc_app::IocApplication

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShutdownSignal {
    Terminate,
    Interrupt,
}

impl ShutdownSignal {
    /// The POSIX number, which is also what the exit status encodes.
    pub const fn number(self) -> i32 {
        match self {
            ShutdownSignal::Terminate => 15,
            ShutdownSignal::Interrupt => 2,
        }
    }

    fn from_number(n: i32) -> Option<Self> {
        match n {
            15 => Some(ShutdownSignal::Terminate),
            2 => Some(ShutdownSignal::Interrupt),
            _ => None,
        }
    }
}

/// The signal [`recv`] returned for, 0 before one arrives.
static RECEIVED: AtomicI32 = AtomicI32::new(0);

/// The shutdown signal this process has received, if any.
pub fn received() -> Option<ShutdownSignal> {
    ShutdownSignal::from_number(RECEIVED.load(Ordering::Acquire))
}

static APPLICATION: AtomicBool = AtomicBool::new(false);

/// Whether an `IocApplication` is answering SIGTERM and SIGINT, so a server
/// it runs must install nothing for them.
pub fn application_owns() -> bool {
    APPLICATION.load(Ordering::Acquire)
}

pub(crate) fn set_application_owns() {
    APPLICATION.store(true, Ordering::Release);
}

static CLAIMS: AtomicUsize = AtomicUsize::new(0);

/// SIGTERM is a drain's while this is held: [`recv`] does not return for it,
/// and the holder waits with [`recv_claimed`] instead.
pub struct TerminateClaim(());

impl TerminateClaim {
    pub fn new() -> Self {
        CLAIMS.fetch_add(1, Ordering::AcqRel);
        TerminateClaim(())
    }
}

impl Default for TerminateClaim {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TerminateClaim {
    fn drop(&mut self) {
        CLAIMS.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Wait for `sig`, then record it for [`received`]. A SIGTERM that arrives
/// while a [`TerminateClaim`] is held is the claim's, and this keeps waiting.
///
/// Never returns on a build without a tokio reactor: `tokio::signal` panics
/// with none entered rather than reporting an error, and an `exec_backend`
/// IOC is polled on the park loop or a callback worker, neither of which
/// enters one. Such a build installs no handler, which is C's disposition.
///
/// Never returns, installing nothing, for a signal the process started with
/// ignored: a C IOC keeps the inherited `SIG_IGN`, which is how a background
/// job survives the terminal's SIGINT.
pub async fn recv(sig: ShutdownSignal) {
    loop {
        listen(sig).await;
        if sig != ShutdownSignal::Terminate || CLAIMS.load(Ordering::Acquire) == 0 {
            break;
        }
    }
    RECEIVED.store(sig.number(), Ordering::Release);
}

/// [`recv`] for SIGTERM, for the holder of `_claim`.
pub async fn recv_claimed(_claim: &TerminateClaim) {
    listen(ShutdownSignal::Terminate).await;
    RECEIVED.store(ShutdownSignal::Terminate.number(), Ordering::Release);
}

async fn listen(sig: ShutdownSignal) {
    if ignored_at_startup(sig) {
        std::future::pending::<()>().await;
    }
    wait(sig).await;
}

/// Whether `sig` was `SIG_IGN` before this module installed anything for it.
/// Read once per signal, on the first wait: a later read would see the
/// handler that first wait put in. Only the reactor build installs one.
#[cfg(all(unix, tokio_backend))]
fn ignored_at_startup(sig: ShutdownSignal) -> bool {
    use std::sync::OnceLock;
    static TERMINATE: OnceLock<bool> = OnceLock::new();
    static INTERRUPT: OnceLock<bool> = OnceLock::new();
    let cell = match sig {
        ShutdownSignal::Terminate => &TERMINATE,
        ShutdownSignal::Interrupt => &INTERRUPT,
    };
    *cell.get_or_init(|| {
        // SAFETY: a query only (null new action) into a local.
        unsafe {
            let mut old: libc::sigaction = std::mem::zeroed();
            libc::sigaction(sig.number(), std::ptr::null(), &mut old) == 0
                && old.sa_sigaction == libc::SIG_IGN
        }
    })
}

#[cfg(not(all(unix, tokio_backend)))]
fn ignored_at_startup(_sig: ShutdownSignal) -> bool {
    false
}

#[cfg(all(unix, tokio_backend))]
async fn wait(sig: ShutdownSignal) {
    use tokio::signal::unix::{SignalKind, signal};
    let kind = match sig {
        ShutdownSignal::Terminate => SignalKind::terminate(),
        ShutdownSignal::Interrupt => SignalKind::interrupt(),
    };
    match signal(kind) {
        Ok(mut s) => {
            s.recv().await;
        }
        Err(e) => {
            tracing::warn!(error = %e, ?sig, "cannot install the signal handler");
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(all(not(unix), tokio_backend))]
async fn wait(sig: ShutdownSignal) {
    match sig {
        ShutdownSignal::Interrupt => {
            if tokio::signal::ctrl_c().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
        ShutdownSignal::Terminate => std::future::pending::<()>().await,
    }
}

#[cfg(not(tokio_backend))]
async fn wait(_sig: ShutdownSignal) {
    std::future::pending::<()>().await;
}

/// Leave the process as [`received`]'s signal would have, once the exit
/// callbacks have run; returns at once when no signal arrived, or on a
/// target without POSIX signals.
pub fn exit_if_received() {
    #[cfg(unix)]
    if let Some(sig) = received() {
        exit_by(sig);
    }
}

/// Run the exit callbacks, then end the process as `sig` does a C IOC.
#[cfg(unix)]
pub fn exit_by(sig: ShutdownSignal) -> ! {
    crate::runtime::exit::exit_by_signal(sig.number())
}
