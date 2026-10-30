//! OpenGL rendering of the 3D views.

use std::sync::Arc;

use slint::{ComponentHandle, Model};

use crate::app::AppContext;
use crate::loco::SharedPositioningData;
use crate::trajectory::{SharedGotoTargets, SharedTrajectoryData};
use crate::units::{has_position, sort_unit_indices};
use crate::planning_ui::PlanningState;
use crate::view3d::VizSceneState;
use crate::{AppWindow, planning, host::renderer, Health, SceneLegend, SceneTip, Theme, UnitLabel, ViewSettings, VizLabel};

/// What the main 3D view shows besides the units.
pub(crate) struct SceneSources {
    pub(crate) positioning_data: SharedPositioningData,
    pub(crate) trajectory_data: SharedTrajectoryData,
    pub(crate) goto_targets: SharedGotoTargets,
    pub(crate) viz_scene_state: Arc<std::sync::Mutex<VizSceneState>>,
}

fn rgb(c: slint::Color) -> [f32; 3] {
    [c.red() as f32 / 255.0, c.green() as f32 / 255.0, c.blue() as f32 / 255.0]
}

/// The scene's colours, from the theme.
fn scene_style(app: &AppWindow, scale: f32) -> renderer::SceneStyle {
    let t = app.global::<Theme>();
    renderer::SceneStyle {
        scene: rgb(t.get_scene()),
        grid: rgb(t.get_grid()),
        grid_major: rgb(t.get_grid_major()),
        area: rgb(t.get_area()),
        waypoint_area: rgb(t.get_waypoint_area()),
        pad: rgb(t.get_pad()),
        drop: rgb(t.get_drop()),
        base_station: rgb(t.get_base_station()),
        surface: rgb(t.get_surface()),
        axes: [rgb(t.get_axis_x()), rgb(t.get_axis_y()), rgb(t.get_axis_z())],
        accent: rgb(t.get_accent()),
        accent_tint: rgb(t.get_accent_tint()),
        muted: rgb(t.get_text3()),
        room: rgb(t.get_room()),
        coverage: {
            let levels = t.get_coverage();
            std::array::from_fn(|k| rgb(levels.row_data(k).unwrap_or_default()))
        },
        scale,
    }
}

/// The colour of a unit in the 3D view: its health, as everywhere else.
fn health_color(app: &AppWindow, health: Health) -> [f32; 3] {
    let t = app.global::<Theme>();
    rgb(match health {
        Health::Ready | Health::Charged => t.get_good(),
        Health::Flying => t.get_flying(),
        Health::Charging | Health::Warn => t.get_warn(),
        Health::Fault => t.get_crit(),
        Health::Off => t.get_off(),
    })
}

fn labels(labels: Vec<VizLabel>) -> slint::ModelRc<VizLabel> {
    slint::ModelRc::new(slint::VecModel::from(labels))
}

/// Draw the Planning view and place its labels.
pub(crate) fn draw_planning_view(app: &AppWindow, renderer: &mut renderer::Scene3DRenderer, planning_state: &std::sync::Mutex<PlanningState>) {
    let (width, height) = (app.get_planning_width(), app.get_planning_height());
    if width < 1.0 || height < 1.0 {
        return;
    }
    let scale = app.window().scale_factor();
    let camera = renderer::Camera {
        yaw: app.get_planning_cam_yaw(),
        pitch: app.get_planning_cam_pitch(),
        distance: app.get_planning_cam_distance(),
        pan_x: app.get_planning_cam_pan_x(),
        pan_y: app.get_planning_cam_pan_y(),
    };
    let style = scene_style(app, scale);
    let ps = planning_state.lock().unwrap();

    let trajectories: Vec<Vec<[f32; 3]>> = if app.get_planning_show_trajectories() {
        let offset = [
            app.get_planning_traj_offset_x().parse().unwrap_or(0.0),
            app.get_planning_traj_offset_y().parse().unwrap_or(0.0),
            app.get_planning_traj_offset_z().parse::<f32>().unwrap_or(0.0),
        ];
        ps.trajectories
            .iter()
            .map(|t| t.iter().map(|p| [p[0] + offset[0], p[1] + offset[1], p[2] + offset[2]]).collect())
            .collect()
    } else {
        Vec::new()
    };

    // The slice: the computed points in the layer nearest the slider
    let resolution: f32 = app.get_planning_resolution().parse::<f32>().unwrap_or(5.0).max(0.5);
    let cell = 1.0 / resolution;
    let oz = ps.room_offset[2];
    let layer = ((app.get_planning_slice_z() - oz) * resolution).round() / resolution + oz;
    let in_layer = |z: f32| (z - layer).abs() < cell / 2.0;
    let mut slice = Vec::new();
    if app.get_planning_show_slice() {
        if app.get_planning_coverage_source() == 0 {
            let shown = [
                app.get_planning_show_coverage_0(),
                app.get_planning_show_coverage_1(),
                app.get_planning_show_coverage_2(),
                app.get_planning_show_coverage_3(),
                app.get_planning_show_coverage_4(),
            ];
            for &(x, y, z, n) in &ps.lh_voxels {
                let level = (n as usize).min(4);
                if in_layer(z) && shown[level] {
                    slice.push((x, y, style.coverage[level]));
                }
            }
        } else {
            let (min, max) = (app.get_planning_tdoa3_scale_min(), app.get_planning_tdoa3_scale_max());
            for &(x, y, z, v) in &ps.tdoa3_voxels {
                if in_layer(z) && v.is_finite() && (min..=max).contains(&v) {
                    // In 16 steps, so the cells draw in a few batches
                    let t = (((v - min) / (max - min).max(1e-3) * 16.0).floor() + 0.5) / 16.0;
                    let (r, g, b) = renderer::gdop_color(t);
                    slice.push((x, y, [r, g, b]));
                }
            }
        }
    }

    let to_box = |v: &planning::BoxVolume| renderer::FlightAreaBox { min: v.min, max: v.max };
    let flight_areas: Vec<_> = ps.flight_areas.iter().map(to_box).collect();
    let waypoint_areas: Vec<_> = ps.waypoint_areas.iter().map(to_box).collect();
    let pads: Vec<_> = ps.takeoff_pads.iter().map(|p| renderer::PadCircle { x: p.x, y: p.y, radius: p.radius }).collect();
    let (selected_type, selected_index) = (app.get_planning_selected_type(), app.get_planning_selected_index());
    let scene = renderer::PlanScene {
        room: ps.room,
        room_offset: ps.room_offset,
        base_stations: &ps.bs_render_data,
        anchors: &ps.anchor_positions,
        obstacle_triangles: &ps.obstacle_triangles,
        obstacle_wireframes: &ps.obstacle_wireframes,
        obstacle_colors: &ps.obstacle_colors,
        flight_areas: &flight_areas,
        waypoint_areas: &waypoint_areas,
        pads: &pads,
        trajectories: &trajectories,
        slice: &slice,
        slice_z: layer,
        cell,
        selected_type,
        selected_index,
        active_handle: app.get_planning_active_handle(),
        selected_area_kind: ps.selected_area_kind,
        selected_area_index: ps.selected_area_index,
    };
    let texture = renderer.render_plan((width * scale) as u32, (height * scale) as u32, camera, &scene, &style);
    app.set_planning_texture(texture);

    // Labels, in logical pixels
    let mvp = renderer::compute_mvp(camera.yaw, camera.pitch, camera.distance, camera.pan_x, camera.pan_y, width / height);
    let at = |p: [f32; 3]| renderer::project_to_screen(p, &mvp, width as u32, height as u32);
    let mut marks = Vec::new();
    for (i, (pos, _)) in ps.bs_render_data.iter().enumerate() {
        if let Some((sx, sy)) = at(*pos) {
            let selected = selected_type == 1 && selected_index == i as i32;
            marks.push(UnitLabel { text: format!("BS {}", i + 1).into(), x: sx + 14.0, y: sy, tone: Health::Off, selected });
        }
    }
    for (i, pos) in ps.anchor_positions.iter().enumerate() {
        if let Some((sx, sy)) = at(*pos) {
            let selected = selected_type == 2 && selected_index == i as i32;
            marks.push(UnitLabel { text: format!("A{}", i + 1).into(), x: sx + 13.0, y: sy, tone: Health::Off, selected });
        }
    }
    app.set_planning_marks(slint::ModelRc::new(slint::VecModel::from(marks)));

    // Metres along two edges of the room
    let [ox, oy, oz] = ps.room_offset;
    let [rw, rd, _] = ps.room;
    let mut grid = Vec::new();
    for x in (ox.ceil() as i32)..=((ox + rw).floor() as i32) {
        if let Some((sx, sy)) = at([x as f32, oy - 0.35, oz]) {
            grid.push(VizLabel { text: format!("{}", x).replace('-', "\u{2212}").into(), screen_x: sx, screen_y: sy });
        }
    }
    for y in (oy.ceil() as i32)..=((oy + rd).floor() as i32) {
        if let Some((sx, sy)) = at([ox - 0.35, y as f32, oz]) {
            grid.push(VizLabel { text: format!("{}", y).replace('-', "\u{2212}").into(), screen_x: sx, screen_y: sy });
        }
    }
    app.set_planning_grid_labels(labels(grid));
}

/// Draw the main 3D view and place the labels over it.
pub(crate) fn draw_main_view(app: &AppWindow, renderer: &mut renderer::Scene3DRenderer, src: &SceneSources) {
    let (width, height) = (app.get_viz_width(), app.get_viz_height());
    if width < 1.0 || height < 1.0 {
        return;
    }
    let scale = app.window().scale_factor();
    let camera = renderer::Camera {
        yaw: app.get_cam_yaw(),
        pitch: app.get_cam_pitch(),
        distance: app.get_cam_distance(),
        pan_x: app.get_cam_pan_x(),
        pan_y: app.get_cam_pan_y(),
    };

    // Per-object visibility, straight from the view settings global that
    // backs the Layers menu.
    let view = app.global::<ViewSettings>();
    let vis = renderer::VizVisibility {
        grid: view.get_show_grid(),
        axes: view.get_show_axes(),
        units: view.get_show_units(),
        drop_lines: view.get_show_drop_lines(),
        trajectories: view.get_show_trajectories(),
        goto_targets: view.get_show_goto_targets(),
        // The scene overlay switch is the master switch for the three scene
        // object classes.
        areas: view.get_show_scene_overlay() && view.get_show_areas(),
        pads: view.get_show_scene_overlay() && view.get_show_pads(),
        obstacles: view.get_show_scene_overlay() && view.get_show_obstacles(),
    };
    let show_lh_stations = view.get_show_lh_stations();
    let show_loco_anchors = view.get_show_loco_anchors();

    let units_model = app.get_units();
    // The selected and the hovered unit are sorted-units rows: map them back
    // to units-model indices.
    let indices = sort_unit_indices(&units_model, app.get_sort_column(), app.get_sort_ascending());
    let original = |row: i32| if row >= 0 { indices.get(row as usize).map_or(-1, |&i| i as i32) } else { -1 };
    let selected_row = app.get_viz_selected_unit();
    let hover_row = app.get_viz_hover_unit();
    let (selected_original, hovered_original) = (original(selected_row), original(hover_row));

    let health = crate::units::health_by_unit(app);
    let mut units = Vec::new();
    for i in 0..units_model.row_count() {
        let Some(u) = units_model.row_data(i) else { continue };
        if !has_position(u.state) {
            continue;
        }
        let health = health.get(i).copied().unwrap_or(Health::Off);
        units.push(renderer::UnitPos {
            x: u.pos_x,
            y: u.pos_y,
            z: u.pos_z,
            color: health_color(app, health),
            selected: i as i32 == selected_original,
            hovered: i as i32 == hovered_original,
            hollow: matches!(health, Health::Charging | Health::Charged),
        });
    }

    // Base stations and anchors, from the positioning data
    let mut base_stations = Vec::new();
    let mut anchors = Vec::new();
    let mut lh_ids = Vec::new();
    let mut loco_ids = Vec::new();
    if let Ok(pd) = src.positioning_data.try_lock() {
        if show_lh_stations {
            for (id, pos) in pd.lighthouse_bs.iter() {
                base_stations.push(renderer::FixedPoint { pos: *pos, active: (pd.lighthouse_active >> id) & 1 != 0 });
                lh_ids.push((*id, *pos));
            }
        }
        if show_loco_anchors {
            // Anchors seen before but not now are drawn faded.
            let present: std::collections::HashSet<u8> = pd.loco_anchors.iter().map(|(id, _)| *id).collect();
            for (id, pos) in pd.loco_anchors.iter() {
                anchors.push(renderer::FixedPoint { pos: *pos, active: true });
                loco_ids.push((*id, *pos));
            }
            for (id, pos) in pd.loco_seen.iter() {
                if !present.contains(id) {
                    anchors.push(renderer::FixedPoint { pos: *pos, active: false });
                    loco_ids.push((*id, *pos));
                }
            }
        }
    }

    // Trajectories, offset so each starts where its unit (or anchor) is
    let mut trajectory_lines = Vec::new();
    if let Ok(td) = src.trajectory_data.try_lock() {
        for (unit_idx, data) in td.iter() {
            let base = if let Some(anchor) = data.anchor {
                anchor
            } else if let Some(u) = units_model.row_data(*unit_idx) {
                [u.pos_x, u.pos_y, u.pos_z]
            } else {
                [0.0, 0.0, 0.0]
            };
            let first = data.points.first().copied().unwrap_or([0.0, 0.0, 0.0]);
            let offset = [base[0] - first[0], base[1] - first[1], base[2] - first[2]];
            for pt in &data.points {
                trajectory_lines.extend_from_slice(&[pt[0] + offset[0], pt[1] + offset[1], pt[2] + offset[2]]);
            }
        }
    }

    // Active go-to targets, and a line from each unit to its target. The
    // telemetry loop removes them once the unit arrives.
    let mut goto_points = Vec::new();
    let mut goto_lines = Vec::new();
    if let Ok(gt) = src.goto_targets.try_lock() {
        for (unit_idx, t) in gt.iter() {
            goto_points.push(*t);
            if let Some(u) = units_model.row_data(*unit_idx) {
                goto_lines.extend_from_slice(&[u.pos_x, u.pos_y, u.pos_z, t[0], t[1], t[2]]);
            }
        }
    }

    // The loaded scene: flight and waypoint areas, pads and obstacles. The
    // obstacle meshes are cloned out of the shared state, so only collect
    // what is going to be drawn.
    let (flight_areas, waypoint_areas, pads, obs_tris, obs_wires, obs_colors) = match src.viz_scene_state.try_lock() {
        Ok(s) if vis.areas || vis.pads || vis.obstacles => {
            let to_box = |v: &planning::BoxVolume| renderer::FlightAreaBox { min: v.min, max: v.max };
            let fas: Vec<_> = if vis.areas { s.flight_areas.iter().map(to_box).collect() } else { Vec::new() };
            let was: Vec<_> = if vis.areas { s.waypoint_areas.iter().map(to_box).collect() } else { Vec::new() };
            let pads: Vec<_> = if vis.pads {
                s.takeoff_pads.iter().map(|p| renderer::PadCircle { x: p.x, y: p.y, radius: p.radius }).collect()
            } else {
                Vec::new()
            };
            let (tris, wires, colors) = if vis.obstacles {
                (s.obstacle_triangles.clone(), s.obstacle_wireframes.clone(), s.obstacle_colors.clone())
            } else {
                (Vec::new(), Vec::new(), Vec::new())
            };
            (fas, was, pads, tris, wires, colors)
        }
        _ => (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new()),
    };

    // The grid covers everything in the scene, at least 3 m each way.
    let mut reach: f32 = 3.0;
    let mut cover = |x: f32, y: f32| reach = reach.max(x.abs() + 0.2).max(y.abs() + 0.2);
    units.iter().for_each(|u| cover(u.x, u.y));
    base_stations.iter().chain(&anchors).for_each(|p| cover(p.pos[0], p.pos[1]));
    flight_areas.iter().chain(&waypoint_areas).for_each(|a| {
        cover(a.min[0], a.min[1]);
        cover(a.max[0], a.max[1]);
    });
    pads.iter().for_each(|p| cover(p.x, p.y));
    let grid_half = reach.ceil().min(50.0);

    let scene = renderer::MainScene {
        units: &units,
        base_stations: &base_stations,
        anchors: &anchors,
        trajectory_lines: &trajectory_lines,
        flight_areas: &flight_areas,
        waypoint_areas: &waypoint_areas,
        pads: &pads,
        obstacle_triangles: &obs_tris,
        obstacle_wireframes: &obs_wires,
        obstacle_colors: &obs_colors,
        goto_points: &goto_points,
        goto_lines: &goto_lines,
        grid_half,
    };
    let style = scene_style(app, scale);
    let texture = renderer.render((width * scale) as u32, (height * scale) as u32, camera, &scene, vis, &style);
    app.set_viz_texture(texture);

    // Labels, in logical pixels
    let (w, h) = (width as u32, height as u32);
    let mvp = renderer::compute_mvp(camera.yaw, camera.pitch, camera.distance, camera.pan_x, camera.pan_y, width / height);
    let at = |p: [f32; 3]| renderer::project_to_screen(p, &mvp, w, h);

    // Unit names: by Layers › Crazyflie names, and always the selected unit
    // and the one under the pointer
    let label_mode = view.get_label_mode();
    let rows = app.get_unit_rows();
    let mut names = Vec::new();
    let mut tip = SceneTip::default();
    for (row, &i) in indices.iter().enumerate() {
        let Some(u) = units_model.row_data(i) else { continue };
        if !has_position(u.state) {
            continue;
        }
        let p = [u.pos_x, u.pos_y, u.pos_z];
        let Some((sx, sy)) = at(p) else { continue };
        let tone = health.get(i).copied().unwrap_or(Health::Off);
        let (selected, hovered) = (row as i32 == selected_row, row as i32 == hover_row);
        let problem = matches!(tone, Health::Warn | Health::Fault);
        let radius = renderer::unit_diameter(renderer::clip_w(p, &mvp), height) / 2.0;
        if selected || hovered || label_mode == 1 || (label_mode == 0 && problem) {
            names.push(UnitLabel { text: u.name.clone(), x: sx + radius + 6.0, y: sy - 10.0, tone, selected });
        }
        if hovered && !selected {
            let info = rows.row_data(row).unwrap_or_default();
            let dash = |s: slint::SharedString| if s.is_empty() { "–".to_string() } else { s.to_string() };
            // Beside the unit, on its left near the right edge
            let x = if sx > width - 240.0 { sx - 200.0 } else { sx + 16.0 };
            tip = SceneTip {
                shown: true,
                name: u.name.clone(),
                line1: format!("{} · {}", info.status, info.radio).into(),
                line2: format!("{} · {} · z {:.2} m", dash(info.battery), dash(info.link), u.pos_z).into(),
                x,
                y: sy + 12.0,
            };
        }
    }
    app.set_unit_labels(slint::ModelRc::new(slint::VecModel::from(names)));
    if app.get_viz_tip() != tip {
        app.set_viz_tip(tip);
    }

    // What the legend counts
    let mut legend = SceneLegend {
        base_stations: base_stations.len() as i32,
        anchors: anchors.len() as i32,
        pads: pads.len() as i32,
        areas: (flight_areas.len() + waypoint_areas.len()) as i32,
        ..Default::default()
    };
    for i in 0..units_model.row_count() {
        let Some(u) = units_model.row_data(i) else { continue };
        if !has_position(u.state) {
            legend.hidden += 1;
            continue;
        }
        match health.get(i).copied().unwrap_or(Health::Off) {
            Health::Ready => legend.ready += 1,
            Health::Flying => legend.flying += 1,
            Health::Charging | Health::Charged => legend.charging += 1,
            Health::Warn => legend.warn += 1,
            Health::Fault => legend.fault += 1,
            Health::Off => {}
        }
    }
    if app.get_viz_legend() != legend {
        app.set_viz_legend(legend);
    }

    if app.get_show_lh_labels() {
        let lh = lh_ids.iter().filter_map(|(id, pos)| {
            let (sx, sy) = at(*pos)?;
            Some(VizLabel { text: format!("BS{}", id).into(), screen_x: sx + 12.0, screen_y: sy - 9.0 })
        });
        app.set_lh_labels(labels(lh.collect()));
    }
    if app.get_show_loco_labels() {
        let loco = loco_ids.iter().filter_map(|(id, pos)| {
            let (sx, sy) = at(*pos)?;
            Some(VizLabel { text: format!("A{}", id).into(), screen_x: sx + 12.0, screen_y: sy - 9.0 })
        });
        app.set_loco_labels(labels(loco.collect()));
    }
    if app.get_show_axis_labels() {
        let axes = [([1.18, 0.0, 0.0], "X"), ([0.0, 1.18, 0.0], "Y"), ([0.0, 0.0, 1.18], "Z")]
            .into_iter()
            .filter_map(|(p, name)| {
                let (sx, sy) = at(p)?;
                Some(VizLabel { text: name.into(), screen_x: sx, screen_y: sy })
            });
        app.set_axis_labels(labels(axes.collect()));
    }
    // Metres along two edges of the grid
    if app.get_show_grid_labels() {
        let edge = -grid_half - 0.35;
        let mut grid = Vec::new();
        let n = grid_half as i32;
        for i in -n..=n {
            let text: slint::SharedString = format!("{}", i).replace('-', "\u{2212}").into();
            if let Some((sx, sy)) = at([i as f32, edge, 0.0]) {
                grid.push(VizLabel { text: text.clone(), screen_x: sx, screen_y: sy });
            }
            if i != -n {
                if let Some((sx, sy)) = at([edge, i as f32, 0.0]) {
                    grid.push(VizLabel { text, screen_x: sx, screen_y: sy });
                }
            }
        }
        app.set_grid_labels(labels(grid));
    }
}

pub(crate) fn setup(ui: &AppWindow, cx: &AppContext) {
    let AppContext { settings: _, positioning_data, goto_targets, viz_scene_state, trajectory_data, coverage_state, tdoa3_state, planning_state, wizard_state, .. } = cx;
    // Set up OpenGL 3D rendering
    {
        let app_weak = ui.as_weak();
        let sources = SceneSources {
            positioning_data: positioning_data.clone(),
            trajectory_data: trajectory_data.clone(),
            goto_targets: goto_targets.clone(),
            viz_scene_state: viz_scene_state.clone(),
        };
        let coverage_state = coverage_state.clone();
        let tdoa3_state = tdoa3_state.clone();
        let wizard_state_render = wizard_state.clone();
        let planning_state_render = planning_state.clone();
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
                            draw_main_view(&app, renderer, &sources);

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
                                draw_planning_view(&app, plan_renderer, &planning_state_render);
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
