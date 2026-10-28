//! The unit table: rows, sorting and the selected unit.

use slint::{ComponentHandle, Model, StandardListViewItem};

use crate::app::AppContext;
use crate::swarm::SwarmState;
use crate::{AppWindow, UnitData, UnitState};
use crate::host;

pub(crate) fn state_symbol(state: UnitState) -> &'static str {
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

pub(crate) fn state_text(state: UnitState) -> &'static str {
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

pub(crate) fn bool_check(v: bool) -> &'static str {
    if v { "✓" } else { "" }
}

/// The URI of a unit as the list shows it: a `radio:///` URI with the
/// Crazyradio it was given when it connected.
pub(crate) fn uri_text(u: &UnitData) -> String {
    if u.uri.starts_with("radio:///") && u.radio >= 0 {
        format!("{} (radio {})", u.uri, u.radio)
    } else {
        u.uri.to_string()
    }
}

pub(crate) fn unit_to_row(u: &UnitData) -> slint::ModelRc<StandardListViewItem> {
    let status_text = if u.state == UnitState::Error {
        u.error_message.to_string()
    } else {
        state_text(u.state).to_string()
    };
    let items: Vec<StandardListViewItem> = vec![
        state_symbol(u.state).into(),
        status_text.as_str().into(),
        u.name.as_str().into(),
        uri_text(u).as_str().into(),
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
            _ => std::cmp::Ordering::Equal,
        };
        if ascending { ord } else { ord.reverse() }
    });
    indices
}

pub(crate) fn rebuild_table_rows(ui: &AppWindow) {
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

pub(crate) fn update_unit(ui_weak: &slint::Weak<AppWindow>, index: usize, f: impl FnOnce(&mut UnitData) + Send + 'static) {
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
