//! C's `dset` for each `device()` line the vendored `.dbd` declares: the
//! entry-point count, the non-NULL slots, and whether `init` calls
//! `devExtend`, transcribed from the C initialisers (std). `dbDumpDevice`
//! prints them; the generated `DEVICES_*` tables name one per row, so a
//! `device()` line without its dset does not build.

use epics_base_rs::server::record::Dset;

pub const DEVEPIDSOFT: Dset = Dset::new(5, &[2, 4], false);
pub const DEVEPIDSOFTCB: Dset = Dset::new(5, &[2, 4], false);
pub const DEVEPIDFAST: Dset = Dset::new(6, &[2, 4], false);
pub const DEVSITODSTRING: Dset = Dset::new(5, &[2, 4], false);
pub const DEVAITODSECONDS: Dset = Dset::new(6, &[2, 4, 5], false);
