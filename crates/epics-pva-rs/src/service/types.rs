//! Argument / response / error types for [`super::PvaService`].

use crate::pvdata::{FieldDesc, PvField, PvStructure, ScalarType, ScalarValue};

/// Errors a service method can surface. The framework converts
/// these into PVA `Status::Error` responses.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ServiceError {
    /// Caller didn't provide a required argument.
    #[error("missing argument '{0}'")]
    MissingArg(String),
    /// Argument was provided but couldn't be coerced to the
    /// expected Rust type.
    #[error("wrong type for argument '{0}': {1}")]
    WrongArgType(String, String),
    /// Method-specific failure (business logic error). Free-form
    /// string; the framework forwards as the PVA error message.
    #[error("{0}")]
    Method(String),
}

impl From<String> for ServiceError {
    fn from(s: String) -> Self {
        ServiceError::Method(s)
    }
}

impl From<&str> for ServiceError {
    fn from(s: &str) -> Self {
        ServiceError::Method(s.to_string())
    }
}

/// One argument deserializable from a [`PvField`]. Every type the
/// `#[pva_service]` macro accepts as a method parameter must
/// implement this. Built-in impls cover the PVA scalar set; users
/// can implement it on their own struct types or pull from
/// [`crate::nt::TypedNT`] via the blanket impl below.
pub trait ServiceArg: Sized {
    fn from_pv_field(field: &PvField) -> Result<Self, String>;
}

macro_rules! impl_arg_scalar {
    ($t:ty, $sv:ident, $coerce:expr) => {
        impl ServiceArg for $t {
            fn from_pv_field(field: &PvField) -> Result<Self, String> {
                match field {
                    PvField::Scalar(ScalarValue::$sv(v)) => Ok(*v),
                    PvField::Scalar(other) => $coerce(other)
                        .ok_or_else(|| format!("expected {}, got {:?}", stringify!($t), other)),
                    other => Err(format!("expected scalar, got {other:?}")),
                }
            }
        }
    };
}

impl_arg_scalar!(f64, Double, |s: &ScalarValue| match s {
    ScalarValue::Float(v) => Some(*v as f64),
    ScalarValue::Long(v) => Some(*v as f64),
    ScalarValue::Int(v) => Some(*v as f64),
    _ => None,
});
impl_arg_scalar!(f32, Float, |s: &ScalarValue| match s {
    ScalarValue::Double(v) => Some(*v as f32),
    _ => None,
});
impl_arg_scalar!(i64, Long, |s: &ScalarValue| match s {
    ScalarValue::Int(v) => Some(*v as i64),
    _ => None,
});
impl_arg_scalar!(i32, Int, |s: &ScalarValue| match s {
    // use try_from so a Long value outside i32 range surfaces
    // as WrongArgType instead of silently truncating modulo 2^32.
    ScalarValue::Long(v) => i32::try_from(*v).ok(),
    ScalarValue::Short(v) => Some(*v as i32),
    _ => None,
});
impl_arg_scalar!(i16, Short, |_: &ScalarValue| None);
impl_arg_scalar!(i8, Byte, |_: &ScalarValue| None);
impl_arg_scalar!(u64, ULong, |_: &ScalarValue| None);
impl_arg_scalar!(u32, UInt, |_: &ScalarValue| None);
impl_arg_scalar!(u16, UShort, |_: &ScalarValue| None);
impl_arg_scalar!(u8, UByte, |_: &ScalarValue| None);
impl_arg_scalar!(bool, Boolean, |_: &ScalarValue| None);

impl ServiceArg for String {
    fn from_pv_field(field: &PvField) -> Result<Self, String> {
        match field {
            PvField::Scalar(ScalarValue::String(s)) => Ok(s.clone()),
            other => Err(format!("expected String scalar, got {other:?}")),
        }
    }
}

/// Typed RPC response. Emitted by the framework after the user's
/// method returns its `T: IntoServiceResponse` value.
pub struct ServiceResponse {
    pub descriptor: FieldDesc,
    pub value: PvField,
}

/// Convert a method's return type into a [`ServiceResponse`]. The
/// `#[pva_service]` macro inserts a call to this on every method's
/// return.
pub trait IntoServiceResponse {
    fn into_service_response(self) -> ServiceResponse;
}

// Scalars become NTScalar-shaped wrappers.
macro_rules! impl_resp_scalar {
    ($t:ty, $st:ident, $sv:ident) => {
        impl IntoServiceResponse for $t {
            fn into_service_response(self) -> ServiceResponse {
                let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
                s.fields
                    .push(("value".into(), PvField::Scalar(ScalarValue::$sv(self))));
                ServiceResponse {
                    descriptor: FieldDesc::Structure {
                        struct_id: "epics:nt/NTScalar:1.0".into(),
                        fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::$st))],
                    },
                    value: PvField::Structure(s),
                }
            }
        }
    };
}

impl_resp_scalar!(f64, Double, Double);
impl_resp_scalar!(f32, Float, Float);
impl_resp_scalar!(i64, Long, Long);
impl_resp_scalar!(i32, Int, Int);
impl_resp_scalar!(i16, Short, Short);
impl_resp_scalar!(i8, Byte, Byte);
impl_resp_scalar!(u64, ULong, ULong);
impl_resp_scalar!(u32, UInt, UInt);
impl_resp_scalar!(u16, UShort, UShort);
impl_resp_scalar!(u8, UByte, UByte);
impl_resp_scalar!(bool, Boolean, Boolean);

impl IntoServiceResponse for String {
    fn into_service_response(self) -> ServiceResponse {
        let mut s = PvStructure::new("epics:nt/NTScalar:1.0");
        s.fields
            .push(("value".into(), PvField::Scalar(ScalarValue::String(self))));
        ServiceResponse {
            descriptor: FieldDesc::Structure {
                struct_id: "epics:nt/NTScalar:1.0".into(),
                fields: vec![("value".into(), FieldDesc::Scalar(ScalarType::String))],
            },
            value: PvField::Structure(s),
        }
    }
}

/// Standard "operation outcome" response. Use as the return type
/// when your service's success path doesn't need to carry data.
#[derive(Debug, Clone)]
pub struct Status {
    pub ok: bool,
    pub message: String,
}

impl Status {
    pub fn ok() -> Self {
        Self {
            ok: true,
            message: String::new(),
        }
    }
    pub fn error(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            message: msg.into(),
        }
    }
}

impl IntoServiceResponse for Status {
    fn into_service_response(self) -> ServiceResponse {
        let mut s = PvStructure::new("epics:nt/NTRPCStatus:1.0");
        s.fields
            .push(("ok".into(), PvField::Scalar(ScalarValue::Boolean(self.ok))));
        s.fields.push((
            "message".into(),
            PvField::Scalar(ScalarValue::String(self.message)),
        ));
        ServiceResponse {
            descriptor: FieldDesc::Structure {
                struct_id: "epics:nt/NTRPCStatus:1.0".into(),
                fields: vec![
                    ("ok".into(), FieldDesc::Scalar(ScalarType::Boolean)),
                    ("message".into(), FieldDesc::Scalar(ScalarType::String)),
                ],
            },
            value: PvField::Structure(s),
        }
    }
}

// Result wrappers — Err short-circuits the dispatch into the
// PVA error path.
impl<T: IntoServiceResponse, E: std::fmt::Display> IntoServiceResponse for Result<T, E> {
    fn into_service_response(self) -> ServiceResponse {
        match self {
            Ok(v) => v.into_service_response(),
            Err(e) => Status::error(e.to_string()).into_service_response(),
        }
    }
}
