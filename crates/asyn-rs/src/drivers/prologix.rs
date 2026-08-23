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

    /// Stage bytes for the next `read_octet` to serve before it talks to the
    /// bridge again — the Rust equivalent of C leaving `pdpvt->bufCount > 0`
    /// (`drvPrologixGPIB.c:250`, `if (pdpvt->bufCount == 0)` gates the whole
    /// `++read` block). Single owner for every write to `read_carry`; the only
    /// other mutation is [`clear_read_carry`], which is C's `bufCount = 0`.
    ///
    /// [`clear_read_carry`]: Self::clear_read_carry
    fn stage_read_carry(&self, bytes: Vec<u8>) {
        self.state.lock().unwrap().read_carry = bytes;
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
        base.init_connected(false);
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

    /// The `++eot_enable` argument the bridge needs for the given EOS mode.
    /// C `prologixSetEos` (drvPrologixGPIB.c:456) sends `++eot_enable (eos<0)`:
    /// in EOI mode (no eos char) the bridge must append the EOT marker on EOI
    /// detection so the reader can find the end of message (enable=1); in EOS
    /// mode the read terminates on the eos char, so the EOT marker must be
    /// disabled (enable=0) — otherwise the bridge trails it after the eos byte
    /// and the eos-terminated read never sees its terminator. (C computes this
    /// from the *stale* pre-update eos and never stores the new value — a bug
    /// not copied; we derive it from the live state.)
    fn eot_enable_arg(eos: Option<u8>) -> u8 {
        u8::from(eos.is_none())
    }

    /// Set EOS char (`Some(c)`) or disable (`None` — EOI / EOT-marker mode).
    /// Mirrors C asyn `prologixSetEos` (drvPrologixGPIB.c:439-459), but realizes
    /// the intent C's never-store bug dropped: the driver-level `eos` actually
    /// changes, and the bridge's `++eot_enable` is re-issued to follow it (1 in
    /// EOI mode, 0 in EOS mode, see `Self::eot_enable_arg`) so the EOT marker
    /// never collides with an eos-terminated read. Single owner of the eos
    /// transition (also reached via `set_input_eos`).
    pub fn set_eos(&mut self, eos: Option<u8>) -> AsynResult<()> {
        if self.state.lock().unwrap().eos == eos {
            return Ok(());
        }
        // Re-issue the bridge EOT mode only while connected; otherwise the
        // connect handshake applies it (it derives `++eot_enable` from this
        // state). Commit `State.eos` only after a successful bridge write, so a
        // failed write never leaves the cached mode out of sync with the
        // device (the DRV-35 commit-after-apply rule).
        if self.base.is_connected() {
            let cmd = format!("++eot_enable {}\n", Self::eot_enable_arg(eos));
            let mut bridge_user = AsynUser::default().with_timeout(Duration::from_secs(1));
            self.inner.write_octet(&mut bridge_user, cmd.as_bytes())?;
        }
        self.state.lock().unwrap().eos = eos;
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

/// Owns the inner IP port's connection for the length of the `++ver`
/// handshake and gives it back unless [`Self::commit`] runs.
///
/// **Invariant: the inner port is connected only while the outer Prologix
/// port is, or while this guard is alive.** The handshake has four exits
/// after the dial — a failed init write, a bridge that closed, a version
/// string past the 200-byte cap, and a read that timed out — and a cleanup
/// branch on each is a patch that the next exit re-opens. Holding the
/// connection in a guard instead means `?`, an explicit `return` and an
/// unwind all give it back, so a half-built session cannot be constructed.
///
/// This is a deliberate deviation from C: `prologixConnect` returns at
/// `drvPrologixGPIB.c:189`, `:196` and `:202` without disconnecting
/// `pasynUserTCPcommon`, which leaves the TCP port holding a socket while the
/// GPIB port stays down, and every retry then answers `drvAsynIPPort.c:424-427`
/// `"Link already open!"`. See `doc/upstream-c-bugs.md`.
struct HandshakeLink<'a> {
    port: &'a mut super::ip_port::DrvAsynIPPort,
    /// C's `prologixConnect` disconnects with the same `pasynUser` it
    /// connected with; keeping it here is what lets `Drop` do the same.
    user: &'a AsynUser,
    committed: bool,
}

impl<'a> HandshakeLink<'a> {
    /// Dial the inner port and take ownership of the connection.
    fn open(port: &'a mut super::ip_port::DrvAsynIPPort, user: &'a AsynUser) -> AsynResult<Self> {
        port.connect(user)?;
        Ok(Self {
            port,
            user,
            committed: false,
        })
    }

    /// The transport, for the handshake's own writes and reads.
    fn port(&mut self) -> &mut super::ip_port::DrvAsynIPPort {
        self.port
    }

    /// The handshake completed — the connection stays.
    fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for HandshakeLink<'_> {
    fn drop(&mut self) {
        if !self.committed {
            // The handshake's own error is what the caller reports; the only
            // thing this has to guarantee is that no socket is left for the
            // already-open guard to trip over on the next connect.
            let _ = self.port.disconnect(self.user);
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

    /// C drvPrologixGPIB takes every interface it has from
    /// `pasynGpib->registerPort` (drvPrologixGPIB.c:592) — asynCommon +
    /// asynOctet + asynGpib + asynInt32 — and registers no asynOption.
    fn capabilities(&self) -> Vec<crate::interfaces::Capability> {
        crate::interfaces::gpib::gpib_port_capabilities()
    }

    /// asynInt32 on a GPIB port is asynGpib's SRQ interrupt source, not a
    /// readable register: `read`/`write` are the asynInt32Base defaults and
    /// fail. See [`crate::interfaces::gpib::int32_read_not_supported`], which
    /// documents why the read reports the READ (CBUG-B10 — C says "write").
    fn read_int32(&mut self, _user: &AsynUser) -> AsynResult<i32> {
        Err(crate::interfaces::gpib::int32_read_not_supported())
    }

    /// See [`Self::read_int32`].
    fn write_int32(&mut self, _user: &mut AsynUser, _value: i32) -> AsynResult<()> {
        Err(crate::interfaces::gpib::int32_write_not_supported())
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
            let mut link = HandshakeLink::open(&mut self.inner, user)?;
            // 8-line init burst — sent as one TCP write to match
            // C asyn (single `pasynOctetSyncIO->write`). C hardcodes
            // `++eot_enable 1` here (drvPrologixGPIB.c:182) because its
            // driver-level eos is always -1 (the never-store bug). This port
            // does eos at the driver level, so the EOT mode must follow the
            // configured eos even when it was set before connect: derive it
            // from State.eos (default None -> 1, matching C's common case).
            let eot_enable = Self::eot_enable_arg(self.state.lock().unwrap().eos);
            let init = format!(
                "++savecfg 0\n++mode 1\n++ifc\n++eos 3\n++eoi 1\n\
                 ++eot_char {EOT_MARKER}\n++eot_enable {eot_enable}\n++ver\n",
            );
            let mut tu = AsynUser::default().with_timeout(Duration::from_secs(1));
            link.port().write_octet(&mut tu, init.as_bytes())?;
            // Read the version response — chars accumulate until the
            // bridge sends `\r\n`. C asyn caps at 200 bytes total.
            let mut acc = Vec::with_capacity(64);
            let mut buf = [0u8; 64];
            loop {
                let ru = AsynUser::default().with_timeout(Duration::from_millis(500));
                let n = link.port().read_octet(&ru, &mut buf)?;
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
            link.commit();
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
    //
    // The `asynUser` selects nothing here even though the port is multi-device:
    // the Prologix adapter has ONE `++eos` register for the whole bus
    // (drvPrologixGPIB.c:449-458 writes the controller, not a per-address
    // table), so every GPIB address on the adapter shares it. That is the
    // driver's own device model, not the port-wide EOS the base hook used to
    // impose on every port.
    fn set_input_eos(&mut self, _user: &AsynUser, eos: &[u8]) -> AsynResult<()> {
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

    fn get_input_eos(&self, _user: &AsynUser) -> Vec<u8> {
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
    fn set_output_eos(&mut self, _user: &AsynUser, _eos: &[u8]) -> AsynResult<()> {
        Err(AsynError::Status {
            status: AsynStatus::Error,
            message: "output EOS not supported on a GPIB port".into(),
        })
    }

    fn get_output_eos(&self, _user: &AsynUser) -> Vec<u8> {
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
                Err(e) => {
                    // C `prologixRead` (drvPrologixGPIB.c:297-303): a failed
                    // chunk read `return status` *without* touching
                    // `pdpvt->bufCount`, which line 303 has already advanced
                    // over every chunk that did arrive. Those bytes stay in
                    // `pdpvt->buf`, and because the accumulate block is gated on
                    // `bufCount == 0` (:250) the next `prologixRead` skips the
                    // bridge entirely and delivers them. The retention is
                    // deliberate: the resize-failure path two lines up (:288-290)
                    // *does* reset `bufCount = 0` before returning.
                    //
                    // The bytes are staged verbatim — C's `bufCount--` that drops
                    // the EOT marker (:330-331) is below the loop and never runs
                    // on this path, so a marker already in `acc` is served as data.
                    if !acc.is_empty() {
                        self.stage_read_carry(acc);
                    }
                    return Err(e);
                }
            }
        }
        // Strip the trailing terminator: the EOT marker in EOI mode (it's
        // framing, not data) or the matched eos byte in EOS mode. Mirrors
        // the asynGpib read layer (asynGpib.c:415-419) and the standard
        // EosInterpose, both of which remove the matched terminator before
        // the record sees the data; streamDevice and asynRecord expect the
        // eos-stripped form.
        if acc.last() == Some(&terminator) {
            acc.pop();
        }
        let remaining = acc.len();
        let n = remaining.min(buf.len());
        buf[..n].copy_from_slice(&acc[..n]);
        if n < remaining {
            // Caller's buffer was too small — stash the remainder so the
            // next read_octet returns it instead of dropping device data.
            // (C keeps the whole reply in `buf` and advances `bufIndex`, :347.)
            self.stage_read_carry(acc.split_off(n));
        }
        let eom = read_eom(remaining, buf.len(), eos.is_some());
        Ok((n, eom))
    }

    // --- asynGpib, C `prologixMethods` (drvPrologixGPIB.c:527-545) ---
    //
    // The bridge driver implements almost none of IEEE-488 bus control: only
    // `ifc` is real. The rest return asynError with the driver's own text, which
    // asynRecord splices into ERRS — the whole point of registering the
    // interface anyway (C's GPIBIV is 1 for this port, so UCMD reaches
    // `prologixUniversalCmd` and reports *its* failure, not "No asynGpib
    // interface").
    //
    // Not ported (no in-tree consumer): `prologixSrqStatus` (:493, always 0),
    // `prologixSrqEnable` (:500, no-op), `prologixSerialPollBegin` / `SerialPoll`
    // / `SerialPollEnd` (:506-524, all unimplemented). They exist in C only to
    // feed asynGpib's SRQ poll thread.

    /// C `prologixAddressedCmd` (drvPrologixGPIB.c:461-467): unimplemented.
    fn gpib_addressed_cmd(&mut self, _user: &mut AsynUser, _data: &[u8]) -> AsynResult<()> {
        Err(AsynError::Status {
            status: AsynStatus::Error,
            message: "prologixAddressedCmd unimplemented".into(),
        })
    }

    /// C `prologixUniversalCmd` (drvPrologixGPIB.c:469-474): unimplemented.
    fn gpib_universal_cmd(&mut self, _user: &mut AsynUser, _cmd: u8) -> AsynResult<()> {
        Err(AsynError::Status {
            status: AsynStatus::Error,
            message: "prologixUniversalCmd unimplemented".into(),
        })
    }

    /// C `prologixIfc` (drvPrologixGPIB.c:476-484): assert Interface Clear by
    /// writing the bridge command `++ifc\n` to the TCP transport. C writes it
    /// with `pasynOctetSyncIO->write(..., 1.0, &nt)` — its own 1 s timeout, not
    /// the caller's.
    fn gpib_ifc(&mut self, _user: &mut AsynUser) -> AsynResult<()> {
        let mut bridge_user = AsynUser::default().with_timeout(Duration::from_secs(1));
        self.inner.write_octet(&mut bridge_user, b"++ifc\n")?;
        Ok(())
    }

    /// C `prologixRen` (drvPrologixGPIB.c:486-491): unimplemented.
    fn gpib_ren(&mut self, _user: &mut AsynUser, _enable: bool) -> AsynResult<()> {
        Err(AsynError::Status {
            status: AsynStatus::Error,
            message: "prologixRen unimplemented".into(),
        })
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
        assert!(drv.base.is_connected());
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

    /// How the fake bridge answers `++ver` — one variant per exit the
    /// handshake can take after the inner port has been dialled.
    #[derive(Clone, Copy)]
    enum BridgeReply {
        /// Read the init burst and answer nothing. The 500 ms handshake read
        /// times out, and a plain timeout does not tear the inner port down
        /// (`disconnect_on_read_timeout` is false by C parity), so this is the
        /// exit that used to wedge the bus.
        Silent,
        /// Read the init burst, then close.
        Close,
        /// Answer past the 200-byte cap with no `\r\n` terminator.
        Overlong,
    }

    /// A bridge that never completes the handshake and keeps accepting, so a
    /// second `connect()` has somewhere to land.
    fn start_failing_bridge(reply: BridgeReply) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            // Accepted sockets are parked rather than dropped: dropping one
            // closes it, which would collapse every variant into `Close`.
            let mut held = Vec::new();
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                // Take the init burst first, so the failure lands on the
                // `++ver` read rather than on the write before it.
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                match reply {
                    BridgeReply::Silent => held.push(stream),
                    BridgeReply::Close => drop(stream),
                    BridgeReply::Overlong => {
                        let _ = stream.write_all(&[b'v'; 256]);
                        held.push(stream);
                    }
                }
            }
        });
        port
    }

    /// What every handshake exit owes: the inner transport is given back, and
    /// the next connect reaches the handshake again instead of answering
    /// `"Link already open!"` for the life of the IOC.
    fn assert_handshake_exit_is_retryable(reply: BridgeReply) {
        let port = start_failing_bridge(reply);
        let mut drv =
            DrvAsynPrologixPort::new("p_retry", &format!("127.0.0.1:{port}"), false).unwrap();
        let user = AsynUser::default().with_addr(-1);

        let first = drv
            .connect(&user)
            .expect_err("the bridge never completes the handshake");
        assert!(!drv.base.is_connected());
        assert!(
            !drv.inner.base().is_connected(),
            "the inner port must be given back, first connect said: {first}"
        );

        let second = drv
            .connect(&user)
            .expect_err("the bridge never completes the handshake");
        assert!(
            !second.to_string().contains("Link already open"),
            "the retry must reach the handshake again, got: {second}"
        );
    }

    #[test]
    fn a_silent_bridge_leaves_the_gpib_port_retryable() {
        assert_handshake_exit_is_retryable(BridgeReply::Silent);
    }

    #[test]
    fn a_bridge_that_closes_mid_handshake_leaves_the_gpib_port_retryable() {
        assert_handshake_exit_is_retryable(BridgeReply::Close);
    }

    #[test]
    fn an_overlong_version_string_leaves_the_gpib_port_retryable() {
        assert_handshake_exit_is_retryable(BridgeReply::Overlong);
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

    /// DRV-50 / R10-55: `PortDriver::gpib_ifc` asserts Interface Clear by
    /// writing `++ifc\n` to the bridge (C `prologixIfc`,
    /// drvPrologixGPIB.c:476-484).
    #[test]
    fn gpib_ifc_writes_bridge_command() {
        let (port, rx) = start_mock_bridge();
        let mut drv = DrvAsynPrologixPort::new("p", &format!("127.0.0.1:{port}"), false).unwrap();
        drv.connect(&AsynUser::default().with_addr(-1)).unwrap();

        drv.gpib_ifc(&mut AsynUser::default().with_timeout(Duration::from_secs(2)))
            .unwrap();

        drv.disconnect(&AsynUser::default().with_addr(-1)).unwrap();
        let captured = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let s = String::from_utf8_lossy(&captured).to_string();
        // Strip the init burst (which contains its own ++ifc\n) and check the
        // post-init bytes are exactly the ifc command.
        let init_end = s.find("++ver\n").unwrap() + "++ver\n".len();
        assert_eq!(&s[init_end..], "++ifc\n", "ifc must write ++ifc\\n");
    }

    /// DRV-50 / R10-55: the GPIB command interface matches C `prologixMethods`
    /// (drvPrologixGPIB.c:527-545) — `ifc` is the only real bus operation; the
    /// other three report the C driver's own "unimplemented" text, which is what
    /// a UCMD/ACMD put lands in ERRS.
    #[test]
    fn gpib_command_interface_matches_c_methods() {
        let mut drv = DrvAsynPrologixPort::new("p", "127.0.0.1:1234", false).unwrap();
        let mut user = AsynUser::default();

        let err = drv
            .gpib_universal_cmd(&mut user, crate::interfaces::gpib::IBDCL)
            .unwrap_err();
        assert_eq!(err.message(), "prologixUniversalCmd unimplemented");

        let err = drv
            .gpib_addressed_cmd(&mut user, &[0x5f, 0x3f, 0x27, 0x08, 0x5f, 0x3f])
            .unwrap_err();
        assert_eq!(err.message(), "prologixAddressedCmd unimplemented");

        let err = drv.gpib_ren(&mut user, true).unwrap_err();
        assert_eq!(err.message(), "prologixRen unimplemented");
    }

    /// R10-55. Prologix takes every interface it has from
    /// `pasynGpib->registerPort` (drvPrologixGPIB.c:592): asynCommon +
    /// asynOctet + asynGpib + asynInt32, and no asynOption. The asynInt32 is
    /// asynGpib's SRQ interrupt source with a NULL vtable (asynGpib.c:140), so
    /// reading or writing it lands in the asynInt32Base defaults and fails.
    #[test]
    fn prologix_registers_the_gpib_port_interfaces() {
        use crate::interfaces::Capability;

        let mut drv = DrvAsynPrologixPort::new("p", "127.0.0.1:1234", false).unwrap();
        let caps = drv.capabilities();
        for cap in [
            Capability::Gpib,
            Capability::Int32Read,
            Capability::Int32Write,
            Capability::OctetRead,
            Capability::OctetWrite,
        ] {
            assert!(caps.contains(&cap), "prologix must declare {cap:?}");
        }
        assert!(
            !caps.contains(&Capability::Option),
            "drvPrologixGPIB registers no asynOption"
        );

        let mut user = AsynUser::default();
        // CBUG-B10: C's asynInt32Base `readDefault` reports "write is not
        // supported" (a copy-paste from `writeDefault`); the read path here
        // names the read.
        assert_eq!(
            drv.read_int32(&user).unwrap_err().message(),
            "read is not supported"
        );
        assert_eq!(
            drv.write_int32(&mut user, 1).unwrap_err().message(),
            "write is not supported"
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
        assert!(drv.get_input_eos(&AsynUser::default()).is_empty());
        assert_eq!(drv.eos(), None);

        // A single EOS byte routes to State.eos and is echoed by get_input_eos.
        drv.set_input_eos(&AsynUser::default(), b"\n").unwrap();
        assert_eq!(drv.eos(), Some(b'\n'));
        assert_eq!(drv.get_input_eos(&AsynUser::default()), vec![b'\n']);

        // Clearing (eoslen 0) returns to None.
        drv.set_input_eos(&AsynUser::default(), b"").unwrap();
        assert_eq!(drv.eos(), None);
        assert!(drv.get_input_eos(&AsynUser::default()).is_empty());

        // eoslen > 1 is rejected (asynGpib "only 1 is allowed" / "Invalid EOS").
        assert!(drv.set_input_eos(&AsynUser::default(), b"\r\n").is_err());
        // The rejected call must not have mutated the driver EOS state.
        assert_eq!(drv.eos(), None);

        // Output EOS is unsupported on a GPIB port (C asynGpib NULL vtable).
        assert!(drv.set_output_eos(&AsynUser::default(), b"\n").is_err());
        assert!(drv.get_output_eos(&AsynUser::default()).is_empty());
    }

    /// DRV-46(b): the bridge `++eot_enable` must follow the configured eos so
    /// the EOT marker never collides with an eos-terminated read (C
    /// prologixSetEos design, drvPrologixGPIB.c:456). Setting an eos char while
    /// connected issues `++eot_enable 0`; clearing it restores `++eot_enable 1`.
    #[test]
    fn eos_mode_toggles_bridge_eot_enable() {
        let (port, rx) = start_mock_bridge();
        let mut drv = DrvAsynPrologixPort::new("p", &format!("127.0.0.1:{port}"), false).unwrap();
        drv.connect(&AsynUser::default().with_addr(-1)).unwrap();

        drv.set_eos(Some(b'\n')).unwrap(); // enter EOS mode -> eot_enable 0
        drv.set_eos(None).unwrap(); // leave EOS mode -> eot_enable 1

        drv.disconnect(&AsynUser::default().with_addr(-1)).unwrap();
        let captured = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let s = String::from_utf8_lossy(&captured).to_string();
        // Strip the init burst (its own ++eot_enable) to focus on the post-init
        // transitions.
        let init_end = s.find("++ver\n").unwrap() + "++ver\n".len();
        assert_eq!(
            &s[init_end..],
            "++eot_enable 0\n++eot_enable 1\n",
            "eos set/clear must toggle the bridge EOT mode: {:?}",
            &s[init_end..]
        );
    }

    /// DRV-46(b): an eos char configured before connect makes the connect init
    /// burst select `++eot_enable 0` — the handshake derives the EOT mode from
    /// State.eos (C's always-1 connect is only correct because its eos is
    /// permanently -1).
    #[test]
    fn eos_set_before_connect_seeds_eot_enable_off() {
        let (port, rx) = start_mock_bridge();
        let mut drv = DrvAsynPrologixPort::new("p", &format!("127.0.0.1:{port}"), false).unwrap();
        // Set eos while disconnected: no bridge write, just cached state.
        drv.set_eos(Some(b'\n')).unwrap();
        drv.connect(&AsynUser::default().with_addr(-1)).unwrap();

        drv.disconnect(&AsynUser::default().with_addr(-1)).unwrap();
        let captured = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let s = String::from_utf8_lossy(&captured).to_string();
        assert!(
            s.contains("++eot_enable 0\n"),
            "init burst must select ++eot_enable 0 when eos preset: {s:?}"
        );
        assert!(
            !s.contains("++eot_enable 1\n"),
            "no ++eot_enable 1 should appear when eos preset: {s:?}"
        );
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
    /// and the matched eos byte is stripped from the data the record
    /// sees, mirroring the asynGpib read layer (asynGpib.c:415-419) and
    /// the standard EosInterpose. The boundary still flags EOS.
    #[test]
    fn read_with_eos_strips_terminator_byte() {
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
            b"OK",
            "matched eos byte must be stripped from the payload"
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

    /// R6-51: a read that fails part-way through the reply must KEEP the bytes
    /// that did arrive. C `prologixRead` returns the chunk-read status
    /// (drvPrologixGPIB.c:301-302) with `pdpvt->bufCount` still counting every
    /// chunk it appended (:303); the next call's `bufCount == 0` gate (:250)
    /// then fails, so it skips the bridge and delivers those bytes. The
    /// deliberateness shows two lines up: the resize-failure path (:288-290)
    /// *does* zero `bufCount` before returning.
    #[test]
    fn read_error_retains_partial_bytes_for_the_next_call() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut acc = Vec::new();
            let mut buf = [0u8; 4096];
            let mut version_sent = false;
            let mut read_replied = false;
            loop {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        acc.extend_from_slice(&buf[..n]);
                        if !version_sent && acc.windows(6).any(|w| w == b"++ver\n") {
                            stream.write_all(b"Prologix Test 1.0\r\n").unwrap();
                            version_sent = true;
                        }
                        // Answer the first `++read 10\n` with a reply that never
                        // reaches its eos byte, then go silent: the driver's next
                        // chunk read times out mid-message.
                        if !read_replied && acc.windows(10).any(|w| w == b"++read 10\n") {
                            stream.write_all(b"PARTI").unwrap();
                            read_replied = true;
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = tx.send(acc);
        });

        let mut drv = DrvAsynPrologixPort::new("p", &format!("127.0.0.1:{port}"), false).unwrap();
        drv.connect(&AsynUser::default().with_addr(-1)).unwrap();
        drv.set_eos(Some(b'\n')).unwrap();

        // Short timeout: the reply is incomplete, so the read errors out.
        let user = AsynUser::default()
            .with_addr(0)
            .with_timeout(Duration::from_millis(300));
        let mut buf = [0u8; 64];
        let err = drv
            .io_read_octet_eom(&user, &mut buf)
            .expect_err("incomplete reply must fail the read");
        assert!(
            matches!(
                err,
                AsynError::Status {
                    status: AsynStatus::Timeout,
                    ..
                }
            ),
            "expected a timeout, got {err:?}"
        );

        // The five bytes that did arrive are staged, not dropped.
        assert_eq!(
            drv.state.lock().unwrap().read_carry,
            b"PARTI".to_vec(),
            "R6-51: bytes read before the error must survive it"
        );

        // The next call serves them without going back to the bridge — C's
        // `bufCount != 0` short-circuit.
        let (n, _eom) = drv
            .io_read_octet_eom(&user, &mut buf)
            .expect("staged bytes are served from the carry");
        assert_eq!(&buf[..n], b"PARTI");

        drv.disconnect(&AsynUser::default().with_addr(-1)).unwrap();
        let captured = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let reads = captured
            .windows(10)
            .filter(|w| *w == b"++read 10\n")
            .count();
        assert_eq!(
            reads, 1,
            "the second read_octet must not issue another ++read"
        );
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
