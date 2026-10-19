//! C's `dset` for each `device()` line the vendored `.dbd` declares: the
//! entry-point count, the non-NULL slots, and whether `init` calls
//! `devExtend`, transcribed from the C initialisers (base `std/dev`, calc,
//! busy, asyn). `dbDumpDevice` prints them; the generated `DEVICES_*` tables
//! name one per row, so a `device()` line without its dset does not build.

use super::Dset;

pub const DEVAAISOFT: Dset = Dset::new(5, &[2, 4], false);
pub const DEVAAOSOFT: Dset = Dset::new(5, &[2, 4], false);
pub const DEVAISOFT: Dset = Dset::new(6, &[2, 4], false);
pub const DEVAISOFTRAW: Dset = Dset::new(6, &[2, 4], false);
pub const DEVAISOFTCALLBACK: Dset = Dset::new(6, &[2, 4], true);
pub const DEVTIMESTAMPAI: Dset = Dset::new(6, &[4], true);
pub const DEVAIGENERALTIME: Dset = Dset::new(6, &[2, 4], false);
pub const DEVAOSOFT: Dset = Dset::new(6, &[2, 4], false);
pub const DEVAOSOFTRAW: Dset = Dset::new(6, &[4], false);
pub const DEVAOSOFTCALLBACK: Dset = Dset::new(6, &[4], false);
pub const DEVBISOFT: Dset = Dset::new(5, &[2, 4], false);
pub const DEVBISOFTRAW: Dset = Dset::new(5, &[2, 4], false);
pub const DEVBISOFTCALLBACK: Dset = Dset::new(5, &[2, 4], true);
pub const DEVBIDBSTATE: Dset = Dset::new(5, &[4], true);
pub const DEVBOSOFT: Dset = Dset::new(5, &[2, 4], false);
pub const DEVBOSOFTRAW: Dset = Dset::new(5, &[2, 4], false);
pub const DEVBOSOFTCALLBACK: Dset = Dset::new(5, &[4], false);
pub const DEVBOGENERALTIME: Dset = Dset::new(5, &[2, 4], false);
pub const DEVBODBSTATE: Dset = Dset::new(5, &[4], true);
pub const DEVCALCOUTSOFT: Dset = Dset::new(5, &[4], false);
pub const DEVCALCOUTSOFTCALLBACK: Dset = Dset::new(5, &[4], false);
pub const DEVEVENTSOFT: Dset = Dset::new(5, &[2, 4], false);
pub const DEVHISTOGRAMSOFT: Dset = Dset::new(6, &[2, 4], false);
pub const DEVI64INSOFT: Dset = Dset::new(5, &[2, 4], false);
pub const DEVI64INSOFTCALLBACK: Dset = Dset::new(5, &[2, 4], true);
pub const DEVI64OUTSOFT: Dset = Dset::new(5, &[2, 4], false);
pub const DEVI64OUTSOFTCALLBACK: Dset = Dset::new(5, &[4], false);
pub const DEVLISOFT: Dset = Dset::new(5, &[2, 4], false);
pub const DEVLISOFTCALLBACK: Dset = Dset::new(5, &[2, 4], true);
pub const DEVLIGENERALTIME: Dset = Dset::new(5, &[2, 4], false);
pub const DEVLOSOFT: Dset = Dset::new(5, &[2, 4], false);
pub const DEVLOSOFTCALLBACK: Dset = Dset::new(5, &[4], false);
pub const DEVLSISOFT: Dset = Dset::new(5, &[2, 4], false);
pub const DEVLSIENVIRON: Dset = Dset::new(5, &[4], true);
pub const DEVLSOSOFT: Dset = Dset::new(5, &[4], false);
pub const DEVLSOSOFTCALLBACK: Dset = Dset::new(5, &[4], false);
pub const DEVLSOSTDIO: Dset = Dset::new(5, &[4], true);
pub const DEVMBBISOFT: Dset = Dset::new(5, &[2, 4], false);
pub const DEVMBBISOFTRAW: Dset = Dset::new(5, &[2, 4], false);
pub const DEVMBBISOFTCALLBACK: Dset = Dset::new(5, &[2, 4], true);
pub const DEVMBBIDIRECTSOFT: Dset = Dset::new(5, &[2, 4], false);
pub const DEVMBBIDIRECTSOFTRAW: Dset = Dset::new(5, &[2, 4], false);
pub const DEVMBBIDIRECTSOFTCALLBACK: Dset = Dset::new(5, &[2, 4], true);
pub const DEVMBBOSOFT: Dset = Dset::new(5, &[2, 4], false);
pub const DEVMBBOSOFTRAW: Dset = Dset::new(5, &[2, 4], false);
pub const DEVMBBOSOFTCALLBACK: Dset = Dset::new(5, &[4], false);
pub const DEVMBBODIRECTSOFT: Dset = Dset::new(5, &[4], false);
pub const DEVMBBODIRECTSOFTRAW: Dset = Dset::new(5, &[2, 4], false);
pub const DEVMBBODIRECTSOFTCALLBACK: Dset = Dset::new(5, &[4], false);
pub const DEVPRINTFSOFT: Dset = Dset::new(5, &[4], false);
pub const DEVPRINTFSOFTCALLBACK: Dset = Dset::new(5, &[4], false);
pub const DEVPRINTFSTDIO: Dset = Dset::new(5, &[4], true);
pub const DEVSISOFT: Dset = Dset::new(5, &[2, 4], false);
pub const DEVSISOFTCALLBACK: Dset = Dset::new(5, &[2, 4], true);
pub const DEVTIMESTAMPSI: Dset = Dset::new(5, &[4], true);
pub const DEVSIGENERALTIME: Dset = Dset::new(5, &[2, 4], false);
pub const DEVSIENVIRON: Dset = Dset::new(5, &[4], true);
pub const DEVSOSOFT: Dset = Dset::new(5, &[4], false);
pub const DEVSOSOFTCALLBACK: Dset = Dset::new(5, &[4], false);
pub const DEVSOSTDIO: Dset = Dset::new(5, &[4], true);
pub const DEVSASOFT: Dset = Dset::new(5, &[2, 4], false);
pub const DEVWFSOFT: Dset = Dset::new(5, &[2, 4], false);
pub const DEVSCALCOUTSOFT: Dset = Dset::new(5, &[4], false);
pub const DEVACALCOUTSOFT: Dset = Dset::new(5, &[4], false);
pub const DEVSWAITIOEVENT: Dset = Dset::new(5, &[3], false);
pub const DEVBUSYSOFT: Dset = Dset::new(5, &[2, 4], false);
pub const DEVBUSYSOFTRAW: Dset = Dset::new(5, &[2, 4], false);
pub const ASYNRECORDDEVICE: Dset = Dset::new(5, &[3], false);
