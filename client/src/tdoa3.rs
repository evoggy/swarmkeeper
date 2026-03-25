/// TDoA3 anchor coverage and GDOP (Geometric Dilution of Precision) computation.
///
/// For each point in space, computes the GDOP from TDoA measurements between
/// all anchor pairs within range. Lower GDOP means the geometry is favorable
/// for accurate positioning.

use serde::{Deserialize, Serialize};

/// A single TDoA3 anchor at a known position.
#[derive(Clone, Debug)]
pub struct Anchor {
    pub pos: [f32; 3],
}

/// Available metrics for visualization.
pub const METRIC_GDOP: usize = 0;
pub const METRIC_HDOP: usize = 1;
pub const METRIC_VDOP: usize = 2;
pub const METRIC_PAIRS: usize = 3;
pub const METRIC_PAIR_SENSITIVITY: usize = 4;

/// Per-voxel metrics: `[GDOP, HDOP, VDOP, pair_count]`.
/// GDOP/HDOP/VDOP are `f32::INFINITY` when fewer than 3 anchor pairs are in range.
/// `pair_count` is always finite.
type VoxelMetrics = [f32; 4];

/// Result of GDOP computation over a voxel grid.
pub struct GdopResult {
    resolution: f32,
    /// Per-voxel metrics indexed as `[iz][iy][ix]`.
    voxels: Vec<Vec<Vec<VoxelMetrics>>>,
}

impl GdopResult {
    /// Iterate over all voxels for a given metric, yielding `(world_x, world_y, world_z, value)`.
    pub fn iter_voxels(&self, offset: [f32; 3], metric: usize) -> impl Iterator<Item = (f32, f32, f32, f32)> + '_ {
        let res = self.resolution;
        self.voxels.iter().enumerate().flat_map(move |(iz, plane)| {
            plane.iter().enumerate().flat_map(move |(iy, row)| {
                row.iter().enumerate().map(move |(ix, v)| {
                    (
                        ix as f32 / res + offset[0],
                        iy as f32 / res + offset[1],
                        iz as f32 / res + offset[2],
                        v[metric],
                    )
                })
            })
        })
    }

    /// Fraction of voxels where the given metric is finite and at or below the threshold.
    /// For pair count, use this as "voxels with >= threshold pairs" by passing the negated
    /// values — or use `coverage_ratio_pairs` instead.
    pub fn coverage_ratio(&self, metric: usize, max_val: f32) -> f32 {
        let mut count = 0u32;
        let mut total = 0u32;
        for plane in &self.voxels {
            for row in plane {
                for v in row {
                    total += 1;
                    let g = v[metric];
                    if g.is_finite() && g <= max_val {
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

    /// Fraction of voxels with at least `min_pairs` anchor pairs in range.
    pub fn coverage_ratio_pairs(&self, min_pairs: f32) -> f32 {
        let mut count = 0u32;
        let mut total = 0u32;
        for plane in &self.voxels {
            for row in plane {
                for v in row {
                    total += 1;
                    if v[METRIC_PAIRS] >= min_pairs {
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

    /// Statistics over finite voxels for the given metric: `(min, max, mean)`.
    /// For pair count all voxels are finite.
    pub fn stats(&self, metric: usize) -> (f32, f32, f32) {
        let mut min = f32::INFINITY;
        let mut max = 0.0f32;
        let mut sum = 0.0f64;
        let mut count = 0u32;
        for plane in &self.voxels {
            for row in plane {
                for v in row {
                    let g = v[metric];
                    if g.is_finite() {
                        min = min.min(g);
                        max = max.max(g);
                        sum += g as f64;
                        count += 1;
                    }
                }
            }
        }
        if count == 0 {
            (0.0, 0.0, 0.0)
        } else {
            (min, max, (sum / count as f64) as f32)
        }
    }
}

/// Compute GDOP for every voxel in the room.
///
/// The GDOP is derived from the TDoA measurement Jacobian. For each pair of
/// anchors `(i, j)` within range, one row of the Jacobian `H` is:
///
/// ```text
/// h = (pos - anchor_i) / d_i  -  (pos - anchor_j) / d_j
/// ```
///
/// Then `GDOP = sqrt(trace((H^T H)^{-1}))`.
pub fn compute_gdop(
    room_x: f32,
    room_y: f32,
    room_z: f32,
    resolution: f32,
    anchors: &[Anchor],
    max_range: f32,
    offset: [f32; 3],
) -> GdopResult {
    let nx = (room_x * resolution) as usize + 1;
    let ny = (room_y * resolution) as usize + 1;
    let nz = (room_z * resolution) as usize + 1;

    let inf_metrics: VoxelMetrics = [f32::INFINITY, f32::INFINITY, f32::INFINITY, 0.0];
    let mut voxels = vec![vec![vec![inf_metrics; nx]; ny]; nz];

    // Precompute all anchor pair indices
    let n_anchors = anchors.len();
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    for i in 0..n_anchors {
        for j in (i + 1)..n_anchors {
            pairs.push((i, j));
        }
    }

    let mut dists = vec![0.0f32; n_anchors];
    let mut in_range = vec![false; n_anchors];

    for iz in 0..nz {
        for iy in 0..ny {
            for ix in 0..nx {
                let wx = ix as f32 / resolution + offset[0];
                let wy = iy as f32 / resolution + offset[1];
                let wz = iz as f32 / resolution + offset[2];

                // Compute distance to each anchor
                for (idx, anchor) in anchors.iter().enumerate() {
                    let d = dist3(&[wx, wy, wz], &anchor.pos);
                    dists[idx] = d;
                    in_range[idx] = d <= max_range && d > 1e-6;
                }

                // Build Jacobian rows from all in-range pairs
                let mut h_rows: Vec<[f32; 3]> = Vec::new();
                for &(i, j) in &pairs {
                    if in_range[i] && in_range[j] {
                        let d_i = dists[i];
                        let d_j = dists[j];
                        let ai = &anchors[i].pos;
                        let aj = &anchors[j].pos;
                        h_rows.push([
                            (wx - ai[0]) / d_i - (wx - aj[0]) / d_j,
                            (wy - ai[1]) / d_i - (wy - aj[1]) / d_j,
                            (wz - ai[2]) / d_i - (wz - aj[2]) / d_j,
                        ]);
                    }
                }

                let pair_count = h_rows.len() as f32;
                voxels[iz][iy][ix][METRIC_PAIRS] = pair_count;

                // Need at least 3 linearly independent rows for a 3D solution
                if h_rows.len() >= 3 {
                    // Compute H^T H (3×3 symmetric)
                    let mut hth = [[0.0f32; 3]; 3];
                    for row in &h_rows {
                        for r in 0..3 {
                            for c in 0..3 {
                                hth[r][c] += row[r] * row[c];
                            }
                        }
                    }

                    if let Some(inv) = invert_3x3(&hth) {
                        let gdop = (inv[0][0] + inv[1][1] + inv[2][2]).sqrt();
                        let hdop = (inv[0][0] + inv[1][1]).sqrt();
                        let vdop = inv[2][2].sqrt();
                        if gdop.is_finite() {
                            voxels[iz][iy][ix][METRIC_GDOP] = gdop;
                            voxels[iz][iy][ix][METRIC_HDOP] = hdop;
                            voxels[iz][iy][ix][METRIC_VDOP] = vdop;
                        }
                    }
                }
            }
        }
    }

    GdopResult { resolution, voxels }
}

/// Compute the TDoA measurement sensitivity `|h|` for a single anchor pair
/// at every voxel in the room.
///
/// `|h| = |(pos - a_i)/d_i - (pos - a_j)/d_j|`
///
/// Ranges from 0 (receiver equidistant and anchors in the same direction — on
/// the perpendicular bisector, far away) to 2 (receiver on the baseline between
/// the anchors). High values mean the TDoA measurement is informative about
/// position; low values mean it's geometrically degenerate.
pub fn compute_pair_sensitivity(
    room_x: f32,
    room_y: f32,
    room_z: f32,
    resolution: f32,
    anchor_a: [f32; 3],
    anchor_b: [f32; 3],
    max_range: f32,
    offset: [f32; 3],
) -> Vec<(f32, f32, f32, f32)> {
    let nx = (room_x * resolution) as usize + 1;
    let ny = (room_y * resolution) as usize + 1;
    let nz = (room_z * resolution) as usize + 1;

    let mut result = Vec::with_capacity(nx * ny * nz);

    for iz in 0..nz {
        for iy in 0..ny {
            for ix in 0..nx {
                let wx = ix as f32 / resolution + offset[0];
                let wy = iy as f32 / resolution + offset[1];
                let wz = iz as f32 / resolution + offset[2];

                let da = dist3(&[wx, wy, wz], &anchor_a);
                let db = dist3(&[wx, wy, wz], &anchor_b);

                let sensitivity = if da > 1e-6 && db > 1e-6 && da <= max_range && db <= max_range {
                    let hx = (wx - anchor_a[0]) / da - (wx - anchor_b[0]) / db;
                    let hy = (wy - anchor_a[1]) / da - (wy - anchor_b[1]) / db;
                    let hz = (wz - anchor_a[2]) / da - (wz - anchor_b[2]) / db;
                    (hx * hx + hy * hy + hz * hz).sqrt()
                } else {
                    f32::INFINITY // out of range
                };

                result.push((wx, wy, wz, sensitivity));
            }
        }
    }

    result
}

/// Compute basic stats over a flat voxel list (for pair sensitivity).
pub fn voxel_stats(voxels: &[(f32, f32, f32, f32)]) -> (f32, f32, f32) {
    let mut min = f32::INFINITY;
    let mut max = 0.0f32;
    let mut sum = 0.0f64;
    let mut count = 0u32;
    for &(_, _, _, v) in voxels {
        if v.is_finite() {
            min = min.min(v);
            max = max.max(v);
            sum += v as f64;
            count += 1;
        }
    }
    if count == 0 {
        (0.0, 0.0, 0.0)
    } else {
        (min, max, (sum / count as f64) as f32)
    }
}

fn dist3(a: &[f32; 3], b: &[f32; 3]) -> f32 {
    let dx = a[0] - b[0];
    let dy = a[1] - b[1];
    let dz = a[2] - b[2];
    (dx * dx + dy * dy + dz * dz).sqrt()
}

/// Invert a 3×3 matrix using the adjugate method.
fn invert_3x3(m: &[[f32; 3]; 3]) -> Option<[[f32; 3]; 3]> {
    let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);

    if det.abs() < 1e-12 {
        return None;
    }

    let inv_det = 1.0 / det;

    Some([
        [
            (m[1][1] * m[2][2] - m[1][2] * m[2][1]) * inv_det,
            (m[0][2] * m[2][1] - m[0][1] * m[2][2]) * inv_det,
            (m[0][1] * m[1][2] - m[0][2] * m[1][1]) * inv_det,
        ],
        [
            (m[1][2] * m[2][0] - m[1][0] * m[2][2]) * inv_det,
            (m[0][0] * m[2][2] - m[0][2] * m[2][0]) * inv_det,
            (m[0][2] * m[1][0] - m[0][0] * m[1][2]) * inv_det,
        ],
        [
            (m[1][0] * m[2][1] - m[1][1] * m[2][0]) * inv_det,
            (m[0][1] * m[2][0] - m[0][0] * m[2][1]) * inv_det,
            (m[0][0] * m[1][1] - m[0][1] * m[1][0]) * inv_det,
        ],
    ])
}

// --- Scene save/load ---

#[derive(Serialize, Deserialize)]
struct SceneAnchor {
    x: f32,
    y: f32,
    z: f32,
}

#[derive(Serialize, Deserialize)]
pub struct Tdoa3Scene {
    pub room_x: f32,
    pub room_y: f32,
    pub room_z: f32,
    pub resolution: f32,
    pub center_origin: bool,
    pub max_range: f32,
    pub max_gdop_display: f32,
    pub show_uncovered: bool,
    anchors: Vec<SceneAnchor>,
}

impl Tdoa3Scene {
    pub fn new(
        room_x: f32,
        room_y: f32,
        room_z: f32,
        resolution: f32,
        center_origin: bool,
        max_range: f32,
        max_gdop_display: f32,
        show_uncovered: bool,
        anchors: &[Anchor],
    ) -> Self {
        Self {
            room_x,
            room_y,
            room_z,
            resolution,
            center_origin,
            max_range,
            max_gdop_display,
            show_uncovered,
            anchors: anchors
                .iter()
                .map(|a| SceneAnchor {
                    x: a.pos[0],
                    y: a.pos[1],
                    z: a.pos[2],
                })
                .collect(),
        }
    }

    pub fn anchors(&self) -> Vec<Anchor> {
        self.anchors
            .iter()
            .map(|s| Anchor {
                pos: [s.x, s.y, s.z],
            })
            .collect()
    }
}

pub fn save_scene(path: &std::path::Path, scene: &Tdoa3Scene) -> Result<(), String> {
    let content =
        serde_yaml::to_string(scene).map_err(|e| format!("Failed to serialize: {}", e))?;
    std::fs::write(path, content).map_err(|e| format!("Failed to write file: {}", e))
}

pub fn load_scene(path: &std::path::Path) -> Result<Tdoa3Scene, String> {
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("Failed to read file: {}", e))?;
    serde_yaml::from_str(&content).map_err(|e| format!("Failed to parse scene: {}", e))
}
