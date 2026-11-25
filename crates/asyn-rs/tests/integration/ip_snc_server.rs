//! testIPServerApp's `ipSNCServer` against a TCP client, as its st.cmd
//! runs it: `drvAsynIPServerPortConfigure(<port>, ..., 1, 0, 0, 0)`,
//! `testIPServer.db`, `iocInit`, then `seq ipSNCServer`. Each line the client
//! sends lands in `stringInput` and is answered `OK`. The expected values are
//! C's (asyn R4-46, sequencer R2-2-9) for the same session.
#![cfg(feature = "epics")]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use asyn_rs::manager::PortManager;
use epics_base_rs::runtime::task::{self, BlockingBridge};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_builder::IocBuilder;
use epics_base_rs::server::iocsh::IocShell;
use epics_base_rs::server::iocsh::registry::CommandOutcome;
use epics_base_rs::server::scan::ScanOwner;
use epics_base_rs::types::EpicsValue;
use named_port::on_a_named_port;

/// A shell over `db` that carries the asyn commands and every registered one.
fn shell(db: Arc<PvDatabase>, bridge: BlockingBridge) -> IocShell {
    let shell = IocShell::new(db, bridge);
    asyn_rs::iocsh::register_asyn_commands_on_shell(&shell, PortManager::global());
    shell
}

fn run(shell: &IocShell, line: &str) {
    match shell.execute_line(line) {
        Ok(CommandOutcome::Continue) => {}
        Ok(_) => panic!("{line}: failed"),
        Err(e) => panic!("{line}: {e}"),
    }
}

/// `line`, run off the runtime's threads as st.cmd is.
async fn sh(db: &Arc<PvDatabase>, line: &str) {
    let (db, bridge, line) = (db.clone(), BlockingBridge::capture(), line.to_owned());
    task::spawn_blocking(move || run(&shell(db, bridge), &line))
        .await
        .unwrap();
}

/// The st.cmd's `drvAsynIPServerPortConfigure`, on a number the kernel had
/// free: its listener port's name and the number it listens on. A lost bind
/// unregisters the port rather than failing the command, so each attempt
/// takes a fresh name.
async fn serve(db: &Arc<PvDatabase>) -> (String, u16) {
    let (db, bridge) = (db.clone(), BlockingBridge::capture());
    task::spawn_blocking(move || {
        let shell = shell(db, bridge);
        let mut spent = 0;
        on_a_named_port(|port| {
            let name = format!("IPSNC{spent}");
            spent += 1;
            run(
                &shell,
                &format!("drvAsynIPServerPortConfigure(\"{name}\",\"127.0.0.1:{port}\",1,0,0,0)"),
            );
            asyn_rs::asyn_record::get_port(&name)
                .is_some()
                .then_some((name, port))
        })
    })
    .await
    .unwrap()
}

struct Ioc {
    db: Arc<PvDatabase>,
    port: u16,
    _scan: ScanOwner,
}

async fn boot() -> Ioc {
    let db = Arc::new(PvDatabase::new());
    let (name, port) = serve(&db).await;
    let macros: HashMap<String, String> = [("P".to_string(), "ipsnc:".to_string())].into();
    let (db, _) = IocBuilder::new()
        .with_database(db)
        .db_file(
            &format!("{}/db/testIPServer.db", env!("CARGO_MANIFEST_DIR")),
            &macros,
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    db.ioc_init().await;
    // iocInit's scanRun and PINI pass: the listener's connection callbacks
    // need interruptAccept, and seq waits for the pass.
    let scan = ScanOwner::start(db.clone());
    asyn_rs::register_test_ip_server_dbd();
    sh(&db, &format!("seq ipSNCServer, \"P=ipsnc:, PORT={name}\"")).await;
    let ioc = Ioc {
        db,
        port,
        _scan: scan,
    };
    // waitConnect's put follows initialize's registerInterruptUser, and the
    // listener announces a connection only to the users it has then
    // (drvAsynIPServerPort.c:388-398).
    assert!(
        ioc.eventually(|ioc| ioc
            .db
            .get_record("ipsnc:connected")
            .unwrap()
            .read()
            .common
            .udf
            == 0)
            .await,
        "waitConnect's put"
    );
    ioc
}

impl Ioc {
    fn input(&self) -> String {
        match self.db.get_pv("ipsnc:stringInput").unwrap() {
            EpicsValue::String(s) => s.as_str_lossy().into_owned(),
            v => panic!("stringInput: {v:?}"),
        }
    }

    fn connected(&self) -> f64 {
        self.db.get_pv("ipsnc:connected").unwrap().to_f64().unwrap()
    }

    /// Poll `probe` for up to ten seconds.
    async fn eventually(&self, mut probe: impl FnMut(&Self) -> bool) -> bool {
        for _ in 0..1000 {
            if probe(self) {
                return true;
            }
            task::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    async fn client(&self) -> Client {
        let stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let client = Client(stream);
        assert!(
            self.eventually(|ioc| ioc.connected() == 1.0).await,
            "Connected"
        );
        client
    }
}

struct Client(TcpStream);

impl Client {
    fn send(&mut self, bytes: &str) {
        self.0.write_all(bytes.as_bytes()).unwrap();
    }

    /// The server's next line, its `\r\n` included.
    async fn reply(&mut self) -> String {
        let mut stream = self.0.try_clone().unwrap();
        task::spawn_blocking(move || {
            let mut line = Vec::new();
            let mut byte = [0];
            while !line.ends_with(b"\r\n") {
                stream.read_exact(&mut byte).unwrap();
                line.push(byte[0]);
            }
            String::from_utf8(line).unwrap()
        })
        .await
        .unwrap()
    }

    /// `line` and its terminator; the server's answer.
    async fn ask(&mut self, line: &str) -> String {
        self.send(&format!("{line}\r\n"));
        self.reply().await
    }
}

#[epics_base_rs::epics_test]
async fn each_client_line_lands_in_the_input_and_is_answered() {
    let ioc = boot().await;
    assert_eq!(ioc.connected(), 0.0, "Disconnected at boot");
    let mut client = ioc.client().await;

    assert_eq!(client.ask("hello").await, "OK\r\n");
    assert!(ioc.eventually(|ioc| ioc.input() == "hello").await);

    // Two lines in one segment are two reads.
    client.send("abc\r\nxyz\r\n");
    assert_eq!(client.reply().await, "OK\r\n");
    assert_eq!(client.reply().await, "OK\r\n");
    assert!(ioc.eventually(|ioc| ioc.input() == "xyz").await);

    let fits = "a".repeat(39);
    assert_eq!(client.ask(&fits).await, "OK\r\n");
    assert!(ioc.eventually(|ioc| ioc.input() == fits).await);

    // C overflows the 40-byte `input` here and glibc aborts the IOC; the
    // port keeps what fits.
    let long = format!("{}{}", "b".repeat(39), "c".repeat(13));
    assert_eq!(client.ask(&long).await, "OK\r\n");
    assert!(ioc.eventually(|ioc| ioc.input() == long[..39]).await);
    assert_eq!(ioc.connected(), 1.0);
}

#[epics_base_rs::epics_test]
async fn a_client_close_disconnects_and_the_next_client_is_served() {
    let ioc = boot().await;
    let mut first = ioc.client().await;
    assert_eq!(first.ask("hello").await, "OK\r\n");
    assert!(ioc.eventually(|ioc| ioc.input() == "hello").await);

    // The close is a successful read of nothing, which C posts; the answer
    // then fails on the closed port, which disconnects.
    first.0.shutdown(Shutdown::Both).unwrap();
    assert!(
        ioc.eventually(|ioc| ioc.connected() == 0.0).await,
        "Disconnected"
    );
    assert!(ioc.eventually(|ioc| ioc.input().is_empty()).await);

    let mut second = ioc.client().await;
    assert_eq!(second.ask("again").await, "OK\r\n");
    assert!(ioc.eventually(|ioc| ioc.input() == "again").await);
    assert_eq!(second.ask("").await, "OK\r\n");
    assert!(ioc.eventually(|ioc| ioc.input().is_empty()).await);
}
