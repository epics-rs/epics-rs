use serde::{Deserialize, Serialize};

/// Protocol-level command enum. 1:1 map from `RequestOp`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PortCommand {
    Int32Read,
    Int32Write {
        value: i32,
    },
    Int64Read,
    Int64Write {
        value: i64,
    },
    Float64Read,
    Float64Write {
        value: f64,
    },
    OctetRead {
        buf_size: usize,
    },
    OctetWrite {
        data: Vec<u8>,
    },
    OctetWriteRead {
        data: Vec<u8>,
        buf_size: usize,
        /// Flush the input buffer before the write (asynOctetSyncIO::writeRead)
        /// vs raw write-then-read (devAsynOctet command-response). See
        /// [`crate::request::RequestOp::OctetWriteRead`].
        flush: bool,
    },
    /// Binary octet write with the driver's output EOS suppressed
    /// (asynRecord binary output, asynRecord.c:1528-1541).
    OctetWriteBinary {
        data: Vec<u8>,
    },
    /// Binary octet read with the driver's input EOS suppressed
    /// (asynRecord binary input, asynRecord.c:1564-1577).
    OctetReadBinary {
        buf_size: usize,
    },
    UInt32DigitalRead {
        mask: u32,
    },
    UInt32DigitalWrite {
        value: u32,
        mask: u32,
    },
    EnumRead,
    EnumWrite {
        index: usize,
    },
    Int32ArrayRead {
        max_elements: usize,
    },
    Int32ArrayWrite {
        data: Vec<i32>,
    },
    Float64ArrayRead {
        max_elements: usize,
    },
    Float64ArrayWrite {
        data: Vec<f64>,
    },
    Int8ArrayRead {
        max_elements: usize,
    },
    Int8ArrayWrite {
        data: Vec<i8>,
    },
    Int16ArrayRead {
        max_elements: usize,
    },
    Int16ArrayWrite {
        data: Vec<i16>,
    },
    Int64ArrayRead {
        max_elements: usize,
    },
    Int64ArrayWrite {
        data: Vec<i64>,
    },
    Float32ArrayRead {
        max_elements: usize,
    },
    Float32ArrayWrite {
        data: Vec<f32>,
    },
    Flush,
    /// C `asynCallbackProcess` with `tmod == asynTMOD_NoIO` — a queued
    /// request that performs no transfer. See [`crate::request::RequestOp::NoIo`].
    NoIo,
    Connect,
    Disconnect,
    /// `ASYN_DESTRUCTIBLE` port shutdown — C asynManager.c:2251.
    ShutdownPort,
    ConnectAddr,
    DisconnectAddr,
    /// Enable / disable (C parity: `pasynManager->enable(pasynUser, value)`).
    /// Port-wide or one device: `findDpCommon` picks from the request's addr
    /// (asynManager.c:538-545).
    SetEnable {
        yes: bool,
    },
    /// Auto-connect toggle (C parity:
    /// `pasynManager->autoConnect(pasynUser, value)`), same addr resolution.
    SetAutoConnect {
        yes: bool,
    },
    GetBoundsInt32,
    GetBoundsInt64,
    /// Query whether the port is currently enabled.
    GetEnable,
    /// Query whether auto-connect is enabled for the port.
    GetAutoConnect,
    /// C's `allDevices` argument to `blockProcessCallback`
    /// (asynManager.c:1692): `true` takes the port-wide holder, `false` the
    /// holder of the `dpCommon` the request's own `addr` resolves to. The
    /// variant was a unit before, which is the wire shape of a port-wide
    /// block only — an externally-tagged unit variant and a struct variant do
    /// not interconvert, so this is a wire change, not an additive one.
    BlockProcess {
        all_devices: bool,
    },
    UnblockProcess {
        all_devices: bool,
    },
    DrvUserCreate {
        drv_info: String,
        /// The record's asyn `addr` (C `drvUserCreate` `checkOffset` input).
        addr: i32,
        /// The bound record's asyn interface name (`"asynFloat64"`, …), so a
        /// remote on-demand driver can create the parameter with the type the
        /// record will read it as. `None` for a port-level resolve with no
        /// record behind it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        iface: Option<String>,
    },
    CallParamCallbacks {
        addr: i32,
    },
    GetOption {
        key: String,
    },
    SetOption {
        key: String,
        value: String,
    },
    /// Driver `report(level)` invocation — iocsh `asynReport`.
    Report {
        level: i32,
    },
    /// Set input EOS bytes — C `pasynOctet->setInputEos` /
    /// asynRecord IEOS.
    SetInputEos {
        eos: Vec<u8>,
    },
    /// Set output EOS bytes — C `pasynOctet->setOutputEos` /
    /// asynRecord OEOS.
    SetOutputEos {
        eos: Vec<u8>,
    },
    /// Query whether the port's transport is connected — C
    /// `pasynManager->isConnected`. Appended last: the variant order is the
    /// wire encoding.
    GetConnected,
    /// Install the echo interpose — C `asynInterposeEcho(portName, addr)`.
    PushEchoInterpose,
    /// Install the delay interpose — C
    /// `asynInterposeDelay(portName, addr, delay)`. The delay travels as
    /// seconds, the C `double` argument's own unit
    /// (asynInterposeDelay.c:217). Appended last: the variant order is the
    /// wire encoding.
    PushDelayInterpose {
        delay_secs: f64,
    },
    /// Install the EOS interpose — C `asynInterposeEosConfig`. Appended last:
    /// the variant order is the wire encoding.
    PushEosInterpose {
        process_in: bool,
        process_out: bool,
    },
    /// Install the flush-timeout interpose — C `asynInterposeFlushConfig`. The
    /// timeout travels as seconds; C's shell argument is milliseconds.
    PushFlushInterpose {
        flush_timeout_secs: f64,
    },
    /// Set or clear the port's time-stamp source by name — C
    /// `asynRegisterTimeStampSource` / `asynUnregisterTimeStampSource`.
    SetTimeStampSource {
        name: Option<String>,
    },
    /// Read back the driver's input EOS — C `pasynOctet->getInputEos`.
    /// Appended last: the variant order is the wire encoding.
    GetInputEos,
    /// Read back the driver's output EOS — C `pasynOctet->getOutputEos`.
    GetOutputEos,
    /// Send a GPIB universal command byte — C `asynGpib::universalCmd`.
    /// Appended last: the variant order is the wire encoding.
    GpibUniversalCmd {
        cmd: u8,
    },
    /// Send a GPIB addressed-command frame — C `asynGpib::addressedCmd`.
    GpibAddressedCmd {
        data: Vec<u8>,
    },
    /// Assert Interface Clear — C `asynGpib::ifc`.
    GpibIfc,
    /// Set the Remote Enable line — C `asynGpib::ren`.
    GpibRen {
        enable: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_roundtrip_all_variants() {
        let commands = vec![
            PortCommand::Int32Read,
            PortCommand::Int32Write { value: 42 },
            PortCommand::Int64Read,
            PortCommand::Int64Write { value: i64::MAX },
            PortCommand::Float64Read,
            PortCommand::Float64Write { value: 3.14 },
            PortCommand::OctetRead { buf_size: 256 },
            PortCommand::OctetWrite {
                data: vec![1, 2, 3],
            },
            PortCommand::OctetWriteRead {
                data: vec![4, 5],
                buf_size: 128,
                flush: true,
            },
            PortCommand::UInt32DigitalRead { mask: 0xFF },
            PortCommand::UInt32DigitalWrite {
                value: 0xAB,
                mask: 0xFF,
            },
            PortCommand::EnumRead,
            PortCommand::EnumWrite { index: 2 },
            PortCommand::Int32ArrayRead { max_elements: 100 },
            PortCommand::Int32ArrayWrite {
                data: vec![1, 2, 3],
            },
            PortCommand::Float64ArrayRead { max_elements: 50 },
            PortCommand::Float64ArrayWrite {
                data: vec![1.0, 2.0],
            },
            PortCommand::Flush,
            PortCommand::NoIo,
            PortCommand::Connect,
            PortCommand::Disconnect,
            PortCommand::ConnectAddr,
            PortCommand::DisconnectAddr,
            PortCommand::SetEnable { yes: true },
            PortCommand::SetAutoConnect { yes: false },
            PortCommand::GetBoundsInt32,
            PortCommand::GetBoundsInt64,
            PortCommand::GetEnable,
            PortCommand::GetAutoConnect,
            PortCommand::BlockProcess { all_devices: true },
            PortCommand::UnblockProcess { all_devices: true },
            PortCommand::DrvUserCreate {
                drv_info: "MOTOR_STATUS".into(),
                addr: 0,
                iface: Some("asynFloat64".into()),
            },
            PortCommand::CallParamCallbacks { addr: 0 },
            PortCommand::GetOption { key: "baud".into() },
            PortCommand::SetOption {
                key: "baud".into(),
                value: "9600".into(),
            },
            PortCommand::GetConnected,
            PortCommand::PushEchoInterpose,
            PortCommand::PushDelayInterpose { delay_secs: 0.001 },
            PortCommand::PushEosInterpose {
                process_in: true,
                process_out: false,
            },
            PortCommand::PushFlushInterpose {
                flush_timeout_secs: 0.05,
            },
            PortCommand::SetTimeStampSource {
                name: Some("myTimeStamp".into()),
            },
            PortCommand::GetInputEos,
            PortCommand::GetOutputEos,
            PortCommand::GpibUniversalCmd { cmd: 0x14 },
            PortCommand::GpibAddressedCmd {
                data: vec![0x5f, 0x3f, 0x25, 0x08, 0x5f, 0x3f],
            },
            PortCommand::GpibIfc,
            PortCommand::GpibRen { enable: true },
        ];
        for cmd in commands {
            let json = serde_json::to_string(&cmd).unwrap();
            let back: PortCommand = serde_json::from_str(&json).unwrap();
            assert_eq!(cmd, back);
        }
    }
}
