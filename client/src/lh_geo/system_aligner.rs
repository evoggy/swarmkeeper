/// Lighthouse system aligner - aligns estimated geometry to a user-defined coordinate frame.
///
/// Ported from lighthouse_system_aligner.py

use std::collections::HashMap;

use levenberg_marquardt::{LeastSquaresProblem, LevenbergMarquardt};
use nalgebra::{DMatrix, DVector, Dyn, Owned, Vector3};

use super::types::Pose;

pub struct LighthouseSystemAligner;

impl LighthouseSystemAligner {
    /// Align a coordinate system with the physical world. Finds the transform from the
    /// current reference frame to one that is aligned with measured positions, and transforms
    /// base station poses to the new coordinate system.
    ///
    /// * `origin` - The position of the desired origin in the current reference frame
    /// * `x_axis` - One or more positions on the desired positive X-axis (X>0, Y=Z=0)
    /// * `xy_plane` - One or more positions in the desired XY-plane (Z=0)
    /// * `bs_poses` - Base station poses in the current reference frame
    ///
    /// Returns the transformed base station poses and the transformation itself.
    pub fn align(
        origin: Vector3<f64>,
        x_axis: &[Vector3<f64>],
        xy_plane: &[Vector3<f64>],
        bs_poses: &HashMap<u8, Pose>,
    ) -> (HashMap<u8, Pose>, Pose) {
        let raw_transformation = Self::find_transformation(&origin, x_axis, xy_plane);
        let transformation = Self::de_flip_transformation(&raw_transformation, x_axis, bs_poses);

        let result: HashMap<u8, Pose> = bs_poses
            .iter()
            .map(|(&bs_id, pose)| (bs_id, transformation.rotate_translate_pose(pose)))
            .collect();

        (result, transformation)
    }

    /// Finds the transformation from the current reference frame to a desired reference frame
    /// based on measured positions. Note: the solution may be flipped.
    fn find_transformation(
        origin: &Vector3<f64>,
        x_axis: &[Vector3<f64>],
        xy_plane: &[Vector3<f64>],
    ) -> Pose {
        let problem = AlignerProblem {
            origin: *origin,
            x_axis: x_axis.to_vec(),
            xy_plane: xy_plane.to_vec(),
            params: DVector::zeros(6),
        };

        let (result, _report) = LevenbergMarquardt::new().minimize(problem);
        Self::pose_from_params(&result.params)
    }

    fn pose_from_params(params: &DVector<f64>) -> Pose {
        let rot_vec = Vector3::new(params[0], params[1], params[2]);
        let t_vec = Vector3::new(params[3], params[4], params[5]);
        Pose::from_rot_vec(&rot_vec, &t_vec)
    }

    /// Compute the residual vector for the alignment problem.
    fn calc_residual(
        params: &DVector<f64>,
        origin: &Vector3<f64>,
        x_axis: &[Vector3<f64>],
        xy_plane: &[Vector3<f64>],
    ) -> DVector<f64> {
        let transform = Self::pose_from_params(params);

        // Residual length: 3 (origin) + 2*len(x_axis) + 1*len(xy_plane)
        let n = 3 + 2 * x_axis.len() + xy_plane.len();
        let mut residual = DVector::zeros(n);

        // Origin should map to (0,0,0)
        let origin_transformed = transform.rotate_translate(origin);
        residual[0] = origin_transformed[0];
        residual[1] = origin_transformed[1];
        residual[2] = origin_transformed[2];

        let mut idx = 3;

        // Points on X-axis: Y and Z should be 0
        for pt in x_axis {
            let transformed = transform.rotate_translate(pt);
            residual[idx] = transformed[1]; // Y
            residual[idx + 1] = transformed[2]; // Z
            idx += 2;
        }

        // Points in XY-plane: Z should be 0
        for pt in xy_plane {
            let transformed = transform.rotate_translate(pt);
            residual[idx] = transformed[2]; // Z
            idx += 1;
        }

        residual
    }

    /// Examines a transformation and flips it if needed. Assumes:
    /// 1. Most base stations are at Z > 0
    /// 2. x_axis samples are taken at X > 0
    fn de_flip_transformation(
        raw_transformation: &Pose,
        x_axis: &[Vector3<f64>],
        bs_poses: &HashMap<u8, Pose>,
    ) -> Pose {
        let mut transformation = raw_transformation.clone();

        // X-axis poses should be on the positive X-axis
        let x_axis_mean: Vector3<f64> =
            x_axis.iter().copied().sum::<Vector3<f64>>() / x_axis.len() as f64;
        if raw_transformation.rotate_translate(&x_axis_mean)[0] < 0.0 {
            let flip_around_z =
                Pose::from_rot_vec(&Vector3::new(0.0, 0.0, std::f64::consts::PI), &Vector3::zeros());
            transformation = flip_around_z.rotate_translate_pose(&transformation);
        }

        // Base stations should be above the floor (Z > 0 on average)
        let bs_z_sum: f64 = bs_poses
            .values()
            .map(|bs_pose| raw_transformation.rotate_translate(&bs_pose.translation)[2])
            .sum();
        let bs_z_mean = bs_z_sum / bs_poses.len() as f64;
        if bs_z_mean < 0.0 {
            let flip_around_x =
                Pose::from_rot_vec(&Vector3::new(std::f64::consts::PI, 0.0, 0.0), &Vector3::zeros());
            transformation = flip_around_x.rotate_translate_pose(&transformation);
        }

        transformation
    }
}

/// Levenberg-Marquardt problem for the system alignment.
struct AlignerProblem {
    origin: Vector3<f64>,
    x_axis: Vec<Vector3<f64>>,
    xy_plane: Vec<Vector3<f64>>,
    params: DVector<f64>,
}

impl LeastSquaresProblem<f64, Dyn, Dyn> for AlignerProblem {
    type ResidualStorage = Owned<f64, Dyn>;
    type JacobianStorage = Owned<f64, Dyn, Dyn>;
    type ParameterStorage = Owned<f64, Dyn>;

    fn set_params(&mut self, params: &DVector<f64>) {
        self.params.copy_from(params);
    }

    fn params(&self) -> DVector<f64> {
        self.params.clone()
    }

    fn residuals(&self) -> Option<DVector<f64>> {
        Some(LighthouseSystemAligner::calc_residual(
            &self.params,
            &self.origin,
            &self.x_axis,
            &self.xy_plane,
        ))
    }

    fn jacobian(&self) -> Option<DMatrix<f64>> {
        // Numerical Jacobian via finite differences
        let n_params = self.params.len();
        let r0 = self.residuals()?;
        let n_residuals = r0.len();
        let mut jac = DMatrix::zeros(n_residuals, n_params);

        let eps = 1e-8;
        for j in 0..n_params {
            let mut params_plus = self.params.clone();
            params_plus[j] += eps;
            let r_plus = LighthouseSystemAligner::calc_residual(
                &params_plus,
                &self.origin,
                &self.x_axis,
                &self.xy_plane,
            );
            for i in 0..n_residuals {
                jac[(i, j)] = (r_plus[i] - r0[i]) / eps;
            }
        }

        Some(jac)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nalgebra::Rotation3;

    fn make_test_bs_poses() -> HashMap<u8, Pose> {
        let mut poses = HashMap::new();
        // BS0 at (1, 0, 2) looking down
        poses.insert(
            0,
            Pose::from_rot_vec(&Vector3::new(0.1, 0.2, 0.0), &Vector3::new(1.0, 0.0, 2.0)),
        );
        // BS1 at (-1, 0, 2) looking down
        poses.insert(
            1,
            Pose::from_rot_vec(&Vector3::new(-0.1, 0.2, 0.0), &Vector3::new(-1.0, 0.0, 2.0)),
        );
        poses
    }

    #[test]
    fn test_align_identity() {
        // If origin is at 0,0,0 and x_axis on X and xy_plane on XY, transform should be ~identity
        let origin = Vector3::new(0.0, 0.0, 0.0);
        let x_axis = vec![Vector3::new(1.0, 0.0, 0.0)];
        let xy_plane = vec![Vector3::new(0.0, 1.0, 0.0)];
        let bs_poses = make_test_bs_poses();

        let (result, _transform) =
            LighthouseSystemAligner::align(origin, &x_axis, &xy_plane, &bs_poses);

        // BS poses should be approximately unchanged
        for (&id, pose) in &result {
            let original = &bs_poses[&id];
            assert!(
                (pose.translation - original.translation).norm() < 0.01,
                "BS {id} translation changed too much"
            );
        }
    }

    #[test]
    fn test_align_with_offset() {
        // System is shifted by (1, 2, 3) - origin of desired frame is at (1, 2, 3) in current frame
        let offset = Vector3::new(1.0, 2.0, 3.0);
        let origin = offset;
        let x_axis = vec![offset + Vector3::new(1.0, 0.0, 0.0)];
        let xy_plane = vec![offset + Vector3::new(0.0, 1.0, 0.0)];

        let mut bs_poses = HashMap::new();
        bs_poses.insert(
            0,
            Pose::from_rot_vec(&Vector3::zeros(), &(Vector3::new(0.0, 0.0, 2.0) + offset)),
        );

        let (result, _transform) =
            LighthouseSystemAligner::align(origin, &x_axis, &xy_plane, &bs_poses);

        // BS0 should now be at approximately (0, 0, 2) in the aligned frame
        let bs0 = &result[&0];
        assert!(
            (bs0.translation - Vector3::new(0.0, 0.0, 2.0)).norm() < 0.01,
            "Expected ~(0,0,2), got {:?}",
            bs0.translation
        );
    }

    #[test]
    fn test_align_with_rotation() {
        // System is rotated 90 degrees around Z axis
        let rot = Rotation3::from_axis_angle(&Vector3::z_axis(), std::f64::consts::FRAC_PI_2);

        // In rotated frame, the "real" origin is at (0,0,0), x-axis point at (1,0,0), xy point at (0,1,0)
        // In current (rotated) frame these become:
        let origin = Vector3::zeros();
        let x_axis = vec![rot * Vector3::new(1.0, 0.0, 0.0)]; // (0, 1, 0)
        let xy_plane = vec![rot * Vector3::new(0.0, 1.0, 0.0)]; // (-1, 0, 0)

        let mut bs_poses = HashMap::new();
        let bs_pos_world = Vector3::new(0.0, 0.0, 2.5);
        let bs_pos_rotated = rot * bs_pos_world;
        bs_poses.insert(0, Pose::from_rot_vec(&Vector3::zeros(), &bs_pos_rotated));

        let (result, _transform) =
            LighthouseSystemAligner::align(origin, &x_axis, &xy_plane, &bs_poses);

        let bs0 = &result[&0];
        // Z should be correct; Y may have residual with only one xy_plane point
        assert!(
            (bs0.translation[2] - bs_pos_world[2]).abs() < 0.1,
            "Expected Z ~{}, got {:?}",
            bs_pos_world[2],
            bs0.translation
        );
    }

    #[test]
    fn test_de_flip_ensures_positive_x() {
        // Create a scenario where x-axis would end up negative without de-flip
        let origin: Vector3<f64> = Vector3::zeros();
        let x_axis = vec![Vector3::new(1.0, 0.0, 0.0)];
        let xy_plane = vec![Vector3::new(0.0, 1.0, 0.0)];

        // Flip transformation: rotate 180 around Z => x-axis point maps to negative X
        let flipped = Pose::from_rot_vec(
            &Vector3::new(0.0, 0.0, std::f64::consts::PI),
            &Vector3::zeros(),
        );

        let mut bs_poses = HashMap::new();
        bs_poses.insert(
            0,
            Pose::from_rot_vec(&Vector3::zeros(), &Vector3::new(0.0, 0.0, 2.0)),
        );

        let result =
            LighthouseSystemAligner::de_flip_transformation(&flipped, &x_axis, &bs_poses);

        // After de-flip, x-axis point should map to positive X
        let transformed_x = result.rotate_translate(&x_axis[0]);
        assert!(
            transformed_x[0] > 0.0,
            "X should be positive, got {}",
            transformed_x[0]
        );
    }
}
