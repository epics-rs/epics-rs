//! Compiles every `src/snl/*.st` with snc-rs to `$OUT_DIR/<stem>.rs`, which
//! the companion calling `include_program!("<stem>")` includes.
fn main() {
    epics_snc_rs::build::compile_dir("src/snl");
}
