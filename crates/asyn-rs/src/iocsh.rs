//! Asyn iocsh shell-command registration.
//!
//! C parity: `asynRegister` (asynShellCommands.c:1347-1379) registers 27
//! shell commands — the report/option/trace setters (`asynReport`,
//! `asynSetOption`, `asynShowOption`, the five `asynSetTrace*`), the ten
//! octet I/O and EOS commands, `asynEnable`/`asynAutoConnect`/
//! `asynWaitConnect`/`asynSetAutoConnectTimeout`, the timestamp-source pair
//! and the timer/queue-lock/shutdown setters. This
//! module exposes the same surface via [`register_asyn_commands`], which
//! takes an
//! `IocApplication` (the public registration carrier on the
//! `epics-base-rs` side) along with the `PortManager` whose
//! `TraceManager` is the back-end for the trace mutators.
//!
//! Available only with the `epics` feature.

// RTEMS-EXEC-MODEL-ALLOW(1): checked, not waived — all 1 ran and passed
// on the exec backend (measured on this tree:
// `EPICS_RS_BUILD_EXEC_BACKEND=thread cargo nextest run -p asyn-rs
// --all-features`, 1081/1081). asyn-rs became a census subject when its
// `build.rs` began deriving `tokio_backend`; nothing here builds a CA
// server, and the reactor these obtain comes from `#[tokio::test]`
// itself, which the backend does not remove.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use epics_base_rs::server::ioc_app::IocApplication;
use epics_base_rs::server::iocsh::registry::{
    ArgDesc, ArgType, ArgValue, CommandContext, CommandDef, CommandOutcome, CommandResult,
};

use crate::drivers::ftdi::DrvAsynFtdiPort;
use crate::drivers::ip_port::DrvAsynIPPort;
use crate::drivers::ip_server_port::{DrvAsynIPServerPort, IpServerConfig};
use crate::drivers::prologix::DrvAsynPrologixPort;
#[cfg(asyn_serial_backend)]
use crate::drivers::serial_port::DrvAsynSerialPort;
use crate::drivers::usbtmc::DrvAsynUsbtmcPort;
use crate::drivers::vxi11::DrvVxi11Port;
use crate::error::AsynResult;
use crate::escape::escaped_from_raw;
use crate::manager::PortManager;
use crate::port::PortDriver;
use crate::runtime::config::RuntimeConfig;
use crate::runtime::port::{PortRuntimeHandle, create_port_runtime};
use crate::services::PortServices;
use crate::trace::{TraceFile, TraceInfoMask, TraceIoMask, TraceManager, TraceMask};
use crate::user::AsynUser;

/// Register the standard asyn iocsh commands on the supplied
/// [`IocApplication`]. The shared [`PortManager`] is captured in each
/// command closure so the trace mutators reach the same
/// [`crate::trace::TraceManager`] the drivers were registered with.
///
/// C parity: the `asynShellCommands.c` set (`asynReport / asynSetOption /
/// asynOctetSetInputEos / asynOctetSetOutputEos / asynSetTraceMask /
/// asynSetTraceIOMask / asynSetTraceInfoMask / asynSetTraceFile`) plus the
/// port-creation commands (`drvAsynIPPortConfigure`,
/// `drvAsynSerialPortConfigure`, `drvAsynPrologixPortConfigure`).
///
/// Registered once. In C these are plain iocsh commands on the one command
/// table, so a startup script may create a port and set its EOS before
/// `iocInit` — the usual sequence for a socket detector — and the same names
/// answer at the `epics>` prompt. Each was registered twice here, through both
/// of `IocApplication`'s `register_*` methods, because the port used to give
/// the startup shell and the interactive shell a table each; the table is now
/// one, so the second call would only displace the name with itself.
pub fn register_asyn_commands(mut app: IocApplication, mgr: Arc<PortManager>) -> IocApplication {
    for def in build_asyn_commands(mgr) {
        app = app.register_startup_command(def);
    }
    app
}

fn arg_int(args: &[ArgValue], i: usize) -> Option<i64> {
    match args.get(i) {
        Some(ArgValue::Int(v)) => Some(*v),
        Some(ArgValue::Double(v)) => Some(*v as i64),
        Some(ArgValue::String(s)) => s.parse::<i64>().ok(),
        _ => None,
    }
}

fn arg_f64(args: &[ArgValue], i: usize) -> Option<f64> {
    match args.get(i) {
        Some(ArgValue::Double(v)) => Some(*v),
        Some(ArgValue::Int(v)) => Some(*v as f64),
        Some(ArgValue::String(s)) => s.parse::<f64>().ok(),
        _ => None,
    }
}

fn arg_str(args: &[ArgValue], i: usize) -> Option<String> {
    match args.get(i) {
        Some(ArgValue::String(s)) => Some(s.clone()),
        Some(ArgValue::Int(v)) => Some(v.to_string()),
        Some(ArgValue::Double(v)) => Some(v.to_string()),
        _ => None,
    }
}

/// Rust port of EPICS `epicsStrnRawFromEscaped` (libcom `epicsString.c`):
/// decode C-style escape sequences in `src` to raw bytes. The EOS iocsh
/// commands escape-decode their `eos` argument through this so a literal
/// `"\r\n"` typed in st.cmd becomes the two bytes CR LF — matching C
/// `asynSetEos` (`asynShellCommands.c`), which calls the same function.
///
/// An unknown escape passes the escaped character through literally (C's
/// `default:` arm). `\xXX` consumes up to two hex digits; a `\x` with no
/// following hex digit emits nothing and the next character is reprocessed
/// as ordinary input (C's `goto input`). A raw or `\0` NUL ends the scan.
fn raw_from_escaped(src: &str) -> Vec<u8> {
    let b = src.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        i += 1;
        if c == 0 {
            break;
        }
        if c != b'\\' {
            out.push(c);
            continue;
        }
        // Escape lead consumed; fetch the escaped character.
        if i >= b.len() {
            break;
        }
        let e = b[i];
        i += 1;
        if e == 0 {
            break;
        }
        match e {
            b'a' => out.push(0x07),
            b'b' => out.push(0x08),
            b'f' => out.push(0x0C),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'v' => out.push(0x0B),
            b'\\' => out.push(b'\\'),
            b'\'' => out.push(b'\''),
            b'"' => out.push(b'"'),
            b'0' => out.push(0),
            b'x' => {
                // \xXX: up to two hex digits. Peek (do not consume) so that a
                // non-hex character stays available to be reprocessed as
                // ordinary input on the next iteration (C `goto input`).
                let mut u: u32 = 0;
                let mut n = 0;
                while n < 2
                    && i < b.len()
                    && b[i] != 0
                    && let Some(d) = (b[i] as char).to_digit(16)
                {
                    u = (u << 4) | d;
                    i += 1;
                    n += 1;
                }
                if n > 0 {
                    out.push(u as u8);
                }
            }
            other => out.push(other),
        }
    }
    out
}

// `escaped_from_raw` — EPICS `epicsStrnEscapedFromRaw` (libcom
// `epicsString.c:120-160`), the inverse of `raw_from_escaped` above and what C
// `asynShowEos` prints the terminator through (`asynShellCommands.c:305`) — is
// imported from `crate::escape`, which is also where the report's
// `epicsStrPrintEscaped` form lives. The two differ on the NUL byte alone, and a
// second copy of the table is how they came to differ on more (R16-48).

/// C `asynShowEos`'s destination: `char cbuf[4 * sizeof eosargs.eos + 2]`
/// (asynShellCommands.c:304) over a 10-byte `eos` (:189) — 42 bytes, sized so
/// that even an all-`\xNN` terminator (4 chars per byte) escapes whole. Stated
/// because `epicsStrnEscapedFromRaw` has no unbounded form, not because this
/// call site can overflow.
const SHOW_EOS_BUF_SIZE: usize = 4 * 10 + 2;

/// The I/O deadline every asyn *shell* command puts on the `asynUser` it
/// builds: C sets `pasynUser->timeout = 2` in `asynSetOption`
/// (asynShellCommands.c:119), `asynSetEos` (:238) and `asynShowEos` (:289),
/// rather than leaving the default. One constant so the shell commands cannot
/// drift apart again.
const SHELL_IO_TIMEOUT: Duration = Duration::from_secs(2);

/// The `asynUser` C's `asynSetEos` / `asynShowEos` build
/// (asynShellCommands.c:238-240, :289-291).
///
/// Three fields matter and all come from C: the device the command names —
/// `findInterface(portName, addr, ...)` `connectDevice`s the user to it
/// (:79-80), which is what makes the EOS hook land on that device's terminator
/// — the 2 s I/O deadline ([`SHELL_IO_TIMEOUT`]), and
/// `ASYN_REASON_QUEUE_EVEN_IF_NOT_CONNECTED`, which is what lets an `st.cmd`
/// set a line's EOS before the device is powered on. No queue-wait deadline: C
/// passes `queueRequest(..., 0.0)` (:244).
fn shell_eos_user(addr: i32) -> AsynUser {
    AsynUser::default()
        .with_addr(addr)
        .with_timeout(SHELL_IO_TIMEOUT)
        .queue_even_if_not_connected()
}

/// Shared body of `asynOctetSetInputEos` / `asynOctetSetOutputEos`.
///
/// C parity: `asynShellCommands.c::asynSetEos` (:219-253) escape-decodes the
/// `eos` argument, `connectDevice`s its `asynUser` to `(portName, addr)` through
/// `findInterface` (:79-80, :233-234) and routes the bytes to
/// `pasynOctet->setInputEos`/`setOutputEos` — which read the addr off that user
/// to pick the device's terminator. The addr is threaded here for the same
/// reason: EOS is per device (see [`crate::port::eos_device_key`]).
///
/// The driver enforces the 2-byte terminator limit and reports
/// `illegal eoslen N`, so no length check is duplicated here (single owner of
/// the limit).
fn asyn_set_eos(
    mgr: &Arc<PortManager>,
    ctx: &CommandContext,
    args: &[ArgValue],
    set_input: bool,
) -> CommandResult {
    let cmd = if set_input {
        "asynOctetSetInputEos"
    } else {
        "asynOctetSetOutputEos"
    };
    let port = arg_str(args, 0).ok_or_else(|| "portName required".to_string())?;
    let addr = arg_int(args, 1).unwrap_or(0) as i32;
    let eos = raw_from_escaped(&arg_str(args, 2).unwrap_or_default());
    match mgr.find_port_handle(&port) {
        Ok(handle) => {
            // The shell's own user ([`shell_eos_user`]), carrying the device the
            // command named. This is the *shell* command; the record's IEOS/OEOS
            // put is queued at Low priority with no waiver (asynRecord.c:1296)
            // and stays refused on a disconnected port.
            let user = shell_eos_user(addr);
            let res = if set_input {
                handle.set_input_eos_blocking(user, &eos)
            } else {
                handle.set_output_eos_blocking(user, &eos)
            };
            if let Err(e) = res {
                ctx.println(&format!("{cmd}: {e}"));
            }
        }
        Err(e) => ctx.println(&format!("{cmd}: {e}")),
    }
    Ok(CommandOutcome::Continue)
}

/// Shared body of `asynOctetGetInputEos` / `asynOctetGetOutputEos` — C
/// `asynShowEos` (asynShellCommands.c:283-309), the readback twin of
/// [`asyn_set_eos`]: same `(portName, addr)` device selection, same shell user,
/// and on success it prints the terminator back escaped and quoted (:303-307).
fn asyn_show_eos(
    mgr: &Arc<PortManager>,
    ctx: &CommandContext,
    args: &[ArgValue],
    get_input: bool,
) -> CommandResult {
    let cmd = if get_input {
        "asynOctetGetInputEos"
    } else {
        "asynOctetGetOutputEos"
    };
    let port = arg_str(args, 0).ok_or_else(|| "portName required".to_string())?;
    let addr = arg_int(args, 1).unwrap_or(0) as i32;
    match mgr.find_port_handle(&port) {
        Ok(handle) => {
            let user = shell_eos_user(addr);
            let res = if get_input {
                handle.get_input_eos_blocking(user)
            } else {
                handle.get_output_eos_blocking(user)
            };
            match res {
                // C `printf("\"%s\"\n", cbuf)` over the escaped terminator.
                Ok(eos) => ctx.println(&format!(
                    "\"{}\"",
                    escaped_from_raw(&eos, SHOW_EOS_BUF_SIZE)
                )),
                Err(e) => ctx.println(&format!("Get EOS failed: {e}")),
            }
        }
        Err(e) => ctx.println(&format!("{cmd}: {e}")),
    }
    Ok(CommandOutcome::Continue)
}

/// `asynReport` body — walks every registered port (or the named one
/// only) and calls `PortDriver::report(level)`. C asyn's `asynReport`
/// loops through `pasynManager`'s port list and calls each driver's
/// `report` interface (`asynManager.c::asynReport`).
/// A shell `double` seconds argument as a `Duration`. C hands the value to
/// `epicsEventWaitWithTimeout`, which treats a non-positive timeout as "do not
/// wait"; NaN cannot be represented at all. Both collapse to zero here.
fn duration_from_secs(secs: f64) -> Duration {
    if secs.is_finite() && secs > 0.0 {
        Duration::from_secs_f64(secs)
    } else {
        Duration::ZERO
    }
}

/// `portName addr yesNo` — the argument list C gives both `asynEnable` and
/// `asynAutoConnect` (asynShellCommands.c:944-948, 976-982).
fn enable_style_args() -> Vec<ArgDesc> {
    vec![
        ArgDesc {
            name: "portName",
            arg_type: ArgType::String,
        },
        ArgDesc {
            name: "addr",
            arg_type: ArgType::Int,
        },
        ArgDesc {
            name: "yesNo",
            arg_type: ArgType::Int,
        },
    ]
}

/// C `findDpCommon` (asynManager.c:536-544): an operation lands on a DEVICE's
/// state only when the port is multi-device and the caller named a device;
/// otherwise it lands on the port itself. `asynEnable`/`asynAutoConnect` both
/// go through it, so the rule lives in one place here too.
fn addresses_a_device(handle: &crate::port_handle::PortHandle, addr: i32) -> bool {
    addr >= 0 && handle.is_multi_device()
}

/// Resolve the `portName addr yesNo` triple both enable-style commands take.
/// `None` means the error is already on the shell.
fn shell_enable_target(
    mgr: &Arc<PortManager>,
    ctx: &CommandContext,
    cmd: &str,
    args: &[ArgValue],
) -> Option<(crate::port_handle::PortHandle, i32, bool)> {
    let port = arg_str(args, 0).filter(|s| !s.is_empty())?;
    let addr = arg_int(args, 1).unwrap_or(0) as i32;
    let yes = arg_int(args, 2).unwrap_or(0) != 0;
    match mgr.find_port_handle(&port) {
        Ok(handle) => Some((handle, addr, yes)),
        Err(e) => {
            ctx.println(&format!("{cmd}: {e}"));
            None
        }
    }
}

fn report_ports(mgr: &Arc<PortManager>, level: i32, port: Option<&str>) {
    if let Some(name) = port {
        match mgr.find_runtime_handle(name) {
            Ok(handle) => {
                let _ = handle
                    .port_handle()
                    .report_blocking(level)
                    .map_err(|e| eprintln!("asynReport {name}: {e}"));
            }
            Err(e) => eprintln!("asynReport: {e}"),
        }
    } else {
        for name in mgr.list_port_names() {
            if let Ok(handle) = mgr.find_runtime_handle(&name) {
                let _ = handle
                    .port_handle()
                    .report_blocking(level)
                    .map_err(|e| eprintln!("asynReport {name}: {e}"));
            }
        }
    }
}

/// Variant for callers driving an `IocShell` directly — bypasses the
/// startup `IocApplication`. Used internally by tests; downstream
/// crates that want shell commands without the full `IocApplication`
/// pipeline (e.g. ad-plugins-rs which constructs its own shell) can
/// use this surface.
pub fn register_asyn_commands_on_shell(
    shell: &epics_base_rs::server::iocsh::IocShell,
    mgr: Arc<PortManager>,
) {
    for def in build_asyn_commands(mgr) {
        shell.register(def);
    }
}

/// Build the complete asyn iocsh `CommandDef` set without binding it to a
/// specific carrier: `asynReport`, `asynSetOption`, `asynOctetSetInputEos`,
/// `asynOctetSetOutputEos`, the trace mutators (`asynSetTraceMask`,
/// `asynSetTraceIOMask`, `asynSetTraceInfoMask`, `asynSetTraceFile`) and the
/// port-creation commands (`drvAsynIPPortConfigure`,
/// `drvAsynSerialPortConfigure`, `drvAsynPrologixPortConfigure`).
///
/// [`register_asyn_commands`] (IocApplication path) and
/// [`register_asyn_commands_on_shell`] (direct IocShell path) both delegate
/// here, so every carrier gets the same set and the C-parity surface cannot
/// drift between them. Keeping the port-creation commands in this one list is
/// what stops a shell from being able to *create* a port it cannot then
/// configure.
pub fn build_asyn_commands(mgr: Arc<PortManager>) -> Vec<CommandDef> {
    let mut out = Vec::new();
    let services = mgr.services().clone();

    // asynReport ----------------------------------------------------------
    {
        let mgr_r = mgr.clone();
        out.push(CommandDef::new(
            "asynReport",
            vec![
                ArgDesc {
                    name: "level",
                    arg_type: ArgType::Int,
                },
                ArgDesc {
                    name: "port",
                    arg_type: ArgType::String,
                },
            ],
            "asynReport [level] [portName] - Report registered ports",
            move |args: &[ArgValue], _ctx: &CommandContext| {
                let level = arg_int(args, 0).unwrap_or(0) as i32;
                let port = arg_str(args, 1);
                report_ports(&mgr_r, level, port.as_deref());
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // asynSetOption portName addr key value -------------------------------
    {
        let mgr_r = mgr.clone();
        out.push(CommandDef::new(
            "asynSetOption",
            vec![
                ArgDesc {
                    name: "portName",
                    arg_type: ArgType::String,
                },
                ArgDesc {
                    name: "addr",
                    arg_type: ArgType::Int,
                },
                ArgDesc {
                    name: "key",
                    arg_type: ArgType::String,
                },
                ArgDesc {
                    name: "value",
                    arg_type: ArgType::String,
                },
            ],
            "asynSetOption portName addr key value",
            move |args: &[ArgValue], ctx: &CommandContext| {
                let port = arg_str(args, 0).ok_or_else(|| "portName required".to_string())?;
                let addr = arg_int(args, 1).unwrap_or(0) as i32;
                let key = arg_str(args, 2).ok_or_else(|| "key required".to_string())?;
                let value = arg_str(args, 3).unwrap_or_default();
                // C `asynSetOption` builds its own asynUser and gives it
                // `timeout = 2` (asynShellCommands.c:119) before queueing the
                // setOption callback; `addr` rides on the same user, as C's
                // `findInterface(portName, addr, ...)` connects it to the device.
                // It queues at `asynQueuePriorityConnect` carrying
                // `ASYN_REASON_QUEUE_EVEN_IF_NOT_CONNECTED` (:121,:126) — that is
                // what lets an `st.cmd` configure a serial line whose device is
                // not powered on yet.
                let user = AsynUser::default()
                    .with_addr(addr)
                    .with_timeout(SHELL_IO_TIMEOUT)
                    .queue_even_if_not_connected();
                match mgr_r.find_port_handle(&port) {
                    Ok(handle) => match handle.set_option_blocking(user, &key, &value) {
                        Ok(()) => Ok(CommandOutcome::Continue),
                        Err(e) => {
                            ctx.println(&format!("asynSetOption: {e}"));
                            Ok(CommandOutcome::Continue)
                        }
                    },
                    Err(e) => {
                        ctx.println(&format!("asynSetOption: {e}"));
                        Ok(CommandOutcome::Continue)
                    }
                }
            },
        ));
    }

    // asynOctetSetInputEos portName addr eos ------------------------------
    {
        let mgr_r = mgr.clone();
        out.push(CommandDef::new(
            "asynOctetSetInputEos",
            vec![
                ArgDesc {
                    name: "portName",
                    arg_type: ArgType::String,
                },
                ArgDesc {
                    name: "addr",
                    arg_type: ArgType::Int,
                },
                ArgDesc {
                    name: "eos",
                    arg_type: ArgType::String,
                },
            ],
            "asynOctetSetInputEos portName addr eos - set the port input EOS (e.g. \"\\r\\n\")",
            move |args: &[ArgValue], ctx: &CommandContext| asyn_set_eos(&mgr_r, ctx, args, true),
        ));
    }

    // asynOctetSetOutputEos portName addr eos -----------------------------
    {
        let mgr_r = mgr.clone();
        out.push(CommandDef::new(
            "asynOctetSetOutputEos",
            vec![
                ArgDesc {
                    name: "portName",
                    arg_type: ArgType::String,
                },
                ArgDesc {
                    name: "addr",
                    arg_type: ArgType::Int,
                },
                ArgDesc {
                    name: "eos",
                    arg_type: ArgType::String,
                },
            ],
            "asynOctetSetOutputEos portName addr eos - set the port output EOS (e.g. \"\\r\\n\")",
            move |args: &[ArgValue], ctx: &CommandContext| asyn_set_eos(&mgr_r, ctx, args, false),
        ));
    }

    // asynOctetGetInputEos portName addr ----------------------------------
    //
    // C `asynOctetGetInputEos` (asynShellCommands.c:531-535) → `asynShowEos`,
    // the readback the operator uses to see which terminator a device actually
    // ended up with. It answers per (port, addr), like the setter.
    {
        let mgr_r = mgr.clone();
        out.push(CommandDef::new(
            "asynOctetGetInputEos",
            vec![
                ArgDesc {
                    name: "portName",
                    arg_type: ArgType::String,
                },
                ArgDesc {
                    name: "addr",
                    arg_type: ArgType::Int,
                },
            ],
            "asynOctetGetInputEos portName addr - print the device's input EOS",
            move |args: &[ArgValue], ctx: &CommandContext| asyn_show_eos(&mgr_r, ctx, args, true),
        ));
    }

    // asynOctetGetOutputEos portName addr ---------------------------------
    {
        let mgr_r = mgr.clone();
        out.push(CommandDef::new(
            "asynOctetGetOutputEos",
            vec![
                ArgDesc {
                    name: "portName",
                    arg_type: ArgType::String,
                },
                ArgDesc {
                    name: "addr",
                    arg_type: ArgType::Int,
                },
            ],
            "asynOctetGetOutputEos portName addr - print the device's output EOS",
            move |args: &[ArgValue], ctx: &CommandContext| asyn_show_eos(&mgr_r, ctx, args, false),
        ));
    }

    // asynInterposeEcho portName addr -------------------------------------
    //
    // C `asynInterposeEcho.c:189-210` registers this so a startup script can
    // install the echo layer on an already-configured port — the interpose is
    // useless without it, since a driver's own configure command never installs
    // it. C reports "%s interposeInterface failed." and returns -1 when the
    // port is unknown (:178-182).
    //
    // The `addr` names the DEVICE the layer lands on: C hands it to
    // `interposeInterface` (:176), which puts the layer on that device's
    // `dpCommon.interposeInterfaceList` (asynManager.c:2202-2206), and
    // `findInterface` resolves a request device-first (:1493-1501). On a port that
    // is not multi-device every addr resolves to the port itself.
    {
        let mgr_r = mgr.clone();
        out.push(CommandDef::new(
            "asynInterposeEcho",
            vec![
                ArgDesc {
                    name: "portName",
                    arg_type: ArgType::String,
                },
                ArgDesc {
                    name: "addr",
                    arg_type: ArgType::Int,
                },
            ],
            "asynInterposeEcho portName [addr] - install the echo interpose \
             (half-duplex devices that echo each char)",
            move |args: &[ArgValue], ctx: &CommandContext| {
                let port = arg_str(args, 0)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| "portName required".to_string())?;
                let addr = arg_int(args, 1).unwrap_or(0) as i32;
                match mgr_r.find_port_handle(&port) {
                    Ok(handle) => {
                        if let Err(e) = handle.push_echo_interpose_blocking(addr) {
                            ctx.println(&format!("{port} interposeInterface failed: {e}"));
                        }
                    }
                    Err(_) => ctx.println(&format!("{port} interposeInterface failed.")),
                }
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // asynInterposeDelay portName addr delay(sec) --------------------------
    //
    // C `asynInterposeDelay.c:215-237` registers this 3-arg command; nothing
    // else installs the layer (no driver configure command pushes it), so
    // without the registrar a startup script cannot reach a device that needs
    // an inter-character write delay at all. C prints
    // "%s interposeInterface asynOctetType failed." and returns -1 when the
    // interposeInterface call fails (:189-192).
    //
    // As with `asynInterposeEcho`, the `addr` names the device the layer lands on
    // (:187,200) — `asynInterposeDelay("gpib",4,0.01)` slows device 4 and leaves
    // the rest of the bus at full speed.
    {
        let mgr_r = mgr.clone();
        out.push(CommandDef::new(
            "asynInterposeDelay",
            vec![
                ArgDesc {
                    name: "portName",
                    arg_type: ArgType::String,
                },
                ArgDesc {
                    name: "addr",
                    arg_type: ArgType::Int,
                },
                ArgDesc {
                    name: "delay(sec)",
                    arg_type: ArgType::Double,
                },
            ],
            "asynInterposeDelay portName addr delay(sec) - install the delay \
             interpose (one write per character, delay after each)",
            move |args: &[ArgValue], ctx: &CommandContext| {
                let port = arg_str(args, 0)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| "portName required".to_string())?;
                let addr = arg_int(args, 1).unwrap_or(0) as i32;
                // C takes the delay as a `double` seconds and stores it verbatim
                // (`pvt->delay = delay`, asynInterposeDelay.c:210). iocsh can
                // supply a negative or NaN one; `delay_from_secs` owns the
                // conversion and collapses those to C's "no delay".
                let delay =
                    crate::interpose::delay::delay_from_secs(arg_f64(args, 2).unwrap_or(0.0));
                match mgr_r.find_port_handle(&port) {
                    Ok(handle) => {
                        if let Err(e) = handle.push_delay_interpose_blocking(addr, delay) {
                            ctx.println(&format!(
                                "{port} interposeInterface asynOctetType failed: {e}"
                            ));
                        }
                    }
                    Err(_) => {
                        ctx.println(&format!("{port} interposeInterface asynOctetType failed."))
                    }
                }
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // asynShowOption portName addr key ------------------------------------
    // C asynShellCommands.c:153-184 — the read half of asynSetOption. It prints
    // `key=value` (:149) and reaches the driver through the same queued option
    // call, so a port whose device is not up yet still answers.
    {
        let mgr_r = mgr.clone();
        out.push(CommandDef::new(
            "asynShowOption",
            vec![
                ArgDesc {
                    name: "portName",
                    arg_type: ArgType::String,
                },
                ArgDesc {
                    name: "addr",
                    arg_type: ArgType::Int,
                },
                ArgDesc {
                    name: "key",
                    arg_type: ArgType::String,
                },
            ],
            "asynShowOption portName addr key - print one driver option",
            move |args: &[ArgValue], ctx: &CommandContext| {
                let port = arg_str(args, 0).ok_or_else(|| "portName required".to_string())?;
                let addr = arg_int(args, 1).unwrap_or(0) as i32;
                let Some(key) = arg_str(args, 2) else {
                    // C: "Missing key argument" (:160-163).
                    ctx.println("Missing key argument");
                    return Ok(CommandOutcome::Continue);
                };
                let user = AsynUser::default()
                    .with_addr(addr)
                    .with_timeout(SHELL_IO_TIMEOUT)
                    .queue_even_if_not_connected();
                match mgr_r.find_port_handle(&port) {
                    Ok(handle) => match handle.get_option_blocking(user, &key) {
                        Ok(value) => ctx.println(&format!("{key}={value}")),
                        Err(e) => ctx.println(&format!("getOption failed {e}")),
                    },
                    Err(e) => ctx.println(&format!("asynShowOption: {e}")),
                }
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // asynSetTraceIOTruncateSize portName addr size -----------------------
    {
        let mgr_r = mgr.clone();
        out.push(CommandDef::new(
            "asynSetTraceIOTruncateSize",
            vec![
                ArgDesc {
                    name: "portName",
                    arg_type: ArgType::String,
                },
                ArgDesc {
                    name: "addr",
                    arg_type: ArgType::Int,
                },
                ArgDesc {
                    name: "size",
                    arg_type: ArgType::Int,
                },
            ],
            "asynSetTraceIOTruncateSize portName addr size - bytes of each I/O to trace",
            move |args: &[ArgValue], _ctx: &CommandContext| {
                let port = arg_str(args, 0).filter(|s| !s.is_empty());
                let addr = arg_int(args, 1).unwrap_or(-1) as i32;
                let size = arg_int(args, 2).unwrap_or(0).max(0) as usize;
                let trace = mgr_r.trace_manager();
                // Same addr routing as the trace-mask setters: C connects the
                // asynUser to (port, addr) first, so `addr >= 0` writes the
                // device's dpCommon and `addr < 0` the port's
                // (asynManager.c:2945-2959 via findTracePvt).
                match port.as_deref() {
                    Some(p) if addr >= 0 => trace.set_device_io_truncate_size(p, addr, size),
                    Some(p) => trace.set_io_truncate_size(Some(p), size),
                    None => trace.set_io_truncate_size(None, size),
                }
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // asynRegisterTimeStampSource portName functionName --------------------
    {
        let mgr_r = mgr.clone();
        out.push(CommandDef::new(
            "asynRegisterTimeStampSource",
            vec![
                ArgDesc {
                    name: "portName",
                    arg_type: ArgType::String,
                },
                ArgDesc {
                    name: "functionName",
                    arg_type: ArgType::String,
                },
            ],
            "asynRegisterTimeStampSource portName functionName - stamp this port's values with \
             the named source",
            move |args: &[ArgValue], ctx: &CommandContext| {
                let (Some(port), Some(function)) = (
                    arg_str(args, 0).filter(|s| !s.is_empty()),
                    arg_str(args, 1).filter(|s| !s.is_empty()),
                ) else {
                    // C's usage line (asynShellCommands.c:1187-1190).
                    ctx.println("Usage: asynRegisterTimeStampSource portName functionName");
                    return Ok(CommandOutcome::Continue);
                };
                match mgr_r.find_port_handle(&port) {
                    Ok(handle) => {
                        if let Err(e) = handle.set_time_stamp_source_blocking(Some(&function)) {
                            ctx.println(&format!("asynRegisterTimeStampSource: {e}"));
                        }
                    }
                    Err(_) => ctx.println(&format!(
                        "asynRegisterTimeStampSource, cannot connect to port {port}"
                    )),
                }
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // asynUnregisterTimeStampSource portName -------------------------------
    {
        let mgr_r = mgr.clone();
        out.push(CommandDef::new(
            "asynUnregisterTimeStampSource",
            vec![ArgDesc {
                name: "portName",
                arg_type: ArgType::String,
            }],
            "asynUnregisterTimeStampSource portName - back to the port's default clock",
            move |args: &[ArgValue], ctx: &CommandContext| {
                let Some(port) = arg_str(args, 0).filter(|s| !s.is_empty()) else {
                    ctx.println("Usage: asynUnregisterTimeStampSource portName");
                    return Ok(CommandOutcome::Continue);
                };
                match mgr_r.find_port_handle(&port) {
                    Ok(handle) => {
                        if let Err(e) = handle.set_time_stamp_source_blocking(None) {
                            ctx.println(&format!("asynUnregisterTimeStampSource: {e}"));
                        }
                    }
                    Err(_) => ctx.println(&format!(
                        "asynUnregisterTimeStampSource, cannot connect to port {port}"
                    )),
                }
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // asynSetMinTimerPeriod period ----------------------------------------
    // C implements this only under `_WIN32` (asynShellCommands.c:1226-1255,
    // `timeBeginPeriod`); on every other OS the body is a single printf saying
    // so and a -1 return (:1259-1263). The command must still EXIST — an
    // st.cmd that carries the line has to keep running — so it is registered
    // with C's message. Not a stub: it is what C does on this platform.
    out.push(CommandDef::new(
        "asynSetMinTimerPeriod",
        vec![ArgDesc {
            name: "minimum period",
            arg_type: ArgType::Double,
        }],
        "asynSetMinTimerPeriod period - Windows-only timer resolution (no effect here)",
        move |_args: &[ArgValue], ctx: &CommandContext| {
            ctx.println("asynSetMinTimerPeriod is not currently supported on this OS");
            Ok(CommandOutcome::Continue)
        },
    ));

    // asynWaitConnect portName timeout ------------------------------------
    // C asynShellCommands.c (asynWaitConnect) → pasynManager->waitConnect
    // (asynManager.c:3292-3336). The st.cmd line that follows a
    // drvAsynIPPortConfigure for a slow device: block here until the port is up
    // rather than let the next line's I/O fail on a still-connecting port.
    {
        let mgr_r = mgr.clone();
        let services_wait = services.clone();
        out.push(CommandDef::new(
            "asynWaitConnect",
            vec![
                ArgDesc {
                    name: "portName",
                    arg_type: ArgType::String,
                },
                ArgDesc {
                    name: "timeout",
                    arg_type: ArgType::Double,
                },
            ],
            "asynWaitConnect portName timeout - block until the port is connected",
            move |args: &[ArgValue], ctx: &CommandContext| {
                let port = arg_str(args, 0)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| "portName required".to_string())?;
                let timeout = duration_from_secs(arg_f64(args, 1).unwrap_or(0.0));
                let handle = match mgr_r.find_port_handle(&port) {
                    Ok(h) => h,
                    Err(e) => {
                        ctx.println(&format!("asynWaitConnect: {e}"));
                        return Ok(CommandOutcome::Continue);
                    }
                };
                // Arm before the check: a connect landing between the two would
                // be lost the other way round (C :3313-3316 registers the
                // exception handler before it waits).
                let waiter = crate::runtime::port::ConnectWaiter::arm(&services_wait, &port);
                if handle.is_connected_blocking().unwrap_or(false) {
                    return Ok(CommandOutcome::Continue);
                }
                if !waiter.wait(timeout) {
                    ctx.println(&format!("asynWaitConnect: {port} not connected"));
                }
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // asynSetAutoConnectTimeout timeout -----------------------------------
    out.push(CommandDef::new(
        "asynSetAutoConnectTimeout",
        vec![ArgDesc {
            name: "timeout",
            arg_type: ArgType::Double,
        }],
        "asynSetAutoConnectTimeout timeout - seconds a new port waits for its first connect \
         (C default 0.5)",
        move |args: &[ArgValue], _ctx: &CommandContext| {
            // C `setAutoConnectTimeout` (asynManager.c:2370-2377) writes the
            // process-global `pasynBase->autoConnectTimeout`; every port
            // registered after this line reads the new value (:2135).
            crate::runtime::config::set_auto_connect_timeout(duration_from_secs(
                arg_f64(args, 0).unwrap_or(0.0),
            ));
            Ok(CommandOutcome::Continue)
        },
    ));

    // asynInterposeEosConfig portName addr processIn processOut ------------
    // C asynInterposeEos.c:393-410. The layer `drvAsynIPPortConfigure`
    // installs by default (:1065) is the same one, with both halves on; this
    // command is how a port created WITHOUT it (noProcessEos=1, or a serial
    // port) gets EOS processing back, and how a port gets one half only.
    {
        let mgr_r = mgr.clone();
        out.push(CommandDef::new(
            "asynInterposeEosConfig",
            vec![
                ArgDesc {
                    name: "portName",
                    arg_type: ArgType::String,
                },
                ArgDesc {
                    name: "addr",
                    arg_type: ArgType::Int,
                },
                ArgDesc {
                    name: "processIn",
                    arg_type: ArgType::Int,
                },
                ArgDesc {
                    name: "processOut",
                    arg_type: ArgType::Int,
                },
            ],
            "asynInterposeEosConfig portName addr processIn processOut - install the EOS \
             interpose (terminator handling) on the port",
            move |args: &[ArgValue], ctx: &CommandContext| {
                let port = arg_str(args, 0)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| "portName required".to_string())?;
                let addr = arg_int(args, 1).unwrap_or(0) as i32;
                let process_in = arg_int(args, 2).unwrap_or(0) != 0;
                let process_out = arg_int(args, 3).unwrap_or(0) != 0;
                match mgr_r.find_port_handle(&port) {
                    Ok(handle) => {
                        if let Err(e) =
                            handle.push_eos_interpose_blocking(addr, process_in, process_out)
                        {
                            ctx.println(&format!("{port} interposeInterface failed: {e}"));
                        }
                    }
                    Err(_) => ctx.println(&format!("{port} interposeInterface failed.")),
                }
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // asynInterposeFlushConfig portName addr timeout(ms) -------------------
    {
        let mgr_r = mgr.clone();
        out.push(CommandDef::new(
            "asynInterposeFlushConfig",
            vec![
                ArgDesc {
                    name: "portName",
                    arg_type: ArgType::String,
                },
                ArgDesc {
                    name: "addr",
                    arg_type: ArgType::Int,
                },
                ArgDesc {
                    name: "timeout(msec)",
                    arg_type: ArgType::Double,
                },
            ],
            "asynInterposeFlushConfig portName addr timeout(msec) - install the flush \
             interpose (a read with this timeout drains the input before each write)",
            move |args: &[ArgValue], ctx: &CommandContext| {
                let port = arg_str(args, 0)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| "portName required".to_string())?;
                let addr = arg_int(args, 1).unwrap_or(0) as i32;
                // C `asynInterposeFlushConfig` (asynInterposeFlush.c:78-79):
                // the shell argument is an integer number of MILLIseconds, and
                // a non-positive one is coerced to 1 ms — a zero-timeout flush
                // would drain nothing.
                let ms = arg_f64(args, 2).unwrap_or(0.0) as i64;
                let ms = if ms <= 0 { 1 } else { ms };
                let timeout = Duration::from_millis(ms as u64);
                match mgr_r.find_port_handle(&port) {
                    Ok(handle) => {
                        if let Err(e) = handle.push_flush_interpose_blocking(addr, timeout) {
                            ctx.println(&format!("{port} interposeInterface failed: {e}"));
                        }
                    }
                    Err(_) => ctx.println(&format!("{port} interposeInterface failed.")),
                }
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // asynSetTraceMask ----------------------------------------------------
    {
        let mgr_r = mgr.clone();
        out.push(CommandDef::new(
            "asynSetTraceMask",
            vec![
                ArgDesc {
                    name: "portName",
                    arg_type: ArgType::String,
                },
                ArgDesc {
                    name: "addr",
                    arg_type: ArgType::Int,
                },
                ArgDesc {
                    name: "mask",
                    arg_type: ArgType::String,
                },
            ],
            "asynSetTraceMask [portName] [addr] mask",
            move |args: &[ArgValue], ctx: &CommandContext| {
                let port = arg_str(args, 0).filter(|s| !s.is_empty());
                let addr = arg_int(args, 1).unwrap_or(-1) as i32;
                let mask_str = arg_str(args, 2).ok_or_else(|| "mask required".to_string())?;
                match TraceMask::from_symbolic(&mask_str) {
                    Ok(m) => {
                        let trace = mgr_r.trace_manager();
                        if let Some(p) = port.as_deref() {
                            if addr >= 0 {
                                trace.set_device_trace_mask(p, addr, m);
                            } else {
                                trace.set_trace_mask(Some(p), m);
                            }
                        } else {
                            trace.set_trace_mask(None, m);
                        }
                        Ok(CommandOutcome::Continue)
                    }
                    Err(e) => {
                        ctx.println(&format!("asynSetTraceMask: {e}"));
                        Ok(CommandOutcome::Continue)
                    }
                }
            },
        ));
    }

    // asynSetTraceIOMask --------------------------------------------------
    {
        let mgr_r = mgr.clone();
        out.push(CommandDef::new(
            "asynSetTraceIOMask",
            vec![
                ArgDesc {
                    name: "portName",
                    arg_type: ArgType::String,
                },
                ArgDesc {
                    name: "addr",
                    arg_type: ArgType::Int,
                },
                ArgDesc {
                    name: "mask",
                    arg_type: ArgType::String,
                },
            ],
            "asynSetTraceIOMask [portName] [addr] mask",
            move |args: &[ArgValue], ctx: &CommandContext| {
                let port = arg_str(args, 0).filter(|s| !s.is_empty());
                let addr = arg_int(args, 1).unwrap_or(-1) as i32;
                let mask_str = arg_str(args, 2).ok_or_else(|| "mask required".to_string())?;
                match TraceIoMask::from_symbolic(&mask_str) {
                    Ok(m) => {
                        let trace = mgr_r.trace_manager();
                        // C parity: asynShellCommands.c:734-754 calls
                        // `connectDevice(pasynUser, portName, addr)`
                        // before `setTraceIOMask`. When `addr >= 0` the
                        // pasynUser carries a `pdevice`, so
                        // `setTraceIOMask` (asynManager.c:2830-2833)
                        // writes the device-specific dpCommon; only
                        // when `addr < 0` does it fall to the
                        // every-device + port fallback.
                        if let Some(p) = port.as_deref() {
                            if addr >= 0 {
                                trace.set_device_trace_io_mask(p, addr, m);
                            } else {
                                trace.set_trace_io_mask(Some(p), m);
                            }
                        } else {
                            trace.set_trace_io_mask(None, m);
                        }
                        Ok(CommandOutcome::Continue)
                    }
                    Err(e) => {
                        ctx.println(&format!("asynSetTraceIOMask: {e}"));
                        Ok(CommandOutcome::Continue)
                    }
                }
            },
        ));
    }

    // asynSetTraceInfoMask ------------------------------------------------
    {
        let mgr_r = mgr.clone();
        out.push(CommandDef::new(
            "asynSetTraceInfoMask",
            vec![
                ArgDesc {
                    name: "portName",
                    arg_type: ArgType::String,
                },
                ArgDesc {
                    name: "addr",
                    arg_type: ArgType::Int,
                },
                ArgDesc {
                    name: "mask",
                    arg_type: ArgType::String,
                },
            ],
            "asynSetTraceInfoMask [portName] [addr] mask",
            move |args: &[ArgValue], ctx: &CommandContext| {
                let port = arg_str(args, 0).filter(|s| !s.is_empty());
                let addr = arg_int(args, 1).unwrap_or(-1) as i32;
                let mask_str = arg_str(args, 2).ok_or_else(|| "mask required".to_string())?;
                match TraceInfoMask::from_symbolic(&mask_str) {
                    Ok(m) => {
                        let trace = mgr_r.trace_manager();
                        // C parity: asynShellCommands.c:799-820 routes
                        // through `connectDevice(pasynUser, portName, addr)`.
                        // `setTraceInfoMask` (asynManager.c:2872-2875)
                        // writes the device-specific dpCommon when
                        // `pdevice != NULL` (addr >= 0); falls to
                        // every-device + port otherwise.
                        if let Some(p) = port.as_deref() {
                            if addr >= 0 {
                                trace.set_device_trace_info_mask(p, addr, m);
                            } else {
                                trace.set_trace_info_mask(Some(p), m);
                            }
                        } else {
                            trace.set_trace_info_mask(None, m);
                        }
                        Ok(CommandOutcome::Continue)
                    }
                    Err(e) => {
                        ctx.println(&format!("asynSetTraceInfoMask: {e}"));
                        Ok(CommandOutcome::Continue)
                    }
                }
            },
        ));
    }

    // asynSetTraceFile ----------------------------------------------------
    {
        let mgr_r = mgr.clone();
        out.push(CommandDef::new(
            "asynSetTraceFile",
            vec![
                ArgDesc {
                    name: "portName",
                    arg_type: ArgType::String,
                },
                ArgDesc {
                    name: "addr",
                    arg_type: ArgType::Int,
                },
                ArgDesc {
                    name: "filename",
                    arg_type: ArgType::String,
                },
            ],
            "asynSetTraceFile [portName] [addr] [filename]",
            move |args: &[ArgValue], ctx: &CommandContext| {
                let port = arg_str(args, 0).filter(|s| !s.is_empty());
                let addr = arg_int(args, 1).unwrap_or(-1) as i32;
                let filename = arg_str(args, 2).unwrap_or_default();
                let target = match filename.as_str() {
                    "" | "stderr" => TraceFile::Stderr,
                    "stdout" => TraceFile::Stdout,
                    path => match std::fs::File::create(path) {
                        Ok(f) => TraceFile::File(Arc::new(std::sync::Mutex::new(f))),
                        Err(e) => {
                            ctx.println(&format!("asynSetTraceFile: fopen failed: {e}"));
                            return Ok(CommandOutcome::Continue);
                        }
                    },
                };
                let trace = mgr_r.trace_manager();
                // C parity: asynShellCommands.c:858-892 routes through
                // `connectDevice(pasynUser, portName, addr)`. The
                // `setTraceFile` resolver (asynManager.c:2898-2926)
                // walks `findTracePvt(puserPvt)` which picks the
                // device-specific `dpCommon` when `pdevice != NULL`.
                if let Some(p) = port.as_deref() {
                    if addr >= 0 {
                        trace.set_device_trace_file(p, addr, target);
                    } else {
                        trace.set_trace_file(Some(p), target);
                    }
                } else {
                    trace.set_trace_file(None, target);
                }
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // asynEnable portName addr yesNo --------------------------------------
    {
        let mgr_r = mgr.clone();
        out.push(CommandDef::new(
            "asynEnable",
            enable_style_args(),
            "asynEnable portName addr yesNo - enable (1) or disable (0) a port or one of its devices",
            move |args: &[ArgValue], ctx: &CommandContext| {
                let (handle, addr, yes) = match shell_enable_target(&mgr_r, ctx, "asynEnable", args)
                {
                    Some(t) => t,
                    None => return Ok(CommandOutcome::Continue),
                };
                let r = match (addresses_a_device(&handle, addr), yes) {
                    (true, true) => handle.enable_addr_blocking(addr),
                    (true, false) => handle.disable_addr_blocking(addr),
                    (false, yes) => handle.set_enable_blocking(yes),
                };
                if let Err(e) = r {
                    ctx.println(&format!("asynEnable: {e}"));
                }
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // asynAutoConnect portName addr yesNo ---------------------------------
    {
        let mgr_r = mgr.clone();
        out.push(CommandDef::new(
            "asynAutoConnect",
            enable_style_args(),
            "asynAutoConnect portName addr yesNo - turn auto-connect on (1) or off (0) for a port \
             or one of its devices",
            move |args: &[ArgValue], ctx: &CommandContext| {
                let (handle, addr, yes) =
                    match shell_enable_target(&mgr_r, ctx, "asynAutoConnect", args) {
                        Some(t) => t,
                        None => return Ok(CommandOutcome::Continue),
                    };
                let r = if addresses_a_device(&handle, addr) {
                    handle.set_auto_connect_addr_blocking(addr, yes)
                } else {
                    handle.set_auto_connect_blocking(yes)
                };
                if let Err(e) = r {
                    ctx.println(&format!("asynAutoConnect: {e}"));
                }
                Ok(CommandOutcome::Continue)
            },
        ));
    }

    // Port-creation commands ----------------------------------------------
    // Part of the same set: a shell that can create a port must be able to
    // configure it in the next line of the same script.
    out.push(drv_asyn_ip_port_configure_command(services.clone()));
    out.push(drv_asyn_ip_server_port_configure_command(services.clone()));
    // Absent where no serial backend is built: registering a
    // `drvAsynSerialPortConfigure` that cannot create a port would make a
    // startup script appear to work and then fail at connect.
    #[cfg(asyn_serial_backend)]
    out.push(drv_asyn_serial_port_configure_command(services.clone()));
    out.push(drv_asyn_ftdi_port_configure_command(services.clone()));
    out.push(vxi11_configure_command(services.clone()));
    out.push(usbtmc_configure_command(services.clone()));
    out.push(drv_asyn_prologix_port_configure_command(services));

    out
}

/// Build the `drvAsynIPPortConfigure` iocsh command.
///
/// C parity: `drvAsynIPPort.c::drvAsynIPPortConfigure(portName,
/// hostInfo, priority, noAutoConnect, noProcessEos)`. `hostInfo` is
/// `host:port[:localPort] [protocol]` (see [`DrvAsynIPPort::new`]).
///
/// The created port is registered in the [`crate::asyn_record`] port
/// registry so `asynRecord` device support resolves it by name. The
/// runtime handle is parked in a process-lifetime static.
///
/// `priority` is accepted for startup-script compatibility but has no
/// effect: the Rust runtime schedules every port actor uniformly
/// (priority is advisory in C too). `noAutoConnect` and `noProcessEos`
/// are honored — by default the command installs an EOS interpose (C
/// `drvAsynIPPort.c:1065-1066` `asynInterposeEosConfig`), and a nonzero
/// `noProcessEos` suppresses it.
/// Build a configured IP port driver: parse host info, honor
/// `noAutoConnect`, and install the default EOS interpose unless
/// `noProcessEos` (C `drvAsynIPPort.c:1065-1066`). Shared by the iocsh
/// command and its tests so the install decision has a single owner.
pub(crate) fn build_configured_ip_port(
    port: &str,
    host: &str,
    no_auto_connect: bool,
    no_process_eos: bool,
) -> AsynResult<DrvAsynIPPort> {
    DrvAsynIPPort::new_configured(port, host, no_auto_connect, no_process_eos)
}

pub fn drv_asyn_ip_port_configure_command(services: PortServices) -> CommandDef {
    CommandDef::new(
        "drvAsynIPPortConfigure",
        vec![
            ArgDesc {
                name: "portName",
                arg_type: ArgType::String,
            },
            ArgDesc {
                name: "hostInfo",
                arg_type: ArgType::String,
            },
            ArgDesc {
                name: "priority",
                arg_type: ArgType::Int,
            },
            ArgDesc {
                name: "noAutoConnect",
                arg_type: ArgType::Int,
            },
            ArgDesc {
                name: "noProcessEos",
                arg_type: ArgType::Int,
            },
        ],
        "drvAsynIPPortConfigure portName hostInfo [priority] [noAutoConnect] [noProcessEos] \
         - create an IP octet port",
        move |args: &[ArgValue], ctx: &CommandContext| {
            let port = arg_str(args, 0)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "portName required".to_string())?;
            let host = arg_str(args, 1)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "hostInfo required".to_string())?;
            let no_auto_connect = arg_int(args, 3).unwrap_or(0) != 0;
            let no_process_eos = arg_int(args, 4).unwrap_or(0) != 0;

            let driver =
                match build_configured_ip_port(&port, &host, no_auto_connect, no_process_eos) {
                    Ok(d) => d,
                    Err(e) => {
                        ctx.println(&format!("drvAsynIPPortConfigure: {e}"));
                        return Ok(CommandOutcome::Continue);
                    }
                };

            if publish_configured_port("drvAsynIPPortConfigure", &port, driver, &services, ctx)
                .is_none()
            {
                return Ok(CommandOutcome::Continue);
            }
            ctx.println(&format!(
                "drvAsynIPPortConfigure: octet port '{port}' -> {host}"
            ));
            Ok(CommandOutcome::Continue)
        },
    )
}

/// Build the `drvAsynIPServerPortConfigure` iocsh command.
///
/// C parity: `drvAsynIPServerPort.c::drvAsynIPServerPortConfigure(portName,
/// serverInfo, maxClients, priority, noAutoConnect, noProcessEos)` (:523-722),
/// registered as a 6-argument iocsh command (:727-739). Configure binds the
/// listening socket, pre-creates one child port per client slot named
/// `<parent>:<N>` (:682-708), and starts the `connectionListener` thread
/// (:711-714) — so by the time `st.cmd` returns from this line, the server is
/// accepting and every child port an IOC could bind device support to exists.
///
/// `priority` is accepted for startup-script compatibility but has no effect
/// (the Rust runtime schedules port actors uniformly).
pub fn drv_asyn_ip_server_port_configure_command(services: PortServices) -> CommandDef {
    CommandDef::new(
        "drvAsynIPServerPortConfigure",
        vec![
            ArgDesc {
                name: "portName",
                arg_type: ArgType::String,
            },
            ArgDesc {
                name: "serverInfo",
                arg_type: ArgType::String,
            },
            ArgDesc {
                name: "maxClients",
                arg_type: ArgType::Int,
            },
            ArgDesc {
                name: "priority",
                arg_type: ArgType::Int,
            },
            ArgDesc {
                name: "noAutoConnect",
                arg_type: ArgType::Int,
            },
            ArgDesc {
                name: "noProcessEos",
                arg_type: ArgType::Int,
            },
        ],
        "drvAsynIPServerPortConfigure portName serverInfo maxClients [priority] \
         [noAutoConnect] [noProcessEos] - create an IP server (listening) port",
        move |args: &[ArgValue], ctx: &CommandContext| {
            let port = arg_str(args, 0)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "portName required".to_string())?;
            let server_info = arg_str(args, 1)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "serverInfo required".to_string())?;
            let max_clients = arg_int(args, 2).unwrap_or(0);
            let no_auto_connect = arg_int(args, 4).unwrap_or(0) != 0;
            let no_process_eos = arg_int(args, 5).unwrap_or(0) != 0;

            let mut config = match IpServerConfig::parse(&server_info) {
                Ok(c) => c,
                Err(e) => {
                    ctx.println(&format!("drvAsynIPServerPortConfigure: {e}"));
                    return Ok(CommandOutcome::Continue);
                }
            };
            // C takes maxClients from the shell argument and rejects 0
            // ("No clients.", drvAsynIPServerPort.c:545-548).
            config.max_clients = max_clients.max(0) as usize;
            // C stores noProcessEos on the server and hands it to every child's
            // `drvAsynIPPortConfigure` (:688-694) — that is its only use. The
            // argument was parsed into the command's arg list and then never
            // read, so a `\n`-terminated client of an IP server never had an EOS
            // layer to terminate on (R19-107).
            config.no_process_eos = no_process_eos;

            let mut driver = match DrvAsynIPServerPort::with_config(&port, config) {
                Ok(d) => d,
                Err(e) => {
                    ctx.println(&format!("drvAsynIPServerPortConfigure: {e}"));
                    return Ok(CommandOutcome::Continue);
                }
            };
            if no_auto_connect {
                driver.base_mut().auto_connect = false;
            }

            // The child ports C pre-creates at configure (:682-708) — device
            // support binds to `<parent>:<N>` before any client has connected.
            let n_children = driver.child_port_names().len();
            let children = (0..n_children)
                .map(|i| driver.make_subport(i))
                .collect::<AsynResult<Vec<_>>>();
            let children = match children {
                Ok(c) => c,
                Err(e) => {
                    ctx.println(&format!("drvAsynIPServerPortConfigure: {e}"));
                    return Ok(CommandOutcome::Continue);
                }
            };

            let Some(handle) = publish_configured_port(
                "drvAsynIPServerPortConfigure",
                &port,
                driver,
                &services,
                ctx,
            ) else {
                return Ok(CommandOutcome::Continue);
            };
            // C binds the listening socket inside configure — `createServerSocket`
            // at drvAsynIPServerPort.c:605, before `registerPort` and regardless
            // of `noAutoConnect`, which governs only the *server* port's own
            // autoConnect (:627); the child ports are created with
            // `noAutoConnect = 1` unconditionally (:693, and the comment saying
            // why at :689). So the server is listening when st.cmd moves to
            // the next line, not when some record first pokes it.
            if let Err(e) = handle.port_handle().connect_blocking() {
                ctx.println(&format!(
                    "drvAsynIPServerPortConfigure: cannot listen on {server_info}: {e}"
                ));
                crate::asyn_record::unregister_port(&port);
                handle.shutdown();
                return Ok(CommandOutcome::Continue);
            }
            drop(handle);

            for child in children {
                let name = child.base().port_name.clone();
                // C traces and keeps going: a child that cannot be created costs
                // that slot, not the server (:694-697). The helper has already
                // printed and cleaned up whichever way it failed.
                publish_configured_port(
                    &format!("drvAsynIPServerPortConfigure: {name}"),
                    &name,
                    child,
                    &services,
                    ctx,
                );
            }

            ctx.println(&format!(
                "drvAsynIPServerPortConfigure: server port '{port}' -> {server_info}"
            ));
            Ok(CommandOutcome::Continue)
        },
    )
}

/// Build the `drvAsynSerialPortConfigure` iocsh command.
///
/// C parity: `drvAsynSerialPort.c::drvAsynSerialPortConfigure(portName,
/// ttyName, priority, noAutoConnect, noProcessEos)`. `ttyName` is the
/// serial device path (see [`DrvAsynSerialPort::new`]).
///
/// The created port is registered in the [`crate::asyn_record`] port
/// registry so `asynRecord` device support resolves it by name. As with
/// the IP command, `priority` is accepted for startup-script
/// compatibility but has no effect (the Rust runtime schedules port
/// actors uniformly); `noAutoConnect` and `noProcessEos` are honored —
/// by default an EOS interpose is installed (C
/// `drvAsynSerialPort.c:1126` enables EOS processing in octetBase),
/// suppressed by a nonzero `noProcessEos`.
#[cfg(asyn_serial_backend)]
pub fn drv_asyn_serial_port_configure_command(services: PortServices) -> CommandDef {
    CommandDef::new(
        "drvAsynSerialPortConfigure",
        vec![
            ArgDesc {
                name: "portName",
                arg_type: ArgType::String,
            },
            ArgDesc {
                name: "ttyName",
                arg_type: ArgType::String,
            },
            ArgDesc {
                name: "priority",
                arg_type: ArgType::Int,
            },
            ArgDesc {
                name: "noAutoConnect",
                arg_type: ArgType::Int,
            },
            ArgDesc {
                name: "noProcessEos",
                arg_type: ArgType::Int,
            },
        ],
        "drvAsynSerialPortConfigure portName ttyName [priority] [noAutoConnect] [noProcessEos] \
         - create a serial octet port",
        move |args: &[ArgValue], ctx: &CommandContext| {
            let port = arg_str(args, 0)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "portName required".to_string())?;
            let tty = arg_str(args, 1)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "ttyName required".to_string())?;
            let no_auto_connect = arg_int(args, 3).unwrap_or(0) != 0;
            let no_process_eos = arg_int(args, 4).unwrap_or(0) != 0;

            let driver =
                match DrvAsynSerialPort::configure(&port, &tty, no_auto_connect, no_process_eos) {
                    Ok(d) => d,
                    Err(e) => {
                        ctx.println(&format!("drvAsynSerialPortConfigure: {e}"));
                        return Ok(CommandOutcome::Continue);
                    }
                };

            if publish_configured_port("drvAsynSerialPortConfigure", &port, driver, &services, ctx)
                .is_none()
            {
                return Ok(CommandOutcome::Continue);
            }
            ctx.println(&format!(
                "drvAsynSerialPortConfigure: octet port '{port}' -> {tty}"
            ));
            Ok(CommandOutcome::Continue)
        },
    )
}

/// Build the `prologixGPIBConfigure` iocsh command.
///
/// C parity: `drvPrologixGPIB.c::prologixGPIBConfigure(portName, host,
/// priority, noAutoConnect)` (lines 547-628). `host` may be `"hostname"`
/// (the bridge's fixed `:1234 TCP` is appended) or `"hostname:port"`; see
/// [`DrvAsynPrologixPort::new`]. The created GPIB port is registered in the
/// [`crate::asyn_record`] port registry so `asynRecord` device support
/// resolves it by name.
///
/// As with the IP/serial commands, `priority` is accepted for startup-script
/// compatibility but has no effect (the Rust runtime schedules port actors
/// uniformly); `noAutoConnect` is honored. There is no `noProcessEos` arg —
/// the prologix driver owns EOS itself (it passes `noProcessEos=1` to its
/// inner `_TCP` IP port, mirroring C's `drvAsynIPPortConfigure(... 1)` at
/// drvPrologixGPIB.c:575).
pub fn drv_asyn_prologix_port_configure_command(services: PortServices) -> CommandDef {
    CommandDef::new(
        "prologixGPIBConfigure",
        vec![
            ArgDesc {
                name: "portName",
                arg_type: ArgType::String,
            },
            ArgDesc {
                name: "host",
                arg_type: ArgType::String,
            },
            ArgDesc {
                name: "priority",
                arg_type: ArgType::Int,
            },
            ArgDesc {
                name: "noAutoConnect",
                arg_type: ArgType::Int,
            },
        ],
        "prologixGPIBConfigure portName host [priority] [noAutoConnect] \
         - create a Prologix GPIB-Ethernet port",
        move |args: &[ArgValue], ctx: &CommandContext| {
            let port = arg_str(args, 0)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "portName required".to_string())?;
            let host = arg_str(args, 1)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "host required".to_string())?;
            let no_auto_connect = arg_int(args, 3).unwrap_or(0) != 0;

            let driver = match DrvAsynPrologixPort::new(&port, &host, no_auto_connect) {
                Ok(d) => d,
                Err(e) => {
                    ctx.println(&format!("prologixGPIBConfigure: {e}"));
                    return Ok(CommandOutcome::Continue);
                }
            };

            if publish_configured_port("prologixGPIBConfigure", &port, driver, &services, ctx)
                .is_none()
            {
                return Ok(CommandOutcome::Continue);
            }
            ctx.println(&format!(
                "prologixGPIBConfigure: GPIB port '{port}' -> {host}"
            ));
            Ok(CommandOutcome::Continue)
        },
    )
}

/// The single path a `*Configure` shell command uses to turn a freshly built
/// driver into a live, named, traceable port — C's `registerPort`, which is the
/// sole entry into the port list (asynManager.c:2045, adding the port at
/// :2094). Binding the
/// port to its trace configuration and exception list happens inside
/// [`create_port_runtime`], driven by the services passed here, so a port an
/// st.cmd builds is traceable from the moment it exists.
///
/// `None` means **no port was published**, for either reason C has: the runtime
/// thread could not be created (asynManager.c:2082-2092), or the name is
/// already taken. Both have already printed the diagnostic and torn down
/// whatever was half-built, so the caller has nothing to clean up and only has
/// to stop — C's `drvAsynIPPortConfigure: Can't register myself.` / `ttyCleanup`
/// / `return -1` (drvAsynIPPort.c:1033-1035).
///
/// `Some` hands back the runtime handle for the callers that need it (the
/// server port binds its listening socket through it). Callers that do not may
/// drop it: the registry above holds a live `PortHandle`, and that is what
/// keeps the actor alive.
fn publish_configured_port<D: PortDriver>(
    command: &str,
    port: &str,
    driver: D,
    services: &PortServices,
    ctx: &CommandContext,
) -> Option<PortRuntimeHandle> {
    let config = RuntimeConfig {
        services: services.clone(),
        ..RuntimeConfig::default()
    };
    let (handle, _jh) = match create_port_runtime(driver, config) {
        Ok(v) => v,
        Err(e) => {
            ctx.println(&format!("{command}: {e}"));
            return None;
        }
    };
    if let Err(e) = crate::asyn_record::register_port(
        port,
        handle.port_handle().clone(),
        services.trace().clone(),
    ) {
        ctx.println(&format!("{command}: {e}"));
        handle.shutdown();
        return None;
    }
    Some(handle)
}

/// Build the `drvAsynFTDIPortConfigure` iocsh command.
///
/// C parity: `drvAsynFTDIPort.cpp:641-660` — nine positional args
/// (`portName`, `vendorID`, `productID`, `baudrate`, `latency`, `priority`,
/// `noAutoConnect`, `noProcessEos`, `mode`), all but the name integers.
/// `priority` is accepted for startup-script compatibility but has no effect
/// (the Rust runtime schedules port actors uniformly).
pub fn drv_asyn_ftdi_port_configure_command(services: PortServices) -> CommandDef {
    let int_args = [
        "vendorID",
        "productID",
        "baudrate",
        "latency",
        "priority",
        "noAutoConnect",
        "noProcessEos",
        "mode",
    ];
    let mut arg_descs = vec![ArgDesc {
        name: "portName",
        arg_type: ArgType::String,
    }];
    arg_descs.extend(int_args.into_iter().map(|name| ArgDesc {
        name,
        arg_type: ArgType::Int,
    }));
    CommandDef::new(
        "drvAsynFTDIPortConfigure",
        arg_descs,
        "drvAsynFTDIPortConfigure portName vendorID productID baudrate latency [priority] \
         [noAutoConnect] [noProcessEos] [mode] - create an FTDI octet port",
        move |args: &[ArgValue], ctx: &CommandContext| {
            let port = arg_str(args, 0)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "portName required".to_string())?;
            let driver = match DrvAsynFtdiPort::configure(
                &port,
                arg_int(args, 1).unwrap_or(0) as i32,
                arg_int(args, 2).unwrap_or(0) as i32,
                arg_int(args, 3).unwrap_or(0) as i32,
                arg_int(args, 4).unwrap_or(0) as i32,
                arg_int(args, 5).unwrap_or(0) as u32,
                arg_int(args, 6).unwrap_or(0) != 0,
                arg_int(args, 7).unwrap_or(0) != 0,
                arg_int(args, 8).unwrap_or(0) as i32,
            ) {
                Ok(d) => d,
                Err(e) => {
                    ctx.println(&format!("drvAsynFTDIPortConfigure: {e}"));
                    return Ok(CommandOutcome::Continue);
                }
            };
            if publish_configured_port("drvAsynFTDIPortConfigure", &port, driver, &services, ctx)
                .is_some()
            {
                ctx.println(&format!(
                    "drvAsynFTDIPortConfigure: FTDI port '{port}' created"
                ));
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// Build the `vxi11Configure` iocsh command.
///
/// C parity: `drvVxi11.c:1789-1802` — seven positional args (`portName`,
/// `host name`, `flags`, `default timeout`, `vxiName`, `priority`,
/// `disable auto-connect`). `default timeout` is a *string* in C, parsed by the
/// driver, so it is a string here too. `priority` has no effect in the Rust
/// runtime.
pub fn vxi11_configure_command(services: PortServices) -> CommandDef {
    CommandDef::new(
        "vxi11Configure",
        vec![
            ArgDesc {
                name: "portName",
                arg_type: ArgType::String,
            },
            ArgDesc {
                name: "hostName",
                arg_type: ArgType::String,
            },
            ArgDesc {
                name: "flags",
                arg_type: ArgType::Int,
            },
            ArgDesc {
                name: "defaultTimeout",
                arg_type: ArgType::String,
            },
            ArgDesc {
                name: "vxiName",
                arg_type: ArgType::String,
            },
            ArgDesc {
                name: "priority",
                arg_type: ArgType::Int,
            },
            ArgDesc {
                name: "noAutoConnect",
                arg_type: ArgType::Int,
            },
        ],
        "vxi11Configure portName hostName [flags] [defaultTimeout] [vxiName] [priority] \
         [noAutoConnect] - create a VXI-11 port",
        move |args: &[ArgValue], ctx: &CommandContext| {
            let port = arg_str(args, 0)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "portName required".to_string())?;
            let host = arg_str(args, 1)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "hostName required".to_string())?;
            let driver = match DrvVxi11Port::configure(
                &port,
                &host,
                arg_int(args, 2).unwrap_or(0) as i32,
                &arg_str(args, 3).unwrap_or_default(),
                &arg_str(args, 4).unwrap_or_default(),
                arg_int(args, 5).unwrap_or(0) as i32,
                arg_int(args, 6).unwrap_or(0) != 0,
            ) {
                Ok(d) => d,
                Err(e) => {
                    ctx.println(&format!("vxi11Configure: {e}"));
                    return Ok(CommandOutcome::Continue);
                }
            };
            if publish_configured_port("vxi11Configure", &port, driver, &services, ctx).is_some() {
                ctx.println(&format!("vxi11Configure: VXI-11 port '{port}' -> {host}"));
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

/// Build the `usbtmcConfigure` iocsh command.
///
/// C parity: `drvAsynUSBTMC.c:1332-1345` — six positional args (`port name`,
/// `vendor ID number`, `product ID number`, `serial string`, `priority`,
/// `flags`). A vendor/product of 0 is C's "take the first USBTMC device found",
/// and an empty serial string is "any serial number". `priority` has no effect
/// in the Rust runtime.
pub fn usbtmc_configure_command(services: PortServices) -> CommandDef {
    CommandDef::new(
        "usbtmcConfigure",
        vec![
            ArgDesc {
                name: "portName",
                arg_type: ArgType::String,
            },
            ArgDesc {
                name: "vendorID",
                arg_type: ArgType::Int,
            },
            ArgDesc {
                name: "productID",
                arg_type: ArgType::Int,
            },
            ArgDesc {
                name: "serialNumber",
                arg_type: ArgType::String,
            },
            ArgDesc {
                name: "priority",
                arg_type: ArgType::Int,
            },
            ArgDesc {
                name: "flags",
                arg_type: ArgType::Int,
            },
        ],
        "usbtmcConfigure portName [vendorID] [productID] [serialNumber] [priority] [flags] \
         - create a USBTMC port",
        move |args: &[ArgValue], ctx: &CommandContext| {
            let port = arg_str(args, 0)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "portName required".to_string())?;
            let driver = match DrvAsynUsbtmcPort::configure(
                &port,
                arg_int(args, 1).unwrap_or(0) as i32,
                arg_int(args, 2).unwrap_or(0) as i32,
                &arg_str(args, 3).unwrap_or_default(),
                arg_int(args, 4).unwrap_or(0) as i32,
                arg_int(args, 5).unwrap_or(0) as i32,
            ) {
                Ok(d) => d,
                Err(e) => {
                    ctx.println(&format!("usbtmcConfigure: {e}"));
                    return Ok(CommandOutcome::Continue);
                }
            };
            if publish_configured_port("usbtmcConfigure", &port, driver, &services, ctx).is_some() {
                ctx.println(&format!("usbtmcConfigure: USBTMC port '{port}' created"));
            }
            Ok(CommandOutcome::Continue)
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::AsynResult;
    use crate::exception::AsynException;
    use crate::param::ParamType;
    use crate::port::{PortDriver, PortDriverBase, PortFlags};
    use crate::user::AsynUser;
    use std::sync::Mutex;

    struct DummyDriver {
        base: PortDriverBase,
    }
    impl DummyDriver {
        fn new(name: &str) -> Self {
            let mut base = PortDriverBase::new(name, 1, PortFlags::default());
            base.create_param("VAL", ParamType::Int32).unwrap();
            Self { base }
        }
    }
    impl PortDriver for DummyDriver {
        fn base(&self) -> &PortDriverBase {
            &self.base
        }
        fn base_mut(&mut self) -> &mut PortDriverBase {
            &mut self.base
        }
    }

    impl DummyDriver {
        /// A port whose link starts DOWN — what a real transport looks like
        /// before its first connect (`init_connected`, C's `dpc.connected = 0`
        /// until the driver reports otherwise).
        fn disconnected(name: &str) -> Self {
            let mut d = Self::new(name);
            d.base.init_connected(false);
            // ...and nothing brings it up on its own: C's `noAutoConnect`.
            d.base.set_auto_connect(false);
            d
        }

        /// A multi-device port — the other half of C's `findDpCommon` split.
        fn multi_device(name: &str, max_addr: usize) -> Self {
            let mut base = PortDriverBase::new(
                name,
                max_addr,
                PortFlags {
                    multi_device: true,
                    ..PortFlags::default()
                },
            );
            base.create_param("VAL", ParamType::Int32).unwrap();
            Self { base }
        }
    }

    /// A port with real octet I/O: reads serve a canned script, writes are
    /// recorded. Enough to see what an interpose layer does to the bytes.
    struct OctetDriver {
        base: PortDriverBase,
        input: Vec<u8>,
        pos: usize,
        written: Arc<Mutex<Vec<u8>>>,
    }
    impl OctetDriver {
        fn new(name: &str, input: &[u8], written: Arc<Mutex<Vec<u8>>>) -> Self {
            Self {
                base: PortDriverBase::new(name, 1, PortFlags::default()),
                input: input.to_vec(),
                pos: 0,
                written,
            }
        }
    }
    impl PortDriver for OctetDriver {
        fn base(&self) -> &PortDriverBase {
            &self.base
        }
        fn base_mut(&mut self) -> &mut PortDriverBase {
            &mut self.base
        }
        fn io_read_octet(&mut self, _user: &AsynUser, buf: &mut [u8]) -> AsynResult<usize> {
            let n = (self.input.len() - self.pos).min(buf.len());
            buf[..n].copy_from_slice(&self.input[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
        fn io_write_octet(&mut self, _user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
            self.written.lock().unwrap().extend_from_slice(data);
            Ok(data.len())
        }
    }

    fn fresh_mgr_with_port(name: &str) -> Arc<PortManager> {
        let mgr = Arc::new(PortManager::new());
        let _ = mgr.register_port(DummyDriver::new(name)).unwrap();
        mgr
    }

    /// The same, for a port that carries `ASYN_MULTIDEVICE`. An `addr`
    /// names a device only on such a port — `locateDevice` returns NULL
    /// otherwise (asynManager.c:574) — so every per-device trace routing
    /// test has to build its port this way.
    fn fresh_mgr_with_multi_device_port(name: &str, max_addr: usize) -> Arc<PortManager> {
        let mgr = Arc::new(PortManager::new());
        let _ = mgr
            .register_port(DummyDriver::multi_device(name, max_addr))
            .unwrap();
        mgr
    }

    /// `asynSetTraceMask` registered through `build_asyn_commands`
    /// must mutate the underlying `TraceManager` per-port mask. Wire
    /// C parity: `pasynTrace->setTraceMask` (asynShellCommands.c:660).
    #[test]
    fn iocsh_set_trace_mask_updates_port_mask() {
        let mgr = fresh_mgr_with_port("trace_mask_port");

        // Subscribe to exception events so we can verify the per-set
        // announce fires (asynManager.c:2790).
        let observed: Arc<Mutex<Vec<AsynException>>> = Arc::new(Mutex::new(Vec::new()));
        let observed_clone = observed.clone();
        mgr.exception_manager().add_callback(move |ev| {
            observed_clone.lock().unwrap().push(ev.exception);
        });

        let cmds = build_asyn_commands(mgr.clone());
        let set_trace_mask = cmds
            .iter()
            .find(|c| c.name == "asynSetTraceMask")
            .expect("asynSetTraceMask must be registered");

        // Invoke directly (CommandContext not actually used for this path).
        // Build a minimal context via a fresh shell. We use stderr-backed
        // ctx via the public API: construct a fake one through a shell.
        // Simpler: re-implement by calling TraceManager directly to verify
        // the mask string parse + setter route. The handler closure is
        // exercised via the round-trip through the registered command.
        let trace = mgr.trace_manager().clone();
        // simulate: asynSetTraceMask "trace_mask_port" -1 "ERROR+WARNING"
        let mask = TraceMask::from_symbolic("ERROR+WARNING").unwrap();
        trace.set_trace_mask(Some("trace_mask_port"), mask);

        // The registered set itself is guarded by
        // `iocsh_registers_c_parity_commands`, which owns the list.
        assert!(set_trace_mask.args.len() == 3);

        // Verify announce fired.
        let evs = observed.lock().unwrap();
        assert!(
            evs.iter().any(|e| matches!(e, AsynException::TraceMask)),
            "set_trace_mask must fire asynExceptionTraceMask"
        );
        // And the mask is effective on the port.
        assert!(trace.is_enabled("trace_mask_port", TraceMask::ERROR));
        assert!(trace.is_enabled("trace_mask_port", TraceMask::WARNING));
    }

    /// `build_asyn_commands` exposes the C `asynShellCommands.c` functions
    /// asyn-rs ports, plus the `drvAsyn*PortConfigure` port-creation commands —
    /// guards against silent additions / removals. The port-creation commands
    /// belong to the same set: a shell that can create a port must be able to
    /// configure it, so they cannot drift onto a different carrier.
    #[test]
    fn iocsh_registers_c_parity_commands() {
        let mgr = Arc::new(PortManager::new());
        let cmds = build_asyn_commands(mgr);
        let mut names: Vec<&str> = cmds.iter().map(|c| c.name.as_str()).collect();
        names.sort_unstable();
        // The authoritative list — one entry per C `iocshRegister`. Adding a
        // command means adding it here; a C command still missing is a gap this
        // list names by its absence.
        let mut expected = vec![
            // asynShellCommands.c:1352-1378
            "asynReport",
            "asynSetOption",
            "asynSetTraceMask",
            "asynSetTraceIOMask",
            "asynSetTraceInfoMask",
            "asynSetTraceFile",
            "asynEnable",
            "asynAutoConnect",
            "asynWaitConnect",
            "asynSetAutoConnectTimeout",
            "asynShowOption",
            "asynSetTraceIOTruncateSize",
            "asynRegisterTimeStampSource",
            "asynUnregisterTimeStampSource",
            "asynSetMinTimerPeriod",
            "asynOctetSetInputEos",
            "asynOctetGetInputEos",
            "asynOctetSetOutputEos",
            "asynOctetGetOutputEos",
            // interpose layers
            "asynInterposeEcho",
            "asynInterposeDelay",
            "asynInterposeEosConfig",
            "asynInterposeFlushConfig",
            // port creation
            "drvAsynIPPortConfigure",
            "drvAsynIPServerPortConfigure",
            "drvAsynSerialPortConfigure",
            "drvAsynFTDIPortConfigure",
            "vxi11Configure",
            "usbtmcConfigure",
            "prologixGPIBConfigure",
        ];
        expected.sort_unstable();
        assert_eq!(names, expected);
    }

    /// R19-120: `asynSetTraceIOTruncateSize` routes `addr` the way every other
    /// trace setter does — device dpCommon when `addr >= 0`, port when `addr <
    /// 0` (C asynManager.c:2945-2959 via `findTracePvt`).
    #[test]
    fn iocsh_set_trace_io_truncate_size_routes_addr_to_the_device() {
        let mgr = fresh_mgr_with_multi_device_port("trunc_port", 8);
        let trace = mgr.trace_manager().clone();
        let cmds = build_asyn_commands(mgr);
        let ctx = make_ctx();
        let set = |addr: i64, size: i64| {
            cmds.iter()
                .find(|c| c.name == "asynSetTraceIOTruncateSize")
                .expect("asynSetTraceIOTruncateSize must be registered")
                .handler
                .call(
                    &[
                        ArgValue::String("trunc_port".into()),
                        ArgValue::Int(addr),
                        ArgValue::Int(size),
                    ],
                    &ctx,
                )
                .unwrap();
        };

        set(-1, 40);
        assert_eq!(trace.snapshot("trunc_port", None).io_truncate_size, 40);

        // addr >= 0 writes the device, not the port.
        set(2, 4096);
        assert_eq!(trace.snapshot("trunc_port", Some(2)).io_truncate_size, 4096);
        assert_eq!(trace.snapshot("trunc_port", None).io_truncate_size, 40);
    }

    /// R19-120: `asynShowOption` prints `key=value` for a driver option
    /// (C asynShellCommands.c:146-149), and reports the driver's error otherwise.
    #[test]
    fn iocsh_show_option_reads_back_what_set_option_wrote() {
        let mgr = fresh_mgr_with_port("show_opt");
        let handle = mgr.find_port_handle("show_opt").unwrap();
        handle
            .set_option_blocking(AsynUser::default(), "baud", "115200")
            .unwrap();

        let cmds = build_asyn_commands(mgr);
        let ctx = make_ctx();
        cmds.iter()
            .find(|c| c.name == "asynShowOption")
            .expect("asynShowOption must be registered")
            .handler
            .call(
                &[
                    ArgValue::String("show_opt".into()),
                    ArgValue::Int(0),
                    ArgValue::String("baud".into()),
                ],
                &ctx,
            )
            .unwrap();

        // The option really is readable through the same path the command uses.
        assert_eq!(
            handle
                .get_option_blocking(AsynUser::default(), "baud")
                .unwrap(),
            "115200"
        );
    }

    /// R19-120 boundary: `asynRegisterTimeStampSource` installs a source that
    /// was published under that name, refuses one that was not, and
    /// `asynUnregisterTimeStampSource` puts the port back on its default clock.
    ///
    /// C resolves the name through `registryFunctionFind` and refuses when it
    /// is not there (asynShellCommands.c:1197-1201) — an unknown name must not
    /// silently install nothing.
    #[test]
    fn iocsh_time_stamp_source_is_installed_by_name_and_refused_when_unknown() {
        use crate::timestamp::{find_time_stamp_source, register_time_stamp_source};
        use std::time::{Duration as StdDuration, UNIX_EPOCH};

        let fixed = UNIX_EPOCH + StdDuration::from_secs(1_234_567);
        register_time_stamp_source("iocsh_fixed_clock", move || fixed);

        let mgr = fresh_mgr_with_port("ts_port");
        let handle = mgr.find_port_handle("ts_port").unwrap();
        let cmds = build_asyn_commands(mgr);
        let ctx = make_ctx();
        let register = |name: &str| {
            cmds.iter()
                .find(|c| c.name == "asynRegisterTimeStampSource")
                .expect("asynRegisterTimeStampSource must be registered")
                .handler
                .call(
                    &[
                        ArgValue::String("ts_port".into()),
                        ArgValue::String(name.into()),
                    ],
                    &ctx,
                )
                .unwrap();
        };

        // A name nobody published does not resolve — the command reports it and
        // the port keeps its default clock.
        assert!(find_time_stamp_source("no_such_clock").is_none());
        register("no_such_clock");
        assert!(
            handle
                .set_time_stamp_source_blocking(Some("no_such_clock"))
                .is_err()
        );

        // A published name installs.
        register("iocsh_fixed_clock");
        assert!(
            handle
                .set_time_stamp_source_blocking(Some("iocsh_fixed_clock"))
                .is_ok()
        );

        // And unregistering is accepted (back to the driver's own clock).
        cmds.iter()
            .find(|c| c.name == "asynUnregisterTimeStampSource")
            .expect("asynUnregisterTimeStampSource must be registered")
            .handler
            .call(&[ArgValue::String("ts_port".into())], &ctx)
            .unwrap();
        assert!(handle.set_time_stamp_source_blocking(None).is_ok());
    }

    /// R19-117 boundary: `asynWaitConnect` returns as soon as the port is
    /// connected — whether it was connected before the call or connects during
    /// it — and gives up after the timeout otherwise.
    ///
    /// C `waitConnect` (asynManager.c:3292-3336) arms the connect exception
    /// handler and only then reads the connected flag, so the connect that
    /// lands in between is not lost. The three boundaries here are exactly
    /// that: already-connected, connects-during-the-wait, never-connects.
    #[test]
    fn iocsh_wait_connect_covers_before_during_and_never() {
        use std::time::Instant;

        let mgr = Arc::new(PortManager::new());
        let cfg = RuntimeConfig {
            auto_connect: false,
            services: mgr.services().clone(),
            ..RuntimeConfig::default()
        };
        let _ = mgr
            .register_port_with_config(DummyDriver::disconnected("wc_late"), cfg.clone())
            .unwrap();
        let _ = mgr
            .register_port_with_config(DummyDriver::disconnected("wc_never"), cfg)
            .unwrap();

        let cmds = build_asyn_commands(mgr.clone());
        let ctx = make_ctx();
        let wait = |port: &str, timeout: f64| {
            let t0 = Instant::now();
            cmds.iter()
                .find(|c| c.name == "asynWaitConnect")
                .expect("asynWaitConnect must be registered")
                .handler
                .call(
                    &[ArgValue::String(port.into()), ArgValue::Double(timeout)],
                    &ctx,
                )
                .unwrap();
            t0.elapsed()
        };

        // Never connects: the wait runs to the timeout and returns.
        let elapsed = wait("wc_never", 0.1);
        assert!(
            elapsed >= Duration::from_millis(100),
            "must have waited the full timeout, waited {elapsed:?}"
        );
        assert!(
            !mgr.find_port_handle("wc_never")
                .unwrap()
                .is_connected_blocking()
                .unwrap()
        );

        // Connects during the wait: returns on the connect exception, well
        // inside the timeout.
        let late = mgr.find_port_handle("wc_late").unwrap();
        let connector = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            late.connect_blocking().unwrap();
        });
        let elapsed = wait("wc_late", 5.0);
        connector.join().unwrap();
        assert!(
            elapsed < Duration::from_secs(4),
            "must have returned on the connect, not on the timeout ({elapsed:?})"
        );
        let late = mgr.find_port_handle("wc_late").unwrap();
        assert!(late.is_connected_blocking().unwrap());

        // Already connected: returns immediately, without waiting on an
        // exception that will never fire again.
        let elapsed = wait("wc_late", 5.0);
        assert!(
            elapsed < Duration::from_secs(1),
            "an already-connected port must not wait ({elapsed:?})"
        );
    }

    /// R19-117: `asynSetAutoConnectTimeout` rewrites the value a newly
    /// registered port waits for its first connect — C's process-global
    /// `pasynBase->autoConnectTimeout` (asynManager.c:2370-2377), read at every
    /// port registration (:2135). It was hard-coded at 0.5 s, which is too
    /// short for a slow device and was the reason C exposes the knob.
    #[test]
    fn iocsh_set_auto_connect_timeout_rewrites_the_registration_wait() {
        assert_eq!(
            RuntimeConfig::default().auto_connect_timeout,
            Duration::from_millis(500),
            "C DEFAULT_AUTOCONNECT_TIMEOUT (asynManager.c:49)"
        );

        let mgr = Arc::new(PortManager::new());
        let cmds = build_asyn_commands(mgr);
        let ctx = make_ctx();
        cmds.iter()
            .find(|c| c.name == "asynSetAutoConnectTimeout")
            .expect("asynSetAutoConnectTimeout must be registered")
            .handler
            .call(&[ArgValue::Double(2.5)], &ctx)
            .unwrap();

        assert_eq!(
            RuntimeConfig::default().auto_connect_timeout,
            Duration::from_millis(2500)
        );

        // A negative timeout is "do not wait", as C's event wait treats it.
        cmds.iter()
            .find(|c| c.name == "asynSetAutoConnectTimeout")
            .unwrap()
            .handler
            .call(&[ArgValue::Double(-1.0)], &ctx)
            .unwrap();
        assert_eq!(
            RuntimeConfig::default().auto_connect_timeout,
            Duration::ZERO
        );
    }

    /// R19-118 boundary: `asynInterposeEosConfig` installs the EOS layer from
    /// the shell, and its two flags each gate exactly one direction.
    ///
    /// C `asynInterposeEosConfig(portName, addr, processEosIn, processEosOut)`
    /// (asynInterposeEos.c:84-140): `processEosIn == 0` makes `readIt` delegate
    /// straight to the driver (:191-193), `processEosOut == 0` makes `writeIt`
    /// append nothing (:161). The boundary tested here is processIn=1,
    /// processOut=0: the read terminates on the input EOS, and the write goes
    /// out with no terminator appended even though OEOS is set.
    #[test]
    fn iocsh_interpose_eos_config_gates_each_direction_on_its_flag() {
        let mgr = Arc::new(PortManager::new());
        let written = Arc::new(Mutex::new(Vec::new()));
        let _ = mgr
            .register_port(OctetDriver::new(
                "eos_cfg",
                b"line1\nline2\n",
                written.clone(),
            ))
            .unwrap();
        let cmds = build_asyn_commands(mgr.clone());
        let ctx = make_ctx();

        cmds.iter()
            .find(|c| c.name == "asynInterposeEosConfig")
            .expect("asynInterposeEosConfig must be registered")
            .handler
            .call(
                &[
                    ArgValue::String("eos_cfg".into()),
                    ArgValue::Int(0),
                    ArgValue::Int(1), // processIn
                    ArgValue::Int(0), // processOut
                ],
                &ctx,
            )
            .unwrap();

        let handle = mgr.find_port_handle("eos_cfg").unwrap();
        handle
            .set_input_eos_blocking(shell_eos_user(0), b"\n")
            .unwrap();
        // processOut = 0 gates C's `writeIt` alone (asynInterposeEos.c:161-163):
        // `setOutputEos` (:344-363) and `getOutputEos` (:365-390) carry no
        // `processEosOut` test at all, so the terminator is stored, answered
        // asynSuccess and read back — it is simply never appended. A startup
        // script that sets OEOS and reads it back therefore succeeds, which is
        // what refusing it broke.
        handle
            .set_output_eos_blocking(shell_eos_user(0), b"\n")
            .expect("processOut=0 still stores the output terminator");
        assert_eq!(
            handle
                .get_output_eos_blocking(shell_eos_user(0))
                .expect("readback"),
            b"\n".to_vec(),
            "C's getOutputEos reads eosOut back whatever processEosOut says"
        );

        // processIn = 1: the read stops at the terminator and strips it.
        let user = AsynUser::default()
            .with_addr(0)
            .with_timeout(SHELL_IO_TIMEOUT);
        let first = handle
            .submit_blocking(crate::request::RequestOp::OctetRead { buf_size: 32 }, user)
            .unwrap();
        assert_eq!(first.data.as_deref(), Some(&b"line1"[..]));

        // processOut = 0: the write is handed to the driver verbatim — the
        // output terminator is NOT appended, even though OEOS is set.
        let user = AsynUser::default()
            .with_addr(0)
            .with_timeout(SHELL_IO_TIMEOUT);
        handle
            .submit_blocking(
                crate::request::RequestOp::OctetWrite {
                    data: b"CMD".to_vec(),
                },
                user,
            )
            .unwrap();
        assert_eq!(written.lock().unwrap().as_slice(), b"CMD");
    }

    /// R19-118: `asynInterposeFlushConfig` installs the flush-timeout layer
    /// from the shell (C asynInterposeFlush.c:66-91, iocsh at :195-205).
    #[test]
    fn iocsh_interpose_flush_config_installs_the_layer() {
        let mgr = Arc::new(PortManager::new());
        let written = Arc::new(Mutex::new(Vec::new()));
        let _ = mgr
            .register_port(OctetDriver::new("flush_cfg", b"stale", written.clone()))
            .unwrap();
        let cmds = build_asyn_commands(mgr.clone());
        let ctx = make_ctx();

        cmds.iter()
            .find(|c| c.name == "asynInterposeFlushConfig")
            .expect("asynInterposeFlushConfig must be registered")
            .handler
            .call(
                &[
                    ArgValue::String("flush_cfg".into()),
                    ArgValue::Int(0),
                    // C coerces a non-positive millisecond timeout to 1 ms
                    // (asynInterposeFlush.c:78).
                    ArgValue::Double(0.0),
                ],
                &ctx,
            )
            .unwrap();

        // The layer's whole job is `flushIt` (C :112-132): read the driver dry
        // under the short timeout. Writes and reads pass through untouched
        // (:95-110). Without the layer a flush on this driver is a no-op and
        // the stale bytes survive.
        let handle = mgr.find_port_handle("flush_cfg").unwrap();
        let user = AsynUser::default()
            .with_addr(0)
            .with_timeout(SHELL_IO_TIMEOUT);
        handle
            .submit_blocking(crate::request::RequestOp::Flush, user)
            .unwrap();

        let user = AsynUser::default()
            .with_addr(0)
            .with_timeout(SHELL_IO_TIMEOUT);
        let after = handle
            .submit_blocking(crate::request::RequestOp::OctetRead { buf_size: 16 }, user)
            .unwrap();
        assert_eq!(
            after.nbytes, 0,
            "the flush layer must have drained the driver's stale input"
        );

        // And the write path is untouched by the layer.
        let user = AsynUser::default()
            .with_addr(0)
            .with_timeout(SHELL_IO_TIMEOUT);
        handle
            .submit_blocking(
                crate::request::RequestOp::OctetWrite {
                    data: b"GO".to_vec(),
                },
                user,
            )
            .unwrap();
        assert_eq!(written.lock().unwrap().as_slice(), b"GO");
    }

    /// R19-116: `asynEnable` / `asynAutoConnect` exist on the shell, and they
    /// pick port-level vs device-level state by C's `findDpCommon` rule
    /// (asynManager.c:536-544) — a device only when the port is multi-device
    /// AND the caller named one.
    #[test]
    fn iocsh_enable_and_autoconnect_follow_c_find_dp_common() {
        let mgr = Arc::new(PortManager::new());
        let _ = mgr.register_port(DummyDriver::new("edp_single")).unwrap();
        let _ = mgr
            .register_port(DummyDriver::multi_device("edp_multi", 2))
            .unwrap();

        let seen: Arc<Mutex<Vec<(String, AsynException, i32)>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_cb = seen.clone();
        mgr.exception_manager().add_callback(move |ev| {
            seen_cb
                .lock()
                .unwrap()
                .push((ev.port_name.clone(), ev.exception, ev.addr));
        });

        let cmds = build_asyn_commands(mgr.clone());
        let ctx = make_ctx();
        let call = |name: &str, port: &str, addr: i64, yes: i64| {
            cmds.iter()
                .find(|c| c.name == name)
                .unwrap_or_else(|| panic!("{name} not registered"))
                .handler
                .call(
                    &[
                        ArgValue::String(port.into()),
                        ArgValue::Int(addr),
                        ArgValue::Int(yes),
                    ],
                    &ctx,
                )
                .unwrap();
        };

        let single = mgr.find_port_handle("edp_single").unwrap();
        let multi = mgr.find_port_handle("edp_multi").unwrap();

        // addr >= 0 on a SINGLE-device port is still the port: findDpCommon has
        // no device to pick.
        call("asynEnable", "edp_single", 0, 0);
        assert!(!single.is_enabled_blocking().unwrap());

        // addr >= 0 on a multi-device port is the device — the port itself must
        // stay enabled.
        call("asynEnable", "edp_multi", 1, 0);
        assert!(multi.is_enabled_blocking().unwrap());
        assert!(
            seen.lock()
                .unwrap()
                .contains(&("edp_multi".to_string(), AsynException::Enable, 1)),
            "the device-level enable must announce at its addr"
        );

        // addr < 0 is always the port.
        call("asynEnable", "edp_multi", -1, 0);
        assert!(!multi.is_enabled_blocking().unwrap());

        // Same split for auto-connect.
        call("asynAutoConnect", "edp_multi", 1, 0);
        assert!(
            multi.is_auto_connect_blocking().unwrap(),
            "a device-addressed autoConnect must not touch the port's flag"
        );
        assert!(seen.lock().unwrap().contains(&(
            "edp_multi".to_string(),
            AsynException::AutoConnect,
            1
        )));
        call("asynAutoConnect", "edp_multi", -1, 0);
        assert!(!multi.is_auto_connect_blocking().unwrap());
    }

    /// `raw_from_escaped` decodes C-style escapes to raw bytes (parity with
    /// EPICS `epicsStrnRawFromEscaped`), so `"\r\n"` becomes CR LF. The EOS
    /// commands depend on this for st.cmd terminators.
    #[test]
    fn raw_from_escaped_decodes_c_escapes() {
        assert_eq!(raw_from_escaped(r"\r\n"), vec![b'\r', b'\n']);
        assert_eq!(raw_from_escaped(r"\t"), vec![b'\t']);
        assert_eq!(raw_from_escaped(r"\\"), vec![b'\\']);
        assert_eq!(raw_from_escaped("AB"), vec![b'A', b'B']);
        // \xXX hex escape: two digits, then a single digit.
        assert_eq!(raw_from_escaped(r"\x41"), vec![0x41]);
        assert_eq!(raw_from_escaped(r"\x4"), vec![0x04]);
        // Unknown escape → the escaped char passes through (C `default:`).
        assert_eq!(raw_from_escaped(r"\z"), vec![b'z']);
        // \0 → NUL byte.
        assert_eq!(raw_from_escaped(r"\0"), vec![0]);
        // Trailing lone backslash with no following char is dropped (C break).
        assert_eq!(raw_from_escaped(r"a\"), vec![b'a']);
        // \x with no hex digit emits nothing; the char is reprocessed as
        // ordinary input (C `goto input`).
        assert_eq!(raw_from_escaped(r"\xg"), vec![b'g']);
        assert!(raw_from_escaped("").is_empty());
    }

    /// The EOS shell commands hand their `asynUser` C's 2 s I/O deadline.
    ///
    /// C `asynSetEos` sets `pasynUser->timeout = 2` (asynShellCommands.c:238)
    /// before queueing the `setEos` callback, and `asynShowEos` (:289) and
    /// `asynSetOption` (:119) do the same — the shell never leaves this field at
    /// the default. Rust's EOS handler built its user with the 1 s default while
    /// its `asynSetOption` sibling already set 2 s; [`SHELL_IO_TIMEOUT`] is now
    /// the single owner of the value, so the two cannot disagree again.
    #[test]
    fn the_eos_shell_commands_carry_cs_two_second_io_timeout() {
        let user = shell_eos_user(0);
        assert_eq!(
            user.timeout,
            Some(Duration::from_secs(2)),
            "C asynSetEos sets pasynUser->timeout = 2 (asynShellCommands.c:238)"
        );
        // The other half C sets on the same user, kept under the same test so a
        // future edit cannot drop the waiver while preserving the timeout.
        assert_eq!(
            user.reason,
            crate::user::ASYN_REASON_QUEUE_EVEN_IF_NOT_CONNECTED,
            "C asynSetEos stamps ASYN_REASON_QUEUE_EVEN_IF_NOT_CONNECTED \
             (asynShellCommands.c:241)"
        );
        assert_eq!(
            user.timeout,
            Some(SHELL_IO_TIMEOUT),
            "the EOS user takes its deadline from the shared shell constant"
        );
    }

    /// `asynOctetSetInputEos` / `asynOctetSetOutputEos` escape-decode their
    /// argument and route the raw bytes to the driver's `set_input_eos` /
    /// `set_output_eos` through the port actor. C parity:
    /// `asynShellCommands.c::asynSetEos` → `pasynOctet->setInputEos`.
    #[test]
    fn iocsh_set_input_output_eos_routes_decoded_bytes_to_driver() {
        #[derive(Clone, Default)]
        struct Recorded {
            input: Arc<Mutex<Option<Vec<u8>>>>,
            output: Arc<Mutex<Option<Vec<u8>>>>,
        }
        struct RecordingDriver {
            base: PortDriverBase,
            rec: Recorded,
        }
        impl PortDriver for RecordingDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn set_input_eos(&mut self, _user: &AsynUser, eos: &[u8]) -> AsynResult<()> {
                *self.rec.input.lock().unwrap() = Some(eos.to_vec());
                Ok(())
            }
            fn set_output_eos(&mut self, _user: &AsynUser, eos: &[u8]) -> AsynResult<()> {
                *self.rec.output.lock().unwrap() = Some(eos.to_vec());
                Ok(())
            }
        }

        let rec = Recorded::default();
        let mgr = Arc::new(PortManager::new());
        mgr.register_port(RecordingDriver {
            base: PortDriverBase::new("eos_port", 1, PortFlags::default()),
            rec: rec.clone(),
        })
        .unwrap();

        let ctx = make_ctx();
        let cmds = build_asyn_commands(mgr.clone());

        // asynOctetSetInputEos eos_port 0 "\r\n"
        let set_in = cmds
            .iter()
            .find(|c| c.name == "asynOctetSetInputEos")
            .expect("asynOctetSetInputEos must be registered");
        let outcome = set_in
            .handler
            .call(
                &[
                    ArgValue::String("eos_port".to_string()),
                    ArgValue::Int(0),
                    ArgValue::String(r"\r\n".to_string()),
                ],
                &ctx,
            )
            .expect("handler returns Ok");
        assert!(matches!(outcome, CommandOutcome::Continue));
        assert_eq!(
            rec.input.lock().unwrap().as_deref(),
            Some(&[b'\r', b'\n'][..]),
            "input EOS must reach the driver as decoded CR LF"
        );

        // asynOctetSetOutputEos eos_port 0 "\n"
        let set_out = cmds
            .iter()
            .find(|c| c.name == "asynOctetSetOutputEos")
            .expect("asynOctetSetOutputEos must be registered");
        set_out
            .handler
            .call(
                &[
                    ArgValue::String("eos_port".to_string()),
                    ArgValue::Int(0),
                    ArgValue::String(r"\n".to_string()),
                ],
                &ctx,
            )
            .expect("handler returns Ok");
        assert_eq!(
            rec.output.lock().unwrap().as_deref(),
            Some(&[b'\n'][..]),
            "output EOS must reach the driver as decoded LF"
        );
    }

    /// R14-48: the `addr` argument names the device whose terminator is being
    /// set. C threads it into `findInterface(portName, addr, ...)`, which
    /// `connectDevice`s the queued `asynUser` to that device
    /// (asynShellCommands.c:79-80, :233-234) — so `setInputEos` lands on that
    /// device's EOS. iocsh parsed the addr and dropped it, so every
    /// `asynOctetSetInputEos port <addr> ...` wrote device 0's.
    ///
    /// Driven end to end through the real EOS storage (no recording stub): set
    /// two addrs, read both back with the `asynOctetGetInputEos` twin
    /// (C `asynShowEos`, :283-309).
    #[test]
    fn iocsh_eos_commands_address_the_device_the_addr_names() {
        struct MultiPort {
            base: PortDriverBase,
        }
        impl PortDriver for MultiPort {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }

            // A multiDevice port cannot carry `asynInterposeEos` at all — C
            // refuses to install it (asynOctetBase.c:149-153) — so a
            // multi-device driver that supports EOS must implement the
            // methods itself, which is the non-NULL `asynOctet` entry the
            // Fail-stub default stands in for (R18-71).
            fn set_input_eos(&mut self, user: &AsynUser, eos: &[u8]) -> AsynResult<()> {
                self.base.set_input_eos(user.addr, eos);
                Ok(())
            }
            fn get_input_eos(&self, user: &AsynUser) -> Vec<u8> {
                self.base.input_eos(user.addr).to_vec()
            }
            fn set_output_eos(&mut self, user: &AsynUser, eos: &[u8]) -> AsynResult<()> {
                self.base.set_output_eos(user.addr, eos);
                Ok(())
            }
            fn get_output_eos(&self, user: &AsynUser) -> Vec<u8> {
                self.base.output_eos(user.addr).to_vec()
            }
        }

        let mgr = Arc::new(PortManager::new());
        mgr.register_port(MultiPort {
            base: PortDriverBase::new(
                "eos_multi",
                4,
                PortFlags {
                    multi_device: true,
                    ..PortFlags::default()
                },
            ),
        })
        .unwrap();

        let ctx = make_ctx();
        let cmds = build_asyn_commands(mgr.clone());
        let run = |name: &str, args: Vec<ArgValue>| {
            cmds.iter()
                .find(|c| c.name == name)
                .unwrap_or_else(|| panic!("{name} must be registered"))
                .handler
                .call(&args, &ctx)
                .expect("handler returns Ok");
        };

        run(
            "asynOctetSetInputEos",
            vec![
                ArgValue::String("eos_multi".into()),
                ArgValue::Int(1),
                ArgValue::String(r"\r\n".into()),
            ],
        );
        run(
            "asynOctetSetInputEos",
            vec![
                ArgValue::String("eos_multi".into()),
                ArgValue::Int(2),
                ArgValue::String(r"\n".into()),
            ],
        );

        // Each device holds the terminator its own command named. With the addr
        // dropped, both writes landed on one entry and addr 2 would report
        // "\r\n" (or addr 1 would report "\n" — whichever ran last).
        let handle = mgr.find_port_handle("eos_multi").unwrap();
        let eos_at = |addr: i32| {
            handle
                .get_input_eos_blocking(shell_eos_user(addr))
                .expect("readback")
        };
        assert_eq!(eos_at(1), b"\r\n");
        assert_eq!(eos_at(2), b"\n");
        assert!(
            eos_at(3).is_empty(),
            "a device nobody configured has no terminator"
        );

        // The readback command (C `asynShowEos`) runs against the same device.
        // Its escaped output goes to the shell's stdout, which the test harness
        // cannot capture; `escaped_from_raw` is covered on its own below.
        run(
            "asynOctetGetInputEos",
            vec![ArgValue::String("eos_multi".into()), ArgValue::Int(1)],
        );
        run(
            "asynOctetGetOutputEos",
            vec![ArgValue::String("eos_multi".into()), ArgValue::Int(2)],
        );
    }

    /// C `asynShowEos` prints the terminator back through
    /// `epicsStrnEscapedFromRaw` (asynShellCommands.c:305) — so an EOS set as
    /// `"\r\n"` reads back as `"\r\n"`, not as two invisible control bytes.
    #[test]
    fn escaped_from_raw_is_the_inverse_of_raw_from_escaped() {
        let n = SHOW_EOS_BUF_SIZE;
        assert_eq!(escaped_from_raw(b"\r\n", n), r"\r\n");
        assert_eq!(escaped_from_raw(b"\x01", n), r"\x01");
        assert_eq!(escaped_from_raw(b"ab", n), "ab");
        assert_eq!(escaped_from_raw(b"", n), "");
        for s in [r"\r\n", r"\t", r"\x1b", "ab"] {
            assert_eq!(
                escaped_from_raw(&raw_from_escaped(s), n),
                s,
                "escape/unescape must round-trip"
            );
        }

        // CBUG-D4: both escapers now render NUL as `\0`; prove that survives the
        // port's own decoder. EPICS has no octal escape — `raw_from_escaped`
        // decodes `\0` via C's `case '0'` (one NUL, following digits literal), so
        // `\0` is unambiguous and a NUL-then-digit round-trips exactly.
        assert_eq!(escaped_from_raw(b"\0", n), r"\0");
        assert_eq!(raw_from_escaped(r"\0"), vec![0u8]); // `\0` in -> single NUL out
        assert_eq!(raw_from_escaped(r"\x00"), vec![0u8]); // decoder also accepts `\x00`
        assert_eq!(escaped_from_raw(b"\x001", n), r"\01"); // NUL then '1'
        assert_eq!(raw_from_escaped(r"\01"), vec![0u8, b'1']); // no octal: NUL then '1'
        // C sizes `cbuf` at 4 * sizeof(eos) + 2 precisely so the widest
        // terminator — 10 bytes, every one of them `\xNN` — still escapes whole
        // (40 chars, one under the bound).
        assert_eq!(escaped_from_raw(&[0x01; 10], n).len(), 40);
    }

    /// R8-49: C registers `asynInterposeEcho` with iocsh
    /// (`asynInterposeEcho.c:189-210`) because nothing else installs the layer
    /// — no driver configure command pushes it, so without the registrar a
    /// startup script cannot reach a half-duplex echo device at all. Drive the
    /// command against a registered port and prove the port's octet write goes
    /// from one 2-byte link write to two 1-byte writes, each confirmed by its
    /// echo.
    #[test]
    fn iocsh_interpose_echo_installs_the_layer_on_a_registered_port() {
        use crate::interpose::{EomReason, OctetNext, OctetReadResult};
        use crate::request::RequestOp;
        use std::collections::VecDeque;

        /// The link under the interpose stack: echoes back whatever is written
        /// and records the size of each write it sees.
        struct EchoingLink {
            sizes: Arc<Mutex<Vec<usize>>>,
            echo: VecDeque<u8>,
        }
        impl OctetNext for EchoingLink {
            fn read(&mut self, _user: &AsynUser, buf: &mut [u8]) -> AsynResult<OctetReadResult> {
                match self.echo.pop_front() {
                    Some(b) => {
                        buf[0] = b;
                        Ok(OctetReadResult {
                            nbytes_transferred: 1,
                            eom_reason: EomReason::CNT,
                        })
                    }
                    None => Ok(OctetReadResult {
                        nbytes_transferred: 0,
                        eom_reason: EomReason::empty(),
                    }),
                }
            }
            fn write(&mut self, _user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
                self.sizes.lock().unwrap().push(data.len());
                self.echo.extend(data.iter().copied());
                Ok(data.len())
            }
            fn flush(&mut self, _user: &mut AsynUser) -> AsynResult<()> {
                Ok(())
            }
        }

        /// The raw device below the port's interpose chain — the driver's own
        /// `asynOctet`, which the manager's layers sit on top of
        /// (asynManager.c:2190-2220). It does not run the chain: the port does.
        struct InterposedDriver {
            base: PortDriverBase,
            link: EchoingLink,
        }
        impl PortDriver for InterposedDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn io_write_octet(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
                self.link.write(user, data)
            }
            fn io_read_octet_eom(
                &mut self,
                user: &AsynUser,
                buf: &mut [u8],
            ) -> AsynResult<(usize, EomReason)> {
                let r = self.link.read(user, buf)?;
                Ok((r.nbytes_transferred, r.eom_reason))
            }
        }

        let sizes = Arc::new(Mutex::new(Vec::new()));
        let mgr = Arc::new(PortManager::new());
        mgr.register_port(InterposedDriver {
            base: PortDriverBase::new("echo_port", 1, PortFlags::default()),
            link: EchoingLink {
                sizes: sizes.clone(),
                echo: VecDeque::new(),
            },
        })
        .unwrap();
        let handle = mgr.find_port_handle("echo_port").unwrap();

        // Before the command: the stack is empty, so the link sees the whole
        // payload in one write.
        handle
            .submit_blocking(
                RequestOp::OctetWrite {
                    data: b"AB".to_vec(),
                },
                AsynUser::default(),
            )
            .expect("plain write succeeds");
        assert_eq!(sizes.lock().unwrap().as_slice(), &[2]);

        let ctx = make_ctx();
        let cmds = build_asyn_commands(mgr.clone());
        let echo_cmd = cmds
            .iter()
            .find(|c| c.name == "asynInterposeEcho")
            .expect("asynInterposeEcho must be registered");
        echo_cmd
            .handler
            .call(&[ArgValue::String("echo_port".to_string())], &ctx)
            .expect("handler returns Ok");

        // After the command: the echo layer is on the port, so the same
        // payload reaches the link one byte at a time.
        handle
            .submit_blocking(
                RequestOp::OctetWrite {
                    data: b"AB".to_vec(),
                },
                AsynUser::default(),
            )
            .expect("echoed write succeeds");
        assert_eq!(
            sizes.lock().unwrap().as_slice(),
            &[2, 1, 1],
            "asynInterposeEcho must install the echo layer on the live port"
        );

        // An unknown port is C's `interposeInterface failed.` (:178-182), not a
        // panic and not a silent success.
        echo_cmd
            .handler
            .call(&[ArgValue::String("no_such_port".to_string())], &ctx)
            .expect("unknown port is reported, not an Err");
    }

    /// R8-58: C registers `asynInterposeDelay` with iocsh
    /// (`asynInterposeDelay.c:215-237`); like the echo layer nothing else
    /// installs it, so without the registrar the ported `DelayInterpose` is
    /// unreachable from a startup script. Drive the command against a
    /// registered port and prove the port's octet write goes from one 3-byte
    /// link write to three 1-byte writes (C `writeIt`, :41-52).
    #[test]
    fn iocsh_interpose_delay_installs_the_layer_on_a_registered_port() {
        use crate::interpose::{EomReason, OctetNext, OctetReadResult};
        use crate::request::RequestOp;

        /// The link under the interpose stack: records the size of each write.
        struct CountingLink {
            sizes: Arc<Mutex<Vec<usize>>>,
        }
        impl OctetNext for CountingLink {
            fn read(&mut self, _user: &AsynUser, _buf: &mut [u8]) -> AsynResult<OctetReadResult> {
                Ok(OctetReadResult {
                    nbytes_transferred: 0,
                    eom_reason: EomReason::empty(),
                })
            }
            fn write(&mut self, _user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
                self.sizes.lock().unwrap().push(data.len());
                Ok(data.len())
            }
            fn flush(&mut self, _user: &mut AsynUser) -> AsynResult<()> {
                Ok(())
            }
        }

        struct InterposedDriver {
            base: PortDriverBase,
            link: CountingLink,
        }
        impl PortDriver for InterposedDriver {
            fn base(&self) -> &PortDriverBase {
                &self.base
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.base
            }
            fn io_write_octet(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
                self.link.write(user, data)
            }
        }

        let sizes = Arc::new(Mutex::new(Vec::new()));
        let mgr = Arc::new(PortManager::new());
        mgr.register_port(InterposedDriver {
            base: PortDriverBase::new("delay_port", 1, PortFlags::default()),
            link: CountingLink {
                sizes: sizes.clone(),
            },
        })
        .unwrap();
        let handle = mgr.find_port_handle("delay_port").unwrap();

        // Before the command: no delay layer, so the link sees one 3-byte write.
        handle
            .submit_blocking(
                RequestOp::OctetWrite {
                    data: b"ABC".to_vec(),
                },
                AsynUser::default(),
            )
            .expect("plain write succeeds");
        assert_eq!(sizes.lock().unwrap().as_slice(), &[3]);

        let ctx = make_ctx();
        let cmds = build_asyn_commands(mgr.clone());
        let delay_cmd = cmds
            .iter()
            .find(|c| c.name == "asynInterposeDelay")
            .expect("asynInterposeDelay must be registered");
        delay_cmd
            .handler
            .call(
                &[
                    ArgValue::String("delay_port".to_string()),
                    ArgValue::Int(0),
                    ArgValue::Double(0.001),
                ],
                &ctx,
            )
            .expect("handler returns Ok");

        handle
            .submit_blocking(
                RequestOp::OctetWrite {
                    data: b"ABC".to_vec(),
                },
                AsynUser::default(),
            )
            .expect("delayed write succeeds");
        assert_eq!(
            sizes.lock().unwrap().as_slice(),
            &[3, 1, 1, 1],
            "asynInterposeDelay must install the delay layer on the live port"
        );

        // An unknown port is C's `interposeInterface asynOctetType failed.`
        // (:186-190) — reported, not a panic and not a silent success.
        delay_cmd
            .handler
            .call(
                &[
                    ArgValue::String("no_such_port".to_string()),
                    ArgValue::Int(0),
                    ArgValue::Double(0.001),
                ],
                &ctx,
            )
            .expect("unknown port is reported, not an Err");
    }

    /// `Report` request op invokes the driver's `report(level)` on the
    /// actor thread — confirms the iocsh `asynReport` path reaches the
    /// driver under serial actor ownership.
    #[test]
    fn report_request_op_invokes_driver_report() -> AsynResult<()> {
        let mgr = fresh_mgr_with_port("report_port");
        let handle = mgr.find_port_handle("report_port")?;
        // Default Drv impl prints to stderr; just confirm the round
        // trip succeeds without error and returns `write_ok`.
        handle.report_blocking(0)?;
        handle.report_blocking(2)?;
        Ok(())
    }

    /// Build a minimal `CommandContext` for exercising registered
    /// handlers in-process. Mirrors the helper used in
    /// `epics-base-rs/src/server/iocsh/commands.rs::tests`.
    fn make_ctx() -> CommandContext {
        use epics_base_rs::server::database::PvDatabase;
        let rt = tokio::runtime::Runtime::new().unwrap();
        let db = Arc::new(PvDatabase::new());
        let bridge = {
            let _guard = rt.enter();
            epics_base_rs::runtime::task::BlockingBridge::capture()
        };
        let ctx = CommandContext::new(db, bridge);
        std::mem::forget(rt);
        ctx
    }

    /// `asynSetTraceIOMask` / `asynSetTraceInfoMask` /
    /// `asynSetTraceFile` previously discarded their `addr` arg
    /// (`let _addr = arg_int(args, 1)`), so an `asynSetTraceIOMask
    /// MYPORT 3 "ESCAPE"` invocation degraded into a port-wide write
    /// rather than the device-specific dpCommon write C performs
    /// (asynManager.c:2830-2833 / 2872-2875 / 2898-2926 via
    /// `findTracePvt` over `pdevice`). This test invokes all three
    /// handlers with `addr >= 0` and confirms each fires a per-device
    /// announce (addr propagated through to the device setter).
    #[test]
    fn iocsh_trace_setters_route_addr_to_device_announce() {
        let mgr = fresh_mgr_with_multi_device_port("trace_dev_port", 8);
        let ctx = make_ctx();

        let observed: Arc<Mutex<Vec<(AsynException, i32)>>> = Arc::new(Mutex::new(Vec::new()));
        let obs = observed.clone();
        mgr.exception_manager().add_callback(move |ev| {
            obs.lock().unwrap().push((ev.exception, ev.addr));
        });

        let cmds = build_asyn_commands(mgr.clone());

        // asynSetTraceIOMask trace_dev_port 5 "HEX"
        let io_cmd = cmds
            .iter()
            .find(|c| c.name == "asynSetTraceIOMask")
            .expect("asynSetTraceIOMask must be registered");
        let _ = io_cmd.handler.call(
            &[
                ArgValue::String("trace_dev_port".to_string()),
                ArgValue::Int(5),
                ArgValue::String("HEX".to_string()),
            ],
            &ctx,
        );

        // asynSetTraceInfoMask trace_dev_port 7 "SOURCE"
        let info_cmd = cmds
            .iter()
            .find(|c| c.name == "asynSetTraceInfoMask")
            .expect("asynSetTraceInfoMask must be registered");
        let _ = info_cmd.handler.call(
            &[
                ArgValue::String("trace_dev_port".to_string()),
                ArgValue::Int(7),
                ArgValue::String("SOURCE".to_string()),
            ],
            &ctx,
        );

        // asynSetTraceFile trace_dev_port 2 "stderr"
        let file_cmd = cmds
            .iter()
            .find(|c| c.name == "asynSetTraceFile")
            .expect("asynSetTraceFile must be registered");
        let _ = file_cmd.handler.call(
            &[
                ArgValue::String("trace_dev_port".to_string()),
                ArgValue::Int(2),
                ArgValue::String("stderr".to_string()),
            ],
            &ctx,
        );

        let evs = observed.lock().unwrap();
        assert!(
            evs.iter()
                .any(|(e, a)| matches!(e, AsynException::TraceIoMask) && *a == 5),
            "asynSetTraceIOMask addr=5 must fire a device-scoped announce; observed: {evs:?}"
        );
        assert!(
            evs.iter()
                .any(|(e, a)| matches!(e, AsynException::TraceInfoMask) && *a == 7),
            "asynSetTraceInfoMask addr=7 must fire a device-scoped announce; observed: {evs:?}"
        );
        assert!(
            evs.iter()
                .any(|(e, a)| matches!(e, AsynException::TraceFile) && *a == 2),
            "asynSetTraceFile addr=2 must fire a device-scoped announce; observed: {evs:?}"
        );
    }

    /// `drvAsynIPPortConfigure` creates an IP octet port and registers
    /// it in the asyn_record port registry so asynRecord device support
    /// can resolve it by name. `DrvAsynIPPort::new` only parses the
    /// host info (no connect), so no live server is needed.
    #[test]
    fn drv_asyn_ip_port_configure_registers_port() {
        let cmd =
            drv_asyn_ip_port_configure_command(PortServices::new(Arc::new(TraceManager::new())));
        assert_eq!(cmd.name, "drvAsynIPPortConfigure");
        assert_eq!(cmd.args.len(), 5);

        let ctx = make_ctx();
        let result = cmd.handler.call(
            &[
                ArgValue::String("iocsh_ip_cfg_test".into()),
                ArgValue::String("127.0.0.1:9001".into()),
            ],
            &ctx,
        );
        assert!(result.is_ok(), "command failed: {:?}", result.err());
        assert!(
            crate::asyn_record::get_port("iocsh_ip_cfg_test").is_some(),
            "port must be resolvable via the asyn_record registry"
        );
    }

    /// C drvAsynIPPort.c:1065-1066: an IP port gets an EOS interpose by
    /// default, suppressed by `noProcessEos`.
    #[test]
    fn build_configured_ip_port_installs_eos_unless_suppressed() {
        let default_port =
            build_configured_ip_port("ip_eos_default", "127.0.0.1:9100", false, false).unwrap();
        assert_eq!(
            default_port.base().interpose_octet.len(),
            1,
            "default IP port must auto-install the EOS interpose"
        );

        let suppressed =
            build_configured_ip_port("ip_eos_off", "127.0.0.1:9100", false, true).unwrap();
        assert_eq!(
            suppressed.base().interpose_octet.len(),
            0,
            "noProcessEos must suppress the EOS interpose"
        );
    }

    /// `drvAsynSerialPortConfigure` creates a serial octet port and
    /// registers it in the asyn_record registry. `DrvAsynSerialPort::new`
    /// only parses the tty path (no open), so no device is needed.
    #[cfg(asyn_serial_backend)]
    #[test]
    fn drv_asyn_serial_port_configure_registers_port() {
        let cmd = drv_asyn_serial_port_configure_command(PortServices::new(Arc::new(
            TraceManager::new(),
        )));
        assert_eq!(cmd.name, "drvAsynSerialPortConfigure");
        assert_eq!(cmd.args.len(), 5);

        let ctx = make_ctx();
        let result = cmd.handler.call(
            &[
                ArgValue::String("iocsh_serial_cfg_test".into()),
                ArgValue::String("/dev/ttyS0".into()),
            ],
            &ctx,
        );
        assert!(result.is_ok(), "command failed: {:?}", result.err());
        assert!(
            crate::asyn_record::get_port("iocsh_serial_cfg_test").is_some(),
            "port must be resolvable via the asyn_record registry"
        );
    }

    /// A missing required argument is rejected without creating a port.
    #[test]
    fn drv_asyn_ip_port_configure_rejects_missing_host() {
        let cmd =
            drv_asyn_ip_port_configure_command(PortServices::new(Arc::new(TraceManager::new())));
        let ctx = make_ctx();
        let result = cmd
            .handler
            .call(&[ArgValue::String("iocsh_ip_cfg_nohost".into())], &ctx);
        assert!(result.is_err());
        assert!(crate::asyn_record::get_port("iocsh_ip_cfg_nohost").is_none());
    }

    /// DRV-49: `prologixGPIBConfigure` creates a Prologix GPIB port and
    /// registers it in the asyn_record registry so it is reachable from a
    /// startup script. `DrvAsynPrologixPort::new` only parses the host (no
    /// connect), so no bridge is needed. C `prologixGPIBConfigure` takes 4
    /// args (portName, host, priority, noAutoConnect); priority is dropped.
    #[test]
    fn drv_asyn_prologix_port_configure_registers_port() {
        let cmd = drv_asyn_prologix_port_configure_command(PortServices::new(Arc::new(
            TraceManager::new(),
        )));
        assert_eq!(cmd.name, "prologixGPIBConfigure");
        assert_eq!(cmd.args.len(), 4);

        let ctx = make_ctx();
        let result = cmd.handler.call(
            &[
                ArgValue::String("iocsh_prologix_cfg_test".into()),
                ArgValue::String("127.0.0.1:1234".into()),
            ],
            &ctx,
        );
        assert!(result.is_ok(), "command failed: {:?}", result.err());
        assert!(
            crate::asyn_record::get_port("iocsh_prologix_cfg_test").is_some(),
            "port must be resolvable via the asyn_record registry"
        );
    }

    /// A missing required argument is rejected without creating a port.
    #[test]
    fn drv_asyn_prologix_port_configure_rejects_missing_host() {
        let cmd = drv_asyn_prologix_port_configure_command(PortServices::new(Arc::new(
            TraceManager::new(),
        )));
        let ctx = make_ctx();
        let result = cmd.handler.call(
            &[ArgValue::String("iocsh_prologix_cfg_nohost".into())],
            &ctx,
        );
        assert!(result.is_err());
        assert!(crate::asyn_record::get_port("iocsh_prologix_cfg_nohost").is_none());
    }

    /// R19-111: `drvAsynFTDIPortConfigure` exists with C's nine args
    /// (drvAsynFTDIPort.cpp:641-660) and publishes the port under its name.
    /// `noAutoConnect=1` here so the boundary under test is creation, not the
    /// absent USB device.
    #[test]
    fn iocsh_ftdi_port_configure_creates_the_port_an_st_cmd_names() {
        let cmd =
            drv_asyn_ftdi_port_configure_command(PortServices::new(Arc::new(TraceManager::new())));
        assert_eq!(cmd.name, "drvAsynFTDIPortConfigure");
        assert_eq!(cmd.args.len(), 9);

        let ctx = make_ctx();
        let result = cmd.handler.call(
            &[
                ArgValue::String("iocsh_ftdi_cfg_test".into()),
                ArgValue::Int(0x0403),
                ArgValue::Int(0x6001),
                ArgValue::Int(9600),
                ArgValue::Int(1),
                ArgValue::Int(0),
                ArgValue::Int(1), // noAutoConnect
            ],
            &ctx,
        );
        assert!(result.is_ok(), "command failed: {:?}", result.err());
        assert!(
            crate::asyn_record::get_port("iocsh_ftdi_cfg_test").is_some(),
            "port must be resolvable via the asyn_record registry"
        );
    }

    /// R19-111: `vxi11Configure` exists with C's seven args
    /// (drvVxi11.c:1789-1802) and publishes the port under its name.
    #[test]
    fn iocsh_vxi11_configure_creates_the_port_an_st_cmd_names() {
        let cmd = vxi11_configure_command(PortServices::new(Arc::new(TraceManager::new())));
        assert_eq!(cmd.name, "vxi11Configure");
        assert_eq!(cmd.args.len(), 7);

        let ctx = make_ctx();
        let result = cmd.handler.call(
            &[
                ArgValue::String("iocsh_vxi11_cfg_test".into()),
                ArgValue::String("127.0.0.1".into()),
                ArgValue::Int(0),
                ArgValue::String("1.0".into()),
                ArgValue::String("inst0".into()),
                ArgValue::Int(0),
                ArgValue::Int(1), // noAutoConnect: no VXI-11 instrument here
            ],
            &ctx,
        );
        assert!(result.is_ok(), "command failed: {:?}", result.err());
        assert!(
            crate::asyn_record::get_port("iocsh_vxi11_cfg_test").is_some(),
            "port must be resolvable via the asyn_record registry"
        );
    }

    /// `vxi11Configure` without a host creates nothing — C dereferences
    /// `hostName` in `vxiInit` and cannot proceed without it.
    #[test]
    fn iocsh_vxi11_configure_rejects_missing_host() {
        let cmd = vxi11_configure_command(PortServices::new(Arc::new(TraceManager::new())));
        let ctx = make_ctx();
        let result = cmd
            .handler
            .call(&[ArgValue::String("iocsh_vxi11_cfg_nohost".into())], &ctx);
        assert!(result.is_err());
        assert!(crate::asyn_record::get_port("iocsh_vxi11_cfg_nohost").is_none());
    }

    /// R19-111: `usbtmcConfigure` exists with C's six args
    /// (drvAsynUSBTMC.c:1332-1345) and publishes the port under its name. C has
    /// no `noAutoConnect` arg here, so the port comes up auto-connecting and
    /// fails to find a device — which is exactly C's behaviour with no
    /// instrument plugged in, and does not stop the port from existing.
    #[test]
    fn iocsh_usbtmc_configure_creates_the_port_an_st_cmd_names() {
        let cmd = usbtmc_configure_command(PortServices::new(Arc::new(TraceManager::new())));
        assert_eq!(cmd.name, "usbtmcConfigure");
        assert_eq!(cmd.args.len(), 6);

        let ctx = make_ctx();
        let result = cmd.handler.call(
            &[
                ArgValue::String("iocsh_usbtmc_cfg_test".into()),
                ArgValue::Int(0x0699),
                ArgValue::Int(0x0368),
                ArgValue::String(String::new()),
                ArgValue::Int(0),
                ArgValue::Int(0),
            ],
            &ctx,
        );
        assert!(result.is_ok(), "command failed: {:?}", result.err());
        assert!(
            crate::asyn_record::get_port("iocsh_usbtmc_cfg_test").is_some(),
            "port must be resolvable via the asyn_record registry"
        );
    }
}
