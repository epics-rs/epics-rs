use crate::error::{CaError, CaResult};

/// `PINI` — `DBF_MENU`, `menu(menuPini)`
/// (`dbCommon.dbd.pod:169`; `menuPini.dbd.pod:59-65`).
///
/// Six choices, not a boolean. C `iocInit.c:598` (`doRecordPini`) compares
/// `precord->pini` against an **exact** menu index, so each value selects the
/// lifecycle point at which the record is processed:
///
/// ```text
/// menuPiniNO      = 0   never
/// menuPiniYES     = 1   initialProcess()          (iocInit.c:656)
/// menuPiniRUN     = 2   initHookAtIocRun          (iocInit.c:632)
/// menuPiniRUNNING = 3   initHookAfterIocRunning   (iocInit.c:635)
/// menuPiniPAUSE   = 4   initHookAtIocPause        (iocInit.c:638)
/// menuPiniPAUSED  = 5   initHookAfterIocPaused    (iocInit.c:641)
/// ```
///
/// The index is wire-visible (`caget REC.PINI` on a C IOC returns `DBR_ENUM`
/// with the `menuPini` choice strings), so the discriminants MUST match the
/// `.dbd` value order.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Hash)]
#[repr(u16)]
pub enum PiniMode {
    #[default]
    No = 0,
    Yes = 1,
    Run = 2,
    Running = 3,
    Pause = 4,
    Paused = 5,
}

impl PiniMode {
    /// Map a wire/menu index to a choice. C `dbPutStringNum` rejects an
    /// out-of-menu index with `S_db_badChoice`; the port's put paths report
    /// that error before reaching here, so an unknown index here can only come
    /// from a corrupted stored value and collapses to `NO` (the dbd default).
    pub fn from_u16(v: u16) -> Self {
        match v {
            1 => Self::Yes,
            2 => Self::Run,
            3 => Self::Running,
            4 => Self::Pause,
            5 => Self::Paused,
            _ => Self::No,
        }
    }

    /// Resolve a `menuPini` label (`"RUN"`) or a bare menu index (`"2"`),
    /// matching C `dbPutStringNum` (exact label first, then `epicsParseUInt16`).
    pub fn from_str(s: &str) -> CaResult<Self> {
        let s = s.trim();
        match s {
            "NO" => return Ok(Self::No),
            "YES" => return Ok(Self::Yes),
            "RUN" => return Ok(Self::Run),
            "RUNNING" => return Ok(Self::Running),
            "PAUSE" => return Ok(Self::Pause),
            "PAUSED" => return Ok(Self::Paused),
            _ => {}
        }
        match s.parse::<u16>() {
            Ok(v) if v <= 5 => Ok(Self::from_u16(v)),
            _ => Err(CaError::InvalidValue(format!("unknown PINI choice: '{s}'"))),
        }
    }

    /// The `DBR_ENUM` / stored menu index.
    pub fn to_u16(self) -> u16 {
        self as u16
    }
}

impl std::fmt::Display for PiniMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(super::menu_choices::MENU_PINI[*self as usize])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_indices_match_the_dbd_value_order() {
        // menuPini.dbd.pod:59-65 — the index is wire-visible.
        assert_eq!(PiniMode::No.to_u16(), 0);
        assert_eq!(PiniMode::Yes.to_u16(), 1);
        assert_eq!(PiniMode::Run.to_u16(), 2);
        assert_eq!(PiniMode::Running.to_u16(), 3);
        assert_eq!(PiniMode::Pause.to_u16(), 4);
        assert_eq!(PiniMode::Paused.to_u16(), 5);
        for v in 0u16..=5 {
            assert_eq!(PiniMode::from_u16(v).to_u16(), v);
        }
    }

    #[test]
    fn labels_resolve_and_round_trip() {
        assert_eq!(PiniMode::from_str("RUN").unwrap(), PiniMode::Run);
        assert_eq!(PiniMode::from_str("RUNNING").unwrap(), PiniMode::Running);
        assert_eq!(PiniMode::from_str("PAUSED").unwrap(), PiniMode::Paused);
        // Bare index, as C `epicsParseUInt16` accepts.
        assert_eq!(PiniMode::from_str("2").unwrap(), PiniMode::Run);
        assert_eq!(PiniMode::Run.to_string(), "RUN");
        assert_eq!(PiniMode::default(), PiniMode::No);
    }

    #[test]
    fn out_of_menu_text_is_an_error_not_a_silent_no() {
        // C `dbPutStringNum` returns S_db_badChoice; the old `bool` field
        // silently turned every unrecognised string into `pini = false`.
        assert!(PiniMode::from_str("MAYBE").is_err());
        assert!(PiniMode::from_str("6").is_err());
        // "true"/"1" were the pre-fix port-only spellings; only the C-legal
        // numeric index survives.
        assert!(PiniMode::from_str("true").is_err());
        assert_eq!(PiniMode::from_str("1").unwrap(), PiniMode::Yes);
    }
}
