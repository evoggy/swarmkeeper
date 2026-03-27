/// Crossing beam calculations for error estimation.
///
/// Calculates the crossing point of two "beams" from two base stations.
/// In reality beams rarely cross, so we use the closest point between them.
/// The distance between the beams serves as an error estimate.
///
/// Ported from lighthouse_utils.py

use nalgebra::Vector3;

use super::bs_vector::{LighthouseBsVector, LighthouseBsVectors};
use super::types::Pose;

pub struct LighthouseCrossingBeam;

impl LighthouseCrossingBeam {
    /// Calculate estimated position and distance for a single sensor from two base stations.
    pub fn position_distance_sensor(
        bs1: &Pose,
        angles_bs1: &LighthouseBsVector,
        bs2: &Pose,
        angles_bs2: &LighthouseBsVector,
    ) -> (Vector3<f64>, f64) {
        let orig_1 = bs1.translation;
        let cart1 = angles_bs1.cart();
        let vec_1 = bs1.rot_matrix * Vector3::new(cart1[0], cart1[1], cart1[2]);

        let orig_2 = bs2.translation;
        let cart2 = angles_bs2.cart();
        let vec_2 = bs2.rot_matrix * Vector3::new(cart2[0], cart2[1], cart2[2]);

        Self::position_distance(&orig_1, &vec_1, &orig_2, &vec_2)
    }

    /// Calculate position only (no distance).
    pub fn position_sensor(
        bs1: &Pose,
        angles_bs1: &LighthouseBsVector,
        bs2: &Pose,
        angles_bs2: &LighthouseBsVector,
    ) -> Vector3<f64> {
        Self::position_distance_sensor(bs1, angles_bs1, bs2, angles_bs2).0
    }

    /// Calculate positions and distances for all 4 sensors.
    pub fn positions_distances(
        bs1: &Pose,
        angles_bs1: &LighthouseBsVectors,
        bs2: &Pose,
        angles_bs2: &LighthouseBsVectors,
    ) -> [(Vector3<f64>, f64); 4] {
        [
            Self::position_distance_sensor(bs1, &angles_bs1[0], bs2, &angles_bs2[0]),
            Self::position_distance_sensor(bs1, &angles_bs1[1], bs2, &angles_bs2[1]),
            Self::position_distance_sensor(bs1, &angles_bs1[2], bs2, &angles_bs2[2]),
            Self::position_distance_sensor(bs1, &angles_bs1[3], bs2, &angles_bs2[3]),
        ]
    }

    /// Calculate average position and max distance across all 4 sensors.
    pub fn position_max_distance(
        bs1: &Pose,
        angles_bs1: &LighthouseBsVectors,
        bs2: &Pose,
        angles_bs2: &LighthouseBsVectors,
    ) -> (Vector3<f64>, f64) {
        let results = Self::positions_distances(bs1, angles_bs1, bs2, angles_bs2);
        let avg_pos = (results[0].0 + results[1].0 + results[2].0 + results[3].0) / 4.0;
        let max_dist = results
            .iter()
            .map(|(_, d)| *d)
            .fold(0.0_f64, f64::max);
        (avg_pos, max_dist)
    }

    /// Calculate max distance across all permutations of base station pairs.
    pub fn max_distance_all_permutations(
        bs_angles: &[(Pose, LighthouseBsVectors)],
    ) -> f64 {
        let n = bs_angles.len();
        let mut max_distance = 0.0_f64;
        for i1 in 0..n {
            for i2 in (i1 + 1)..n {
                let (_, distance) = Self::position_max_distance(
                    &bs_angles[i1].0,
                    &bs_angles[i1].1,
                    &bs_angles[i2].0,
                    &bs_angles[i2].1,
                );
                max_distance = max_distance.max(distance);
            }
        }
        max_distance
    }

    /// Calculate average position and max distance across all BS pair permutations.
    pub fn position_max_distance_all_permutations(
        bs_angles: &[(Pose, LighthouseBsVectors)],
    ) -> (Vector3<f64>, f64) {
        let n = bs_angles.len();
        let mut positions = Vec::new();
        let mut max_distance = 0.0_f64;
        for i1 in 0..n {
            for i2 in (i1 + 1)..n {
                let (position, distance) = Self::position_max_distance(
                    &bs_angles[i1].0,
                    &bs_angles[i1].1,
                    &bs_angles[i2].0,
                    &bs_angles[i2].1,
                );
                positions.push(position);
                max_distance = max_distance.max(distance);
            }
        }
        let avg_pos = if positions.is_empty() {
            Vector3::zeros()
        } else {
            let sum: Vector3<f64> = positions.iter().sum();
            sum / positions.len() as f64
        };
        (avg_pos, max_distance)
    }

    /// Core algorithm: find closest points between two skew lines.
    fn position_distance(
        orig_1: &Vector3<f64>,
        vec_1: &Vector3<f64>,
        orig_2: &Vector3<f64>,
        vec_2: &Vector3<f64>,
    ) -> (Vector3<f64>, f64) {
        let w0 = orig_1 - orig_2;
        let a = vec_1.dot(vec_1);
        let b = vec_1.dot(vec_2);
        let c = vec_2.dot(vec_2);
        let d = vec_1.dot(&w0);
        let e = vec_2.dot(&w0);

        let denom = a * c - b * b;

        if denom.abs() < f64::EPSILON {
            // Lines are parallel
            return (*orig_1, (orig_1 - orig_2).norm());
        }

        // Closest point on line 1 to line 2
        let t1 = (b * e - c * d) / denom;
        let pt1 = orig_1 + t1 * vec_1;

        // Closest point on line 2 to line 1
        let t2 = (a * e - b * d) / denom;
        let pt2 = orig_2 + t2 * vec_2;

        // Midpoint between the two closest points
        let pt = (pt1 + pt2) / 2.0;
        let distance = (pt1 - pt2).norm();

        (pt, distance)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_crossing_beams_intersect() {
        // Two beams that perfectly cross at (1, 0, 0)
        let bs1 = Pose::new(
            nalgebra::Matrix3::identity(),
            Vector3::new(0.0, -1.0, 0.0),
        );
        let bs2 = Pose::new(
            nalgebra::Matrix3::identity(),
            Vector3::new(0.0, 1.0, 0.0),
        );

        // BS1 at (0,-1,0) looking toward (1,0,0): direction = (1,1,0)/sqrt(2)
        let angles_bs1 = LighthouseBsVector::from_cart(&[1.0, 1.0, 0.0]);
        // BS2 at (0,1,0) looking toward (1,0,0): direction = (1,-1,0)/sqrt(2)
        let angles_bs2 = LighthouseBsVector::from_cart(&[1.0, -1.0, 0.0]);

        let (pos, dist) = LighthouseCrossingBeam::position_distance_sensor(
            &bs1, &angles_bs1, &bs2, &angles_bs2,
        );

        assert!(dist < 1e-10, "Distance should be ~0 for crossing beams, got {}", dist);
        assert!(
            (pos - Vector3::new(1.0, 0.0, 0.0)).norm() < 1e-10,
            "Position should be (1,0,0), got {:?}",
            pos
        );
    }
}
