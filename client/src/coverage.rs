/// Lighthouse base station coverage computation.
///
/// Ported from the Python lh-volume-coverage tool. Computes which voxels in a
/// room are visible by one or more lighthouse base stations given their
/// position, orientation and sweep angles.

use std::collections::BTreeMap;
use std::f32::consts::PI;

use serde::{Deserialize, Serialize};

/// Configuration for a single lighthouse base station.
#[derive(Clone, Debug)]
pub struct BaseStation {
    /// Position in metres (x, y, z).
    pub pos: [f32; 3],
    /// Azimuth in degrees – rotation around the Z axis (yaw).
    pub azimuth_deg: f32,
    /// Elevation in degrees – tilt downward from horizontal (pitch).
    pub elevation_deg: f32,
}

impl BaseStation {
    /// Create a BaseStation from a position and a 3×3 rotation matrix (row-major).
    /// Extracts azimuth and elevation from the matrix.
    pub fn from_rotation_matrix(pos: [f32; 3], r: [[f32; 3]; 3]) -> Self {
        // Forward direction is the first column of R: (r[0][0], r[1][0], r[2][0])
        let azimuth_deg = r[1][0].atan2(r[0][0]) * 180.0 / PI;
        let elevation_deg = (-r[2][0]).asin() * 180.0 / PI;
        Self {
            pos,
            azimuth_deg,
            elevation_deg,
        }
    }

    /// Build the 3×3 rotation matrix (column-major, stored row-major for
    /// convenience) that maps base-station-local coordinates to world
    /// coordinates.
    ///
    /// Local axes:
    ///   X = forward (look direction)
    ///   Y = right
    ///   Z = up
    pub fn rotation_matrix(&self) -> [[f32; 3]; 3] {
        let a = self.azimuth_deg * PI / 180.0;
        let e = self.elevation_deg * PI / 180.0;

        let (sa, ca) = a.sin_cos();
        let (se, ce) = e.sin_cos();

        // Columns of R (world ← local)
        // forward  = ( ce*ca,  ce*sa, -se)
        // right    = (-sa,     ca,     0 )
        // up       = ( se*ca,  se*sa,  ce)
        [
            [ce * ca, -sa, se * ca],
            [ce * sa, ca, se * sa],
            [-se, 0.0, ce],
        ]
    }
}

/// Room & sweep parameters together with computed coverage.
pub struct CoverageResult {
    /// Voxels per metre.
    resolution: f32,
    /// Per-voxel coverage count (how many BSs see this voxel).
    /// Indexed as [iz][iy][ix].
    voxels: Vec<Vec<Vec<u8>>>,
}

impl CoverageResult {
    pub fn coverage_ratio(&self, min_bs: u8) -> f32 {
        let mut count = 0u32;
        let mut total = 0u32;
        for plane in &self.voxels {
            for row in plane {
                for &v in row {
                    total += 1;
                    if v >= min_bs {
                        count += 1;
                    }
                }
            }
        }
        if total == 0 {
            0.0
        } else {
            count as f32 / total as f32
        }
    }

    /// Iterate over all voxels, yielding (world_x, world_y, world_z, count).
    pub fn iter_voxels(&self, offset: [f32; 3]) -> impl Iterator<Item = (f32, f32, f32, u8)> + '_ {
        let res = self.resolution;
        self.voxels.iter().enumerate().flat_map(move |(iz, plane)| {
            plane.iter().enumerate().flat_map(move |(iy, row)| {
                row.iter().enumerate().map(move |(ix, &count)| {
                    (
                        ix as f32 / res + offset[0],
                        iy as f32 / res + offset[1],
                        iz as f32 / res + offset[2],
                        count,
                    )
                })
            })
        })
    }
}

/// Compute coverage for the given room and base stations.
///
/// `horiz_deg` and `vert_deg` are the full sweep angles (e.g. 160 and 115).
/// `receiver_fov_deg`: if `Some(fov)`, also check that the angle from the
/// receiver's up-direction (0,0,1) to the base station is within fov/2.
/// `tilt_reduction_deg`: if `Some(deg)`, reduce the effective receiver FOV by this amount
/// on each side to account for worst-case drone tilt during flight.
/// `offset`: world-space offset of the room's min corner (e.g. `(-rx/2, -ry/2, 0)` to center).
pub fn compute_coverage(
    room_x: f32,
    room_y: f32,
    room_z: f32,
    resolution: f32,
    base_stations: &[BaseStation],
    horiz_deg: f32,
    vert_deg: f32,
    receiver_fov_deg: Option<f32>,
    tilt_reduction_deg: Option<f32>,
    max_dist: f32,
    offset: [f32; 3],
) -> CoverageResult {
    let horiz_half = (horiz_deg / 2.0) * PI / 180.0;
    let vert_half = (vert_deg / 2.0) * PI / 180.0;
    let tilt_rad = tilt_reduction_deg.unwrap_or(0.0) * PI / 180.0;
    let receiver_half = receiver_fov_deg.map(|fov| (fov / 2.0) * PI / 180.0 - tilt_rad);

    let nx = (room_x * resolution) as usize + 1;
    let ny = (room_y * resolution) as usize + 1;
    let nz = (room_z * resolution) as usize + 1;

    let mut voxels = vec![vec![vec![0u8; nx]; ny]; nz];

    for bs in base_stations {
        let r = bs.rotation_matrix();
        // R^T to go from world to local
        let rt = [
            [r[0][0], r[1][0], r[2][0]],
            [r[0][1], r[1][1], r[2][1]],
            [r[0][2], r[1][2], r[2][2]],
        ];

        for iz in 0..nz {
            for iy in 0..ny {
                for ix in 0..nx {
                    let wx = ix as f32 / resolution + offset[0];
                    let wy = iy as f32 / resolution + offset[1];
                    let wz = iz as f32 / resolution + offset[2];

                    // Vector from BS to voxel in world space
                    let dx = wx - bs.pos[0];
                    let dy = wy - bs.pos[1];
                    let dz = wz - bs.pos[2];

                    // Transform to BS local frame
                    let lx = rt[0][0] * dx + rt[0][1] * dy + rt[0][2] * dz;
                    let ly = rt[1][0] * dx + rt[1][1] * dy + rt[1][2] * dz;
                    let lz = rt[2][0] * dx + rt[2][1] * dy + rt[2][2] * dz;

                    if lx <= 0.0 {
                        continue; // behind the base station
                    }

                    let dist = (lx * lx + ly * ly + lz * lz).sqrt();
                    if dist > max_dist {
                        continue;
                    }

                    // Horizontal angle: Y sweep (left-right)
                    let ang_h = ly.atan2(lx).abs();
                    // Vertical angle: Z sweep (up-down)
                    let ang_v = lz.atan2(lx).abs();

                    if ang_h < horiz_half && ang_v < vert_half {
                        // Receiver FOV check: angle between voxel-to-BS direction
                        // and receiver up-vector (0,0,1)
                        if let Some(rx_half) = receiver_half {
                            // Vector from voxel to BS (unnormalised), we only need the angle
                            // cos(angle) = (-dz) / dist  (dot with (0,0,1))
                            let cos_angle = (-dz) / dist;
                            if cos_angle.acos() > rx_half {
                                continue;
                            }
                        }
                        voxels[iz][iy][ix] = voxels[iz][iy][ix].saturating_add(1);
                    }
                }
            }
        }
    }

    CoverageResult {
        resolution,
        voxels,
    }
}

/// Load base stations from a Crazyflie lighthouse system configuration YAML file.
/// Returns the list of base stations sorted by ID.
pub fn load_geometry_yaml(path: &std::path::Path) -> Result<Vec<BaseStation>, String> {
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("Failed to read file: {}", e))?;
    let doc: serde_yaml::Value =
        serde_yaml::from_str(&content).map_err(|e| format!("Failed to parse YAML: {}", e))?;

    let geos = doc
        .get("geos")
        .ok_or("Missing 'geos' key in file")?
        .as_mapping()
        .ok_or("'geos' is not a mapping")?;

    let mut stations: BTreeMap<i64, BaseStation> = BTreeMap::new();

    for (key, geo) in geos {
        let id = key
            .as_i64()
            .ok_or_else(|| format!("Invalid base station key: {:?}", key))?;

        let origin = geo
            .get("origin")
            .ok_or("Missing 'origin'")?
            .as_sequence()
            .ok_or("'origin' is not a list")?;
        if origin.len() != 3 {
            return Err(format!("BS {}: origin must have 3 elements", id));
        }
        let pos = [
            origin[0].as_f64().ok_or("Bad origin[0]")? as f32,
            origin[1].as_f64().ok_or("Bad origin[1]")? as f32,
            origin[2].as_f64().ok_or("Bad origin[2]")? as f32,
        ];

        let rotation = geo
            .get("rotation")
            .ok_or("Missing 'rotation'")?
            .as_sequence()
            .ok_or("'rotation' is not a list")?;
        if rotation.len() != 3 {
            return Err(format!("BS {}: rotation must have 3 rows", id));
        }
        let mut r = [[0.0f32; 3]; 3];
        for (i, row) in rotation.iter().enumerate() {
            let row = row.as_sequence().ok_or("rotation row not a list")?;
            if row.len() != 3 {
                return Err(format!("BS {}: rotation row {} must have 3 elements", id, i));
            }
            for (j, val) in row.iter().enumerate() {
                r[i][j] = val.as_f64().ok_or("Bad rotation element")? as f32;
            }
        }

        stations.insert(id, BaseStation::from_rotation_matrix(pos, r));
    }

    Ok(stations.into_values().collect())
}

// --- Scene save/load ---

#[derive(Serialize, Deserialize)]
struct SceneBaseStation {
    x: f32,
    y: f32,
    z: f32,
    azimuth_deg: f32,
    elevation_deg: f32,
}

#[derive(Serialize, Deserialize)]
pub struct Scene {
    pub room_x: f32,
    pub room_y: f32,
    pub room_z: f32,
    pub resolution: f32,
    pub center_origin: bool,
    pub receiver_fov_enabled: bool,
    #[serde(default = "default_true")]
    pub tilt_compensation_enabled: bool,
    #[serde(default = "default_tilt")]
    pub max_tilt_angle: f32,
    #[serde(default = "default_max_dist")]
    pub max_bs_distance: f32,
    pub show_coverage: [bool; 5],
    base_stations: Vec<SceneBaseStation>,
}

fn default_true() -> bool { true }
fn default_tilt() -> f32 { 10.0 }
fn default_max_dist() -> f32 { 5.0 }

impl Scene {
    pub fn new(
        room_x: f32,
        room_y: f32,
        room_z: f32,
        resolution: f32,
        center_origin: bool,
        receiver_fov_enabled: bool,
        tilt_compensation_enabled: bool,
        max_tilt_angle: f32,
        max_bs_distance: f32,
        show_coverage: [bool; 5],
        base_stations: &[BaseStation],
    ) -> Self {
        Self {
            room_x,
            room_y,
            room_z,
            resolution,
            center_origin,
            receiver_fov_enabled,
            tilt_compensation_enabled,
            max_tilt_angle,
            max_bs_distance,
            show_coverage,
            base_stations: base_stations
                .iter()
                .map(|bs| SceneBaseStation {
                    x: bs.pos[0],
                    y: bs.pos[1],
                    z: bs.pos[2],
                    azimuth_deg: bs.azimuth_deg,
                    elevation_deg: bs.elevation_deg,
                })
                .collect(),
        }
    }

    pub fn base_stations(&self) -> Vec<BaseStation> {
        self.base_stations
            .iter()
            .map(|s| BaseStation {
                pos: [s.x, s.y, s.z],
                azimuth_deg: s.azimuth_deg,
                elevation_deg: s.elevation_deg,
            })
            .collect()
    }
}

pub fn save_scene(path: &std::path::Path, scene: &Scene) -> Result<(), String> {
    let content =
        serde_yaml::to_string(scene).map_err(|e| format!("Failed to serialize: {}", e))?;
    std::fs::write(path, content).map_err(|e| format!("Failed to write file: {}", e))
}

pub fn load_scene(path: &std::path::Path) -> Result<Scene, String> {
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("Failed to read file: {}", e))?;
    serde_yaml::from_str(&content).map_err(|e| format!("Failed to parse scene: {}", e))
}
