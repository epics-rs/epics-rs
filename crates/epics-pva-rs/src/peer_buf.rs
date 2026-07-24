//! Fallible growth for buffers whose size a *peer* chooses.
//!
//! **Invariant: no buffer sized by a peer may be grown infallibly.**
//!
//! `Vec::reserve` / `Vec::extend_from_slice` route an allocation failure to
//! `handle_alloc_error`, which **aborts the process**. Every buffer this crate
//! grows from a socket is therefore a way for one unauthenticated peer to kill
//! the whole IOC — no protocol violation required, just a message bigger than
//! the heap.
//!
//! pvxs is equally uncapped on receive but cannot abort: every bufferevent
//! callback runs inside `catch(std::exception&)` followed by `conn->cleanup()`
//! (`conn.cpp:307-335`), so a `bad_alloc` sheds exactly one connection and the
//! server keeps serving. The *cap* was already at parity; the failure
//! behaviour was not, and that was the parity defect. `try_reserve` is how
//! Rust reaches the same outcome — the error returns through the connection's
//! own task or thread, which tears down that connection and nothing else.
//!
//! This module is the single owner of that growth. Both the server's
//! reassembly and receive paths and the client's go through
//! [`try_extend`]; a bare `extend_from_slice` on such a buffer is the
//! defect, and source guards in `server_native::tcp` and
//! `client_native::server_conn` assert it does not come back.

use crate::error::{PvaError, PvaResult};

/// Reserve room for `additional` bytes, or shed this connection.
///
/// `what` names the buffer in the error text — it reaches an operator's log,
/// so it should read as a noun phrase ("the segment-reassembly buffer").
pub fn try_reserve_or_shed(buf: &mut Vec<u8>, additional: usize, what: &str) -> PvaResult<()> {
    buf.try_reserve(additional).map_err(|_| {
        PvaError::ResourceExhausted(format!(
            "cannot grow {what} by {additional} bytes (currently {}); shedding this connection",
            buf.len()
        ))
    })
}

/// Append `data` to `buf` without a path to `handle_alloc_error`.
///
/// The reserve happens first, so the `extend_from_slice` that follows is
/// guaranteed not to reallocate and therefore cannot abort.
pub fn try_extend(buf: &mut Vec<u8>, data: &[u8], what: &str) -> PvaResult<()> {
    try_reserve_or_shed(buf, data.len(), what)?;
    buf.extend_from_slice(data);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The failure path is reachable without actually exhausting the heap:
    /// `usize::MAX` overflows the layout computation, so `try_reserve` returns
    /// `CapacityOverflow` *without* asking the allocator for anything. That
    /// makes this deterministic on every target rather than a test that only
    /// fires when the machine is already dying.
    #[test]
    fn an_impossible_reservation_is_an_error_not_an_abort() {
        let mut buf = vec![1u8, 2, 3];
        let err = try_reserve_or_shed(&mut buf, usize::MAX, "the test buffer").unwrap_err();
        assert!(
            matches!(err, PvaError::ResourceExhausted(_)),
            "expected ResourceExhausted, got {err:?}"
        );
        // The buffer is untouched: shedding must not corrupt what was there.
        assert_eq!(buf, [1, 2, 3]);
    }

    #[test]
    fn the_refusal_names_the_buffer_and_the_consequence() {
        let mut buf = vec![0u8; 7];
        let err =
            try_reserve_or_shed(&mut buf, usize::MAX, "the segment-reassembly buffer").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("the segment-reassembly buffer"),
            "message does not name the buffer: {msg}"
        );
        assert!(
            msg.contains('7'),
            "message does not report the current length: {msg}"
        );
        assert!(
            msg.contains("shedding this connection"),
            "message does not say what happens next: {msg}"
        );
    }

    #[test]
    fn try_extend_appends_exactly_once() {
        let mut buf = Vec::new();
        try_extend(&mut buf, b"abc", "the test buffer").unwrap();
        try_extend(&mut buf, b"de", "the test buffer").unwrap();
        assert_eq!(buf, b"abcde");
    }

    #[test]
    fn try_extend_of_nothing_succeeds_and_changes_nothing() {
        let mut buf = vec![9u8];
        try_extend(&mut buf, &[], "the test buffer").unwrap();
        assert_eq!(buf, [9]);
    }

    /// Every buffer a peer's stream sizes, and the file that owns it.
    ///
    /// Adding a socket-fed accumulator to this crate means adding a row here.
    /// The buffer name is the anchor: `<name>.extend_from_slice` /
    /// `<name>.push` / `<name>.resize` are all infallible growth, and all
    /// three end at `handle_alloc_error`.
    const PEER_SIZED_BUFFERS: &[(&str, &str)] = &[
        // Server: reassembly across SegFirst..SegLast, and the socket
        // accumulator both drivers share via `read_frame`.
        ("src/server_native/tcp.rs", "seg_buf"),
        ("src/server_native/tcp.rs", "rx_buf"),
        // Client: the reader task's accumulator and its reassembly buffer.
        ("src/client_native/server_conn.rs", "seg_buf"),
        ("src/client_native/server_conn.rs", "rx_buf"),
        // Client: the name-server circuit's accumulator.
        ("src/client_native/search_engine.rs", "rx_buf"),
    ];

    /// Production text of `path` — everything before its first column-0
    /// `#[cfg(test)]`, with comment lines dropped so a needle quoted in prose
    /// (this module's own docs do quote them) cannot fail the guard.
    fn production_source(path: &str) -> String {
        let full = std::fs::read_to_string(format!("{}/{path}", env!("CARGO_MANIFEST_DIR")))
            .unwrap_or_else(|e| panic!("{path}: {e}"));
        let prod = match full.find("\n#[cfg(test)]") {
            Some(i) => &full[..i],
            None => &full[..],
        };
        prod.lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The invariant, enforced against the source rather than against a
    /// runtime path that a future edit could simply route around: no
    /// peer-sized buffer grows through an infallible `Vec` method.
    #[test]
    fn no_peer_sized_buffer_grows_infallibly() {
        for (path, buf) in PEER_SIZED_BUFFERS {
            let src = production_source(path);
            for method in ["extend_from_slice", "push", "resize", "extend"] {
                let banned = format!("{buf}.{method}(");
                assert!(
                    !src.contains(&banned),
                    "{path}: `{banned}` grows a peer-sized buffer infallibly \
                     — an allocation failure there calls handle_alloc_error \
                     and aborts the IOC. Route it through peer_buf::try_extend."
                );
            }
        }
    }

    /// The ban above is satisfiable by deleting the growth entirely, which
    /// would be a different bug. Each buffer must still be *grown*, through
    /// the owner.
    #[test]
    fn every_peer_sized_buffer_still_grows_through_the_owner() {
        for (path, buf) in PEER_SIZED_BUFFERS {
            let src = production_source(path);
            // The buffer must appear as an *argument of* a `try_extend` call,
            // not merely somewhere in the file — `std::mem::take(&mut seg_buf)`
            // would otherwise satisfy a bare containment check while the growth
            // itself had been deleted.
            let reached = src.split("try_extend(").skip(1).any(|after| {
                let head = &after[..after.len().min(160)];
                head.split(',')
                    .next()
                    .is_some_and(|first_arg| first_arg.split_whitespace().last() == Some(buf))
            });
            assert!(
                reached,
                "{path}: `{buf}` is never the buffer passed to peer_buf::try_extend \
                 — the fallible growth for it has gone missing"
            );
        }
    }

    /// After a successful reserve the following `extend_from_slice` must not
    /// reallocate — that is the whole reason the reserve comes first.
    #[test]
    fn a_successful_reserve_leaves_the_append_allocation_free() {
        let mut buf = Vec::with_capacity(4);
        buf.extend_from_slice(b"ab");
        try_reserve_or_shed(&mut buf, 1024, "the test buffer").unwrap();
        let cap = buf.capacity();
        assert!(cap >= 1026);
        buf.extend_from_slice(&[0u8; 1024]);
        assert_eq!(buf.capacity(), cap, "the append reallocated after reserve");
    }
}
