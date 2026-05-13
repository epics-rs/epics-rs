use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::SystemTime;

/// Closure that returns the current time, or `None` if unavailable.
type CurrentTimeFn = Box<dyn Fn() -> Option<SystemTime> + Send + Sync>;

/// Closure that returns the time for a given event number, or `None`.
type EventTimeFn = Box<dyn Fn(i32) -> Option<SystemTime> + Send + Sync>;

struct CurrentTimeProvider {
    name: String,
    priority: i32,
    get_time: CurrentTimeFn,
}

struct EventTimeProvider {
    name: String,
    priority: i32,
    get_event: EventTimeFn,
}

struct GeneralTimeInner {
    current_providers: Vec<CurrentTimeProvider>,
    event_providers: Vec<EventTimeProvider>,
    /// Monotonic ratchet for current time.
    last_provided_time: SystemTime,
    /// Per-event ratchet for events 1..=255.
    event_times: [SystemTime; 256],
    /// Ratchet for event -1 (BestTime).
    last_best_time: SystemTime,
    /// Name of the provider that last supplied current time.
    last_current_name: Option<String>,
    /// Name of the provider that last supplied event time.
    last_event_name: Option<String>,
    /// epics-base 8-D `5cfff383` `osiClockTime` sync hooks: callbacks
    /// notified when an external time source (PTP, NTP, beacon-driven
    /// PPS) reports a clock-step or fresh sync. Registered via
    /// [`register_clock_sync_hook`]; fired by [`notify_clock_sync`]
    /// when the time-source provider has new sync information.
    sync_hooks: Vec<Box<dyn Fn(SystemTime) + Send + Sync>>,
}

impl GeneralTimeInner {
    fn new() -> Self {
        let mut inner = Self {
            current_providers: Vec::new(),
            event_providers: Vec::new(),
            last_provided_time: SystemTime::UNIX_EPOCH,
            event_times: [SystemTime::UNIX_EPOCH; 256],
            last_best_time: SystemTime::UNIX_EPOCH,
            last_current_name: None,
            last_event_name: None,
            sync_hooks: Vec::new(),
        };
        // Register the OS clock as the last-resort current time provider.
        inner.current_providers.push(CurrentTimeProvider {
            name: "OS Clock".to_string(),
            priority: 999,
            get_time: Box::new(|| Some(SystemTime::now())),
        });
        inner
    }
}

static GENERAL_TIME: LazyLock<Mutex<GeneralTimeInner>> =
    LazyLock::new(|| Mutex::new(GeneralTimeInner::new()));

static ERROR_COUNTS: AtomicU64 = AtomicU64::new(0);

/// Register a current-time provider at the given priority (lower = higher priority).
pub fn register_current_provider(
    name: impl Into<String>,
    priority: i32,
    get_time: impl Fn() -> Option<SystemTime> + Send + Sync + 'static,
) {
    let mut inner = GENERAL_TIME.lock().unwrap();
    let provider = CurrentTimeProvider {
        name: name.into(),
        priority,
        get_time: Box::new(get_time),
    };
    let pos = inner
        .current_providers
        .iter()
        .position(|p| p.priority > priority)
        .unwrap_or(inner.current_providers.len());
    inner.current_providers.insert(pos, provider);
}

/// Register an event-time provider at the given priority (lower = higher priority).
pub fn register_event_provider(
    name: impl Into<String>,
    priority: i32,
    get_event: impl Fn(i32) -> Option<SystemTime> + Send + Sync + 'static,
) {
    let mut inner = GENERAL_TIME.lock().unwrap();
    let provider = EventTimeProvider {
        name: name.into(),
        priority,
        get_event: Box::new(get_event),
    };
    let pos = inner
        .event_providers
        .iter()
        .position(|p| p.priority > priority)
        .unwrap_or(inner.event_providers.len());
    inner.event_providers.insert(pos, provider);
}

/// Register a callback fired whenever a time provider reports a fresh
/// external sync (PTP master step, NTP sync window, GPS PPS). Mirrors
/// epics-base 8-D `5cfff383` `osiClockTime` sync-hook interface.
///
/// Hooks are invoked from [`notify_clock_sync`] in registration order
/// with the new (post-sync) time. They MUST be cheap — they execute
/// inside the general-time mutex; long work should defer to a spawn.
/// There is no de-registration API: hooks live for the process
/// lifetime, mirroring the upstream's atomic-add semantics.
pub fn register_clock_sync_hook<F>(hook: F)
where
    F: Fn(SystemTime) + Send + Sync + 'static,
{
    let mut inner = GENERAL_TIME.lock().unwrap();
    inner.sync_hooks.push(Box::new(hook));
}

/// Time-source providers (PTP/NTP integrations, hardware-clock
/// drivers) call this when they receive a fresh sync from their
/// upstream master. Every registered [`register_clock_sync_hook`]
/// callback fires with `t_synced` — the time the source reports as
/// authoritative right now.
///
/// `notify_clock_sync` does NOT itself update any internal cache —
/// `get_current` and `get_event` keep their existing ratchet semantics
/// (a backward step is rejected). The hook is purely a notification
/// channel for downstream consumers (records that want to log a
/// step, archivers that want to insert a discontinuity marker).
pub fn notify_clock_sync(t_synced: SystemTime) {
    let inner = GENERAL_TIME.lock().unwrap();
    for hook in &inner.sync_hooks {
        hook(t_synced);
    }
}

/// Get the current time from the highest-priority provider that succeeds.
///
/// The returned time is monotonically enforced: if a provider returns a time
/// earlier than the last provided time, the last provided time is returned
/// and the error counter is incremented.
pub fn get_current() -> SystemTime {
    let mut inner = GENERAL_TIME.lock().unwrap();
    for i in 0..inner.current_providers.len() {
        if let Some(t) = (inner.current_providers[i].get_time)() {
            let name = inner.current_providers[i].name.clone();
            if t >= inner.last_provided_time {
                inner.last_provided_time = t;
                inner.last_current_name = Some(name);
                return t;
            } else {
                ERROR_COUNTS.fetch_add(1, Ordering::Relaxed);
                inner.last_current_name = Some(name);
                return inner.last_provided_time;
            }
        }
    }
    // All providers failed — return last known time.
    ERROR_COUNTS.fetch_add(1, Ordering::Relaxed);
    inner.last_provided_time
}

/// Get the time for a specific event number.
///
/// - `event == 0`: delegates to [`get_current()`].
/// - `event == -1`: "BestTime" — queries current providers with its own ratchet.
/// - `event 1..=255`: per-slot ratcheted event time from event providers.
/// - `event >= 256`: event time from event providers, no ratchet.
pub fn get_event(event: i32) -> SystemTime {
    if event == 0 {
        return get_current();
    }

    let mut inner = GENERAL_TIME.lock().unwrap();

    if event == -1 {
        // BestTime: query current providers, apply separate ratchet.
        for i in 0..inner.current_providers.len() {
            if let Some(t) = (inner.current_providers[i].get_time)() {
                let name = inner.current_providers[i].name.clone();
                if t >= inner.last_best_time {
                    inner.last_best_time = t;
                    inner.last_event_name = Some(name);
                    return t;
                } else {
                    ERROR_COUNTS.fetch_add(1, Ordering::Relaxed);
                    inner.last_event_name = Some(name);
                    return inner.last_best_time;
                }
            }
        }
        ERROR_COUNTS.fetch_add(1, Ordering::Relaxed);
        return inner.last_best_time;
    }

    // Positive event: query event providers.
    for i in 0..inner.event_providers.len() {
        if let Some(t) = (inner.event_providers[i].get_event)(event) {
            let name = inner.event_providers[i].name.clone();
            inner.last_event_name = Some(name);

            if (1..=255).contains(&event) {
                let slot = event as usize;
                if t >= inner.event_times[slot] {
                    inner.event_times[slot] = t;
                    return t;
                } else {
                    ERROR_COUNTS.fetch_add(1, Ordering::Relaxed);
                    return inner.event_times[slot];
                }
            }
            // event >= 256: no ratchet
            return t;
        }
    }

    // No event provider succeeded — fall back to get_current().
    drop(inner);
    get_current()
}

/// Install a last-resort event provider that returns `SystemTime::now()` for any event.
pub fn install_last_resort_event_provider() {
    register_event_provider("OS Clock", 999, |_| Some(SystemTime::now()));
}

/// Return the cumulative count of monotonic-enforcement errors.
pub fn error_counts() -> u64 {
    ERROR_COUNTS.load(Ordering::Relaxed)
}

/// Reset the error counter to zero.
pub fn reset_error_counts() {
    ERROR_COUNTS.store(0, Ordering::Relaxed);
}

/// Return the name of the provider that last supplied current time.
pub fn current_provider_name() -> Option<String> {
    GENERAL_TIME.lock().unwrap().last_current_name.clone()
}

/// Return the name of the provider that last supplied event time.
pub fn event_provider_name() -> Option<String> {
    GENERAL_TIME.lock().unwrap().last_event_name.clone()
}

/// Generate a report of registered providers.
///
/// `level`: 0 = brief, 1+ = detailed.
pub fn report(level: i32) -> String {
    let inner = GENERAL_TIME.lock().unwrap();
    let mut out = String::new();
    out.push_str("Current Time Providers:\n");
    for p in &inner.current_providers {
        out.push_str(&format!("  \"{}\" priority {}\n", p.name, p.priority));
    }
    out.push_str("Event Time Providers:\n");
    if inner.event_providers.is_empty() {
        out.push_str("  (none)\n");
    } else {
        for p in &inner.event_providers {
            out.push_str(&format!("  \"{}\" priority {}\n", p.name, p.priority));
        }
    }
    if level > 0 {
        out.push_str(&format!("Error count: {}\n", error_counts()));
        if let Some(ref name) = inner.last_current_name {
            out.push_str(&format!("Last current provider: \"{}\"\n", name));
        }
        if let Some(ref name) = inner.last_event_name {
            out.push_str(&format!("Last event provider: \"{}\"\n", name));
        }
    }
    out
}

/// Reset all state for test isolation. Only available in tests.
#[cfg(test)]
fn _reset_for_testing() {
    let mut inner = GENERAL_TIME.lock().unwrap();
    *inner = GeneralTimeInner::new();
    ERROR_COUNTS.store(0, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Serialize all tests that touch the global GENERAL_TIME state.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn os_clock_default_returns_reasonable_time() {
        let _g = TEST_LOCK.lock().unwrap();
        _reset_for_testing();
        let t = get_current();
        let secs = t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
        // Should be after 2020-01-01
        assert!(secs > 1_577_836_800, "time should be after 2020");
    }

    #[test]
    fn custom_provider_overrides_os_clock() {
        let _g = TEST_LOCK.lock().unwrap();
        _reset_for_testing();
        let fixed = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000);
        register_current_provider("Test Clock", 10, move || Some(fixed));

        let t = get_current();
        assert_eq!(t, fixed);
        assert_eq!(current_provider_name().as_deref(), Some("Test Clock"));
    }

    #[test]
    fn provider_returning_none_falls_through() {
        let _g = TEST_LOCK.lock().unwrap();
        _reset_for_testing();
        let fixed = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000);
        // High-priority provider that always fails.
        register_current_provider("Broken", 1, || None);
        // Lower-priority provider that succeeds.
        register_current_provider("Fallback", 50, move || Some(fixed));

        let t = get_current();
        assert_eq!(t, fixed);
        assert_eq!(current_provider_name().as_deref(), Some("Fallback"));
    }

    #[test]
    fn monotonic_enforcement() {
        let _g = TEST_LOCK.lock().unwrap();
        _reset_for_testing();
        let t1 = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000);
        let t2 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_999_999_000); // backwards

        let call = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let call_c = call.clone();

        register_current_provider("Stepper", 10, move || {
            let n = call_c.fetch_add(1, Ordering::Relaxed);
            match n {
                0 => Some(t1),
                _ => Some(t2),
            }
        });

        reset_error_counts();
        let first = get_current();
        assert_eq!(first, t1);
        assert_eq!(error_counts(), 0);

        let second = get_current();
        // Should return the last provided (t1), not t2.
        assert_eq!(second, t1);
        assert_eq!(error_counts(), 1);
    }

    #[test]
    fn event_zero_delegates_to_get_current() {
        let _g = TEST_LOCK.lock().unwrap();
        _reset_for_testing();
        let fixed = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000);
        register_current_provider("Fixed", 10, move || Some(fixed));

        let t = get_event(0);
        assert_eq!(t, fixed);
    }

    #[test]
    fn event_per_slot_ratcheting() {
        let _g = TEST_LOCK.lock().unwrap();
        _reset_for_testing();
        let t1 = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000);
        let t2 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_999_999_000); // backwards
        let t3 = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_001_000); // forward

        let call = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let call_c = call.clone();

        register_event_provider("EventSrc", 10, move |_ev| {
            let n = call_c.fetch_add(1, Ordering::Relaxed);
            match n {
                0 => Some(t1),
                1 => Some(t2),
                _ => Some(t3),
            }
        });

        reset_error_counts();
        let first = get_event(42);
        assert_eq!(first, t1);

        let second = get_event(42);
        // Ratcheted: returns t1, not t2
        assert_eq!(second, t1);
        assert_eq!(error_counts(), 1);

        let third = get_event(42);
        assert_eq!(third, t3);
    }

    #[test]
    fn event_best_time_ratcheting() {
        let _g = TEST_LOCK.lock().unwrap();
        _reset_for_testing();
        let t1 = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000);
        let t2 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_999_999_000);

        let call = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let call_c = call.clone();

        register_current_provider("BestSrc", 10, move || {
            let n = call_c.fetch_add(1, Ordering::Relaxed);
            match n {
                0 => Some(t1),
                _ => Some(t2),
            }
        });

        reset_error_counts();
        let first = get_event(-1);
        assert_eq!(first, t1);

        let second = get_event(-1);
        assert_eq!(second, t1); // ratcheted
        assert_eq!(error_counts(), 1);
    }

    #[test]
    fn error_counts_reset() {
        let _g = TEST_LOCK.lock().unwrap();
        _reset_for_testing();
        let t_back = SystemTime::UNIX_EPOCH + Duration::from_secs(1);
        register_current_provider("AlwaysBack", 10, move || Some(t_back));

        // First call sets last_provided_time, second triggers backward detection
        // Actually: UNIX_EPOCH ratchet means first call with t > EPOCH is fine,
        // but we need the OS clock at priority 999 to not interfere.
        // After _reset_for_testing, OS clock is present. AlwaysBack at priority 10
        // wins. First call: t=1s > EPOCH → ok. Then the ratchet is at 1s.
        // Need to trigger backward. Let's use get_event(-1) to get a fresh ratchet.

        reset_error_counts();
        assert_eq!(error_counts(), 0);

        // Force an error via best-time ratchet.
        let t_high = SystemTime::UNIX_EPOCH + Duration::from_secs(3_000_000_000);
        {
            let mut inner = GENERAL_TIME.lock().unwrap();
            inner.last_best_time = t_high;
        }
        // Now any current provider returning < t_high on event -1 path will error.
        let _ = get_event(-1);
        assert!(error_counts() > 0);

        reset_error_counts();
        assert_eq!(error_counts(), 0);
    }

    /// epics-base 8-D `5cfff383` regression: a registered sync hook
    /// fires when `notify_clock_sync` is invoked. Multiple hooks
    /// fire in registration order. Hooks live for process lifetime
    /// — the test asserts via Arc<Mutex<Vec<...>>> capture rather
    /// than de-registering.
    #[test]
    fn sync_hooks_fire_in_registration_order() {
        use std::sync::{Arc, Mutex};
        let captured: Arc<Mutex<Vec<(usize, SystemTime)>>> = Arc::new(Mutex::new(Vec::new()));

        let cap1 = captured.clone();
        register_clock_sync_hook(move |t| {
            cap1.lock().unwrap().push((1, t));
        });
        let cap2 = captured.clone();
        register_clock_sync_hook(move |t| {
            cap2.lock().unwrap().push((2, t));
        });

        let synced = SystemTime::UNIX_EPOCH + Duration::from_secs(5_000_000_000);
        notify_clock_sync(synced);

        let log = captured.lock().unwrap();
        // Other tests may have registered hooks too — filter to ours.
        let ours: Vec<_> = log.iter().filter(|(id, _)| *id == 1 || *id == 2).collect();
        assert!(
            ours.len() >= 2,
            "both hooks must have fired at least once: {ours:?}"
        );
        // Find the most recent pair-firing — registration order
        // means the (1, _) entry must precede the (2, _) entry that
        // share our exact `synced` value.
        let last1_idx = ours
            .iter()
            .rposition(|(id, t)| *id == 1 && *t == synced)
            .expect("hook 1 fired with our synced timestamp");
        let last2_idx = ours
            .iter()
            .rposition(|(id, t)| *id == 2 && *t == synced)
            .expect("hook 2 fired with our synced timestamp");
        assert!(
            last1_idx < last2_idx,
            "hook 1 must fire before hook 2 (registration order)"
        );
    }
}
