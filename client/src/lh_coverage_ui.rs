//! The Lighthouse coverage tab.

use slint::{ComponentHandle, Model};

use crate::app::AppContext;
use crate::{AppWindow, coverage, LhBaseStationData, host::renderer};
use crate::host;

// LH Coverage shared state (Mutex for thread-safe access from rendering notifier)
pub(crate) struct CoverageState {
    pub(crate) base_stations: Vec<coverage::BaseStation>,
    pub(crate) voxels: Vec<(f32, f32, f32, u8)>,
    pub(crate) room: [f32; 3],
    pub(crate) room_offset: [f32; 3],
    pub(crate) bs_render_data: Vec<([f32; 3], [[f32; 3]; 3])>,
    pub(crate) undo_stack: Vec<Vec<coverage::BaseStation>>,
    pub(crate) trajectories: Vec<Vec<[f32; 3]>>,
}

pub(crate) struct GizmoState {
    pub(crate) drag_start_screen: [f32; 2],
    pub(crate) drag_start_pos: [f32; 3],
    pub(crate) drag_start_azimuth: f32,
    pub(crate) drag_start_elevation: f32,
}

pub(crate) fn setup(ui: &AppWindow, cx: &AppContext) {
    let AppContext { coverage_state, gizmo_state, .. } = cx;
    // LH Coverage callbacks
    {
        let ui_weak = ui.as_weak();
        let coverage_state = coverage_state.clone();

        // Load geometry from YAML file
        let cs = coverage_state.clone();
        let uw = ui_weak.clone();
        ui.on_lh_cov_load_geometry(move || {
            let Some(ui) = uw.upgrade() else { return };
            let uw2 = ui.as_weak();
            let cs = cs.clone();
            slint::spawn_local(async move {
                let Some(handle) = host::dialog::FileDialog::new()
                    .add_filter("YAML", &["yaml", "yml"])
                    .pick_file().await
                else { return };
                let path = handle.path().to_path_buf();
                let Some(ui) = uw2.upgrade() else { return };

                match coverage::load_geometry_yaml(&path) {
                    Ok(stations) => {
                        let mut cs = cs.lock().unwrap();
                        cs.base_stations = stations;
                        cs.bs_render_data = cs
                            .base_stations
                            .iter()
                            .map(|bs| (bs.pos, bs.rotation_matrix()))
                            .collect();
                        // Clear previous coverage
                        cs.voxels.clear();

                        let model: Vec<LhBaseStationData> = cs
                            .base_stations
                            .iter()
                            .map(|bs| LhBaseStationData {
                                x: format!("{:.2}", bs.pos[0]).into(),
                                y: format!("{:.2}", bs.pos[1]).into(),
                                z: format!("{:.2}", bs.pos[2]).into(),
                                azimuth: format!("{:.1}", bs.azimuth_deg).into(),
                                elevation: format!("{:.1}", bs.elevation_deg).into(),
                            })
                            .collect();
                        ui.set_lh_cov_base_stations(slint::ModelRc::new(slint::VecModel::from(
                            model,
                        )));
                        ui.set_lh_cov_coverage_1_text("".into());
                        ui.set_lh_cov_coverage_2_text("".into());
                        ui.set_lh_cov_selected_bs(-1);
                    }
                    Err(e) => {
                        eprintln!("Failed to load geometry: {}", e);
                    }
                }
            }).unwrap();
        });

        // Save scene
        let cs = coverage_state.clone();
        let uw = ui_weak.clone();
        ui.on_lh_cov_save_scene(move || {
            let Some(ui) = uw.upgrade() else { return };
            let uw2 = ui.as_weak();
            let cs = cs.clone();
            slint::spawn_local(async move {
                let Some(handle) = host::dialog::FileDialog::new()
                    .add_filter("YAML", &["yaml", "yml"])
                    .set_file_name("lh_coverage.yaml")
                    .save_file().await
                else { return };
                let path = handle.path().to_path_buf();
                let Some(ui) = uw2.upgrade() else { return };

                let cs = cs.lock().unwrap();
                let scene = coverage::Scene::new(
                    ui.get_lh_cov_room_x().parse().unwrap_or(8.0),
                    ui.get_lh_cov_room_y().parse().unwrap_or(8.0),
                    ui.get_lh_cov_room_z().parse().unwrap_or(3.0),
                    ui.get_lh_cov_resolution().parse().unwrap_or(5.0),
                    ui.get_lh_cov_center_origin(),
                    ui.get_lh_cov_receiver_fov_enabled(),
                    ui.get_lh_cov_tilt_compensation_enabled(),
                    ui.get_lh_cov_max_tilt_angle().parse().unwrap_or(10.0),
                    ui.get_lh_cov_max_bs_distance().parse().unwrap_or(5.0),
                    [
                        ui.get_lh_cov_show_coverage_0(),
                        ui.get_lh_cov_show_coverage_1(),
                        ui.get_lh_cov_show_coverage_2(),
                        ui.get_lh_cov_show_coverage_3(),
                        ui.get_lh_cov_show_coverage_4(),
                    ],
                    [
                        ui.get_lh_cov_room_offset_x().parse().unwrap_or(0.0),
                        ui.get_lh_cov_room_offset_y().parse().unwrap_or(0.0),
                        ui.get_lh_cov_room_offset_z().parse().unwrap_or(0.0),
                    ],
                    &cs.base_stations,
                );
                if let Err(e) = coverage::save_scene(&path, &scene) {
                    eprintln!("Failed to save scene: {}", e);
                }
            }).unwrap();
        });

        // Load scene
        let cs = coverage_state.clone();
        let uw = ui_weak.clone();
        ui.on_lh_cov_load_scene(move || {
            let Some(ui) = uw.upgrade() else { return };
            let uw2 = ui.as_weak();
            let cs = cs.clone();
            slint::spawn_local(async move {
                let Some(handle) = host::dialog::FileDialog::new()
                    .add_filter("YAML", &["yaml", "yml"])
                    .pick_file().await
                else { return };
                let path = handle.path().to_path_buf();
                let Some(ui) = uw2.upgrade() else { return };

                match coverage::load_scene(&path) {
                    Ok(scene) => {
                        let mut cs = cs.lock().unwrap();
                        cs.base_stations = scene.base_stations();
                        cs.bs_render_data = cs
                            .base_stations
                            .iter()
                            .map(|bs| (bs.pos, bs.rotation_matrix()))
                            .collect();
                        cs.voxels.clear();
                        cs.undo_stack.clear();

                        ui.set_lh_cov_room_x(format!("{}", scene.room_x).into());
                        ui.set_lh_cov_room_y(format!("{}", scene.room_y).into());
                        ui.set_lh_cov_room_z(format!("{}", scene.room_z).into());
                        ui.set_lh_cov_resolution(format!("{}", scene.resolution).into());
                        ui.set_lh_cov_center_origin(scene.center_origin);
                        ui.set_lh_cov_receiver_fov_enabled(scene.receiver_fov_enabled);
                        ui.set_lh_cov_tilt_compensation_enabled(scene.tilt_compensation_enabled);
                        ui.set_lh_cov_max_tilt_angle(format!("{}", scene.max_tilt_angle).into());
                        ui.set_lh_cov_max_bs_distance(format!("{}", scene.max_bs_distance).into());
                        ui.set_lh_cov_show_coverage_0(scene.show_coverage[0]);
                        ui.set_lh_cov_show_coverage_1(scene.show_coverage[1]);
                        ui.set_lh_cov_show_coverage_2(scene.show_coverage[2]);
                        ui.set_lh_cov_show_coverage_3(scene.show_coverage[3]);
                        ui.set_lh_cov_show_coverage_4(scene.show_coverage[4]);
                        ui.set_lh_cov_room_offset_x(format!("{}", scene.room_offset[0]).into());
                        ui.set_lh_cov_room_offset_y(format!("{}", scene.room_offset[1]).into());
                        ui.set_lh_cov_room_offset_z(format!("{}", scene.room_offset[2]).into());

                        let model: Vec<LhBaseStationData> = cs
                            .base_stations
                            .iter()
                            .map(|bs| LhBaseStationData {
                                x: format!("{:.2}", bs.pos[0]).into(),
                                y: format!("{:.2}", bs.pos[1]).into(),
                                z: format!("{:.2}", bs.pos[2]).into(),
                                azimuth: format!("{:.1}", bs.azimuth_deg).into(),
                                elevation: format!("{:.1}", bs.elevation_deg).into(),
                            })
                            .collect();
                        ui.set_lh_cov_base_stations(slint::ModelRc::new(slint::VecModel::from(
                            model,
                        )));
                        ui.set_lh_cov_coverage_1_text("".into());
                        ui.set_lh_cov_coverage_2_text("".into());
                        ui.set_lh_cov_selected_bs(-1);

                        drop(cs);
                        ui.invoke_lh_cov_recompute();
                    }
                    Err(e) => {
                        eprintln!("Failed to load scene: {}", e);
                    }
                }
            }).unwrap();
        });

        // Undo last BS movement
        let cs = coverage_state.clone();
        let uw = ui_weak.clone();
        ui.on_lh_cov_undo(move || {
            let Some(ui) = uw.upgrade() else { return };
            let mut cs = cs.lock().unwrap();
            if let Some(prev) = cs.undo_stack.pop() {
                cs.base_stations = prev;
                cs.bs_render_data = cs
                    .base_stations
                    .iter()
                    .map(|bs| (bs.pos, bs.rotation_matrix()))
                    .collect();
                let model: Vec<LhBaseStationData> = cs
                    .base_stations
                    .iter()
                    .map(|bs| LhBaseStationData {
                        x: format!("{:.2}", bs.pos[0]).into(),
                        y: format!("{:.2}", bs.pos[1]).into(),
                        z: format!("{:.2}", bs.pos[2]).into(),
                        azimuth: format!("{:.1}", bs.azimuth_deg).into(),
                        elevation: format!("{:.1}", bs.elevation_deg).into(),
                    })
                    .collect();
                ui.set_lh_cov_base_stations(slint::ModelRc::new(slint::VecModel::from(model)));
            }
        });

        // Add base station
        let cs = coverage_state.clone();
        let uw = ui_weak.clone();
        ui.on_lh_cov_add_bs(move || {
            let Some(ui) = uw.upgrade() else { return };
            let mut model: Vec<LhBaseStationData> = {
                let m = ui.get_lh_cov_base_stations();
                (0..m.row_count()).filter_map(|i| m.row_data(i)).collect()
            };
            model.push(LhBaseStationData {
                x: "4.0".into(),
                y: "4.0".into(),
                z: "3.0".into(),
                azimuth: "0.0".into(),
                elevation: "45.0".into(),
            });
            ui.set_lh_cov_base_stations(slint::ModelRc::new(slint::VecModel::from(model)));

            // Also update internal state
            let mut cs = cs.lock().unwrap();
            cs.base_stations.push(coverage::BaseStation {
                pos: [4.0, 4.0, 3.0],
                azimuth_deg: 0.0,
                elevation_deg: 45.0,
            });
        });

        // Remove base station
        let cs = coverage_state.clone();
        let uw = ui_weak.clone();
        ui.on_lh_cov_remove_bs(move |idx| {
            let Some(ui) = uw.upgrade() else { return };
            let idx = idx as usize;
            let mut model: Vec<LhBaseStationData> = {
                let m = ui.get_lh_cov_base_stations();
                (0..m.row_count()).filter_map(|i| m.row_data(i)).collect()
            };
            if idx < model.len() {
                model.remove(idx);
            }
            ui.set_lh_cov_base_stations(slint::ModelRc::new(slint::VecModel::from(model)));

            let mut cs = cs.lock().unwrap();
            if idx < cs.base_stations.len() {
                cs.base_stations.remove(idx);
            }
        });

        // Update base station
        let cs = coverage_state.clone();
        ui.on_lh_cov_update_bs(move |idx, data| {
            let idx = idx as usize;
            let mut cs = cs.lock().unwrap();
            if idx < cs.base_stations.len() {
                cs.base_stations[idx] = coverage::BaseStation {
                    pos: [
                        data.x.parse().unwrap_or(0.0),
                        data.y.parse().unwrap_or(0.0),
                        data.z.parse().unwrap_or(0.0),
                    ],
                    azimuth_deg: data.azimuth.parse().unwrap_or(0.0),
                    elevation_deg: data.elevation.parse().unwrap_or(0.0),
                };
            }
        });

        // Recompute coverage
        let cs = coverage_state.clone();
        let uw = ui_weak.clone();
        ui.on_lh_cov_recompute(move || {
            let Some(ui) = uw.upgrade() else { return };

            let room_x: f32 = ui.get_lh_cov_room_x().parse().unwrap_or(8.0);
            let room_y: f32 = ui.get_lh_cov_room_y().parse().unwrap_or(8.0);
            let room_z: f32 = ui.get_lh_cov_room_z().parse().unwrap_or(3.0);
            let resolution: f32 = ui.get_lh_cov_resolution().parse().unwrap_or(2.0);
            let center = ui.get_lh_cov_center_origin();

            let user_offset_x: f32 = ui.get_lh_cov_room_offset_x().parse().unwrap_or(0.0);
            let user_offset_y: f32 = ui.get_lh_cov_room_offset_y().parse().unwrap_or(0.0);
            let user_offset_z: f32 = ui.get_lh_cov_room_offset_z().parse().unwrap_or(0.0);
            let offset = if center {
                [-room_x / 2.0 + user_offset_x, -room_y / 2.0 + user_offset_y, user_offset_z]
            } else {
                [user_offset_x, user_offset_y, user_offset_z]
            };

            let mut cs = cs.lock().unwrap();

            let receiver_fov = if ui.get_lh_cov_receiver_fov_enabled() {
                Some(170.0_f32)
            } else {
                None
            };

            let tilt_reduction = if ui.get_lh_cov_tilt_compensation_enabled() {
                Some(ui.get_lh_cov_max_tilt_angle().parse::<f32>().unwrap_or(10.0))
            } else {
                None
            };

            let max_dist: f32 = ui.get_lh_cov_max_bs_distance().parse().unwrap_or(5.0);

            let result = coverage::compute_coverage(
                room_x, room_y, room_z, resolution,
                &cs.base_stations,
                160.0, 115.0,
                receiver_fov,
                tilt_reduction,
                max_dist,
                offset,
            );

            let ratio1 = result.coverage_ratio(1);
            let ratio2 = result.coverage_ratio(2);
            ui.set_lh_cov_coverage_1_text(
                format!("Coverage (1+ BS): {:.1}%", ratio1 * 100.0).into(),
            );
            ui.set_lh_cov_coverage_2_text(
                format!("Coverage (2+ BS): {:.1}%", ratio2 * 100.0).into(),
            );

            cs.room = [room_x, room_y, room_z];
            cs.room_offset = offset;
            cs.voxels = result.iter_voxels(offset).collect();
            cs.bs_render_data = cs
                .base_stations
                .iter()
                .map(|bs| (bs.pos, bs.rotation_matrix()))
                .collect();
        });

        // Load trajectories from CSV
        let cs = coverage_state.clone();
        let uw = ui_weak.clone();
        ui.on_lh_cov_load_trajectories(move || {
            let Some(ui) = uw.upgrade() else { return };
            let uw2 = ui.as_weak();
            let cs = cs.clone();
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
                        ui.set_lh_cov_trajectories_status(
                            format!("{} CFs, {} steps", n_cfs, n_steps).into(),
                        );
                        cs.lock().unwrap().trajectories = trajectories;
                    }
                    Err(e) => {
                        ui.set_lh_cov_trajectories_status(
                            format!("Error: {}", e).into(),
                        );
                    }
                }
            }).unwrap();
        });

        // --- Gizmo mouse interaction callbacks ---

        // Helper to sync BS data back to the UI model
        fn update_bs_ui_model(ui: &AppWindow, base_stations: &[coverage::BaseStation]) {
            let model: Vec<LhBaseStationData> = base_stations
                .iter()
                .map(|bs| LhBaseStationData {
                    x: format!("{:.2}", bs.pos[0]).into(),
                    y: format!("{:.2}", bs.pos[1]).into(),
                    z: format!("{:.2}", bs.pos[2]).into(),
                    azimuth: format!("{:.1}", bs.azimuth_deg).into(),
                    elevation: format!("{:.1}", bs.elevation_deg).into(),
                })
                .collect();
            ui.set_lh_cov_base_stations(slint::ModelRc::new(slint::VecModel::from(model)));
        }

        // Mouse pressed: hit-test handles then base stations
        let cs = coverage_state.clone();
        let gs = gizmo_state.clone();
        let uw = ui_weak.clone();
        ui.on_lh_cov_view_mouse_pressed(move |x, y| {
            let Some(ui) = uw.upgrade() else { return };

            let width = ui.get_lh_coverage_width() as u32;
            let height = ui.get_lh_coverage_height() as u32;
            if width == 0 || height == 0 {
                return;
            }
            let mvp = renderer::compute_mvp(
                ui.get_lh_cov_cam_yaw(),
                ui.get_lh_cov_cam_pitch(),
                ui.get_lh_cov_cam_distance(),
                ui.get_lh_cov_cam_pan_x(),
                ui.get_lh_cov_cam_pan_y(),
                width as f32 / height as f32,
            );

            let mut cs = cs.lock().unwrap();
            let mut gs = gs.lock().unwrap();
            let selected = ui.get_lh_cov_selected_bs();

            // If a BS is selected, check handles first
            if selected >= 0 && (selected as usize) < cs.bs_render_data.len() {
                let (pos, rot) = &cs.bs_render_data[selected as usize];
                let handle =
                    renderer::hit_test_gizmo(x, y, *pos, rot, &mvp, width, height);
                if handle != renderer::HANDLE_NONE {
                    // Save undo snapshot before drag
                    let snapshot = cs.base_stations.clone();
                    cs.undo_stack.push(snapshot);
                    let bs = &cs.base_stations[selected as usize];
                    gs.drag_start_screen = [x, y];
                    gs.drag_start_pos = bs.pos;
                    gs.drag_start_azimuth = bs.azimuth_deg;
                    gs.drag_start_elevation = bs.elevation_deg;
                    ui.set_lh_cov_handle_active(true);
                    ui.set_lh_cov_active_handle(handle);
                    return;
                }
            }

            // Try selecting a BS
            let hit = renderer::hit_test_base_station(
                x, y, &cs.bs_render_data, &mvp, width, height, 20.0,
            );
            ui.set_lh_cov_selected_bs(hit);
            ui.set_lh_cov_handle_active(false);
            ui.set_lh_cov_active_handle(renderer::HANDLE_NONE);
        });

        // Mouse moved: drag the active handle
        let cs = coverage_state.clone();
        let gs = gizmo_state.clone();
        let uw = ui_weak.clone();
        ui.on_lh_cov_view_mouse_moved(move |x, y| {
            let Some(ui) = uw.upgrade() else { return };
            let active_handle = ui.get_lh_cov_active_handle();
            let selected = ui.get_lh_cov_selected_bs();
            if active_handle == renderer::HANDLE_NONE || selected < 0 {
                return;
            }

            let idx = selected as usize;
            let gs = gs.lock().unwrap();
            let mut cs = cs.lock().unwrap();
            if idx >= cs.base_stations.len() {
                return;
            }

            let dx_screen = x - gs.drag_start_screen[0];
            let dy_screen = y - gs.drag_start_screen[1];

            match active_handle {
                renderer::HANDLE_TRANSLATE_X
                | renderer::HANDLE_TRANSLATE_Y
                | renderer::HANDLE_TRANSLATE_Z => {
                    let width = ui.get_lh_coverage_width() as u32;
                    let height = ui.get_lh_coverage_height() as u32;
                    if width == 0 || height == 0 {
                        return;
                    }
                    let mvp = renderer::compute_mvp(
                        ui.get_lh_cov_cam_yaw(),
                        ui.get_lh_cov_cam_pitch(),
                        ui.get_lh_cov_cam_distance(),
                        ui.get_lh_cov_cam_pan_x(),
                        ui.get_lh_cov_cam_pan_y(),
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
                            cs.base_stations[idx].pos = new_pos;
                            cs.bs_render_data[idx].0 = new_pos;
                            update_bs_ui_model(&ui, &cs.base_stations);
                        }
                    }
                }
                renderer::HANDLE_ROTATE_AZ => {
                    let sensitivity = 0.5;
                    let new_az = gs.drag_start_azimuth - dx_screen * sensitivity;
                    cs.base_stations[idx].azimuth_deg = new_az;
                    cs.bs_render_data[idx].1 = cs.base_stations[idx].rotation_matrix();
                    update_bs_ui_model(&ui, &cs.base_stations);
                }
                renderer::HANDLE_ROTATE_EL => {
                    let sensitivity = 0.5;
                    let new_el =
                        (gs.drag_start_elevation + dy_screen * sensitivity).clamp(-90.0, 90.0);
                    cs.base_stations[idx].elevation_deg = new_el;
                    cs.bs_render_data[idx].1 = cs.base_stations[idx].rotation_matrix();
                    update_bs_ui_model(&ui, &cs.base_stations);
                }
                _ => {}
            }
        });

        // Mouse released: stop handle drag
        let uw = ui_weak.clone();
        ui.on_lh_cov_view_mouse_released(move || {
            if let Some(ui) = uw.upgrade() {
                ui.set_lh_cov_handle_active(false);
                ui.set_lh_cov_active_handle(renderer::HANDLE_NONE);
            }
        });

        // Pan: transform screen-space mouse delta to world XY using camera yaw
        let uw = ui_weak.clone();
        ui.on_lh_cov_view_pan(move |dx_px, dy_px| {
            let Some(ui) = uw.upgrade() else { return };
            let yaw = ui.get_lh_cov_cam_yaw();
            let scale = 0.01_f32;
            let (sy, cy) = yaw.sin_cos();
            // Camera right vector on ground plane: (-sin(yaw), cos(yaw))
            // Camera forward on ground plane (toward target): (-cos(yaw), -sin(yaw))
            // Mouse up (dy<0) → move target away from camera → along forward
            let dx = dx_px * scale;
            let dy = -dy_px * scale; // flip so mouse-up is positive
            ui.set_lh_cov_cam_pan_x(
                ui.get_lh_cov_cam_pan_x() + dx * sy + dy * cy,
            );
            ui.set_lh_cov_cam_pan_y(
                ui.get_lh_cov_cam_pan_y() + dx * (-cy) + dy * sy,
            );
        });
    }
}
