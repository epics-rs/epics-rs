//! Extended wire-golden test cases. See `../wire_golden_ext.rs`.
//!
//! New test must:
//! 1. Cite the C reference (`rsrv/camessage.c:NNN` / `libca/cac.cpp:NNN`)
//!    in the docstring.
//! 2. Capture the expected hex bytes from a real `softIoc` /
//!    `caget` run (`tcpdump -X port 5064`) or derive them from
//!    the C source. Don't compute the expected bytes from the
//!    Rust encoder under test — that defeats the golden's purpose.
//! 3. Test name starts with `golden_ext_` for `cargo nextest run -E
//!    'test(/^golden_ext_/)'` discoverability.

pub mod access_rights;
pub mod create_chan_reply;
pub mod event_add_error;
pub mod extended_form;
