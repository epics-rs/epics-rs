//! The scan-data writer — port of `saveData_writeXDR.c`, `writeXDR.c` and
//! `req_file.c`, the files sscan's Makefile builds (`saveData.c` and the
//! `xdr_*.c` it used are commented out there).

pub mod mda;
pub mod req_file;
pub mod task;
pub mod write_xdr;
