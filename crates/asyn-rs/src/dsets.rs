//! C's `dset` for each `device()` line the vendored `.dbd` declares: the
//! entry-point count, the non-NULL slots, and whether `init` calls
//! `devExtend`, transcribed from the C initialisers (asyn `devEpics`). `dbDumpDevice`
//! prints them; the generated `DEVICES_*` tables name one per row, so a
//! `device()` line without its dset does not build.

use epics_base_rs::server::record::Dset;

pub const ASYNSIOCTETCMDRESPONSE: Dset = Dset::new(5, &[2, 4], false);
pub const ASYNSIOCTETWRITEREAD: Dset = Dset::new(5, &[2, 4], false);
pub const ASYNSIOCTETREAD: Dset = Dset::new(5, &[2, 3, 4], false);
pub const ASYNSOOCTETWRITE: Dset = Dset::new(5, &[2, 4], false);
pub const ASYNWFOCTETCMDRESPONSE: Dset = Dset::new(5, &[2, 4], false);
pub const ASYNWFOCTETWRITEREAD: Dset = Dset::new(5, &[2, 4], false);
pub const ASYNWFOCTETREAD: Dset = Dset::new(5, &[2, 3, 4], false);
pub const ASYNWFOCTETWRITE: Dset = Dset::new(5, &[2, 4], false);
pub const ASYNWFOCTETWRITEBINARY: Dset = Dset::new(5, &[2, 4], false);
pub const ASYNAIINT32: Dset = Dset::new(6, &[2, 3, 4, 5], false);
pub const ASYNAIINT32AVERAGE: Dset = Dset::new(6, &[2, 3, 4, 5], false);
pub const ASYNAOINT32: Dset = Dset::new(6, &[2, 3, 4, 5], false);
pub const ASYNBIINT32: Dset = Dset::new(5, &[2, 3, 4], false);
pub const ASYNBOINT32: Dset = Dset::new(5, &[2, 3, 4], false);
pub const ASYNMBBIINT32: Dset = Dset::new(5, &[2, 3, 4], false);
pub const ASYNMBBOINT32: Dset = Dset::new(5, &[2, 3, 4], false);
pub const ASYNLIINT32: Dset = Dset::new(5, &[2, 3, 4], false);
pub const ASYNLOINT32: Dset = Dset::new(5, &[2, 3, 4], false);
pub const ASYNINT8ARRAYWFIN: Dset = Dset::new(6, &[2, 3, 4], false);
pub const ASYNINT8ARRAYWFOUT: Dset = Dset::new(6, &[2, 3, 4], false);
pub const ASYNINT8ARRAYAAI: Dset = Dset::new(6, &[2, 3, 4], false);
pub const ASYNINT8ARRAYAAO: Dset = Dset::new(6, &[2, 3, 4], false);
pub const ASYNINT16ARRAYWFIN: Dset = Dset::new(6, &[2, 3, 4], false);
pub const ASYNINT16ARRAYWFOUT: Dset = Dset::new(6, &[2, 3, 4], false);
pub const ASYNINT16ARRAYAAI: Dset = Dset::new(6, &[2, 3, 4], false);
pub const ASYNINT16ARRAYAAO: Dset = Dset::new(6, &[2, 3, 4], false);
pub const ASYNINT32ARRAYWFIN: Dset = Dset::new(6, &[2, 3, 4], false);
pub const ASYNINT32ARRAYWFOUT: Dset = Dset::new(6, &[2, 3, 4], false);
pub const ASYNINT32ARRAYAAI: Dset = Dset::new(6, &[2, 3, 4], false);
pub const ASYNINT32ARRAYAAO: Dset = Dset::new(6, &[2, 3, 4], false);
pub const ASYNFLOAT32ARRAYWFIN: Dset = Dset::new(6, &[2, 3, 4], false);
pub const ASYNFLOAT32ARRAYWFOUT: Dset = Dset::new(6, &[2, 3, 4], false);
pub const ASYNFLOAT32ARRAYAAI: Dset = Dset::new(6, &[2, 3, 4], false);
pub const ASYNFLOAT32ARRAYAAO: Dset = Dset::new(6, &[2, 3, 4], false);
pub const ASYNFLOAT64ARRAYWFIN: Dset = Dset::new(6, &[2, 3, 4], false);
pub const ASYNFLOAT64ARRAYWFOUT: Dset = Dset::new(6, &[2, 3, 4], false);
pub const ASYNFLOAT64ARRAYAAI: Dset = Dset::new(6, &[2, 3, 4], false);
pub const ASYNFLOAT64ARRAYAAO: Dset = Dset::new(6, &[2, 3, 4], false);
pub const ASYNINT32TIMESERIES: Dset = Dset::new(6, &[2, 4], false);
pub const ASYNBIUINT32DIGITAL: Dset = Dset::new(6, &[2, 3, 4, 5], false);
pub const ASYNBOUINT32DIGITAL: Dset = Dset::new(6, &[2, 3, 4], false);
pub const ASYNLIUINT32DIGITAL: Dset = Dset::new(5, &[2, 3, 4], false);
pub const ASYNLOUINT32DIGITAL: Dset = Dset::new(5, &[2, 3, 4], false);
pub const ASYNMBBIUINT32DIGITAL: Dset = Dset::new(5, &[2, 3, 4], false);
pub const ASYNMBBOUINT32DIGITAL: Dset = Dset::new(5, &[2, 3, 4], false);
pub const ASYNMBBIDIRECTUINT32DIGITAL: Dset = Dset::new(5, &[2, 3, 4], false);
pub const ASYNMBBODIRECTUINT32DIGITAL: Dset = Dset::new(5, &[2, 3, 4], false);
pub const ASYNAIFLOAT64: Dset = Dset::new(6, &[2, 3, 4], false);
pub const ASYNAIFLOAT64AVERAGE: Dset = Dset::new(6, &[2, 3, 4], false);
pub const ASYNAOFLOAT64: Dset = Dset::new(6, &[2, 3, 4], false);
pub const ASYNFLOAT64TIMESERIES: Dset = Dset::new(6, &[2, 4], false);
pub const ASYNLSIOCTETCMDRESPONSE: Dset = Dset::new(5, &[2, 4], false);
pub const ASYNLSIOCTETWRITEREAD: Dset = Dset::new(5, &[2, 4], false);
pub const ASYNLSIOCTETREAD: Dset = Dset::new(5, &[2, 3, 4], false);
pub const ASYNLSOOCTETWRITE: Dset = Dset::new(5, &[2, 4], false);
pub const ASYNPFOCTETWRITE: Dset = Dset::new(5, &[2, 4], false);
pub const ASYNINT64IN: Dset = Dset::new(5, &[2, 3, 4], false);
pub const ASYNINT64OUT: Dset = Dset::new(5, &[2, 3, 4], false);
pub const ASYNINT64ARRAYWFIN: Dset = Dset::new(6, &[2, 3, 4], false);
pub const ASYNINT64ARRAYWFOUT: Dset = Dset::new(6, &[2, 3, 4], false);
pub const ASYNINT64ARRAYAAI: Dset = Dset::new(6, &[2, 3, 4], false);
pub const ASYNINT64ARRAYAAO: Dset = Dset::new(6, &[2, 3, 4], false);
pub const ASYNINT64TIMESERIES: Dset = Dset::new(6, &[2, 4], false);
pub const ASYNLIINT64: Dset = Dset::new(5, &[2, 3, 4], false);
pub const ASYNLOINT64: Dset = Dset::new(5, &[2, 3, 4], false);
pub const ASYNAIINT64: Dset = Dset::new(6, &[2, 3, 4, 5], false);
pub const ASYNAOINT64: Dset = Dset::new(6, &[2, 3, 4, 5], false);
/// busy's `devBusyAsyn.c`, declared by busySupport_withASYN.dbd, not asyn's.
pub const ASYNBUSYINT32: Dset = Dset::new(5, &[2, 4], false);
