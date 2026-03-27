use nalgebra::{Matrix3, Rotation3, UnitQuaternion, Vector3};
use serde::{Deserialize, Serialize};

/// Full 6-DOF pose (position and orientation) of an object.
/// Ported from lighthouse_types.py Pose class.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pose {
    /// Rotation as a 3x3 matrix
    #[serde(with = "matrix3_serde")]
    pub rot_matrix: Matrix3<f64>,
    /// Translation vector
    pub translation: Vector3<f64>,
}

impl Default for Pose {
    fn default() -> Self {
        Pose {
            rot_matrix: Matrix3::identity(),
            translation: Vector3::zeros(),
        }
    }
}

impl PartialEq for Pose {
    fn eq(&self, other: &Self) -> bool {
        self.rot_matrix == other.rot_matrix && self.translation == other.translation
    }
}

impl Pose {
    pub fn new(rot_matrix: Matrix3<f64>, translation: Vector3<f64>) -> Self {
        Pose {
            rot_matrix,
            translation,
        }
    }

    /// Create a Pose from a rotation vector (axis-angle) and translation vector
    pub fn from_rot_vec(rot_vec: &Vector3<f64>, t_vec: &Vector3<f64>) -> Self {
        let rotation = Rotation3::new(*rot_vec);
        Pose {
            rot_matrix: *rotation.matrix(),
            translation: *t_vec,
        }
    }

    /// Create a Pose from a quaternion (x, y, z, w format matching scipy) and translation vector
    pub fn from_quat(quat: &[f64; 4], t_vec: &Vector3<f64>) -> Self {
        // scipy uses (x, y, z, w) format, nalgebra UnitQuaternion::new_unchecked uses (w, x, y, z)
        let q = UnitQuaternion::from_quaternion(nalgebra::Quaternion::new(
            quat[3], quat[0], quat[1], quat[2],
        ));
        Pose {
            rot_matrix: *q.to_rotation_matrix().matrix(),
            translation: *t_vec,
        }
    }

    /// Create a Pose from roll, pitch, yaw (radians) using XYZ intrinsic Euler angles
    pub fn from_rpy(roll: f64, pitch: f64, yaw: f64, t_vec: &Vector3<f64>) -> Self {
        // XYZ intrinsic = ZYX extrinsic
        // scipy Rotation.from_euler('xyz', [r, p, y]) applies rotations in order X, Y, Z
        let rx = Rotation3::from_axis_angle(&Vector3::x_axis(), roll);
        let ry = Rotation3::from_axis_angle(&Vector3::y_axis(), pitch);
        let rz = Rotation3::from_axis_angle(&Vector3::z_axis(), yaw);
        let r = rz * ry * rx;
        Pose {
            rot_matrix: *r.matrix(),
            translation: *t_vec,
        }
    }

    /// Create a Pose from Crazyflie convention RPY (degrees)
    /// CF convention: from_rpy(roll, -pitch, yaw) in degrees
    pub fn from_cf_rpy(roll: f64, pitch: f64, yaw: f64, t_vec: &Vector3<f64>) -> Self {
        Self::from_rpy(
            roll.to_radians(),
            (-pitch).to_radians(),
            yaw.to_radians(),
            t_vec,
        )
    }

    /// Scale the translation vector
    pub fn scale(&mut self, scale: f64) {
        self.translation *= scale;
    }

    /// Get the rotation as a rotation vector (axis-angle)
    pub fn rot_vec(&self) -> Vector3<f64> {
        let rotation = Rotation3::from_matrix_unchecked(self.rot_matrix);
        let axis_angle = rotation.scaled_axis();
        axis_angle
    }

    /// Get the rotation as a quaternion in (x, y, z, w) format (scipy convention)
    pub fn rot_quat(&self) -> [f64; 4] {
        let rotation = Rotation3::from_matrix_unchecked(self.rot_matrix);
        let q = UnitQuaternion::from_rotation_matrix(&rotation);
        [q.i, q.j, q.k, q.w]
    }

    /// Get Euler angles as (roll, pitch, yaw) in radians, XYZ intrinsic order
    pub fn rot_euler(&self) -> (f64, f64, f64) {
        // Extract euler angles from rotation matrix
        // For XYZ intrinsic (= ZYX extrinsic):
        // R = Rz(yaw) * Ry(pitch) * Rx(roll)
        let r = &self.rot_matrix;

        let pitch = (-r[(2, 0)]).asin();

        let (roll, yaw) = if pitch.cos().abs() > 1e-10 {
            let roll = r[(2, 1)].atan2(r[(2, 2)]);
            let yaw = r[(1, 0)].atan2(r[(0, 0)]);
            (roll, yaw)
        } else {
            // Gimbal lock
            let roll = 0.0;
            let yaw = (-r[(0, 1)]).atan2(r[(1, 1)]);
            (roll, yaw)
        };

        (roll, pitch, yaw)
    }

    /// Get roll, pitch, yaw in CF convention (degrees)
    pub fn rot_cf_rpy(&self) -> (f64, f64, f64) {
        let (roll, pitch, yaw) = self.rot_euler();
        (roll.to_degrees(), -pitch.to_degrees(), yaw.to_degrees())
    }

    /// Rotate and translate a point: transform from local to global reference frame
    /// result = R * point + t
    pub fn rotate_translate(&self, point: &Vector3<f64>) -> Vector3<f64> {
        self.rot_matrix * point + self.translation
    }

    /// Inverse rotate and translate a point: transform from global to local reference frame
    /// result = R^T * (point - t)
    pub fn inv_rotate_translate(&self, point: &Vector3<f64>) -> Vector3<f64> {
        self.rot_matrix.transpose() * (point - self.translation)
    }

    /// Rotate and translate a pose
    /// result.R = self.R * pose.R
    /// result.t = self.R * pose.t + self.t
    pub fn rotate_translate_pose(&self, pose: &Pose) -> Pose {
        let t = self.rot_matrix * pose.translation + self.translation;
        let r = self.rot_matrix * pose.rot_matrix;
        Pose::new(r, t)
    }

    /// Inverse rotate and translate a pose
    /// result.R = self.R^T * pose.R
    /// result.t = self.R^T * (pose.t - self.t)
    pub fn inv_rotate_translate_pose(&self, pose: &Pose) -> Pose {
        let inv_rot = self.rot_matrix.transpose();
        let t = inv_rot * (pose.translation - self.translation);
        let r = inv_rot * pose.rot_matrix;
        Pose::new(r, t)
    }
}

/// Positions of the 4 sensors on the Lighthouse deck
pub struct LhDeck4SensorPositions;

impl LhDeck4SensorPositions {
    const SENSOR_DISTANCE_WIDTH: f64 = 0.015;
    const SENSOR_DISTANCE_LENGTH: f64 = 0.03;

    /// Sensor positions in the Crazyflie reference frame (4 sensors, each [x, y, z])
    pub fn positions() -> [[f64; 3]; 4] {
        let w = Self::SENSOR_DISTANCE_WIDTH;
        let l = Self::SENSOR_DISTANCE_LENGTH;
        [
            [-l / 2.0, w / 2.0, 0.0],
            [-l / 2.0, -w / 2.0, 0.0],
            [l / 2.0, w / 2.0, 0.0],
            [l / 2.0, -w / 2.0, 0.0],
        ]
    }

    /// Diagonal distance between sensors
    pub fn diagonal_distance() -> f64 {
        let l = Self::SENSOR_DISTANCE_LENGTH;
        (l * l + l * l).sqrt()
    }
}

/// Custom serde for Matrix3<f64> as nested arrays
mod matrix3_serde {
    use nalgebra::Matrix3;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S>(matrix: &Matrix3<f64>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let rows: [[f64; 3]; 3] = [
            [matrix[(0, 0)], matrix[(0, 1)], matrix[(0, 2)]],
            [matrix[(1, 0)], matrix[(1, 1)], matrix[(1, 2)]],
            [matrix[(2, 0)], matrix[(2, 1)], matrix[(2, 2)]],
        ];
        rows.serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Matrix3<f64>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let rows: [[f64; 3]; 3] = Deserialize::deserialize(deserializer)?;
        Ok(Matrix3::new(
            rows[0][0], rows[0][1], rows[0][2],
            rows[1][0], rows[1][1], rows[1][2],
            rows[2][0], rows[2][1], rows[2][2],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    #[test]
    fn test_pose_default() {
        let p = Pose::default();
        assert_eq!(p.rot_matrix, Matrix3::identity());
        assert_eq!(p.translation, Vector3::zeros());
    }

    #[test]
    fn test_pose_from_rot_vec_identity() {
        let p = Pose::from_rot_vec(&Vector3::zeros(), &Vector3::new(1.0, 2.0, 3.0));
        assert_eq!(p.translation, Vector3::new(1.0, 2.0, 3.0));
        assert!((p.rot_matrix - Matrix3::identity()).norm() < 1e-10);
    }

    #[test]
    fn test_pose_rot_vec_roundtrip() {
        let rv = Vector3::new(0.1, 0.2, 0.3);
        let t = Vector3::new(1.0, 2.0, 3.0);
        let p = Pose::from_rot_vec(&rv, &t);
        let rv2 = p.rot_vec();
        assert!((rv - rv2).norm() < 1e-10);
    }

    #[test]
    fn test_pose_rotate_translate() {
        let p = Pose::from_rot_vec(&Vector3::zeros(), &Vector3::new(1.0, 0.0, 0.0));
        let pt = Vector3::new(0.0, 0.0, 0.0);
        let result = p.rotate_translate(&pt);
        assert!((result - Vector3::new(1.0, 0.0, 0.0)).norm() < 1e-10);
    }

    #[test]
    fn test_pose_inv_rotate_translate_roundtrip() {
        let rv = Vector3::new(0.5, -0.3, 0.1);
        let t = Vector3::new(1.0, 2.0, 3.0);
        let p = Pose::from_rot_vec(&rv, &t);
        let pt = Vector3::new(4.0, 5.0, 6.0);
        let transformed = p.rotate_translate(&pt);
        let back = p.inv_rotate_translate(&transformed);
        assert!((pt - back).norm() < 1e-10);
    }

    #[test]
    fn test_pose_rotate_translate_pose_roundtrip() {
        let p1 = Pose::from_rot_vec(
            &Vector3::new(0.1, 0.2, 0.3),
            &Vector3::new(1.0, 2.0, 3.0),
        );
        let p2 = Pose::from_rot_vec(
            &Vector3::new(-0.1, 0.5, -0.2),
            &Vector3::new(4.0, 5.0, 6.0),
        );
        let composed = p1.rotate_translate_pose(&p2);
        let back = p1.inv_rotate_translate_pose(&composed);
        assert!((p2.rot_matrix - back.rot_matrix).norm() < 1e-10);
        assert!((p2.translation - back.translation).norm() < 1e-10);
    }

    #[test]
    fn test_pose_rpy_90deg_yaw() {
        let p = Pose::from_rpy(0.0, 0.0, PI / 2.0, &Vector3::zeros());
        let pt = Vector3::new(1.0, 0.0, 0.0);
        let result = p.rotate_translate(&pt);
        assert!((result - Vector3::new(0.0, 1.0, 0.0)).norm() < 1e-10);
    }

    #[test]
    fn test_sensor_positions() {
        let pos = LhDeck4SensorPositions::positions();
        assert_eq!(pos.len(), 4);
        // All on z=0
        for p in &pos {
            assert_eq!(p[2], 0.0);
        }
    }
}
