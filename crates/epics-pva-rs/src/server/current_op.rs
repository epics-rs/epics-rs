//! pvxs `CurrentOp` (`ioc/iocsource.cpp:645-662`): the PVA operation whose
//! put the calling thread runs, which QSRV's `dbServer.client` names in a
//! TPRO trace.
//!
//! pvxs sets it for the span of a put on the thread that runs it: a single
//! record's put that does not wait (`ioc/singlesource.cpp:371`) and a group's
//! whole put (`ioc/groupsource.cpp:591`). A blocking single put goes to
//! `dbProcessNotify` before it, so its trace names the thread. A put here is
//! a future polled on whichever thread is free, so the op is set per poll.

use std::cell::Cell;
use std::future::Future;

use crate::server_native::source::ChannelContext;

thread_local! {
    /// The attached op's context, valid while the [`Attach`] that set it
    /// lives.
    static CURRENT: Cell<Option<*const ChannelContext>> = const { Cell::new(None) };
}

/// The attach for one poll; the drop puts back the one it replaced.
struct Attach<'a> {
    prev: Option<*const ChannelContext>,
    _op: std::marker::PhantomData<&'a ChannelContext>,
}

impl<'a> Attach<'a> {
    fn new(op: &'a ChannelContext) -> Self {
        Self {
            prev: CURRENT.replace(Some(op)),
            _op: std::marker::PhantomData,
        }
    }
}

impl Drop for Attach<'_> {
    fn drop(&mut self) {
        CURRENT.set(self.prev);
    }
}

/// `fut` with `op` current on every thread that polls it — pvxs's
/// `CurrentOp op(putOperation.get())` around the put.
pub async fn attached<F: Future>(op: &ChannelContext, fut: F) -> F::Output {
    let mut fut = std::pin::pin!(fut);
    std::future::poll_fn(|cx| {
        let _attach = Attach::new(op);
        fut.as_mut().poll(cx)
    })
    .await
}

/// pvxs `qClient` (`ioc/singlesourcehooks.cpp:108-136`), QSRV2's
/// `dbServer.client`: `q2:<account>@<peer>` for a `ca` login, else
/// `q2:<method>/<account>@<peer>`.
pub(crate) fn q_client() -> Option<String> {
    CURRENT.get().map(|op| {
        // SAFETY: only an `Attach` sets the pointer, from a borrow that
        // outlives it, and its drop clears it on this thread before it ends.
        let op = unsafe { &*op };
        let account = String::from_utf8_lossy(op.creds.account.as_bytes());
        if op.creds.method == "ca" {
            format!("q2:{account}@{}", op.peer)
        } else {
            format!("q2:{}/{account}@{}", op.creds.method, op.peer)
        }
    })
}
