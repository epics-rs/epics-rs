//! The value an RPC carries back — pvxs's two `ExecOp::reply()` overloads
//! as one type.

use super::{FieldDesc, PvField};

/// An RPC reply's payload: either a `(descriptor, value)` pair or **no value
/// at all**.
///
/// pvxs writes an RPC reply as
///
/// ```c
/// } else if(cmd==CMD_RPC) {
///     auto type = Value::Helper::desc(value);
///     to_wire(R, type);
///     if(value)
///         to_wire_full(R, value);
/// }
/// ```
///
/// (`serverget.cpp:105-109`). The no-argument `ExecOp::reply()` overload
/// (`pvxs/srvcommon.h:108`) reaches `doReply(Value(), …)` with a
/// default-constructed `Value`, so `desc()` is `nullptr` and
/// `to_wire(Buf&, const FieldDesc*)` emits exactly one `0xFF` byte with no
/// value body (`dataencode.cpp:29-33`). The pvxs client accepts that:
/// `from_wire_type(M, rxRegistry, data); if(data) from_wire_full(...)`
/// (`clientget.cpp:415-421`) leaves `data` an empty `Value`.
///
/// [`RpcReply::Empty`] is that reply. It is **not** the same as
/// `Value(FieldDesc::Variant, PvField::Null)`, which is a *present* `any`
/// field holding nothing and serializes as two bytes (`0x82 0xFF`) — keeping
/// the two apart is why this is an enum and not an `Option` over the tuple.
#[derive(Debug, Clone, PartialEq)]
pub enum RpcReply {
    /// pvxs `ExecOp::reply()` — a single `0xFF` NULL type code, no body.
    Empty,
    /// pvxs `ExecOp::reply(Value)` — type descriptor + full value.
    Value(FieldDesc, PvField),
}

impl RpcReply {
    /// The reply's `(descriptor, value)`, or `None` for [`RpcReply::Empty`].
    pub fn into_value(self) -> Option<(FieldDesc, PvField)> {
        match self {
            RpcReply::Empty => None,
            RpcReply::Value(desc, value) => Some((desc, value)),
        }
    }

    /// True for the no-value reply.
    pub fn is_empty(&self) -> bool {
        matches!(self, RpcReply::Empty)
    }
}

impl From<(FieldDesc, PvField)> for RpcReply {
    fn from((desc, value): (FieldDesc, PvField)) -> Self {
        RpcReply::Value(desc, value)
    }
}
