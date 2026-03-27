/// Solution types for lighthouse geometry estimation
///
/// Ported from lighthouse_geometry_solution.py

use std::collections::HashMap;

use super::sample::LhCfPoseSampleWrapper;
use super::types::Pose;

#[derive(Debug, Clone)]
pub struct ErrorStats {
    pub mean: f64,
    pub max: f64,
    pub std: f64,
}

#[derive(Debug, Clone)]
pub struct LighthouseGeometrySolution {
    pub samples: Vec<LhCfPoseSampleWrapper>,
    pub bs_poses: HashMap<u8, Pose>,
    pub error_stats: Option<ErrorStats>,
    pub verification_stats: Option<ErrorStats>,
    pub has_converged: bool,
    pub progress_info: String,
    pub progress_is_ok: bool,

    pub is_origin_sample_valid: bool,
    pub origin_sample_info: String,
    pub is_x_axis_samples_valid: bool,
    pub x_axis_samples_info: String,
    pub is_xy_plane_samples_valid: bool,
    pub xy_plane_samples_info: String,
    pub xyz_space_samples_info: String,
    pub general_failure_info: String,

    /// link_count[bs_a][bs_b] = count of samples where both are visible
    pub link_count: HashMap<u8, HashMap<u8, u32>>,
    /// Number of samples containing each base station
    pub bs_sample_count: HashMap<u8, u32>,
    pub link_count_ok_threshold: u32,
    pub contains_samples: bool,
}

impl LighthouseGeometrySolution {
    pub fn new(samples: Vec<LhCfPoseSampleWrapper>) -> Self {
        LighthouseGeometrySolution {
            samples,
            bs_poses: HashMap::new(),
            error_stats: None,
            verification_stats: None,
            has_converged: false,
            progress_info: String::new(),
            progress_is_ok: true,
            is_origin_sample_valid: true,
            origin_sample_info: String::new(),
            is_x_axis_samples_valid: true,
            x_axis_samples_info: String::new(),
            is_xy_plane_samples_valid: true,
            xy_plane_samples_info: String::new(),
            xyz_space_samples_info: String::new(),
            general_failure_info: String::new(),
            link_count: HashMap::new(),
            bs_sample_count: HashMap::new(),
            link_count_ok_threshold: 1,
            contains_samples: false,
        }
    }

    pub fn empty() -> Self {
        Self::new(Vec::new())
    }
}
