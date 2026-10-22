//! SNL program variables as seq 2.2.9 keeps them for optics, which builds
//! without safe mode (`+r`, no `+s`): a `monitor`ed variable is written as
//! each monitor arrives and raises the flag `sync`ed to it (seq_ca.c:303
//! `ss_write_buffer`, `seq_efSet`), and an assignment writes the same memory,
//! overwriting what arrived before it. efTest/efClear touch only the flag.
//!
//! Monitors wait in a queue here until taken, so every read, assignment, put
//! and flag operation first takes those already queued: they arrived first.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use epics_base_rs::runtime::task::Instant;
use epics_base_rs::server::database::PvDatabase;
use epics_base_rs::server::snl::{SeqPv, StateSet};
use epics_base_rs::types::EpicsValue;
use tokio::sync::Notify;

use crate::db_access::DbMultiMonitor;

/// How C declares a variable: `short` or `double`, `monitor`ed, `sync`ed.
#[derive(Clone, Copy)]
pub(crate) struct Decl {
    short: bool,
    monitor: bool,
    sync: bool,
}

/// `if (xxxCtlDebug) printf(...)`; `DEBUG` is the calling program's debug
/// variable.
macro_rules! ctl_debug {
    ($v:expr, $($arg:tt)*) => {
        if $v.on(DEBUG) {
            println!($($arg)*);
        }
    };
}
pub(crate) use ctl_debug;

pub(crate) const SHORT: Decl = Decl {
    short: true,
    monitor: false,
    sync: false,
};
pub(crate) const SHORT_MON: Decl = Decl {
    monitor: true,
    ..SHORT
};
pub(crate) const SHORT_SYNC: Decl = Decl {
    sync: true,
    ..SHORT_MON
};
pub(crate) const DOUBLE: Decl = Decl {
    short: false,
    ..SHORT
};
pub(crate) const DOUBLE_MON: Decl = Decl {
    monitor: true,
    ..DOUBLE
};
pub(crate) const DOUBLE_SYNC: Decl = Decl {
    sync: true,
    ..DOUBLE_MON
};

/// One state set's view of a program's variables, by index into the table
/// it was built from. An unmonitored variable holds what the program last
/// assigned (0 until then).
pub(crate) struct Vars {
    ss: StateSet,
    chans: Vec<SeqPv>,
    decls: Vec<Decl>,
    val: Vec<f64>,
    flag: Vec<bool>,
    by_pv: HashMap<String, usize>,
    monitor: DbMultiMonitor,
}

impl Vars {
    /// Subscribe the monitored variables among `watch`. Their connect-time
    /// monitors raise their flags, so each found starts raised; a PV that
    /// never connects delivers no monitor and raises nothing. A table entry
    /// is the program's variable name, its PV and its declaration.
    pub(crate) async fn new(
        db: &PvDatabase,
        ss: &StateSet,
        table: &[(&str, String, Decl)],
        origin: u64,
        watch: impl IntoIterator<Item = usize>,
    ) -> Self {
        let mut by_pv = HashMap::new();
        for i in watch {
            let (_, pv, d) = &table[i];
            if d.monitor && db.get_pv(pv).is_ok() {
                by_pv.insert(pv.clone(), i);
            }
        }
        let names: Vec<String> = by_pv.keys().cloned().collect();
        let monitor = DbMultiMonitor::new(db, &names).await;
        let mut val = vec![0.0; table.len()];
        let mut flag = vec![false; table.len()];
        for (pv, &i) in &by_pv {
            val[i] = db.get_pv(pv).ok().and_then(|v| v.to_f64()).unwrap_or(0.0);
            flag[i] = table[i].2.sync;
        }
        Self {
            ss: ss.clone(),
            chans: table
                .iter()
                .map(|(var, pv, _)| ss.pv_with_origin(var, pv, origin))
                .collect(),
            decls: table.iter().map(|(_, _, d)| *d).collect(),
            val,
            flag,
            by_pv,
            monitor,
        }
    }

    fn take(&mut self, pv: &str, v: f64) {
        if let Some(&i) = self.by_pv.get(pv) {
            self.val[i] = v;
            self.flag[i] |= self.decls[i].sync;
        }
    }

    fn poll(&mut self) {
        for (pv, v) in self.monitor.try_changes() {
            self.take(&pv, v);
        }
    }

    /// `pvSysFlush` at a state's entry (seq_task.c:303-304): the puts made
    /// before it have been taken, and so have the monitors they posted.
    pub(crate) async fn flush(&self) {
        self.ss.flush().await;
    }

    /// Wait for a monitor or `deadline`, whichever is first.
    pub(crate) async fn wait(&mut self, deadline: Option<Instant>) {
        let changes = match deadline {
            Some(at) => tokio::select! {
                r = self.monitor.wait_changes() => r,
                _ = epics_base_rs::runtime::task::sleep_until(at) => return,
            },
            None => self.monitor.wait_changes().await,
        };
        for (pv, v) in changes {
            self.take(&pv, v);
        }
    }

    pub(crate) fn get(&mut self, i: usize) -> f64 {
        self.poll();
        self.val[i]
    }

    pub(crate) fn on(&mut self, i: usize) -> bool {
        self.get(i) != 0.0
    }

    /// `var = v;`
    pub(crate) fn assign(&mut self, i: usize, v: f64) {
        self.poll();
        self.val[i] = v;
    }

    /// `pvPut(var)`.
    pub(crate) async fn put(&mut self, i: usize) {
        let v = self.get(i);
        let _ = if self.decls[i].short {
            self.chans[i].put_i16_process(v as i16).await
        } else {
            self.chans[i].put_f64_process(v).await
        };
    }

    /// `pvPut(var, ASYNC)`; a put already pending on the channel is refused.
    pub(crate) fn put_async(&mut self, i: usize) {
        let v = self.get(i);
        let _ = self.chans[i].put_async(if self.decls[i].short {
            EpicsValue::Short(v as i16)
        } else {
            EpicsValue::Double(v)
        });
    }

    /// `pvPutComplete(var)`.
    pub(crate) fn put_complete(&self, i: usize) -> bool {
        self.chans[i].put_complete()
    }

    /// `var = v; pvPut(var);`
    pub(crate) async fn set(&mut self, i: usize, v: f64) {
        self.assign(i, v);
        self.put(i).await;
    }

    /// `efTest`.
    pub(crate) fn test(&mut self, i: usize) -> bool {
        self.poll();
        self.flag[i]
    }

    /// `efClear`.
    pub(crate) fn clear(&mut self, i: usize) {
        self.poll();
        self.flag[i] = false;
    }

    /// `efTestAndClear`.
    pub(crate) fn test_and_clear(&mut self, i: usize) -> bool {
        self.poll();
        std::mem::take(&mut self.flag[i])
    }
}

/// An `evflag` one state set sets with `efSet` and another tests.
#[derive(Default)]
pub(crate) struct EventFlag {
    raised: AtomicBool,
    wake: Notify,
}

impl EventFlag {
    pub(crate) fn set(&self) {
        self.raised.store(true, Ordering::Release);
        self.wake.notify_one();
    }

    pub(crate) fn test_and_clear(&self) -> bool {
        self.raised.swap(false, Ordering::AcqRel)
    }

    /// Wait for a [`Self::set`] since the last wait.
    pub(crate) async fn raised(&self) {
        self.wake.notified().await;
    }
}

/// `delay()` in a state's `when` clauses: counted from state entry.
pub(crate) struct Delays {
    entered: Instant,
    pub(crate) next: Option<Instant>,
}

impl Delays {
    pub(crate) fn new() -> Self {
        Self {
            entered: Instant::now(),
            next: None,
        }
    }

    pub(crate) fn delay(&mut self, secs: f64) -> bool {
        let at = self.entered + epics_base_rs::runtime::time::duration_from_secs(secs);
        if Instant::now() >= at {
            return true;
        }
        self.next = Some(self.next.map_or(at, |n| n.min(at)));
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use epics_base_rs::server::records::ao::AoRecord;
    use epics_base_rs::server::snl::PutQueue;
    use std::time::Duration;

    /// A Vars over `T:x`, sync'd, and a monitor of 1 queued on it unread.
    async fn queued_arrival() -> Vars {
        let db = PvDatabase::new();
        db.add_record("T:x", Box::new(AoRecord::new(0.0)))
            .await
            .unwrap();
        let ss = PutQueue::new(&db, "t").state_set("t");
        let mut v = Vars::new(&db, &ss, &[("x", "T:x".into(), SHORT_SYNC)], 0, [0]).await;
        v.clear(0);
        crate::db_access::DbChannel::new(&db, "T:x")
            .put_f64_process(1.0)
            .await
            .unwrap();
        epics_base_rs::runtime::task::sleep(Duration::from_millis(20)).await;
        v
    }

    /// Wait as a state set does when no `when` is true.
    async fn idle(v: &mut Vars) {
        let at = Instant::now() + Duration::from_millis(50);
        v.wait(Some(at)).await;
    }

    /// kohzuCtl's `opAck = 0` overwrites an ack that arrived before it
    /// (kohzuCtl.st:651), so the next wait finds nothing left to take.
    #[epics_base_rs::epics_test]
    async fn an_arrival_queued_before_an_assignment_is_overwritten() {
        let mut v = queued_arrival().await;
        v.assign(0, 0.0);
        idle(&mut v).await;
        assert_eq!(v.get(0), 0.0);
    }

    /// `efClear` after a monitor arrived clears the flag it raised.
    #[epics_base_rs::epics_test]
    async fn a_clear_after_an_arrival_stays_clear() {
        let mut v = queued_arrival().await;
        v.clear(0);
        idle(&mut v).await;
        assert!(!v.test(0));
        assert_eq!(v.get(0), 1.0);
    }
}
