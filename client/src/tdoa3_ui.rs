//! The TDoA3 coverage tab.

use slint::ComponentHandle;

use crate::app::AppContext;
use crate::{AppWindow, host::renderer, tdoa3, Tdoa3AnchorData};
use crate::host;

// TDoA3 Coverage shared state
pub(crate) struct Tdoa3State {
    pub(crate) anchors: Vec<tdoa3::Anchor>,
    pub(crate) voxels: Vec<(f32, f32, f32, f32)>,
    pub(crate) anchor_positions: Vec<[f32; 3]>,
    pub(crate) room: [f32; 3],
    pub(crate) room_offset: [f32; 3],
    pub(crate) undo_stack: Vec<Vec<tdoa3::Anchor>>,
    pub(crate) gdop_result: Option<tdoa3::GdopResult>,
    pub(crate) convex_hull: Option<tdoa3::ConvexHull>,
}

pub(crate) struct Tdoa3GizmoState {
    pub(crate) drag_start_screen: [f32; 2],
    pub(crate) drag_start_pos: [f32; 3],
}

pub(crate) fn setup(ui: &AppWindow, cx: &AppContext) {
    let AppContext { tdoa3_state, tdoa3_gizmo_state, .. } = cx;
    // --- TDoA3 Coverage callbacks ---
    {
        let tdoa3_state = tdoa3_state.clone();
        let ui_weak = ui.as_weak();

        // Save scene
        let ts = tdoa3_state.clone();
        let uw = ui_weak.clone();
        ui.on_tdoa3_save_scene(move || {
            let Some(ui) = uw.upgrade() else { return };
            let ts_lock = ts.lock().unwrap();
            let scene = tdoa3::Tdoa3Scene::new(
                ui.get_tdoa3_room_x().parse().unwrap_or(10.0),
                ui.get_tdoa3_room_y().parse().unwrap_or(10.0),
                ui.get_tdoa3_room_z().parse().unwrap_or(3.0),
                ui.get_tdoa3_resolution().parse().unwrap_or(2.0),
                ui.get_tdoa3_center_origin(),
                ui.get_tdoa3_max_range().parse().unwrap_or(15.0),
                ui.get_tdoa3_scale_max_value(),
                ui.get_tdoa3_show_outside_hull(),
                &ts_lock.anchors,
            );
            drop(ts_lock);
            slint::spawn_local(async move {
                let Some(handle) = host::dialog::FileDialog::new()
                    .set_title("Save TDoA3 Scene")
                    .add_filter("YAML", &["yaml", "yml"])
                    .save_file().await
                else { return };
                let path = handle.path().to_path_buf();

                if let Err(e) = tdoa3::save_scene(&path, &scene) {
                    eprintln!("Save error: {}", e);
                }
            }).unwrap();
        });

        // Load scene
        let ts = tdoa3_state.clone();
        let uw = ui_weak.clone();
        ui.on_tdoa3_load_scene(move || {
            let Some(ui) = uw.upgrade() else { return };
            let uw2 = ui.as_weak();
            let ts = ts.clone();
            slint::spawn_local(async move {
                let Some(handle) = host::dialog::FileDialog::new()
                    .set_title("Open TDoA3 Scene")
                    .add_filter("YAML", &["yaml", "yml"])
                    .pick_file().await
                else { return };
                let path = handle.path().to_path_buf();
                let Some(ui) = uw2.upgrade() else { return };

                match tdoa3::load_scene(&path) {
                    Ok(scene) => {
                        let mut ts = ts.lock().unwrap();
                        ts.anchors = scene.anchors();
                        update_anchor_ui_model(&ui, &ts.anchors);
                        ui.set_tdoa3_room_x(format!("{}", scene.room_x).into());
                        ui.set_tdoa3_room_y(format!("{}", scene.room_y).into());
                        ui.set_tdoa3_room_z(format!("{}", scene.room_z).into());
                        ui.set_tdoa3_resolution(format!("{}", scene.resolution).into());
                        ui.set_tdoa3_center_origin(scene.center_origin);
                        ui.set_tdoa3_max_range(format!("{}", scene.max_range).into());
                        ui.set_tdoa3_metric_index(DISP_GDOP as i32);
                        ui.set_tdoa3_scale_limit(2.0);
                        ui.set_tdoa3_scale_min_value(0.0);
                        ui.set_tdoa3_scale_max_value(0.5);
                        ui.set_tdoa3_show_outside_hull(scene.show_uncovered);
                        drop(ts);
                        ui.invoke_tdoa3_recompute();
                    }
                    Err(e) => eprintln!("Load error: {}", e),
                }
            }).unwrap();
        });

        // Add anchor
        let ts = tdoa3_state.clone();
        let uw = ui_weak.clone();
        ui.on_tdoa3_add_anchor(move || {
            let Some(ui) = uw.upgrade() else { return };
            let mut ts = ts.lock().unwrap();
            let snapshot = ts.anchors.clone();
            ts.undo_stack.push(snapshot);
            ts.anchors.push(tdoa3::Anchor {
                pos: [0.0, 0.0, 2.5],
            });
            update_anchor_ui_model(&ui, &ts.anchors);
            drop(ts);
            ui.invoke_tdoa3_recompute();
        });

        // Undo
        let ts = tdoa3_state.clone();
        let uw = ui_weak.clone();
        ui.on_tdoa3_undo(move || {
            let Some(ui) = uw.upgrade() else { return };
            let mut ts = ts.lock().unwrap();
            if let Some(prev) = ts.undo_stack.pop() {
                ts.anchors = prev;
                update_anchor_ui_model(&ui, &ts.anchors);
                ui.set_tdoa3_selected_anchor(-1);
            }
        });

        // Remove anchor
        let ts = tdoa3_state.clone();
        let uw = ui_weak.clone();
        ui.on_tdoa3_remove_anchor(move |idx| {
            let Some(ui) = uw.upgrade() else { return };
            let idx = idx as usize;
            let mut ts = ts.lock().unwrap();
            if idx < ts.anchors.len() {
                let snapshot = ts.anchors.clone();
                ts.undo_stack.push(snapshot);
                ts.anchors.remove(idx);
                update_anchor_ui_model(&ui, &ts.anchors);
                ui.set_tdoa3_selected_anchor(-1);
                drop(ts);
                ui.invoke_tdoa3_recompute();
            }
        });

        // Update anchor from UI fields
        let ts = tdoa3_state.clone();
        ui.on_tdoa3_update_anchor(move |idx, data| {
            let idx = idx as usize;
            let mut ts = ts.lock().unwrap();
            if idx < ts.anchors.len() {
                ts.anchors[idx] = tdoa3::Anchor {
                    pos: [
                        data.x.parse().unwrap_or(0.0),
                        data.y.parse().unwrap_or(0.0),
                        data.z.parse().unwrap_or(0.0),
                    ],
                };
            }
        });

        // Helper: extract voxels and update stats for the selected metric.
        // Display metric indices (ComboBox order)
        const DISP_GDOP: usize = 0;
        const DISP_HDOP: usize = 1;
        const DISP_VDOP: usize = 2;
        const DISP_XDOP: usize = 3;
        const DISP_YDOP: usize = 4;
        const DISP_PAIRS: usize = 5;
        const DISP_PAIR_SENS: usize = 6;
        const DISP_X_ERR: usize = 7;
        const DISP_Y_ERR: usize = 8;
        const DISP_Z_ERR: usize = 9;
        const DISP_XY_ERR: usize = 10;
        const DISP_GLOBAL_ERR: usize = 11;
        const DISP_X_SENS: usize = 12;
        const DISP_Y_SENS: usize = 13;
        const DISP_Z_SENS: usize = 14;
        const DISP_MIN_SENS: usize = 15;
        const DISP_MIN_XY_SENS: usize = 16;

        fn update_tdoa3_display(
            ui: &AppWindow,
            ts: &mut Tdoa3State,
        ) {
            let display_metric = ui.get_tdoa3_metric_index() as usize;
            let scale: f32 = ui.get_tdoa3_scale_max_value();

            if display_metric == DISP_PAIR_SENS {
                update_tdoa3_pair_sensitivity(ui, ts);
                return;
            }

            if display_metric >= DISP_X_SENS && display_metric <= DISP_Z_SENS {
                update_tdoa3_axis_sensitivity(ui, ts, display_metric - DISP_X_SENS);
                return;
            }

            if display_metric == DISP_MIN_SENS {
                update_tdoa3_min_sensitivity(ui, ts, true);
                return;
            }

            if display_metric == DISP_MIN_XY_SENS {
                update_tdoa3_min_sensitivity(ui, ts, false);
                return;
            }

            let Some(result) = &ts.gdop_result else { return };

            // Map display metric to storage metric + optional σ multiplier
            let sigma: f32 = ui.get_tdoa3_measurement_noise().parse().unwrap_or(0.15);
            let (storage_metric, metric_name, sigma_mult) = match display_metric {
                DISP_GDOP => (tdoa3::METRIC_GDOP, "GDOP", 1.0),
                DISP_HDOP => (tdoa3::METRIC_HDOP, "HDOP", 1.0),
                DISP_VDOP => (tdoa3::METRIC_VDOP, "VDOP", 1.0),
                DISP_XDOP => (tdoa3::METRIC_XDOP, "XDOP", 1.0),
                DISP_YDOP => (tdoa3::METRIC_YDOP, "YDOP", 1.0),
                DISP_PAIRS => (tdoa3::METRIC_PAIRS, "Pairs", 1.0),
                DISP_X_ERR => (tdoa3::METRIC_XDOP, "σ_x", sigma),
                DISP_Y_ERR => (tdoa3::METRIC_YDOP, "σ_y", sigma),
                DISP_Z_ERR => (tdoa3::METRIC_VDOP, "σ_z", sigma),
                DISP_XY_ERR => (tdoa3::METRIC_HDOP, "σ_xy", sigma),
                DISP_GLOBAL_ERR => (tdoa3::METRIC_GDOP, "σ_xyz", sigma),
                _ => (tdoa3::METRIC_GDOP, "GDOP", 1.0),
            };

            // Extract voxels and apply scaling
            let show_outside_hull = ui.get_tdoa3_show_outside_hull();
            ts.voxels = result.iter_voxels(ts.room_offset, storage_metric)
                .filter(|(x, y, z, _)| {
                    show_outside_hull || ts.convex_hull.as_ref().map_or(true, |h| h.contains(&[*x, *y, *z]))
                })
                .collect();
            if sigma_mult != 1.0 {
                for v in &mut ts.voxels {
                    if v.3.is_finite() {
                        v.3 *= sigma_mult;
                    }
                }
            }

            // Stats within slider range (min_scale..max_scale after sigma scaling)
            let min_slider = ui.get_tdoa3_scale_min_value();
            let max_slider = scale;

            // Compute min, max, mean, stddev over visible voxels
            let mut vis_min = f32::INFINITY;
            let mut vis_max = f32::NEG_INFINITY;
            let mut vis_sum = 0.0_f64;
            let mut vis_sum_sq = 0.0_f64;
            let mut vis_count = 0u32;
            let mut total_finite = 0u32;
            for &(_, _, _, v) in &ts.voxels {
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
            } else {
                0.0
            };

            if (DISP_X_ERR..=DISP_GLOBAL_ERR).contains(&display_metric) {
                let axis = match display_metric {
                    DISP_X_ERR => "X",
                    DISP_Y_ERR => "Y",
                    DISP_Z_ERR => "Z",
                    DISP_XY_ERR => "XY",
                    _ => "Global",
                };
                ui.set_tdoa3_stats_text_1(
                    format!("{} error: min {:.3}m  max {:.3}m  avg {:.3}m  σ {:.3}m",
                        axis, vis_min, vis_max, vis_mean, vis_std).into(),
                );
                let ratio = if total_finite > 0 { vis_count as f32 / total_finite as f32 } else { 0.0 };
                ui.set_tdoa3_stats_text_2(
                    format!("{} ∈ [{:.2}, {:.2}]m: {:.1}% (σ_tdoa = {}m)",
                        metric_name, min_slider, max_slider, ratio * 100.0, sigma).into(),
                );
            } else if display_metric == DISP_PAIRS {
                ui.set_tdoa3_stats_text_1(
                    format!("{} min: {:.0}  max: {:.0}  avg: {:.1}", metric_name,
                        if vis_min.is_finite() { vis_min } else { 0.0 },
                        if vis_max.is_finite() { vis_max } else { 0.0 },
                        vis_mean).into(),
                );
                let ratio3 = result.coverage_ratio_pairs(3.0);
                let ratio6 = result.coverage_ratio_pairs(6.0);
                ui.set_tdoa3_stats_text_2(
                    format!("≥3 pairs: {:.1}%  ≥6 pairs: {:.1}%", ratio3 * 100.0, ratio6 * 100.0).into(),
                );
            } else {
                ui.set_tdoa3_stats_text_1(
                    format!("{} min: {:.3}  max: {:.3}  avg: {:.3}  σ: {:.3}", metric_name,
                        if vis_min.is_finite() { vis_min } else { 0.0 },
                        if vis_max.is_finite() { vis_max } else { 0.0 },
                        vis_mean, vis_std).into(),
                );
                let ratio = if total_finite > 0 { vis_count as f32 / total_finite as f32 } else { 0.0 };
                ui.set_tdoa3_stats_text_2(
                    format!("{} ∈ [{:.2}, {:.2}]: {:.1}%", metric_name, min_slider, max_slider, ratio * 100.0).into(),
                );
            }

            let n = ts.anchors.len();
            ui.set_tdoa3_stats_text_3(
                format!("{} anchors, {} pairs", n, n * n.saturating_sub(1) / 2).into(),
            );
        }

        // Compute and display pair sensitivity for a single anchor pair.
        fn update_tdoa3_pair_sensitivity(
            ui: &AppWindow,
            ts: &mut Tdoa3State,
        ) {
            let idx_a: usize = ui.get_tdoa3_pair_a().parse().unwrap_or(0);
            let idx_b: usize = ui.get_tdoa3_pair_b().parse().unwrap_or(1);

            if idx_a >= ts.anchors.len() || idx_b >= ts.anchors.len() || idx_a == idx_b {
                ui.set_tdoa3_stats_text_1("Select two different valid anchor indices".into());
                ui.set_tdoa3_stats_text_2("".into());
                ui.set_tdoa3_stats_text_3("".into());
                ts.voxels.clear();
                return;
            }

            let room_x: f32 = ui.get_tdoa3_room_x().parse().unwrap_or(10.0);
            let room_y: f32 = ui.get_tdoa3_room_y().parse().unwrap_or(10.0);
            let room_z: f32 = ui.get_tdoa3_room_z().parse().unwrap_or(3.0);
            let resolution: f32 = ui.get_tdoa3_resolution().parse().unwrap_or(2.0);
            let max_range: f32 = ui.get_tdoa3_max_range().parse().unwrap_or(15.0);

            let voxels = tdoa3::compute_pair_sensitivity(
                room_x, room_y, room_z, resolution,
                ts.anchors[idx_a].pos,
                ts.anchors[idx_b].pos,
                max_range,
                ts.room_offset,
            );

            let show_outside_hull = ui.get_tdoa3_show_outside_hull();
            let voxels: Vec<_> = if show_outside_hull {
                voxels
            } else {
                voxels.into_iter()
                    .filter(|(x, y, z, _)| ts.convex_hull.as_ref().map_or(true, |h| h.contains(&[*x, *y, *z])))
                    .collect()
            };

            let (min_v, max_v, avg_v) = tdoa3::voxel_stats(&voxels);
            ui.set_tdoa3_stats_text_1(
                format!("|h| min: {:.2}  max: {:.2}  avg: {:.2}", min_v, max_v, avg_v).into(),
            );
            ui.set_tdoa3_stats_text_2(
                format!("Pair: anchor {} ↔ anchor {} (0 = degenerate, 2 = ideal)", idx_a, idx_b).into(),
            );
            ui.set_tdoa3_stats_text_3("".into());

            ts.voxels = voxels;
        }

        // Compute and display axis sensitivity (X, Y, or Z).
        fn update_tdoa3_axis_sensitivity(
            ui: &AppWindow,
            ts: &mut Tdoa3State,
            axis: usize, // 0=X, 1=Y, 2=Z
        ) {
            let axis_name = ["X", "Y", "Z"][axis];

            let room_x: f32 = ui.get_tdoa3_room_x().parse().unwrap_or(10.0);
            let room_y: f32 = ui.get_tdoa3_room_y().parse().unwrap_or(10.0);
            let room_z: f32 = ui.get_tdoa3_room_z().parse().unwrap_or(3.0);
            let resolution: f32 = ui.get_tdoa3_resolution().parse().unwrap_or(2.0);
            let max_range: f32 = ui.get_tdoa3_max_range().parse().unwrap_or(15.0);

            let voxels = tdoa3::compute_axis_sensitivity(
                room_x, room_y, room_z, resolution,
                &ts.anchors,
                max_range,
                ts.room_offset,
                axis,
            );

            let show_outside_hull = ui.get_tdoa3_show_outside_hull();
            let voxels: Vec<_> = if show_outside_hull {
                voxels
            } else {
                voxels.into_iter()
                    .filter(|(x, y, z, _)| ts.convex_hull.as_ref().map_or(true, |h| h.contains(&[*x, *y, *z])))
                    .collect()
            };

            let (min_v, max_v, avg_v) = tdoa3::voxel_stats(&voxels);
            ui.set_tdoa3_stats_text_1(
                format!("{} sensitivity: min {:.2}  max {:.2}  avg {:.2}", axis_name, min_v, max_v, avg_v).into(),
            );
            ui.set_tdoa3_stats_text_2(
                "High = measurements respond to movement, low = flat hyperbolas".into(),
            );
            ui.set_tdoa3_stats_text_3("".into());

            ts.voxels = voxels;
        }

        // Compute and display min-of-three-axes sensitivity.
        fn update_tdoa3_min_sensitivity(
            ui: &AppWindow,
            ts: &mut Tdoa3State,
            include_z: bool,
        ) {
            let room_x: f32 = ui.get_tdoa3_room_x().parse().unwrap_or(10.0);
            let room_y: f32 = ui.get_tdoa3_room_y().parse().unwrap_or(10.0);
            let room_z: f32 = ui.get_tdoa3_room_z().parse().unwrap_or(3.0);
            let resolution: f32 = ui.get_tdoa3_resolution().parse().unwrap_or(2.0);
            let max_range: f32 = ui.get_tdoa3_max_range().parse().unwrap_or(15.0);

            let voxels = tdoa3::compute_min_axis_sensitivity(
                room_x, room_y, room_z, resolution,
                &ts.anchors,
                max_range,
                ts.room_offset,
                include_z,
            );

            let show_outside_hull = ui.get_tdoa3_show_outside_hull();
            let voxels: Vec<_> = if show_outside_hull {
                voxels
            } else {
                voxels.into_iter()
                    .filter(|(x, y, z, _)| ts.convex_hull.as_ref().map_or(true, |h| h.contains(&[*x, *y, *z])))
                    .collect()
            };

            let label = if include_z { "Min XYZ" } else { "Min XY" };
            let (min_v, max_v, avg_v) = tdoa3::voxel_stats(&voxels);
            ui.set_tdoa3_stats_text_1(
                format!("{} sensitivity: min {:.2}  max {:.2}  avg {:.2}", label, min_v, max_v, avg_v).into(),
            );
            ui.set_tdoa3_stats_text_2(
                "Weakest axis at each point (low = bottleneck for positioning)".into(),
            );
            ui.set_tdoa3_stats_text_3("".into());

            ts.voxels = voxels;
        }

        // Recompute
        let ts = tdoa3_state.clone();
        let uw = ui_weak.clone();
        ui.on_tdoa3_recompute(move || {
            let Some(ui) = uw.upgrade() else { return };

            let room_x: f32 = ui.get_tdoa3_room_x().parse().unwrap_or(10.0);
            let room_y: f32 = ui.get_tdoa3_room_y().parse().unwrap_or(10.0);
            let room_z: f32 = ui.get_tdoa3_room_z().parse().unwrap_or(3.0);
            let resolution: f32 = ui.get_tdoa3_resolution().parse().unwrap_or(2.0);
            let max_range: f32 = ui.get_tdoa3_max_range().parse().unwrap_or(15.0);
            let center = ui.get_tdoa3_center_origin();

            let offset = if center {
                [-room_x / 2.0, -room_y / 2.0, 0.0]
            } else {
                [0.0, 0.0, 0.0]
            };

            let mut ts = ts.lock().unwrap();

            let result = tdoa3::compute_gdop(
                room_x, room_y, room_z, resolution,
                &ts.anchors,
                max_range,
                offset,
            );

            ts.room = [room_x, room_y, room_z];
            ts.room_offset = offset;
            ts.anchor_positions = ts.anchors.iter().map(|a| a.pos).collect();
            ts.convex_hull = tdoa3::ConvexHull::build(&ts.anchor_positions);
            ts.gdop_result = Some(result);

            update_tdoa3_display(&ui, &mut ts);
        });

        // Metric changed: re-extract voxels from cached result (no recompute needed)
        let ts = tdoa3_state.clone();
        let uw = ui_weak.clone();
        ui.on_tdoa3_metric_changed(move |idx| {
            let Some(ui) = uw.upgrade() else { return };
            let idx = idx as usize;
            // Set sensible slider range and default for the selected metric
            let (max, default): (f32, f32) = match idx {
                DISP_PAIRS => (30.0, 10.0),
                DISP_PAIR_SENS => (2.0, 2.0),
                DISP_X_ERR | DISP_Y_ERR | DISP_Z_ERR | DISP_XY_ERR => (1.0, 0.03),
                DISP_GLOBAL_ERR => (1.0, 0.05),
                DISP_X_SENS | DISP_Y_SENS | DISP_Z_SENS | DISP_MIN_SENS | DISP_MIN_XY_SENS => (10.0, 5.0),
                DISP_XDOP | DISP_YDOP => (2.0, 0.2),
                DISP_GDOP => (2.0, 0.5),
                _ => (2.0, 0.3), // HDOP, VDOP
            };
            ui.set_tdoa3_scale_limit(max);
            ui.set_tdoa3_scale_min_value(0.0);
            ui.set_tdoa3_scale_max_value(default.min(max));
            let mut ts = ts.lock().unwrap();
            update_tdoa3_display(&ui, &mut ts);
        });

        // Pair selection changed: recompute pair sensitivity
        let ts = tdoa3_state.clone();
        let uw = ui_weak.clone();
        ui.on_tdoa3_pair_changed(move || {
            let Some(ui) = uw.upgrade() else { return };
            if ui.get_tdoa3_metric_index() as usize == DISP_PAIR_SENS {
                let mut ts = ts.lock().unwrap();
                update_tdoa3_pair_sensitivity(&ui, &mut ts);
            }
        });

        // Helper to sync anchor data back to the UI model
        fn update_anchor_ui_model(ui: &AppWindow, anchors: &[tdoa3::Anchor]) {
            let model: Vec<Tdoa3AnchorData> = anchors
                .iter()
                .map(|a| Tdoa3AnchorData {
                    x: format!("{:.2}", a.pos[0]).into(),
                    y: format!("{:.2}", a.pos[1]).into(),
                    z: format!("{:.2}", a.pos[2]).into(),
                })
                .collect();
            ui.set_tdoa3_anchors(slint::ModelRc::new(slint::VecModel::from(model)));
        }

        // --- TDoA3 gizmo mouse interaction ---

        let ts = tdoa3_state.clone();
        let gs = tdoa3_gizmo_state.clone();
        let uw = ui_weak.clone();
        ui.on_tdoa3_view_mouse_pressed(move |x, y| {
            let Some(ui) = uw.upgrade() else { return };

            let width = ui.get_tdoa3_width() as u32;
            let height = ui.get_tdoa3_height() as u32;
            if width == 0 || height == 0 {
                return;
            }

            let mvp = renderer::compute_mvp(
                ui.get_tdoa3_cam_yaw(),
                ui.get_tdoa3_cam_pitch(),
                ui.get_tdoa3_cam_distance(),
                ui.get_tdoa3_cam_pan_x(),
                ui.get_tdoa3_cam_pan_y(),
                width as f32 / height as f32,
            );

            let ts = ts.lock().unwrap();
            let sel = ui.get_tdoa3_selected_anchor();

            // First check gizmo handles if an anchor is selected
            if sel >= 0 && (sel as usize) < ts.anchor_positions.len() {
                let handle = renderer::hit_test_anchor_gizmo(
                    x, y,
                    ts.anchor_positions[sel as usize],
                    &mvp, width, height,
                );
                if handle != renderer::HANDLE_NONE {
                    let mut gs = gs.lock().unwrap();
                    gs.drag_start_screen = [x, y];
                    gs.drag_start_pos = ts.anchor_positions[sel as usize];
                    ui.set_tdoa3_handle_active(true);
                    ui.set_tdoa3_active_handle(handle);
                    return;
                }
            }

            // Then check anchor hit
            let hit = renderer::hit_test_anchor(
                x, y,
                &ts.anchor_positions,
                &mvp, width, height,
                20.0,
            );
            ui.set_tdoa3_selected_anchor(hit);
            ui.set_tdoa3_handle_active(false);
            ui.set_tdoa3_active_handle(renderer::HANDLE_NONE);
        });

        // Mouse moved: drag handle
        let ts = tdoa3_state.clone();
        let gs = tdoa3_gizmo_state.clone();
        let uw = ui_weak.clone();
        ui.on_tdoa3_view_mouse_moved(move |x, y| {
            let Some(ui) = uw.upgrade() else { return };
            if !ui.get_tdoa3_handle_active() {
                return;
            }

            let sel = ui.get_tdoa3_selected_anchor();
            let active_handle = ui.get_tdoa3_active_handle();
            let mut ts = ts.lock().unwrap();
            let gs = gs.lock().unwrap();

            let idx = sel as usize;
            if idx >= ts.anchors.len() {
                return;
            }

            let dx_screen = x - gs.drag_start_screen[0];
            let dy_screen = y - gs.drag_start_screen[1];

            match active_handle {
                renderer::HANDLE_TRANSLATE_X
                | renderer::HANDLE_TRANSLATE_Y
                | renderer::HANDLE_TRANSLATE_Z => {
                    let width = ui.get_tdoa3_width() as u32;
                    let height = ui.get_tdoa3_height() as u32;
                    if width == 0 || height == 0 {
                        return;
                    }
                    let mvp = renderer::compute_mvp(
                        ui.get_tdoa3_cam_yaw(),
                        ui.get_tdoa3_cam_pitch(),
                        ui.get_tdoa3_cam_distance(),
                        ui.get_tdoa3_cam_pan_x(),
                        ui.get_tdoa3_cam_pan_y(),
                        width as f32 / height as f32,
                    );

                    let axis_idx = (active_handle - 1) as usize;
                    let mut axis_dir = [0.0f32; 3];
                    axis_dir[axis_idx] = 1.0;

                    let p0 = gs.drag_start_pos;
                    let p1 = [
                        p0[0] + axis_dir[0],
                        p0[1] + axis_dir[1],
                        p0[2] + axis_dir[2],
                    ];

                    if let (Some((sx0, sy0)), Some((sx1, sy1))) = (
                        renderer::project_to_screen(p0, &mvp, width, height),
                        renderer::project_to_screen(p1, &mvp, width, height),
                    ) {
                        let sdx = sx1 - sx0;
                        let sdy = sy1 - sy0;
                        let slen = (sdx * sdx + sdy * sdy).sqrt();
                        if slen > 1e-5 {
                            let proj = (dx_screen * sdx + dy_screen * sdy) / slen;
                            let world_delta = proj / slen;
                            let mut new_pos = gs.drag_start_pos;
                            new_pos[axis_idx] += world_delta;
                            ts.anchors[idx].pos = new_pos;
                            ts.anchor_positions[idx] = new_pos;
                            update_anchor_ui_model(&ui, &ts.anchors);
                        }
                    }
                }
                _ => {}
            }
        });

        // Mouse released
        let uw = ui_weak.clone();
        ui.on_tdoa3_view_mouse_released(move || {
            if let Some(ui) = uw.upgrade() {
                ui.set_tdoa3_handle_active(false);
                ui.set_tdoa3_active_handle(renderer::HANDLE_NONE);
            }
        });

        // Pan
        let uw = ui_weak.clone();
        ui.on_tdoa3_view_pan(move |dx_px, dy_px| {
            let Some(ui) = uw.upgrade() else { return };
            let yaw = ui.get_tdoa3_cam_yaw();
            let scale = 0.01_f32;
            let (sy, cy) = yaw.sin_cos();
            let dx = dx_px * scale;
            let dy = -dy_px * scale;
            ui.set_tdoa3_cam_pan_x(
                ui.get_tdoa3_cam_pan_x() + dx * sy + dy * cy,
            );
            ui.set_tdoa3_cam_pan_y(
                ui.get_tdoa3_cam_pan_y() + dx * (-cy) + dy * sy,
            );
        });
    }
}
