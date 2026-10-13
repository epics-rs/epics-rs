//! The real-time switch defaults on, as C's
//! `EPICS_ALLOW_POSIX_THREAD_PRIORITY_SCHEDULING` does (`CONFIG_ENV:57`): a
//! default process asks for SCHED_FIFO, falls back to the default policy where
//! it may not have it, and prints nothing either way (`osdThread.c:276-288`,
//! `:646-670`). Each arm is a child process, because the switch and the probed
//! range are resolved once per process.
//!
//! The switch pin denies SCHED_FIFO itself, so it holds on any host. The FIFO
//! pin needs a process that may have SCHED_FIFO and fails without one; CI
//! runners have none, so `profile.ci` excludes it by name.

#![cfg(all(target_os = "linux", target_env = "gnu"))]

use epics_libcom_rs::runtime::task::{
    C_RT_PRIORITY_ENV, MandatoryThread, RT_PRIORITY_ENV, StackSizeClass, ThreadPriority,
    sched_calls_made,
};

const CHILD: &str = "EPICS_RS_RT_DEFAULT_CHILD";

/// The policy a fresh IOC thread at ScanHigh runs under, as the kernel reports it.
fn spawned_policy() -> &'static str {
    let (tx, rx) = std::sync::mpsc::channel();
    MandatoryThread::new(
        "rt-default",
        ThreadPriority::ScanHigh,
        StackSizeClass::Small,
    )
    .spawn(move || {
        let mut policy = 0;
        let mut param = libc::sched_param { sched_priority: 0 };
        // SAFETY: the calling thread, with stack-local out-params.
        unsafe { libc::pthread_getschedparam(libc::pthread_self(), &mut policy, &mut param) };
        tx.send(policy).unwrap();
    })
    .join()
    .unwrap();
    if rx.recv().unwrap() == libc::SCHED_FIFO {
        "FIFO"
    } else {
        "OTHER"
    }
}

/// Take SCHED_FIFO away from this process: RLIMIT_RTPRIO 0, and out of uid 0,
/// whose CAP_SYS_NICE ignores the limit.
fn drop_rtprio() {
    let limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: a stack-local limit for this process.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_RTPRIO, &limit) }, 0);
    // SAFETY: plain getter; setresuid acts on this process only.
    if unsafe { libc::geteuid() } == 0 {
        assert_eq!(unsafe { libc::setresuid(65534, 65534, 65534) }, 0);
    }
}

fn child(mode: &str) {
    match mode {
        "plain" => println!("policy={}", spawned_policy()),
        // C's `usePolicy == 0`: the probe finds no FIFO permission.
        "rtprio0-before" => {
            drop_rtprio();
            println!("policy={}", spawned_policy());
        }
        // The range was probed settable, then the permission went away: C's
        // pthread_create EPERM retry without SCHED_FIFO.
        "rtprio0-after" => {
            println!("first={}", spawned_policy());
            drop_rtprio();
            println!("policy={}", spawned_policy());
        }
        other => panic!("unknown mode {other}"),
    }
    println!("calls={}", sched_calls_made());
}

/// Re-run `test` as a child in `mode` with the two switches as given; its
/// stdout, after asserting it succeeded and printed nothing on stderr.
fn run(test: &str, mode: &str, c: Option<&str>, alias: Option<&str>) -> String {
    let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", test, "--nocapture"])
        .env(CHILD, mode)
        .env_remove(C_RT_PRIORITY_ENV)
        .env_remove(RT_PRIORITY_ENV);
    if let Some(c) = c {
        cmd.env(C_RT_PRIORITY_ENV, c);
    }
    if let Some(alias) = alias {
        cmd.env(RT_PRIORITY_ENV, alias);
    }
    let out = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "{mode} {c:?} {alias:?}: {stdout}{stderr}"
    );
    assert!(
        stderr.trim().is_empty(),
        "C prints nothing on this path; {mode} {c:?} {alias:?} printed: {stderr}"
    );
    stdout
}

fn has(out: &str, line: &str) -> bool {
    out.lines().any(|l| l == line)
}

/// The probe's one refused call is the trace of a switch that is on.
fn asked(out: &str) -> bool {
    !has(out, "calls=0")
}

#[test]
fn the_switch_defaults_on_and_falls_back_silently() {
    const TEST: &str = "the_switch_defaults_on_and_falls_back_silently";
    if let Ok(mode) = std::env::var(CHILD) {
        return child(&mode);
    }
    let run = |c, alias| {
        let out = run(TEST, "rtprio0-before", c, alias);
        assert!(has(&out, "policy=OTHER"), "{c:?} {alias:?}: {out}");
        out
    };

    let out = run(None, None);
    assert!(asked(&out), "unset switches are on: {out}");
    let out = run(Some("NO"), None);
    assert!(!asked(&out), "C=NO: {out}");
    let out = run(Some("NO"), Some("YES"));
    assert!(!asked(&out), "C's name wins: {out}");
    let out = run(Some("YES"), Some("NO"));
    assert!(asked(&out), "C's name wins: {out}");
    let out = run(None, Some("NO"));
    assert!(!asked(&out), "the alias counts when C's is unset: {out}");
}

/// Excluded from `profile.ci` by name: it fails, not skips, where the process
/// may not have SCHED_FIFO.
#[test]
fn the_default_gets_fifo_where_the_process_may_have_it() {
    const TEST: &str = "the_default_gets_fifo_where_the_process_may_have_it";
    if let Ok(mode) = std::env::var(CHILD) {
        return child(&mode);
    }
    // SAFETY: plain getters.
    let (root, mut rtprio) = (
        unsafe { libc::geteuid() } == 0,
        libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        },
    );
    // SAFETY: a stack-local out-param.
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_RTPRIO, &mut rtprio) },
        0
    );
    assert!(
        root || rtprio.rlim_cur > 0,
        "this pin needs root or a non-zero RLIMIT_RTPRIO; \
         without it the FIFO arms are unverified"
    );

    let out = run(TEST, "plain", None, None);
    assert!(has(&out, "policy=FIFO"), "unset switches are on: {out}");
    let out = run(TEST, "plain", Some("YES"), Some("NO"));
    assert!(has(&out, "policy=FIFO"), "C's name wins: {out}");
    let out = run(TEST, "plain", Some("NO"), None);
    assert!(
        has(&out, "policy=OTHER") && has(&out, "calls=0"),
        "C=NO: {out}"
    );
    let out = run(TEST, "rtprio0-after", None, None);
    assert!(
        has(&out, "first=FIFO") && has(&out, "policy=OTHER"),
        "EPERM after a settable probe: {out}"
    );
}
