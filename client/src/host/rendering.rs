//! OpenGL rendering of the 3D views.

use slint::{ComponentHandle, Model};

use crate::app::AppContext;
use crate::units::sort_unit_indices;
use crate::{AppWindow, planning, host::renderer, UnitState, ViewSettings, VizLabel};

pub(crate) fn setup(ui: &AppWindow, cx: &AppContext) {
    let AppContext { settings: _, positioning_data, goto_targets, viz_scene_state, trajectory_data, coverage_state, tdoa3_state, planning_state, wizard_state, .. } = cx;
    // Set up OpenGL 3D rendering
    {
        let app_weak = ui.as_weak();
        let positioning_data = positioning_data.clone();
        let trajectory_data = trajectory_data.clone();
        let goto_targets = goto_targets.clone();
        let coverage_state = coverage_state.clone();
        let tdoa3_state = tdoa3_state.clone();
        let wizard_state_render = wizard_state.clone();
        let planning_state_render = planning_state.clone();
        let viz_scene_state = viz_scene_state.clone();
        let mut scene_renderer: Option<renderer::Scene3DRenderer> = None;
        let mut coverage_renderer: Option<renderer::Scene3DRenderer> = None;
        let mut tdoa3_renderer: Option<renderer::Scene3DRenderer> = None;
        let mut wizard_renderer: Option<renderer::Scene3DRenderer> = None;
        let mut planning_renderer: Option<renderer::Scene3DRenderer> = None;

        ui.window()
            .set_rendering_notifier(move |state, graphics_api| {
                match state {
                    slint::RenderingState::RenderingSetup => {
                        let context = match graphics_api {
                            slint::GraphicsAPI::NativeOpenGL { get_proc_address } => unsafe {
                                glow::Context::from_loader_function_cstr(|s| get_proc_address(s))
                            },
                            _ => return,
                        };
                        let context2 = match graphics_api {
                            slint::GraphicsAPI::NativeOpenGL { get_proc_address } => unsafe {
                                glow::Context::from_loader_function_cstr(|s| get_proc_address(s))
                            },
                            _ => return,
                        };
                        let context3 = match graphics_api {
                            slint::GraphicsAPI::NativeOpenGL { get_proc_address } => unsafe {
                                glow::Context::from_loader_function_cstr(|s| get_proc_address(s))
                            },
                            _ => return,
                        };
                        let context4 = match graphics_api {
                            slint::GraphicsAPI::NativeOpenGL { get_proc_address } => unsafe {
                                glow::Context::from_loader_function_cstr(|s| get_proc_address(s))
                            },
                            _ => return,
                        };
                        let context6 = match graphics_api {
                            slint::GraphicsAPI::NativeOpenGL { get_proc_address } => unsafe {
                                glow::Context::from_loader_function_cstr(|s| get_proc_address(s))
                            },
                            _ => return,
                        };
                        scene_renderer = Some(renderer::Scene3DRenderer::new(context));
                        coverage_renderer = Some(renderer::Scene3DRenderer::new(context2));
                        tdoa3_renderer = Some(renderer::Scene3DRenderer::new(context3));
                        wizard_renderer = Some(renderer::Scene3DRenderer::new(context4));
                        planning_renderer = Some(renderer::Scene3DRenderer::new(context6));
                    }
                    slint::RenderingState::BeforeRendering => {
                        if let (Some(renderer), Some(app)) =
                            (scene_renderer.as_mut(), app_weak.upgrade())
                        {
                            let width = app.get_viz_width() as u32;
                            let height = app.get_viz_height() as u32;

                            if width == 0 || height == 0 {
                                return;
                            }

                            let yaw = app.get_cam_yaw();
                            let pitch = app.get_cam_pitch();
                            let distance = app.get_cam_distance();
                            let pan_x = app.get_cam_pan_x();
                            let pan_y = app.get_cam_pan_y();

                            // Per-object visibility, straight from the view settings
                            // global that backs the 3D view's context menu.
                            let view = app.global::<ViewSettings>();
                            let vis = renderer::VizVisibility {
                                grid: view.get_show_grid(),
                                axes: view.get_show_axes(),
                                units: view.get_show_units(),
                                drop_lines: view.get_show_drop_lines(),
                                trajectories: view.get_show_trajectories(),
                                goto_targets: view.get_show_goto_targets(),
                                // The scene overlay checkbox is the master switch
                                // for the three scene object classes.
                                areas: view.get_show_scene_overlay() && view.get_show_areas(),
                                pads: view.get_show_scene_overlay() && view.get_show_pads(),
                                obstacles: view.get_show_scene_overlay() && view.get_show_obstacles(),
                            };
                            let show_lh_stations = view.get_show_lh_stations();
                            let show_loco_anchors = view.get_show_loco_anchors();

                            // Read unit positions from model
                            let units_model = app.get_units();

                            // Map the selected sorted-units row back to a units-model index.
                            let selected_row = app.get_viz_selected_unit();
                            let selected_original: i32 = if selected_row >= 0 {
                                let indices = sort_unit_indices(
                                    &units_model, app.get_sort_column(), app.get_sort_ascending(),
                                );
                                indices.get(selected_row as usize).map(|&i| i as i32).unwrap_or(-1)
                            } else {
                                -1
                            };

                            let mut unit_positions = Vec::new();
                            for i in 0..units_model.row_count() {
                                if let Some(u) = units_model.row_data(i) {
                                    let color = match u.state {
                                        UnitState::Disconnected => [0.5, 0.5, 0.5],
                                        UnitState::Connected => [0.12, 0.56, 1.0],
                                        UnitState::Charging => [1.0, 0.65, 0.0],
                                        UnitState::Charged => [0.2, 0.9, 0.4],
                                        UnitState::Flying => [0.2, 0.9, 0.4],
                                        UnitState::Crashed | UnitState::Error => [0.94, 0.27, 0.27],
                                    };
                                    unit_positions.push(renderer::UnitPos {
                                        x: u.pos_x,
                                        y: u.pos_y,
                                        z: u.pos_z,
                                        color,
                                        selected: i as i32 == selected_original,
                                    });
                                }
                            }

                            // Build fixed points from positioning data
                            let mut fixed_points = Vec::new();
                            if let Ok(pd) = positioning_data.try_lock() {
                                if show_lh_stations {
                                    for (id, pos) in pd.lighthouse_bs.iter() {
                                        let active = (pd.lighthouse_active >> id) & 1 != 0;
                                        let alpha = if active { 1.0 } else { 0.5 };
                                        fixed_points.push(renderer::UnitPos {
                                            x: pos[0], y: pos[1], z: pos[2],
                                            color: [0.94 * alpha, 0.27 * alpha, 0.27 * alpha],
                                            selected: false,
                                        });
                                    }
                                }
                                if show_loco_anchors {
                                    // Track which loco anchors are currently present
                                    let mut loco_present: std::collections::HashSet<u8> = std::collections::HashSet::new();
                                    for (id, pos) in pd.loco_anchors.iter() {
                                        loco_present.insert(*id);
                                        fixed_points.push(renderer::UnitPos {
                                            x: pos[0], y: pos[1], z: pos[2],
                                            color: [1.0, 0.85, 0.0],
                                            selected: false,
                                        });
                                    }
                                    // Show previously seen anchors that are no longer present at 50%
                                    for (id, pos) in pd.loco_seen.iter() {
                                        if !loco_present.contains(id) {
                                            let alpha = 0.5;
                                            fixed_points.push(renderer::UnitPos {
                                                x: pos[0], y: pos[1], z: pos[2],
                                                color: [1.0 * alpha, 0.85 * alpha, 0.0],
                                                selected: false,
                                            });
                                        }
                                    }
                                }
                            }

                            // Build trajectory visualization lines (offset by unit position)
                            let mut trajectory_lines = Vec::new();
                            if let Ok(td) = trajectory_data.try_lock() {
                                for (unit_idx, data) in td.iter() {
                                    let base = if let Some(anchor) = data.anchor {
                                        anchor
                                    } else if let Some(u) = units_model.row_data(*unit_idx) {
                                        [u.pos_x, u.pos_y, u.pos_z]
                                    } else {
                                        [0.0, 0.0, 0.0]
                                    };
                                    // Offset so the first trajectory point aligns with the base position
                                    let first = data.points.first().copied().unwrap_or([0.0, 0.0, 0.0]);
                                    let ox = base[0] - first[0];
                                    let oy = base[1] - first[1];
                                    let oz = base[2] - first[2];
                                    for pt in &data.points {
                                        trajectory_lines.push(pt[0] + ox);
                                        trajectory_lines.push(pt[1] + oy);
                                        trajectory_lines.push(pt[2] + oz);
                                    }
                                }
                            }

                            // Active goto targets: a magenta marker at each target plus a
                            // faint line from the unit's current position to it. Entries are
                            // removed by the telemetry loop once the unit arrives.
                            let mut goto_points = Vec::new();
                            let mut goto_lines = Vec::new();
                            if let Ok(gt) = goto_targets.try_lock() {
                                for (unit_idx, t) in gt.iter() {
                                    goto_points.push(renderer::UnitPos {
                                        x: t[0], y: t[1], z: t[2],
                                        color: [1.0, 0.2, 0.9],
                                        selected: false,
                                    });
                                    if let Some(u) = units_model.row_data(*unit_idx) {
                                        goto_lines.extend_from_slice(&[
                                            u.pos_x, u.pos_y, u.pos_z, t[0], t[1], t[2],
                                        ]);
                                    }
                                }
                            }

                            // Scene overlay (flight/waypoint areas + pads + obstacles).
                            // The obstacle meshes are cloned out of the shared state,
                            // so only collect what is actually going to be drawn.
                            let (flight_areas, waypoint_areas, pads, obs_tris, obs_wires, obs_colors) =
                                match viz_scene_state.try_lock() {
                                    Ok(s) if vis.areas || vis.pads || vis.obstacles => {
                                        let to_box = |v: &planning::BoxVolume| renderer::FlightAreaBox { min: v.min, max: v.max };
                                        let fas: Vec<renderer::FlightAreaBox> = if vis.areas {
                                            s.flight_areas.iter().map(to_box).collect()
                                        } else { Vec::new() };
                                        let was: Vec<renderer::FlightAreaBox> = if vis.areas {
                                            s.waypoint_areas.iter().map(to_box).collect()
                                        } else { Vec::new() };
                                        let pads: Vec<renderer::PadCircle> = if vis.pads {
                                            s.takeoff_pads.iter()
                                                .map(|p| renderer::PadCircle { x: p.x, y: p.y, radius: p.radius })
                                                .collect()
                                        } else { Vec::new() };
                                        let (tris, wires, colors) = if vis.obstacles {
                                            (s.obstacle_triangles.clone(), s.obstacle_wireframes.clone(), s.obstacle_colors.clone())
                                        } else {
                                            (Vec::new(), Vec::new(), Vec::new())
                                        };
                                        (fas, was, pads, tris, wires, colors)
                                    }
                                    _ => (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new()),
                                };

                            let texture = renderer.render(
                                width, height, yaw, pitch, distance, pan_x, pan_y, &unit_positions, &fixed_points, &trajectory_lines,
                                &flight_areas, &waypoint_areas, &pads, &obs_tris, &obs_wires, &obs_colors,
                                &goto_points, &goto_lines, vis,
                            );
                            app.set_viz_texture(texture);

                            // Compute label overlays
                            let aspect = width as f32 / height as f32;
                            let mvp = renderer::compute_mvp(yaw, pitch, distance, pan_x, pan_y, aspect);

                            // Crazyflie name labels
                            if app.get_show_cf_labels() {
                                let labels: Vec<VizLabel> = (0..units_model.row_count())
                                    .filter_map(|i| {
                                        let u = units_model.row_data(i)?;
                                        let (sx, sy) = renderer::project_to_screen(
                                            [u.pos_x, u.pos_y, u.pos_z], &mvp, width, height,
                                        )?;
                                        Some(VizLabel {
                                            text: u.name.clone(),
                                            screen_x: sx + 8.0,
                                            screen_y: sy - 12.0,
                                        })
                                    })
                                    .collect();
                                app.set_cf_labels(slint::ModelRc::new(slint::VecModel::from(labels)));
                            }

                            // Lighthouse base station labels
                            if app.get_show_lh_labels() {
                                let mut labels = Vec::new();
                                if let Ok(pd) = positioning_data.try_lock() {
                                    for (id, pos) in pd.lighthouse_bs.iter() {
                                        if let Some((sx, sy)) = renderer::project_to_screen(
                                            *pos, &mvp, width, height,
                                        ) {
                                            labels.push(VizLabel {
                                                text: format!("BS{}", id).into(),
                                                screen_x: sx + 8.0,
                                                screen_y: sy - 12.0,
                                            });
                                        }
                                    }
                                }
                                app.set_lh_labels(slint::ModelRc::new(slint::VecModel::from(labels)));
                            }

                            // Loco anchor labels (includes seen-but-gone anchors)
                            if app.get_show_loco_labels() {
                                let mut labels = Vec::new();
                                if let Ok(pd) = positioning_data.try_lock() {
                                    for (id, pos) in pd.loco_seen.iter() {
                                        if let Some((sx, sy)) = renderer::project_to_screen(
                                            *pos, &mvp, width, height,
                                        ) {
                                            labels.push(VizLabel {
                                                text: format!("A{}", id).into(),
                                                screen_x: sx + 8.0,
                                                screen_y: sy - 12.0,
                                            });
                                        }
                                    }
                                }
                                app.set_loco_labels(slint::ModelRc::new(slint::VecModel::from(labels)));
                            }

                            // Axis labels
                            if app.get_show_axis_labels() {
                                let mut labels = Vec::new();
                                for (point, name) in &[
                                    ([2.1, 0.0, 0.0], "X"),
                                    ([0.0, 2.1, 0.0], "Y"),
                                    ([0.0, 0.0, 2.1], "Z"),
                                ] {
                                    if let Some((sx, sy)) = renderer::project_to_screen(
                                        *point, &mvp, width, height,
                                    ) {
                                        labels.push(VizLabel {
                                            text: (*name).into(),
                                            screen_x: sx,
                                            screen_y: sy - 8.0,
                                        });
                                    }
                                }
                                app.set_axis_labels(slint::ModelRc::new(slint::VecModel::from(labels)));
                            }

                            // Grid measurement labels
                            if app.get_show_grid_labels() {
                                let mut labels = Vec::new();
                                for i in -5..=5 {
                                    let v = i as f32;
                                    // Labels along X axis (Y=0)
                                    if let Some((sx, sy)) = renderer::project_to_screen(
                                        [v, 0.0, 0.0], &mvp, width, height,
                                    ) {
                                        labels.push(VizLabel {
                                            text: format!("{}", i).into(),
                                            screen_x: sx,
                                            screen_y: sy + 4.0,
                                        });
                                    }
                                    // Labels along Y axis (X=0), skip 0 to avoid overlap
                                    if i != 0 {
                                        if let Some((sx, sy)) = renderer::project_to_screen(
                                            [0.0, v, 0.0], &mvp, width, height,
                                        ) {
                                            labels.push(VizLabel {
                                                text: format!("{}", i).into(),
                                                screen_x: sx + 4.0,
                                                screen_y: sy,
                                            });
                                        }
                                    }
                                }
                                app.set_grid_labels(slint::ModelRc::new(slint::VecModel::from(labels)));
                            }

                            // --- LH Coverage rendering ---
                            if let Some(cov_renderer) = coverage_renderer.as_mut() {
                                let cov_w = app.get_lh_coverage_width() as u32;
                                let cov_h = app.get_lh_coverage_height() as u32;
                                if cov_w > 0 && cov_h > 0 {
                                    let cov_yaw = app.get_lh_cov_cam_yaw();
                                    let cov_pitch = app.get_lh_cov_cam_pitch();
                                    let cov_dist = app.get_lh_cov_cam_distance();
                                    let cov_pan_x = app.get_lh_cov_cam_pan_x();
                                    let cov_pan_y = app.get_lh_cov_cam_pan_y();

                                    let (room, room_offset, bs_render, voxels, trajectories) = {
                                        let cs = coverage_state.lock().unwrap();
                                        (cs.room, cs.room_offset, cs.bs_render_data.clone(), cs.voxels.clone(), cs.trajectories.clone())
                                    };

                                    let show_coverage = [
                                        app.get_lh_cov_show_coverage_0(),
                                        app.get_lh_cov_show_coverage_1(),
                                        app.get_lh_cov_show_coverage_2(),
                                        app.get_lh_cov_show_coverage_3(),
                                        app.get_lh_cov_show_coverage_4(),
                                    ];

                                    let offset_traj = if app.get_lh_cov_show_trajectories() && !trajectories.is_empty() {
                                        let ox: f32 = app.get_lh_cov_traj_offset_x().parse().unwrap_or(0.0);
                                        let oy: f32 = app.get_lh_cov_traj_offset_y().parse().unwrap_or(0.0);
                                        let oz: f32 = app.get_lh_cov_traj_offset_z().parse().unwrap_or(0.0);
                                        if ox == 0.0 && oy == 0.0 && oz == 0.0 {
                                            trajectories.clone()
                                        } else {
                                            trajectories.iter().map(|traj| {
                                                traj.iter().map(|p| [p[0] + ox, p[1] + oy, p[2] + oz]).collect()
                                            }).collect()
                                        }
                                    } else {
                                        Vec::new()
                                    };
                                    let traj_to_render = &offset_traj;

                                    let cov_tex = cov_renderer.render_coverage(
                                        cov_w, cov_h,
                                        cov_yaw, cov_pitch, cov_dist, cov_pan_x, cov_pan_y,
                                        room, room_offset,
                                        &bs_render,
                                        &voxels,
                                        160.0, 115.0,
                                        &show_coverage,
                                        app.get_lh_cov_selected_bs(),
                                        app.get_lh_cov_active_handle(),
                                        traj_to_render,
                                    );
                                    app.set_lh_coverage_texture(cov_tex);

                                    // Grid measurement labels along room edges
                                    let cov_mvp = renderer::compute_mvp(
                                        cov_yaw, cov_pitch, cov_dist, cov_pan_x, cov_pan_y,
                                        cov_w as f32 / cov_h as f32,
                                    );
                                    let mut labels = Vec::new();
                                    let gx = room[0].ceil() as i32;
                                    let gy = room[1].ceil() as i32;
                                    let ox = room_offset[0];
                                    let oy = room_offset[1];
                                    let oz = room_offset[2];
                                    // Labels along X axis (Y=min)
                                    for i in 0..=gx {
                                        let wx = i as f32 + ox;
                                        if let Some((sx, sy)) = renderer::project_to_screen(
                                            [wx, oy, oz], &cov_mvp, cov_w, cov_h,
                                        ) {
                                            labels.push(VizLabel {
                                                text: format!("{}", wx as i32).into(),
                                                screen_x: sx,
                                                screen_y: sy + 4.0,
                                            });
                                        }
                                    }
                                    // Labels along Y axis (X=min)
                                    for i in 1..=gy {
                                        let wy = i as f32 + oy;
                                        if let Some((sx, sy)) = renderer::project_to_screen(
                                            [ox, wy, oz], &cov_mvp, cov_w, cov_h,
                                        ) {
                                            labels.push(VizLabel {
                                                text: format!("{}", wy as i32).into(),
                                                screen_x: sx + 4.0,
                                                screen_y: sy,
                                            });
                                        }
                                    }
                                    app.set_lh_cov_grid_labels(
                                        slint::ModelRc::new(slint::VecModel::from(labels)),
                                    );
                                }
                            }

                            // --- TDoA3 Coverage rendering ---
                            if let Some(t3_renderer) = tdoa3_renderer.as_mut() {
                                let t3_w = app.get_tdoa3_width() as u32;
                                let t3_h = app.get_tdoa3_height() as u32;
                                if t3_w > 0 && t3_h > 0 {
                                    let t3_yaw = app.get_tdoa3_cam_yaw();
                                    let t3_pitch = app.get_tdoa3_cam_pitch();
                                    let t3_dist = app.get_tdoa3_cam_distance();
                                    let t3_pan_x = app.get_tdoa3_cam_pan_x();
                                    let t3_pan_y = app.get_tdoa3_cam_pan_y();

                                    let (room, room_offset, anchor_positions, voxels) = {
                                        let ts = tdoa3_state.lock().unwrap();
                                        (ts.room, ts.room_offset, ts.anchor_positions.clone(), ts.voxels.clone())
                                    };

                                    let min_scale: f32 = app.get_tdoa3_scale_min_value();
                                    let max_scale: f32 = app.get_tdoa3_scale_max_value();
                                    let metric_index = app.get_tdoa3_metric_index() as usize;
                                    // Invert for metrics where higher = better:
                                    // Pairs (5), pair sensitivity (6), axis sensitivity (12+)
                                    let invert_gradient = metric_index == 5
                                        || metric_index == 6
                                        || metric_index >= 12;

                                    let t3_tex = t3_renderer.render_tdoa3(
                                        t3_w, t3_h,
                                        t3_yaw, t3_pitch, t3_dist, t3_pan_x, t3_pan_y,
                                        room, room_offset,
                                        &anchor_positions,
                                        &voxels,
                                        min_scale,
                                        max_scale,
                                        true, // uncovered voxels already filtered by hull
                                        invert_gradient,
                                        app.get_tdoa3_selected_anchor(),
                                        app.get_tdoa3_active_handle(),
                                    );
                                    app.set_tdoa3_texture(t3_tex);

                                    // Grid measurement labels
                                    let t3_mvp = renderer::compute_mvp(
                                        t3_yaw, t3_pitch, t3_dist, t3_pan_x, t3_pan_y,
                                        t3_w as f32 / t3_h as f32,
                                    );
                                    let mut labels = Vec::new();
                                    let gx = room[0].ceil() as i32;
                                    let gy = room[1].ceil() as i32;
                                    for i in 0..=gx {
                                        let wx = i as f32 + room_offset[0];
                                        if let Some((sx, sy)) = renderer::project_to_screen(
                                            [wx, room_offset[1], room_offset[2]], &t3_mvp, t3_w, t3_h,
                                        ) {
                                            labels.push(VizLabel {
                                                text: format!("{}", i).into(),
                                                screen_x: sx,
                                                screen_y: sy + 12.0,
                                            });
                                        }
                                    }
                                    for i in 0..=gy {
                                        let wy = i as f32 + room_offset[1];
                                        if i != 0 {
                                            if let Some((sx, sy)) = renderer::project_to_screen(
                                                [room_offset[0], wy, room_offset[2]], &t3_mvp, t3_w, t3_h,
                                            ) {
                                                labels.push(VizLabel {
                                                    text: format!("{}", i).into(),
                                                    screen_x: sx + 4.0,
                                                    screen_y: sy,
                                                });
                                            }
                                        }
                                    }
                                    app.set_tdoa3_grid_labels(
                                        slint::ModelRc::new(slint::VecModel::from(labels)),
                                    );
                                }
                            }

                            // Wizard 3D rendering
                            if let Some(wiz_renderer) = wizard_renderer.as_mut() {
                                let wiz_w = app.get_lh_wizard_width() as u32;
                                let wiz_h = app.get_lh_wizard_height() as u32;
                                if wiz_w > 0 && wiz_h > 0 {
                                    let yaw = app.get_lh_wizard_cam_yaw();
                                    let pitch = app.get_lh_wizard_cam_pitch();
                                    let dist = app.get_lh_wizard_cam_distance();
                                    let pan_x = app.get_lh_wizard_cam_pan_x();
                                    let pan_y = app.get_lh_wizard_cam_pan_y();

                                    let ws = wizard_state_render.lock().unwrap();
                                    let mut bs_data: Vec<([f32; 3], [[f32; 3]; 3])> = Vec::new();

                                    if let Some(ref sol) = ws.latest_solution {
                                        for pose in sol.bs_poses.values() {
                                            let p = [pose.translation[0] as f32, pose.translation[1] as f32, pose.translation[2] as f32];
                                            let r = [
                                                [pose.rot_matrix[(0,0)] as f32, pose.rot_matrix[(1,0)] as f32, pose.rot_matrix[(2,0)] as f32],
                                                [pose.rot_matrix[(0,1)] as f32, pose.rot_matrix[(1,1)] as f32, pose.rot_matrix[(2,1)] as f32],
                                                [pose.rot_matrix[(0,2)] as f32, pose.rot_matrix[(1,2)] as f32, pose.rot_matrix[(2,2)] as f32],
                                            ];
                                            bs_data.push((p, r));
                                        }
                                    }
                                    drop(ws);

                                    let room = [10.0f32, 10.0, 3.0];
                                    let room_offset = [-5.0f32, -5.0, 0.0];
                                    let show_cov = [false; 5];

                                    let wiz_tex = wiz_renderer.render_coverage(
                                        wiz_w, wiz_h, yaw, pitch, dist, pan_x, pan_y,
                                        room, room_offset,
                                        &bs_data, &[],
                                        60.0, 60.0, &show_cov,
                                        -1, 0,
                                        &[],
                                    );
                                    app.set_lh_wizard_texture(wiz_tex);

                                    // Grid labels along room edges (matching the grid the renderer draws)
                                    let aspect = wiz_w as f32 / wiz_h.max(1) as f32;
                                    let mvp = renderer::compute_mvp(yaw, pitch, dist, pan_x, pan_y, aspect);
                                    let mut labels = Vec::new();
                                    let gx = room[0].ceil() as i32;
                                    let gy = room[1].ceil() as i32;
                                    let ox = room_offset[0];
                                    let oy = room_offset[1];
                                    let oz = room_offset[2];
                                    // Labels along X axis (at Y=min edge)
                                    for i in 0..=gx {
                                        let wx = i as f32 + ox;
                                        if let Some((sx, sy)) = renderer::project_to_screen(
                                            [wx, oy, oz], &mvp, wiz_w, wiz_h,
                                        ) {
                                            labels.push(VizLabel {
                                                text: format!("{}", wx as i32).into(),
                                                screen_x: sx,
                                                screen_y: sy + 4.0,
                                            });
                                        }
                                    }
                                    // Labels along Y axis (at X=min edge)
                                    for i in 1..=gy {
                                        let wy = i as f32 + oy;
                                        if let Some((sx, sy)) = renderer::project_to_screen(
                                            [ox, wy, oz], &mvp, wiz_w, wiz_h,
                                        ) {
                                            labels.push(VizLabel {
                                                text: format!("{}", wy as i32).into(),
                                                screen_x: sx + 4.0,
                                                screen_y: sy,
                                            });
                                        }
                                    }
                                    app.set_lh_wizard_grid_labels(
                                        slint::ModelRc::new(slint::VecModel::from(labels)),
                                    );
                                }
                            }

                            // --- Planning rendering ---
                            if let Some(plan_renderer) = planning_renderer.as_mut() {
                                let pw = app.get_planning_width() as u32;
                                let ph = app.get_planning_height() as u32;
                                if pw > 0 && ph > 0 {
                                    let yaw = app.get_planning_cam_yaw();
                                    let pitch = app.get_planning_cam_pitch();
                                    let dist = app.get_planning_cam_distance();
                                    let pan_x = app.get_planning_cam_pan_x();
                                    let pan_y = app.get_planning_cam_pan_y();

                                    let (room, room_offset, bs_render, lh_voxels, anchor_positions, tdoa3_voxels, obstacle_tris, obstacle_wires, obstacle_cols, trajectories, plan_flight_areas, plan_waypoint_areas, sel_area_kind, sel_area_index) = {
                                        let ps = planning_state_render.lock().unwrap();
                                        (ps.room, ps.room_offset, ps.bs_render_data.clone(),
                                         ps.lh_voxels.clone(), ps.anchor_positions.clone(),
                                         ps.tdoa3_voxels.clone(),
                                         ps.obstacle_triangles.clone(),
                                         ps.obstacle_wireframes.clone(),
                                         ps.obstacle_colors.clone(),
                                         ps.trajectories.clone(),
                                         ps.flight_areas.clone(),
                                         ps.waypoint_areas.clone(),
                                         ps.selected_area_kind,
                                         ps.selected_area_index)
                                    };

                                    let traj_for_render: Vec<Vec<[f32; 3]>> = if app.get_planning_show_trajectories() {
                                        let tox: f32 = app.get_planning_traj_offset_x().parse().unwrap_or(0.0);
                                        let toy: f32 = app.get_planning_traj_offset_y().parse().unwrap_or(0.0);
                                        let toz: f32 = app.get_planning_traj_offset_z().parse().unwrap_or(0.0);
                                        trajectories.iter()
                                            .map(|t| t.iter().map(|p| [p[0] + tox, p[1] + toy, p[2] + toz]).collect())
                                            .collect()
                                    } else {
                                        Vec::new()
                                    };

                                    let show_lh_coverage = [
                                        app.get_planning_show_coverage_0(),
                                        app.get_planning_show_coverage_1(),
                                        app.get_planning_show_coverage_2(),
                                        app.get_planning_show_coverage_3(),
                                        app.get_planning_show_coverage_4(),
                                    ];

                                    // Areas come from planning_state (kept in sync with the UI list editors)
                                    let to_box = |v: &planning::BoxVolume| renderer::FlightAreaBox { min: v.min, max: v.max };
                                    let flight_area_boxes: Vec<renderer::FlightAreaBox> = plan_flight_areas.iter().map(to_box).collect();
                                    let waypoint_area_boxes: Vec<renderer::FlightAreaBox> = plan_waypoint_areas.iter().map(to_box).collect();

                                    let tex = plan_renderer.render_planning(
                                        pw, ph,
                                        yaw, pitch, dist, pan_x, pan_y,
                                        room, room_offset,
                                        &bs_render, &lh_voxels,
                                        &show_lh_coverage,
                                        &anchor_positions, &tdoa3_voxels,
                                        app.get_planning_tdoa3_scale_min(),
                                        app.get_planning_tdoa3_scale_max(),
                                        app.get_planning_show_tdoa3_voxels(),
                                        &obstacle_tris,
                                        &obstacle_wires,
                                        &obstacle_cols,
                                        app.get_planning_selected_type(),
                                        app.get_planning_selected_index(),
                                        app.get_planning_active_handle(),
                                        &traj_for_render,
                                        &flight_area_boxes,
                                        &waypoint_area_boxes,
                                        sel_area_kind,
                                        sel_area_index,
                                    );
                                    app.set_planning_texture(tex);

                                    // Grid labels
                                    let mvp = renderer::compute_mvp(
                                        yaw, pitch, dist, pan_x, pan_y,
                                        pw as f32 / ph as f32,
                                    );
                                    let mut labels = Vec::new();
                                    let gx = room[0].ceil() as i32;
                                    let gy = room[1].ceil() as i32;
                                    let ox = room_offset[0];
                                    let oy = room_offset[1];
                                    let oz = room_offset[2];
                                    for i in 0..=gx {
                                        let wx = i as f32 + ox;
                                        if let Some((sx, sy)) = renderer::project_to_screen(
                                            [wx, oy, oz], &mvp, pw, ph,
                                        ) {
                                            labels.push(VizLabel {
                                                text: format!("{}", wx as i32).into(),
                                                screen_x: sx,
                                                screen_y: sy + 4.0,
                                            });
                                        }
                                    }
                                    for i in 1..=gy {
                                        let wy = i as f32 + oy;
                                        if let Some((sx, sy)) = renderer::project_to_screen(
                                            [ox, wy, oz], &mvp, pw, ph,
                                        ) {
                                            labels.push(VizLabel {
                                                text: format!("{}", wy as i32).into(),
                                                screen_x: sx + 4.0,
                                                screen_y: sy,
                                            });
                                        }
                                    }
                                    app.set_planning_grid_labels(
                                        slint::ModelRc::new(slint::VecModel::from(labels)),
                                    );
                                }
                            }

                            app.window().request_redraw();
                        }
                    }
                    slint::RenderingState::RenderingTeardown => {
                        drop(scene_renderer.take());
                        drop(coverage_renderer.take());
                        drop(tdoa3_renderer.take());
                        drop(wizard_renderer.take());
                        drop(planning_renderer.take());
                    }
                    _ => {}
                }
            })
            .expect("Unable to set rendering notifier");
    }
}
