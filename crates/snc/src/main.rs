use clap::Parser;
use std::path::PathBuf;

use snc_core::analysis::analyze;
use snc_core::codegen::generate;
use snc_core::lexer::Lexer;
use snc_core::parser;
use snc_core::preprocess::preprocess;

#[derive(Parser)]
#[command(name = "snc", about = "SNL to Rust compiler")]
struct Args {
    /// Input .st file
    input: PathBuf,

    /// Output .rs file (default: stdout)
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Dump AST (debug)
    #[arg(long)]
    dump_ast: bool,

    /// Dump IR (debug)
    #[arg(long)]
    dump_ir: bool,
}

fn main() {
    let args = Args::parse();

    let source = match std::fs::read_to_string(&args.input) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: cannot read {}: {e}", args.input.display());
            std::process::exit(1);
        }
    };

    // Preprocess
    let pp = preprocess(&source);
    for warning in &pp.warnings {
        eprintln!("warning: {warning}");
    }
    let source = pp.source;

    // Lex
    let tokens = match Lexer::new(&source).tokenize() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };

    // Parse
    let ast = match parser::Parser::new(tokens).parse_program() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };

    if args.dump_ast {
        eprintln!("{ast:#?}");
        if !args.dump_ir && args.output.is_none() {
            return;
        }
    }

    // Analyze
    let ir = match analyze(&ast) {
        Ok(ir) => ir,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };

    if args.dump_ir {
        eprintln!("{ir:#?}");
        if args.output.is_none() && !args.dump_ast {
            return;
        }
    }

    // Codegen
    let code = generate(&ir);

    match args.output {
        Some(path) => {
            if let Err(e) = std::fs::write(&path, &code) {
                eprintln!("error: cannot write {}: {e}", path.display());
                std::process::exit(1);
            }
            eprintln!("wrote {}", path.display());
        }
        None => {
            print!("{code}");
        }
    }
}
