//! C `drvScalerSoftConfigure(port, maxChans, pvTemplate)`
//! (`drvScalerSoft.c:178-275`): a soft scaler port whose counters are the
//! database PVs `pvTemplate % 1 .. pvTemplate % maxChans`.
//!
//! C reads and zeroes those PVs with `dbGetField` / `dbPutField` from inside
//! the scaler's `process()`, nesting the input's lock under the scaler's. The
//! port never takes another record's lock under the scaler's hold: each input
//! is a subscription a task of the port's own reads into the counts the driver
//! copies, and that task writes the arm's zeros, after the hold.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::task::Poll;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::runtime::log::errlog_printf;
use epics_base_rs::runtime::task::{CallbackPriority, spawn_background};
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::event_queue::EventReader;
use epics_base_rs::server::pv::{MonitorEvent, SubscribeRefused};
use epics_base_rs::server::recgbl::EventMask;
use epics_base_rs::server::record::RecordCell;
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

/// One port's input PVs, shared by its driver, the port registry and the
/// port's delivery task.
struct PvInputs {
    /// Each input, channel order.
    names: Vec<InputPv>,
    state: Mutex<PvState>,
    /// Wakes the delivery task to write an arm's zeros to the inputs.
    zero: tokio::sync::Notify,
    /// Counts the deliveries the task has stored.
    delivered: tokio::sync::watch::Sender<u64>,
}

struct PvState {
    connection: Connection,
    /// The latest value each input posted, as the delivery task read it.
    values: [u32; MAX_SCALER_CHANNELS],
    /// The arms that cleared `values`, and those whose zeros the delivery
    /// task has written and read back. While they differ, what the task reads
    /// is superseded by the zeros.
    cleared: u64,
    zeroed: u64,
}

enum Connection {
    /// C `stateUnconnected`: C connects once `interruptAccept` is set.
    Unconnected,
    Connected,
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

impl CountSource for PvCounts {
    fn sample(&mut self, counts: &mut [u32; MAX_SCALER_CHANNELS]) -> CaResult<()> {
        let state = self.0.state.lock().unwrap();
        match state.connection {
            Connection::Connected => {
                *counts = state.values;
                Ok(())
            }
            // C `if (pPvt->connectState != stateConnected) return(asynError)`.
            _ => Err(CaError::Disconnected),
        }
    }

    fn clear(&mut self) -> CaResult<()> {
        let state = &mut *self.0.state.lock().unwrap();
        match state.connection {
            Connection::Connected => {
                state.values = [0; MAX_SCALER_CHANNELS];
                state.cleared += 1;
                self.0.zero.notify_one();
                Ok(())
            }
            _ => Err(CaError::Disconnected),
        }
    }
}

/// A port's subscriptions, read by its delivery task, never under the
/// scaler's hold: a log that holds no value (C `dbfl_type_ref`) is read from
/// its record, and the scaler may itself be that record.
struct Delivery {
    inputs: Arc<PvInputs>,
    readers: Vec<EventReader>,
    records: Vec<Arc<RecordCell>>,
    closed: [bool; MAX_SCALER_CHANNELS],
    db: Arc<PvDatabase>,
}

enum Next {
    Zero,
    Event(usize, MonitorEvent),
}

/// `ev`'s count, `record`'s read lock released before it returns.
fn count(record: &RecordCell, field: &str, ev: &MonitorEvent) -> Option<u32> {
    match ev.value().as_value() {
        Some(v) => dbr_long(v),
        None => {
            let value = ev.delivered_value(&record.read(), field);
            value.as_ref().and_then(dbr_long)
        }
    }
}

impl Delivery {
    async fn next(&mut self) -> Next {
        let zero = self.inputs.zero.notified();
        let mut zero = std::pin::pin!(zero);
        let (readers, closed) = (&mut self.readers, &mut self.closed);
        std::future::poll_fn(|cx| {
            if zero.as_mut().poll(cx).is_ready() {
                return Poll::Ready(Next::Zero);
            }
            for (i, reader) in readers.iter_mut().enumerate() {
                if closed[i] {
                    continue;
                }
                match reader.poll_recv(cx) {
                    Poll::Ready(Some(ev)) => return Poll::Ready(Next::Event(i, ev)),
                    Poll::Ready(None) => closed[i] = true,
                    Poll::Pending => {}
                }
            }
            Poll::Pending
        })
        .await
    }

    /// C `scalerArmCommand` writes the zeros with `dbPutField` and the next
    /// read takes each input with `dbGetField`; the task writes them once the
    /// arming cycle has released the scaler, then reads each input's field
    /// live. A zero put need not post (a monitor deadband, a refused put), so
    /// nothing queued is taken: what an input posted before that read is
    /// discarded under the input's own lock, which no post can pass.
    async fn write_zeros(&mut self) {
        let arm = self.inputs.state.lock().unwrap().cleared;
        for pv in &self.inputs.names {
            let put = self
                .db
                .put_record_field_from_ca(&pv.record, &pv.field, EpicsValue::Long(0))
                .await;
            if let Err(e) = put {
                errlog_printf(&format!("drvScalerSoft: dbPutField {} 0: {e}\n", pv.name));
            }
        }
        let mut live = [None; MAX_SCALER_CHANNELS];
        let inputs = self.readers.iter_mut().zip(&self.records);
        for ((reader, record), (pv, live)) in inputs.zip(self.inputs.names.iter().zip(&mut live)) {
            let record = record.read();
            while reader.try_recv().is_ok() {}
            *live = record
                .channel_field_value(&pv.field, false)
                .as_ref()
                .and_then(dbr_long);
        }
        {
            let state = &mut *self.inputs.state.lock().unwrap();
            // A later arm's zeros supersede this read.
            if state.cleared == arm {
                for (value, live) in state.values.iter_mut().zip(live) {
                    if let Some(v) = live {
                        *value = v;
                    }
                }
            }
            state.zeroed = arm;
        }
        self.inputs.delivered.send_modify(|n| *n += 1);
    }

    async fn run(mut self) {
        loop {
            match self.next().await {
                Next::Zero => self.write_zeros().await,
                Next::Event(i, ev) => {
                    if let Some(v) = count(&self.records[i], &self.inputs.names[i].field, &ev) {
                        let state = &mut *self.inputs.state.lock().unwrap();
                        if state.cleared == state.zeroed {
                            state.values[i] = v;
                        }
                    }
                    self.inputs.delivered.send_modify(|n| *n += 1);
                }
            }
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
                cleared: 0,
                zeroed: 0,
            }),
            zero: tokio::sync::Notify::new(),
            delivered: tokio::sync::watch::Sender::new(0),
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

    /// The count of input deliveries `port`'s task has taken, an arm's zeros
    /// counting as one: a post is in the counts once this has moved past the
    /// count read before it.
    pub fn deliveries(&self, port: &str) -> Option<tokio::sync::watch::Receiver<u64>> {
        Some(
            self.ports
                .lock()
                .unwrap()
                .get(port)?
                .inputs
                .delivered
                .subscribe(),
        )
    }

    /// C `connectPVs` (`drvScalerSoft.c:558-583`), run once the database is
    /// loaded: subscribe to every input and start the delivery task. A port
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
            let mut records = Vec::with_capacity(pv_inputs.names.len());
            let mut values = [0; MAX_SCALER_CHANNELS];
            for (i, pv) in pv_inputs.names.iter().enumerate() {
                let record = db.get_record(&pv.record);
                let reader = record
                    .as_ref()
                    .ok_or(SubscribeRefused::NoRecord)
                    .and_then(|cell| {
                        cell.write().add_subscriber(
                            &pv.field,
                            i as u32,
                            DbFieldType::Long,
                            EventMask::VALUE.bits(),
                        )
                    });
                // C `connectPVs` never subscribes (`dbNameToAddr` only), so a
                // full cap has no C line: the port just fails.
                if matches!(reader, Err(SubscribeRefused::CapReached)) {
                    break;
                }
                let current = db.get_pv(&format!("{}.{}", pv.record, pv.field)).ok();
                let (Ok(reader), Some(record), Some(current)) = (reader, record, current) else {
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
                records.push(record);
            }
            let complete = inputs.len() == pv_inputs.names.len();
            {
                let mut state = pv_inputs.state.lock().unwrap();
                if !matches!(state.connection, Connection::Unconnected) {
                    continue;
                }
                *state = if complete {
                    PvState {
                        connection: Connection::Connected,
                        values,
                        cleared: 0,
                        zeroed: 0,
                    }
                } else {
                    PvState {
                        connection: Connection::Failed,
                        values: [0; MAX_SCALER_CHANNELS],
                        cleared: 0,
                        zeroed: 0,
                    }
                };
            }
            if !complete {
                continue;
            }

            let delivery = Delivery {
                inputs: pv_inputs,
                readers: inputs,
                records,
                closed: [false; MAX_SCALER_CHANNELS],
                db: Arc::clone(db),
            };
            spawn_background(CallbackPriority::Medium, delivery.run());
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
