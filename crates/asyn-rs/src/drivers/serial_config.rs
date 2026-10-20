//! Platform-independent serial line configuration.
//!
//! Shared by the POSIX termios backend (`serial_port.rs`, unix) and the
//! Win32 DCB backend (`serial_port_win32.rs`, Windows). Named in prose rather
//! than linked: neither module exists on RTEMS or VxWorks (see
//! `drivers/mod.rs`), where an intra-doc link to it would dangle. This module
//! itself is platform-independent and builds everywhere. C asyn keeps the two
//! device backends
//! (`drvAsynSerialPort.c` / `drvAsynSerialPortWin32.c`) as separate files that
//! share only the spec grammar and option names; this module is the single
//! source of that shared grammar so the two Rust backends cannot drift.

// --- Configuration types ---

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataBits {
    Five,
    Six,
    Seven,
    Eight,
}

impl Default for DataBits {
    fn default() -> Self {
        DataBits::Eight
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parity {
    None,
    Odd,
    Even,
}

impl Default for Parity {
    fn default() -> Self {
        Parity::None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopBits {
    One,
    Two,
}

impl Default for StopBits {
    fn default() -> Self {
        StopBits::One
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowControl {
    None,
    Hardware,
    Software,
}

impl Default for FlowControl {
    fn default() -> Self {
        FlowControl::None
    }
}

#[derive(Debug, Clone)]
pub struct SerialConfig {
    pub device: String,
    pub baud: u32,
    pub data_bits: DataBits,
    pub parity: Parity,
    pub stop_bits: StopBits,
    pub flow_control: FlowControl,
}

impl SerialConfig {
    /// The line a configured port starts with: 9600 8N1, no flow control, on
    /// `device` exactly as given (`epicsStrDup(ttyName)`,
    /// drvAsynSerialPort.c:1069). An empty or padded name is not refused here;
    /// it fails at `open`, as in C.
    pub fn new(device: &str) -> Self {
        Self {
            device: device.to_owned(),
            baud: 9600,
            data_bits: DataBits::default(),
            parity: Parity::default(),
            stop_bits: StopBits::default(),
            flow_control: FlowControl::default(),
        }
    }
}

/// The name the Win32 port keeps for `ttyName` and names in every message:
/// `\\.\` prepended unless the name already starts with it
/// (drvAsynSerialPortWin32.c:753-762).
#[cfg(any(windows, test))]
pub(crate) fn win32_device_name(tty: &str) -> String {
    if tty.starts_with(r"\\.\") {
        tty.to_owned()
    } else {
        format!(r"\\.\{tty}")
    }
}

// The boolean option value grammar (C's `epicsStrCaseCmp(val,"Y"/"N")`) and its
// per-key diagnostic live with the rest of the C option grammar, in
// [`super::option_parse::parse_yn_option`] — every backend, serial and IP, takes
// it from there.

#[cfg(test)]
mod tests {
    use super::win32_device_name;

    /// drvAsynSerialPortWin32.c:753-762: `epicsStrnCaseCmp(ttyName, "\\\\.\\", 4)`.
    #[test]
    fn the_win32_name_gains_the_device_prefix_once() {
        for (tty, kept) in [
            ("COM3", r"\\.\COM3"),
            (r"\\.\COM10", r"\\.\COM10"),
            ("", r"\\.\"),
            (r"\\.", r"\\.\\\."),
        ] {
            assert_eq!(win32_device_name(tty), kept, "{tty:?}");
        }
    }
}
