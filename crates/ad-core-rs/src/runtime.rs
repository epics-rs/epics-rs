//! Driver-facing async runtime facade.
//!
//! Acquisition tasks in `ad-core-rs` drivers run inside an async runtime so
//! they can `await` on parameter I/O, array publishing, and command-channel
//! receives. This module hides the underlying runtime (tokio) behind a
//! minimal API, so driver authors never need to import `tokio::*` directly.
//!
//! # Typical shape
//!
//! ```ignore
//! use ad_core_rs::runtime as rt;
//!
//! pub enum AcqCommand { Start, Stop }
//!
//! pub struct AcquisitionContext {
//!     pub cmd_rx: rt::CommandReceiver<AcqCommand>,
//!     // ...
//! }
//!
//! pub fn spawn_task(ctx: AcquisitionContext) -> std::thread::JoinHandle<()> {
//!     rt::run_thread_named("MyDetTask", move || async move {
//!         acquisition_loop(ctx).await;
//!     })
//! }
//!
//! async fn acquisition_loop(mut ctx: AcquisitionContext) {
//!     loop {
//!         match rt::timeout(std::time::Duration::from_millis(100), ctx.cmd_rx.recv()).await {
//!             Ok(Some(AcqCommand::Start)) => { /* ... */ }
//!             Ok(Some(AcqCommand::Stop)) => break,
//!             Ok(None) => break,       // channel closed
//!             Err(_elapsed) => {}      // timeout
//!         }
//!     }
//! }
//! ```

use std::fmt;
use std::future::Future;
use std::time::Duration;

// ──────────────────────────────────────────────────────────────────────────
// Command channel (bounded MPSC, async)
// ──────────────────────────────────────────────────────────────────────────

/// Error returned by [`CommandSender::send`] when the receiver has been dropped.
#[derive(Debug, Clone)]
pub struct SendError<T>(pub T);

impl<T> fmt::Display for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "command channel closed")
    }
}

impl<T: fmt::Debug> std::error::Error for SendError<T> {}

/// Non-blocking receive errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TryRecvError {
    /// No command is currently queued; try again later.
    Empty,
    /// The sender has been dropped; no further commands will arrive.
    Disconnected,
}

impl fmt::Display for TryRecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("command channel is empty"),
            Self::Disconnected => f.write_str("command channel disconnected"),
        }
    }
}

impl std::error::Error for TryRecvError {}

/// Cloneable sender side of an acquisition command channel.
pub struct CommandSender<T>(tokio::sync::mpsc::Sender<T>);

impl<T> Clone for CommandSender<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> CommandSender<T> {
    /// Send a command, awaiting channel space if the buffer is full.
    pub async fn send(&self, value: T) -> Result<(), SendError<T>> {
        self.0.send(value).await.map_err(|e| SendError(e.0))
    }

    /// Send a command without waiting. Returns the value back on failure.
    pub fn try_send(&self, value: T) -> Result<(), SendError<T>> {
        self.0.try_send(value).map_err(|e| match e {
            tokio::sync::mpsc::error::TrySendError::Full(v)
            | tokio::sync::mpsc::error::TrySendError::Closed(v) => SendError(v),
        })
    }
}

/// Receiver side of an acquisition command channel.
pub struct CommandReceiver<T>(tokio::sync::mpsc::Receiver<T>);

impl<T> CommandReceiver<T> {
    /// Await the next command. Returns `None` when all senders have been dropped.
    pub async fn recv(&mut self) -> Option<T> {
        self.0.recv().await
    }

    /// Poll for the next command without awaiting.
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        self.0.try_recv().map_err(|e| match e {
            tokio::sync::mpsc::error::TryRecvError::Empty => TryRecvError::Empty,
            tokio::sync::mpsc::error::TryRecvError::Disconnected => TryRecvError::Disconnected,
        })
    }
}

/// Create a bounded acquisition command channel.
pub fn command_channel<T>(capacity: usize) -> (CommandSender<T>, CommandReceiver<T>) {
    let (tx, rx) = tokio::sync::mpsc::channel(capacity.max(1));
    (CommandSender(tx), CommandReceiver(rx))
}

// ──────────────────────────────────────────────────────────────────────────
// Time utilities
// ──────────────────────────────────────────────────────────────────────────

/// Error returned when [`timeout`] elapses before the future resolves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Elapsed;

impl fmt::Display for Elapsed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("deadline elapsed")
    }
}

impl std::error::Error for Elapsed {}

/// Await `fut`, returning `Err(Elapsed)` if it doesn't resolve within `duration`.
pub async fn timeout<F: Future>(duration: Duration, fut: F) -> Result<F::Output, Elapsed> {
    tokio::time::timeout(duration, fut)
        .await
        .map_err(|_| Elapsed)
}

/// Async sleep for the given duration.
pub async fn sleep(duration: Duration) {
    tokio::time::sleep(duration).await;
}

// ──────────────────────────────────────────────────────────────────────────
// Thread bootstrap
// ──────────────────────────────────────────────────────────────────────────

/// Drive `fut` on a current-thread runtime of its own: the body of every
/// thread that loops on one for its life.
pub(crate) fn block_on_for_thread<Fut: Future>(fut: Fut) -> Fut::Output {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build thread runtime");
    epics_libcom_rs::runtime::task::block_on_thread_loop(&rt, fut)
}

/// Spawn a dedicated OS thread that runs the given async closure inside an
/// async runtime. Returns the OS-thread `JoinHandle`.
pub fn run_thread<F, Fut>(make_fut: F) -> std::thread::JoinHandle<()>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + 'static,
{
    std::thread::spawn(move || block_on_for_thread(make_fut()))
}

/// Same as [`run_thread`], but with a thread name (useful in `ps`, crash dumps).
pub fn run_thread_named<F, Fut>(name: &str, make_fut: F) -> std::thread::JoinHandle<()>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + 'static,
{
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || block_on_for_thread(make_fut()))
        .expect("failed to spawn driver thread")
}
