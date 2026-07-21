//! The per-client receive accumulation buffer — the one place a CA server
//! grows memory from bytes a peer sent, and the one place it decides that a
//! peer-declared message is too large to hold.
//!
//! # Invariant
//!
//! **The accumulation buffer's length is bounded by a server-chosen
//! constant, never by a peer-declared size.**
//!
//! Two things can violate that, and both belong here because both are
//! *growth*:
//!
//! 1. Appending received bytes without a ceiling. A peer that opens a
//!    circuit and streams without ever completing a message grows the
//!    buffer until the process dies.
//! 2. Believing a declared size. A CA v4.9 extended header carries a `u32`
//!    `m_postsize`, so one 24-byte frame can declare a ~4 GiB body. A
//!    framing loop that waits for `hdr_size + actual_postsize()` bytes
//!    before dispatching will accumulate every dribbled byte on the way to
//!    a total that will never arrive — the buffer is then sized by the
//!    attacker, which is the DoS.
//!
//! # C parity
//!
//! C refuses at `rsrv/camessage.c:2471-2489`. When `msgsize` exceeds what
//! the receive buffer can grow to, it does **not** close the circuit:
//!
//! ```c
//! if ( msgsize > client->recv.maxstk ) {
//!     casExpandRecvBuffer ( client, msgsize );
//!     if ( msgsize > client->recv.maxstk ) {
//!         send_err ( &msg, ECA_TOLARGE, client,
//!             "CAS: Server unable to load large request message. Max bytes=%lu",
//!             rsrvSizeofLargeBufTCP );
//!         client->recvBytesToDrain = msgsize - bytes_left;
//!         client->recv.stk = client->recv.cnt;
//!         status = RSRV_OK;
//!         break;
//!     }
//! }
//! ```
//!
//! The ceiling is `rsrvSizeofLargeBufTCP`, derived from
//! `EPICS_CA_MAX_ARRAY_BYTES` at `caservertask.c:510-532`. The refusal is
//! "reply, remember how much of the body has not landed yet, throw those
//! bytes away as they arrive, keep every channel and subscription" — so a
//! single oversize `caput` costs the client one message, not its circuit.
//!
//! # Why a type rather than a check
//!
//! There are two receive loops — the async host driver (`server::tcp`) and
//! the blocking driver RTEMS runs (`server::blocking`) — and the same defect
//! can exist independently in each. A check written twice is a check that
//! will be added once. Here the buffer is private, the only way to grow it
//! is [`RecvAccumulator::accept`], which enforces the cap, and the only way
//! to skip a message is [`RecvAccumulator::refuse`], which owns the drain
//! counter. Both loops get the rule by construction.

/// What [`RecvAccumulator::accept`] left for the caller to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Admit {
    /// Bytes are buffered and no refusal is outstanding — parse.
    Parse,
    /// Everything just read belonged to a message already refused, and more
    /// of it is still owed. Read again; there is nothing to parse.
    Draining,
    /// The buffer would have grown past the server's own ceiling. Nothing
    /// was appended; the circuit must close. Carries the ceiling for the
    /// diagnostic.
    Overflow(usize),
}

/// What [`RecvAccumulator::refuse`] left for the parse loop to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refused {
    /// The whole body was already buffered — nothing to drain from future
    /// reads. Resume parsing at this offset.
    ResumeAt(usize),
    /// The body has not fully arrived. Everything still in the buffer
    /// belongs to the refused message, and the drain counter carries the
    /// shortfall for the next [`RecvAccumulator::accept`].
    DrainPending,
}

/// A per-client receive buffer that cannot be grown by a peer.
#[derive(Debug, Default)]
pub(crate) struct RecvAccumulator {
    buf: Vec<u8>,
    /// C `client->recvBytesToDrain` (`camessage.c:2375-2384`): bytes of an
    /// already-refused message that have not arrived yet and must be thrown
    /// away as they land. Written only by [`Self::refuse`], consumed only
    /// by [`Self::accept`].
    to_drain: usize,
}

impl RecvAccumulator {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The largest message **body** this server will hold for one message.
    ///
    /// C `rsrvSizeofLargeBufTCP` (`caservertask.c:510-532`), which is
    /// `EPICS_CA_MAX_ARRAY_BYTES` plus the header allowance — so comparing
    /// bodies here is the same comparison C makes on whole messages.
    pub(crate) fn body_ceiling() -> usize {
        crate::protocol::max_frame_body_bytes()
    }

    /// C `camessage.c:2471-2473`: can the receive buffer ever hold a message
    /// whose declared body is this long?
    ///
    /// `Err(ceiling)` means no, and the caller owes the peer exactly what C
    /// sends at `:2474-2488` — `ECA_TOLARGE` naming the ceiling, then
    /// [`Self::refuse`] — rather than a disconnect.
    ///
    /// Applied to the declared body of **every** message, normal form and
    /// extended alike, so there is no boundary at which the rule changes. A
    /// normal-form header cannot exceed the ceiling in practice (`m_postsize`
    /// is a `u16`), but making that a consequence of the size rather than a
    /// special case is what keeps a future header form from arriving
    /// unguarded.
    pub(crate) fn admits_body(declared_body: usize) -> Result<(), usize> {
        let ceiling = Self::body_ceiling();
        if declared_body > ceiling {
            Err(ceiling)
        } else {
            Ok(())
        }
    }

    /// The ceiling on the accumulation buffer itself.
    ///
    /// MUST be at least the largest legal single frame, or a valid large
    /// waveform would trip the guard before it could be dispatched — a
    /// permanent failure that survives reconnect. So: one maximum body, plus
    /// the 24-byte extended header that carries it, plus 64 KiB of slack so a
    /// partially-received *next* frame pipelined behind a full one in the
    /// same read burst does not trip it either. A buffer longer than that
    /// cannot be explained by a legal peer.
    pub(crate) fn accumulation_ceiling() -> usize {
        Self::body_ceiling()
            .saturating_add(24)
            .saturating_add(64 * 1024)
    }

    /// Take `chunk` from the socket.
    ///
    /// The single growth point. Runs C's drain preamble
    /// (`camessage.c:2375-2384`) first — bytes owed to an already-refused
    /// message are discarded before any header parsing, so a refused ~4 GiB
    /// body costs no memory however slowly it is dribbled — then enforces
    /// [`Self::accumulation_ceiling`] before appending.
    pub(crate) fn accept(&mut self, chunk: &[u8]) -> Admit {
        let chunk = if self.to_drain > 0 {
            let drop_now = self.to_drain.min(chunk.len());
            self.to_drain -= drop_now;
            &chunk[drop_now..]
        } else {
            chunk
        };
        if self.to_drain > 0 {
            // The whole read belonged to the refused message and more is
            // owed. Nothing was appended, so the buffer did not grow.
            return Admit::Draining;
        }
        let ceiling = Self::accumulation_ceiling();
        if self.buf.len().saturating_add(chunk.len()) > ceiling {
            return Admit::Overflow(ceiling);
        }
        self.buf.extend_from_slice(chunk);
        Admit::Parse
    }

    /// The bytes available to parse.
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.buf
    }

    pub(crate) fn len(&self) -> usize {
        self.buf.len()
    }

    /// Discard the first `upto` bytes — the messages the parse loop finished
    /// with.
    pub(crate) fn consume(&mut self, upto: usize) {
        if upto > 0 {
            self.buf.drain(..upto.min(self.buf.len()));
        }
    }

    /// Refuse the message that starts at `offset` **without tearing the
    /// circuit down**: discard exactly its `msg_len` bytes and let the
    /// stream resume at the next message.
    ///
    /// The single owner of the drain counter. C refuses the same way at both
    /// of its refuse-but-keep-serving sites — `ECA_DEFUNCT`
    /// (`camessage.c:2438-2439`) and `ECA_TOLARGE` (`:2484-2486`): the error
    /// goes out, `recvBytesToDrain` remembers the part of the body that has
    /// not arrived, and the drain preamble throws those bytes away as they
    /// land. Neither closes the connection.
    ///
    /// The accounting is exact in both directions, which is why callers never
    /// reason about the counter: a body already fully buffered leaves nothing
    /// to drain and parsing resumes in-buffer, while a short body carries
    /// only the shortfall forward.
    pub(crate) fn refuse(&mut self, offset: usize, msg_len: usize) -> Refused {
        let arrived = self.buf.len().saturating_sub(offset);
        match msg_len.checked_sub(arrived) {
            None | Some(0) => Refused::ResumeAt(offset + msg_len),
            Some(shortfall) => {
                self.to_drain = shortfall;
                Refused::DrainPending
            }
        }
    }

    /// Bytes still owed to a refused message. Observation only — no caller
    /// may set it, and the loops never need to read it.
    #[cfg(test)]
    pub(crate) fn drain_pending(&self) -> usize {
        self.to_drain
    }
}

/// C's text at `camessage.c:2477-2478`, with the ceiling it names.
pub(crate) fn too_large_message(ceiling: usize) -> String {
    format!("CAS: Server unable to load large request message. Max bytes={ceiling}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Boundary values of the growth guard, one case per boundary rather
    /// than one per story: under the ceiling, exactly at it, one byte over,
    /// and over reached by two reads that are each individually small.
    #[test]
    fn accumulation_stops_exactly_at_the_ceiling() {
        let ceiling = RecvAccumulator::accumulation_ceiling();

        let mut acc = RecvAccumulator::new();
        assert_eq!(acc.accept(&vec![0u8; ceiling - 1]), Admit::Parse);
        assert_eq!(acc.len(), ceiling - 1);

        let mut acc = RecvAccumulator::new();
        assert_eq!(acc.accept(&vec![0u8; ceiling]), Admit::Parse);
        assert_eq!(acc.len(), ceiling);

        let mut acc = RecvAccumulator::new();
        assert_eq!(
            acc.accept(&vec![0u8; ceiling + 1]),
            Admit::Overflow(ceiling)
        );
        assert_eq!(acc.len(), 0, "an overflowing read must append nothing");

        // Reached incrementally: the guard is on the total, not the read.
        let mut acc = RecvAccumulator::new();
        assert_eq!(acc.accept(&vec![0u8; ceiling]), Admit::Parse);
        assert_eq!(acc.accept(&[0u8]), Admit::Overflow(ceiling));
        assert_eq!(acc.len(), ceiling);
    }

    /// The declared-size gate, at its boundary. The ceiling is the body, so
    /// a body exactly at it is admitted and one byte more is not.
    #[test]
    fn declared_bodies_are_admitted_up_to_the_ceiling_and_no_further() {
        let ceiling = RecvAccumulator::body_ceiling();
        assert_eq!(RecvAccumulator::admits_body(0), Ok(()));
        assert_eq!(RecvAccumulator::admits_body(ceiling - 1), Ok(()));
        assert_eq!(RecvAccumulator::admits_body(ceiling), Ok(()));
        assert_eq!(RecvAccumulator::admits_body(ceiling + 1), Err(ceiling));
        // The v4.9 extended form's worst case: a `u32` postsize.
        assert_eq!(
            RecvAccumulator::admits_body(u32::MAX as usize),
            Err(ceiling)
        );
    }

    /// The dribble: a peer declares a ~4 GiB body, is refused, then sends
    /// the body a few bytes at a time. Every one of those bytes must be
    /// discarded on arrival — the buffer must not grow at all.
    #[test]
    fn a_refused_body_costs_no_memory_however_slowly_it_arrives() {
        let huge = u32::MAX as usize;
        let mut acc = RecvAccumulator::new();

        // 24-byte extended header lands; the body is refused before any of
        // it is believed.
        assert_eq!(acc.accept(&[0u8; 24]), Admit::Parse);
        assert_eq!(
            RecvAccumulator::admits_body(huge),
            Err(RecvAccumulator::body_ceiling())
        );
        let msg_len = 24 + huge;
        assert_eq!(acc.refuse(0, msg_len), Refused::DrainPending);
        assert_eq!(acc.drain_pending(), huge);
        // The header itself is still buffered; the loop consumes it.
        acc.consume(acc.len());
        assert_eq!(acc.len(), 0);

        // Now dribble. 4096 reads of 8 bytes: the buffer stays empty and
        // the counter comes down by exactly what arrived.
        let mut delivered = 0usize;
        for _ in 0..4096 {
            assert_eq!(acc.accept(&[0u8; 8]), Admit::Draining);
            delivered += 8;
            assert_eq!(acc.len(), 0, "a refused body must never be buffered");
            assert_eq!(acc.drain_pending(), huge - delivered);
        }
    }

    /// The drain's own boundary: the read that finishes the outstanding
    /// drain and carries the first bytes of the next message must buffer
    /// exactly those bytes and no more.
    #[test]
    fn a_read_that_ends_a_drain_keeps_only_the_bytes_past_it() {
        let mut acc = RecvAccumulator::new();
        acc.accept(&[0u8; 16]);
        // Declared 100-byte message, 16 arrived → 84 owed.
        assert_eq!(acc.refuse(0, 100), Refused::DrainPending);
        assert_eq!(acc.drain_pending(), 84);
        acc.consume(acc.len());

        // 84 tail bytes plus 5 bytes of the next message.
        let mut chunk = vec![0u8; 84];
        chunk.extend_from_slice(&[1, 2, 3, 4, 5]);
        assert_eq!(acc.accept(&chunk), Admit::Parse);
        assert_eq!(acc.drain_pending(), 0);
        assert_eq!(acc.bytes(), &[1, 2, 3, 4, 5]);

        // Exactly-consuming read: nothing owed, nothing buffered, and the
        // caller is told to parse (an empty buffer parses to nothing).
        let mut acc = RecvAccumulator::new();
        acc.accept(&[0u8; 16]);
        assert_eq!(acc.refuse(0, 100), Refused::DrainPending);
        acc.consume(acc.len());
        assert_eq!(acc.accept(&[0u8; 84]), Admit::Parse);
        assert_eq!(acc.drain_pending(), 0);
        assert_eq!(acc.len(), 0);
    }

    /// [`RecvAccumulator::refuse`] accounts exactly at every boundary of
    /// "how much of this message has arrived".
    #[test]
    fn refuse_accounts_exactly_at_every_boundary() {
        // Short body: only the shortfall is carried forward.
        let mut acc = RecvAccumulator::new();
        acc.accept(&[0u8; 100]);
        assert_eq!(acc.refuse(0, 4128), Refused::DrainPending);
        assert_eq!(acc.drain_pending(), 4028);

        // Exactly buffered: nothing to drain, parsing resumes in-buffer.
        let mut acc = RecvAccumulator::new();
        acc.accept(&vec![0u8; 4128]);
        assert_eq!(acc.refuse(0, 4128), Refused::ResumeAt(4128));
        assert_eq!(acc.drain_pending(), 0);

        // Over-buffered: the next message's bytes are already here.
        let mut acc = RecvAccumulator::new();
        acc.accept(&vec![0u8; 4144]);
        assert_eq!(acc.refuse(0, 4128), Refused::ResumeAt(4128));
        assert_eq!(acc.drain_pending(), 0);

        // Non-zero offset: `arrived` is measured from the message, not the
        // buffer start.
        let mut acc = RecvAccumulator::new();
        acc.accept(&vec![0u8; 4160]);
        assert_eq!(acc.refuse(16, 4128), Refused::ResumeAt(4144));
        assert_eq!(acc.drain_pending(), 0);

        // One byte short of the body is still a drain, of exactly one byte.
        let mut acc = RecvAccumulator::new();
        acc.accept(&vec![0u8; 4127]);
        assert_eq!(acc.refuse(0, 4128), Refused::DrainPending);
        assert_eq!(acc.drain_pending(), 1);
    }
}
