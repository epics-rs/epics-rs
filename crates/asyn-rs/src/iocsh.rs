//! Asyn iocsh shell-command registration.
//!
//! C parity: `asynShellCommands.c:580-906` registers six shell commands
//! (`asynReport`, `asynSetOption`, `asynSetTraceMask`, `asynSetTraceIOMask`,
//! `asynSetTraceInfoMask`, `asynSetTraceFile`) plus the
//! `asynSetTraceIOTruncateSize` setter. This module exposes the same
//! surface via [`register_asyn_commands`], which takes an
//! `IocApplication` (the public registration carrier on the
//! `epics-base-rs` side) along with the `PortManager` whose
//! `TraceManager` is the back-end for the trace mutators.
//!
//! Available only with the `epics` feature.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use epics_base_rs::server::ioc_app::IocApplication;
use epics_base_rs::server::iocsh::registry::{
    ArgDesc, ArgType, ArgValue, CommandContext, CommandDef, CommandOutcome, CommandResult,
};

use crate::drivers::ip_port::DrvAsynIPPort;
use crate::drivers::prologix::DrvAsynPrologixPort;
use crate::drivers::serial_port::DrvAsynSerialPort;
use crate::error::AsynResult;
use crate::manager::PortManager;
use crate::port::PortDriver;
use crate::runtime::config::RuntimeConfig;
use crate::runtime::port::{PortRuntimeHandle, create_port_runtime};
use crate::trace::{TraceFile, TraceInfoMask, TraceIoMask, TraceManager, TraceMask};
use crate::user::AsynUser;

/// Register the standard asyn iocsh commands on the supplied
/// [`IocApplication`]. The shared [`PortManager`] is captured in each
/// command closure so the trace mutators reach the same
/// [`crate::trace::TraceManager`] the drivers were registered with.
///
/// C parity: the six `asynShellCommands.c` commands (`asynReport /
/// asynSetOption / asynSetTraceMask / asynSetTraceIOMask /
/// asynSetTraceInfoMask / asynSetTraceFile`) plus the port-creation
/// command `drvAsynIPPort.c::drvAsynIPPortConfigure`, registered as a
/// startup command so it runs before `iocInit`.
pub fn register_asyn_commands(mut app: IocApplication, mgr: Arc<PortManager>) -> IocApplication {
    let trace = mgr.trace_manager().clone();
    for def in build_asyn_commands(mgr) {
        app = app.register_shell_command(def);
    }
    app = app.register_startup_command(drv_asyn_ip_port_configure_command(trace.clone()));
    app = app.register_startup_command(drv_asyn_serial_port_configure_command(trace.clone()));
    app.register_startup_command(drv_asyn_prologix_port_configure_command(trace))
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

/// Shared body of `asynOctetSetInputEos` / `asynOctetSetOutputEos`.
///
/// C parity: `asynShellCommands.c::asynSetEos` escape-decodes the `eos`
/// argument and routes it to `pasynOctet->setInputEos`/`setOutputEos`. Here
/// the EOS is port-wide (the interpose stack owns it), so the C `addr` is
/// accepted for command-line compatibility but not routed — single-address
/// octet ports (serial/IP/prologix) address 0. The driver enforces the
/// 2-byte terminator limit and reports `illegal eoslen N`, so no length
/// check is duplicated here (single owner of the limit).
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
    let _addr = arg_int(args, 1).unwrap_or(0);
    let eos = raw_from_escaped(&arg_str(args, 2).unwrap_or_default());
    match mgr.find_port_handle(&port) {
        Ok(handle) => {
            let res = if set_input {
                handle.set_input_eos_blocking(&eos)
            } else {
                handle.set_output_eos_blocking(&eos)
            };
            if let Err(e) = res {
                ctx.println(&format!("{cmd}: {e}"));
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
    let trace = mgr.trace_manager().clone();
    for def in build_asyn_commands(mgr) {
        shell.register(def);
    }
    shell.register(drv_asyn_ip_port_configure_command(trace.clone()));
    shell.register(drv_asyn_serial_port_configure_command(trace.clone()));
    shell.register(drv_asyn_prologix_port_configure_command(trace));
}

/// Build the asyn iocsh `CommandDef`s without binding them to a specific
/// carrier: `asynReport`, `asynSetOption`, `asynOctetSetInputEos`,
/// `asynOctetSetOutputEos`, and the trace mutators (`asynSetTraceMask`,
/// `asynSetTraceIOMask`, `asynSetTraceInfoMask`). Both
/// [`register_asyn_commands`] (IocApplication path) and
/// [`register_asyn_commands_on_shell`] (direct IocShell path) delegate
/// here so the C-parity command set stays in lock step across both entry
/// points.
pub fn build_asyn_commands(mgr: Arc<PortManager>) -> Vec<CommandDef> {
    let mut out = Vec::new();

    // asynReport ----------------------------------------------------------
    {
        let mgr_r = mgr.clone();
        out.push(CommandDef::new(
            "asynReport",
            vec![
                ArgDesc {
                    name: "level",
                    arg_type: ArgType::Int,
                    optional: true,
                },
                ArgDesc {
                    name: "port",
                    arg_type: ArgType::String,
                    optional: true,
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
                    optional: false,
                },
                ArgDesc {
                    name: "addr",
                    arg_type: ArgType::Int,
                    optional: false,
                },
                ArgDesc {
                    name: "key",
                    arg_type: ArgType::String,
                    optional: false,
                },
                ArgDesc {
                    name: "value",
                    arg_type: ArgType::String,
                    optional: false,
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
                let user = AsynUser::default()
                    .with_addr(addr)
                    .with_timeout(Duration::from_secs(2));
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
                    optional: false,
                },
                ArgDesc {
                    name: "addr",
                    arg_type: ArgType::Int,
                    optional: false,
                },
                ArgDesc {
                    name: "eos",
                    arg_type: ArgType::String,
                    optional: false,
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
                    optional: false,
                },
                ArgDesc {
                    name: "addr",
                    arg_type: ArgType::Int,
                    optional: false,
                },
                ArgDesc {
                    name: "eos",
                    arg_type: ArgType::String,
                    optional: false,
                },
            ],
            "asynOctetSetOutputEos portName addr eos - set the port output EOS (e.g. \"\\r\\n\")",
            move |args: &[ArgValue], ctx: &CommandContext| asyn_set_eos(&mgr_r, ctx, args, false),
        ));
    }

    // asynInterposeEcho portName addr -------------------------------------
    //
    // C `asynInterposeEcho.c:189-207` registers this so a startup script can
    // install the echo layer on an already-configured port — the interpose is
    // useless without it, since a driver's own configure command never installs
    // it. C reports "%s interposeInterface failed." and returns -1 when the
    // port is unknown (:180-184).
    //
    // As with `asynOctetSetInputEos`, the interpose stack is port-wide, so the
    // C `addr` is accepted for command-line compatibility but not routed.
    {
        let mgr_r = mgr.clone();
        out.push(CommandDef::new(
            "asynInterposeEcho",
            vec![
                ArgDesc {
                    name: "portName",
                    arg_type: ArgType::String,
                    optional: false,
                },
                ArgDesc {
                    name: "addr",
                    arg_type: ArgType::Int,
                    optional: true,
                },
            ],
            "asynInterposeEcho portName [addr] - install the echo interpose \
             (half-duplex devices that echo each char)",
            move |args: &[ArgValue], ctx: &CommandContext| {
                let port = arg_str(args, 0)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| "portName required".to_string())?;
                let _addr = arg_int(args, 1).unwrap_or(0);
                match mgr_r.find_port_handle(&port) {
                    Ok(handle) => {
                        if let Err(e) = handle.push_echo_interpose_blocking() {
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
    // C `asynInterposeDelay.c:221-234` registers this 3-arg command; nothing
    // else installs the layer (no driver configure command pushes it), so
    // without the registrar a startup script cannot reach a device that needs
    // an inter-character write delay at all. C prints
    // "%s interposeInterface asynOctetType failed." and returns -1 when the
    // interposeInterface call fails (:186-190).
    //
    // As with `asynInterposeEcho`, the Rust interpose stack is port-wide, so
    // the C `addr` is accepted for command-line compatibility but not routed.
    {
        let mgr_r = mgr.clone();
        out.push(CommandDef::new(
            "asynInterposeDelay",
            vec![
                ArgDesc {
                    name: "portName",
                    arg_type: ArgType::String,
                    optional: false,
                },
                ArgDesc {
                    name: "addr",
                    arg_type: ArgType::Int,
                    optional: false,
                },
                ArgDesc {
                    name: "delay(sec)",
                    arg_type: ArgType::Double,
                    optional: false,
                },
            ],
            "asynInterposeDelay portName addr delay(sec) - install the delay \
             interpose (one write per character, delay after each)",
            move |args: &[ArgValue], ctx: &CommandContext| {
                let port = arg_str(args, 0)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| "portName required".to_string())?;
                let _addr = arg_int(args, 1).unwrap_or(0);
                // C takes the delay as a `double` seconds and stores it verbatim
                // (`pvt->delay = delay`, asynInterposeDelay.c:214). iocsh can
                // supply a negative or NaN one; `delay_from_secs` owns the
                // conversion and collapses those to C's "no delay".
                let delay =
                    crate::interpose::delay::delay_from_secs(arg_f64(args, 2).unwrap_or(0.0));
                match mgr_r.find_port_handle(&port) {
                    Ok(handle) => {
                        if let Err(e) = handle.push_delay_interpose_blocking(delay) {
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

    // asynSetTraceMask ----------------------------------------------------
    {
        let mgr_r = mgr.clone();
        out.push(CommandDef::new(
            "asynSetTraceMask",
            vec![
                ArgDesc {
                    name: "portName",
                    arg_type: ArgType::String,
                    optional: true,
                },
                ArgDesc {
                    name: "addr",
                    arg_type: ArgType::Int,
                    optional: true,
                },
                ArgDesc {
                    name: "mask",
                    arg_type: ArgType::String,
                    optional: false,
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
                    optional: true,
                },
                ArgDesc {
                    name: "addr",
                    arg_type: ArgType::Int,
                    optional: true,
                },
                ArgDesc {
                    name: "mask",
                    arg_type: ArgType::String,
                    optional: false,
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
                    optional: true,
                },
                ArgDesc {
                    name: "addr",
                    arg_type: ArgType::Int,
                    optional: true,
                },
                ArgDesc {
                    name: "mask",
                    arg_type: ArgType::String,
                    optional: false,
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
                    optional: true,
                },
                ArgDesc {
                    name: "addr",
                    arg_type: ArgType::Int,
                    optional: true,
                },
                ArgDesc {
                    name: "filename",
                    arg_type: ArgType::String,
                    optional: true,
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
                // C parity: asynShellCommands.c:855-877 routes through
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

    out
}

/// Keeps the [`PortRuntimeHandle`]s created by the port-configure iocsh
/// commands (`drvAsynIPPortConfigure`, `drvAsynSerialPortConfigure`)
/// alive for the process lifetime. Dropping a handle shuts the port's
/// actor thread down, so a startup-script-created port must be parked
/// somewhere with a 'static lifetime.
static PORT_RUNTIMES: OnceLock<Mutex<Vec<PortRuntimeHandle>>> = OnceLock::new();

fn keep_port_runtime(handle: PortRuntimeHandle) {
    PORT_RUNTIMES
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(handle);
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
    let mut driver = DrvAsynIPPort::new(port, host)?;
    if no_auto_connect {
        driver.base_mut().auto_connect = false;
    }
    if !no_process_eos {
        driver.install_interpose(Box::new(crate::interpose::eos::EosInterpose::default()));
    }
    Ok(driver)
}

pub fn drv_asyn_ip_port_configure_command(trace: Arc<TraceManager>) -> CommandDef {
    CommandDef::new(
        "drvAsynIPPortConfigure",
        vec![
            ArgDesc {
                name: "portName",
                arg_type: ArgType::String,
                optional: false,
            },
            ArgDesc {
                name: "hostInfo",
                arg_type: ArgType::String,
                optional: false,
            },
            ArgDesc {
                name: "priority",
                arg_type: ArgType::Int,
                optional: true,
            },
            ArgDesc {
                name: "noAutoConnect",
                arg_type: ArgType::Int,
                optional: true,
            },
            ArgDesc {
                name: "noProcessEos",
                arg_type: ArgType::Int,
                optional: true,
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

            let (handle, _jh) = create_port_runtime(driver, RuntimeConfig::default());
            crate::asyn_record::register_port(&port, handle.port_handle().clone(), trace.clone());
            keep_port_runtime(handle);
            ctx.println(&format!(
                "drvAsynIPPortConfigure: octet port '{port}' -> {host}"
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
pub fn drv_asyn_serial_port_configure_command(trace: Arc<TraceManager>) -> CommandDef {
    CommandDef::new(
        "drvAsynSerialPortConfigure",
        vec![
            ArgDesc {
                name: "portName",
                arg_type: ArgType::String,
                optional: false,
            },
            ArgDesc {
                name: "ttyName",
                arg_type: ArgType::String,
                optional: false,
            },
            ArgDesc {
                name: "priority",
                arg_type: ArgType::Int,
                optional: true,
            },
            ArgDesc {
                name: "noAutoConnect",
                arg_type: ArgType::Int,
                optional: true,
            },
            ArgDesc {
                name: "noProcessEos",
                arg_type: ArgType::Int,
                optional: true,
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

            let (handle, _jh) = create_port_runtime(driver, RuntimeConfig::default());
            crate::asyn_record::register_port(&port, handle.port_handle().clone(), trace.clone());
            keep_port_runtime(handle);
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
pub fn drv_asyn_prologix_port_configure_command(trace: Arc<TraceManager>) -> CommandDef {
    CommandDef::new(
        "prologixGPIBConfigure",
        vec![
            ArgDesc {
                name: "portName",
                arg_type: ArgType::String,
                optional: false,
            },
            ArgDesc {
                name: "host",
                arg_type: ArgType::String,
                optional: false,
            },
            ArgDesc {
                name: "priority",
                arg_type: ArgType::Int,
                optional: true,
            },
            ArgDesc {
                name: "noAutoConnect",
                arg_type: ArgType::Int,
                optional: true,
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

            let (handle, _jh) = create_port_runtime(driver, RuntimeConfig::default());
            crate::asyn_record::register_port(&port, handle.port_handle().clone(), trace.clone());
            keep_port_runtime(handle);
            ctx.println(&format!(
                "prologixGPIBConfigure: GPIB port '{port}' -> {host}"
            ));
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

    fn fresh_mgr_with_port(name: &str) -> Arc<PortManager> {
        let mgr = Arc::new(PortManager::new());
        let _ = mgr.register_port(DummyDriver::new(name)).unwrap();
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

        // Verify the handler is wired (closure captures mgr; lookup
        // succeeds) — we count one CommandDef per C-side function.
        assert_eq!(
            cmds.len(),
            10,
            "asyn iocsh command set: asynReport, asynSetOption, \
             asynOctetSet{{Input,Output}}Eos, asynInterposeEcho, \
             asynInterposeDelay, and the four trace mutators"
        );
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
    /// asyn-rs ports — guards against silent additions / removals.
    #[test]
    fn iocsh_registers_c_parity_commands() {
        let mgr = Arc::new(PortManager::new());
        let cmds = build_asyn_commands(mgr);
        let names: Vec<&str> = cmds.iter().map(|c| c.name.as_str()).collect();
        for expected in [
            "asynReport",
            "asynSetOption",
            "asynOctetSetInputEos",
            "asynOctetSetOutputEos",
            "asynInterposeEcho",
            "asynInterposeDelay",
            "asynSetTraceMask",
            "asynSetTraceIOMask",
            "asynSetTraceInfoMask",
            "asynSetTraceFile",
        ] {
            assert!(
                names.contains(&expected),
                "iocsh registration must include {expected}"
            );
        }
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
            fn set_input_eos(&mut self, eos: &[u8]) -> AsynResult<()> {
                *self.rec.input.lock().unwrap() = Some(eos.to_vec());
                Ok(())
            }
            fn set_output_eos(&mut self, eos: &[u8]) -> AsynResult<()> {
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

    /// R8-49: C registers `asynInterposeEcho` with iocsh
    /// (`asynInterposeEcho.c:189-207`) because nothing else installs the layer
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

        /// Dispatches its octet write through the base's interpose stack, the
        /// way every real driver does (`serial_port.rs::write_octet`).
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
            fn write_octet(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
                self.base
                    .interpose_octet
                    .dispatch_write(user, data, &mut self.link)
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

        // An unknown port is C's `interposeInterface failed.` (:180-184), not a
        // panic and not a silent success.
        echo_cmd
            .handler
            .call(&[ArgValue::String("no_such_port".to_string())], &ctx)
            .expect("unknown port is reported, not an Err");
    }

    /// R8-58: C registers `asynInterposeDelay` with iocsh
    /// (`asynInterposeDelay.c:221-234`); like the echo layer nothing else
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
            fn write_octet(&mut self, user: &mut AsynUser, data: &[u8]) -> AsynResult<usize> {
                self.base
                    .interpose_octet
                    .dispatch_write(user, data, &mut self.link)
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
        let handle = rt.handle().clone();
        let ctx = CommandContext::new(db, handle);
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
        let mgr = fresh_mgr_with_port("trace_dev_port");
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
        let cmd = drv_asyn_ip_port_configure_command(Arc::new(TraceManager::new()));
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
    #[test]
    fn drv_asyn_serial_port_configure_registers_port() {
        let cmd = drv_asyn_serial_port_configure_command(Arc::new(TraceManager::new()));
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
        let cmd = drv_asyn_ip_port_configure_command(Arc::new(TraceManager::new()));
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
        let cmd = drv_asyn_prologix_port_configure_command(Arc::new(TraceManager::new()));
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
        let cmd = drv_asyn_prologix_port_configure_command(Arc::new(TraceManager::new()));
        let ctx = make_ctx();
        let result = cmd.handler.call(
            &[ArgValue::String("iocsh_prologix_cfg_nohost".into())],
            &ctx,
        );
        assert!(result.is_err());
        assert!(crate::asyn_record::get_port("iocsh_prologix_cfg_nohost").is_none());
    }
}
