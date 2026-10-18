const SOURCES: &[&str] = &[
    "pcre_byte_order.c",
    "pcre_chartables.c",
    "pcre_compile.c",
    "pcre_config.c",
    "pcre_dfa_exec.c",
    "pcre_exec.c",
    "pcre_fullinfo.c",
    "pcre_get.c",
    "pcre_globals.c",
    "pcre_jit_compile.c",
    "pcre_maketables.c",
    "pcre_newline.c",
    "pcre_ord2utf8.c",
    "pcre_refcount.c",
    "pcre_string_utils.c",
    "pcre_study.c",
    "pcre_tables.c",
    "pcre_ucd.c",
    "pcre_valid_utf8.c",
    "pcre_version.c",
    "pcre_xclass.c",
];

fn main() {
    let mut build = cc::Build::new();
    // The options are PSI's EPICS Makefile for PCRE, which StreamDevice's
    // docs/formats.html points to for building it (USR_CPPFLAGS, no
    // config.h); each is also the PCRE configure default.
    build
        .include("pcre")
        .define("HAVE_MEMMOVE", "1")
        // Newline is LF for $, \Z and (?m).
        .define("NEWLINE", "10")
        // PSI defines INT64_MAX so pcre_internal.h picks int64_t; stdint.h
        // gives it the same type portably.
        .define("HAVE_STDINT_H", "1")
        .define("LINK_SIZE", "2")
        .define("MAX_NAME_COUNT", "10000")
        .define("MAX_NAME_SIZE", "32")
        .define("MATCH_LIMIT", "10000000")
        .define("MATCH_LIMIT_RECURSION", "MATCH_LIMIT")
        .define("POSIX_MALLOC_THRESHOLD", "10")
        // Not in PSI's list, which predates it; 8.45's pcre_compile.c needs
        // it, and 250 is its configure default.
        .define("PARENS_NEST_LIMIT", "250")
        // Left undefined, as PSI does: SUPPORT_UTF, SUPPORT_UCP, SUPPORT_JIT,
        // BSR_ANYCRLF, NO_RECURSE, EBCDIC.
        // StreamDevice compiles against PCRE with this (src/Makefile).
        .define("PCRE_STATIC", None)
        .warnings(false);
    for src in SOURCES {
        build.file(format!("pcre/{src}"));
    }
    build.compile("pcre");
    println!("cargo:rerun-if-changed=pcre");
    println!("cargo:include={}/pcre", env!("CARGO_MANIFEST_DIR"));
}
