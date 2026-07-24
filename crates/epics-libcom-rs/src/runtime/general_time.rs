use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::SystemTime;

/// Closure that returns the current time, or `None` if unavailable.
type CurrentTimeFn = Box<dyn Fn() -> Option<SystemTime> + Send + Sync>;

/// Closure that returns the time for a given event number, or `None`.
type EventTimeFn = Box<dyn Fn(i32) -> Option<SystemTime> + Send + Sync>;

/// Seconds between the Unix epoch (1970-01-01) and the EPICS epoch
/// (1990-01-01 00:00:00 UTC).
pub const EPICS_EPOCH_UNIX_SECS: u64 = 631_152_000;

/// The EPICS epoch (1990-01-01 00:00:00 UTC) expressed as a Unix
/// `SystemTime`.
///
/// This is the value of an all-zero `epicsTimeStamp`, and therefore the
/// single owner of "a time nobody has set yet" everywhere in the port —
/// a record's `TIME` before its first `process()`, the general-time
/// ratchet's seed. Seeding such a field with `SystemTime::UNIX_EPOCH`
/// instead is a 20-year error that survives every conversion: C's
/// `epicsTimeToTimespec` adds `POSIX_TIME_AT_EPICS_EPOCH`, so the wire
/// value C derives from `{0,0}` is 631152000, not 0.
///
/// C parity: `epicsGeneralTime.c:66` zero-initialises `lastProvidedTime`
/// to `epicsTimeStamp {0,0}`; `dbCommon.time` is likewise zeroed by
/// `calloc` in `dbStaticLib` and never touched until the record first
/// processes.
pub fn epics_epoch() -> SystemTime {
    SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(EPICS_EPOCH_UNIX_SECS)
}

struct CurrentTimeProvider {
    name: String,
    priority: i32,
    get_time: CurrentTimeFn,
    /// Whether this provider is safe to call from interrupt context.
    /// C parity: `generalTimeAddIntCurrentTimeProvider` registers an
    /// interrupt-callable variant queried by `epicsTimeGetCurrentInt`.
    interrupt_safe: bool,
    /// `true` for the built-in last-resort OS-clock provider. C tracks
    /// this via the `osdTimeGetCurrent` function-pointer identity.
    is_os_default: bool,
}

struct EventTimeProvider {
    name: String,
    priority: i32,
    get_event: EventTimeFn,
    /// Interrupt-callable variant — see [`CurrentTimeProvider::interrupt_safe`].
    interrupt_safe: bool,
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
    /// C parity: `epicsGeneralTime.c:84` `useOsdGetCurrent`. Starts
    /// `true`; while only the built-in OS-clock provider is registered,
    /// `get_current` short-circuits straight to the OS clock and the
    /// monotonic ratchet is **never consulted** — a real backward
    /// wall-clock step is returned verbatim, exactly as a C IOC does.
    /// Cleared by [`register_current_provider`] the moment a
    /// non-default provider is registered.
    use_osd_get_current: bool,
    /// Rust-only notification channel — see [`register_clock_sync_hook`].
    /// This is **not** a C-base API; it is an additive extension.
    sync_hooks: Vec<Box<dyn Fn(SystemTime) + Send + Sync>>,
}

impl GeneralTimeInner {
    fn new() -> Self {
        let mut inner = Self {
            current_providers: Vec::new(),
            event_providers: Vec::new(),
            last_provided_time: epics_epoch(),
            event_times: [epics_epoch(); 256],
            last_best_time: epics_epoch(),
            last_current_name: None,
            last_event_name: None,
            use_osd_get_current: true,
            sync_hooks: Vec::new(),
        };
        // Register the OS clock as the last-resort current time provider.
        inner.current_providers.push(CurrentTimeProvider {
            name: "OS Clock".to_string(),
            priority: 999,
            get_time: Box::new(|| Some(SystemTime::now())),
            interrupt_safe: true,
            is_os_default: true,
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
    register_current_provider_impl(name.into(), priority, Box::new(get_time), false);
}

/// Register an **interrupt-callable** current-time provider.
///
/// C parity: `epicsGeneralTime.c:445-459` `generalTimeAddIntCurrentTimeProvider`.
/// The closure MUST be callable from interrupt context — it must not
/// block, allocate, or take locks. Only providers registered this way
/// are consulted by [`get_current_int`].
pub fn register_int_current_provider(
    name: impl Into<String>,
    priority: i32,
    get_time: impl Fn() -> Option<SystemTime> + Send + Sync + 'static,
) {
    register_current_provider_impl(name.into(), priority, Box::new(get_time), true);
}

fn register_current_provider_impl(
    name: String,
    priority: i32,
    get_time: CurrentTimeFn,
    interrupt_safe: bool,
) {
    let mut inner = GENERAL_TIME.lock().unwrap();
    let provider = CurrentTimeProvider {
        name,
        priority,
        get_time,
        interrupt_safe,
        is_os_default: false,
    };
    let pos = inner
        .current_providers
        .iter()
        .position(|p| p.priority > priority)
        .unwrap_or(inner.current_providers.len());
    inner.current_providers.insert(pos, provider);
    // C `insertProvider`: clear `useOsdGetCurrent` once the provider
    // list holds more than just the built-in OS default. Any provider
    // registered through this path is non-default, so clear the flag.
    inner.use_osd_get_current = false;
}

/// Register an event-time provider at the given priority (lower = higher priority).
pub fn register_event_provider(
    name: impl Into<String>,
    priority: i32,
    get_event: impl Fn(i32) -> Option<SystemTime> + Send + Sync + 'static,
) {
    register_event_provider_impl(name.into(), priority, Box::new(get_event), false);
}

/// Register an **interrupt-callable** event-time provider.
///
/// C parity: `epicsGeneralTime.c:488-502` `generalTimeAddIntEventProvider`.
/// Same interrupt-context constraints as [`register_int_current_provider`].
/// Only providers registered this way are consulted by [`get_event_int`].
pub fn register_int_event_provider(
    name: impl Into<String>,
    priority: i32,
    get_event: impl Fn(i32) -> Option<SystemTime> + Send + Sync + 'static,
) {
    register_event_provider_impl(name.into(), priority, Box::new(get_event), true);
}

fn register_event_provider_impl(
    name: String,
    priority: i32,
    get_event: EventTimeFn,
    interrupt_safe: bool,
) {
    let mut inner = GENERAL_TIME.lock().unwrap();
    let provider = EventTimeProvider {
        name,
        priority,
        get_event,
        interrupt_safe,
    };
    let pos = inner
        .event_providers
        .iter()
        .position(|p| p.priority > priority)
        .unwrap_or(inner.event_providers.len());
    inner.event_providers.insert(pos, provider);
}

/// Register a callback fired whenever a time provider reports a fresh
/// external sync (PTP master step, NTP sync window, GPS PPS).
///
/// **Rust-only extension — not an epics-base API.** EPICS base has no
/// public registerable clock-sync hook: `osiClockTime.c` keeps its
/// `ClockTimeSync` logic strictly internal. This is an additive
/// notification channel for the Rust port's downstream consumers and is
/// intentionally kept separate from the C-parity `get_current` /
/// `get_event` paths.
///
/// Hooks are invoked from [`notify_clock_sync`] in registration order
/// with the new (post-sync) time. They MUST be cheap — they execute
/// inside the general-time mutex; long work should defer to a spawn.
/// There is no de-registration API: hooks live for the process
/// lifetime.
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
/// C parity (`epicsGeneralTime.c:111-112`): while only the built-in
/// OS-clock provider is registered (`use_osd_get_current`), this
/// short-circuits straight to the OS clock and the monotonic ratchet is
/// **not** consulted — a backward wall-clock step (NTP slew, manual
/// `date` change) is returned verbatim and does **not** count an error.
///
/// Once any non-default provider is registered, the returned time is
/// monotonically enforced: if a provider returns a time earlier than
/// the last provided time, the last provided time is returned and the
/// error counter is incremented.
pub fn get_current() -> SystemTime {
    let mut inner = GENERAL_TIME.lock().unwrap();

    // C `useOsdGetCurrent` short-circuit: no ratchet, no error count.
    if inner.use_osd_get_current {
        if let Some(idx) = inner.current_providers.iter().position(|p| p.is_os_default) {
            if let Some(t) = (inner.current_providers[idx].get_time)() {
                let name = inner.current_providers[idx].name.clone();
                inner.last_provided_time = t;
                inner.last_current_name = Some(name);
                return t;
            }
        }
        // OS clock unavailable (should not happen) — fall through to
        // the ratcheted path below as a last resort.
    }

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

/// Get the current time, querying only providers **other than** the one
/// at `ignore_priority`.
///
/// C parity: `epicsGeneralTime.c:106-151` `generalTimeGetExceptPriority`.
/// Used by a time provider (typically an NTP/clock provider) that needs
/// to read "the best time other than mine" to validate its own sync
/// without recursing into itself.
///
/// Returns `(time, priority_used)` — the priority of the provider that
/// answered — or `None` when no other provider succeeded. **No ratchet
/// is applied**: this query may legitimately go backwards, exactly as
/// the C function documents ("No ratchet, time from this routine may go
/// backwards").
///
/// `ignore_priority` follows the C convention: a positive value skips
/// the provider *at* that priority; a negative value `-n` skips every
/// provider *except* the one at priority `n`.
pub fn get_current_except_priority(ignore_priority: i32) -> Option<(SystemTime, i32)> {
    let inner = GENERAL_TIME.lock().unwrap();
    for p in &inner.current_providers {
        if (ignore_priority > 0 && p.priority == ignore_priority)
            || (ignore_priority < 0 && p.priority != -ignore_priority)
        {
            continue;
        }
        if let Some(t) = (p.get_time)() {
            return Some((t, p.priority));
        }
    }
    None
}

/// Interrupt-context current-time query.
///
/// C parity: `epicsGeneralTime.c:226-238` `epicsTimeGetCurrentInt`.
/// Consults only providers registered via
/// [`register_int_current_provider`] (interrupt-callable). Returns
/// `None` when no interrupt-safe provider answers — the C function
/// returns `S_time_noProvider` in that case. **No ratchet** — the C
/// `*Int` path does not touch the shared ratchet state (it must be
/// interrupt-safe).
pub fn get_current_int() -> Option<SystemTime> {
    let inner = GENERAL_TIME.lock().unwrap();
    for p in &inner.current_providers {
        if !p.interrupt_safe {
            continue;
        }
        if let Some(t) = (p.get_time)() {
            return Some(t);
        }
    }
    None
}

/// Interrupt-context event-time query.
///
/// C parity: `epicsGeneralTime.c:351-367` `epicsTimeGetEventInt`.
/// Consults only event providers registered via
/// [`register_int_event_provider`]. Returns `None` when no
/// interrupt-safe event provider answers. **No ratchet.**
pub fn get_event_int(event: i32) -> Option<SystemTime> {
    let inner = GENERAL_TIME.lock().unwrap();
    for p in &inner.event_providers {
        if !p.interrupt_safe {
            continue;
        }
        if let Some(t) = (p.get_event)(event) {
            return Some(t);
        }
    }
    None
}

/// Name of the highest-priority registered current-time provider.
///
/// C parity: `epicsGeneralTime.c` `generalTimeHighestCurrentName` —
/// the provider at the front of the priority-ordered list. Returns
/// `None` only when no provider is registered (the Rust port always
/// has the built-in OS clock, so this is effectively always `Some`).
pub fn highest_current_name() -> Option<String> {
    GENERAL_TIME
        .lock()
        .unwrap()
        .current_providers
        .first()
        .map(|p| p.name.clone())
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

/// Format a `SystemTime` the way C `generalTimeReport` does
/// (`epicsTimeToStrftime` with `"%Y-%m-%d %H:%M:%S.%06f"`).
///
/// `epicsTimeToStrftime` converts via `epicsTime_localtime` -> `localtime_r`
/// (epicsTime.cpp:202 -> :318, osdTime.cpp:82), i.e. LOCAL wall-clock, not
/// UTC — so this uses `chrono::Local` to match the C report output.
fn format_time_sample(t: SystemTime) -> String {
    let dt: chrono::DateTime<chrono::Local> = t.into();
    dt.format("%Y-%m-%d %H:%M:%S.%6f").to_string()
}

/// Generate a report of registered providers.
///
/// C parity: `epicsGeneralTime.c:530-618` `generalTimeReport`.
/// - First line: `Backwards time errors prevented N times.` followed
///   by a blank line.
/// - `Current Time Providers:` / `Event Time Providers:` headers.
/// - Each provider line is indented with 4 spaces and formatted
///   `"name", priority = N`.
/// - At `level > 0`, each *current* provider also prints its current
///   time sample on the next line (tab-indented), or
///   `Current Time not available`.
/// - When a list is empty, prints a tab-indented
///   `No Providers registered.` line.
///
/// `level`: 0 = brief, 1+ = detailed.
pub fn report(level: i32) -> String {
    let inner = GENERAL_TIME.lock().unwrap();
    let mut out = String::new();

    // C: printf("Backwards time errors prevented %u times.\n\n", ...)
    out.push_str(&format!(
        "Backwards time errors prevented {} times.\n\n",
        error_counts()
    ));

    // Current Time Providers.
    out.push_str("Current Time Providers:\n");
    if inner.current_providers.is_empty() {
        out.push_str("\tNo Providers registered.\n");
    } else {
        for p in &inner.current_providers {
            out.push_str(&format!("    \"{}\", priority = {}\n", p.name, p.priority));
            if level != 0 {
                match (p.get_time)() {
                    Some(t) => {
                        out.push_str(&format!("\tCurrent Time is {}.\n", format_time_sample(t)))
                    }
                    None => out.push_str("\tCurrent Time not available\n"),
                }
            }
        }
        // C `puts(message)` appends one newline after the provider block.
        out.push('\n');
    }

    // Event Time Providers.
    out.push_str("Event Time Providers:\n");
    if inner.event_providers.is_empty() {
        out.push_str("\tNo Providers registered.\n");
    } else {
        for p in &inner.event_providers {
            out.push_str(&format!("    \"{}\", priority = {}\n", p.name, p.priority));
        }
        out.push('\n');
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

    /// Rust-only sync-hook extension: a registered sync hook fires
    /// when `notify_clock_sync` is invoked. Multiple hooks fire in
    /// registration order. Hooks live for process lifetime — the test
    /// asserts via Arc<Mutex<Vec<...>>> capture rather than
    /// de-registering. (No C-base counterpart; see
    /// `register_clock_sync_hook`.)
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

    /// M1 C-parity: while only the built-in OS clock is registered,
    /// `get_current` bypasses the monotonic ratchet — a backward
    /// wall-clock step is returned verbatim and counts no error.
    #[test]
    fn os_clock_only_bypasses_ratchet() {
        let _g = TEST_LOCK.lock().unwrap();
        _reset_for_testing();

        // Replace the built-in OS clock with a controllable stepping
        // clock that is still flagged as the OS default, so the
        // `use_osd_get_current` short-circuit stays active.
        let t_high = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000);
        let t_low = SystemTime::UNIX_EPOCH + Duration::from_secs(1_999_999_000);
        let call = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let call_c = call.clone();
        {
            let mut inner = GENERAL_TIME.lock().unwrap();
            inner.current_providers.clear();
            inner.current_providers.push(CurrentTimeProvider {
                name: "OS Clock".to_string(),
                priority: 999,
                get_time: Box::new(move || {
                    let n = call_c.fetch_add(1, Ordering::Relaxed);
                    Some(if n == 0 { t_high } else { t_low })
                }),
                interrupt_safe: true,
                is_os_default: true,
            });
            inner.use_osd_get_current = true;
        }

        reset_error_counts();
        let first = get_current();
        assert_eq!(first, t_high);
        // Backward step is returned verbatim — NO ratchet, NO error.
        let second = get_current();
        assert_eq!(
            second, t_low,
            "OS-clock-only path must follow a backward step (C useOsdGetCurrent)"
        );
        assert_eq!(
            error_counts(),
            0,
            "OS-clock-only backward step must not count an error"
        );
    }

    /// M1 C-parity: registering a non-default provider clears the
    /// `use_osd_get_current` flag, so the ratchet is back in force.
    #[test]
    fn registering_provider_enables_ratchet() {
        let _g = TEST_LOCK.lock().unwrap();
        _reset_for_testing();
        let t_high = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000);
        let t_low = SystemTime::UNIX_EPOCH + Duration::from_secs(1_999_999_000);
        let call = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let call_c = call.clone();
        register_current_provider("Stepper", 10, move || {
            let n = call_c.fetch_add(1, Ordering::Relaxed);
            Some(if n == 0 { t_high } else { t_low })
        });

        reset_error_counts();
        assert_eq!(get_current(), t_high);
        // With a non-default provider present, the ratchet clamps the
        // backward step and counts an error.
        assert_eq!(get_current(), t_high);
        assert_eq!(error_counts(), 1);
    }

    /// M2 C-parity: `get_current_except_priority` skips the named
    /// provider and applies no ratchet.
    #[test]
    fn except_priority_skips_named_provider() {
        let _g = TEST_LOCK.lock().unwrap();
        _reset_for_testing();
        let t10 = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000);
        let t20 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_900_000_000);
        register_current_provider("P10", 10, move || Some(t10));
        register_current_provider("P20", 20, move || Some(t20));

        // Ignoring priority 10 -> answered by P20 (priority 20).
        let (t, prio) = get_current_except_priority(10).expect("P20 answers");
        assert_eq!(t, t20);
        assert_eq!(prio, 20);

        // Negative ignore: keep only priority 10.
        let (t, prio) = get_current_except_priority(-10).expect("P10 answers");
        assert_eq!(t, t10);
        assert_eq!(prio, 10);
    }

    /// M2 C-parity: interrupt-callable providers are consulted only by
    /// the `*_int` query paths.
    #[test]
    fn int_providers_only_seen_by_int_queries() {
        let _g = TEST_LOCK.lock().unwrap();
        _reset_for_testing();
        let t_int = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000);
        register_int_current_provider("IntClock", 5, move || Some(t_int));
        assert_eq!(get_current_int(), Some(t_int));

        let t_evt = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_500);
        register_int_event_provider("IntEvent", 5, move |_| Some(t_evt));
        assert_eq!(get_event_int(7), Some(t_evt));
    }

    /// M2 C-parity: `get_event_int` returns `None` when no
    /// interrupt-safe event provider is registered.
    #[test]
    fn int_event_query_none_without_int_provider() {
        let _g = TEST_LOCK.lock().unwrap();
        _reset_for_testing();
        // A non-int event provider must not satisfy the int query.
        register_event_provider("Plain", 10, |_| {
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1))
        });
        assert_eq!(get_event_int(3), None);
    }

    /// M2 C-parity: `highest_current_name` returns the front-of-list
    /// (highest-priority) current provider.
    #[test]
    fn highest_current_name_is_top_priority() {
        let _g = TEST_LOCK.lock().unwrap();
        _reset_for_testing();
        // Only the OS clock (priority 999) -> it is the highest.
        assert_eq!(highest_current_name().as_deref(), Some("OS Clock"));
        register_current_provider("Primary", 1, || None);
        assert_eq!(highest_current_name().as_deref(), Some("Primary"));
    }

    /// M3 C-parity: `report` output mirrors `generalTimeReport`.
    #[test]
    fn report_format_matches_general_time_report() {
        let _g = TEST_LOCK.lock().unwrap();
        _reset_for_testing();
        let r = report(0);
        // First line is the backwards-error count.
        assert!(
            r.starts_with("Backwards time errors prevented 0 times.\n\n"),
            "report must lead with the backwards-error line: {r:?}"
        );
        assert!(r.contains("Current Time Providers:\n"));
        // 4-space indent, `, priority = N` form.
        assert!(
            r.contains("    \"OS Clock\", priority = 999\n"),
            "provider line must use C `\"name\", priority = N` form: {r:?}"
        );
        // No event providers -> tab-indented "No Providers registered."
        assert!(
            r.contains("Event Time Providers:\n\tNo Providers registered.\n"),
            "empty event list must print the C placeholder: {r:?}"
        );
        // The old format strings must be gone.
        assert!(!r.contains("\" priority "));
        assert!(!r.contains("(none)"));
    }

    /// M3 C-parity: at `level > 0`, each current provider prints its
    /// time sample on a tab-indented line.
    #[test]
    fn report_level_one_prints_time_sample() {
        let _g = TEST_LOCK.lock().unwrap();
        _reset_for_testing();
        let r = report(1);
        assert!(
            r.contains("\tCurrent Time is "),
            "level>0 report must print a per-provider time sample: {r:?}"
        );
    }

    /// L2 C-parity: the ratchet seeds at the EPICS epoch (1990-01-01),
    /// not the Unix epoch (1970-01-01) — `epicsTimeStamp {0,0}`.
    #[test]
    fn ratchet_seeds_at_epics_epoch() {
        // 631_152_000 s past the Unix epoch == 1990-01-01 00:00:00 UTC.
        let secs = epics_epoch()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(secs, EPICS_EPOCH_UNIX_SECS);
        assert_eq!(secs, 631_152_000);

        let _g = TEST_LOCK.lock().unwrap();
        _reset_for_testing();
        let inner = GENERAL_TIME.lock().unwrap();
        assert_eq!(inner.last_provided_time, epics_epoch());
        assert_eq!(inner.last_best_time, epics_epoch());
        assert!(inner.event_times.iter().all(|t| *t == epics_epoch()));
    }
}
