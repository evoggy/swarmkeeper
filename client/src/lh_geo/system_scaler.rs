/// Lighthouse system scaler - re-scales a system based on various measurements.
///
/// Ported from lighthouse_system_scaler.py

use std::collections::HashMap;

use nalgebra::Vector3;

use super::bs_vector::LighthouseBsVector;
use super::sample::LhCfPoseSampleWrapper;
use super::types::Pose;

pub struct LighthouseSystemScaler;

impl LighthouseSystemScaler {
    /// Scale a system based on a position in the physical world in relation to where it is
    /// in the estimated system geometry. Assumes the system is aligned and simply uses the
    /// distance to the points for scaling.
    ///
    /// * `bs_poses` - Base station poses in the current reference frame
    /// * `cf_poses` - List of CF poses
    /// * `expected` - The real world position to use as reference
    /// * `actual` - The estimated position in the current system geometry
    ///
    /// Returns (scaled BS poses, scaled CF poses, scale factor).
    pub fn scale_fixed_point(
        bs_poses: &HashMap<u8, Pose>,
        cf_poses: &[Pose],
        expected: &Vector3<f64>,
        actual: &Pose,
    ) -> (HashMap<u8, Pose>, Vec<Pose>, f64) {
        let expected_distance = expected.norm();
        let actual_distance = actual.translation.norm();
        let scale_factor = expected_distance / actual_distance;
        Self::scale_system(bs_poses, cf_poses, scale_factor)
    }

    /// Scale a system based on where base station "rays" intersect the lighthouse deck
    /// in relation to sensor positions. Calculates the intersection points for all samples
    /// and scales the system to match the expected distance between sensors on the deck.
    ///
    /// * `bs_poses` - Base station poses in the current reference frame
    /// * `cf_poses` - List of CF poses
    /// * `matched_samples` - List of samples (length must match cf_poses)
    /// * `expected_diagonal` - Expected diagonal sensor distance
    ///
    /// Returns (scaled BS poses, scaled CF poses, scale factor).
    pub fn scale_diagonals(
        bs_poses: &HashMap<u8, Pose>,
        cf_poses: &[Pose],
        matched_samples: &[LhCfPoseSampleWrapper],
        expected_diagonal: f64,
    ) -> (HashMap<u8, Pose>, Vec<Pose>, f64) {
        let estimated_diagonal =
            Self::calculate_mean_diagonal(bs_poses, cf_poses, matched_samples);
        let scale_factor = expected_diagonal / estimated_diagonal;
        Self::scale_system(bs_poses, cf_poses, scale_factor)
    }

    /// Apply a scale factor to all base station and crazyflie poses.
    fn scale_system(
        bs_poses: &HashMap<u8, Pose>,
        cf_poses: &[Pose],
        scale_factor: f64,
    ) -> (HashMap<u8, Pose>, Vec<Pose>, f64) {
        let bs_scaled: HashMap<u8, Pose> = bs_poses
            .iter()
            .map(|(&bs_id, pose)| {
                let mut p = pose.clone();
                p.scale(scale_factor);
                (bs_id, p)
            })
            .collect();

        let cf_scaled: Vec<Pose> = cf_poses
            .iter()
            .map(|pose| {
                let mut p = pose.clone();
                p.scale(scale_factor);
                p
            })
            .collect();

        (bs_scaled, cf_scaled, scale_factor)
    }

    /// Calculate the average diagonal sensor distance based on where the rays
    /// intersect the lighthouse deck.
    fn calculate_mean_diagonal(
        bs_poses: &HashMap<u8, Pose>,
        cf_poses: &[Pose],
        matched_samples: &[LhCfPoseSampleWrapper],
    ) -> f64 {
        let mut diagonals: Vec<f64> = Vec::new();

        for (cf_pose, sample) in cf_poses.iter().zip(matched_samples.iter()) {
            for (&bs_id, vectors) in sample.angles_calibrated() {
                if let Some(bs_pose) = bs_poses.get(&bs_id) {
                    // Diagonal 1: sensor 0 to sensor 3
                    diagonals.push(Self::calc_intersection_distance(
                        &vectors[0],
                        &vectors[3],
                        bs_pose,
                        cf_pose,
                    ));
                    // Diagonal 2: sensor 1 to sensor 2
                    diagonals.push(Self::calc_intersection_distance(
                        &vectors[1],
                        &vectors[2],
                        bs_pose,
                        cf_pose,
                    ));
                }
            }
        }

        let sum: f64 = diagonals.iter().sum();
        sum / diagonals.len() as f64
    }

    /// Calculate distance between intersection points of rays on the plane
    /// defined by the lighthouse deck.
    pub fn calc_intersection_distance(
        vector1: &LighthouseBsVector,
        vector2: &LighthouseBsVector,
        bs_pose: &Pose,
        cf_pose: &Pose,
    ) -> f64 {
        let intersection1 = Self::calc_intersection_point(vector1, bs_pose, cf_pose);
        let intersection2 = Self::calc_intersection_point(vector2, bs_pose, cf_pose);
        (intersection1 - intersection2).norm()
    }

    /// Calculate the intersection point of a line and a plane.
    /// The line is the intersection of the two light planes from a base station,
    /// while the plane is defined by the lighthouse deck of the Crazyflie.
    pub fn calc_intersection_point(
        vector: &LighthouseBsVector,
        bs_pose: &Pose,
        cf_pose: &Pose,
    ) -> Vector3<f64> {
        let plane_base = cf_pose.translation;
        let plane_normal = cf_pose.rot_matrix * Vector3::new(0.0, 0.0, 1.0);

        let line_base = bs_pose.translation;
        let cart = vector.cart();
        let cart_vec = Vector3::new(cart[0], cart[1], cart[2]);
        let line_vector = bs_pose.rot_matrix * cart_vec;

        let dist_on_line =
            (plane_base - line_base).dot(&plane_normal) / line_vector.dot(&plane_normal);

        line_base + line_vector * dist_on_line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_bs_poses() -> HashMap<u8, Pose> {
        let mut poses = HashMap::new();
        poses.insert(
            0,
            Pose::from_rot_vec(&Vector3::zeros(), &Vector3::new(1.0, 0.0, 2.0)),
        );
        poses.insert(
            1,
            Pose::from_rot_vec(&Vector3::zeros(), &Vector3::new(-1.0, 0.0, 2.0)),
        );
        poses
    }

    #[test]
    fn test_scale_fixed_point() {
        let bs_poses = make_bs_poses();
        let cf_poses = vec![Pose::from_rot_vec(
            &Vector3::zeros(),
            &Vector3::new(0.5, 0.0, 0.0),
        )];

        let expected = Vector3::new(1.0, 0.0, 0.0);
        let actual = Pose::from_rot_vec(&Vector3::zeros(), &Vector3::new(0.5, 0.0, 0.0));

        let (bs_scaled, cf_scaled, scale_factor) =
            LighthouseSystemScaler::scale_fixed_point(&bs_poses, &cf_poses, &expected, &actual);

        assert!((scale_factor - 2.0).abs() < 1e-10, "Scale factor should be 2.0");

        // BS0 should be at (2, 0, 4) after scaling
        assert!(
            (bs_scaled[&0].translation - Vector3::new(2.0, 0.0, 4.0)).norm() < 1e-10,
            "BS0 translation should be scaled"
        );

        // CF pose should be at (1, 0, 0) after scaling
        assert!(
            (cf_scaled[0].translation - Vector3::new(1.0, 0.0, 0.0)).norm() < 1e-10,
            "CF translation should be scaled"
        );
    }

    #[test]
    fn test_scale_system_preserves_rotation() {
        let bs_poses = make_bs_poses();
        let cf_poses = vec![Pose::from_rot_vec(
            &Vector3::new(0.1, 0.2, 0.3),
            &Vector3::new(1.0, 2.0, 3.0),
        )];

        let (bs_scaled, cf_scaled, _) =
            LighthouseSystemScaler::scale_system(&bs_poses, &cf_poses, 2.0);

        // Rotation should be preserved
        assert!(
            (cf_scaled[0].rot_matrix - cf_poses[0].rot_matrix).norm() < 1e-10,
            "Rotation should not change"
        );

        // Translation should be doubled
        assert!(
            (cf_scaled[0].translation - cf_poses[0].translation * 2.0).norm() < 1e-10,
            "Translation should be scaled by 2"
        );

        for (&id, pose) in &bs_scaled {
            assert!(
                (pose.rot_matrix - bs_poses[&id].rot_matrix).norm() < 1e-10,
                "BS rotation should not change"
            );
        }
    }

    #[test]
    fn test_calc_intersection_point_simple() {
        // BS at (0, 0, 2) looking down (-Z direction). CF at origin with identity rotation.
        // The CF deck is the z=0 plane.
        let bs_pose = Pose::from_rot_vec(&Vector3::zeros(), &Vector3::new(0.0, 0.0, 2.0));
        let cf_pose = Pose::from_rot_vec(&Vector3::zeros(), &Vector3::zeros());

        // Vector pointing toward (1, 0, -2) from BS frame → hits z=0 at (1, 0, 0)
        let vector = LighthouseBsVector::from_cart(&[1.0, 0.0, -2.0]);

        let point =
            LighthouseSystemScaler::calc_intersection_point(&vector, &bs_pose, &cf_pose);

        // The ray from (0,0,2) toward (1,0,-2) hits z=0 at (1,0,0)
        assert!(
            (point - Vector3::new(1.0, 0.0, 0.0)).norm() < 0.01,
            "Intersection should be near (1,0,0), got {:?}",
            point
        );
    }
}
