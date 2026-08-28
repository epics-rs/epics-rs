//! Conversions between internal types and protocol types.

use crate::error::AsynStatus;
use crate::interrupt::InterruptValue;
use crate::request::{RequestOp, RequestResult};

use super::command::PortCommand;
use super::event::EventPayload;
use super::reply::{PortReply, ReplyPayload};
use super::status::ReplyStatus;
use super::value::{AlarmMeta, ParamValue, Timestamp};

// --- RequestOp <-> PortCommand ---

impl From<&RequestOp> for PortCommand {
    fn from(op: &RequestOp) -> Self {
        match op {
            RequestOp::Int32Read => Self::Int32Read,
            RequestOp::Int32Write { value } => Self::Int32Write { value: *value },
            RequestOp::Int64Read => Self::Int64Read,
            RequestOp::Int64Write { value } => Self::Int64Write { value: *value },
            RequestOp::Float64Read => Self::Float64Read,
            RequestOp::Float64Write { value } => Self::Float64Write { value: *value },
            RequestOp::OctetRead { buf_size } => Self::OctetRead {
                buf_size: *buf_size,
            },
            RequestOp::OctetWrite { data } => Self::OctetWrite { data: data.clone() },
            RequestOp::OctetWriteRead {
                data,
                buf_size,
                flush,
            } => Self::OctetWriteRead {
                data: data.clone(),
                buf_size: *buf_size,
                flush: *flush,
            },
            RequestOp::OctetWriteBinary { data } => Self::OctetWriteBinary { data: data.clone() },
            RequestOp::OctetReadBinary { buf_size } => Self::OctetReadBinary {
                buf_size: *buf_size,
            },
            RequestOp::UInt32DigitalRead { mask } => Self::UInt32DigitalRead { mask: *mask },
            RequestOp::UInt32DigitalWrite { value, mask } => Self::UInt32DigitalWrite {
                value: *value,
                mask: *mask,
            },
            RequestOp::EnumRead => Self::EnumRead,
            RequestOp::EnumWrite { index } => Self::EnumWrite { index: *index },
            RequestOp::Int32ArrayRead { max_elements } => Self::Int32ArrayRead {
                max_elements: *max_elements,
            },
            RequestOp::Int32ArrayWrite { data } => Self::Int32ArrayWrite { data: data.clone() },
            RequestOp::Float64ArrayRead { max_elements } => Self::Float64ArrayRead {
                max_elements: *max_elements,
            },
            RequestOp::Float64ArrayWrite { data } => Self::Float64ArrayWrite { data: data.clone() },
            RequestOp::Int8ArrayRead { max_elements } => Self::Int8ArrayRead {
                max_elements: *max_elements,
            },
            RequestOp::Int8ArrayWrite { data } => Self::Int8ArrayWrite { data: data.clone() },
            RequestOp::Int16ArrayRead { max_elements } => Self::Int16ArrayRead {
                max_elements: *max_elements,
            },
            RequestOp::Int16ArrayWrite { data } => Self::Int16ArrayWrite { data: data.clone() },
            RequestOp::Int64ArrayRead { max_elements } => Self::Int64ArrayRead {
                max_elements: *max_elements,
            },
            RequestOp::Int64ArrayWrite { data } => Self::Int64ArrayWrite { data: data.clone() },
            RequestOp::Float32ArrayRead { max_elements } => Self::Float32ArrayRead {
                max_elements: *max_elements,
            },
            RequestOp::Float32ArrayWrite { data } => Self::Float32ArrayWrite { data: data.clone() },
            RequestOp::Flush => Self::Flush,
            RequestOp::NoIo => Self::NoIo,
            RequestOp::Connect => Self::Connect,
            RequestOp::Disconnect => Self::Disconnect,
            RequestOp::ShutdownPort => Self::ShutdownPort,
            RequestOp::ConnectAddr => Self::ConnectAddr,
            RequestOp::DisconnectAddr => Self::DisconnectAddr,
            RequestOp::EnableAddr => Self::EnableAddr,
            RequestOp::DisableAddr => Self::DisableAddr,
            RequestOp::SetEnable { yes } => Self::SetEnable { yes: *yes },
            RequestOp::SetAutoConnect { yes } => Self::SetAutoConnect { yes: *yes },
            RequestOp::SetAutoConnectAddr { yes } => Self::SetAutoConnectAddr { yes: *yes },
            RequestOp::GetBoundsInt32 => Self::GetBoundsInt32,
            RequestOp::GetBoundsInt64 => Self::GetBoundsInt64,
            RequestOp::GetEnable => Self::GetEnable,
            RequestOp::GetAutoConnect => Self::GetAutoConnect,
            RequestOp::GetConnected => Self::GetConnected,
            RequestOp::PushEchoInterpose => Self::PushEchoInterpose,
            RequestOp::PushDelayInterpose { delay } => Self::PushDelayInterpose {
                delay_secs: delay.as_secs_f64(),
            },
            RequestOp::PushEosInterpose {
                process_in,
                process_out,
            } => Self::PushEosInterpose {
                process_in: *process_in,
                process_out: *process_out,
            },
            RequestOp::PushFlushInterpose { flush_timeout } => Self::PushFlushInterpose {
                flush_timeout_secs: flush_timeout.as_secs_f64(),
            },
            RequestOp::BlockProcess { all_devices } => Self::BlockProcess {
                all_devices: *all_devices,
            },
            RequestOp::UnblockProcess { all_devices } => Self::UnblockProcess {
                all_devices: *all_devices,
            },
            RequestOp::DrvUserCreate(req) => Self::DrvUserCreate {
                drv_info: req.drv_info.clone(),
                addr: req.addr,
                iface: req.iface.map(|i| i.asyn_name().to_string()),
            },
            RequestOp::CallParamCallbacks { addr, .. } => Self::CallParamCallbacks { addr: *addr },
            RequestOp::GetOption { key } => Self::GetOption { key: key.clone() },
            RequestOp::SetOption { key, value } => Self::SetOption {
                key: key.clone(),
                value: value.clone(),
            },
            RequestOp::Report { level } => Self::Report { level: *level },
            RequestOp::SetInputEos { eos } => Self::SetInputEos { eos: eos.clone() },
            RequestOp::SetOutputEos { eos } => Self::SetOutputEos { eos: eos.clone() },
            RequestOp::SetTimeStampSource { name } => {
                Self::SetTimeStampSource { name: name.clone() }
            }
            RequestOp::GetInputEos => Self::GetInputEos,
            RequestOp::GetOutputEos => Self::GetOutputEos,
            RequestOp::GpibUniversalCmd { cmd } => Self::GpibUniversalCmd { cmd: *cmd },
            RequestOp::GpibAddressedCmd { data } => Self::GpibAddressedCmd { data: data.clone() },
            RequestOp::GpibIfc => Self::GpibIfc,
            RequestOp::GpibRen { enable } => Self::GpibRen { enable: *enable },
        }
    }
}

impl From<RequestOp> for PortCommand {
    fn from(op: RequestOp) -> Self {
        Self::from(&op)
    }
}

impl From<&PortCommand> for RequestOp {
    fn from(cmd: &PortCommand) -> Self {
        match cmd {
            PortCommand::Int32Read => Self::Int32Read,
            PortCommand::Int32Write { value } => Self::Int32Write { value: *value },
            PortCommand::Int64Read => Self::Int64Read,
            PortCommand::Int64Write { value } => Self::Int64Write { value: *value },
            PortCommand::Float64Read => Self::Float64Read,
            PortCommand::Float64Write { value } => Self::Float64Write { value: *value },
            PortCommand::OctetRead { buf_size } => Self::OctetRead {
                buf_size: *buf_size,
            },
            PortCommand::OctetWrite { data } => Self::OctetWrite { data: data.clone() },
            PortCommand::OctetWriteRead {
                data,
                buf_size,
                flush,
            } => Self::OctetWriteRead {
                data: data.clone(),
                buf_size: *buf_size,
                flush: *flush,
            },
            PortCommand::OctetWriteBinary { data } => Self::OctetWriteBinary { data: data.clone() },
            PortCommand::OctetReadBinary { buf_size } => Self::OctetReadBinary {
                buf_size: *buf_size,
            },
            PortCommand::UInt32DigitalRead { mask } => Self::UInt32DigitalRead { mask: *mask },
            PortCommand::UInt32DigitalWrite { value, mask } => Self::UInt32DigitalWrite {
                value: *value,
                mask: *mask,
            },
            PortCommand::EnumRead => Self::EnumRead,
            PortCommand::EnumWrite { index } => Self::EnumWrite { index: *index },
            PortCommand::Int32ArrayRead { max_elements } => Self::Int32ArrayRead {
                max_elements: *max_elements,
            },
            PortCommand::Int32ArrayWrite { data } => Self::Int32ArrayWrite { data: data.clone() },
            PortCommand::Float64ArrayRead { max_elements } => Self::Float64ArrayRead {
                max_elements: *max_elements,
            },
            PortCommand::Float64ArrayWrite { data } => {
                Self::Float64ArrayWrite { data: data.clone() }
            }
            PortCommand::Int8ArrayRead { max_elements } => Self::Int8ArrayRead {
                max_elements: *max_elements,
            },
            PortCommand::Int8ArrayWrite { data } => Self::Int8ArrayWrite { data: data.clone() },
            PortCommand::Int16ArrayRead { max_elements } => Self::Int16ArrayRead {
                max_elements: *max_elements,
            },
            PortCommand::Int16ArrayWrite { data } => Self::Int16ArrayWrite { data: data.clone() },
            PortCommand::Int64ArrayRead { max_elements } => Self::Int64ArrayRead {
                max_elements: *max_elements,
            },
            PortCommand::Int64ArrayWrite { data } => Self::Int64ArrayWrite { data: data.clone() },
            PortCommand::Float32ArrayRead { max_elements } => Self::Float32ArrayRead {
                max_elements: *max_elements,
            },
            PortCommand::Float32ArrayWrite { data } => {
                Self::Float32ArrayWrite { data: data.clone() }
            }
            PortCommand::Flush => Self::Flush,
            PortCommand::NoIo => Self::NoIo,
            PortCommand::Connect => Self::Connect,
            PortCommand::Disconnect => Self::Disconnect,
            PortCommand::ShutdownPort => Self::ShutdownPort,
            PortCommand::ConnectAddr => Self::ConnectAddr,
            PortCommand::DisconnectAddr => Self::DisconnectAddr,
            PortCommand::EnableAddr => Self::EnableAddr,
            PortCommand::DisableAddr => Self::DisableAddr,
            PortCommand::SetEnable { yes } => Self::SetEnable { yes: *yes },
            PortCommand::SetAutoConnect { yes } => Self::SetAutoConnect { yes: *yes },
            PortCommand::SetAutoConnectAddr { yes } => Self::SetAutoConnectAddr { yes: *yes },
            PortCommand::GetBoundsInt32 => Self::GetBoundsInt32,
            PortCommand::GetBoundsInt64 => Self::GetBoundsInt64,
            PortCommand::GetEnable => Self::GetEnable,
            PortCommand::GetAutoConnect => Self::GetAutoConnect,
            PortCommand::GetConnected => Self::GetConnected,
            PortCommand::PushEchoInterpose => Self::PushEchoInterpose,
            // A negative / non-finite wire value would panic
            // `Duration::from_secs_f64`; `delay_from_secs` owns the conversion
            // and collapses those to C's "no delay".
            PortCommand::PushDelayInterpose { delay_secs } => Self::PushDelayInterpose {
                delay: crate::interpose::delay::delay_from_secs(*delay_secs),
            },
            PortCommand::PushEosInterpose {
                process_in,
                process_out,
            } => Self::PushEosInterpose {
                process_in: *process_in,
                process_out: *process_out,
            },
            PortCommand::PushFlushInterpose { flush_timeout_secs } => Self::PushFlushInterpose {
                flush_timeout: std::time::Duration::from_secs_f64(*flush_timeout_secs),
            },
            PortCommand::BlockProcess { all_devices } => Self::BlockProcess {
                all_devices: *all_devices,
            },
            PortCommand::UnblockProcess { all_devices } => Self::UnblockProcess {
                all_devices: *all_devices,
            },
            PortCommand::DrvUserCreate {
                drv_info,
                addr,
                iface,
            } => Self::DrvUserCreate(
                crate::port::DrvUserRequest::new(drv_info.clone(), *addr).with_iface(
                    iface
                        .as_deref()
                        .and_then(crate::interfaces::InterfaceType::from_asyn_name),
                ),
            ),
            PortCommand::CallParamCallbacks { addr } => Self::CallParamCallbacks {
                addr: *addr,
                updates: vec![],
            },
            PortCommand::GetOption { key } => Self::GetOption { key: key.clone() },
            PortCommand::SetOption { key, value } => Self::SetOption {
                key: key.clone(),
                value: value.clone(),
            },
            PortCommand::Report { level } => Self::Report { level: *level },
            PortCommand::SetInputEos { eos } => Self::SetInputEos { eos: eos.clone() },
            PortCommand::SetOutputEos { eos } => Self::SetOutputEos { eos: eos.clone() },
            PortCommand::SetTimeStampSource { name } => {
                Self::SetTimeStampSource { name: name.clone() }
            }
            PortCommand::GetInputEos => Self::GetInputEos,
            PortCommand::GetOutputEos => Self::GetOutputEos,
            PortCommand::GpibUniversalCmd { cmd } => Self::GpibUniversalCmd { cmd: *cmd },
            PortCommand::GpibAddressedCmd { data } => Self::GpibAddressedCmd { data: data.clone() },
            PortCommand::GpibIfc => Self::GpibIfc,
            PortCommand::GpibRen { enable } => Self::GpibRen { enable: *enable },
        }
    }
}

impl From<PortCommand> for RequestOp {
    fn from(cmd: PortCommand) -> Self {
        Self::from(&cmd)
    }
}

// --- RequestResult -> PortReply ---

/// Convert a `RequestResult` into a `PortReply` with a given request_id.
pub fn result_to_reply(result: &RequestResult, request_id: u64) -> PortReply {
    let payload = if result.status != AsynStatus::Success {
        ReplyPayload::Error {
            code: ReplyStatus::from(result.status),
            detail: result.message.clone(),
        }
    } else if let Some(data) = &result.data {
        ReplyPayload::OctetData {
            data: data.clone(),
            nbytes: result.nbytes,
            eom_reason: result.eom_reason,
        }
    } else if let Some(v) = result.int_val {
        ReplyPayload::Value(ParamValue::Int32(v))
    } else if let Some(v) = result.int64_val {
        ReplyPayload::Value(ParamValue::Int64(v))
    } else if let Some(v) = result.float_val {
        ReplyPayload::Value(ParamValue::Float64(v))
    } else if let Some(v) = result.uint_val {
        ReplyPayload::Value(ParamValue::UInt32Digital(v))
    } else if let Some(v) = result.enum_index {
        ReplyPayload::Value(ParamValue::Enum {
            index: v,
            choices: Vec::new(),
        })
    } else if let Some(ref v) = result.int32_array {
        ReplyPayload::Value(ParamValue::Int32Array(v.clone()))
    } else if let Some(ref v) = result.float64_array {
        ReplyPayload::Value(ParamValue::Float64Array(v.clone()))
    } else if let Some(ref v) = result.int8_array {
        ReplyPayload::Value(ParamValue::Int8Array(v.clone()))
    } else if let Some(ref v) = result.int16_array {
        ReplyPayload::Value(ParamValue::Int16Array(v.clone()))
    } else if let Some(ref v) = result.int64_array {
        ReplyPayload::Value(ParamValue::Int64Array(v.clone()))
    } else if let Some(ref v) = result.float32_array {
        ReplyPayload::Value(ParamValue::Float32Array(v.clone()))
    } else if let Some(v) = result.reason {
        // DrvUserCreate returns reason index
        ReplyPayload::Value(ParamValue::Int32(v as i32))
    } else if let Some((low, high)) = result.bounds {
        // GetBoundsInt32 / GetBoundsInt64
        ReplyPayload::Bounds { low, high }
    } else if let Some(ref v) = result.option_value {
        // GetOption returns a string value
        ReplyPayload::OctetData {
            data: v.as_bytes().to_vec(),
            nbytes: v.len(),
            eom_reason: 0,
        }
    } else {
        ReplyPayload::Ack
    };

    let alarm = if result.alarm_status != 0 || result.alarm_severity != 0 {
        Some(AlarmMeta {
            status: result.alarm_status,
            severity: result.alarm_severity,
        })
    } else {
        None
    };

    let timestamp = result.timestamp.map(Timestamp::from);

    PortReply {
        request_id,
        payload,
        alarm,
        timestamp,
    }
}

// --- InterruptValue -> EventPayload ---

impl From<&InterruptValue> for EventPayload {
    fn from(iv: &InterruptValue) -> Self {
        Self::ValueChanged {
            reason: iv.reason,
            addr: iv.addr,
            value: ParamValue::from(&iv.value),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;

    #[test]
    fn request_op_to_command_roundtrip() {
        let ops = vec![
            RequestOp::Int32Read,
            RequestOp::Int32Write { value: 42 },
            RequestOp::Int64Read,
            RequestOp::Int64Write { value: i64::MAX },
            RequestOp::Float64Read,
            RequestOp::Float64Write { value: 3.14 },
            RequestOp::OctetRead { buf_size: 256 },
            RequestOp::OctetWrite {
                data: vec![1, 2, 3],
            },
            RequestOp::OctetWriteRead {
                data: vec![4, 5],
                buf_size: 128,
                flush: true,
            },
            RequestOp::UInt32DigitalRead { mask: 0xFF },
            RequestOp::UInt32DigitalWrite {
                value: 0xAB,
                mask: 0xFF,
            },
            RequestOp::EnumRead,
            RequestOp::EnumWrite { index: 2 },
            RequestOp::Int32ArrayRead { max_elements: 100 },
            RequestOp::Int32ArrayWrite {
                data: vec![1, 2, 3],
            },
            RequestOp::Float64ArrayRead { max_elements: 50 },
            RequestOp::Float64ArrayWrite {
                data: vec![1.0, 2.0],
            },
            RequestOp::Flush,
            RequestOp::NoIo,
            RequestOp::Connect,
            RequestOp::Disconnect,
            RequestOp::BlockProcess { all_devices: true },
            RequestOp::BlockProcess { all_devices: false },
            RequestOp::UnblockProcess { all_devices: true },
            RequestOp::UnblockProcess { all_devices: false },
            RequestOp::DrvUserCreate(
                crate::port::DrvUserRequest::new("INFO", 0)
                    .with_iface(crate::interfaces::InterfaceType::Float64),
            ),
            RequestOp::GetOption { key: "baud".into() },
            RequestOp::SetOption {
                key: "baud".into(),
                value: "9600".into(),
            },
        ];
        for op in ops {
            let cmd = PortCommand::from(&op);
            let back = RequestOp::from(&cmd);
            // Verify field-level equality since RequestOp doesn't derive PartialEq
            let cmd2 = PortCommand::from(&back);
            assert_eq!(cmd, cmd2, "roundtrip failed for {op:?}");
        }
    }

    #[test]
    fn result_to_reply_write_ok() {
        let result = RequestResult::write_ok();
        let reply = result_to_reply(&result, 1);
        assert_eq!(reply.request_id, 1);
        assert_eq!(reply.payload, ReplyPayload::Ack);
        assert!(reply.alarm.is_none());
        assert!(reply.timestamp.is_none());
    }

    #[test]
    fn result_to_reply_int32_read() {
        let result = RequestResult::int32_read(42);
        let reply = result_to_reply(&result, 2);
        assert_eq!(reply.payload, ReplyPayload::Value(ParamValue::Int32(42)));
    }

    #[test]
    fn result_to_reply_float64_read() {
        let result = RequestResult::float64_read(3.14);
        let reply = result_to_reply(&result, 3);
        assert_eq!(
            reply.payload,
            ReplyPayload::Value(ParamValue::Float64(3.14))
        );
    }

    #[test]
    fn result_to_reply_bounds() {
        // GetBoundsInt32/Int64 results carry low/high in `bounds` only —
        // they must not collapse to an empty Ack.
        let result = RequestResult::bounds_read(-2048, 2047);
        let reply = result_to_reply(&result, 8);
        assert_eq!(
            reply.payload,
            ReplyPayload::Bounds {
                low: -2048,
                high: 2047,
            }
        );
    }

    #[test]
    fn result_to_reply_octet_carries_eom_reason() {
        let result = RequestResult::octet_read_eom(vec![0x41], 1, 0x01 | 0x02);
        let reply = result_to_reply(&result, 9);
        assert_eq!(
            reply.payload,
            ReplyPayload::OctetData {
                data: vec![0x41],
                nbytes: 1,
                eom_reason: 0x03,
            }
        );
    }

    #[test]
    fn result_to_reply_octet_read() {
        let result = RequestResult::octet_read(vec![0x48, 0x65], 2);
        let reply = result_to_reply(&result, 4);
        assert_eq!(
            reply.payload,
            ReplyPayload::OctetData {
                data: vec![0x48, 0x65],
                nbytes: 2,
                eom_reason: 0,
            }
        );
    }

    #[test]
    fn result_to_reply_with_alarm() {
        let result = RequestResult::int32_read(0).with_alarm(1, 2, Some(SystemTime::UNIX_EPOCH));
        let reply = result_to_reply(&result, 5);
        assert_eq!(
            reply.alarm,
            Some(AlarmMeta {
                status: 1,
                severity: 2
            })
        );
        assert!(reply.timestamp.is_some());
    }

    #[test]
    fn result_to_reply_error() {
        let result = RequestResult {
            status: AsynStatus::Timeout,
            message: "timed out".into(),
            ..RequestResult::write_ok()
        };
        let reply = result_to_reply(&result, 6);
        match reply.payload {
            ReplyPayload::Error { code, detail } => {
                assert_eq!(code, ReplyStatus::Timeout);
                assert_eq!(detail, "timed out");
            }
            _ => panic!("expected Error payload"),
        }
    }

    #[test]
    fn interrupt_value_to_event_payload() {
        let iv = InterruptValue {
            reason: 5,
            addr: 2,
            value: crate::param::ParamValue::Float64(1.5),
            timestamp: SystemTime::now(),
            uint32_changed_mask: 0,
            ..Default::default()
        };
        let payload = EventPayload::from(&iv);
        match payload {
            EventPayload::ValueChanged {
                reason,
                addr,
                value,
            } => {
                assert_eq!(reason, 5);
                assert_eq!(addr, 2);
                assert_eq!(value, ParamValue::Float64(1.5));
            }
            _ => panic!("expected ValueChanged"),
        }
    }

    #[test]
    fn result_to_reply_int32_array() {
        let result = RequestResult::int32_array_read(vec![10, 20, 30]);
        let reply = result_to_reply(&result, 7);
        assert_eq!(
            reply.payload,
            ReplyPayload::Value(ParamValue::Int32Array(vec![10, 20, 30]))
        );
    }

    #[test]
    fn result_to_reply_float64_array() {
        let result = RequestResult::float64_array_read(vec![1.0, 2.0]);
        let reply = result_to_reply(&result, 8);
        assert_eq!(
            reply.payload,
            ReplyPayload::Value(ParamValue::Float64Array(vec![1.0, 2.0]))
        );
    }
}
