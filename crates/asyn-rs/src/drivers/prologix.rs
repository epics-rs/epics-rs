//! Prologix GPIB-Ethernet controller driver.
//!
//! Port of `asyn/drvPrologixGPIB/drvPrologixGPIB.c`. The Prologix
//! GPIB-Ethernet bridge exposes GPIB instruments over a TCP socket;
//! lines beginning with `++` configure the bridge (address, EOS,
//! EOI, EOT-marker), all other lines are forwarded to the currently-
//! addressed instrument.
//!
//! ## Architecture (mirrors C asyn)
//!
//! Outer port = this `DrvAsynPrologixPort` (GPIB driver, multi-device,
//! one slot per GPIB primary address 0..30 plus secondary encoding).
//!
//! Inner = a private [`super::ip_port::DrvAsynIPPort`] held as a field —
//! C asyn registers a separate `<port>_TCP` asyn port; we just embed
//! the IP driver (no asynManager port-name indirection needed in Rust).
//!
//! ## Address encoding (C parity, `setAddress` in drvPrologixGPIB.c)
//!
//! `pasynUser->addr` carries the GPIB target. `addr < 100` means
//! primary-only; `addr >= 100` decodes as `primary = addr/100`,
//! `secondary = addr%100` (must be < 31, sent on the wire as
//! `secondary + 96` per IEEE-488 MSA encoding).
//!
//! ## On-connect handshake
//!
//! When the outer port connects (address < 0), C asyn sends:
//! `++savecfg 0` / `++mode 1` / `++ifc` / `++eos 3` / `++eoi 1` /
//! `++eot_char <EOT_MARKER>` / `++eot_enable 1` / `++ver` and then
//! reads the bridge's version line up to `\r\n`. Done verbatim here.
//!
//! ## Per-write flow (C parity, `prologixWrite` + `stashChar`)
//!
//! 1. setAddress(addr) — emits `++addr` only when changed.
//! 2. stashChar each byte: `\r`, `\n`, `\033`, `+` get a `\033` prefix.
//! 3. If `eos >= 0`, append `eos` (also stash-escaped).
//! 4. Append literal `\n` terminator.
//! 5. Single inner-TCP write.
//!
//! ## Per-read flow (C parity, `prologixRead`)
//!
//! 1. setAddress(addr).
//! 2. Send `++read <eos>\n` or `++read eoi\n`.
//! 3. Loop reading chunks until the terminator (eos char or
//!    EOT_MARKER if no eos) is the last byte.
//! 4. With no eos: do one extra short-timeout (5ms) read to
//!    disambiguate a binary EOT byte from real end-of-message; if
//!    it times out, that confirms EOM.
//! 5. Strip trailing EOT marker when eos < 0.

use std::sync::Mutex;
use std::time::Duration;

use crate::error::{AsynError, AsynResult, AsynStatus};
use crate::interpose::EomReason;
use crate::port::{PortDriver, PortDriverBase, PortFlags};
use crate::user::AsynUser;

/// EOT marker the bridge appends to incoming data when
/// `++eot_enable 1`. C asyn `EOT_MARKER`.
pub const EOT_MARKER: u8 = 0xEF;

/// Default TCP port for the Prologix bridge — set in the bridge
/// firmware (Prologix doesn't allow it to be changed). C asyn
/// hard-codes `:1234 TCP` when the host string has no `:`.
pub const DEFAULT_TCP_PORT: u16 = 1234;

/// Initial output staging buffer capacity. Matches C asyn
/// `pdpvt->bufCapacity = 4096`. Read path grows as needed.
pub const DEFAULT_BUF_CAPACITY: usize = 4096;

/// End-of-message reason for a prologix read chunk. Single owner of
/// the C `readIt` rule (drvPrologixGPIB.c:334-345): the device message
/// is fully buffered, then served in caller-sized chunks. The final
/// chunk — the caller buffer (`maxchars`) holds the rest of the
/// message (`remaining`) — carries `ASYN_EOM_EOS` when an EOS char is
/// configured, else `ASYN_EOM_END` (binary/EOI mode). A buffer-limited
/// chunk carries `ASYN_EOM_CNT`; an exact fit (`remaining == maxchars`)
/// sets both.
fn read_eom(remaining: usize, maxchars: usize, eos_set: bool) -> EomReason {
    let mut eom = EomReason::empty();
    if maxchars >= remaining {
        eom |= if eos_set {
            EomReason::EOS
        } else {
            EomReason::END
        };
    }
    if remaining >= maxchars {
        eom |= EomReason::CNT;
    }
    eom
}

/// Mutable per-driver state — last-sent GPIB address (so `++addr`
/// is suppressed when unchanged), EOS char (or `None` for "let the
/// bridge use EOT marker"), and a small staging buffer we reuse for
/// reads. Wrapped in a Mutex so the trait's `&self` / `&mut self`
/// boundary stays clean while still allowing `read_octet(&self,...)`
/// to mutate the read accumulator.
struct State {
    /// Last GPIB primary address sent via `++addr`, `-1` if not set.
    last_primary: i32,
    /// Last GPIB secondary address sent via `++addr`, `-1` if none.
    last_secondary: i32,
    /// `Some(c)` selects EOS character; `None` means "EOI / EOT marker".
    eos: Option<u8>,
    /// Bridge version string captured during connect. Mainly for
    /// diagnostics — not used for protocol decisions.
    version: String,
    /// Bytes of a bridge response that did not fit the caller's buffer
    /// on the previous `read_octet`. Drained first on the next call so
    /// no device data is lost when the caller's buffer is too small.
    read_carry: Vec<u8>,
}

pub struct DrvAsynPrologixPort {
    base: PortDriverBase,
    /// Embedded TCP transport. C asyn registers this as a separate
    /// asyn port `<port>_TCP`; we keep it private so the outer GPIB
    /// driver is the only public surface.
    inner: super::ip_port::DrvAsynIPPort,
    state: Mutex<State>,
}

impl DrvAsynPrologixPort {
    /// Discard any staged read remainder (`read_carry`). C parity:
    /// `drvPrologixGPIB.c` resets `bufCount = 0` on every transaction
    /// boundary (write begin/end) and session boundary (connect) so a reply
    /// tail left unconsumed by a too-small read buffer never leaks into the
    /// next command's response. Single owner for that invariant — called
    /// from `write_octet`, `connect`, `io_flush`, and `disconnect`.
    fn clear_read_carry(&self) {
        self.state.lock().unwrap().read_carry.clear();
    }

    /// Construct a new Prologix driver. Mirrors C asyn
    /// `prologixGPIBConfigure(portName, host, priority, noAutoConnect)`.
    /// `host` may be `"hostname"` (default port 1234 appended) or
    /// `"hostname:port"`. `no_auto_connect` defers connection until
    /// the framework triggers it — same flag semantics as C asyn.
    pub fn new(port_name: &str, host: &str, no_auto_connect: bool) -> AsynResult<Self> {
        // Inner TCP spec — `"host:port TCP"`. C asyn always appends
        // `:1234 TCP` when no colon present; we do the same. When a
        // colon is already present we trust the caller's port and
        // still append the `TCP` token so the inner ip_port parser
        // selects the right protocol.
        let ip_spec = if host.contains(':') {
            // Caller supplied host:port — append " TCP" if not already present.
            if host.to_ascii_uppercase().ends_with(" TCP") {
                host.to_string()
            } else {
                format!("{host} TCP")
            }
        } else {
            format!("{host}:{DEFAULT_TCP_PORT} TCP")
        };
        let inner = super::ip_port::DrvAsynIPPort::new(&format!("{port_name}_TCP"), &ip_spec)?;
        let mut base = PortDriverBase::new(
            port_name,
            // GPIB primary addresses 0..30 → 31 slots; secondary
            // addresses are encoded into addr (addr/100 + addr%100)
            // so they don't bump the slot count.
            31,
            PortFlags {
                multi_device: true,
                can_block: true,
                destructible: true,
            },
        );
        base.connected = false;
        base.auto_connect = !no_auto_connect;
        Ok(Self {
            base,
            inner,
            state: Mutex::new(State {
                last_primary: -1,
                last_secondary: -1,
                eos: None,
                version: String::new(),
                read_carry: Vec::new(),
            }),
        })
    }

    /// Bridge version string captured during connect (or empty
    /// before connect). Diagnostic-only.
    pub fn version(&self) -> String {
        self.state.lock().unwrap().version.clone()
    }

    /// Currently-selected EOS char, or `None` when EOI / EOT marker
    /// terminates instead. Mirrors C asyn `pdpvt->eos < 0` sentinel.
    pub fn eos(&self) -> Option<u8> {
        self.state.lock().unwrap().eos
    }

    /// Set EOS char (`Some(c)`) or disable (`None` — EOT marker
    /// fallback). Mirrors C asyn `prologixSetEos`. The actual wire
    /// effect is applied lazily on the next read/write — same as C
    /// asyn which only re-issues `++eot_enable` when toggling
    /// between "EOS-driven" and "EOT-marker" modes.
    pub fn set_eos(&self, eos: Option<u8>) -> AsynResult<()> {
        let mut s = self.state.lock().unwrap();
        if s.eos == eos {
            return Ok(());
        }
        // C asyn sends `++eot_enable %d` with the *previous* eos<0
        // boolean to flip the bridge between "use EOI/EOT marker" and
        // "trust user EOS char". The inner write requires &mut self
        // routing — we punt the bridge command to the next read/write
        // path which already knows how to send `++` lines.
        s.eos = eos;
        Ok(())
    }

    /// Encode a GPIB target into primary/secondary, validating the
    /// IEEE-488 ranges. Returns `(primary, secondary_or_-1)`. C
    /// asyn `setAddress` lines 67-83.
    fn decode_addr(addr: i32) -> AsynResult<(i32, i32)> {
        let (primary, secondary) = if addr < 100 {
            (addr, -1)
        } else {
            let p = addr / 100;
            let s = addr % 100;
            if !(0..31).contains(&s) {
                return Err(AsynError::Status {
                    status: AsynStatus::Error,
                    message: format!("Invalid GPIB secondary address {s}"),
                });
            }
            (p, s)
        };
        if !(0..31).contains(&primary) {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: format!("Invalid GPIB primary address {primary}"),
            });
        }
        Ok((primary, secondary))
    }

    /// Build the `++addr` line for a given decoded primary/secondary.
    /// Secondary is sent on the wire as `secondary + 96` per IEEE-488
    /// MSA encoding. C asyn `setAddress` lines 87-90.
    fn addr_line(primary: i32, secondary: i32) -> String {
        if secondary < 0 {
            format!("++addr {primary}\n")
        } else {
            format!("++addr {primary} {}\n", secondary + 96)
        }
    }

    /// Escape a single byte into the output buffer per the C asyn
    /// `stashChar` convention: `\r`, `\n`, `\033`, `+` get a `\033`
    /// prefix; everything else is passed through. The bridge
    /// interprets unescaped `\r`/`\n`/`++` as command boundaries —
    /// without this every user payload containing those bytes would
    /// confuse the bridge.
    pub fn stash_char(buf: &mut Vec<u8>, c: u8) {
        if matches!(c, b'\r' | b'\n' | 0x1B | b'+') {
            buf.push(0x1B);
        }
        buf.push(c);
    }

    /// Issue a `++addr` line to the bridge if the GPIB target
    /// changed since last call. C asyn `setAddress` (lines 84-101).
    fn set_address(&mut self, user: &AsynUser) -> AsynResult<()> {
        let (primary, secondary) = Self::decode_addr(user.addr)?;
        {
            let s = self.state.lock().unwrap();
            if s.last_primary == primary && s.last_secondary == secondary {
                return Ok(());
            }
        }
        let cmd = Self::addr_line(primary, secondary);
        let mut bridge_user = AsynUser::default().with_timeout(Duration::from_secs(1));
        match self.inner.write_octet(&mut bridge_user, cmd.as_bytes()) {
            Ok(_) => {
                let mut s = self.state.lock().unwrap();
                s.last_primary = primary;
                s.last_secondary = secondary;
                Ok(())
            }
            Err(e) => {
                // C asyn resets last_primary/last_secondary to -1 on
                // failure so the next call re-issues addressing.
                let mut s = self.state.lock().unwrap();
                s.last_primary = -1;
                s.last_secondary = -1;
                Err(e)
            }
        }
    }
}

impl PortDriver for DrvAsynPrologixPort {
    fn base(&self) -> &PortDriverBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        &mut self.base
    }

    fn connect(&mut self, user: &AsynUser) -> AsynResult<()> {
        // Reset addressing state — fresh TCP connection means the
        // bridge's last-sent address is unknown, so the next write
        // must re-issue `++addr`.
        {
            let mut s = self.state.lock().unwrap();
            s.last_primary = -1;
            s.last_secondary = -1;
        }
        // C parity: prologixConnect also resets bufCount=0 — a fresh session
        // must not serve a previous connection's staged reply tail (the F6
        // staged-read-discard invariant). disconnect clears it, but a
        // reconnect driven without an intervening disconnect (e.g. the inner
        // ip_port auto-disconnected on a read error) would otherwise leak it.
        self.clear_read_carry();
        // Port-level connect: address < 0 in C asyn. For per-device
        // connect (address >= 0) C asyn does nothing protocol-side
        // beyond announcing the exception, since GPIB devices live
        // behind the single shared TCP socket.
        if user.addr < 0 {
            self.inner.connect(user)?;
            // 8-line init burst — sent as one TCP write to match
            // C asyn (single `pasynOctetSyncIO->write`).
            let init = format!(
                "++savecfg 0\n++mode 1\n++ifc\n++eos 3\n++eoi 1\n\
                 ++eot_char {EOT_MARKER}\n++eot_enable 1\n++ver\n",
            );
            let mut tu = AsynUser::default().with_timeout(Duration::from_secs(1));
            self.inner.write_octet(&mut tu, init.as_bytes())?;
            // Read the version response — chars accumulate until the
            // bridge sends `\r\n`. C asyn caps at 200 bytes total.
            let mut acc = Vec::with_capacity(64);
            let mut buf = [0u8; 64];
            loop {
                let ru = AsynUser::default().with_timeout(Duration::from_millis(500));
                let n = self.inner.read_octet(&ru, &mut buf)?;
                if n == 0 {
                    return Err(AsynError::Status {
                        status: AsynStatus::Error,
                        message: "Prologix: bridge closed during version handshake".into(),
                    });
                }
                acc.extend_from_slice(&buf[..n]);
                if acc.len() > 200 {
                    return Err(AsynError::Status {
                        status: AsynStatus::Error,
                        message: "Prologix: version string too long".into(),
                    });
                }
                if acc.len() >= 2 && acc[acc.len() - 2] == b'\r' && acc[acc.len() - 1] == b'\n' {
                    let v = String::from_utf8_lossy(&acc[..acc.len() - 2]).to_string();
                    self.state.lock().unwrap().version = v;
                    break;
                }
            }
        }
        self.base.set_connected(true);
        Ok(())
    }

    fn disconnect(&mut self, user: &AsynUser) -> AsynResult<()> {
        if user.addr < 0 {
            self.inner.disconnect(user)?;
        }
        // Drop any buffered read remainder — it belongs to the old
        // connection and must not leak into the next session.
        self.clear_read_carry();
        self.base.set_connected(false);
        Ok(())
    }

    fn io_flush(&mut self, user: &mut AsynUser) -> AsynResult<()> {
        // OctetWriteRead does flush -> write -> read to drop stale input.
        // The transport flush cannot see `read_carry` (an application
        // buffer), so clear it here too or a stale carry would be
        // returned as the response to the new command.
        self.clear_read_carry();
        self.inner.io_flush(user)
    }

    // C parity: prologix is an asynGpibPort whose octet EOS interface maps to
    // prologixSetEos / prologixGetEos over the single driver `eos` field
    // (drvPrologixGPIB.c:422-459), not to a generic base cache. The default
    // PortDriver::{set,get}_input_eos write `base.input_eos` and forward to an
    // (empty) interpose stack, so they would store EOS bytes the prologix
    // read/write path never consults — `get_input_eos` would echo bytes with no
    // protocol effect. Route the interface to `State.eos`, the field the read
    // (`++read <eos>` vs `++read eoi`) and write (append-on-`eos>=0`) paths
    // actually use.
    fn set_input_eos(&mut self, eos: &[u8]) -> AsynResult<()> {
        // asynGpib's wrapper rejects eoslen > 1 ("only 1 is allowed",
        // asynGpib.c:443) and prologixSetEos rejects the same with asynError
        // "Invalid EOS" (drvPrologixGPIB.c:449-452); 0 disables EOS (eos < 0).
        let new_eos = match eos.len() {
            0 => None,
            1 => Some(eos[0]),
            _ => {
                return Err(AsynError::Status {
                    status: AsynStatus::Error,
                    message: "Invalid EOS".into(),
                });
            }
        };
        self.set_eos(new_eos)
    }

    fn get_input_eos(&self) -> Vec<u8> {
        // C prologixGetEos (drvPrologixGPIB.c:422-437): eos < 0 reports
        // eoslen 0; otherwise eoslen 1 carrying the single EOS byte.
        match self.state.lock().unwrap().eos {
            Some(c) => vec![c],
            None => Vec::new(),
        }
    }

    // C parity: asynGpib's octet vtable leaves setOutputEos / getOutputEos NULL
    // (asynGpib.c:132 — `...setInputEos, getInputEos, 0, 0`), so a GPIB port has
    // no output-EOS support; prologix appends its single `eos` on write
    // (write_octet) rather than a separate output terminator. Reject
    // set_output_eos instead of silently caching ineffective bytes in
    // `base.output_eos`, and report none — the output twin of the input-EOS
    // routing above (same defect family: the EOS interface must reflect the
    // driver's real EOS state, never a dead base cache).
    fn set_output_eos(&mut self, _eos: &[u8]) -> AsynResult<()> {
        Err(AsynError::Status {
            status: AsynStatus::Error,
            message: "output EOS not supported on a GPIB port".into(),
        })
    }

    fn get_output_eos(&self) -> Vec<u8> {
        Vec::new()
    }

    fn write_octet(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
        self.base.check_ready()?;
        // C parity: prologixWrite sets bufCount=0 at the start of every
        // write, discarding any reply tail the previous read left staged —
        // otherwise the next read returns that stale data as the response to
        // *this* command (cross-transaction leak).
        self.clear_read_carry();
        self.set_address(user)?;
        let eos = self.state.lock().unwrap().eos;
        let mut out: Vec<u8> = Vec::with_capacity(data.len() + 4);
        for &c in data {
            Self::stash_char(&mut out, c);
        }
        if let Some(c) = eos {
            Self::stash_char(&mut out, c);
        }
        out.push(b'\n');
        // Report the caller's data length as bytes transferred, not the
        // GPIB-framed wire length (`out`): on a successful inner write all of
        // `data` was accepted (C asyn reports the application payload count).
        self.inner.write_octet(user, &out)?;
        Ok(data.len())
    }

    fn read_octet(&mut self, user: &AsynUser, buf: &mut [u8]) -> AsynResult<usize> {
        self.io_read_octet_eom(user, buf).map(|(n, _)| n)
    }

    /// Octet read that also reports the end-of-message reason. Single
    /// owner of the prologix read path; [`read_octet`] delegates here
    /// and discards the EOM. C `readIt` (drvPrologixGPIB.c:334-349)
    /// returns `eomReason` (END/EOS/CNT) alongside the byte count; the
    /// default actor synthesis would report CNT-only and lose the GPIB
    /// EOI / EOS message boundary.
    ///
    /// [`read_octet`]: PortDriver::read_octet
    fn io_read_octet_eom(
        &mut self,
        user: &AsynUser,
        buf: &mut [u8],
    ) -> AsynResult<(usize, EomReason)> {
        self.base.check_ready()?;
        // Drain bytes left over from a previous read whose buffer was
        // too small before issuing a new bridge `++read` — otherwise
        // that data is lost and the next read returns fresh device
        // output out of order.
        {
            let mut st = self.state.lock().unwrap();
            if !st.read_carry.is_empty() {
                let remaining = st.read_carry.len();
                let n = remaining.min(buf.len());
                buf[..n].copy_from_slice(&st.read_carry[..n]);
                st.read_carry.drain(..n);
                let eom = read_eom(remaining, buf.len(), st.eos.is_some());
                return Ok((n, eom));
            }
        }
        self.set_address(user)?;
        let eos = self.state.lock().unwrap().eos;
        // Issue the bridge `++read` command. Two flavours mirror C
        // asyn — explicit EOS char vs `eoi` (use the EOI line as
        // terminator and rely on EOT marker to wrap up).
        let cmd = match eos {
            Some(c) => format!("++read {c}\n"),
            None => "++read eoi\n".to_string(),
        };
        let mut bridge_user = AsynUser::default().with_timeout(Duration::from_secs(1));
        self.inner.write_octet(&mut bridge_user, cmd.as_bytes())?;

        // Loop reading from the bridge until the terminator byte
        // appears at the end of the most recent chunk. With no eos,
        // C asyn does an extra 5ms-timeout read to disambiguate a
        // binary EOT byte from the real end-of-message — when it
        // times out, EOM is confirmed.
        let terminator = eos.unwrap_or(EOT_MARKER);
        let mut acc: Vec<u8> = Vec::with_capacity(4096);
        let mut chunk = vec![0u8; 4096];
        let user_timeout = if user.timeout.is_zero() {
            Duration::from_secs(1)
        } else {
            user.timeout
        };
        let mut read_timeout = user_timeout;
        let mut at_eot = false;
        loop {
            let ru = AsynUser::default().with_timeout(read_timeout);
            match self.inner.read_octet(&ru, &mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    acc.extend_from_slice(&chunk[..n]);
                    if let Some(&last) = acc.last() {
                        if last == terminator {
                            if eos.is_some() {
                                break;
                            }
                            // Binary-mode terminator ambiguity — try
                            // one more short-timeout read.
                            read_timeout = Duration::from_millis(5);
                            at_eot = true;
                            continue;
                        }
                    }
                    read_timeout = user_timeout;
                    at_eot = false;
                }
                Err(AsynError::Status {
                    status: AsynStatus::Timeout,
                    ..
                }) if at_eot => break,
                Err(e) => return Err(e),
            }
        }
        // Drop trailing EOT marker when no eos char (it's framing,
        // not data). When eos is set, the eos char IS data the
        // record consumer expects, so we leave it.
        if eos.is_none() && acc.last() == Some(&EOT_MARKER) {
            acc.pop();
        }
        let remaining = acc.len();
        let n = remaining.min(buf.len());
        buf[..n].copy_from_slice(&acc[..n]);
        if n < remaining {
            // Caller's buffer was too small — stash the remainder so the
            // next read_octet returns it instead of dropping device data.
            self.state.lock().unwrap().read_carry = acc.split_off(n);
        }
        let eom = read_eom(remaining, buf.len(), eos.is_some());
        Ok((n, eom))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    #[test]
    fn decode_addr_primary_only() {
        assert_eq!(DrvAsynPrologixPort::decode_addr(0).unwrap(), (0, -1));
        assert_eq!(DrvAsynPrologixPort::decode_addr(15).unwrap(), (15, -1));
        assert_eq!(DrvAsynPrologixPort::decode_addr(30).unwrap(), (30, -1));
    }

    #[test]
    fn decode_addr_secondary() {
        // addr=512 → primary=5, secondary=12
        assert_eq!(DrvAsynPrologixPort::decode_addr(512).unwrap(), (5, 12));
        // addr=2030 → primary=20, secondary=30
        assert_eq!(DrvAsynPrologixPort::decode_addr(2030).unwrap(), (20, 30));
    }

    #[test]
    fn decode_addr_rejects_oob_primary() {
        assert!(DrvAsynPrologixPort::decode_addr(31).is_err());
        assert!(DrvAsynPrologixPort::decode_addr(-1).is_err());
    }

    #[test]
    fn decode_addr_rejects_oob_secondary() {
        // addr=531 → secondary=31 (out of range)
        assert!(DrvAsynPrologixPort::decode_addr(531).is_err());
    }

    #[test]
    fn addr_line_primary_only_format() {
        assert_eq!(DrvAsynPrologixPort::addr_line(7, -1), "++addr 7\n");
    }

    #[test]
    fn addr_line_secondary_adds_96() {
        // C parity: secondary on the wire = secondary + 96 (MSA).
        assert_eq!(DrvAsynPrologixPort::addr_line(5, 12), "++addr 5 108\n");
    }

    #[test]
    fn stash_char_escapes_special_bytes() {
        let mut buf = Vec::new();
        for c in [b'\r', b'\n', 0x1B, b'+'] {
            buf.clear();
            DrvAsynPrologixPort::stash_char(&mut buf, c);
            assert_eq!(buf, vec![0x1B, c], "byte 0x{c:02X} not escaped");
        }
    }

    #[test]
    fn stash_char_passes_normal_bytes() {
        let mut buf = Vec::new();
        for c in [b'A', b'0', b' ', 0x00, 0xFF] {
            buf.clear();
            DrvAsynPrologixPort::stash_char(&mut buf, c);
            assert_eq!(buf, vec![c], "byte 0x{c:02X} unexpectedly escaped");
        }
    }

    /// Spin up a TCP listener that mimics a Prologix bridge: it
    /// accepts the connection, swallows the 8-line init burst, and
    /// answers the `++ver` query with `Prologix Test 1.0\r\n`. Then
    /// it drains any subsequent writes and ships them back via the
    /// channel for assertion.
    fn start_mock_bridge() -> (u16, mpsc::Receiver<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut acc = Vec::new();
            let mut buf = [0u8; 4096];
            // Wait until the init burst (ending with `++ver\n`)
            // arrives, then send the version line.
            let mut version_sent = false;
            loop {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        acc.extend_from_slice(&buf[..n]);
                        if !version_sent && acc.windows(6).any(|w| w == b"++ver\n") {
                            stream.write_all(b"Prologix Test 1.0\r\n").unwrap();
                            version_sent = true;
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = tx.send(acc);
        });
        (port, rx)
    }

    /// End-to-end: connect the driver against the mock bridge,
    /// confirm the init burst is sent verbatim and the version
    /// string is captured.
    #[test]
    fn connect_sends_init_burst_and_captures_version() {
        let (port, rx) = start_mock_bridge();
        let mut drv = DrvAsynPrologixPort::new("p", &format!("127.0.0.1:{port}"), false).unwrap();
        let user = AsynUser::default().with_addr(-1);
        drv.connect(&user).unwrap();
        assert!(drv.base.connected);
        assert_eq!(drv.version(), "Prologix Test 1.0");
        // Tear down so the mock thread can drop and ship its capture.
        drv.disconnect(&user).unwrap();
        let captured = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let s = String::from_utf8_lossy(&captured);
        let expected_init = format!(
            "++savecfg 0\n++mode 1\n++ifc\n++eos 3\n++eoi 1\n\
             ++eot_char {EOT_MARKER}\n++eot_enable 1\n++ver\n",
        );
        assert!(
            s.starts_with(&expected_init),
            "init burst mismatch — got: {s:?}"
        );
    }

    /// `++addr` only emitted when address changes (C parity:
    /// `last_primary`/`last_secondary` cache).
    #[test]
    fn write_emits_addr_only_when_changed() {
        let (port, rx) = start_mock_bridge();
        let mut drv = DrvAsynPrologixPort::new("p", &format!("127.0.0.1:{port}"), false).unwrap();
        let user_connect = AsynUser::default().with_addr(-1);
        drv.connect(&user_connect).unwrap();

        let mut user_w = AsynUser::default()
            .with_addr(7)
            .with_timeout(Duration::from_secs(2));
        drv.write_octet(&mut user_w, b"*IDN?").unwrap();
        // Same address — no new ++addr.
        drv.write_octet(&mut user_w, b"*IDN?").unwrap();
        // Different address.
        let mut user_w2 = AsynUser::default()
            .with_addr(12)
            .with_timeout(Duration::from_secs(2));
        drv.write_octet(&mut user_w2, b"VAL?").unwrap();

        drv.disconnect(&AsynUser::default().with_addr(-1)).unwrap();
        let captured = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let s = String::from_utf8_lossy(&captured).to_string();
        // Strip the init burst (everything up to and including the
        // ++ver\n response trigger) so we focus on the post-init
        // wire bytes.
        let init_end = s.find("++ver\n").unwrap() + "++ver\n".len();
        let post = &s[init_end..];
        // Expect: ++addr 7\n, payload + \n, payload + \n, ++addr 12\n, payload + \n
        assert_eq!(
            post, "++addr 7\n*IDN?\n*IDN?\n++addr 12\nVAL?\n",
            "post-init wire bytes wrong: {post:?}"
        );
    }

    #[test]
    fn write_discards_staged_read_carry() {
        // DRV-48: a new write must discard any reply tail the previous read
        // left staged (C prologixWrite sets bufCount=0 at the start), or the
        // next read returns that stale data as the response to *this*
        // command.
        let (port, _rx) = start_mock_bridge();
        let mut drv = DrvAsynPrologixPort::new("p", &format!("127.0.0.1:{port}"), false).unwrap();
        drv.connect(&AsynUser::default().with_addr(-1)).unwrap();

        // Simulate a prior small-buffer read that left a tail staged.
        drv.state.lock().unwrap().read_carry = b"STALE_TAIL".to_vec();

        let mut user_w = AsynUser::default()
            .with_addr(3)
            .with_timeout(Duration::from_secs(2));
        drv.write_octet(&mut user_w, b"*IDN?").unwrap();

        assert!(
            drv.state.lock().unwrap().read_carry.is_empty(),
            "DRV-48: write_octet must clear the staged read_carry"
        );
        drv.disconnect(&AsynUser::default().with_addr(-1)).unwrap();
    }

    #[test]
    fn connect_discards_staged_read_carry() {
        // DRV-51: a fresh session (connect) must discard a prior
        // connection's staged reply tail (C prologixConnect resets
        // bufCount=0), so a reconnect without an intervening disconnect does
        // not leak stale bytes into the first read.
        let (port, _rx) = start_mock_bridge();
        let mut drv = DrvAsynPrologixPort::new("p", &format!("127.0.0.1:{port}"), false).unwrap();

        // Stale tail from a hypothetical prior session.
        drv.state.lock().unwrap().read_carry = b"OLD_SESSION_TAIL".to_vec();

        drv.connect(&AsynUser::default().with_addr(-1)).unwrap();

        assert!(
            drv.state.lock().unwrap().read_carry.is_empty(),
            "DRV-51: connect must clear the staged read_carry"
        );
        drv.disconnect(&AsynUser::default().with_addr(-1)).unwrap();
    }

    /// DRV-46: the octet EOS interface must reflect the driver's real `eos`
    /// state (C prologixSetEos / prologixGetEos over `pdpvt->eos`), not the
    /// dead `base.{input,output}_eos` cache. `set_input_eos` routes to
    /// `State.eos`; `get_input_eos` reports it (eoslen 0 unset / 1 with the
    /// byte); eoslen > 1 is rejected; output EOS is unsupported on a GPIB port
    /// (asynGpib leaves the output-EOS vtable slots NULL).
    #[test]
    fn eos_interface_routes_to_driver_state() {
        let mut drv = DrvAsynPrologixPort::new("p", "127.0.0.1:1234", false).unwrap();

        // Default: no EOS -> eoslen 0 (C eos < 0).
        assert!(drv.get_input_eos().is_empty());
        assert_eq!(drv.eos(), None);

        // A single EOS byte routes to State.eos and is echoed by get_input_eos.
        drv.set_input_eos(b"\n").unwrap();
        assert_eq!(drv.eos(), Some(b'\n'));
        assert_eq!(drv.get_input_eos(), vec![b'\n']);

        // Clearing (eoslen 0) returns to None.
        drv.set_input_eos(b"").unwrap();
        assert_eq!(drv.eos(), None);
        assert!(drv.get_input_eos().is_empty());

        // eoslen > 1 is rejected (asynGpib "only 1 is allowed" / "Invalid EOS").
        assert!(drv.set_input_eos(b"\r\n").is_err());
        // The rejected call must not have mutated the driver EOS state.
        assert_eq!(drv.eos(), None);

        // Output EOS is unsupported on a GPIB port (C asynGpib NULL vtable).
        assert!(drv.set_output_eos(b"\n").is_err());
        assert!(drv.get_output_eos().is_empty());
    }

    /// DRV-47: the end-of-message rule must match C `readIt`
    /// (drvPrologixGPIB.c:334-345) at every boundary — full fit flags
    /// the boundary (EOS if configured, else END), a buffer-limited
    /// chunk flags CNT, and an exact fit flags both.
    #[test]
    fn read_eom_rule_matches_c_readit() {
        // Full fit, binary/EOI mode -> END only.
        let e = read_eom(5, 16, false);
        assert!(e.contains(EomReason::END));
        assert!(!e.contains(EomReason::EOS));
        assert!(!e.contains(EomReason::CNT));
        // Full fit, EOS configured -> EOS only.
        let e = read_eom(5, 16, true);
        assert!(e.contains(EomReason::EOS));
        assert!(!e.contains(EomReason::END));
        assert!(!e.contains(EomReason::CNT));
        // Buffer-limited (more of the message remains) -> CNT, no boundary.
        let e = read_eom(20, 8, false);
        assert!(e.contains(EomReason::CNT));
        assert!(!e.contains(EomReason::END));
        assert!(!e.contains(EomReason::EOS));
        // Exact fit -> boundary AND CNT (C sets both when remaining == maxchars).
        let e = read_eom(8, 8, false);
        assert!(e.contains(EomReason::END));
        assert!(e.contains(EomReason::CNT));
        let e = read_eom(8, 8, true);
        assert!(e.contains(EomReason::EOS));
        assert!(e.contains(EomReason::CNT));
    }

    /// DRV-47: serving a staged `read_carry` remainder through
    /// `io_read_octet_eom` must report the boundary — CNT while the
    /// caller buffer is too small, END once the remainder is fully
    /// drained (binary/EOI mode, no eos char).
    #[test]
    fn read_eom_carry_path_reports_boundary() {
        let (port, _rx) = start_mock_bridge();
        let mut drv = DrvAsynPrologixPort::new("p", &format!("127.0.0.1:{port}"), false).unwrap();
        drv.connect(&AsynUser::default().with_addr(-1)).unwrap();

        // Stage a remainder (EOT marker already stripped, so its end is
        // the true end of message).
        drv.state.lock().unwrap().read_carry = b"RESULT".to_vec();
        let user = AsynUser::default()
            .with_addr(3)
            .with_timeout(Duration::from_secs(2));

        // Buffer too small (4 < 6): partial -> CNT, no END.
        let mut small = [0u8; 4];
        let (n, eom) = drv.io_read_octet_eom(&user, &mut small).unwrap();
        assert_eq!(n, 4);
        assert_eq!(&small[..4], b"RESU");
        assert!(eom.contains(EomReason::CNT));
        assert!(!eom.contains(EomReason::END));

        // Remainder fits -> END, no CNT.
        let mut rest = [0u8; 16];
        let (n, eom) = drv.io_read_octet_eom(&user, &mut rest).unwrap();
        assert_eq!(n, 2);
        assert_eq!(&rest[..2], b"LT");
        assert!(eom.contains(EomReason::END));
        assert!(!eom.contains(EomReason::CNT));

        drv.disconnect(&AsynUser::default().with_addr(-1)).unwrap();
    }

    /// `read_octet` end-to-end: driver sends `++read eoi\n`, the
    /// bridge replies with payload + EOT_MARKER, the driver returns
    /// the payload with the marker stripped. Covers the no-EOS path
    /// (the more involved branch that does the disambiguation read).
    #[test]
    fn read_strips_eot_marker_in_eoi_mode() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut acc = Vec::new();
            let mut buf = [0u8; 4096];
            let mut version_sent = false;
            let mut read_replied = false;
            // Loop reading; reply to `++ver` and `++read eoi`. The
            // mock answers `++read eoi\n` with `"42.5\n\xEF"` —
            // EOT marker terminates the reply per `++eot_enable 1`.
            loop {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        acc.extend_from_slice(&buf[..n]);
                        if !version_sent && acc.windows(6).any(|w| w == b"++ver\n") {
                            stream.write_all(b"Prologix Test 1.0\r\n").unwrap();
                            version_sent = true;
                        }
                        if !read_replied && acc.windows(11).any(|w| w == b"++read eoi\n") {
                            stream.write_all(b"42.5\n\xEF").unwrap();
                            read_replied = true;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let mut drv = DrvAsynPrologixPort::new("p", &format!("127.0.0.1:{port}"), false).unwrap();
        drv.connect(&AsynUser::default().with_addr(-1)).unwrap();
        let user = AsynUser::default()
            .with_addr(0)
            .with_timeout(Duration::from_secs(2));
        let mut buf = [0u8; 64];
        let (n, eom) = drv.io_read_octet_eom(&user, &mut buf).unwrap();
        assert_eq!(
            &buf[..n],
            b"42.5\n",
            "EOT marker should be stripped, leaving `42.5\\n`"
        );
        // DRV-47: binary/EOI mode, the whole message fits the buffer ->
        // ASYN_EOM_END (not EOS, no CNT) per C readIt:339-340.
        assert!(
            eom.contains(EomReason::END),
            "EOI message boundary must flag END"
        );
        assert!(!eom.contains(EomReason::EOS));
        assert!(
            !eom.contains(EomReason::CNT),
            "full-fit read must NOT flag CNT"
        );
        drv.disconnect(&AsynUser::default().with_addr(-1)).unwrap();
    }

    /// `read_octet` with EOS char set: driver sends `++read <eos>\n`
    /// and the eos char IS data — must NOT be stripped (it's the
    /// payload terminator the consumer expects).
    #[test]
    fn read_with_eos_keeps_terminator_byte() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut acc = Vec::new();
            let mut buf = [0u8; 4096];
            let mut version_sent = false;
            let mut read_replied = false;
            // `eos = b'\n'` (10) → driver sends `++read 10\n`.
            loop {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        acc.extend_from_slice(&buf[..n]);
                        if !version_sent && acc.windows(6).any(|w| w == b"++ver\n") {
                            stream.write_all(b"Prologix Test 1.0\r\n").unwrap();
                            version_sent = true;
                        }
                        if !read_replied && acc.windows(10).any(|w| w == b"++read 10\n") {
                            stream.write_all(b"OK\n").unwrap();
                            read_replied = true;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let mut drv = DrvAsynPrologixPort::new("p", &format!("127.0.0.1:{port}"), false).unwrap();
        drv.connect(&AsynUser::default().with_addr(-1)).unwrap();
        drv.set_eos(Some(b'\n')).unwrap();
        let user = AsynUser::default()
            .with_addr(0)
            .with_timeout(Duration::from_secs(2));
        let mut buf = [0u8; 64];
        let (n, eom) = drv.io_read_octet_eom(&user, &mut buf).unwrap();
        assert_eq!(
            &buf[..n],
            b"OK\n",
            "eos char must be preserved as part of payload"
        );
        // DRV-47: with an EOS char configured the final chunk carries
        // ASYN_EOM_EOS (not END) per C readIt:337-338.
        assert!(
            eom.contains(EomReason::EOS),
            "EOS-mode message boundary must flag EOS"
        );
        assert!(!eom.contains(EomReason::END));
        drv.disconnect(&AsynUser::default().with_addr(-1)).unwrap();
    }

    /// Special chars in the user payload get the `\033` escape on
    /// the wire, mirroring C asyn's `stashChar` behaviour. Bridges
    /// without escaping would interpret an unescaped `++` in the
    /// payload as a configuration command.
    #[test]
    fn write_escapes_special_chars_on_wire() {
        let (port, rx) = start_mock_bridge();
        let mut drv = DrvAsynPrologixPort::new("p", &format!("127.0.0.1:{port}"), false).unwrap();
        drv.connect(&AsynUser::default().with_addr(-1)).unwrap();
        let mut user_w = AsynUser::default()
            .with_addr(0)
            .with_timeout(Duration::from_secs(2));
        // Payload contains `+` (must escape) and `A` (must not).
        drv.write_octet(&mut user_w, b"A+B").unwrap();
        drv.disconnect(&AsynUser::default().with_addr(-1)).unwrap();
        let captured = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let post_init_idx = captured.windows(6).position(|w| w == b"++ver\n").unwrap() + 6;
        let post = &captured[post_init_idx..];
        // After `++addr 0\n` the payload should be `A\x1B+B\n`.
        assert!(
            post.windows(5).any(|w| w == b"A\x1B+B\n"),
            "expected escaped payload `A\\033+B\\n` — got: {:?}",
            String::from_utf8_lossy(post)
        );
    }
}
