//! Argument / response / error types for [`super::PvaService`].

use crate::nt::NTScalar;
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
            PvField::Scalar(ScalarValue::String(s)) => Ok(s.as_str_lossy().into_owned()),
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
                // Route through the shared NTScalar builder so the RPC
                // response carries value + alarm + timeStamp by
                // construction (pvxs nt.cpp:44-53), not a value-only
                // truncated NTScalar that claims the normative id while
                // omitting the mandatory metadata.
                let builder = NTScalar::new(ScalarType::$st);
                let descriptor = builder.build();
                let mut value = builder.create();
                if let PvField::Structure(s) = &mut value {
                    s.set("value", PvField::Scalar(ScalarValue::$sv(self)));
                }
                ServiceResponse { descriptor, value }
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
        // Same NTScalar baseline as the numeric scalars: value + alarm +
        // timeStamp present by construction (pvxs nt.cpp:44-53).
        let builder = NTScalar::new(ScalarType::String);
        let descriptor = builder.build();
        let mut value = builder.create();
        if let PvField::Structure(s) = &mut value {
            s.set("value", PvField::Scalar(ScalarValue::String(self.into())));
        }
        ServiceResponse { descriptor, value }
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
            PvField::Scalar(ScalarValue::String(self.message.into())),
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

// NOTE: there is deliberately no `IntoServiceResponse for Result<T, E>`.
// A `ServiceResponse` is a *success* payload, so converting a method's
// `Err` through it could only ever produce a success-shaped reply
// (an NTRPCStatus with `ok=false`) — exactly the bug where a failed
// RPC looked successful to the client. The `Result` arm lives on the
// separate [`IntoServiceOutcome`] trait below, which routes `Err` to an
// RPC operation error instead of a success payload.

/// Terminal conversion the `#[pva_service]` macro inserts on every
/// method's return value: it produces the dispatch outcome
/// (`Ok(success)` or `Err(operation-error)`) for both a plain
/// `T: IntoServiceResponse` and a `Result<T, E>`.
///
/// Routing `Result` through the type system here — rather than the
/// macro syntactically testing whether the return type spells `Result`
/// — is what makes a method whose return is a *type alias*
/// (`type RpcResult<T> = Result<T, String>`, `anyhow::Result<T>`, …)
/// behave identically to a literal `Result`: a proc-macro cannot
/// resolve an alias, but the compiler resolves it before selecting the
/// impl. A plain success value hits the blanket impl; a `Result` hits
/// the `Result` impl, whose `Err` becomes [`ServiceError::Method`] (a
/// wire `Status::error` / RPC operation error, matching pvxs
/// `op->error(...)`, `sharedpv.cpp:162-180`). An app that wants an
/// explicit non-error status payload returns `Ok(Status::error(...))`.
pub trait IntoServiceOutcome {
    fn into_service_outcome(self) -> Result<ServiceResponse, ServiceError>;
}

// Plain success value. `Result<T, E>` never reaches this impl: it does
// not implement `IntoServiceResponse` (see the NOTE above), so the two
// impls do not overlap.
impl<T: IntoServiceResponse> IntoServiceOutcome for T {
    fn into_service_outcome(self) -> Result<ServiceResponse, ServiceError> {
        Ok(self.into_service_response())
    }
}

// `Result`-returning method: `Ok(T)` is the success response, `Err(E)`
// is an RPC operation error rather than a success-shaped payload.
impl<T: IntoServiceResponse, E: std::fmt::Display> IntoServiceOutcome for Result<T, E> {
    fn into_service_outcome(self) -> Result<ServiceResponse, ServiceError> {
        match self {
            Ok(v) => Ok(v.into_service_response()),
            Err(e) => Err(ServiceError::Method(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pvdata::value_matches_descriptor;

    fn member_names(d: &FieldDesc) -> Vec<&str> {
        match d {
            FieldDesc::Structure { fields, .. } => fields.iter().map(|(n, _)| n.as_str()).collect(),
            _ => Vec::new(),
        }
    }

    /// A scalar `#[pva_service]` return must produce the full NTScalar
    /// baseline (value + alarm + timeStamp), identical to the dedicated
    /// builder — not a value-only truncated NTScalar.
    #[test]
    fn scalar_response_is_full_ntscalar() {
        let r = 1.5_f64.into_service_response();
        assert_eq!(r.descriptor, NTScalar::new(ScalarType::Double).build());
        assert_eq!(
            member_names(&r.descriptor),
            vec!["value", "alarm", "timeStamp"]
        );
        // the value body is internally consistent with its descriptor.
        assert!(value_matches_descriptor(&r.value, &r.descriptor).is_ok());

        let r = 7_i32.into_service_response();
        assert_eq!(r.descriptor, NTScalar::new(ScalarType::Int).build());
        assert!(value_matches_descriptor(&r.value, &r.descriptor).is_ok());

        let r = true.into_service_response();
        assert_eq!(r.descriptor, NTScalar::new(ScalarType::Boolean).build());
        assert!(value_matches_descriptor(&r.value, &r.descriptor).is_ok());
    }

    /// The `String` return shares the same full NTScalar baseline and
    /// carries the real value plus alarm/timeStamp.
    #[test]
    fn string_response_is_full_ntscalar() {
        let r = "hi".to_string().into_service_response();
        assert_eq!(r.descriptor, NTScalar::new(ScalarType::String).build());
        assert_eq!(
            member_names(&r.descriptor),
            vec!["value", "alarm", "timeStamp"]
        );
        if let PvField::Structure(s) = &r.value {
            assert!(matches!(
                s.get_field("value"),
                Some(PvField::Scalar(ScalarValue::String(v))) if v == "hi"
            ));
            assert!(s.get_field("alarm").is_some(), "alarm present");
            assert!(s.get_field("timeStamp").is_some(), "timeStamp present");
        } else {
            panic!("response value must be a structure");
        }
        assert!(value_matches_descriptor(&r.value, &r.descriptor).is_ok());
    }
}
