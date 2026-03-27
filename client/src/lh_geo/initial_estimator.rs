/// Initial estimator using IPPE
///
/// Makes rough estimates of base station and CF poses using IPPE (analytical solution).
/// Ported from lighthouse_initial_estimator.py

use std::collections::{HashMap, HashSet};

use nalgebra::Vector3;

use super::sample::{BsPairPoses, LhCfPoseSampleStatus, LhCfPoseSampleWrapper};
use super::solution::LighthouseGeometrySolution;
use super::types::Pose;

/// Pair of base station IDs
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BsPairIds(pub u8, pub u8);

pub struct LighthouseInitialEstimator;

const AMBIGUOUS_DETECTION_ERROR: f64 = 0.5;
const ACCEPT_RADIUS: f64 = 0.8;

impl LighthouseInitialEstimator {
    /// Full estimation pipeline: initial IPPE estimation, clustering, and pose propagation.
    pub fn estimate_full(
        matched_samples: &mut [LhCfPoseSampleWrapper],
        solution: &mut LighthouseGeometrySolution,
    ) {
        // Step 1: Find BS-to-BS positions using IPPE clustering
        let bs_positions = Self::find_bs_to_bs_poses(matched_samples);

        // Step 2: Pick correct IPPE solutions per sample, remove ambiguous
        let (bs_poses_ref_cfs, cleaned_indices) =
            Self::angles_to_poses(matched_samples, &bs_positions, solution);

        // Remove non-cleaned samples (keep only cleaned)
        // We need to be careful about modifying in place
        let mut kept_indices: Vec<usize> = cleaned_indices;

        // Build link stats
        Self::build_link_stats(matched_samples, &kept_indices, solution);
        if !solution.progress_is_ok {
            return;
        }

        // Step 3: Estimate BS poses from a reference
        match Self::estimate_bs_poses(&bs_poses_ref_cfs) {
            Ok(bs_poses) => {
                // Step 4: Estimate CF poses
                let cf_poses = Self::estimate_cf_poses(&bs_poses_ref_cfs, &bs_poses);

                // Store results
                for (i, &idx) in kept_indices.iter().enumerate() {
                    if i < cf_poses.len() {
                        matched_samples[idx].set_pose(cf_poses[i].clone());
                    }
                }
                solution.bs_poses = bs_poses;
            }
            Err(e) => {
                solution.progress_is_ok = false;
                solution.general_failure_info = e;
            }
        }

    }

    /// Find BS-to-BS positions via IPPE solution clustering
    fn find_bs_to_bs_poses(
        matched_samples: &[LhCfPoseSampleWrapper],
    ) -> HashMap<BsPairIds, Vector3<f64>> {
        let mut position_permutations: HashMap<BsPairIds, Vec<Vec<Vector3<f64>>>> = HashMap::new();

        for sample in matched_samples {
            Self::add_solution_permutations(
                sample.ippe_solutions(),
                &mut position_permutations,
            );
        }

        Self::find_most_likely_positions(&position_permutations)
    }

    /// Add permutations of BS positions for one sample
    fn add_solution_permutations(
        solutions: &HashMap<u8, BsPairPoses>,
        position_permutations: &mut HashMap<BsPairIds, Vec<Vec<Vector3<f64>>>>,
    ) {
        let mut ids: Vec<u8> = solutions.keys().copied().collect();
        ids.sort();

        for (idx_i, &id_i) in ids.iter().enumerate() {
            let solution_i = &solutions[&id_i];

            for idx_j in (idx_i + 1)..ids.len() {
                let id_j = ids[idx_j];
                let solution_j = &solutions[&id_j];

                // 4 permutations of (sol_i_0 or sol_i_1) x (sol_j_0 or sol_j_1)
                let pose1 = solution_i.0.inv_rotate_translate_pose(&solution_j.0);
                let pose2 = solution_i.0.inv_rotate_translate_pose(&solution_j.1);
                let pose3 = solution_i.1.inv_rotate_translate_pose(&solution_j.0);
                let pose4 = solution_i.1.inv_rotate_translate_pose(&solution_j.1);

                let pair = BsPairIds(id_i, id_j);
                position_permutations
                    .entry(pair)
                    .or_default()
                    .push(vec![
                        pose1.translation,
                        pose2.translation,
                        pose3.translation,
                        pose4.translation,
                    ]);
            }
        }
    }

    /// Find the most likely positions by clustering
    fn find_most_likely_positions(
        position_permutations: &HashMap<BsPairIds, Vec<Vec<Vector3<f64>>>>,
    ) -> HashMap<BsPairIds, Vector3<f64>> {
        let mut result = HashMap::new();

        for (&pair, position_lists) in position_permutations {
            if position_lists.is_empty() {
                continue;
            }

            // Use first sample's positions as reference buckets
            let bucket_refs = &position_lists[0];
            let mut buckets: Vec<Vec<Vector3<f64>>> = vec![vec![]; 4];

            // Map positions to closest reference bucket
            for pos_list in position_lists {
                for pos in pos_list {
                    for (i, ref_pos) in bucket_refs.iter().enumerate() {
                        if (pos - ref_pos).norm() < ACCEPT_RADIUS {
                            buckets[i].push(*pos);
                            break;
                        }
                    }
                }
            }

            // Pick the bucket with the most entries
            let best_bucket = buckets
                .iter()
                .max_by_key(|b| b.len())
                .unwrap();

            if !best_bucket.is_empty() {
                let sum: Vector3<f64> = best_bucket.iter().sum();
                result.insert(pair, sum / best_bucket.len() as f64);
            }
        }

        result
    }

    /// Pick correct IPPE solutions per sample based on BS positions.
    /// Returns (bs_poses_per_sample, cleaned_sample_indices).
    fn angles_to_poses(
        matched_samples: &mut [LhCfPoseSampleWrapper],
        bs_positions: &HashMap<BsPairIds, Vector3<f64>>,
        solution: &mut LighthouseGeometrySolution,
    ) -> (Vec<HashMap<u8, Pose>>, Vec<usize>) {
        let mut result: Vec<HashMap<u8, Pose>> = Vec::new();
        let mut cleaned_indices: Vec<usize> = Vec::new();
        let mut ambiguous_count = 0;

        for (sample_idx, sample) in matched_samples.iter_mut().enumerate() {
            let solutions = sample.ippe_solutions().clone();
            let mut poses: HashMap<u8, Pose> = HashMap::new();

            let mut ids: Vec<u8> = solutions.keys().copied().collect();
            ids.sort();

            if ids.is_empty() {
                continue;
            }

            let first = ids[0];
            let mut is_sample_valid = true;

            for &other in &ids[1..] {
                let pair_ids = BsPairIds(first, other);
                if let Some(expected) = bs_positions.get(&pair_ids) {
                    let (success, pair_poses) =
                        Self::choose_solutions(&solutions[&first], &solutions[&other], expected);
                    if success {
                        poses.insert(pair_ids.0, pair_poses.0);
                        poses.insert(pair_ids.1, pair_poses.1);
                    } else {
                        is_sample_valid = false;
                        sample.status = LhCfPoseSampleStatus::Ambiguous;
                        if sample.is_mandatory {
                            solution.progress_is_ok = false;
                        } else {
                            ambiguous_count += 1;
                            solution.xyz_space_samples_info =
                                format!("{} sample(s) with ambiguities skipped", ambiguous_count);
                        }
                        break;
                    }
                } else {
                    is_sample_valid = false;
                    break;
                }
            }

            if is_sample_valid || sample.is_mandatory {
                result.push(poses);
                cleaned_indices.push(sample_idx);
            }
        }

        (result, cleaned_indices)
    }

    /// Pick the best IPPE solution pair based on expected position
    fn choose_solutions(
        solutions_1: &BsPairPoses,
        solutions_2: &BsPairPoses,
        expected: &Vector3<f64>,
    ) -> (bool, BsPairPoses) {
        let mut min_dist = f64::MAX;
        let mut best = BsPairPoses(Pose::default(), Pose::default());

        for sol1 in solutions_1.as_slice() {
            for sol2 in solutions_2.as_slice() {
                let pose_second_ref_first = sol1.inv_rotate_translate_pose(sol2);
                let dist = (expected - pose_second_ref_first.translation).norm();
                if dist < min_dist {
                    min_dist = dist;
                    best = BsPairPoses(sol1.clone(), sol2.clone());
                }
            }
        }

        (min_dist <= AMBIGUOUS_DETECTION_ERROR, best)
    }

    /// Build link statistics between base stations
    fn build_link_stats(
        matched_samples: &[LhCfPoseSampleWrapper],
        cleaned_indices: &[usize],
        solution: &mut LighthouseGeometrySolution,
    ) {
        for &idx in cleaned_indices {
            let sample = &matched_samples[idx];
            let bs_ids: Vec<u8> = sample.angles_calibrated().keys().copied().collect();

            for &bs1 in &bs_ids {
                *solution.bs_sample_count.entry(bs1).or_insert(0) += 1;
                for &bs2 in &bs_ids {
                    if bs1 != bs2 {
                        *solution
                            .link_count
                            .entry(bs1)
                            .or_default()
                            .entry(bs2)
                            .or_insert(0) += 1;
                    }
                }
            }
        }

        if solution.link_count.len() > 2 {
            solution.link_count_ok_threshold = 2;
        }
    }

    /// Estimate BS poses using graph traversal (onion peeling)
    fn estimate_bs_poses(
        bs_poses_ref_cfs: &[HashMap<u8, Pose>],
    ) -> Result<HashMap<u8, Pose>, String> {
        // Find reference BS from first non-empty sample
        let mut reference_bs_id = None;
        let mut reference_bs_pose = None;

        for bs_poses in bs_poses_ref_cfs {
            if !bs_poses.is_empty() {
                let (&id, pose) = bs_poses.iter().next().unwrap();
                reference_bs_id = Some(id);
                reference_bs_pose = Some(pose.clone());
                break;
            }
        }

        let reference_bs_id = reference_bs_id.ok_or("Too little data, no reference")?;
        let reference_bs_pose = reference_bs_pose.unwrap();

        let mut bs_poses: HashMap<u8, Pose> = HashMap::new();
        bs_poses.insert(reference_bs_id, reference_bs_pose);

        // Find all BS IDs
        let mut all_bs: HashSet<u8> = HashSet::new();
        for poses in bs_poses_ref_cfs {
            all_bs.extend(poses.keys());
        }

        let mut to_find: HashSet<u8> = all_bs.difference(&bs_poses.keys().copied().collect()).copied().collect();
        let mut remaining = to_find.len();

        while remaining > 0 {
            let mut averaging_storage: HashMap<u8, Vec<Pose>> = HashMap::new();

            for bs_poses_in_sample in bs_poses_ref_cfs {
                let known_in_sample: HashSet<u8> = bs_poses
                    .keys()
                    .copied()
                    .collect::<HashSet<u8>>()
                    .intersection(&bs_poses_in_sample.keys().copied().collect())
                    .copied()
                    .collect();
                let unknown_in_sample: HashSet<u8> = to_find
                    .intersection(&bs_poses_in_sample.keys().copied().collect())
                    .copied()
                    .collect();

                if let Some(&known_bs) = known_in_sample.iter().next() {
                    let known_global = &bs_poses[&known_bs];
                    let known_cf = &bs_poses_in_sample[&known_bs];

                    for bs_id in &unknown_in_sample {
                        let unknown_cf = &bs_poses_in_sample[bs_id];
                        let bs_pose = Self::map_pose_to_ref_frame(known_global, known_cf, unknown_cf);
                        averaging_storage.entry(*bs_id).or_default().push(bs_pose);
                    }
                }
            }

            for (bs_id, poses) in &averaging_storage {
                bs_poses.insert(*bs_id, Self::average_poses(poses));
            }

            to_find = all_bs.difference(&bs_poses.keys().copied().collect()).copied().collect();
            if to_find.is_empty() {
                break;
            }
            if to_find.len() == remaining {
                return Err("Can not link positions between all base stations".into());
            }
            remaining = to_find.len();
        }

        Ok(bs_poses)
    }

    /// Average multiple poses (position averaging + quaternion averaging)
    fn average_poses(poses: &[Pose]) -> Pose {
        if poses.is_empty() {
            return Pose::default();
        }
        if poses.len() == 1 {
            return poses[0].clone();
        }

        // Average position
        let sum_pos: Vector3<f64> = poses.iter().map(|p| p.translation).sum();
        let avg_pos = sum_pos / poses.len() as f64;

        // Average quaternion using eigendecomposition
        // Q = [q1, q2, ...], result = eigenvector of Q^T * Q with largest eigenvalue
        let n = poses.len();
        let mut q_mat = nalgebra::DMatrix::zeros(n, 4);
        for (i, pose) in poses.iter().enumerate() {
            let q = pose.rot_quat(); // [x, y, z, w]
            q_mat[(i, 0)] = q[0];
            q_mat[(i, 1)] = q[1];
            q_mat[(i, 2)] = q[2];
            q_mat[(i, 3)] = q[3];
        }

        let qtq = q_mat.transpose() * &q_mat;
        let eig = qtq.symmetric_eigen();

        // Find eigenvector with largest eigenvalue
        let mut max_idx = 0;
        let mut max_val = eig.eigenvalues[0];
        for i in 1..4 {
            if eig.eigenvalues[i] > max_val {
                max_val = eig.eigenvalues[i];
                max_idx = i;
            }
        }

        let avg_quat = [
            eig.eigenvectors[(0, max_idx)],
            eig.eigenvectors[(1, max_idx)],
            eig.eigenvectors[(2, max_idx)],
            eig.eigenvectors[(3, max_idx)],
        ];

        Pose::from_quat(&avg_quat, &avg_pos)
    }

    /// Estimate CF poses from known BS poses
    fn estimate_cf_poses(
        bs_poses_ref_cfs: &[HashMap<u8, Pose>],
        bs_poses: &HashMap<u8, Pose>,
    ) -> Vec<Pose> {
        let mut cf_poses = Vec::new();

        for est_ref_cf in bs_poses_ref_cfs {
            let mut poses = Vec::new();
            for (bs_id, pose_cf) in est_ref_cf {
                if let Some(pose_global) = bs_poses.get(bs_id) {
                    let est_ref_global = Self::map_cf_pos_to_cf_pos(pose_global, pose_cf);
                    poses.push(est_ref_global);
                }
            }
            cf_poses.push(Self::average_poses(&poses));
        }

        cf_poses
    }

    /// Express pose2 in reference system 1
    fn map_pose_to_ref_frame(pose1_ref1: &Pose, pose1_ref2: &Pose, pose2_ref2: &Pose) -> Pose {
        let transform = Self::map_cf_pos_to_cf_pos(pose1_ref1, pose1_ref2);
        let t = transform.rot_matrix * pose2_ref2.translation + transform.translation;
        let r = transform.rot_matrix * pose2_ref2.rot_matrix;
        Pose::new(r, t)
    }

    /// Find the rotation/translation from ref1 to ref2
    fn map_cf_pos_to_cf_pos(pose1_ref1: &Pose, pose1_ref2: &Pose) -> Pose {
        let r_inv_ref2 = pose1_ref2.rot_matrix.transpose();
        let r = pose1_ref1.rot_matrix * r_inv_ref2;
        let t = pose1_ref1.translation - r * pose1_ref2.translation;
        Pose::new(r, t)
    }
}
