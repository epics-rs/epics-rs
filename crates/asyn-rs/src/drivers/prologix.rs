//! Prologix GPIB-Ethernet controller driver (asyn PR #129 equivalent).
//!
//! The Prologix GPIB-Ethernet bridge is a small box that exposes
//! GPIB instruments as a TCP socket — text commands prefixed with
//! `++` configure the bridge (address selection, EOI, EOS), plain
//! lines are forwarded to the currently-addressed GPIB instrument.
//!
//! Unlike [`super::ftdi`] / [`super::usbtmc`] this driver has no
//! external-dep gating: it's pure TCP atop [`super::ip_port`]'s
//! existing infrastructure. The wrapper here adds the Prologix-
//! specific addressing protocol so user code writes plain command
//! strings and the driver inserts the `++addr` headers.
//!
//! ## Configuration string
//!
//! `"hostname:1234 [addr=12]"` — TCP host:port for the Prologix
//! bridge plus optional default GPIB address. The address can be
//! changed at runtime via [`DrvAsynPrologixPort::set_gpib_address`]
//! before each instrument access.

use crate::error::{AsynError, AsynResult, AsynStatus};
use crate::port::{PortDriver, PortDriverBase, PortFlags};
use crate::user::AsynUser;
use std::sync::Mutex;

/// Configuration parsed from `"host:port [addr=N]"`.
#[derive(Debug, Clone)]
pub struct PrologixConfig {
    pub host: String,
    pub port: u16,
    /// Initial GPIB primary address (0..30). `None` defers selection
    /// until the first [`DrvAsynPrologixPort::set_gpib_address`].
    pub gpib_addr: Option<u8>,
}

impl PrologixConfig {
    pub fn parse(spec: &str) -> AsynResult<Self> {
        let mut tokens = spec.split_whitespace();
        let addr_part = tokens.next().ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: "empty Prologix spec".into(),
        })?;
        let (host, port_str) = addr_part.rsplit_once(':').ok_or_else(|| AsynError::Status {
            status: AsynStatus::Error,
            message: format!("expected host:port, got '{addr_part}'"),
        })?;
        let port: u16 = port_str.parse().map_err(|_| AsynError::Status {
            status: AsynStatus::Error,
            message: format!("invalid port in '{addr_part}'"),
        })?;

        let mut gpib_addr: Option<u8> = None;
        for tok in tokens {
            let (k, v) = tok.split_once('=').ok_or_else(|| AsynError::Status {
                status: AsynStatus::Error,
                message: format!("expected key=value, got '{tok}'"),
            })?;
            match k.to_ascii_lowercase().as_str() {
                "addr" | "gpib_addr" => {
                    let n: u8 = v.parse().map_err(|_| AsynError::Status {
                        status: AsynStatus::Error,
                        message: format!("invalid GPIB address '{v}'"),
                    })?;
                    if n > 30 {
                        return Err(AsynError::Status {
                            status: AsynStatus::Error,
                            message: format!("GPIB address {n} out of range (0..30)"),
                        });
                    }
                    gpib_addr = Some(n);
                }
                _ => {
                    return Err(AsynError::Status {
                        status: AsynStatus::Error,
                        message: format!("unknown Prologix config key '{k}'"),
                    });
                }
            }
        }

        Ok(Self {
            host: host.to_string(),
            port,
            gpib_addr,
        })
    }
}

pub struct DrvAsynPrologixPort {
    /// TCP transport is provided by [`super::ip_port::DrvAsynIPPort`].
    /// We own one and proxy `base()` / `connect()` / `read_octet()`
    /// to it; `write_octet()` injects the `++addr N\n` prefix when
    /// the selected GPIB address changes.
    inner: super::ip_port::DrvAsynIPPort,
    config: PrologixConfig,
    /// Currently-addressed GPIB primary address (0..30) or None
    /// when no address has been selected yet. Mutable for runtime
    /// switching between instruments on the same bridge.
    current_addr: Mutex<Option<u8>>,
    /// Last `++addr` value actually written to the bridge. `++addr`
    /// only re-sent when `current_addr` differs from this — the
    /// bridge keeps state between TCP writes, so re-sending the
    /// same address on every write wastes bandwidth.
    last_sent_addr: Mutex<Option<u8>>,
}

impl DrvAsynPrologixPort {
    pub fn new(port_name: &str, spec: &str) -> AsynResult<Self> {
        let config = PrologixConfig::parse(spec)?;
        let initial = config.gpib_addr;
        // Synthesize an ip_port spec — Prologix bridges are TCP.
        let ip_spec = format!("{}:{} TCP", config.host, config.port);
        let inner = super::ip_port::DrvAsynIPPort::new(port_name, &ip_spec)?;
        Ok(Self {
            inner,
            config,
            current_addr: Mutex::new(initial),
            last_sent_addr: Mutex::new(None),
        })
    }

    pub fn config(&self) -> &PrologixConfig {
        &self.config
    }

    /// Select which GPIB instrument the next read/write targets.
    /// Wire-side this becomes a `++addr <N>\n` command sent to the
    /// bridge before the user payload. Validates the range so a
    /// typo doesn't get sent to the bridge.
    pub fn set_gpib_address(&self, addr: u8) -> AsynResult<()> {
        if addr > 30 {
            return Err(AsynError::Status {
                status: AsynStatus::Error,
                message: format!("GPIB address {addr} out of range (0..30)"),
            });
        }
        *self.current_addr.lock().unwrap() = Some(addr);
        Ok(())
    }

    /// Build the `++addr` prefix line that selects the current
    /// GPIB target. Returns an empty string when no address has been
    /// configured (caller's responsibility — the bridge will use
    /// its last-configured address otherwise).
    pub fn addr_select_line(&self) -> String {
        match *self.current_addr.lock().unwrap() {
            Some(a) => format!("++addr {a}\n"),
            None => String::new(),
        }
    }

    /// Compute and return the `++addr N\n` prefix to send before the
    /// next write, advancing `last_sent_addr` to match `current_addr`.
    /// Returns `None` if no address change is required (first-write
    /// with no address selected, or already-current).
    fn take_addr_prefix(&self) -> Option<String> {
        let cur = *self.current_addr.lock().unwrap();
        let last = *self.last_sent_addr.lock().unwrap();
        match cur {
            Some(addr) if Some(addr) != last => {
                *self.last_sent_addr.lock().unwrap() = Some(addr);
                Some(format!("++addr {addr}\n"))
            }
            _ => None,
        }
    }
}

impl PortDriver for DrvAsynPrologixPort {
    fn base(&self) -> &PortDriverBase {
        self.inner.base()
    }
    fn base_mut(&mut self) -> &mut PortDriverBase {
        self.inner.base_mut()
    }

    fn connect(&mut self, user: &AsynUser) -> AsynResult<()> {
        // Reset addr-select tracking — a fresh TCP connection means the
        // bridge's last-configured address is unknown to us; force the
        // next write to re-send `++addr` so we don't accidentally target
        // whatever instrument was last selected by another client.
        *self.last_sent_addr.lock().unwrap() = None;
        self.inner.connect(user)
    }

    fn read_octet(&mut self, user: &AsynUser, buf: &mut [u8]) -> AsynResult<usize> {
        self.inner.read_octet(user, buf)
    }

    fn write_octet(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<()> {
        if let Some(prefix) = self.take_addr_prefix() {
            self.inner.write_octet(user, prefix.as_bytes())?;
        }
        self.inner.write_octet(user, data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal() {
        let cfg = PrologixConfig::parse("192.168.1.10:1234").unwrap();
        assert_eq!(cfg.host, "192.168.1.10");
        assert_eq!(cfg.port, 1234);
        assert!(cfg.gpib_addr.is_none());
    }

    #[test]
    fn parse_with_addr() {
        let cfg = PrologixConfig::parse("prologix.lab:1234 addr=12").unwrap();
        assert_eq!(cfg.host, "prologix.lab");
        assert_eq!(cfg.gpib_addr, Some(12));
    }

    #[test]
    fn parse_rejects_out_of_range_addr() {
        assert!(PrologixConfig::parse("h:1 addr=31").is_err());
    }

    #[test]
    fn parse_rejects_unknown_key() {
        assert!(PrologixConfig::parse("h:1 foo=2").is_err());
    }

    #[test]
    fn set_address_validates_range() {
        let drv = DrvAsynPrologixPort::new("p", "h:1234").unwrap();
        assert!(drv.set_gpib_address(0).is_ok());
        assert!(drv.set_gpib_address(30).is_ok());
        assert!(drv.set_gpib_address(31).is_err());
    }

    #[test]
    fn addr_select_line_formats_correctly() {
        let drv = DrvAsynPrologixPort::new("p", "h:1234").unwrap();
        assert_eq!(drv.addr_select_line(), "");
        drv.set_gpib_address(7).unwrap();
        assert_eq!(drv.addr_select_line(), "++addr 7\n");
    }

    #[test]
    fn initial_addr_from_spec() {
        let drv = DrvAsynPrologixPort::new("p", "h:1234 addr=15").unwrap();
        assert_eq!(drv.addr_select_line(), "++addr 15\n");
    }

    #[test]
    fn take_addr_prefix_emits_only_on_change() {
        let drv = DrvAsynPrologixPort::new("p", "h:1234").unwrap();
        // No address selected — no prefix.
        assert_eq!(drv.take_addr_prefix(), None);
        drv.set_gpib_address(7).unwrap();
        // First time after selecting — emit prefix.
        assert_eq!(drv.take_addr_prefix(), Some("++addr 7\n".into()));
        // Second time with same address — suppress.
        assert_eq!(drv.take_addr_prefix(), None);
        // Switch — emit new prefix.
        drv.set_gpib_address(12).unwrap();
        assert_eq!(drv.take_addr_prefix(), Some("++addr 12\n".into()));
        assert_eq!(drv.take_addr_prefix(), None);
    }

    /// Loopback test — start a TCP listener that mimics a Prologix
    /// bridge, drive the Prologix driver, and assert the captured
    /// wire bytes include `++addr N\n` before the payload.
    ///
    /// Verifies: TCP delegation actually opens a socket via the
    /// embedded `DrvAsynIPPort`, write_octet injects the address
    /// header on first write, and a re-write with no address change
    /// does NOT re-emit the header.
    #[test]
    fn loopback_write_emits_addr_prefix() {
        use std::io::Read;
        use std::net::TcpListener;
        use std::sync::mpsc;
        use std::thread;
        use std::time::Duration;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            // Read everything the driver sends in this test (two write
            // calls — only one `++addr` prefix expected). 256-byte buffer
            // is plenty for `"++addr 9\n*IDN?\n*IDN?\n"`.
            let mut acc = Vec::new();
            let mut buf = [0u8; 256];
            // First read picks up prefix + payload1; second picks up payload2.
            for _ in 0..2 {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => acc.extend_from_slice(&buf[..n]),
                    Err(_) => break,
                }
            }
            tx.send(acc).unwrap();
        });

        let mut drv = DrvAsynPrologixPort::new("p", &format!("127.0.0.1:{port}")).unwrap();
        drv.set_gpib_address(9).unwrap();
        let user = AsynUser::default();
        drv.connect(&user).unwrap();
        let mut user = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        drv.write_octet(&mut user, b"*IDN?\n").unwrap();
        // Same address — must NOT re-emit prefix.
        drv.write_octet(&mut user, b"*IDN?\n").unwrap();

        handle.join().unwrap();
        let captured = rx.recv().unwrap();
        let s = String::from_utf8(captured).unwrap();
        assert_eq!(
            s, "++addr 9\n*IDN?\n*IDN?\n",
            "expected exactly one ++addr header followed by both payloads, got {s:?}"
        );
    }

    /// Loopback test — switching address mid-stream re-emits `++addr`
    /// once at the boundary, then suppresses again.
    #[test]
    fn loopback_address_switch_re_emits_prefix() {
        use std::io::Read;
        use std::net::TcpListener;
        use std::sync::mpsc;
        use std::thread;
        use std::time::Duration;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut acc = Vec::new();
            let mut buf = [0u8; 256];
            // Three writes max — first burst (`++addr 3\nP1\n`), second
            // burst (`++addr 7\nP2\n`). Loop until timeout or EOF.
            while let Ok(n) = stream.read(&mut buf) {
                if n == 0 {
                    break;
                }
                acc.extend_from_slice(&buf[..n]);
                if acc.windows(2).filter(|w| w == b"P2").count() > 0 {
                    break;
                }
            }
            tx.send(acc).unwrap();
        });

        let mut drv = DrvAsynPrologixPort::new("p", &format!("127.0.0.1:{port}")).unwrap();
        let user = AsynUser::default();
        drv.connect(&user).unwrap();
        let mut user = AsynUser::new(0).with_timeout(Duration::from_secs(2));
        drv.set_gpib_address(3).unwrap();
        drv.write_octet(&mut user, b"P1\n").unwrap();
        drv.set_gpib_address(7).unwrap();
        drv.write_octet(&mut user, b"P2\n").unwrap();

        handle.join().unwrap();
        let s = String::from_utf8(rx.recv().unwrap()).unwrap();
        assert_eq!(s, "++addr 3\nP1\n++addr 7\nP2\n");
    }

    /// `connect()` resets `last_sent_addr` so a reconnect forces the
    /// next write to re-send `++addr` — preventing accidental targeting
    /// of an instrument the bridge selected for some other client.
    #[test]
    fn connect_clears_last_sent_addr() {
        use std::net::TcpListener;
        use std::thread;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let _handle = thread::spawn(move || {
            // Accept twice — once for the first connect, once for the
            // post-disconnect reconnect. After the second accept just
            // drain whatever shows up so writes don't block.
            for _ in 0..2 {
                if let Ok((mut s, _)) = listener.accept() {
                    use std::io::Read;
                    let _ = s.set_read_timeout(Some(std::time::Duration::from_millis(200)));
                    let mut buf = [0u8; 64];
                    let _ = s.read(&mut buf);
                }
            }
        });

        let mut drv = DrvAsynPrologixPort::new("p", &format!("127.0.0.1:{port}")).unwrap();
        drv.set_gpib_address(5).unwrap();
        let user = AsynUser::default();
        drv.connect(&user).unwrap();
        // After connect the prefix should still pend (it hasn't been
        // emitted yet — connect itself doesn't send anything).
        assert_eq!(drv.take_addr_prefix(), Some("++addr 5\n".into()));
        assert_eq!(drv.take_addr_prefix(), None);
        // Simulate a reconnect — should re-arm the prefix.
        drv.connect(&user).unwrap();
        assert_eq!(drv.take_addr_prefix(), Some("++addr 5\n".into()));
    }
}
