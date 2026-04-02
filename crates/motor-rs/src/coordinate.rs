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
pub fn dial_to_raw(dial: f64, mres: f64) -> Result<i32, MotorError> {
    if mres == 0.0 {
        return Err(MotorError::InvalidFieldValue("MRES cannot be zero".into()));
    }
    Ok((dial / mres).round() as i32)
}

/// Convert raw steps to dial position.
/// dial = raw * mres
pub fn raw_to_dial(raw: i32, mres: f64) -> f64 {
    raw as f64 * mres
}

/// Normalize limits so high >= low.
pub fn normalize_limits(a: f64, b: f64) -> (f64, f64) {
    if a >= b { (a, b) } else { (b, a) }
}

/// Convert user limits to dial limits considering direction.
pub fn user_limits_to_dial(hlm: f64, llm: f64, dir: MotorDir, off: f64) -> (f64, f64) {
    let a = user_to_dial(hlm, dir, off);
    let b = user_to_dial(llm, dir, off);
    normalize_limits(a, b)
}

/// Convert dial limits to user limits considering direction.
pub fn dial_limits_to_user(dhlm: f64, dllm: f64, dir: MotorDir, off: f64) -> (f64, f64) {
    let a = dial_to_user(dhlm, dir, off);
    let b = dial_to_user(dllm, dir, off);
    normalize_limits(a, b)
}

/// Check soft limit violation.
/// Returns true if target violates limits. Limits disabled if dhlm == dllm.
pub fn check_soft_limits(dval: f64, dhlm: f64, dllm: f64) -> bool {
    if dhlm == dllm {
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
    foff: FreezeOffset,
    mres: f64,
    set_mode: bool,
    current_dval: f64,
) -> Result<(f64, i32, f64), MotorError> {
    if set_mode {
        // SET mode: redefine offset, no move
        let new_off = calc_offset(val, current_dval, dir);
        let rval = dial_to_raw(current_dval, mres)?;
        Ok((current_dval, rval, new_off))
    } else {
        match foff {
            FreezeOffset::Variable => {
                let dval = user_to_dial(val, dir, off);
                let rval = dial_to_raw(dval, mres)?;
                Ok((dval, rval, off))
            }
            FreezeOffset::Frozen => {
                // FOFF=Frozen: DVAL stays the same, OFF adjusts
                let new_off = calc_offset(val, current_dval, dir);
                let rval = dial_to_raw(current_dval, mres)?;
                Ok((current_dval, rval, new_off))
            }
        }
    }
}

/// Update position cascade when DVAL is written.
/// Returns (new_val, new_rval, new_off).
pub fn cascade_from_dval(
    dval: f64,
    dir: MotorDir,
    off: f64,
    foff: FreezeOffset,
    mres: f64,
    set_mode: bool,
    current_val: f64,
) -> Result<(f64, i32, f64), MotorError> {
    let rval = dial_to_raw(dval, mres)?;
    if set_mode {
        let new_off = calc_offset(current_val, dval, dir);
        Ok((current_val, rval, new_off))
    } else {
        match foff {
            FreezeOffset::Variable => {
                let val = dial_to_user(dval, dir, off);
                Ok((val, rval, off))
            }
            FreezeOffset::Frozen => {
                let new_off = calc_offset(current_val, dval, dir);
                Ok((current_val, rval, new_off))
            }
        }
    }
}

/// Update position cascade when RVAL is written.
/// Returns (new_val, new_dval, new_off).
pub fn cascade_from_rval(
    rval: i32,
    dir: MotorDir,
    off: f64,
    foff: FreezeOffset,
    mres: f64,
    set_mode: bool,
    current_val: f64,
) -> (f64, f64, f64) {
    let dval = raw_to_dial(rval, mres);
    if set_mode {
        let new_off = calc_offset(current_val, dval, dir);
        (current_val, dval, new_off)
    } else {
        match foff {
            FreezeOffset::Variable => {
                let val = dial_to_user(dval, dir, off);
                (val, dval, off)
            }
            FreezeOffset::Frozen => {
                let new_off = calc_offset(current_val, dval, dir);
                (current_val, dval, new_off)
            }
        }
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
    fn test_dial_to_raw_rounding() {
        assert_eq!(dial_to_raw(0.005, 0.01).unwrap(), 1); // 0.5 rounds to 1
        assert_eq!(dial_to_raw(0.004, 0.01).unwrap(), 0); // 0.4 rounds to 0
    }

    #[test]
    fn test_normalize_limits() {
        assert_eq!(normalize_limits(10.0, -10.0), (10.0, -10.0));
        assert_eq!(normalize_limits(-10.0, 10.0), (10.0, -10.0));
        assert_eq!(normalize_limits(5.0, 5.0), (5.0, 5.0));
    }

    #[test]
    fn test_user_limits_to_dial_pos() {
        let (dhlm, dllm) = user_limits_to_dial(100.0, -100.0, MotorDir::Pos, 0.0);
        assert_eq!(dhlm, 100.0);
        assert_eq!(dllm, -100.0);
    }

    #[test]
    fn test_user_limits_to_dial_neg() {
        let (dhlm, dllm) = user_limits_to_dial(100.0, -100.0, MotorDir::Neg, 0.0);
        assert_eq!(dhlm, 100.0);
        assert_eq!(dllm, -100.0);
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
        assert_eq!(val, 10.0); // unchanged
        assert_eq!(rval, 500);
        assert_eq!(off, 5.0); // 10 - 1*5
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
    fn test_dir_neg_swaps_limit_mapping() {
        // When DIR=Neg, user HLM maps to dial LLM and vice versa
        let (dhlm, dllm) = user_limits_to_dial(100.0, -50.0, MotorDir::Neg, 10.0);
        // user 100 -> dial: (100-10)*(-1) = -90
        // user -50 -> dial: (-50-10)*(-1) = 60
        // normalize: (60, -90)
        assert_eq!(dhlm, 60.0);
        assert_eq!(dllm, -90.0);
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
    fn test_frozen_offset_preserves_off() {
        // FOFF=Frozen, writing DVAL changes OFF to keep VAL constant
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
        assert_eq!(val, 30.0); // VAL preserved
        assert_eq!(off, 10.0); // 30 - 1*20
    }
}
