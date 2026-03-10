#[cfg(feature = "cli")]
use clap::Parser;

#[cfg(feature = "cli")]
#[derive(Parser)]
#[command(name = "msi-rs", about = "EPICS Macro Substitution and Include tool")]
struct Cli {
    /// Include search directories
    #[arg(short = 'I', action = clap::ArgAction::Append)]
    include_dirs: Vec<std::path::PathBuf>,

    /// Macro definitions (A=val,B=val2)
    #[arg(short = 'M', action = clap::ArgAction::Append)]
    macros: Vec<String>,

    /// Substitution file
    #[arg(short = 'S')]
    subst_file: Option<std::path::PathBuf>,

    /// Output file (default: stdout)
    #[arg(short = 'o')]
    output: Option<std::path::PathBuf>,

    /// Report undefined macros as errors
    #[arg(short = 'V')]
    strict: bool,

    /// Template file
    template: Option<std::path::PathBuf>,
}

#[cfg(feature = "cli")]
fn main() {
    let cli = Cli::parse();

    let mut mac = msi_rs::MacHandle::new();
    if !cli.strict {
        mac.suppress_warnings(true);
    }

    // Install command-line macros
    for m in &cli.macros {
        let defs = msi_rs::MacHandle::parse_defns(m);
        mac.install_macros(&defs);
    }

    let mut proc = msi_rs::TemplateProcessor::new();
    for dir in &cli.include_dirs {
        proc.add_include_path(dir);
    }

    let mut output = String::new();

    if let Some(subst_path) = &cli.subst_file {
        // Substitution file mode
        let sets = msi_rs::parse_subst_file(subst_path).unwrap_or_else(|e| {
            eprintln!("msi-rs: {}", e);
            std::process::exit(1);
        });

        for set in &sets {
            mac.push_scope();
            let defs = msi_rs::MacHandle::parse_defns(&set.replacements);
            mac.install_macros(&defs);

            // Determine template file
            let tmpl_path = set
                .filename
                .as_deref()
                .map(std::path::PathBuf::from)
                .or_else(|| cli.template.clone());

            if let Some(tmpl) = tmpl_path {
                match proc.process_file(&tmpl, &mut mac) {
                    Ok(result) => output.push_str(&result),
                    Err(e) => {
                        eprintln!("msi-rs: {}", e);
                        std::process::exit(1);
                    }
                }
            } else {
                eprintln!("msi-rs: no template file specified");
                std::process::exit(1);
            }

            mac.pop_scope();
        }
    } else if let Some(tmpl) = &cli.template {
        // Direct template mode
        match proc.process_file(tmpl, &mut mac) {
            Ok(result) => output.push_str(&result),
            Err(e) => {
                eprintln!("msi-rs: {}", e);
                std::process::exit(1);
            }
        }
    } else {
        // Read from stdin
        let content = std::io::read_to_string(std::io::stdin()).unwrap_or_else(|e| {
            eprintln!("msi-rs: failed to read stdin: {}", e);
            std::process::exit(1);
        });
        match proc.process_string(&content, std::path::Path::new("."), &mut mac) {
            Ok(result) => output.push_str(&result),
            Err(e) => {
                eprintln!("msi-rs: {}", e);
                std::process::exit(1);
            }
        }
    }

    if cli.strict && mac.had_warnings() {
        eprintln!("msi-rs: undefined macros encountered");
        std::process::exit(1);
    }

    if let Some(out_path) = &cli.output {
        std::fs::write(out_path, &output).unwrap_or_else(|e| {
            eprintln!("msi-rs: failed to write {}: {}", out_path.display(), e);
            std::process::exit(1);
        });
    } else {
        print!("{}", output);
    }
}

#[cfg(not(feature = "cli"))]
fn main() {
    eprintln!("msi-rs CLI requires the 'cli' feature. Build with: cargo build --features cli");
    std::process::exit(1);
}
