//! `saveData_writeXDR.c`'s MDA file writes: `writeScanRecInProgress`,
//! `writeScanRecCompleted`, `saveExtraPV` and the file half of
//! `proc_scan_cpt`, over the scan state the CA side keeps.
//!
//! The scans of one file are a chain (`nxt`), here an index into the
//! task's scan list. One C defect is not copied: every `ftell` failure
//! check compares a `long` against `(u_int)(-1)`, which never matches on
//! LP64, so a failed `getpos` went on to write at offset -1; here it fails
//! the write.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use epics_libcom_rs::runtime::crt_errno;
use parking_lot::Mutex;

use super::write_xdr::Xdr;

/// `FILE_FORMAT_VERSION`.
pub const FILE_FORMAT_VERSION: f32 = 1.4;

pub const SCAN_NBP: usize = 4;
pub const SCAN_NBD: usize = 70;
pub const SCAN_NBT: usize = 4;

pub const XXNV_OK: i16 = 0;
pub const XXNV_BADPV: i16 = 1;
pub const XXNV_NOPV: i16 = 2;

pub const STATUS_INACTIVE: i16 = 0;
pub const STATUS_ACTIVE_OK: i16 = 1;
pub const STATUS_ACTIVE_FS_ERROR: i16 = 2;
pub const STATUS_ERROR: i16 = 3;

/// `MAX_STRING_SIZE`: a `DBR_STRING`, NUL included.
pub const MAX_STRING_SIZE: usize = 40;

/// `debug_saveData`.
pub static DEBUG_SAVE_DATA: AtomicI32 = AtomicI32::new(0);
/// `debug_saveDataMsg`.
pub static DEBUG_SAVE_DATA_MSG: AtomicI32 = AtomicI32::new(0);

/// C `Debug<n>(d, ...)`: printed when `d <= debug_saveData`.
macro_rules! debug {
    ($d:expr, $($arg:tt)*) => {
        if $d <= $crate::save_data::mda::DEBUG_SAVE_DATA.load(std::sync::atomic::Ordering::Relaxed) {
            print!($($arg)*);
        }
    };
}
pub(crate) use debug;

/// One positioner and its readback (`pxnv` … `rxcv`).
#[derive(Debug, Clone)]
pub struct Positioner {
    pub pxnv: i16,
    pub pxpv: String,
    pub pxds: String,
    pub pxsm: String,
    /// `pxeu.units`.
    pub pxeu: String,
    pub rxnv: i16,
    pub rxpv: String,
    pub rxds: String,
    /// `rxeu.units`.
    pub rxeu: String,
    /// `pxra`: the readback array, `mpts` long once allocated.
    pub pxra: Vec<f64>,
    pub pxra_fpos: i64,
    pub rxcv: f64,
}

impl Default for Positioner {
    fn default() -> Self {
        Self {
            pxnv: XXNV_NOPV,
            pxpv: String::new(),
            pxds: String::new(),
            pxsm: String::new(),
            pxeu: String::new(),
            rxnv: XXNV_NOPV,
            rxpv: String::new(),
            rxds: String::new(),
            rxeu: String::new(),
            pxra: Vec::new(),
            pxra_fpos: 0,
            rxcv: 0.0,
        }
    }
}

impl Positioner {
    /// C's test for a positioner the file carries.
    pub fn valid(&self) -> bool {
        self.pxnv == XXNV_OK || self.rxnv == XXNV_OK
    }
}

/// One detector (`dxnv` … `dxcv`).
#[derive(Debug, Clone)]
pub struct Detector {
    pub dxnv: i16,
    pub dxpv: String,
    pub dxds: String,
    /// `dxeu.units`.
    pub dxeu: String,
    /// `dxda`, allocated `mpts` long once the detector is valid.
    pub dxda: Option<Vec<f32>>,
    pub dxda_fpos: i64,
    pub dxcv: f32,
}

impl Default for Detector {
    fn default() -> Self {
        Self {
            dxnv: XXNV_NOPV,
            dxpv: String::new(),
            dxds: String::new(),
            dxeu: String::new(),
            dxda: None,
            dxda_fpos: 0,
            dxcv: 0.0,
        }
    }
}

/// One trigger (`txnv` … `txpvRec`).
#[derive(Debug, Clone)]
pub struct Trigger {
    pub txnv: i16,
    /// 0 when the trigger is another sscan record's `.EXSC`.
    pub txsc: i16,
    pub txpv: String,
    pub txcd: f32,
    pub txpv_rec: String,
}

impl Default for Trigger {
    fn default() -> Self {
        Self {
            txnv: XXNV_NOPV,
            txsc: 1,
            txpv: String::new(),
            txcd: 0.0,
            txpv_rec: String::new(),
        }
    }
}

/// C `SCAN` without its channels.
#[derive(Debug, Clone)]
pub struct Scan {
    pub name: String,
    pub scan_dim: i16,
    pub fname: String,
    pub ffname: String,
    pub first_scan: bool,
    /// The inner scan, an index into the scan list.
    pub nxt: Option<usize>,
    pub saved_seek_pos: i64,
    pub offset: i64,
    pub offset_extra_pv: i64,
    pub counter: i64,
    pub dims_offset: i64,
    pub regular_offset: i64,
    pub old_npts: i64,
    pub data: i16,
    pub mpts: i64,
    pub npts: i64,
    pub cpt: i64,
    pub bcpt: i64,
    pub cpt_fpos: i64,
    pub time_fpos: i64,
    pub nb_pos: i32,
    pub nb_det: i32,
    pub nb_trg: i32,
    pub pos: [Positioner; SCAN_NBP],
    pub det: Vec<Detector>,
    pub trg: [Trigger; SCAN_NBT],
}

impl Scan {
    /// `connectScan`'s initial state.
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            scan_dim: 1,
            fname: String::new(),
            ffname: String::new(),
            first_scan: true,
            nxt: None,
            saved_seek_pos: 0,
            offset: 0,
            offset_extra_pv: 0,
            counter: 0,
            dims_offset: 0,
            regular_offset: 0,
            old_npts: 0,
            data: -1,
            mpts: 0,
            npts: 0,
            cpt: 0,
            bcpt: 0,
            cpt_fpos: 0,
            time_fpos: 0,
            nb_pos: 0,
            nb_det: 0,
            nb_trg: 0,
            pos: Default::default(),
            det: vec![Detector::default(); SCAN_NBD],
            trg: Default::default(),
        }
    }
}

/// An extra PV's value as the last `DBR_CTRL_*` / `DBR_STRING` get left it
/// (`PV_NODE.pval`, `count`).
#[derive(Debug, Clone, PartialEq)]
pub enum ExtraValue {
    String(String),
    Char { units: String, value: Vec<u8> },
    Short { units: String, value: Vec<i16> },
    Long { units: String, value: Vec<i32> },
    Float { units: String, value: Vec<f32> },
    Double { units: String, value: Vec<f64> },
}

impl ExtraValue {
    /// The `dbr_type` the file records.
    pub fn dbr_type(&self) -> i32 {
        match self {
            ExtraValue::String(_) => 0,
            ExtraValue::Short { .. } => 29,
            ExtraValue::Float { .. } => 30,
            ExtraValue::Char { .. } => 32,
            ExtraValue::Long { .. } => 33,
            ExtraValue::Double { .. } => 34,
        }
    }

    fn count(&self) -> usize {
        match self {
            ExtraValue::String(_) => 1,
            ExtraValue::Char { value, .. } => value.len(),
            ExtraValue::Short { value, .. } => value.len(),
            ExtraValue::Long { value, .. } => value.len(),
            ExtraValue::Float { value, .. } => value.len(),
            ExtraValue::Double { value, .. } => value.len(),
        }
    }
}

/// One `PV_NODE`: the name, and the description and value its gets fill
/// under the node's lock.
#[derive(Debug)]
pub struct ExtraPv {
    pub name: String,
    pub state: Mutex<ExtraPvState>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExtraPvState {
    pub desc: String,
    pub value: ExtraValue,
}

/// What the file writes reach beyond the scan: `save_status` and its PV,
/// the `[message]` PV, and the extra PVs.
pub trait Env {
    fn save_status(&self) -> i16;
    /// `save_status = status`, then the put to the `[status]` PV.
    fn set_save_status(&mut self, status: i16);
    /// `sendUserMessage`.
    fn user_message(&mut self, msg: &str);
    fn extra_pvs(&self) -> &[Arc<ExtraPv>];
}

/// `msg[MAX_STRING_SIZE-1] = '\0'`.
pub fn truncate_message(msg: &mut String) {
    let mut end = msg.len().min(MAX_STRING_SIZE - 1);
    while !msg.is_char_boundary(end) {
        end -= 1;
    }
    msg.truncate(end);
}

/// C `fileStatus`: `stat`, a trailing `/` or `\` ignored.
pub fn file_status(fname: &str) -> std::io::Result<()> {
    let local = fname
        .strip_suffix('/')
        .or_else(|| fname.strip_suffix('\\'))
        .unwrap_or(fname);
    std::fs::metadata(local).map(|_| ()).inspect_err(|e| {
        if DEBUG_SAVE_DATA.load(Ordering::Relaxed) != 0 {
            println!(
                "saveData: stat returned -1 for filename '{local}'; errno={}",
                crt_errno::errno(e).unwrap_or(0)
            );
        }
    })
}

type MdaFile = Xdr<BufWriter<File>>;

fn errno_text(e: &std::io::Error) -> (i32, String) {
    (crt_errno::errno(e).unwrap_or(0), crt_errno::strerror(e))
}

/// C `fclose` with the retry after 0.1 s: true when the buffered bytes
/// reached the file. C's retry calls `fclose` on the stream it already
/// closed; this retries the flush.
fn close(fd: MdaFile, who: &str, ffname: &str) -> bool {
    let mut w = fd.into_inner();
    if let Err(e) = w.flush() {
        let (errno, text) = errno_text(&e);
        println!("saveData:{who}: fclose({ffname}) returned -1, errno = {errno} ('{text}')");
        std::thread::sleep(Duration::from_millis(100));
        if let Err(e) = w.flush() {
            let (errno, text) = errno_text(&e);
            println!(
                "saveData:{who}: retry fclose({ffname}) returned -1, errno = {errno} ('{text}')"
            );
            return false;
        }
    }
    true
}

fn open(ffname: &str, create: bool) -> std::io::Result<MdaFile> {
    let file = if create {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(ffname)?
    } else {
        OpenOptions::new().read(true).write(true).open(ffname)?
    };
    Ok(Xdr::new(BufWriter::new(file)))
}

/// `saveExtraPV`: true when every write succeeded.
fn save_extra_pv(fd: &mut MdaFile, extra: &[Arc<ExtraPv>]) -> bool {
    let mut ok = fd.int(extra.len() as i32);
    for pv in extra {
        let state = pv.state.lock();
        ok &= fd.counted_string(pv.name.as_bytes());
        ok &= fd.counted_string(state.desc.as_bytes());
        let value = &state.value;
        ok &= fd.int(value.dbr_type());
        if !matches!(value, ExtraValue::String(_)) {
            ok &= fd.long(value.count() as i64);
        }
        match value {
            ExtraValue::String(s) => ok &= fd.counted_string(s.as_bytes()),
            ExtraValue::Char { units, value } => {
                ok &= fd.counted_string(units.as_bytes());
                ok &= fd.vector(value, Xdr::char);
            }
            ExtraValue::Short { units, value } => {
                ok &= fd.counted_string(units.as_bytes());
                ok &= fd.vector(value, Xdr::short);
            }
            ExtraValue::Long { units, value } => {
                ok &= fd.counted_string(units.as_bytes());
                ok &= fd.vector(value, Xdr::int);
            }
            ExtraValue::Float { units, value } => {
                ok &= fd.counted_string(units.as_bytes());
                ok &= fd.vector(value, Xdr::float);
            }
            ExtraValue::Double { units, value } => {
                ok &= fd.counted_string(units.as_bytes());
                ok &= fd.vector(value, Xdr::double);
            }
        }
    }
    ok
}

/// `ftell` as the file writes use it: `None` on failure.
fn getpos(fd: &mut MdaFile) -> Option<i64> {
    match fd.getpos() {
        -1 => None,
        pos => Some(pos),
    }
}

/// `writeScanRecInProgress`: open the file, write the header when this is
/// the outermost scan, the scan's header and info, and reserve its data.
/// `stamp` is the scan's start time as `epicsTimeToStrftime` gave it.
pub fn write_scan_rec_in_progress(
    scans: &mut [Scan],
    i: usize,
    stamp: &str,
    is_retry: bool,
    env: &mut impl Env,
) -> bool {
    const WHO: &str = "writeScanRecInProgress";
    debug!(
        3,
        "saveData:writeScanRecInProgress: Opening file '{}'\n", scans[i].ffname
    );
    let first_scan = scans[i].first_scan;
    let opened = match open(&scans[i].ffname, first_scan) {
        Ok(fd) => match file_status(&scans[i].ffname) {
            Ok(()) => Some(fd),
            Err(e) => {
                let (errno, text) = errno_text(&e);
                println!(
                    "saveData:writeScanRecInProgress: stat({}) set errno to {errno} ('{text}')",
                    scans[i].ffname
                );
                None
            }
        },
        Err(_) => None,
    };
    let mut fd = match opened {
        Some(fd) => fd,
        None => {
            println!(
                "saveData:writeScanRecInProgress({}): can't open data file!!",
                scans[i].name
            );
            let mut msg = format!("!! Can't open file {}", scans[i].fname);
            truncate_message(&mut msg);
            env.user_message(&msg);
            env.set_save_status(STATUS_ERROR);
            return false;
        }
    };

    if !first_scan {
        let pscan = &mut scans[i];
        if is_retry && pscan.saved_seek_pos != 0 {
            // A failed earlier attempt moved the end of the file: go back
            // to where it was on the first attempt.
            if fd
                .get_mut()
                .seek(SeekFrom::Start(pscan.saved_seek_pos as u64))
                .is_err()
            {
                return false;
            }
        } else {
            // Append this scan to the data file.
            match fd.get_mut().seek(SeekFrom::End(0)) {
                Ok(end) => pscan.saved_seek_pos = end as i64,
                Err(_) => {
                    pscan.saved_seek_pos = 0;
                    return false;
                }
            }
        }
    }

    let ok = write_in_progress_body(scans, i, stamp, &mut fd);
    let closed = close(fd, WHO, &scans[i].ffname);
    if ok {
        if env.save_status() == STATUS_ERROR {
            env.set_save_status(STATUS_ACTIVE_OK);
        }
        if is_retry {
            println!(
                "saveData:writeScanRecInProgress({}): retry succeeded",
                scans[i].name
            );
            let mut msg = format!("Retry succeeded for {}", scans[i].fname);
            truncate_message(&mut msg);
            env.user_message(&msg);
        } else {
            env.user_message(&format!("Wrote data to {}", scans[i].fname));
        }
    }
    ok && closed
}

/// The writes of `writeScanRecInProgress` up to its `cleanup:` label.
fn write_in_progress_body(scans: &mut [Scan], i: usize, stamp: &str, fd: &mut MdaFile) -> bool {
    let nxt = scans[i].nxt;
    {
        let pscan = &mut scans[i];
        if pscan.first_scan {
            debug!(3, "saveData:writeScanRecInProgress: Writing file header\n");
            let mut ok = fd.float(FILE_FORMAT_VERSION);
            ok &= fd.long(pscan.counter);
            debug!(
                2,
                "saveData:writeScanRecInProgress: scan_dim={}\n", pscan.scan_dim
            );
            ok &= fd.short(pscan.scan_dim);
            let Some(dims_offset) = getpos(fd) else {
                return false;
            };
            pscan.dims_offset = dims_offset;
            debug!(
                2,
                "saveData:writeScanRecInProgress:({}) scan_dim={}, dims_offset={}\n",
                pscan.name,
                pscan.scan_dim,
                pscan.dims_offset
            );
            for _ in 0..pscan.scan_dim {
                ok &= fd.int(-1);
            }
            let Some(regular_offset) = getpos(fd) else {
                return false;
            };
            pscan.regular_offset = regular_offset;
            // regular (rectangular array) = true
            ok &= fd.int(1);
            let Some(offset_extra_pv) = getpos(fd) else {
                return false;
            };
            pscan.offset_extra_pv = offset_extra_pv;
            ok &= fd.long(0);
            if !ok {
                return false;
            }
            debug!(3, "saveData:writeScanRecInProgress: File Header written\n");
        }
    }

    // The inner scan's offsets.
    if let Some(n) = nxt {
        let (dims_offset, regular_offset) = (scans[i].dims_offset, scans[i].regular_offset);
        scans[n].dims_offset = dims_offset + 4;
        debug!(
            2,
            "saveData:proc_scan_data:({}) scan_dim={}, dims_offset={}\n",
            scans[i].name,
            scans[i].scan_dim,
            dims_offset
        );
        scans[n].regular_offset = regular_offset;
    }

    let Some(scan_offset) = getpos(fd) else {
        return false;
    };
    debug!(
        2,
        "saveData:writeScanRecInProgress:({}) scan_offset={}\n", scans[i].name, scan_offset
    );

    // Scan header.
    debug!(
        3,
        "saveData:writeScanRecInProgress: Writing per-scan header\n"
    );
    let mut ok = fd.short(scans[i].scan_dim);
    ok &= fd.long(scans[i].npts);
    let Some(cpt_fpos) = getpos(fd) else {
        return false;
    };
    scans[i].cpt_fpos = cpt_fpos;
    ok &= fd.long(scans[i].cpt);
    if scans[i].scan_dim > 1 {
        // The inner scans' offsets go here.
        let Some(table) = getpos(fd) else {
            return false;
        };
        if let Some(n) = nxt {
            scans[n].offset = table;
            debug!(
                15,
                "saveData:writeScanRecInProgress({}) telling {} to write its next offset at loc {}\n",
                scans[i].name,
                scans[n].name,
                table
            );
        }
        ok &= fd.setpos(table + scans[i].npts * 4);
    }
    if !ok {
        return false;
    }

    // Scan info.
    let pscan = &mut scans[i];
    debug!(3, "saveData:writeScanRecInProgress: Save scan info\n");
    ok &= fd.counted_string(pscan.name.as_bytes());
    let Some(time_fpos) = getpos(fd) else {
        return false;
    };
    pscan.time_fpos = time_fpos;
    ok &= fd.counted_string(stamp.as_bytes());
    ok &= fd.int(pscan.nb_pos);
    ok &= fd.int(pscan.nb_det);
    ok &= fd.int(pscan.nb_trg);
    if !ok {
        return false;
    }

    if pscan.nb_pos != 0 {
        for (n, p) in pscan.pos.iter().enumerate() {
            if p.valid() {
                debug!(3, "saveData:writeScanRecInProgress: Pos[{n}] info\n");
                ok &= fd.int(n as i32);
                for s in [
                    &p.pxpv, &p.pxds, &p.pxsm, &p.pxeu, &p.rxpv, &p.rxds, &p.rxeu,
                ] {
                    ok &= fd.counted_string(s.as_bytes());
                }
            }
        }
    }
    if !ok {
        return false;
    }
    if pscan.nb_det != 0 {
        for (n, d) in pscan.det.iter().enumerate() {
            if d.dxnv == XXNV_OK {
                debug!(3, "saveData:writeScanRecInProgress: Det[{n}] info\n");
                ok &= fd.int(n as i32);
                for s in [&d.dxpv, &d.dxds, &d.dxeu] {
                    ok &= fd.counted_string(s.as_bytes());
                }
            }
        }
    }
    if !ok {
        return false;
    }
    if pscan.nb_trg != 0 {
        for (n, t) in pscan.trg.iter().enumerate() {
            if t.txnv == XXNV_OK {
                debug!(3, "saveData:writeScanRecInProgress: Trg[{n}] info\n");
                ok &= fd.int(n as i32);
                ok &= fd.counted_string(t.txpv.as_bytes());
                ok &= fd.float(t.txcd);
            }
        }
    }
    if !ok {
        return false;
    }

    // File space for the data.
    let Some(data_start) = getpos(fd) else {
        return false;
    };
    let mut data_size = 0;
    if pscan.nb_pos != 0 {
        for (n, p) in pscan.pos.iter_mut().enumerate() {
            if p.valid() {
                debug!(
                    3,
                    "saveData:writeScanRecInProgress: Allocate space for Pos[{n}]\n"
                );
                p.pxra_fpos = data_start + data_size;
                data_size += pscan.npts * 8;
            }
        }
    }
    if pscan.nb_det != 0 {
        for (n, d) in pscan.det.iter_mut().enumerate() {
            if d.dxnv == XXNV_OK {
                debug!(
                    3,
                    "saveData:writeScanRecInProgress: Allocate space for Det[{n}]\n"
                );
                d.dxda_fpos = data_start + data_size;
                data_size += pscan.npts * 4;
            }
        }
    }
    if data_size > 0 {
        if fd.get_mut().seek(SeekFrom::Current(data_size - 1)).is_err() {
            return false;
        }
        // C does not check this fwrite.
        let _ = fd.get_mut().write_all(&[0]);
    }

    if pscan.old_npts < pscan.npts {
        ok &= fd.setpos(pscan.dims_offset);
        debug!(
            2,
            "saveData:writeScanRecInProgress:({}) scan_dim={}, dims_offset={}\n",
            pscan.name,
            pscan.scan_dim,
            pscan.dims_offset
        );
        if !ok {
            return false;
        }
        ok &= fd.long(pscan.npts);
    }
    if pscan.old_npts != -1 && pscan.old_npts != pscan.npts {
        // npts changed during a multidimensional scan: regular = false.
        ok &= fd.setpos(pscan.regular_offset);
        if !ok {
            return false;
        }
        ok &= fd.int(0);
    }
    pscan.old_npts = pscan.npts;

    if !pscan.first_scan {
        debug!(
            15,
            "saveData:writeScanRecInProgress({}): writing offset {} at loc {}\n",
            pscan.name,
            scan_offset,
            pscan.offset
        );
        ok &= fd.setpos(pscan.offset);
        if !ok {
            return false;
        }
        ok &= fd.long(scan_offset);
        let Some(next) = getpos(fd) else {
            return false;
        };
        pscan.offset = next;
    }
    ok
}

/// `writeScanRecCompleted`: write the arrays `fetch` fills (C's
/// `ca_array_get`s of `PxRA`/`DxDA`, `false` when they did not all
/// complete), the last point, and, for the outermost scan, the extra PVs.
pub fn write_scan_rec_completed(
    scan: &mut Scan,
    is_retry: bool,
    env: &mut impl Env,
    fetch: impl FnOnce(&mut Scan) -> bool,
) -> bool {
    const WHO: &str = "writeScanRecCompleted";
    let mut fd = match open(&scan.ffname, false) {
        Ok(fd) if file_status(&scan.ffname).is_ok() => fd,
        _ => {
            println!(
                "saveData:writeScanRecCompleted({}): can't open data file!!",
                scan.name
            );
            let mut msg = format!("!! Can't open file {}", scan.fname);
            truncate_message(&mut msg);
            env.user_message(&msg);
            env.set_save_status(STATUS_ERROR);
            return false;
        }
    };

    debug!(
        3,
        "saveData:writeScanRecCompleted: writing {} to {}\n", scan.name, scan.fname
    );
    // The cpt that goes with the buffered arrays.
    scan.cpt = scan.bcpt;
    debug!(
        5,
        "saveData:writeScanRecCompleted({}): bcpt={}\n", scan.name, scan.bcpt
    );

    // A valid detector with no array yet gets one (C allocates it here).
    if scan.nb_det != 0 {
        for (n, d) in scan.det.iter_mut().enumerate() {
            if d.dxnv == XXNV_OK && d.dxda.is_none() {
                d.dxda = Some(vec![0.0; scan.mpts.max(0) as usize]);
                println!(
                    "saveData:writeScanRecCompleted: Allocated array for det {}.D{:02}DA",
                    scan.name,
                    n + 1
                );
                let mut msg = format!("Allocated mem for {}.D{:02}DA", scan.name, n + 1);
                truncate_message(&mut msg);
                env.user_message(&msg);
            }
        }
    }
    let fetched = fetch(scan);
    // Zero the points the scan did not acquire.
    let (bcpt, npts) = (scan.bcpt.max(0) as usize, scan.npts.max(0) as usize);
    if scan.nb_pos != 0 {
        for p in scan.pos.iter_mut().filter(|p| p.valid()) {
            for v in p.pxra.iter_mut().take(npts).skip(bcpt) {
                *v = 0.0;
            }
        }
    }
    if scan.nb_det != 0 {
        for d in scan.det.iter_mut() {
            if let Some(dxda) = d.dxda.as_mut() {
                for v in dxda.iter_mut().take(npts).skip(bcpt) {
                    *v = 0.0;
                }
            }
        }
    }
    if !fetched {
        debug!(
            3,
            "saveData:writeScanRecCompleted: unable to get all valid arrays \n"
        );
        env.user_message("!! Can't get data");
        return false;
    }

    let ok = write_completed_body(scan, is_retry, env, &mut fd);
    let closed = close(fd, WHO, &scan.ffname);
    if ok {
        if env.save_status() == STATUS_ERROR {
            env.set_save_status(STATUS_ACTIVE_OK);
        }
        if is_retry {
            println!(
                "saveData:writeScanRecCompleted({}): retry succeeded",
                scan.name
            );
            let mut msg = format!("Retry succeeded for '{}'", scan.name);
            truncate_message(&mut msg);
            env.user_message(&msg);
        } else {
            env.user_message(&format!("Wrote data to {}", scan.fname));
        }
    }
    ok && closed
}

/// An array `npts` long as C's `writeXDR_vector` reads it: the buffer,
/// zero past its end (C's buffer is `mpts` long and zeroed at allocation).
fn padded<T: Copy + Default>(v: &[T], npts: i64) -> Vec<T> {
    let npts = npts.max(0) as usize;
    let mut out: Vec<T> = v.iter().copied().take(npts).collect();
    out.resize(npts, T::default());
    out
}

fn write_completed_body(
    scan: &mut Scan,
    is_retry: bool,
    env: &mut impl Env,
    fd: &mut MdaFile,
) -> bool {
    let mut ok = true;
    if scan.nb_pos != 0 {
        for p in scan.pos.iter().filter(|p| p.valid()) {
            ok &= fd.setpos(p.pxra_fpos);
            if !ok {
                return false;
            }
            ok &= fd.vector(&padded(&p.pxra, scan.npts), Xdr::double);
        }
    }
    if scan.nb_det != 0 {
        for d in scan.det.iter().filter(|d| d.dxnv == XXNV_OK) {
            if let Some(dxda) = &d.dxda {
                ok &= fd.setpos(d.dxda_fpos);
                if !ok {
                    return false;
                }
                ok &= fd.vector(&padded(dxda, scan.npts), Xdr::float);
            }
        }
    }
    ok &= fd.setpos(scan.cpt_fpos);
    if !ok {
        return false;
    }
    ok &= fd.long(scan.bcpt);

    if scan.first_scan {
        // The extra PVs go at the end of the file; a retry goes back to
        // where the end was on the first attempt.
        if is_retry && scan.saved_seek_pos != 0 {
            if fd
                .get_mut()
                .seek(SeekFrom::Start(scan.saved_seek_pos as u64))
                .is_err()
            {
                return false;
            }
        } else {
            match fd.get_mut().seek(SeekFrom::End(0)) {
                Ok(end) => scan.saved_seek_pos = end as i64,
                Err(_) => {
                    scan.saved_seek_pos = 0;
                    return false;
                }
            }
        }
        let Some(extra_at) = getpos(fd) else {
            return false;
        };
        ok &= save_extra_pv(fd, env.extra_pvs());
        ok &= fd.setpos(scan.offset_extra_pv);
        if !ok {
            return false;
        }
        ok &= fd.long(extra_at);
        env.user_message(&format!("Done writing {}", scan.fname));
    }
    ok
}

/// The file half of `proc_scan_cpt`: point `cpt` of every valid
/// positioner (`rxcv`) and detector (`dxcv`).
pub fn write_cpt(scan: &Scan, env: &mut impl Env) {
    let mut fd = match open(&scan.ffname, false) {
        Ok(fd) => fd,
        Err(_) => {
            println!(
                "saveData:proc_scan_cpt({}): can't open data file!!",
                scan.name
            );
            let mut msg = format!("!! Can't open file {}", scan.fname);
            truncate_message(&mut msg);
            env.user_message(&msg);
            env.set_save_status(STATUS_ERROR);
            return;
        }
    };
    let ok = write_cpt_body(scan, &mut fd);
    if ok && env.save_status() == STATUS_ERROR {
        let mut msg = format!("Wrote data to {}", scan.fname);
        truncate_message(&mut msg);
        env.user_message(&msg);
        env.set_save_status(STATUS_ACTIVE_OK);
    }
    close(fd, "proc_scan_cpt", &scan.ffname);
}

fn write_cpt_body(scan: &Scan, fd: &mut MdaFile) -> bool {
    if !(fd.setpos(scan.cpt_fpos) && fd.long(scan.cpt)) {
        return false;
    }
    if scan.nb_pos != 0 {
        for p in scan.pos.iter().filter(|p| p.valid()) {
            if !(fd.setpos(p.pxra_fpos + (scan.cpt - 1) * 8) && fd.double(p.rxcv)) {
                return false;
            }
        }
    }
    if scan.nb_det != 0 {
        for d in scan.det.iter().filter(|d| d.dxnv == XXNV_OK) {
            if !(fd.setpos(d.dxda_fpos + (scan.cpt - 1) * 4) && fd.float(d.dxcv)) {
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAMP: &str = "Sep 09, 2021 01:46:40.123457";

    struct TestEnv {
        status: i16,
        messages: Vec<String>,
        extra: Vec<Arc<ExtraPv>>,
    }

    impl Env for TestEnv {
        fn save_status(&self) -> i16 {
            self.status
        }
        fn set_save_status(&mut self, status: i16) {
            self.status = status;
        }
        fn user_message(&mut self, msg: &str) {
            self.messages.push(msg.to_string());
        }
        fn extra_pvs(&self) -> &[Arc<ExtraPv>] {
            &self.extra
        }
    }

    fn pv(name: &str, desc: &str, value: ExtraValue) -> Arc<ExtraPv> {
        Arc::new(ExtraPv {
            name: name.into(),
            state: Mutex::new(ExtraPvState {
                desc: desc.into(),
                value,
            }),
        })
    }

    /// `extra_pvs()` of `tests/fixtures/save_data/mda.c`.
    fn env() -> TestEnv {
        let units = |u: &str| u.to_string();
        TestEnv {
            status: STATUS_ACTIVE_OK,
            messages: Vec::new(),
            extra: vec![
                pv("ioc:str", "a string", ExtraValue::String("hello".into())),
                pv(
                    "ioc:chr",
                    "",
                    ExtraValue::Char {
                        units: units("ch"),
                        value: vec![b'a', 0x80, b'c'],
                    },
                ),
                pv(
                    "ioc:shrt",
                    "shorts",
                    ExtraValue::Short {
                        units: units("s"),
                        value: vec![-1, 2],
                    },
                ),
                pv(
                    "ioc:lng",
                    "long",
                    ExtraValue::Long {
                        units: units("counts"),
                        value: vec![100000],
                    },
                ),
                pv(
                    "ioc:flt",
                    "floats",
                    ExtraValue::Float {
                        units: units("V"),
                        value: vec![1.5, 2.5],
                    },
                ),
                pv(
                    "ioc:dbl",
                    "double",
                    ExtraValue::Double {
                        units: units(""),
                        value: vec![3.25],
                    },
                ),
            ],
        }
    }

    fn new_scan(name: &str, mpts: i64) -> Scan {
        let mut s = Scan::new(name);
        s.mpts = mpts;
        s.data = 0;
        s
    }

    fn set_pos(s: &mut Scan, i: usize, pxnv: i16, rxnv: i16, tag: &str) {
        let n = i + 1;
        let mpts = s.mpts as usize;
        s.pos[i] = Positioner {
            pxnv,
            pxpv: format!("{tag}:m{n}"),
            pxds: format!("motor {n}"),
            pxsm: "LINEAR".into(),
            pxeu: "mm".into(),
            rxnv,
            rxpv: format!("{tag}:m{n}.RBV"),
            rxds: format!("readback {n}"),
            rxeu: if rxnv == XXNV_OK { "um" } else { "" }.into(),
            pxra: (0..mpts)
                .map(|j| n as f64 * 100.0 + j as f64 * 0.5)
                .collect(),
            ..Positioner::default()
        };
    }

    fn set_det(s: &mut Scan, i: usize, alloc: bool, tag: &str) {
        let n = i + 1;
        let mpts = s.mpts as usize;
        s.det[i] = Detector {
            dxnv: XXNV_OK,
            dxpv: format!("{tag}:det{n}"),
            dxds: format!("detector {n}"),
            dxeu: "cts".into(),
            dxda: alloc.then(|| {
                (0..mpts)
                    .map(|j| n as f32 * 10.0 + j as f32 * 0.25)
                    .collect()
            }),
            ..Detector::default()
        };
    }

    fn count(s: &mut Scan) {
        s.nb_pos = s.pos.iter().filter(|p| p.valid()).count() as i32;
        s.nb_det = s.det.iter().filter(|d| d.dxnv == XXNV_OK).count() as i32;
        s.nb_trg = s.trg.iter().filter(|t| t.txnv == XXNV_OK).count() as i32;
    }

    fn point(s: &mut Scan, cpt: i64, env: &mut TestEnv) {
        for (i, p) in s.pos.iter_mut().enumerate() {
            p.rxcv = -((i + 1) as f64) - cpt as f64 * 0.125;
        }
        for (i, d) in s.det.iter_mut().enumerate() {
            d.dxcv = (cpt * 1000 + i as i64) as f32;
        }
        s.cpt = cpt;
        write_cpt(s, env);
    }

    fn start(scans: &mut [Scan], i: usize, retry: bool, env: &mut TestEnv) {
        count(&mut scans[i]);
        scans[i].cpt = 0;
        if scans[i].first_scan {
            let mut dim = 0;
            let mut cur = Some(i);
            while let Some(c) = cur {
                dim += 1;
                scans[c].old_npts = -1;
                cur = scans[c].nxt;
            }
            scans[i].scan_dim = dim;
        }
        if let Some(n) = scans[i].nxt {
            let (dim, fname, ffname) = (
                scans[i].scan_dim,
                scans[i].fname.clone(),
                scans[i].ffname.clone(),
            );
            scans[n].first_scan = false;
            scans[n].scan_dim = dim - 1;
            scans[n].fname = fname;
            scans[n].ffname = ffname;
        }
        scans[i].saved_seek_pos = 0;
        assert!(write_scan_rec_in_progress(scans, i, STAMP, false, env));
        if retry {
            assert!(write_scan_rec_in_progress(scans, i, STAMP, true, env));
        }
    }

    fn end(scans: &mut [Scan], i: usize, bcpt: i64, retry: bool, env: &mut TestEnv) {
        scans[i].bcpt = bcpt;
        scans[i].saved_seek_pos = 0;
        assert!(write_scan_rec_completed(&mut scans[i], false, env, |_| {
            true
        }));
        if retry {
            assert!(write_scan_rec_completed(&mut scans[i], true, env, |_| true));
        }
        if scans[i].first_scan {
            let mut cur = scans[i].nxt;
            while let Some(c) = cur {
                scans[c].first_scan = true;
                cur = scans[c].nxt;
            }
        }
    }

    /// The 1D scan of `mda.c`: a readback-only positioner, a trigger, a
    /// detector without an array, stopped after 3 of 5 points.
    #[test]
    fn scan_1d_matches_c() {
        let dir = tempfile::tempdir().unwrap();
        let mut env = env();
        let mut s = new_scan("ioc:scan1", 10);
        s.counter = 7;
        s.npts = 5;
        set_pos(&mut s, 0, XXNV_OK, XXNV_OK, "ioc");
        set_pos(&mut s, 2, XXNV_BADPV, XXNV_OK, "ioc");
        set_det(&mut s, 0, true, "ioc");
        set_det(&mut s, 5, true, "ioc");
        set_det(&mut s, 69, false, "ioc");
        s.trg[1].txnv = XXNV_OK;
        s.trg[1].txpv = "ioc:scanH.EXSC".into();
        s.trg[1].txcd = 1.0;
        s.fname = "t_0007.mda".into();
        s.ffname = dir.path().join("mda1d.mda").to_str().unwrap().into();
        let mut scans = vec![s];
        start(&mut scans, 0, false, &mut env);
        for p in 1..=3 {
            point(&mut scans[0], p, &mut env);
        }
        end(&mut scans, 0, 3, false, &mut env);
        assert_eq!(
            std::fs::read(&scans[0].ffname).unwrap(),
            include_bytes!("../../tests/fixtures/save_data/mda1d.mda")
        );
        assert_eq!(
            env.messages,
            [
                "Wrote data to t_0007.mda",
                "Allocated mem for ioc:scan1.D70DA",
                "Done writing t_0007.mda",
                "Wrote data to t_0007.mda",
            ]
        );
    }

    /// The 2D scan of `mda.c`: the inner npts drops on the last outer
    /// point, and an inner start and the outer end are retried.
    #[test]
    fn scan_2d_matches_c() {
        let dir = tempfile::tempdir().unwrap();
        let mut env = env();
        let mut o = new_scan("ioc:scan2", 3);
        let mut inner = new_scan("ioc:scan1", 4);
        o.nxt = Some(1);
        o.counter = 8;
        o.npts = 3;
        set_pos(&mut o, 1, XXNV_OK, XXNV_NOPV, "out");
        set_det(&mut o, 2, true, "out");
        inner.npts = 4;
        set_pos(&mut inner, 0, XXNV_OK, XXNV_OK, "in");
        set_det(&mut inner, 0, true, "in");
        set_det(&mut inner, 1, true, "in");
        o.fname = "t_0008.mda".into();
        o.ffname = dir.path().join("mda2d.mda").to_str().unwrap().into();
        let mut scans = vec![o, inner];
        start(&mut scans, 0, false, &mut env);
        for k in 1..=3 {
            if k == 3 {
                scans[1].npts = 3;
            }
            start(&mut scans, 1, k == 2, &mut env);
            for p in 1..=scans[1].npts {
                point(&mut scans[1], p, &mut env);
            }
            let npts = scans[1].npts;
            end(&mut scans, 1, npts, false, &mut env);
            point(&mut scans[0], k, &mut env);
        }
        end(&mut scans, 0, 3, true, &mut env);
        assert_eq!(
            std::fs::read(&scans[0].ffname).unwrap(),
            include_bytes!("../../tests/fixtures/save_data/mda2d.mda")
        );
        assert_eq!(
            env.messages.last().unwrap(),
            "Retry succeeded for 'ioc:scan2'"
        );
    }

    /// A missing file is C's "can't open": the error status and message.
    #[test]
    fn missing_file_sets_error_status() {
        let dir = tempfile::tempdir().unwrap();
        let mut env = env();
        let mut s = new_scan("ioc:scan1", 4);
        s.first_scan = false;
        s.fname = "gone.mda".into();
        s.ffname = dir.path().join("gone.mda").to_str().unwrap().into();
        let mut scans = vec![s];
        assert!(!write_scan_rec_in_progress(
            &mut scans, 0, STAMP, false, &mut env
        ));
        assert_eq!(env.status, STATUS_ERROR);
        assert_eq!(env.messages, ["!! Can't open file gone.mda"]);
    }

    /// A missing directory is the CRT's ENOENT on Windows too, not
    /// ERROR_PATH_NOT_FOUND's code and sentence.
    #[test]
    fn a_failed_stat_reads_as_c_errno() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gone").join("x.mda");
        let e = file_status(path.to_str().unwrap()).unwrap_err();
        assert_eq!(errno_text(&e), (2, "No such file or directory".into()));
    }
}
