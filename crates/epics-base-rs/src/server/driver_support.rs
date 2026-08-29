//! Driver support — the port of C's `drvet` (`drvSup.h:26-32`) and of the
//! name→`drvet` registry `registryDriverSupport.c` keeps.
//!
//! C splits a driver across two structures. The `.dbd` `driver(drvXxx)`
//! declaration puts a named `drvSup` node on `pdbbase->drvList` with a null
//! `pdrvet`; the generated `<app>_registerRecordDeviceDriver.cpp` hands the
//! linked-in entry table to `registerDrivers`, which calls
//! `registryDriverSupportAdd(name, drvet)` (`registryCommon.c:65-76`); and
//! `initDrvSup` (`iocInit.c:396-413`) joins the two by looking the name back up
//! and storing the pointer, printing `iocInit: driver %s not found` for a name
//! that was declared but never registered. `dbior` then walks `drvList` and
//! calls each `pdrvet->report(level)`.
//!
//! The port has one structure, not two, and the join cannot fail. There is no
//! run-time `.dbd` load here — `dbLoadDatabase` is a build-time tool
//! (`tools/dbd-codegen`) — so nothing can declare a driver name without also
//! supplying its entry table; [`register_driver_support`] takes both at once.
//! That makes C's "declared but no entry table" state unrepresentable rather
//! than guarded against, which is why `dbior`'s
//! `No driver entry table is present for %s` branch (`dbTest.c:733-736`) has no
//! counterpart in this port: the port cannot construct the state that prints
//! it.
//!
//! C's other nullable slot, `drvet.report`, IS representable and is what
//! [`DriverSupport::report`]'s `Option` return models — see there.
//!
//! `drvet.init`, called once by `initDrvSup` during `iocInit`, has deliberately
//! no counterpart on the trait. Nothing in this crate's `iocInit`
//! (`server/ioc_app.rs`) runs a driver-init pass, so a slot here would be a
//! method no caller ever reaches; a driver that needs one-time setup does it
//! where it registers. Adding the pass and the slot together is the change that
//! closes it.

use std::sync::{Arc, Mutex, OnceLock};

/// A driver's entry table — C `drvet` (`drvSup.h:26-32`).
///
/// Registered by name with [`register_driver_support`], found by
/// [`find_driver_support`] (C `registryDriverSupportFind`) and reported by the
/// `dbior` iocsh command.
pub trait DriverSupport: Send + Sync + 'static {
    /// C `drvet.report(int lvl)`, the entry `dbior` calls
    /// (`dbTest.c:738-742`).
    ///
    /// `None` is C's NULL `report` slot: `dbior` prints
    /// `Driver: <name> No report available` and moves on. `Some(text)` is a
    /// report that ran — `dbior` prints `Driver: <name>` and then the text,
    /// so `Some("")` reproduces C's header-only output for a `report` that
    /// printed nothing at this level.
    ///
    /// The report is RETURNED rather than printed because the iocsh `>`
    /// redirect swaps one command's output stream, not the process's stdout
    /// (`iocsh.cpp:417` `epicsSetThreadStdout`, which this port models as a
    /// per-context writer). A driver that wrote to `println!` would escape
    /// `dbior > file` no matter what the command did; a driver that returns
    /// its text cannot.
    fn report(&self, level: i32) -> Option<String>;
}

/// The one table, in registration order.
///
/// C keeps its `drvet` pointers in the single `registryID`-keyed gpHash
/// (`registry.c:25`) and its ORDER in `pdbbase->drvList`, which is `.dbd`
/// declaration order and is what `dbior` walks. With no second list to hold
/// the order, registration order is it — the same order the generated
/// `registerDrivers` call would have added them in.
static DRIVER_SUPPORT: OnceLock<Mutex<Vec<(String, Arc<dyn DriverSupport>)>>> = OnceLock::new();

fn table() -> &'static Mutex<Vec<(String, Arc<dyn DriverSupport>)>> {
    DRIVER_SUPPORT.get_or_init(|| Mutex::new(Vec::new()))
}

/// Register `drvet` under `name` — C `registryDriverSupportAdd`
/// (`registryDriverSupport.c:20-24`).
///
/// Returns `false` and changes nothing when `name` is already registered,
/// which is C's `registryAdd` returning 0 for a name `gphAdd` refuses
/// (`registry.c:45-56`); `registerDrivers` skips such a name before even
/// trying (`registryCommon.c:70`), so first registration wins in both.
pub fn register_driver_support(name: &str, drvet: Arc<dyn DriverSupport>) -> bool {
    let mut table = table().lock().expect("driver support registry poisoned");
    if table.iter().any(|(n, _)| n == name) {
        return false;
    }
    table.push((name.to_string(), drvet));
    true
}

/// Look up a driver's entry table — C `registryDriverSupportFind`
/// (`registryDriverSupport.c:26-30`).
pub fn find_driver_support(name: &str) -> Option<Arc<dyn DriverSupport>> {
    let table = table().lock().expect("driver support registry poisoned");
    table
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, d)| Arc::clone(d))
}

/// Every registered driver, in registration order — what `dbior` walks in
/// place of C's `pdbbase->drvList`.
pub fn driver_supports() -> Vec<(String, Arc<dyn DriverSupport>)> {
    table()
        .lock()
        .expect("driver support registry poisoned")
        .clone()
}

/// Every registered driver as `(name, entry address)`, for the
/// `registryDriverSupportFind` / `registryDump` iocsh family.
///
/// The address is the registered entry table's own, which is what C's
/// `%p` prints: `registryAdd` stores the `drvet *` and
/// `registryIocRegister.c:53` prints that stored value.
pub(crate) fn driver_support_entries() -> Vec<(String, usize)> {
    table()
        .lock()
        .expect("driver support registry poisoned")
        .iter()
        .map(|(name, drvet)| (name.clone(), Arc::as_ptr(drvet) as *const () as usize))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Reporting(&'static str);
    impl DriverSupport for Reporting {
        fn report(&self, level: i32) -> Option<String> {
            Some(format!("{} at level {level}", self.0))
        }
    }

    struct Silent;
    impl DriverSupport for Silent {
        fn report(&self, _level: i32) -> Option<String> {
            None
        }
    }

    /// C `registryDriverSupportAdd` then `registryDriverSupportFind`: the name
    /// resolves to the table that was added, and an unregistered name does not
    /// resolve at all (the `(nil)` `registryDriverSupportFind` prints).
    #[test]
    fn a_registered_driver_is_found_and_an_unknown_one_is_not() {
        assert!(register_driver_support(
            "drvRegistryProbe",
            Arc::new(Reporting("probe"))
        ));
        let found = find_driver_support("drvRegistryProbe").expect("just registered");
        assert_eq!(found.report(2).as_deref(), Some("probe at level 2"));
        assert!(find_driver_support("drvNoSuchDriverProbe").is_none());
    }

    /// C's `registryAdd` refuses a duplicate name and `registerDrivers` skips
    /// one it already finds, so the FIRST entry table registered under a name
    /// is the one that stays.
    #[test]
    fn a_duplicate_registration_is_refused_and_the_first_entry_stands() {
        assert!(register_driver_support("drvDupProbe", Arc::new(Silent)));
        assert!(!register_driver_support(
            "drvDupProbe",
            Arc::new(Reporting("second"))
        ));
        assert_eq!(
            find_driver_support("drvDupProbe")
                .expect("registered")
                .report(0),
            None,
            "the first, report-less entry table is still the registered one"
        );
        assert_eq!(
            driver_support_entries()
                .iter()
                .filter(|(n, _)| n == "drvDupProbe")
                .count(),
            1
        );
    }

    /// `registryDump` and `registryDriverSupportFind` both print the entry
    /// table's address, so two drivers must not share one.
    #[test]
    fn each_driver_entry_has_its_own_address() {
        register_driver_support("drvAddrProbeA", Arc::new(Silent));
        register_driver_support("drvAddrProbeB", Arc::new(Silent));
        let entries = driver_support_entries();
        let a = entries
            .iter()
            .find(|(n, _)| n == "drvAddrProbeA")
            .unwrap()
            .1;
        let b = entries
            .iter()
            .find(|(n, _)| n == "drvAddrProbeB")
            .unwrap()
            .1;
        assert_ne!(a, 0);
        assert_ne!(a, b);
    }
}
