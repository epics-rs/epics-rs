//! IOC children typed at through their iocsh, for tests that compare an
//! rs IOC's shell output with a softIocPVX capture. Each test binary
//! re-runs itself on its own `ioc_child` test, which calls [`ioc_child`].

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

use named_port::HeldPort;

pub const CHILD: &str = "PVA_IOC_SHELL_CHILD_STCMD";

/// The child half; a no-op unless the parent set [`CHILD`].
pub fn ioc_child() {
    let Ok(stcmd) = std::env::var(CHILD) else {
        return;
    };
    epics_base_rs::runtime::task::test_block_on(epics_bridge_rs::qsrv::run_ca_pva_qsrv_ioc_app(
        epics_base_rs::server::ioc_app::IocApplication::new()
            .port(0)
            .startup_script(&stcmd),
    ))
    .expect("the IOC runs");
}

/// An IOC child on `db` serving PVA on `port`, finding PVs at `servers`.
/// The caller keeps `port` held as long as any IOC may dial it: a number
/// probed and released is any socket's, under load a connect's source port.
///
/// Its stdout and stderr go to `{name}.out` and `{name}.err` in `dir`: a
/// pipe nobody reads blocks the shell once its prompts and report headers
/// fill it, at 8 KiB when the user is past `fs.pipe-user-pages-soft`.
pub fn spawn_ioc(dir: &Path, name: &str, db: &str, port: &HeldPort, servers: &str) -> Child {
    let db_path = dir.join(format!("{name}.db"));
    std::fs::write(&db_path, db).unwrap();
    let stcmd = dir.join(format!("{name}.cmd"));
    std::fs::write(
        &stcmd,
        format!("dbLoadRecords(\"{}\")\niocInit\n", db_path.display()),
    )
    .unwrap();
    let log = |ext: &str| File::create(dir.join(format!("{name}.{ext}"))).unwrap();
    Command::new(std::env::current_exe().unwrap())
        .args(["ioc_child", "--exact", "--nocapture"])
        .env(CHILD, &stcmd)
        .env("EPICS_PVAS_SERVER_PORT", port.port.to_string())
        .env("EPICS_PVAS_BROADCAST_PORT", "0")
        .env("EPICS_PVAS_INTF_ADDR_LIST", "127.0.0.1")
        .env("EPICS_PVAS_AUTO_BEACON_ADDR_LIST", "NO")
        .env("EPICS_PVAS_BEACON_ADDR_LIST", "")
        .env("EPICS_PVA_NAME_SERVERS", servers)
        .env("EPICS_PVA_ADDR_LIST", "")
        .env("EPICS_PVA_AUTO_ADDR_LIST", "NO")
        .env("EPICS_CAS_INTF_ADDR_LIST", "127.0.0.1")
        .stdin(Stdio::piped())
        .stdout(log("out"))
        .stderr(log("err"))
        .spawn()
        .expect("spawn the IOC child")
}

pub fn signal(child: &Child, sig: &str) {
    let status = Command::new("kill")
        .args([sig, &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success(), "kill {sig}");
}

/// An IOC's shell: types a line, and reads back a report.
pub struct Shell<'a> {
    pub stdin: ChildStdin,
    pub dir: &'a Path,
    pub tag: &'a str,
    pub n: usize,
}

impl Shell<'_> {
    pub fn line(&mut self, line: &str) {
        writeln!(self.stdin, "{line}").unwrap();
        self.stdin.flush().unwrap();
    }

    /// `cmd > f`, then `f` once the shell has written it.
    pub fn capture(&mut self, cmd: &str) -> String {
        self.n += 1;
        let f = self.dir.join(format!("{}{}.txt", self.tag, self.n));
        // quoted: outside a quote iocsh takes a Windows path's `\` as an escape
        self.line(&format!("{cmd} > \"{}\"", f.display()));
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(text) = std::fs::read_to_string(&f)
                && text.ends_with('\n')
            {
                return text;
            }
            assert!(Instant::now() < deadline, "{cmd} never ran");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// [`Self::capture`] repeated until `done` holds for it.
    pub fn capture_when(&mut self, cmd: &str, done: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let text = self.capture(cmd);
            if done(&text) || Instant::now() > deadline {
                return text;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// [`Self::capture`] repeated until it ends in `tail`.
    pub fn capture_until(&mut self, cmd: &str, tail: &str) -> String {
        self.capture_when(cmd, |text| text.ends_with(&format!("{tail}\n")))
    }
}
