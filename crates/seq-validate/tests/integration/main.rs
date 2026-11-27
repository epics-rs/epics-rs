//! The `tests/*.rs` of `seq-validate` that need no process of their own, built
//! as one binary: test `t` of the former binary `X` is `X::t` here, and
//! `binary(integration) & test(/^X::/)` selects what `X` ran.

mod validate;
