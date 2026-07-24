//! R18-57: a port built the way an st.cmd builds one — `drvAsynIPPortConfigure`
//! — must come up with a trace configuration and an exception list.
//!
//! In C those are properties of the port itself: `registerPort` calls
//! `tracePvtInit(&pport->dpc.trace)` (asynManager.c:503) and initialises
//! `dpc.exceptionUserList`, which `announceExceptionOccurred` walks
//! (asynManager.c:611-637). There is no way to register a port without them.
//!
//! Before the fix, injection lived only in `PortManager::register_port`, which
//! the `drvAsyn*PortConfigure` iocsh commands bypass: on a port an st.cmd
//! actually builds, `asynSetTraceMask MYPORT -1 …` produced no output and no
//! exception callback ever fired.

use std::io::Read;
use std::sync::{Arc, Mutex};

use asyn_rs::exception::AsynException;
use asyn_rs::iocsh::build_asyn_commands;
use asyn_rs::manager::PortManager;
use asyn_rs::services::PortServices;
use asyn_rs::trace::{TraceFile, TraceManager};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::iocsh::registry::{ArgValue, CommandContext, CommandDef};

fn make_ctx() -> CommandContext {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let bridge = {
        let _guard = rt.enter();
        epics_base_rs::runtime::task::BlockingBridge::capture()
    };
    let ctx = CommandContext::new(Arc::new(PvDatabase::new()), bridge);
    // The context borrows the runtime for the life of the command; the test
    // process exits before the leak matters.
    std::mem::forget(rt);
    ctx
}

fn find<'a>(cmds: &'a [CommandDef], name: &str) -> &'a CommandDef {
    cmds.iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("{name} not registered"))
}

/// The st.cmd sequence:
///
/// ```text
/// drvAsynIPPortConfigure("R1857", "127.0.0.1:<port>", 0, 1, 0)
/// asynSetTraceMask("R1857", -1, 0x11)          # ERROR|FLOW
/// ```
///
/// then a connect. C traces the connect (`asynPrint` ASYN_TRACE_FLOW) and
/// announces `asynExceptionConnect` to the port's exception list. Both must
/// reach the IOC's trace file and exception callbacks.
#[test]
fn iocsh_created_port_traces_and_announces_exceptions() {
    // Take the port by binding, and keep the listener alive so the connect
    // completes.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let host = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
    std::thread::spawn(move || {
        let _accepted = listener.accept();
        std::thread::sleep(std::time::Duration::from_secs(5));
    });

    let mut path = std::env::temp_dir();
    path.push(format!("asyn_r18_57_{}.log", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let file = std::fs::File::create(&path).unwrap();

    let services = PortServices::new(Arc::new(TraceManager::new()));
    let events: Arc<Mutex<Vec<AsynException>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let sink = events.clone();
        services.exceptions().add_callback(move |ev| {
            sink.lock().unwrap().push(ev.exception);
        });
    }

    let mgr = Arc::new(PortManager::with_services(services.clone()));
    let cmds = build_asyn_commands(mgr.clone());
    let ctx = make_ctx();

    // st.cmd line 1 — create the port. `noAutoConnect=1` so the connect edge
    // this test observes is the one it asks for.
    find(&cmds, "drvAsynIPPortConfigure")
        .handler
        .call(
            &[
                ArgValue::String("R1857".into()),
                ArgValue::String(host.clone()),
                ArgValue::Int(0),
                ArgValue::Int(1),
            ],
            &ctx,
        )
        .expect("drvAsynIPPortConfigure failed");

    // Route the trace output somewhere we can read back, then st.cmd line 2.
    services
        .trace()
        .set_trace_file(Some("R1857"), TraceFile::File(Arc::new(Mutex::new(file))));
    find(&cmds, "asynSetTraceMask")
        .handler
        .call(
            &[
                ArgValue::String("R1857".into()),
                ArgValue::Int(-1),
                ArgValue::String("0x11".into()),
            ],
            &ctx,
        )
        .expect("asynSetTraceMask failed");

    let port = asyn_rs::asyn_record::get_port("R1857").expect("port not in registry");
    port.handle.connect_blocking().expect("connect failed");

    let mut log = String::new();
    std::fs::File::open(&path)
        .unwrap()
        .read_to_string(&mut log)
        .unwrap();
    let _ = std::fs::remove_file(&path);

    assert!(
        log.contains("connected to"),
        "an iocsh-created port must honour asynSetTraceMask — C traces the \
         connect at ASYN_TRACE_FLOW (drvAsynIPPort.c:1311); trace file held: {log:?}"
    );

    let evs = events.lock().unwrap();
    assert!(
        evs.iter().any(|e| matches!(e, AsynException::Connect)),
        "an iocsh-created port must announce asynExceptionConnect on the IOC's \
         exception list (asynManager.c:611-637); observed: {evs:?}"
    );
}
