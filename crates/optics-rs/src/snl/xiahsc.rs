//! `xiahsc.st`, compiled by snc-rs from `build.rs` and run on
//! `epics-seq-rs`. Its escaped C is three `#include`s, the
//! `validateResponse` prototype and definition (`:203`, `:976-1008`), here
//! in Rust under its C name, and the `communications error` `sprintf` in
//! `idle` (`:621`).

use std::ffi::c_int;

use epics_seq_rs::c::*;

macro_rules! seqg_escaped {
    (r#"#include <math.h>"#) => {};
    (r#"#include <string.h>"#) => {};
    (r#"#include <epicsThread.h>"#) => {};
    (r#"int validateResponse(char *out, char *in);"#) => {};
    // `:976-1008`, below in Rust.
    (r#"int validateResponse(char *out, char *in) { int return_val; int k=1; if (strlen(in) == 0) { return_val = 0; } else { while (out[k] != ' ') { if (out[k] != in[k]) { return_val = 0; break; } else { return_val = 1; k += 1; } } } return return_val; }"#) => {};
    ($env:ident, r#"sprintf (pVar->scratch, "communications error: %d", pVar->s_sevr);"#) => {{
        let v = $env.var();
        epics_seq_rs::c::sprintf(
            &v.scratch,
            "communications error: %d",
            &[epics_seq_rs::c::CArg::from(v.s_sevr.get())],
        );
    }};
}

epics_seq_rs::include_program!("xiahsc");

/// `validateResponse` (`xiahsc.st:984-1016`): the command's ID, its bytes
/// from the second to the first space, against the reply's bytes at the
/// same offsets, a byte past the reply's end reading as its terminator.
#[allow(non_snake_case)]
fn validateResponse(out: impl CSrc, input: impl CSrc) -> c_int {
    let out = out.to_c_vec();
    input.with_c_bytes(|input| {
        if input.is_empty() {
            return 0;
        }
        // C leaves return_val unset for an empty ID (`out[1] == ' '`), which
        // `init`'s ID check keeps from being reached.
        let mut return_val = 0;
        for (k, &c) in out.iter().enumerate().skip(1) {
            if c == b' ' {
                break;
            }
            if input.get(k).copied().unwrap_or(0) != c {
                return 0;
            }
            return_val = 1;
        }
        return_val
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// C `validateResponse` on each row, glibc-run against `"!H-1234 P"`.
    #[test]
    fn validate_response_is_c_prefix_compare() {
        for (reply, valid) in [
            ("%H-1234 500 600 DONE;", 1),
            ("%V-5678 500 600 DONE;", 0),
            ("", 0),
            ("%H-12345 500 600 DONE;", 1),
            ("XH-1234 500 600", 1),
            ("%H-1234;", 1),
            ("%H-1234 OK\r\n", 1),
            (" %H-1234 500 600", 0),
            ("H-1234 500 600", 0),
            ("Z%H-1234", 0),
            ("%H-12", 0),
            ("%h-1234 OK", 0),
        ] {
            assert_eq!(validateResponse("!H-1234 P", reply), valid, "{reply:?}");
        }
    }
}
