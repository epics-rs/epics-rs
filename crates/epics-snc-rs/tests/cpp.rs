//! `cpp::preprocess` on small inputs, against the text `gcc -E` (GCC 13)
//! writes for them after its `# 1 "<main>"` line.

use std::path::Path;

use epics_snc_rs::cpp;

fn run(src: &str) -> cpp::Output {
    cpp::preprocess(Path::new("t.c"), Path::new("t.c"), src.as_bytes(), &[], &[])
}

fn text(src: &str) -> String {
    let out = run(src);
    assert!(!out.failed(), "{:?}", out.diagnostics);
    String::from_utf8(out.text).unwrap()
}

fn diags(src: &str) -> Vec<String> {
    run(src).diagnostics.iter().map(|d| d.to_string()).collect()
}

#[test]
fn prefixed_literals_are_one_token() {
    assert_eq!(
        text("L\"x\" u8\"y\" u\"z\" U\"w\" u8 L Lx L'a' u8'b'\n#define STR(x) #x\nSTR(L\"x\")\n"),
        "# 1 \"t.c\"\nL\"x\" u8\"y\" u\"z\" U\"w\" u8 L Lx L'a' u8'b'\n\n\"L\\\"x\\\"\"\n"
    );
}

#[test]
fn pragma_once_leaves_a_line_indented_to_its_name() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("m.c");
    let o = dir.path().join("o.h");
    std::fs::write(&o, "#  pragma   once\nint o2;\n").unwrap();
    let src = "x\n#include \"o.h\"\n  #pragma once\ny\n";
    let out = cpp::preprocess(&main, &main, src.as_bytes(), &[], &[]);
    // A marker escapes the `\` of a Windows path.
    let marker = |p: &Path| p.display().to_string().replace('\\', "\\\\");
    let (m, o) = (marker(&main), marker(&o));
    assert_eq!(
        String::from_utf8(out.text).unwrap(),
        format!(
            "# 1 \"{m}\"\nx\n# 1 \"{o}\" 1\n           \nint o2;\n\
             # 3 \"{m}\" 2\n         \ny\n"
        )
    );
    let diags: Vec<_> = out.diagnostics.iter().map(|d| d.to_string()).collect();
    assert_eq!(
        diags,
        [format!(
            "{}:3:11: warning: #pragma once in main file",
            main.display()
        )]
    );
}

#[cfg(unix)]
#[test]
fn an_unreadable_include_is_fatal_with_strerror_text() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let h = dir.path().join("h.h");
    std::fs::write(&h, "x\n").unwrap();
    std::fs::set_permissions(&h, std::fs::Permissions::from_mode(0o000)).unwrap();
    assert!(std::fs::read(&h).is_err(), "mode 0 is readable to root");
    let main = dir.path().join("t.c");
    let out = cpp::preprocess(&main, &main, b"#include \"h.h\"\n", &[], &[]);
    let diags: Vec<_> = out.diagnostics.iter().map(|d| d.to_string()).collect();
    assert_eq!(
        diags,
        [format!(
            "{}:1:10: fatal error: {}: Permission denied",
            main.display(),
            h.display()
        )]
    );
}

#[test]
fn argument_errors_point_where_cpplib_stopped_reading() {
    assert_eq!(
        diags("#define F(x) x\nF(1,\n2)\n"),
        [
            "t.c:3:2: error: macro \"F\" passed 2 arguments, but takes just 1",
            "t.c:1: note: macro \"F\" defined here",
        ]
    );
    assert_eq!(
        diags("#define F(x) x\nF(1,\n  2"),
        ["t.c:3:4: error: unterminated argument list invoking macro \"F\""]
    );
}

#[test]
fn a_space_separates_tokens_that_would_lex_as_one() {
    assert_eq!(
        text("#define P +\nP+ +P a P+P\n"),
        "# 1 \"t.c\"\n\n+ + + + a + + +\n"
    );
    assert_eq!(
        text("x\"s\" L\"w\" y'c'\n#define N x\nN\"s\" N'c'\n"),
        "# 1 \"t.c\"\nx\"s\" L\"w\" y'c'\n\nx \"s\" x 'c'\n"
    );
    // A failed paste leaves both operands; `/` `/` would lex as a comment.
    let out = run("#define C(a,b) a##b\nC(/,=) C(/,/)\n");
    assert_eq!(
        String::from_utf8(out.text).unwrap(),
        "# 1 \"t.c\"\n\n/= / /\n"
    );
}

#[test]
fn stringify_drops_the_argument_s_edge_whitespace() {
    let out = run("#define S(x) #x\nS( a b ) S(  a) S(a  ,) \n");
    assert_eq!(
        String::from_utf8(out.text).unwrap(),
        "# 1 \"t.c\"\n\n\"a b\" \"a\" S\n"
    );
    let diags: Vec<_> = out.diagnostics.iter().map(|d| d.to_string()).collect();
    assert_eq!(
        diags,
        [
            "t.c:2:23: error: macro \"S\" passed 2 arguments, but takes just 1",
            "t.c:1: note: macro \"S\" defined here",
        ]
    );
}

#[test]
fn arguments_expand_before_substitution_except_under_stringify() {
    assert_eq!(
        text("#define S(x) #x\n#define X(x) S(x)\n#define V 42\nX(V) S(V)\n"),
        "# 1 \"t.c\"\n\n\n\n\"42\" \"V\"\n"
    );
}

#[test]
fn counter_starts_at_zero() {
    assert_eq!(text("__COUNTER__ __COUNTER__\n"), "# 1 \"t.c\"\n0 1\n");
}

#[test]
fn if_follows_c_unsigned_promotion_and_zero_for_names() {
    assert_eq!(
        text(
            "#if (1 ? -1 : 0u) > 0\nyes\n#else\nno\n#endif\n\
             #if (0 ? 0u : -1) > 0\nyes2\n#endif\n"
        ),
        "# 1 \"t.c\"\n\nyes\n\n\n\n\nyes2\n"
    );
    assert_eq!(
        text("#if -1 < 0u\nyes\n#else\nno\n#endif\n"),
        "# 1 \"t.c\"\n\n\n\nno\n"
    );
    assert_eq!(
        text(
            "#if undefined_name\nyes\n#else\nno\n#endif\n\
             #if !undefined_name && defined undefined_name == 0\nzero\n#endif\n"
        ),
        "# 1 \"t.c\"\n\n\n\nno\n\n\nzero\n"
    );
}

#[test]
fn splices_warn_at_the_backslash_once_the_lexer_reaches_it() {
    assert_eq!(text("a \\  \nb\n"), "# 1 \"t.c\"\na\n b\n");
    assert_eq!(
        diags("a \\  \nb\n"),
        ["t.c:1:3: warning: backslash and newline separated by space"]
    );
    // The first splice of a last line the end of the file ends takes the
    // end-of-file warning, in a comment too; a spliced space is diagnosed
    // in a string but not in a comment.
    let src = "x\n/* \\ \n*/ \"s\\ \n\" y \\\n";
    assert_eq!(text(src), "# 1 \"t.c\"\nx\n\n   \"s\"\n  y\n");
    assert_eq!(
        diags(src),
        [
            "t.c:2:4: warning: backslash-newline at end of file",
            "t.c:3:6: warning: backslash and newline separated by space",
        ]
    );
}

#[test]
fn a_backslash_the_end_of_the_file_follows_is_kept() {
    assert_eq!(text("q \\  "), "# 1 \"t.c\"\nq \\\n");
    assert!(diags("q \\  ").is_empty());
}

#[test]
fn trigraphs_stay_and_warn_outside_comments_or_before_a_newline() {
    let src = "a ??= b /* ??( */ c\n\"??/\" d\n// ??/  \n/* ??/ x */\n";
    assert_eq!(text(src), "# 1 \"t.c\"\na ??= b c\n\"??/\" d\n");
    assert_eq!(
        diags(src),
        [
            "t.c:1:3: warning: trigraph ??= ignored, use -trigraphs to enable [-Wtrigraphs]",
            "t.c:2:2: warning: trigraph ??/ ignored, use -trigraphs to enable [-Wtrigraphs]",
            "t.c:3:4: warning: trigraph ??/ ignored, use -trigraphs to enable [-Wtrigraphs]",
        ]
    );
}

#[test]
fn null_characters_warn_once_per_blank_run_or_literal() {
    assert_eq!(
        diags("a \0 \0\0 bc\n \0/**/\0 b\n\"\0\n"),
        [
            "t.c:1:2: warning: null character(s) ignored",
            "t.c:2:1: warning: null character(s) ignored",
            "t.c:2:7: warning: null character(s) ignored",
            "t.c:3:1: warning: null character(s) preserved in literal",
            "t.c:3:1: warning: missing terminating \" character",
        ]
    );
    assert!(diags("\"\\\0\" a\n#if 0\n\"\0\"\n#endif\n").is_empty());
}

#[test]
fn line_numbers_wrap_past_u32_max_as_gcc_linenum_type() {
    assert_eq!(
        text("#line 4294967295\n__LINE__\n__LINE__\n#line 4294967294\na\n\n\n\n\n\n\n\n\n\nb\n"),
        "# 1 \"t.c\"\n# 4294967295 \"t.c\"\n# 4294967295 \"t.c\"\n4294967295\n0\n\
         # 4294967294 \"t.c\"\n# 4294967294 \"t.c\"\na\n# 8 \"t.c\"\nb\n"
    );
    assert_eq!(
        diags("#line 4294967295\na b\n#if 1/0\n#endif\n#warning w\n"),
        [
            "t.c: error: division by zero in #if",
            "t.c:2:2: warning: #warning w [-Wcpp]",
        ]
    );
}

#[test]
fn columns_are_display_columns_of_the_file_a_location_names() {
    // A tab reaches the next multiple of 8, é counts 1, 中 2 and a
    // combining accent 0; a #line name no file was read under keeps the
    // byte column, as gcc does when it cannot open that name.
    assert_eq!(
        diags(
            "\u{feff}\t#warning x\n/* é中\u{301} */ #warning y\n\
             #line 5 \"other.c\"\n\t#warning z\n"
        ),
        [
            "t.c:1:10: warning: #warning x [-Wcpp]",
            "t.c:2:12: warning: #warning y [-Wcpp]",
            "other.c:5:3: warning: #warning z [-Wcpp]",
        ]
    );
    // An overlong form, a surrogate and a five-byte form: 2 + 3 + 1.
    let src = b"/*\xc0\x80\xed\xa0\x80\xf8\x88\x80\x80\x80*/ \"\n";
    let out = cpp::preprocess(Path::new("t.c"), Path::new("t.c"), src, &[], &[]);
    assert_eq!(
        out.diagnostics[0].to_string(),
        "t.c:1:12: warning: missing terminating \" character"
    );
}
