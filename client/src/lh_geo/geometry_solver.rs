/// Lighthouse geometry solver using Levenberg-Marquardt optimization.
///
/// Ported from lighthouse_geometry_solver.py
///
/// Finds the poses of base stations and Crazyflie samples given a list of
/// matched samples. The solver is iterative and uses least squares fitting to
/// minimize the distance from the lighthouse sensors to each "ray" measured
/// in the samples.

use std::collections::HashMap;

use levenberg_marquardt::{LeastSquaresProblem, LevenbergMarquardt};
use nalgebra::{DMatrix, DVector, Dyn, Owned, Vector3};

use super::bs_vector::angle_list;
use super::sample::LhCfPoseSampleWrapper;
use super::solution::LighthouseGeometrySolution;
use super::types::Pose;

const LEN_ROT_VEC: usize = 3;
const LEN_POSE: usize = 6;

/// Context data used during solving
struct SolverData {
    /// Number of base stations
    n_bss: usize,
    /// Number of sampled CF poses
    n_cfs: usize,
    /// Number of CF poses in parameter vector (n_cfs - 1, since CF0 is the origin)
    n_cfs_in_params: usize,
    /// Number of sensors per sample
    n_sensors: usize,
    /// Maps BS id -> contiguous index
    bs_id_to_index: HashMap<u8, usize>,
    /// Maps contiguous index -> BS id
    bs_index_to_id: HashMap<usize, u8>,
}

/// The least-squares problem for the LM solver
struct GeometryProblem {
    /// Current parameter vector
    params: DVector<f64>,
    /// Solver context
    defs: SolverData,
    /// Index: angle_pair -> BS index
    idx_pair_to_bs: Vec<usize>,
    /// Index: angle_pair -> CF index
    idx_pair_to_cf: Vec<usize>,
    /// Index: angle_pair -> sensor index
    idx_pair_to_sensor: Vec<usize>,
    /// Target (measured) angles, flat
    target_angles: Vec<f64>,
    /// Sensor positions in CF frame
    sensor_positions: [[f64; 3]; 4],
}

impl LeastSquaresProblem<f64, Dyn, Dyn> for GeometryProblem {
    type ResidualStorage = Owned<f64, Dyn>;
    type JacobianStorage = Owned<f64, Dyn, Dyn>;
    type ParameterStorage = Owned<f64, Dyn>;

    fn set_params(&mut self, x: &DVector<f64>) {
        self.params = x.clone();
    }

    fn params(&self) -> DVector<f64> {
        self.params.clone()
    }

    fn residuals(&self) -> Option<DVector<f64>> {
        Some(self.calc_residual())
    }

    fn jacobian(&self) -> Option<DMatrix<f64>> {
        Some(self.numerical_jacobian())
    }
}

impl GeometryProblem {
    /// Calculate residuals for the current parameters
    fn calc_residual(&self) -> DVector<f64> {
        self.calc_residual_for_params(&self.params)
    }

    fn calc_residual_for_params(&self, params: &DVector<f64>) -> DVector<f64> {
        let (bss, cfs) = params_to_struct(params, &self.defs);

        // CF0 defines the origin: identity rotation (zero rot vec), zero translation
        let mut cfs_full = vec![[0.0; LEN_POSE]];
        cfs_full.extend_from_slice(&cfs);

        let n_pairs = self.idx_pair_to_bs.len();
        let mut residuals = DVector::zeros(n_pairs * 2);

        for pair_i in 0..n_pairs {
            let bs_idx = self.idx_pair_to_bs[pair_i];
            let cf_idx = self.idx_pair_to_cf[pair_i];
            let sensor_idx = self.idx_pair_to_sensor[pair_i];

            let bs_params = &bss[bs_idx];
            let cf_params = &cfs_full[cf_idx];
            let sensor_pos = &self.sensor_positions[sensor_idx];

            // Calculate the estimated angle pair for this sensor
            let angle_pair = calc_angle_pair(bs_params, cf_params, sensor_pos);

            // Target angles for this pair
            let target_h = self.target_angles[pair_i * 2];
            let target_v = self.target_angles[pair_i * 2 + 1];

            let diff_h = angle_pair[0] - target_h;
            let diff_v = angle_pair[1] - target_v;

            // Distance from BS to CF position
            let bs_t = &bs_params[LEN_ROT_VEC..LEN_POSE];
            let cf_t = &cf_params[LEN_ROT_VEC..LEN_POSE];
            let dx = bs_t[0] - cf_t[0];
            let dy = bs_t[1] - cf_t[1];
            let dz = bs_t[2] - cf_t[2];
            let distance = (dx * dx + dy * dy + dz * dz).sqrt();

            residuals[pair_i * 2] = diff_h.tan() * distance;
            residuals[pair_i * 2 + 1] = diff_v.tan() * distance;
        }

        residuals
    }

    /// Compute the Jacobian numerically using forward differences
    fn numerical_jacobian(&self) -> DMatrix<f64> {
        let n_residuals = self.idx_pair_to_bs.len() * 2;
        let n_params = self.params.len();
        let eps = 1e-8;

        let r0 = self.calc_residual_for_params(&self.params);

        // Build sparsity info to skip zero blocks
        let n_bss = self.defs.n_bss;
        let n_pairs = self.idx_pair_to_bs.len();

        let mut jac = DMatrix::zeros(n_residuals, n_params);

        let mut params_perturbed = self.params.clone();

        for p in 0..n_params {
            // Check if this parameter affects any residual
            // BS params: param p is in bs block p / LEN_POSE if p < n_bss * LEN_POSE
            // CF params: otherwise
            let old_val = params_perturbed[p];
            params_perturbed[p] = old_val + eps;

            let r1 = self.calc_residual_for_params(&params_perturbed);

            for pair_i in 0..n_pairs {
                let bs_idx = self.idx_pair_to_bs[pair_i];
                let cf_idx = self.idx_pair_to_cf[pair_i];

                // Which parameter blocks affect this pair?
                let bs_param_start = bs_idx * LEN_POSE;
                let bs_param_end = bs_param_start + LEN_POSE;

                let cf_param_start = if cf_idx > 0 {
                    n_bss * LEN_POSE + (cf_idx - 1) * LEN_POSE
                } else {
                    usize::MAX // CF0 is not in params
                };
                let cf_param_end = if cf_idx > 0 {
                    cf_param_start + LEN_POSE
                } else {
                    0
                };

                let in_bs_block = p >= bs_param_start && p < bs_param_end;
                let in_cf_block = cf_idx > 0 && p >= cf_param_start && p < cf_param_end;

                if in_bs_block || in_cf_block {
                    let row0 = pair_i * 2;
                    jac[(row0, p)] = (r1[row0] - r0[row0]) / eps;
                    jac[(row0 + 1, p)] = (r1[row0 + 1] - r0[row0 + 1]) / eps;
                }
            }

            params_perturbed[p] = old_val;
        }

        jac
    }
}

/// Rodrigues' rotation: rotate `point` by rotation vector `rot_vec` and add `translation`
fn rodrigues_rotate_translate(
    point: &[f64; 3],
    rot_vec: &[f64],
    translation: &[f64],
) -> [f64; 3] {
    let theta = (rot_vec[0] * rot_vec[0] + rot_vec[1] * rot_vec[1] + rot_vec[2] * rot_vec[2])
        .sqrt();

    if theta < 1e-15 {
        // No rotation
        return [
            point[0] + translation[0],
            point[1] + translation[1],
            point[2] + translation[2],
        ];
    }

    let vx = rot_vec[0] / theta;
    let vy = rot_vec[1] / theta;
    let vz = rot_vec[2] / theta;

    let dot = point[0] * vx + point[1] * vy + point[2] * vz;
    let cos_t = theta.cos();
    let sin_t = theta.sin();

    // cross(v, point)
    let cx = vy * point[2] - vz * point[1];
    let cy = vz * point[0] - vx * point[2];
    let cz = vx * point[1] - vy * point[0];

    [
        cos_t * point[0] + sin_t * cx + dot * (1.0 - cos_t) * vx + translation[0],
        cos_t * point[1] + sin_t * cy + dot * (1.0 - cos_t) * vy + translation[1],
        cos_t * point[2] + sin_t * cz + dot * (1.0 - cos_t) * vz + translation[2],
    ]
}

/// Calculate the angle pair (horiz, vert) for a single sensor given BS and CF params
fn calc_angle_pair(bs_params: &[f64; LEN_POSE], cf_params: &[f64; LEN_POSE], sensor_pos: &[f64; 3]) -> [f64; 2] {
    let cf_rot = &cf_params[..LEN_ROT_VEC];
    let cf_trans = &cf_params[LEN_ROT_VEC..LEN_POSE];

    // Transform sensor position from CF local frame to global frame
    let sensor_global = rodrigues_rotate_translate(sensor_pos, cf_rot, cf_trans);

    // Transform from global frame to BS local frame: inverse rotation
    let bs_rot = &bs_params[..LEN_ROT_VEC];
    let bs_trans = &bs_params[LEN_ROT_VEC..LEN_POSE];

    // point_relative = sensor_global - bs_translation
    let rel = [
        sensor_global[0] - bs_trans[0],
        sensor_global[1] - bs_trans[1],
        sensor_global[2] - bs_trans[2],
    ];

    // Inverse rotation = rotation by -rot_vec
    let neg_rot = [-bs_rot[0], -bs_rot[1], -bs_rot[2]];
    let zero_trans = [0.0, 0.0, 0.0];
    let point_bs = rodrigues_rotate_translate(&rel, &neg_rot, &zero_trans);

    // Angles: atan2(y, x) and atan2(z, x)
    [
        point_bs[1].atan2(point_bs[0]),
        point_bs[2].atan2(point_bs[0]),
    ]
}

/// Split flat parameter vector into BS and CF parameter arrays
fn params_to_struct(params: &DVector<f64>, defs: &SolverData) -> (Vec<[f64; LEN_POSE]>, Vec<[f64; LEN_POSE]>) {
    let bs_count = defs.n_bss * LEN_POSE;

    let mut bss = Vec::with_capacity(defs.n_bss);
    for i in 0..defs.n_bss {
        let start = i * LEN_POSE;
        let mut p = [0.0; LEN_POSE];
        for j in 0..LEN_POSE {
            p[j] = params[start + j];
        }
        bss.push(p);
    }

    let mut cfs = Vec::with_capacity(defs.n_cfs_in_params);
    for i in 0..defs.n_cfs_in_params {
        let start = bs_count + i * LEN_POSE;
        let mut p = [0.0; LEN_POSE];
        for j in 0..LEN_POSE {
            p[j] = params[start + j];
        }
        cfs.push(p);
    }

    (bss, cfs)
}

fn pose_to_params(pose: &Pose) -> [f64; LEN_POSE] {
    let rv = pose.rot_vec();
    [
        rv[0],
        rv[1],
        rv[2],
        pose.translation[0],
        pose.translation[1],
        pose.translation[2],
    ]
}

fn params_to_pose(params: &[f64; LEN_POSE]) -> Pose {
    let rv = Vector3::new(params[0], params[1], params[2]);
    let tv = Vector3::new(params[3], params[4], params[5]);
    Pose::from_rot_vec(&rv, &tv)
}

fn create_bs_map(bs_poses: &HashMap<u8, Pose>) -> (HashMap<u8, usize>, HashMap<usize, u8>) {
    let mut id_to_index = HashMap::new();
    let mut index_to_id = HashMap::new();

    let mut sorted_ids: Vec<u8> = bs_poses.keys().copied().collect();
    sorted_ids.sort();

    for (index, &id) in sorted_ids.iter().enumerate() {
        id_to_index.insert(id, index);
        index_to_id.insert(index, id);
    }

    (id_to_index, index_to_id)
}

pub struct LighthouseGeometrySolver;

impl LighthouseGeometrySolver {
    /// Solve for the pose of base stations and CF samples.
    ///
    /// The pose of the CF in sample 0 defines the global reference frame.
    /// Matched_samples is a subset of solution.samples. The solution is
    /// written into the provided LighthouseGeometrySolution.
    pub fn solve(
        matched_samples: &mut [LhCfPoseSampleWrapper],
        sensor_positions: &[[f64; 3]; 4],
        solution: &mut LighthouseGeometrySolution,
    ) {
        let initial_guess_bs_poses = &solution.bs_poses;

        let (bs_id_to_index, bs_index_to_id) = create_bs_map(initial_guess_bs_poses);
        let n_bss = initial_guess_bs_poses.len();
        let n_cfs = matched_samples.len();
        let n_cfs_in_params = n_cfs - 1;
        let n_sensors = sensor_positions.len();

        let defs = SolverData {
            n_bss,
            n_cfs,
            n_cfs_in_params,
            n_sensors,
            bs_id_to_index: bs_id_to_index.clone(),
            bs_index_to_id: bs_index_to_id.clone(),
        };

        // Populate target angles
        let mut target_angles = Vec::new();
        for sample in matched_samples.iter() {
            let mut sorted_bs_ids: Vec<u8> = sample.angles_calibrated().keys().copied().collect();
            sorted_bs_ids.sort();
            for bs_id in sorted_bs_ids {
                let angles = &sample.angles_calibrated()[&bs_id];
                let al = angle_list(angles);
                target_angles.extend_from_slice(&al);
            }
        }

        // Populate index arrays
        let mut idx_pair_to_bs = Vec::new();
        let mut idx_pair_to_cf = Vec::new();
        let mut idx_pair_to_sensor = Vec::new();

        for (cf_i, sample) in matched_samples.iter().enumerate() {
            let mut sorted_bs_ids: Vec<u8> = sample.angles_calibrated().keys().copied().collect();
            sorted_bs_ids.sort();
            for bs_id in sorted_bs_ids {
                let bs_index = bs_id_to_index[&bs_id];
                for sensor_i in 0..n_sensors {
                    idx_pair_to_cf.push(cf_i);
                    idx_pair_to_bs.push(bs_index);
                    idx_pair_to_sensor.push(sensor_i);
                }
            }
        }

        // Populate initial guess parameters
        let mut params_vec = Vec::new();

        // BS params (sorted by index, which corresponds to sorted BS ids)
        let mut bs_params_ordered: Vec<(usize, [f64; LEN_POSE])> = Vec::new();
        for (&bs_id, pose) in initial_guess_bs_poses.iter() {
            let idx = bs_id_to_index[&bs_id];
            bs_params_ordered.push((idx, pose_to_params(pose)));
        }
        bs_params_ordered.sort_by_key(|&(idx, _)| idx);
        for (_, p) in &bs_params_ordered {
            params_vec.extend_from_slice(p);
        }

        // CF params (skip CF0 which defines the origin)
        for sample in matched_samples.iter().skip(1) {
            let cf_pose = sample.pose().cloned().unwrap_or_default();
            let p = pose_to_params(&cf_pose);
            params_vec.extend_from_slice(&p);
        }

        let x0 = DVector::from_vec(params_vec);

        let problem = GeometryProblem {
            params: x0,
            defs,
            idx_pair_to_bs,
            idx_pair_to_cf,
            idx_pair_to_sensor,
            target_angles,
            sensor_positions: *sensor_positions,
        };

        let (result, report) = LevenbergMarquardt::new()
            .with_patience(100)
            .minimize(problem);

        // Extract results
        let (bss, cfs) = params_to_struct(&result.params, &result.defs);

        // Set CF0 pose to identity (origin)
        matched_samples[0].set_pose(Pose::default());

        // Set remaining CF poses
        for i in 0..cfs.len() {
            matched_samples[i + 1].set_pose(params_to_pose(&cfs[i]));
        }

        // Set BS poses
        solution.bs_poses = HashMap::new();
        for (index, bs_params) in bss.iter().enumerate() {
            let bs_id = bs_index_to_id[&index];
            solution.bs_poses.insert(bs_id, params_to_pose(bs_params));
        }

        solution.has_converged = report.termination.was_successful();
    }
}
