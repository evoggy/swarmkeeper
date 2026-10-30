//! The swarm file: units, URIs and persistent parameters.

use serde::Deserialize;

use crate::units::rebuild_table_rows;
use crate::{AppWindow, UnitData, UnitState};
use crate::host;

/// Receive timeout of the radio link, in milliseconds. The crazyflie-link
/// default is 1000 ms, which is too tight when a whole swarm shares one radio:
/// a unit that goes a second without a packet has its link dropped as
/// "Connection timeout". Applied through the `timeout` URI parameter.
pub(crate) const LINK_TIMEOUT_MS: u32 = 3000;

/// Add `timeout=`[`LINK_TIMEOUT_MS`] to a Crazyflie URI, leaving a URI that
/// already sets its own `timeout` untouched.
pub(crate) fn uri_with_link_timeout(uri: &str) -> String {
    if uri.contains("timeout=") {
        return uri.to_string();
    }
    let separator = if uri.contains('?') { '&' } else { '?' };
    format!("{}{}timeout={}", uri, separator, LINK_TIMEOUT_MS)
}

#[derive(Deserialize)]
pub(crate) struct SwarmConfig {
    pub(crate) name: String,
    #[allow(dead_code)]
    pub(crate) description: Option<String>,
    pub(crate) units: Vec<UnitConfigYaml>,
}

#[derive(Deserialize)]
pub(crate) struct UnitConfigYaml {
    pub(crate) uri: String,
    pub(crate) name: String,
    #[allow(dead_code)]
    pub(crate) description: Option<String>,
}

pub(crate) fn load_swarm_config(path: &std::path::Path) -> Result<SwarmConfig, String> {
    let contents = host::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read swarm config {:?}: {}", path, e))?;
    serde_yaml::from_str(&contents)
        .map_err(|e| format!("Failed to parse swarm config {:?}: {}", path, e))
}

/// A single entry in a persistent parameter state file.
#[derive(Deserialize)]
pub(crate) struct PersistentParamEntry {
    #[serde(default)]
    pub(crate) is_stored: bool,
    pub(crate) stored_value: Option<serde_yaml::Value>,
}

/// Format produced by the persistent parameter dump (`type: persistent_param_state`).
#[derive(Deserialize)]
pub(crate) struct PersistentParamState {
    pub(crate) params: std::collections::BTreeMap<String, PersistentParamEntry>,
}

/// Load a persistent parameter file and return the list of `(name, value)` pairs
/// that have a stored value to upload.
pub(crate) fn load_persistent_params(path: &std::path::Path) -> Result<Vec<(String, f64)>, String> {
    let contents = host::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read params file {:?}: {}", path, e))?;
    let state: PersistentParamState = serde_yaml::from_str(&contents)
        .map_err(|e| format!("Failed to parse params file {:?}: {}", path, e))?;

    let mut params: Vec<(String, f64)> = Vec::new();
    for (name, entry) in state.params {
        if !entry.is_stored {
            continue;
        }
        let Some(value) = entry.stored_value.as_ref() else { continue };
        let num = match value {
            serde_yaml::Value::Number(n) => n.as_f64(),
            serde_yaml::Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            _ => None,
        };
        match num {
            Some(v) => params.push((name, v)),
            None => eprintln!("Skipping param {} with non-numeric stored value", name),
        }
    }
    Ok(params)
}

/// Ensure a radio URI has a default `timeout` query parameter.
/// If the URI already contains a `timeout` parameter, it is left unchanged.
/// Otherwise, `timeout=2000` (2 seconds) is appended.
pub(crate) fn uri_with_default_timeout(uri: &str) -> String {
    // Check if a timeout parameter is already present in the query string
    if let Some(query) = uri.split('?').nth(1) {
        if query.split('&').any(|p| p.starts_with("timeout=")) {
            return uri.to_string();
        }
        return format!("{}&timeout=2000", uri);
    }
    format!("{}?timeout=2000", uri)
}

pub(crate) fn apply_swarm_config(ui: &AppWindow, config: &SwarmConfig) {
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
            armed: false,
            journal_entry_count: 0,
            console_line_count: 0,
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
            radio: -1,
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
