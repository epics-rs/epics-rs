//! Trace/logging system (asynTrace equivalent).
//!
//! Provides per-port configurable tracing with support for multiple output
//! destinations, I/O data formatting, and bitflag-based mask filtering.

use std::collections::HashMap;
use std::io::Write;
use std::sync::{Arc, Mutex};

use bitflags::bitflags;

bitflags! {
    /// What to trace — control message categories.
    ///
    /// Values match C asyn `asynDriver.h:211-216` exactly:
    /// ```text
    ///   ASYN_TRACE_ERROR     0x0001
    ///   ASYN_TRACEIO_DEVICE  0x0002
    ///   ASYN_TRACEIO_FILTER  0x0004
    ///   ASYN_TRACEIO_DRIVER  0x0008
    ///   ASYN_TRACE_FLOW      0x0010
    ///   ASYN_TRACE_WARNING   0x0020
    /// ```
    /// C asyn defines exactly these 6 bits — no `ASYN_TRACE_STATE`
    /// or any other bit is referenced anywhere in the C source.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct TraceMask: u32 {
        const ERROR      = 0x0001;
        const IO_DEVICE  = 0x0002;
        const IO_FILTER  = 0x0004;
        const IO_DRIVER  = 0x0008;
        const FLOW       = 0x0010;
        const WARNING    = 0x0020;
    }
}

impl TraceMask {
    /// Parse a symbolic mask string the way C asyn does
    /// (asynShellCommands.c:670-699 `asynTraceMaskStringToInt`):
    ///
    /// - **Tokens**: short names `ERROR`, `DEVICE`, `FILTER`,
    ///   `DRIVER`, `FLOW`, `WARNING`. C accepts these directly OR
    ///   with `ASYN_` and `TRACE_` / `TRACEIO_` prefixes stripped
    ///   (`ASYN_TRACEIO_DEVICE` → `DEVICE`).
    /// - **Separators**: `|` OR `+` (C: `*maskStr == '|' || == '+'`).
    /// - **Numeric**: decimal / `0x` hex / leading-`0` octal via
    ///   `strtol(.., 0)`.
    /// - **Case-insensitive** (C uses `epicsStrnCaseCmp`).
    /// - **Whitespace** between tokens / separators tolerated.
    ///
    /// Empty input returns the empty mask. Unknown tokens are
    /// reported as `Err` naming the offending text — callers
    /// (iocsh `asynSetTraceMask`) can choose to fail or silently
    /// drop. C just `printf`s an error and returns whatever it
    /// accumulated so far; we surface explicitly.
    pub fn from_symbolic(s: &str) -> Result<TraceMask, String> {
        let mut mask = TraceMask::empty();
        for raw in split_mask_tokens(s) {
            let tok = raw.trim();
            if tok.is_empty() {
                continue;
            }
            if let Some(n) = parse_numeric(tok) {
                mask |= TraceMask::from_bits_truncate(n);
                continue;
            }
            let normalized = strip_c_prefixes(tok, &["TRACE_", "TRACEIO_"]);
            let bit = match normalized.as_str() {
                "ERROR" => TraceMask::ERROR,
                "DEVICE" => TraceMask::IO_DEVICE,
                "FILTER" => TraceMask::IO_FILTER,
                "DRIVER" => TraceMask::IO_DRIVER,
                "FLOW" => TraceMask::FLOW,
                "WARNING" => TraceMask::WARNING,
                _ => {
                    return Err(format!("unknown trace mask token: '{tok}'"));
                }
            };
            mask |= bit;
        }
        Ok(mask)
    }
}

impl TraceIoMask {
    /// Parse a symbolic IO-mask string the way C asyn does
    /// (asynShellCommands.c:756-783 `asynTraceIOMaskStringToInt`):
    ///
    /// Tokens: `NODATA` (= 0x0, suppress payload), `ASCII`,
    /// `ESCAPE`, `HEX`. Accepts `ASYN_` / `TRACEIO_` prefixes.
    /// Separators `|` or `+`. Numeric strtol-style.
    pub fn from_symbolic(s: &str) -> Result<TraceIoMask, String> {
        let mut mask = TraceIoMask::empty();
        for raw in split_mask_tokens(s) {
            let tok = raw.trim();
            if tok.is_empty() {
                continue;
            }
            if let Some(n) = parse_numeric(tok) {
                mask |= TraceIoMask::from_bits_truncate(n);
                continue;
            }
            let normalized = strip_c_prefixes(tok, &["TRACEIO_"]);
            let bit = match normalized.as_str() {
                // C asyn `ASYN_TRACEIO_NODATA = 0x0000` — accepted
                // as a token name but contributes no bits. Caller
                // setting only NODATA effectively clears the mask,
                // which matches the C semantic of "show no payload".
                "NODATA" => TraceIoMask::empty(),
                "ASCII" => TraceIoMask::ASCII,
                "ESCAPE" => TraceIoMask::ESCAPE,
                "HEX" => TraceIoMask::HEX,
                _ => return Err(format!("unknown trace I/O mask token: '{tok}'")),
            };
            mask |= bit;
        }
        Ok(mask)
    }
}

impl TraceInfoMask {
    /// Parse a symbolic info-mask string the way C asyn does
    /// (asynShellCommands.c:822-849 `asynTraceInfoMaskStringToInt`):
    ///
    /// Tokens: `TIME`, `PORT`, `SOURCE`, `THREAD`. Accepts
    /// `ASYN_` / `TRACEINFO_` prefixes. Separators `|` or `+`.
    pub fn from_symbolic(s: &str) -> Result<TraceInfoMask, String> {
        let mut mask = TraceInfoMask::empty();
        for raw in split_mask_tokens(s) {
            let tok = raw.trim();
            if tok.is_empty() {
                continue;
            }
            if let Some(n) = parse_numeric(tok) {
                mask |= TraceInfoMask::from_bits_truncate(n);
                continue;
            }
            let normalized = strip_c_prefixes(tok, &["TRACEINFO_"]);
            let bit = match normalized.as_str() {
                "TIME" => TraceInfoMask::TIME,
                "PORT" => TraceInfoMask::PORT,
                "SOURCE" => TraceInfoMask::SOURCE,
                "THREAD" => TraceInfoMask::THREAD,
                _ => return Err(format!("unknown trace info mask token: '{tok}'")),
            };
            mask |= bit;
        }
        Ok(mask)
    }
}

/// Split a mask string on `|` or `+` (C asyn: see the do/while
/// `*maskStr == '|' || == '+'` at asynShellCommands.c:693).
fn split_mask_tokens(s: &str) -> impl Iterator<Item = &str> {
    s.split(['|', '+'])
}

/// Strip the `ASYN_` prefix (always tried) plus any of the
/// `category_prefixes` (e.g. `TRACE_`, `TRACEIO_`, `TRACEINFO_`),
/// uppercase the result. Mirrors the `STARTSWITH(maskStr, ASYN_) +
/// STARTSWITH(maskStr, TRACE_) || STARTSWITH(maskStr, TRACEIO_)`
/// pattern that asynShellCommands.c uses to fold long forms
/// (`ASYN_TRACEIO_DEVICE`) into short ones (`DEVICE`).
fn strip_c_prefixes(tok: &str, category_prefixes: &[&str]) -> String {
    let upper = tok.to_ascii_uppercase();
    let stripped = upper.strip_prefix("ASYN_").unwrap_or(&upper);
    for p in category_prefixes {
        if let Some(rest) = stripped.strip_prefix(p) {
            return rest.to_string();
        }
    }
    stripped.to_string()
}

fn parse_numeric(tok: &str) -> Option<u32> {
    if let Some(rest) = tok.strip_prefix("0x").or_else(|| tok.strip_prefix("0X")) {
        u32::from_str_radix(rest, 16).ok()
    } else if let Some(rest) = tok.strip_prefix("0o").or_else(|| tok.strip_prefix("0O")) {
        u32::from_str_radix(rest, 8).ok()
    } else if let Some(rest) = tok.strip_prefix('0').filter(|s| !s.is_empty()) {
        // C `strtol(., ., 0)` treats `"0..."` as octal. Match that
        // for parity with C asyn's symbolic-or-numeric token
        // handling. Plain "0" (no following digits) parses as 0
        // via the strip-fail branch below.
        u32::from_str_radix(rest, 8)
            .ok()
            .or_else(|| tok.parse::<u32>().ok())
    } else {
        tok.parse::<u32>().ok()
    }
}

bitflags! {
    /// How to format I/O data.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct TraceIoMask: u32 {
        const ASCII  = 0x0001;
        const ESCAPE = 0x0002;
        const HEX    = 0x0004;
    }
}

bitflags! {
    /// What metadata to include in trace prefix.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct TraceInfoMask: u32 {
        const TIME   = 0x0001;
        const PORT   = 0x0002;
        const SOURCE = 0x0004;
        const THREAD = 0x0008;
    }
}

/// Output destination for trace messages.
pub enum TraceFile {
    Stderr,
    Stdout,
    File(Arc<Mutex<std::fs::File>>),
}

impl TraceFile {
    /// Write a complete line atomically (single write_all call under lock).
    pub fn write_line(&self, line: &str) {
        match self {
            TraceFile::Stderr => {
                let _ = std::io::stderr().write_all(line.as_bytes());
            }
            TraceFile::Stdout => {
                let _ = std::io::stdout().write_all(line.as_bytes());
            }
            TraceFile::File(f) => {
                if let Ok(mut f) = f.lock() {
                    let _ = f.write_all(line.as_bytes());
                }
            }
        }
    }
}

impl Default for TraceFile {
    fn default() -> Self {
        TraceFile::Stderr
    }
}

/// Per-port (or global) trace configuration.
pub struct TraceConfig {
    pub trace_mask: TraceMask,
    pub trace_io_mask: TraceIoMask,
    pub trace_info_mask: TraceInfoMask,
    pub io_truncate_size: usize,
    pub file: TraceFile,
}

impl Default for TraceConfig {
    fn default() -> Self {
        Self {
            trace_mask: TraceMask::ERROR | TraceMask::WARNING,
            trace_io_mask: TraceIoMask::ASCII,
            trace_info_mask: TraceInfoMask::TIME | TraceInfoMask::PORT,
            io_truncate_size: 80,
            file: TraceFile::default(),
        }
    }
}

/// Global trace manager with per-port and per-device override support.
/// C parity: 3-level hierarchy: device → port → global.
pub struct TraceManager {
    global_config: Mutex<TraceConfig>,
    port_configs: Mutex<HashMap<String, TraceConfig>>,
    /// Per-device overrides keyed by (portName, addr).
    device_configs: Mutex<HashMap<(String, i32), TraceConfig>>,
}

impl TraceManager {
    pub fn new() -> Self {
        Self {
            global_config: Mutex::new(TraceConfig::default()),
            port_configs: Mutex::new(HashMap::new()),
            device_configs: Mutex::new(HashMap::new()),
        }
    }

    /// Check if a trace level is enabled for a port (optionally device).
    ///
    /// `mask` should be a single trace level (e.g. `TraceMask::ERROR`), not a
    /// combination. In debug builds, passing a multi-bit mask triggers a
    /// `debug_assert` failure.
    pub fn is_enabled(&self, port: &str, mask: TraceMask) -> bool {
        debug_assert!(
            mask.bits().is_power_of_two(),
            "is_enabled expects a single trace level, got {:?}",
            mask
        );
        if let Ok(configs) = self.port_configs.lock() {
            if let Some(cfg) = configs.get(port) {
                return cfg.trace_mask.intersects(mask);
            }
        }
        if let Ok(cfg) = self.global_config.lock() {
            return cfg.trace_mask.intersects(mask);
        }
        false
    }

    /// Check if a trace level is enabled for a specific device address.
    /// Hierarchy: device → port → global (C parity).
    pub fn is_enabled_device(&self, port: &str, addr: i32, mask: TraceMask) -> bool {
        if let Ok(configs) = self.device_configs.lock() {
            if let Some(cfg) = configs.get(&(port.to_string(), addr)) {
                return cfg.trace_mask.intersects(mask);
            }
        }
        self.is_enabled(port, mask)
    }

    /// Set trace configuration for a specific device address.
    pub fn set_device_trace_mask(&self, port: &str, addr: i32, mask: TraceMask) {
        if let Ok(mut configs) = self.device_configs.lock() {
            configs
                .entry((port.to_string(), addr))
                .or_insert_with(TraceConfig::default)
                .trace_mask = mask;
        }
    }

    /// Output a trace message.
    pub fn output(&self, port: &str, mask: TraceMask, msg: &str) {
        let configs = self.port_configs.lock().ok();
        let port_cfg = configs.as_ref().and_then(|c| c.get(port));

        let global_cfg = self.global_config.lock().ok();
        let cfg = port_cfg.or(global_cfg.as_deref());

        if let Some(cfg) = cfg {
            let prefix = format_prefix(port, mask, cfg);
            let line = format!("{prefix}{msg}\n");
            cfg.file.write_line(&line);
        }
    }

    /// Output a trace message with source file/line info (C parity: __FILE__/__LINE__).
    pub fn output_with_source(
        &self,
        port: &str,
        mask: TraceMask,
        file: &str,
        line: u32,
        msg: &str,
    ) {
        let configs = self.port_configs.lock().ok();
        let port_cfg = configs.as_ref().and_then(|c| c.get(port));

        let global_cfg = self.global_config.lock().ok();
        let cfg = port_cfg.or(global_cfg.as_deref());

        if let Some(cfg) = cfg {
            let prefix = format_prefix(port, mask, cfg);
            let source = if cfg.trace_info_mask.contains(TraceInfoMask::SOURCE) {
                format!("[{file}:{line}] ")
            } else {
                String::new()
            };
            let out = format!("{prefix}{source}{msg}\n");
            cfg.file.write_line(&out);
        }
    }

    /// Output I/O data with formatting according to TraceIoMask.
    pub fn output_io(&self, port: &str, mask: TraceMask, data: &[u8], label: &str) {
        let configs = self.port_configs.lock().ok();
        let port_cfg = configs.as_ref().and_then(|c| c.get(port));

        let global_cfg = self.global_config.lock().ok();
        let cfg = port_cfg.or(global_cfg.as_deref());

        if let Some(cfg) = cfg {
            let prefix = format_prefix(port, mask, cfg);
            let truncate = if cfg.io_truncate_size > 0 {
                cfg.io_truncate_size
            } else {
                usize::MAX
            };
            let data = if data.len() > truncate {
                &data[..truncate]
            } else {
                data
            };

            let formatted = format_io_data(data, cfg.trace_io_mask);
            let line = format!("{prefix}{label} {formatted}\n");
            cfg.file.write_line(&line);
        }
    }

    // --- Configuration mutators ---

    pub fn set_trace_mask(&self, port: Option<&str>, mask: TraceMask) {
        match port {
            Some(name) => {
                if let Ok(mut configs) = self.port_configs.lock() {
                    configs
                        .entry(name.to_string())
                        .or_insert_with(TraceConfig::default)
                        .trace_mask = mask;
                }
            }
            None => {
                if let Ok(mut cfg) = self.global_config.lock() {
                    cfg.trace_mask = mask;
                }
            }
        }
    }

    pub fn set_trace_io_mask(&self, port: Option<&str>, mask: TraceIoMask) {
        match port {
            Some(name) => {
                if let Ok(mut configs) = self.port_configs.lock() {
                    configs
                        .entry(name.to_string())
                        .or_insert_with(TraceConfig::default)
                        .trace_io_mask = mask;
                }
            }
            None => {
                if let Ok(mut cfg) = self.global_config.lock() {
                    cfg.trace_io_mask = mask;
                }
            }
        }
    }

    pub fn set_trace_info_mask(&self, port: Option<&str>, mask: TraceInfoMask) {
        match port {
            Some(name) => {
                if let Ok(mut configs) = self.port_configs.lock() {
                    configs
                        .entry(name.to_string())
                        .or_insert_with(TraceConfig::default)
                        .trace_info_mask = mask;
                }
            }
            None => {
                if let Ok(mut cfg) = self.global_config.lock() {
                    cfg.trace_info_mask = mask;
                }
            }
        }
    }

    pub fn set_trace_file(&self, port: Option<&str>, file: TraceFile) {
        match port {
            Some(name) => {
                if let Ok(mut configs) = self.port_configs.lock() {
                    configs
                        .entry(name.to_string())
                        .or_insert_with(TraceConfig::default)
                        .file = file;
                }
            }
            None => {
                if let Ok(mut cfg) = self.global_config.lock() {
                    cfg.file = file;
                }
            }
        }
    }

    pub fn set_io_truncate_size(&self, port: Option<&str>, size: usize) {
        match port {
            Some(name) => {
                if let Ok(mut configs) = self.port_configs.lock() {
                    configs
                        .entry(name.to_string())
                        .or_insert_with(TraceConfig::default)
                        .io_truncate_size = size;
                }
            }
            None => {
                if let Ok(mut cfg) = self.global_config.lock() {
                    cfg.io_truncate_size = size;
                }
            }
        }
    }

    pub fn get_trace_mask(&self, port: Option<&str>) -> TraceMask {
        if let Some(name) = port {
            if let Ok(configs) = self.port_configs.lock() {
                if let Some(cfg) = configs.get(name) {
                    return cfg.trace_mask;
                }
            }
        }
        self.global_config
            .lock()
            .map(|c| c.trace_mask)
            .unwrap_or(TraceMask::ERROR | TraceMask::WARNING)
    }

    pub fn get_trace_io_mask(&self, port: Option<&str>) -> TraceIoMask {
        if let Some(name) = port {
            if let Ok(configs) = self.port_configs.lock() {
                if let Some(cfg) = configs.get(name) {
                    return cfg.trace_io_mask;
                }
            }
        }
        self.global_config
            .lock()
            .map(|c| c.trace_io_mask)
            .unwrap_or(TraceIoMask::ASCII)
    }
}

impl Default for TraceManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Format the prefix line (timestamp, port name, etc).
fn format_prefix(port: &str, mask: TraceMask, cfg: &TraceConfig) -> String {
    let mut parts = Vec::new();

    if cfg.trace_info_mask.contains(TraceInfoMask::TIME) {
        use std::time::SystemTime;
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        let secs = now.as_secs();
        let millis = now.subsec_millis();
        parts.push(format!("{secs}.{millis:03}"));
    }

    if cfg.trace_info_mask.contains(TraceInfoMask::PORT) {
        parts.push(port.to_string());
    }

    if cfg.trace_info_mask.contains(TraceInfoMask::THREAD) {
        if let Some(name) = std::thread::current().name() {
            parts.push(name.to_string());
        } else {
            parts.push(format!("{:?}", std::thread::current().id()));
        }
    }

    let mask_name = mask_label(mask);
    parts.push(mask_name.to_string());

    parts.join(" ") + " "
}

fn mask_label(mask: TraceMask) -> &'static str {
    if mask.contains(TraceMask::ERROR) {
        "ERROR"
    } else if mask.contains(TraceMask::WARNING) {
        "WARNING"
    } else if mask.contains(TraceMask::FLOW) {
        "FLOW"
    } else if mask.contains(TraceMask::IO_DEVICE) {
        "IO_DEVICE"
    } else if mask.contains(TraceMask::IO_DRIVER) {
        "IO_DRIVER"
    } else if mask.contains(TraceMask::IO_FILTER) {
        "IO_FILTER"
    } else {
        "TRACE"
    }
}

/// Format I/O data according to the trace I/O mask.
pub fn format_io_data(data: &[u8], mask: TraceIoMask) -> String {
    if mask.contains(TraceIoMask::HEX) {
        format_hex(data)
    } else if mask.contains(TraceIoMask::ESCAPE) {
        format_escape(data)
    } else {
        // ASCII (default)
        format_ascii(data)
    }
}

fn format_ascii(data: &[u8]) -> String {
    data.iter()
        .map(|&b| {
            if b >= 0x20 && b < 0x7f {
                b as char
            } else {
                '.'
            }
        })
        .collect()
}

fn format_escape(data: &[u8]) -> String {
    let mut s = String::with_capacity(data.len() * 2);
    for &b in data {
        match b {
            b'\r' => s.push_str("\\r"),
            b'\n' => s.push_str("\\n"),
            b'\t' => s.push_str("\\t"),
            b'\\' => s.push_str("\\\\"),
            0x20..=0x7e => s.push(b as char),
            _ => {
                s.push_str(&format!("\\x{b:02x}"));
            }
        }
    }
    s
}

fn format_hex(data: &[u8]) -> String {
    data.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Log a trace message (checks `is_enabled` first for short-circuit).
///
/// Accepts either `&TraceManager` or `Option<&TraceManager>` as the first argument.
/// When given `Option`, `None` is a silent no-op.
#[macro_export]
macro_rules! asyn_trace {
    (Some($mgr:expr), $port:expr, $mask:expr, $($arg:tt)*) => {
        if let Some(ref __mgr) = $mgr {
            let __mgr: &$crate::trace::TraceManager = __mgr;
            if __mgr.is_enabled($port, $mask) {
                __mgr.output_with_source($port, $mask, file!(), line!(), &format!($($arg)*));
            }
        }
    };
    ($mgr:expr, $port:expr, $mask:expr, $($arg:tt)*) => {
        if $mgr.is_enabled($port, $mask) {
            $mgr.output_with_source($port, $mask, file!(), line!(), &format!($($arg)*));
        }
    };
}

/// Log I/O data with formatting.
///
/// Accepts either `&TraceManager` or `Option<&TraceManager>` as the first argument.
/// When given `Option`, `None` is a silent no-op.
#[macro_export]
macro_rules! asyn_trace_io {
    (Some($mgr:expr), $port:expr, $mask:expr, $data:expr, $($arg:tt)*) => {
        if let Some(ref __mgr) = $mgr {
            let __mgr: &$crate::trace::TraceManager = __mgr;
            if __mgr.is_enabled($port, $mask) {
                __mgr.output_io($port, $mask, $data, &format!($($arg)*));
            }
        }
    };
    ($mgr:expr, $port:expr, $mask:expr, $data:expr, $($arg:tt)*) => {
        if $mgr.is_enabled($port, $mask) {
            $mgr.output_io($port, $mask, $data, &format!($($arg)*));
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_mask_error_warning() {
        let mgr = TraceManager::new();
        assert!(mgr.is_enabled("port1", TraceMask::ERROR));
        assert!(mgr.is_enabled("port1", TraceMask::WARNING));
        assert!(!mgr.is_enabled("port1", TraceMask::FLOW));
        assert!(!mgr.is_enabled("port1", TraceMask::IO_DRIVER));
    }

    /// C asyn defines 6 trace bits in `asynDriver.h:211-216`. This
    /// test fences that we don't accidentally re-introduce extra
    /// bits — `grep -rn "ASYN_TRACE_STATE" ~/codes/epics-modules/asyn`
    /// returns 0 hits, so any additional bit would be invented.
    #[test]
    fn test_six_bits_match_c_asyn_header() {
        assert_eq!(TraceMask::ERROR.bits(), 0x0001);
        assert_eq!(TraceMask::IO_DEVICE.bits(), 0x0002);
        assert_eq!(TraceMask::IO_FILTER.bits(), 0x0004);
        assert_eq!(TraceMask::IO_DRIVER.bits(), 0x0008);
        assert_eq!(TraceMask::FLOW.bits(), 0x0010);
        assert_eq!(TraceMask::WARNING.bits(), 0x0020);
        // Union of all 6 = 0x3F.
        let all = TraceMask::ERROR
            | TraceMask::IO_DEVICE
            | TraceMask::IO_FILTER
            | TraceMask::IO_DRIVER
            | TraceMask::FLOW
            | TraceMask::WARNING;
        assert_eq!(all.bits(), 0x003F);
    }

    /// C asyn `asynShellCommands.c:670-699`: short token names
    /// `ERROR/DEVICE/FILTER/DRIVER/FLOW/WARNING` (no `IO_` prefix).
    /// C accepts the long forms via `STARTSWITH(maskStr, ASYN_)` +
    /// `STARTSWITH(maskStr, TRACE_)/(TRACEIO_)` prefix stripping.
    #[test]
    fn test_trace_mask_from_symbolic_basic() {
        let m = TraceMask::from_symbolic("ERROR|FLOW|DEVICE").unwrap();
        assert_eq!(m, TraceMask::ERROR | TraceMask::FLOW | TraceMask::IO_DEVICE);
    }

    /// C parity: long tokens `ASYN_TRACEIO_DRIVER`, `ASYN_TRACE_FLOW`
    /// fold to short names via prefix-strip; `+` separator works as
    /// well as `|` (asynShellCommands.c:693).
    #[test]
    fn test_trace_mask_from_symbolic_long_form_and_plus_separator() {
        let m = TraceMask::from_symbolic("ASYN_TRACEIO_DRIVER+ASYN_TRACE_FLOW").unwrap();
        assert_eq!(m, TraceMask::IO_DRIVER | TraceMask::FLOW);
    }

    #[test]
    fn test_trace_mask_from_symbolic_case_insensitive_and_aliases() {
        let m = TraceMask::from_symbolic("error|asyn_traceio_driver|Warning").unwrap();
        assert_eq!(
            m,
            TraceMask::ERROR | TraceMask::IO_DRIVER | TraceMask::WARNING
        );
    }

    #[test]
    fn test_trace_mask_from_symbolic_numeric_mix() {
        // 0x10 = FLOW (C asyn `ASYN_TRACE_FLOW`).
        let m = TraceMask::from_symbolic("ERROR|0x10|0o20").unwrap();
        assert_eq!(m, TraceMask::ERROR | TraceMask::FLOW);
    }

    #[test]
    fn test_trace_mask_from_symbolic_unknown_token_errors() {
        let err = TraceMask::from_symbolic("ERROR|NOPE").unwrap_err();
        assert!(err.contains("NOPE"), "error must name the bad token: {err}");
    }

    #[test]
    fn test_trace_mask_from_symbolic_empty_and_whitespace() {
        assert_eq!(TraceMask::from_symbolic("").unwrap(), TraceMask::empty());
        assert_eq!(TraceMask::from_symbolic("  ").unwrap(), TraceMask::empty());
        assert_eq!(
            TraceMask::from_symbolic(" ERROR | | FLOW ").unwrap(),
            TraceMask::ERROR | TraceMask::FLOW
        );
    }

    #[test]
    fn test_trace_io_mask_and_info_mask_symbolic() {
        // `+` separator + ASYN_-prefixed long form, per C asyn usage
        // strings (asynShellCommands.c:723 example).
        let io = TraceIoMask::from_symbolic("ESCAPE+HEX").unwrap();
        assert_eq!(io, TraceIoMask::ESCAPE | TraceIoMask::HEX);
        let io2 = TraceIoMask::from_symbolic("ASYN_TRACEIO_ASCII").unwrap();
        assert_eq!(io2, TraceIoMask::ASCII);
        let info = TraceInfoMask::from_symbolic("TIME|THREAD").unwrap();
        assert_eq!(info, TraceInfoMask::TIME | TraceInfoMask::THREAD);
    }

    /// C asyn `ASYN_TRACEIO_NODATA = 0x0000` (asynDriver.h:219) is
    /// a valid token that means "no payload". Setting only NODATA
    /// gives an empty mask (asynShellCommands.c:770).
    #[test]
    fn test_trace_io_nodata_token() {
        assert_eq!(
            TraceIoMask::from_symbolic("NODATA").unwrap(),
            TraceIoMask::empty()
        );
        // NODATA OR'd with other bits is a no-op.
        assert_eq!(
            TraceIoMask::from_symbolic("NODATA+HEX").unwrap(),
            TraceIoMask::HEX
        );
    }

    #[test]
    fn test_set_global_mask() {
        let mgr = TraceManager::new();
        mgr.set_trace_mask(None, TraceMask::ERROR | TraceMask::FLOW);
        assert!(mgr.is_enabled("any", TraceMask::ERROR));
        assert!(mgr.is_enabled("any", TraceMask::FLOW));
        assert!(!mgr.is_enabled("any", TraceMask::WARNING));
    }

    #[test]
    fn test_port_override_vs_global() {
        let mgr = TraceManager::new();
        mgr.set_trace_mask(None, TraceMask::ERROR);
        mgr.set_trace_mask(Some("myport"), TraceMask::FLOW);

        // myport uses its override
        assert!(mgr.is_enabled("myport", TraceMask::FLOW));
        assert!(!mgr.is_enabled("myport", TraceMask::ERROR));

        // other ports use global
        assert!(mgr.is_enabled("other", TraceMask::ERROR));
        assert!(!mgr.is_enabled("other", TraceMask::FLOW));
    }

    #[test]
    fn test_format_ascii() {
        assert_eq!(format_ascii(b"hello"), "hello");
        assert_eq!(format_ascii(b"hi\r\n"), "hi..");
        assert_eq!(format_ascii(&[0x00, 0x7f, 0x41]), "..A");
    }

    #[test]
    fn test_format_escape() {
        assert_eq!(format_escape(b"OK\r\n"), "OK\\r\\n");
        assert_eq!(format_escape(b"\t\\"), "\\t\\\\");
        assert_eq!(format_escape(&[0x01]), "\\x01");
        assert_eq!(format_escape(b"hi"), "hi");
    }

    #[test]
    fn test_format_hex() {
        assert_eq!(format_hex(b"AB"), "41 42");
        assert_eq!(format_hex(b"\r\n"), "0d 0a");
        assert_eq!(format_hex(b""), "");
    }

    #[test]
    fn test_io_truncate() {
        let data = b"hello world";
        let truncated = &data[..4];
        assert_eq!(format_ascii(truncated), "hell");
    }

    #[test]
    fn test_format_io_data_dispatch() {
        let data = b"OK\r\n";
        assert_eq!(format_io_data(data, TraceIoMask::ASCII), "OK..");
        assert_eq!(format_io_data(data, TraceIoMask::ESCAPE), "OK\\r\\n");
        assert_eq!(format_io_data(data, TraceIoMask::HEX), "4f 4b 0d 0a");
    }

    #[test]
    fn test_output_to_buffer() {
        let mgr = TraceManager::new();
        mgr.set_trace_mask(None, TraceMask::ERROR | TraceMask::IO_DRIVER);
        mgr.set_trace_info_mask(None, TraceInfoMask::PORT); // only port name for predictability

        // Create a shared buffer as a file
        let temp = std::env::temp_dir().join("asyn_trace_test.txt");
        let file = std::fs::File::create(&temp).unwrap();
        mgr.set_trace_file(None, TraceFile::File(Arc::new(Mutex::new(file))));

        mgr.output("testport", TraceMask::ERROR, "something broke");

        // Read back
        let contents = std::fs::read_to_string(&temp).unwrap();
        assert!(contents.contains("testport"));
        assert!(contents.contains("ERROR"));
        assert!(contents.contains("something broke"));
        let _ = std::fs::remove_file(&temp);
    }

    #[test]
    fn test_output_io_to_buffer() {
        let mgr = TraceManager::new();
        mgr.set_trace_mask(None, TraceMask::IO_DRIVER);
        mgr.set_trace_info_mask(None, TraceInfoMask::PORT);
        mgr.set_trace_io_mask(None, TraceIoMask::ESCAPE);

        let temp = std::env::temp_dir().join("asyn_trace_io_test.txt");
        let file = std::fs::File::create(&temp).unwrap();
        mgr.set_trace_file(None, TraceFile::File(Arc::new(Mutex::new(file))));

        mgr.output_io("testport", TraceMask::IO_DRIVER, b"OK\r\n", "read:");

        let contents = std::fs::read_to_string(&temp).unwrap();
        assert!(contents.contains("testport"));
        assert!(contents.contains("IO_DRIVER"));
        assert!(contents.contains("read:"));
        assert!(contents.contains("OK\\r\\n"));
        let _ = std::fs::remove_file(&temp);
    }

    #[test]
    fn test_get_masks() {
        let mgr = TraceManager::new();
        assert_eq!(
            mgr.get_trace_mask(None),
            TraceMask::ERROR | TraceMask::WARNING
        );
        assert_eq!(mgr.get_trace_io_mask(None), TraceIoMask::ASCII);

        mgr.set_trace_mask(Some("p1"), TraceMask::FLOW);
        assert_eq!(mgr.get_trace_mask(Some("p1")), TraceMask::FLOW);
        // Global unaffected
        assert_eq!(
            mgr.get_trace_mask(None),
            TraceMask::ERROR | TraceMask::WARNING
        );
    }

    #[test]
    fn test_macro_short_circuit() {
        let mgr = TraceManager::new();
        // FLOW is not enabled by default
        // This should not panic or produce output
        asyn_trace!(mgr, "port", TraceMask::FLOW, "should not appear");
    }

    #[test]
    fn test_io_truncate_integration() {
        let mgr = TraceManager::new();
        mgr.set_trace_mask(None, TraceMask::IO_DRIVER);
        mgr.set_trace_info_mask(None, TraceInfoMask::PORT);
        mgr.set_io_truncate_size(None, 3);

        let temp = std::env::temp_dir().join("asyn_trace_trunc_test.txt");
        let file = std::fs::File::create(&temp).unwrap();
        mgr.set_trace_file(None, TraceFile::File(Arc::new(Mutex::new(file))));

        mgr.output_io("p", TraceMask::IO_DRIVER, b"hello world", "write:");

        let contents = std::fs::read_to_string(&temp).unwrap();
        // ASCII format, truncated to 3 bytes: "hel"
        assert!(contents.contains("hel"));
        assert!(!contents.contains("hello"));
        let _ = std::fs::remove_file(&temp);
    }

    #[test]
    fn test_write_line_single_call() {
        // Verify that File variant does a single write_all
        let temp = std::env::temp_dir().join("asyn_trace_single_write.txt");
        let file = std::fs::File::create(&temp).unwrap();
        let tf = TraceFile::File(Arc::new(Mutex::new(file)));

        tf.write_line("line one\n");
        tf.write_line("line two\n");

        let contents = std::fs::read_to_string(&temp).unwrap();
        assert_eq!(contents, "line one\nline two\n");
        let _ = std::fs::remove_file(&temp);
    }
}
