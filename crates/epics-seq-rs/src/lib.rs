//! Rust port of the EPICS sequencer run-time (sequencer R2-2-9 `src/seq`,
//! `src/pv`) that programs compiled by snc-rs run on.
//!
//! An upstream `.st` file is unchanged. A crate's `build.rs` runs the Rust
//! cpp and snc-rs on it and writes `$OUT_DIR/<stem>.rs`, named after the
//! `.st` file as snc names its `.c`. A hand-written companion module
//! includes that file with [`include_program!`] and supplies what the
//! program's escaped C supplied in C.
//!
//! # Execution
//!
//! As in C, each state set runs on its own OS thread, and every call the
//! generated code makes on [`StateSet`] blocks that thread where C's does
//! (a `SYNC` get or put, `delay` in a `when`, the connect wait). Generated
//! functions are plain synchronous Rust, with no `async`.
//!
//! # What snc-rs emits
//!
//! `tests/golden/delayDo.rs` is the reference for std's `delayDo.st`. Its
//! shape, for a program `P`, is:
//!
//! ```rust,ignore
//! mod seqg {
//!     #![allow(non_camel_case_types, non_snake_case, non_upper_case_globals, clippy::all)]
//!     use super::*;
//!     use epics_seq_rs as seqg_rt;
//!     // event flags, `pub struct seqg_vars`, `fn seqg_init`, the
//!     // per-state event/action/entry/exit functions, the tables, then:
//!     pub static P: seqg_rt::SeqProgram<seqg_vars> = ...;
//!     pub fn PRegistrar() { ... }
//! }
//! pub use seqg::{P, PRegistrar};
//! ```
//!
//! The rules that shape:
//!
//! - Names. Every run-time name is written `seqg_rt::...`. Every other
//!   name (a C library function, a companion function, a `%%#define`
//!   constant) is left unqualified. It resolves through `use super::*` to
//!   the companion, whose own items shadow the [`c`] glob. Generated names
//!   start with `seqg_`, as in C. User identifiers keep their SNL spelling,
//!   which is why the lints in the `allow` above are off. SNL statement
//!   shapes come from upstream, so `clippy::all` is off as well.
//! - Tables. They are `static`s. [`SeqChan::var`] is a non-capturing
//!   closure, `|v| &v.x`, which stands in for C's `offsetof`.
//!   [`SeqProgram::validate`] is the check `seq` makes before it starts a
//!   program.
//! - Signatures. The functions match [`EventFn`], [`ActionFn`], [`SsFn`]
//!   and [`InitFn`]. A fixed parameter that is not used gets a `_` prefix,
//!   and `let seqg_var = seqg_env.var();` is emitted only where it is
//!   used. Each statement is preceded by a `// <file>.st:<line>` comment.
//! - Variables. Each variable is a [`SeqVar<T>`] field of `seqg_vars`,
//!   using the [`types`] map, which covers every SNL type: `char`
//!   [`std::ffi::c_char`], `int8_t` `i8`, `unsigned char` and `uint8_t`
//!   `u8`, `short` and `int16_t` `i16`, `unsigned short` and `uint16_t`
//!   `u16`, `int` and `int32_t` `i32`, `unsigned int` and `uint32_t` `u32`,
//!   `long` `i64`, `unsigned long` `u64`, `float` `f32`, `double` `f64`,
//!   `string` [`SeqString`], `char *` [`CharPtr`], `void *`
//!   `Ptr<c_void>`, another `T *` [`Ptr<T>`]. `char` is written
//!   `std::ffi::c_char` in full and the run-time types `seqg_rt::...`, so
//!   a companion imports neither. The outer dimension of a multi-PV
//!   array is split: `[SeqVar<T>; N]`. A block-local variable is a `let`
//!   [`SeqVar`] too, so every variable has the same access path.
//! - Reads and writes. A read is `x.get()` and a write is `x.set(e)`. An
//!   element is `a.at(i)` (`a.at(i).at(j)`), with the subscript emitted
//!   `as usize`; a `string`'s character is `s.at(i)` the same way, a
//!   `c_char`. A compound assignment or increment is
//!   `x.modify(|v| *v += e)`. Every operand is evaluated before a lock is
//!   taken, so `x = x + 1` is a `get` followed by a `set`, and `x += y`
//!   reads `y` first. No code holds two variable locks at once.
//! - Pointers. A [`Ptr`] points into a variable's cell, as C's points into
//!   its memory, so it sees later writes and keeps the cell alive. Where C
//!   converts a value to a pointer type (an assignment, an initializer, a
//!   `return`, an SNL function's argument), `&x` is `x.ptr()`
//!   (`a.at(i).ptr()`) and an array or a `string` decays with
//!   `Ptr::from(&a)`; a call without a prototype passes them as places
//!   (see Calls). `*p` and `p[i]` are places: `p.get()`, `p.set(e)`,
//!   `p.offset(i).get()`. `p + n` is `p.offset(n)`, `p - q` is
//!   `p.diff(&q)`, and `==` compares addresses, an array's or a `string`'s
//!   too, as C's decay makes it. A read or write through null, outside the
//!   variable, or into a literal panics where C's is undefined. A
//!   `void *` is a `Ptr<c_void>`: C's conversion to it is `p.erase()` and
//!   from it `v.cast::<T>()`, which panics unless `T` is the type it was
//!   made from. snc-rs refuses `*v` and arithmetic on it, as ISO C does,
//!   and a cast of an integer to a pointer, which has no cell to point to.
//! - C semantics. A value assigned to, or passed as, a numeric of another
//!   type is emitted `as _`. A C truth value is spelled out as `!= 0` or
//!   `== 0`. Source parentheses and empty statements are dropped.
//! - Literals. A string literal is a `&str`, or `&b"..."[..]` when it is
//!   not UTF-8.
//! - Calls. A run-time call is `seqg_env.<snake_case>(...)`, with every
//!   argument snc fills in written out. A foreign call passes each argument
//!   by C's decay rule, since snc-rs sees no prototype: an array or `&x`
//!   is the place (`&seqg_var.x`); anything else is its value. A variadic
//!   argument is `seqg_rt::c::CArg::from(e)` in a `&[...]` slice. See [`c`].
//!   A run-time call's string parameter (`pvAssign`, `macValueGet`) is a
//!   [`c::CSrc`] and takes its argument by the same rule. A destination is
//!   a [`c::CDst`]. A [`c`] function's `char *` parameter that it returns
//!   or keeps a pointer into (`strchr`, `strstr`, `epicsStrtok_r`) is
//!   passed a [`CharPtr`] value by the Pointers rule, so C's null constant
//!   is `CharPtr::default()`. A foreign call's
//!   result is cast `as _` where it is used (an assignment or `pvPut`
//!   target, an initializer, a typed `seqg_env` parameter), which makes
//!   C's implicit conversion explicit, so a companion function returns its
//!   C prototype's type.
//!
//! # Escaped C and the companion
//!
//! cpp has already expanded the program's macros (`DEBUG_PRINT`,
//! `PVPUTSTR`), so snc-rs sees their expansions. Escaped C cannot be
//! translated, so snc-rs binds each fragment to the companion by its exact
//! text:
//!
//! - At item position (before or after the program, at the start of a
//!   block's definitions): `seqg_escaped!(r#"<text>"#);`.
//! - At statement position: `seqg_escaped!(seqg_env, r#"<text>"#);`.
//!   `seqg_env` is passed so the arm can reach `$env.var()`. Macro hygiene
//!   keeps every other local out of the arm's reach.
//!
//! `<text>` is the fragment's binding key, built from the token C snc's
//! lexer produces: for `%%` the rest of the line after the marker, for
//! `%{ }%` everything between the delimiters. cpp's line-marker lines
//! (`^#\s*\d+\s+"`, such as `# 540 "Io.st"`) are removed, then every run
//! of spaces, tabs and newlines outside C string and character literals
//! and comments becomes one space, or one newline when it follows a `//`
//! comment (so `a // c` newline `b` and `a // c b` are two keys), and the
//! ends are trimmed. Literals and comments stay byte-exact, as cpp leaves
//! them, and the scan that finds them is the bracket count's below. The
//! marker paths and gcc's spacing, which depend on how `build.rs` runs
//! cpp, never reach an arm. The raw string uses the fewest `#` (at least
//! one) that can enclose the key, and a `macro_rules!` arm matches only
//! that exact token.
//!
//! Each `%%` line is a fragment of its own, so C split across lines is
//! bound as one call. A run of statement-position fragments whose `(`,
//! `{` and `[` balance (counted outside C string and character literals
//! and comments) is not zero is one call, which continues until the
//! balance returns to zero and takes the SNL statements in between as
//! block arguments. Fragments that balance only together are one call
//! with several texts:
//!
//! ```rust,ignore
//! // hrCtl.st:572-580: `%%if (hrCtl_LS_TEST) {` SNL `%%} else {` SNL `%%}`
//! seqg_escaped!(seqg_env, r#"if (hrCtl_LS_TEST) {"#, { /* SNL */ },
//!     r#"} else {"#, { /* SNL */ }, r#"}"#);
//! // ml_monoCtl.st:610-611: a `printf` over two `%%` lines
//! seqg_escaped!(
//!     seqg_env,
//!     r#"printf("ml_monoCtl:waitForCmndEnter: ml_monoMoving=%d; ml_monoDone=%d\n","#,
//!     r#"ml_monoMoving, ml_monoDone);"#
//! );
//! ```
//!
//! The companion defines `macro_rules! seqg_escaped` with one arm per
//! distinct fragment, ahead of the [`include_program!`] call, because
//! `macro_rules!` scope is textual:
//!
//! ```rust,ignore
//! use epics_seq_rs::c::*;
//!
//! const MAX_AXIS: i32 = 4; // from `%%#define MAX_AXIS 4`
//!
//! macro_rules! seqg_escaped {
//!     (r#"#define MAX_AXIS 4"#) => {};
//!     ($env:ident, r#"pVar->x = 2*pVar->x;"#) => {
//!         $env.var().x.set($env.var().x.get() * 2)
//!     };
//! }
//!
//! epics_seq_rs::include_program!("kohzuCtl");
//! ```
//!
//! An item arm can expand to the item itself rather than to nothing. A
//! constant, global or function that the escaped C defined, and that SNL
//! code uses, is a companion item with the same name. Generated code names
//! it bare (`powers`), and `mod seqg` reaches it through `use super::*`,
//! so the item sits beside the [`include_program!`] call. A function takes
//! its arguments as the call rule above passes them; under `-r` it reaches
//! the program's variables through `seqg::seqg_static_vars()`.
//!
//! A global is a constant table: a `static`, an array for a C array, read
//! by value. femto.st:42's `%%static int powers[] = {...}` is
//! `static powers: [i32; 16]`, read as `powers[i as usize]`. A buffer
//! that only a callee writes, through C's decay of an array
//! (Keithley2kDMM.st:216's `%%char pvname[...]` passed to `sprintf`), is
//! such a static holding a reference to a companion type that implements
//! [`c::CDst`] and [`c::CSrc`]. snc-rs refuses SNL code that assigns a
//! global.
//!
//! When a fragment has no arm, the build fails with that text, so a
//! changed upstream `.st` cannot compile against a stale companion. A
//! companion holds one program, because each program defines its own
//! `mod seqg`.
//!
//! An SNL function definition is a Rust `fn` that takes the calling state
//! set first, C's implicit `SS_ID`, then its parameters by value, each
//! held in a [`SeqVar`] as a local is; a call converts every argument to
//! its parameter's type as C's prototype does. snc-rs refuses an SNL
//! `struct`, escaped C inside one, and a function's array or `string`
//! parameter, with an error naming the line.
//!
//! # Options: `+r`, `+s` and the rest
//!
//! snc's `OPT_REENT` and `OPT_SAFE` are not bits here. They are the
//! [`VarModel`] of [`SeqProgram::vars`]:
//!
//! - `-r` is [`VarModel::Static`]: one block shared by every start, kept
//!   in a `LazyLock`, as C keeps its statics. snc-rs emits
//!   `pub fn seqg_static_vars() -> &'static seqg_vars` in `mod seqg`
//!   to return it, and the table names that function.
//! - `+r` is [`VarModel::Reentrant`]: a zeroed block for each start.
//! - `+s` is [`VarModel::Safe`]: a block for each start, plus a copy for
//!   each state set. The copy is taken after `seqg_init` and before
//!   connect, in `seq_task.c` order. [`StateSet::var`] returns the state
//!   set's own copy.
//!
//! `+s` without `+r` cannot be expressed, so the implication holds by
//! construction rather than by a check. [`ProgOptions`] carries only
//! `+d`, `+a`, `+c` and `+e`; its constructor refuses any other bit,
//! `OPT_REENT` and `OPT_SAFE` included. `seq_optGet('r'/'s')` reads the
//! [`VarModel`].
//! Generated code is the same text in every model; only `seqg_var`'s
//! source changes.
//!
//! # The pv layer, and epics-base-rs `server::snl`
//!
//! [`pv`] is C's `pv.h` interface over the IOC's CA client context, which
//! is libca with `dbContext` installed. A name this IOC serves gets an
//! in-process channel. Any other name goes out on the network client that
//! epics-ca-rs registers. So remote pvs work, as they do with C's
//! `seq_ca.c`, and a program waits for its pvs to connect just as C's
//! does.
//!
//! `SeqMacros` (`seq_mac.c`), the `seq` banner, thread naming and the PINI
//! gate that `epics_base_rs::server::snl::spawn_program` gave the
//! hand-written ports are this crate's own. The gate is needed because an
//! in-process channel connects at once, whereas C's channel to its own IOC
//! connects only once rsrv is up, after the PINI pass. So `seq` holds the
//! program's first state until the pass is done. No program in this
//! workspace runs on `server::snl` any more.
//!
//! # Locks
//!
//! C has a recursive `sp->lock` per program over its dynamic state, each
//! channel's `dbch` included, and a `varLock` per channel over the shared
//! buffer and its meta data. It takes `varLock` under `sp->lock` (the
//! `seq_ca.c` handlers) and never the reverse. Here, outermost first:
//!
//! 1. The program registry (`seq_cmd.c` `globals.lock`), held across a
//!    traversal's callback.
//! 2. [`pvt::Program::dynamic`]: `sp->lock`. A channel's `DbChan` lives in
//!    it, read and replaced only under it, so there is no channel lock.
//! 3. [`pvt::Chan::var_lock`]: `varLock`, around the meta data and the
//!    channel's variable cells.
//! 4. A [`SeqVar`] cell. Two cells of one channel (the buffer and a `+s`
//!    state set's copy) are held together only under its `var_lock`.
//!
//! These are leaves, under which nothing is taken: an `Sscb`'s `mask`,
//! `get_req`, `put_req`, `meta`, `time_entered` and `wakeup_time`, a
//! `SeqQueue`, a [`pvt::SeqEvent`], and the pv layer's own locks.
//!
//! Where C relocks `sp->lock`, the callee takes the caller's hold instead:
//! `ss_wakeup` takes only the masks, `ef_set_locked` and
//! `ss_read_buffer_selective` take the `ProgDyn`, and the connection
//! handler has a locked form of `seq_camonitor`. No seq lock is held
//! across `PvVar::destroy` or a blocking wait. The pv layer never runs a
//! callback inside a request call, so a caller may hold `dynamic` across
//! one, as C's connection handler holds `sp->lock` across
//! `pvVarMonitorOn`.
//!
//! # Modules
//!
//! One module per C file: `seq.rs` `seq_main.c`, `task.rs` `seq_task.c`,
//! `ca.rs` `seq_ca.c`, `state_set.rs` `seq_if.c`, `queue.rs`
//! `seq_queue.c`, `mac.rs` `seq_mac.c`, `prog.rs` `seq_prog.c`, `cmd.rs`
//! `seq_cmd.c`, `qry.rs` `seq_qry.c`, `pv.rs` `pvCa.c`. [`ChanCell`]
//! carries the value transfer and [`c`] the `printf` family, where a null
//! `%s` prints `(null)`, as glibc does. What one module calls in another
//! is public and reached through [`pvt`].

pub mod c;
mod ca;
mod cmd;
mod mac;
mod prog;
mod program;
pub mod pv;
#[doc(hidden)]
pub mod pvt;
mod qry;
mod queue;
mod seq;
mod state_set;
mod task;
pub mod types;
pub mod var;

pub use cmd::{RegisteredProgram, seq_register_sequencer_commands, seq_register_sequencer_program};
pub use program::*;
pub use qry::{SeqStats, seq_chan_show, seq_gather_stats, seq_queue_show, seq_show, seqcar};
pub use seq::{SeqThreadId, seq};
pub use state_set::{CompleteFlags, StateSet};
pub use task::seq_stop;
pub use types::*;
pub use var::{
    Addr, At, ChanCell, ElemKind, Indexed, Place, Prim, ProgramVars, Ptr, SeqVar, Zeroed,
};

/// Include the program snc-rs wrote for `<name>.st` to `$OUT_DIR/<name>.rs`.
/// The companion's `seqg_escaped!` must be defined before this call.
#[macro_export]
macro_rules! include_program {
    ($name:literal) => {
        include!(concat!(env!("OUT_DIR"), "/", $name, ".rs"));
    };
}
