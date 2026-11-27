//! One case per boundary of the rule, then the binary end to end in a scratch
//! repository.

use fixed_wait_gate::{Added, Change, Kind, added, is_test_source, test_code, test_modules, waits};
use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

fn keys(src: &str) -> Vec<String> {
    waits(src).into_iter().map(|w| w.key).collect()
}

fn kinds(src: &str) -> Vec<(String, Kind)> {
    waits(src).into_iter().map(|w| (w.key, w.kind)).collect()
}

#[test]
fn a_sleep_outside_a_loop_is_a_fixed_wait_whatever_its_argument() {
    let src = r#"
fn t() {
    std::thread::sleep(Duration::from_millis(50));
    tokio::time::sleep(SLOW * 6).await;
    rt.sleep(d);
    sleep_until(start + d).await;
}
"#;
    assert_eq!(
        keys(src),
        [
            "sleep(Duration::from_millis(50))",
            "sleep(SLOW*6)",
            "sleep(d)",
            "sleep_until(start+d)",
        ]
    );
    assert_eq!(waits(src)[0].line, 3);
}

#[test]
fn in_a_loop_the_sleep_is_the_poll_interval_and_the_bound_is_checked() {
    let src = r#"
async fn t() {
    let deadline = Instant::now() + FACT_BUDGET;
    loop {
        if done() { break; }
        assert!(Instant::now() < deadline);
        sleep(Duration::from_millis(10)).await;
    }
    while !done() {
        if t0.elapsed() > Duration::from_secs(5) { panic!() }
        std::thread::spawn(|| { sleep(Duration::from_millis(1)) });
    }
    while t0.elapsed() < Duration::from_secs(1) {
        sleep(Duration::from_millis(2));
    }
    for _ in 0..50 {
        if done() { break; }
        sleep(Duration::from_millis(3));
    }
    loop {
        fn helper() { sleep(Duration::from_millis(4)); }
        break;
    }
    assert!(t0.elapsed() >= Duration::from_millis(350));
}
"#;
    assert_eq!(
        keys(src),
        [
            "elapsed()>Duration::from_secs(5)",
            "elapsed()<Duration::from_secs(1)",
            "sleep(Duration::from_millis(3))",
            "sleep(Duration::from_millis(4))",
        ]
    );
}

#[test]
fn a_timeout_on_each_attempt_in_a_loop_is_its_poll_interval() {
    let src = r#"
async fn t() {
    let deadline = Instant::now() + FACT_BUDGET;
    loop {
        if let Ok(Some(_)) = timeout(Duration::from_millis(500), rx.recv()).await { break; }
        assert!(Instant::now() < deadline);
    }
    while t0.elapsed() < Duration::from_secs(2) {
        let _ = sock.recv_timeout(Duration::from_millis(50));
    }
    while let Ok(Some(_)) = timeout(Duration::from_millis(100), rx.recv()).await {}
    timeout(Duration::from_millis(200), rx.recv()).await;
}
"#;
    assert_eq!(
        keys(src),
        [
            "elapsed()<Duration::from_secs(2)",
            "timeout(Duration::from_millis(100))",
            "timeout(Duration::from_millis(200))",
        ]
    );
}

#[test]
fn a_timeout_whose_expiry_ends_its_loop_is_the_loops_bound() {
    let src = r#"
async fn t() -> Result<(), E> {
    loop {
        match timeout(Duration::from_millis(1), rx.recv()).await {
            Ok(v) => seen.push(v),
            Err(_) => break,
        }
    }
    loop {
        match tokio::time::timeout(Duration::from_millis(2), rx.recv()).await {
            Ok(_) => {}
            Err(Elapsed { .. }) => return Ok(()),
        }
    }
    while ok() {
        match rx.recv_timeout(Duration::from_millis(3)) {
            Err(mpsc::RecvTimeoutError::Timeout) => break,
            _ => {}
        }
    }
    loop {
        let Ok(v) = timeout(Duration::from_millis(4), rx.recv()).await else { break };
    }
    loop {
        if timeout(Duration::from_millis(5), rx.recv()).await.is_err() { break; }
    }
    loop {
        timeout(Duration::from_millis(6), rx.recv()).await.expect("a reply");
        if done() { break; }
    }
    loop {
        timeout(Duration::from_millis(7), rx.recv()).await.map_err(E::from)?;
        if done() { break; }
    }
    loop {
        if let Ok(v) = timeout(Duration::from_millis(8), rx.recv()).await { seen.push(v); } else { return Ok(()); }
    }
    loop {
        let r = timeout(Duration::from_millis(9), rx.recv()).await;
        if r.is_err() { break; }
    }
    loop {
        match rx.recv_timeout(Duration::from_millis(10)) {
            Ok(_) => {}
            Err(e) => panic!("no reply: {e}"),
        }
    }
    'outer: loop {
        loop {
            match timeout(Duration::from_millis(11), rx.recv()).await {
                Err(_) => continue 'outer,
                Ok(_) => {}
            }
        }
    }
    loop {
        if !timeout(Duration::from_millis(12), rx.recv()).await.is_ok() { break; }
    }
    loop {
        let v = rx.recv_timeout(Duration::from_millis(13)).unwrap_or_else(|e| panic!("{e}"));
    }

    loop {
        match timeout(Duration::from_millis(101), rx.recv()).await {
            Ok(_) => break,
            Err(_) => continue,
        }
    }
    loop {
        if let Ok(_) = timeout(Duration::from_millis(102), rx.recv()).await { break; }
    }
    loop {
        let _ = timeout(Duration::from_millis(103), rx.recv()).await;
        if done() { break; }
    }
    loop {
        match rx.recv_timeout(Duration::from_millis(104)) {
            Err(RecvTimeoutError::Disconnected) => break,
            _ => {}
        }
    }
    loop {
        let Ok(v) = timeout(Duration::from_millis(105), rx.recv()).await else { continue };
        if v { break; }
    }
    loop {
        if timeout(Duration::from_millis(106), rx.recv()).await.is_ok() { break; }
    }
    loop {
        match timeout(Duration::from_millis(107), rx.recv()).await {
            Err(_) => { for x in xs { if x { break; } } }
            Ok(_) => break,
        }
    }
    loop {
        let f = async { timeout(Duration::from_millis(108), rx.recv()).await?; Ok(()) };
        if done() { break; }
    }
    loop {
        let v = rx.recv_timeout(Duration::from_millis(109)).unwrap_or(0);
        if v > 0 { break; }
    }
    loop {
        sock.set_read_timeout(Some(Duration::from_millis(110))).unwrap();
        if done() { break; }
    }
    loop {
        let (g, _) = cv.wait_timeout(g, Duration::from_millis(111)).unwrap();
        if done() { break; }
    }
}
"#;
    let to = |n: u32| {
        (
            format!("timeout(Duration::from_millis({n}))"),
            Kind::LoopBound,
        )
    };
    let recv = |n: u32| {
        (
            format!("recv_timeout(Duration::from_millis({n}))"),
            Kind::LoopBound,
        )
    };
    assert_eq!(
        kinds(src),
        [
            to(1),
            to(2),
            recv(3),
            to(4),
            to(5),
            to(6),
            to(7),
            to(8),
            to(9),
            recv(10),
            to(11),
            to(12),
            recv(13),
        ]
    );
}

#[test]
fn an_expiry_an_assertion_or_a_helper_in_this_file_fails_on_is_the_loops_bound() {
    let src = r#"
async fn t() -> Result<(), E> {
    loop {
        let r = timeout(Duration::from_millis(1), rx.recv()).await;
        assert!(r.is_ok());
        if done() { break; }
    }
    loop {
        assert!(timeout(Duration::from_millis(2), rx.recv()).await.is_ok(), "no reply");
        if done() { break; }
    }
    loop {
        debug_assert!(!rx.recv_timeout(Duration::from_millis(3)).is_err());
        if done() { break; }
    }
    loop {
        assert!(matches!(timeout(Duration::from_millis(4), rx.recv()).await, Ok(Some(_))));
        if done() { break; }
    }
    loop {
        let r = timeout(Duration::from_millis(5), rx.recv()).await;
        if matches!(r, Err(_)) { break; }
    }
    loop {
        let r = timeout(Duration::from_millis(6), rx.recv()).await;
        expect_reply(r);
        if done() { break; }
    }
    loop {
        check(&mut seen, timeout(Duration::from_millis(7), rx.recv()).await)?;
        if done() { break; }
    }
    loop {
        self.must(timeout(Duration::from_millis(8), rx.recv()).await);
        if done() { break; }
    }
    loop {
        let r = timeout(Duration::from_millis(9), rx.recv()).await;
        let reply = r;
        assert!(reply.is_ok());
        if done() { break; }
    }
    loop {
        assert!(matches!(timeout(Duration::from_millis(10), rx.recv()).await, Ok(v) if v.is_some()));
        if done() { break; }
    }

    loop {
        let r = timeout(Duration::from_millis(101), rx.recv()).await;
        assert!(r.is_err());
        if done() { break; }
    }
    loop {
        assert!(matches!(timeout(Duration::from_millis(102), rx.recv()).await, Ok(_) | Err(_)));
        if done() { break; }
    }
    loop {
        if matches!(timeout(Duration::from_millis(103), rx.recv()).await, Err(e) if e.fatal()) { break; }
    }
    loop {
        tolerate(timeout(Duration::from_millis(104), rx.recv()).await);
        if done() { break; }
    }
    loop {
        other.must(timeout(Duration::from_millis(105), rx.recv()).await);
        if done() { break; }
    }
    loop {
        assert!(timeout(Duration::from_millis(106), rx.recv()).await.is_ok() || late());
        if done() { break; }
    }
    loop {
        check(&mut seen, timeout(Duration::from_millis(107), rx.recv()).await).ok();
        if done() { break; }
    }
    loop {
        remote::expect_reply(timeout(Duration::from_millis(108), rx.recv()).await);
        if done() { break; }
    }
}

fn expect_reply<T>(r: Result<T, Elapsed>) -> T {
    r.expect("a reply in time")
}

fn check<F: Fn() -> u8>(seen: &mut Vec<u8>, r: Result<Option<u8>, Elapsed>) -> Result<(), E> {
    let v = r?;
    seen.push(v);
    Ok(())
}

fn tolerate(r: Result<Option<u8>, Elapsed>) {
    if let Ok(v) = r { drop(v); }
}

impl S {
    fn must(&self, r: Result<Option<u8>, Elapsed>) {
        match r {
            Ok(_) => {}
            Err(_) => panic!("no reply"),
        }
    }
}
"#;
    let to = |n: u32| {
        (
            format!("timeout(Duration::from_millis({n}))"),
            Kind::LoopBound,
        )
    };
    assert_eq!(
        kinds(src),
        [
            to(1),
            to(2),
            (
                "recv_timeout(Duration::from_millis(3))".into(),
                Kind::LoopBound
            ),
            to(4),
            to(5),
            to(6),
            to(7),
            to(8),
            to(9),
            to(10),
        ]
    );
}

#[test]
fn a_select_sleep_arm_is_a_timeout_on_the_other_arms() {
    let src = r#"
async fn t() {
    loop {
        tokio::select! {
            v = rx.recv() => seen.push(v),
            _ = tokio::time::sleep(Duration::from_millis(1)) => break,
        }
    }
    'outer: loop {
        loop {
            select! {
                biased;
                _ = sleep(Duration::from_millis(2)), if armed => continue 'outer,
                v = rx.recv() => seen.push(v),
            }
        }
    }
    while ok() {
        futures::select_biased! {
            v = rx.next() => seen.push(v),
            _ = sleep(Duration::from_millis(3)).fuse() => { panic!("no reply"); }
        }
    }
    tokio::select! {
        _ = sleep(Duration::from_millis(4)) => {}
        v = rx.recv() => panic!("{v:?}"),
    }
    for _ in 0..5 {
        tokio::select! {
            _ = sleep(Duration::from_millis(5)) => return,
            _ = tick.tick() => {}
        }
    }

    loop {
        tokio::select! {
            v = rx.recv() => if v.is_none() { break },
            _ = sleep(Duration::from_millis(101)) => continue,
        }
    }
    loop {
        tokio::select! {
            v = rx.recv() => break,
            _ = sleep(Duration::from_millis(102)) => {}
        }
    }
    loop {
        tokio::select! {
            _ = sleep(FACT_BUDGET) => panic!("no reply"),
            v = rx.recv() => seen.push(v),
        }
    }
    tokio::select! {
        _ = sleep(FACT_BUDGET) => panic!("no reply"),
        v = rx.recv() => seen.push(v),
    }
    loop {
        tokio::select! {
            _ = tick.tick() => { sleep(Duration::from_millis(103)).await; break; }
        }
    }
}
"#;
    let ms = |n: u32| format!("sleep(Duration::from_millis({n}))");
    assert_eq!(
        kinds(src),
        [
            (ms(1), Kind::LoopBound),
            (ms(2), Kind::LoopBound),
            (ms(3), Kind::LoopBound),
            (ms(4), Kind::Fixed),
            (ms(5), Kind::BoundedPoll),
        ]
    );
}

#[test]
fn continue_to_an_outer_loop_leaves_the_inner_one() {
    let src = r#"
async fn t() {
    'outer: for _ in 0..3 {
        for _ in 0..50 {
            if done() { continue 'outer; }
            sleep(Duration::from_millis(1));
        }
    }
    for _ in 0..50 {
        if skip() { continue; }
        sleep(Duration::from_millis(2));
    }
    'own: for _ in 0..50 {
        if skip() { continue 'own; }
        sleep(Duration::from_millis(3));
    }
    'a: loop {
        loop {
            if let Err(_) = timeout(Duration::from_millis(4), rx.recv()).await { continue 'a; }
        }
    }
    'b: loop {
        if let Err(_) = timeout(Duration::from_millis(5), rx.recv()).await { continue 'b; }
        if done() { break; }
    }
}
"#;
    let ms = |n: u32| format!("sleep(Duration::from_millis({n}))");
    assert_eq!(
        kinds(src),
        [
            (ms(1), Kind::BoundedPoll),
            (ms(2), Kind::Pacing),
            (ms(3), Kind::Pacing),
            ("timeout(Duration::from_millis(4))".into(), Kind::LoopBound),
        ]
    );
}

#[test]
fn a_for_loop_that_breaks_is_a_bounded_poll_and_one_that_never_breaks_is_pacing() {
    let src = r#"
async fn t() {
    for _ in 0..50 {
        if done() { break; }
        sleep(Duration::from_millis(1));
    }
    for _ in 0..50 {
        if done() { return; }
        sleep(Duration::from_millis(2));
    }
    for v in [1.0, 2.0] {
        pv.put(v);
        sleep(Duration::from_millis(3));
    }
    for _ in 0..3 {
        loop { if ready() { break; } }
        sleep(Duration::from_millis(4));
    }
    'outer: for _ in 0..50 {
        loop { if done() { break 'outer; } }
        sleep(Duration::from_millis(5));
    }
    for _ in 0..3 {
        std::thread::spawn(move || { return; });
        tokio::spawn(async move { return; });
        let f = |x| { return x; };
        sleep(Duration::from_millis(6));
    }
    for _ in 0..50 {
        if timeout(Duration::from_millis(7), rx.recv()).await.is_ok() { break; }
    }
    for _ in 0..3 {
        timeout(Duration::from_millis(8), rx.recv()).await.unwrap();
    }
    'a: for _ in 0..3 {
        for _ in 0..3 {
            if done() { break 'a; }
            sleep(Duration::from_millis(9));
        }
    }
    for _ in 0..3 {
        'b: { if skip() { break 'b; } }
        sleep(Duration::from_millis(10));
    }
}
"#;
    let ms = |n: u32| format!("sleep(Duration::from_millis({n}))");
    assert_eq!(
        kinds(src),
        [
            (ms(1), Kind::BoundedPoll),
            (ms(2), Kind::BoundedPoll),
            (ms(3), Kind::Pacing),
            (ms(4), Kind::Pacing),
            (ms(5), Kind::BoundedPoll),
            (ms(6), Kind::Pacing),
            (
                "timeout(Duration::from_millis(7))".into(),
                Kind::BoundedPoll
            ),
            ("timeout(Duration::from_millis(8))".into(), Kind::Fixed),
            (ms(9), Kind::BoundedPoll),
            (ms(10), Kind::Pacing),
        ]
    );
}

#[test]
fn a_timeout_is_fixed_only_when_its_duration_is_a_number() {
    let src = r#"
timeout(Duration::from_secs(5), rx.recv()).await;
timeout(FACT_BUDGET, rx.recv()).await;
rx.recv_timeout(remaining);
sock.set_read_timeout(Some(std::time::Duration::from_secs(5)));
wait_until(Duration::from_millis(100 * 50), || done()).await;
until(Duration::from_secs_f64(0.5), || done());
Period::new(Duration::from_secs(60));
poll_interval(Duration::from_millis(10));
rx.recv_timeout(Duration::from_millis(ms));
"#;
    assert_eq!(
        keys(src),
        [
            "timeout(Duration::from_secs(5))",
            "set_read_timeout(Duration::from_secs(5))",
            "wait_until(Duration::from_millis(100*50))",
            "until(Duration::from_secs_f64(0.5))",
        ]
    );
}

#[test]
fn a_deadline_built_from_a_number_is_a_fixed_wait() {
    let src = r#"
let a = Instant::now() + Duration::from_secs(10);
let b = std::time::Instant::now() + FACT_BUDGET;
let c = deadline_after(Instant::now(), Duration::from_secs(10));
let d = time::deadline_from_now(Duration::from_secs(10));
let e = deadline_after(t0, Duration::from_secs(10));
let f = t0 + Duration::from_secs(10);
"#;
    assert_eq!(keys(src), ["now()+Duration::from_secs(10)"; 3]);
}

#[test]
fn a_sum_rewritten_as_its_deadline_helper_adds_no_wait() {
    let base = "fn t() {\n    let a = Instant::now() + Duration::from_secs(5);\n    let b = t0 + Duration::from_secs(5);\n}\n";
    let now = "fn t() {\n    let a = deadline_from_now(Duration::from_secs(5));\n    let b = deadline_after(t0, Duration::from_secs(5));\n}\n";
    assert_eq!(added(&[change("t/tests/a.rs", base, now)]), []);
    let other = now.replace(
        "from_now(Duration::from_secs(5))",
        "from_now(Duration::from_secs(6))",
    );
    assert_eq!(added(&[change("t/tests/a.rs", base, &other)]).len(), 1);
}

#[test]
fn comments_strings_and_char_literals_hold_no_wait() {
    let src = r##"
// sleep(Duration::from_millis(5));
/* outer /* sleep(Duration::from_millis(5)) */ still a comment */
let s = "sleep(Duration::from_millis(5)) \" sleep(x)";
let r = r#"sleep(Duration::from_millis(5)) " sleep(x)"#;
let q = '"'; sleep(one);
fn f<'a>(x: &'a str) { sleep(two); }
let u = "https://host/x"; sleep(three);
"##;
    assert_eq!(keys(src), ["sleep(one)", "sleep(two)", "sleep(three)"]);
}

#[test]
fn a_fn_named_like_a_wait_is_not_a_call() {
    let src = "async fn sleep_ms(ms: u64) {}\nfn wait_timeout(d: Duration) {}\n";
    assert_eq!(keys(src), Vec::<String>::new());
}

#[test]
fn a_block_argument_does_not_make_its_timeout_literal() {
    let src = r#"
timeout(FACT_BUDGET, async {
    sleep(Duration::from_millis(5)).await;
})
"#;
    assert_eq!(keys(src), ["sleep(Duration::from_millis(5))"]);
}

#[test]
fn the_marker_keeps_a_wait_on_its_line_or_in_the_comments_directly_above() {
    let src = r#"
sleep(a); // absence-window: no second monitor may arrive
// absence-window: the put's own echo is the only update
// this circuit may carry before the probe.
sleep(b);
// absence-window:
sleep(c);
// absence-window: separated from its wait by code
let x = 1;
sleep(d);
sleep(e); // a note that is not the marker
"#;
    assert_eq!(keys(src), ["sleep(c)", "sleep(d)", "sleep(e)"]);
}

#[test]
fn not_a_wait_keeps_a_duration_that_waits_for_nothing_but_not_a_poll() {
    let src = r#"
async fn t() {
    let client = PvaClient::builder()
        .timeout(Duration::from_secs_f64(1e19)) // not-a-wait: the unbounded timeout is under test
        .build();
    for v in [1.0, 2.0] {
        pv.put(v);
        // not-a-wait: spaces the puts so each is its own update
        sleep(Duration::from_millis(1));
    }
    for _ in 0..50 {
        if done() { break; }
        sleep(Duration::from_millis(2)); // not-a-wait: it waits for done()
    }
    for _ in 0..50 {
        if seen() { break; }
        sleep(Duration::from_millis(3)); // absence-window: seen() must stay false
    }
    sleep(Duration::from_millis(4)); // not-a-wait:
    pv.put(1.0);
    sleep(Duration::from_millis(5)); // not-a-wait: spaces two puts
    pv.put(2.0);
    loop {
        // not-a-wait: it waits for a reply
        if timeout(Duration::from_millis(6), rx.recv()).await.is_err() { break; }
    }
    loop {
        // absence-window: no reply may arrive
        if timeout(Duration::from_millis(7), rx.recv()).await.is_err() { break; }
    }
}
"#;
    assert_eq!(
        keys(src),
        [
            "sleep(Duration::from_millis(2))",
            "sleep(Duration::from_millis(4))",
            "timeout(Duration::from_millis(6))",
        ]
    );
}

#[test]
fn only_rust_files_under_a_tests_directory_are_test_sources() {
    assert!(is_test_source("crates/a/tests/b.rs"));
    assert!(is_test_source("crates/a/tests/common/mod.rs"));
    assert!(is_test_source("crates/a/src/tests/mod.rs"));
    assert!(!is_test_source("crates/a/src/b.rs"));
    assert!(!is_test_source("crates/a/tests/data.txt"));
    assert!(!is_test_source("crates/contests/b.rs"));
    assert!(!is_test_source("crates/a/src/tests.rs"));
}

/// Under `src/`, a test item is test code and the rest is not: a `cfg`
/// predicate that implies `test`, or a test attribute. The same literal in a
/// production function, under `cfg(not(test))` or under a predicate that also
/// ships (`any(target_os = "rtems", test)`), is production code's business.
#[test]
fn in_src_only_test_items_are_test_code() {
    let src = "\
pub fn ship() {
    sleep(Duration::from_millis(800));
}
#[cfg(not(test))]
fn real() {
    sleep(Duration::from_millis(801));
}
#[cfg(any(target_os = \"rtems\", test))]
fn both() {
    sleep(Duration::from_millis(802));
}
#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn t() {
        sleep(Duration::from_millis(803)).await;
    }
}
#[test]
fn top_level() {
    sleep(Duration::from_millis(804));
}
";
    let none = BTreeSet::new();
    let lines = |path: &str| -> Vec<(usize, String)> {
        waits(&test_code(path, src, &none))
            .into_iter()
            .map(|w| (w.line, w.key))
            .collect()
    };
    assert_eq!(
        lines("crates/x/src/lib.rs"),
        [
            (16, "sleep(Duration::from_millis(803))".to_owned()),
            (21, "sleep(Duration::from_millis(804))".to_owned()),
        ]
    );
    // Under tests/ the whole file is test code.
    assert_eq!(
        waits(&test_code("crates/x/tests/t.rs", src, &none)).len(),
        5
    );
}

/// A file only test code declares is test code throughout: a `mod` in a test
/// item, by name or by `#[path]`, at the top or in an inline test module, and
/// what such a module declares in turn. A `mod` under a predicate that ships,
/// or a file production code also declares, is not one.
#[test]
fn a_module_only_test_code_declares_is_test_code_throughout() {
    let files: BTreeMap<String, String> = [
        (
            "c/src/lib.rs",
            "\
pub mod wire;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod inline {
    mod sub;
}
#[cfg(not(test))]
mod real;
#[cfg(any(target_os = \"rtems\", test))]
mod both;
#[cfg(test)]
mod shared;
mod shared_too {
    #[path = \"../shared.rs\"]
    mod again;
}
",
        ),
        ("c/src/tests.rs", "mod deeper;\n"),
        (
            "c/src/server/blocking.rs",
            "#[cfg(test)]\n#[path = \"send_buf_tests.rs\"]\nmod send_buf_tests;\n",
        ),
        ("c/src/state_set.rs", "#[cfg(test)]\nmod tests;\n"),
    ]
    .into_iter()
    .map(|(p, s)| (p.to_owned(), s.to_owned()))
    .collect();
    let got = test_modules(&files);
    for p in [
        "c/src/tests.rs",
        "c/src/tests/mod.rs",
        "c/src/inline/sub.rs",
        "c/src/tests/deeper.rs",
        "c/src/server/send_buf_tests.rs",
        "c/src/state_set/tests.rs",
    ] {
        assert!(got.contains(p), "{p} is test code: {got:?}");
    }
    for p in [
        "c/src/wire.rs",
        "c/src/real.rs",
        "c/src/both.rs",
        "c/src/shared.rs",
    ] {
        assert!(!got.contains(p), "{p} ships: {got:?}");
    }
}

fn change(path: &str, base: &str, now: &str) -> Change {
    Change {
        path: path.into(),
        base: base.into(),
        now: now.into(),
    }
}

#[test]
fn a_change_adds_only_what_the_merge_base_lacked() {
    let one = "fn t() {\n    sleep(Duration::from_millis(50));\n}\n";
    let reindented = "fn t() {\n    if x {\n        sleep(Duration::from_millis(50));\n    }\n}\n";
    assert_eq!(added(&[change("t/tests/a.rs", one, reindented)]), []);

    let moved = [
        change("t/tests/a.rs", one, ""),
        change("t/tests/common/mod.rs", "", one),
    ];
    assert_eq!(added(&moved), []);

    let retuned = one.replace("50", "500");
    let got = added(&[change("t/tests/a.rs", one, &retuned)]);
    assert_eq!(
        got,
        [Added {
            path: "t/tests/a.rs".into(),
            line: 2,
            key: "sleep(Duration::from_millis(500))".into(),
            kind: Kind::Fixed,
            at_base: 0,
            now: 1,
        }]
    );

    let doubled = format!("{one}{one}");
    let got = added(&[change("t/tests/a.rs", one, &doubled)]);
    assert_eq!(
        got.iter()
            .map(|a| (a.line, a.at_base, a.now))
            .collect::<Vec<_>>(),
        [(2, 1, 2), (5, 1, 2)]
    );

    let marked = one.replace(");", "); // absence-window: nothing may arrive");
    assert_eq!(added(&[change("t/tests/a.rs", one, &marked)]), []);
    assert_eq!(added(&[change("t/tests/a.rs", &marked, one)]).len(), 1);

    // A break turns marked pacing into a poll, which the mark does not keep.
    let pacing = "fn t() {\n    for v in vs {\n        put(v);\n        sleep(Duration::from_millis(50)); // not-a-wait: spaces the puts\n    }\n}\n";
    let polls = pacing.replace("put(v);", "if done() { break; }");
    let got = added(&[change("t/tests/a.rs", pacing, &polls)]);
    assert_eq!(
        got.iter().map(|a| (a.line, a.kind)).collect::<Vec<_>>(),
        [(4, Kind::BoundedPoll)]
    );

    // So does a retry turned into an exit: the same timeout becomes the bound.
    let retries = "fn t() {\n    loop {\n        match rx.recv_timeout(Duration::from_millis(50)) {\n            Err(_) => continue,\n            Ok(_) => {}\n        }\n    }\n}\n";
    let ends = retries.replace("continue", "break");
    assert_eq!(added(&[change("t/tests/a.rs", retries, retries)]), []);
    let got = added(&[change("t/tests/a.rs", retries, &ends)]);
    assert_eq!(
        got.iter().map(|a| (a.line, a.kind)).collect::<Vec<_>>(),
        [(3, Kind::LoopBound)]
    );
}

struct Repo(tempfile::TempDir);

impl Repo {
    fn new() -> Self {
        let repo = Repo(tempfile::tempdir().expect("tempdir"));
        std::fs::write(repo.0.path().join(".gitconfig-empty"), "").unwrap();
        repo.git(&["init", "-q", "-b", "main"]);
        repo.write(".gitignore", ".gitconfig-empty\n");
        repo
    }

    fn cmd(&self, program: &str) -> Command {
        let mut c = Command::new(program);
        c.current_dir(self.0.path())
            .env("GIT_CONFIG_GLOBAL", self.0.path().join(".gitconfig-empty"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "gate")
            .env("GIT_AUTHOR_EMAIL", "gate@example.invalid")
            .env("GIT_COMMITTER_NAME", "gate")
            .env("GIT_COMMITTER_EMAIL", "gate@example.invalid");
        c
    }

    fn git(&self, args: &[&str]) {
        let out = self.cmd("git").args(args).output().expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn write(&self, path: &str, text: &str) {
        let p = self.0.path().join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn commit(&self, msg: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", msg]);
    }

    fn gate(&self) -> (i32, String) {
        let out = self
            .cmd(env!("CARGO_BIN_EXE_fixed-wait-gate"))
            .arg("main")
            .output()
            .expect("gate");
        (
            out.status.code().expect("exit code"),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    }
}

const BASE: &str = "fn t() {\n    sleep(Duration::from_millis(50));\n}\n";
const ADDS: &str =
    "fn t() {\n    sleep(Duration::from_millis(50));\n    sleep(Duration::from_millis(800));\n}\n";
const MARKED: &str = "fn t() {\n    sleep(Duration::from_millis(50));\n    // absence-window: no reply may follow the refused put\n    sleep(Duration::from_millis(800));\n}\n";

#[test]
fn the_gate_fails_an_added_sleep_and_passes_it_marked() {
    let repo = Repo::new();
    repo.write("crates/x/tests/t.rs", BASE);
    repo.write("crates/x/src/lib.rs", "");
    repo.commit("base");
    repo.git(&["checkout", "-q", "-b", "topic"]);
    assert_eq!(repo.gate().0, 0);

    // Uncommitted, as a local run sees it.
    repo.write("crates/x/tests/t.rs", ADDS);
    let (code, out) = repo.gate();
    assert_eq!(code, 1, "{out}");
    assert!(
        out.contains("crates/x/tests/t.rs:3  sleep(Duration::from_millis(800))"),
        "{out}"
    );
    assert!(out.contains("fact_budget::barrier::until_async"), "{out}");

    repo.write("crates/x/tests/t.rs", MARKED);
    let (code, out) = repo.gate();
    assert_eq!(code, 0, "{out}");

    // Committed, as CI sees it.
    repo.write("crates/x/tests/t.rs", ADDS);
    repo.commit("adds a sleep");
    assert_eq!(repo.gate().0, 1);

    // In production code the same sleep is production code's business.
    repo.write("crates/x/tests/t.rs", BASE);
    repo.write("crates/x/src/lib.rs", ADDS);
    repo.commit("moves it to src");
    assert_eq!(repo.gate().0, 0);

    // An untracked new test file counts.
    repo.write("crates/x/tests/new.rs", ADDS);
    let (code, out) = repo.gate();
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("crates/x/tests/new.rs:2"), "{out}");
}

/// End to end under `src/`: a wait added to a test module, a `#[test]`, or
/// a file only a test `mod` declares fails; the same literal added to
/// production code, or to a module production declares, passes.
#[test]
fn the_gate_reads_test_code_under_src_and_leaves_production_alone() {
    const PROD: &str = "pub fn ship() {\n    poll();\n}\n";
    const TESTS: &str = "#[cfg(test)]\nmod tests {\n    fn t() {\n        sleep(Duration::from_millis(50));\n    }\n}\n";
    let repo = Repo::new();
    repo.write("crates/x/src/lib.rs", &format!("{PROD}{TESTS}"));
    repo.commit("base");
    repo.git(&["checkout", "-q", "-b", "topic"]);
    assert_eq!(repo.gate().0, 0);

    let in_prod = "pub fn ship() {\n    sleep(Duration::from_millis(800));\n}\n";
    repo.write("crates/x/src/lib.rs", &format!("{in_prod}{TESTS}"));
    let (code, out) = repo.gate();
    assert_eq!(code, 0, "production code is not the gate's: {out}");

    let in_test = TESTS.replace(
        "    }\n}\n",
        "        sleep(Duration::from_millis(800));\n    }\n}\n",
    );
    repo.write("crates/x/src/lib.rs", &format!("{PROD}{in_test}"));
    let (code, out) = repo.gate();
    assert_eq!(code, 1, "{out}");
    assert!(
        out.contains("crates/x/src/lib.rs:8  sleep(Duration::from_millis(800))"),
        "{out}"
    );

    let test_fn = "#[test]\nfn t2() {\n    sleep(Duration::from_millis(800));\n}\n";
    repo.write("crates/x/src/lib.rs", &format!("{PROD}{TESTS}{test_fn}"));
    let (code, out) = repo.gate();
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("crates/x/src/lib.rs:12"), "{out}");

    // Out of line: the declaring file is unchanged, and still decides.
    let decls = "#[cfg(test)]\n#[path = \"lib_tests.rs\"]\nmod lib_tests;\nmod wire;\n";
    repo.write("crates/x/src/lib.rs", &format!("{PROD}{TESTS}{decls}"));
    repo.write("crates/x/src/lib_tests.rs", "fn helper() {}\n");
    repo.write("crates/x/src/wire.rs", "fn send() {}\n");
    repo.commit("declares a test module and a production one");
    assert_eq!(repo.gate().0, 0);
    repo.write("crates/x/src/wire.rs", in_prod);
    let (code, out) = repo.gate();
    assert_eq!(code, 0, "wire.rs ships: {out}");
    repo.write(
        "crates/x/src/lib_tests.rs",
        "fn helper() {\n    sleep(Duration::from_millis(800));\n}\n",
    );
    let (code, out) = repo.gate();
    assert_eq!(code, 1, "{out}");
    assert!(
        out.contains("crates/x/src/lib_tests.rs:2  sleep(Duration::from_millis(800))"),
        "{out}"
    );
    assert!(!out.contains("wire.rs"), "{out}");
}

#[test]
fn a_base_the_repository_lacks_is_an_error_not_a_pass() {
    let repo = Repo::new();
    repo.write("crates/x/tests/t.rs", BASE);
    repo.commit("base");
    let out = repo
        .cmd(env!("CARGO_BIN_EXE_fixed-wait-gate"))
        .arg("no-such-ref")
        .output()
        .expect("gate");
    assert_eq!(out.status.code(), Some(2));
}
