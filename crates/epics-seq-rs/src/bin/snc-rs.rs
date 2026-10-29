//! The SNL compiler's command line, as C snc's: `snc-rs <options> <infile>`
//! reads a preprocessed program and writes the generated code.

fn main() {
    // C takes argv as bytes; a non-UTF-8 argument reaches it lossily.
    let args: Vec<String> = std::env::args_os()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    std::process::exit(epics_seq_rs::snc::driver::main(&args));
}
