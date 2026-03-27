/// Estimation manager - orchestrates the full geometry estimation pipeline
///
/// Ported from LhGeoEstimationManager in lighthouse_geo_estimation_manager.py

use std::collections::HashMap;

use nalgebra::Vector3;

use super::bs_vector::LighthouseBsVectors;
use super::container::LhGeoInputContainerData;
use super::crossing_beam::LighthouseCrossingBeam;
use super::geometry_solver::LighthouseGeometrySolver;
use super::initial_estimator::LighthouseInitialEstimator;
use super::sample::{LhCfPoseSampleStatus, LhCfPoseSampleType, LhCfPoseSampleWrapper};
use super::solution::{ErrorStats, LighthouseGeometrySolution};
use super::system_aligner::LighthouseSystemAligner;
use super::system_scaler::LighthouseSystemScaler;
use super::types::Pose;

pub struct LhGeoEstimationManager;

const REFERENCE_DIST: f64 = 1.0;

const ESTIMATION_TYPES: &[LhCfPoseSampleType] = &[
    LhCfPoseSampleType::Origin,
    LhCfPoseSampleType::XAxis,
    LhCfPoseSampleType::XyPlane,
    LhCfPoseSampleType::XyzSpace,
];

impl LhGeoEstimationManager {
    /// Run the full estimation pipeline
    pub fn estimate_geometry(container: &mut LhGeoInputContainerData) -> LighthouseGeometrySolution {
        let mut matched_samples = container.get_matched_samples();
        eprintln!("[LH Solver] Starting estimation with {} samples", matched_samples.len());
        eprintln!("[LH Solver]   origin: {}", container.origin.is_some());
        eprintln!("[LH Solver]   x_axis: {}", container.x_axis.len());
        eprintln!("[LH Solver]   xy_plane: {}", container.xy_plane.len());
        eprintln!("[LH Solver]   xyz_space: {}", container.xyz_space.len());
        eprintln!("[LH Solver]   verification: {}", container.verification.len());
        for (i, s) in matched_samples.iter().enumerate() {
            eprintln!("[LH Solver]   sample[{}]: type={}, bs={:?}, ippe_augmented={}",
                i, s.sample_type, s.base_station_ids(), s.pose_sample.is_augmented);
        }
        let mut solution = LighthouseGeometrySolution::new(matched_samples.clone());

        solution.progress_info = "Data validation".into();
        let _validated = Self::data_validation(&mut matched_samples, container, &mut solution);
        eprintln!("[LH Solver] After validation: progress_ok={}, contains_samples={}",
            solution.progress_is_ok, solution.contains_samples);
        eprintln!("[LH Solver]   origin_valid={} ({}), x_axis_valid={} ({}), xy_plane_valid={} ({})",
            solution.is_origin_sample_valid, solution.origin_sample_info,
            solution.is_x_axis_samples_valid, solution.x_axis_samples_info,
            solution.is_xy_plane_samples_valid, solution.xy_plane_samples_info);

        if solution.progress_is_ok {
            solution.progress_info = "Initial estimation of geometry".into();
            eprintln!("[LH Solver] Running initial estimator...");
            // Take samples out temporarily to avoid double-borrow
            let mut samples = std::mem::take(&mut solution.samples);
            LighthouseInitialEstimator::estimate_full(&mut samples, &mut solution);
            solution.samples = samples;
            eprintln!("[LH Solver] After initial estimation: progress_ok={}, bs_poses={}",
                solution.progress_is_ok, solution.bs_poses.len());
            for (id, pose) in &solution.bs_poses {
                eprintln!("[LH Solver]   BS {}: t={:?}", id, pose.translation);
            }
            if !solution.general_failure_info.is_empty() {
                eprintln!("[LH Solver]   general_failure: {}", solution.general_failure_info);
            }

            if solution.progress_is_ok {
                solution.progress_info = "Refining geometry solution".into();
                eprintln!("[LH Solver] Running geometry solver...");
                let mut samples = std::mem::take(&mut solution.samples);
                LighthouseGeometrySolver::solve(&mut samples, &container.sensor_positions, &mut solution);
                solution.samples = samples;
                eprintln!("[LH Solver] After solver: converged={}, bs_poses={}",
                    solution.has_converged, solution.bs_poses.len());
                for (id, pose) in &solution.bs_poses {
                    eprintln!("[LH Solver]   BS {}: t={:?}", id, pose.translation);
                }

                solution.progress_info = "Align and scale solution".into();
                eprintln!("[LH Solver] Aligning and scaling...");
                Self::align_and_scale_solution(container, &mut solution, REFERENCE_DIST);
                eprintln!("[LH Solver] After align/scale: bs_poses={}", solution.bs_poses.len());
                for (id, pose) in &solution.bs_poses {
                    eprintln!("[LH Solver]   BS {}: t={:?}", id, pose.translation);
                }

                Self::create_solution_stats(&mut solution);
                Self::create_verification_stats(&mut solution);
                if let Some(ref stats) = solution.error_stats {
                    eprintln!("[LH Solver] Error stats: mean={:.6}, max={:.6}, std={:.6}", stats.mean, stats.max, stats.std);
                }
            }
        } else {
            eprintln!("[LH Solver] Validation failed, skipping estimation");
        }

        Self::humanize_error_info(&mut solution);

        solution
    }

    /// Align and scale the solution to the user-defined coordinate frame
    fn align_and_scale_solution(
        container: &LhGeoInputContainerData,
        solution: &mut LighthouseGeometrySolution,
        reference_distance: f64,
    ) {
        // Get positions from the solution samples
        let samples = &solution.samples;
        if samples.is_empty() {
            return;
        }

        let origin_idx = container.origin_index();
        let origin_pos = match samples.get(origin_idx).and_then(|s| s.pose()) {
            Some(p) => p.translation,
            None => return,
        };

        // X-axis sample positions
        let x_start = container.x_axis_start_index();
        let x_count = container.x_axis_sample_count();
        let x_axis_pos: Vec<Vector3<f64>> = (x_start..x_start + x_count)
            .filter_map(|i| samples.get(i).and_then(|s| s.pose()).map(|p| p.translation))
            .collect();

        // XY-plane sample positions
        let xy_start = container.xy_plane_start_index();
        let xy_count = container.xy_plane_sample_count();
        let xy_plane_pos: Vec<Vector3<f64>> = (xy_start..xy_start + xy_count)
            .filter_map(|i| samples.get(i).and_then(|s| s.pose()).map(|p| p.translation))
            .collect();

        if x_axis_pos.is_empty() || xy_plane_pos.is_empty() {
            return;
        }

        // Align
        {
            let (bs_aligned, transform) =
                LighthouseSystemAligner::align(origin_pos, &x_axis_pos, &xy_plane_pos, &solution.bs_poses);
            // Transform all CF poses
            let cf_aligned: Vec<Pose> = samples
                .iter()
                .filter_map(|s| s.pose().map(|p: &Pose| transform.rotate_translate_pose(p)))
                .collect();

            // Scale
            let expected = Vector3::new(reference_distance, 0.0, 0.0);
            if cf_aligned.len() > 1 {
                let (bs_scaled, cf_scaled, _scale) =
                    LighthouseSystemScaler::scale_fixed_point(&bs_aligned, &cf_aligned, &expected, &cf_aligned[1]);

                // Update solution
                solution.bs_poses = bs_scaled;
                let mut cf_idx = 0;
                for sample in &mut solution.samples {
                    if sample.has_pose() && cf_idx < cf_scaled.len() {
                        sample.set_pose(cf_scaled[cf_idx].clone());
                        cf_idx += 1;
                    }
                }
            }
        }
    }

    /// Validate collected data
    fn data_validation(
        matched_samples: &mut [LhCfPoseSampleWrapper],
        container: &LhGeoInputContainerData,
        solution: &mut LighthouseGeometrySolution,
    ) -> Vec<usize> {
        let mut valid_indices = Vec::new();
        let no_data = "No data";

        if matched_samples.is_empty() {
            solution.is_origin_sample_valid = false;
            solution.origin_sample_info = no_data.into();
            solution.progress_is_ok = false;
            return valid_indices;
        }

        // Check origin
        let origin = &mut matched_samples[0];
        if origin.angles_calibrated().is_empty() {
            origin.status = LhCfPoseSampleStatus::NoData;
            solution.progress_is_ok = false;
        } else if origin.angles_calibrated().len() < 2 {
            origin.status = LhCfPoseSampleStatus::TooFewBs;
            solution.progress_is_ok = false;
        }
        valid_indices.push(0);

        // Check x-axis
        if container.x_axis_sample_count() == 0 {
            solution.is_x_axis_samples_valid = false;
            solution.x_axis_samples_info = no_data.into();
            solution.progress_is_ok = false;
        }

        // Check xy-plane
        if container.xy_plane_sample_count() == 0 {
            solution.is_xy_plane_samples_valid = false;
            solution.xy_plane_samples_info = no_data.into();
            solution.progress_is_ok = false;
        }

        // Check remaining samples
        for (idx, sample) in matched_samples[1..].iter_mut().enumerate() {
            let actual_idx = idx + 1;
            if sample.angles_calibrated().len() >= 2 {
                if ESTIMATION_TYPES.contains(&sample.sample_type) {
                    valid_indices.push(actual_idx);
                }
            } else {
                sample.status = LhCfPoseSampleStatus::TooFewBs;
                if sample.is_mandatory {
                    valid_indices.push(actual_idx);
                    solution.progress_is_ok = false;
                }
            }
        }

        solution.contains_samples = valid_indices.len() > 1
            || (valid_indices.len() == 1 && matched_samples[valid_indices[0]].is_valid());

        valid_indices
    }

    /// Calculate error statistics using crossing beams
    fn create_solution_stats(solution: &mut LighthouseGeometrySolution) {
        let mut cf_errors: Vec<f64> = Vec::new();

        for sample in &mut solution.samples {
            if sample.sample_type == LhCfPoseSampleType::Verification {
                continue;
            }

            let bs_ids: Vec<u8> = sample.angles_calibrated().keys().copied().collect();
            let bs_angle_list: Vec<(Pose, LighthouseBsVectors)> = bs_ids
                .iter()
                .filter_map(|&bs_id| {
                    solution.bs_poses.get(&bs_id).map(|pose| {
                        (pose.clone(), sample.angles_calibrated()[&bs_id])
                    })
                })
                .collect();

            if bs_angle_list.len() >= 2 {
                let max_err = LighthouseCrossingBeam::max_distance_all_permutations(&bs_angle_list);
                sample.error_distance = max_err;
                cf_errors.push(max_err);
            }
        }

        if !cf_errors.is_empty() {
            let mean = cf_errors.iter().sum::<f64>() / cf_errors.len() as f64;
            let max = cf_errors.iter().cloned().fold(0.0_f64, f64::max);
            let variance = cf_errors.iter().map(|e| (e - mean).powi(2)).sum::<f64>() / cf_errors.len() as f64;
            let std = variance.sqrt();
            solution.error_stats = Some(ErrorStats { mean, max, std });
        }
    }

    /// Compute verification sample poses and errors
    fn create_verification_stats(solution: &mut LighthouseGeometrySolution) {
        let mut cf_errors: Vec<f64> = Vec::new();

        for sample in &mut solution.samples {
            if sample.sample_type != LhCfPoseSampleType::Verification {
                continue;
            }

            let bs_ids: Vec<u8> = sample.angles_calibrated().keys().copied().collect();

            // Check all BS are known
            let all_known = bs_ids.iter().all(|id| solution.bs_poses.contains_key(id));
            if !all_known {
                sample.status = LhCfPoseSampleStatus::BsUnknown;
                continue;
            }

            let bs_angle_list: Vec<(Pose, LighthouseBsVectors)> = bs_ids
                .iter()
                .map(|&bs_id| {
                    (solution.bs_poses[&bs_id].clone(), sample.angles_calibrated()[&bs_id])
                })
                .collect();

            if bs_angle_list.len() >= 2 {
                let (position, error) =
                    LighthouseCrossingBeam::position_max_distance_all_permutations(&bs_angle_list);
                sample.set_pose(Pose::from_rot_vec(&Vector3::zeros(), &position));
                sample.error_distance = error;
                cf_errors.push(error);
            }
        }

        if !cf_errors.is_empty() {
            let mean = cf_errors.iter().sum::<f64>() / cf_errors.len() as f64;
            let max = cf_errors.iter().cloned().fold(0.0_f64, f64::max);
            let variance = cf_errors.iter().map(|e| (e - mean).powi(2)).sum::<f64>() / cf_errors.len() as f64;
            let std = variance.sqrt();
            solution.verification_stats = Some(ErrorStats { mean, max, std });
        }
    }

    /// Generate human-readable error info
    fn humanize_error_info(solution: &mut LighthouseGeometrySolution) {
        if solution.is_origin_sample_valid {
            let (valid, info) = Self::error_info_for(&solution.samples, LhCfPoseSampleType::Origin);
            solution.is_origin_sample_valid = valid;
            solution.origin_sample_info = info;
        }
        if solution.is_x_axis_samples_valid {
            let (valid, info) = Self::error_info_for(&solution.samples, LhCfPoseSampleType::XAxis);
            solution.is_x_axis_samples_valid = valid;
            solution.x_axis_samples_info = info;
        }
        if solution.is_xy_plane_samples_valid {
            let (valid, info) = Self::error_info_for(&solution.samples, LhCfPoseSampleType::XyPlane);
            solution.is_xy_plane_samples_valid = valid;
            solution.xy_plane_samples_info = info;
        }
    }

    fn error_info_for(
        samples: &[LhCfPoseSampleWrapper],
        sample_type: LhCfPoseSampleType,
    ) -> (bool, String) {
        let infos: Vec<String> = samples
            .iter()
            .filter(|s| s.sample_type == sample_type && s.status != LhCfPoseSampleStatus::Ok)
            .map(|s| format!("{:?}", s.status))
            .collect();

        if infos.is_empty() {
            (true, String::new())
        } else {
            (false, infos.join(", "))
        }
    }
}
