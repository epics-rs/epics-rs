use serde::{Deserialize, Serialize};

/// Protocol-level command enum. 1:1 map from `RequestOp`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PortCommand {
    Int32Read,
    Int32Write { value: i32 },
    Int64Read,
    Int64Write { value: i64 },
    Float64Read,
    Float64Write { value: f64 },
    OctetRead { buf_size: usize },
    OctetWrite { data: Vec<u8> },
    OctetWriteRead { data: Vec<u8>, buf_size: usize },
    UInt32DigitalRead { mask: u32 },
    UInt32DigitalWrite { value: u32, mask: u32 },
    EnumRead,
    EnumWrite { index: usize },
    Int32ArrayRead { max_elements: usize },
    Int32ArrayWrite { data: Vec<i32> },
    Float64ArrayRead { max_elements: usize },
    Float64ArrayWrite { data: Vec<f64> },
    Int8ArrayRead { max_elements: usize },
    Int8ArrayWrite { data: Vec<i8> },
    Int16ArrayRead { max_elements: usize },
    Int16ArrayWrite { data: Vec<i16> },
    Int64ArrayRead { max_elements: usize },
    Int64ArrayWrite { data: Vec<i64> },
    Float32ArrayRead { max_elements: usize },
    Float32ArrayWrite { data: Vec<f32> },
    Flush,
    Connect,
    Disconnect,
    BlockProcess,
    UnblockProcess,
    DrvUserCreate { drv_info: String },
    CallParamCallbacks { addr: i32 },
    GetOption { key: String },
    SetOption { key: String, value: String },
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
            PortCommand::Connect,
            PortCommand::Disconnect,
            PortCommand::BlockProcess,
            PortCommand::UnblockProcess,
            PortCommand::DrvUserCreate {
                drv_info: "MOTOR_STATUS".into(),
            },
            PortCommand::CallParamCallbacks { addr: 0 },
            PortCommand::GetOption { key: "baud".into() },
            PortCommand::SetOption {
                key: "baud".into(),
                value: "9600".into(),
            },
        ];
        for cmd in commands {
            let json = serde_json::to_string(&cmd).unwrap();
            let back: PortCommand = serde_json::from_str(&json).unwrap();
            assert_eq!(cmd, back);
        }
    }
}
