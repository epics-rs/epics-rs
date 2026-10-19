//! softIoc's init-hook sequence (`fixtures/softioc/init_hooks.out`, from
//! `init_hooks.c`) against a build path's: each state, the IOC state its
//! observer reads, and whether `dbCaLinkInit` has run — the `dbCaLink`
//! worker up and the link set [`installer`] installs registered — and, for
//! a database handed to [`WATCH`], the PACT of `close_links.db`'s records at
//! the first two `iocShutdown` states.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::ioc_app::get_ioc_state;
use epics_base_rs::server::ioc_app::init_hooks::{InitHookState, init_hook_register};
use epics_base_rs::server::iocsh::registry::CommandDef;

static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());
static INSTALLED: AtomicBool = AtomicBool::new(false);

/// The database whose [`RECORDS`] the hook reports, as `init_hooks.c` does.
pub static WATCH: Mutex<Option<Arc<PvDatabase>>> = Mutex::new(None);

const RECORDS: [&str; 5] = ["CL:ca", "CL:json", "CL:db", "CL:soft", "CL:plain"];

const C: &str = include_str!("../fixtures/softioc/init_hooks.out");

pub fn register() {
    init_hook_register(Arc::new(|state| {
        let calink = INSTALLED.load(Ordering::SeqCst) && link_owner_up();
        LOG.lock().unwrap().push(format!(
            "HOOK {} {:?} calink={}",
            state.name(),
            get_ioc_state(),
            calink as i32
        ));
        if !matches!(
            state,
            InitHookState::AtShutdown | InitHookState::AfterCloseLinks
        ) {
            return;
        }
        let Some(db) = WATCH.lock().unwrap().clone() else {
            return;
        };
        for name in RECORDS {
            if let Some(rec) = db.get_record(name) {
                let pact = rec.read().is_processing();
                LOG.lock().unwrap().push(format!(
                    "HOOK PACT {} {name}={}",
                    state.name(),
                    pact as i32
                ));
            }
        }
    }));
}

/// The `dbCaLink` worker is on the watchdog table, where C's thread is.
fn link_owner_up() -> bool {
    let up = std::cell::Cell::new(false);
    epics_base_rs::runtime::taskwd::taskwd_show(1, &|line| {
        up.set(up.get() || line.contains("dbCaLink"))
    });
    up.get()
}

/// The link set of the test's `ca` link type.
pub async fn installer(_db: Arc<PvDatabase>) -> Vec<CommandDef> {
    INSTALLED.store(true, Ordering::SeqCst);
    Vec::new()
}

/// The lines from the first `from` state on through the next `to`.
pub fn section<'a>(lines: impl Iterator<Item = &'a str>, from: &str, to: &str) -> Vec<String> {
    let mut lines = lines.skip_while(|l| !l.starts_with(&format!("HOOK {from} ")));
    let mut out = Vec::new();
    for l in lines.by_ref() {
        out.push(l.to_string());
        if l.starts_with(&format!("HOOK {to} ")) {
            break;
        }
    }
    out
}

/// softIoc's lines from `from` through `to`.
pub fn c(from: &str, to: &str) -> Vec<String> {
    section(C.lines(), from, to)
}

/// The build path's lines from `from` through `to`.
pub fn rs(from: &str, to: &str) -> Vec<String> {
    let log = LOG.lock().unwrap();
    section(log.iter().map(String::as_str), from, to)
}

/// `iocShutdown`'s states, without [`WATCH`]'s PACT lines.
fn shutdown(lines: Vec<String>) -> Vec<String> {
    lines
        .into_iter()
        .filter(|l| !l.starts_with("HOOK PACT "))
        .collect()
}

/// softIoc's `iocShutdown` from its exit callbacks, as [`shutdown`] keeps it.
pub fn c_shutdown() -> Vec<String> {
    shutdown(c("initHookAtShutdown", "initHookAfterShutdown"))
}

/// The build path's `iocShutdown`, as [`shutdown`] keeps it.
pub fn rs_shutdown() -> Vec<String> {
    shutdown(rs("initHookAtShutdown", "initHookAfterShutdown"))
}
