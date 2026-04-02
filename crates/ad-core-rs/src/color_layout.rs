use crate::driver::ColorMode;
use crate::ndarray::NDDimension;

/// Describes the color layout and provides pixel indexing.
#[derive(Debug, Clone)]
pub struct ColorLayout {
    pub color_mode: ColorMode,
    pub size_x: usize,
    pub size_y: usize,
}

impl ColorLayout {
    pub fn num_colors(&self) -> usize {
        match self.color_mode {
            ColorMode::Mono => 1,
            _ => 3,
        }
    }

    pub fn num_elements(&self) -> usize {
        self.size_x * self.size_y * self.num_colors()
    }

    /// Compute the linear index for a pixel at (x, y, channel).
    ///
    /// - Mono: `y * size_x + x`
    /// - RGB1: `(y * size_x + x) * 3 + channel`
    #[inline]
    pub fn index(&self, x: usize, y: usize, channel: usize) -> usize {
        match self.color_mode {
            ColorMode::Mono => y * self.size_x + x,
            ColorMode::RGB1 => (y * self.size_x + x) * 3 + channel,
            _ => unreachable!("v1 only supports Mono and RGB1"),
        }
    }

    /// Create NDDimension array matching D4 contract.
    ///
    /// - Mono: `[size_x, size_y]`
    /// - RGB1: `[3, size_x, size_y]` (colorDim=0)
    pub fn make_dims(&self) -> Vec<NDDimension> {
        match self.color_mode {
            ColorMode::Mono => vec![NDDimension::new(self.size_x), NDDimension::new(self.size_y)],
            ColorMode::RGB1 => vec![
                NDDimension::new(3),
                NDDimension::new(self.size_x),
                NDDimension::new(self.size_y),
            ],
            _ => unreachable!("v1 only supports Mono and RGB1"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ndarray::NDArray;
    use crate::ndarray::NDDataType;

    #[test]
    fn test_mono_index() {
        let layout = ColorLayout {
            color_mode: ColorMode::Mono,
            size_x: 10,
            size_y: 8,
        };
        assert_eq!(layout.index(0, 0, 0), 0);
        assert_eq!(layout.index(5, 0, 0), 5);
        assert_eq!(layout.index(0, 1, 0), 10);
        assert_eq!(layout.index(9, 7, 0), 79);
        assert_eq!(layout.num_elements(), 80);
        assert_eq!(layout.num_colors(), 1);
    }

    #[test]
    fn test_rgb1_index() {
        let layout = ColorLayout {
            color_mode: ColorMode::RGB1,
            size_x: 4,
            size_y: 3,
        };
        // (0,0) R=0, G=1, B=2
        assert_eq!(layout.index(0, 0, 0), 0);
        assert_eq!(layout.index(0, 0, 1), 1);
        assert_eq!(layout.index(0, 0, 2), 2);
        // (1,0) R=3, G=4, B=5
        assert_eq!(layout.index(1, 0, 0), 3);
        // (0,1) R=12, G=13, B=14
        assert_eq!(layout.index(0, 1, 0), 12);
        assert_eq!(layout.num_elements(), 36);
        assert_eq!(layout.num_colors(), 3);
    }

    #[test]
    fn test_mono_make_dims_consistency() {
        let layout = ColorLayout {
            color_mode: ColorMode::Mono,
            size_x: 640,
            size_y: 480,
        };
        let dims = layout.make_dims();
        assert_eq!(dims.len(), 2);
        let arr = NDArray::new(dims, NDDataType::UInt8);
        let info = arr.info();
        assert_eq!(info.x_size, 640);
        assert_eq!(info.y_size, 480);
        assert_eq!(info.color_size, 1);
        assert_eq!(info.num_elements, layout.num_elements());
    }

    #[test]
    fn test_rgb1_make_dims_consistency() {
        let layout = ColorLayout {
            color_mode: ColorMode::RGB1,
            size_x: 320,
            size_y: 240,
        };
        let dims = layout.make_dims();
        assert_eq!(dims.len(), 3);
        let arr = NDArray::new(dims, NDDataType::UInt8);
        let info = arr.info();
        assert_eq!(info.x_size, 320);
        assert_eq!(info.y_size, 240);
        assert_eq!(info.color_size, 3);
        assert_eq!(info.num_elements, layout.num_elements());
    }
}
