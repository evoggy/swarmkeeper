use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::app::AppContext;
use crate::console::{load_console, SharedConsoleStore};
use crate::journal::{load_journal, SharedJournalStore};
use crate::lh_coverage_ui::{CoverageState, GizmoState};
use crate::loco::{PositioningData, SharedPositioningData};
use crate::manual_control::{SharedGamepadIds, SharedManualControl};
use crate::planning_ui::{PlanningGizmoState, PlanningState};
use crate::settings::{AppSettings, save_view_state};
use crate::swarm::{follow_jobs, SwarmState, unit_uris, UnitSetup};
use crate::swarm_file::{apply_swarm_config, load_swarm_config};
use crate::tdoa3_ui::{Tdoa3GizmoState, Tdoa3State};
use crate::host::toc_cache::FileTocCache;
use crate::trajectory::{SharedGotoTargets, SharedTrajectoryData};
use crate::view3d::VizSceneState;

mod app;
mod connection;
mod console;
mod coverage;
mod demo;
mod flashing;
mod flight;
mod health;
mod host;
mod journal;
mod lh_coverage_ui;
mod lh_geo;
mod lh_wizard;
mod lh_wizard_ui;
mod lighthouse;
mod loco;
mod manual_control;
mod open_swarm;
mod params;
mod planning;
mod planning_ui;
mod plot;
mod rechannel;
mod rechannel_dialog;
mod settings;
mod status;
mod shared_swarms;
mod sniffer_ui;
mod swarm;
mod swarm_file;
mod system;
mod tdoa3;
mod tdoa3_ui;
mod telemetry;
mod trajectory;
mod trajectory_ui;
mod units;
mod view3d;

slint::include_modules!();


#[tokio::main]
async fn main() {
    if let Some(path) = host::screenshot::requested() {
        return host::screenshot::take(&path);
    }
    host::env::enter_data_dir();

    slint::BackendSelector::new()
        .require_opengl_es()
        .select()
        .expect("Unable to select OpenGL ES backend");

    let ui = AppWindow::new().expect("Failed to create window");

    let settings: AppSettings = confy::load("swarmkeeper", None).unwrap_or_default();

    // Restore the shared Units/Visualization state (sidebar fields, table layout,
    // camera, overlays) before anything builds a table or renders a frame, so the
    // persisted sort order and camera pose are the ones used from the first draw.
    settings.view.apply(&ui.global::<ViewSettings>());

    if let Some(ref path) = settings.last_swarm_config {
        match load_swarm_config(std::path::Path::new(path)) {
            Ok(config) => {
                apply_swarm_config(&ui, &config);
                ui.set_swarm_source(open_swarm::file_source(std::path::Path::new(path)).into());
            }
            Err(e) => eprintln!("{}", e),
        }
    }

    // Auto-load last scene so the visualizer overlay shows up on launch
    let initial_viz_scene: Option<(Vec<planning::BoxVolume>, Vec<planning::BoxVolume>, Vec<planning::TakeoffPad>, Vec<planning::Obstacle>)> = if let Some(ref path) = settings.last_scene_config {
        let path_buf = std::path::PathBuf::from(path);
        match planning::load_scene(&path_buf) {
            Ok(scene) => {
                let name = path_buf.file_name()
                    .and_then(|n| n.to_str())
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                ui.set_viz_loaded_scene_name(name.into());
                Some((scene.flight_areas.clone(), scene.waypoint_areas.clone(), scene.takeoff_pads.clone(), scene.obstacles()))
            }
            Err(e) => {
                eprintln!("Failed to auto-load last scene: {}", e);
                None
            }
        }
    } else {
        None
    };

    // Live data plot dock, shared by the Units and Visualization tabs. The
    // reusable plot component (ported from swarmfinity) renders the graph;
    // `wire_logging` (after swarm_state below) connects the streaming lifecycle.
    let plot_state = plot::setup(&ui);

    ui.on_exit_requested(move || {
        slint::quit_event_loop().ok();
    });

    let link_context = Arc::new(crazyflie_link::LinkContext::new());
    let toc_cache = FileTocCache::new();
    // How the Crazyflies are reached: this computer's Crazyradios (shared with
    // other programs or not) or a connection server.
    let connection: connection::SharedConnection = Arc::new(tokio::sync::RwLock::new(
        connection::Connection::open(&connection::mode(settings.connection.as_deref()), link_context.clone(), toc_cache.clone())
            .await,
    ));
    let swarm_state: SwarmState = Arc::new(Mutex::new(HashMap::new()));
    // Which Crazyradio the `radio:///` URIs of the swarm use.
    let radio_plan: host::radios::SharedRadioPlan = Arc::new(Mutex::new(host::radios::RadioPlan::default()));
    radio_plan.lock().await.set_swarm(unit_uris(&ui));

    // Logging for the Plot screen and the dock: the unit selected (unsorted
    // original index) is the one the dock's Start logs.
    let plot_selected_index: plot::SelectedIndex = Arc::new(std::sync::Mutex::new(None));
    plot::wire_logging(&ui, &plot_state, swarm_state.clone(), plot_selected_index.clone());
    // Firmware binary selected in the Flash Firmware dialog (path on disk).
    let flash_binary: Arc<Mutex<Option<std::path::PathBuf>>> = Arc::new(Mutex::new(None));
    let positioning_source: Arc<Mutex<Option<usize>>> = Arc::new(Mutex::new(None));
    let positioning_data: SharedPositioningData = Arc::new(Mutex::new(PositioningData::default()));
    let goto_targets: SharedGotoTargets = Arc::new(Mutex::new(HashMap::new()));
    let journal_store: SharedJournalStore = Arc::new(Mutex::new(load_journal()));
    let console_store: SharedConsoleStore = Arc::new(Mutex::new(load_console()));
    tokio::spawn(follow_jobs(
        connection.read().await.client().clone(),
        link_context.clone(),
        radio_plan.clone(),
        toc_cache.clone(),
        UnitSetup {
            swarm_state: swarm_state.clone(),
            ui_weak: ui.as_weak(),
            positioning_source: positioning_source.clone(),
            positioning_data: positioning_data.clone(),
            goto_targets: goto_targets.clone(),
            journal_store: journal_store.clone(),
            console_store: console_store.clone(),
            unit_count: 0,
            connection: connection.clone(),
        },
    ));

    let viz_scene_state = Arc::new(std::sync::Mutex::new(VizSceneState::default()));
    if let Some((flight_areas, waypoint_areas, pads, obstacles)) = initial_viz_scene {
        let mut s = viz_scene_state.lock().unwrap();
        s.flight_areas = flight_areas;
        s.waypoint_areas = waypoint_areas;
        s.takeoff_pads = pads;
        s.set_obstacles(&obstacles);
    }

    let gilrs = Arc::new(std::sync::Mutex::new(
        gilrs::Gilrs::new().expect("Failed to initialize gamepad library"),
    ));
    let gamepad_ids: SharedGamepadIds = Arc::new(std::sync::Mutex::new(Vec::new()));
    let manual_control: SharedManualControl = Arc::new(Mutex::new(None));

    let trajectory_data: SharedTrajectoryData = Arc::new(Mutex::new(HashMap::new()));

    // Open a swarm: a file, or one shared on the server; and the account
    // that shared swarms need
    open_swarm::setup(&ui, swarm_state.clone(), positioning_data.clone(), radio_plan.clone(), &settings);

    let coverage_state = std::sync::Arc::new(std::sync::Mutex::new(CoverageState {
        base_stations: Vec::new(),
        voxels: Vec::new(),
        room: [8.0, 8.0, 3.0],
        room_offset: [0.0; 3],
        bs_render_data: Vec::new(),
        undo_stack: Vec::new(),
        trajectories: Vec::new(),
    }));

    let gizmo_state = std::sync::Arc::new(std::sync::Mutex::new(GizmoState {
        drag_start_screen: [0.0, 0.0],
        drag_start_pos: [0.0; 3],
        drag_start_azimuth: 0.0,
        drag_start_elevation: 0.0,
    }));

    let tdoa3_state = std::sync::Arc::new(std::sync::Mutex::new(Tdoa3State {
        anchors: Vec::new(),
        voxels: Vec::new(),
        anchor_positions: Vec::new(),
        room: [10.0, 10.0, 3.0],
        room_offset: [0.0; 3],
        undo_stack: Vec::new(),
        gdop_result: None,
        convex_hull: None,
    }));

    let tdoa3_gizmo_state = std::sync::Arc::new(std::sync::Mutex::new(Tdoa3GizmoState {
        drag_start_screen: [0.0, 0.0],
        drag_start_pos: [0.0; 3],
    }));

    let planning_state = std::sync::Arc::new(std::sync::Mutex::new(PlanningState::default()));

    let planning_gizmo_state = std::sync::Arc::new(std::sync::Mutex::new(PlanningGizmoState {
        drag_start_screen: [0.0, 0.0],
        drag_start_pos: [0.0; 3],
        drag_start_azimuth: 0.0,
        drag_start_elevation: 0.0,
        drag_start_yaw: 0.0,
        area_handle: -1,
        area_start_min: [0.0; 3],
        area_start_max: [0.0; 3],
        area_axis_screen: [0.0, 0.0],
        area_world_per_px: 0.0,
    }));

    // LH Wizard shared state
    let wizard_state = Arc::new(std::sync::Mutex::new(lh_wizard::LhWizardState::new()));

    let cx = AppContext {
        settings,
        link_context,
        toc_cache,
        connection,
        swarm_state,
        radio_plan,
        plot_selected_index,
        flash_binary,
        positioning_source,
        positioning_data,
        goto_targets,
        journal_store,
        console_store,
        viz_scene_state,
        gilrs,
        gamepad_ids,
        manual_control,
        trajectory_data,
        coverage_state,
        gizmo_state,
        tdoa3_state,
        tdoa3_gizmo_state,
        planning_state,
        planning_gizmo_state,
        wizard_state,
    };
    settings::setup(&ui, &cx);
    params::setup(&ui, &cx);
    units::setup(&ui, &cx);
    swarm::setup(&ui, &cx).await;
    flashing::setup(&ui, &cx);
    rechannel_dialog::setup(&ui, &cx);
    lighthouse::setup(&ui, &cx);
    trajectory_ui::setup(&ui, &cx);
    flight::setup(&ui, &cx);
    system::setup(&ui, &cx);
    loco::setup(&ui, &cx);
    journal::setup(&ui, &cx);
    console::setup(&ui, &cx);
    view3d::setup(&ui, &cx);
    manual_control::setup(&ui, &cx);
    host::rendering::setup(&ui, &cx);
    host::radio_test::setup(&ui, &cx);
    health::setup(&ui, &cx);
    lh_coverage_ui::setup(&ui, &cx);
    tdoa3_ui::setup(&ui, &cx);
    planning_ui::setup(&ui, &cx);
    lh_wizard_ui::setup(&ui, &cx);
    sniffer_ui::setup(&ui, &cx);

    ui.run().expect("Failed to run UI");

    // Flush any view-state edit that the debounce timer above didn't get to
    // before the window closed.
    save_view_state(&ui);
}
