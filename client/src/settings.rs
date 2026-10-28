//! Settings kept between runs, and the view state.

use serde::{Deserialize, Serialize};
use slint::{ComponentHandle, Model};

use crate::app::AppContext;
use crate::{AppWindow, Features, SettingsBridge, Theme, ViewSettings};
use crate::host;

#[derive(Serialize, Deserialize, Default)]
pub(crate) struct AppSettings {
    pub(crate) last_swarm_config: Option<String>,
    /// The shared swarm open last (`<org>/<swarm>`), when it wasn't a file.
    pub(crate) last_shared_swarm: Option<String>,
    pub(crate) last_scene_config: Option<String>,
    pub(crate) tuning_thrust_base: Option<u16>,
    pub(crate) tuning_vx_ki: Option<f32>,
    pub(crate) tuning_vy_ki: Option<f32>,
    pub(crate) tuning_prop_test_threshold: Option<f32>,
    pub(crate) tuning_prop_test_pwm_ratio: Option<u16>,
    /// How to reach the Crazyflies (see `connection`); None is the default, auto.
    #[serde(default)]
    pub(crate) connection: Option<String>,
    /// Theme: 0 follows the system, 1 light, 2 dark.
    #[serde(default)]
    pub(crate) appearance: i32,
    #[serde(default)]
    pub(crate) features: FeatureSettings,
    // Keep last: serialized as a [view] table, which TOML requires after the
    // plain values above.
    #[serde(default)]
    pub(crate) view: ViewState,
}

/// Conversion glue between the property types of the `ViewSettings` Slint global
/// and the plain Rust types stored in the settings file.
pub(crate) trait ViewValue {
    type Slint;
    fn from_slint(value: Self::Slint) -> Self;
    fn to_slint(&self) -> Self::Slint;
}

impl ViewValue for String {
    type Slint = slint::SharedString;
    fn from_slint(value: slint::SharedString) -> Self {
        value.to_string()
    }
    fn to_slint(&self) -> slint::SharedString {
        self.as_str().into()
    }
}

impl ViewValue for bool {
    type Slint = bool;
    fn from_slint(value: bool) -> Self {
        value
    }
    fn to_slint(&self) -> bool {
        *self
    }
}

impl ViewValue for i32 {
    type Slint = i32;
    fn from_slint(value: i32) -> Self {
        value
    }
    fn to_slint(&self) -> i32 {
        *self
    }
}

impl ViewValue for f32 {
    type Slint = f32;
    fn from_slint(value: f32) -> Self {
        value
    }
    fn to_slint(&self) -> f32 {
        *self
    }
}

/// Declares the persisted view state once: the struct, its defaults, and the
/// conversions to and from the `ViewSettings` Slint global. Persisting one more
/// value is one more line in the table below (plus the property and its
/// `changed` handler in view-settings.slint).
macro_rules! view_state {
    ($($field:ident: $ty:ty = $default:expr, $getter:ident / $setter:ident;)*) => {
        /// State of the Units and Visualization tabs that survives both a tab
        /// switch (the two tabs share one copy through the Slint global) and a
        /// restart (this is its on-disk mirror). Defaults must match the ones
        /// declared in view-settings.slint.
        #[derive(Serialize, Deserialize)]
        #[serde(default)]
        pub(crate) struct ViewState {
            $($field: $ty,)*
        }

        impl Default for ViewState {
            fn default() -> Self {
                Self { $($field: $default,)* }
            }
        }

        impl ViewState {
            /// Seed the Slint global from the settings file.
            pub(crate) fn apply(&self, view: &ViewSettings<'_>) {
                $(view.$setter(ViewValue::to_slint(&self.$field));)*
            }

            /// Read the Slint global back out, for saving.
            pub(crate) fn capture(view: &ViewSettings<'_>) -> Self {
                Self { $($field: ViewValue::from_slint(view.$getter()),)* }
            }
        }
    };
}

view_state! {
    // Unit sidebar: takeoff
    takeoff_height: String = "0.5".into(), get_takeoff_height / set_takeoff_height;
    takeoff_yaw: String = "0".into(), get_takeoff_yaw / set_takeoff_yaw;
    takeoff_time: String = "2".into(), get_takeoff_time / set_takeoff_time;
    takeoff_relative: bool = true, get_takeoff_relative / set_takeoff_relative;

    // Units table
    compact_rows: bool = false, get_compact_rows / set_compact_rows;

    // Unit sidebar: goto
    goto_x: String = "0".into(), get_goto_x / set_goto_x;
    goto_y: String = "0".into(), get_goto_y / set_goto_y;
    goto_z: String = "0".into(), get_goto_z / set_goto_z;
    goto_yaw: String = "0".into(), get_goto_yaw / set_goto_yaw;
    goto_speed: String = "1.0".into(), get_goto_speed / set_goto_speed;
    goto_relative: bool = false, get_goto_relative / set_goto_relative;

    // Unit sidebar: trajectory generators
    z_offset: String = "0.0".into(), get_z_offset / set_z_offset;
    z_iterations: String = "1".into(), get_z_iterations / set_z_iterations;
    primitive_size: String = "0.5".into(), get_primitive_size / set_primitive_size;
    primitive_size_y: String = "1.0".into(), get_primitive_size_y / set_primitive_size_y;
    wave_x: String = "0.3".into(), get_wave_x / set_wave_x;
    wave_y: String = "0.3".into(), get_wave_y / set_wave_y;
    wave_reps: String = "3".into(), get_wave_reps / set_wave_reps;
    wave_speed: String = "0.5".into(), get_wave_speed / set_wave_speed;
    wave_simultaneous: bool = false, get_wave_simultaneous / set_wave_simultaneous;

    // Unit sidebar: collapsible sections
    section_status_expanded: bool = true, get_section_status_expanded / set_section_status_expanded;
    section_info_expanded: bool = false, get_section_info_expanded / set_section_info_expanded;
    section_link_expanded: bool = false, get_section_link_expanded / set_section_link_expanded;
    section_flight_expanded: bool = true, get_section_flight_expanded / set_section_flight_expanded;
    section_trajectory_expanded: bool = false, get_section_trajectory_expanded / set_section_trajectory_expanded;
    section_tools_expanded: bool = false, get_section_tools_expanded / set_section_tools_expanded;
    section_power_expanded: bool = false, get_section_power_expanded / set_section_power_expanded;

    // Units tab: table layout and sorting
    sidebar_width: f32 = 360.0, get_sidebar_width / set_sidebar_width;
    sort_column: i32 = -1, get_sort_column / set_sort_column;
    sort_ascending: bool = true, get_sort_ascending / set_sort_ascending;
    col_indicator_w: f32 = 40.0, get_col_indicator_w / set_col_indicator_w;
    col_status_w: f32 = 90.0, get_col_status_w / set_col_status_w;
    col_name_w: f32 = 100.0, get_col_name_w / set_col_name_w;
    col_uri_w: f32 = 160.0, get_col_uri_w / set_col_uri_w;
    col_x_w: f32 = 60.0, get_col_x_w / set_col_x_w;
    col_y_w: f32 = 60.0, get_col_y_w / set_col_y_w;
    col_z_w: f32 = 60.0, get_col_z_w / set_col_z_w;
    col_battery_w: f32 = 110.0, get_col_battery_w / set_col_battery_w;
    col_link_w: f32 = 90.0, get_col_link_w / set_col_link_w;
    col_power_w: f32 = 80.0, get_col_power_w / set_col_power_w;
    col_supervisor_w: f32 = 90.0, get_col_supervisor_w / set_col_supervisor_w;
    col_lh_w: f32 = 50.0, get_col_lh_w / set_col_lh_w;
    col_loco_w: f32 = 50.0, get_col_loco_w / set_col_loco_w;
    col_ledup_w: f32 = 55.0, get_col_ledup_w / set_col_ledup_w;
    col_leddn_w: f32 = 55.0, get_col_leddn_w / set_col_leddn_w;

    // Visualization tab: camera
    cam_yaw: f32 = 0.8, get_cam_yaw / set_cam_yaw;
    cam_pitch: f32 = 0.6, get_cam_pitch / set_cam_pitch;
    cam_distance: f32 = 6.0, get_cam_distance / set_cam_distance;
    cam_pan_x: f32 = 0.0, get_cam_pan_x / set_cam_pan_x;
    cam_pan_y: f32 = 0.0, get_cam_pan_y / set_cam_pan_y;

    // Visualization tab: overlays
    show_cf_labels: bool = false, get_show_cf_labels / set_show_cf_labels;
    show_lh_labels: bool = false, get_show_lh_labels / set_show_lh_labels;
    show_loco_labels: bool = false, get_show_loco_labels / set_show_loco_labels;
    show_axis_labels: bool = false, get_show_axis_labels / set_show_axis_labels;
    show_grid_labels: bool = false, get_show_grid_labels / set_show_grid_labels;
    show_scene_overlay: bool = true, get_show_scene_overlay / set_show_scene_overlay;

    // Visualization tab: per-object visibility
    show_units: bool = true, get_show_units / set_show_units;
    show_drop_lines: bool = true, get_show_drop_lines / set_show_drop_lines;
    show_trajectories: bool = true, get_show_trajectories / set_show_trajectories;
    show_goto_targets: bool = true, get_show_goto_targets / set_show_goto_targets;
    show_lh_stations: bool = true, get_show_lh_stations / set_show_lh_stations;
    show_loco_anchors: bool = true, get_show_loco_anchors / set_show_loco_anchors;
    show_grid: bool = true, get_show_grid / set_show_grid;
    show_axes: bool = true, get_show_axes / set_show_axes;
    show_areas: bool = true, get_show_areas / set_show_areas;
    show_pads: bool = true, get_show_pads / set_show_pads;
    show_obstacles: bool = true, get_show_obstacles / set_show_obstacles;
}

/// Write the current Units/Visualization view state to the settings file,
/// leaving the rest of the file untouched.
pub(crate) fn save_view_state(ui: &AppWindow) {
    let mut settings: AppSettings = host::store::load();
    settings.view = ViewState::capture(&ui.global::<ViewSettings>());
    if let Err(e) = host::store::save(&settings) {
        eprintln!("Failed to save view settings: {}", e);
    }
}

#[cfg(test)]
mod settings_tests {
    use super::*;

    /// The view state is a TOML table, so it has to come after every plain value
    /// in AppSettings - and settings files written before it existed must still
    /// load, falling back to the defaults.
    #[test]
    fn view_state_round_trips_and_tolerates_older_files() {
        let path = std::env::temp_dir().join("swarmkeeper-view-state-test.toml");
        let mut settings = AppSettings::default();
        settings.last_swarm_config = Some("swarm.yaml".into());
        settings.view.takeoff_height = "1.25".into();
        settings.view.sort_column = 3;
        settings.view.cam_pitch = 0.42;
        confy::store_path(&path, &settings).expect("store");

        let loaded: AppSettings = confy::load_path(&path).expect("load");
        assert_eq!(loaded.view.takeoff_height, "1.25");
        assert_eq!(loaded.view.sort_column, 3);
        assert_eq!(loaded.view.cam_pitch, 0.42);

        std::fs::write(&path, "last_swarm_config = \"swarm.yaml\"\n").expect("write");
        let legacy: AppSettings = confy::load_path(&path).expect("load legacy");
        assert_eq!(legacy.view.takeoff_height, ViewState::default().takeoff_height);
        assert_eq!(legacy.view.sort_column, -1);

        std::fs::remove_file(&path).ok();
    }
}

/// Which features are shown (Settings › Features).
#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub(crate) struct FeatureSettings {
    /// 0 Automatic (when a connected unit has the deck), 1 On, 2 Off
    pub(crate) lighthouse: i32,
    pub(crate) loco: i32,
    pub(crate) radio: bool,
    pub(crate) planning: bool,
    pub(crate) trajectories: bool,
    pub(crate) manual_control: bool,
    pub(crate) flashing: bool,
    pub(crate) developer: bool,
}

impl Default for FeatureSettings {
    fn default() -> Self {
        FeatureSettings {
            lighthouse: 0,
            loco: 0,
            radio: true,
            planning: true,
            trajectories: true,
            manual_control: true,
            flashing: true,
            developer: true,
        }
    }
}

/// Show the stored settings on the Settings screen.
pub(crate) fn show(ui: &AppWindow, settings: &AppSettings) {
    let bridge = ui.global::<SettingsBridge>();
    let f = &settings.features;
    bridge.set_lighthouse_mode(f.lighthouse);
    bridge.set_loco_mode(f.loco);
    bridge.set_radio(f.radio);
    bridge.set_planning(f.planning);
    bridge.set_trajectories(f.trajectories);
    bridge.set_manual_control(f.manual_control);
    bridge.set_flashing(f.flashing);
    bridge.set_developer(f.developer);
    bridge.set_data_folder(host::env::data_dir().display().to_string().into());
    bridge.set_version(env!("CARGO_PKG_VERSION").into());
    ui.global::<Theme>().set_mode(settings.appearance);
    apply_features(ui);
}

/// Show or hide the features. Lighthouse and Loco on Automatic follow the
/// decks of the connected units, so this runs whenever the units change.
pub(crate) fn apply_features(ui: &AppWindow) {
    let bridge = ui.global::<SettingsBridge>();
    let features = ui.global::<Features>();
    let units = ui.get_units();
    let connected = |u: &crate::UnitData| !matches!(u.state, crate::UnitState::Disconnected | crate::UnitState::Error);
    let lighthouse_units = units.iter().filter(|u| connected(u) && u.deck_lighthouse).count() as i32;
    let loco_units = units.iter().filter(|u| connected(u) && u.deck_loco).count() as i32;
    bridge.set_lighthouse_units(lighthouse_units);
    bridge.set_loco_units(loco_units);
    let shown = |mode: i32, units: i32| mode == 1 || (mode == 0 && units > 0);
    features.set_lighthouse(shown(bridge.get_lighthouse_mode(), lighthouse_units));
    features.set_loco(shown(bridge.get_loco_mode(), loco_units));
    features.set_radio(bridge.get_radio());
    features.set_planning(bridge.get_planning());
    features.set_trajectories(bridge.get_trajectories());
    features.set_manual_control(bridge.get_manual_control());
    features.set_flashing(bridge.get_flashing());
    features.set_developer(bridge.get_developer());
}

pub(crate) fn setup(ui: &AppWindow, cx: &AppContext) {
    show(ui, &cx.settings);
    // Store what changed on the Settings screen, and apply it.
    {
        let ui_weak = ui.as_weak();
        ui.global::<SettingsBridge>().on_changed(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let bridge = ui.global::<SettingsBridge>();
            let mut settings: AppSettings = host::store::load();
            settings.appearance = ui.global::<Theme>().get_mode();
            settings.features = FeatureSettings {
                lighthouse: bridge.get_lighthouse_mode(),
                loco: bridge.get_loco_mode(),
                radio: bridge.get_radio(),
                planning: bridge.get_planning(),
                trajectories: bridge.get_trajectories(),
                manual_control: bridge.get_manual_control(),
                flashing: bridge.get_flashing(),
                developer: bridge.get_developer(),
            };
            if let Err(e) = host::store::save(&settings) {
                eprintln!("Saving the settings: {}", e);
            }
            apply_features(&ui);
        });
    }
    {
        // The `changed` handlers in the global fire on every keystroke and on
        // every mouse move while orbiting, so coalesce them into one write.
        let ui_weak = ui.as_weak();
        let save_timer = slint::Timer::default();
        ui.global::<ViewSettings>().on_save(move || {
            let ui_weak = ui_weak.clone();
            save_timer.start(
                slint::TimerMode::SingleShot,
                std::time::Duration::from_millis(500),
                move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        save_view_state(&ui);
                    }
                },
            );
        });
    }
}
