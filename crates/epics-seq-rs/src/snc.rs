//! The SNL compiler of the EPICS sequencer (C `src/snc`), ported so an
//! unchanged `.st` program compiles at build time.
//!
//! This module holds the front end: the lexer (`lexer`, C `snl.re`), the
//! grammar (`snl`, C `snl.lem`) and its parser driver ([`parser`]), the
//! syntax tree ([`types`], [`node`]), symbol tables, variable types and the
//! driver with its reporting procedures ([`driver`], C `main.c`), the
//! analysis of the tree ([`analysis`], [`builtin`], [`type_check`]), and
//! the entries that run it: the `snc-rs` command line and [`build`] for
//! build scripts. Code generation builds a [`rust_out`] tree and prints it.

pub mod analysis;
pub mod build;
pub mod builtin;
pub mod cpp;
pub mod driver;
pub mod gen_code;
pub mod gen_ss_code;
pub mod gen_tables;
mod lemon;
mod lexer;
pub mod node;
pub mod parser;
pub mod rust_out;
pub mod seq_prim_types;
mod snl;
pub mod sym_table;
pub mod type_check;
pub mod types;
pub mod var_types;

/// lemon's report on the grammar (the `snl.out` the C build writes), for
/// the reference test that compares the two.
#[doc(hidden)]
pub fn grammar_report() -> String {
    snl::lalr().report()
}
