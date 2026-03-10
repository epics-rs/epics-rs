use crate::CalcError;

/// Polynomial interpolation using Neville's algorithm.
/// `order` is the polynomial order (1=linear, 2=quadratic, etc.).
pub fn poly_interp(x_data: &[f64], y_data: &[f64], x: f64, order: usize) -> f64 {
    let n = x_data.len();
    if n == 0 || x_data.len() != y_data.len() {
        return 0.0;
    }
    if n == 1 {
        return y_data[0];
    }

    // Find the closest point to x
    let center = find_closest(x_data, x);
    let npts = (order + 1).min(n);
    let half = npts / 2;
    let start = if center >= half {
        (center - half).min(n - npts)
    } else {
        0
    };

    // Neville's algorithm
    let mut c: Vec<f64> = y_data[start..start + npts].to_vec();
    for j in 1..npts {
        for i in 0..npts - j {
            let xi = x_data[start + i];
            let xij = x_data[start + i + j];
            let denom = xi - xij;
            if denom.abs() < 1e-30 {
                continue;
            }
            c[i] = ((x - xij) * c[i] + (xi - x) * c[i + 1]) / denom;
        }
    }
    c[0]
}

/// Find the fractional index where x would fall in sorted x_data.
/// Returns a float where the integer part is the lower index and
/// the fractional part is the interpolation fraction.
pub fn find_index(x_data: &[f64], x: f64) -> f64 {
    let n = x_data.len();
    if n == 0 {
        return 0.0;
    }
    if n == 1 {
        return 0.0;
    }

    // Determine if ascending or descending
    let ascending = x_data[n - 1] > x_data[0];

    for i in 0..n - 1 {
        let (lo, hi) = if ascending {
            (x_data[i], x_data[i + 1])
        } else {
            (x_data[i + 1], x_data[i])
        };
        if x >= lo && x <= hi {
            let frac = if (x_data[i + 1] - x_data[i]).abs() > 1e-30 {
                (x - x_data[i]) / (x_data[i + 1] - x_data[i])
            } else {
                0.0
            };
            return i as f64 + frac;
        }
    }

    // x is outside the range — clamp
    if ascending {
        if x < x_data[0] { 0.0 } else { (n - 1) as f64 }
    } else {
        if x > x_data[0] { 0.0 } else { (n - 1) as f64 }
    }
}

fn find_closest(x_data: &[f64], x: f64) -> usize {
    let mut best = 0;
    let mut best_dist = (x_data[0] - x).abs();
    for (i, &xi) in x_data.iter().enumerate().skip(1) {
        let dist = (xi - x).abs();
        if dist < best_dist {
            best_dist = dist;
            best = i;
        }
    }
    best
}

/// Interpolation table: stores (x, y) pairs and supports polynomial interpolation.
pub struct InterpTable {
    x: Vec<f64>,
    y: Vec<f64>,
}

impl InterpTable {
    pub fn new() -> Self {
        InterpTable {
            x: Vec::new(),
            y: Vec::new(),
        }
    }

    pub fn add_point(&mut self, x: f64, y: f64) {
        // Insert in sorted order
        let pos = self.x.partition_point(|&xi| xi < x);
        self.x.insert(pos, x);
        self.y.insert(pos, y);
    }

    pub fn clear(&mut self) {
        self.x.clear();
        self.y.clear();
    }

    pub fn len(&self) -> usize {
        self.x.len()
    }

    pub fn is_empty(&self) -> bool {
        self.x.is_empty()
    }

    pub fn interpolate(&self, x: f64, order: usize) -> f64 {
        poly_interp(&self.x, &self.y, x, order)
    }

    pub fn interpolate_array(&self, xs: &[f64], order: usize) -> Vec<f64> {
        xs.iter().map(|&x| self.interpolate(x, order)).collect()
    }
}

impl Default for InterpTable {
    fn default() -> Self {
        Self::new()
    }
}

/// Rolling average with linear fit capability.
pub struct RollingAverage {
    buffer: Vec<f64>,
    capacity: usize,
    count: usize,
    write_pos: usize,
}

impl RollingAverage {
    pub fn new(capacity: usize) -> Self {
        RollingAverage {
            buffer: vec![0.0; capacity],
            capacity,
            count: 0,
            write_pos: 0,
        }
    }

    pub fn push(&mut self, value: f64) {
        self.buffer[self.write_pos] = value;
        self.write_pos = (self.write_pos + 1) % self.capacity;
        if self.count < self.capacity {
            self.count += 1;
        }
    }

    pub fn average(&self) -> f64 {
        if self.count == 0 {
            return 0.0;
        }
        let sum: f64 = self.values().sum();
        sum / self.count as f64
    }

    pub fn linear_fit(&self) -> Result<(f64, f64), CalcError> {
        if self.count < 2 {
            return Err(CalcError::Underflow);
        }
        let vals: Vec<f64> = self.values().collect();
        let n = vals.len() as f64;
        let xbar = (n - 1.0) / 2.0;
        let ybar = vals.iter().sum::<f64>() / n;

        let mut num = 0.0;
        let mut den = 0.0;
        for (i, &y) in vals.iter().enumerate() {
            let x = i as f64 - xbar;
            num += x * (y - ybar);
            den += x * x;
        }

        if den.abs() < 1e-30 {
            return Ok((0.0, ybar));
        }

        let slope = num / den;
        let intercept = ybar - slope * xbar;
        Ok((slope, intercept))
    }

    fn values(&self) -> impl Iterator<Item = f64> + '_ {
        let start = if self.count < self.capacity {
            0
        } else {
            self.write_pos
        };
        (0..self.count).map(move |i| self.buffer[(start + i) % self.capacity])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- poly_interp ---

    #[test]
    fn test_poly_interp_linear() {
        let x = [0.0, 1.0, 2.0, 3.0];
        let y = [0.0, 2.0, 4.0, 6.0];
        assert!((poly_interp(&x, &y, 1.5, 1) - 3.0).abs() < 1e-10);
    }

    #[test]
    fn test_poly_interp_quadratic() {
        let x = [0.0, 1.0, 2.0, 3.0, 4.0];
        let y: Vec<f64> = x.iter().map(|&xi| xi * xi).collect();
        let result = poly_interp(&x, &y, 2.5, 2);
        assert!((result - 6.25).abs() < 0.1, "result={}", result);
    }

    #[test]
    fn test_poly_interp_exact() {
        let x = [0.0, 1.0, 2.0];
        let y = [0.0, 1.0, 4.0];
        assert!((poly_interp(&x, &y, 1.0, 2) - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_poly_interp_empty() {
        assert_eq!(poly_interp(&[], &[], 1.0, 1), 0.0);
    }

    #[test]
    fn test_poly_interp_single() {
        assert_eq!(poly_interp(&[5.0], &[10.0], 1.0, 1), 10.0);
    }

    // --- find_index ---

    #[test]
    fn test_find_index_ascending() {
        let x = [0.0, 1.0, 2.0, 3.0];
        assert!((find_index(&x, 1.5) - 1.5).abs() < 1e-10);
        assert!((find_index(&x, 0.0) - 0.0).abs() < 1e-10);
        assert!((find_index(&x, 3.0) - 3.0).abs() < 1e-10);
    }

    #[test]
    fn test_find_index_below_range() {
        let x = [1.0, 2.0, 3.0];
        assert_eq!(find_index(&x, 0.0), 0.0);
    }

    #[test]
    fn test_find_index_above_range() {
        let x = [1.0, 2.0, 3.0];
        assert_eq!(find_index(&x, 5.0), 2.0);
    }

    #[test]
    fn test_find_index_empty() {
        assert_eq!(find_index(&[], 1.0), 0.0);
    }

    // --- InterpTable ---

    #[test]
    fn test_interp_table_basic() {
        let mut table = InterpTable::new();
        table.add_point(0.0, 0.0);
        table.add_point(1.0, 2.0);
        table.add_point(2.0, 4.0);
        assert_eq!(table.len(), 3);
        let result = table.interpolate(1.5, 1);
        assert!((result - 3.0).abs() < 1e-10);
    }

    #[test]
    fn test_interp_table_clear() {
        let mut table = InterpTable::new();
        table.add_point(0.0, 0.0);
        table.add_point(1.0, 1.0);
        assert_eq!(table.len(), 2);
        table.clear();
        assert!(table.is_empty());
    }

    #[test]
    fn test_interp_table_sorted_insert() {
        let mut table = InterpTable::new();
        table.add_point(3.0, 9.0);
        table.add_point(1.0, 1.0);
        table.add_point(2.0, 4.0);
        assert_eq!(table.x, vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn test_interp_table_array() {
        let mut table = InterpTable::new();
        table.add_point(0.0, 0.0);
        table.add_point(2.0, 4.0);
        let results = table.interpolate_array(&[0.0, 1.0, 2.0], 1);
        assert!((results[0] - 0.0).abs() < 1e-10);
        assert!((results[1] - 2.0).abs() < 1e-10);
        assert!((results[2] - 4.0).abs() < 1e-10);
    }

    // --- RollingAverage ---

    #[test]
    fn test_rolling_average_basic() {
        let mut ra = RollingAverage::new(5);
        ra.push(1.0);
        ra.push(2.0);
        ra.push(3.0);
        assert!((ra.average() - 2.0).abs() < 1e-10);
    }

    #[test]
    fn test_rolling_average_full() {
        let mut ra = RollingAverage::new(3);
        ra.push(1.0);
        ra.push(2.0);
        ra.push(3.0);
        ra.push(4.0); // oldest (1.0) is evicted
        assert!((ra.average() - 3.0).abs() < 1e-10);
    }

    #[test]
    fn test_rolling_average_empty() {
        let ra = RollingAverage::new(5);
        assert_eq!(ra.average(), 0.0);
    }

    #[test]
    fn test_rolling_linear_fit() {
        let mut ra = RollingAverage::new(10);
        // Push linear data: y = 2x + 1
        for i in 0..5 {
            ra.push(2.0 * i as f64 + 1.0);
        }
        let (slope, intercept) = ra.linear_fit().unwrap();
        assert!((slope - 2.0).abs() < 1e-10, "slope={}", slope);
        assert!((intercept - 1.0).abs() < 1e-10, "intercept={}", intercept);
    }

    #[test]
    fn test_rolling_linear_fit_too_few() {
        let mut ra = RollingAverage::new(5);
        ra.push(1.0);
        assert!(ra.linear_fit().is_err());
    }

    #[test]
    fn test_rolling_linear_fit_empty() {
        let ra = RollingAverage::new(5);
        assert!(ra.linear_fit().is_err());
    }

    #[test]
    fn test_rolling_wraparound() {
        let mut ra = RollingAverage::new(3);
        ra.push(10.0);
        ra.push(20.0);
        ra.push(30.0);
        ra.push(40.0);
        ra.push(50.0);
        // Buffer should contain [40, 50, 30] but values should be [30, 40, 50]
        assert!((ra.average() - 40.0).abs() < 1e-10);
    }
}
