//! `cargo run -p fixed-wait-gate -- [<base>]`: fails when the working tree
//! adds a fixed wait to a test since the merge base of `<base>` (default
//! `origin/main`) and `HEAD`. CI passes the pull request's base commit.

use std::process::ExitCode;

fn main() -> ExitCode {
    let base = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "origin/main".to_owned());
    match fixed_wait_gate::check(&base) {
        Ok(out) => {
            print!("{}", out.text);
            if out.passed {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(e) => {
            println!("fixed-wait-gate: {e}");
            ExitCode::from(2)
        }
    }
}
