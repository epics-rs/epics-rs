//! pvData runtime model: scalar tags/values, structures, and field
//! descriptors. Wire encoding/decoding lives in [`encode`].

pub(crate) mod cpp_cast;
mod field;
pub(crate) mod monitor_squash;
mod rpc_reply;
mod scalar;
mod shared_array;
mod structure;
mod typed_array;
mod value;
mod value_check;

pub mod convert;
pub mod encode;
pub mod fmt;

pub use convert::{Kind, NoConvert};
pub use field::{FieldDesc, Member, TypeDef};
pub use fmt::render_value;
pub use rpc_reply::RpcReply;
pub use scalar::{ScalarType, ScalarValue};
pub use shared_array::SharedArray;
pub use structure::{PvField, PvStructure, UnionItem, VariantValue};
pub use typed_array::TypedScalarArray;
pub use value::{FromScalarValue, IntoScalarValue, Value, ValueError};
pub use value_check::{ValueDescMismatch, value_matches_descriptor};
