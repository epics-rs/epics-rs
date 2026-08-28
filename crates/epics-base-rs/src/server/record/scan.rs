use super::menu_scan::{SCAN_1ST_PERIODIC, menu_scan};

/// The value of a `menu(menuScan)` field — the SCAN field's domain.
///
/// The domain is the *loaded* menu, not a fixed list of rates. C fixes exactly
/// three choices — `dbScan.c` tests `menuScanPassive`, `menuScanEvent` and
/// `menuScanI_O_Intr` by name — and reads everything from
/// `SCAN_1ST_PERIODIC` up out of `menuScan` at run time (`initPeriodic`,
/// `dbScan.c:856-918` at `R7.0.10`). A site that ships its own `menuScan.dbd`
/// with `60 Hz` or `5 minutes` gets those rates, which is why the periodic
/// choices are carried here as a menu INDEX and resolved through
/// [`menu_scan`], instead of being enumerated as variants.
///
/// [`Self::Menu`] therefore covers three cases that only the loaded menu can
/// tell apart — a working rate, a menu entry whose choice string did not parse
/// (C's `papPeriodic[i] == NULL`), and an index outside the menu entirely. The
/// gate that separates them is [`ScanList::of`], exactly as C's `scanAdd` is.
///
/// An out-of-menu index is not a defensive case, it is C's state. `dbPut`
/// stores the `epicsEnum16` the client wrote and only THEN calls `scanAdd`,
/// which tests the index against the menu and, when it is outside,
///
/// ```c
/// /* dbScan.c:248-251 */
/// if (scan < 0 || scan >= nPeriodic + SCAN_1ST_PERIODIC) {
///     recGblRecordError(-1, (void *)precord,
///         "scanAdd detected illegal SCAN value");
/// }
/// ```
///
/// logs and adds the record to **no scan list**. The field itself keeps the
/// written value: verified on the softIoc, `caput REC.SCAN 10` succeeds and
/// `caget REC.SCAN` answers `10`.
///
/// Modelling SCAN as the legal choices alone forced `from_u16` to *erase* an
/// out-of-menu index to `Passive`, which is wrong twice over: the field then
/// read back `0` instead of `10`, and the record became put-processable
/// (C tests `precord->scan == 0` literally in `dbPutField`, dbAccess.c:1263, so
/// an illegal SCAN does NOT process on a `pp(TRUE)` put — a `Passive` one does).
/// Carrying the index removes both.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, Default)]
pub enum ScanType {
    #[default]
    Passive,
    Event,
    IoIntr,
    /// A menu index at or above [`SCAN_1ST_PERIODIC`]. What it means — a rate,
    /// a dead menu entry, or nothing at all — is a property of the loaded
    /// `menuScan`, and [`ScanList::of`] is the single place that decides.
    Menu(u16),
}

impl ScanType {
    /// The stock `menuScan.dbd` rates, by the index each occupies in base's
    /// own menu. These are the port's equivalent of the `menuScan10_second` …
    /// `menuScan_1_second` constants `dbToMenuH` generates from a site's
    /// `menuScan.dbd`, and they carry C's caveat with them: they name an
    /// INDEX, so under a site menu that replaces the rates they name whatever
    /// that menu put there. Nothing in the scan machinery uses them — it reads
    /// [`menu_scan`] — they exist so a `.db` fixture or a caller that means
    /// "base's 1 second rate" can say so.
    pub const SEC10: Self = Self::Menu(3);
    pub const SEC5: Self = Self::Menu(4);
    pub const SEC2: Self = Self::Menu(5);
    pub const SEC1: Self = Self::Menu(6);
    pub const SEC05: Self = Self::Menu(7);
    pub const SEC02: Self = Self::Menu(8);
    pub const SEC01: Self = Self::Menu(9);

    pub fn from_u16(v: u16) -> Self {
        match v {
            0 => Self::Passive,
            1 => Self::Event,
            2 => Self::IoIntr,
            other => Self::Menu(other),
        }
    }

    /// The `DBR_ENUM` index this value is served and stored as.
    pub fn to_u16(self) -> u16 {
        match self {
            Self::Passive => 0,
            Self::Event => 1,
            Self::IoIntr => 2,
            Self::Menu(v) => v,
        }
    }

    /// The scan list this SCAN value names, if it names one — see [`ScanList`].
    pub fn scan_list(self) -> Option<ScanList> {
        ScanList::of(self)
    }

    /// This rate's period, from the loaded menu — C's `papPeriodic[scan -
    /// SCAN_1ST_PERIODIC]->period`. `None` for the three fixed choices and for
    /// any index the menu has no usable rate at.
    pub fn interval(&self) -> Option<std::time::Duration> {
        match self {
            Self::Menu(v) => menu_scan().period_at(*v),
            _ => None,
        }
    }

    /// C's `ind` — the offset into `papPeriodic`, which is also the offset in
    /// the scan-thread priority ladder (`dbScan.c:945`). `None` unless this is
    /// a periodic choice with a usable rate.
    pub fn periodic_index(self) -> Option<usize> {
        match self {
            Self::Menu(v) if menu_scan().period_at(v).is_some() => {
                Some((v - SCAN_1ST_PERIODIC) as usize)
            }
            _ => None,
        }
    }
}

/// The key of a scan list: a `SCAN` value that actually names one.
///
/// C `scanAdd` (`dbScan.c:241-251`) is the sole gate on scan-list membership,
/// and it admits neither of the SCAN values that name no list:
///
/// ```c
/// if (scan == menuScanPassive) return;                       /* no list */
/// if (scan < 0 || scan >= nPeriodic + SCAN_1ST_PERIODIC) {   /* no list */
///     recGblRecordError(-1, precord, "scanAdd detected illegal SCAN value");
/// } else if (scan == menuScanEvent) { ... }
/// ...
/// } else if (scan >= SCAN_1ST_PERIODIC) {
///     periodic_scan_list *ppsl = papPeriodic[scan - SCAN_1ST_PERIODIC];
///     if (ppsl) addToList(precord, &ppsl->scan_list);        /* no list if NULL */
/// }
/// ```
///
/// The scan index is keyed by this type rather than by [`ScanType`], so those
/// cases are refused by construction at every insert site — a `Passive`
/// record, an out-of-menu one, and one whose menu entry has no usable rate
/// cannot be put in a bucket, and no consumer of a bucket has to re-check.
/// Before this type existed the index was keyed by `ScanType` and each site
/// spelled out `!= ScanType::Passive`, which admitted exactly the illegal
/// index C refuses.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct ScanList(ScanType);

impl ScanList {
    /// `None` when this SCAN names no list — `Passive`, an index outside the
    /// loaded `menuScan`, or a menu entry C would leave `papPeriodic[i] ==
    /// NULL` for.
    pub fn of(scan: ScanType) -> Option<Self> {
        match scan {
            ScanType::Passive => None,
            ScanType::Event | ScanType::IoIntr => Some(Self(scan)),
            ScanType::Menu(v) => menu_scan().period_at(v).is_some().then_some(Self(scan)),
        }
    }

    /// The SCAN value this list holds the records of.
    pub fn scan(self) -> ScanType {
        self.0
    }

    /// How many scan lists exist: `Event`, `IoIntr` and one per periodic menu
    /// entry. C sizes its own list array the same way — `nPeriodic +
    /// SCAN_1ST_PERIODIC` (`dbScan.c:250`) over `papPeriodic` plus the two
    /// special lists — and, like C, counts a menu entry whose choice string
    /// did not parse: the slot exists and stays empty, so a bad entry does not
    /// shift the index of the rates after it.
    pub fn count() -> usize {
        menu_scan().n_periodic() + (SCAN_1ST_PERIODIC as usize - 1)
    }

    /// Dense slot in `0..count()`, so a per-list table can be a fixed-length
    /// allocation rather than a map behind its own lock.
    ///
    /// Total by construction: [`Self::of`] is the only constructor and it
    /// refuses `Passive` (menu index 0) and every index the loaded menu has no
    /// rate at, so the wrapped [`ScanType`] is always an index in
    /// `1..count()+1`.
    pub fn slot(self) -> usize {
        self.0.to_u16() as usize - 1
    }

    /// Every scan list, in menu order — the enumeration a per-list table is
    /// built from. Menu entries with no usable rate are absent, so this is
    /// shorter than [`Self::count`] exactly when the site menu carries a
    /// choice string C would reject.
    pub fn all() -> Vec<Self> {
        (1..=menu_scan().n_periodic() as u16 + SCAN_1ST_PERIODIC - 1)
            .filter_map(|v| Self::of(ScanType::from_u16(v)))
            .collect()
    }
}

/// `SSCN` — the simulation-mode scan field (`DBF_MENU`, `menu(menuScan)`).
///
/// The same domain as `SCAN` — it is the same menu — so it is the same type,
/// and an out-of-menu index is carried, not erased. Its dbd default is the
/// out-of-range `65535` (`field(SSCN,DBF_MENU){ menu(menuScan) initial("65535")
/// }`, identical across all 21 records that carry SSCN), which C reads as "not
/// set — keep scanning at SCAN while in simulation mode".
///
/// That sentinel is `65535` and *only* `65535`. Both recGbl helpers test it
/// literally —
///
/// ```c
/// /* recGbl.c: recGblSaveSimm and recGblCheckSimm both open with */
/// if (*psscn == USHRT_MAX) return;
/// ```
///
/// — so an SSCN of, say, `10` is NOT "unset": C performs the swap, lands the
/// illegal `10` in SCAN, and `scanAdd` then leaves the record in no scan list.
/// Treating every illegal index as the sentinel would be a different behaviour
/// from C, so the distinction is kept.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SimModeScan(ScanType);

impl Default for SimModeScan {
    fn default() -> Self {
        Self(ScanType::Menu(Self::DO_NOT_USE))
    }
}

impl SimModeScan {
    /// C's dbd default, and the one index the recGbl simulation helpers bail on.
    pub const DO_NOT_USE: u16 = 65535;

    pub fn from_u16(v: u16) -> Self {
        Self(ScanType::from_u16(v))
    }

    pub fn from_scan(s: ScanType) -> Self {
        Self(s)
    }

    /// The `DBR_ENUM`/wire index — whatever was written, sentinel included.
    pub fn to_u16(self) -> u16 {
        self.0.to_u16()
    }

    /// C's `*psscn == USHRT_MAX` test.
    pub fn is_unset(self) -> bool {
        self.to_u16() == Self::DO_NOT_USE
    }

    /// The scan SSCN swaps SCAN to. `None` only for the unset sentinel — an
    /// illegal-but-not-sentinel index still swaps, exactly as it does in C.
    pub fn scan(self) -> Option<ScanType> {
        (!self.is_unset()).then_some(self.0)
    }
}

impl std::fmt::Display for SimModeScan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::fmt::Display for ScanType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match menu_scan().label_at(self.to_u16()) {
            Some(label) => write!(f, "{label}"),
            // C has no label for an out-of-menu index; `caget` renders the
            // number itself (measured: `caget REC.SCAN` -> `10`).
            None => write!(f, "{}", self.to_u16()),
        }
    }
}

#[cfg(test)]
mod sim_mode_scan_tests {
    use super::*;

    #[test]
    fn default_is_the_65535_sentinel() {
        // C dbd `field(SSCN,DBF_MENU){ initial("65535") }`.
        assert!(SimModeScan::default().is_unset());
        assert_eq!(SimModeScan::default().to_u16(), 65535);
        assert_eq!(SimModeScan::default().scan(), None);
    }

    #[test]
    fn sentinel_round_trips_through_u16() {
        assert!(SimModeScan::from_u16(65535).is_unset());
        assert_eq!(SimModeScan::from_u16(65535).to_u16(), 65535);
    }

    #[test]
    fn valid_menu_indices_map_to_scan_choices() {
        for v in 0u16..=9 {
            assert_eq!(SimModeScan::from_u16(v).scan(), Some(ScanType::from_u16(v)));
            assert_eq!(SimModeScan::from_u16(v).to_u16(), v);
            assert!(!SimModeScan::from_u16(v).is_unset());
        }
    }

    /// The labels this type renders are the loaded `menuScan`'s, so the one
    /// menu converter (which owns every string→menu-index put, see
    /// `tests/menu_common_field_scan_pini.rs`) and this type agree on every
    /// choice — including the `".5 second"` spellings that the deleted
    /// `ScanType::from_str` used to accept a `"0.5 second"` alias for.
    #[test]
    fn labels_match_the_loaded_menu() {
        for (i, label) in menu_scan().choices().iter().enumerate() {
            assert_eq!(ScanType::from_u16(i as u16).to_string(), *label);
        }
    }

    /// CORRECTED — this test used to assert `from_u16(10) == DoNotUse`, i.e.
    /// that an out-of-menu index collapses to the sentinel. It does not. C's
    /// recGbl simulation helpers bail on `*psscn == USHRT_MAX` and on nothing
    /// else, so `10` is an ordinary (illegal) index that still drives the swap;
    /// and the field itself reads back `10`, not `65535`.
    #[test]
    fn an_out_of_menu_index_is_carried_and_is_not_the_sentinel() {
        for v in [10u16, 40000] {
            let s = SimModeScan::from_u16(v);
            assert_eq!(s.to_u16(), v, "the written index is what reads back");
            assert!(!s.is_unset(), "{v} is illegal, but it is not USHRT_MAX");
            assert_eq!(
                s.scan(),
                Some(ScanType::Menu(v)),
                "C swaps it into SCAN; scanAdd then scans nothing"
            );
        }
    }

    /// The SCAN field's own boundary: at the last legal choice, one past it,
    /// and the `-1` a client sends as `65535`.
    #[test]
    fn scan_carries_an_illegal_index_instead_of_erasing_it_to_passive() {
        assert_eq!(ScanType::from_u16(9), ScanType::SEC01);
        assert_eq!(ScanType::from_u16(9).to_u16(), 9);

        // One past the last choice. Measured: `caput REC.SCAN 10` succeeds and
        // `caget REC.SCAN` answers 10 — it does NOT become Passive.
        assert_eq!(ScanType::from_u16(10), ScanType::Menu(10));
        assert_eq!(ScanType::from_u16(10).to_u16(), 10);
        assert_ne!(ScanType::from_u16(10), ScanType::Passive);

        // `caput REC.SCAN -1` reaches the field as the epicsEnum16 65535.
        assert_eq!(ScanType::from_u16(65535), ScanType::Menu(65535));
        assert_eq!(ScanType::from_u16(65535).to_u16(), 65535);

        // An out-of-menu index is in no scan list: not periodic, not I/O Intr.
        assert_eq!(ScanType::Menu(10).interval(), None);
        assert_ne!(ScanType::Menu(10), ScanType::IoIntr);
    }

    /// `scanAdd`'s gate, at its boundaries: the last legal index names a list,
    /// one past it names none, and `Passive` names none.
    #[test]
    fn only_a_menu_choice_other_than_passive_names_a_scan_list() {
        assert_eq!(ScanType::Passive.scan_list(), None);
        for v in 1u16..=9 {
            let scan = ScanType::from_u16(v);
            assert_eq!(
                scan.scan_list().map(ScanList::scan),
                Some(scan),
                "index {v} is a menuScan choice"
            );
        }
        for v in [10u16, 42, 65535] {
            assert_eq!(
                ScanType::from_u16(v).scan_list(),
                None,
                "index {v} is outside menuScan; scanAdd adds the record nowhere"
            );
        }
    }

    /// Every legal index survives the round trip, so nothing above is bought at
    /// the cost of the ordinary path.
    #[test]
    fn every_legal_index_round_trips() {
        for v in 0u16..=9 {
            assert_eq!(ScanType::from_u16(v).to_u16(), v);
        }
    }

    /// `slot()` is a total, collision-free index into a `count()`-sized array —
    /// the property the per-list scan-index table indexes by, without a
    /// bounds check or a fallible lookup.
    ///
    /// Boundary cases, not a narrative: every `of`-admissible SCAN maps into
    /// `0..count()`; `all()` is exactly that set in slot order; and both values
    /// `of` refuses (`Passive`, out-of-menu) yield no slot at all.
    #[test]
    fn every_scan_list_has_a_distinct_slot() {
        let mut seen = vec![false; ScanList::count()];
        for v in 0u16..=9 {
            let Some(list) = ScanType::from_u16(v).scan_list() else {
                assert_eq!(v, 0, "only Passive names no list inside the menu");
                continue;
            };
            let slot = list.slot();
            assert!(slot < ScanList::count(), "slot {slot} out of the table");
            assert!(!seen[slot], "slot {slot} claimed twice");
            seen[slot] = true;
        }
        assert!(seen.iter().all(|s| *s), "every slot must be claimed");

        for (i, list) in ScanList::all().iter().enumerate() {
            assert_eq!(list.slot(), i, "all() must be in slot order");
        }
        assert_eq!(ScanType::Menu(65535).scan_list(), None);
    }

    /// The stock menu's rates, read through the same path a site menu's would
    /// be — the seven periods that used to be `interval()`'s match arms.
    #[test]
    fn the_stock_rates_come_back_through_the_loaded_menu() {
        use std::time::Duration;
        for (scan, period) in [
            (ScanType::SEC10, Duration::from_secs(10)),
            (ScanType::SEC5, Duration::from_secs(5)),
            (ScanType::SEC2, Duration::from_secs(2)),
            (ScanType::SEC1, Duration::from_secs(1)),
            (ScanType::SEC05, Duration::from_millis(500)),
            (ScanType::SEC02, Duration::from_millis(200)),
            (ScanType::SEC01, Duration::from_millis(100)),
        ] {
            assert_eq!(scan.interval(), Some(period), "{scan}");
        }
        assert_eq!(ScanType::Passive.interval(), None);
        assert_eq!(ScanType::Event.interval(), None);
        assert_eq!(ScanType::IoIntr.interval(), None);
    }
}
