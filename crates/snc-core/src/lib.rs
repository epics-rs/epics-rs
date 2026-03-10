pub mod analysis;
pub mod ast;
pub mod codegen;
pub mod error;
pub mod ir;
pub mod lexer;
pub mod parser;
pub mod preprocess;

#[cfg(test)]
mod codegen_test;
