pub mod registry;
pub use registry::{
    PortEntry, PortRegistry, asyn_record_factory, get_port, register_asyn_record_type,
    register_port,
};

use std::sync::Arc;

use epics_base_rs::error::{CaError, CaResult};
use epics_base_rs::server::record::{FieldDesc, ProcessOutcome, Record};
use epics_base_rs::types::{DbFieldType, EpicsValue};

use crate::port_handle::PortHandle;
use crate::trace::{TraceFile, TraceInfoMask, TraceIoMask, TraceManager, TraceMask};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
enum TransferMode {
    WriteRead = 0,
    Write = 1,
    Read = 2,
    Flush = 3,
    NoIo = 4,
}

impl TransferMode {
    fn from_u16(v: u16) -> Self {
        match v {
            0 => Self::WriteRead,
            1 => Self::Write,
            2 => Self::Read,
            3 => Self::Flush,
            4 => Self::NoIo,
            _ => Self::WriteRead,
        }
    }
}

// ===== Interface Type =====

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
enum InterfaceType {
    Octet = 0,
    Int32 = 1,
    UInt32Digital = 2,
    Float64 = 3,
}

impl InterfaceType {
    fn from_u16(v: u16) -> Self {
        match v {
            0 => Self::Octet,
            1 => Self::Int32,
            2 => Self::UInt32Digital,
            3 => Self::Float64,
            _ => Self::Octet,
        }
    }
}

// ===== Baud rate menu mapping =====

/// Map a baud rate integer to the serialBAUD menu index.
fn baud_rate_to_menu_index(baud: i32) -> i32 {
    match baud {
        300 => 1,
        600 => 2,
        1200 => 3,
        2400 => 4,
        4800 => 5,
        9600 => 6,
        19200 => 7,
        38400 => 8,
        57600 => 9,
        115200 => 10,
        230400 => 11,
        460800 => 12,
        576000 => 13,
        921600 => 14,
        1152000 => 15,
        _ => 0, // Unknown
    }
}

/// Map a serialBAUD menu index to a baud rate integer.
fn menu_index_to_baud_rate(idx: i32) -> i32 {
    match idx {
        1 => 300,
        2 => 600,
        3 => 1200,
        4 => 2400,
        5 => 4800,
        6 => 9600,
        7 => 19200,
        8 => 38400,
        9 => 57600,
        10 => 115200,
        11 => 230400,
        12 => 460800,
        13 => 576000,
        14 => 921600,
        15 => 1152000,
        _ => 0,
    }
}

// ===== AsynRecord =====

/// Full asynRecord with all 67 fields.
pub struct AsynRecord {
    // --- Address fields ---
    pub port: String,
    pub addr: i32,
    pub pcnct: i32, // Port Connect/Disconnect (menu: 0=Disconnect, 1=Connect)
    pub drvinfo: String,
    pub reason: i32,

    // --- I/O control ---
    pub tmod: i32,     // Transfer mode (menu asynTMOD)
    pub tmot: f64,     // Timeout (sec)
    pub iface: i32,    // Interface (menu asynINTERFACE)
    pub octetiv: i32,  // asynOctet is valid
    pub optioniv: i32, // asynOption is valid
    pub gpibiv: i32,   // asynGPIB is valid
    pub i32iv: i32,    // asynInt32 is valid
    pub ui32iv: i32,   // asynUInt32Digital is valid
    pub f64iv: i32,    // asynFloat64 is valid

    // --- asynOctet output ---
    pub aout: String,
    pub oeos: String,
    pub bout: Vec<u8>,
    pub omax: i32,
    pub nowt: i32,
    pub nawt: i32,
    pub ofmt: i32, // Output format (menu asynFMT)

    // --- asynOctet input ---
    pub ainp: String,
    pub tinp: String,
    pub ieos: String,
    pub binp: Vec<u8>,
    pub imax: i32,
    pub nrrd: i32,
    pub nord: i32,
    pub ifmt: i32, // Input format (menu asynFMT)
    pub eomr: i32, // EOM reason

    // --- Int32/UInt32/Float64 data ---
    pub i32inp: i32,
    pub i32out: i32,
    pub ui32inp: u32,
    pub ui32out: u32,
    pub ui32mask: u32,
    pub f64inp: f64,
    pub f64out: f64,

    // --- Serial control ---
    pub baud: i32,
    pub lbaud: i32,
    pub prty: i32,
    pub dbit: i32,
    pub sbit: i32,
    pub mctl: i32,
    pub fctl: i32,
    pub ixon: i32,
    pub ixoff: i32,
    pub ixany: i32,

    // --- IP options ---
    pub hostinfo: String,
    pub drto: i32,

    // --- GPIB ---
    pub ucmd: i32,
    pub acmd: i32,
    pub spr: i32,

    // --- Trace control ---
    pub tmsk: i32,
    pub tb0: i32,
    pub tb1: i32,
    pub tb2: i32,
    pub tb3: i32,
    pub tb4: i32,
    pub tb5: i32,
    pub tiom: i32,
    pub tib0: i32,
    pub tib1: i32,
    pub tib2: i32,
    pub tinm: i32,
    pub tinb0: i32,
    pub tinb1: i32,
    pub tinb2: i32,
    pub tinb3: i32,
    pub tsiz: i32,
    pub tfil: String,

    // --- Connection management ---
    pub auct: i32, // Autoconnect (menu: 0=noAutoConnect, 1=autoConnect)
    pub cnct: i32, // Connect/Disconnect (menu: 0=Disconnect, 1=Connect)
    pub enbl: i32, // Enable/Disable (menu: 0=Disable, 1=Enable)

    // --- Misc ---
    pub val: i32,
    pub errs: String,
    pub aqr: i32,

    // --- Runtime state (not EPICS fields) ---
    port_entry: Option<PortEntry>,
    resolved_reason: usize,
}

impl Default for AsynRecord {
    fn default() -> Self {
        Self {
            port: String::new(),
            addr: 0,
            pcnct: 0,
            drvinfo: String::new(),
            reason: 0,
            tmod: 0,
            tmot: 1.0,
            iface: 0,
            octetiv: 0,
            optioniv: 0,
            gpibiv: 0,
            i32iv: 0,
            ui32iv: 0,
            f64iv: 0,
            aout: String::new(),
            oeos: String::new(),
            bout: Vec::new(),
            omax: 80,
            nowt: 80,
            nawt: 0,
            ofmt: 0,
            ainp: String::new(),
            tinp: String::new(),
            ieos: String::new(),
            binp: Vec::new(),
            imax: 80,
            nrrd: 0,
            nord: 0,
            ifmt: 0,
            eomr: 0,
            i32inp: 0,
            i32out: 0,
            ui32inp: 0,
            ui32out: 0,
            ui32mask: 0xFFFFFFFF,
            f64inp: 0.0,
            f64out: 0.0,
            baud: 0,
            lbaud: 0,
            prty: 0,
            dbit: 0,
            sbit: 0,
            mctl: 0,
            fctl: 0,
            ixon: 0,
            ixoff: 0,
            ixany: 0,
            hostinfo: String::new(),
            drto: 0,
            ucmd: 0,
            acmd: 0,
            spr: 0,
            tmsk: 0,
            tb0: 0,
            tb1: 0,
            tb2: 0,
            tb3: 0,
            tb4: 0,
            tb5: 0,
            tiom: 0,
            tib0: 0,
            tib1: 0,
            tib2: 0,
            tinm: 0,
            tinb0: 0,
            tinb1: 0,
            tinb2: 0,
            tinb3: 0,
            tsiz: 80,
            tfil: String::new(),
            auct: 1,
            cnct: 0,
            enbl: 1,
            val: 0,
            errs: String::new(),
            aqr: 0,
            port_entry: None,
            resolved_reason: 0,
        }
    }
}

// ===== Field descriptor table =====

static FIELD_LIST: &[FieldDesc] = &[
    // Address
    FieldDesc {
        name: "PORT",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "ADDR",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "PCNCT",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "DRVINFO",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "REASON",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    // I/O control
    FieldDesc {
        name: "TMOD",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "TMOT",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    FieldDesc {
        name: "IFACE",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "OCTETIV",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "OPTIONIV",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "GPIBIV",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "I32IV",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "UI32IV",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "F64IV",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    // Octet output
    FieldDesc {
        name: "AOUT",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "OEOS",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "BOUT",
        dbf_type: DbFieldType::Char,
        read_only: false,
    },
    FieldDesc {
        name: "OMAX",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "NOWT",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "NAWT",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "OFMT",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    // Octet input
    FieldDesc {
        name: "AINP",
        dbf_type: DbFieldType::String,
        read_only: true,
    },
    FieldDesc {
        name: "TINP",
        dbf_type: DbFieldType::String,
        read_only: true,
    },
    FieldDesc {
        name: "IEOS",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "BINP",
        dbf_type: DbFieldType::Char,
        read_only: true,
    },
    FieldDesc {
        name: "IMAX",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "NRRD",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "NORD",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "IFMT",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "EOMR",
        dbf_type: DbFieldType::Short,
        read_only: true,
    },
    // Int32/UInt32/Float64
    FieldDesc {
        name: "I32INP",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "I32OUT",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "UI32INP",
        dbf_type: DbFieldType::Long,
        read_only: true,
    },
    FieldDesc {
        name: "UI32OUT",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "UI32MASK",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "F64INP",
        dbf_type: DbFieldType::Double,
        read_only: true,
    },
    FieldDesc {
        name: "F64OUT",
        dbf_type: DbFieldType::Double,
        read_only: false,
    },
    // Serial
    FieldDesc {
        name: "BAUD",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "LBAUD",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "PRTY",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "DBIT",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "SBIT",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "MCTL",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "FCTL",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "IXON",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "IXOFF",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "IXANY",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    // IP options
    FieldDesc {
        name: "HOSTINFO",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    FieldDesc {
        name: "DRTO",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    // GPIB
    FieldDesc {
        name: "UCMD",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "ACMD",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "SPR",
        dbf_type: DbFieldType::Char,
        read_only: true,
    },
    // Trace
    FieldDesc {
        name: "TMSK",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "TB0",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "TB1",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "TB2",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "TB3",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "TB4",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "TB5",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "TIOM",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "TIB0",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "TIB1",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "TIB2",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "TINM",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "TINB0",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "TINB1",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "TINB2",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "TINB3",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "TSIZ",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "TFIL",
        dbf_type: DbFieldType::String,
        read_only: false,
    },
    // Connection management
    FieldDesc {
        name: "AUCT",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "CNCT",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    FieldDesc {
        name: "ENBL",
        dbf_type: DbFieldType::Short,
        read_only: false,
    },
    // Misc
    FieldDesc {
        name: "VAL",
        dbf_type: DbFieldType::Long,
        read_only: false,
    },
    FieldDesc {
        name: "ERRS",
        dbf_type: DbFieldType::String,
        read_only: true,
    },
    FieldDesc {
        name: "AQR",
        dbf_type: DbFieldType::Char,
        read_only: false,
    },
];

// ===== Trace bit helpers =====

impl AsynRecord {
    /// Rebuild TB0-TB5 from the trace mask value.
    fn update_trace_bits_from_mask(&mut self) {
        let mask = self.tmsk as u32;
        self.tb0 = if mask & TraceMask::ERROR.bits() != 0 {
            1
        } else {
            0
        };
        self.tb1 = if mask & TraceMask::IO_DEVICE.bits() != 0 {
            1
        } else {
            0
        };
        self.tb2 = if mask & TraceMask::IO_FILTER.bits() != 0 {
            1
        } else {
            0
        };
        self.tb3 = if mask & TraceMask::IO_DRIVER.bits() != 0 {
            1
        } else {
            0
        };
        self.tb4 = if mask & TraceMask::FLOW.bits() != 0 {
            1
        } else {
            0
        };
        self.tb5 = if mask & TraceMask::WARNING.bits() != 0 {
            1
        } else {
            0
        };
    }

    /// Rebuild TMSK from TB0-TB5 bit fields.
    fn update_mask_from_trace_bits(&mut self) {
        let mut mask: u32 = 0;
        if self.tb0 != 0 {
            mask |= TraceMask::ERROR.bits();
        }
        if self.tb1 != 0 {
            mask |= TraceMask::IO_DEVICE.bits();
        }
        if self.tb2 != 0 {
            mask |= TraceMask::IO_FILTER.bits();
        }
        if self.tb3 != 0 {
            mask |= TraceMask::IO_DRIVER.bits();
        }
        if self.tb4 != 0 {
            mask |= TraceMask::FLOW.bits();
        }
        if self.tb5 != 0 {
            mask |= TraceMask::WARNING.bits();
        }
        self.tmsk = mask as i32;
    }

    /// Rebuild TIB0-TIB2 from trace I/O mask value.
    fn update_io_bits_from_mask(&mut self) {
        let mask = self.tiom as u32;
        self.tib0 = if mask & TraceIoMask::ASCII.bits() != 0 {
            1
        } else {
            0
        };
        self.tib1 = if mask & TraceIoMask::ESCAPE.bits() != 0 {
            1
        } else {
            0
        };
        self.tib2 = if mask & TraceIoMask::HEX.bits() != 0 {
            1
        } else {
            0
        };
    }

    /// Rebuild TIOM from TIB0-TIB2.
    fn update_mask_from_io_bits(&mut self) {
        let mut mask: u32 = 0;
        if self.tib0 != 0 {
            mask |= TraceIoMask::ASCII.bits();
        }
        if self.tib1 != 0 {
            mask |= TraceIoMask::ESCAPE.bits();
        }
        if self.tib2 != 0 {
            mask |= TraceIoMask::HEX.bits();
        }
        self.tiom = mask as i32;
    }

    /// Rebuild TINB0-TINB3 from trace info mask value.
    fn update_info_bits_from_mask(&mut self) {
        let mask = self.tinm as u32;
        self.tinb0 = if mask & TraceInfoMask::TIME.bits() != 0 {
            1
        } else {
            0
        };
        self.tinb1 = if mask & TraceInfoMask::PORT.bits() != 0 {
            1
        } else {
            0
        };
        self.tinb2 = if mask & TraceInfoMask::SOURCE.bits() != 0 {
            1
        } else {
            0
        };
        self.tinb3 = if mask & TraceInfoMask::THREAD.bits() != 0 {
            1
        } else {
            0
        };
    }

    /// Rebuild TINM from TINB0-TINB3.
    fn update_mask_from_info_bits(&mut self) {
        let mut mask: u32 = 0;
        if self.tinb0 != 0 {
            mask |= TraceInfoMask::TIME.bits();
        }
        if self.tinb1 != 0 {
            mask |= TraceInfoMask::PORT.bits();
        }
        if self.tinb2 != 0 {
            mask |= TraceInfoMask::SOURCE.bits();
        }
        if self.tinb3 != 0 {
            mask |= TraceInfoMask::THREAD.bits();
        }
        self.tinm = mask as i32;
    }

    /// Apply current trace mask fields to the TraceManager.
    fn apply_trace_mask(&self) {
        if let Some(ref entry) = self.port_entry {
            let mask = TraceMask::from_bits_truncate(self.tmsk as u32);
            entry.trace.set_trace_mask(Some(&self.port), mask);
        }
    }

    /// Apply current trace I/O mask to the TraceManager.
    fn apply_trace_io_mask(&self) {
        if let Some(ref entry) = self.port_entry {
            let mask = TraceIoMask::from_bits_truncate(self.tiom as u32);
            entry.trace.set_trace_io_mask(Some(&self.port), mask);
        }
    }

    /// Apply current trace info mask to the TraceManager.
    fn apply_trace_info_mask(&self) {
        if let Some(ref entry) = self.port_entry {
            let mask = TraceInfoMask::from_bits_truncate(self.tinm as u32);
            entry.trace.set_trace_info_mask(Some(&self.port), mask);
        }
    }

    /// Apply truncate size to TraceManager.
    fn apply_trace_truncate_size(&self) {
        if let Some(ref entry) = self.port_entry {
            entry
                .trace
                .set_io_truncate_size(Some(&self.port), self.tsiz as usize);
        }
    }

    /// Apply trace file to TraceManager.
    fn apply_trace_file(&self) {
        if let Some(ref entry) = self.port_entry {
            let file = match self.tfil.as_str() {
                "" | "stderr" => TraceFile::Stderr,
                "stdout" => TraceFile::Stdout,
                path => match std::fs::File::create(path) {
                    Ok(f) => TraceFile::File(Arc::new(std::sync::Mutex::new(f))),
                    Err(_) => {
                        eprintln!("asynRecord: cannot open trace file '{path}', using stderr");
                        TraceFile::Stderr
                    }
                },
            };
            entry.trace.set_trace_file(Some(&self.port), file);
        }
    }

    /// Read current trace state from TraceManager into record fields.
    fn read_trace_state(&mut self) {
        let (trace_mask, io_mask) = match self.port_entry {
            Some(ref entry) => {
                let port = &self.port;
                (
                    entry.trace.get_trace_mask(Some(port)).bits(),
                    entry.trace.get_trace_io_mask(Some(port)).bits(),
                )
            }
            None => return,
        };

        self.tmsk = trace_mask as i32;
        self.update_trace_bits_from_mask();

        self.tiom = io_mask as i32;
        self.update_io_bits_from_mask();
    }

    /// Read serial/IP options from the driver into record fields.
    fn read_options_from_driver(&mut self, handle: &PortHandle) {
        // Baud rate
        if let Ok(val) = handle.get_option_blocking("baud") {
            self.lbaud = val.parse::<i32>().unwrap_or(0);
            self.baud = baud_rate_to_menu_index(self.lbaud);
        }
        // Parity
        if let Ok(val) = handle.get_option_blocking("parity") {
            self.prty = match val.as_str() {
                "none" => 1,
                "even" => 2,
                "odd" => 3,
                _ => 0, // unknown
            };
        }
        // Data bits
        if let Ok(val) = handle.get_option_blocking("csize") {
            self.dbit = match val.as_str() {
                "5" => 1,
                "6" => 2,
                "7" => 3,
                "8" => 4,
                _ => 0,
            };
        }
        // Stop bits
        if let Ok(val) = handle.get_option_blocking("stop") {
            self.sbit = match val.as_str() {
                "1" => 1,
                "2" => 2,
                _ => 0,
            };
        }
        // Flow control
        if let Ok(val) = handle.get_option_blocking("crtscts") {
            self.fctl = match val.as_str() {
                "Y" | "Yes" => 2,         // Hardware
                "N" | "No" | "none" => 1, // None
                _ => 0,
            };
        }
        // Modem control
        if let Ok(val) = handle.get_option_blocking("clocal") {
            self.mctl = match val.as_str() {
                "Y" | "Yes" => 1, // CLOCAL
                "N" | "No" => 2,  // YES (hardware modem control)
                _ => 0,
            };
        }
        // XON/XOFF
        if let Ok(val) = handle.get_option_blocking("ixon") {
            self.ixon = match val.as_str() {
                "Y" | "Yes" => 2,
                "N" | "No" => 1,
                _ => 0,
            };
        }
        if let Ok(val) = handle.get_option_blocking("ixoff") {
            self.ixoff = match val.as_str() {
                "Y" | "Yes" => 2,
                "N" | "No" => 1,
                _ => 0,
            };
        }
        if let Ok(val) = handle.get_option_blocking("ixany") {
            self.ixany = match val.as_str() {
                "Y" | "Yes" => 2,
                "N" | "No" => 1,
                _ => 0,
            };
        }
        // IP options
        if let Ok(val) = handle.get_option_blocking("hostinfo") {
            self.hostinfo = val;
        }
        if let Ok(val) = handle.get_option_blocking("disconnectOnReadTimeout") {
            self.drto = match val.as_str() {
                "Y" | "Yes" => 2,
                "N" | "No" => 1,
                _ => 0,
            };
        }
    }

    /// Write a serial/IP option to the driver via SetOption.
    fn write_option(&mut self, key: &str, value: &str) {
        if let Some(ref entry) = self.port_entry {
            if let Err(e) = entry.handle.set_option_blocking(key, value) {
                self.errs = format!("set_option({key}): {e}");
            }
        }
    }

    /// Attempt to connect to the port specified in the PORT field.
    fn connect_device(&mut self) {
        if self.port.is_empty() {
            self.pcnct = 0;
            self.cnct = 0;
            self.port_entry = None;
            return;
        }

        match registry::get_port(&self.port) {
            Some(entry) => {
                // Resolve drvinfo → reason if specified
                if !self.drvinfo.is_empty() {
                    match entry.handle.drv_user_create_blocking(&self.drvinfo) {
                        Ok(r) => {
                            self.resolved_reason = r;
                            self.reason = r as i32;
                        }
                        Err(e) => {
                            self.errs = format!("drvUserCreate failed: {e}");
                            self.resolved_reason = 0;
                        }
                    }
                } else {
                    self.resolved_reason = self.reason as usize;
                }

                // All standard interfaces valid for our ports
                self.octetiv = 1;
                self.i32iv = 1;
                self.ui32iv = 1;
                self.f64iv = 1;
                self.optioniv = 1;
                self.gpibiv = 0; // No GPIB hardware in Rust ports

                // Read trace state from manager
                self.port_entry = Some(entry.clone());
                self.read_trace_state();

                // Read serial/IP options from driver
                self.read_options_from_driver(&entry.handle);

                // Mark connected
                self.pcnct = 1;
                self.cnct = 1;
                self.enbl = 1;
                self.auct = 1;
                self.errs.clear();
            }
            None => {
                self.errs = format!("port '{}' not found", self.port);
                self.pcnct = 0;
                self.cnct = 0;
                self.port_entry = None;
            }
        }
    }

    /// Perform I/O based on TMOD and IFACE.
    fn perform_io(&mut self) -> CaResult<()> {
        let entry = match &self.port_entry {
            Some(e) => e.clone(),
            None => {
                self.errs = "not connected".to_string();
                return Ok(());
            }
        };

        let tmod = TransferMode::from_u16(self.tmod as u16);
        let iface = InterfaceType::from_u16(self.iface as u16);

        // Write phase
        if matches!(tmod, TransferMode::Write | TransferMode::WriteRead) {
            match iface {
                InterfaceType::Octet => {
                    let data = self.aout.as_bytes().to_vec();
                    match entry.handle.submit_blocking(
                        crate::request::RequestOp::OctetWrite { data: data.clone() },
                        crate::user::AsynUser::new(self.resolved_reason).with_addr(self.addr),
                    ) {
                        Ok(_) => {
                            self.nawt = data.len() as i32;
                        }
                        Err(e) => {
                            self.errs = format!("write: {e}");
                        }
                    }
                }
                InterfaceType::Int32 => {
                    match entry.handle.write_int32_blocking(
                        self.resolved_reason,
                        self.addr,
                        self.i32out,
                    ) {
                        Ok(_) => {}
                        Err(e) => {
                            self.errs = format!("write: {e}");
                        }
                    }
                }
                InterfaceType::UInt32Digital => {
                    match entry.handle.submit_blocking(
                        crate::request::RequestOp::UInt32DigitalWrite {
                            value: self.ui32out,
                            mask: self.ui32mask,
                        },
                        crate::user::AsynUser::new(self.resolved_reason).with_addr(self.addr),
                    ) {
                        Ok(_) => {}
                        Err(e) => {
                            self.errs = format!("write: {e}");
                        }
                    }
                }
                InterfaceType::Float64 => {
                    match entry.handle.write_float64_blocking(
                        self.resolved_reason,
                        self.addr,
                        self.f64out,
                    ) {
                        Ok(_) => {}
                        Err(e) => {
                            self.errs = format!("write: {e}");
                        }
                    }
                }
            }
        }

        // Read phase
        if matches!(tmod, TransferMode::Read | TransferMode::WriteRead) {
            match iface {
                InterfaceType::Octet => {
                    let buf_size = if self.nrrd > 0 {
                        self.nrrd as usize
                    } else {
                        self.imax as usize
                    };
                    match entry.handle.submit_blocking(
                        crate::request::RequestOp::OctetRead { buf_size },
                        crate::user::AsynUser::new(self.resolved_reason).with_addr(self.addr),
                    ) {
                        Ok(result) => {
                            if let Some(data) = result.data {
                                self.nord = data.len() as i32;
                                self.ainp = String::from_utf8_lossy(&data).to_string();
                                self.tinp =
                                    crate::trace::format_io_data(&data, TraceIoMask::ESCAPE);
                                self.binp = data;
                            }
                        }
                        Err(e) => {
                            self.errs = format!("read: {e}");
                        }
                    }
                }
                InterfaceType::Int32 => {
                    match entry
                        .handle
                        .read_int32_blocking(self.resolved_reason, self.addr)
                    {
                        Ok(v) => {
                            self.i32inp = v;
                        }
                        Err(e) => {
                            self.errs = format!("read: {e}");
                        }
                    }
                }
                InterfaceType::UInt32Digital => {
                    match entry.handle.submit_blocking(
                        crate::request::RequestOp::UInt32DigitalRead {
                            mask: self.ui32mask,
                        },
                        crate::user::AsynUser::new(self.resolved_reason).with_addr(self.addr),
                    ) {
                        Ok(result) => {
                            if let Some(v) = result.uint_val {
                                self.ui32inp = v;
                            }
                        }
                        Err(e) => {
                            self.errs = format!("read: {e}");
                        }
                    }
                }
                InterfaceType::Float64 => {
                    match entry
                        .handle
                        .read_float64_blocking(self.resolved_reason, self.addr)
                    {
                        Ok(v) => {
                            self.f64inp = v;
                        }
                        Err(e) => {
                            self.errs = format!("read: {e}");
                        }
                    }
                }
            }
        }

        // Flush
        if matches!(tmod, TransferMode::Flush) {
            match entry.handle.submit_blocking(
                crate::request::RequestOp::Flush,
                crate::user::AsynUser::new(self.resolved_reason).with_addr(self.addr),
            ) {
                Ok(_) => {}
                Err(e) => {
                    self.errs = format!("flush: {e}");
                }
            }
        }

        Ok(())
    }
}

// ===== Record trait implementation =====

impl Record for AsynRecord {
    fn record_type(&self) -> &'static str {
        "asyn"
    }

    fn field_list(&self) -> &'static [FieldDesc] {
        FIELD_LIST
    }

    fn get_field(&self, name: &str) -> Option<EpicsValue> {
        match name {
            "PORT" => Some(EpicsValue::String(self.port.clone())),
            "ADDR" => Some(EpicsValue::Long(self.addr)),
            "PCNCT" => Some(EpicsValue::Short(self.pcnct as i16)),
            "DRVINFO" => Some(EpicsValue::String(self.drvinfo.clone())),
            "REASON" => Some(EpicsValue::Long(self.reason)),
            "TMOD" => Some(EpicsValue::Short(self.tmod as i16)),
            "TMOT" => Some(EpicsValue::Double(self.tmot)),
            "IFACE" => Some(EpicsValue::Short(self.iface as i16)),
            "OCTETIV" => Some(EpicsValue::Long(self.octetiv)),
            "OPTIONIV" => Some(EpicsValue::Long(self.optioniv)),
            "GPIBIV" => Some(EpicsValue::Long(self.gpibiv)),
            "I32IV" => Some(EpicsValue::Long(self.i32iv)),
            "UI32IV" => Some(EpicsValue::Long(self.ui32iv)),
            "F64IV" => Some(EpicsValue::Long(self.f64iv)),
            "AOUT" => Some(EpicsValue::String(self.aout.clone())),
            "OEOS" => Some(EpicsValue::String(self.oeos.clone())),
            "BOUT" => Some(EpicsValue::CharArray(self.bout.clone())),
            "OMAX" => Some(EpicsValue::Long(self.omax)),
            "NOWT" => Some(EpicsValue::Long(self.nowt)),
            "NAWT" => Some(EpicsValue::Long(self.nawt)),
            "OFMT" => Some(EpicsValue::Short(self.ofmt as i16)),
            "AINP" => Some(EpicsValue::String(self.ainp.clone())),
            "TINP" => Some(EpicsValue::String(self.tinp.clone())),
            "IEOS" => Some(EpicsValue::String(self.ieos.clone())),
            "BINP" => Some(EpicsValue::CharArray(self.binp.clone())),
            "IMAX" => Some(EpicsValue::Long(self.imax)),
            "NRRD" => Some(EpicsValue::Long(self.nrrd)),
            "NORD" => Some(EpicsValue::Long(self.nord)),
            "IFMT" => Some(EpicsValue::Short(self.ifmt as i16)),
            "EOMR" => Some(EpicsValue::Short(self.eomr as i16)),
            "I32INP" => Some(EpicsValue::Long(self.i32inp)),
            "I32OUT" => Some(EpicsValue::Long(self.i32out)),
            "UI32INP" => Some(EpicsValue::Long(self.ui32inp as i32)),
            "UI32OUT" => Some(EpicsValue::Long(self.ui32out as i32)),
            "UI32MASK" => Some(EpicsValue::Long(self.ui32mask as i32)),
            "F64INP" => Some(EpicsValue::Double(self.f64inp)),
            "F64OUT" => Some(EpicsValue::Double(self.f64out)),
            "BAUD" => Some(EpicsValue::Short(self.baud as i16)),
            "LBAUD" => Some(EpicsValue::Long(self.lbaud)),
            "PRTY" => Some(EpicsValue::Short(self.prty as i16)),
            "DBIT" => Some(EpicsValue::Short(self.dbit as i16)),
            "SBIT" => Some(EpicsValue::Short(self.sbit as i16)),
            "MCTL" => Some(EpicsValue::Short(self.mctl as i16)),
            "FCTL" => Some(EpicsValue::Short(self.fctl as i16)),
            "IXON" => Some(EpicsValue::Short(self.ixon as i16)),
            "IXOFF" => Some(EpicsValue::Short(self.ixoff as i16)),
            "IXANY" => Some(EpicsValue::Short(self.ixany as i16)),
            "HOSTINFO" => Some(EpicsValue::String(self.hostinfo.clone())),
            "DRTO" => Some(EpicsValue::Short(self.drto as i16)),
            "UCMD" => Some(EpicsValue::Short(self.ucmd as i16)),
            "ACMD" => Some(EpicsValue::Short(self.acmd as i16)),
            "SPR" => Some(EpicsValue::Char(self.spr as u8)),
            "TMSK" => Some(EpicsValue::Long(self.tmsk)),
            "TB0" => Some(EpicsValue::Short(self.tb0 as i16)),
            "TB1" => Some(EpicsValue::Short(self.tb1 as i16)),
            "TB2" => Some(EpicsValue::Short(self.tb2 as i16)),
            "TB3" => Some(EpicsValue::Short(self.tb3 as i16)),
            "TB4" => Some(EpicsValue::Short(self.tb4 as i16)),
            "TB5" => Some(EpicsValue::Short(self.tb5 as i16)),
            "TIOM" => Some(EpicsValue::Long(self.tiom)),
            "TIB0" => Some(EpicsValue::Short(self.tib0 as i16)),
            "TIB1" => Some(EpicsValue::Short(self.tib1 as i16)),
            "TIB2" => Some(EpicsValue::Short(self.tib2 as i16)),
            "TINM" => Some(EpicsValue::Long(self.tinm)),
            "TINB0" => Some(EpicsValue::Short(self.tinb0 as i16)),
            "TINB1" => Some(EpicsValue::Short(self.tinb1 as i16)),
            "TINB2" => Some(EpicsValue::Short(self.tinb2 as i16)),
            "TINB3" => Some(EpicsValue::Short(self.tinb3 as i16)),
            "TSIZ" => Some(EpicsValue::Long(self.tsiz)),
            "TFIL" => Some(EpicsValue::String(self.tfil.clone())),
            "AUCT" => Some(EpicsValue::Short(self.auct as i16)),
            "CNCT" => Some(EpicsValue::Short(self.cnct as i16)),
            "ENBL" => Some(EpicsValue::Short(self.enbl as i16)),
            "VAL" => Some(EpicsValue::Long(self.val)),
            "ERRS" => Some(EpicsValue::String(self.errs.clone())),
            "AQR" => Some(EpicsValue::Char(self.aqr as u8)),
            _ => None,
        }
    }

    fn put_field(&mut self, name: &str, value: EpicsValue) -> CaResult<()> {
        // Helper closures for type coercion
        let to_i32 = |v: &EpicsValue| -> i32 { v.to_f64().unwrap_or(0.0) as i32 };
        let to_u32 = |v: &EpicsValue| -> u32 { v.to_f64().unwrap_or(0.0) as u32 };
        let to_f64 = |v: &EpicsValue| -> f64 { v.to_f64().unwrap_or(0.0) };
        let to_str = |v: &EpicsValue| -> String { format!("{v}") };
        let to_bytes = |v: &EpicsValue| -> Vec<u8> {
            match v {
                EpicsValue::CharArray(b) => b.clone(),
                EpicsValue::String(s) => s.as_bytes().to_vec(),
                _ => Vec::new(),
            }
        };

        match name {
            "PORT" => {
                self.port = to_str(&value);
            }
            "ADDR" => {
                self.addr = to_i32(&value);
            }
            "PCNCT" => {
                self.pcnct = to_i32(&value);
            }
            "DRVINFO" => {
                self.drvinfo = to_str(&value);
            }
            "REASON" => {
                self.reason = to_i32(&value);
            }
            "TMOD" => {
                self.tmod = to_i32(&value);
            }
            "TMOT" => {
                self.tmot = to_f64(&value);
            }
            "IFACE" => {
                self.iface = to_i32(&value);
            }
            "OCTETIV" => {
                self.octetiv = to_i32(&value);
            }
            "OPTIONIV" => {
                self.optioniv = to_i32(&value);
            }
            "GPIBIV" => {
                self.gpibiv = to_i32(&value);
            }
            "I32IV" => {
                self.i32iv = to_i32(&value);
            }
            "UI32IV" => {
                self.ui32iv = to_i32(&value);
            }
            "F64IV" => {
                self.f64iv = to_i32(&value);
            }
            "AOUT" => {
                self.aout = to_str(&value);
            }
            "OEOS" => {
                self.oeos = to_str(&value);
            }
            "BOUT" => {
                self.bout = to_bytes(&value);
            }
            "OMAX" => {
                self.omax = to_i32(&value);
            }
            "NOWT" => {
                self.nowt = to_i32(&value);
            }
            "NAWT" => {
                self.nawt = to_i32(&value);
            }
            "OFMT" => {
                self.ofmt = to_i32(&value);
            }
            "AINP" => {
                self.ainp = to_str(&value);
            }
            "TINP" => {
                self.tinp = to_str(&value);
            }
            "IEOS" => {
                self.ieos = to_str(&value);
            }
            "BINP" => {
                self.binp = to_bytes(&value);
            }
            "IMAX" => {
                self.imax = to_i32(&value);
            }
            "NRRD" => {
                self.nrrd = to_i32(&value);
            }
            "NORD" => {
                self.nord = to_i32(&value);
            }
            "IFMT" => {
                self.ifmt = to_i32(&value);
            }
            "EOMR" => {
                self.eomr = to_i32(&value);
            }
            "I32INP" => {
                self.i32inp = to_i32(&value);
            }
            "I32OUT" => {
                self.i32out = to_i32(&value);
            }
            "UI32INP" => {
                self.ui32inp = to_u32(&value);
            }
            "UI32OUT" => {
                self.ui32out = to_u32(&value);
            }
            "UI32MASK" => {
                self.ui32mask = to_u32(&value);
            }
            "F64INP" => {
                self.f64inp = to_f64(&value);
            }
            "F64OUT" => {
                self.f64out = to_f64(&value);
            }
            "BAUD" => {
                self.baud = to_i32(&value);
            }
            "LBAUD" => {
                self.lbaud = to_i32(&value);
            }
            "PRTY" => {
                self.prty = to_i32(&value);
            }
            "DBIT" => {
                self.dbit = to_i32(&value);
            }
            "SBIT" => {
                self.sbit = to_i32(&value);
            }
            "MCTL" => {
                self.mctl = to_i32(&value);
            }
            "FCTL" => {
                self.fctl = to_i32(&value);
            }
            "IXON" => {
                self.ixon = to_i32(&value);
            }
            "IXOFF" => {
                self.ixoff = to_i32(&value);
            }
            "IXANY" => {
                self.ixany = to_i32(&value);
            }
            "HOSTINFO" => {
                self.hostinfo = to_str(&value);
            }
            "DRTO" => {
                self.drto = to_i32(&value);
            }
            "UCMD" => {
                self.ucmd = to_i32(&value);
            }
            "ACMD" => {
                self.acmd = to_i32(&value);
            }
            "SPR" => {
                self.spr = to_i32(&value);
            }
            "TMSK" => {
                self.tmsk = to_i32(&value);
            }
            "TB0" => {
                self.tb0 = to_i32(&value);
            }
            "TB1" => {
                self.tb1 = to_i32(&value);
            }
            "TB2" => {
                self.tb2 = to_i32(&value);
            }
            "TB3" => {
                self.tb3 = to_i32(&value);
            }
            "TB4" => {
                self.tb4 = to_i32(&value);
            }
            "TB5" => {
                self.tb5 = to_i32(&value);
            }
            "TIOM" => {
                self.tiom = to_i32(&value);
            }
            "TIB0" => {
                self.tib0 = to_i32(&value);
            }
            "TIB1" => {
                self.tib1 = to_i32(&value);
            }
            "TIB2" => {
                self.tib2 = to_i32(&value);
            }
            "TINM" => {
                self.tinm = to_i32(&value);
            }
            "TINB0" => {
                self.tinb0 = to_i32(&value);
            }
            "TINB1" => {
                self.tinb1 = to_i32(&value);
            }
            "TINB2" => {
                self.tinb2 = to_i32(&value);
            }
            "TINB3" => {
                self.tinb3 = to_i32(&value);
            }
            "TSIZ" => {
                self.tsiz = to_i32(&value);
            }
            "TFIL" => {
                self.tfil = to_str(&value);
            }
            "AUCT" => {
                self.auct = to_i32(&value);
            }
            "CNCT" => {
                self.cnct = to_i32(&value);
            }
            "ENBL" => {
                self.enbl = to_i32(&value);
            }
            "VAL" => {
                self.val = to_i32(&value);
            }
            "ERRS" => {
                self.errs = to_str(&value);
            }
            "AQR" => {
                self.aqr = to_i32(&value);
            }
            _ => {
                return Err(CaError::InvalidValue(format!("unknown field: {name}")));
            }
        }
        Ok(())
    }

    fn init_record(&mut self, pass: u8) -> CaResult<()> {
        if pass == 1 && !self.port.is_empty() {
            self.connect_device();
        }
        Ok(())
    }

    fn special(&mut self, field: &str, after: bool) -> CaResult<()> {
        if !after {
            return Ok(());
        }

        match field {
            // Connection fields → reconnect
            "PORT" | "ADDR" | "DRVINFO" => {
                self.connect_device();
            }

            // Trace mask (numeric) → update bit fields and apply
            "TMSK" => {
                self.update_trace_bits_from_mask();
                self.apply_trace_mask();
            }

            // Trace bit fields → update mask and apply
            "TB0" | "TB1" | "TB2" | "TB3" | "TB4" | "TB5" => {
                self.update_mask_from_trace_bits();
                self.apply_trace_mask();
            }

            // Trace I/O mask (numeric) → update bits and apply
            "TIOM" => {
                self.update_io_bits_from_mask();
                self.apply_trace_io_mask();
            }

            // Trace I/O bit fields → update mask and apply
            "TIB0" | "TIB1" | "TIB2" => {
                self.update_mask_from_io_bits();
                self.apply_trace_io_mask();
            }

            // Trace info mask (numeric) → update bits and apply
            "TINM" => {
                self.update_info_bits_from_mask();
                self.apply_trace_info_mask();
            }

            // Trace info bit fields → update mask and apply
            "TINB0" | "TINB1" | "TINB2" | "TINB3" => {
                self.update_mask_from_info_bits();
                self.apply_trace_info_mask();
            }

            // Trace truncate size
            "TSIZ" => {
                self.apply_trace_truncate_size();
            }

            // Trace file
            "TFIL" => {
                self.apply_trace_file();
            }

            // Connection management
            "CNCT" => {
                if self.cnct != 0 {
                    self.connect_device();
                } else {
                    self.pcnct = 0;
                    self.port_entry = None;
                }
            }
            "PCNCT" => {
                if self.pcnct != 0 {
                    self.connect_device();
                } else {
                    self.cnct = 0;
                    self.port_entry = None;
                }
            }

            // Interface change → update validity flags
            "IFACE" => {
                // All interfaces are valid for our port drivers
            }

            // REASON change
            "REASON" => {
                self.resolved_reason = self.reason as usize;
            }

            // --- Serial options ---
            "BAUD" => {
                let rate = menu_index_to_baud_rate(self.baud);
                if rate > 0 {
                    self.lbaud = rate;
                    self.write_option("baud", &rate.to_string());
                }
            }
            "LBAUD" => {
                if self.lbaud > 0 {
                    self.baud = baud_rate_to_menu_index(self.lbaud);
                    self.write_option("baud", &self.lbaud.to_string());
                }
            }
            "PRTY" => {
                let val = match self.prty {
                    1 => "none",
                    2 => "even",
                    3 => "odd",
                    _ => return Ok(()),
                };
                self.write_option("parity", val);
            }
            "DBIT" => {
                let val = match self.dbit {
                    1 => "5",
                    2 => "6",
                    3 => "7",
                    4 => "8",
                    _ => return Ok(()),
                };
                self.write_option("csize", val);
            }
            "SBIT" => {
                let val = match self.sbit {
                    1 => "1",
                    2 => "2",
                    _ => return Ok(()),
                };
                self.write_option("stop", val);
            }
            "MCTL" => {
                let val = match self.mctl {
                    1 => "Y", // CLOCAL
                    2 => "N", // Hardware modem control
                    _ => return Ok(()),
                };
                self.write_option("clocal", val);
            }
            "FCTL" => {
                let val = match self.fctl {
                    1 => "N", // None
                    2 => "Y", // Hardware
                    _ => return Ok(()),
                };
                self.write_option("crtscts", val);
            }
            "IXON" => {
                let val = match self.ixon {
                    1 => "N",
                    2 => "Y",
                    _ => return Ok(()),
                };
                self.write_option("ixon", val);
            }
            "IXOFF" => {
                let val = match self.ixoff {
                    1 => "N",
                    2 => "Y",
                    _ => return Ok(()),
                };
                self.write_option("ixoff", val);
            }
            "IXANY" => {
                let val = match self.ixany {
                    1 => "N",
                    2 => "Y",
                    _ => return Ok(()),
                };
                self.write_option("ixany", val);
            }

            // --- IP options ---
            "HOSTINFO" => {
                if !self.hostinfo.is_empty() {
                    self.write_option("hostinfo", &self.hostinfo.clone());
                }
            }
            "DRTO" => {
                let val = match self.drto {
                    1 => "N",
                    2 => "Y",
                    _ => return Ok(()),
                };
                self.write_option("disconnectOnReadTimeout", val);
            }

            // --- GPIB commands (no GPIB hardware, log as stub) ---
            "UCMD" | "ACMD" => {
                // GPIB not supported in Rust ports
                if self.gpibiv == 0 && (self.ucmd != 0 || self.acmd != 0) {
                    self.errs = "GPIB not supported on this port".to_string();
                }
            }

            // --- AQR (Abort Queue Request) ---
            "AQR" => {
                // Not yet implemented — would need CancelToken tracking per-record
            }

            // --- EOS (end-of-string) delimiters ---
            "OEOS" => {
                self.write_option("oeos", &self.oeos.clone());
            }
            "IEOS" => {
                self.write_option("ieos", &self.ieos.clone());
            }

            // --- UI32MASK change ---
            "UI32MASK" => {
                // Just record the value, used during I/O
            }

            _ => {}
        }
        Ok(())
    }

    fn process(&mut self) -> CaResult<ProcessOutcome> {
        let tmod = TransferMode::from_u16(self.tmod as u16);
        if tmod == TransferMode::NoIo {
            return Ok(ProcessOutcome::complete());
        }

        self.errs.clear();
        self.perform_io()?;
        Ok(ProcessOutcome::complete())
    }

    fn clears_udf(&self) -> bool {
        true
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;
    use epics_base_rs::server::record::RecordProcessResult;

    #[test]
    fn test_default_fields() {
        let rec = AsynRecord::default();
        assert_eq!(rec.record_type(), "asyn");
        assert_eq!(rec.cnct, 0);
        assert_eq!(rec.tmot, 1.0);
        assert_eq!(rec.omax, 80);
        assert_eq!(rec.imax, 80);
        assert_eq!(rec.tsiz, 80);
        assert_eq!(rec.ui32mask, 0xFFFFFFFF);
        assert_eq!(rec.auct, 1);
        assert_eq!(rec.enbl, 1);
    }

    #[test]
    fn test_field_list_count() {
        let rec = AsynRecord::default();
        assert_eq!(rec.field_list().len(), 76);
    }

    #[test]
    fn test_get_put_roundtrip() {
        let mut rec = AsynRecord::default();
        rec.put_field("PORT", EpicsValue::String("SIM1".into()))
            .unwrap();
        assert_eq!(
            rec.get_field("PORT"),
            Some(EpicsValue::String("SIM1".into()))
        );

        rec.put_field("ADDR", EpicsValue::Long(3)).unwrap();
        assert_eq!(rec.get_field("ADDR"), Some(EpicsValue::Long(3)));

        rec.put_field("TMOT", EpicsValue::Double(2.5)).unwrap();
        assert_eq!(rec.get_field("TMOT"), Some(EpicsValue::Double(2.5)));

        rec.put_field("F64OUT", EpicsValue::Double(3.14)).unwrap();
        assert_eq!(rec.get_field("F64OUT"), Some(EpicsValue::Double(3.14)));
    }

    #[test]
    fn test_trace_bit_sync() {
        let mut rec = AsynRecord::default();

        // Set TMSK → bits should update
        rec.tmsk = (TraceMask::ERROR | TraceMask::FLOW).bits() as i32;
        rec.update_trace_bits_from_mask();
        assert_eq!(rec.tb0, 1); // ERROR
        assert_eq!(rec.tb4, 1); // FLOW
        assert_eq!(rec.tb1, 0);
        assert_eq!(rec.tb2, 0);
        assert_eq!(rec.tb3, 0);
        assert_eq!(rec.tb5, 0);

        // Set bits → mask should update
        rec.tb0 = 1;
        rec.tb1 = 1;
        rec.tb2 = 0;
        rec.tb3 = 0;
        rec.tb4 = 0;
        rec.tb5 = 1;
        rec.update_mask_from_trace_bits();
        let expected = TraceMask::ERROR | TraceMask::IO_DEVICE | TraceMask::WARNING;
        assert_eq!(rec.tmsk, expected.bits() as i32);
    }

    #[test]
    fn test_io_bit_sync() {
        let mut rec = AsynRecord::default();

        rec.tiom = (TraceIoMask::ASCII | TraceIoMask::HEX).bits() as i32;
        rec.update_io_bits_from_mask();
        assert_eq!(rec.tib0, 1); // ASCII
        assert_eq!(rec.tib1, 0); // ESCAPE
        assert_eq!(rec.tib2, 1); // HEX
    }

    #[test]
    fn test_info_bit_sync() {
        let mut rec = AsynRecord::default();

        rec.tinm = (TraceInfoMask::TIME | TraceInfoMask::THREAD).bits() as i32;
        rec.update_info_bits_from_mask();
        assert_eq!(rec.tinb0, 1); // TIME
        assert_eq!(rec.tinb1, 0); // PORT
        assert_eq!(rec.tinb2, 0); // SOURCE
        assert_eq!(rec.tinb3, 1); // THREAD
    }

    #[test]
    fn test_connect_nonexistent_port() {
        let mut rec = AsynRecord::default();
        rec.port = "NONEXISTENT".to_string();
        rec.connect_device();
        assert_eq!(rec.cnct, 0);
        assert!(rec.errs.contains("not found"));
    }

    #[test]
    fn test_connect_empty_port() {
        let mut rec = AsynRecord::default();
        rec.connect_device();
        assert_eq!(rec.cnct, 0);
        assert!(rec.port_entry.is_none());
    }

    #[test]
    fn test_process_no_io_mode() {
        let mut rec = AsynRecord::default();
        rec.tmod = TransferMode::NoIo as i32;
        let result = rec.process().unwrap();
        assert_eq!(result.result, RecordProcessResult::Complete);
    }

    #[test]
    fn test_process_not_connected() {
        let mut rec = AsynRecord::default();
        rec.tmod = TransferMode::Read as i32;
        rec.process().unwrap();
        assert_eq!(rec.errs, "not connected");
    }

    #[test]
    fn test_special_trace_mask() {
        let mut rec = AsynRecord::default();
        rec.tmsk = (TraceMask::ERROR | TraceMask::WARNING | TraceMask::FLOW).bits() as i32;
        rec.special("TMSK", true).unwrap();
        assert_eq!(rec.tb0, 1); // ERROR
        assert_eq!(rec.tb4, 1); // FLOW
        assert_eq!(rec.tb5, 1); // WARNING
    }

    #[test]
    fn test_special_trace_bits() {
        let mut rec = AsynRecord::default();
        rec.tb0 = 1;
        rec.tb3 = 1;
        rec.special("TB0", true).unwrap();
        assert_eq!(
            rec.tmsk as u32,
            (TraceMask::ERROR | TraceMask::IO_DRIVER).bits()
        );
    }

    #[test]
    fn test_register_and_get_port() {
        use crate::interrupt::InterruptManager;
        use crate::port::{PortDriver, PortDriverBase, PortFlags};
        use crate::port_actor::PortActor;
        use tokio::sync::mpsc;

        struct TestDriver(PortDriverBase);
        impl TestDriver {
            fn new() -> Self {
                Self(PortDriverBase::new(
                    "test_asyn_rec",
                    1,
                    PortFlags::default(),
                ))
            }
        }
        impl PortDriver for TestDriver {
            fn base(&self) -> &PortDriverBase {
                &self.0
            }
            fn base_mut(&mut self) -> &mut PortDriverBase {
                &mut self.0
            }
        }

        let interrupts = Arc::new(InterruptManager::new(256));
        let (tx, rx) = mpsc::channel(256);
        let actor = PortActor::new(Box::new(TestDriver::new()), rx);
        std::thread::spawn(move || actor.run());
        let handle = PortHandle::new(tx, "test_asyn_rec".into(), interrupts);
        let trace = Arc::new(TraceManager::new());

        register_port("test_asyn_rec", handle, trace);

        let entry = registry::get_port("test_asyn_rec");
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().handle.port_name(), "test_asyn_rec");
    }

    #[test]
    fn test_register_asyn_record_type() {
        register_asyn_record_type();
        let rec = epics_base_rs::server::db_loader::create_record("asyn").unwrap();
        assert_eq!(rec.record_type(), "asyn");
        // Verify it's our full version with all fields
        assert!(rec.field_list().len() > 3);
    }
}
