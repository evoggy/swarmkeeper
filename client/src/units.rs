//! The unit table: rows, sorting and the selected unit.

use slint::{ComponentHandle, Model};

use crate::app::AppContext;
use crate::swarm::SwarmState;
use crate::{AppWindow, FilterCounts, Health, SwarmSummary, UnitData, UnitRow, UnitState};
use crate::host;

pub(crate) fn pm_state_text(state: i8) -> &'static str {
    match state {
        0 => "Battery",
        1 => "Charging",
        2 => "Charged",
        3 => "Low power",
        4 => "Shutdown",
        _ => "Unknown",
    }
}

pub(crate) fn supervisor_text(info: i32) -> String {
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

pub(crate) fn sort_unit_indices(units: &slint::ModelRc<UnitData>, col: i32, ascending: bool) -> Vec<usize> {
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
            15 => ua.radio.cmp(&ub.radio),
            16 => ua.firmware_version.to_string().cmp(&ub.firmware_version.to_string()),
            _ => std::cmp::Ordering::Equal,
        };
        if ascending { ord } else { ord.reverse() }
    });
    indices
}

/// Below this a unit on battery should land.
const LOW_BATTERY_V: f32 = 3.6;
/// Below this link quality a unit is flagged.
const WEAK_LINK: f32 = 0.7;
/// Supervisor bits that keep a unit from flying until it is recovered.
const SUPERVISOR_CRASHED: i32 = 0x0080;
const SUPERVISOR_LOCKED: i32 = 0x0040;
const SUPERVISOR_TUMBLED: i32 = 0x0020;

/// The charge of a LiPo cell at rest, roughly, from its voltage.
fn battery_level(voltage: f32) -> f32 {
    ((voltage - 3.4) / (4.15 - 3.4)).clamp(0.0, 1.0)
}

fn connected(u: &UnitData) -> bool {
    !matches!(u.state, UnitState::Disconnected | UnitState::Error)
}

/// How a unit is doing, what to call it, the reason, and a short word for
/// the Needs attention tile.
fn classify(u: &UnitData, firmware: &str) -> (Health, String, String, &'static str) {
    match u.state {
        UnitState::Disconnected => return (Health::Off, "Not connected".into(), String::new(), ""),
        UnitState::Error => {
            let timeout = u.error_message.to_lowercase().contains("timed out")
                || u.error_message.to_lowercase().contains("timeout");
            let status = if timeout { "Timed out" } else { "Connection failed" };
            return (Health::Fault, status.into(), u.error_message.to_string(), if timeout { "timeout" } else { "error" });
        }
        _ => {}
    }
    if u.supervisor_info & SUPERVISOR_TUMBLED != 0 || u.state == UnitState::Crashed {
        return (Health::Fault, "Tumbled".into(), "Recover it to fly again".into(), "tumbled");
    }
    if u.supervisor_info & (SUPERVISOR_LOCKED | SUPERVISOR_CRASHED) != 0 {
        return (Health::Fault, "Locked".into(), "Recover it to fly again".into(), "locked");
    }
    if !u.selftest_passed {
        return (Health::Fault, "Selftest failed".into(), "See the console".into(), "selftest");
    }
    match u.state {
        UnitState::Charging => return (Health::Charging, "Charging".into(), String::new(), ""),
        UnitState::Charged => return (Health::Charged, "Charged".into(), String::new(), ""),
        UnitState::Flying => return (Health::Flying, "Flying".into(), String::new(), ""),
        _ => {}
    }
    if u.battery_voltage > 0.0 && u.battery_voltage < LOW_BATTERY_V {
        return (Health::Warn, "Low battery".into(), "Land and charge".into(), "low battery");
    }
    if u.link_quality < WEAK_LINK {
        return (
            Health::Warn,
            "Weak link".into(),
            format!("{:.0} % of packets get through", u.link_quality * 100.0),
            "weak link",
        );
    }
    if !firmware.is_empty() && !u.firmware_version.is_empty() && u.firmware_version != firmware {
        return (Health::Warn, "Other firmware".into(), format!("The rest run {}", firmware), "firmware");
    }
    (Health::Ready, "Ready to fly".into(), String::new(), "")
}

/// "D91F700117 · ch 84" from a radio URI, else the URI without options.
fn short_address(uri: &str) -> String {
    let uri = uri.split('?').next().unwrap_or(uri);
    if let Some(rest) = uri.strip_prefix("radio://") {
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() >= 4 {
            return format!("{} · ch {}", parts[3], parts[1]);
        }
    }
    uri.to_string()
}

/// Whether a row passes the filter chip (`filter`) and the search text.
fn shown(u: &UnitData, health: Health, filter: i32, search: &str) -> bool {
    let chip = match filter {
        1 => matches!(health, Health::Ready | Health::Flying),
        2 => matches!(health, Health::Charging | Health::Charged),
        3 => matches!(health, Health::Warn | Health::Fault),
        4 => !connected(u),
        _ => true,
    };
    chip && (search.is_empty()
        || u.name.to_lowercase().contains(search)
        || u.uri.to_lowercase().contains(search))
}

/// Rebuild what the Units screen (and the Status screen) show from the
/// `units` model: the sorted list, each row's status and place in the
/// filtered table, the filter counts and the swarm summary.
pub(crate) fn rebuild_table_rows(ui: &AppWindow) {
    let units = ui.get_units();
    let all: Vec<UnitData> = units.iter().collect();
    let indices = sort_unit_indices(&units, ui.get_sort_column(), ui.get_sort_ascending());
    let filter = ui.get_unit_filter();
    let search = ui.get_unit_search().to_lowercase();

    // The firmware most connected units run.
    let mut versions: Vec<(String, usize)> = Vec::new();
    for u in all.iter().filter(|u| connected(u) && !u.firmware_version.is_empty()) {
        match versions.iter_mut().find(|(v, _)| v.as_str() == u.firmware_version.as_str()) {
            Some((_, n)) => *n += 1,
            None => versions.push((u.firmware_version.to_string(), 1)),
        }
    }
    let firmware = versions.iter().max_by_key(|(_, n)| *n).map(|(v, _)| v.clone()).unwrap_or_default();

    let mut summary = SwarmSummary { total: all.len() as i32, firmware: firmware.clone().into(), ..Default::default() };
    let mut counts = FilterCounts { all: all.len() as i32, ..Default::default() };
    let mut reasons: Vec<&'static str> = Vec::new();
    let mut lowest: Option<(f32, String)> = None;

    let mut rows = Vec::with_capacity(indices.len());
    let mut sorted = Vec::with_capacity(indices.len());
    let mut slot = 0;
    for &i in &indices {
        let u = &all[i];
        let (health, status, detail, reason) = classify(u, &firmware);
        let on = connected(u);
        if on {
            summary.connected += 1;
        } else {
            summary.disconnected += 1;
            counts.disconnected += 1;
        }
        match health {
            Health::Ready => {
                summary.ready += 1;
                counts.ready += 1;
            }
            Health::Flying => {
                summary.flying += 1;
                counts.ready += 1;
            }
            Health::Charging => summary.charging += 1,
            Health::Charged => summary.charged += 1,
            Health::Warn | Health::Fault => {
                summary.attention += 1;
                counts.attention += 1;
                if !reasons.contains(&reason) {
                    reasons.push(reason);
                }
            }
            Health::Off => {}
        }
        if on && !matches!(u.state, UnitState::Charging | UnitState::Charged) {
            summary.on_battery += 1;
            if u.battery_voltage > 0.0 && lowest.as_ref().is_none_or(|(v, _)| u.battery_voltage < *v) {
                lowest = Some((u.battery_voltage, u.name.to_string()));
            }
        }
        if !firmware.is_empty() && on && !u.firmware_version.is_empty() && u.firmware_version != firmware.as_str() {
            summary.firmware_differs += 1;
        }

        let visible = shown(u, health, filter, &search);
        rows.push(UnitRow {
            slot: if visible { slot } else { -1 },
            health,
            status: status.into(),
            detail: detail.into(),
            address: short_address(&u.uri).into(),
            battery: if on && u.battery_voltage > 0.0 { format!("{:.2} V", u.battery_voltage).into() } else { "".into() },
            battery_level: battery_level(u.battery_voltage),
            link: if on { format!("{:.0} %", u.link_quality * 100.0).into() } else { "".into() },
            position: if on {
                format!("{:6.2} {:6.2} {:6.2}", u.pos_x, u.pos_y, u.pos_z).into()
            } else {
                "–".into()
            },
            radio: if u.radio >= 0 { format!("Radio {}", u.radio).into() } else { "–".into() },
            firmware_differs: on && !firmware.is_empty() && !u.firmware_version.is_empty() && u.firmware_version != firmware.as_str(),
        });
        if visible {
            slot += 1;
        }
        sorted.push(u.clone());
    }
    counts.chargers = summary.charging + summary.charged;
    summary.attention_text = reasons.join(", ").into();
    if let Some((voltage, name)) = lowest {
        summary.lowest_battery = format!("{:.2}", voltage).into();
        summary.lowest_battery_unit = name.into();
        summary.lowest_battery_level = battery_level(voltage);
    }

    ui.set_unit_rows(slint::ModelRc::new(slint::VecModel::from(rows.clone())));
    ui.set_unit_visible_count(slot);
    ui.set_sorted_units(slint::ModelRc::new(slint::VecModel::from(sorted.clone())));
    ui.set_unit_filter_counts(counts);

    let selftest_passed = all.iter().filter(|u| connected(u) && u.selftest_passed).count();
    ui.set_connected_count(summary.connected);
    ui.set_selftest_passed_count(selftest_passed as i32);
    ui.set_battery_count(summary.on_battery);
    ui.set_charging_count(summary.charging);
    ui.set_charged_count(summary.charged);
    crate::status::rebuild(ui, &sorted, &rows, &summary);
    crate::settings::apply_features(ui);
    ui.set_swarm_summary(summary);
}

pub(crate) fn update_unit(ui_weak: &slint::Weak<AppWindow>, index: usize, f: impl FnOnce(&mut UnitData) + Send + 'static) {
    let ui_weak = ui_weak.clone();
    slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak.upgrade() {
            let model = ui.get_units();
            if let Some(mut unit) = model.row_data(index) {
                f(&mut unit);
                model.set_row_data(index, unit);
            }
            schedule_rebuild(&ui);
        }
    }).ok();
}

thread_local! {
    static REBUILD_PENDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Rebuild the tables soon. Telemetry updates every unit several times a
/// second; one rebuild per 100 ms covers them all.
fn schedule_rebuild(ui: &AppWindow) {
    if REBUILD_PENDING.with(|p| p.replace(true)) {
        return;
    }
    let ui_weak = ui.as_weak();
    slint::Timer::single_shot(std::time::Duration::from_millis(100), move || {
        REBUILD_PENDING.with(|p| p.set(false));
        if let Some(ui) = ui_weak.upgrade() {
            rebuild_table_rows(&ui);
        }
    });
}

/// Lazily fetch platform/firmware info for the unit at a `sorted-units` row
/// index, updating the UI when done. Shared by the Units-table selection and
/// the visualization click-to-select so both populate the sidebar identically.
pub(crate) fn fetch_unit_details(ui_weak: slint::Weak<AppWindow>, swarm_state: SwarmState, row_index: i32) {
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

    host::spawn(async move {
        let cf = {
            let state = swarm_state.lock().await;
            match state.get(&original_index) {
                Some(cu) => cu.cf.clone(),
                None => return,
            }
        };

        // Read when the unit was connected.
        let platform_type = cf.platform.device_type_name().unwrap_or_default().to_string();
        let firmware_version = cf.platform.firmware_version().unwrap_or_default().to_string();

        update_unit(&ui_weak, original_index, move |u| {
            u.platform_type = platform_type.into();
            u.firmware_version = firmware_version.into();
        });
    });
}

pub(crate) fn setup(ui: &AppWindow, cx: &AppContext) {
    let AppContext { swarm_state, plot_selected_index, .. } = cx;
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

    // Filter chips and search
    {
        let ui_weak = ui.as_weak();
        ui.on_unit_filter_changed(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            rebuild_table_rows(&ui);
        });
    }

    // Lazy-fetch platform info when sidebar row is selected
    {
        let swarm_state = swarm_state.clone();
        let ui_weak = ui.as_weak();
        let plot_selected_index = plot_selected_index.clone();

        ui.on_unit_row_selected(move |row_index| {
            // Remember which unit a "Start logging" should bind to.
            if let Some(ui) = ui_weak.upgrade() {
                let units = ui.get_units();
                let indices = sort_unit_indices(&units, ui.get_sort_column(), ui.get_sort_ascending());
                let orig = if row_index >= 0 { indices.get(row_index as usize).copied() } else { None };
                *plot_selected_index.lock().unwrap() = orig;
            }
            fetch_unit_details(ui_weak.clone(), swarm_state.clone(), row_index);
        });
    }
}
