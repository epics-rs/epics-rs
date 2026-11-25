//! Running one test in a process of its own: a test that changes
//! process-wide state, such as `RLIMIT_NOFILE`, re-executes the test binary
//! so that under `cargo test` no other test runs under that state.

/// Run `body` in a child run of this test binary, and assert there that it
/// passed.
///
/// `test` is the calling test's full path, `module::tests::name`. The child
/// must report that test as `ok`, so a misspelt path, which the child would
/// answer by running nothing, fails instead of passing.
pub(crate) fn with_a_process_of_its_own(test: &str, body: impl FnOnce()) {
    const ROLE: &str = "EPICS_RS_FRESH_PROCESS";
    if std::env::var(ROLE).as_deref() == Ok(test) {
        body();
        return;
    }
    let out = std::process::Command::new(std::env::current_exe().expect("the test binary"))
        .args(["--exact", test, "--nocapture"])
        .env(ROLE, test)
        .output()
        .expect("re-exec the test binary");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains(&format!("test {test} ... ok")),
        "{test} failed in a process of its own: {stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
