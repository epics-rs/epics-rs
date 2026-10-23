//! PCRE 8.45 (PCRE1) in Rust, for [`regexp_converter`](crate::regexp_converter),
//! in the one configuration stream-pcre-sys/build.rs builds the C with:
//! 8-bit, no UTF, UCP, JIT or DFA, NEWLINE 10, LINK_SIZE 2. The compiled
//! bytecode is C's byte for byte, so each module ports one C file:
//!
//! | C source                         | module           |
//! |----------------------------------|------------------|
//! | `pcre_internal.h`, `pcre.h`      | [`internal`]     |
//! | `pcre_compile.c`                 | [`compile`]      |
//! | `pcre_chartables.c` (`.dist`)    | [`chartables`]   |
//! | `pcre_tables.c`                  | [`internal`] (`OP_LENGTHS`, `HSPACE_LIST`, `VSPACE_LIST`) |
//! | `pcre_newline.c`                 | [`newline`]      |
//! | `pcre_config.c`                  | [`config`]       |
//! | `pcre_fullinfo.c`                | [`fullinfo`]     |
//! | `pcre_exec.c`                    | [`exec`]         |
//!
//! `pcre_string_utils.c` is empty in the 8-bit library, and
//! `pcre_globals.c` holds only the malloc/free hooks, which `Vec` replaces.

pub mod chartables;
pub mod compile;
pub mod config;
pub mod exec;
pub mod fullinfo;
pub mod internal;
pub mod newline;
