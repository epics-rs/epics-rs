/// 3-element vector.
pub type Vec3 = [f64; 3];
/// 3×3 matrix stored in row-major order.
pub type Mat3 = [[f64; 3]; 3];

/// Numerical singularity threshold.
pub const SMALL: f64 = 1.0e-11;

pub const IDENTITY: Mat3 = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

/// Dot product: a · b.
pub fn dot(a: &Vec3, b: &Vec3) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// Cross product: a × b.
pub fn cross(a: &Vec3, b: &Vec3) -> Vec3 {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

/// Scalar triple product: a · (b × c).
pub fn dotcross(a: &Vec3, b: &Vec3, c: &Vec3) -> f64 {
    a[0] * (b[1] * c[2] - b[2] * c[1])
        + a[1] * (b[2] * c[0] - b[0] * c[2])
        + a[2] * (b[0] * c[1] - b[1] * c[0])
}

/// Matrix × matrix multiplication.
pub fn mult_mat_mat(a: &Mat3, b: &Mat3) -> Mat3 {
    let mut r = [[0.0; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            for k in 0..3 {
                r[i][j] += a[i][k] * b[k][j];
            }
        }
    }
    r
}

/// Matrix × vector multiplication.
pub fn mult_mat_vec(a: &Mat3, v: &Vec3) -> Vec3 {
    [
        a[0][0] * v[0] + a[0][1] * v[1] + a[0][2] * v[2],
        a[1][0] * v[0] + a[1][1] * v[1] + a[1][2] * v[2],
        a[2][0] * v[0] + a[2][1] * v[1] + a[2][2] * v[2],
    ]
}

/// Determinant of a 3×3 matrix.
pub fn determinant(a: &Mat3) -> f64 {
    a[0][0] * (a[1][1] * a[2][2] - a[2][1] * a[1][2])
        + a[0][1] * (a[1][2] * a[2][0] - a[1][0] * a[2][2])
        + a[0][2] * (a[1][0] * a[2][1] - a[1][1] * a[2][0])
}

/// Invert a 3×3 matrix using Cramer's rule.
/// Returns `None` if the matrix is singular (|det| ≤ SMALL).
pub fn invert(a: &Mat3) -> Option<Mat3> {
    let det = determinant(a);
    if det.abs() <= SMALL {
        return None;
    }
    let inv_det = 1.0 / det;
    Some([
        [
            (a[1][1] * a[2][2] - a[1][2] * a[2][1]) * inv_det,
            -(a[0][1] * a[2][2] - a[2][1] * a[0][2]) * inv_det,
            (a[0][1] * a[1][2] - a[1][1] * a[0][2]) * inv_det,
        ],
        [
            -(a[1][0] * a[2][2] - a[1][2] * a[2][0]) * inv_det,
            (a[0][0] * a[2][2] - a[2][0] * a[0][2]) * inv_det,
            -(a[0][0] * a[1][2] - a[1][0] * a[0][2]) * inv_det,
        ],
        [
            (a[1][0] * a[2][1] - a[2][0] * a[1][1]) * inv_det,
            -(a[0][0] * a[2][1] - a[2][0] * a[0][1]) * inv_det,
            (a[0][0] * a[1][1] - a[0][1] * a[1][0]) * inv_det,
        ],
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f64 = 1e-8;

    fn assert_vec_eq(a: &Vec3, b: &Vec3) {
        for i in 0..3 {
            assert!((a[i] - b[i]).abs() < EPS, "index {i}: {} != {}", a[i], b[i]);
        }
    }

    fn assert_mat_eq(a: &Mat3, b: &Mat3) {
        for i in 0..3 {
            for j in 0..3 {
                assert!(
                    (a[i][j] - b[i][j]).abs() < EPS,
                    "[{i}][{j}]: {} != {}",
                    a[i][j],
                    b[i][j]
                );
            }
        }
    }

    // Test data from ~/codes/optics/tests/matrixTest.cpp
    const ZEROES: Vec3 = [0.0, 0.0, 0.0];
    const ONES: Vec3 = [1.0, 1.0, 1.0];
    const V1: Vec3 = [1.0, 2.0, 3.0];
    const V2: Vec3 = [10.0, 71.0, 45.0];
    const V3: Vec3 = [5.0, 10.0, 15.0];

    const MATRIX1: Mat3 = [[1.0, 2.0, 3.0], [4.0, 5.0, 6.0], [7.0, 8.0, 9.0]];
    const MATRIX2: Mat3 = [[-1.0, 10.0, 2.0], [4.0, -1.0, 5.0], [1.0, 6.0, -1.0]];
    const IDENTITY_MAT: Mat3 = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    const ZEROES_MAT: Mat3 = [[0.0; 3]; 3];

    // --- dot tests (16) ---
    #[test]
    fn test_dot() {
        assert!((dot(&[-1., -1., -1.], &[1., 1., 1.]) - (-3.0)).abs() < EPS);
        assert!((dot(&[0., -1., -1.], &[1., 2., 3.]) - (-5.0)).abs() < EPS);
        assert!((dot(&[-1., 0., -1.], &[1., 2., 3.]) - (-4.0)).abs() < EPS);
        assert!((dot(&[-1., -1., 0.], &[1., 2., 3.]) - (-3.0)).abs() < EPS);
        assert!((dot(&[0., 0., -1.], &[1., 2., 3.]) - (-3.0)).abs() < EPS);
        assert!((dot(&[0., -1., 0.], &[1., 2., 3.]) - (-2.0)).abs() < EPS);
        assert!((dot(&[-1., 0., 0.], &[1., 2., 3.]) - (-1.0)).abs() < EPS);
        assert!((dot(&ZEROES, &ZEROES) - 0.0).abs() < EPS);
        assert!((dot(&[1., 0., 0.], &[1., 2., 3.]) - 1.0).abs() < EPS);
        assert!((dot(&[0., 1., 0.], &[1., 2., 3.]) - 2.0).abs() < EPS);
        assert!((dot(&[0., 0., 1.], &[1., 2., 3.]) - 3.0).abs() < EPS);
        assert!((dot(&[1., 1., 0.], &[1., 2., 3.]) - 3.0).abs() < EPS);
        assert!((dot(&[1., 0., 1.], &[1., 2., 3.]) - 4.0).abs() < EPS);
        assert!((dot(&[0., 1., 1.], &[1., 2., 3.]) - 5.0).abs() < EPS);
        assert!((dot(&ONES, &V1) - 6.0).abs() < EPS);
        assert!((dot(&ONES, &ONES) - 3.0).abs() < EPS);
    }

    // --- cross tests (6) ---
    #[test]
    fn test_cross() {
        assert_vec_eq(&cross(&ZEROES, &ZEROES), &ZEROES);
        assert_vec_eq(&cross(&ONES, &ONES), &ZEROES);
        assert_vec_eq(&cross(&V1, &V2), &[-123.0, -15.0, 51.0]);
        assert_vec_eq(&cross(&V1, &V3), &ZEROES); // parallel
        assert_vec_eq(&cross(&V2, &V3), &[615.0, 75.0, -255.0]);
        assert_vec_eq(&cross(&V3, &V2), &[-615.0, -75.0, 255.0]);
    }

    // --- dotcross tests (16) ---
    #[test]
    fn test_dotcross() {
        assert!((dotcross(&ZEROES, &ZEROES, &ZEROES) - 0.0).abs() < EPS);
        assert!((dotcross(&ONES, &ONES, &ONES) - 0.0).abs() < EPS);

        assert!((dotcross(&V1, &V2, &ZEROES) - 0.0).abs() < EPS);
        let expected = dot(&V1, &cross(&V2, &ONES));
        assert!((dotcross(&V1, &V2, &ONES) - expected).abs() < EPS);
        assert!((dotcross(&V1, &V2, &V3) - 0.0).abs() < EPS); // V1 || V3

        assert!((dotcross(&V1, &V3, &ZEROES) - 0.0).abs() < EPS);
        let expected = dot(&V1, &cross(&V3, &ONES));
        assert!((dotcross(&V1, &V3, &ONES) - expected).abs() < EPS);
        assert!((dotcross(&V1, &V3, &V1) - 0.0).abs() < EPS);

        assert!((dotcross(&V2, &V1, &ZEROES) - 0.0).abs() < EPS);
        let expected = dot(&V2, &cross(&V1, &ONES));
        assert!((dotcross(&V2, &V1, &ONES) - expected).abs() < EPS);
        assert!((dotcross(&V2, &V1, &V3) - 0.0).abs() < EPS); // V1 || V3

        assert!((dotcross(&V3, &V2, &ZEROES) - 0.0).abs() < EPS);
        let expected = dot(&V3, &cross(&V2, &ONES));
        assert!((dotcross(&V3, &V2, &ONES) - expected).abs() < EPS);
        let expected = dot(&V3, &cross(&V2, &V1));
        assert!((dotcross(&V3, &V2, &V1) - expected).abs() < EPS);

        assert!((dotcross(&V3, &V1, &ZEROES) - 0.0).abs() < EPS);
        let expected = dot(&V3, &cross(&V1, &ONES));
        assert!((dotcross(&V3, &V1, &ONES) - expected).abs() < EPS);
        let expected = dot(&V3, &cross(&V1, &V2));
        assert!((dotcross(&V3, &V1, &V2) - expected).abs() < EPS);
    }

    // --- determinant tests (3) ---
    #[test]
    fn test_determinant() {
        assert!((determinant(&MATRIX1) - 0.0).abs() < EPS);
        assert!((determinant(&MATRIX2) - 169.0).abs() < EPS);
        assert!((determinant(&IDENTITY_MAT) - 1.0).abs() < EPS);
    }

    // --- invert tests (3) ---
    #[test]
    fn test_invert() {
        // Singular matrix → None
        assert!(invert(&MATRIX1).is_none());

        // Identity inverse = identity
        assert_mat_eq(&invert(&IDENTITY_MAT).unwrap(), &IDENTITY_MAT);

        // matrix2 inverse
        let expected = [
            [-29.0 / 169.0, 22.0 / 169.0, 52.0 / 169.0],
            [9.0 / 169.0, -1.0 / 169.0, 13.0 / 169.0],
            [25.0 / 169.0, 16.0 / 169.0, -39.0 / 169.0],
        ];
        assert_mat_eq(&invert(&MATRIX2).unwrap(), &expected);
    }

    // --- multArrayArray tests (4) ---
    #[test]
    fn test_mult_mat_mat() {
        assert_mat_eq(&mult_mat_mat(&MATRIX1, &ZEROES_MAT), &ZEROES_MAT);
        assert_mat_eq(&mult_mat_mat(&MATRIX1, &IDENTITY_MAT), &MATRIX1);

        let expected = [[10.0, 26.0, 9.0], [22.0, 71.0, 27.0], [34.0, 116.0, 45.0]];
        assert_mat_eq(&mult_mat_mat(&MATRIX1, &MATRIX2), &expected);

        // A * A^-1 = I
        let inv = invert(&MATRIX2).unwrap();
        assert_mat_eq(&mult_mat_mat(&MATRIX2, &inv), &IDENTITY_MAT);
    }

    // --- multArrayVector tests (7) ---
    #[test]
    fn test_mult_mat_vec() {
        assert_vec_eq(&mult_mat_vec(&MATRIX1, &ZEROES), &ZEROES);
        assert_vec_eq(&mult_mat_vec(&MATRIX1, &ONES), &[6.0, 15.0, 24.0]);
        assert_vec_eq(&mult_mat_vec(&MATRIX2, &ONES), &[11.0, 8.0, 6.0]);
        assert_vec_eq(&mult_mat_vec(&MATRIX1, &V1), &[14.0, 32.0, 50.0]);
        assert_vec_eq(&mult_mat_vec(&MATRIX1, &V2), &[287.0, 665.0, 1043.0]);
        assert_vec_eq(&mult_mat_vec(&MATRIX2, &V1), &[25.0, 17.0, 10.0]);
        assert_vec_eq(&mult_mat_vec(&MATRIX2, &V2), &[790.0, 194.0, 391.0]);
    }
}
