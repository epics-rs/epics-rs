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
//! `EPICS_CA_MAX_ARRAY_BYTES` at `caservertask.c:511-533`. The refusal is
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
//! to advance past a message is [`RecvAccumulator::next_message`], which owns
//! the drain counter and the parse cursor. Both loops get the rule by
//! construction.
//!
//! # The protocol gates, and why they live here too
//!
//! Bounding the buffer was only the first rule the two loops held
//! independently. C `camessage.c` runs a fixed sequence of tests between
//! "a header parsed" and "dispatch this message", and each test has one of
//! two outcomes that are *not* interchangeable:
//!
//! | gate | C site | reply | outcome |
//! |---|---|---|---|
//! | `msgsize` not representable | `4128a7c07:2459-2466` | none | `RSRV_ERROR` — circuit torn down |
//! | peer version too old | `:2427-2446` | `ECA_DEFUNCT` | `RSRV_OK` + drain — circuit kept |
//! | misaligned `msgsize` | `:2452-2463` | `ECA_INTERNAL` | `RSRV_ERROR` — circuit torn down |
//! | declared body over the ceiling | `:2471-2489` | `ECA_TOLARGE` | `RSRV_OK` + drain — circuit kept |
//! | opcode outside `tcpJumpTable` | `:337-352` | `ECA_INTERNAL` | `RSRV_ERROR` — circuit torn down |
//!
//! Every unqualified `:N` above is `camessage.c` at the `R7.0.10` pin. The
//! first row is the exception: that guard is not in `R7.0.10` at all — it
//! arrives 141 commits later in `4128a7c07`, which is why that row alone
//! names a commit. Its position in C's order is that commit's, inside the
//! extended-header branch and therefore ahead of every row below it.
//!
//! The rows are in C's order, and that order is part of the wire contract,
//! not an implementation detail: a frame that is *both* misaligned and
//! oversize gets `ECA_INTERNAL` and a close from C, and answering it
//! `ECA_TOLARGE` and staying up is a different protocol. So the order lives
//! in one place —
//! [`RecvAccumulator::next_message`] — and neither loop can reorder it,
//! because neither loop can see the individual gates at all. What a caller
//! gets back is [`Gate`], whose variants *are* the two outcomes: a
//! [`Gate::Refuse`] cannot be mistaken for a [`Gate::TearDown`] the way two
//! hand-copied `if` blocks could.
//!
//! One gate here is ours and not C's: the per-client message-rate policy
//! (`EPICS_CAS_RATE_LIMIT_*`, `server::rate_limit`). It runs *after* every
//! row above, on a message C would have dispatched, so the C order is
//! untouched — what it decides is whether this server hands that message on.
//! It lives here for the same reason the rest do: it was written in the async
//! loop alone, and the blocking driver RTEMS runs — the target with the least
//! memory and no other defence — silently ignored the three documented
//! variables for as long as the check had two possible homes.

use epics_base_rs::error::CaError;

use crate::protocol::{
    BAD_TCP_COMMAND_DIAGNOSTIC, CA_MINIMUM_SUPPORTED_VERSION, CA_PROTO_VERSION, CaHeader,
    ECA_DEFUNCT, ECA_INTERNAL, ECA_TOLARGE, EXTENDED_EXTRA, ca_v49, is_legal_tcp_command,
};
use crate::server::rate_limit::{RateLimitConfig, RateLimiter};

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
///
/// Private to this module on purpose: `refuse` is reached only through
/// [`RecvAccumulator::next_message`], so no receive loop can decide on its own
/// that a message is refusable, nor forget the cursor bookkeeping that follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refused {
    /// The whole body was already buffered — nothing to drain from future
    /// reads. Resume parsing at this offset.
    ResumeAt(usize),
    /// The body has not fully arrived. Everything still in the buffer
    /// belongs to the refused message, and the drain counter carries the
    /// shortfall for the next [`RecvAccumulator::accept`].
    DrainPending,
}

/// The `CA_PROTO_ERROR` a gate decided the peer is owed, ready to hand to
/// `send_ca_error`. C builds the same three things at every `send_err` call
/// site: the request header to echo, the ECA status, and the diagnostic.
#[derive(Debug, Clone)]
pub(crate) struct GateError {
    /// The request header to echo back, per C `vsend_err`.
    pub(crate) hdr: CaHeader,
    /// `caerr.h` status — `ECA_TOLARGE`, `ECA_INTERNAL`, …
    pub(crate) status: u32,
    /// C's own diagnostic text for this gate, verbatim.
    pub(crate) diagnostic: String,
}

/// What [`RecvAccumulator::next_message`] decided about the bytes at the parse
/// cursor — **the one place C's `RSRV_OK`/`RSRV_ERROR` distinction is
/// represented in this crate.**
///
/// The two failure variants are deliberately not one variant plus a flag: C's
/// refuse-and-keep-serving and refuse-and-close differ in what the peer is
/// left holding (every channel and subscription, versus nothing), and a
/// caller that has to read a `bool` to tell them apart is a caller that can
/// read it wrong. Both receive loops match on this enum, so both answer every
/// gate the same way or neither compiles.
#[derive(Debug)]
pub(crate) enum Gate {
    /// Nothing further can be decided from what is buffered. The parsed
    /// prefix has already been discarded; read more bytes from the socket.
    NeedMore,
    /// A complete, gate-passing message. The cursor has already advanced past
    /// it, so a caller cannot mis-advance it — dispatch and come back.
    Deliver { hdr: CaHeader, payload: Vec<u8> },
    /// C `RSRV_OK` after a `send_err`: answer the peer and **keep serving**.
    /// The refused message is already skipped — drained across later reads if
    /// its body has not fully arrived — so the caller sends the error and
    /// asks for the next message.
    Refuse(GateError),
    /// C `RSRV_ERROR`: answer the peer if there is an answer, then **end the
    /// circuit**. `reason` is what the connection handler returns.
    TearDown {
        error: Option<GateError>,
        reason: CaError,
    },
    /// Ours, not C's: the per-client rate policy had no token for this
    /// message. The cursor is past it exactly as for [`Gate::Deliver`], so
    /// the caller simply asks for the next one; the peer keeps every channel
    /// and subscription and is told nothing, because C has no reply for
    /// "slow down" and libca has no status to carry one.
    Discard,
    /// Ours, not C's: consecutive [`Gate::Discard`]s reached
    /// `EPICS_CAS_RATE_LIMIT_STRIKES`. End the circuit — as a policy
    /// disconnect, not an error, which is why this is not a
    /// [`Gate::TearDown`] with a `reason`.
    RateLimited { strikes: u32 },
}

/// A per-client receive buffer that cannot be grown by a peer, and cannot be
/// parsed except through the gate sequence.
#[derive(Debug, Default)]
pub(crate) struct RecvAccumulator {
    buf: Vec<u8>,
    /// C `client->recvBytesToDrain` (`camessage.c:2375-2384`): bytes of an
    /// already-refused message that have not arrived yet and must be thrown
    /// away as they land. Written only by [`Self::refuse`], consumed only
    /// by [`Self::accept`].
    to_drain: usize,
    /// C `client->recv.stk`: how far into `buf` the gate has parsed. Written
    /// only by [`Self::next_message`]. A receive loop never sees it, which is
    /// why a refused or misframed message cannot leave the cursor off an
    /// 8-byte boundary and de-sync every later frame on the circuit.
    offset: usize,
    /// This circuit's token bucket, or `None` when the policy is disabled
    /// (the default). Per-connection, like the buffer around it.
    rate: Option<RateLimiter>,
    /// Consecutive messages the bucket had no token for. Reset by the first
    /// message that does get one, so a peer that is merely bursty never
    /// reaches the threshold.
    rate_strikes: u32,
    /// `EPICS_CAS_RATE_LIMIT_STRIKES`; zero never disconnects.
    rate_strike_threshold: u32,
}

impl RecvAccumulator {
    /// A receive buffer carrying this server's configured rate policy. Every
    /// receive loop builds its buffer through here, so no loop can be written
    /// that reads a socket without the policy attached.
    pub(crate) fn new() -> Self {
        Self::with_rate_limit(&RateLimitConfig::from_env())
    }

    /// [`Self::new`] with the policy passed in rather than read from the
    /// environment — for tests, which must not depend on process-wide state.
    pub(crate) fn with_rate_limit(cfg: &RateLimitConfig) -> Self {
        Self {
            rate: cfg.build(),
            rate_strike_threshold: cfg.strike_threshold,
            ..Self::default()
        }
    }

    /// Gate 10, and the only one that is not C's: draw this message's token.
    ///
    /// Called with the cursor already past the message, so both outcomes
    /// leave the buffer where [`Gate::Deliver`] would have.
    fn admit_rate(&mut self) -> Option<Gate> {
        let limiter = self.rate.as_ref()?;
        if limiter.try_acquire().is_ok() {
            self.rate_strikes = 0;
            return None;
        }
        metrics::counter!("ca_server_rate_limit_drops_total").increment(1);
        self.rate_strikes = self.rate_strikes.saturating_add(1);
        if self.rate_strike_threshold > 0 && self.rate_strikes >= self.rate_strike_threshold {
            metrics::counter!("ca_server_rate_limit_disconnects_total").increment(1);
            return Some(Gate::RateLimited {
                strikes: self.rate_strikes,
            });
        }
        Some(Gate::Discard)
    }

    /// The largest message **body** this server will hold for one message.
    ///
    /// C `rsrvSizeofLargeBufTCP` (`caservertask.c:511-533`), which is
    /// `EPICS_CA_MAX_ARRAY_BYTES` plus the header allowance — so comparing
    /// bodies here is the same comparison C makes on whole messages.
    pub(crate) fn body_ceiling() -> usize {
        crate::protocol::max_frame_body_bytes()
    }

    /// C `camessage.c:2471-2473`: can the receive buffer ever hold a message
    /// whose declared body is this long?
    ///
    /// `Err(ceiling)` means no, and the caller owes the peer exactly what C
    /// sends at `:2473-2487` — `ECA_TOLARGE` naming the ceiling, then
    /// [`Self::refuse`] — rather than a disconnect.
    ///
    /// Applied to the declared body of **every** message, normal form and
    /// extended alike, so there is no boundary at which the rule changes. A
    /// normal-form header cannot exceed the ceiling in practice (`m_postsize`
    /// is a `u16`), but making that a consequence of the size rather than a
    /// special case is what keeps a future header form from arriving
    /// unguarded.
    fn admits_body(declared_body: usize) -> Result<(), usize> {
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

    /// The bytes available to parse. Observation only — the gate reads the
    /// buffer directly, and no receive loop may.
    #[cfg(test)]
    fn bytes(&self) -> &[u8] {
        &self.buf
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.buf.len()
    }

    /// Discard the first `upto` bytes — the messages the gate finished with.
    fn consume(&mut self, upto: usize) {
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
    /// (`camessage.c:2432-2440`) and `ECA_TOLARGE` (`:2475-2487`): the error
    /// goes out, `recvBytesToDrain` remembers the part of the body that has
    /// not arrived, and the drain preamble throws those bytes away as they
    /// land. Neither closes the connection.
    ///
    /// The accounting is exact in both directions, which is why callers never
    /// reason about the counter: a body already fully buffered leaves nothing
    /// to drain and parsing resumes in-buffer, while a short body carries
    /// only the shortfall forward.
    fn refuse(&mut self, offset: usize, msg_len: usize) -> Refused {
        let arrived = self.buf.len().saturating_sub(offset);
        match msg_len.checked_sub(arrived) {
            None | Some(0) => Refused::ResumeAt(offset + msg_len),
            Some(shortfall) => {
                self.to_drain = shortfall;
                Refused::DrainPending
            }
        }
    }

    /// **The CA server's one TCP gate sequence**, run by the async loop
    /// (`server::tcp::handle_client`) and by the blocking loop the RTEMS and
    /// VxWorks drivers use (`server::blocking::serve_client`).
    ///
    /// Decides what the bytes at the parse cursor are, in C `camessage.c`'s
    /// order, and advances the cursor by exactly what it decided. A caller
    /// gets a [`Gate`] and nothing else: it cannot see the individual tests,
    /// cannot reorder them, cannot skip one, and cannot move the cursor
    /// itself. That is the whole point — the previous arrangement was two
    /// hand-maintained lists of `if` blocks in two files, and they had
    /// already drifted twice.
    ///
    /// The sequence, with C's site and outcome for each step:
    ///
    /// 1. fewer than 16 bytes buffered → `RSRV_OK` break, wait
    ///    (`camessage.c:2397-2400`)
    /// 2. V49 peer, `m_postsize == 0xffff`, fewer than 24 bytes → wait for
    ///    the annex (`camessage.c:2410-2415`). Gated on V49 exactly as C
    ///    gates the whole branch, so a pre-V49 peer's `0xffff` stays a
    ///    normal-form postsize and falls through to the alignment test.
    /// 3. the header parser rejects 16 present bytes → tear down. C's parser
    ///    cannot fail here, and steps 1-2 exclude both inputs ours rejects,
    ///    so this is unreachable today; it is C's close rather than an
    ///    `unreachable!` because a parser that later grows a rejection must
    ///    reach the peer-is-malformed path, not panic a client's task.
    /// 4. non-VERSION message from a peer below
    ///    `CA_MINIMUM_SUPPORTED_VERSION` → `ECA_DEFUNCT` "CAS: Client
    ///    version %u too old", `RSRV_OK` + `recvBytesToDrain`
    ///    (`camessage.c:2427-2446`)
    /// 5. declared body over [`Self::body_ceiling`] → `ECA_TOLARGE`,
    ///    `RSRV_OK` + `recvBytesToDrain` (`camessage.c:2471-2489`)
    /// 6. `msgsize & 0x7` → `ECA_INTERNAL` "CAS: Missaligned protocol
    ///    rejected", `RSRV_ERROR` (`camessage.c:2452-2463`)
    /// 7. body not fully arrived → `RSRV_OK` break, wait
    ///    (`camessage.c:2495-2498`)
    /// 8. opcode outside `tcpJumpTable`'s legal slots → `ECA_INTERNAL`
    ///    "invalid (damaged?) request code from TCP", `RSRV_ERROR`
    ///    (`camessage.c:337-352`, dispatched at `:2519-2529`)
    ///
    /// Step 5 is tested on `actual_postsize()` before `hdr_size + body` is
    /// formed anywhere, because `usize` is 32-bit on RTEMS and a V49 peer can
    /// declare a body near `u32::MAX`: that sum wraps, and every later use of
    /// it would be meaningless. Step 6 is tested on the body alone rather
    /// than on the total, which is the same test — both header forms are 8-byte
    /// multiples, so `msgsize & 7 == m_postsize & 7`.
    pub(crate) fn next_message(&mut self, client_minor: u16) -> Gate {
        let head = &self.buf[self.offset..];

        // 1. A complete `caHdr` has not arrived.
        if head.len() < CaHeader::SIZE {
            self.compact();
            return Gate::NeedMore;
        }

        // 2. A partial extended-form header at a TCP segment boundary. C
        //    waits; parsing it would fail and close a circuit on a benign
        //    boundary.
        if ca_v49(client_minor) && head.len() < 24 && head[2] == 0xFF && head[3] == 0xFF {
            self.compact();
            return Gate::NeedMore;
        }

        // 3. Header parse.
        let (hdr, hdr_size) = match CaHeader::from_bytes_for_peer(head, client_minor) {
            Ok(v) => v,
            Err(e) => {
                return Gate::TearDown {
                    error: None,
                    reason: e,
                };
            }
        };
        let body = hdr.actual_postsize();

        // 4. C forms `msgsize` here, ahead of every gate below, and refuses
        //    a declared body it cannot add a header to. That refusal is
        //    silent: the peer gets the close and no error frame.
        let Some(msg_len) = Self::message_len(hdr_size, body) else {
            return Gate::TearDown {
                error: None,
                reason: CaError::Protocol(format!(
                    "declared CA body of {body} bytes has no representable message length"
                )),
            };
        };

        // 5. A peer too old to speak a protocol this server still serves.
        //    Refuse the message and keep the circuit: C is explicit that the
        //    connection stays open "to avoid a re-connect loop". CA_PROTO_VERSION
        //    itself is exempt, which is how a peer raises its version at all.
        if hdr.cmmd != CA_PROTO_VERSION && client_minor < CA_MINIMUM_SUPPORTED_VERSION {
            self.skip_refused(msg_len);
            return Gate::Refuse(GateError {
                hdr,
                status: ECA_DEFUNCT,
                diagnostic: format!("CAS: Client version {client_minor} too old"),
            });
        }

        // 6. Misalignment: tear the circuit down. C's comment is explicit
        //    that clients are not expected to recover, and the alternative —
        //    advancing the cursor by a non-multiple of 8 — mis-frames every
        //    later message on the connection for the life of the socket.
        //    Ahead of the ceiling test, because a frame that is both
        //    misaligned and oversize is a torn-down circuit in C, not a
        //    refusal the peer survives.
        if msg_len & 0x7 != 0 {
            return Gate::TearDown {
                error: Some(GateError {
                    hdr,
                    status: ECA_INTERNAL,
                    diagnostic: MISALIGNED_DIAGNOSTIC.to_string(),
                }),
                reason: CaError::Protocol("misaligned CA payload".into()),
            };
        }

        // 7. Oversize: refuse this message, keep the circuit.
        if let Err(ceiling) = Self::admits_body(body) {
            self.skip_refused(msg_len);
            return Gate::Refuse(GateError {
                hdr,
                status: ECA_TOLARGE,
                diagnostic: too_large_message(ceiling),
            });
        }

        // 8. The body has not fully arrived.
        if self.offset + msg_len > self.buf.len() {
            self.compact();
            return Gate::NeedMore;
        }

        // 9. C dispatches through `tcpJumpTable` (`camessage.c:2519-2525`)
        //    only now, with a whole message in hand. Every illegal index
        //    lands on `bad_tcp_cmd_action`: `ECA_INTERNAL` and `RSRV_ERROR`,
        //    with C's own comment saying clients are not expected to
        //    recover. Advancing past the message first keeps the wire
        //    accounting identical to the delivered case.
        self.offset += msg_len;
        if !is_legal_tcp_command(hdr.cmmd) {
            return Gate::TearDown {
                error: Some(GateError {
                    hdr,
                    status: ECA_INTERNAL,
                    diagnostic: BAD_TCP_COMMAND_DIAGNOSTIC.to_string(),
                }),
                reason: CaError::Protocol(format!(
                    "illegal TCP command {} (C bad_tcp_cmd_action)",
                    hdr.cmmd
                )),
            };
        }

        // 10. Ours, not C's, and last for that reason: C would dispatch this
        //     message, so anything this gate does is this server's own policy
        //     rather than a difference in how the protocol is read.
        if let Some(gate) = self.admit_rate() {
            return gate;
        }

        let payload = self.buf[self.offset - msg_len + hdr_size..self.offset].to_vec();
        Gate::Deliver { hdr, payload }
    }

    /// C's `msgsize` — header plus declared body — formed at
    /// `camessage.c:2418` for the extended header and `:2422` for the normal
    /// one, or `None` where C breaks with `RSRV_ERROR` because the sum does
    /// not fit the `ca_uint32_t` it is formed in.
    ///
    /// That refusal is not at the `R7.0.10` pin: upstream added the
    /// `ca_uint32_max` guard in `4128a7c07` ("rsrv: cross-check m_count
    /// message field with payload buffer length", 141 commits past the tag,
    /// on `7.0`). This port carries it, so the citation names the commit
    /// rather than a line the pinned revision does not have.
    ///
    /// Every gate below the parse reads this one number, so it has to mean
    /// the message's real length on all of them; a clamped stand-in would
    /// hand the drain accounting a length shorter than the body it is
    /// draining. C's bound is 32 bits wide by construction, which is also
    /// what keeps the add from wrapping where `usize` is 32 bits — RTEMS and
    /// VxWorks, the two targets the blocking loop was written for.
    fn message_len(hdr_size: usize, declared_body: usize) -> Option<usize> {
        const LIMIT: usize = u32::MAX as usize - (CaHeader::SIZE + EXTENDED_EXTRA);
        if declared_body >= LIMIT {
            None
        } else {
            Some(hdr_size + declared_body)
        }
    }

    /// Drop the bytes the gate has finished with and rewind the cursor.
    ///
    /// Runs at exactly the moment the parse loop must go back to the socket,
    /// so `accept` is only ever called with the cursor at zero and the buffer
    /// never retains a parsed prefix across reads.
    fn compact(&mut self) {
        let parsed = self.offset;
        self.offset = 0;
        self.consume(parsed);
    }

    /// Advance past a message the gate refused, through [`Self::refuse`].
    fn skip_refused(&mut self, msg_len: usize) {
        match self.refuse(self.offset, msg_len) {
            Refused::ResumeAt(next) => self.offset = next,
            // Everything buffered belongs to the refused message and the
            // drain counter carries the rest; the next call compacts to
            // empty and asks for more bytes.
            Refused::DrainPending => self.offset = self.buf.len(),
        }
    }

    /// Bytes still owed to a refused message. Observation only — no caller
    /// may set it, and the loops never need to read it.
    #[cfg(test)]
    pub(crate) fn drain_pending(&self) -> usize {
        self.to_drain
    }
}

/// C's text at `camessage.c:2456`.
pub(crate) const MISALIGNED_DIAGNOSTIC: &str = "CAS: Missaligned protocol rejected";

/// C's text at `camessage.c:2477`, with the ceiling it names.
///
/// `pub(crate)` only so the driver parity tests can state the expected wire
/// text once; the gate is the sole production caller.
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

    /// `n` back-to-back bodiless `CA_PROTO_VERSION` frames — the smallest
    /// message that passes every C gate, so what the rate gate does to it is
    /// all that is under test.
    fn frames(n: usize) -> Vec<u8> {
        let mut buf = Vec::new();
        for _ in 0..n {
            buf.extend_from_slice(&CaHeader::new(CA_PROTO_VERSION).to_bytes());
        }
        buf
    }

    /// A bucket of exactly `burst` tokens. `msgs_per_sec` is the slowest
    /// non-zero refill the config admits — one token per second — so no
    /// token can return during a test that runs in microseconds.
    fn policy(burst: u64, strike_threshold: u32) -> RateLimitConfig {
        RateLimitConfig {
            msgs_per_sec: 1,
            burst,
            strike_threshold,
        }
    }

    fn drain_gates(acc: &mut RecvAccumulator, n: usize) -> Vec<Gate> {
        (0..n)
            .map(|_| acc.next_message(CA_MINIMUM_SUPPORTED_VERSION))
            .collect()
    }

    /// The default: no policy, so the gate is not in the path at all.
    #[test]
    fn a_disabled_policy_delivers_every_message() {
        let mut acc = RecvAccumulator::with_rate_limit(&RateLimitConfig::default());
        assert_eq!(acc.accept(&frames(8)), Admit::Parse);
        for gate in drain_gates(&mut acc, 8) {
            assert!(matches!(gate, Gate::Deliver { .. }), "{gate:?}");
        }
        assert!(matches!(
            acc.next_message(CA_MINIMUM_SUPPORTED_VERSION),
            Gate::NeedMore
        ));
    }

    /// An empty bucket costs the peer the message and nothing else — and the
    /// cursor is past it, so the same bytes cannot come back as the next
    /// message. That is the boundary a caller could get wrong if the gate
    /// lived in the receive loop.
    #[test]
    fn an_empty_bucket_discards_the_message_and_advances_the_cursor() {
        let mut acc = RecvAccumulator::with_rate_limit(&policy(1, 0));
        assert_eq!(acc.accept(&frames(2)), Admit::Parse);
        let gates = drain_gates(&mut acc, 2);
        assert!(matches!(gates[0], Gate::Deliver { .. }), "{:?}", gates[0]);
        assert!(matches!(gates[1], Gate::Discard), "{:?}", gates[1]);
        assert!(
            matches!(
                acc.next_message(CA_MINIMUM_SUPPORTED_VERSION),
                Gate::NeedMore
            ),
            "a discarded message must not be re-parsed"
        );
    }

    /// Strike boundary: the run reaches the threshold on the `n`th
    /// consecutive discard, not the one before or after.
    #[test]
    fn consecutive_discards_end_the_circuit_at_the_threshold() {
        let mut acc = RecvAccumulator::with_rate_limit(&policy(1, 3));
        assert_eq!(acc.accept(&frames(5)), Admit::Parse);
        let gates = drain_gates(&mut acc, 4);
        assert!(matches!(gates[0], Gate::Deliver { .. }), "{:?}", gates[0]);
        assert!(matches!(gates[1], Gate::Discard), "{:?}", gates[1]);
        assert!(matches!(gates[2], Gate::Discard), "{:?}", gates[2]);
        assert!(
            matches!(gates[3], Gate::RateLimited { strikes: 3 }),
            "{:?}",
            gates[3]
        );
    }

    /// Zero is the documented "drop, never disconnect" setting
    /// (`EPICS_CAS_RATE_LIMIT_STRIKES`), so the run may grow without bound.
    #[test]
    fn a_zero_threshold_never_ends_the_circuit() {
        let mut acc = RecvAccumulator::with_rate_limit(&policy(1, 0));
        assert_eq!(acc.accept(&frames(64)), Admit::Parse);
        for gate in drain_gates(&mut acc, 64).into_iter().skip(1) {
            assert!(matches!(gate, Gate::Discard), "{gate:?}");
        }
        assert_eq!(acc.rate_strikes, 63);
    }

    /// Only *consecutive* discards count: one message that draws a token
    /// puts the run back to zero, so a bursty peer never disconnects.
    #[test]
    fn a_delivered_message_resets_the_strike_run() {
        let mut acc = RecvAccumulator::with_rate_limit(&policy(1, 3));
        acc.rate_strikes = 2;
        assert_eq!(acc.accept(&frames(1)), Admit::Parse);
        assert!(matches!(
            acc.next_message(CA_MINIMUM_SUPPORTED_VERSION),
            Gate::Deliver { .. }
        ));
        assert_eq!(acc.rate_strikes, 0);
    }
}
