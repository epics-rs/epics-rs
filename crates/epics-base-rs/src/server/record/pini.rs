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

    /// The label⇄index mapping this type renders is `menuPini`'s, so the one
    /// menu converter (`menu_choices::resolve_menu_field_string`, which owns
    /// every string→menu-index put — see `tests/menu_field_put_bad_choice.rs`
    /// and `tests/menu_common_field_scan_pini.rs`) and this type agree on
    /// every choice.
    #[test]
    fn labels_match_the_shared_menu_table() {
        for (i, label) in super::super::menu_choices::MENU_PINI.iter().enumerate() {
            assert_eq!(PiniMode::from_u16(i as u16).to_string(), *label);
        }
        assert_eq!(PiniMode::Run.to_string(), "RUN");
        assert_eq!(PiniMode::default(), PiniMode::No);
    }
}
