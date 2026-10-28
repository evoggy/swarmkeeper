//! The Planning tab.

use slint::{ComponentHandle, Model};

use crate::app::AppContext;
use crate::settings::AppSettings;
use crate::{AppWindow, coverage, LhBaseStationData, PlanBoxVolumeData, planning, PlanObstacleData, PlanPadData, host::renderer, tdoa3, Tdoa3AnchorData};
use crate::host;

// Planning shared state
pub(crate) struct PlanningState {
    pub(crate) base_stations: Vec<coverage::BaseStation>,
    pub(crate) anchors: Vec<tdoa3::Anchor>,
    pub(crate) obstacles: Vec<planning::Obstacle>,
    pub(crate) lh_voxels: Vec<(f32, f32, f32, u8)>,
    pub(crate) tdoa3_voxels: Vec<(f32, f32, f32, f32)>,
    pub(crate) tdoa3_gdop_result: Option<tdoa3::GdopResult>,
    pub(crate) bs_render_data: Vec<([f32; 3], [[f32; 3]; 3])>,
    pub(crate) anchor_positions: Vec<[f32; 3]>,
    pub(crate) obstacle_triangles: Vec<Vec<f32>>,
    pub(crate) obstacle_wireframes: Vec<Vec<f32>>,
    pub(crate) obstacle_colors: Vec<[f32; 3]>,
    pub(crate) room: [f32; 3],
    pub(crate) room_offset: [f32; 3],
    pub(crate) flight_areas: Vec<planning::BoxVolume>,
    pub(crate) waypoint_areas: Vec<planning::BoxVolume>,
    /// Selected area for handle editing: 0=none, 1=flight, 2=waypoint.
    pub(crate) selected_area_kind: i32,
    pub(crate) selected_area_index: i32,
    pub(crate) takeoff_pads: Vec<planning::TakeoffPad>,
    pub(crate) undo_stack: Vec<(Vec<coverage::BaseStation>, Vec<tdoa3::Anchor>, Vec<planning::Obstacle>)>,
    pub(crate) convex_hull: Option<tdoa3::ConvexHull>,
    pub(crate) trajectories: Vec<Vec<[f32; 3]>>,
    pub(crate) lh_ratio_1: f32,
    pub(crate) lh_ratio_2: f32,
    pub(crate) gdop_mean: f32,
    pub(crate) hull_stats: Option<(f32, f32, f32)>,
    pub(crate) last_metric: i32,
}

pub(crate) struct PlanningGizmoState {
    pub(crate) drag_start_screen: [f32; 2],
    pub(crate) drag_start_pos: [f32; 3],
    pub(crate) drag_start_azimuth: f32,
    pub(crate) drag_start_elevation: f32,
    pub(crate) drag_start_yaw: f32,
    // Area face-handle drag (swarmfinity-style box resize)
    pub(crate) area_handle: i32, // -1 none, else 0..6 (XMin,XMax,YMin,YMax,ZMin,ZMax)
    pub(crate) area_start_min: [f32; 3],
    pub(crate) area_start_max: [f32; 3],
    pub(crate) area_axis_screen: [f32; 2], // screen-space unit vector of the dragged axis
    pub(crate) area_world_per_px: f32,     // world meters per screen pixel along that axis
}

pub(crate) fn setup(ui: &AppWindow, cx: &AppContext) {
    let AppContext { settings: _, viz_scene_state, planning_state, planning_gizmo_state, .. } = cx;
    // Planning callbacks
    {
        let ui_weak = ui.as_weak();

        // Load LH Scene into planning (imports base stations)
        let ps = planning_state.clone();
        let uw = ui_weak.clone();
        ui.on_planning_load_lh_scene(move || {
            let Some(ui) = uw.upgrade() else { return };
            let uw2 = ui.as_weak();
            let ps = ps.clone();
            slint::spawn_local(async move {
                let Some(handle) = host::dialog::FileDialog::new()
                    .set_title("Import LH Scene")
                    .add_filter("YAML", &["yaml", "yml"])
                    .pick_file().await
                else { return };
                let path = handle.path().to_path_buf();
                let Some(ui) = uw2.upgrade() else { return };

                match coverage::load_scene(&path) {
                    Ok(scene) => {
                        let mut pstate = ps.lock().unwrap();
                        pstate.base_stations = scene.base_stations();
                        rebuild_bs_render_data(&mut pstate);

                        // Update room dimensions to match
                        ui.set_planning_room_x(format!("{}", scene.room_x).into());
                        ui.set_planning_room_y(format!("{}", scene.room_y).into());
                        ui.set_planning_room_z(format!("{}", scene.room_z).into());
                        ui.set_planning_center_origin(scene.center_origin);
                        ui.set_planning_room_offset_x(format!("{}", scene.room_offset[0]).into());
                        ui.set_planning_room_offset_y(format!("{}", scene.room_offset[1]).into());
                        ui.set_planning_room_offset_z(format!("{}", scene.room_offset[2]).into());
                        ui.set_planning_receiver_fov_enabled(scene.receiver_fov_enabled);
                        ui.set_planning_max_bs_distance(format!("{}", scene.max_bs_distance).into());

                        let bs_model: Vec<LhBaseStationData> = pstate.base_stations.iter().map(|bs| LhBaseStationData {
                            x: format!("{:.2}", bs.pos[0]).into(),
                            y: format!("{:.2}", bs.pos[1]).into(),
                            z: format!("{:.2}", bs.pos[2]).into(),
                            azimuth: format!("{:.1}", bs.azimuth_deg).into(),
                            elevation: format!("{:.1}", bs.elevation_deg).into(),
                        }).collect();
                        ui.set_planning_base_stations(slint::ModelRc::new(slint::VecModel::from(bs_model)));
                    }
                    Err(e) => eprintln!("Load LH scene error: {}", e),
                }
            }).unwrap();
        });

        // Load TDoA3 Scene into planning (imports anchors)
        let ps = planning_state.clone();
        let uw = ui_weak.clone();
        ui.on_planning_load_tdoa3_scene(move || {
            let Some(ui) = uw.upgrade() else { return };
            let uw2 = ui.as_weak();
            let ps = ps.clone();
            slint::spawn_local(async move {
                let Some(handle) = host::dialog::FileDialog::new()
                    .set_title("Import TDoA3 Scene")
                    .add_filter("YAML", &["yaml", "yml"])
                    .pick_file().await
                else { return };
                let path = handle.path().to_path_buf();
                let Some(ui) = uw2.upgrade() else { return };

                match tdoa3::load_scene(&path) {
                    Ok(scene) => {
                        let mut pstate = ps.lock().unwrap();
                        pstate.anchors = scene.anchors();
                        rebuild_anchor_positions(&mut pstate);

                        // Update room dimensions and range
                        ui.set_planning_room_x(format!("{}", scene.room_x).into());
                        ui.set_planning_room_y(format!("{}", scene.room_y).into());
                        ui.set_planning_room_z(format!("{}", scene.room_z).into());
                        ui.set_planning_center_origin(scene.center_origin);
                        ui.set_planning_max_range(format!("{}", scene.max_range).into());

                        let anchor_model: Vec<Tdoa3AnchorData> = pstate.anchors.iter().map(|a| Tdoa3AnchorData {
                            x: format!("{:.2}", a.pos[0]).into(),
                            y: format!("{:.2}", a.pos[1]).into(),
                            z: format!("{:.2}", a.pos[2]).into(),
                        }).collect();
                        ui.set_planning_anchors(slint::ModelRc::new(slint::VecModel::from(anchor_model)));
                    }
                    Err(e) => eprintln!("Load TDoA3 scene error: {}", e),
                }
            }).unwrap();
        });

        // Helper: rebuild obstacle meshes in state
        fn rebuild_obstacle_meshes(ps: &mut PlanningState) {
            ps.obstacle_triangles = ps.obstacles.iter().map(|o| o.triangulate()).collect();
            ps.obstacle_wireframes = ps.obstacles.iter().map(|o| o.wireframe()).collect();
            ps.obstacle_colors = ps.obstacles.iter().map(|o| o.color).collect();
        }

        // Build the Slint model rows for a list of area box volumes.
        fn area_models(areas: &[planning::BoxVolume]) -> Vec<PlanBoxVolumeData> {
            areas
                .iter()
                .map(|a| PlanBoxVolumeData {
                    label: a.label.clone().into(),
                    min_x: format!("{:.2}", a.min[0]).into(),
                    min_y: format!("{:.2}", a.min[1]).into(),
                    min_z: format!("{:.2}", a.min[2]).into(),
                    max_x: format!("{:.2}", a.max[0]).into(),
                    max_y: format!("{:.2}", a.max[1]).into(),
                    max_z: format!("{:.2}", a.max[2]).into(),
                })
                .collect()
        }

        // Parse a Slint area row back into a BoxVolume.
        fn box_volume_from_model(d: &PlanBoxVolumeData) -> planning::BoxVolume {
            planning::BoxVolume {
                min: [
                    d.min_x.parse().unwrap_or(0.0),
                    d.min_y.parse().unwrap_or(0.0),
                    d.min_z.parse().unwrap_or(0.0),
                ],
                max: [
                    d.max_x.parse().unwrap_or(0.0),
                    d.max_y.parse().unwrap_or(0.0),
                    d.max_z.parse().unwrap_or(0.0),
                ],
                label: d.label.to_string(),
            }
        }

        // Compute the displayed TDoA3 voxels + two stats lines for a given metric.
        // Ported from the standalone TDoA3 Coverage tab so Planning supports the
        // full set of metrics (DOP / error / sensitivity / pair sensitivity).
        #[allow(clippy::too_many_arguments)]
        fn tdoa3_metric_voxels_and_stats(
            display_metric: usize,
            gdop_result: Option<&tdoa3::GdopResult>,
            anchors: &[tdoa3::Anchor],
            convex_hull: Option<&tdoa3::ConvexHull>,
            room: [f32; 3],
            resolution: f32,
            max_range: f32,
            offset: [f32; 3],
            sigma: f32,
            min_slider: f32,
            max_slider: f32,
            show_outside_hull: bool,
            pair_a: usize,
            pair_b: usize,
        ) -> (Vec<(f32, f32, f32, f32)>, [String; 2]) {
            const DISP_PAIRS: usize = 5;
            const DISP_PAIR_SENS: usize = 6;
            const DISP_X_ERR: usize = 7;
            const DISP_Y_ERR: usize = 8;
            const DISP_Z_ERR: usize = 9;
            const DISP_XY_ERR: usize = 10;
            const DISP_GLOBAL_ERR: usize = 11;
            const DISP_X_SENS: usize = 12;
            const DISP_Z_SENS: usize = 14;
            const DISP_MIN_SENS: usize = 15;
            const DISP_MIN_XY_SENS: usize = 16;

            let hull_filter = |voxels: Vec<(f32, f32, f32, f32)>| -> Vec<(f32, f32, f32, f32)> {
                if show_outside_hull {
                    voxels
                } else {
                    voxels.into_iter()
                        .filter(|(x, y, z, _)| convex_hull.map_or(true, |h| h.contains(&[*x, *y, *z])))
                        .collect()
                }
            };

            // Pair sensitivity (single anchor pair)
            if display_metric == DISP_PAIR_SENS {
                if pair_a >= anchors.len() || pair_b >= anchors.len() || pair_a == pair_b {
                    return (Vec::new(), [
                        "Select two different valid anchor indices".to_string(),
                        String::new(),
                    ]);
                }
                let voxels = hull_filter(tdoa3::compute_pair_sensitivity(
                    room[0], room[1], room[2], resolution,
                    anchors[pair_a].pos, anchors[pair_b].pos, max_range, offset,
                ));
                let (min_v, max_v, avg_v) = tdoa3::voxel_stats(&voxels);
                return (voxels, [
                    format!("|h| min: {:.2}  max: {:.2}  avg: {:.2}", min_v, max_v, avg_v),
                    format!("Pair: anchor {} ↔ anchor {} (0 = degenerate, 2 = ideal)", pair_a, pair_b),
                ]);
            }

            // Axis sensitivity (X / Y / Z)
            if (DISP_X_SENS..=DISP_Z_SENS).contains(&display_metric) {
                let axis = display_metric - DISP_X_SENS;
                let axis_name = ["X", "Y", "Z"][axis];
                let voxels = hull_filter(tdoa3::compute_axis_sensitivity(
                    room[0], room[1], room[2], resolution, anchors, max_range, offset, axis,
                ));
                let (min_v, max_v, avg_v) = tdoa3::voxel_stats(&voxels);
                return (voxels, [
                    format!("{} sensitivity: min {:.2}  max {:.2}  avg {:.2}", axis_name, min_v, max_v, avg_v),
                    "High = measurements respond to movement, low = flat hyperbolas".to_string(),
                ]);
            }

            // Min-of-axes sensitivity
            if display_metric == DISP_MIN_SENS || display_metric == DISP_MIN_XY_SENS {
                let include_z = display_metric == DISP_MIN_SENS;
                let voxels = hull_filter(tdoa3::compute_min_axis_sensitivity(
                    room[0], room[1], room[2], resolution, anchors, max_range, offset, include_z,
                ));
                let label = if include_z { "Min XYZ" } else { "Min XY" };
                let (min_v, max_v, avg_v) = tdoa3::voxel_stats(&voxels);
                return (voxels, [
                    format!("{} sensitivity: min {:.2}  max {:.2}  avg {:.2}", label, min_v, max_v, avg_v),
                    "Weakest axis at each point (low = bottleneck for positioning)".to_string(),
                ]);
            }

            // DOP / error metrics from the precomputed GDOP result
            let Some(result) = gdop_result else {
                return (Vec::new(), [String::new(), String::new()]);
            };
            let (storage_metric, metric_name, sigma_mult) = match display_metric {
                0 => (tdoa3::METRIC_GDOP, "GDOP", 1.0),
                1 => (tdoa3::METRIC_HDOP, "HDOP", 1.0),
                2 => (tdoa3::METRIC_VDOP, "VDOP", 1.0),
                3 => (tdoa3::METRIC_XDOP, "XDOP", 1.0),
                4 => (tdoa3::METRIC_YDOP, "YDOP", 1.0),
                DISP_PAIRS => (tdoa3::METRIC_PAIRS, "Pairs", 1.0),
                DISP_X_ERR => (tdoa3::METRIC_XDOP, "σ_x", sigma),
                DISP_Y_ERR => (tdoa3::METRIC_YDOP, "σ_y", sigma),
                DISP_Z_ERR => (tdoa3::METRIC_VDOP, "σ_z", sigma),
                DISP_XY_ERR => (tdoa3::METRIC_HDOP, "σ_xy", sigma),
                DISP_GLOBAL_ERR => (tdoa3::METRIC_GDOP, "σ_xyz", sigma),
                _ => (tdoa3::METRIC_GDOP, "GDOP", 1.0),
            };

            let mut voxels: Vec<(f32, f32, f32, f32)> =
                hull_filter(result.iter_voxels(offset, storage_metric).collect());
            if sigma_mult != 1.0 {
                for v in &mut voxels {
                    if v.3.is_finite() { v.3 *= sigma_mult; }
                }
            }

            // Stats over visible voxels within slider range
            let mut vis_min = f32::INFINITY;
            let mut vis_max = f32::NEG_INFINITY;
            let mut vis_sum = 0.0_f64;
            let mut vis_sum_sq = 0.0_f64;
            let mut vis_count = 0u32;
            let mut total_finite = 0u32;
            for &(_, _, _, v) in &voxels {
                if v.is_finite() {
                    total_finite += 1;
                    if v >= min_slider && v <= max_slider {
                        vis_min = vis_min.min(v);
                        vis_max = vis_max.max(v);
                        vis_sum += v as f64;
                        vis_sum_sq += (v as f64) * (v as f64);
                        vis_count += 1;
                    }
                }
            }
            let vis_mean = if vis_count > 0 { vis_sum / vis_count as f64 } else { 0.0 };
            let vis_std = if vis_count > 1 {
                ((vis_sum_sq / vis_count as f64) - vis_mean * vis_mean).max(0.0).sqrt()
            } else { 0.0 };
            let ratio = if total_finite > 0 { vis_count as f32 / total_finite as f32 } else { 0.0 };

            let lines = if (DISP_X_ERR..=DISP_GLOBAL_ERR).contains(&display_metric) {
                let axis = match display_metric {
                    DISP_X_ERR => "X", DISP_Y_ERR => "Y", DISP_Z_ERR => "Z",
                    DISP_XY_ERR => "XY", _ => "Global",
                };
                [
                    format!("{} error: min {:.3}m  max {:.3}m  avg {:.3}m  σ {:.3}m",
                        axis, vis_min, vis_max, vis_mean, vis_std),
                    format!("{} ∈ [{:.2}, {:.2}]m: {:.1}% (σ_tdoa = {}m)",
                        metric_name, min_slider, max_slider, ratio * 100.0, sigma),
                ]
            } else if display_metric == DISP_PAIRS {
                [
                    format!("{} min: {:.0}  max: {:.0}  avg: {:.1}", metric_name,
                        if vis_min.is_finite() { vis_min } else { 0.0 },
                        if vis_max.is_finite() { vis_max } else { 0.0 }, vis_mean),
                    format!("≥3 pairs: {:.1}%  ≥6 pairs: {:.1}%",
                        result.coverage_ratio_pairs(3.0) * 100.0,
                        result.coverage_ratio_pairs(6.0) * 100.0),
                ]
            } else {
                [
                    format!("{} min: {:.3}  max: {:.3}  avg: {:.3}  σ: {:.3}", metric_name,
                        if vis_min.is_finite() { vis_min } else { 0.0 },
                        if vis_max.is_finite() { vis_max } else { 0.0 }, vis_mean, vis_std),
                    format!("{} ∈ [{:.2}, {:.2}]: {:.1}%", metric_name, min_slider, max_slider, ratio * 100.0),
                ]
            };
            (voxels, lines)
        }

        // Refresh Planning's TDoA3 voxels + stats text for the current metric,
        // reusing the already-computed GDOP result (no recompute needed).
        fn refresh_planning_tdoa3(ui: &AppWindow, ps: &mut PlanningState) {
            let metric = ui.get_planning_error_metric_index().max(0) as usize;
            let sigma: f32 = ui.get_planning_measurement_noise().parse().unwrap_or(0.15);
            let resolution: f32 = ui.get_planning_resolution().parse().unwrap_or(5.0);
            let max_range: f32 = ui.get_planning_max_range().parse().unwrap_or(15.0);
            let min_slider = ui.get_planning_tdoa3_scale_min();
            let max_slider = ui.get_planning_tdoa3_scale_max();
            let show_outside_hull = ui.get_planning_show_outside_hull();
            let pair_a: usize = ui.get_planning_pair_a().parse().unwrap_or(0);
            let pair_b: usize = ui.get_planning_pair_b().parse().unwrap_or(1);

            let (voxels, stats) = tdoa3_metric_voxels_and_stats(
                metric,
                ps.tdoa3_gdop_result.as_ref(),
                &ps.anchors,
                ps.convex_hull.as_ref(),
                ps.room,
                resolution,
                max_range,
                ps.room_offset,
                sigma,
                min_slider,
                max_slider,
                show_outside_hull,
                pair_a,
                pair_b,
            );
            ps.tdoa3_voxels = voxels;

            // Line 1: LH coverage + room global-error glance (+ hull glance if available)
            let mut line1 = format!(
                "LH: {:.0}% ≥1BS, {:.0}% ≥2BS  |  Room σ_xyz {:.1}cm",
                ps.lh_ratio_1 * 100.0, ps.lh_ratio_2 * 100.0, ps.gdop_mean * sigma * 100.0,
            );
            if let Some((g, _, _)) = ps.hull_stats {
                line1 += &format!("  Hull {:.1}cm", g * sigma * 100.0);
            }
            ui.set_planning_stats_text_1(line1.into());
            ui.set_planning_stats_text_2(stats[0].clone().into());
            ui.set_planning_stats_text_3(stats[1].clone().into());
        }

        fn rebuild_bs_render_data(ps: &mut PlanningState) {
            ps.bs_render_data = ps
                .base_stations
                .iter()
                .map(|bs| (bs.pos, bs.rotation_matrix()))
                .collect();
        }

        fn rebuild_anchor_positions(ps: &mut PlanningState) {
            ps.anchor_positions = ps.anchors.iter().map(|a| a.pos).collect();
        }

        // Save scene
        let ps = planning_state.clone();
        let uw = ui_weak.clone();
        let viz_scene_state_save = viz_scene_state.clone();
        ui.on_planning_save_scene(move || {
            let Some(ui) = uw.upgrade() else { return };
            let uw2 = ui.as_weak();
            let ps = ps.clone();
            let viz_scene_state = viz_scene_state_save.clone();
            slint::spawn_local(async move {
                let Some(handle) = host::dialog::FileDialog::new()
                    .add_filter("YAML", &["yaml", "yml"])
                    .set_file_name("planning.yaml")
                    .save_file().await
                else { return };
                let path = handle.path().to_path_buf();
                let Some(ui) = uw2.upgrade() else { return };

                let pstate = ps.lock().unwrap();
                // Areas are kept in sync with the UI list editors via their
                // update/add/remove callbacks, so just save what's in state.
                let scene = planning::PlanningScene::new(
                    ui.get_planning_room_x().parse().unwrap_or(8.0),
                    ui.get_planning_room_y().parse().unwrap_or(8.0),
                    ui.get_planning_room_z().parse().unwrap_or(3.0),
                    ui.get_planning_resolution().parse().unwrap_or(5.0),
                    ui.get_planning_center_origin(),
                    [
                        ui.get_planning_room_offset_x().parse().unwrap_or(0.0),
                        ui.get_planning_room_offset_y().parse().unwrap_or(0.0),
                        ui.get_planning_room_offset_z().parse().unwrap_or(0.0),
                    ],
                    &pstate.base_stations,
                    &pstate.anchors,
                    &pstate.obstacles,
                    pstate.flight_areas.clone(),
                    pstate.waypoint_areas.clone(),
                    pstate.takeoff_pads.clone(),
                    ui.get_planning_receiver_fov_enabled(),
                    ui.get_planning_max_bs_distance().parse().unwrap_or(5.0),
                    [
                        ui.get_planning_show_coverage_0(),
                        ui.get_planning_show_coverage_1(),
                        ui.get_planning_show_coverage_2(),
                        ui.get_planning_show_coverage_3(),
                        ui.get_planning_show_coverage_4(),
                    ],
                    ui.get_planning_max_range().parse().unwrap_or(15.0),
                    ui.get_planning_tdoa3_scale_min(),
                    ui.get_planning_tdoa3_scale_max(),
                    ui.get_planning_tilt_compensation_enabled(),
                    ui.get_planning_max_tilt_angle().parse().unwrap_or(10.0),
                );
                if let Err(e) = planning::save_scene(&path, &scene) {
                    eprintln!("Failed to save planning scene: {}", e);
                } else {
                    let mut settings: AppSettings = host::store::load();
                    settings.last_scene_config = Some(path.to_string_lossy().into_owned());
                    if let Err(e) = host::store::save(&settings) {
                        eprintln!("Failed to persist last_scene_config: {}", e);
                    }
                    // Sync the visualizer overlay so it reflects the saved scene
                    let mut s = viz_scene_state.lock().unwrap();
                    s.flight_areas = pstate.flight_areas.clone();
                    s.waypoint_areas = pstate.waypoint_areas.clone();
                    s.takeoff_pads = pstate.takeoff_pads.clone();
                    s.set_obstacles(&pstate.obstacles);
                }
            }).unwrap();
        });

        // Load scene
        let ps = planning_state.clone();
        let uw = ui_weak.clone();
        let viz_scene_state_load = viz_scene_state.clone();
        ui.on_planning_load_scene(move || {
            let Some(ui) = uw.upgrade() else { return };
            let uw2 = ui.as_weak();
            let ps = ps.clone();
            let viz_scene_state = viz_scene_state_load.clone();
            slint::spawn_local(async move {
                let Some(handle) = host::dialog::FileDialog::new()
                    .add_filter("YAML", &["yaml", "yml"])
                    .pick_file().await
                else { return };
                let path = handle.path().to_path_buf();
                let Some(ui) = uw2.upgrade() else { return };

                match planning::load_scene(&path) {
                    Ok(scene) => {
                        let mut ps = ps.lock().unwrap();
                        ps.base_stations = scene.base_stations();
                        ps.anchors = scene.anchors();
                        ps.obstacles = scene.obstacles();
                        ps.flight_areas = scene.flight_areas.clone();
                        ps.waypoint_areas = scene.waypoint_areas.clone();
                        ps.selected_area_kind = 0;
                        ps.selected_area_index = -1;
                        ps.takeoff_pads = scene.takeoff_pads.clone();
                        // Sync visualizer overlay
                        {
                            let mut s = viz_scene_state.lock().unwrap();
                            s.flight_areas = ps.flight_areas.clone();
                            s.waypoint_areas = ps.waypoint_areas.clone();
                            s.takeoff_pads = ps.takeoff_pads.clone();
                            s.set_obstacles(&ps.obstacles);
                        }
                        rebuild_bs_render_data(&mut ps);
                        rebuild_anchor_positions(&mut ps);
                        rebuild_obstacle_meshes(&mut ps);
                        ps.undo_stack.clear();

                        let mut settings: AppSettings = host::store::load();
                        settings.last_scene_config = Some(path.to_string_lossy().into_owned());
                        if let Err(e) = host::store::save(&settings) {
                            eprintln!("Failed to persist last_scene_config: {}", e);
                        }

                        ui.set_planning_room_x(format!("{}", scene.room_x).into());
                        ui.set_planning_room_y(format!("{}", scene.room_y).into());
                        ui.set_planning_room_z(format!("{}", scene.room_z).into());
                        ui.set_planning_resolution(format!("{}", scene.resolution).into());
                        ui.set_planning_center_origin(scene.center_origin);
                        ui.set_planning_room_offset_x(format!("{}", scene.room_offset[0]).into());
                        ui.set_planning_room_offset_y(format!("{}", scene.room_offset[1]).into());
                        ui.set_planning_room_offset_z(format!("{}", scene.room_offset[2]).into());
                        ui.set_planning_receiver_fov_enabled(scene.receiver_fov_enabled);
                        ui.set_planning_max_bs_distance(format!("{}", scene.max_bs_distance).into());
                        ui.set_planning_show_coverage_0(scene.show_coverage[0]);
                        ui.set_planning_show_coverage_1(scene.show_coverage[1]);
                        ui.set_planning_show_coverage_2(scene.show_coverage[2]);
                        ui.set_planning_show_coverage_3(scene.show_coverage[3]);
                        ui.set_planning_show_coverage_4(scene.show_coverage[4]);
                        ui.set_planning_max_range(format!("{}", scene.max_range).into());
                        ui.set_planning_tilt_compensation_enabled(scene.tilt_compensation_enabled);
                        ui.set_planning_max_tilt_angle(format!("{}", scene.max_tilt_angle).into());

                        // Compute coverage
                        let uo = scene.room_offset;
                        let offset = if scene.center_origin {
                            [-scene.room_x / 2.0 + uo[0], -scene.room_y / 2.0 + uo[1], uo[2]]
                        } else {
                            [uo[0], uo[1], uo[2]]
                        };
                        ps.room = [scene.room_x, scene.room_y, scene.room_z];
                        ps.room_offset = offset;
                        ps.lh_voxels.clear();
                        ps.tdoa3_voxels.clear();
                        ps.tdoa3_gdop_result = None;

                        ui.set_planning_tdoa3_scale_min(scene.tdoa3_scale_min);
                        ui.set_planning_tdoa3_scale_max(scene.tdoa3_scale_max);

                        let bs_model: Vec<LhBaseStationData> = ps.base_stations.iter().map(|bs| LhBaseStationData {
                            x: format!("{:.2}", bs.pos[0]).into(),
                            y: format!("{:.2}", bs.pos[1]).into(),
                            z: format!("{:.2}", bs.pos[2]).into(),
                            azimuth: format!("{:.1}", bs.azimuth_deg).into(),
                            elevation: format!("{:.1}", bs.elevation_deg).into(),
                        }).collect();
                        ui.set_planning_base_stations(slint::ModelRc::new(slint::VecModel::from(bs_model)));

                        let anchor_model: Vec<Tdoa3AnchorData> = ps.anchors.iter().map(|a| Tdoa3AnchorData {
                            x: format!("{:.2}", a.pos[0]).into(),
                            y: format!("{:.2}", a.pos[1]).into(),
                            z: format!("{:.2}", a.pos[2]).into(),
                        }).collect();
                        ui.set_planning_anchors(slint::ModelRc::new(slint::VecModel::from(anchor_model)));

                        let obs_model: Vec<PlanObstacleData> = ps.obstacles.iter().map(|o| PlanObstacleData {
                            kind: match o.kind { planning::ObstacleKind::Box => "Box", planning::ObstacleKind::Cylinder => "Cylinder" }.into(),
                            x: format!("{:.2}", o.pos[0]).into(),
                            y: format!("{:.2}", o.pos[1]).into(),
                            z: format!("{:.2}", o.pos[2]).into(),
                            yaw: format!("{:.1}", o.yaw_deg).into(),
                            width: format!("{:.2}", o.width).into(),
                            depth: format!("{:.2}", o.depth).into(),
                            height: format!("{:.2}", o.height).into(),
                            radius: format!("{:.2}", o.radius).into(),
                            color_r: o.color[0], color_g: o.color[1], color_b: o.color[2],
                            on_floor: o.on_floor,
                        }).collect();
                        ui.set_planning_obstacles(slint::ModelRc::new(slint::VecModel::from(obs_model)));

                        // Flight + waypoint areas
                        ui.set_planning_flight_areas(slint::ModelRc::new(slint::VecModel::from(area_models(&ps.flight_areas))));
                        ui.set_planning_waypoint_areas(slint::ModelRc::new(slint::VecModel::from(area_models(&ps.waypoint_areas))));
                        ui.set_planning_selected_area_kind(0);
                        ui.set_planning_selected_area_index(-1);

                        // Takeoff pads (read-only display)
                        let pads_model: Vec<PlanPadData> = ps.takeoff_pads.iter().map(|p| PlanPadData {
                            label: p.label.clone().into(),
                            x: format!("{:.2}", p.x).into(),
                            y: format!("{:.2}", p.y).into(),
                            radius: format!("{:.2}", p.radius).into(),
                        }).collect();
                        ui.set_planning_takeoff_pads(slint::ModelRc::new(slint::VecModel::from(pads_model)));

                        ui.set_planning_selected_type(0);
                        ui.set_planning_selected_index(-1);
                        ui.set_planning_stats_text_1("".into());
                        ui.set_planning_stats_text_2("".into());
                        ui.set_planning_stats_text_3("".into());

                        // Drop the lock before triggering recompute (which also locks)
                        drop(ps);
                        ui.invoke_planning_recompute();
                    }
                    Err(e) => {
                        eprintln!("Failed to load planning scene: {}", e);
                    }
                }
            }).unwrap();
        });

        // Load LH base-station geometry from a YAML file
        let ps = planning_state.clone();
        let uw = ui_weak.clone();
        ui.on_planning_load_geometry(move || {
            let Some(ui) = uw.upgrade() else { return };
            let uw2 = ui.as_weak();
            let ps = ps.clone();
            slint::spawn_local(async move {
                let Some(handle) = host::dialog::FileDialog::new()
                    .add_filter("YAML", &["yaml", "yml"])
                    .pick_file().await
                else { return };
                let path = handle.path().to_path_buf();
                let Some(ui) = uw2.upgrade() else { return };
                match coverage::load_geometry_yaml(&path) {
                    Ok(stations) => {
                        let mut ps = ps.lock().unwrap();
                        ps.base_stations = stations;
                        rebuild_bs_render_data(&mut ps);
                        let model: Vec<LhBaseStationData> = ps.base_stations.iter().map(|bs| LhBaseStationData {
                            x: format!("{:.2}", bs.pos[0]).into(),
                            y: format!("{:.2}", bs.pos[1]).into(),
                            z: format!("{:.2}", bs.pos[2]).into(),
                            azimuth: format!("{:.1}", bs.azimuth_deg).into(),
                            elevation: format!("{:.1}", bs.elevation_deg).into(),
                        }).collect();
                        ui.set_planning_base_stations(slint::ModelRc::new(slint::VecModel::from(model)));
                        let n = ps.base_stations.len();
                        ui.set_planning_selected_type(0);
                        ui.set_planning_selected_index(-1);
                        ui.set_planning_stats_text_1(format!("Loaded {} base stations from geometry", n).into());
                        drop(ps);
                        ui.invoke_planning_recompute();
                    }
                    Err(e) => {
                        eprintln!("Failed to load geometry: {}", e);
                        ui.set_planning_stats_text_1(format!("Load geometry failed: {}", e).into());
                    }
                }
            }).unwrap();
        });

        // Load trajectory CSV overlay
        let ps = planning_state.clone();
        let uw = ui_weak.clone();
        ui.on_planning_load_trajectories(move || {
            let Some(ui) = uw.upgrade() else { return };
            let uw2 = ui.as_weak();
            let ps = ps.clone();
            slint::spawn_local(async move {
                let Some(handle) = host::dialog::FileDialog::new()
                    .add_filter("CSV files", &["csv"])
                    .set_title("Load Trajectories CSV")
                    .pick_file().await
                else { return };
                let path = handle.path().to_path_buf();
                let Some(ui) = uw2.upgrade() else { return };
                match coverage::load_trajectories_csv(&path) {
                    Ok(trajectories) => {
                        let n_cfs = trajectories.len();
                        let n_steps = trajectories.first().map(|t| t.len()).unwrap_or(0);
                        ui.set_planning_trajectories_status(
                            format!("{} CFs, {} steps", n_cfs, n_steps).into(),
                        );
                        ps.lock().unwrap().trajectories = trajectories;
                        ui.window().request_redraw();
                    }
                    Err(e) => {
                        ui.set_planning_trajectories_status(format!("Error: {}", e).into());
                    }
                }
            }).unwrap();
        });

        // Add base station
        let ps = planning_state.clone();
        let uw = ui_weak.clone();
        ui.on_planning_add_bs(move || {
            let Some(ui) = uw.upgrade() else { return };
            let mut model: Vec<LhBaseStationData> = {
                let m = ui.get_planning_base_stations();
                (0..m.row_count()).filter_map(|i| m.row_data(i)).collect()
            };
            model.push(LhBaseStationData {
                x: "4.0".into(), y: "4.0".into(), z: "3.0".into(),
                azimuth: "0.0".into(), elevation: "45.0".into(),
            });
            ui.set_planning_base_stations(slint::ModelRc::new(slint::VecModel::from(model)));
            let mut ps = ps.lock().unwrap();
            ps.base_stations.push(coverage::BaseStation {
                pos: [4.0, 4.0, 3.0], azimuth_deg: 0.0, elevation_deg: 45.0,
            });
            rebuild_bs_render_data(&mut ps);
        });

        // Remove base station
        let ps = planning_state.clone();
        let uw = ui_weak.clone();
        ui.on_planning_remove_bs(move |idx| {
            let Some(ui) = uw.upgrade() else { return };
            let idx = idx as usize;
            let mut model: Vec<LhBaseStationData> = {
                let m = ui.get_planning_base_stations();
                (0..m.row_count()).filter_map(|i| m.row_data(i)).collect()
            };
            if idx < model.len() { model.remove(idx); }
            ui.set_planning_base_stations(slint::ModelRc::new(slint::VecModel::from(model)));
            let mut ps = ps.lock().unwrap();
            if idx < ps.base_stations.len() { ps.base_stations.remove(idx); }
            rebuild_bs_render_data(&mut ps);
        });

        // Duplicate base station
        let ps = planning_state.clone();
        let uw = ui_weak.clone();
        ui.on_planning_duplicate_bs(move |idx| {
            let Some(ui) = uw.upgrade() else { return };
            let idx = idx as usize;
            let mut ps = ps.lock().unwrap();
            if idx < ps.base_stations.len() {
                let mut bs = ps.base_stations[idx].clone();
                bs.pos[0] += 0.5; // offset so it's visible
                ps.base_stations.push(bs);
                rebuild_bs_render_data(&mut ps);

                let mut model: Vec<LhBaseStationData> = {
                    let m = ui.get_planning_base_stations();
                    (0..m.row_count()).filter_map(|i| m.row_data(i)).collect()
                };
                let new_bs = ps.base_stations.last().unwrap();
                model.push(LhBaseStationData {
                    x: format!("{:.2}", new_bs.pos[0]).into(),
                    y: format!("{:.2}", new_bs.pos[1]).into(),
                    z: format!("{:.2}", new_bs.pos[2]).into(),
                    azimuth: format!("{:.1}", new_bs.azimuth_deg).into(),
                    elevation: format!("{:.1}", new_bs.elevation_deg).into(),
                });
                ui.set_planning_base_stations(slint::ModelRc::new(slint::VecModel::from(model)));
            }
        });

        // Update base station
        let ps = planning_state.clone();
        ui.on_planning_update_bs(move |idx, data| {
            let idx = idx as usize;
            let mut ps = ps.lock().unwrap();
            if idx < ps.base_stations.len() {
                ps.base_stations[idx] = coverage::BaseStation {
                    pos: [
                        data.x.parse().unwrap_or(0.0),
                        data.y.parse().unwrap_or(0.0),
                        data.z.parse().unwrap_or(0.0),
                    ],
                    azimuth_deg: data.azimuth.parse().unwrap_or(0.0),
                    elevation_deg: data.elevation.parse().unwrap_or(0.0),
                };
                rebuild_bs_render_data(&mut ps);
            }
        });

        // Add anchor
        let ps = planning_state.clone();
        let uw = ui_weak.clone();
        ui.on_planning_add_anchor(move || {
            let Some(ui) = uw.upgrade() else { return };
            let mut model: Vec<Tdoa3AnchorData> = {
                let m = ui.get_planning_anchors();
                (0..m.row_count()).filter_map(|i| m.row_data(i)).collect()
            };
            model.push(Tdoa3AnchorData {
                x: "0.0".into(), y: "0.0".into(), z: "2.5".into(),
            });
            ui.set_planning_anchors(slint::ModelRc::new(slint::VecModel::from(model)));
            let mut ps = ps.lock().unwrap();
            ps.anchors.push(tdoa3::Anchor { pos: [0.0, 0.0, 2.5] });
            rebuild_anchor_positions(&mut ps);
        });

        // Remove anchor
        let ps = planning_state.clone();
        let uw = ui_weak.clone();
        ui.on_planning_remove_anchor(move |idx| {
            let Some(ui) = uw.upgrade() else { return };
            let idx = idx as usize;
            let mut model: Vec<Tdoa3AnchorData> = {
                let m = ui.get_planning_anchors();
                (0..m.row_count()).filter_map(|i| m.row_data(i)).collect()
            };
            if idx < model.len() { model.remove(idx); }
            ui.set_planning_anchors(slint::ModelRc::new(slint::VecModel::from(model)));
            let mut ps = ps.lock().unwrap();
            if idx < ps.anchors.len() { ps.anchors.remove(idx); }
            rebuild_anchor_positions(&mut ps);
        });

        // Duplicate anchor
        let ps = planning_state.clone();
        let uw = ui_weak.clone();
        ui.on_planning_duplicate_anchor(move |idx| {
            let Some(ui) = uw.upgrade() else { return };
            let idx = idx as usize;
            let mut ps = ps.lock().unwrap();
            if idx < ps.anchors.len() {
                let mut a = ps.anchors[idx].clone();
                a.pos[0] += 0.5;
                ps.anchors.push(a);
                rebuild_anchor_positions(&mut ps);

                let mut model: Vec<Tdoa3AnchorData> = {
                    let m = ui.get_planning_anchors();
                    (0..m.row_count()).filter_map(|i| m.row_data(i)).collect()
                };
                let new_a = ps.anchors.last().unwrap();
                model.push(Tdoa3AnchorData {
                    x: format!("{:.2}", new_a.pos[0]).into(),
                    y: format!("{:.2}", new_a.pos[1]).into(),
                    z: format!("{:.2}", new_a.pos[2]).into(),
                });
                ui.set_planning_anchors(slint::ModelRc::new(slint::VecModel::from(model)));
            }
        });

        // Update anchor
        let ps = planning_state.clone();
        ui.on_planning_update_anchor(move |idx, data| {
            let idx = idx as usize;
            let mut ps = ps.lock().unwrap();
            if idx < ps.anchors.len() {
                ps.anchors[idx] = tdoa3::Anchor {
                    pos: [
                        data.x.parse().unwrap_or(0.0),
                        data.y.parse().unwrap_or(0.0),
                        data.z.parse().unwrap_or(0.0),
                    ],
                };
                rebuild_anchor_positions(&mut ps);
            }
        });

        // Add box obstacle
        let ps = planning_state.clone();
        let uw = ui_weak.clone();
        ui.on_planning_add_box(move || {
            let Some(ui) = uw.upgrade() else { return };
            let mut obs = planning::Obstacle::new_box([2.0, 2.0, 0.5]);
            // on_floor defaults to true, so place on floor
            obs.pos[2] = obs.height / 2.0;
            let mut model: Vec<PlanObstacleData> = {
                let m = ui.get_planning_obstacles();
                (0..m.row_count()).filter_map(|i| m.row_data(i)).collect()
            };
            model.push(PlanObstacleData {
                kind: "Box".into(),
                x: format!("{:.2}", obs.pos[0]).into(),
                y: format!("{:.2}", obs.pos[1]).into(),
                z: format!("{:.2}", obs.pos[2]).into(),
                yaw: "0.0".into(),
                width: format!("{:.2}", obs.width).into(),
                depth: format!("{:.2}", obs.depth).into(),
                height: format!("{:.2}", obs.height).into(),
                radius: "0.00".into(),
                color_r: obs.color[0], color_g: obs.color[1], color_b: obs.color[2],
                on_floor: obs.on_floor,
            });
            ui.set_planning_obstacles(slint::ModelRc::new(slint::VecModel::from(model)));
            let mut ps = ps.lock().unwrap();
            ps.obstacles.push(obs);
            rebuild_obstacle_meshes(&mut ps);
        });

        // Add cylinder obstacle
        let ps = planning_state.clone();
        let uw = ui_weak.clone();
        ui.on_planning_add_cylinder(move || {
            let Some(ui) = uw.upgrade() else { return };
            let mut obs = planning::Obstacle::new_cylinder([2.0, 2.0, 0.5]);
            obs.pos[2] = obs.height / 2.0;
            let mut model: Vec<PlanObstacleData> = {
                let m = ui.get_planning_obstacles();
                (0..m.row_count()).filter_map(|i| m.row_data(i)).collect()
            };
            model.push(PlanObstacleData {
                kind: "Cylinder".into(),
                x: format!("{:.2}", obs.pos[0]).into(),
                y: format!("{:.2}", obs.pos[1]).into(),
                z: format!("{:.2}", obs.pos[2]).into(),
                yaw: "0.0".into(),
                width: "0.00".into(),
                depth: "0.00".into(),
                height: format!("{:.2}", obs.height).into(),
                radius: format!("{:.2}", obs.radius).into(),
                color_r: obs.color[0], color_g: obs.color[1], color_b: obs.color[2],
                on_floor: obs.on_floor,
            });
            ui.set_planning_obstacles(slint::ModelRc::new(slint::VecModel::from(model)));
            let mut ps = ps.lock().unwrap();
            ps.obstacles.push(obs);
            rebuild_obstacle_meshes(&mut ps);
        });

        // Remove obstacle
        let ps = planning_state.clone();
        let uw = ui_weak.clone();
        ui.on_planning_remove_obstacle(move |idx| {
            let Some(ui) = uw.upgrade() else { return };
            let idx = idx as usize;
            let mut model: Vec<PlanObstacleData> = {
                let m = ui.get_planning_obstacles();
                (0..m.row_count()).filter_map(|i| m.row_data(i)).collect()
            };
            if idx < model.len() { model.remove(idx); }
            ui.set_planning_obstacles(slint::ModelRc::new(slint::VecModel::from(model)));
            let mut ps = ps.lock().unwrap();
            if idx < ps.obstacles.len() { ps.obstacles.remove(idx); }
            rebuild_obstacle_meshes(&mut ps);
        });

        // Duplicate obstacle
        let ps = planning_state.clone();
        let uw = ui_weak.clone();
        ui.on_planning_duplicate_obstacle(move |idx| {
            let Some(ui) = uw.upgrade() else { return };
            let idx = idx as usize;
            let mut ps = ps.lock().unwrap();
            if idx < ps.obstacles.len() {
                let mut o = ps.obstacles[idx].clone();
                o.pos[0] += 0.5;
                ps.obstacles.push(o);
                rebuild_obstacle_meshes(&mut ps);

                let mut model: Vec<PlanObstacleData> = {
                    let m = ui.get_planning_obstacles();
                    (0..m.row_count()).filter_map(|i| m.row_data(i)).collect()
                };
                let o = ps.obstacles.last().unwrap();
                model.push(PlanObstacleData {
                    kind: match o.kind { planning::ObstacleKind::Box => "Box", planning::ObstacleKind::Cylinder => "Cylinder" }.into(),
                    x: format!("{:.2}", o.pos[0]).into(),
                    y: format!("{:.2}", o.pos[1]).into(),
                    z: format!("{:.2}", o.pos[2]).into(),
                    yaw: format!("{:.1}", o.yaw_deg).into(),
                    width: format!("{:.2}", o.width).into(),
                    depth: format!("{:.2}", o.depth).into(),
                    height: format!("{:.2}", o.height).into(),
                    radius: format!("{:.2}", o.radius).into(),
                    color_r: o.color[0], color_g: o.color[1], color_b: o.color[2],
                    on_floor: o.on_floor,
                });
                ui.set_planning_obstacles(slint::ModelRc::new(slint::VecModel::from(model)));
            }
        });

        // Update obstacle
        let ps = planning_state.clone();
        let _uw = ui_weak.clone();
        ui.on_planning_update_obstacle(move |idx, data| {
            let idx = idx as usize;
            let mut ps = ps.lock().unwrap();
            if idx < ps.obstacles.len() {
                let kind = if data.kind.as_str() == "Cylinder" {
                    planning::ObstacleKind::Cylinder
                } else {
                    planning::ObstacleKind::Box
                };
                let height: f32 = data.height.parse().unwrap_or(1.0);
                let mut z: f32 = data.z.parse().unwrap_or(0.0);

                // Per-obstacle on_floor: keep bottom at Z=0
                if data.on_floor {
                    z = height / 2.0;
                }

                ps.obstacles[idx] = planning::Obstacle {
                    kind,
                    pos: [
                        data.x.parse().unwrap_or(0.0),
                        data.y.parse().unwrap_or(0.0),
                        z,
                    ],
                    yaw_deg: data.yaw.parse().unwrap_or(0.0),
                    width: data.width.parse().unwrap_or(1.0),
                    depth: data.depth.parse().unwrap_or(1.0),
                    height,
                    radius: data.radius.parse().unwrap_or(0.5),
                    color: [data.color_r, data.color_g, data.color_b],
                    on_floor: data.on_floor,
                };
                rebuild_obstacle_meshes(&mut ps);
            }
        });

        // Undo
        let ps = planning_state.clone();
        let uw = ui_weak.clone();
        ui.on_planning_undo(move || {
            let Some(ui) = uw.upgrade() else { return };
            let mut ps = ps.lock().unwrap();
            if let Some((prev_bs, prev_anchors, prev_obs)) = ps.undo_stack.pop() {
                ps.base_stations = prev_bs;
                ps.anchors = prev_anchors;
                ps.obstacles = prev_obs;
                rebuild_bs_render_data(&mut ps);
                rebuild_anchor_positions(&mut ps);
                rebuild_obstacle_meshes(&mut ps);

                let bs_model: Vec<LhBaseStationData> = ps.base_stations.iter().map(|bs| LhBaseStationData {
                    x: format!("{:.2}", bs.pos[0]).into(),
                    y: format!("{:.2}", bs.pos[1]).into(),
                    z: format!("{:.2}", bs.pos[2]).into(),
                    azimuth: format!("{:.1}", bs.azimuth_deg).into(),
                    elevation: format!("{:.1}", bs.elevation_deg).into(),
                }).collect();
                ui.set_planning_base_stations(slint::ModelRc::new(slint::VecModel::from(bs_model)));

                let anchor_model: Vec<Tdoa3AnchorData> = ps.anchors.iter().map(|a| Tdoa3AnchorData {
                    x: format!("{:.2}", a.pos[0]).into(),
                    y: format!("{:.2}", a.pos[1]).into(),
                    z: format!("{:.2}", a.pos[2]).into(),
                }).collect();
                ui.set_planning_anchors(slint::ModelRc::new(slint::VecModel::from(anchor_model)));

                let obs_model: Vec<PlanObstacleData> = ps.obstacles.iter().map(|o| PlanObstacleData {
                    kind: match o.kind { planning::ObstacleKind::Box => "Box", planning::ObstacleKind::Cylinder => "Cylinder" }.into(),
                    x: format!("{:.2}", o.pos[0]).into(),
                    y: format!("{:.2}", o.pos[1]).into(),
                    z: format!("{:.2}", o.pos[2]).into(),
                    yaw: format!("{:.1}", o.yaw_deg).into(),
                    width: format!("{:.2}", o.width).into(),
                    depth: format!("{:.2}", o.depth).into(),
                    height: format!("{:.2}", o.height).into(),
                    radius: format!("{:.2}", o.radius).into(),
                    color_r: o.color[0], color_g: o.color[1], color_b: o.color[2],
                    on_floor: o.on_floor,
                }).collect();
                ui.set_planning_obstacles(slint::ModelRc::new(slint::VecModel::from(obs_model)));
            }
        });

        // --- Flight / waypoint area editing callbacks ---
        // A new area defaults to a 2x2x1.3 m box near the floor.
        fn default_area() -> planning::BoxVolume {
            planning::BoxVolume::new([-1.0, -1.0, 0.2], [1.0, 1.0, 1.5])
        }

        // Add flight area
        let ps = planning_state.clone();
        let uw = ui_weak.clone();
        let vss = viz_scene_state.clone();
        ui.on_planning_add_flight_area(move || {
            let Some(ui) = uw.upgrade() else { return };
            let mut ps = ps.lock().unwrap();
            ps.flight_areas.push(default_area());
            ui.set_planning_flight_areas(slint::ModelRc::new(slint::VecModel::from(area_models(&ps.flight_areas))));
            vss.lock().unwrap().flight_areas = ps.flight_areas.clone();
        });

        // Remove flight area
        let ps = planning_state.clone();
        let uw = ui_weak.clone();
        let vss = viz_scene_state.clone();
        ui.on_planning_remove_flight_area(move |idx| {
            let Some(ui) = uw.upgrade() else { return };
            let mut ps = ps.lock().unwrap();
            if (idx as usize) < ps.flight_areas.len() {
                ps.flight_areas.remove(idx as usize);
                if ps.selected_area_kind == 1 {
                    ps.selected_area_kind = 0;
                    ps.selected_area_index = -1;
                    ui.set_planning_selected_area_kind(0);
                    ui.set_planning_selected_area_index(-1);
                }
                ui.set_planning_flight_areas(slint::ModelRc::new(slint::VecModel::from(area_models(&ps.flight_areas))));
                vss.lock().unwrap().flight_areas = ps.flight_areas.clone();
            }
        });

        // Update flight area
        let ps = planning_state.clone();
        let vss = viz_scene_state.clone();
        ui.on_planning_update_flight_area(move |idx, data| {
            let mut ps = ps.lock().unwrap();
            if let Some(area) = ps.flight_areas.get_mut(idx as usize) {
                *area = box_volume_from_model(&data);
                vss.lock().unwrap().flight_areas = ps.flight_areas.clone();
            }
        });

        // Select flight area (for handle editing)
        let ps = planning_state.clone();
        let uw = ui_weak.clone();
        ui.on_planning_select_flight_area(move |idx| {
            let Some(ui) = uw.upgrade() else { return };
            let mut ps = ps.lock().unwrap();
            let toggle = ps.selected_area_kind == 1 && ps.selected_area_index == idx;
            if toggle {
                ps.selected_area_kind = 0;
                ps.selected_area_index = -1;
            } else {
                ps.selected_area_kind = 1;
                ps.selected_area_index = idx;
            }
            ui.set_planning_selected_area_kind(ps.selected_area_kind);
            ui.set_planning_selected_area_index(ps.selected_area_index);
        });

        // Add waypoint area
        let ps = planning_state.clone();
        let uw = ui_weak.clone();
        let vss = viz_scene_state.clone();
        ui.on_planning_add_waypoint_area(move || {
            let Some(ui) = uw.upgrade() else { return };
            let mut ps = ps.lock().unwrap();
            ps.waypoint_areas.push(default_area());
            ui.set_planning_waypoint_areas(slint::ModelRc::new(slint::VecModel::from(area_models(&ps.waypoint_areas))));
            vss.lock().unwrap().waypoint_areas = ps.waypoint_areas.clone();
        });

        // Remove waypoint area
        let ps = planning_state.clone();
        let uw = ui_weak.clone();
        let vss = viz_scene_state.clone();
        ui.on_planning_remove_waypoint_area(move |idx| {
            let Some(ui) = uw.upgrade() else { return };
            let mut ps = ps.lock().unwrap();
            if (idx as usize) < ps.waypoint_areas.len() {
                ps.waypoint_areas.remove(idx as usize);
                if ps.selected_area_kind == 2 {
                    ps.selected_area_kind = 0;
                    ps.selected_area_index = -1;
                    ui.set_planning_selected_area_kind(0);
                    ui.set_planning_selected_area_index(-1);
                }
                ui.set_planning_waypoint_areas(slint::ModelRc::new(slint::VecModel::from(area_models(&ps.waypoint_areas))));
                vss.lock().unwrap().waypoint_areas = ps.waypoint_areas.clone();
            }
        });

        // Update waypoint area
        let ps = planning_state.clone();
        let vss = viz_scene_state.clone();
        ui.on_planning_update_waypoint_area(move |idx, data| {
            let mut ps = ps.lock().unwrap();
            if let Some(area) = ps.waypoint_areas.get_mut(idx as usize) {
                *area = box_volume_from_model(&data);
                vss.lock().unwrap().waypoint_areas = ps.waypoint_areas.clone();
            }
        });

        // Select waypoint area (for handle editing)
        let ps = planning_state.clone();
        let uw = ui_weak.clone();
        ui.on_planning_select_waypoint_area(move |idx| {
            let Some(ui) = uw.upgrade() else { return };
            let mut ps = ps.lock().unwrap();
            let toggle = ps.selected_area_kind == 2 && ps.selected_area_index == idx;
            if toggle {
                ps.selected_area_kind = 0;
                ps.selected_area_index = -1;
            } else {
                ps.selected_area_kind = 2;
                ps.selected_area_index = idx;
            }
            ui.set_planning_selected_area_kind(ps.selected_area_kind);
            ui.set_planning_selected_area_index(ps.selected_area_index);
        });

        // Recompute coverage (background thread with timer polling)
        type CoverageComputeResult = (
            Vec<(f32, f32, f32, u8)>,   // lh_voxels
            f32, f32,                    // lh_ratio_1, lh_ratio_2
            tdoa3::GdopResult,           // tdoa3_result
            f32, f32, f32,              // gdop_mean, hdop_mean, vdop_mean
            Option<(f32, f32, f32)>,    // hull_stats
            [f32; 3], [f32; 3],         // offset, room
        );

        let ps = planning_state.clone();
        let uw = ui_weak.clone();
        // Shared receiver for background computation results
        let compute_rx: std::rc::Rc<std::cell::RefCell<Option<std::sync::mpsc::Receiver<CoverageComputeResult>>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let compute_rx_timer = compute_rx.clone();
        let compute_timer = slint::Timer::default();
        let ps_timer = planning_state.clone();
        let uw_timer = ui_weak.clone();

        // Timer polls for computation results every 50ms
        compute_timer.start(slint::TimerMode::Repeated, std::time::Duration::from_millis(50), move || {
            let mut rx_opt = compute_rx_timer.borrow_mut();
            let Some(ref rx) = *rx_opt else { return };
            let Ok(result) = rx.try_recv() else { return };

            // Result received — update state and UI
            let (lh_voxels, lh_ratio_1, lh_ratio_2, tdoa3_result, gdop_mean, _hdop_mean, _vdop_mean, hull_stats, offset, room) = result;

            let Some(ui) = uw_timer.upgrade() else { return };

            let mut pstate = ps_timer.lock().unwrap();
            pstate.room = room;
            pstate.room_offset = offset;
            pstate.lh_voxels = lh_voxels;
            pstate.lh_ratio_1 = lh_ratio_1;
            pstate.lh_ratio_2 = lh_ratio_2;
            pstate.gdop_mean = gdop_mean;
            pstate.hull_stats = hull_stats;
            pstate.tdoa3_gdop_result = Some(tdoa3_result);
            let hull = tdoa3::ConvexHull::build(&pstate.anchor_positions);
            pstate.convex_hull = hull;
            refresh_planning_tdoa3(&ui, &mut pstate);
            drop(pstate);

            ui.set_planning_computing(false);
            *rx_opt = None; // clear receiver
        });

        ui.on_planning_recompute(move || {
            let Some(ui) = uw.upgrade() else { return };
            if ui.get_planning_computing() { return; }
            ui.set_planning_computing(true);

            let room_x: f32 = ui.get_planning_room_x().parse().unwrap_or(8.0);
            let room_y: f32 = ui.get_planning_room_y().parse().unwrap_or(8.0);
            let room_z: f32 = ui.get_planning_room_z().parse().unwrap_or(3.0);
            let resolution: f32 = ui.get_planning_resolution().parse().unwrap_or(5.0);
            let center = ui.get_planning_center_origin();
            let max_bs_dist: f32 = ui.get_planning_max_bs_distance().parse().unwrap_or(5.0);
            let max_range: f32 = ui.get_planning_max_range().parse().unwrap_or(15.0);
            let receiver_fov = if ui.get_planning_receiver_fov_enabled() { Some(170.0) } else { None };
            let tilt_reduction = if ui.get_planning_tilt_compensation_enabled() {
                Some(ui.get_planning_max_tilt_angle().parse().unwrap_or(10.0))
            } else { None };

            let user_offset_x: f32 = ui.get_planning_room_offset_x().parse().unwrap_or(0.0);
            let user_offset_y: f32 = ui.get_planning_room_offset_y().parse().unwrap_or(0.0);
            let user_offset_z: f32 = ui.get_planning_room_offset_z().parse().unwrap_or(0.0);
            let offset = if center {
                [-room_x / 2.0 + user_offset_x, -room_y / 2.0 + user_offset_y, user_offset_z]
            } else {
                [user_offset_x, user_offset_y, user_offset_z]
            };

            let (base_stations, anchors, obstacles) = {
                let pstate = ps.lock().unwrap();
                (pstate.base_stations.clone(), pstate.anchors.clone(), pstate.obstacles.clone())
            };

            let (tx, rx) = std::sync::mpsc::channel();
            *compute_rx.borrow_mut() = Some(rx);

            host::spawn_thread(move || {
                let lh_result = planning::compute_coverage_with_obstacles(
                    room_x, room_y, room_z, resolution,
                    &base_stations, 160.0, 115.0,
                    receiver_fov, tilt_reduction, max_bs_dist, offset,
                    &obstacles,
                );
                let lh_ratio_1 = lh_result.coverage_ratio(1);
                let lh_ratio_2 = lh_result.coverage_ratio(2);
                let lh_voxels: Vec<(f32, f32, f32, u8)> = lh_result.iter_voxels(offset).collect();

                let tdoa3_result = tdoa3::compute_gdop(
                    room_x, room_y, room_z, resolution,
                    &anchors, max_range, offset,
                );
                let (_, _, gdop_mean) = tdoa3_result.stats(tdoa3::METRIC_GDOP);
                let (_, _, hdop_mean) = tdoa3_result.stats(tdoa3::METRIC_HDOP);
                let (_, _, vdop_mean) = tdoa3_result.stats(tdoa3::METRIC_VDOP);

                let anchor_positions: Vec<[f32; 3]> = anchors.iter().map(|a| a.pos).collect();
                let hull = tdoa3::ConvexHull::build(&anchor_positions);
                let hull_stats = hull.as_ref().map(|h| {
                    let (_, _, g) = tdoa3_result.stats_in_hull(tdoa3::METRIC_GDOP, offset, h);
                    let (_, _, hh) = tdoa3_result.stats_in_hull(tdoa3::METRIC_HDOP, offset, h);
                    let (_, _, v) = tdoa3_result.stats_in_hull(tdoa3::METRIC_VDOP, offset, h);
                    (g, hh, v)
                });

                let _ = tx.send((lh_voxels, lh_ratio_1, lh_ratio_2, tdoa3_result,
                    gdop_mean, hdop_mean, vdop_mean, hull_stats,
                    offset, [room_x, room_y, room_z]));
            });
        });

        // Keep the timer alive — it runs forever, polling for results
        std::mem::forget(compute_timer);

        // Metric changed / display option changed — re-extract voxels + stats
        // without recomputing the GDOP grid. Resets the colour scale only when
        // the metric index actually changes (so editing noise/pair/hull doesn't
        // clobber the user's slider positions).
        let ps = planning_state.clone();
        let uw = ui_weak.clone();
        ui.on_planning_error_metric_changed(move |idx| {
            let Some(ui) = uw.upgrade() else { return };
            let mut ps = ps.lock().unwrap();
            if idx != ps.last_metric {
                ps.last_metric = idx;
                // Error metrics (7-11) are in metres; DOP/sensitivity are unitless.
                let (limit, default_min, default_max) = match idx.max(0) as usize {
                    7..=11 => (2.0_f32, 0.0_f32, 0.5_f32),   // error in metres
                    12..=16 => (2.0_f32, 0.0_f32, 1.0_f32),  // sensitivity
                    5 => (20.0, 0.0, 12.0),                  // anchor pairs (count)
                    6 => (2.0, 0.0, 2.0),                    // pair sensitivity
                    _ => (10.0, 0.0, 5.0),                   // DOP values
                };
                ui.set_planning_tdoa3_scale_limit(limit);
                ui.set_planning_tdoa3_scale_min(default_min);
                ui.set_planning_tdoa3_scale_max(default_max);
            }
            refresh_planning_tdoa3(&ui, &mut ps);
        });

        // Mouse interaction: press
        let ps = planning_state.clone();
        let gs = planning_gizmo_state.clone();
        let uw = ui_weak.clone();
        ui.on_planning_view_mouse_pressed(move |mx, my| {
            let Some(ui) = uw.upgrade() else { return };
            let pw = ui.get_planning_width() as u32;
            let ph = ui.get_planning_height() as u32;
            if pw == 0 || ph == 0 { return; }
            let yaw = ui.get_planning_cam_yaw();
            let pitch = ui.get_planning_cam_pitch();
            let dist = ui.get_planning_cam_distance();
            let pan_x = ui.get_planning_cam_pan_x();
            let pan_y = ui.get_planning_cam_pan_y();
            let mvp = renderer::compute_mvp(yaw, pitch, dist, pan_x, pan_y, pw as f32 / ph as f32);

            let ps_lock = ps.lock().unwrap();
            let sel_type = ui.get_planning_selected_type();
            let sel_idx = ui.get_planning_selected_index();

            // If an area is selected for editing, check its face handles first.
            let area_kind = ui.get_planning_selected_area_kind();
            let area_idx = ui.get_planning_selected_area_index();
            if area_kind != 0 && area_idx >= 0 {
                let area = match area_kind {
                    1 => ps_lock.flight_areas.get(area_idx as usize),
                    2 => ps_lock.waypoint_areas.get(area_idx as usize),
                    _ => None,
                };
                if let Some(area) = area {
                    let handles = renderer::area_handle_positions(area.min, area.max);
                    let mut best: Option<(usize, f32)> = None;
                    for (i, h) in handles.iter().enumerate() {
                        if let Some((sx, sy)) = renderer::project_to_screen(*h, &mvp, pw, ph) {
                            let d = ((mx - sx).powi(2) + (my - sy).powi(2)).sqrt();
                            if d < 28.0 && best.map(|(_, bd)| d < bd).unwrap_or(true) {
                                best = Some((i, d));
                            }
                        }
                    }
                    if let Some((hi, _)) = best {
                        // World axis the handle slides along.
                        let axis = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]][hi / 2];
                        let h = handles[hi];
                        let h2 = [h[0] + axis[0] * 0.5, h[1] + axis[1] * 0.5, h[2] + axis[2] * 0.5];
                        let (s1, s2) = (
                            renderer::project_to_screen(h, &mvp, pw, ph),
                            renderer::project_to_screen(h2, &mvp, pw, ph),
                        );
                        if let (Some(s1), Some(s2)) = (s1, s2) {
                            let dvec = [s2.0 - s1.0, s2.1 - s1.1];
                            let len = (dvec[0] * dvec[0] + dvec[1] * dvec[1]).sqrt();
                            if len > 0.01 {
                                let mut gs = gs.lock().unwrap();
                                gs.area_handle = hi as i32;
                                gs.area_start_min = area.min;
                                gs.area_start_max = area.max;
                                gs.area_axis_screen = [dvec[0] / len, dvec[1] / len];
                                gs.area_world_per_px = 0.5 / len;
                                gs.drag_start_screen = [mx, my];
                                ui.set_planning_handle_active(true);
                                return;
                            }
                        }
                    }
                }
            }

            // If something is selected, check gizmo handles first
            if sel_idx >= 0 {
                let handle = match sel_type {
                    1 if (sel_idx as usize) < ps_lock.bs_render_data.len() => {
                        let (pos, rot) = &ps_lock.bs_render_data[sel_idx as usize];
                        let h = renderer::hit_test_anchor_gizmo(mx, my, *pos, &mvp, pw, ph);
                        if h != renderer::HANDLE_NONE {
                            h
                        } else {
                            // Also check rotation arcs for BS
                            let azimuth = rot[1][0].atan2(rot[0][0]);
                            let elevation = (-rot[2][0]).asin();
                            // Check azimuth arc
                            let az_end = [
                                pos[0] + 0.7 * (azimuth + std::f32::consts::PI / 3.0).cos(),
                                pos[1] + 0.7 * (azimuth + std::f32::consts::PI / 3.0).sin(),
                                pos[2],
                            ];
                            if let Some((sx, sy)) = renderer::project_to_screen(az_end, &mvp, pw, ph) {
                                if ((mx - sx).powi(2) + (my - sy).powi(2)).sqrt() < 15.0 {
                                    renderer::HANDLE_ROTATE_AZ
                                } else {
                                    let az_end2 = [
                                        pos[0] + 0.7 * (azimuth - std::f32::consts::PI / 3.0).cos(),
                                        pos[1] + 0.7 * (azimuth - std::f32::consts::PI / 3.0).sin(),
                                        pos[2],
                                    ];
                                    if let Some((sx2, sy2)) = renderer::project_to_screen(az_end2, &mvp, pw, ph) {
                                        if ((mx - sx2).powi(2) + (my - sy2).powi(2)).sqrt() < 15.0 {
                                            renderer::HANDLE_ROTATE_AZ
                                        } else {
                                            // Check elevation arc endpoints
                                            let el_end = [
                                                pos[0] + 0.7 * (elevation + std::f32::consts::PI / 3.0).cos() * azimuth.cos(),
                                                pos[1] + 0.7 * (elevation + std::f32::consts::PI / 3.0).cos() * azimuth.sin(),
                                                pos[2] - 0.7 * (elevation + std::f32::consts::PI / 3.0).sin(),
                                            ];
                                            if let Some((sx3, sy3)) = renderer::project_to_screen(el_end, &mvp, pw, ph) {
                                                if ((mx - sx3).powi(2) + (my - sy3).powi(2)).sqrt() < 15.0 {
                                                    renderer::HANDLE_ROTATE_EL
                                                } else {
                                                    renderer::HANDLE_NONE
                                                }
                                            } else {
                                                renderer::HANDLE_NONE
                                            }
                                        }
                                    } else {
                                        renderer::HANDLE_NONE
                                    }
                                }
                            } else {
                                renderer::HANDLE_NONE
                            }
                        }
                    }
                    2 if (sel_idx as usize) < ps_lock.anchor_positions.len() => {
                        renderer::hit_test_anchor_gizmo(mx, my, ps_lock.anchor_positions[sel_idx as usize], &mvp, pw, ph)
                    }
                    3 if (sel_idx as usize) < ps_lock.obstacles.len() => {
                        let pos = ps_lock.obstacles[sel_idx as usize].pos;
                        renderer::hit_test_anchor_gizmo(mx, my, pos, &mvp, pw, ph)
                    }
                    _ => renderer::HANDLE_NONE,
                };

                if handle != renderer::HANDLE_NONE {
                    ui.set_planning_active_handle(handle);
                    ui.set_planning_handle_active(true);
                    let mut gs = gs.lock().unwrap();
                    gs.drag_start_screen = [mx, my];
                    match sel_type {
                        1 => {
                            gs.drag_start_pos = ps_lock.bs_render_data[sel_idx as usize].0;
                            let bs = &ps_lock.base_stations[sel_idx as usize];
                            gs.drag_start_azimuth = bs.azimuth_deg;
                            gs.drag_start_elevation = bs.elevation_deg;
                        }
                        2 => {
                            gs.drag_start_pos = ps_lock.anchor_positions[sel_idx as usize];
                        }
                        3 => {
                            let obs = &ps_lock.obstacles[sel_idx as usize];
                            gs.drag_start_pos = obs.pos;
                            gs.drag_start_yaw = obs.yaw_deg;
                        }
                        _ => {}
                    }
                    return;
                }
            }

            // Hit test all objects
            // Check BS
            let bs_hit = renderer::hit_test_anchor(
                mx, my,
                &ps_lock.bs_render_data.iter().map(|(p, _)| *p).collect::<Vec<_>>(),
                &mvp, pw, ph, 15.0,
            );
            if bs_hit >= 0 {
                ui.set_planning_selected_type(1);
                ui.set_planning_selected_index(bs_hit);
                return;
            }

            // Check anchors
            let anc_hit = renderer::hit_test_anchor(
                mx, my, &ps_lock.anchor_positions, &mvp, pw, ph, 15.0,
            );
            if anc_hit >= 0 {
                ui.set_planning_selected_type(2);
                ui.set_planning_selected_index(anc_hit);
                return;
            }

            // Check obstacles
            let obs_positions: Vec<[f32; 3]> = ps_lock.obstacles.iter().map(|o| o.pos).collect();
            let obs_hit = renderer::hit_test_anchor(
                mx, my, &obs_positions, &mvp, pw, ph, 20.0,
            );
            if obs_hit >= 0 {
                ui.set_planning_selected_type(3);
                ui.set_planning_selected_index(obs_hit);
                return;
            }

            // Deselect
            ui.set_planning_selected_type(0);
            ui.set_planning_selected_index(-1);
        });

        // Mouse move (dragging gizmo)
        let ps = planning_state.clone();
        let gs = planning_gizmo_state.clone();
        let uw = ui_weak.clone();
        ui.on_planning_view_mouse_moved(move |mx, my| {
            let Some(ui) = uw.upgrade() else { return };
            if !ui.get_planning_handle_active() { return; }

            // Area face-handle drag (swarmfinity-style box resize).
            {
                let (handle, start_min, start_max, axis, wpp, start_screen) = {
                    let gs = gs.lock().unwrap();
                    (gs.area_handle, gs.area_start_min, gs.area_start_max,
                     gs.area_axis_screen, gs.area_world_per_px, gs.drag_start_screen)
                };
                if handle >= 0 {
                    let area_kind = ui.get_planning_selected_area_kind();
                    let area_idx = ui.get_planning_selected_area_index();
                    if area_kind != 0 && area_idx >= 0 {
                        // World delta along the handle's axis.
                        let dpx = (mx - start_screen[0]) * axis[0] + (my - start_screen[1]) * axis[1];
                        let delta = dpx * wpp;
                        let mut min = start_min;
                        let mut max = start_max;
                        let comp = (handle as usize) / 2; // 0=X,1=Y,2=Z
                        let is_max = (handle as usize) % 2 == 1;
                        const MIN_SPAN: f32 = 0.2;
                        if is_max {
                            max[comp] = (start_max[comp] + delta).max(min[comp] + MIN_SPAN);
                        } else {
                            min[comp] = (start_min[comp] + delta).min(max[comp] - MIN_SPAN);
                            if comp == 2 { min[comp] = min[comp].max(0.05); }
                        }
                        let mut ps = ps.lock().unwrap();
                        let area = match area_kind {
                            1 => ps.flight_areas.get_mut(area_idx as usize),
                            2 => ps.waypoint_areas.get_mut(area_idx as usize),
                            _ => None,
                        };
                        if let Some(area) = area {
                            area.min = min;
                            area.max = max;
                        }
                    }
                    return;
                }
            }

            let sel_type = ui.get_planning_selected_type();
            let sel_idx = ui.get_planning_selected_index() as usize;
            let handle = ui.get_planning_active_handle();

            let pw = ui.get_planning_width() as u32;
            let ph = ui.get_planning_height() as u32;
            let yaw_cam = ui.get_planning_cam_yaw();
            let pitch_cam = ui.get_planning_cam_pitch();
            let dist = ui.get_planning_cam_distance();
            let pan_x = ui.get_planning_cam_pan_x();
            let pan_y = ui.get_planning_cam_pan_y();

            let gs = gs.lock().unwrap();
            let dx_px = mx - gs.drag_start_screen[0];
            let dy_px = my - gs.drag_start_screen[1];
            let start_pos = gs.drag_start_pos;

            let mvp = renderer::compute_mvp(yaw_cam, pitch_cam, dist, pan_x, pan_y, pw as f32 / ph as f32);

            let sensitivity = dist * 0.002;
            let translate_delta = |axis: usize| -> f32 {
                let mut dir = [0.0f32; 3];
                dir[axis] = 1.0;
                let p0 = renderer::project_to_screen(start_pos, &mvp, pw, ph);
                let p1_pos = [start_pos[0] + dir[0], start_pos[1] + dir[1], start_pos[2] + dir[2]];
                let p1 = renderer::project_to_screen(p1_pos, &mvp, pw, ph);
                if let (Some((sx0, sy0)), Some((sx1, sy1))) = (p0, p1) {
                    let ax = sx1 - sx0;
                    let ay = sy1 - sy0;
                    let len2 = ax * ax + ay * ay;
                    if len2 > 0.001 {
                        (dx_px * ax + dy_px * ay) / len2
                    } else {
                        0.0
                    }
                } else {
                    dx_px * sensitivity
                }
            };

            let mut ps = ps.lock().unwrap();

            match handle {
                renderer::HANDLE_TRANSLATE_X | renderer::HANDLE_TRANSLATE_Y | renderer::HANDLE_TRANSLATE_Z => {
                    let axis = (handle - 1) as usize;
                    let delta = translate_delta(axis);
                    let mut new_pos = start_pos;
                    new_pos[axis] += delta;

                    match sel_type {
                        1 if sel_idx < ps.base_stations.len() => {
                            ps.base_stations[sel_idx].pos = new_pos;
                            rebuild_bs_render_data(&mut ps);
                        }
                        2 if sel_idx < ps.anchors.len() => {
                            ps.anchors[sel_idx].pos = new_pos;
                            rebuild_anchor_positions(&mut ps);
                        }
                        3 if sel_idx < ps.obstacles.len() => {
                            ps.obstacles[sel_idx].pos = new_pos;
                            rebuild_obstacle_meshes(&mut ps);
                        }
                        _ => {}
                    }
                }
                renderer::HANDLE_ROTATE_AZ => {
                    if sel_type == 1 && sel_idx < ps.base_stations.len() {
                        let delta = dx_px * 0.5;
                        ps.base_stations[sel_idx].azimuth_deg = gs.drag_start_azimuth + delta;
                        rebuild_bs_render_data(&mut ps);
                    }
                }
                renderer::HANDLE_ROTATE_EL => {
                    if sel_type == 1 && sel_idx < ps.base_stations.len() {
                        let delta = dy_px * 0.5;
                        ps.base_stations[sel_idx].elevation_deg = gs.drag_start_elevation + delta;
                        rebuild_bs_render_data(&mut ps);
                    }
                }
                renderer::HANDLE_ROTATE_YAW => {
                    if sel_type == 3 && sel_idx < ps.obstacles.len() {
                        let delta = dx_px * 0.5;
                        ps.obstacles[sel_idx].yaw_deg = gs.drag_start_yaw + delta;
                        rebuild_obstacle_meshes(&mut ps);
                    }
                }
                _ => {}
            }

            // Update UI model for the moved item
            match sel_type {
                1 if sel_idx < ps.base_stations.len() => {
                    let bs = &ps.base_stations[sel_idx];
                    let mut model: Vec<LhBaseStationData> = {
                        let m = ui.get_planning_base_stations();
                        (0..m.row_count()).filter_map(|i| m.row_data(i)).collect()
                    };
                    if sel_idx < model.len() {
                        model[sel_idx] = LhBaseStationData {
                            x: format!("{:.2}", bs.pos[0]).into(),
                            y: format!("{:.2}", bs.pos[1]).into(),
                            z: format!("{:.2}", bs.pos[2]).into(),
                            azimuth: format!("{:.1}", bs.azimuth_deg).into(),
                            elevation: format!("{:.1}", bs.elevation_deg).into(),
                        };
                        ui.set_planning_base_stations(slint::ModelRc::new(slint::VecModel::from(model)));
                    }
                }
                2 if sel_idx < ps.anchors.len() => {
                    let a = &ps.anchors[sel_idx];
                    let mut model: Vec<Tdoa3AnchorData> = {
                        let m = ui.get_planning_anchors();
                        (0..m.row_count()).filter_map(|i| m.row_data(i)).collect()
                    };
                    if sel_idx < model.len() {
                        model[sel_idx] = Tdoa3AnchorData {
                            x: format!("{:.2}", a.pos[0]).into(),
                            y: format!("{:.2}", a.pos[1]).into(),
                            z: format!("{:.2}", a.pos[2]).into(),
                        };
                        ui.set_planning_anchors(slint::ModelRc::new(slint::VecModel::from(model)));
                    }
                }
                3 if sel_idx < ps.obstacles.len() => {
                    let o = &ps.obstacles[sel_idx];
                    let mut model: Vec<PlanObstacleData> = {
                        let m = ui.get_planning_obstacles();
                        (0..m.row_count()).filter_map(|i| m.row_data(i)).collect()
                    };
                    if sel_idx < model.len() {
                        model[sel_idx] = PlanObstacleData {
                            kind: match o.kind { planning::ObstacleKind::Box => "Box", planning::ObstacleKind::Cylinder => "Cylinder" }.into(),
                            x: format!("{:.2}", o.pos[0]).into(),
                            y: format!("{:.2}", o.pos[1]).into(),
                            z: format!("{:.2}", o.pos[2]).into(),
                            yaw: format!("{:.1}", o.yaw_deg).into(),
                            width: format!("{:.2}", o.width).into(),
                            depth: format!("{:.2}", o.depth).into(),
                            height: format!("{:.2}", o.height).into(),
                            radius: format!("{:.2}", o.radius).into(),
                            color_r: o.color[0], color_g: o.color[1], color_b: o.color[2],
                            on_floor: o.on_floor,
                        };
                        ui.set_planning_obstacles(slint::ModelRc::new(slint::VecModel::from(model)));
                    }
                }
                _ => {}
            }
        });

        // Mouse released (end drag)
        let ps = planning_state.clone();
        let gs = planning_gizmo_state.clone();
        let vss = viz_scene_state.clone();
        let uw = ui_weak.clone();
        ui.on_planning_view_mouse_released(move || {
            let Some(ui) = uw.upgrade() else { return };
            ui.set_planning_handle_active(false);
            ui.set_planning_active_handle(0);
            // If we were dragging an area handle, finalize: refresh the list
            // editor rows and the visualizer overlay, then clear the drag.
            let was_area = {
                let mut gs = gs.lock().unwrap();
                let h = gs.area_handle;
                gs.area_handle = -1;
                h >= 0
            };
            if was_area {
                let ps = ps.lock().unwrap();
                ui.set_planning_flight_areas(slint::ModelRc::new(slint::VecModel::from(area_models(&ps.flight_areas))));
                ui.set_planning_waypoint_areas(slint::ModelRc::new(slint::VecModel::from(area_models(&ps.waypoint_areas))));
                let mut s = vss.lock().unwrap();
                s.flight_areas = ps.flight_areas.clone();
                s.waypoint_areas = ps.waypoint_areas.clone();
            }
        });

        // View pan
        let uw = ui_weak.clone();
        ui.on_planning_view_pan(move |dx_px, dy_px| {
            let Some(ui) = uw.upgrade() else { return };
            let yaw = ui.get_planning_cam_yaw();
            let scale = 0.01_f32;
            let (sy, cy) = yaw.sin_cos();
            let dx = dx_px * scale;
            let dy = -dy_px * scale;
            ui.set_planning_cam_pan_x(
                ui.get_planning_cam_pan_x() + dx * sy + dy * cy,
            );
            ui.set_planning_cam_pan_y(
                ui.get_planning_cam_pan_y() + dx * (-cy) + dy * sy,
            );
        });
    }
}
