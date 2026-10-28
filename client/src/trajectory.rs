//! Trajectories: files, generated shapes, compression and upload.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::planning;
use crate::swarm::SwarmState;

#[derive(Deserialize, Serialize, Clone)]
pub(crate) struct TrajectoryConfig {
    pub(crate) segments: Vec<TrajectorySegmentYaml>,
}

#[derive(Deserialize, Serialize, Clone)]
pub(crate) struct TrajectorySegmentYaml {
    pub(crate) duration: f32,
    pub(crate) x: Vec<f32>,
    pub(crate) y: Vec<f32>,
    pub(crate) z: Vec<f32>,
    pub(crate) yaw: Vec<f32>,
}

pub(crate) fn eval_poly(coeffs: &[f32], t: f32) -> f32 {
    let mut result = 0.0f32;
    let mut t_pow = 1.0f32;
    for &c in coeffs {
        result += c * t_pow;
        t_pow *= t;
    }
    result
}

pub(crate) fn sample_trajectory(config: &TrajectoryConfig) -> Vec<[f32; 3]> {
    let mut points = Vec::new();
    let steps_per_segment = 20;
    for seg in &config.segments {
        for step in 0..steps_per_segment {
            let t = seg.duration * step as f32 / steps_per_segment as f32;
            points.push([
                eval_poly(&seg.x, t),
                eval_poly(&seg.y, t),
                eval_poly(&seg.z, t),
            ]);
        }
    }
    // Add final point of last segment
    if let Some(seg) = config.segments.last() {
        points.push([
            eval_poly(&seg.x, seg.duration),
            eval_poly(&seg.y, seg.duration),
            eval_poly(&seg.z, seg.duration),
        ]);
    }
    points
}

/// The polynomial trajectory last uploaded to a unit, kept so it can be written back
/// out to a YAML file by "Download Trajectory". Generated shapes (circle, wave, ...) only
/// ever exist in memory otherwise, so without this they cannot be saved or replayed later.
/// `name` seeds the save dialog's filename.
#[derive(Clone)]
pub(crate) struct UploadedTrajectory {
    pub(crate) name: String,
    pub(crate) config: TrajectoryConfig,
}

#[derive(Default, Clone)]
pub(crate) struct TrajectoryData {
    pub(crate) points: Vec<[f32; 3]>,
    pub(crate) duration: f32,
    pub(crate) anchor: Option<[f32; 3]>,
    pub(crate) saved_points: Option<Vec<[f32; 3]>>,
    pub(crate) uploaded: Option<UploadedTrajectory>,
}

pub(crate) type SharedTrajectoryData = Arc<Mutex<HashMap<usize, TrajectoryData>>>;

/// Active HLC goto targets per unit index (absolute world coords). Set when a
/// goto is issued and removed once the unit reaches the point, so the
/// visualization can render a marker only while the move is in progress.
pub(crate) type SharedGotoTargets = Arc<Mutex<HashMap<usize, [f32; 3]>>>;

/// Nominal path speed (m/s) used to time the generated primitive trajectories.
pub(crate) const PRIMITIVE_SPEED: f32 = 0.5;

/// A minimum-jerk (rest-to-rest) polynomial segment from `start` to `end` over
/// `duration` seconds. Velocity and acceleration are zero at both ends, so chained
/// segments trace straight edges with smooth stops at the corners. Coefficients are
/// in the monomial basis expected by `eval_poly` / the firmware (position in t).
pub(crate) fn min_jerk_segment(start: [f32; 3], end: [f32; 3], duration: f32) -> TrajectorySegmentYaml {
    let t = duration.max(0.001);
    let axis = |s: f32, e: f32| -> Vec<f32> {
        let d = e - s;
        vec![
            s,
            0.0,
            0.0,
            10.0 * d / t.powi(3),
            -15.0 * d / t.powi(4),
            6.0 * d / t.powi(5),
        ]
    };
    TrajectorySegmentYaml {
        duration,
        x: axis(start[0], end[0]),
        y: axis(start[1], end[1]),
        z: axis(start[2], end[2]),
        yaw: vec![0.0],
    }
}

/// A cubic Bézier arc (XY plane, constant `z`) expressed as monomial polynomial
/// coefficients over `duration` seconds. Four quarter-arc Béziers stitch into a
/// smooth, velocity-continuous circle.
pub(crate) fn bezier_segment(
    p0: [f32; 2],
    p1: [f32; 2],
    p2: [f32; 2],
    p3: [f32; 2],
    z: f32,
    duration: f32,
) -> TrajectorySegmentYaml {
    let t = duration.max(0.001);
    let axis = |a: f32, b: f32, c: f32, d: f32| -> Vec<f32> {
        // Bézier basis -> monomial in tau, then rescale tau = t / T.
        let c0 = a;
        let c1 = 3.0 * (b - a);
        let c2 = 3.0 * (a - 2.0 * b + c);
        let c3 = -a + 3.0 * b - 3.0 * c + d;
        vec![c0, c1 / t, c2 / (t * t), c3 / (t * t * t)]
    };
    TrajectorySegmentYaml {
        duration,
        x: axis(p0[0], p1[0], p2[0], p3[0]),
        y: axis(p0[1], p1[1], p2[1], p3[1]),
        z: vec![z],
        yaw: vec![0.0],
    }
}

/// Clamp a shape side to a sane minimum length while keeping its sign: a negative side
/// builds the shape towards decreasing coordinates instead of increasing ones, so a
/// drone parked at the far end of the arena can still fly a box back into it.
pub(crate) fn signed_extent(side: f32) -> f32 {
    if side < 0.0 { side.min(-0.05) } else { side.max(0.05) }
}

/// Circle of the given radius in the horizontal plane, built from four
/// quarter-arc cubic Béziers (starts and ends at (r, 0), traversed CCW).
/// A negative radius describes the same circle, so only the magnitude is used.
pub(crate) fn generate_circle(radius: f32, speed: f32) -> TrajectoryConfig {
    let r = radius.abs().max(0.05);
    // Control-point offset for a 90° cubic-Bézier circle approximation.
    let kappa = 0.552_284_75 * r;
    let dur = (std::f32::consts::FRAC_PI_2 * r / speed.max(0.05)).max(0.4);
    let mut segments = Vec::new();
    for q in 0..4 {
        let a0 = q as f32 * std::f32::consts::FRAC_PI_2;
        let a1 = a0 + std::f32::consts::FRAC_PI_2;
        let s = [r * a0.cos(), r * a0.sin()];
        let e = [r * a1.cos(), r * a1.sin()];
        let ts = [-a0.sin(), a0.cos()]; // unit tangent, CCW
        let te = [-a1.sin(), a1.cos()];
        let p1 = [s[0] + kappa * ts[0], s[1] + kappa * ts[1]];
        let p2 = [e[0] - kappa * te[0], e[1] - kappa * te[1]];
        segments.push(bezier_segment(s, p1, p2, e, 0.0, dur));
    }
    TrajectoryConfig { segments }
}

/// Square of the given side length in the horizontal plane (four min-jerk edges).
/// A negative side mirrors the square into -X/-Y, so it grows away from the start
/// position in the other direction.
pub(crate) fn generate_square(side: f32, speed: f32) -> TrajectoryConfig {
    let l = signed_extent(side);
    let dur = (l.abs() / speed.max(0.05)).max(0.4);
    let corners = [
        [0.0, 0.0, 0.0],
        [l, 0.0, 0.0],
        [l, l, 0.0],
        [0.0, l, 0.0],
        [0.0, 0.0, 0.0],
    ];
    let segments = (0..4).map(|i| min_jerk_segment(corners[i], corners[i + 1], dur)).collect();
    TrajectoryConfig { segments }
}

/// Rectangle `width` (X) by `depth` (Y) in the horizontal plane (four min-jerk edges).
/// Each edge is timed from its own length, so the drone holds the same path speed on the
/// long and the short sides instead of racing along the long ones. Either side may be
/// negative to extend the rectangle along -X and/or -Y instead of +X/+Y.
pub(crate) fn generate_rectangle(width: f32, depth: f32, speed: f32) -> TrajectoryConfig {
    let w = signed_extent(width);
    let d = signed_extent(depth);
    let v = speed.max(0.05);
    let dur_w = (w.abs() / v).max(0.4);
    let dur_d = (d.abs() / v).max(0.4);
    let corners = [
        [0.0, 0.0, 0.0],
        [w, 0.0, 0.0],
        [w, d, 0.0],
        [0.0, d, 0.0],
        [0.0, 0.0, 0.0],
    ];
    let durations = [dur_w, dur_d, dur_w, dur_d];
    let segments = (0..4)
        .map(|i| min_jerk_segment(corners[i], corners[i + 1], durations[i]))
        .collect();
    TrajectoryConfig { segments }
}

/// Cube wireframe of the given side length: a closed 16-edge walk that traces all
/// 12 edges of the cube (four edges are retraced), each edge a min-jerk segment.
/// A negative side mirrors the footprint into -X/-Y; the cube is always built *upwards*
/// from the start height, since the layer below it is usually the floor.
pub(crate) fn generate_cube(side: f32, speed: f32) -> TrajectoryConfig {
    let l = signed_extent(side);
    let up = l.abs();
    let dur = (up / speed.max(0.05)).max(0.4);
    let v = |x: f32, y: f32, z: f32| [x * l, y * l, z * up];
    let seq = [
        v(0., 0., 0.), v(1., 0., 0.), v(1., 1., 0.), v(0., 1., 0.), v(0., 0., 0.),
        v(0., 0., 1.), v(1., 0., 1.), v(1., 1., 1.), v(0., 1., 1.), v(0., 0., 1.),
        v(1., 0., 1.), v(1., 0., 0.), v(1., 1., 0.), v(1., 1., 1.), v(0., 1., 1.),
        v(0., 1., 0.), v(0., 0., 0.),
    ];
    let segments = (0..seq.len() - 1).map(|i| min_jerk_segment(seq[i], seq[i + 1], dur)).collect();
    TrajectoryConfig { segments }
}

/// Build a primitive trajectory by name. `size` is the radius for a circle, the side
/// length for a square/cube and the X side for a rectangle; `size_y` is the rectangle's
/// Y side and is ignored by every other shape. Returns None for an unknown shape.
pub(crate) fn generate_primitive(shape: &str, size: f32, size_y: f32) -> Option<TrajectoryConfig> {
    match shape {
        "circle" => Some(generate_circle(size, PRIMITIVE_SPEED)),
        "square" => Some(generate_square(size, PRIMITIVE_SPEED)),
        "rectangle" => Some(generate_rectangle(size, size_y, PRIMITIVE_SPEED)),
        "cube" => Some(generate_cube(size, PRIMITIVE_SPEED)),
        _ => None,
    }
}

/// Bytes one uncompressed Poly4D segment occupies in the Crazyflie trajectory memory.
pub(crate) const POLY4D_SEGMENT_BYTES: usize = 132;

/// Size of the trajectory memory on the Crazyflie.
pub(crate) const TRAJECTORY_MEMORY_BYTES: usize = 4096;

/// Position of a polynomial segment at time `t`.
pub(crate) fn segment_point(seg: &TrajectorySegmentYaml, t: f32) -> [f32; 3] {
    [eval_poly(&seg.x, t), eval_poly(&seg.y, t), eval_poly(&seg.z, t)]
}

/// Reverse one polynomial in time so it traces the same curve from its end back to its
/// start. Substituting `t -> T - t` into `p(t) = sum_k a_k t^k` and re-expanding gives
/// `b_j = (-1)^j * sum_{k>=j} a_k * C(k,j) * T^(k-j)`.
pub(crate) fn reverse_poly(coeffs: &[f32], duration: f32) -> Vec<f32> {
    let n = coeffs.len();
    let mut out = vec![0.0f32; n];
    for j in 0..n {
        let mut acc = 0.0f32;
        let mut binom = 1.0f32; // C(j, j)
        for k in j..n {
            acc += coeffs[k] * binom * duration.powi((k - j) as i32);
            // C(k, j) -> C(k+1, j)
            binom = binom * (k + 1) as f32 / (k + 1 - j) as f32;
        }
        out[j] = if j % 2 == 0 { acc } else { -acc };
    }
    out
}

/// The whole trajectory flown backwards: segments in reverse order, each reversed in time.
pub(crate) fn reverse_trajectory(config: &TrajectoryConfig) -> TrajectoryConfig {
    let segments = config
        .segments
        .iter()
        .rev()
        .map(|s| TrajectorySegmentYaml {
            duration: s.duration,
            x: reverse_poly(&s.x, s.duration),
            y: reverse_poly(&s.y, s.duration),
            z: reverse_poly(&s.z, s.duration),
            yaw: reverse_poly(&s.yaw, s.duration),
        })
        .collect();
    TrajectoryConfig { segments }
}

/// Lift a trajectory by `dz` metres (only the constant term of z moves).
pub(crate) fn shift_trajectory_z(config: &TrajectoryConfig, dz: f32) -> TrajectoryConfig {
    let segments = config
        .segments
        .iter()
        .map(|s| {
            let mut z = s.z.clone();
            if z.is_empty() {
                z.push(dz);
            } else {
                z[0] += dz;
            }
            TrajectorySegmentYaml { duration: s.duration, x: s.x.clone(), y: s.y.clone(), z, yaw: s.yaw.clone() }
        })
        .collect();
    TrajectoryConfig { segments }
}

/// Stack `iterations` copies of `config` `z_offset` metres apart. Every copy after the
/// first is the previous one flown *backwards*, so it begins directly above the point
/// where the previous one ended and the only move in between is a straight climb (added
/// here as a min-jerk segment). `iterations <= 1` or a zero offset returns `config`
/// unchanged.
pub(crate) fn stack_trajectory_z(
    config: TrajectoryConfig,
    z_offset: f32,
    iterations: u32,
    speed: f32,
) -> TrajectoryConfig {
    if iterations <= 1 || z_offset.abs() < 1e-4 || config.segments.is_empty() {
        return config;
    }
    let climb_dur = (z_offset.abs() / speed.max(0.05)).max(0.4);
    let mut out: Vec<TrajectorySegmentYaml> = config.segments.clone();
    let mut layer = config;
    for _ in 1..iterations {
        let next = shift_trajectory_z(&reverse_trajectory(&layer), z_offset);
        let last = out.last().expect("layer is non-empty");
        let from = segment_point(last, last.duration);
        let to = segment_point(&next.segments[0], 0.0);
        out.push(min_jerk_segment(from, to, climb_dur));
        out.extend(next.segments.iter().cloned());
        layer = next;
    }
    TrajectoryConfig { segments: out }
}

/// Stack `iterations` copies of a waypoint path `z_offset` metres apart, every copy after
/// the first reversed, so each layer starts directly above where the previous one ended
/// and the joining leg is a pure vertical climb. The compressed-trajectory counterpart of
/// [`stack_trajectory_z`].
pub(crate) fn stack_waypoints_z(pts: Vec<[f32; 3]>, z_offset: f32, iterations: u32) -> Vec<[f32; 3]> {
    if iterations <= 1 || z_offset.abs() < 1e-4 || pts.is_empty() {
        return pts;
    }
    let mut out = pts.clone();
    let mut layer = pts;
    for _ in 1..iterations {
        let next: Vec<[f32; 3]> =
            layer.iter().rev().map(|p| [p[0], p[1], p[2] + z_offset]).collect();
        out.extend_from_slice(&next);
        layer = next;
    }
    out
}

/// Ordered waypoint list for a back-and-forth "wave" in the horizontal plane at a
/// constant (relative) z, starting at the origin. `y_amp` is the half-amplitude — Y
/// swings between +y_amp and -y_amp (peak-to-peak 2·y_amp). `x_step` is how far X
/// advances per leg; pass a negative value to walk in the -X direction. `cycles` is the
/// number of full up/down periods.
///
/// When `simultaneous` is false the X and Y moves alternate — X holds while Y swings, Y
/// holds while X advances — tracing a castellated **square wave**. When true, X and Y
/// move together on every leg, tracing a **zig-zag / sawtooth**.
pub(crate) fn wave_waypoints(x_step: f32, y_amp: f32, cycles: u32, simultaneous: bool) -> Vec<[f32; 3]> {
    let a = y_amp.abs();
    let cycles = cycles.max(1);
    let mut pts: Vec<[f32; 3]> = vec![[0.0, 0.0, 0.0]];
    let mut x = 0.0f32;

    if simultaneous {
        // Zig-zag: every leg advances X and flips Y to the opposite extreme at once.
        for leg in 0..(2 * cycles) {
            x += x_step;
            let y = if leg % 2 == 0 { a } else { -a };
            pts.push([x, y, 0.0]);
        }
    } else {
        // Square wave: rise to +amp first, then alternate (advance X, swing Y) so only
        // one axis moves on each leg.
        pts.push([x, a, 0.0]); // initial Y rise, X held
        for _ in 0..cycles {
            x += x_step;
            pts.push([x, a, 0.0]); // advance X, Y held high
            pts.push([x, -a, 0.0]); // swing Y down, X held
            x += x_step;
            pts.push([x, -a, 0.0]); // advance X, Y held low
            pts.push([x, a, 0.0]); // swing Y up, X held
        }
    }
    pts
}

/// Build a back-and-forth wave as a *compressed* trajectory: each leg is a straight,
/// constant-velocity move that the firmware interpolates from the previous endpoint to
/// the stored endpoint, so only the axes that actually change cost any bytes (the rest
/// are flagged "constant"). Yaw is held throughout, and z within a layer. Compressed trajectories are
/// terminated by the firmware at the first zero-duration piece, so a sentinel segment is
/// appended (the 4 KB trajectory memory is persistent and otherwise still holds stale
/// bytes from a previous upload, which would be parsed as bogus extra pieces).
///
/// `z_offset` / `z_iterations` stack the wave at several heights (see
/// [`stack_waypoints_z`]); each extra layer is flown backwards from where the previous
/// one ended, so the joining leg is a pure vertical climb.
///
/// Returns the compressed start point, the segment list (sentinel included), the polyline
/// points for visualization, and the total duration in seconds.
pub(crate) fn build_wave_compressed(
    x_step: f32,
    y_amp: f32,
    cycles: u32,
    simultaneous: bool,
    speed: f32,
    z_offset: f32,
    z_iterations: u32,
) -> (
    crazyflie_connection_server::memory::CompressedStart,
    Vec<crazyflie_connection_server::memory::CompressedSegment>,
    Vec<[f32; 3]>,
    f32,
    TrajectoryConfig,
) {
    use crazyflie_connection_server::memory::{CompressedSegment, CompressedStart};

    let pts = stack_waypoints_z(
        wave_waypoints(x_step, y_amp, cycles, simultaneous),
        z_offset,
        z_iterations,
    );
    let speed = speed.max(0.05);
    let start = CompressedStart::new(pts[0][0], pts[0][1], pts[0][2], 0.0);

    let mut segments = Vec::new();
    // The same legs in the uncompressed poly4d form, so "Download Trajectory" can write
    // the generated wave out as a YAML file the upload path can read back. A compressed
    // piece with one control point interpolates linearly over its duration, so the exact
    // equivalent is a first-degree polynomial per axis.
    let mut poly_segments: Vec<TrajectorySegmentYaml> = Vec::new();
    let mut total = 0.0f32;
    for w in pts.windows(2) {
        let (from, to) = (w[0], w[1]);
        let (dx, dy, dz) = (to[0] - from[0], to[1] - from[1], to[2] - from[2]);
        let d = (dx * dx + dy * dy + dz * dz).sqrt();
        // Drop degenerate zero-length legs (e.g. x_step == 0 in square-wave mode).
        if d < 1e-4 {
            continue;
        }
        let dur = (d / speed).max(0.4);
        total += dur;
        // Linear (1 control point = the endpoint) on axes that move; constant (no control
        // points, holds the previous value) on axes that don't.
        let xe = if dx.abs() > 1e-4 { vec![to[0]] } else { vec![] };
        let ye = if dy.abs() > 1e-4 { vec![to[1]] } else { vec![] };
        let ze = if dz.abs() > 1e-4 { vec![to[2]] } else { vec![] };
        if let Ok(seg) = CompressedSegment::new(dur, xe, ye, ze, vec![]) {
            segments.push(seg);
        }
        poly_segments.push(TrajectorySegmentYaml {
            duration: dur,
            x: vec![from[0], dx / dur],
            y: vec![from[1], dy / dur],
            z: vec![from[2], dz / dur],
            yaw: vec![0.0],
        });
    }
    // Terminating sentinel: a zero-duration, all-constant piece packs to [0, 0, 0],
    // which the firmware parser reads as "duration 0" and stops.
    if let Ok(term) = CompressedSegment::new(0.0, vec![], vec![], vec![], vec![]) {
        segments.push(term);
    }

    (start, segments, pts, total, TrajectoryConfig { segments: poly_segments })
}

/// Upload a trajectory definition to a single unit's trajectory memory, define it
/// as trajectory id 1, and store its sampled points for visualization. Shared by
/// the file-based upload and the generated-primitive buttons. If the unit is not
/// connected, the visualization data is still stored so the path can be previewed.
///
/// On failure the unit's stored trajectory is *removed* rather than left untouched: a
/// half-written or rejected upload means the points we hold no longer describe what the
/// Crazyflie would fly, and a stale path would silently be reused by the visualization
/// and the pre-flight geofence check.
pub(crate) async fn upload_trajectory_config_to_unit(
    original_index: usize,
    traj_config: TrajectoryConfig,
    name: &str,
    swarm_state: SwarmState,
    trajectory_data: SharedTrajectoryData,
) -> Result<(), String> {
    use crazyflie_connection_server::memory::{MemoryType, Poly, Poly4D, TrajectoryMemory};

    let viz_points = sample_trajectory(&traj_config);
    let uploaded = UploadedTrajectory { name: name.to_string(), config: traj_config.clone() };
    let total_duration: f32 = traj_config.segments.iter().map(|s| s.duration).sum();
    let segment_count = traj_config.segments.len();

    // The firmware rejects any write past the end of its 4 KB trajectory buffer
    // (crtpCommanderHighLevelWriteTrajectory), and a rejected chunk aborts the transfer
    // part-way, leaving the unit's memory holding a mix of old and new bytes. Refuse
    // up front so nothing is written at all.
    let needed_bytes = segment_count * POLY4D_SEGMENT_BYTES;
    if needed_bytes > TRAJECTORY_MEMORY_BYTES {
        let max_segments = TRAJECTORY_MEMORY_BYTES / POLY4D_SEGMENT_BYTES;
        trajectory_data.lock().await.remove(&original_index);
        return Err(format!(
            "The trajectory has {} segments ({} bytes) but the Crazyflie's trajectory \
             memory holds at most {} segments ({} bytes). Reduce the Z iterations or pick \
             a simpler shape.",
            segment_count, needed_bytes, max_segments, TRAJECTORY_MEMORY_BYTES
        ));
    }

    let segments: Vec<Poly4D> = traj_config
        .segments
        .iter()
        .map(|s| {
            Poly4D::new(
                s.duration,
                Poly::from_slice(&s.x),
                Poly::from_slice(&s.y),
                Poly::from_slice(&s.z),
                Poly::from_slice(&s.yaw),
            )
        })
        .collect();

    let cf = {
        let state = swarm_state.lock().await;
        match state.get(&original_index) {
            Some(cu) => cu.cf.clone(),
            None => {
                eprintln!("Unit {} is not connected", original_index);
                // Still store visualization data even if not connected.
                let mut td = trajectory_data.lock().await;
                td.insert(original_index, TrajectoryData {
                    points: viz_points,
                    duration: total_duration,
                    anchor: None,
                    saved_points: None,
                    uploaded: Some(uploaded),
                });
                return Ok(());
            }
        }
    };

    let traj_mems = cf.memory.get_memories(Some(MemoryType::Trajectory));
    if let Some(mem) = traj_mems.first() {
        if let Ok(traj_mem) = cf.memory.open_memory::<TrajectoryMemory>(mem).await {
            let written = traj_mem.write_uncompressed(&segments, 0).await;
            match written {
                Ok(bytes) => eprintln!("Uploaded {} bytes of trajectory data", bytes),
                Err(e) => {
                    eprintln!("Failed to upload trajectory: {:?}", e);
                    trajectory_data.lock().await.remove(&original_index);
                    return Err(format!(
                        "Writing the trajectory to unit {} failed: {:?}. The unit's \
                         trajectory memory may be partly overwritten - upload again before \
                         flying it.",
                        original_index, e
                    ));
                }
            }
        }
    }

    if let Err(e) = cf
        .high_level_commander
        .define_trajectory(1, 0, segment_count as u8, None)
        .await
    {
        eprintln!("Failed to define trajectory: {:?}", e);
        trajectory_data.lock().await.remove(&original_index);
        return Err(format!("Defining the trajectory on unit {} failed: {:?}", original_index, e));
    }

    eprintln!("Trajectory uploaded and defined ({} segments, {:.1}s)", segment_count, total_duration);

    let mut td = trajectory_data.lock().await;
    td.insert(original_index, TrajectoryData {
        points: viz_points,
        duration: total_duration,
        anchor: None,
        saved_points: None,
        uploaded: Some(uploaded),
    });
    Ok(())
}

/// Upload a *compressed* trajectory to a unit's trajectory memory, define it as
/// compressed trajectory id 1, and store its polyline points for visualization. The
/// counterpart to [`upload_trajectory_config_to_unit`] for the compressed format; the
/// `segments` slice must already include the terminating zero-duration sentinel (see
/// [`build_wave_compressed`]). If the unit is not connected the visualization data is
/// still stored so the path can be previewed.
pub(crate) async fn upload_compressed_trajectory_to_unit(
    original_index: usize,
    start: crazyflie_connection_server::memory::CompressedStart,
    segments: Vec<crazyflie_connection_server::memory::CompressedSegment>,
    viz_points: Vec<[f32; 3]>,
    total_duration: f32,
    uploaded: Option<UploadedTrajectory>,
    swarm_state: SwarmState,
    trajectory_data: SharedTrajectoryData,
) -> Result<(), String> {
    use crazyflie_lib::subsystems::high_level_commander::TRAJECTORY_TYPE_POLY4D_COMPRESSED;
    use crazyflie_connection_server::memory::{MemoryType, TrajectoryMemory};

    // The sentinel does not count as a flight piece; report the real leg count.
    let piece_count = segments.len().saturating_sub(1);

    let cf = {
        let state = swarm_state.lock().await;
        match state.get(&original_index) {
            Some(cu) => cu.cf.clone(),
            None => {
                eprintln!("Unit {} is not connected", original_index);
                let mut td = trajectory_data.lock().await;
                td.insert(original_index, TrajectoryData {
                    points: viz_points,
                    duration: total_duration,
                    anchor: None,
                    saved_points: None,
                    uploaded,
                });
                return Ok(());
            }
        }
    };

    let traj_mems = cf.memory.get_memories(Some(MemoryType::Trajectory));
    if let Some(mem) = traj_mems.first() {
        if let Ok(traj_mem) = cf.memory.open_memory::<TrajectoryMemory>(mem).await {
            let written = traj_mem.write_compressed(&start, &segments, 0).await;
            match written {
                Ok(bytes) => eprintln!("Uploaded {} bytes of compressed trajectory data", bytes),
                Err(e) => {
                    eprintln!("Failed to upload compressed trajectory: {:?}", e);
                    trajectory_data.lock().await.remove(&original_index);
                    return Err(format!(
                        "Writing the compressed trajectory to unit {} failed: {:?}. The \
                         unit's trajectory memory may be partly overwritten - upload again \
                         before flying it.",
                        original_index, e
                    ));
                }
            }
        }
    }

    // num_pieces is ignored by the firmware for compressed trajectories (it stops at the
    // zero-duration sentinel), but the define command still carries the type.
    if let Err(e) = cf
        .high_level_commander
        .define_trajectory(1, 0, piece_count as u8, Some(TRAJECTORY_TYPE_POLY4D_COMPRESSED))
        .await
    {
        eprintln!("Failed to define compressed trajectory: {:?}", e);
        trajectory_data.lock().await.remove(&original_index);
        return Err(format!(
            "Defining the compressed trajectory on unit {} failed: {:?}",
            original_index, e
        ));
    }

    eprintln!("Compressed trajectory uploaded and defined ({} pieces, {:.1}s)", piece_count, total_duration);

    let mut td = trajectory_data.lock().await;
    td.insert(original_index, TrajectoryData {
        points: viz_points,
        duration: total_duration,
        anchor: None,
        saved_points: None,
        uploaded,
    });
    Ok(())
}

/// How far (metres) units rise above the surface they launched from before executing a
/// trajectory. This is a *relative* rise, not a world-frame height: the world origin is
/// not necessarily the floor and can sit above the units (making their z negative), so
/// the absolute target handed to `take_off` is always `current_z + TRAJECTORY_TAKEOFF_RISE`.
/// Kept as a constant so the pre-flight geofence check anchors the trajectory at exactly
/// the height the unit will actually fly at.
pub(crate) const TRAJECTORY_TAKEOFF_RISE: f32 = 0.5;

/// Verify that a trajectory, anchored at `anchor` with the firmware's relative shift
/// applied (trajectory point 0 maps to `anchor`), stays inside the flight area(s).
/// Returns the first offending absolute point if it would leave the geofence. When no
/// flight areas are defined there is no geofence to enforce, so this always passes.
pub(crate) fn trajectory_within_flight_areas(
    points: &[[f32; 3]],
    anchor: [f32; 3],
    flight_areas: &[planning::BoxVolume],
) -> Result<(), [f32; 3]> {
    if flight_areas.is_empty() {
        return Ok(());
    }
    let first = points.first().copied().unwrap_or([0.0, 0.0, 0.0]);
    for pt in points {
        let abs = [
            pt[0] + anchor[0] - first[0],
            pt[1] + anchor[1] - first[1],
            pt[2] + anchor[2] - first[2],
        ];
        if !planning::point_in_areas(flight_areas, abs) {
            return Err(abs);
        }
    }
    Ok(())
}

#[cfg(test)]
mod trajectory_tests {
    use super::*;

    /// Reversing a segment in time must trace the same curve backwards.
    #[test]
    fn reverse_poly_mirrors_the_curve() {
        let seg = min_jerk_segment([0.3, -1.0, 0.5], [2.1, 0.4, 0.5], 1.7);
        let rev = reverse_poly(&seg.x, seg.duration);
        for i in 0..=20 {
            let t = seg.duration * i as f32 / 20.0;
            let expected = eval_poly(&seg.x, seg.duration - t);
            assert!((eval_poly(&rev, t) - expected).abs() < 1e-3, "t={t}");
        }
    }

    /// Each stacked layer must begin exactly above where the previous one ended, with the
    /// climb segment as the only thing in between.
    #[test]
    fn stacked_layers_are_continuous() {
        let base = generate_square(0.5, PRIMITIVE_SPEED);
        let base_len = base.segments.len();
        let stacked = stack_trajectory_z(base, 0.25, 3, PRIMITIVE_SPEED);

        // 3 layers plus 2 connecting climbs.
        assert_eq!(stacked.segments.len(), 3 * base_len + 2);

        for pair in stacked.segments.windows(2) {
            let end = segment_point(&pair[0], pair[0].duration);
            let start = segment_point(&pair[1], 0.0);
            for axis in 0..3 {
                assert!((end[axis] - start[axis]).abs() < 1e-3, "gap on axis {axis}");
            }
        }

        let first = segment_point(&stacked.segments[0], 0.0);
        let last = stacked.segments.last().unwrap();
        let end = segment_point(last, last.duration);
        // Odd iteration count: the top layer runs forwards again, so it ends above the start.
        assert!((end[0] - first[0]).abs() < 1e-3);
        assert!((end[1] - first[1]).abs() < 1e-3);
        assert!((end[2] - (first[2] + 0.5)).abs() < 1e-3);
    }

    /// Stacking multiplies the segment count, and uncompressed segments are 132 bytes in a
    /// 4 KB buffer - so the fit has to be checked before anything is written to the unit.
    #[test]
    fn stacking_can_overflow_the_trajectory_memory() {
        let max_segments = TRAJECTORY_MEMORY_BYTES / POLY4D_SEGMENT_BYTES;
        assert_eq!(max_segments, 31);

        // A cube is 16 segments; two layers plus the climb is 33 - over the limit.
        let cube = stack_trajectory_z(generate_cube(0.5, PRIMITIVE_SPEED), 0.5, 2, PRIMITIVE_SPEED);
        assert!(cube.segments.len() > max_segments);

        // A 4-segment square still fits at 6 layers (4*6 + 5 climbs = 29).
        let square =
            stack_trajectory_z(generate_square(0.5, PRIMITIVE_SPEED), 0.3, 6, PRIMITIVE_SPEED);
        assert!(square.segments.len() <= max_segments);
    }

    /// A zero offset or a single iteration must leave the path untouched.
    #[test]
    fn stacking_is_a_no_op_when_disabled() {
        let n = generate_circle(0.5, PRIMITIVE_SPEED).segments.len();
        assert_eq!(
            stack_trajectory_z(generate_circle(0.5, PRIMITIVE_SPEED), 0.3, 1, PRIMITIVE_SPEED)
                .segments
                .len(),
            n
        );
        assert_eq!(
            stack_trajectory_z(generate_circle(0.5, PRIMITIVE_SPEED), 0.0, 4, PRIMITIVE_SPEED)
                .segments
                .len(),
            n
        );
    }

    /// The compressed (waypoint) stacker mirrors the polynomial one: reversed layers,
    /// joined by a pure vertical step.
    #[test]
    fn stacked_waypoints_join_vertically() {
        let base = wave_waypoints(0.3, 0.3, 2, true);
        let n = base.len();
        let stacked = stack_waypoints_z(base.clone(), 0.25, 3);

        assert_eq!(stacked.len(), 3 * n);
        // Layer boundary: same XY, one offset up.
        assert_eq!(stacked[n - 1][0], stacked[n][0]);
        assert_eq!(stacked[n - 1][1], stacked[n][1]);
        assert!((stacked[n][2] - (stacked[n - 1][2] + 0.25)).abs() < 1e-6);
        // Second layer is the first reversed.
        for i in 0..n {
            assert_eq!(stacked[n + i][0], base[n - 1 - i][0]);
            assert_eq!(stacked[n + i][1], base[n - 1 - i][1]);
        }
    }
}
