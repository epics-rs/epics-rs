use crate::flags::{FreezeOffset, MotorDir, MotorError};

/// Convert dial position to user position.
/// user = dir.sign() * dial + off
pub fn dial_to_user(dial: f64, dir: MotorDir, off: f64) -> f64 {
    dir.sign() * dial + off
}

/// Convert user position to dial position.
/// dial = (user - off) * dir.sign()
pub fn user_to_dial(user: f64, dir: MotorDir, off: f64) -> f64 {
    (user - off) * dir.sign()
}

/// Convert dial position to raw steps.
/// raw = round(dial / mres)
/// 64-bit result to cover high-resolution / long-travel axes
/// (epics-modules/motor #192).
pub fn dial_to_raw(dial: f64, mres: f64) -> Result<i64, MotorError> {
    if mres == 0.0 {
        return Err(MotorError::InvalidFieldValue("MRES cannot be zero".into()));
    }
    let raw = dial / mres;
    // A non-finite result (NaN dial, infinite ratio) would silently cast to
    // 0 or i64::MIN/MAX. Reject it so the caller does not corrupt RVAL.
    if !raw.is_finite() {
        return Err(MotorError::InvalidFieldValue(format!(
            "dial/mres is not finite (dial={dial}, mres={mres})"
        )));
    }
    Ok(raw.round() as i64)
}

/// Convert raw steps to dial position.
/// dial = raw * mres
pub fn raw_to_dial(raw: i64, mres: f64) -> f64 {
    raw as f64 * mres
}

/// Translate dial limits to user limits — C `set_userlimits`
/// (motorRecord.cc:4334-4348). DIR=Neg cross-maps the pair: the user
/// high limit comes from the dial LOW limit (`hlm = -dllm + off`) and
/// vice versa. An inverted pair is carried through as written, never
/// re-ordered — C blocks all moves on it via LVIO instead.
pub fn dial_limits_to_user(dhlm: f64, dllm: f64, dir: MotorDir, off: f64) -> (f64, f64) {
    match dir {
        MotorDir::Pos => (dhlm + off, dllm + off),
        MotorDir::Neg => (-dllm + off, -dhlm + off),
    }
}

/// Check soft limit violation.
/// Returns true if target violates limits.
/// C: limits disabled only when dhlm == dllm == 0.0.
pub fn check_soft_limits(dval: f64, dhlm: f64, dllm: f64) -> bool {
    if dhlm == dllm && dllm == 0.0 {
        return false;
    }
    dval > dhlm || dval < dllm
}

/// Calculate offset from user and dial values.
/// off = user - dir.sign() * dial
pub fn calc_offset(user: f64, dial: f64, dir: MotorDir) -> f64 {
    user - dir.sign() * dial
}

/// Update position cascade when VAL is written.
/// Returns (new_dval, new_rval, new_off).
pub fn cascade_from_val(
    val: f64,
    dir: MotorDir,
    off: f64,
    _foff: FreezeOffset,
    mres: f64,
    set_mode: bool,
    current_dval: f64,
) -> Result<(f64, i64, f64), MotorError> {
    if set_mode {
        // SET mode: redefine offset, no move
        let new_off = calc_offset(val, current_dval, dir);
        let rval = dial_to_raw(current_dval, mres)?;
        Ok((current_dval, rval, new_off))
    } else {
        // C: non-SET mode always cascades VAL -> DVAL normally.
        // FOFF has no effect outside SET mode.
        let dval = user_to_dial(val, dir, off);
        let rval = dial_to_raw(dval, mres)?;
        Ok((dval, rval, off))
    }
}

/// Update position cascade when DVAL is written.
/// Returns (new_val, new_rval, new_off).
pub fn cascade_from_dval(
    dval: f64,
    dir: MotorDir,
    off: f64,
    _foff: FreezeOffset,
    mres: f64,
    set_mode: bool,
    current_val: f64,
) -> Result<(f64, i64, f64), MotorError> {
    let rval = dial_to_raw(dval, mres)?;
    if set_mode {
        let new_off = calc_offset(current_val, dval, dir);
        Ok((current_val, rval, new_off))
    } else {
        // C: non-SET mode always recalculates VAL from DVAL. FOFF has no effect.
        let val = dial_to_user(dval, dir, off);
        Ok((val, rval, off))
    }
}

/// Update position cascade when RVAL is written.
/// Returns (new_val, new_dval, new_off).
pub fn cascade_from_rval(
    rval: i64,
    dir: MotorDir,
    off: f64,
    _foff: FreezeOffset,
    mres: f64,
    set_mode: bool,
    current_val: f64,
) -> (f64, f64, f64) {
    let dval = raw_to_dial(rval, mres);
    if set_mode {
        let new_off = calc_offset(current_val, dval, dir);
        (current_val, dval, new_off)
    } else {
        // C: non-SET mode always recalculates VAL from DVAL. FOFF has no effect.
        let val = dial_to_user(dval, dir, off);
        (val, dval, off)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dial_to_user_pos_no_off() {
        assert_eq!(dial_to_user(10.0, MotorDir::Pos, 0.0), 10.0);
    }

    #[test]
    fn test_dial_to_user_neg_no_off() {
        assert_eq!(dial_to_user(10.0, MotorDir::Neg, 0.0), -10.0);
    }

    #[test]
    fn test_dial_to_user_pos_with_off() {
        assert_eq!(dial_to_user(10.0, MotorDir::Pos, 5.0), 15.0);
    }

    #[test]
    fn test_dial_to_user_neg_with_off() {
        assert_eq!(dial_to_user(10.0, MotorDir::Neg, 5.0), -5.0);
    }

    #[test]
    fn test_user_to_dial_pos_no_off() {
        assert_eq!(user_to_dial(10.0, MotorDir::Pos, 0.0), 10.0);
    }

    #[test]
    fn test_user_to_dial_neg_no_off() {
        assert_eq!(user_to_dial(-10.0, MotorDir::Neg, 0.0), 10.0);
    }

    #[test]
    fn test_user_to_dial_pos_with_off() {
        assert_eq!(user_to_dial(15.0, MotorDir::Pos, 5.0), 10.0);
    }

    #[test]
    fn test_user_to_dial_neg_with_off() {
        assert_eq!(user_to_dial(-5.0, MotorDir::Neg, 5.0), 10.0);
    }

    #[test]
    fn test_dial_to_raw_positive_mres() {
        assert_eq!(dial_to_raw(10.0, 0.01).unwrap(), 1000);
    }

    #[test]
    fn test_dial_to_raw_negative_mres() {
        assert_eq!(dial_to_raw(10.0, -0.01).unwrap(), -1000);
    }

    #[test]
    fn test_dial_to_raw_zero_mres() {
        assert!(dial_to_raw(10.0, 0.0).is_err());
    }

    #[test]
    fn test_dial_to_raw_rejects_non_finite() {
        // NaN/Inf must error, not silently cast to 0 or i64 saturation.
        assert!(dial_to_raw(f64::NAN, 0.01).is_err());
        assert!(dial_to_raw(f64::INFINITY, 0.01).is_err());
        assert!(dial_to_raw(f64::NEG_INFINITY, 0.01).is_err());
    }

    #[test]
    fn test_dial_to_raw_rounding() {
        assert_eq!(dial_to_raw(0.005, 0.01).unwrap(), 1); // 0.5 rounds to 1
        assert_eq!(dial_to_raw(0.004, 0.01).unwrap(), 0); // 0.4 rounds to 0
    }

    // C set_userlimits (motorRecord.cc:4336-4340): DIR=Pos maps each
    // dial limit straight across.
    #[test]
    fn test_dial_limits_to_user_pos() {
        let (hlm, llm) = dial_limits_to_user(100.0, -50.0, MotorDir::Pos, 10.0);
        assert_eq!(hlm, 110.0);
        assert_eq!(llm, -40.0);
    }

    // C set_userlimits (motorRecord.cc:4341-4345): DIR=Neg cross-maps —
    // hlm = -dllm + off, llm = -dhlm + off.
    #[test]
    fn test_dial_limits_to_user_neg_cross_maps() {
        let (hlm, llm) = dial_limits_to_user(100.0, -50.0, MotorDir::Neg, 10.0);
        assert_eq!(hlm, 60.0); // -(-50) + 10
        assert_eq!(llm, -90.0); // -(100) + 10
    }

    // An inverted dial pair stays inverted in user coordinates — C never
    // re-orders limits; LVIO blocks moves instead.
    #[test]
    fn test_dial_limits_to_user_preserves_inverted_pair() {
        let (hlm, llm) = dial_limits_to_user(-20.0, -10.0, MotorDir::Pos, 0.0);
        assert_eq!(hlm, -20.0);
        assert_eq!(llm, -10.0);
        assert!(hlm < llm, "inverted pair must be carried through");
    }

    #[test]
    fn test_check_soft_limits() {
        assert!(check_soft_limits(110.0, 100.0, -100.0));
        assert!(check_soft_limits(-110.0, 100.0, -100.0));
        assert!(!check_soft_limits(50.0, 100.0, -100.0));
        assert!(!check_soft_limits(50.0, 0.0, 0.0)); // disabled
    }

    #[test]
    fn test_cascade_val_normal() {
        let (dval, rval, off) = cascade_from_val(
            10.0,
            MotorDir::Pos,
            0.0,
            FreezeOffset::Variable,
            0.01,
            false,
            0.0,
        )
        .unwrap();
        assert_eq!(dval, 10.0);
        assert_eq!(rval, 1000);
        assert_eq!(off, 0.0);
    }

    #[test]
    fn test_cascade_val_set_mode() {
        let (dval, rval, off) = cascade_from_val(
            20.0,
            MotorDir::Pos,
            0.0,
            FreezeOffset::Variable,
            0.01,
            true,
            10.0,
        )
        .unwrap();
        assert_eq!(dval, 10.0); // unchanged
        assert_eq!(rval, 1000);
        assert_eq!(off, 10.0); // 20 - 1*10
    }

    #[test]
    fn test_cascade_dval_frozen_off() {
        // C: FOFF has no effect in non-SET mode -- VAL recalculated normally
        let (val, rval, off) = cascade_from_dval(
            5.0,
            MotorDir::Pos,
            0.0,
            FreezeOffset::Frozen,
            0.01,
            false,
            10.0,
        )
        .unwrap();
        assert_eq!(val, 5.0); // recalculated: dial_to_user(5.0, Pos, 0.0)
        assert_eq!(rval, 500);
        assert_eq!(off, 0.0); // unchanged
    }

    #[test]
    fn test_cascade_rval_normal() {
        let (val, dval, off) = cascade_from_rval(
            1000,
            MotorDir::Pos,
            0.0,
            FreezeOffset::Variable,
            0.01,
            false,
            0.0,
        );
        assert_eq!(dval, 10.0);
        assert_eq!(val, 10.0);
        assert_eq!(off, 0.0);
    }

    #[test]
    fn test_cascade_rval_neg_dir() {
        let (val, dval, off) = cascade_from_rval(
            1000,
            MotorDir::Neg,
            5.0,
            FreezeOffset::Variable,
            0.01,
            false,
            0.0,
        );
        assert_eq!(dval, 10.0);
        assert_eq!(val, -5.0); // -1*10 + 5
        assert_eq!(off, 5.0);
    }

    #[test]
    fn test_set_mode_val_write_updates_off() {
        // SET=1, writing VAL changes OFF but not DVAL
        let (dval, _rval, off) = cascade_from_val(
            100.0,
            MotorDir::Pos,
            50.0,
            FreezeOffset::Variable,
            0.01,
            true,
            25.0,
        )
        .unwrap();
        assert_eq!(dval, 25.0); // DVAL unchanged (= current_dval)
        assert_eq!(off, 75.0); // 100 - 1*25
    }

    #[test]
    fn test_frozen_offset_non_set_cascades_normally() {
        // C: FOFF has no effect in non-SET mode -- VAL recalculated, OFF unchanged
        let (val, _rval, off) = cascade_from_dval(
            20.0,
            MotorDir::Pos,
            5.0,
            FreezeOffset::Frozen,
            0.01,
            false,
            30.0,
        )
        .unwrap();
        assert_eq!(val, 25.0); // dial_to_user(20.0, Pos, 5.0) = 20+5
        assert_eq!(off, 5.0); // unchanged
    }
}
