//! C `drvScalerSoftConfigure(port, maxChans, pvTemplate)`
//! (`drvScalerSoft.c:178-275`): a soft scaler port whose counters are the
//! database PVs `pvTemplate % 1 .. pvTemplate % maxChans`.
//!
//! C reads and zeroes those PVs with `dbGetField` / `dbPutField` from inside
//! the scaler's `process()`, nesting the input's lock under the scaler's. The
//! port never takes another record's lock under the scaler's hold: each input
//! is a subscription the driver drains, and the arm's zeros are written by a
//! task of the port's own, after the hold.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::runtime::task::{CallbackPriority, spawn_background};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::types::{DbFieldType, EpicsValue};

use super::scaler_asyn::{ScalerAsynDeviceSupport, ScalerDriver};
use super::scaler_soft::{CountSource, SoftScalerDriver};
use crate::records::scaler::MAX_SCALER_CHANNELS;

/// C `char pvName[MAX_PVNAME_SIZE]` (`drvScalerSoft.c:48`, `:564`).
const MAX_PVNAME_SIZE: usize = 40;

/// One printf integer conversion: `%<flags><width>.<precision>d` (or `i`).
#[derive(Default)]
struct IntSpec {
    left: bool,
    plus: bool,
    space: bool,
    zero: bool,
    width: usize,
    precision: Option<usize>,
}

impl IntSpec {
    fn format(&self, n: i64) -> String {
        let mut digits = n.unsigned_abs().to_string();
        if let Some(p) = self.precision {
            if digits.len() < p {
                digits.insert_str(0, &"0".repeat(p - digits.len()));
            }
        }
        let sign = if n < 0 {
            "-"
        } else if self.plus {
            "+"
        } else if self.space {
            " "
        } else {
            ""
        };
        let pad = self.width.saturating_sub(sign.len() + digits.len());
        if self.left {
            format!("{sign}{digits}{}", " ".repeat(pad))
        } else if self.zero && self.precision.is_none() {
            format!("{sign}{}{digits}", "0".repeat(pad))
        } else {
            format!("{}{sign}{digits}", " ".repeat(pad))
        }
    }
}

/// A `drvScalerSoftConfigure` template, parsed once: C hands it to
/// `epicsSnprintf(pvName, 40, pvTemplate, i+1)` (`drvScalerSoft.c:569`), so
/// it is a printf format taking at most one `int`.
enum Piece {
    Text(String),
    Int(IntSpec),
}

fn parse_template(template: &str) -> Result<Vec<Piece>, String> {
    let mut pieces = Vec::new();
    let mut text = String::new();
    let mut chars = template.chars().peekable();
    let mut ints = 0;
    while let Some(c) = chars.next() {
        if c != '%' {
            text.push(c);
            continue;
        }
        if chars.peek() == Some(&'%') {
            chars.next();
            text.push('%');
            continue;
        }
        let mut spec = IntSpec::default();
        while let Some(&f) = chars.peek() {
            match f {
                '-' => spec.left = true,
                '+' => spec.plus = true,
                ' ' => spec.space = true,
                '0' => spec.zero = true,
                // No effect on `d`.
                '#' => {}
                _ => break,
            }
            chars.next();
        }
        let number = |chars: &mut std::iter::Peekable<std::str::Chars>| {
            let mut n = 0usize;
            while let Some(d) = chars.peek().and_then(|c| c.to_digit(10)) {
                n = n * 10 + d as usize;
                chars.next();
            }
            n
        };
        spec.width = number(&mut chars);
        if chars.peek() == Some(&'.') {
            chars.next();
            spec.precision = Some(number(&mut chars));
        }
        match chars.next() {
            Some('d' | 'i') => {}
            other => {
                return Err(format!(
                    "PV template {template:?}: %{} is not an int conversion",
                    other.map(String::from).unwrap_or_default()
                ));
            }
        }
        ints += 1;
        if ints > 1 {
            return Err(format!("PV template {template:?} takes more than one int"));
        }
        pieces.push(Piece::Text(std::mem::take(&mut text)));
        pieces.push(Piece::Int(spec));
    }
    pieces.push(Piece::Text(text));
    Ok(pieces)
}

/// C `epicsSnprintf(pvName, MAX_PVNAME_SIZE, template, n)`: the output cut to
/// the buffer, less its NUL.
fn pv_name(pieces: &[Piece], n: i64) -> String {
    let mut name = String::new();
    for piece in pieces {
        match piece {
            Piece::Text(t) => name.push_str(t),
            Piece::Int(spec) => name.push_str(&spec.format(n)),
        }
    }
    let mut cut = name.len().min(MAX_PVNAME_SIZE - 1);
    while !name.is_char_boundary(cut) {
        cut -= 1;
    }
    name.truncate(cut);
    name
}

/// An input PV as `dbNameToAddr` parses it: the record, and the field
/// (`VAL` when the name has none).
struct InputPv {
    name: String,
    record: String,
    field: String,
}

impl InputPv {
    fn new(name: String) -> Self {
        let (record, field) = match name.split_once('.') {
            Some((record, field)) => (record.to_string(), field.to_string()),
            None => (name.clone(), "VAL".to_string()),
        };
        Self {
            name,
            record,
            field,
        }
    }
}

/// One port's input PVs, shared by its driver and the port registry.
struct PvInputs {
    /// Each input, channel order.
    names: Vec<InputPv>,
    state: Mutex<PvState>,
    /// Wakes the task that writes an arm's zeros to the inputs.
    zero: tokio::sync::Notify,
}

struct PvState {
    connection: Connection,
    /// The latest value each input posted.
    values: [u32; MAX_SCALER_CHANNELS],
}

enum Connection {
    /// C `stateUnconnected`: C connects once `interruptAccept` is set.
    Unconnected,
    Connected(Vec<EventReader>),
    /// C `stateFailed`: an input PV does not exist.
    Failed,
}

/// A [`CountSource`] reading a port's input PVs.
pub struct PvCounts(Arc<PvInputs>);

/// C `dbGetField(addr, DBR_LONG, &counts[i], NULL, NULL, NULL)`: the field's
/// first element through the GET conversion, as the driver's `epicsInt32`
/// count. `None` where C's conversion fails.
fn dbr_long(value: &EpicsValue) -> Option<u32> {
    match value.get_convert(DbFieldType::Long).ok()? {
        EpicsValue::Long(v) => Some(v as u32),
        EpicsValue::LongArray(a) => a.first().map(|&v| v as u32),
        _ => None,
    }
}

/// The latest value each input posted, kept in `values`.
fn drain(inputs: &mut [EventReader], values: &mut [u32; MAX_SCALER_CHANNELS]) {
    for (input, value) in inputs.iter_mut().zip(values.iter_mut()) {
        while let Ok(ev) = input.try_recv() {
            if let Some(v) = dbr_long(&ev.snapshot.value) {
                *value = v;
            }
        }
    }
}

impl CountSource for PvCounts {
    fn sample(&mut self, counts: &mut [u32; MAX_SCALER_CHANNELS]) -> CaResult<()> {
        let state = &mut *self.0.state.lock().unwrap();
        match &mut state.connection {
            Connection::Connected(inputs) => {
                drain(inputs, &mut state.values);
                *counts = state.values;
                Ok(())
            }
            // C `if (pPvt->connectState != stateConnected) return(asynError)`.
            _ => Err(CaError::Disconnected),
        }
    }

    fn clear(&mut self) -> CaResult<()> {
        let state = &mut *self.0.state.lock().unwrap();
        match &mut state.connection {
            Connection::Connected(inputs) => {
                // What the inputs held before the zeros is superseded by them.
                drain(inputs, &mut state.values);
                state.values = [0; MAX_SCALER_CHANNELS];
                self.0.zero.notify_one();
                Ok(())
            }
            _ => Err(CaError::Disconnected),
        }
    }
}

struct SoftPort {
    driver: Arc<Mutex<Box<dyn ScalerDriver>>>,
    inputs: Arc<PvInputs>,
}

/// The soft scaler ports an IOC configured — C's asyn port registry, for the
/// ports `drvScalerSoftConfigure` registers.
#[derive(Default)]
pub struct SoftScalerPorts {
    ports: Mutex<HashMap<String, SoftPort>>,
}

impl SoftScalerPorts {
    /// C `drvScalerSoftConfigure(port, max_chans, pv_template)`. The template
    /// is formatted as C's `epicsSnprintf` formats it, with the channel
    /// number from 1, into a 40-byte name; a template C would read anything
    /// but one `int` from is refused.
    pub fn configure(&self, port: &str, max_chans: usize, pv_template: &str) -> Result<(), String> {
        let pieces = parse_template(pv_template).map_err(|e| format!("{port}: {e}"))?;
        let mut ports = self.ports.lock().unwrap();
        if ports.contains_key(port) {
            // C `registerPort` refuses a second port of the same name.
            return Err(format!("{port}: port already registered"));
        }
        let max_chans = max_chans.min(MAX_SCALER_CHANNELS);
        let names = (1..=max_chans)
            .map(|i| InputPv::new(pv_name(&pieces, i as i64)))
            .collect();
        let inputs = Arc::new(PvInputs {
            names,
            state: Mutex::new(PvState {
                connection: Connection::Unconnected,
                values: [0; MAX_SCALER_CHANNELS],
            }),
            zero: tokio::sync::Notify::new(),
        });
        let driver = SoftScalerDriver::with_source(max_chans, PvCounts(Arc::clone(&inputs)));
        ports.insert(
            port.to_string(),
            SoftPort {
                driver: Arc::new(Mutex::new(Box::new(driver))),
                inputs,
            },
        );
        Ok(())
    }

    /// `Asyn Scaler` device support on `port`, or `None` for a port nobody
    /// configured (C `devScalerAsyn` fails `connectDevice`).
    pub fn device_support(&self, port: &str) -> Option<ScalerAsynDeviceSupport> {
        let ports = self.ports.lock().unwrap();
        let driver = Arc::clone(&ports.get(port)?.driver);
        Some(ScalerAsynDeviceSupport::shared(driver))
    }

    /// C `connectPVs` (`drvScalerSoft.c:558-583`), run once the database is
    /// loaded: subscribe to every input and start the zero writer. A port
    /// with a missing input prints C's message, is `stateFailed` and refuses
    /// reads and arms; the other ports are unaffected, as in C, where each
    /// port connects on its own thread.
    ///
    /// No lock is held while another is taken. A port's readers are built
    /// with neither the registry nor its `state` held, and `state` is taken
    /// only to install them: the process path takes the scaler record, then
    /// the driver, then `state`, and an input may be a record (a scaler) on
    /// that path.
    pub fn connect(&self, db: &Arc<PvDatabase>) {
        let ports: Vec<(String, Arc<PvInputs>)> = self
            .ports
            .lock()
            .unwrap()
            .iter()
            .map(|(port, soft)| (port.clone(), Arc::clone(&soft.inputs)))
            .collect();
        for (port, pv_inputs) in ports {
            if !matches!(
                pv_inputs.state.lock().unwrap().connection,
                Connection::Unconnected
            ) {
                continue;
            }
            let mut inputs = Vec::with_capacity(pv_inputs.names.len());
            let mut values = [0; MAX_SCALER_CHANNELS];
            for (i, pv) in pv_inputs.names.iter().enumerate() {
                let reader = db.get_record(&pv.record).and_then(|cell| {
                    cell.write().add_subscriber(
                        &pv.field,
                        i as u32,
                        DbFieldType::Long,
                        EventMask::VALUE.bits(),
                    )
                });
                let current = db.get_pv(&format!("{}.{}", pv.record, pv.field)).ok();
                let (Some(reader), Some(current)) = (reader, current) else {
                    // C asynPrint ASYN_TRACE_ERROR, then `stateFailed`.
                    eprintln!(
                        "drvScalerSoft::connectPVs port {port} can't find PV {}.",
                        pv.name
                    );
                    break;
                };
                if let Some(v) = dbr_long(&current) {
                    values[i] = v;
                }
                inputs.push(reader);
            }
            let complete = inputs.len() == pv_inputs.names.len();
            {
                let mut state = pv_inputs.state.lock().unwrap();
                if !matches!(state.connection, Connection::Unconnected) {
                    continue;
                }
                *state = if complete {
                    PvState {
                        connection: Connection::Connected(inputs),
                        values,
                    }
                } else {
                    PvState {
                        connection: Connection::Failed,
                        values: [0; MAX_SCALER_CHANNELS],
                    }
                };
            }
            if !complete {
                continue;
            }

            // C writes the zeros inside `scalerArmCommand`; this task writes
            // them once the arming cycle has released the scaler.
            let db = Arc::clone(db);
            spawn_background(CallbackPriority::Medium, async move {
                loop {
                    pv_inputs.zero.notified().await;
                    for pv in &pv_inputs.names {
                        let _ = db
                            .put_record_field_from_ca(&pv.record, &pv.field, EpicsValue::Long(0))
                            .await;
                    }
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(template: &str, n: i64) -> String {
        pv_name(&parse_template(template).unwrap(), n)
    }

    /// Each row is glibc `snprintf(buf, 40, template, n)`.
    #[test]
    fn templates_format_as_c_snprintf() {
        let rows = [
            ("c%d", 12, "c12"),
            ("c%03d", 1, "c001"),
            ("c%-3d|", 1, "c1  |"),
            ("c%3d", 12, "c 12"),
            ("c%+d", 1, "c+1"),
            ("c% d", 12, "c 12"),
            ("c%.3d", 12, "c012"),
            ("c%5.3d", 1, "c  001"),
            ("c%-05d|", 12, "c12   |"),
            ("c%#d", 1, "c1"),
            ("c%i", 12, "c12"),
            ("100%%c%d", 1, "100%c1"),
            ("fixed", 12, "fixed"),
            ("c%0-4d|", 1, "c1   |"),
            ("c%+05d", 12, "c+0012"),
            ("x%d_y", 1, "x1_y"),
            (
                "abcdefghijklmnopqrstuvwxyz0123456789ABC%d",
                5,
                "abcdefghijklmnopqrstuvwxyz0123456789ABC",
            ),
            (
                "abcdefghijklmnopqrstuvwxyz0123456789AB%03d",
                5,
                "abcdefghijklmnopqrstuvwxyz0123456789AB0",
            ),
        ];
        for (template, n, c) in rows {
            assert_eq!(name(template, n), c, "{template:?} {n}");
        }
    }

    #[test]
    fn a_template_reading_other_than_one_int_is_refused() {
        for template in ["c%s", "c%ld", "c%d%d", "c%"] {
            assert!(parse_template(template).is_err(), "{template:?}");
        }
    }
}
