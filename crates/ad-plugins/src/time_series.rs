


/// Accumulation mode for time series.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeSeriesMode {
    OneShot,
    RingBuffer,
}

/// Time-series accumulator: stores scalar/1D values from successive arrays.
pub struct TimeSeries {
    pub num_points: usize,
    pub mode: TimeSeriesMode,
    buffer: Vec<f64>,
    write_pos: usize,
    count: usize,
}

impl TimeSeries {
    pub fn new(num_points: usize, mode: TimeSeriesMode) -> Self {
        Self {
            num_points,
            mode,
            buffer: vec![0.0; num_points],
            write_pos: 0,
            count: 0,
        }
    }

    /// Add a value (e.g., mean of an array) to the time series.
    pub fn add_value(&mut self, value: f64) {
        match self.mode {
            TimeSeriesMode::OneShot => {
                if self.write_pos < self.num_points {
                    self.buffer[self.write_pos] = value;
                    self.write_pos += 1;
                    self.count = self.write_pos;
                }
            }
            TimeSeriesMode::RingBuffer => {
                self.buffer[self.write_pos % self.num_points] = value;
                self.write_pos += 1;
                self.count = self.count.max(self.write_pos.min(self.num_points));
            }
        }
    }

    /// Get the accumulated values in order.
    pub fn values(&self) -> Vec<f64> {
        match self.mode {
            TimeSeriesMode::OneShot => self.buffer[..self.count].to_vec(),
            TimeSeriesMode::RingBuffer => {
                if self.write_pos <= self.num_points {
                    self.buffer[..self.count].to_vec()
                } else {
                    let start = self.write_pos % self.num_points;
                    let mut result = Vec::with_capacity(self.num_points);
                    result.extend_from_slice(&self.buffer[start..]);
                    result.extend_from_slice(&self.buffer[..start]);
                    result
                }
            }
        }
    }

    pub fn count(&self) -> usize {
        self.count
    }

    pub fn reset(&mut self) {
        self.buffer.fill(0.0);
        self.write_pos = 0;
        self.count = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_one_shot() {
        let mut ts = TimeSeries::new(5, TimeSeriesMode::OneShot);
        for i in 0..5 {
            ts.add_value(i as f64);
        }
        assert_eq!(ts.count(), 5);
        assert_eq!(ts.values(), vec![0.0, 1.0, 2.0, 3.0, 4.0]);

        // Adding beyond capacity is a no-op
        ts.add_value(99.0);
        assert_eq!(ts.count(), 5);
    }

    #[test]
    fn test_ring_buffer() {
        let mut ts = TimeSeries::new(4, TimeSeriesMode::RingBuffer);
        for i in 0..6 {
            ts.add_value(i as f64);
        }
        assert_eq!(ts.count(), 4);
        // Should contain [2, 3, 4, 5] in order
        assert_eq!(ts.values(), vec![2.0, 3.0, 4.0, 5.0]);
    }

    #[test]
    fn test_ring_buffer_partial() {
        let mut ts = TimeSeries::new(4, TimeSeriesMode::RingBuffer);
        ts.add_value(10.0);
        ts.add_value(20.0);
        assert_eq!(ts.count(), 2);
        assert_eq!(ts.values(), vec![10.0, 20.0]);
    }

    #[test]
    fn test_reset() {
        let mut ts = TimeSeries::new(3, TimeSeriesMode::OneShot);
        ts.add_value(1.0);
        ts.add_value(2.0);
        ts.reset();
        assert_eq!(ts.count(), 0);
        assert!(ts.values().is_empty());
    }
}
