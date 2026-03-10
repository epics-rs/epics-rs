/// Simulation mode for image generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum SimMode {
    LinearRamp = 0,
    Peaks = 1,
    Sine = 2,
    OffsetNoise = 3,
}

impl SimMode {
    pub fn from_i32(v: i32) -> Self {
        match v {
            0 => Self::LinearRamp,
            1 => Self::Peaks,
            2 => Self::Sine,
            3 => Self::OffsetNoise,
            _ => Self::LinearRamp,
        }
    }
}

/// Operation to combine sine waves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum SineOperation {
    Add = 0,
    Multiply = 1,
}

impl SineOperation {
    pub fn from_i32(v: i32) -> Self {
        match v {
            1 => Self::Multiply,
            _ => Self::Add,
        }
    }
}

/// Flags indicating which parts of the computation state need to be reset.
#[derive(Debug, Default)]
pub struct DirtyFlags {
    pub reallocate_buffers: bool,
    pub rebuild_background: bool,
    pub reset_ramp: bool,
    pub reset_peak_cache: bool,
    pub reset_sine_state: bool,
}

impl DirtyFlags {
    pub fn any(&self) -> bool {
        self.reallocate_buffers
            || self.rebuild_background
            || self.reset_ramp
            || self.reset_peak_cache
            || self.reset_sine_state
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn set_all(&mut self) {
        self.reallocate_buffers = true;
        self.rebuild_background = true;
        self.reset_ramp = true;
        self.reset_peak_cache = true;
        self.reset_sine_state = true;
    }

    /// Take all flags (return current state and clear).
    pub fn take(&mut self) -> DirtyFlags {
        let taken = DirtyFlags {
            reallocate_buffers: self.reallocate_buffers,
            rebuild_background: self.rebuild_background,
            reset_ramp: self.reset_ramp,
            reset_peak_cache: self.reset_peak_cache,
            reset_sine_state: self.reset_sine_state,
        };
        self.clear();
        taken
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sim_mode_roundtrip() {
        for v in 0..4 {
            let mode = SimMode::from_i32(v);
            assert_eq!(mode as i32, v);
        }
    }

    #[test]
    fn test_sine_operation_roundtrip() {
        assert_eq!(SineOperation::from_i32(0), SineOperation::Add);
        assert_eq!(SineOperation::from_i32(1), SineOperation::Multiply);
    }

    #[test]
    fn test_dirty_flags_default_is_clean() {
        let f = DirtyFlags::default();
        assert!(!f.any());
    }

    #[test]
    fn test_dirty_flags_set_all_and_clear() {
        let mut f = DirtyFlags::default();
        f.set_all();
        assert!(f.any());
        assert!(f.reallocate_buffers);
        assert!(f.rebuild_background);
        f.clear();
        assert!(!f.any());
    }

    #[test]
    fn test_dirty_flags_take() {
        let mut f = DirtyFlags::default();
        f.reallocate_buffers = true;
        f.reset_ramp = true;
        let taken = f.take();
        assert!(taken.reallocate_buffers);
        assert!(taken.reset_ramp);
        assert!(!taken.rebuild_background);
        assert!(!f.any());
    }
}
