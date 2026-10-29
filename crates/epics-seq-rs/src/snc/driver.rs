//! Main program, reporting and printing procedures (C `main.c`).
//!
//! C keeps the options, the output file and the error count in statics and
//! leaves through `exit()`. Here they live in [`Snc`], and every C `exit`
//! returns [`Exit`] up to [`main`].

use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::path::Path;

use epics_libcom_rs::runtime::crt_errno::strerror;
use epics_libcom_rs::runtime::log::StagedStderr;

use crate::snc::types::{Ast, Node, Options};
use crate::snc::{analysis, gen_code, parser};

pub const EXIT_SUCCESS: i32 = 0;
pub const EXIT_FAILURE: i32 = 1;

/// C `SEQ_RELEASE`, in seq_release.pl's format with this crate's version.
pub const SEQ_RELEASE: &str = concat!(
    "Sequencer release ",
    env!("CARGO_PKG_VERSION"),
    ", compiled ",
    env!("EPICS_SNC_COMPILED")
);

/// A C `exit(status)` on its way up to [`main`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Exit(pub i32);

/// The compiler's global state: command-line options, file names, error
/// count and the two output streams.
pub struct Snc {
    pub options: Options,
    /// input file name
    pub input_name: Option<String>,
    /// output file name
    pub output_name: Option<String>,
    err_cnt: u32,
    /// `None` writes diagnostics to stderr, `Some` collects them.
    diag: Option<Vec<u8>>,
    output: Option<Box<dyn Write>>,
    /// First failed write to `output`; C finds it at `fclose`.
    output_error: Option<io::Error>,
}

impl Default for Snc {
    fn default() -> Self {
        Self::new()
    }
}

/// The program, as C `main` runs it; `args[0]` is the program name.
pub fn main(args: &[String]) -> i32 {
    let mut snc = Snc::new();
    snc.run(args).unwrap_or_else(|Exit(status)| status)
}

impl Snc {
    /// Default options, diagnostics to stderr.
    pub fn new() -> Self {
        Snc {
            options: Options::default(),
            input_name: None,
            output_name: None,
            err_cnt: 0,
            diag: None,
            output: None,
            output_error: None,
        }
    }

    /// Like [`Snc::new`], but diagnostics are collected for
    /// [`Snc::diagnostics`] instead of written to stderr.
    pub fn capturing() -> Self {
        Snc {
            diag: Some(Vec::new()),
            ..Self::new()
        }
    }

    /// The diagnostics collected so far by a [`Snc::capturing`] instance.
    pub fn diagnostics(&self) -> &[u8] {
        self.diag.as_deref().unwrap_or_default()
    }

    /// Number of errors reported so far.
    pub fn err_cnt(&self) -> u32 {
        self.err_cnt
    }

    /// Send generated code to `output` instead of the output file.
    pub fn set_output(&mut self, output: Box<dyn Write>) {
        self.output = Some(output);
        self.output_error = None;
    }

    fn run(&mut self, args: &[String]) -> Result<i32, Exit> {
        self.parse_args(args)?;
        let input_name = self.input_name.clone().expect("parse_args sets input_name");
        let input = self.read_input(Path::new(&input_name))?;
        let output_name = self
            .output_name
            .clone()
            .expect("parse_args sets output_name");
        self.translate(&input, &input_name, Path::new(&output_name))
    }

    /// The whole input file, or C main's message for why not.
    pub(crate) fn read_input(&mut self, input_name: &Path) -> Result<Vec<u8>, Exit> {
        let mut input_file = match File::open(input_name) {
            Ok(f) => f,
            Err(e) => {
                self.report(format_args!(
                    "error opening input file: {}: {}\n",
                    input_name.display(),
                    strerror(&e)
                ));
                return Err(Exit(EXIT_FAILURE));
            }
        };
        let mut input = Vec::new();
        if let Err(e) = input_file.read_to_end(&mut input) {
            self.report(format_args!("error reading input: {}\n", strerror(&e)));
            return Err(Exit(EXIT_FAILURE));
        }
        Ok(input)
    }

    /// C `main` from `parse_program` on: parse the preprocessed `input`,
    /// analyse it, and generate code into the file `output_name`, which is
    /// removed again when anything failed.
    pub(crate) fn translate(
        &mut self,
        input: &[u8],
        input_name: &str,
        output_name: &Path,
    ) -> Result<i32, Exit> {
        let mut ast = Ast::new();
        let exp = parser::parse_program(self, &mut ast, input, input_name)?;

        let options = self.options;
        let prg = exp.map(|exp| analysis::analyse_program(self, &mut ast, exp, options));

        // establish precondition for generate_code
        if self.err_cnt > 0 {
            return Ok(EXIT_FAILURE);
        }

        match File::create(output_name) {
            Ok(f) => self.set_output(Box::new(BufWriter::new(f))),
            Err(e) => {
                self.report(format_args!(
                    "error opening output file: {}: {}\n",
                    output_name.display(),
                    strerror(&e)
                ));
                return Ok(EXIT_FAILURE);
            }
        }

        let prg = prg.expect("parse_program returns None only after an error");
        gen_code::generate_code(self, &ast, &prg)?;

        if let Err(e) = self.close_output() {
            self.report(format_args!(
                "error closing output file: {}: {}\n",
                output_name.display(),
                strerror(&e)
            ));
            self.err_cnt += 1;
        }

        if self.err_cnt > 0 {
            if let Err(e) = fs::remove_file(output_name) {
                self.report(format_args!(
                    "error removing partially written output file: {}: {}\n",
                    output_name.display(),
                    strerror(&e)
                ));
            }
            return Ok(EXIT_FAILURE);
        }
        Ok(EXIT_SUCCESS)
    }

    /// Flush and drop the output, returning the first write error.
    fn close_output(&mut self) -> io::Result<()> {
        let flushed = match self.output.take() {
            Some(mut out) => out.flush(),
            None => Ok(()),
        };
        match self.output_error.take() {
            Some(e) => Err(e),
            None => flushed,
        }
    }

    /// Initialize options, input_name, and output_name from arguments.
    pub fn parse_args(&mut self, args: &[String]) -> Result<(), Exit> {
        if args.len() < 2 {
            self.print_usage();
            return Err(Exit(EXIT_FAILURE));
        }

        let mut i = 1;
        while i < args.len() {
            let s = &args[i];
            if s == "-o" {
                if i + 1 == args.len() {
                    self.report(format_args!("missing filename after option -o\n"));
                    self.print_usage();
                    return Err(Exit(EXIT_FAILURE));
                }
                i += 1;
                self.output_name = Some(args[i].clone());
            } else if !matches!(s.as_bytes().first(), Some(b'+' | b'-')) {
                self.input_name = Some(s.clone());
            } else {
                self.parse_option(s);
            }
            i += 1;
        }
        if self.options.safe && !self.options.reent {
            self.options.reent = true;
        }

        let Some(input_name) = &self.input_name else {
            self.report(format_args!("no input file argument given\n"));
            self.print_usage();
            return Err(Exit(EXIT_FAILURE));
        };

        if self.output_name.is_none() {
            // no -o option given; the generated code is Rust, not C
            self.output_name = Some(replace_extension(input_name, ".rs"));
        }
        Ok(())
    }

    fn parse_option(&mut self, s: &str) {
        let opt_val = s.starts_with('+');

        let o = &mut self.options;
        match s.as_bytes().get(1) {
            Some(b'a') => o.async_ = opt_val,
            Some(b'c') => o.conn = opt_val,
            Some(b'd') => o.debug = opt_val,
            Some(b'e') => o.newef = opt_val,
            Some(b'r') => o.reent = opt_val,
            Some(b'l') => o.line = opt_val,
            Some(b'm') => o.main = opt_val,
            Some(b's') => o.safe = opt_val,
            Some(b'w') => o.warn = opt_val,
            Some(b'W') => o.xwarn = opt_val,
            _ => self.report(format_args!("unknown option ignored: '{s}'\n")),
        }
    }

    fn print_usage(&mut self) {
        self.report(format_args!("{SEQ_RELEASE}\n"));
        self.report(format_args!(
            "usage: snc <options> <infile>\n\
             options:\n  \
             -o <outfile> - override name of output file\n  \
             +a           - do asynchronous pvGet\n  \
             -c           - don't wait for all connects\n  \
             +d           - turn on debug run-time option\n  \
             -e           - don't use new event flag mode\n  \
             -l           - suppress line numbering\n  \
             +m           - generate main program\n  \
             -i           - don't register commands/programs\n  \
             +r           - make reentrant at run-time\n  \
             +s           - safe mode (implies +r, overrides -r)\n  \
             -w           - suppress compiler warnings\n  \
             +W           - enable extra compiler warnings\n\
             example:\n snc +a -c vacuum.st\n"
        ));
    }

    // Code generation support

    /// Append `# line <line_num> "<src_file>"` to the output, unless
    /// disabled by the command-line option.
    pub fn gen_line_marker_prim(&mut self, line_num: i32, src_file: Option<&str>) {
        if self.options.line {
            self.gen_code(format_args!(
                "# line {} \"{}\"\n",
                line_num,
                src_file.unwrap_or("(null)")
            ));
        }
    }

    /// C `gen_line_marker(ep)`.
    pub fn gen_line_marker(&mut self, ep: &Node) {
        self.gen_line_marker_prim(ep.token.line, ep.token.file.as_deref());
    }

    pub fn gen_code(&mut self, args: fmt::Arguments<'_>) {
        let out = self
            .output
            .as_mut()
            .expect("gen_code called with no output open");
        if let Err(e) = out.write_fmt(args) {
            self.output_error.get_or_insert(e);
        }
    }

    // Errors and warnings

    fn diag_write(&mut self, args: fmt::Arguments<'_>) {
        // As C ignores fprintf(stderr) failures.
        let _ = match &mut self.diag {
            Some(buf) => buf.write_fmt(args),
            None => StagedStderr.write_fmt(args),
        };
    }

    /// Just the location info.
    pub fn report_loc(&mut self, src_file: Option<&str>, line_num: i32) {
        self.diag_write(format_args!(
            "{}:{}: ",
            src_file.unwrap_or("(null)"),
            line_num
        ));
    }

    /// Location plus message.
    pub fn report_at(&mut self, src_file: Option<&str>, line_num: i32, args: fmt::Arguments<'_>) {
        self.report_loc(src_file, line_num);
        self.diag_write(args);
    }

    /// Location plus message and increase error count.
    pub fn error_at(&mut self, src_file: Option<&str>, line_num: i32, args: fmt::Arguments<'_>) {
        self.err_cnt += 1;
        self.report_loc(src_file, line_num);
        self.diag_write(args);
    }

    /// Location plus message and report a bug in snc.
    pub fn assert_at(
        &mut self,
        cond: bool,
        src_file: Option<&str>,
        line_num: i32,
        args: fmt::Arguments<'_>,
    ) -> Result<(), Exit> {
        if cond {
            return Ok(());
        }
        self.report_loc(src_file, line_num);
        self.diag_write(format_args!("snc bug (assertion failed): "));
        self.diag_write(args);
        Err(Exit(EXIT_FAILURE))
    }

    /// With location from this node.
    pub fn report_at_node(&mut self, ep: &Node, args: fmt::Arguments<'_>) {
        self.report_at(ep.token.file.as_deref(), ep.token.line, args);
    }

    /// With location from this node but only if warnings are enabled.
    pub fn warning_at_node(&mut self, ep: &Node, args: fmt::Arguments<'_>) {
        if !self.options.warn {
            return;
        }
        self.report_loc(ep.token.file.as_deref(), ep.token.line);
        self.diag_write(format_args!("warning: "));
        self.diag_write(args);
    }

    /// With location from this node but only if extra warnings are enabled.
    pub fn extra_warning_at_node(&mut self, ep: &Node, args: fmt::Arguments<'_>) {
        if !self.options.xwarn {
            return;
        }
        self.report_loc(ep.token.file.as_deref(), ep.token.line);
        self.diag_write(format_args!("warning: "));
        self.diag_write(args);
    }

    /// With location from this node and increase error count.
    pub fn error_at_node(&mut self, ep: &Node, args: fmt::Arguments<'_>) {
        self.err_cnt += 1;
        self.report_loc(ep.token.file.as_deref(), ep.token.line);
        self.diag_write(format_args!("error: "));
        self.diag_write(args);
    }

    /// With location from this node and report a bug in snc.
    pub fn assert_at_node(
        &mut self,
        cond: bool,
        ep: &Node,
        args: fmt::Arguments<'_>,
    ) -> Result<(), Exit> {
        self.assert_at(cond, ep.token.file.as_deref(), ep.token.line, args)
    }

    /// Message only.
    pub fn report(&mut self, args: fmt::Arguments<'_>) {
        self.diag_write(args);
    }
}

/// C `replace_extension`: the last `.` anywhere in `in_` starts the
/// extension, directories included.
fn replace_extension(in_: &str, ext: &str) -> String {
    let stem = in_.rfind('.').map_or(in_, |i| &in_[..i]);
    format!("{stem}{ext}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        std::iter::once("snc")
            .chain(a.iter().copied())
            .map(String::from)
            .collect()
    }

    fn text(snc: &Snc) -> &str {
        std::str::from_utf8(snc.diagnostics()).unwrap()
    }

    #[test]
    fn options_and_output_name() {
        let mut snc = Snc::capturing();
        snc.parse_args(&args(&["+s", "-l", "-x", "+W", "dir.d/prog.st"]))
            .unwrap();
        assert!(snc.options.safe && snc.options.reent);
        assert!(!snc.options.line && snc.options.xwarn);
        assert_eq!(snc.output_name.as_deref(), Some("dir.d/prog.rs"));
        assert_eq!(text(&snc), "unknown option ignored: '-x'\n");
    }

    #[test]
    fn replace_extension_takes_last_dot_anywhere() {
        assert_eq!(replace_extension("../x/prog", ".c"), "..c");
        assert_eq!(replace_extension("prog", ".c"), "prog.c");
    }

    #[test]
    fn missing_input_and_missing_o_argument() {
        let mut snc = Snc::capturing();
        assert_eq!(snc.parse_args(&args(&["+a"])), Err(Exit(1)));
        assert!(text(&snc).starts_with("no input file argument given\nSequencer release"));

        let mut snc = Snc::capturing();
        assert_eq!(snc.parse_args(&args(&["x.st", "-o"])), Err(Exit(1)));
        assert!(text(&snc).starts_with("missing filename after option -o\n"));
    }

    #[test]
    fn report_formats() {
        let mut snc = Snc::capturing();
        snc.error_at(Some("f.st"), 3, format_args!("bad\n"));
        snc.report_loc(None, 0);
        assert_eq!(
            snc.assert_at(false, Some("g"), 1, format_args!("x\n")),
            Err(Exit(1))
        );
        assert_eq!(snc.err_cnt(), 1);
        assert_eq!(
            text(&snc),
            "f.st:3: bad\n(null):0: g:1: snc bug (assertion failed): x\n"
        );
    }
}
