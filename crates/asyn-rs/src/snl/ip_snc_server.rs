//! `ipSNCServer.st` (asyn's testIPServerApp), compiled by snc-rs from
//! `build.rs` and run on `epics-seq-rs`. Its escaped C is the asyn glue in
//! the `%{ }%` block: `initialize` registers for the listener port's
//! connection callback, which names each client's port, and `readSocket` /
//! `writeSocket` move one line through it.
//!
//! The block keeps its asynUser, event and registration in the `char *`
//! variables `pasynUser`, `eventId` and `registrarPvt`. Here they are a
//! [`Server`] owned by the program's one state set, on its thread.

use std::cell::RefCell;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use epics_seq_rs::CharPtr;
use epics_seq_rs::c::*;

use crate::interfaces::InterfaceType;
use crate::interrupt::{InterruptFilter, SyncCallbackSubscription};
use crate::param::ParamValue;
use crate::sync_io::SyncIOHandle;
use crate::trace::TraceMask;

macro_rules! seqg_escaped {
    (r#"#include <string.h>"#) => {};
    (r#"#include <epicsString.h>"#) => {};
    (r#"#include <epicsEvent.h>"#) => {};
    (r#"#include <asynDriver.h>"#) => {};
    (r#"#include <asynOctet.h>"#) => {};
    (r#"#include <asynOctetSyncIO.h>"#) => {};
    (r#"static void initialize(SS_ID ssId, struct UserVar *pVar);"#) => {};
    (r#"static int readSocket(SS_ID ssId, struct UserVar *pVar);"#) => {};
    (r#"static int writeSocket(SS_ID ssId, struct UserVar *pVar);"#) => {};
    (r#"static void connectionCallback(void *drvPvt, asynUser *pasynUserIn, char *portName, size_t len, int eomReason) { struct UserVar *pVar = (struct UserVar *)drvPvt; asynUser *pasynUser; asynStatus status; pVar->IOPortName = epicsStrDup(portName); status = pasynOctetSyncIO->connect(portName, 0, &pasynUser, NULL); pVar->pasynUser = (char *)pasynUser; asynPrint(pasynUser, ASYN_TRACE_FLOW, "ipSNCServer: connectionCallback, portName=%s\n", portName); if (status) { asynPrint(pasynUser, ASYN_TRACE_ERROR, "ipSNCServer:connectionCallback: unable to connect to port %s\n", portName); return; } status = pasynOctetSyncIO->setInputEos(pasynUser, "\r\n", 2); if (status) { asynPrint(pasynUser, ASYN_TRACE_ERROR, "ipSNCServer:connectionCallback: unable to set input EOS on %s: %s\n", portName, pasynUser->errorMessage); return; } status = pasynOctetSyncIO->setOutputEos(pasynUser, "\r\n", 2); if (status) { asynPrint(pasynUser, ASYN_TRACE_ERROR, "ipSNCServer:connectionCallback: unable to set output EOS on %s: %s\n", portName, pasynUser->errorMessage); return; } pVar->connected = 1; epicsEventSignal( (epicsEventId)pVar->eventId); } static void initialize(SS_ID ssId, struct UserVar *pVar) { int addr=0; asynInterface *pasynInterface; asynUser *pasynUser; asynOctet *pasynOctet; void *registrarPvt; int status; pVar->eventId = (char *)epicsEventCreate(epicsEventEmpty); pasynUser = pasynManager->createAsynUser(0,0); pVar->pasynUser = (char *)pasynUser; pasynUser->userPvt = pVar; status = pasynManager->connectDevice(pasynUser, pVar->listenerPortName, addr); if(status!=asynSuccess) { printf("can't connect to port %s: %s\n", pVar->listenerPortName, pasynUser->errorMessage); return; } pasynInterface = pasynManager->findInterface(pasynUser,asynOctetType,1); if(!pasynInterface) { printf("%s driver not supported\n",asynOctetType); return; } pasynOctet = (asynOctet *)pasynInterface->pinterface; status = pasynOctet->registerInterruptUser( pasynInterface->drvPvt, pasynUser, connectionCallback,pVar, &registrarPvt); pVar->registrarPvt = registrarPvt; if(status!=asynSuccess) { printf("ipSNCServer devAsynOctet registerInterruptUser %s\n", pasynUser->errorMessage); } } static int readSocket(SS_ID ssId, struct UserVar *pVar) { char buffer[80]; size_t nread; int eomReason; asynUser *pasynUser = (asynUser *)pVar->pasynUser; asynStatus status; status = pasynOctetSyncIO->read(pasynUser, buffer, 80, -1.0, &nread, &eomReason); if (status) { asynPrint(pasynUser, ASYN_TRACE_ERROR, "ipSNCServer:readSocket: read error on: %s: %s\n", pVar->IOPortName, pasynUser->errorMessage); pVar->connected = 0; strcpy(pVar->input, ""); } else { asynPrint(pasynUser, ASYN_TRACEIO_DEVICE, "ipSNCServer:readSocket: %s read %s\n", pVar->IOPortName, buffer); strcpy(pVar->input, buffer); } return(status); } static int writeSocket(SS_ID ssId, struct UserVar *pVar) { size_t nwrite; asynUser *pasynUser = (asynUser *)pVar->pasynUser; asynStatus status; status = pasynOctetSyncIO->write(pasynUser, pVar->output, strlen(pVar->output), 0.0, &nwrite); if (status) { asynPrint(pasynUser, ASYN_TRACE_ERROR, "ipSNCServer:writeSocket: write error on: %s: %s\n", pVar->IOPortName, pasynUser->errorMessage); pVar->connected = 0; } else { asynPrint(pasynUser, ASYN_TRACEIO_DEVICE, "ipSNCServer:writeSocket: %s write %s\n", pVar->IOPortName, pVar->output); } return(status); }"#) => {};
    ($env:ident, r#"initialize(ssId, pVar);"#) => {
        initialize($env.var())
    };
    ($env:ident, r#"epicsEventWait( (epicsEventId)pVar->eventId);"#) => {
        wait_connect($env.var())
    };
    ($env:ident, r#"pVar->readStatus = readSocket(ssId, pVar);"#) => {
        $env.var().readStatus.set(read_socket($env.var()))
    };
    ($env:ident, r#"pVar->writeStatus = writeSocket(ssId, pVar);"#) => {
        $env.var().writeStatus.set(write_socket($env.var()))
    };
}

epics_seq_rs::include_program!("ipSNCServer");

use seqg::seqg_vars;

/// `BUFFER_SIZE`, which cpp has expanded in the block.
const BUFFER_SIZE: usize = 80;

/// The child port each connection callback names, until the state set's
/// wait takes it: C's `eventId`, a binary event the callback signals.
#[derive(Default)]
struct Connections {
    named: Mutex<Option<String>>,
    signal: Condvar,
}

impl Connections {
    fn name(&self, port: String) {
        *self.named.lock().unwrap() = Some(port);
        self.signal.notify_one();
    }

    fn wait(&self) -> String {
        let named = self.named.lock().unwrap();
        let mut named = self.signal.wait_while(named, |n| n.is_none()).unwrap();
        named.take().unwrap()
    }
}

struct Server {
    connections: Arc<Connections>,
    _registration: Option<SyncCallbackSubscription>,
    /// The client's SyncIO user, set by each connect.
    client: Option<SyncIOHandle>,
}

thread_local! {
    /// Each running copy of the program has one state set, so one thread.
    static SERVER: RefCell<Option<Server>> = const { RefCell::new(None) };
}

/// `initialize`: the listener port's octet callback, registered as C's
/// `registerInterruptUser` on `connectDevice(PORT, 0)`. The wait in
/// `waitConnect` outlives any failure here, as C's event does.
fn initialize(v: &seqg_vars) {
    let connections = Arc::<Connections>::default();
    let listener = string(&v.listenerPortName.get());
    let registration = register(&listener, &connections);
    SERVER.set(Some(Server {
        connections,
        _registration: registration,
        client: None,
    }));
}

fn register(listener: &str, connections: &Arc<Connections>) -> Option<SyncCallbackSubscription> {
    let Some(entry) = crate::registry::get_port(listener) else {
        printf(
            "can't connect to port %s: %s\n",
            &[
                CArg::from(listener),
                CArg::from(format!("asynManager:connectDevice port {listener} not found").as_str()),
            ],
        );
        return None;
    };
    if !entry.handle.has_interface(InterfaceType::Octet) {
        printf("%s driver not supported\n", &[CArg::from("asynOctet")]);
        return None;
    }
    let connections = Arc::clone(connections);
    let filter = InterruptFilter {
        iface: Some(InterfaceType::Octet),
        ..Default::default()
    };
    let named = move |value: &crate::interrupt::InterruptValue| {
        if let ParamValue::Octet(port) = &value.value {
            connections.name(String::from_utf8_lossy(port).into_owned());
        }
    };
    match entry
        .handle
        .interrupts()
        .register_sync_callback(filter, named)
    {
        Ok(registration) => Some(registration),
        Err(e) => {
            printf(
                "ipSNCServer devAsynOctet registerInterruptUser %s\n",
                &[CArg::from(e.message().as_str())],
            );
            None
        }
    }
}

/// `epicsEventWait`, then the rest of C's `connectionCallback`, which ran
/// on the listener's thread before it signalled.
///
/// C defect fixed: the callback replaced `pVar->pasynUser` and set
/// `pVar->connected` from the listener's thread while the state set could
/// be reading or writing through the old user, and leaked the old one.
/// Here the client changes only on the state set's own thread, in
/// `waitConnect`, and the old user is dropped.
fn wait_connect(v: &seqg_vars) {
    SERVER.with_borrow_mut(|server| {
        let server = server.as_mut().expect("init ran initialize");
        loop {
            let port = server.connections.wait();
            if let Some(client) = connect(v, &port) {
                server.client = Some(client);
                v.connected.set(1);
                return;
            }
        }
    })
}

/// `connectionCallback` from its SyncIO connect on: the client's user, with
/// the protocol's `\r\n` both ways, or `None` on C's early returns.
fn connect(v: &seqg_vars, port: &str) -> Option<SyncIOHandle> {
    v.IOPortName.set(CharPtr::from(port));
    let (client, status) =
        SyncIOHandle::connect_port(port, 0, Duration::ZERO, InterfaceType::Octet);
    client.print(
        0,
        TraceMask::FLOW,
        format_args!("ipSNCServer: connectionCallback, portName={port}"),
    );
    if status.is_err() {
        client.print(
            0,
            TraceMask::ERROR,
            format_args!("ipSNCServer:connectionCallback: unable to connect to port {port}"),
        );
        return None;
    }
    if let Err(e) = client.set_input_eos(0, b"\r\n") {
        client.print(
            0,
            TraceMask::ERROR,
            format_args!(
                "ipSNCServer:connectionCallback: unable to set input EOS on {port}: {}",
                e.message_display()
            ),
        );
        return None;
    }
    if let Err(e) = client.set_output_eos(0, b"\r\n") {
        client.print(
            0,
            TraceMask::ERROR,
            format_args!(
                "ipSNCServer:connectionCallback: unable to set output EOS on {port}: {}",
                e.message_display()
            ),
        );
        return None;
    }
    Some(client)
}

/// A `char *` variable's string, empty for NULL.
fn string(p: &CharPtr) -> String {
    if p.is_null() {
        String::new()
    } else {
        String::from_utf8_lossy(&p.to_c_vec()).into_owned()
    }
}

/// `pVar->IOPortName`, for the trace lines.
fn io_port_name(v: &seqg_vars) -> String {
    string(&v.IOPortName.get())
}

/// `readSocket`: one line, waiting as long as it takes, into `input`.
fn read_socket(v: &seqg_vars) -> i32 {
    SERVER.with_borrow(|server| {
        let client = server
            .as_ref()
            .and_then(|s| s.client.as_ref())
            .expect("processCommands follows a connect");
        let (line, status) = client
            .waiting_forever()
            .read_octet(0, BUFFER_SIZE)
            .into_parts();
        match status {
            Err(e) => {
                client.print(
                    0,
                    TraceMask::ERROR,
                    format_args!(
                        "ipSNCServer:readSocket: read error on: {}: {}",
                        io_port_name(v),
                        e.message_display()
                    ),
                );
                v.connected.set(0);
                strcpy(&v.input, "");
                e.status() as i32
            }
            Ok(()) => {
                // C defect fixed: C strcpy'd the 80-byte buffer into the
                // 40-byte `input`, so a line of 40 characters or more
                // overflowed it (glibc aborts the IOC), and a line that
                // filled the buffer had no terminator to stop at. The copy
                // takes the bytes read and keeps `input` terminated.
                client.print(
                    0,
                    TraceMask::IO_DEVICE,
                    format_args!(
                        "ipSNCServer:readSocket: {} read {}",
                        io_port_name(v),
                        String::from_utf8_lossy(&line.to_c_vec())
                    ),
                );
                strcpy(&v.input, &line[..]);
                0
            }
        }
    })
}

/// `writeSocket`: `output`, with no wait for room.
fn write_socket(v: &seqg_vars) -> i32 {
    SERVER.with_borrow(|server| {
        let client = server
            .as_ref()
            .and_then(|s| s.client.as_ref())
            .expect("processCommands follows a connect");
        let output = v.output.get().to_c_vec();
        match client.with_timeout(Duration::ZERO).write_octet(0, &output) {
            Err(e) => {
                client.print(
                    0,
                    TraceMask::ERROR,
                    format_args!(
                        "ipSNCServer:writeSocket: write error on: {}: {}",
                        io_port_name(v),
                        e.message_display()
                    ),
                );
                v.connected.set(0);
                e.status() as i32
            }
            Ok(_) => {
                client.print(
                    0,
                    TraceMask::IO_DEVICE,
                    format_args!(
                        "ipSNCServer:writeSocket: {} write {}",
                        io_port_name(v),
                        String::from_utf8_lossy(&output)
                    ),
                );
                0
            }
        }
    })
}
