//! pvData runtime model: scalar tags/values, structures, and field
//! descriptors. Wire encoding/decoding lives in [`encode`].

mod field;
mod rpc_reply;
mod scalar;
mod shared_array;
mod structure;
mod typed_array;
mod value;
mod value_check;

pub mod convert;
pub mod encode;

pub use convert::{Kind, NoConvert};
pub use field::{FieldDesc, Member, TypeDef};
pub use rpc_reply::RpcReply;
pub use scalar::{ScalarType, ScalarValue};
pub use shared_array::SharedArray;
pub use structure::{PvField, PvStructure, UnionItem, VariantValue};
pub use typed_array::TypedScalarArray;
pub use value::{FromScalarValue, IntoScalarValue, Value, ValueError};
pub use value_check::{ValueDescMismatch, value_matches_descriptor};
