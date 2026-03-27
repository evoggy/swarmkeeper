use serde::{Deserialize, Serialize};
use std::f64::consts::PI;

/// A vector from a base station into space, in the base station reference frame.
/// Represents the intersection of two light planes defined by sweep angles.
/// Ported from lighthouse_bs_vector.py
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct LighthouseBsVector {
    pub lh_v1_horiz_angle: f64,
    pub lh_v1_vert_angle: f64,
}

/// Tilt angle between sweeps in Lighthouse V2
const T: f64 = PI / 6.0;

impl LighthouseBsVector {
    pub fn new(lh_v1_horiz_angle: f64, lh_v1_vert_angle: f64) -> Self {
        LighthouseBsVector {
            lh_v1_horiz_angle,
            lh_v1_vert_angle,
        }
    }

    /// Create from lighthouse V2 angles
    pub fn from_lh2(lh_v2_angle_1: f64, lh_v2_angle_2: f64) -> Self {
        let a1 = lh_v2_angle_1;
        let a2 = lh_v2_angle_2;
        let lh_v1_horiz_angle = (a1 + a2) / 2.0;
        let lh_v1_vert_angle =
            (a2 - a1).sin().atan2(T.tan() * (a1.cos() + a2.cos()));

        LighthouseBsVector {
            lh_v1_horiz_angle,
            lh_v1_vert_angle,
        }
    }

    /// Create from cartesian vector (x, y, z)
    pub fn from_cart(cart: &[f64; 3]) -> Self {
        let lh_v1_horiz_angle = cart[1].atan2(cart[0]);
        let lh_v1_vert_angle = cart[2].atan2(cart[0]);

        LighthouseBsVector {
            lh_v1_horiz_angle,
            lh_v1_vert_angle,
        }
    }

    /// Create from projection point (y, z) on the plane x=1.0
    pub fn from_projection(y: f64, z: f64) -> Self {
        LighthouseBsVector {
            lh_v1_horiz_angle: y.atan(),
            lh_v1_vert_angle: z.atan(),
        }
    }

    /// V1 angle pair (horiz, vert)
    pub fn lh_v1_angle_pair(&self) -> (f64, f64) {
        (self.lh_v1_horiz_angle, self.lh_v1_vert_angle)
    }

    /// Lighthouse V2 first sweep angle
    pub fn lh_v2_angle_1(&self) -> f64 {
        self.lh_v1_horiz_angle + (self.q() * (-T).tan()).asin()
    }

    /// Lighthouse V2 second sweep angle
    pub fn lh_v2_angle_2(&self) -> f64 {
        self.lh_v1_horiz_angle + (self.q() * T.tan()).asin()
    }

    /// Normalized cartesian vector
    pub fn cart(&self) -> [f64; 3] {
        let x = 1.0;
        let y = self.lh_v1_horiz_angle.tan();
        let z = self.lh_v1_vert_angle.tan();
        let norm = (x * x + y * y + z * z).sqrt();
        [x / norm, y / norm, z / norm]
    }

    /// Projection point (y, z) on the plane x=1.0
    pub fn projection(&self) -> [f64; 2] {
        [
            self.lh_v1_horiz_angle.tan(),
            self.lh_v1_vert_angle.tan(),
        ]
    }

    fn q(&self) -> f64 {
        self.lh_v1_vert_angle.tan()
            / (1.0 + self.lh_v1_horiz_angle.tan().powi(2)).sqrt()
    }
}

/// A list of 4 LighthouseBsVector, one for each sensor on the Lighthouse deck.
/// Ported from LighthouseBsVectors in lighthouse_bs_vector.py
pub type LighthouseBsVectors = [LighthouseBsVector; 4];

/// Generate a 4x2 array of projection pairs for all 4 vectors
pub fn projection_pair_list(vectors: &LighthouseBsVectors) -> [[f64; 2]; 4] {
    let mut result = [[0.0; 2]; 4];
    for (i, v) in vectors.iter().enumerate() {
        result[i] = v.projection();
    }
    result
}

/// Generate a flat list of angles: [h0, v0, h1, v1, h2, v2, h3, v3]
pub fn angle_list(vectors: &LighthouseBsVectors) -> [f64; 8] {
    let mut result = [0.0; 8];
    for (i, v) in vectors.iter().enumerate() {
        result[i * 2] = v.lh_v1_horiz_angle;
        result[i * 2 + 1] = v.lh_v1_vert_angle;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_cart_forward() {
        let v = LighthouseBsVector::from_cart(&[1.0, 0.0, 0.0]);
        assert!(v.lh_v1_horiz_angle.abs() < 1e-10);
        assert!(v.lh_v1_vert_angle.abs() < 1e-10);
    }

    #[test]
    fn test_cart_roundtrip() {
        let v = LighthouseBsVector::new(0.3, -0.2);
        let c = v.cart();
        let v2 = LighthouseBsVector::from_cart(&c);
        assert!((v.lh_v1_horiz_angle - v2.lh_v1_horiz_angle).abs() < 1e-10);
        assert!((v.lh_v1_vert_angle - v2.lh_v1_vert_angle).abs() < 1e-10);
    }

    #[test]
    fn test_projection_roundtrip() {
        let v = LighthouseBsVector::new(0.3, -0.2);
        let p = v.projection();
        let v2 = LighthouseBsVector::from_projection(p[0], p[1]);
        assert!((v.lh_v1_horiz_angle - v2.lh_v1_horiz_angle).abs() < 1e-10);
        assert!((v.lh_v1_vert_angle - v2.lh_v1_vert_angle).abs() < 1e-10);
    }

    #[test]
    fn test_v2_roundtrip() {
        let v = LighthouseBsVector::new(0.3, -0.2);
        let a1 = v.lh_v2_angle_1();
        let a2 = v.lh_v2_angle_2();
        let v2 = LighthouseBsVector::from_lh2(a1, a2);
        assert!((v.lh_v1_horiz_angle - v2.lh_v1_horiz_angle).abs() < 1e-10);
        assert!((v.lh_v1_vert_angle - v2.lh_v1_vert_angle).abs() < 1e-10);
    }

    #[test]
    fn test_angle_list() {
        let vectors: LighthouseBsVectors = [
            LighthouseBsVector::new(0.1, 0.2),
            LighthouseBsVector::new(0.3, 0.4),
            LighthouseBsVector::new(0.5, 0.6),
            LighthouseBsVector::new(0.7, 0.8),
        ];
        let al = angle_list(&vectors);
        assert_eq!(al, [0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8]);
    }
}
