/// Planning scene: combines LH base stations, TDoA3 anchors, and opaque
/// geometric obstacles in a single scene with save/load support.

use std::f32::consts::PI;

use serde::{Deserialize, Serialize};

use crate::coverage;
use crate::tdoa3;

// --- Obstacle types ---

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum ObstacleKind {
    Box,
    Cylinder,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Obstacle {
    pub kind: ObstacleKind,
    pub pos: [f32; 3],
    pub yaw_deg: f32,
    /// Box width (X extent) or ignored for Cylinder.
    pub width: f32,
    /// Box depth (Y extent) or ignored for Cylinder.
    pub depth: f32,
    /// Height (Z extent) for both Box and Cylinder.
    pub height: f32,
    /// Cylinder radius (ignored for Box).
    pub radius: f32,
    /// RGB color [0..1].
    #[serde(default = "default_color")]
    pub color: [f32; 3],
    /// If true, bottom of obstacle is pinned to Z=0.
    #[serde(default)]
    pub on_floor: bool,
}

fn default_color() -> [f32; 3] {
    [0.45, 0.45, 0.50]
}

impl Obstacle {
    pub fn new_box(pos: [f32; 3]) -> Self {
        Self {
            kind: ObstacleKind::Box,
            pos,
            yaw_deg: 0.0,
            width: 1.0,
            depth: 1.0,
            height: 1.0,
            radius: 0.0,
            color: default_color(),
            on_floor: true,
        }
    }

    pub fn new_cylinder(pos: [f32; 3]) -> Self {
        Self {
            kind: ObstacleKind::Cylinder,
            pos,
            yaw_deg: 0.0,
            width: 0.0,
            depth: 0.0,
            height: 1.0,
            radius: 0.5,
            color: default_color(),
            on_floor: true,
        }
    }

    /// Test if a point is inside this obstacle.
    pub fn contains_point(&self, p: [f32; 3]) -> bool {
        let a = -self.yaw_deg * PI / 180.0;
        let (sa, ca) = a.sin_cos();
        let dx = p[0] - self.pos[0];
        let dy = p[1] - self.pos[1];
        let dz = p[2] - self.pos[2];
        let lx = ca * dx - sa * dy;
        let ly = sa * dx + ca * dy;
        let lz = dz;

        match self.kind {
            ObstacleKind::Box => {
                lx.abs() <= self.width / 2.0
                    && ly.abs() <= self.depth / 2.0
                    && lz.abs() <= self.height / 2.0
            }
            ObstacleKind::Cylinder => {
                lx * lx + ly * ly <= self.radius * self.radius
                    && lz.abs() <= self.height / 2.0
            }
        }
    }

    /// Test if a ray from `origin` to `target` intersects this obstacle.
    /// Returns true if the ray segment is blocked (hits the obstacle between 0 and 1).
    pub fn blocks_ray(&self, origin: [f32; 3], target: [f32; 3]) -> bool {
        // Transform ray into obstacle's local frame (undo yaw rotation + translation)
        let a = -self.yaw_deg * PI / 180.0;
        let (sa, ca) = a.sin_cos();

        let inv_transform = |p: [f32; 3]| -> [f32; 3] {
            let dx = p[0] - self.pos[0];
            let dy = p[1] - self.pos[1];
            let dz = p[2] - self.pos[2];
            [ca * dx - sa * dy, sa * dx + ca * dy, dz]
        };

        let lo = inv_transform(origin);
        let lt = inv_transform(target);
        let dir = [lt[0] - lo[0], lt[1] - lo[1], lt[2] - lo[2]];

        match self.kind {
            ObstacleKind::Box => {
                let hw = self.width / 2.0;
                let hd = self.depth / 2.0;
                let hh = self.height / 2.0;
                ray_aabb_intersect(lo, dir, [-hw, -hd, -hh], [hw, hd, hh])
            }
            ObstacleKind::Cylinder => {
                let r = self.radius;
                let hh = self.height / 2.0;
                ray_cylinder_intersect(lo, dir, r, hh)
            }
        }
    }

    /// Generate triangle mesh vertices for this obstacle.
    /// Returns flat [x,y,z, x,y,z, ...] suitable for GL_TRIANGLES.
    pub fn triangulate(&self) -> Vec<f32> {
        match self.kind {
            ObstacleKind::Box => self.triangulate_box(),
            ObstacleKind::Cylinder => self.triangulate_cylinder(),
        }
    }

    fn transform_point(&self, lx: f32, ly: f32, lz: f32) -> [f32; 3] {
        let a = self.yaw_deg * PI / 180.0;
        let (sa, ca) = a.sin_cos();
        [
            self.pos[0] + ca * lx - sa * ly,
            self.pos[1] + sa * lx + ca * ly,
            self.pos[2] + lz,
        ]
    }

    fn triangulate_box(&self) -> Vec<f32> {
        let hw = self.width / 2.0;
        let hd = self.depth / 2.0;
        let hh = self.height / 2.0;

        // 8 corners in local space
        let corners = [
            [-hw, -hd, -hh], // 0
            [ hw, -hd, -hh], // 1
            [ hw,  hd, -hh], // 2
            [-hw,  hd, -hh], // 3
            [-hw, -hd,  hh], // 4
            [ hw, -hd,  hh], // 5
            [ hw,  hd,  hh], // 6
            [-hw,  hd,  hh], // 7
        ];

        // Transform all corners to world space
        let wc: Vec<[f32; 3]> = corners
            .iter()
            .map(|c| self.transform_point(c[0], c[1], c[2]))
            .collect();

        // 6 faces, 2 triangles each (12 triangles total)
        let faces: [[usize; 4]; 6] = [
            [0, 1, 2, 3], // bottom
            [4, 7, 6, 5], // top
            [0, 4, 5, 1], // front (-Y)
            [2, 6, 7, 3], // back (+Y)
            [0, 3, 7, 4], // left (-X)
            [1, 5, 6, 2], // right (+X)
        ];

        let mut verts = Vec::with_capacity(6 * 2 * 3 * 3);
        for face in &faces {
            // Triangle 1: 0, 1, 2
            for &idx in &[face[0], face[1], face[2]] {
                verts.extend_from_slice(&wc[idx]);
            }
            // Triangle 2: 0, 2, 3
            for &idx in &[face[0], face[2], face[3]] {
                verts.extend_from_slice(&wc[idx]);
            }
        }
        verts
    }

    fn triangulate_cylinder(&self) -> Vec<f32> {
        const SEGMENTS: usize = 24;
        let r = self.radius;
        let hh = self.height / 2.0;

        let mut verts = Vec::new();

        // Generate circle points
        let circle: Vec<[f32; 2]> = (0..SEGMENTS)
            .map(|i| {
                let angle = 2.0 * PI * i as f32 / SEGMENTS as f32;
                [r * angle.cos(), r * angle.sin()]
            })
            .collect();

        // Top and bottom caps (triangle fans)
        for i in 0..SEGMENTS {
            let j = (i + 1) % SEGMENTS;
            // Bottom cap
            let c = self.transform_point(0.0, 0.0, -hh);
            let p0 = self.transform_point(circle[i][0], circle[i][1], -hh);
            let p1 = self.transform_point(circle[j][0], circle[j][1], -hh);
            verts.extend_from_slice(&c);
            verts.extend_from_slice(&p1);
            verts.extend_from_slice(&p0);

            // Top cap
            let ct = self.transform_point(0.0, 0.0, hh);
            let t0 = self.transform_point(circle[i][0], circle[i][1], hh);
            let t1 = self.transform_point(circle[j][0], circle[j][1], hh);
            verts.extend_from_slice(&ct);
            verts.extend_from_slice(&t0);
            verts.extend_from_slice(&t1);

            // Side quad (2 triangles)
            let b0 = self.transform_point(circle[i][0], circle[i][1], -hh);
            let b1 = self.transform_point(circle[j][0], circle[j][1], -hh);
            // t0, t1 already computed
            // Triangle 1
            verts.extend_from_slice(&b0);
            verts.extend_from_slice(&b1);
            verts.extend_from_slice(&t1);
            // Triangle 2
            verts.extend_from_slice(&b0);
            verts.extend_from_slice(&t1);
            verts.extend_from_slice(&t0);
        }

        verts
    }

    /// Generate wireframe edge vertices for this obstacle.
    /// Returns flat [x,y,z, x,y,z, ...] suitable for GL_LINES.
    pub fn wireframe(&self) -> Vec<f32> {
        match self.kind {
            ObstacleKind::Box => self.wireframe_box(),
            ObstacleKind::Cylinder => self.wireframe_cylinder(),
        }
    }

    fn wireframe_box(&self) -> Vec<f32> {
        let hw = self.width / 2.0;
        let hd = self.depth / 2.0;
        let hh = self.height / 2.0;

        let corners = [
            [-hw, -hd, -hh],
            [ hw, -hd, -hh],
            [ hw,  hd, -hh],
            [-hw,  hd, -hh],
            [-hw, -hd,  hh],
            [ hw, -hd,  hh],
            [ hw,  hd,  hh],
            [-hw,  hd,  hh],
        ];

        let wc: Vec<[f32; 3]> = corners
            .iter()
            .map(|c| self.transform_point(c[0], c[1], c[2]))
            .collect();

        let edges: [(usize, usize); 12] = [
            (0, 1), (1, 2), (2, 3), (3, 0), // bottom
            (4, 5), (5, 6), (6, 7), (7, 4), // top
            (0, 4), (1, 5), (2, 6), (3, 7), // verticals
        ];

        let mut verts = Vec::with_capacity(12 * 2 * 3);
        for (a, b) in &edges {
            verts.extend_from_slice(&wc[*a]);
            verts.extend_from_slice(&wc[*b]);
        }
        verts
    }

    fn wireframe_cylinder(&self) -> Vec<f32> {
        const SEGMENTS: usize = 24;
        let r = self.radius;
        let hh = self.height / 2.0;

        let circle: Vec<[f32; 2]> = (0..SEGMENTS)
            .map(|i| {
                let angle = 2.0 * PI * i as f32 / SEGMENTS as f32;
                [r * angle.cos(), r * angle.sin()]
            })
            .collect();

        let mut verts = Vec::new();
        for i in 0..SEGMENTS {
            let j = (i + 1) % SEGMENTS;
            // Bottom circle
            let b0 = self.transform_point(circle[i][0], circle[i][1], -hh);
            let b1 = self.transform_point(circle[j][0], circle[j][1], -hh);
            verts.extend_from_slice(&b0);
            verts.extend_from_slice(&b1);
            // Top circle
            let t0 = self.transform_point(circle[i][0], circle[i][1], hh);
            let t1 = self.transform_point(circle[j][0], circle[j][1], hh);
            verts.extend_from_slice(&t0);
            verts.extend_from_slice(&t1);
            // Vertical lines (every 4th segment for clarity)
            if i % 4 == 0 {
                verts.extend_from_slice(&b0);
                verts.extend_from_slice(&t0);
            }
        }
        verts
    }
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
struct SceneAnchor {
    x: f32,
    y: f32,
    z: f32,
}

#[derive(Serialize, Deserialize)]
pub struct PlanningScene {
    pub room_x: f32,
    pub room_y: f32,
    pub room_z: f32,
    pub resolution: f32,
    #[serde(default)]
    pub center_origin: bool,
    base_stations: Vec<SceneBaseStation>,
    anchors: Vec<SceneAnchor>,
    #[serde(default)]
    obstacles: Vec<Obstacle>,
    #[serde(default = "default_true")]
    pub receiver_fov_enabled: bool,
    #[serde(default = "default_max_bs_dist")]
    pub max_bs_distance: f32,
    #[serde(default = "default_show_coverage")]
    pub show_coverage: [bool; 5],
    #[serde(default = "default_max_range")]
    pub max_range: f32,
    #[serde(default)]
    pub tdoa3_scale_min: f32,
    #[serde(default = "default_tdoa3_scale_max")]
    pub tdoa3_scale_max: f32,
}

fn default_true() -> bool {
    true
}
fn default_max_bs_dist() -> f32 {
    5.0
}
fn default_show_coverage() -> [bool; 5] {
    [true, true, true, true, true]
}
fn default_max_range() -> f32 {
    15.0
}
fn default_tdoa3_scale_max() -> f32 {
    0.5
}

impl PlanningScene {
    pub fn new(
        room_x: f32,
        room_y: f32,
        room_z: f32,
        resolution: f32,
        center_origin: bool,
        base_stations: &[coverage::BaseStation],
        anchors: &[tdoa3::Anchor],
        obstacles: &[Obstacle],
        receiver_fov_enabled: bool,
        max_bs_distance: f32,
        show_coverage: [bool; 5],
        max_range: f32,
        tdoa3_scale_min: f32,
        tdoa3_scale_max: f32,
    ) -> Self {
        Self {
            room_x,
            room_y,
            room_z,
            resolution,
            center_origin,
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
            anchors: anchors
                .iter()
                .map(|a| SceneAnchor {
                    x: a.pos[0],
                    y: a.pos[1],
                    z: a.pos[2],
                })
                .collect(),
            obstacles: obstacles.to_vec(),
            receiver_fov_enabled,
            max_bs_distance,
            show_coverage,
            max_range,
            tdoa3_scale_min,
            tdoa3_scale_max,
        }
    }

    pub fn base_stations(&self) -> Vec<coverage::BaseStation> {
        self.base_stations
            .iter()
            .map(|s| coverage::BaseStation {
                pos: [s.x, s.y, s.z],
                azimuth_deg: s.azimuth_deg,
                elevation_deg: s.elevation_deg,
            })
            .collect()
    }

    pub fn anchors(&self) -> Vec<tdoa3::Anchor> {
        self.anchors
            .iter()
            .map(|s| tdoa3::Anchor {
                pos: [s.x, s.y, s.z],
            })
            .collect()
    }

    pub fn obstacles(&self) -> Vec<Obstacle> {
        self.obstacles.clone()
    }
}

pub fn save_scene(path: &std::path::Path, scene: &PlanningScene) -> Result<(), String> {
    let content =
        serde_yaml::to_string(scene).map_err(|e| format!("Failed to serialize: {}", e))?;
    std::fs::write(path, content).map_err(|e| format!("Failed to write file: {}", e))
}

pub fn load_scene(path: &std::path::Path) -> Result<PlanningScene, String> {
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("Failed to read file: {}", e))?;
    serde_yaml::from_str(&content).map_err(|e| format!("Failed to parse scene: {}", e))
}

/// Test if any obstacle blocks the ray from `origin` to `target`.
pub fn any_obstacle_blocks(obstacles: &[Obstacle], origin: [f32; 3], target: [f32; 3]) -> bool {
    obstacles.iter().any(|o| o.blocks_ray(origin, target))
}

/// Compute LH coverage with obstacle occlusion.
/// Wraps `coverage::compute_coverage` then removes coverage for voxels occluded by obstacles.
pub fn compute_coverage_with_obstacles(
    room_x: f32,
    room_y: f32,
    room_z: f32,
    resolution: f32,
    base_stations: &[coverage::BaseStation],
    horiz_deg: f32,
    vert_deg: f32,
    receiver_fov_deg: Option<f32>,
    tilt_reduction_deg: Option<f32>,
    max_dist: f32,
    offset: [f32; 3],
    obstacles: &[Obstacle],
) -> coverage::CoverageResult {
    if obstacles.is_empty() {
        return coverage::compute_coverage(
            room_x, room_y, room_z, resolution, base_stations,
            horiz_deg, vert_deg, receiver_fov_deg, tilt_reduction_deg,
            max_dist, offset,
        );
    }

    // Compute per-BS coverage with occlusion
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

                    let dx = wx - bs.pos[0];
                    let dy = wy - bs.pos[1];
                    let dz = wz - bs.pos[2];

                    let lx = rt[0][0] * dx + rt[0][1] * dy + rt[0][2] * dz;
                    let ly = rt[1][0] * dx + rt[1][1] * dy + rt[1][2] * dz;
                    let lz = rt[2][0] * dx + rt[2][1] * dy + rt[2][2] * dz;

                    if lx <= 0.0 { continue; }

                    let dist = (lx * lx + ly * ly + lz * lz).sqrt();
                    if dist > max_dist { continue; }

                    let ang_h = ly.atan2(lx).abs();
                    let ang_v = lz.atan2(lx).abs();

                    if ang_h < horiz_half && ang_v < vert_half {
                        if let Some(rx_half) = receiver_half {
                            let cos_angle = (-dz) / dist;
                            if cos_angle.acos() > rx_half { continue; }
                        }

                        // Obstacle occlusion check
                        if any_obstacle_blocks(obstacles, bs.pos, [wx, wy, wz]) {
                            continue;
                        }

                        voxels[iz][iy][ix] = voxels[iz][iy][ix].saturating_add(1);
                    }
                }
            }
        }
    }

    // Also mark voxels inside obstacles as 0 coverage
    for iz in 0..nz {
        for iy in 0..ny {
            for ix in 0..nx {
                if voxels[iz][iy][ix] > 0 {
                    let wx = ix as f32 / resolution + offset[0];
                    let wy = iy as f32 / resolution + offset[1];
                    let wz = iz as f32 / resolution + offset[2];
                    for obs in obstacles {
                        if obs.contains_point([wx, wy, wz]) {
                            voxels[iz][iy][ix] = 0;
                            break;
                        }
                    }
                }
            }
        }
    }

    coverage::CoverageResult::new(resolution, voxels)
}

/// Ray-AABB intersection test (slab method).
/// Ray: P(t) = origin + t * dir, t in [0, 1].
fn ray_aabb_intersect(origin: [f32; 3], dir: [f32; 3], min: [f32; 3], max: [f32; 3]) -> bool {
    let mut tmin = 0.0_f32;
    let mut tmax = 1.0_f32;

    for i in 0..3 {
        if dir[i].abs() < 1e-12 {
            // Ray parallel to slab — check if origin is inside
            if origin[i] < min[i] || origin[i] > max[i] {
                return false;
            }
        } else {
            let inv_d = 1.0 / dir[i];
            let mut t1 = (min[i] - origin[i]) * inv_d;
            let mut t2 = (max[i] - origin[i]) * inv_d;
            if t1 > t2 {
                std::mem::swap(&mut t1, &mut t2);
            }
            tmin = tmin.max(t1);
            tmax = tmax.min(t2);
            if tmin > tmax {
                return false;
            }
        }
    }
    true
}

/// Ray-cylinder intersection test (axis-aligned along Z in local frame).
/// Ray: P(t) = origin + t * dir, t in [0, 1].
/// Cylinder: x² + y² ≤ r², -hh ≤ z ≤ hh.
fn ray_cylinder_intersect(origin: [f32; 3], dir: [f32; 3], r: f32, hh: f32) -> bool {
    // First check infinite cylinder (x² + y² = r²)
    let a = dir[0] * dir[0] + dir[1] * dir[1];
    let b = 2.0 * (origin[0] * dir[0] + origin[1] * dir[1]);
    let c = origin[0] * origin[0] + origin[1] * origin[1] - r * r;

    let mut tmin = 0.0_f32;
    let mut tmax = 1.0_f32;

    if a.abs() < 1e-12 {
        // Ray nearly parallel to cylinder axis
        if c > 0.0 {
            return false; // outside radius
        }
        // Inside radius, just check Z caps
    } else {
        let disc = b * b - 4.0 * a * c;
        if disc < 0.0 {
            return false;
        }
        let sqrt_disc = disc.sqrt();
        let t1 = (-b - sqrt_disc) / (2.0 * a);
        let t2 = (-b + sqrt_disc) / (2.0 * a);
        tmin = tmin.max(t1);
        tmax = tmax.min(t2);
        if tmin > tmax {
            return false;
        }
    }

    // Now clip to Z caps: -hh ≤ z ≤ hh
    if dir[2].abs() < 1e-12 {
        if origin[2] < -hh || origin[2] > hh {
            return false;
        }
    } else {
        let inv_dz = 1.0 / dir[2];
        let mut tz1 = (-hh - origin[2]) * inv_dz;
        let mut tz2 = (hh - origin[2]) * inv_dz;
        if tz1 > tz2 {
            std::mem::swap(&mut tz1, &mut tz2);
        }
        tmin = tmin.max(tz1);
        tmax = tmax.min(tz2);
        if tmin > tmax {
            return false;
        }
    }

    true
}
