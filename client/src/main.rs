use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use serde::{Deserialize, Serialize};
use slint::{Model, StandardListViewItem};
use tokio::sync::Mutex;

mod coverage;
mod lh_geo;
mod lh_wizard;
mod planning;
mod renderer;
mod tdoa3;

slint::include_modules!();

#[derive(Clone)]
struct FileTocCache {
    cache_dir: std::path::PathBuf,
}

impl FileTocCache {
    fn new() -> Self {
        let cache_dir = dirs_next::cache_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("swarmkeeper")
            .join("toc_cache");
        std::fs::create_dir_all(&cache_dir).ok();
        FileTocCache { cache_dir }
    }

    fn path_for(&self, key: &[u8]) -> std::path::PathBuf {
        let hex: String = key.iter().map(|b| format!("{:02x}", b)).collect();
        self.cache_dir.join(format!("{}.json", hex))
    }
}

impl crazyflie_lib::TocCache for FileTocCache {
    fn get_toc(&self, key: &[u8]) -> Option<String> {
        std::fs::read_to_string(self.path_for(key)).ok()
    }

    fn store_toc(&self, key: &[u8], toc: &str) {
        std::fs::write(self.path_for(key), toc).ok();
    }
}

#[derive(Serialize, Deserialize, Default)]
struct AppSettings {
    last_swarm_config: Option<String>,
    tuning_thrust_base: Option<u16>,
    tuning_vx_ki: Option<f32>,
    tuning_vy_ki: Option<f32>,
    tuning_prop_test_threshold: Option<f32>,
    tuning_prop_test_pwm_ratio: Option<u16>,
}

#[derive(Deserialize)]
struct SwarmConfig {
    name: String,
    #[allow(dead_code)]
    description: Option<String>,
    units: Vec<UnitConfigYaml>,
}

#[derive(Deserialize)]
struct UnitConfigYaml {
    uri: String,
    name: String,
    #[allow(dead_code)]
    description: Option<String>,
}

fn load_swarm_config(path: &std::path::Path) -> Result<SwarmConfig, String> {
    let contents = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read swarm config {:?}: {}", path, e))?;
    serde_yaml::from_str(&contents)
        .map_err(|e| format!("Failed to parse swarm config {:?}: {}", path, e))
}

/// Ensure a radio URI has a default `timeout` query parameter.
/// If the URI already contains a `timeout` parameter, it is left unchanged.
/// Otherwise, `timeout=2000` (2 seconds) is appended.
fn uri_with_default_timeout(uri: &str) -> String {
    // Check if a timeout parameter is already present in the query string
    if let Some(query) = uri.split('?').nth(1) {
        if query.split('&').any(|p| p.starts_with("timeout=")) {
            return uri.to_string();
        }
        return format!("{}&timeout=2000", uri);
    }
    format!("{}?timeout=2000", uri)
}

fn apply_swarm_config(ui: &AppWindow, config: &SwarmConfig) {
    ui.set_swarm_name(config.name.clone().into());

    let units_data: Vec<UnitData> = config
        .units
        .iter()
        .map(|u| UnitData {
            name: u.name.clone().into(),
            uri: uri_with_default_timeout(&u.uri).into(),
            description: u.description.clone().unwrap_or_default().into(),
            state: UnitState::Disconnected,
            pos_x: 0.0,
            pos_y: 0.0,
            pos_z: 0.0,
            battery_voltage: 0.0,
            deck_lighthouse: false,
            deck_loco: false,
            deck_led_top: false,
            deck_led_bottom: false,
            serial: "".into(),
            pm_state: "".into(),
            supervisor_info: 0,
            supervisor_state: "".into(),
            journal_entry_count: 0,
            platform_type: "".into(),
            firmware_version: "".into(),
            link_quality: 0.0,
            uplink_rate: 0.0,
            downlink_rate: 0.0,
            radio_send_rate: 0.0,
            avg_retries: 0.0,
            rssi: 0.0,
            has_rssi: false,
            error_message: "".into(),
            identifying: false,
            selftest_passed: false,
        })
        .collect();

    let unit_names: Vec<slint::SharedString> = config.units.iter().map(|u| u.name.clone().into()).collect();
    ui.set_positioning_source_names(slint::ModelRc::new(slint::VecModel::from(unit_names.clone())));
    ui.set_positioning_source_index(0);

    ui.set_radio_test_unit_names(slint::ModelRc::new(slint::VecModel::from(unit_names.clone())));
    ui.set_radio_test_selected_unit(0);

    // Populate wizard CF names
    ui.set_lh_wizard_cf_names(slint::ModelRc::new(slint::VecModel::from(unit_names.clone())));
    if !unit_names.is_empty() {
        ui.set_lh_wizard_selected_cf(0);
        ui.set_lh_wizard_measure_enabled(true);
    }

    ui.set_units(slint::ModelRc::new(slint::VecModel::from(units_data)));
    ui.set_swarm_connected(false);
    ui.set_connected_count(0);
    rebuild_table_rows(ui);
}

fn state_symbol(state: UnitState) -> &'static str {
    match state {
        UnitState::Disconnected => "○",
        UnitState::Connected => "●",
        UnitState::Charging => "⚡",
        UnitState::Charged => "🔌",
        UnitState::Flying => "▲",
        UnitState::Crashed => "✕",
        UnitState::Error => "✕",
    }
}

fn state_text(state: UnitState) -> &'static str {
    match state {
        UnitState::Disconnected => "Disconnected",
        UnitState::Connected => "Connected",
        UnitState::Charging => "Charging",
        UnitState::Charged => "Charged",
        UnitState::Flying => "Flying",
        UnitState::Crashed => "Crashed",
        UnitState::Error => "Error",
    }
}

fn pm_state_text(state: i8) -> &'static str {
    match state {
        0 => "Battery",
        1 => "Charging",
        2 => "Charged",
        3 => "Low power",
        4 => "Shutdown",
        _ => "Unknown",
    }
}

fn supervisor_text(info: i32) -> String {
    let info = info as u16;
    let mut flags = Vec::new();
    if info & 0x0080 != 0 {
        flags.push("Crashed");
    }
    if info & 0x0040 != 0 {
        flags.push("Locked");
    }
    if info & 0x0020 != 0 {
        flags.push("Tumbled");
    }
    if info & 0x0010 != 0 {
        flags.push("Flying");
    }
    if info & 0x0002 != 0 {
        flags.push("Armed");
    }
    if flags.is_empty() {
        "Idle".to_string()
    } else {
        flags.join(", ")
    }
}

fn bool_check(v: bool) -> &'static str {
    if v { "✓" } else { "" }
}

fn unit_to_row(u: &UnitData) -> slint::ModelRc<StandardListViewItem> {
    let status_text = if u.state == UnitState::Error {
        u.error_message.to_string()
    } else {
        state_text(u.state).to_string()
    };
    let items: Vec<StandardListViewItem> = vec![
        state_symbol(u.state).into(),
        status_text.as_str().into(),
        u.name.as_str().into(),
        u.uri.as_str().into(),
        format!("{:.2}", u.pos_x).as_str().into(),
        format!("{:.2}", u.pos_y).as_str().into(),
        format!("{:.2}", u.pos_z).as_str().into(),
        format!("{:.2}V", u.battery_voltage).as_str().into(),
        format!("{}%", (u.link_quality * 100.0).round() as i32).as_str().into(),
        u.pm_state.as_str().into(),
        supervisor_text(u.supervisor_info).as_str().into(),
        bool_check(u.deck_lighthouse).into(),
        bool_check(u.deck_loco).into(),
        bool_check(u.deck_led_top).into(),
        bool_check(u.deck_led_bottom).into(),
    ];
    slint::ModelRc::new(slint::VecModel::from(items))
}

fn sort_unit_indices(units: &slint::ModelRc<UnitData>, col: i32, ascending: bool) -> Vec<usize> {
    let mut indices: Vec<usize> = (0..units.row_count()).collect();
    if col < 0 {
        return indices;
    }
    indices.sort_by(|&a, &b| {
        let ua = units.row_data(a).unwrap();
        let ub = units.row_data(b).unwrap();
        let ord = match col {
            0 | 1 => (ua.state as i32).cmp(&(ub.state as i32)),
            2 => ua.name.to_string().cmp(&ub.name.to_string()),
            3 => ua.uri.to_string().cmp(&ub.uri.to_string()),
            4 => ua.pos_x.partial_cmp(&ub.pos_x).unwrap_or(std::cmp::Ordering::Equal),
            5 => ua.pos_y.partial_cmp(&ub.pos_y).unwrap_or(std::cmp::Ordering::Equal),
            6 => ua.pos_z.partial_cmp(&ub.pos_z).unwrap_or(std::cmp::Ordering::Equal),
            7 => ua.battery_voltage.partial_cmp(&ub.battery_voltage).unwrap_or(std::cmp::Ordering::Equal),
            8 => ua.link_quality.partial_cmp(&ub.link_quality).unwrap_or(std::cmp::Ordering::Equal),
            9 => ua.pm_state.to_string().cmp(&ub.pm_state.to_string()),
            10 => ua.supervisor_info.cmp(&ub.supervisor_info),
            11 => ua.deck_lighthouse.cmp(&ub.deck_lighthouse),
            12 => ua.deck_loco.cmp(&ub.deck_loco),
            13 => ua.deck_led_top.cmp(&ub.deck_led_top),
            14 => ua.deck_led_bottom.cmp(&ub.deck_led_bottom),
            _ => std::cmp::Ordering::Equal,
        };
        if ascending { ord } else { ord.reverse() }
    });
    indices
}

fn rebuild_table_rows(ui: &AppWindow) {
    let units = ui.get_units();
    let col = ui.get_sort_column();
    let ascending = ui.get_sort_ascending();
    let indices = sort_unit_indices(&units, col, ascending);
    let mut rows = Vec::new();
    let mut sorted = Vec::new();
    for &i in &indices {
        if let Some(u) = units.row_data(i) {
            rows.push(unit_to_row(&u));
            sorted.push(u);
        }
    }
    ui.set_unit_table_rows(slint::ModelRc::new(slint::VecModel::from(rows)));
    ui.set_sorted_units(slint::ModelRc::new(slint::VecModel::from(sorted)));

    let mut connected = 0i32;
    let mut selftest_passed = 0i32;
    let mut battery_count = 0i32;
    let mut charging_count = 0i32;
    let mut charged_count = 0i32;
    for i in 0..units.row_count() {
        if let Some(u) = units.row_data(i) {
            if !matches!(u.state, UnitState::Disconnected | UnitState::Error) {
                connected += 1;
                if u.selftest_passed {
                    selftest_passed += 1;
                }
            }
            match u.state {
                UnitState::Connected | UnitState::Flying => battery_count += 1,
                UnitState::Charging => charging_count += 1,
                UnitState::Charged => charged_count += 1,
                _ => {}
            }
        }
    }
    ui.set_connected_count(connected);
    ui.set_selftest_passed_count(selftest_passed);
    ui.set_battery_count(battery_count);
    ui.set_charging_count(charging_count);
    ui.set_charged_count(charged_count);
}

fn update_unit(ui_weak: &slint::Weak<AppWindow>, index: usize, f: impl FnOnce(&mut UnitData) + Send + 'static) {
    let ui_weak = ui_weak.clone();
    slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak.upgrade() {
            let model = ui.get_units();
            if let Some(mut unit) = model.row_data(index) {
                f(&mut unit);
                model.set_row_data(index, unit);
            }
            rebuild_table_rows(&ui);
        }
    }).ok();
}

struct ConnectedUnit {
    cf: Arc<crazyflie_lib::Crazyflie>,
    identify_stop: Option<Arc<AtomicBool>>,
}

type SwarmState = Arc<Mutex<HashMap<usize, ConnectedUnit>>>;

#[derive(Default, Clone)]
struct PositioningData {
    lighthouse_bs: Vec<(u8, [f32; 3])>,  // (base station ID, position)
    loco_anchors: Vec<(u8, [f32; 3])>,   // (anchor ID, position)
    loco_seen: HashMap<u8, [f32; 3]>,    // all loco anchors ever seen (ID -> position)
    lighthouse_active: u16,
    loco_active: u16,
}

type SharedPositioningData = Arc<Mutex<PositioningData>>;

struct ManualControlState {
    running: Arc<AtomicBool>,
}

type SharedManualControl = Arc<Mutex<Option<ManualControlState>>>;
type SharedGamepadIds = Arc<std::sync::Mutex<Vec<gilrs::GamepadId>>>;

fn apply_deadzone(value: f32, deadzone: f32) -> f32 {
    if value.abs() < deadzone { 0.0 } else { value }
}

fn map_joystick_axes(lx: f32, ly: f32, rx: f32, ry: f32) -> (f32, f32, f32, u16) {
    let deadzone = 0.1;
    let roll = apply_deadzone(rx, deadzone) * 30.0;
    let pitch = apply_deadzone(ry, deadzone) * 30.0;
    let yawrate = apply_deadzone(lx, deadzone) * 200.0;
    let thrust_f = apply_deadzone(ly, deadzone).max(0.0) * 60000.0;
    let thrust = thrust_f as u16;
    (roll, pitch, yawrate, thrust)
}

async fn run_manual_control(
    cf: Arc<crazyflie_lib::Crazyflie>,
    gilrs: Arc<std::sync::Mutex<gilrs::Gilrs>>,
    gamepad_id: gilrs::GamepadId,
    running: Arc<AtomicBool>,
    ui_weak: slint::Weak<AppWindow>,
) {
    // Unlock thrust
    if let Err(e) = cf.commander.setpoint_rpyt(0.0, 0.0, 0.0, 0).await {
        eprintln!("Failed to unlock thrust: {:?}", e);
        return;
    }

    // Control loop at ~50Hz
    while running.load(Ordering::Relaxed) {
        let (raw_lx, raw_ly, raw_rx, raw_ry) = {
            let mut g = gilrs.lock().unwrap();
            while g.next_event().is_some() {}
            let gp = g.gamepad(gamepad_id);
            (
                gp.value(gilrs::Axis::LeftStickX),
                gp.value(gilrs::Axis::LeftStickY),
                gp.value(gilrs::Axis::RightStickX),
                gp.value(gilrs::Axis::RightStickY),
            )
        };

        // Update stick indicators in the UI
        let ui_weak_inner = ui_weak.clone();
        slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_weak_inner.upgrade() {
                ui.set_stick_lx(raw_lx);
                ui.set_stick_ly(raw_ly);
                ui.set_stick_rx(raw_rx);
                ui.set_stick_ry(raw_ry);
            }
        }).ok();

        let (roll, pitch, yawrate, thrust) = map_joystick_axes(raw_lx, raw_ly, raw_rx, raw_ry);

        if let Err(e) = cf.commander.setpoint_rpyt(roll, pitch, yawrate, thrust).await {
            eprintln!("Setpoint send failed: {:?}", e);
            break;
        }

        tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
    }

    // Cleanup: zero thrust then notify stop for HL commander handoff
    let _ = cf.commander.setpoint_rpyt(0.0, 0.0, 0.0, 0).await;
    let _ = cf.commander.notify_setpoint_stop(0).await;

    // Reset stick indicators
    let ui_weak_inner = ui_weak.clone();
    slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak_inner.upgrade() {
            ui.set_stick_lx(0.0);
            ui.set_stick_ly(0.0);
            ui.set_stick_rx(0.0);
            ui.set_stick_ry(0.0);
        }
    }).ok();
}

/// Stop any running manual control loop, returning after it exits.
async fn stop_manual_control_loop(manual_control: &SharedManualControl) {
    let mut mc = manual_control.lock().await;
    if let Some(prev) = mc.take() {
        prev.running.store(false, Ordering::Relaxed);
        drop(mc);
        // Brief delay to let the loop exit and send cleanup packets
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }
}

// Bootloader-level nRF51 commands (link-level, bypass CRTP)
const BOOTLOADER_TARGET_NRF51: u8 = 0xFE;
const BOOTLOADER_CMD_ALL_OFF: u8 = 0x01;
const BOOTLOADER_CMD_SYS_OFF: u8 = 0x02;
const BOOTLOADER_CMD_SYS_ON: u8 = 0x03;
const BOOTLOADER_CMD_RESET_INIT: u8 = 0xFF;
const BOOTLOADER_CMD_RESET: u8 = 0xF0;

async fn send_bootloader_command(
    link_context: &crazyflie_link::LinkContext,
    uri: &str,
    cmd: u8,
    data: Option<&[u8]>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let link = link_context.open_link(uri).await?;
    let mut command = vec![0xFF, BOOTLOADER_TARGET_NRF51, cmd];
    if let Some(d) = data {
        command.extend_from_slice(d);
    }
    let packet: crazyflie_link::Packet = command.into();
    link.send_packet(packet).await?;
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    Ok(())
}

// Lighthouse geometry YAML file format (compatible with crazyflie-lib-python)
#[derive(Deserialize)]
struct LighthouseConfigFile {
    #[serde(default)]
    geos: HashMap<u8, GeoEntry>,
    #[serde(default)]
    calibs: HashMap<u8, CalibEntry>,
}

#[derive(Deserialize)]
struct GeoEntry {
    origin: [f64; 3],
    rotation: [[f64; 3]; 3],
}

#[derive(Deserialize)]
struct CalibEntry {
    uid: u32,
    sweeps: [SweepEntry; 2],
}

#[derive(Deserialize)]
struct SweepEntry {
    phase: f64,
    tilt: f64,
    curve: f64,
    gibmag: f64,
    gibphase: f64,
    ogeemag: f64,
    ogeephase: f64,
}

#[derive(Deserialize)]
struct TrajectoryConfig {
    segments: Vec<TrajectorySegmentYaml>,
}

#[derive(Deserialize)]
struct TrajectorySegmentYaml {
    duration: f32,
    x: Vec<f32>,
    y: Vec<f32>,
    z: Vec<f32>,
    yaw: Vec<f32>,
}

fn eval_poly(coeffs: &[f32], t: f32) -> f32 {
    let mut result = 0.0f32;
    let mut t_pow = 1.0f32;
    for &c in coeffs {
        result += c * t_pow;
        t_pow *= t;
    }
    result
}

fn sample_trajectory(config: &TrajectoryConfig) -> Vec<[f32; 3]> {
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

#[derive(Default, Clone)]
struct TrajectoryData {
    points: Vec<[f32; 3]>,
    duration: f32,
    anchor: Option<[f32; 3]>,
    saved_points: Option<Vec<[f32; 3]>>,
}

type SharedTrajectoryData = Arc<Mutex<HashMap<usize, TrajectoryData>>>;

#[derive(Serialize, Deserialize, Clone)]
struct JournalEntry {
    timestamp: String,
    text: String,
}

type JournalStore = HashMap<String, Vec<JournalEntry>>;
type SharedJournalStore = Arc<Mutex<JournalStore>>;

fn journal_path() -> std::path::PathBuf {
    std::path::PathBuf::from("journals/journal.yaml")
}

fn load_journal() -> JournalStore {
    let path = journal_path();
    if path.exists() {
        let contents = std::fs::read_to_string(&path).unwrap_or_default();
        serde_yaml::from_str(&contents).unwrap_or_default()
    } else {
        HashMap::new()
    }
}

fn save_journal(store: &JournalStore) {
    let path = journal_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    if let Ok(yaml) = serde_yaml::to_string(store) {
        std::fs::write(&path, yaml).ok();
    }
}

#[tokio::main]
async fn main() {
    slint::BackendSelector::new()
        .require_opengl_es()
        .select()
        .expect("Unable to select OpenGL ES backend");

    let ui = AppWindow::new().expect("Failed to create window");

    let settings: AppSettings = confy::load("swarmkeeper", None).unwrap_or_default();
    if let Some(ref path) = settings.last_swarm_config {
        match load_swarm_config(std::path::Path::new(path)) {
            Ok(config) => apply_swarm_config(&ui, &config),
            Err(e) => eprintln!("{}", e),
        }
    }

    // Restore persisted tuning parameters
    if let Some(v) = settings.tuning_thrust_base {
        ui.set_tuning_thrust_base(v.to_string().into());
    }
    if let Some(v) = settings.tuning_vx_ki {
        ui.set_tuning_vx_ki(v.to_string().into());
    }
    if let Some(v) = settings.tuning_vy_ki {
        ui.set_tuning_vy_ki(v.to_string().into());
    }
    if let Some(v) = settings.tuning_prop_test_threshold {
        ui.set_tuning_prop_test_threshold(v.to_string().into());
    }
    if let Some(v) = settings.tuning_prop_test_pwm_ratio {
        ui.set_tuning_prop_test_pwm_ratio(v.to_string().into());
    }

    // Sort units table
    {
        let ui_weak = ui.as_weak();
        ui.on_sort_units(move |col, ascending| {
            let Some(ui) = ui_weak.upgrade() else { return };
            ui.set_sort_column(col);
            ui.set_sort_ascending(ascending);
            rebuild_table_rows(&ui);
        });
    }

    ui.on_exit_requested(move || {
        slint::quit_event_loop().ok();
    });

    let link_context = Arc::new(crazyflie_link::LinkContext::new());
    let toc_cache = FileTocCache::new();
    let swarm_state: SwarmState = Arc::new(Mutex::new(HashMap::new()));
    let positioning_source: Arc<Mutex<Option<usize>>> = Arc::new(Mutex::new(None));
    let positioning_data: SharedPositioningData = Arc::new(Mutex::new(PositioningData::default()));
    let journal_store: SharedJournalStore = Arc::new(Mutex::new(load_journal()));

    let gilrs = Arc::new(std::sync::Mutex::new(
        gilrs::Gilrs::new().expect("Failed to initialize gamepad library"),
    ));
    let gamepad_ids: SharedGamepadIds = Arc::new(std::sync::Mutex::new(Vec::new()));
    let manual_control: SharedManualControl = Arc::new(Mutex::new(None));

    // Connect swarm
    {
        let link_context = link_context.clone();
        let toc_cache = toc_cache.clone();
        let swarm_state = swarm_state.clone();
        let ui_weak = ui.as_weak();

        let positioning_source = positioning_source.clone();
        let positioning_data = positioning_data.clone();
        let journal_store = journal_store.clone();
        ui.on_connect_swarm(move || {
            let Some(ui_ref) = ui_weak.upgrade() else { return };
            let units = ui_ref.get_units();
            let unit_count = units.row_count();
            for i in 0..unit_count {
                let Some(unit) = units.row_data(i) else {
                    continue;
                };
                let uri = unit.uri.to_string();
                let link_context = link_context.clone();
                let toc_cache = toc_cache.clone();
                let ui_weak = ui_weak.clone();
                let swarm_state = swarm_state.clone();
                let positioning_source = positioning_source.clone();
                let positioning_data = positioning_data.clone();
                let journal_store = journal_store.clone();

                tokio::spawn(async move {
                    eprintln!("Connecting to {} ...", uri);

                    let cf = match tokio::time::timeout(
                        std::time::Duration::from_secs(30),
                        crazyflie_lib::Crazyflie::connect_from_uri(link_context.as_ref(), &uri, toc_cache),
                    ).await {
                        Ok(Ok(cf)) => Arc::new(cf),
                        Ok(Err(e)) => {
                            dbg!(&e);
                            eprintln!("Failed to connect to 22-- >{}: {}", uri, e);
                            let error_msg = format!("{}", e);
                            update_unit(&ui_weak, i, move |u| {
                                u.state = UnitState::Error;
                                u.error_message = error_msg.into();
                            });
                            return;
                        }
                        Err(_) => {
                            eprintln!("Connection to {} timed out", uri);
                            update_unit(&ui_weak, i, move |u| {
                                u.state = UnitState::Error;
                                u.error_message = "Connection timed out".into();
                            });
                            return;
                        }
                    };

                    eprintln!("Connected to {}", uri);

                    // Store connected Crazyflie
                    {
                        let mut state = swarm_state.lock().await;
                        state.insert(i, ConnectedUnit { cf: cf.clone(), identify_stop: None });
                    }

                    // Read installed decks
                    let deck_lighthouse: u8 = cf.param.get("deck.bcLighthouse4").await.unwrap_or(0);
                    let deck_loco: u8 = cf.param.get("deck.bcLoco").await.unwrap_or(0);
                    let deck_led_top: u8 = cf.param.get("deck.bcColorLedTop").await.unwrap_or(0);
                    let deck_led_bottom: u8 = cf.param.get("deck.bcColorLedBot").await.unwrap_or(0);

                    // Read selftest result
                    let selftest_passed: i8 = cf.param.get("system.selftestPassed").await.unwrap_or(1);

                    // Read CPU serial number
                    let id0: u32 = cf.param.get("cpu.id0").await.unwrap_or(0);
                    let id1: u32 = cf.param.get("cpu.id1").await.unwrap_or(0);
                    let id2: u32 = cf.param.get("cpu.id2").await.unwrap_or(0);
                    let serial = format!("{:08X}{:08X}{:08X}", id0, id1, id2);

                    let journal_count = {
                        let store = journal_store.lock().await;
                        store.get(&serial).map_or(0, |entries| entries.len()) as i32
                    };

                    update_unit(&ui_weak, i, move |u| {
                        u.state = UnitState::Connected;
                        u.deck_lighthouse = deck_lighthouse != 0;
                        u.deck_loco = deck_loco != 0;
                        u.deck_led_top = deck_led_top != 0;
                        u.deck_led_bottom = deck_led_bottom != 0;
                        u.serial = serial.into();
                        u.selftest_passed = selftest_passed != 0;
                        u.journal_entry_count = journal_count;
                    });

                    // Update swarm-connected flag
                    let ui_weak_inner = ui_weak.clone();
                    slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_weak_inner.upgrade() {
                            ui.set_swarm_connected(true);
                        }
                    }).ok();

                    // Auto-select as positioning source if none selected (initial connect)
                    {
                        let mut ps: tokio::sync::MutexGuard<'_, Option<usize>> = positioning_source.lock().await;
                        if ps.is_none() {
                            *ps = Some(i);
                            let ui_weak_inner = ui_weak.clone();
                            slint::invoke_from_event_loop(move || {
                                if let Some(ui) = ui_weak_inner.upgrade() {
                                    ui.set_positioning_source_index(i as i32);
                                }
                            }).ok();
                            eprintln!("Auto-selected unit {} as positioning source", i);
                        }
                    }

                    start_telemetry(i, uri.clone(), cf.clone(), ui_weak, positioning_data, positioning_source).await;
                });
            }
        });
    }

    // Disconnect swarm
    {
        let swarm_state = swarm_state.clone();
        let positioning_source = positioning_source.clone();
        let ui_weak = ui.as_weak();

        ui.on_disconnect_swarm(move || {
            let swarm_state = swarm_state.clone();
            let positioning_source = positioning_source.clone();
            let ui_weak = ui_weak.clone();

            tokio::spawn(async move {
                // Reset positioning source
                {
                    let mut ps = positioning_source.lock().await;
                    *ps = None;
                }
                let units: Vec<(usize, ConnectedUnit)> = {
                    let mut state = swarm_state.lock().await;
                    state.drain().collect()
                };

                for (index, connected) in units {
                    eprintln!("Disconnecting unit {} ...", index);
                    connected.cf.disconnect().await;
                    update_unit(&ui_weak, index, |u| {
                        u.state = UnitState::Disconnected;
                        u.pos_x = 0.0;
                        u.pos_y = 0.0;
                        u.pos_z = 0.0;
                        u.battery_voltage = 0.0;
                        u.link_quality = 0.0;
                        u.deck_lighthouse = false;
                        u.deck_loco = false;
                        u.deck_led_top = false;
                        u.deck_led_bottom = false;
                        u.serial = "".into();
                        u.pm_state = "".into();
                        u.journal_entry_count = 0;
                        u.platform_type = "".into();
                        u.firmware_version = "".into();
                    });
                }

                let ui_weak_inner = ui_weak.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak_inner.upgrade() {
                        ui.set_swarm_connected(false);
                    }
                }).ok();
            });
        });
    }

    // Reconnect disconnected units
    {
        let link_context = link_context.clone();
        let toc_cache = toc_cache.clone();
        let swarm_state = swarm_state.clone();
        let ui_weak = ui.as_weak();
        let positioning_source = positioning_source.clone();
        let positioning_data = positioning_data.clone();
        let journal_store = journal_store.clone();

        ui.on_reconnect_swarm(move || {
            let Some(ui_ref) = ui_weak.upgrade() else { return };
            let units = ui_ref.get_units();
            let unit_count = units.row_count();
            for i in 0..unit_count {
                let Some(unit) = units.row_data(i) else {
                    continue;
                };
                if !matches!(unit.state, UnitState::Disconnected | UnitState::Error) {
                    continue;
                }
                let uri = unit.uri.to_string();
                let link_context = link_context.clone();
                let toc_cache = toc_cache.clone();
                let ui_weak = ui_weak.clone();
                let swarm_state = swarm_state.clone();
                let positioning_source = positioning_source.clone();
                let positioning_data = positioning_data.clone();
                let journal_store = journal_store.clone();

                tokio::spawn(async move {
                    eprintln!("Reconnecting to {} ...", uri);

                    let cf = match tokio::time::timeout(
                        std::time::Duration::from_secs(30),
                        crazyflie_lib::Crazyflie::connect_from_uri(link_context.as_ref(), &uri, toc_cache),
                    ).await {
                        Ok(Ok(cf)) => Arc::new(cf),
                        Ok(Err(e)) => {
                            eprintln!("Failed to reconnect to {}: {:?}", uri, e);
                            let error_msg = format!("{}", e);
                            update_unit(&ui_weak, i, move |u| {
                                u.state = UnitState::Error;
                                u.error_message = error_msg.into();
                            });
                            return;
                        }
                        Err(_) => {
                            eprintln!("Reconnect to {} timed out", uri);
                            update_unit(&ui_weak, i, move |u| {
                                u.state = UnitState::Error;
                                u.error_message = "Connection timed out".into();
                            });
                            return;
                        }
                    };

                    eprintln!("Reconnected to {}", uri);

                    {
                        let mut state = swarm_state.lock().await;
                        state.insert(i, ConnectedUnit { cf: cf.clone(), identify_stop: None });
                    }

                    let deck_lighthouse: u8 = cf.param.get("deck.bcLighthouse4").await.unwrap_or(0);
                    let deck_loco: u8 = cf.param.get("deck.bcLoco").await.unwrap_or(0);
                    let deck_led_top: u8 = cf.param.get("deck.bcColorLedTop").await.unwrap_or(0);
                    let deck_led_bottom: u8 = cf.param.get("deck.bcColorLedBot").await.unwrap_or(0);

                    let selftest_passed: i8 = cf.param.get("system.selftestPassed").await.unwrap_or(1);

                    let id0: u32 = cf.param.get("cpu.id0").await.unwrap_or(0);
                    let id1: u32 = cf.param.get("cpu.id1").await.unwrap_or(0);
                    let id2: u32 = cf.param.get("cpu.id2").await.unwrap_or(0);
                    let serial = format!("{:08X}{:08X}{:08X}", id0, id1, id2);

                    let journal_count = {
                        let store = journal_store.lock().await;
                        store.get(&serial).map_or(0, |entries| entries.len()) as i32
                    };

                    update_unit(&ui_weak, i, move |u| {
                        u.state = UnitState::Connected;
                        u.deck_lighthouse = deck_lighthouse != 0;
                        u.deck_loco = deck_loco != 0;
                        u.deck_led_top = deck_led_top != 0;
                        u.deck_led_bottom = deck_led_bottom != 0;
                        u.serial = serial.into();
                        u.selftest_passed = selftest_passed != 0;
                        u.journal_entry_count = journal_count;
                    });

                    // Auto-select as positioning source if none selected
                    {
                        let mut ps: tokio::sync::MutexGuard<'_, Option<usize>> = positioning_source.lock().await;
                        if ps.is_none() {
                            *ps = Some(i);
                            let ui_weak_inner = ui_weak.clone();
                            slint::invoke_from_event_loop(move || {
                                if let Some(ui) = ui_weak_inner.upgrade() {
                                    ui.set_positioning_source_index(i as i32);
                                }
                            }).ok();
                            eprintln!("Auto-selected unit {} as positioning source", i);
                        }
                    }

                    start_telemetry(i, uri.clone(), cf.clone(), ui_weak, positioning_data, positioning_source).await;
                });
            }
        });
    }

    // Download TOC (sequential connect/disconnect to populate cache)
    {
        let link_context = link_context.clone();
        let toc_cache = toc_cache.clone();
        let ui_weak = ui.as_weak();

        ui.on_download_toc(move || {
            let Some(ui_ref) = ui_weak.upgrade() else { return };
            let units = ui_ref.get_units();
            let unit_count = units.row_count();

            // Collect URIs and names up front
            let mut unit_info: Vec<(String, String)> = Vec::new();
            for i in 0..unit_count {
                if let Some(unit) = units.row_data(i) {
                    unit_info.push((unit.uri.to_string(), unit.name.to_string()));
                }
            }

            ui_ref.set_progress_dialog_visible(true);
            ui_ref.set_progress_dialog_progress(0.0);
            ui_ref.set_progress_dialog_status("Starting...".into());
            ui_ref.set_progress_dialog_title("Downloading TOC".into());

            let link_context = link_context.clone();
            let toc_cache = toc_cache.clone();
            let ui_weak = ui_weak.clone();

            tokio::spawn(async move {
                let total = unit_info.len();
                for (idx, (uri, name)) in unit_info.iter().enumerate() {
                    let status: slint::SharedString = format!("Connecting to {} ({}/{})...", name, idx + 1, total).into();
                    let ui_weak_inner = ui_weak.clone();
                    slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_weak_inner.upgrade() {
                            ui.set_progress_dialog_status(status);
                        }
                    }).ok();

                    match tokio::time::timeout(
                        std::time::Duration::from_secs(30),
                        crazyflie_lib::Crazyflie::connect_from_uri(link_context.as_ref(), uri, toc_cache.clone()),
                    ).await {
                        Ok(Ok(cf)) => {
                            eprintln!("TOC downloaded for {}", uri);
                            cf.disconnect().await;
                        }
                        Ok(Err(e)) => {
                            eprintln!("TOC download failed for {}: {:?}", uri, e);
                            let error_msg = format!("{}", e);
                            update_unit(&ui_weak, idx, move |u| {
                                u.state = UnitState::Error;
                                u.error_message = error_msg.into();
                            });
                        }
                        Err(_) => {
                            eprintln!("TOC download for {} timed out", uri);
                            update_unit(&ui_weak, idx, move |u| {
                                u.state = UnitState::Error;
                                u.error_message = "Connection timed out".into();
                            });
                        }
                    }

                    let progress = (idx + 1) as f32 / total as f32;
                    let ui_weak_inner = ui_weak.clone();
                    slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_weak_inner.upgrade() {
                            ui.set_progress_dialog_progress(progress);
                        }
                    }).ok();
                }

                // Brief pause so the user sees 100% before closing
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;

                let ui_weak_inner = ui_weak.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak_inner.upgrade() {
                        ui.set_progress_dialog_visible(false);
                    }
                }).ok();
            });
        });
    }

    // Upload lighthouse geometry + calibration
    {
        let link_context = link_context.clone();
        let toc_cache = toc_cache.clone();
        let ui_weak = ui.as_weak();

        ui.on_upload_geometry(move || {
            let link_context = link_context.clone();
            let toc_cache = toc_cache.clone();
            let ui_weak = ui_weak.clone();

            tokio::spawn(async move {
                // Open file dialog
                let Some(handle) = rfd::AsyncFileDialog::new()
                    .add_filter("YAML", &["yaml", "yml"])
                    .pick_file()
                    .await
                else { return };
                let path = handle.path().to_path_buf();

                // Parse YAML
                let contents = match std::fs::read_to_string(&path) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("Failed to read geometry file: {:?}", e);
                        return;
                    }
                };
                let config: LighthouseConfigFile = match serde_yaml::from_str(&contents) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("Failed to parse geometry file: {:?}", e);
                        return;
                    }
                };

                // Convert to crazyflie-lib types
                use crazyflie_lib::subsystems::memory::{LighthouseBsGeometry, LighthouseBsCalibration, LighthouseCalibrationSweep};

                let geometries: HashMap<u8, LighthouseBsGeometry> = config.geos.into_iter().map(|(id, g)| {
                    (id, LighthouseBsGeometry {
                        origin: [g.origin[0] as f32, g.origin[1] as f32, g.origin[2] as f32],
                        rotation_matrix: [
                            [g.rotation[0][0] as f32, g.rotation[0][1] as f32, g.rotation[0][2] as f32],
                            [g.rotation[1][0] as f32, g.rotation[1][1] as f32, g.rotation[1][2] as f32],
                            [g.rotation[2][0] as f32, g.rotation[2][1] as f32, g.rotation[2][2] as f32],
                        ],
                        valid: true,
                    })
                }).collect();

                let calibrations: HashMap<u8, LighthouseBsCalibration> = config.calibs.into_iter().map(|(id, c)| {
                    let sweeps = [
                        LighthouseCalibrationSweep {
                            phase: c.sweeps[0].phase as f32,
                            tilt: c.sweeps[0].tilt as f32,
                            curve: c.sweeps[0].curve as f32,
                            gibmag: c.sweeps[0].gibmag as f32,
                            gibphase: c.sweeps[0].gibphase as f32,
                            ogeemag: c.sweeps[0].ogeemag as f32,
                            ogeephase: c.sweeps[0].ogeephase as f32,
                        },
                        LighthouseCalibrationSweep {
                            phase: c.sweeps[1].phase as f32,
                            tilt: c.sweeps[1].tilt as f32,
                            curve: c.sweeps[1].curve as f32,
                            gibmag: c.sweeps[1].gibmag as f32,
                            gibphase: c.sweeps[1].gibphase as f32,
                            ogeemag: c.sweeps[1].ogeemag as f32,
                            ogeephase: c.sweeps[1].ogeephase as f32,
                        },
                    ];
                    (id, LighthouseBsCalibration { sweeps, uid: c.uid, valid: true })
                }).collect();

                eprintln!("Parsed {} geometries and {} calibrations", geometries.len(), calibrations.len());

                // Collect unit URIs
                let unit_info: Vec<(String, String)> = {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    let ui_weak_inner = ui_weak.clone();
                    slint::invoke_from_event_loop(move || {
                        let mut info = Vec::new();
                        if let Some(ui) = ui_weak_inner.upgrade() {
                            let units = ui.get_units();
                            for i in 0..units.row_count() {
                                if let Some(unit) = units.row_data(i) {
                                    info.push((unit.uri.to_string(), unit.name.to_string()));
                                }
                            }
                        }
                        tx.send(info).ok();
                    }).ok();
                    match rx.await {
                        Ok(i) => i,
                        Err(_) => return,
                    }
                };

                let total = unit_info.len();
                if total == 0 { return; }

                // Show progress dialog
                {
                    let ui_weak_inner = ui_weak.clone();
                    slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_weak_inner.upgrade() {
                            ui.set_progress_dialog_title("Uploading Geometry".into());
                            ui.set_progress_dialog_progress(0.0);
                            ui.set_progress_dialog_status("Starting...".into());
                            ui.set_progress_dialog_visible(true);
                        }
                    }).ok();
                }

                // Upload to all units in parallel
                let completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let geometries = Arc::new(geometries);
                let calibrations = Arc::new(calibrations);

                let mut join_set = tokio::task::JoinSet::new();
                for (uri, name) in unit_info {
                    let link_context = link_context.clone();
                    let toc_cache = toc_cache.clone();
                    let ui_weak = ui_weak.clone();
                    let completed = completed.clone();
                    let geometries = geometries.clone();
                    let calibrations = calibrations.clone();

                    join_set.spawn(async move {
                        eprintln!("Uploading geometry to {} ...", uri);

                        let connect_result = tokio::time::timeout(
                            std::time::Duration::from_secs(30),
                            crazyflie_lib::Crazyflie::connect_from_uri(link_context.as_ref(), &uri, toc_cache),
                        ).await;
                        let cf = match connect_result {
                            Ok(Ok(cf)) => cf,
                            Ok(Err(e)) => {
                                eprintln!("Failed to connect to {} for geometry upload: {:?}", uri, e);
                                let done = completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                                let progress = done as f32 / total as f32;
                                let status: slint::SharedString = format!("Failed: {} - {} ({}/{})", name, e, done, total).into();
                                let ui_weak_inner = ui_weak.clone();
                                slint::invoke_from_event_loop(move || {
                                    if let Some(ui) = ui_weak_inner.upgrade() {
                                        ui.set_progress_dialog_progress(progress);
                                        ui.set_progress_dialog_status(status);
                                    }
                                }).ok();
                                return;
                            }
                            Err(_) => {
                                eprintln!("Connection to {} timed out for geometry upload", uri);
                                let done = completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                                let progress = done as f32 / total as f32;
                                let status: slint::SharedString = format!("Failed: {} - Connection timed out ({}/{})", name, done, total).into();
                                let ui_weak_inner = ui_weak.clone();
                                slint::invoke_from_event_loop(move || {
                                    if let Some(ui) = ui_weak_inner.upgrade() {
                                        ui.set_progress_dialog_progress(progress);
                                        ui.set_progress_dialog_status(status);
                                    }
                                }).ok();
                                return;
                            }
                        };

                        use crazyflie_lib::subsystems::memory::{MemoryType, LighthouseMemory};

                        let lh_mems = cf.memory.get_memories(Some(MemoryType::Lighthouse));
                        if let Some(mem) = lh_mems.first() {
                            if let Some(Ok(lh)) = cf.memory.open_memory::<LighthouseMemory>((*mem).clone()).await {
                                if let Err(e) = lh.write_geometries(&geometries).await {
                                    eprintln!("Failed to write geometries to {}: {:?}", uri, e);
                                }
                                if let Err(e) = lh.write_calibrations(&calibrations).await {
                                    eprintln!("Failed to write calibrations to {}: {:?}", uri, e);
                                }
                                cf.memory.close_memory(lh).await.ok();
                            }
                        }

                        // Persist to flash so data survives reboot
                        let geo_ids: Vec<u8> = geometries.keys().copied().collect();
                        let calib_ids: Vec<u8> = calibrations.keys().copied().collect();
                        match cf.localization.lighthouse.persist_lighthouse_data(&geo_ids, &calib_ids).await {
                            Ok(true) => eprintln!("Persisted lighthouse data on {}", uri),
                            Ok(false) => eprintln!("Failed to persist lighthouse data on {}: firmware reported failure", uri),
                            Err(e) => eprintln!("Failed to persist lighthouse data on {}: {:?}", uri, e),
                        }

                        cf.disconnect().await;
                        eprintln!("Geometry uploaded to {}", uri);

                        let done = completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                        let progress = done as f32 / total as f32;
                        let status: slint::SharedString = format!("Uploaded to {} ({}/{})", name, done, total).into();
                        let ui_weak_inner = ui_weak.clone();
                        slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_weak_inner.upgrade() {
                                ui.set_progress_dialog_progress(progress);
                                ui.set_progress_dialog_status(status);
                            }
                        }).ok();
                    });
                }

                // Wait for all uploads to complete
                while join_set.join_next().await.is_some() {}

                tokio::time::sleep(std::time::Duration::from_millis(500)).await;

                let ui_weak_inner = ui_weak.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak_inner.upgrade() {
                        ui.set_progress_dialog_visible(false);
                    }
                }).ok();
            });
        });
    }

    let trajectory_data: SharedTrajectoryData = Arc::new(Mutex::new(HashMap::new()));

    // Upload trajectory to all connected units in parallel
    {
        let swarm_state = swarm_state.clone();
        let trajectory_data = trajectory_data.clone();
        let ui_weak = ui.as_weak();

        ui.on_upload_trajectory_swarm(move || {
            let swarm_state = swarm_state.clone();
            let trajectory_data = trajectory_data.clone();
            let ui_weak = ui_weak.clone();

            tokio::spawn(async move {
                // Open file dialog
                let Some(handle) = rfd::AsyncFileDialog::new()
                    .add_filter("YAML", &["yaml", "yml"])
                    .pick_file()
                    .await
                else { return };
                let path = handle.path().to_path_buf();

                // Parse trajectory
                let contents = match std::fs::read_to_string(&path) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("Failed to read trajectory file: {}", e);
                        return;
                    }
                };
                let traj_config: TrajectoryConfig = match serde_yaml::from_str(&contents) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("Failed to parse trajectory file: {}", e);
                        return;
                    }
                };

                let viz_points = sample_trajectory(&traj_config);
                let total_duration: f32 = traj_config.segments.iter().map(|s| s.duration).sum();
                let segment_count = traj_config.segments.len();

                // Convert to Poly4D segments
                use crazyflie_lib::subsystems::memory::{Poly4D, Poly};
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
                let segments = Arc::new(segments);

                // Get all connected units
                let connected_units: Vec<(usize, Arc<crazyflie_lib::Crazyflie>)> = {
                    let state = swarm_state.lock().await;
                    state.iter().map(|(idx, cu)| (*idx, cu.cf.clone())).collect()
                };

                let total = connected_units.len();
                if total == 0 {
                    eprintln!("No connected units for trajectory upload");
                    return;
                }

                // Show progress dialog
                {
                    let ui_weak_inner = ui_weak.clone();
                    slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_weak_inner.upgrade() {
                            ui.set_progress_dialog_title("Uploading Trajectory".into());
                            ui.set_progress_dialog_progress(0.0);
                            ui.set_progress_dialog_status("Starting...".into());
                            ui.set_progress_dialog_visible(true);
                        }
                    }).ok();
                }

                let completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));

                let mut join_set = tokio::task::JoinSet::new();
                for (unit_idx, cf) in &connected_units {
                    let unit_idx = *unit_idx;
                    let cf = cf.clone();
                    let ui_weak = ui_weak.clone();
                    let completed = completed.clone();
                    let segments = segments.clone();

                    join_set.spawn(async move {
                        use crazyflie_lib::subsystems::memory::{MemoryType, TrajectoryMemory};

                        let mut success = true;
                        let traj_mems = cf.memory.get_memories(Some(MemoryType::Trajectory));
                        if let Some(mem) = traj_mems.first() {
                            if let Some(Ok(traj_mem)) = cf.memory.open_memory::<TrajectoryMemory>((*mem).clone()).await {
                                match traj_mem.write_uncompressed(&segments, 0).await {
                                    Ok(bytes) => eprintln!("Unit {}: uploaded {} bytes", unit_idx, bytes),
                                    Err(e) => {
                                        eprintln!("Unit {}: failed to upload trajectory: {:?}", unit_idx, e);
                                        success = false;
                                    }
                                }
                                cf.memory.close_memory(traj_mem).await.ok();
                            }
                        }

                        if success {
                            if let Err(e) = cf.high_level_commander
                                .define_trajectory(1, 0, segment_count as u8, None)
                                .await
                            {
                                eprintln!("Unit {}: failed to define trajectory: {:?}", unit_idx, e);
                            }
                        }

                        let done = completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                        let progress = done as f32 / total as f32;
                        let status: slint::SharedString = format!("Uploaded ({}/{})", done, total).into();
                        let ui_weak_inner = ui_weak.clone();
                        slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_weak_inner.upgrade() {
                                ui.set_progress_dialog_progress(progress);
                                ui.set_progress_dialog_status(status);
                            }
                        }).ok();
                    });
                }

                while join_set.join_next().await.is_some() {}

                // Store trajectory visualization data for each connected unit
                {
                    let mut td = trajectory_data.lock().await;
                    for (unit_idx, _) in &connected_units {
                        td.insert(*unit_idx, TrajectoryData {
                            points: viz_points.clone(),
                            duration: total_duration,
                            anchor: None,
                            saved_points: None,
                        });
                    }
                }

                tokio::time::sleep(std::time::Duration::from_millis(500)).await;

                let ui_weak_inner = ui_weak.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak_inner.upgrade() {
                        ui.set_progress_dialog_visible(false);
                    }
                }).ok();
            });
        });
    }

    // Fly trajectory on all connected units simultaneously
    {
        let swarm_state = swarm_state.clone();
        let trajectory_data = trajectory_data.clone();
        let ui_weak = ui.as_weak();
        ui.on_fly_trajectory_swarm(move || {
            let swarm_state = swarm_state.clone();
            let trajectory_data = trajectory_data.clone();
            let ui_weak = ui_weak.clone();

            tokio::spawn(async move {
                let connected_units: Vec<(usize, Arc<crazyflie_lib::Crazyflie>)> = {
                    let state = swarm_state.lock().await;
                    state.iter().map(|(idx, cu)| (*idx, cu.cf.clone())).collect()
                };

                if connected_units.is_empty() {
                    eprintln!("No connected units");
                    return;
                }

                // Arm all units in parallel
                eprintln!("Arming {} units...", connected_units.len());
                let mut join_set = tokio::task::JoinSet::new();
                for (idx, cf) in &connected_units {
                    let cf = cf.clone();
                    let idx = *idx;
                    join_set.spawn(async move {
                        if let Err(e) = cf.platform.send_arming_request(true).await {
                            eprintln!("Unit {}: arming failed: {:?}", idx, e);
                        }
                    });
                }
                while join_set.join_next().await.is_some() {}
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

                // Snapshot positions before takeoff and show takeoff line
                {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    let ui_weak_inner = ui_weak.clone();
                    let unit_indices: Vec<usize> = connected_units.iter().map(|(idx, _)| *idx).collect();
                    let _ = slint::invoke_from_event_loop(move || {
                        let mut positions = HashMap::new();
                        if let Some(ui) = ui_weak_inner.upgrade() {
                            let units = ui.get_units();
                            for idx in &unit_indices {
                                if let Some(u) = units.row_data(*idx) {
                                    positions.insert(*idx, [u.pos_x, u.pos_y, u.pos_z]);
                                }
                            }
                        }
                        let _ = tx.send(positions);
                    });
                    if let Ok(positions) = rx.await {
                        let mut td = trajectory_data.lock().await;
                        for (idx, pos) in &positions {
                            if let Some(data) = td.get_mut(idx) {
                                // Save real trajectory points and show a takeoff line instead
                                data.saved_points = Some(std::mem::take(&mut data.points));
                                data.points = vec![[0.0, 0.0, 0.0], [0.0, 0.0, 0.5]];
                                data.anchor = Some(*pos);
                            }
                        }
                    }
                }

                // Take off all units in parallel
                eprintln!("Taking off...");
                let mut join_set = tokio::task::JoinSet::new();
                for (idx, cf) in &connected_units {
                    let cf = cf.clone();
                    let idx = *idx;
                    join_set.spawn(async move {
                        if let Err(e) = cf.high_level_commander.take_off(0.5, None, 2.0, None).await {
                            eprintln!("Unit {}: take-off failed: {:?}", idx, e);
                        }
                    });
                }
                while join_set.join_next().await.is_some() {}
                tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;

                // Restore real trajectory points and snapshot post-takeoff positions as anchors
                {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    let ui_weak_inner = ui_weak.clone();
                    let unit_indices: Vec<usize> = connected_units.iter().map(|(idx, _)| *idx).collect();
                    let _ = slint::invoke_from_event_loop(move || {
                        let mut positions = HashMap::new();
                        if let Some(ui) = ui_weak_inner.upgrade() {
                            let units = ui.get_units();
                            for idx in &unit_indices {
                                if let Some(u) = units.row_data(*idx) {
                                    positions.insert(*idx, [u.pos_x, u.pos_y, u.pos_z]);
                                }
                            }
                        }
                        let _ = tx.send(positions);
                    });
                    if let Ok(positions) = rx.await {
                        let mut td = trajectory_data.lock().await;
                        for (idx, pos) in positions {
                            if let Some(data) = td.get_mut(&idx) {
                                // Restore the real trajectory points
                                if let Some(saved) = data.saved_points.take() {
                                    data.points = saved;
                                }
                                data.anchor = Some(pos);
                            }
                        }
                    }
                }

                // Start trajectory on all units as close together as possible
                eprintln!("Starting trajectory on all units...");
                for (idx, cf) in &connected_units {
                    if let Err(e) = cf
                        .high_level_commander
                        .start_trajectory(1, 1.0, true, false, false, None)
                        .await
                    {
                        eprintln!("Unit {}: start trajectory failed: {:?}", idx, e);
                    }
                }
            });
        });
    }

    // Land all connected units
    {
        let swarm_state = swarm_state.clone();
        ui.on_land_swarm(move || {
            let swarm_state = swarm_state.clone();

            tokio::spawn(async move {
                let connected_units: Vec<(usize, Arc<crazyflie_lib::Crazyflie>)> = {
                    let state = swarm_state.lock().await;
                    state.iter().map(|(idx, cu)| (*idx, cu.cf.clone())).collect()
                };

                eprintln!("Landing {} units...", connected_units.len());
                for (idx, cf) in &connected_units {
                    if let Err(e) = cf.high_level_commander.land(0.0, None, 2.0, None).await {
                        eprintln!("Unit {}: land failed: {:?}", idx, e);
                    }
                }
            });
        });
    }

    // Sync blink: broadcast synchronized blink command to all units via P2P radio broadcast
    {
        let link_context = link_context.clone();
        let ui_weak = ui.as_weak();
        ui.on_sync_blink(move || {
            let Some(ui_ref) = ui_weak.upgrade() else { return };
            let units = ui_ref.get_units();
            let uris: Vec<String> = (0..units.row_count())
                .filter_map(|i| units.row_data(i).map(|u| u.uri.to_string()))
                .collect();

            let link_context = link_context.clone();

            tokio::spawn(async move {
                // Collect unique (radio_nth, channel) pairs from all unit URIs
                let mut radio_channels: Vec<(usize, u8)> = uris
                    .iter()
                    .filter_map(|uri| parse_radio_uri(uri).map(|(r, ch, _)| (r, ch)))
                    .collect();
                radio_channels.sort();
                radio_channels.dedup();

                if radio_channels.is_empty() {
                    eprintln!("No radio URIs found for sync blink");
                    return;
                }

                const BROADCAST_ADDR: [u8; 5] = [0xff, 0xe7, 0xe7, 0xe7, 0xe7];
                const P2P_PORT: u8 = 0;

                // Send multiple broadcasts with decreasing delay so all units
                // converge on the same execution time
                let delays_ms: &[u16] = &[100, 80, 60, 40, 20];

                eprintln!("Broadcasting sync blink on {} channel(s)...", radio_channels.len());
                for &delay_ms in delays_ms {
                    // P2P packet format: [0xF3, 0x80 | port, ...payload...]
                    let packet = vec![
                        0xF3,
                        0x80 | P2P_PORT,
                        0x01, // CMD_SYNC_EXECUTE
                        0x00, // function index 0 (white blink)
                        (delay_ms & 0xFF) as u8,
                        (delay_ms >> 8) as u8,
                    ];

                    for &(radio_nth, ch) in &radio_channels {
                        let channel = match crazyradio::Channel::from_number(ch) {
                            Ok(c) => c,
                            Err(_) => continue,
                        };
                        match link_context.get_radio(radio_nth).await {
                            Ok(mut radio) => {
                                if let Err(e) = radio.send_packet_no_ack_async(channel, BROADCAST_ADDR, packet.clone()).await {
                                    eprintln!("Broadcast on channel {} failed: {:?}", ch, e);
                                }
                            }
                            Err(e) => eprintln!("Failed to get radio {}: {:?}", radio_nth, e),
                        }
                    }

                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                eprintln!("Sync blink broadcast done");
            });
        });
    }

    // Delayed sync blink: broadcast with 5 second delay, repeated broadcasts with decreasing time
    {
        let link_context = link_context.clone();
        let ui_weak = ui.as_weak();
        ui.on_delayed_sync_blink(move || {
            let Some(ui_ref) = ui_weak.upgrade() else { return };
            let units = ui_ref.get_units();
            let uris: Vec<String> = (0..units.row_count())
                .filter_map(|i| units.row_data(i).map(|u| u.uri.to_string()))
                .collect();

            let link_context = link_context.clone();

            tokio::spawn(async move {
                let mut radio_channels: Vec<(usize, u8)> = uris
                    .iter()
                    .filter_map(|uri| parse_radio_uri(uri).map(|(r, ch, _)| (r, ch)))
                    .collect();
                radio_channels.sort();
                radio_channels.dedup();

                if radio_channels.is_empty() {
                    eprintln!("No radio URIs found for delayed sync blink");
                    return;
                }

                const BROADCAST_ADDR: [u8; 5] = [0xff, 0xe7, 0xe7, 0xe7, 0xe7];
                const P2P_PORT: u8 = 0;
                const TOTAL_DELAY_MS: u16 = 5000;
                const BROADCAST_INTERVAL_MS: u64 = 100;

                eprintln!("Broadcasting delayed sync blink (5s) on {} channel(s)...", radio_channels.len());

                let start = std::time::Instant::now();
                loop {
                    let elapsed_ms = start.elapsed().as_millis() as u16;
                    let remaining_ms = TOTAL_DELAY_MS.saturating_sub(elapsed_ms);
                    if remaining_ms == 0 {
                        break;
                    }

                    let packet = vec![
                        0xF3,
                        0x80 | P2P_PORT,
                        0x01, // CMD_SYNC_EXECUTE
                        0x00, // function index 0 (white blink)
                        (remaining_ms & 0xFF) as u8,
                        (remaining_ms >> 8) as u8,
                    ];

                    for &(radio_nth, ch) in &radio_channels {
                        let channel = match crazyradio::Channel::from_number(ch) {
                            Ok(c) => c,
                            Err(_) => continue,
                        };
                        match link_context.get_radio(radio_nth).await {
                            Ok(mut radio) => {
                                if let Err(e) = radio.send_packet_no_ack_async(channel, BROADCAST_ADDR, packet.clone()).await {
                                    eprintln!("Broadcast on channel {} failed: {:?}", ch, e);
                                }
                            }
                            Err(e) => eprintln!("Failed to get radio {}: {:?}", radio_nth, e),
                        }
                    }

                    tokio::time::sleep(std::time::Duration::from_millis(BROADCAST_INTERVAL_MS)).await;
                }
                eprintln!("Delayed sync blink broadcast done");
            });
        });
    }

    // Emergency stop: disarm all connected units
    {
        let swarm_state = swarm_state.clone();
        ui.on_emergency_stop(move || {
            let swarm_state = swarm_state.clone();

            tokio::spawn(async move {
                let connected_units: Vec<(usize, Arc<crazyflie_lib::Crazyflie>)> = {
                    let state = swarm_state.lock().await;
                    state.iter().map(|(idx, cu)| (*idx, cu.cf.clone())).collect()
                };

                eprintln!("EMERGENCY STOP: disarming {} units...", connected_units.len());
                for (idx, cf) in &connected_units {
                    if let Err(e) = cf.platform.send_arming_request(false).await {
                        eprintln!("Unit {}: disarm failed: {:?}", idx, e);
                    }
                }
            });
        });
    }

    // Reboot swarm (send reboot to all units, then disconnect)
    {
        let link_context = link_context.clone();
        let swarm_state = swarm_state.clone();
        let positioning_source = positioning_source.clone();
        let ui_weak = ui.as_weak();
        ui.on_reboot_swarm(move || {
            let Some(ui_ref) = ui_weak.upgrade() else { return };
            let units = ui_ref.get_units();
            let uris: Vec<String> = (0..units.row_count())
                .filter_map(|i| units.row_data(i).map(|u| u.uri.to_string()))
                .collect();

            let link_context = link_context.clone();
            let swarm_state = swarm_state.clone();
            let positioning_source = positioning_source.clone();
            let ui_weak = ui_weak.clone();

            tokio::spawn(async move {
                let futs: Vec<_> = uris.iter().map(|uri| {
                    let link_context = link_context.clone();
                    let uri = uri.clone();
                    async move {
                        eprintln!("Rebooting {} ...", uri);
                        if let Err(e) = send_bootloader_command(&link_context, &uri, BOOTLOADER_CMD_RESET_INIT, None).await {
                            eprintln!("Reboot reset-init failed for {}: {:?}", uri, e);
                        }
                        if let Err(e) = send_bootloader_command(&link_context, &uri, BOOTLOADER_CMD_RESET, Some(&[0x01])).await {
                            eprintln!("Reboot reset failed for {}: {:?}", uri, e);
                        }
                    }
                }).collect();
                futures::future::join_all(futs).await;

                // Disconnect all units
                {
                    let mut ps = positioning_source.lock().await;
                    *ps = None;
                }
                let connected: Vec<(usize, ConnectedUnit)> = {
                    let mut state = swarm_state.lock().await;
                    state.drain().collect()
                };
                for (index, cu) in connected {
                    cu.cf.disconnect().await;
                    update_unit(&ui_weak, index, |u| {
                        u.state = UnitState::Disconnected;
                        u.pos_x = 0.0; u.pos_y = 0.0; u.pos_z = 0.0;
                        u.battery_voltage = 0.0; u.link_quality = 0.0;
                        u.pm_state = "".into(); u.serial = "".into();
                        u.platform_type = "".into(); u.firmware_version = "".into();
                        u.journal_entry_count = 0;
                    });
                }
                let ui_weak_inner = ui_weak.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak_inner.upgrade() {
                        ui.set_swarm_connected(false);
                    }
                }).ok();
            });
        });
    }

    // Power off swarm (send power-off to all units, then disconnect)
    {
        let link_context = link_context.clone();
        let swarm_state = swarm_state.clone();
        let positioning_source = positioning_source.clone();
        let ui_weak = ui.as_weak();
        ui.on_power_off_swarm(move || {
            let Some(ui_ref) = ui_weak.upgrade() else { return };
            let units = ui_ref.get_units();
            let uris: Vec<String> = (0..units.row_count())
                .filter_map(|i| units.row_data(i).map(|u| u.uri.to_string()))
                .collect();

            let link_context = link_context.clone();
            let swarm_state = swarm_state.clone();
            let positioning_source = positioning_source.clone();
            let ui_weak = ui_weak.clone();

            tokio::spawn(async move {
                let futs: Vec<_> = uris.iter().map(|uri| {
                    let link_context = link_context.clone();
                    let uri = uri.clone();
                    async move {
                        eprintln!("Powering off {} ...", uri);
                        if let Err(e) = send_bootloader_command(&link_context, &uri, BOOTLOADER_CMD_ALL_OFF, None).await {
                            eprintln!("Power off failed for {}: {:?}", uri, e);
                        }
                    }
                }).collect();
                futures::future::join_all(futs).await;

                // Disconnect all units
                {
                    let mut ps = positioning_source.lock().await;
                    *ps = None;
                }
                let connected: Vec<(usize, ConnectedUnit)> = {
                    let mut state = swarm_state.lock().await;
                    state.drain().collect()
                };
                for (index, cu) in connected {
                    cu.cf.disconnect().await;
                    update_unit(&ui_weak, index, |u| {
                        u.state = UnitState::Disconnected;
                        u.pos_x = 0.0; u.pos_y = 0.0; u.pos_z = 0.0;
                        u.battery_voltage = 0.0; u.link_quality = 0.0;
                        u.pm_state = "".into(); u.serial = "".into();
                        u.platform_type = "".into(); u.firmware_version = "".into();
                        u.journal_entry_count = 0;
                    });
                }
                let ui_weak_inner = ui_weak.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak_inner.upgrade() {
                        ui.set_swarm_connected(false);
                    }
                }).ok();
            });
        });
    }

    // SysOff swarm (sleep all units, do NOT disconnect)
    {
        let link_context = link_context.clone();
        let ui_weak = ui.as_weak();
        ui.on_sysoff_swarm(move || {
            let Some(ui_ref) = ui_weak.upgrade() else { return };
            let units = ui_ref.get_units();
            let uris: Vec<String> = (0..units.row_count())
                .filter_map(|i| units.row_data(i).map(|u| u.uri.to_string()))
                .collect();

            let link_context = link_context.clone();

            tokio::spawn(async move {
                let futs: Vec<_> = uris.iter().map(|uri| {
                    let link_context = link_context.clone();
                    let uri = uri.clone();
                    async move {
                        eprintln!("Sending sysoff (sleep) to {} ...", uri);
                        if let Err(e) = send_bootloader_command(&link_context, &uri, BOOTLOADER_CMD_SYS_OFF, None).await {
                            eprintln!("SysOff failed for {}: {:?}", uri, e);
                        }
                    }
                }).collect();
                futures::future::join_all(futs).await;
            });
        });
    }

    // SysOn swarm (wake all units, do NOT disconnect)
    {
        let link_context = link_context.clone();
        let ui_weak = ui.as_weak();
        ui.on_syson_swarm(move || {
            let Some(ui_ref) = ui_weak.upgrade() else { return };
            let units = ui_ref.get_units();
            let uris: Vec<String> = (0..units.row_count())
                .filter_map(|i| units.row_data(i).map(|u| u.uri.to_string()))
                .collect();

            let link_context = link_context.clone();

            tokio::spawn(async move {
                let futs: Vec<_> = uris.iter().map(|uri| {
                    let link_context = link_context.clone();
                    let uri = uri.clone();
                    async move {
                        eprintln!("Sending syson (wake) to {} ...", uri);
                        if let Err(e) = send_bootloader_command(&link_context, &uri, BOOTLOADER_CMD_SYS_ON, None).await {
                            eprintln!("SysOn failed for {}: {:?}", uri, e);
                        }
                    }
                }).collect();
                futures::future::join_all(futs).await;
            });
        });
    }

    // Handle positioning source dropdown change
    {
        let positioning_source = positioning_source.clone();
        ui.on_positioning_source_changed(move |index| {
            let positioning_source = positioning_source.clone();
            tokio::spawn(async move {
                let mut ps = positioning_source.lock().await;
                *ps = Some(index as usize);
                eprintln!("Positioning source changed to unit {}", index);
            });
        });
    }

    // Continuous positioning data reader (every 2 seconds)
    {
        let swarm_state = swarm_state.clone();
        let positioning_data = positioning_data.clone();
        let positioning_source = positioning_source.clone();

        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

                let source_index = {
                    let ps = positioning_source.lock().await;
                    match *ps {
                        Some(idx) => idx,
                        None => continue,
                    }
                };

                let cf = {
                    let state = swarm_state.lock().await;
                    match state.get(&source_index) {
                        Some(cu) => cu.cf.clone(),
                        None => continue, // Not connected, skip
                    }
                };

                use crazyflie_lib::subsystems::memory::MemoryType;

                let mut lighthouse_positions = Vec::new();
                let mut loco_positions = Vec::new();

                // Read Lighthouse base station geometries
                let lh_mems = cf.memory.get_memories(Some(MemoryType::Lighthouse));
                if let Some(mem) = lh_mems.first() {
                    use crazyflie_lib::subsystems::memory::LighthouseMemory;
                    if let Some(Ok(lh)) = cf.memory.open_memory::<LighthouseMemory>((*mem).clone()).await {
                        match lh.read_all_geometries().await {
                            Ok(geos) => {
                                for (id, geo) in &geos {
                                    if geo.valid {
                                        lighthouse_positions.push((*id as u8, geo.origin));
                                    }
                                }
                            }
                            Err(e) => eprintln!("Failed to read lighthouse geometries: {:?}", e),
                        }
                        cf.memory.close_memory(lh).await.ok();
                    }
                }

                // Read Loco anchor positions
                let loco_mems = cf.memory.get_memories(Some(MemoryType::Loco2));
                if let Some(mem) = loco_mems.first() {
                    use crazyflie_lib::subsystems::memory::LocoMemory2;
                    if let Some(Ok(loco)) = cf.memory.open_memory::<LocoMemory2>((*mem).clone()).await {
                        match loco.read_all().await {
                            Ok(data) => {
                                for (id, anchor) in &data.anchors {
                                    if anchor.is_valid {
                                        loco_positions.push((*id as u8, anchor.position));
                                    }
                                }
                            }
                            Err(e) => eprintln!("Failed to read loco data: {:?}", e),
                        }
                        cf.memory.close_memory(loco).await.ok();
                    }
                }

                // Store positioning data
                {
                    let mut pd = positioning_data.lock().await;
                    pd.lighthouse_bs = lighthouse_positions;
                    // Update the seen-cache with current positions
                    for &(id, pos) in &loco_positions {
                        pd.loco_seen.insert(id, pos);
                    }
                    pd.loco_anchors = loco_positions;
                }
            }
        });
    }

    // Open journal for selected unit
    {
        let journal_store = journal_store.clone();
        let ui_weak = ui.as_weak();

        ui.on_open_journal(move |row_index| {
            if row_index < 0 {
                return;
            }

            let serial = {
                let Some(ui) = ui_weak.upgrade() else { return };
                let units = ui.get_units();
                let col = ui.get_sort_column();
                let ascending = ui.get_sort_ascending();
                let indices = sort_unit_indices(&units, col, ascending);
                let row = row_index as usize;
                if row >= indices.len() {
                    return;
                }
                let original = indices[row];
                match units.row_data(original) {
                    Some(u) => u.serial.to_string(),
                    None => return,
                }
            };

            let journal_store = journal_store.clone();
            let ui_weak = ui_weak.clone();

            tokio::spawn(async move {
                let entries = {
                    let store = journal_store.lock().await;
                    store.get(&serial).cloned().unwrap_or_default()
                };

                let slint_entries: Vec<JournalEntryData> = entries
                    .iter()
                    .rev()
                    .map(|e| JournalEntryData {
                        timestamp: e.timestamp.clone().into(),
                        text: e.text.clone().into(),
                    })
                    .collect();

                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_journal_entries(slint::ModelRc::new(slint::VecModel::from(slint_entries)));
                    }
                }).ok();
            });
        });
    }

    // Add journal entry
    {
        let journal_store = journal_store.clone();
        let ui_weak = ui.as_weak();

        ui.on_add_journal_entry(move |row_index, text| {
            if row_index < 0 || text.is_empty() {
                return;
            }

            let (serial, original_index) = {
                let Some(ui) = ui_weak.upgrade() else { return };
                let units = ui.get_units();
                let col = ui.get_sort_column();
                let ascending = ui.get_sort_ascending();
                let indices = sort_unit_indices(&units, col, ascending);
                let row = row_index as usize;
                if row >= indices.len() {
                    return;
                }
                let original = indices[row];
                match units.row_data(original) {
                    Some(u) => (u.serial.to_string(), original),
                    None => return,
                }
            };

            if serial.is_empty() {
                eprintln!("Cannot add journal entry: unit has no serial number");
                return;
            }

            let text = text.to_string();
            let journal_store = journal_store.clone();
            let ui_weak = ui_weak.clone();

            tokio::spawn(async move {
                let (entries, new_count) = {
                    let mut store = journal_store.lock().await;
                    let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
                    let entry = JournalEntry { timestamp, text };
                    store.entry(serial.clone()).or_default().push(entry);
                    save_journal(&store);
                    let entries = store.get(&serial).cloned().unwrap_or_default();
                    let count = entries.len() as i32;
                    (entries, count)
                };

                let slint_entries: Vec<JournalEntryData> = entries
                    .iter()
                    .rev()
                    .map(|e| JournalEntryData {
                        timestamp: e.timestamp.clone().into(),
                        text: e.text.clone().into(),
                    })
                    .collect();

                let ui_weak_count = ui_weak.clone();
                update_unit(&ui_weak_count, original_index, move |u| {
                    u.journal_entry_count = new_count;
                });

                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_journal_entries(slint::ModelRc::new(slint::VecModel::from(slint_entries)));
                    }
                }).ok();
            });
        });
    }

    // Delete journal entry
    {
        let journal_store = journal_store.clone();
        let ui_weak = ui.as_weak();

        ui.on_delete_journal_entry(move |row_index, entry_index| {
            if row_index < 0 || entry_index < 0 {
                return;
            }

            let (serial, original_index) = {
                let Some(ui) = ui_weak.upgrade() else { return };
                let units = ui.get_units();
                let col = ui.get_sort_column();
                let ascending = ui.get_sort_ascending();
                let indices = sort_unit_indices(&units, col, ascending);
                let row = row_index as usize;
                if row >= indices.len() {
                    return;
                }
                let original = indices[row];
                match units.row_data(original) {
                    Some(u) => (u.serial.to_string(), original),
                    None => return,
                }
            };

            if serial.is_empty() {
                return;
            }

            let journal_store = journal_store.clone();
            let ui_weak = ui_weak.clone();
            let entry_index = entry_index as usize;

            tokio::spawn(async move {
                let (entries, new_count) = {
                    let mut store = journal_store.lock().await;
                    let Some(unit_entries) = store.get_mut(&serial) else {
                        return;
                    };
                    // UI displays entries in reverse order, so map index back
                    let store_index = unit_entries.len().saturating_sub(1 + entry_index);
                    if store_index < unit_entries.len() {
                        unit_entries.remove(store_index);
                    }
                    save_journal(&store);
                    let entries = store.get(&serial).cloned().unwrap_or_default();
                    let count = entries.len() as i32;
                    (entries, count)
                };

                let slint_entries: Vec<JournalEntryData> = entries
                    .iter()
                    .rev()
                    .map(|e| JournalEntryData {
                        timestamp: e.timestamp.clone().into(),
                        text: e.text.clone().into(),
                    })
                    .collect();

                let ui_weak_count = ui_weak.clone();
                update_unit(&ui_weak_count, original_index, move |u| {
                    u.journal_entry_count = new_count;
                });

                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_journal_entries(slint::ModelRc::new(slint::VecModel::from(slint_entries)));
                    }
                }).ok();
            });
        });
    }

    // Upload trajectory
    {
        let swarm_state = swarm_state.clone();
        let trajectory_data = trajectory_data.clone();
        let ui_weak = ui.as_weak();

        ui.on_upload_trajectory(move |row_index| {
            if row_index < 0 {
                return;
            }

            let original_index = {
                let Some(ui) = ui_weak.upgrade() else { return };
                let units = ui.get_units();
                let col = ui.get_sort_column();
                let ascending = ui.get_sort_ascending();
                let indices = sort_unit_indices(&units, col, ascending);
                let row = row_index as usize;
                if row >= indices.len() {
                    return;
                }
                indices[row]
            };

            let swarm_state = swarm_state.clone();
            let trajectory_data = trajectory_data.clone();

            tokio::spawn(async move {
                // Open file dialog
                let Some(handle) = rfd::AsyncFileDialog::new()
                    .add_filter("YAML", &["yaml", "yml"])
                    .pick_file()
                    .await
                else { return };
                let path = handle.path().to_path_buf();

                // Parse trajectory
                let contents = match std::fs::read_to_string(&path) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("Failed to read trajectory file: {}", e);
                        return;
                    }
                };
                let traj_config: TrajectoryConfig = match serde_yaml::from_str(&contents) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("Failed to parse trajectory file: {}", e);
                        return;
                    }
                };

                // Sample points for visualization (relative to unit position)
                let viz_points = sample_trajectory(&traj_config);
                let total_duration: f32 = traj_config.segments.iter().map(|s| s.duration).sum();
                let segment_count = traj_config.segments.len();

                // Convert to Poly4D segments
                use crazyflie_lib::subsystems::memory::{Poly4D, Poly};
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

                // Get connected Crazyflie
                let cf = {
                    let state = swarm_state.lock().await;
                    match state.get(&original_index) {
                        Some(cu) => cu.cf.clone(),
                        None => {
                            eprintln!("Unit {} is not connected", original_index);
                            // Still store visualization data even if not connected
                            let mut td = trajectory_data.lock().await;
                            td.insert(original_index, TrajectoryData {
                                points: viz_points,
                                duration: total_duration,
                                anchor: None,
                                saved_points: None,
                            });
                            return;
                        }
                    }
                };

                // Upload trajectory to Crazyflie memory
                use crazyflie_lib::subsystems::memory::MemoryType;
                use crazyflie_lib::subsystems::memory::TrajectoryMemory;

                let traj_mems = cf.memory.get_memories(Some(MemoryType::Trajectory));
                if let Some(mem) = traj_mems.first() {
                    if let Some(Ok(traj_mem)) = cf.memory.open_memory::<TrajectoryMemory>((*mem).clone()).await {
                        match traj_mem.write_uncompressed(&segments, 0).await {
                            Ok(bytes) => eprintln!("Uploaded {} bytes of trajectory data", bytes),
                            Err(e) => {
                                eprintln!("Failed to upload trajectory: {:?}", e);
                                cf.memory.close_memory(traj_mem).await.ok();
                                return;
                            }
                        }
                        cf.memory.close_memory(traj_mem).await.ok();
                    }
                }

                // Define trajectory (ID=1, offset=0)
                if let Err(e) = cf
                    .high_level_commander
                    .define_trajectory(1, 0, segment_count as u8, None)
                    .await
                {
                    eprintln!("Failed to define trajectory: {:?}", e);
                }

                eprintln!("Trajectory uploaded and defined ({} segments, {:.1}s)", segment_count, total_duration);

                // Store trajectory data for visualization and flight
                {
                    let mut td = trajectory_data.lock().await;
                    td.insert(original_index, TrajectoryData {
                        points: viz_points,
                        duration: total_duration,
                        anchor: None,
                        saved_points: None,
                    });
                }
            });
        });
    }

    // Fly trajectory
    {
        let swarm_state = swarm_state.clone();
        let trajectory_data = trajectory_data.clone();
        let ui_weak = ui.as_weak();

        ui.on_fly_trajectory(move |row_index| {
            if row_index < 0 {
                return;
            }

            let original_index = {
                let Some(ui) = ui_weak.upgrade() else { return };
                let units = ui.get_units();
                let col = ui.get_sort_column();
                let ascending = ui.get_sort_ascending();
                let indices = sort_unit_indices(&units, col, ascending);
                let row = row_index as usize;
                if row >= indices.len() {
                    return;
                }
                indices[row]
            };

            let swarm_state = swarm_state.clone();
            let trajectory_data = trajectory_data.clone();
            let ui_weak = ui_weak.clone();

            tokio::spawn(async move {
                let duration = {
                    let td = trajectory_data.lock().await;
                    match td.get(&original_index) {
                        Some(d) => d.duration,
                        None => {
                            eprintln!("No trajectory uploaded for unit {}", original_index);
                            return;
                        }
                    }
                };

                let cf = {
                    let state = swarm_state.lock().await;
                    match state.get(&original_index) {
                        Some(cu) => cu.cf.clone(),
                        None => {
                            eprintln!("Unit {} is not connected", original_index);
                            return;
                        }
                    }
                };

                eprintln!("Arming...");
                if let Err(e) = cf.platform.send_arming_request(true).await {
                    eprintln!("Arming failed: {:?}", e);
                    return;
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

                // Snapshot position before takeoff and show takeoff line
                {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    let ui_weak_inner = ui_weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        let pos = if let Some(ui) = ui_weak_inner.upgrade() {
                            let units = ui.get_units();
                            units.row_data(original_index).map(|u| [u.pos_x, u.pos_y, u.pos_z])
                        } else {
                            None
                        };
                        let _ = tx.send(pos);
                    });
                    if let Ok(Some(pos)) = rx.await {
                        let mut td = trajectory_data.lock().await;
                        if let Some(data) = td.get_mut(&original_index) {
                            data.saved_points = Some(std::mem::take(&mut data.points));
                            data.points = vec![[0.0, 0.0, 0.0], [0.0, 0.0, 0.5]];
                            data.anchor = Some(pos);
                        }
                    }
                }

                eprintln!("Taking off...");
                if let Err(e) = cf.high_level_commander.take_off(0.5, None, 2.0, None).await {
                    eprintln!("Take-off failed: {:?}", e);
                }
                tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;

                // Restore real trajectory points and snapshot post-takeoff position as anchor
                {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    let ui_weak_inner = ui_weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        let pos = if let Some(ui) = ui_weak_inner.upgrade() {
                            let units = ui.get_units();
                            units.row_data(original_index).map(|u| [u.pos_x, u.pos_y, u.pos_z])
                        } else {
                            None
                        };
                        let _ = tx.send(pos);
                    });
                    if let Ok(Some(pos)) = rx.await {
                        let mut td = trajectory_data.lock().await;
                        if let Some(data) = td.get_mut(&original_index) {
                            if let Some(saved) = data.saved_points.take() {
                                data.points = saved;
                            }
                            data.anchor = Some(pos);
                        }
                    }
                }

                eprintln!("Starting trajectory ({:.1}s)...", duration);
                if let Err(e) = cf
                    .high_level_commander
                    .start_trajectory(1, 1.0, true, false, false, None)
                    .await
                {
                    eprintln!("Start trajectory failed: {:?}", e);
                }
                tokio::time::sleep(tokio::time::Duration::from_secs_f32(duration + 0.5)).await;

                eprintln!("Landing...");
                if let Err(e) = cf.high_level_commander.land(0.0, None, 2.0, None).await {
                    eprintln!("Land failed: {:?}", e);
                }
            });
        });
    }

    // Clear trajectory
    {
        let trajectory_data = trajectory_data.clone();
        let ui_weak = ui.as_weak();

        ui.on_clear_trajectory(move |row_index| {
            if row_index < 0 {
                return;
            }

            let original_index = {
                let Some(ui) = ui_weak.upgrade() else { return };
                let units = ui.get_units();
                let col = ui.get_sort_column();
                let ascending = ui.get_sort_ascending();
                let indices = sort_unit_indices(&units, col, ascending);
                let row = row_index as usize;
                if row >= indices.len() {
                    return;
                }
                indices[row]
            };

            let trajectory_data = trajectory_data.clone();
            tokio::spawn(async move {
                let mut td = trajectory_data.lock().await;
                td.remove(&original_index);
                eprintln!("Cleared trajectory for unit {}", original_index);
            });
        });
    }

    // HLC Takeoff
    {
        let swarm_state = swarm_state.clone();
        let ui_weak = ui.as_weak();

        ui.on_hlc_takeoff(move |row_index, height_str, yaw_str, time_str| {
            if row_index < 0 {
                return;
            }

            let original_index = {
                let Some(ui) = ui_weak.upgrade() else { return };
                let units = ui.get_units();
                let col = ui.get_sort_column();
                let ascending = ui.get_sort_ascending();
                let indices = sort_unit_indices(&units, col, ascending);
                let row = row_index as usize;
                if row >= indices.len() {
                    return;
                }
                indices[row]
            };

            let height: f32 = height_str.parse().unwrap_or(0.5);
            let yaw_deg: f32 = yaw_str.parse().unwrap_or(0.0);
            let duration: f32 = time_str.parse().unwrap_or(2.0);
            let yaw_rad = yaw_deg.to_radians();

            let swarm_state = swarm_state.clone();

            tokio::spawn(async move {
                let cf = {
                    let state = swarm_state.lock().await;
                    match state.get(&original_index) {
                        Some(cu) => cu.cf.clone(),
                        None => {
                            eprintln!("Unit {} is not connected", original_index);
                            return;
                        }
                    }
                };

                eprintln!("Arming unit {}...", original_index);
                if let Err(e) = cf.platform.send_arming_request(true).await {
                    eprintln!("Arming failed: {:?}", e);
                    return;
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

                eprintln!("Taking off unit {} to {:.2}m, yaw={:.1}deg, over {:.1}s...", original_index, height, yaw_deg, duration);
                if let Err(e) = cf.high_level_commander.take_off(height, Some(yaw_rad), duration, None).await {
                    eprintln!("Takeoff failed: {:?}", e);
                }
            });
        });
    }

    // HLC Goto
    {
        let swarm_state = swarm_state.clone();
        let ui_weak = ui.as_weak();

        ui.on_hlc_goto(move |row_index, x_str, y_str, z_str, yaw_str, relative| {
            if row_index < 0 {
                return;
            }

            let original_index = {
                let Some(ui) = ui_weak.upgrade() else { return };
                let units = ui.get_units();
                let col = ui.get_sort_column();
                let ascending = ui.get_sort_ascending();
                let indices = sort_unit_indices(&units, col, ascending);
                let row = row_index as usize;
                if row >= indices.len() {
                    return;
                }
                indices[row]
            };

            let x: f32 = x_str.parse().unwrap_or(0.0);
            let y: f32 = y_str.parse().unwrap_or(0.0);
            let z: f32 = z_str.parse().unwrap_or(0.0);
            let yaw_deg: f32 = yaw_str.parse().unwrap_or(0.0);
            let yaw_rad = yaw_deg.to_radians();

            let swarm_state = swarm_state.clone();

            tokio::spawn(async move {
                let cf = {
                    let state = swarm_state.lock().await;
                    match state.get(&original_index) {
                        Some(cu) => cu.cf.clone(),
                        None => {
                            eprintln!("Unit {} is not connected", original_index);
                            return;
                        }
                    }
                };

                eprintln!(
                    "Goto unit {} ({:.2}, {:.2}, {:.2}) yaw={:.1}deg relative={} over 2.0s...",
                    original_index, x, y, z, yaw_deg, relative
                );
                if let Err(e) = cf.high_level_commander.go_to(x, y, z, yaw_rad, 2.0, relative, false, None).await {
                    eprintln!("Goto failed: {:?}", e);
                }
            });
        });
    }

    // HLC Land
    {
        let swarm_state = swarm_state.clone();
        let ui_weak = ui.as_weak();

        ui.on_hlc_land(move |row_index| {
            if row_index < 0 {
                return;
            }

            let original_index = {
                let Some(ui) = ui_weak.upgrade() else { return };
                let units = ui.get_units();
                let col = ui.get_sort_column();
                let ascending = ui.get_sort_ascending();
                let indices = sort_unit_indices(&units, col, ascending);
                let row = row_index as usize;
                if row >= indices.len() {
                    return;
                }
                indices[row]
            };

            let swarm_state = swarm_state.clone();

            tokio::spawn(async move {
                let cf = {
                    let state = swarm_state.lock().await;
                    match state.get(&original_index) {
                        Some(cu) => cu.cf.clone(),
                        None => {
                            eprintln!("Unit {} is not connected", original_index);
                            return;
                        }
                    }
                };

                eprintln!("Landing unit {}...", original_index);
                if let Err(e) = cf.high_level_commander.land(0.0, None, 2.0, None).await {
                    eprintln!("Land failed: {:?}", e);
                }
            });
        });
    }

    // Identify unit (blink LEDs or pulse motors)
    {
        let swarm_state = swarm_state.clone();
        let ui_weak = ui.as_weak();

        ui.on_identify_unit(move |row_index| {
            if row_index < 0 {
                return;
            }

            let (original_index, has_led_top, has_led_bottom) = {
                let Some(ui) = ui_weak.upgrade() else { return };
                let units = ui.get_units();
                let col = ui.get_sort_column();
                let ascending = ui.get_sort_ascending();
                let indices = sort_unit_indices(&units, col, ascending);
                let row = row_index as usize;
                if row >= indices.len() {
                    return;
                }
                let idx = indices[row];
                let sorted = ui.get_sorted_units();
                let unit = sorted.row_data(row).unwrap();
                (idx, unit.deck_led_top, unit.deck_led_bottom)
            };

            let swarm_state = swarm_state.clone();
            let ui_weak = ui_weak.clone();

            tokio::spawn(async move {
                let (cf, stop_flag) = {
                    let mut state = swarm_state.lock().await;
                    let Some(cu) = state.get_mut(&original_index) else {
                        eprintln!("Unit {} is not connected", original_index);
                        return;
                    };

                    if let Some(existing_stop) = cu.identify_stop.take() {
                        // Already identifying — signal the loop to stop; it will clean up.
                        existing_stop.store(true, Ordering::Relaxed);
                        return;
                    }

                    let stop = Arc::new(AtomicBool::new(false));
                    cu.identify_stop = Some(stop.clone());
                    (cu.cf.clone(), stop)
                };

                update_unit(&ui_weak, original_index, |u| { u.identifying = true; });

                // Colors: white, blue, red, green (WRGB8888 format: 0x00RRGGBB)
                let colors: [u32; 4] = [0x00FFFFFF, 0x000000FF, 0x00FF0000, 0x0000FF00];
                let mut color_idx = 0usize;

                let _ = cf.param.set("motorPowerSet.enable", 2u8).await;

                loop {
                    if stop_flag.load(Ordering::Relaxed) { break; }

                    let color = colors[color_idx % colors.len()];
                    color_idx += 1;

                    if has_led_top { let _ = cf.param.set("colorLedTop.wrgb8888", color).await; }
                    if has_led_bottom { let _ = cf.param.set("colorLedBot.wrgb8888", color).await; }

                    let _ = cf.param.set("motorPowerSet.m1", 5000u16).await;
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                    let _ = cf.param.set("motorPowerSet.m1", 0u16).await;

                    if stop_flag.load(Ordering::Relaxed) { break; }
                    tokio::time::sleep(tokio::time::Duration::from_millis(400)).await;
                }

                // Reset LEDs and motors
                if has_led_top { let _ = cf.param.set("colorLedTop.wrgb8888", 0u32).await; }
                if has_led_bottom { let _ = cf.param.set("colorLedBot.wrgb8888", 0u32).await; }
                let _ = cf.param.set("motorPowerSet.m1", 0u16).await;
                let _ = cf.param.set("motorPowerSet.enable", 0u8).await;

                {
                    let mut state = swarm_state.lock().await;
                    if let Some(cu) = state.get_mut(&original_index) {
                        cu.identify_stop = None;
                    }
                }
                update_unit(&ui_weak, original_index, |u| { u.identifying = false; });
            });
        });
    }

    // Lazy-fetch platform info when sidebar row is selected
    {
        let swarm_state = swarm_state.clone();
        let ui_weak = ui.as_weak();

        ui.on_unit_row_selected(move |row_index| {
            if row_index < 0 {
                return;
            }

            let (original_index, already_fetched) = {
                let Some(ui) = ui_weak.upgrade() else { return };
                let units = ui.get_units();
                let col = ui.get_sort_column();
                let ascending = ui.get_sort_ascending();
                let indices = sort_unit_indices(&units, col, ascending);
                let row = row_index as usize;
                if row >= indices.len() {
                    return;
                }
                let idx = indices[row];
                let sorted = ui.get_sorted_units();
                let unit = sorted.row_data(row).unwrap();
                if unit.state == UnitState::Disconnected {
                    return;
                }
                (idx, !unit.platform_type.is_empty())
            };

            if already_fetched {
                return;
            }

            let swarm_state = swarm_state.clone();
            let ui_weak = ui_weak.clone();

            tokio::spawn(async move {
                let cf = {
                    let state = swarm_state.lock().await;
                    match state.get(&original_index) {
                        Some(cu) => cu.cf.clone(),
                        None => return,
                    }
                };

                let platform_type = cf.platform.device_type_name().await.unwrap_or_default();
                let firmware_version = cf.platform.firmware_version().await.unwrap_or_default();

                update_unit(&ui_weak, original_index, move |u| {
                    u.platform_type = platform_type.into();
                    u.firmware_version = firmware_version.into();
                });
            });
        });
    }

    // Connect single unit
    {
        let link_context = link_context.clone();
        let toc_cache = toc_cache.clone();
        let swarm_state = swarm_state.clone();
        let ui_weak = ui.as_weak();
        let positioning_source = positioning_source.clone();
        let positioning_data = positioning_data.clone();
        let journal_store = journal_store.clone();

        ui.on_connect_unit(move |row_index| {
            if row_index < 0 {
                return;
            }

            let (original_index, uri) = {
                let Some(ui) = ui_weak.upgrade() else { return };
                let units = ui.get_units();
                let col = ui.get_sort_column();
                let ascending = ui.get_sort_ascending();
                let indices = sort_unit_indices(&units, col, ascending);
                let row = row_index as usize;
                if row >= indices.len() {
                    return;
                }
                let idx = indices[row];
                let sorted = ui.get_sorted_units();
                let unit = sorted.row_data(row).unwrap();
                if !matches!(unit.state, UnitState::Disconnected | UnitState::Error) {
                    eprintln!("Unit {} is already connected", idx);
                    return;
                }
                (idx, unit.uri.to_string())
            };

            let link_context = link_context.clone();
            let toc_cache = toc_cache.clone();
            let swarm_state = swarm_state.clone();
            let ui_weak = ui_weak.clone();
            let positioning_source = positioning_source.clone();
            let positioning_data = positioning_data.clone();
            let journal_store = journal_store.clone();

            tokio::spawn(async move {
                eprintln!("Connecting to {} ...", uri);

                let cf = match tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    crazyflie_lib::Crazyflie::connect_from_uri(link_context.as_ref(), &uri, toc_cache),
                ).await {
                    Ok(Ok(cf)) => Arc::new(cf),
                    Ok(Err(e)) => {
                        eprintln!("Failed to connect to 1-- >{}: {:?}", uri, e);
                        let error_msg = format!("{}", e);
                        update_unit(&ui_weak, original_index, move |u| {
                            u.state = UnitState::Error;
                            u.error_message = error_msg.into();
                        });
                        return;
                    }
                    Err(_) => {
                        eprintln!("Connection to {} timed out", uri);
                        update_unit(&ui_weak, original_index, move |u| {
                            u.state = UnitState::Error;
                            u.error_message = "Connection timed out".into();
                        });
                        return;
                    }
                };

                eprintln!("Connected to {}", uri);

                {
                    let mut state = swarm_state.lock().await;
                    state.insert(original_index, ConnectedUnit { cf: cf.clone(), identify_stop: None });
                }

                let deck_lighthouse: u8 = cf.param.get("deck.bcLighthouse4").await.unwrap_or(0);
                let deck_loco: u8 = cf.param.get("deck.bcLoco").await.unwrap_or(0);
                let deck_led_top: u8 = cf.param.get("deck.bcColorLedTop").await.unwrap_or(0);
                let deck_led_bottom: u8 = cf.param.get("deck.bcColorLedBot").await.unwrap_or(0);

                let selftest_passed: i8 = cf.param.get("system.selftestPassed").await.unwrap_or(1);

                let id0: u32 = cf.param.get("cpu.id0").await.unwrap_or(0);
                let id1: u32 = cf.param.get("cpu.id1").await.unwrap_or(0);
                let id2: u32 = cf.param.get("cpu.id2").await.unwrap_or(0);
                let serial = format!("{:08X}{:08X}{:08X}", id0, id1, id2);

                let journal_count = {
                    let store = journal_store.lock().await;
                    store.get(&serial).map_or(0, |entries| entries.len()) as i32
                };

                update_unit(&ui_weak, original_index, move |u| {
                    u.state = UnitState::Connected;
                    u.deck_lighthouse = deck_lighthouse != 0;
                    u.deck_loco = deck_loco != 0;
                    u.deck_led_top = deck_led_top != 0;
                    u.deck_led_bottom = deck_led_bottom != 0;
                    u.serial = serial.into();
                    u.selftest_passed = selftest_passed != 0;
                    u.journal_entry_count = journal_count;
                });

                let ui_weak_inner = ui_weak.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak_inner.upgrade() {
                        ui.set_swarm_connected(true);
                    }
                }).ok();

                {
                    let mut ps = positioning_source.lock().await;
                    if ps.is_none() {
                        *ps = Some(original_index);
                        let ui_weak_inner = ui_weak.clone();
                        slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_weak_inner.upgrade() {
                                ui.set_positioning_source_index(original_index as i32);
                            }
                        }).ok();
                    }
                }

                start_telemetry(original_index, uri.clone(), cf.clone(), ui_weak, positioning_data, positioning_source).await;
            });
        });
    }

    // Disconnect single unit
    {
        let swarm_state = swarm_state.clone();
        let positioning_source = positioning_source.clone();
        let ui_weak = ui.as_weak();

        ui.on_disconnect_unit(move |row_index| {
            if row_index < 0 {
                return;
            }

            let original_index = {
                let Some(ui) = ui_weak.upgrade() else { return };
                let units = ui.get_units();
                let col = ui.get_sort_column();
                let ascending = ui.get_sort_ascending();
                let indices = sort_unit_indices(&units, col, ascending);
                let row = row_index as usize;
                if row >= indices.len() {
                    return;
                }
                indices[row]
            };

            let swarm_state = swarm_state.clone();
            let positioning_source = positioning_source.clone();
            let ui_weak = ui_weak.clone();

            tokio::spawn(async move {
                let connected = {
                    let mut state = swarm_state.lock().await;
                    state.remove(&original_index)
                };

                let Some(connected) = connected else {
                    eprintln!("Unit {} is not connected", original_index);
                    return;
                };

                eprintln!("Disconnecting unit {} ...", original_index);
                connected.cf.disconnect().await;

                update_unit(&ui_weak, original_index, |u| {
                    u.state = UnitState::Disconnected;
                    u.pos_x = 0.0;
                    u.pos_y = 0.0;
                    u.pos_z = 0.0;
                    u.battery_voltage = 0.0;
                    u.link_quality = 0.0;
                    u.deck_lighthouse = false;
                    u.deck_loco = false;
                    u.deck_led_top = false;
                    u.deck_led_bottom = false;
                    u.serial = "".into();
                    u.pm_state = "".into();
                    u.journal_entry_count = 0;
                    u.platform_type = "".into();
                    u.firmware_version = "".into();
                });

                // Reset positioning source if this was it
                {
                    let mut ps = positioning_source.lock().await;
                    if *ps == Some(original_index) {
                        *ps = None;
                    }
                }

                // Check if any units are still connected
                let any_connected = {
                    let state = swarm_state.lock().await;
                    !state.is_empty()
                };
                if !any_connected {
                    let ui_weak_inner = ui_weak.clone();
                    slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_weak_inner.upgrade() {
                            ui.set_swarm_connected(false);
                        }
                    }).ok();
                }

                eprintln!("Disconnected unit {}", original_index);
            });
        });
    }

    // Open manual control popup (enumerate joysticks, auto-start with first)
    {
        let gilrs = gilrs.clone();
        let gamepad_ids = gamepad_ids.clone();
        let swarm_state = swarm_state.clone();
        let manual_control = manual_control.clone();
        let ui_weak = ui.as_weak();

        ui.on_open_manual_control(move |row_index| {
            if row_index < 0 {
                return;
            }

            // Enumerate connected gamepads
            let mut names = Vec::new();
            let mut ids = Vec::new();

            {
                let mut g = gilrs.lock().unwrap();
                // Process pending events so connected state is up to date
                while g.next_event().is_some() {}

                for (id, gamepad) in g.gamepads() {
                    if gamepad.is_connected() {
                        names.push(gamepad.name().to_string());
                        ids.push(id);
                    }
                }
            }

            eprintln!("Manual control: found {} joystick(s)", names.len());

            // Store gamepad IDs synchronously for immediate use
            let first_gamepad_id = ids.first().copied();
            {
                let mut gids = gamepad_ids.lock().unwrap();
                *gids = ids;
            }

            // Reset armed state and update joystick names in UI
            let ui_weak_inner = ui_weak.clone();
            let names_shared: Vec<slint::SharedString> = names.into_iter().map(|n| n.into()).collect();
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak_inner.upgrade() {
                    ui.set_joystick_names(slint::ModelRc::new(slint::VecModel::from(names_shared)));
                    ui.set_manual_control_armed(false);
                }
            }).ok();

            // Auto-start control with the first joystick if available
            if let Some(gamepad_id) = first_gamepad_id {
                let original_index = {
                    let Some(ui) = ui_weak.upgrade() else { return };
                    let units = ui.get_units();
                    let col = ui.get_sort_column();
                    let ascending = ui.get_sort_ascending();
                    let indices = sort_unit_indices(&units, col, ascending);
                    let row = row_index as usize;
                    if row >= indices.len() {
                        return;
                    }
                    indices[row]
                };

                let swarm_state = swarm_state.clone();
                let manual_control = manual_control.clone();
                let gilrs = gilrs.clone();
                let ui_weak = ui_weak.clone();

                tokio::spawn(async move {
                    stop_manual_control_loop(&manual_control).await;

                    let cf = {
                        let state = swarm_state.lock().await;
                        match state.get(&original_index) {
                            Some(cu) => cu.cf.clone(),
                            None => {
                                eprintln!("Unit {} is not connected", original_index);
                                return;
                            }
                        }
                    };

                    let running = Arc::new(AtomicBool::new(true));
                    {
                        let mut mc = manual_control.lock().await;
                        *mc = Some(ManualControlState {
                            running: running.clone(),
                        });
                    }

                    eprintln!("Auto-starting manual control for unit {} with first joystick", original_index);

                    run_manual_control(cf, gilrs, gamepad_id, running, ui_weak).await;

                    eprintln!("Manual control ended for unit {}", original_index);
                });
            }
        });
    }

    // Start manual control (joystick switched via ComboBox)
    {
        let gilrs = gilrs.clone();
        let gamepad_ids = gamepad_ids.clone();
        let swarm_state = swarm_state.clone();
        let manual_control = manual_control.clone();
        let ui_weak = ui.as_weak();

        ui.on_start_manual_control(move |row_index, joystick_index| {
            if row_index < 0 || joystick_index < 0 {
                return;
            }

            let original_index = {
                let Some(ui) = ui_weak.upgrade() else { return };
                let units = ui.get_units();
                let col = ui.get_sort_column();
                let ascending = ui.get_sort_ascending();
                let indices = sort_unit_indices(&units, col, ascending);
                let row = row_index as usize;
                if row >= indices.len() {
                    return;
                }
                indices[row]
            };

            let joystick_idx = joystick_index as usize;
            let gamepad_id = {
                let gids = gamepad_ids.lock().unwrap();
                if joystick_idx >= gids.len() {
                    eprintln!("Invalid joystick index {}", joystick_idx);
                    return;
                }
                gids[joystick_idx]
            };

            let swarm_state = swarm_state.clone();
            let manual_control = manual_control.clone();
            let gilrs = gilrs.clone();
            let ui_weak = ui_weak.clone();

            tokio::spawn(async move {
                stop_manual_control_loop(&manual_control).await;

                let cf = {
                    let state = swarm_state.lock().await;
                    match state.get(&original_index) {
                        Some(cu) => cu.cf.clone(),
                        None => {
                            eprintln!("Unit {} is not connected", original_index);
                            return;
                        }
                    }
                };

                let running = Arc::new(AtomicBool::new(true));
                {
                    let mut mc = manual_control.lock().await;
                    *mc = Some(ManualControlState {
                        running: running.clone(),
                    });
                }

                eprintln!("Switching manual control for unit {} to joystick {}", original_index, joystick_idx);

                run_manual_control(cf, gilrs, gamepad_id, running, ui_weak).await;

                eprintln!("Manual control ended for unit {}", original_index);
            });
        });
    }

    // Stop manual control (popup closed)
    {
        let manual_control = manual_control.clone();
        let swarm_state = swarm_state.clone();
        let ui_weak = ui.as_weak();

        ui.on_stop_manual_control(move |row_index| {
            let manual_control = manual_control.clone();
            let swarm_state = swarm_state.clone();
            let ui_weak = ui_weak.clone();

            // Resolve original unit index for disarm
            let original_index = if row_index >= 0 {
                let Some(ui) = ui_weak.upgrade() else { return };
                let units = ui.get_units();
                let col = ui.get_sort_column();
                let ascending = ui.get_sort_ascending();
                let indices = sort_unit_indices(&units, col, ascending);
                let row = row_index as usize;
                if row < indices.len() { Some(indices[row]) } else { None }
            } else {
                None
            };

            tokio::spawn(async move {
                // Stop the control loop
                let mut mc = manual_control.lock().await;
                if let Some(state) = mc.take() {
                    state.running.store(false, Ordering::Relaxed);
                    eprintln!("Manual control stop requested");
                }
                drop(mc);

                // Auto-disarm on close
                if let Some(idx) = original_index {
                    let cf = {
                        let state = swarm_state.lock().await;
                        state.get(&idx).map(|cu| cu.cf.clone())
                    };
                    if let Some(cf) = cf {
                        if let Err(e) = cf.platform.send_arming_request(false).await {
                            eprintln!("Disarm on close failed: {:?}", e);
                        } else {
                            eprintln!("Auto-disarmed unit {} on popup close", idx);
                        }
                    }
                }

                // Reset armed state in UI
                let ui_weak_inner = ui_weak.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak_inner.upgrade() {
                        ui.set_manual_control_armed(false);
                    }
                }).ok();
            });
        });
    }

    // Arm/disarm unit
    {
        let swarm_state = swarm_state.clone();
        let ui_weak = ui.as_weak();

        ui.on_arm_unit(move |row_index, arm| {
            if row_index < 0 {
                return;
            }

            let original_index = {
                let Some(ui) = ui_weak.upgrade() else { return };
                let units = ui.get_units();
                let col = ui.get_sort_column();
                let ascending = ui.get_sort_ascending();
                let indices = sort_unit_indices(&units, col, ascending);
                let row = row_index as usize;
                if row >= indices.len() {
                    return;
                }
                indices[row]
            };

            let swarm_state = swarm_state.clone();
            let ui_weak = ui_weak.clone();

            tokio::spawn(async move {
                let cf = {
                    let state = swarm_state.lock().await;
                    match state.get(&original_index) {
                        Some(cu) => cu.cf.clone(),
                        None => {
                            eprintln!("Unit {} is not connected", original_index);
                            return;
                        }
                    }
                };

                match cf.platform.send_arming_request(arm).await {
                    Ok(()) => {
                        eprintln!("Unit {} {}", original_index, if arm { "armed" } else { "disarmed" });
                        let ui_weak_inner = ui_weak.clone();
                        slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_weak_inner.upgrade() {
                                ui.set_manual_control_armed(arm);
                            }
                        }).ok();
                    }
                    Err(e) => {
                        eprintln!("Arm request failed for unit {}: {:?}", original_index, e);
                    }
                }
            });
        });
    }

    // Recover crashed unit
    {
        let swarm_state = swarm_state.clone();
        let ui_weak = ui.as_weak();

        ui.on_recover_unit(move |row_index| {
            if row_index < 0 {
                return;
            }

            let original_index = {
                let Some(ui) = ui_weak.upgrade() else { return };
                let units = ui.get_units();
                let col = ui.get_sort_column();
                let ascending = ui.get_sort_ascending();
                let indices = sort_unit_indices(&units, col, ascending);
                let row = row_index as usize;
                if row >= indices.len() {
                    return;
                }
                indices[row]
            };

            let swarm_state = swarm_state.clone();

            tokio::spawn(async move {
                let cf = {
                    let state = swarm_state.lock().await;
                    match state.get(&original_index) {
                        Some(cu) => cu.cf.clone(),
                        None => {
                            eprintln!("Unit {} is not connected", original_index);
                            return;
                        }
                    }
                };

                match cf.platform.send_crash_recovery_request().await {
                    Ok(()) => {
                        eprintln!("Crash recovery sent for unit {}", original_index);
                    }
                    Err(e) => {
                        eprintln!("Crash recovery failed for unit {}: {:?}", original_index, e);
                    }
                }
            });
        });
    }

    // Reboot single unit
    {
        let link_context = link_context.clone();
        let swarm_state = swarm_state.clone();
        let ui_weak = ui.as_weak();
        ui.on_reboot_unit(move |row_index| {
            if row_index < 0 { return; }

            let (original_index, uri) = {
                let Some(ui) = ui_weak.upgrade() else { return };
                let units = ui.get_units();
                let col = ui.get_sort_column();
                let ascending = ui.get_sort_ascending();
                let indices = sort_unit_indices(&units, col, ascending);
                let row = row_index as usize;
                if row >= indices.len() { return; }
                let idx = indices[row];
                let Some(unit) = units.row_data(idx) else { return };
                (idx, unit.uri.to_string())
            };

            let link_context = link_context.clone();
            let swarm_state = swarm_state.clone();
            let ui_weak = ui_weak.clone();

            tokio::spawn(async move {
                eprintln!("Rebooting unit {} ({}) ...", original_index, uri);
                if let Err(e) = send_bootloader_command(&link_context, &uri, BOOTLOADER_CMD_RESET_INIT, None).await {
                    eprintln!("Reboot reset-init failed for {}: {:?}", uri, e);
                }
                if let Err(e) = send_bootloader_command(&link_context, &uri, BOOTLOADER_CMD_RESET, Some(&[0x01])).await {
                    eprintln!("Reboot reset failed for {}: {:?}", uri, e);
                }

                // Disconnect the unit
                let connected = {
                    let mut state = swarm_state.lock().await;
                    state.remove(&original_index)
                };
                if let Some(cu) = connected {
                    cu.cf.disconnect().await;
                }
                update_unit(&ui_weak, original_index, |u| {
                    u.state = UnitState::Disconnected;
                    u.pos_x = 0.0; u.pos_y = 0.0; u.pos_z = 0.0;
                    u.battery_voltage = 0.0; u.link_quality = 0.0;
                    u.pm_state = "".into(); u.serial = "".into();
                    u.platform_type = "".into(); u.firmware_version = "".into();
                    u.journal_entry_count = 0;
                });
            });
        });
    }

    // Power off single unit
    {
        let link_context = link_context.clone();
        let swarm_state = swarm_state.clone();
        let ui_weak = ui.as_weak();
        ui.on_power_off_unit(move |row_index| {
            if row_index < 0 { return; }

            let (original_index, uri) = {
                let Some(ui) = ui_weak.upgrade() else { return };
                let units = ui.get_units();
                let col = ui.get_sort_column();
                let ascending = ui.get_sort_ascending();
                let indices = sort_unit_indices(&units, col, ascending);
                let row = row_index as usize;
                if row >= indices.len() { return; }
                let idx = indices[row];
                let Some(unit) = units.row_data(idx) else { return };
                (idx, unit.uri.to_string())
            };

            let link_context = link_context.clone();
            let swarm_state = swarm_state.clone();
            let ui_weak = ui_weak.clone();

            tokio::spawn(async move {
                eprintln!("Powering off unit {} ({}) ...", original_index, uri);
                if let Err(e) = send_bootloader_command(&link_context, &uri, BOOTLOADER_CMD_ALL_OFF, None).await {
                    eprintln!("Power off failed for {}: {:?}", uri, e);
                }

                // Disconnect the unit
                let connected = {
                    let mut state = swarm_state.lock().await;
                    state.remove(&original_index)
                };
                if let Some(cu) = connected {
                    cu.cf.disconnect().await;
                }
                update_unit(&ui_weak, original_index, |u| {
                    u.state = UnitState::Disconnected;
                    u.pos_x = 0.0; u.pos_y = 0.0; u.pos_z = 0.0;
                    u.battery_voltage = 0.0; u.link_quality = 0.0;
                    u.pm_state = "".into(); u.serial = "".into();
                    u.platform_type = "".into(); u.firmware_version = "".into();
                    u.journal_entry_count = 0;
                });
            });
        });
    }

    // SysOff single unit (sleep, do NOT disconnect)
    {
        let link_context = link_context.clone();
        let ui_weak = ui.as_weak();
        ui.on_sysoff_unit(move |row_index| {
            if row_index < 0 { return; }

            let (original_index, uri) = {
                let Some(ui) = ui_weak.upgrade() else { return };
                let units = ui.get_units();
                let col = ui.get_sort_column();
                let ascending = ui.get_sort_ascending();
                let indices = sort_unit_indices(&units, col, ascending);
                let row = row_index as usize;
                if row >= indices.len() { return; }
                let idx = indices[row];
                let Some(unit) = units.row_data(idx) else { return };
                (idx, unit.uri.to_string())
            };

            let link_context = link_context.clone();

            tokio::spawn(async move {
                eprintln!("Sending sysoff (sleep) to unit {} ({}) ...", original_index, uri);
                if let Err(e) = send_bootloader_command(&link_context, &uri, BOOTLOADER_CMD_SYS_OFF, None).await {
                    eprintln!("SysOff failed for {}: {:?}", uri, e);
                }
            });
        });
    }

    // SysOn single unit (wake, do NOT disconnect)
    {
        let link_context = link_context.clone();
        let ui_weak = ui.as_weak();
        ui.on_syson_unit(move |row_index| {
            if row_index < 0 { return; }

            let (_original_index, uri) = {
                let Some(ui) = ui_weak.upgrade() else { return };
                let units = ui.get_units();
                let col = ui.get_sort_column();
                let ascending = ui.get_sort_ascending();
                let indices = sort_unit_indices(&units, col, ascending);
                let row = row_index as usize;
                if row >= indices.len() { return; }
                let idx = indices[row];
                let Some(unit) = units.row_data(idx) else { return };
                (idx, unit.uri.to_string())
            };

            let link_context = link_context.clone();

            tokio::spawn(async move {
                eprintln!("Sending syson (wake) to {} ...", uri);
                if let Err(e) = send_bootloader_command(&link_context, &uri, BOOTLOADER_CMD_SYS_ON, None).await {
                    eprintln!("SysOn failed for {}: {:?}", uri, e);
                }
            });
        });
    }

    // Open swarm config file
    {
        let swarm_state = swarm_state.clone();
        let positioning_data = positioning_data.clone();
        let ui_weak = ui.as_weak();

        ui.on_open_swarm_requested(move || {
            let swarm_state = swarm_state.clone();
            let positioning_data = positioning_data.clone();
            let ui_weak = ui_weak.clone();

            tokio::spawn(async move {
                // Open file dialog
                let Some(handle) = rfd::AsyncFileDialog::new()
                    .add_filter("YAML", &["yaml", "yml"])
                    .pick_file()
                    .await
                else { return };
                let path = handle.path().to_path_buf();

                // Parse the config
                let config = match load_swarm_config(&path) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("{}", e);
                        return;
                    }
                };

                // Remember the selected config path (preserve tuning settings)
                let mut settings: AppSettings = confy::load("swarmkeeper", None).unwrap_or_default();
                settings.last_swarm_config = Some(path.to_string_lossy().into_owned());
                if let Err(e) = confy::store("swarmkeeper", None, &settings) {
                    eprintln!("Failed to save settings: {}", e);
                }

                // Disconnect all connected units
                let units: Vec<(usize, ConnectedUnit)> = {
                    let mut state = swarm_state.lock().await;
                    state.drain().collect()
                };
                for (index, connected) in units {
                    eprintln!("Disconnecting unit {} ...", index);
                    connected.cf.disconnect().await;
                }

                // Clear positioning data
                {
                    let mut pd = positioning_data.lock().await;
                    *pd = PositioningData::default();
                }

                // Apply new config on UI thread
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        apply_swarm_config(&ui, &config);
                    }
                })
                .ok();
            });
        });
    }

    // LH Coverage shared state (Mutex for thread-safe access from rendering notifier)
    struct CoverageState {
        base_stations: Vec<coverage::BaseStation>,
        voxels: Vec<(f32, f32, f32, u8)>,
        room: [f32; 3],
        room_offset: [f32; 3],
        bs_render_data: Vec<([f32; 3], [[f32; 3]; 3])>,
        undo_stack: Vec<Vec<coverage::BaseStation>>,
        trajectories: Vec<Vec<[f32; 3]>>,
    }
    let coverage_state = std::sync::Arc::new(std::sync::Mutex::new(CoverageState {
        base_stations: Vec::new(),
        voxels: Vec::new(),
        room: [8.0, 8.0, 3.0],
        room_offset: [0.0; 3],
        bs_render_data: Vec::new(),
        undo_stack: Vec::new(),
        trajectories: Vec::new(),
    }));

    struct GizmoState {
        drag_start_screen: [f32; 2],
        drag_start_pos: [f32; 3],
        drag_start_azimuth: f32,
        drag_start_elevation: f32,
    }
    let gizmo_state = std::sync::Arc::new(std::sync::Mutex::new(GizmoState {
        drag_start_screen: [0.0, 0.0],
        drag_start_pos: [0.0; 3],
        drag_start_azimuth: 0.0,
        drag_start_elevation: 0.0,
    }));

    // TDoA3 Coverage shared state
    struct Tdoa3State {
        anchors: Vec<tdoa3::Anchor>,
        voxels: Vec<(f32, f32, f32, f32)>,
        anchor_positions: Vec<[f32; 3]>,
        room: [f32; 3],
        room_offset: [f32; 3],
        undo_stack: Vec<Vec<tdoa3::Anchor>>,
        gdop_result: Option<tdoa3::GdopResult>,
        convex_hull: Option<tdoa3::ConvexHull>,
    }
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

    struct Tdoa3GizmoState {
        drag_start_screen: [f32; 2],
        drag_start_pos: [f32; 3],
    }
    let tdoa3_gizmo_state = std::sync::Arc::new(std::sync::Mutex::new(Tdoa3GizmoState {
        drag_start_screen: [0.0, 0.0],
        drag_start_pos: [0.0; 3],
    }));

    // Planning shared state
    struct PlanningState {
        base_stations: Vec<coverage::BaseStation>,
        anchors: Vec<tdoa3::Anchor>,
        obstacles: Vec<planning::Obstacle>,
        lh_voxels: Vec<(f32, f32, f32, u8)>,
        tdoa3_voxels: Vec<(f32, f32, f32, f32)>,
        tdoa3_gdop_result: Option<tdoa3::GdopResult>,
        bs_render_data: Vec<([f32; 3], [[f32; 3]; 3])>,
        anchor_positions: Vec<[f32; 3]>,
        obstacle_triangles: Vec<Vec<f32>>,
        obstacle_wireframes: Vec<Vec<f32>>,
        obstacle_colors: Vec<[f32; 3]>,
        room: [f32; 3],
        room_offset: [f32; 3],
        undo_stack: Vec<(Vec<coverage::BaseStation>, Vec<tdoa3::Anchor>, Vec<planning::Obstacle>)>,
    }
    let planning_state = std::sync::Arc::new(std::sync::Mutex::new(PlanningState {
        base_stations: Vec::new(),
        anchors: Vec::new(),
        obstacles: Vec::new(),
        lh_voxels: Vec::new(),
        tdoa3_voxels: Vec::new(),
        tdoa3_gdop_result: None,
        bs_render_data: Vec::new(),
        anchor_positions: Vec::new(),
        obstacle_triangles: Vec::new(),
        obstacle_wireframes: Vec::new(),
        obstacle_colors: Vec::new(),
        room: [8.0, 8.0, 3.0],
        room_offset: [0.0; 3],
        undo_stack: Vec::new(),
    }));

    struct PlanningGizmoState {
        drag_start_screen: [f32; 2],
        drag_start_pos: [f32; 3],
        drag_start_azimuth: f32,
        drag_start_elevation: f32,
        drag_start_yaw: f32,
    }
    let planning_gizmo_state = std::sync::Arc::new(std::sync::Mutex::new(PlanningGizmoState {
        drag_start_screen: [0.0, 0.0],
        drag_start_pos: [0.0; 3],
        drag_start_azimuth: 0.0,
        drag_start_elevation: 0.0,
        drag_start_yaw: 0.0,
    }));

    // LH Wizard shared state
    let wizard_state = Arc::new(std::sync::Mutex::new(lh_wizard::LhWizardState::new()));

    // Set up OpenGL 3D rendering
    {
        let app_weak = ui.as_weak();
        let positioning_data = positioning_data.clone();
        let trajectory_data = trajectory_data.clone();
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

                            // Read unit positions from model
                            let units_model = app.get_units();
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
                                    });
                                }
                            }

                            // Build fixed points from positioning data
                            let mut fixed_points = Vec::new();
                            if let Ok(pd) = positioning_data.try_lock() {
                                for (id, pos) in pd.lighthouse_bs.iter() {
                                    let active = (pd.lighthouse_active >> id) & 1 != 0;
                                    let alpha = if active { 1.0 } else { 0.5 };
                                    fixed_points.push(renderer::UnitPos {
                                        x: pos[0], y: pos[1], z: pos[2],
                                        color: [0.94 * alpha, 0.27 * alpha, 0.27 * alpha],
                                    });
                                }
                                // Track which loco anchors are currently present
                                let mut loco_present: std::collections::HashSet<u8> = std::collections::HashSet::new();
                                for (id, pos) in pd.loco_anchors.iter() {
                                    loco_present.insert(*id);
                                    fixed_points.push(renderer::UnitPos {
                                        x: pos[0], y: pos[1], z: pos[2],
                                        color: [1.0, 0.85, 0.0],
                                    });
                                }
                                // Show previously seen anchors that are no longer present at 50%
                                for (id, pos) in pd.loco_seen.iter() {
                                    if !loco_present.contains(id) {
                                        let alpha = 0.5;
                                        fixed_points.push(renderer::UnitPos {
                                            x: pos[0], y: pos[1], z: pos[2],
                                            color: [1.0 * alpha, 0.85 * alpha, 0.0],
                                        });
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

                            let texture = renderer.render(
                                width, height, yaw, pitch, distance, pan_x, pan_y, &unit_positions, &fixed_points, &trajectory_lines,
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

                                    let (room, room_offset, bs_render, lh_voxels, anchor_positions, tdoa3_voxels, obstacle_tris, obstacle_wires, obstacle_cols) = {
                                        let ps = planning_state_render.lock().unwrap();
                                        (ps.room, ps.room_offset, ps.bs_render_data.clone(),
                                         ps.lh_voxels.clone(), ps.anchor_positions.clone(),
                                         ps.tdoa3_voxels.clone(),
                                         ps.obstacle_triangles.clone(),
                                         ps.obstacle_wireframes.clone(),
                                         ps.obstacle_colors.clone())
                                    };

                                    let show_lh_coverage = [
                                        app.get_planning_show_coverage_0(),
                                        app.get_planning_show_coverage_1(),
                                        app.get_planning_show_coverage_2(),
                                        app.get_planning_show_coverage_3(),
                                        app.get_planning_show_coverage_4(),
                                    ];

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

    // Radio channel test
    {
        let link_context = link_context.clone();
        let swarm_state = swarm_state.clone();
        let ui_weak = ui.as_weak();

        ui.on_run_radio_test(move |unit_index| {
            let link_context = link_context.clone();
            let swarm_state = swarm_state.clone();
            let ui_weak = ui_weak.clone();

            let Some(ui_ref) = ui_weak.upgrade() else { return };
            let units = ui_ref.get_units();
            let Some(unit) = units.row_data(unit_index as usize) else { return };
            let uri = unit.uri.to_string();

            ui_ref.set_radio_test_running(true);
            ui_ref.set_radio_test_progress(0.0);
            ui_ref.set_radio_test_status("Starting...".into());
            ui_ref.set_radio_test_results(slint::ModelRc::new(slint::VecModel::from(Vec::<ChannelResult>::new())));

            tokio::spawn(async move {
                run_radio_channel_test(uri, swarm_state, unit_index as usize, ui_weak).await;
            });
        });
    }

    // Set tuning parameters on all units (works with or without swarm connected)
    {
        let swarm_state = swarm_state.clone();
        let link_context = link_context.clone();
        let toc_cache = toc_cache.clone();
        let ui_weak = ui.as_weak();
        ui.on_set_tuning_params(move |thrust_base, vx_ki, vy_ki, prop_threshold, prop_pwm_ratio| {
            let thrust_base: u16 = match thrust_base.trim().parse() {
                Ok(v) => v,
                Err(e) => { eprintln!("Invalid thrustBase value '{}': {}", thrust_base, e); return; }
            };
            let vx_ki: f32 = match vx_ki.trim().parse() {
                Ok(v) => v,
                Err(e) => { eprintln!("Invalid vxKi value '{}': {}", vx_ki, e); return; }
            };
            let vy_ki: f32 = match vy_ki.trim().parse() {
                Ok(v) => v,
                Err(e) => { eprintln!("Invalid vyKi value '{}': {}", vy_ki, e); return; }
            };
            let prop_threshold: f32 = match prop_threshold.trim().parse() {
                Ok(v) => v,
                Err(e) => { eprintln!("Invalid propTestThreshold value '{}': {}", prop_threshold, e); return; }
            };
            let prop_pwm_ratio: u16 = match prop_pwm_ratio.trim().parse() {
                Ok(v) => v,
                Err(e) => { eprintln!("Invalid propTestPWMRatio value '{}': {}", prop_pwm_ratio, e); return; }
            };

            // Persist the tuning values locally
            let mut settings: AppSettings = confy::load("swarmkeeper", None).unwrap_or_default();
            settings.tuning_thrust_base = Some(thrust_base);
            settings.tuning_vx_ki = Some(vx_ki);
            settings.tuning_vy_ki = Some(vy_ki);
            settings.tuning_prop_test_threshold = Some(prop_threshold);
            settings.tuning_prop_test_pwm_ratio = Some(prop_pwm_ratio);
            if let Err(e) = confy::store("swarmkeeper", None, &settings) {
                eprintln!("Failed to save tuning settings: {}", e);
            }

            // Update UI properties
            if let Some(ui_ref) = ui_weak.upgrade() {
                ui_ref.set_tuning_thrust_base(thrust_base.to_string().into());
                ui_ref.set_tuning_vx_ki(vx_ki.to_string().into());
                ui_ref.set_tuning_vy_ki(vy_ki.to_string().into());
                ui_ref.set_tuning_prop_test_threshold(prop_threshold.to_string().into());
                ui_ref.set_tuning_prop_test_pwm_ratio(prop_pwm_ratio.to_string().into());
            }

            // Collect unit URIs and names from the UI
            let Some(ui_ref) = ui_weak.upgrade() else { return };
            let units = ui_ref.get_units();
            let unit_count = units.row_count();
            let mut unit_info: Vec<(String, String)> = Vec::new();
            for i in 0..unit_count {
                if let Some(unit) = units.row_data(i) {
                    unit_info.push((unit.uri.to_string(), unit.name.to_string()));
                }
            }
            let total = unit_info.len();
            if total == 0 {
                eprintln!("No units configured for tuning");
                return;
            }

            // Show progress dialog
            ui_ref.set_progress_dialog_title("Setting Tuning Parameters".into());
            ui_ref.set_progress_dialog_progress(0.0);
            ui_ref.set_progress_dialog_status("Starting...".into());
            ui_ref.set_progress_dialog_visible(true);

            let swarm_state = swarm_state.clone();
            let link_context = link_context.clone();
            let toc_cache = toc_cache.clone();
            let ui_weak = ui_weak.clone();

            tokio::spawn(async move {
                // Check if units are already connected
                let connected_units: HashMap<usize, Arc<crazyflie_lib::Crazyflie>> = {
                    let state = swarm_state.lock().await;
                    state.iter().map(|(idx, cu)| (*idx, cu.cf.clone())).collect()
                };
                let swarm_connected = !connected_units.is_empty();
                eprintln!("Tuning: {} units, swarm connected: {}", total, swarm_connected);

                let completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));

                let mut join_set = tokio::task::JoinSet::new();
                for (idx, (uri, name)) in unit_info.into_iter().enumerate() {
                    let ui_weak = ui_weak.clone();
                    let completed = completed.clone();
                    let existing_cf = connected_units.get(&idx).cloned();
                    let link_context = link_context.clone();
                    let toc_cache = toc_cache.clone();

                    join_set.spawn(async move {
                        let params: [(&str, f64); 5] = [
                            ("posCtlPid.thrustBase", thrust_base as f64),
                            ("velCtlPid.vxKi", vx_ki as f64),
                            ("velCtlPid.vyKi", vy_ki as f64),
                            ("health.propTestThreshold", prop_threshold as f64),
                            ("health.propTestPWMRatio", prop_pwm_ratio as f64),
                        ];

                        // Use existing connection or connect temporarily
                        let (cf, temp_connection) = if let Some(cf) = existing_cf {
                            eprintln!("Tuning {}: using existing connection", name);
                            (cf, false)
                        } else {
                            eprintln!("Tuning {}: connecting to {}...", name, uri);
                            match tokio::time::timeout(
                                std::time::Duration::from_secs(30),
                                crazyflie_lib::Crazyflie::connect_from_uri(link_context.as_ref(), &uri, toc_cache),
                            ).await {
                                Ok(Ok(cf)) => (Arc::new(cf), true),
                                Ok(Err(e)) => {
                                    eprintln!("Tuning {}: connect FAILED: {:?}", name, e);
                                    let done = completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                                    let progress = done as f32 / total as f32;
                                    let status: slint::SharedString = format!("{} failed ({}/{})", name, done, total).into();
                                    let ui_weak_inner = ui_weak.clone();
                                    slint::invoke_from_event_loop(move || {
                                        if let Some(ui) = ui_weak_inner.upgrade() {
                                            ui.set_progress_dialog_progress(progress);
                                            ui.set_progress_dialog_status(status);
                                        }
                                    }).ok();
                                    return;
                                }
                                Err(_) => {
                                    eprintln!("Tuning {}: connect timed out", name);
                                    let done = completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                                    let progress = done as f32 / total as f32;
                                    let status: slint::SharedString = format!("{} timed out ({}/{})", name, done, total).into();
                                    let ui_weak_inner = ui_weak.clone();
                                    slint::invoke_from_event_loop(move || {
                                        if let Some(ui) = ui_weak_inner.upgrade() {
                                            ui.set_progress_dialog_progress(progress);
                                            ui.set_progress_dialog_status(status);
                                        }
                                    }).ok();
                                    return;
                                }
                            }
                        };

                        for (param_name, value) in &params {
                            match cf.param.set_lossy(param_name, *value).await {
                                Ok(()) => eprintln!("  {} {} = {} OK", name, param_name, value),
                                Err(e) => eprintln!("  {} {} set FAILED: {:?}", name, param_name, e),
                            }
                            match cf.param.persistent_store(param_name).await {
                                Ok(()) => eprintln!("  {} {} stored OK", name, param_name),
                                Err(e) => eprintln!("  {} {} persistent_store FAILED: {:?}", name, param_name, e),
                            }
                        }

                        if temp_connection {
                            cf.disconnect().await;
                            eprintln!("Tuning {}: disconnected", name);
                        }

                        let done = completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                        let progress = done as f32 / total as f32;
                        let status: slint::SharedString = format!("Done ({}/{})", done, total).into();
                        let ui_weak_inner = ui_weak.clone();
                        slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_weak_inner.upgrade() {
                                ui.set_progress_dialog_progress(progress);
                                ui.set_progress_dialog_status(status);
                            }
                        }).ok();
                    });
                }

                while join_set.join_next().await.is_some() {}

                eprintln!("All tuning params set, closing dialog");
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;

                let ui_weak_inner = ui_weak.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak_inner.upgrade() {
                        ui.set_progress_dialog_visible(false);
                    }
                }).ok();
            });
        });
    }

    // Health test handler
    {
        let link_context = link_context.clone();
        let toc_cache = toc_cache.clone();
        let ui_weak = ui.as_weak();
        ui.on_run_health_test(move || {
            let Some(ui_ref) = ui_weak.upgrade() else { return };
            let units = ui_ref.get_units();
            let unit_count = units.row_count();
            let mut unit_info: Vec<(String, String)> = Vec::new();
            for i in 0..unit_count {
                if let Some(unit) = units.row_data(i) {
                    unit_info.push((unit.uri.to_string(), unit.name.to_string()));
                }
            }
            let total = unit_info.len();
            if total == 0 {
                eprintln!("No units configured for health test");
                return;
            }

            // Initialize results
            let initial_results: Vec<HealthTestResult> = unit_info
                .iter()
                .map(|(_, name)| HealthTestResult {
                    name: name.clone().into(),
                    status: "Waiting".into(),
                    m1_pass: false,
                    m2_pass: false,
                    m3_pass: false,
                    m4_pass: false,
                })
                .collect();
            let results_model = std::rc::Rc::new(slint::VecModel::from(initial_results));
            ui_ref.set_health_test_results(results_model.clone().into());
            ui_ref.set_health_test_progress(0.0);
            ui_ref.set_health_test_status("Starting...".into());
            ui_ref.set_health_test_running(true);

            let link_context = link_context.clone();
            let toc_cache = toc_cache.clone();
            let ui_weak = ui_weak.clone();

            tokio::spawn(async move {
                let completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));

                let mut join_set = tokio::task::JoinSet::new();
                for (idx, (uri, name)) in unit_info.into_iter().enumerate() {
                    let ui_weak = ui_weak.clone();
                    let completed = completed.clone();
                    let link_context = link_context.clone();
                    let toc_cache = toc_cache.clone();

                    join_set.spawn(async move {
                        // Update status to "Connecting"
                        {
                            let ui_weak = ui_weak.clone();
                            slint::invoke_from_event_loop(move || {
                                if let Some(ui) = ui_weak.upgrade() {
                                    let results = ui.get_health_test_results();
                                    if let Some(mut r) = results.row_data(idx) {
                                        r.status = "Connecting".into();
                                        results.set_row_data(idx, r);
                                    }
                                }
                            }).ok();
                        }

                        eprintln!("Health test {}: connecting to {}...", name, uri);
                        let cf = match tokio::time::timeout(
                            std::time::Duration::from_secs(30),
                            crazyflie_lib::Crazyflie::connect_from_uri(link_context.as_ref(), &uri, toc_cache),
                        ).await {
                            Ok(Ok(cf)) => Arc::new(cf),
                            Ok(Err(e)) => {
                                eprintln!("Health test {}: connect FAILED: {:?}", name, e);
                                let done = completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                                let progress = done as f32 / total as f32;
                                let ui_weak = ui_weak.clone();
                                slint::invoke_from_event_loop(move || {
                                    if let Some(ui) = ui_weak.upgrade() {
                                        let results = ui.get_health_test_results();
                                        if let Some(mut r) = results.row_data(idx) {
                                            r.status = "Error".into();
                                            results.set_row_data(idx, r);
                                        }
                                        ui.set_health_test_progress(progress);
                                        ui.set_health_test_status(format!("{}/{} done", done, total).into());
                                    }
                                }).ok();
                                return;
                            }
                            Err(_) => {
                                eprintln!("Health test {}: connect timed out", name);
                                let done = completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                                let progress = done as f32 / total as f32;
                                let ui_weak = ui_weak.clone();
                                slint::invoke_from_event_loop(move || {
                                    if let Some(ui) = ui_weak.upgrade() {
                                        let results = ui.get_health_test_results();
                                        if let Some(mut r) = results.row_data(idx) {
                                            r.status = "Error".into();
                                            results.set_row_data(idx, r);
                                        }
                                        ui.set_health_test_progress(progress);
                                        ui.set_health_test_status(format!("{}/{} done", done, total).into());
                                    }
                                }).ok();
                                return;
                            }
                        };

                        // Update status to "Testing"
                        {
                            let ui_weak = ui_weak.clone();
                            slint::invoke_from_event_loop(move || {
                                if let Some(ui) = ui_weak.upgrade() {
                                    let results = ui.get_health_test_results();
                                    if let Some(mut r) = results.row_data(idx) {
                                        r.status = "Testing".into();
                                        results.set_row_data(idx, r);
                                    }
                                }
                            }).ok();
                        }

                        // Create log block for health.motorPass BEFORE starting the test
                        let motor_pass_result = async {
                            let mut log_block = cf.log.create_block().await?;
                            log_block.add_variable("health.motorPass").await?;
                            let period = crazyflie_lib::subsystems::log::LogPeriod::from_millis(100)?;
                            let log_stream = log_block.start(period).await?;

                            // Now start the prop test
                            cf.param.set("health.startPropTest", 1u8).await
                                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                                    Box::new(std::io::Error::new(std::io::ErrorKind::Other, format!("{:?}", e)))
                                })?;
                            eprintln!("Health test {}: prop test started", name);

                            let result = tokio::time::timeout(
                                std::time::Duration::from_secs(15),
                                async {
                                    loop {
                                        let data = log_stream.next().await?;
                                        eprintln!("Health test {}: log data: {:?}", name, data.data);
                                        let motor_pass: u8 = data
                                            .data
                                            .get("health.motorPass")
                                            .and_then(|v| (*v).try_into().ok())
                                            .unwrap_or(0);
                                        if motor_pass != 0 {
                                            return Ok::<u8, Box<dyn std::error::Error + Send + Sync>>(motor_pass);
                                        }
                                    }
                                },
                            )
                            .await;

                            match result {
                                Ok(Ok(motor_pass)) => Ok(Some(motor_pass)),
                                Ok(Err(e)) => Err(e),
                                Err(_) => Ok(None), // timeout
                            }
                        }
                        .await;

                        cf.disconnect().await;
                        eprintln!("Health test {}: disconnected", name);

                        let done = completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                        let progress = done as f32 / total as f32;

                        let (status, m1, m2, m3, m4) = match motor_pass_result {
                            Ok(Some(motor_pass)) => {
                                let m1 = motor_pass & 0x01 != 0;
                                let m2 = motor_pass & 0x02 != 0;
                                let m3 = motor_pass & 0x04 != 0;
                                let m4 = motor_pass & 0x08 != 0;
                                let status = if motor_pass & 0x0F == 0x0F { "Pass" } else { "Fail" };
                                eprintln!("Health test {}: {} (motorPass=0x{:02X})", name, status, motor_pass);
                                (status, m1, m2, m3, m4)
                            }
                            Ok(None) => {
                                eprintln!("Health test {}: timeout (15s)", name);
                                ("Timeout", false, false, false, false)
                            }
                            Err(e) => {
                                eprintln!("Health test {}: log error: {:?}", name, e);
                                ("Error", false, false, false, false)
                            }
                        };

                        let ui_weak = ui_weak.clone();
                        let status: slint::SharedString = status.into();
                        slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_weak.upgrade() {
                                let results = ui.get_health_test_results();
                                if let Some(mut r) = results.row_data(idx) {
                                    r.status = status;
                                    r.m1_pass = m1;
                                    r.m2_pass = m2;
                                    r.m3_pass = m3;
                                    r.m4_pass = m4;
                                    results.set_row_data(idx, r);
                                }
                                ui.set_health_test_progress(progress);
                                ui.set_health_test_status(format!("{}/{} done", done, total).into());
                            }
                        }).ok();
                    });
                }

                while join_set.join_next().await.is_some() {}

                eprintln!("All health tests complete");
                let ui_weak_inner = ui_weak.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak_inner.upgrade() {
                        ui.set_health_test_running(false);
                        ui.set_health_test_status("Complete".into());
                    }
                }).ok();
            });
        });
    }

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
                let Some(handle) = rfd::AsyncFileDialog::new()
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
                let Some(handle) = rfd::AsyncFileDialog::new()
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
                let Some(handle) = rfd::AsyncFileDialog::new()
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
                let Some(handle) = rfd::AsyncFileDialog::new()
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
                let Some(handle) = rfd::AsyncFileDialog::new()
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
                let Some(handle) = rfd::AsyncFileDialog::new()
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
                let Some(handle) = rfd::AsyncFileDialog::new()
                    .set_title("Import LH Scene")
                    .add_filter("YAML", &["yaml", "yml"])
                    .set_directory(std::env::current_dir().unwrap_or_default())
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
                let Some(handle) = rfd::AsyncFileDialog::new()
                    .set_title("Import TDoA3 Scene")
                    .add_filter("YAML", &["yaml", "yml"])
                    .set_directory(std::env::current_dir().unwrap_or_default())
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

        // Map error metric index to DOP metric and name
        fn error_metric_to_dop(idx: i32) -> (usize, &'static str) {
            match idx {
                1 => (tdoa3::METRIC_HDOP, "HERR"),
                2 => (tdoa3::METRIC_VDOP, "VERR"),
                _ => (tdoa3::METRIC_GDOP, "GERR"),
            }
        }

        // Extract tdoa3 voxels for the selected error metric, scaled by sigma
        fn extract_tdoa3_voxels(ps: &mut PlanningState, metric_idx: i32, sigma: f32) {
            if let Some(ref result) = ps.tdoa3_gdop_result {
                let (dop_metric, _) = error_metric_to_dop(metric_idx);
                ps.tdoa3_voxels = result.iter_voxels(ps.room_offset, dop_metric).collect();
                for v in &mut ps.tdoa3_voxels {
                    v.3 *= sigma;
                }
            }
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
        ui.on_planning_save_scene(move || {
            let Some(ui) = uw.upgrade() else { return };
            let uw2 = ui.as_weak();
            let ps = ps.clone();
            slint::spawn_local(async move {
                let Some(handle) = rfd::AsyncFileDialog::new()
                    .add_filter("YAML", &["yaml", "yml"])
                    .set_file_name("planning.yaml")
                    .set_directory(std::env::current_dir().unwrap_or_default())
                    .save_file().await
                else { return };
                let path = handle.path().to_path_buf();
                let Some(ui) = uw2.upgrade() else { return };

                let pstate = ps.lock().unwrap();
                let scene = planning::PlanningScene::new(
                    ui.get_planning_room_x().parse().unwrap_or(8.0),
                    ui.get_planning_room_y().parse().unwrap_or(8.0),
                    ui.get_planning_room_z().parse().unwrap_or(3.0),
                    ui.get_planning_resolution().parse().unwrap_or(5.0),
                    ui.get_planning_center_origin(),
                    &pstate.base_stations,
                    &pstate.anchors,
                    &pstate.obstacles,
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
                );
                if let Err(e) = planning::save_scene(&path, &scene) {
                    eprintln!("Failed to save planning scene: {}", e);
                }
            }).unwrap();
        });

        // Load scene
        let ps = planning_state.clone();
        let uw = ui_weak.clone();
        ui.on_planning_load_scene(move || {
            let Some(ui) = uw.upgrade() else { return };
            let uw2 = ui.as_weak();
            let ps = ps.clone();
            slint::spawn_local(async move {
                let Some(handle) = rfd::AsyncFileDialog::new()
                    .add_filter("YAML", &["yaml", "yml"])
                    .set_directory(std::env::current_dir().unwrap_or_default())
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
                        rebuild_bs_render_data(&mut ps);
                        rebuild_anchor_positions(&mut ps);
                        rebuild_obstacle_meshes(&mut ps);
                        ps.undo_stack.clear();

                        ui.set_planning_room_x(format!("{}", scene.room_x).into());
                        ui.set_planning_room_y(format!("{}", scene.room_y).into());
                        ui.set_planning_room_z(format!("{}", scene.room_z).into());
                        ui.set_planning_resolution(format!("{}", scene.resolution).into());
                        ui.set_planning_center_origin(scene.center_origin);
                        ui.set_planning_receiver_fov_enabled(scene.receiver_fov_enabled);
                        ui.set_planning_max_bs_distance(format!("{}", scene.max_bs_distance).into());
                        ui.set_planning_show_coverage_0(scene.show_coverage[0]);
                        ui.set_planning_show_coverage_1(scene.show_coverage[1]);
                        ui.set_planning_show_coverage_2(scene.show_coverage[2]);
                        ui.set_planning_show_coverage_3(scene.show_coverage[3]);
                        ui.set_planning_show_coverage_4(scene.show_coverage[4]);
                        ui.set_planning_max_range(format!("{}", scene.max_range).into());

                        // Compute coverage
                        let offset = if scene.center_origin {
                            [-scene.room_x / 2.0, -scene.room_y / 2.0, 0.0]
                        } else {
                            [0.0, 0.0, 0.0]
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
        let uw = ui_weak.clone();
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
            let (lh_voxels, lh_ratio_1, lh_ratio_2, tdoa3_result, gdop_mean, hdop_mean, vdop_mean, hull_stats, offset, room) = result;

            let Some(ui) = uw_timer.upgrade() else { return };
            let sigma: f32 = ui.get_planning_measurement_noise().parse().unwrap_or(0.15);
            let metric_idx = ui.get_planning_error_metric_index();

            let mut pstate = ps_timer.lock().unwrap();
            pstate.room = room;
            pstate.room_offset = offset;
            pstate.lh_voxels = lh_voxels;
            pstate.tdoa3_gdop_result = Some(tdoa3_result);
            extract_tdoa3_voxels(&mut pstate, metric_idx, sigma);
            drop(pstate);

            ui.set_planning_stats_text_1(
                format!("LH: {:.0}% ≥1 BS, {:.0}% ≥2 BS", lh_ratio_1 * 100.0, lh_ratio_2 * 100.0).into()
            );
            ui.set_planning_stats_text_2(
                format!("Room  GERR={:.1} HERR={:.1} VERR={:.1} cm",
                    gdop_mean * sigma * 100.0, hdop_mean * sigma * 100.0, vdop_mean * sigma * 100.0).into()
            );
            if let Some((g, h, v)) = hull_stats {
                ui.set_planning_stats_text_3(
                    format!("Hull  GERR={:.1} HERR={:.1} VERR={:.1} cm",
                        g * sigma * 100.0, h * sigma * 100.0, v * sigma * 100.0).into()
                );
            } else {
                ui.set_planning_stats_text_3("".into());
            }

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

            let offset = if center {
                [-room_x / 2.0, -room_y / 2.0, 0.0]
            } else {
                [0.0, 0.0, 0.0]
            };

            let (base_stations, anchors, obstacles) = {
                let pstate = ps.lock().unwrap();
                (pstate.base_stations.clone(), pstate.anchors.clone(), pstate.obstacles.clone())
            };

            let (tx, rx) = std::sync::mpsc::channel();
            *compute_rx.borrow_mut() = Some(rx);

            std::thread::spawn(move || {
                let lh_result = planning::compute_coverage_with_obstacles(
                    room_x, room_y, room_z, resolution,
                    &base_stations, 160.0, 115.0,
                    receiver_fov, None, max_bs_dist, offset,
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

        // Error metric changed (switch GERR/HERR/VERR without recomputing)
        let ps = planning_state.clone();
        let uw = ui_weak.clone();
        ui.on_planning_error_metric_changed(move |idx| {
            let Some(ui) = uw.upgrade() else { return };
            let sigma: f32 = ui.get_planning_measurement_noise().parse().unwrap_or(0.15);
            let mut ps = ps.lock().unwrap();
            extract_tdoa3_voxels(&mut ps, idx, sigma);
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
        let uw = ui_weak.clone();
        ui.on_planning_view_mouse_released(move || {
            let Some(ui) = uw.upgrade() else { return };
            ui.set_planning_handle_active(false);
            ui.set_planning_active_handle(0);
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

    // LH Wizard callbacks
    {
        let ui_weak = ui.as_weak();
        let ws = wizard_state.clone();
        let uw = ui_weak.clone();
        ui.on_lh_wizard_step_next(move || {
            let mut ws = ws.lock().unwrap();
            let next = (ws.current_step as i32 + 1).min(4);
            ws.current_step = lh_wizard::WizardStep::from_index(next);
            if let Some(ui) = uw.upgrade() {
                ui.set_lh_wizard_current_step(next);
                ui.set_lh_wizard_step_instructions(ws.current_step.instructions().into());
                ui.set_lh_wizard_measure_button_text(ws.current_step.button_text().into());
            }
        });

        let ws = wizard_state.clone();
        let uw = ui_weak.clone();
        ui.on_lh_wizard_step_prev(move || {
            let mut ws = ws.lock().unwrap();
            let prev = (ws.current_step as i32 - 1).max(0);
            ws.current_step = lh_wizard::WizardStep::from_index(prev);
            if let Some(ui) = uw.upgrade() {
                ui.set_lh_wizard_current_step(prev);
                ui.set_lh_wizard_step_instructions(ws.current_step.instructions().into());
                ui.set_lh_wizard_measure_button_text(ws.current_step.button_text().into());
            }
        });

        let ws = wizard_state.clone();
        let uw = ui_weak.clone();
        ui.on_lh_wizard_clear(move || {
            let mut ws = ws.lock().unwrap();
            ws.container.clear_all_samples();
            ws.latest_solution = None;
            ws.current_step = lh_wizard::WizardStep::Origin;
            if let Some(ui) = uw.upgrade() {
                ui.set_lh_wizard_current_step(0);
                ui.set_lh_wizard_step_instructions(lh_wizard::WizardStep::Origin.instructions().into());
                ui.set_lh_wizard_measure_button_text(lh_wizard::WizardStep::Origin.button_text().into());
                ui.set_lh_wizard_origin_ok(false);
                ui.set_lh_wizard_x_axis_ok(false);
                ui.set_lh_wizard_xy_plane_ok(false);
                ui.set_lh_wizard_has_converged(false);
                ui.set_lh_wizard_error_mean("".into());
                ui.set_lh_wizard_error_max("".into());
                ui.set_lh_wizard_progress_info("".into());
                ui.set_lh_wizard_notification_text("Samples cleared".into());
                ui.set_lh_wizard_notification_color(slint::Color::from_argb_u8(255, 255, 235, 59));
                ui.set_lh_wizard_xy_plane_count(0);
                ui.set_lh_wizard_xyz_space_count(0);
                ui.set_lh_wizard_verification_count(0);
                ui.set_lh_wizard_sample_details(Default::default());
                ui.set_lh_wizard_bs_details(Default::default());
            }
        });

        let ws = wizard_state.clone();
        let uw = ui_weak.clone();
        let ss = swarm_state.clone();
        ui.on_lh_wizard_measure(move || {
            let ws = ws.clone();
            let uw = uw.clone();
            let ss = ss.clone();

            // Get selected CF index and current step
            let (selected_cf, current_step) = {
                let ws_lock = ws.lock().unwrap();
                (
                    if let Some(ui) = uw.upgrade() { ui.get_lh_wizard_selected_cf() } else { -1 },
                    ws_lock.current_step,
                )
            };

            if selected_cf < 0 {
                if let Some(ui) = uw.upgrade() {
                    ui.set_lh_wizard_notification_text("No Crazyflie selected".into());
                    ui.set_lh_wizard_notification_color(slint::Color::from_argb_u8(255, 255, 183, 77));
                }
                return;
            }

            // Set measuring state
            if let Some(ui) = uw.upgrade() {
                ui.set_lh_wizard_measuring(true);
                ui.set_lh_wizard_notification_text("Collecting angles...".into());
                ui.set_lh_wizard_notification_color(slint::Color::from_argb_u8(255, 255, 235, 59));
            }

            let unit_index = selected_cf as usize;

            tokio::spawn(async move {
                // Get CF from swarm state
                let cf = {
                    let state = ss.lock().await;
                    state.get(&unit_index).map(|u| u.cf.clone())
                };

                let Some(cf) = cf else {
                    slint::invoke_from_event_loop(move || {
                        if let Some(ui) = uw.upgrade() {
                            ui.set_lh_wizard_measuring(false);
                            ui.set_lh_wizard_notification_text("Crazyflie not connected".into());
                            ui.set_lh_wizard_notification_color(slint::Color::from_argb_u8(255, 244, 67, 54));
                        }
                    }).ok();
                    return;
                };

                // Enable angle stream
                if let Err(e) = cf.param.set("locSrv.enLhAngleStream", 1u8).await {
                    eprintln!("Failed to enable angle stream: {}", e);
                    slint::invoke_from_event_loop(move || {
                        if let Some(ui) = uw.upgrade() {
                            ui.set_lh_wizard_measuring(false);
                            ui.set_lh_wizard_notification_text("Failed to enable angle stream".into());
                            ui.set_lh_wizard_notification_color(slint::Color::from_argb_u8(255, 244, 67, 54));
                        }
                    }).ok();
                    return;
                };

                // Collect angle samples (average 50 readings per BS)
                use futures::StreamExt;
                let mut angle_stream = cf.localization.lighthouse.angle_stream().await;
                let mut bs_samples: std::collections::HashMap<u8, Vec<([f32; 4], [f32; 4])>> = std::collections::HashMap::new();
                let samples_needed = 50;
                let timeout = tokio::time::Duration::from_secs(3);

                let result = tokio::time::timeout(timeout, async {
                    loop {
                        let all_have_enough = !bs_samples.is_empty() &&
                            bs_samples.values().all(|v| v.len() >= samples_needed);
                        if all_have_enough {
                            break;
                        }

                        if let Some(data) = angle_stream.next().await {
                            bs_samples.entry(data.base_station)
                                .or_default()
                                .push((data.angles.x, data.angles.y));
                        }
                    }
                }).await;

                // Disable angle stream
                let _ = cf.param.set("locSrv.enLhAngleStream", 0u8).await;

                // Debug: print collection results
                eprintln!("[LH Wizard] Angle collection done. Timeout: {}, BS count: {}", result.is_err(), bs_samples.len());
                for (bs_id, samples) in &bs_samples {
                    eprintln!("[LH Wizard]   BS {}: {} samples", bs_id, samples.len());
                    if let Some(first) = samples.first() {
                        eprintln!("[LH Wizard]     first x: {:?}", first.0);
                        eprintln!("[LH Wizard]     first y: {:?}", first.1);
                    }
                }

                if result.is_err() && bs_samples.is_empty() {
                    slint::invoke_from_event_loop(move || {
                        if let Some(ui) = uw.upgrade() {
                            ui.set_lh_wizard_measuring(false);
                            ui.set_lh_wizard_notification_text("Timeout - no angle data received".into());
                            ui.set_lh_wizard_notification_color(slint::Color::from_argb_u8(255, 244, 67, 54));
                        }
                    }).ok();
                    return;
                }

                // Average the collected angles and create a sample
                let mut averaged_angles = std::collections::HashMap::new();
                let bs_count = bs_samples.len();
                for (bs_id, samples) in &bs_samples {
                    let n = samples.len() as f64;
                    let mut avg_x = [0.0f64; 4];
                    let mut avg_y = [0.0f64; 4];
                    for (x, y) in samples {
                        for s in 0..4 {
                            avg_x[s] += x[s] as f64;
                            avg_y[s] += y[s] as f64;
                        }
                    }
                    for s in 0..4 {
                        avg_x[s] /= n;
                        avg_y[s] /= n;
                    }

                    // Convert to LighthouseBsVectors (V2 angles → V1 via from_lh2)
                    use crate::lh_geo::bs_vector::LighthouseBsVector;
                    let vectors = [
                        LighthouseBsVector::from_lh2(avg_x[0], avg_y[0]),
                        LighthouseBsVector::from_lh2(avg_x[1], avg_y[1]),
                        LighthouseBsVector::from_lh2(avg_x[2], avg_y[2]),
                        LighthouseBsVector::from_lh2(avg_x[3], avg_y[3]),
                    ];
                    averaged_angles.insert(*bs_id, vectors);
                }

                let sample = crate::lh_geo::sample::LhCfPoseSample::new(averaged_angles);
                eprintln!("[LH Wizard] Created sample with {} BS, uid={}", sample.angles_calibrated.len(), sample.uid());

                // Add sample to container based on current step
                {
                    let ws_lock = ws.lock().unwrap();
                    eprintln!("[LH Wizard] Adding sample as {:?}, container version before: {}", current_step, ws_lock.container.get_data_version());
                    match current_step {
                        lh_wizard::WizardStep::Origin => ws_lock.container.set_origin_sample(sample),
                        lh_wizard::WizardStep::XAxis => ws_lock.container.set_x_axis_sample(sample),
                        lh_wizard::WizardStep::XyPlane => ws_lock.container.append_xy_plane_sample(sample),
                        lh_wizard::WizardStep::XyzSpace => ws_lock.container.append_xyz_space_samples(vec![sample]),
                        lh_wizard::WizardStep::Verification => ws_lock.container.append_verification_samples(vec![sample]),
                    }
                }

                // Run solver in background
                let solution = {
                    let ws_lock = ws.lock().unwrap();
                    lh_wizard::run_solver(&ws_lock.container)
                };

                eprintln!("[LH Wizard] Solver done. converged={}, bs_poses={}, progress_ok={}, progress_info={}",
                    solution.has_converged, solution.bs_poses.len(), solution.progress_is_ok, solution.progress_info);

                let has_converged = solution.has_converged;
                let sample_details = lh_wizard::get_sample_details(&solution);
                let bs_details = lh_wizard::get_bs_details(&solution);
                let error_stats = solution.error_stats.clone();
                let origin_ok = solution.is_origin_sample_valid;
                let origin_info = solution.origin_sample_info.clone();
                let x_axis_ok = solution.is_x_axis_samples_valid;
                let x_axis_info = solution.x_axis_samples_info.clone();
                let xy_plane_ok = solution.is_xy_plane_samples_valid;
                let xy_plane_info = solution.xy_plane_samples_info.clone();
                let progress_info = solution.progress_info.clone();

                // Count samples by type
                let mut xy_count = 0i32;
                let mut xyz_count = 0i32;
                let mut verif_count = 0i32;
                for s in &solution.samples {
                    match s.sample_type {
                        crate::lh_geo::sample::LhCfPoseSampleType::XyPlane => xy_count += 1,
                        crate::lh_geo::sample::LhCfPoseSampleType::XyzSpace => xyz_count += 1,
                        crate::lh_geo::sample::LhCfPoseSampleType::Verification => verif_count += 1,
                        _ => {}
                    }
                }

                // Store solution
                ws.lock().unwrap().latest_solution = Some(solution);

                // Update UI
                let step_name = format!("{:?}", current_step);
                slint::invoke_from_event_loop(move || {
                    let Some(ui) = uw.upgrade() else { return };
                    ui.set_lh_wizard_measuring(false);
                    ui.set_lh_wizard_notification_text(
                        format!("{} measured ({} BS, {} samples avg)", step_name, bs_count,
                            samples_needed.min(bs_samples.values().map(|v| v.len()).min().unwrap_or(0))).into()
                    );
                    ui.set_lh_wizard_notification_color(slint::Color::from_argb_u8(255, 129, 199, 132));

                    // Update solution status
                    ui.set_lh_wizard_origin_ok(origin_ok);
                    ui.set_lh_wizard_origin_status(origin_info.into());
                    ui.set_lh_wizard_x_axis_ok(x_axis_ok);
                    ui.set_lh_wizard_x_axis_status(x_axis_info.into());
                    ui.set_lh_wizard_xy_plane_ok(xy_plane_ok);
                    ui.set_lh_wizard_xy_plane_status(xy_plane_info.into());
                    ui.set_lh_wizard_has_converged(has_converged);
                    ui.set_lh_wizard_progress_info(progress_info.into());
                    ui.set_lh_wizard_xy_plane_count(xy_count);
                    ui.set_lh_wizard_xyz_space_count(xyz_count);
                    ui.set_lh_wizard_verification_count(verif_count);

                    if let Some(stats) = &error_stats {
                        ui.set_lh_wizard_error_mean(format!("{:.4}m", stats.mean).into());
                        ui.set_lh_wizard_error_max(format!("{:.4}m", stats.max).into());
                    }

                    // Update sample details
                    let sd: Vec<WizardSampleData> = sample_details.iter().map(|s| WizardSampleData {
                        sample_type: s.sample_type.clone().into(),
                        x: s.x.clone().into(),
                        y: s.y.clone().into(),
                        z: s.z.clone().into(),
                        error: s.error.clone().into(),
                        is_verification: s.is_verification,
                        is_invalid: s.is_invalid,
                        is_large_error: s.is_large_error,
                    }).collect();
                    ui.set_lh_wizard_sample_details(slint::ModelRc::new(slint::VecModel::from(sd)));

                    // Update BS details
                    let bd: Vec<WizardBsData> = bs_details.iter().map(|b| WizardBsData {
                        id: b.id,
                        x: b.x.clone().into(),
                        y: b.y.clone().into(),
                        z: b.z.clone().into(),
                        samples: b.samples,
                        links: b.links,
                        low_links: b.low_links,
                    }).collect();
                    ui.set_lh_wizard_bs_details(slint::ModelRc::new(slint::VecModel::from(bd)));
                }).ok();
            });
        });

        // Import/Export session
        let ws = wizard_state.clone();
        let uw = ui_weak.clone();
        ui.on_lh_wizard_import(move || {
            let ws = ws.clone();
            let uw = uw.clone();
            slint::spawn_local(async move {
                let Some(handle) = rfd::AsyncFileDialog::new()
                    .add_filter("YAML", &["yaml", "yml"])
                    .pick_file().await
                else { return };
                let path = handle.path().to_path_buf();

                match std::fs::read_to_string(&path) {
                    Ok(yaml_str) => {
                        let ws_lock = ws.lock().unwrap();
                        match ws_lock.container.load_from_yaml(&yaml_str) {
                            Ok(()) => {
                                if let Some(ui) = uw.upgrade() {
                                    ui.set_lh_wizard_notification_text("Session imported".into());
                                    ui.set_lh_wizard_notification_color(slint::Color::from_argb_u8(255, 129, 199, 132));
                                }
                            }
                            Err(e) => {
                                eprintln!("Failed to import session: {}", e);
                            }
                        }
                    }
                    Err(e) => eprintln!("Failed to read file: {}", e),
                }
            }).unwrap();
        });

        let ws = wizard_state.clone();
        ui.on_lh_wizard_export(move || {
            let ws = ws.clone();
            slint::spawn_local(async move {
                let Some(handle) = rfd::AsyncFileDialog::new()
                    .add_filter("YAML", &["yaml", "yml"])
                    .set_file_name("lh_geo_session.yaml")
                    .save_file().await
                else { return };
                let path = handle.path().to_path_buf();

                let ws_lock = ws.lock().unwrap();
                match ws_lock.container.save_to_yaml() {
                    Ok(yaml_str) => {
                        if let Err(e) = std::fs::write(&path, yaml_str) {
                            eprintln!("Failed to write file: {}", e);
                        }
                    }
                    Err(e) => eprintln!("Failed to serialize: {}", e),
                }
            }).unwrap();
        });

        // Upload geometry - placeholder
        ui.on_lh_wizard_upload(move || {
            // TODO: Upload geometry to connected CFs
        });

        // CF selection - placeholder
        ui.on_lh_wizard_select_cf(move |_idx| {
            // TODO: Select which CF to use for measurements
        });

        // View interaction callbacks (camera rotation via mouse drag)
        let uw = ui_weak.clone();
        ui.on_lh_wizard_view_mouse_pressed(move |_x, _y| {
            // Start camera rotation
        });

        let uw = ui_weak.clone();
        ui.on_lh_wizard_view_mouse_moved(move |x, y| {
            // Camera rotation while dragging
            // The Slint touch area handles basic yaw/pitch updates
        });

        ui.on_lh_wizard_view_mouse_released(move || {});

        let uw = ui_weak.clone();
        ui.on_lh_wizard_view_pan(move |dx_px, dy_px| {
            let Some(ui) = uw.upgrade() else { return };
            let yaw = ui.get_lh_wizard_cam_yaw();
            let sy = yaw.sin();
            let cy = yaw.cos();
            let dx = dx_px * 0.01;
            let dy = dy_px * 0.01;
            ui.set_lh_wizard_cam_pan_x(
                ui.get_lh_wizard_cam_pan_x() + dx * sy + dy * cy,
            );
            ui.set_lh_wizard_cam_pan_y(
                ui.get_lh_wizard_cam_pan_y() + dx * (-cy) + dy * sy,
            );
        });
    }

    ui.run().expect("Failed to run UI");
}

fn parse_radio_uri(uri: &str) -> Option<(usize, u8, [u8; 5])> {
    // Parse radio://N/CH/RATE/ADDR
    let uri = uri.strip_prefix("radio://")?;
    let parts: Vec<&str> = uri.splitn(2, '?').collect(); // strip query params
    let path = parts[0];
    let segments: Vec<&str> = path.split('/').collect();
    if segments.len() != 4 {
        return None;
    }
    let radio_nth: usize = segments[0].parse().ok()?;
    let channel: u8 = segments[1].parse().ok()?;
    // segments[2] = rate (not needed)
    let addr_str = segments[3];

    // Pad to 10 hex chars and parse
    let padded = format!("{:0>10}", addr_str);
    let mut address = [0u8; 5];
    for i in 0..5 {
        address[i] = u8::from_str_radix(&padded[i * 2..i * 2 + 2], 16).ok()?;
    }

    Some((radio_nth, channel, address))
}

async fn run_radio_channel_test(
    uri: String,
    swarm_state: SwarmState,
    unit_index: usize,
    ui_weak: slint::Weak<AppWindow>,
) {
    let (radio_nth, original_channel, address) = match parse_radio_uri(&uri) {
        Some(v) => v,
        None => {
            eprintln!("Radio test: failed to parse URI: {}", uri);
            let ui_weak = ui_weak.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_radio_test_running(false);
                    ui.set_radio_test_status("Error: invalid URI".into());
                }
            }).ok();
            return;
        }
    };

    // Disconnect all Crazyflies to ensure a fresh radio with RSSI support
    {
        let mut state = swarm_state.lock().await;
        let indices: Vec<usize> = state.keys().cloned().collect();
        let had_connections = !indices.is_empty();
        for idx in indices {
            if let Some(connected) = state.remove(&idx) {
                eprintln!("Radio test: disconnecting unit {} ...", idx);
                connected.cf.disconnect().await;
                update_unit(&ui_weak, idx, |u| {
                    u.state = UnitState::Disconnected;
                    u.pos_x = 0.0;
                    u.pos_y = 0.0;
                    u.pos_z = 0.0;
                    u.battery_voltage = 0.0;
                    u.link_quality = 0.0;
                    u.deck_lighthouse = false;
                    u.deck_loco = false;
                    u.deck_led_top = false;
                    u.deck_led_bottom = false;
                    u.serial = "".into();
                    u.pm_state = "".into();
                    u.supervisor_info = 0;
                    u.supervisor_state = "".into();
                    u.journal_entry_count = 0;
                    u.platform_type = "".into();
                    u.firmware_version = "".into();
                });
            }
        }
        // Brief delay to let all radio connections fully close
        if had_connections {
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        }
    }

    // Open the Crazyradio directly (bypassing crazyflie-link) to ensure
    // a fresh radio with InlineMode::OnWithRssi set by reset()
    let mut radio = match crazyradio::Crazyradio::open_nth_async(radio_nth).await {
        Ok(r) => crazyradio::SharedCrazyradio::new(r),
        Err(e) => {
            eprintln!("Radio test: failed to open radio: {:?}", e);
            let ui_weak = ui_weak.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_radio_test_running(false);
                    ui.set_radio_test_status("Error: could not open radio".into());
                }
            }).ok();
            return;
        }
    };

    const NUM_CHANNELS: u8 = 81;
    const PACKETS_PER_CHANNEL: usize = 20;
    const SET_RADIO_CHANNEL: u8 = 0x01;
    let mut results: Vec<ChannelResult> = vec![ChannelResult::default(); NUM_CHANNELS as usize];

    // Push the initial empty results so all bars are in place from the start
    {
        let ui_weak_init = ui_weak.clone();
        let results_init = results.clone();
        slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_weak_init.upgrade() {
                ui.set_radio_test_results(slint::ModelRc::new(slint::VecModel::from(results_init)));
            }
        }).ok();
    }

    let orig_channel = match crazyradio::Channel::from_number(original_channel) {
        Ok(c) => c,
        Err(_) => {
            eprintln!("Radio test: invalid original channel {}", original_channel);
            let ui_weak = ui_weak.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_radio_test_running(false);
                    ui.set_radio_test_status("Error: invalid channel in URI".into());
                }
            }).ok();
            return;
        }
    };

    // Disable safelink on the Crazyflie. After a crazyflie-link connection,
    // the NRF firmware stays in safelink mode and drops packets without
    // correct safelink counter bits. This command is handled before counter
    // validation so it always gets through.
    for _ in 0..10 {
        let _ = radio.send_packet_async(orig_channel, address, vec![0xff, 0x05, 0x00]).await;
    }

    for ch in 0..NUM_CHANNELS {
        let channel = match crazyradio::Channel::from_number(ch) {
            Ok(c) => c,
            Err(_) => continue,
        };

        // Tell the Crazyflie to switch to the test channel
        // Send on the Crazyflie's current channel (original for first iteration,
        // previous test channel for subsequent ones)
        let cmd_channel = if ch == 0 { orig_channel } else {
            crazyradio::Channel::from_number(ch - 1).unwrap_or(orig_channel)
        };
        for _ in 0..50 {
            let _ = radio.send_packet_async(cmd_channel, address, vec![0xff, 0x03, SET_RADIO_CHANNEL, ch]).await;
        }

        // Now test on the new channel
        let mut ack_count = 0usize;
        let mut rssi_sum = 0u64;
        let mut rssi_count = 0usize;

        for _ in 0..PACKETS_PER_CHANNEL {
            match radio.send_packet_async(channel, address, vec![0xff]).await {
                Ok((ack, _data)) => {
                    if ack.received {
                        ack_count += 1;
                        if let Some(rssi) = ack.rssi_dbm {
                            rssi_sum += rssi as u64;
                            rssi_count += 1;
                        }
                    }
                }
                Err(_) => {}
            }
        }

        let ack_rate = ack_count as f32 / PACKETS_PER_CHANNEL as f32;
        let avg_rssi = if rssi_count > 0 {
            -(rssi_sum as f32 / rssi_count as f32)
        } else {
            0.0
        };

        results[ch as usize] = ChannelResult {
            ack_rate: ack_rate,
            rssi: avg_rssi,
        };

        // Update progress
        let progress = (ch as f32 + 1.0) / NUM_CHANNELS as f32;
        let status: slint::SharedString = format!("Testing channel {}/{}...", ch + 1, NUM_CHANNELS).into();
        let ui_weak_inner = ui_weak.clone();
        let results_snapshot = results.clone();
        slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_weak_inner.upgrade() {
                ui.set_radio_test_progress(progress);
                ui.set_radio_test_status(status);
                ui.set_radio_test_results(slint::ModelRc::new(slint::VecModel::from(results_snapshot)));
            }
        }).ok();
    }

    // Restore the Crazyflie back to its original channel
    let last_tested = crazyradio::Channel::from_number(NUM_CHANNELS - 1).unwrap_or(orig_channel);
    for _ in 0..50 {
        let _ = radio.send_packet_async(last_tested, address, vec![0xff, 0x03, SET_RADIO_CHANNEL, original_channel]).await;
    }

    // Done
    let ui_weak_inner = ui_weak.clone();
    let final_status: slint::SharedString = "Complete".into();
    slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak_inner.upgrade() {
            ui.set_radio_test_running(false);
            ui.set_radio_test_status(final_status);
            ui.set_radio_test_results(slint::ModelRc::new(slint::VecModel::from(results)));
        }
    }).ok();
}

async fn start_telemetry(
    index: usize,
    uri: String,
    cf: Arc<crazyflie_lib::Crazyflie>,
    ui_weak: slint::Weak<AppWindow>,
    positioning_data: SharedPositioningData,
    positioning_source: Arc<Mutex<Option<usize>>>,
) {
    let mut log_block = match cf.log.create_block().await {
        Ok(block) => block,
        Err(e) => {
            eprintln!("Failed to create log block for {}: {:?}", uri, e);
            let error_msg = format!("{}", e);
            update_unit(&ui_weak, index, move |u| {
                u.state = UnitState::Error;
                u.error_message = error_msg.into();
            });
            return;
        }
    };

    if log_block.add_variable("stateEstimate.x").await.is_err()
        || log_block.add_variable("stateEstimate.y").await.is_err()
        || log_block.add_variable("stateEstimate.z").await.is_err()
        || log_block.add_variable("pm.vbat").await.is_err()
        || log_block.add_variable("pm.state").await.is_err()
    {
        eprintln!("Failed to add log variables for {}", uri);
        update_unit(&ui_weak, index, |u| {
            u.state = UnitState::Error;
            u.error_message = "Failed to add log variables".into();
        });
        return;
    }

    // Optional supervisor and positioning status variables (may not exist on all firmware)
    let has_supervisor_info = log_block.add_variable("supervisor.info").await.is_ok();
    let has_lh_active = log_block.add_variable("lighthouse.bsActive").await.is_ok();
    let has_ranging_state = log_block.add_variable("ranging.state").await.is_ok();
    eprintln!("Telemetry {}: has_supervisor_info={}, has_lh_active={}, has_ranging_state={}", uri, has_supervisor_info, has_lh_active, has_ranging_state);

    let period = match crazyflie_lib::subsystems::log::LogPeriod::from_millis(100) {
        Ok(p) => p,
        Err(_) => return,
    };

    let log_stream = match log_block.start(period).await {
        Ok(stream) => stream,
        Err(e) => {
            eprintln!("Failed to start log stream for {}: {:?}", uri, e);
            let error_msg = format!("{}", e);
            update_unit(&ui_weak, index, move |u| {
                u.state = UnitState::Error;
                u.error_message = error_msg.into();
            });
            return;
        }
    };

    loop {
        let data = match log_stream.next().await {
            Ok(d) => d,
            Err(_) => {
                eprintln!("Log stream error for {} (index {}): disconnected or error", uri, index);
                break
            },
        };

        let x: f32 = data
            .data
            .get("stateEstimate.x")
            .and_then(|v| (*v).try_into().ok())
            .unwrap_or(0.0);
        let y: f32 = data
            .data
            .get("stateEstimate.y")
            .and_then(|v| (*v).try_into().ok())
            .unwrap_or(0.0);
        let z: f32 = data
            .data
            .get("stateEstimate.z")
            .and_then(|v| (*v).try_into().ok())
            .unwrap_or(0.0);
        let vbat: f32 = data
            .data
            .get("pm.vbat")
            .and_then(|v| (*v).try_into().ok())
            .unwrap_or(0.0);
        let pm_state: i8 = data
            .data
            .get("pm.state")
            .and_then(|v| (*v).try_into().ok())
            .unwrap_or(-1);
        let pm_state_str = pm_state_text(pm_state);

        let supervisor_info: u16 = if has_supervisor_info {
            data.data
                .get("supervisor.info")
                .and_then(|v| (*v).try_into().ok())
                .unwrap_or(0)
        } else {
            0
        };

        // Derive unit state from supervisor bitfield
        let unit_state = if has_supervisor_info {
            if supervisor_info & 0x0080 != 0 {
                UnitState::Crashed
            } else if supervisor_info & 0x0010 != 0 {
                UnitState::Flying
            } else if pm_state == 1 {
                UnitState::Charging
            } else if pm_state == 2 {
                UnitState::Charged
            } else {
                UnitState::Connected
            }
        } else if pm_state == 1 {
            UnitState::Charging
        } else if pm_state == 2 {
            UnitState::Charged
        } else {
            UnitState::Connected
        };

        // Read positioning active status if this is the positioning source unit
        let is_source = {
            if let Ok(ps) = positioning_source.try_lock() {
                *ps == Some(index)
            } else {
                false
            }
        };
        if is_source {
            let lh_active: u16 = if has_lh_active {
                data.data.get("lighthouse.bsActive")
                    .and_then(|v| (*v).try_into().ok())
                    .unwrap_or(0)
            } else {
                0
            };
            let ranging_active: u16 = if has_ranging_state {
                let raw_val = data.data.get("ranging.state");
                let val: u16 = raw_val
                    .and_then(|v| (*v).try_into().ok())
                    .unwrap_or(0);
                if val != 0 {
                    eprintln!("ranging.state = {} (raw: {:?})", val, raw_val);
                }
                val
            } else {
                0
            };
            if let Ok(mut pd) = positioning_data.try_lock() {
                pd.lighthouse_active = lh_active;
                pd.loco_active = ranging_active;
            }
        }

        let supervisor_state_str = supervisor_text(supervisor_info as i32);

        let stats = cf.link_service.get_statistics().await;
        let link_quality = stats.link_quality.unwrap_or(0.0);
        let uplink_rate = stats.uplink_rate.unwrap_or(0.0);
        let downlink_rate = stats.downlink_rate.unwrap_or(0.0);
        let radio_send_rate = stats.radio_send_rate.unwrap_or(0.0);
        let avg_retries = stats.avg_retries.unwrap_or(0.0);
        let rssi = stats.rssi.unwrap_or(0.0);
        let has_rssi = stats.rssi.is_some();

        update_unit(&ui_weak, index, move |u| {
            u.pos_x = x;
            u.pos_y = y;
            u.pos_z = z;
            u.battery_voltage = vbat;
            u.link_quality = link_quality;
            u.uplink_rate = uplink_rate;
            u.downlink_rate = downlink_rate;
            u.radio_send_rate = radio_send_rate;
            u.avg_retries = avg_retries;
            u.rssi = rssi;
            u.has_rssi = has_rssi;
            u.pm_state = pm_state_str.into();
            u.supervisor_info = supervisor_info as i32;
            u.supervisor_state = supervisor_state_str.into();
            u.state = unit_state;
        });
    }

    // Connection lost
    update_unit(&ui_weak, index, |u| {
        u.state = UnitState::Disconnected;
        u.pos_x = 0.0;
        u.pos_y = 0.0;
        u.pos_z = 0.0;
        u.battery_voltage = 0.0;
        u.link_quality = 0.0;
        u.uplink_rate = 0.0;
        u.downlink_rate = 0.0;
        u.radio_send_rate = 0.0;
        u.avg_retries = 0.0;
        u.rssi = 0.0;
        u.has_rssi = false;
        u.deck_lighthouse = false;
        u.deck_loco = false;
        u.deck_led_top = false;
        u.deck_led_bottom = false;
        u.serial = "".into();
        u.pm_state = "".into();
        u.supervisor_info = 0;
        u.supervisor_state = "".into();
        u.journal_entry_count = 0;
        u.platform_type = "".into();
        u.firmware_version = "".into();
    });
}
