/// Thread-safe sample container for lighthouse geometry estimation
///
/// Ported from LhGeoInputContainer and LhGeoInputContainerData in
/// lighthouse_geo_estimation_manager.py

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use super::sample::{LhCfPoseSample, LhCfPoseSampleType, LhCfPoseSampleWrapper};

/// Internal data storage for the container
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LhGeoInputContainerData {
    pub sensor_positions: [[f64; 3]; 4],
    pub origin: Option<LhCfPoseSample>,
    pub x_axis: Vec<LhCfPoseSample>,
    pub xy_plane: Vec<LhCfPoseSample>,
    pub xyz_space: Vec<LhCfPoseSample>,
    pub verification: Vec<LhCfPoseSample>,
    #[serde(skip)]
    pub version: u64,
}

impl LhGeoInputContainerData {
    pub fn new(sensor_positions: [[f64; 3]; 4]) -> Self {
        LhGeoInputContainerData {
            sensor_positions,
            ..Default::default()
        }
    }

    pub fn origin_index(&self) -> usize {
        0
    }

    pub fn x_axis_start_index(&self) -> usize {
        1
    }

    pub fn x_axis_sample_count(&self) -> usize {
        self.x_axis.len()
    }

    pub fn xy_plane_start_index(&self) -> usize {
        self.x_axis_start_index() + self.x_axis.len()
    }

    pub fn xy_plane_sample_count(&self) -> usize {
        self.xy_plane.len()
    }

    pub fn xyz_space_start_index(&self) -> usize {
        self.xy_plane_start_index() + self.xy_plane.len()
    }

    pub fn xyz_space_sample_count(&self) -> usize {
        self.xyz_space.len()
    }

    /// Build list of all samples as wrapped samples with IPPE augmentation
    pub fn get_matched_samples(&mut self) -> Vec<LhCfPoseSampleWrapper> {
        // Augment all samples with IPPE
        if let Some(ref mut origin) = self.origin {
            origin.augment_with_ippe(&self.sensor_positions);
        }
        for s in &mut self.x_axis {
            s.augment_with_ippe(&self.sensor_positions);
        }
        for s in &mut self.xy_plane {
            s.augment_with_ippe(&self.sensor_positions);
        }
        for s in &mut self.xyz_space {
            s.augment_with_ippe(&self.sensor_positions);
        }
        for s in &mut self.verification {
            s.augment_with_ippe(&self.sensor_positions);
        }

        let mut result = Vec::new();

        if let Some(ref origin) = self.origin {
            result.push(LhCfPoseSampleWrapper::new(
                origin.clone(),
                LhCfPoseSampleType::Origin,
            ));
        } else {
            // Add an empty origin sample
            result.push(LhCfPoseSampleWrapper::new(
                LhCfPoseSample::new(Default::default()),
                LhCfPoseSampleType::Origin,
            ));
        }

        for s in &self.x_axis {
            result.push(LhCfPoseSampleWrapper::new(
                s.clone(),
                LhCfPoseSampleType::XAxis,
            ));
        }
        for s in &self.xy_plane {
            result.push(LhCfPoseSampleWrapper::new(
                s.clone(),
                LhCfPoseSampleType::XyPlane,
            ));
        }
        for s in &self.xyz_space {
            result.push(LhCfPoseSampleWrapper::new(
                s.clone(),
                LhCfPoseSampleType::XyzSpace,
            ));
        }
        for s in &self.verification {
            result.push(LhCfPoseSampleWrapper::new(
                s.clone(),
                LhCfPoseSampleType::Verification,
            ));
        }

        result
    }

    pub fn is_empty(&self) -> bool {
        self.origin.is_none()
            && self.x_axis.is_empty()
            && self.xy_plane.is_empty()
            && self.xyz_space.is_empty()
            && self.verification.is_empty()
    }
}

/// Thread-safe container wrapping LhGeoInputContainerData
#[derive(Clone)]
pub struct LhGeoInputContainer {
    data: Arc<Mutex<LhGeoInputContainerData>>,
}

impl LhGeoInputContainer {
    pub fn new(sensor_positions: [[f64; 3]; 4]) -> Self {
        LhGeoInputContainer {
            data: Arc::new(Mutex::new(LhGeoInputContainerData::new(sensor_positions))),
        }
    }

    pub fn get_data_version(&self) -> u64 {
        self.data.lock().unwrap().version
    }

    pub fn get_data_copy(&self) -> LhGeoInputContainerData {
        self.data.lock().unwrap().clone()
    }

    pub fn set_origin_sample(&self, mut sample: LhCfPoseSample) {
        let mut data = self.data.lock().unwrap();
        sample.augment_with_ippe(&data.sensor_positions);
        data.origin = Some(sample);
        data.version += 1;
    }

    pub fn set_x_axis_sample(&self, mut sample: LhCfPoseSample) {
        let mut data = self.data.lock().unwrap();
        sample.augment_with_ippe(&data.sensor_positions);
        data.x_axis = vec![sample];
        data.version += 1;
    }

    pub fn append_xy_plane_sample(&self, mut sample: LhCfPoseSample) {
        let mut data = self.data.lock().unwrap();
        sample.augment_with_ippe(&data.sensor_positions);
        data.xy_plane.push(sample);
        data.version += 1;
    }

    pub fn append_xyz_space_samples(&self, samples: Vec<LhCfPoseSample>) {
        let mut data = self.data.lock().unwrap();
        let sensor_pos = data.sensor_positions;
        for mut s in samples {
            s.augment_with_ippe(&sensor_pos);
            data.xyz_space.push(s);
        }
        data.version += 1;
    }

    pub fn append_verification_samples(&self, samples: Vec<LhCfPoseSample>) {
        let mut data = self.data.lock().unwrap();
        let sensor_pos = data.sensor_positions;
        for mut s in samples {
            s.augment_with_ippe(&sensor_pos);
            data.verification.push(s);
        }
        data.version += 1;
    }

    pub fn remove_sample(&self, uid: u64) {
        let mut data = self.data.lock().unwrap();
        if let Some(ref origin) = data.origin {
            if origin.uid() == uid {
                data.origin = None;
                data.version += 1;
                return;
            }
        }

        fn remove_from(list: &mut Vec<LhCfPoseSample>, uid: u64) -> bool {
            if let Some(idx) = list.iter().position(|s| s.uid() == uid) {
                list.remove(idx);
                return true;
            }
            false
        }

        if remove_from(&mut data.x_axis, uid)
            || remove_from(&mut data.xy_plane, uid)
            || remove_from(&mut data.xyz_space, uid)
            || remove_from(&mut data.verification, uid)
        {
            data.version += 1;
        }
    }

    pub fn convert_to_verification_sample(&self, uid: u64) {
        let mut data = self.data.lock().unwrap();

        fn find_and_remove(list: &mut Vec<LhCfPoseSample>, uid: u64) -> Option<LhCfPoseSample> {
            list.iter().position(|s| s.uid() == uid).map(|idx| list.remove(idx))
        }

        let removed = find_and_remove(&mut data.x_axis, uid)
            .or_else(|| find_and_remove(&mut data.xy_plane, uid))
            .or_else(|| find_and_remove(&mut data.xyz_space, uid));

        if let Some(sample) = removed {
            data.verification.push(sample);
            data.version += 1;
        }
    }

    pub fn convert_to_xyz_space_sample(&self, uid: u64) {
        let mut data = self.data.lock().unwrap();

        fn find_and_remove(list: &mut Vec<LhCfPoseSample>, uid: u64) -> Option<LhCfPoseSample> {
            list.iter().position(|s| s.uid() == uid).map(|idx| list.remove(idx))
        }

        let removed = find_and_remove(&mut data.xy_plane, uid)
            .or_else(|| find_and_remove(&mut data.verification, uid));

        if let Some(sample) = removed {
            data.xyz_space.push(sample);
            data.version += 1;
        }
    }

    pub fn clear_all_samples(&self) {
        let mut data = self.data.lock().unwrap();
        let sensor_pos = data.sensor_positions;
        *data = LhGeoInputContainerData::new(sensor_pos);
        data.version += 1;
    }

    pub fn is_empty(&self) -> bool {
        self.data.lock().unwrap().is_empty()
    }

    /// Save container data to YAML string
    pub fn save_to_yaml(&self) -> Result<String, String> {
        let data = self.data.lock().unwrap();
        serde_yaml::to_string(&*data).map_err(|e| e.to_string())
    }

    /// Load container data from YAML string
    pub fn load_from_yaml(&self, yaml_str: &str) -> Result<(), String> {
        let new_data: LhGeoInputContainerData =
            serde_yaml::from_str(yaml_str).map_err(|e| e.to_string())?;
        let mut data = self.data.lock().unwrap();
        let version = data.version;
        *data = new_data;
        data.version = version + 1;
        Ok(())
    }
}
