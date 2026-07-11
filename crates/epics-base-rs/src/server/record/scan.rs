/// Scan types matching EPICS base SCAN field menu.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, Default)]
#[repr(u16)]
pub enum ScanType {
    #[default]
    Passive = 0,
    Event = 1,
    IoIntr = 2,
    Sec10 = 3,
    Sec5 = 4,
    Sec2 = 5,
    Sec1 = 6,
    Sec05 = 7,
    Sec02 = 8,
    Sec01 = 9,
}

impl ScanType {
    pub fn from_u16(v: u16) -> Self {
        match v {
            0 => Self::Passive,
            1 => Self::Event,
            2 => Self::IoIntr,
            3 => Self::Sec10,
            4 => Self::Sec5,
            5 => Self::Sec2,
            6 => Self::Sec1,
            7 => Self::Sec05,
            8 => Self::Sec02,
            9 => Self::Sec01,
            _ => Self::Passive,
        }
    }

    /// Return the interval duration for periodic scan types.
    pub fn interval(&self) -> Option<std::time::Duration> {
        match self {
            Self::Sec10 => Some(std::time::Duration::from_secs(10)),
            Self::Sec5 => Some(std::time::Duration::from_secs(5)),
            Self::Sec2 => Some(std::time::Duration::from_secs(2)),
            Self::Sec1 => Some(std::time::Duration::from_secs(1)),
            Self::Sec05 => Some(std::time::Duration::from_millis(500)),
            Self::Sec02 => Some(std::time::Duration::from_millis(200)),
            Self::Sec01 => Some(std::time::Duration::from_millis(100)),
            _ => None,
        }
    }
}

/// `SSCN` — the simulation-mode scan field (`DBF_MENU`, `menu(menuScan)`).
/// Unlike `SCAN`, its dbd default is the out-of-range sentinel `65535`
/// (`field(SSCN,DBF_MENU){ menu(menuScan) initial("65535") }`, identical
/// across all 21 records that carry SSCN), which C uses to mean "not set —
/// keep scanning at SCAN while in simulation mode". Modeled as its own type
/// so the `65535` sentinel can never leak into `SCAN`, whose value is always
/// a valid menuScan index (0-9).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SimModeScan {
    /// `menuScan(65535)` — "DO_NOT_USE": simulation mode keeps the SCAN rate.
    /// This is the dbd default.
    #[default]
    DoNotUse,
    /// A real menuScan choice, the same domain as `SCAN`.
    Scan(ScanType),
}

impl SimModeScan {
    /// C's sentinel value for "not set" (out of the 0-9 menuScan range).
    pub const DO_NOT_USE: u16 = 65535;

    /// Map a wire/menu index to a state. Only 0-9 are valid menuScan
    /// choices; `65535` (and any other out-of-menu value) is the
    /// "use SCAN" sentinel, matching the C dbd `initial("65535")`.
    pub fn from_u16(v: u16) -> Self {
        match v {
            0..=9 => Self::Scan(ScanType::from_u16(v)),
            _ => Self::DoNotUse,
        }
    }

    /// The `DBR_ENUM`/wire index: a real choice's menuScan index, or the
    /// `65535` sentinel for [`Self::DoNotUse`].
    pub fn to_u16(self) -> u16 {
        match self {
            Self::DoNotUse => Self::DO_NOT_USE,
            Self::Scan(s) => s as u16,
        }
    }
}

impl std::fmt::Display for SimModeScan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // C serves the raw DBR_ENUM index 65535; there is no menuScan
            // label for it, so render the sentinel value.
            Self::DoNotUse => write!(f, "{}", Self::DO_NOT_USE),
            Self::Scan(s) => s.fmt(f),
        }
    }
}

impl std::fmt::Display for ScanType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Passive => write!(f, "Passive"),
            Self::Event => write!(f, "Event"),
            Self::IoIntr => write!(f, "I/O Intr"),
            Self::Sec10 => write!(f, "10 second"),
            Self::Sec5 => write!(f, "5 second"),
            Self::Sec2 => write!(f, "2 second"),
            Self::Sec1 => write!(f, "1 second"),
            Self::Sec05 => write!(f, ".5 second"),
            Self::Sec02 => write!(f, ".2 second"),
            Self::Sec01 => write!(f, ".1 second"),
        }
    }
}

#[cfg(test)]
mod sim_mode_scan_tests {
    use super::*;

    #[test]
    fn default_is_the_65535_sentinel() {
        // C dbd `field(SSCN,DBF_MENU){ initial("65535") }`.
        assert_eq!(SimModeScan::default(), SimModeScan::DoNotUse);
        assert_eq!(SimModeScan::default().to_u16(), 65535);
    }

    #[test]
    fn sentinel_round_trips_through_u16() {
        assert_eq!(SimModeScan::from_u16(65535), SimModeScan::DoNotUse);
        assert_eq!(SimModeScan::DoNotUse.to_u16(), 65535);
    }

    #[test]
    fn valid_menu_indices_map_to_scan_choices() {
        for v in 0u16..=9 {
            assert_eq!(
                SimModeScan::from_u16(v),
                SimModeScan::Scan(ScanType::from_u16(v))
            );
            assert_eq!(SimModeScan::from_u16(v).to_u16(), v);
        }
    }

    /// The labels this type renders are `menuScan`'s, so the one menu
    /// converter (which owns every string→menu-index put, see
    /// `tests/menu_common_field_scan_pini.rs`) and this type agree on every
    /// choice — including the `".5 second"` spellings that the deleted
    /// `ScanType::from_str` used to accept a `"0.5 second"` alias for.
    #[test]
    fn labels_match_the_shared_menu_table() {
        for (i, label) in super::super::menu_choices::MENU_SCAN.iter().enumerate() {
            assert_eq!(ScanType::from_u16(i as u16).to_string(), *label);
        }
    }

    #[test]
    fn out_of_menu_indices_collapse_to_the_sentinel() {
        // 10..65534 are not menuScan choices; like 65535 they mean "use SCAN".
        assert_eq!(SimModeScan::from_u16(10), SimModeScan::DoNotUse);
        assert_eq!(SimModeScan::from_u16(40000), SimModeScan::DoNotUse);
    }
}
