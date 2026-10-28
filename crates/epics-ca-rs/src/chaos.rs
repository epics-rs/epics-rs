//! Optional fault-injection harness for testing.
//!
//! Enabled via the `EPICS_CA_RS_CHAOS` env var, which is parsed once
//! at process start. Format: comma-separated `kind:value` pairs.
//!
//! - `drop:<percent>` — randomly drop bytes-pending-on-read events
//!   at the given percentage (0–100)
//! - `stall:<millis>` — sleep this many ms before completing each
//!   read
//! - `reorder:<percent>` — reorder pairs of consecutive frames
//! - `seed:<u64>` — make the RNG deterministic for repeatable runs
//!
//! Examples:
//! ```bash
//! EPICS_CA_RS_CHAOS=drop:5%,stall:50ms      # 5% drop, 50ms stall
//! EPICS_CA_RS_CHAOS=stall:10ms,seed:42      # deterministic stall
//! ```
//!
//! When the variable is unset (the default everywhere outside soak /
//! integration testing) the entire module compiles to no-ops: each
//! call is one atomic load + branch on the disabled flag.
//!
//! This is *not* a production tool. It exists so the chaos / soak
//! tests can exercise reconnect, backoff, and timeout paths without
//! requiring a network simulator.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Parsed chaos configuration. `enabled == false` makes every method
/// a fast no-op.
#[derive(Debug, Clone, Copy, Default)]
pub struct ChaosConfig {
    pub enabled: bool,
    pub drop_pct: u8,
    pub reorder_pct: u8,
    pub stall: Duration,
    pub seed: u64,
}

impl ChaosConfig {
    fn from_env_str(s: &str) -> Self {
        let mut cfg = Self {
            enabled: !s.trim().is_empty(),
            ..Self::default()
        };
        for tok in s.split(',') {
            let tok = tok.trim();
            if tok.is_empty() {
                continue;
            }
            let (k, v) = match tok.split_once(':') {
                Some(p) => p,
                None => continue,
            };
            let v = v.trim().trim_end_matches(['%', 's', 'm']);
            match k.trim() {
                "drop" => cfg.drop_pct = v.parse().unwrap_or(0).min(100),
                "reorder" => cfg.reorder_pct = v.parse().unwrap_or(0).min(100),
                "stall" => {
                    let raw = tok.split_once(':').unwrap().1;
                    cfg.stall = parse_duration(raw);
                }
                "seed" => cfg.seed = v.parse().unwrap_or(0),
                _ => {}
            }
        }
        cfg
    }
}

fn parse_duration(s: &str) -> Duration {
    let s = s.trim();
    if let Some(num) = s.strip_suffix("ms") {
        return Duration::from_millis(num.parse().unwrap_or(0));
    }
    if let Some(num) = s.strip_suffix('s') {
        return Duration::from_secs(num.parse().unwrap_or(0));
    }
    Duration::from_millis(s.parse().unwrap_or(0))
}

static CONFIG: OnceLock<ChaosConfig> = OnceLock::new();
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Resolve the global chaos config, parsing the env once.
pub fn config() -> &'static ChaosConfig {
    CONFIG.get_or_init(
        || match epics_base_rs::runtime::env::get_lossy("EPICS_CA_RS_CHAOS") {
            Some(s) => ChaosConfig::from_env_str(&s),
            None => ChaosConfig::default(),
        },
    )
}

/// Cheap "is anything injected?" check. Single atomic load.
#[inline]
pub fn enabled() -> bool {
    config().enabled
}

/// Linear-congruential pseudo RNG keyed by `seed + counter`. We want
/// reproducibility, not cryptographic strength — call sites that
/// matter (auth, keys) never use this. A separate counter per call
/// site is overkill; one global is enough for fuzzing-grade noise.
fn rand_u32() -> u32 {
    let cfg = config();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let s = cfg.seed ^ n;
    // SplitMix64 finalizer
    let mut x = s.wrapping_add(0x9E3779B97F4A7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D049BB133111EB);
    x ^= x >> 31;
    x as u32
}

/// `true` with probability `pct`.
fn roll(pct: u8) -> bool {
    if pct == 0 {
        return false;
    }
    if pct >= 100 {
        return true;
    }
    let r = rand_u32() % 100;
    r < pct as u32
}

/// If chaos is enabled and a stall is configured, sleep before the
/// caller proceeds. Awaitable so it integrates naturally into async
/// I/O paths.
pub async fn maybe_stall() {
    stall(config()).await;
}

async fn stall(cfg: &ChaosConfig) {
    if cfg.enabled && cfg.stall > Duration::ZERO {
        epics_base_rs::runtime::task::sleep(cfg.stall).await;
    }
}

/// Should this read return zero bytes (simulated drop)? When `true`,
/// the caller treats the I/O as if no data arrived. Combined with
/// inactivity timeouts this exercises the half-open detection path.
pub fn should_drop_read() -> bool {
    let cfg = config();
    cfg.enabled && roll(cfg.drop_pct)
}

/// Should this write be deferred briefly to simulate reorder? Returns
/// the number of microseconds to wait, or zero. Caller sleeps and
/// then sends.
pub fn reorder_delay() -> Duration {
    let cfg = config();
    if !cfg.enabled || !roll(cfg.reorder_pct) {
        return Duration::ZERO;
    }
    Duration::from_micros(((rand_u32() % 5_000) + 100) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_disabled_when_unset() {
        let c = ChaosConfig::from_env_str("");
        assert!(!c.enabled);
    }

    #[test]
    fn parse_drop_and_stall() {
        let c = ChaosConfig::from_env_str("drop:5%,stall:50ms");
        assert!(c.enabled);
        assert_eq!(c.drop_pct, 5);
        assert_eq!(c.stall, Duration::from_millis(50));
    }

    #[test]
    fn parse_seconds_stall() {
        let c = ChaosConfig::from_env_str("stall:2s");
        assert_eq!(c.stall, Duration::from_secs(2));
    }

    #[test]
    fn parse_seed_is_deterministic() {
        let c = ChaosConfig::from_env_str("drop:50%,seed:42");
        assert_eq!(c.seed, 42);
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let c = ChaosConfig::from_env_str("totally_made_up:1,drop:1%");
        assert_eq!(c.drop_pct, 1);
    }

    /// The stall waits on the runtime seam's timer, so it runs on the exec
    /// backend's executor, which has no tokio runtime. Bypasses the global
    /// config as `roll_extremes_are_honoured` does.
    #[epics_macros_rs::epics_test]
    async fn a_configured_stall_waits_it_out() {
        let cfg = ChaosConfig::from_env_str("stall:20ms");
        let start = std::time::Instant::now();
        stall(&cfg).await;
        assert!(start.elapsed() >= Duration::from_millis(20));
    }

    #[test]
    fn roll_extremes_are_honoured() {
        // Bypass the global config — exercise roll() directly.
        for _ in 0..1000 {
            assert!(!roll(0));
            assert!(roll(100));
        }
    }
}
