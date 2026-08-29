//! `menuScan` as runtime data — the table C reads with `dbFindMenu(pdbbase,
//! "menuScan")` in `initPeriodic` (`dbScan.c:856-918` at `R7.0.10`).
//!
//! The scan rates are NOT a property of the record system. They are a property
//! of the `menuScan.dbd` a site loaded, and overriding it is documented,
//! supported base behaviour: the choice strings are parsed at run time and any
//! number with any of C's units — `second(s)`, `minute(s)`, `hour(s)`, `Hz`,
//! `Hertz`, or nothing at all (seconds) — becomes a scan rate. A site that
//! wants `60 Hz` or `5 minutes` writes it in the menu and gets it.
//!
//! # The invariant
//!
//! **There is exactly one `menuScan` table per process. It is installed by the
//! `.dbd`/`.db` loader before anything resolves a SCAN value, and the first
//! resolution freezes it.** After the freeze the table is immutable, so no
//! record can be sitting in a scan list keyed by a menu that has since changed
//! shape — the failure the freeze exists to make unrepresentable.
//!
//! C has the same two-phase rule and enforces it by ordering alone:
//! `dbLoadDatabase` fills `pdbbase`, `iocInit` calls `scanInit` →
//! `initPeriodic`, and `nPeriodic`/`papPeriodic` never change again. Loading a
//! second, different `menuScan` after that point is a C-side foot-gun with no
//! diagnostic; here it is an error at the loader.

use std::sync::OnceLock;
use std::time::Duration;

use crate::runtime::stdlib::epics_parse_double_units;

/// C `SCAN_1ST_PERIODIC` (`dbScan.h:32`) — `menuScanI_O_Intr + 1`. The three
/// choices below it are fixed by `dbScan.c` itself (it tests
/// `menuScanPassive`, `menuScanEvent` and `menuScanI_O_Intr` by name), so a
/// site may append rates but may not renumber the first three.
pub const SCAN_1ST_PERIODIC: u16 = 3;

/// The stock `menuScan.dbd` shipped with base (`menuScan.dbd.pod:46-58`) —
/// what an IOC that loads no site menu scans at.
///
/// It is the GENERATED table, not a copy of it. Writing the ten choices out
/// again here would be a second declaration of a wire-visible index→label
/// mapping, which is the thing `dbd_generated` exists to prevent.
pub fn stock_choices() -> &'static [&'static str] {
    super::dbd_generated::MENU_SCAN
}

/// The loaded `menuScan`: its choice strings and, for each periodic choice,
/// the period C's `initPeriodic` computed from it.
pub struct MenuScan {
    choices: &'static [&'static str],
    /// One entry per choice at or above [`SCAN_1ST_PERIODIC`]. `None` is C's
    /// `papPeriodic[i] == NULL`: the choice string did not parse, or named a
    /// non-positive period, so the rate exists in the menu but has no scan
    /// list and no thread (`dbScan.c:899-902`, `free(ppsl); continue;`).
    periods: Vec<Option<Duration>>,
}

/// Why a `menuScan` could not be installed.
#[derive(Debug, PartialEq, Eq)]
pub enum InstallError {
    /// A SCAN value was already resolved against the table in force, so
    /// records may already be in scan lists keyed by it.
    AlreadyInUse,
    /// The menu does not carry the three choices `dbScan.c` names literally.
    /// C would not diagnose this; it would silently scan the wrong list.
    FixedChoicesRenamed,
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyInUse => write!(
                f,
                "menuScan is already in use — load it before any record or SCAN value"
            ),
            Self::FixedChoicesRenamed => write!(
                f,
                "menuScan must begin with the three choices dbScan.c names: \
                 Passive, Event, I/O Intr"
            ),
        }
    }
}

impl std::error::Error for InstallError {}

static MENU_SCAN: OnceLock<MenuScan> = OnceLock::new();

/// The pending table an [`install`] left for the first reader to freeze.
///
/// Two cells rather than one because installing must NOT freeze: a `.dbd` that
/// declares `menu(menuScan)` may be followed by another that declares it
/// again, and only the first *use* — a SCAN value resolved, a scan list built
/// — closes the door.
static PENDING: std::sync::Mutex<Option<Vec<String>>> = std::sync::Mutex::new(None);

/// Install a site `menuScan`. Idempotent for an identical menu, so loading the
/// same `.dbd` twice is not an error; a *different* menu after the table is in
/// use is [`InstallError::AlreadyInUse`].
///
/// `choices` is the menu's choice values in declaration order — the index is
/// the wire value of a `DBF_MENU` field, so the order is load-bearing.
pub fn install(choices: &[String]) -> Result<(), InstallError> {
    if choices.len() < SCAN_1ST_PERIODIC as usize
        || choices[..SCAN_1ST_PERIODIC as usize]
            .iter()
            .zip(stock_choices())
            .any(|(have, want)| have != want)
    {
        return Err(InstallError::FixedChoicesRenamed);
    }
    if let Some(frozen) = MENU_SCAN.get() {
        return if frozen.choices.len() == choices.len()
            && frozen.choices.iter().zip(choices).all(|(a, b)| a == b)
        {
            Ok(())
        } else {
            Err(InstallError::AlreadyInUse)
        };
    }
    *PENDING.lock().expect("menuScan install") = Some(choices.to_vec());
    Ok(())
}

/// The table in force, freezing it on the first call — C's `initPeriodic`
/// moment. Every period, label and scan-list decision reads it and nothing
/// else, so there is no second answer to "what rates does this IOC have".
pub fn menu_scan() -> &'static MenuScan {
    if let Some(m) = MENU_SCAN.get() {
        return m;
    }
    let pending = PENDING.lock().expect("menuScan freeze").take();
    let choices: &'static [&'static str] = match pending {
        // Leaked on purpose: this is a load-once process global that outlives
        // every reader, exactly as C's `pdbbase` menu does — C never frees it
        // either.
        Some(v) => Box::leak(
            v.into_iter()
                .map(|s| &*Box::leak(s.into_boxed_str()))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        ),
        None => stock_choices(),
    };
    let menu = MENU_SCAN.get_or_init(|| MenuScan::from_choices(choices));
    // C picks the scanOnce band from `nPeriodic` when `scanInit` creates the
    // thread, which is after `initPeriodic` — this is that moment, and the
    // facility lives a crate below, so the count is pushed rather than pulled.
    crate::runtime::background::scan_once::set_periodic_scan_band_count(menu.n_periodic());
    menu
}

impl MenuScan {
    fn from_choices(choices: &'static [&'static str]) -> Self {
        let periods: Vec<Option<Duration>> = choices[SCAN_1ST_PERIODIC as usize..]
            .iter()
            .map(|choice| period_of(choice))
            .collect();
        // C reports both of `initPeriodic`'s complaints and carries on
        // (`dbScan.c:877`, `:897`, `:912`). Without them a mistyped choice in a
        // site menu is a rate that silently never scans — the failure the
        // message exists to name.
        for (choice, period) in choices[SCAN_1ST_PERIODIC as usize..].iter().zip(&periods) {
            match period {
                None => crate::runtime::log::errlog_printf(&format!(
                    "initPeriodic: Bad menuScan choice '{choice}'\n"
                )),
                Some(p) if !is_achievable(*p) => crate::runtime::log::errlog_printf(&format!(
                    "initPeriodic: Scan rate '{choice}' is not achievable.\n"
                )),
                Some(_) => {}
            }
        }
        Self { choices, periods }
    }

    /// Every choice, in menu order — the `DBR_ENUM` label list a client is
    /// served for `.SCAN` and `.SSCN`.
    pub fn choices(&self) -> &'static [&'static str] {
        self.choices
    }

    /// C `nPeriodic` (`dbScan.c:866`) — `nChoice - SCAN_1ST_PERIODIC`. Note
    /// that this counts menu entries, INCLUDING one whose choice string did
    /// not parse: C sizes `papPeriodic` the same way and leaves the bad entry
    /// NULL, so the index of every later rate is unaffected by a bad one.
    pub fn n_periodic(&self) -> usize {
        self.periods.len()
    }

    /// The period of the choice at menu index `index`, or `None` when that
    /// index is not a periodic choice with a usable rate — either outside the
    /// menu, below [`SCAN_1ST_PERIODIC`], or C's `papPeriodic[i] == NULL`.
    pub fn period_at(&self, index: u16) -> Option<Duration> {
        if index < SCAN_1ST_PERIODIC {
            return None;
        }
        self.periods
            .get((index - SCAN_1ST_PERIODIC) as usize)
            .copied()
            .flatten()
    }

    /// The label of the choice at `index`, or `None` outside the menu.
    pub fn label_at(&self, index: u16) -> Option<&'static str> {
        self.choices.get(index as usize).copied()
    }

    /// The menu index a choice string names — the `.db` put path
    /// (`field(SCAN,"1 second")`) and C's `dbPutStringMenu`.
    pub fn index_of(&self, label: &str) -> Option<u16> {
        self.choices
            .iter()
            .position(|c| *c == label)
            .map(|i| i as u16)
    }

    /// Whether `index` is inside the menu at all — C's
    /// `scan >= nPeriodic + SCAN_1ST_PERIODIC` test in `scanAdd`
    /// (`dbScan.c:246`), which is the whole of its legality rule.
    pub fn is_in_menu(&self, index: u16) -> bool {
        (index as usize) < self.choices.len()
    }
}

/// C `initPeriodic`'s per-choice parse (`dbScan.c:874-899`): a number, then a
/// unit that is empty (seconds), `second(s)`, `minute(s)`, `hour(s)`, `Hz` or
/// `Hertz`, case-insensitively. A parse failure, a non-positive number, an
/// unrecognised unit, or a rate that works out to zero all leave the choice
/// with NO period — C frees the list and leaves the slot NULL.
fn period_of(choice: &str) -> Option<Duration> {
    let (number, unit) = epics_parse_double_units(choice).ok()?;
    if number <= 0.0 {
        return None;
    }
    let seconds = if unit.is_empty()
        || unit.eq_ignore_ascii_case("second")
        || unit.eq_ignore_ascii_case("seconds")
    {
        number
    } else if unit.eq_ignore_ascii_case("minute") || unit.eq_ignore_ascii_case("minutes") {
        number * 60.0
    } else if unit.eq_ignore_ascii_case("hour") || unit.eq_ignore_ascii_case("hours") {
        number * 60.0 * 60.0
    } else if unit.eq_ignore_ascii_case("Hz") || unit.eq_ignore_ascii_case("Hertz") {
        1.0 / number
    } else {
        return None;
    };
    // C's `if (ppsl->period == 0)` guard, reached the same way: `1/number`
    // for an enormous frequency underflows to zero, and a zero period would
    // be a scan thread in a spin loop.
    if seconds <= 0.0 || !seconds.is_finite() {
        return None;
    }
    Some(Duration::from_secs_f64(seconds))
}

/// C's achievability warning (`dbScan.c:909-914`): a period below two clock
/// ticks, or one that is not close to a whole number of ticks, cannot be kept.
/// C reports it and uses the rate anyway, and so does the caller of this.
fn is_achievable(period: Duration) -> bool {
    let quantum = crate::runtime::time::thread_sleep_quantum();
    if quantum <= 0.0 {
        return true;
    }
    let period = period.as_secs_f64();
    let ticks = period / quantum;
    period >= 2.0 * quantum && ticks / ticks.floor() <= 1.1
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stock menu, parsed rather than enumerated, must give exactly the
    /// seven periods the port used to hard-code.
    #[test]
    fn the_stock_menu_parses_to_bases_seven_rates() {
        let m = MenuScan::from_choices(stock_choices());
        assert_eq!(m.n_periodic(), 7);
        let want = [
            (3, Duration::from_secs(10)),
            (4, Duration::from_secs(5)),
            (5, Duration::from_secs(2)),
            (6, Duration::from_secs(1)),
            (7, Duration::from_millis(500)),
            (8, Duration::from_millis(200)),
            (9, Duration::from_millis(100)),
        ];
        for (index, period) in want {
            assert_eq!(m.period_at(index), Some(period), "menu index {index}");
        }
        assert_eq!(m.period_at(2), None, "I/O Intr is not periodic");
        assert_eq!(m.period_at(10), None, "outside the menu");
    }

    /// The units `initPeriodic` accepts, each at its boundary.
    #[test]
    fn every_unit_c_accepts_is_accepted_here() {
        for (choice, want) in [
            ("2", Duration::from_secs(2)),
            ("2 second", Duration::from_secs(2)),
            ("2 seconds", Duration::from_secs(2)),
            ("2 SECONDS", Duration::from_secs(2)),
            ("2 minute", Duration::from_secs(120)),
            ("2 minutes", Duration::from_secs(120)),
            ("1 hour", Duration::from_secs(3600)),
            ("2 hours", Duration::from_secs(7200)),
            ("60 Hz", Duration::from_secs_f64(1.0 / 60.0)),
            ("60 hertz", Duration::from_secs_f64(1.0 / 60.0)),
            ("0.5 Hertz", Duration::from_secs(2)),
        ] {
            assert_eq!(period_of(choice), Some(want), "choice {choice:?}");
        }
    }

    /// C's four ways to end up with no scan list for a menu entry.
    #[test]
    fn a_choice_c_rejects_yields_no_period() {
        for choice in [
            "Passive",     // no conversion at all
            "0 second",    // number <= 0
            "-1 second",   // number <= 0
            "1 fortnight", // unrecognised unit
            "",            // no conversion
        ] {
            assert_eq!(period_of(choice), None, "choice {choice:?}");
        }
    }

    /// A bad entry does not shift the entries after it — C sizes `papPeriodic`
    /// from `nChoice` and leaves the bad slot NULL, so index 4 stays index 4.
    #[test]
    fn a_bad_choice_holds_its_slot_instead_of_shifting_the_rest() {
        let choices: &'static [&'static str] =
            &["Passive", "Event", "I/O Intr", "1 fortnight", "1 second"];
        let m = MenuScan::from_choices(choices);
        assert_eq!(m.n_periodic(), 2);
        assert_eq!(m.period_at(3), None);
        assert_eq!(m.period_at(4), Some(Duration::from_secs(1)));
    }

    /// A site menu is a menu, not a patch: it may append, drop or reorder
    /// rates, and the labels are what a client is served.
    #[test]
    fn a_site_menu_replaces_the_rates_wholesale() {
        let choices: &'static [&'static str] =
            &["Passive", "Event", "I/O Intr", "60 Hz", "5 minutes"];
        let m = MenuScan::from_choices(choices);
        assert_eq!(m.n_periodic(), 2);
        assert_eq!(m.period_at(3), Some(Duration::from_secs_f64(1.0 / 60.0)));
        assert_eq!(m.period_at(4), Some(Duration::from_secs(300)));
        assert_eq!(m.label_at(4), Some("5 minutes"));
        assert_eq!(m.index_of("60 Hz"), Some(3));
        assert_eq!(m.index_of("1 second"), None, "not in this site's menu");
        assert!(!m.is_in_menu(5));
    }

    /// Renaming the three choices `dbScan.c` tests by name is refused rather
    /// than silently scanning the wrong list.
    #[test]
    fn the_three_fixed_choices_may_not_be_renamed_or_dropped() {
        let bad: Vec<String> = ["Passive", "Event", "IoIntr", "1 second"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(install(&bad), Err(InstallError::FixedChoicesRenamed));
        let short: Vec<String> = ["Passive", "Event"].iter().map(|s| s.to_string()).collect();
        assert_eq!(install(&short), Err(InstallError::FixedChoicesRenamed));
    }

    /// `Hz` is `1/number`, so a frequency big enough to underflow the period
    /// leaves the choice with no list rather than a spinning scan thread.
    #[test]
    fn a_frequency_that_underflows_to_a_zero_period_has_no_list() {
        assert_eq!(period_of("1e400 Hz"), None);
    }
}
