//! The 3D view: the scene shown and picking a unit.

use slint::{ComponentHandle, Model};

use crate::app::AppContext;
use crate::settings::AppSettings;
use crate::units::{fetch_unit_details, has_position, sort_unit_indices};
use crate::{AppWindow, planning, host::renderer, ViewSettings};
use crate::host;

// Visualizer scene overlay (loaded scene's flight area + pads + obstacles)
#[derive(Default)]
pub(crate) struct VizSceneState {
    pub(crate) flight_areas: Vec<planning::BoxVolume>,
    pub(crate) waypoint_areas: Vec<planning::BoxVolume>,
    pub(crate) takeoff_pads: Vec<planning::TakeoffPad>,
    pub(crate) obstacles: Vec<planning::Obstacle>,
    pub(crate) obstacle_triangles: Vec<Vec<f32>>,
    pub(crate) obstacle_wireframes: Vec<Vec<f32>>,
    pub(crate) obstacle_colors: Vec<[f32; 3]>,
}
impl VizSceneState {
    pub(crate) fn set_obstacles(&mut self, obstacles: &[planning::Obstacle]) {
        self.obstacles = obstacles.to_vec();
        self.obstacle_triangles = obstacles.iter().map(|o| o.triangulate()).collect();
        self.obstacle_wireframes = obstacles.iter().map(|o| o.wireframe()).collect();
        self.obstacle_colors = obstacles.iter().map(|o| o.color).collect();
    }
}

pub(crate) fn setup(ui: &AppWindow, cx: &AppContext) {
    let AppContext { settings: _, swarm_state, plot_selected_index, viz_scene_state, .. } = cx;
    // Visualizer: Load Scene (read flight area + pads from a yaml)
    {
        let viz_scene_state = viz_scene_state.clone();
        let ui_weak = ui.as_weak();
        ui.on_viz_load_scene(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let uw = ui.as_weak();
            let viz_scene_state = viz_scene_state.clone();
            slint::spawn_local(async move {
                let Some(handle) = host::dialog::FileDialog::new()
                    .add_filter("YAML", &["yaml", "yml"])
                    .pick_file().await
                else { return };
                let path = handle.path().to_path_buf();
                match planning::load_scene(&path) {
                    Ok(scene) => {
                        {
                            let mut s = viz_scene_state.lock().unwrap();
                            s.flight_areas = scene.flight_areas.clone();
                            s.waypoint_areas = scene.waypoint_areas.clone();
                            s.takeoff_pads = scene.takeoff_pads.clone();
                            let obs = scene.obstacles();
                            s.set_obstacles(&obs);
                        }
                        let mut settings: AppSettings = host::store::load();
                        settings.last_scene_config = Some(path.to_string_lossy().into_owned());
                        if let Err(e) = host::store::save(&settings) {
                            eprintln!("Failed to persist last_scene_config: {}", e);
                        }
                        if let Some(ui) = uw.upgrade() {
                            let name = path.file_name()
                                .and_then(|n| n.to_str())
                                .map(|s| s.to_string())
                                .unwrap_or_default();
                            ui.set_viz_loaded_scene_name(name.into());
                        }
                        eprintln!("Loaded scene from {:?}", path);
                    }
                    Err(e) => eprintln!("Failed to load scene: {}", e),
                }
            }).unwrap();
        });
    }

    // Visualizer: Save Pad Positions (snapshot connected drone xy → scene yaml)
    {
        let viz_scene_state = viz_scene_state.clone();
        let swarm_state = swarm_state.clone();
        let ui_weak = ui.as_weak();
        ui.on_viz_save_pads(move || {
            let Some(ui) = ui_weak.upgrade() else { return };

            // Snapshot pads from current connected units' positions
            let units_model = ui.get_units();
            let swarm_state_snap = swarm_state.clone();
            let connected_indices: std::collections::HashSet<usize> = {
                let Ok(state) = swarm_state_snap.try_lock() else {
                    eprintln!("Swarm state busy; try again");
                    return;
                };
                state.keys().copied().collect()
            };

            let mut pads: Vec<planning::TakeoffPad> = Vec::new();
            for i in 0..units_model.row_count() {
                if !connected_indices.contains(&i) { continue; }
                let Some(u) = units_model.row_data(i) else { continue };
                // Skip units with no positioning fix (origin)
                if u.pos_x == 0.0 && u.pos_y == 0.0 && u.pos_z == 0.0 { continue; }
                pads.push(planning::TakeoffPad {
                    x: u.pos_x,
                    y: u.pos_y,
                    radius: 0.10,
                    label: u.name.to_string(),
                });
            }

            if pads.is_empty() {
                eprintln!("No connected units with positions to capture as pads");
                return;
            }
            eprintln!("Capturing {} pads", pads.len());

            // Update overlay state immediately so they render
            {
                let mut s = viz_scene_state.lock().unwrap();
                s.takeoff_pads = pads.clone();
            }

            // Pick file: last-loaded scene if present, else file picker
            let settings: AppSettings = host::store::load();
            let known_path = settings.last_scene_config.clone();

            let uw = ui.as_weak();
            slint::spawn_local(async move {
                let path = if let Some(p) = known_path {
                    std::path::PathBuf::from(p)
                } else {
                    let Some(handle) = host::dialog::FileDialog::new()
                        .add_filter("YAML", &["yaml", "yml"])
                        .set_file_name("scene.yaml")
                        .save_file().await
                    else { return };
                    handle.path().to_path_buf()
                };

                if let Err(e) = planning::save_pads_to_scene(&path, pads) {
                    eprintln!("Failed to save pads: {}", e);
                    return;
                }
                let mut settings: AppSettings = host::store::load();
                settings.last_scene_config = Some(path.to_string_lossy().into_owned());
                if let Err(e) = host::store::save(&settings) {
                    eprintln!("Failed to persist last_scene_config: {}", e);
                }
                if let Some(ui) = uw.upgrade() {
                    let name = path.file_name()
                        .and_then(|n| n.to_str())
                        .map(|s| s.to_string())
                        .unwrap_or_default();
                    ui.set_viz_loaded_scene_name(name.into());
                }
                eprintln!("Saved pads to {:?}", path);
            }).unwrap();
        });
    }

    // Click-to-select a Crazyflie in the 3D visualization → open the unit sidebar
    {
        let swarm_state = swarm_state.clone();
        let ui_weak = ui.as_weak();
        let plot_selected_index = plot_selected_index.clone();

        ui.on_viz_unit_clicked(move |click_x, click_y| {
            let Some(ui) = ui_weak.upgrade() else { return };

            match unit_at(&ui, click_x, click_y) {
                Some(row) => {
                    ui.set_viz_selected_unit(row);
                    let indices = sort_unit_indices(&ui.get_units(), ui.get_sort_column(), ui.get_sort_ascending());
                    *plot_selected_index.lock().unwrap() = indices.get(row as usize).copied();
                    fetch_unit_details(ui_weak.clone(), swarm_state.clone(), row);
                }
                None => {
                    ui.set_viz_selected_unit(-1);
                    *plot_selected_index.lock().unwrap() = None;
                }
            }
        });
    }

    // The unit under the pointer, for its tooltip
    {
        let ui_weak = ui.as_weak();
        ui.on_viz_hover(move |x, y| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let row = if x < 0.0 { -1 } else { unit_at(&ui, x, y).unwrap_or(-1) };
            if ui.get_viz_hover_unit() != row {
                ui.set_viz_hover_unit(row);
            }
        });
    }

    // Fit: the units in view, from the angle the camera has
    {
        let ui_weak = ui.as_weak();
        ui.on_viz_fit(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            fit(&ui);
        });
    }
}

/// The sorted-units row of the unit drawn at (`x`, `y`) in the 3D view: the
/// nearest one whose dot, with a margin around it, is there.
fn unit_at(ui: &AppWindow, x: f32, y: f32) -> Option<i32> {
    let (width, height) = (ui.get_viz_width(), ui.get_viz_height());
    // Nothing to pick while the Crazyflies are switched off in the view.
    if width < 1.0 || height < 1.0 || !ui.global::<ViewSettings>().get_show_units() {
        return None;
    }
    let mvp = renderer::compute_mvp(
        ui.get_cam_yaw(),
        ui.get_cam_pitch(),
        ui.get_cam_distance(),
        ui.get_cam_pan_x(),
        ui.get_cam_pan_y(),
        width / height,
    );
    let sorted = ui.get_sorted_units();
    let mut best: Option<(i32, f32)> = None;
    for row in 0..sorted.row_count() {
        let Some(u) = sorted.row_data(row) else { continue };
        if !has_position(u.state) {
            continue;
        }
        let p = [u.pos_x, u.pos_y, u.pos_z];
        let Some((sx, sy)) = renderer::project_to_screen(p, &mvp, width as u32, height as u32) else { continue };
        let reach = renderer::unit_diameter(renderer::clip_w(p, &mvp), height) / 2.0 + 7.0;
        let dist = ((sx - x).powi(2) + (sy - y).powi(2)).sqrt();
        if dist <= reach && best.is_none_or(|(_, d)| dist < d) {
            best = Some((row as i32, dist));
        }
    }
    best.map(|(row, _)| row)
}

/// Point the camera at the middle of the units and back off until they all
/// fit, keeping its angle.
pub(crate) fn fit(ui: &AppWindow) {
    let units = ui.get_units();
    let (mut min, mut max) = ([f32::MAX; 2], [f32::MIN; 2]);
    for u in units.iter().filter(|u| has_position(u.state)) {
        min = [min[0].min(u.pos_x), min[1].min(u.pos_y)];
        max = [max[0].max(u.pos_x), max[1].max(u.pos_y)];
    }
    // Nothing to fit: the middle of the room
    if min[0] > max[0] {
        (min, max) = ([-2.0, -2.0], [2.0, 2.0]);
    }
    let center = [(min[0] + max[0]) / 2.0, (min[1] + max[1]) / 2.0];
    let radius = ((max[0] - min[0]).hypot(max[1] - min[1]) / 2.0 + 0.5).max(1.5);
    let (width, height) = (ui.get_viz_width().max(1.0), ui.get_viz_height().max(1.0));
    let half_v = renderer::FOV_DEG.to_radians() / 2.0;
    let half_h = (half_v.tan() * width / height).atan();
    // A little more, for the cards over the edges of the view
    let distance = radius / half_v.min(half_h).sin() * 1.15;
    let view = ui.global::<ViewSettings>();
    view.set_cam_pan_x(center[0]);
    view.set_cam_pan_y(center[1]);
    view.set_cam_distance(distance.clamp(2.5, 80.0));
}
