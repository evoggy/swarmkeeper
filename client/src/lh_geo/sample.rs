/// Sample types for the lighthouse geometry estimation
///
/// Ported from lighthouse_cf_pose_sample.py

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use nalgebra::{Matrix3, Vector3};
use serde::{Deserialize, Serialize};

use super::bs_vector::{projection_pair_list, LighthouseBsVectors};
use super::ippe_cf::IppeCf;
use super::types::Pose;

static GLOBAL_UID: AtomicU64 = AtomicU64::new(0);

fn next_uid() -> u64 {
    GLOBAL_UID.fetch_add(1, Ordering::Relaxed)
}

/// Two possible IPPE poses for a base station (in CF reference frame)
#[derive(Debug, Clone)]
pub struct BsPairPoses(pub Pose, pub Pose);

impl BsPairPoses {
    /// Access the two solutions as a slice for iteration
    pub fn as_slice(&self) -> [&Pose; 2] {
        [&self.0, &self.1]
    }
}

/// Type of sample in the collection process
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LhCfPoseSampleType {
    Origin,
    XAxis,
    XyPlane,
    XyzSpace,
    Verification,
}

impl std::fmt::Display for LhCfPoseSampleType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LhCfPoseSampleType::Origin => write!(f, "origin"),
            LhCfPoseSampleType::XAxis => write!(f, "x-axis"),
            LhCfPoseSampleType::XyPlane => write!(f, "xy-plane"),
            LhCfPoseSampleType::XyzSpace => write!(f, "xyz-space"),
            LhCfPoseSampleType::Verification => write!(f, "verification"),
        }
    }
}

/// Status of a pose sample
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LhCfPoseSampleStatus {
    Ok,
    TooFewBs,
    Ambiguous,
    NoData,
    BsUnknown,
}

/// Raw measurement sample with calibrated angles from each visible base station.
/// Ported from LhCfPoseSample in lighthouse_cf_pose_sample.py
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LhCfPoseSample {
    /// Calibrated angles from each base station (keyed by BS id)
    pub angles_calibrated: HashMap<u8, LighthouseBsVectors>,
    /// IPPE solutions per BS (computed on demand, not serialized)
    #[serde(skip)]
    pub ippe_solutions: HashMap<u8, BsPairPoses>,
    #[serde(skip)]
    pub is_augmented: bool,
    uid: u64,
}

impl LhCfPoseSample {
    pub fn new(angles_calibrated: HashMap<u8, LighthouseBsVectors>) -> Self {
        LhCfPoseSample {
            angles_calibrated,
            ippe_solutions: HashMap::new(),
            is_augmented: false,
            uid: next_uid(),
        }
    }

    pub fn uid(&self) -> u64 {
        self.uid
    }

    pub fn is_empty(&self) -> bool {
        self.angles_calibrated.is_empty()
    }

    /// Compute IPPE solutions for all base stations.
    /// Each BS gets two possible poses in the CF reference frame.
    pub fn augment_with_ippe(&mut self, sensor_positions: &[[f64; 3]; 4]) {
        if self.is_augmented {
            return;
        }

        for (&bs_id, angles) in &self.angles_calibrated {
            let projections = projection_pair_list(angles);
            if let Some(estimates) = IppeCf::solve(sensor_positions, &projections) {
                // Convert from BS reference frame to CF reference frame
                // In BS frame: P_cam = R * P_world + t
                // Inverse: P_world = R^T * (P_cam - t) = R^T * P_cam - R^T * t
                // So the BS pose in CF frame: R_cf = R^T, t_cf = -R^T * t
                let rot_1 = estimates[0].r.transpose();
                let t_1 = rot_1 * (-estimates[0].t);
                let rot_2 = estimates[1].r.transpose();
                let t_2 = rot_2 * (-estimates[1].t);

                self.ippe_solutions.insert(
                    bs_id,
                    BsPairPoses(Pose::new(rot_1, t_1), Pose::new(rot_2, t_2)),
                );
            }
        }

        self.is_augmented = true;
    }
}

/// Wrapper around a sample with type, status, and estimated pose.
/// Ported from LhCfPoseSampleWrapper in lighthouse_cf_pose_sample.py
#[derive(Debug, Clone)]
pub struct LhCfPoseSampleWrapper {
    pub pose_sample: LhCfPoseSample,
    pub sample_type: LhCfPoseSampleType,
    pub is_mandatory: bool,
    pub status: LhCfPoseSampleStatus,
    pose: Option<Pose>,
    pub error_distance: f64,
}

impl LhCfPoseSampleWrapper {
    pub const LARGE_ERROR_THRESHOLD: f64 = 0.01;

    pub fn new(pose_sample: LhCfPoseSample, sample_type: LhCfPoseSampleType) -> Self {
        let is_mandatory = matches!(
            sample_type,
            LhCfPoseSampleType::Origin | LhCfPoseSampleType::XAxis | LhCfPoseSampleType::XyPlane
        );

        LhCfPoseSampleWrapper {
            pose_sample,
            sample_type,
            is_mandatory,
            status: LhCfPoseSampleStatus::Ok,
            pose: None,
            error_distance: 0.0,
        }
    }

    pub fn uid(&self) -> u64 {
        self.pose_sample.uid()
    }

    pub fn has_pose(&self) -> bool {
        self.pose.is_some()
    }

    pub fn pose(&self) -> Option<&Pose> {
        self.pose.as_ref()
    }

    pub fn set_pose(&mut self, pose: Pose) {
        self.pose = Some(pose);
    }

    pub fn is_valid(&self) -> bool {
        self.status == LhCfPoseSampleStatus::Ok
    }

    pub fn is_error_large(&self) -> bool {
        self.error_distance > Self::LARGE_ERROR_THRESHOLD
    }

    pub fn base_station_ids(&self) -> Vec<u8> {
        self.pose_sample.angles_calibrated.keys().copied().collect()
    }

    pub fn angles_calibrated(&self) -> &HashMap<u8, LighthouseBsVectors> {
        &self.pose_sample.angles_calibrated
    }

    pub fn ippe_solutions(&self) -> &HashMap<u8, BsPairPoses> {
        &self.pose_sample.ippe_solutions
    }
}
