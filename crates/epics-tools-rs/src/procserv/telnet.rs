//! Minimal telnet IAC parser/encoder.
//!
//! C procServ uses libtelnet but only exercises a tiny subset:
//! - 5 functions: `telnet_init` / `negotiate` / `recv` / `send` / `free`
//! - 3 events: DATA, SEND, ERROR
//! - 2 telnet options: `WILL ECHO`, `DO LINEMODE`
//!
//! Vendoring libtelnet just for this is overkill, so we hand-roll a
//! ~100 LOC IAC state machine.
//!
//! ## Wire format reminder
//!
//! ```text
//! IAC = 0xFF
//! IAC IAC          → literal 0xFF byte (escape)
//! IAC <cmd>        → 2-byte command (e.g. NOP, AYT, BRK)
//! IAC <neg> <opt>  → 3-byte negotiation (WILL/WONT/DO/DONT + option)
//! IAC SB <opt> ... IAC SE  → subnegotiation (we only need to skip these)
//! ```

/// Telnet protocol bytes we care about.
#[allow(dead_code)]
pub mod codes {
    pub const IAC: u8 = 0xFF;
    pub const DONT: u8 = 0xFE;
    pub const DO: u8 = 0xFD;
    pub const WONT: u8 = 0xFC;
    pub const WILL: u8 = 0xFB;
    pub const SB: u8 = 0xFA;
    pub const SE: u8 = 0xF0;

    pub const TELOPT_ECHO: u8 = 0x01;
    pub const TELOPT_SGA: u8 = 0x03; // suppress-go-ahead
    pub const TELOPT_LINEMODE: u8 = 0x22;
}

/// Output of one feed into [`TelnetParser::feed`]. The supervisor
/// task forwards `Data` to the read task [`super::client::spawn_client`]
/// starts, and writes `Reply` back to the socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TelnetEvent {
    /// Plain user data (IAC sequences stripped, IAC-IAC unescaped).
    Data(Vec<u8>),
    /// Bytes to write back to the peer (responses to negotiations).
    Reply(Vec<u8>),
}

/// procServ's telnet option policy (`my_telopts`, `clientFactory.cc:25-30`):
/// we advertise `WILL ECHO` and request `DO LINEMODE`; every other option is
/// refused on both sides. `us_will` ⇒ we agree to enable the option locally;
/// `him_do` ⇒ we want the peer to enable it.
struct TelOptPolicy {
    opt: u8,
    us_will: bool,
    him_do: bool,
}

const TELOPTS: &[TelOptPolicy] = &[
    TelOptPolicy {
        opt: codes::TELOPT_ECHO,
        us_will: true,
        him_do: false,
    },
    TelOptPolicy {
        opt: codes::TELOPT_LINEMODE,
        us_will: false,
        him_do: true,
    },
];

/// libtelnet `_check_telopt` (`libtelnet.c:262-284`): is the option in our
/// table with the matching policy? `us` selects the local (`WILL`) policy,
/// `!us` the remote (`DO`) policy.
fn check_telopt(opt: u8, us: bool) -> bool {
    for t in TELOPTS {
        if t.opt == opt {
            return if us { t.us_will } else { t.him_do };
        }
    }
    false
}

/// RFC1143 per-option negotiation state (libtelnet `Q_*`,
/// `libtelnet.c:118-123`), restricted to the states reachable in procServ.
///
/// libtelnet's full set also has `WANTNO`, `WANTNO_OP`, and `WANTYES_OP`.
/// Those are entered only by the *outgoing* `telnet_negotiate` — `WANTNO` when
/// retracting an enabled option (`libtelnet.c:1286,1318`), the `_OP` pair when
/// a second offer for the same side races the first (`libtelnet.c:1273,1290`).
/// procServ offers each side once at startup (`WILL ECHO`, `DO LINEMODE`,
/// `clientFactory.cc:167-174`) and never re-negotiates or retracts, so from the
/// seeded `WANTYES` every incoming WILL/WONT/DO/DONT keeps each side within
/// `{No, Yes, WantYes}`. The omitted states are unreachable here.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum QState {
    #[default]
    No,
    Yes,
    WantYes,
}

/// RFC1143 state for one option: our side (`us`) and the peer's side (`him`).
#[derive(Debug, Clone, Copy)]
struct OptState {
    opt: u8,
    us: QState,
    him: QState,
}

/// Streaming IAC parser. Hold one per client socket.
#[derive(Debug)]
pub struct TelnetParser {
    state: ParseState,
    /// RFC1143 option states (libtelnet's `q` array). Grows as options are
    /// negotiated; absent ⇒ `(No, No)`.
    q: Vec<OptState>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum ParseState {
    #[default]
    Data,
    Iac,
    Negotiate(u8), // command (WILL/WONT/DO/DONT) recorded
    Subneg,
    SubnegIac,
}

impl Default for TelnetParser {
    fn default() -> Self {
        Self::new()
    }
}

impl TelnetParser {
    pub fn new() -> Self {
        let mut p = Self {
            state: ParseState::Data,
            q: Vec::new(),
        };
        // Mirror clientItem::clientItem's startup negotiation
        // (clientFactory.cc:167-174 → telnet_negotiate): for each policy
        // option, request DO (him) and/or advertise WILL (us). The write task
        // emits those offer bytes via initial_negotiation(); here we record
        // the matching RFC1143 transition (Q_NO → Q_WANTYES, libtelnet.c:1269,
        // 1301) so the peer's confirmations are recognised as confirmations,
        // not treated as fresh requests.
        for t in TELOPTS {
            let us = if t.us_will {
                QState::WantYes
            } else {
                QState::No
            };
            let him = if t.him_do {
                QState::WantYes
            } else {
                QState::No
            };
            if t.us_will || t.him_do {
                p.q.push(OptState {
                    opt: t.opt,
                    us,
                    him,
                });
            }
        }
        p
    }

    /// libtelnet `_get_rfc1143` (`libtelnet.c:287-303`): option state, or
    /// `(No, No)` if never negotiated.
    fn q_get(&self, opt: u8) -> (QState, QState) {
        for e in &self.q {
            if e.opt == opt {
                return (e.us, e.him);
            }
        }
        (QState::No, QState::No)
    }

    /// libtelnet `_set_rfc1143` (`libtelnet.c:306-...`): store option state.
    fn q_set(&mut self, opt: u8, us: QState, him: QState) {
        for e in &mut self.q {
            if e.opt == opt {
                e.us = us;
                e.him = him;
                return;
            }
        }
        self.q.push(OptState { opt, us, him });
    }

    /// Feed raw bytes from the socket; returns the events produced.
    pub fn feed(&mut self, input: &[u8]) -> Vec<TelnetEvent> {
        let mut events = Vec::new();
        let mut data_buf = Vec::with_capacity(input.len());
        let mut reply_buf = Vec::new();

        for &b in input {
            self.state = match self.state {
                ParseState::Data => {
                    if b == codes::IAC {
                        ParseState::Iac
                    } else {
                        data_buf.push(b);
                        ParseState::Data
                    }
                }
                ParseState::Iac => match b {
                    codes::IAC => {
                        // Escaped 0xFF byte.
                        data_buf.push(0xFF);
                        ParseState::Data
                    }
                    codes::WILL | codes::WONT | codes::DO | codes::DONT => ParseState::Negotiate(b),
                    codes::SB => ParseState::Subneg,
                    _ => {
                        // Single-byte command (NOP, AYT, EC, EL, …).
                        // We ignore them per procServ semantics.
                        ParseState::Data
                    }
                },
                ParseState::Negotiate(cmd) => {
                    // Run the RFC1143 Q-method (libtelnet `_negotiate`) so the
                    // peer's confirmations of our startup offers stay silent and
                    // ECHO/LINEMODE are accepted per the option table — instead
                    // of blanket-refusing every request.
                    self.handle_negotiate(cmd, b, &mut reply_buf);
                    ParseState::Data
                }
                ParseState::Subneg => {
                    if b == codes::IAC {
                        ParseState::SubnegIac
                    } else {
                        // Discard subnegotiation payload — we don't
                        // care about the specifics.
                        ParseState::Subneg
                    }
                }
                ParseState::SubnegIac => match b {
                    codes::SE => ParseState::Data,
                    codes::IAC => ParseState::Subneg,
                    _ => ParseState::Subneg,
                },
            };
        }

        if !data_buf.is_empty() {
            events.push(TelnetEvent::Data(data_buf));
        }
        if !reply_buf.is_empty() {
            events.push(TelnetEvent::Reply(reply_buf));
        }
        events
    }

    /// RFC1143 negotiation handling (libtelnet `_negotiate`,
    /// `libtelnet.c:365-511`). Appends any `IAC <cmd> <opt>` response to
    /// `reply`. procServ's `telnet_eh` ignores the WILL/WONT/DO/DONT events
    /// (`clientFactory.cc:299-311` → `default`), so the only wire-observable
    /// effect is the `_send_negotiate` response reproduced here.
    fn handle_negotiate(&mut self, cmd: u8, opt: u8, reply: &mut Vec<u8>) {
        let (us, him) = self.q_get(opt);
        match cmd {
            // Request to enable option on remote end, or confirm DO.
            codes::WILL => match him {
                QState::No => {
                    if check_telopt(opt, false) {
                        self.q_set(opt, us, QState::Yes);
                        send_neg(reply, codes::DO, opt);
                    } else {
                        send_neg(reply, codes::DONT, opt);
                    }
                }
                QState::WantYes => self.q_set(opt, us, QState::Yes),
                QState::Yes => {}
            },
            // Request to disable option on remote end, confirm DONT, reject DO.
            codes::WONT => match him {
                QState::Yes => {
                    self.q_set(opt, us, QState::No);
                    send_neg(reply, codes::DONT, opt);
                }
                QState::WantYes => self.q_set(opt, us, QState::No),
                QState::No => {}
            },
            // Request to enable option on local end, or confirm WILL.
            codes::DO => match us {
                QState::No => {
                    if check_telopt(opt, true) {
                        self.q_set(opt, QState::Yes, him);
                        send_neg(reply, codes::WILL, opt);
                    } else {
                        send_neg(reply, codes::WONT, opt);
                    }
                }
                QState::WantYes => self.q_set(opt, QState::Yes, him),
                QState::Yes => {}
            },
            // Request to disable option on local end, confirm WONT, reject WILL.
            codes::DONT => match us {
                QState::Yes => {
                    self.q_set(opt, QState::No, him);
                    send_neg(reply, codes::WONT, opt);
                }
                QState::WantYes => self.q_set(opt, QState::No, him),
                QState::No => {}
            },
            _ => {}
        }
    }
}

/// libtelnet `_send_negotiate` (`libtelnet.c:356-363`): emit `IAC <cmd> <opt>`.
fn send_neg(reply: &mut Vec<u8>, cmd: u8, opt: u8) {
    reply.extend_from_slice(&[codes::IAC, cmd, opt]);
}

/// Build the initial negotiation handshake to send when a client connects.
/// Mirrors C procServ's `telnet_negotiate` calls in `clientItem::clientItem`
/// (`clientFactory.cc:167-174`): per option in table order, request `DO`
/// (him) then advertise `WILL` (us). For procServ this is `WILL ECHO` then
/// `DO LINEMODE`. Derived from the same `TELOPTS` table the parser seeds its
/// RFC1143 state from, so the offered bytes and the seeded state cannot drift.
pub fn initial_negotiation() -> Vec<u8> {
    let mut out = Vec::new();
    for t in TELOPTS {
        if t.him_do {
            out.extend_from_slice(&[codes::IAC, codes::DO, t.opt]);
        }
        if t.us_will {
            out.extend_from_slice(&[codes::IAC, codes::WILL, t.opt]);
        }
    }
    out
}

/// IAC-encode an outgoing data buffer: any literal `0xFF` is
/// doubled. Other bytes pass through. Equivalent to libtelnet's
/// `telnet_send` for the raw-data path.
pub fn iac_escape(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    for &b in data {
        if b == codes::IAC {
            out.push(codes::IAC);
            out.push(codes::IAC);
        } else {
            out.push(b);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_plain_data_through() {
        let mut p = TelnetParser::new();
        let evs = p.feed(b"hello");
        assert_eq!(evs, vec![TelnetEvent::Data(b"hello".to_vec())]);
    }

    #[test]
    fn unescapes_iac_iac() {
        let mut p = TelnetParser::new();
        let evs = p.feed(&[b'a', codes::IAC, codes::IAC, b'b']);
        assert_eq!(evs, vec![TelnetEvent::Data(vec![b'a', 0xFF, b'b'])]);
    }

    fn reply_bytes(evs: &[TelnetEvent]) -> Vec<u8> {
        evs.iter()
            .filter_map(|e| match e {
                TelnetEvent::Reply(r) => Some(r.clone()),
                TelnetEvent::Data(_) => None,
            })
            .flatten()
            .collect()
    }

    #[test]
    fn initial_negotiation_offers_will_echo_then_do_linemode() {
        // C clientItem::clientItem (clientFactory.cc:167-174): WILL ECHO then
        // DO LINEMODE.
        assert_eq!(
            initial_negotiation(),
            vec![
                codes::IAC,
                codes::WILL,
                codes::TELOPT_ECHO,
                codes::IAC,
                codes::DO,
                codes::TELOPT_LINEMODE,
            ]
        );
    }

    #[test]
    fn refuses_peer_will_echo() {
        // ECHO him-policy is 0 (we don't want the peer to echo), so a peer
        // `WILL ECHO` is refused: `_check_telopt(ECHO, remote)` = false →
        // DONT ECHO (libtelnet.c:397-403).
        let mut p = TelnetParser::new();
        let evs = p.feed(&[codes::IAC, codes::WILL, codes::TELOPT_ECHO]);
        assert_eq!(
            reply_bytes(&evs),
            vec![codes::IAC, codes::DONT, codes::TELOPT_ECHO]
        );
    }

    #[test]
    fn confirms_do_echo_silently() {
        // After our startup WILL ECHO offer, US(ECHO) = WANTYES, so the peer's
        // `DO ECHO` is a confirmation: Q_WANTYES → Q_YES, send nothing
        // (libtelnet.c:475-478). The old blanket-refuse sent WONT ECHO here,
        // contradicting our own offer.
        let mut p = TelnetParser::new();
        let evs = p.feed(&[codes::IAC, codes::DO, codes::TELOPT_ECHO]);
        assert_eq!(reply_bytes(&evs), Vec::<u8>::new());
    }

    #[test]
    fn confirms_will_linemode_silently() {
        // After our startup DO LINEMODE offer, HIM(LINEMODE) = WANTYES, so the
        // peer's `WILL LINEMODE` is a confirmation: Q_WANTYES → Q_YES, send
        // nothing (libtelnet.c:417-420).
        let mut p = TelnetParser::new();
        let evs = p.feed(&[codes::IAC, codes::WILL, codes::TELOPT_LINEMODE]);
        assert_eq!(reply_bytes(&evs), Vec::<u8>::new());
    }

    #[test]
    fn rejects_do_linemode() {
        // LINEMODE us-policy is 0 (we won't run linemode locally), so a peer
        // `DO LINEMODE` is rejected: `_check_telopt(LINEMODE, local)` = false →
        // WONT LINEMODE (libtelnet.c:456-461).
        let mut p = TelnetParser::new();
        let evs = p.feed(&[codes::IAC, codes::DO, codes::TELOPT_LINEMODE]);
        assert_eq!(
            reply_bytes(&evs),
            vec![codes::IAC, codes::WONT, codes::TELOPT_LINEMODE]
        );
    }

    #[test]
    fn refuses_unknown_options() {
        // An option absent from the table is refused both ways: WILL → DONT,
        // DO → WONT; WONT/DONT for a never-enabled option get no reply.
        let mut p = TelnetParser::new();
        let evs = p.feed(&[
            codes::IAC,
            codes::WILL,
            codes::TELOPT_SGA,
            codes::IAC,
            codes::DO,
            codes::TELOPT_SGA,
            codes::IAC,
            codes::WONT,
            codes::TELOPT_SGA,
            codes::IAC,
            codes::DONT,
            codes::TELOPT_SGA,
        ]);
        assert_eq!(
            reply_bytes(&evs),
            vec![
                codes::IAC,
                codes::DONT,
                codes::TELOPT_SGA,
                codes::IAC,
                codes::WONT,
                codes::TELOPT_SGA,
            ]
        );
    }

    #[test]
    fn skips_subnegotiation_block() {
        let mut p = TelnetParser::new();
        let evs = p.feed(&[
            b'a',
            codes::IAC,
            codes::SB,
            0x18,
            0x01,
            0x02,
            codes::IAC,
            codes::SE,
            b'b',
        ]);
        assert_eq!(evs, vec![TelnetEvent::Data(vec![b'a', b'b'])]);
    }

    #[test]
    fn iac_escape_doubles_ff() {
        assert_eq!(iac_escape(&[1, 0xFF, 2]), vec![1, 0xFF, 0xFF, 2]);
    }
}
