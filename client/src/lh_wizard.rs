/// Lighthouse Geometry Wizard - bridges UI to estimation pipeline
///
/// Manages wizard state, background solver task, and CF communication.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::lh_geo::container::{LhGeoInputContainer, LhGeoInputContainerData};
use crate::lh_geo::estimation_manager::LhGeoEstimationManager;
use crate::lh_geo::sample::{LhCfPoseSampleType, LhCfPoseSampleStatus};
use crate::lh_geo::solution::{LighthouseGeometrySolution, ErrorStats};
use crate::lh_geo::types::{LhDeck4SensorPositions, Pose};

/// Wizard collection step
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WizardStep {
    Origin = 0,
    XAxis = 1,
    XyPlane = 2,
    XyzSpace = 3,
    Verification = 4,
}

impl WizardStep {
    pub fn from_index(idx: i32) -> Self {
        match idx {
            0 => WizardStep::Origin,
            1 => WizardStep::XAxis,
            2 => WizardStep::XyPlane,
            3 => WizardStep::XyzSpace,
            4 => WizardStep::Verification,
            _ => WizardStep::Origin,
        }
    }

    pub fn instructions(&self) -> &'static str {
        match self {
            WizardStep::Origin => "Put the Crazyflie where you want the origin of your coordinate system.",
            WizardStep::XAxis => "Put the Crazyflie on the positive X-axis, exactly 1 meter from the origin. This sample defines the X-axis and the scale of the system.",
            WizardStep::XyPlane => "Put the Crazyflie somewhere in the XY-plane, but not on the X-axis. This defines the floor plane. You can sample multiple positions.",
            WizardStep::XyzSpace => "Sample points in the space to refine the geometry. You need at least two base stations visible. Sample by rotating the Crazyflie quickly left-right, or click the button.",
            WizardStep::Verification => "Sample points for verification (optional). These check the geometry quality but don't affect the estimation. When satisfied, use 'Upload Geometry' below to write the result to the Crazyflie.",
        }
    }

    pub fn button_text(&self) -> &'static str {
        match self {
            WizardStep::Origin => "Measure Origin",
            WizardStep::XAxis => "Measure X-Axis",
            WizardStep::XyPlane => "Measure XY-Plane",
            WizardStep::XyzSpace => "Sample Position",
            WizardStep::Verification => "Sample Position",
        }
    }
}

/// Shared wizard state
pub struct LhWizardState {
    pub container: LhGeoInputContainer,
    pub current_step: WizardStep,
    pub latest_solution: Option<LighthouseGeometrySolution>,
    pub is_measuring: bool,
    /// Version of container when last solved
    pub last_solved_version: u64,
}

impl LhWizardState {
    pub fn new() -> Self {
        let sensor_positions = LhDeck4SensorPositions::positions();
        LhWizardState {
            container: LhGeoInputContainer::new(sensor_positions),
            current_step: WizardStep::Origin,
            latest_solution: None,
            is_measuring: false,
            last_solved_version: 0,
        }
    }
}

/// Run the solver on the container data and return the solution
pub fn run_solver(container: &LhGeoInputContainer) -> LighthouseGeometrySolution {
    let mut data = container.get_data_copy();
    LhGeoEstimationManager::estimate_geometry(&mut data)
}

/// Extract UI-friendly sample details from a solution
pub fn get_sample_details(solution: &LighthouseGeometrySolution) -> Vec<SampleDetail> {
    solution.samples.iter().map(|s| {
        let (x, y, z) = if let Some(pose) = s.pose() {
            (
                format!("{:.3}", pose.translation[0]),
                format!("{:.3}", pose.translation[1]),
                format!("{:.3}", pose.translation[2]),
            )
        } else {
            ("—".into(), "—".into(), "—".into())
        };

        SampleDetail {
            sample_type: format!("{}", s.sample_type),
            x,
            y,
            z,
            error: if s.error_distance > 0.0 {
                format!("{:.4}", s.error_distance)
            } else {
                "—".into()
            },
            is_verification: s.sample_type == LhCfPoseSampleType::Verification,
            is_invalid: s.status != LhCfPoseSampleStatus::Ok,
            is_large_error: s.is_error_large(),
        }
    }).collect()
}

/// Extract UI-friendly base station details from a solution
pub fn get_bs_details(solution: &LighthouseGeometrySolution) -> Vec<BsDetail> {
    let mut details: Vec<BsDetail> = solution.bs_poses.iter().map(|(&id, pose)| {
        let sample_count = solution.bs_sample_count.get(&id).copied().unwrap_or(0);

        // Sum links for this BS
        let total_links: u32 = solution.link_count
            .get(&id)
            .map(|links| links.values().sum())
            .unwrap_or(0);

        BsDetail {
            id: id as i32,
            x: format!("{:.3}", pose.translation[0]),
            y: format!("{:.3}", pose.translation[1]),
            z: format!("{:.3}", pose.translation[2]),
            samples: sample_count as i32,
            links: total_links as i32,
            low_links: total_links < solution.link_count_ok_threshold,
        }
    }).collect();

    details.sort_by_key(|d| d.id);
    details
}

pub struct SampleDetail {
    pub sample_type: String,
    pub x: String,
    pub y: String,
    pub z: String,
    pub error: String,
    pub is_verification: bool,
    pub is_invalid: bool,
    pub is_large_error: bool,
}

pub struct BsDetail {
    pub id: i32,
    pub x: String,
    pub y: String,
    pub z: String,
    pub samples: i32,
    pub links: i32,
    pub low_links: bool,
}
