//! C `rsrvCurrentClient` (`caservertask.c:50`): the client whose request the
//! calling thread runs, which `dbServerClient` names in a TPRO trace.
//!
//! C attaches a client's own thread to it for the thread's life
//! (`casAttachThreadToClient`, `caservertask.c:1316-1323`). A client here is
//! a task polled on whichever thread is free, so the attach is per poll, and
//! only the put is attached: no other request of a client's reaches
//! `dbProcess`.

use std::cell::Cell;
use std::future::Future;

thread_local! {
    /// The attached client's `CLIENT_NAME` and host identity, valid while the
    /// [`Attach`] that set them lives.
    static CURRENT: Cell<Option<(*const [u8], *const [u8])>> = const { Cell::new(None) };
}

/// The attach for one poll; the drop puts back the one it replaced.
struct Attach<'a> {
    prev: Option<(*const [u8], *const [u8])>,
    _names: std::marker::PhantomData<&'a [u8]>,
}

impl<'a> Attach<'a> {
    fn new(user: &'a [u8], host: &'a [u8]) -> Self {
        Self {
            prev: CURRENT.replace(Some((user, host))),
            _names: std::marker::PhantomData,
        }
    }
}

impl Drop for Attach<'_> {
    fn drop(&mut self) {
        CURRENT.set(self.prev);
    }
}

/// `fut` with the client `user`@`host` attached to every thread that polls it.
pub(crate) async fn attached<F: Future>(user: &[u8], host: &[u8], fut: F) -> F::Output {
    let mut fut = std::pin::pin!(fut);
    std::future::poll_fn(|cx| {
        let _attach = Attach::new(user, host);
        fut.as_mut().poll(cx)
    })
    .await
}

/// C `casClientInitiatingCurrentThread` (`camsgtask.c:164-179`), RSRV's
/// `dbServer.client`: `ca:<user>@<host>` for the client attached to this
/// thread.
pub(crate) fn client_initiating_current_thread() -> Option<String> {
    CURRENT.get().map(|(user, host)| {
        // SAFETY: only an `Attach` sets the pointers, from borrows that
        // outlive it, and its drop clears them on this thread before they end.
        let (user, host) = unsafe { (&*user, &*host) };
        format!(
            "ca:{}@{}",
            String::from_utf8_lossy(user),
            String::from_utf8_lossy(host)
        )
    })
}
