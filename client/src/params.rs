//! Parameters: tuning, the set-parameter dialog and persistent parameters.

use std::collections::HashMap;
use std::sync::Arc;

use slint::{ComponentHandle, Model};

use crate::app::AppContext;
use crate::settings::AppSettings;
use crate::swarm::client_units;
use crate::swarm_file::{load_persistent_params, uri_with_link_timeout};
use crate::{AppWindow, SwarmParamData};

/// The `ValueType` of a parameter type name, as the client API gives it
/// (`u8`, `f32`, …).
pub(crate) fn value_type_from_name(name: &str) -> Option<crazyflie_lib::ValueType> {
    use crazyflie_lib::ValueType::*;
    Some(match name {
        "u8" => U8,
        "u16" => U16,
        "u32" => U32,
        "u64" => U64,
        "i8" => I8,
        "i16" => I16,
        "i32" => I32,
        "i64" => I64,
        "f16" => F16,
        "f32" => F32,
        "f64" => F64,
        _ => return None,
    })
}

/// Parse a user-entered string into a [`crazyflie_lib::Value`] of the given
/// type, enforcing that the value actually fits the type (range for integers,
/// numeric for floats). Returns `Err(message)` describing the problem otherwise.
pub(crate) fn parse_param_value(
    vt: crazyflie_lib::ValueType,
    s: &str,
) -> Result<crazyflie_lib::Value, String> {
    use crazyflie_lib::{Value, ValueType};
    let s = s.trim();
    if s.is_empty() {
        return Err("Enter a value".to_string());
    }
    macro_rules! parse_int {
        ($variant:ident, $ty:ty) => {
            match s.parse::<$ty>() {
                Ok(v) => Ok(Value::$variant(v)),
                Err(_) => Err(format!(
                    "Expected an integer between {} and {}",
                    <$ty>::MIN,
                    <$ty>::MAX
                )),
            }
        };
    }
    match vt {
        ValueType::U8 => parse_int!(U8, u8),
        ValueType::U16 => parse_int!(U16, u16),
        ValueType::U32 => parse_int!(U32, u32),
        ValueType::U64 => parse_int!(U64, u64),
        ValueType::I8 => parse_int!(I8, i8),
        ValueType::I16 => parse_int!(I16, i16),
        ValueType::I32 => parse_int!(I32, i32),
        ValueType::I64 => parse_int!(I64, i64),
        ValueType::F16 => match s.parse::<f64>() {
            Ok(v) if v.is_finite() => Ok(Value::from_f64_lossy(ValueType::F16, v)),
            Ok(_) => Err("Expected a finite number".to_string()),
            Err(_) => Err("Expected a number".to_string()),
        },
        ValueType::F32 => match s.parse::<f32>() {
            Ok(v) => Ok(Value::F32(v)),
            Err(_) => Err("Expected a number".to_string()),
        },
        ValueType::F64 => match s.parse::<f64>() {
            Ok(v) => Ok(Value::F64(v)),
            Err(_) => Err("Expected a number".to_string()),
        },
    }
}

pub(crate) fn setup(ui: &AppWindow, cx: &AppContext) {
    let AppContext { settings, connection, swarm_state, .. } = cx;
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

    // Set tuning parameters on all units (works with or without swarm connected)
    {
        let swarm_state = swarm_state.clone();
        let connection = connection.clone();
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
            let connection = connection.clone();
            let ui_weak = ui_weak.clone();

            tokio::spawn(async move {
                // Check if units are already connected
                let connected_units: HashMap<usize, Arc<crazyflie_connection_server::Crazyflie>> = {
                    let state = swarm_state.lock().await;
                    client_units(&state).into_iter().collect()
                };
                let client = connection.read().await.client().clone();
                let swarm_connected = !connected_units.is_empty();
                eprintln!("Tuning: {} units, swarm connected: {}", total, swarm_connected);

                let completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));

                let mut join_set = tokio::task::JoinSet::new();
                for (idx, (uri, name)) in unit_info.into_iter().enumerate() {
                    let ui_weak = ui_weak.clone();
                    let completed = completed.clone();
                    let existing_cf = connected_units.get(&idx).cloned();
                    let client = client.clone();

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
                                client.attach(&uri_with_link_timeout(&uri)),
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
                            cf.detach();
                            eprintln!("Tuning {}: detached", name);
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

    // ---- Set swarm parameter dialog ----

    // Live validation of an entered value against the selected parameter's type.
    ui.on_validate_param_value(|type_name, value| {
        match value_type_from_name(type_name.as_str()) {
            Some(vt) => match parse_param_value(vt, value.as_str()) {
                Ok(_) => slint::SharedString::new(),
                Err(e) => e.into(),
            },
            // Unknown type: don't block the user with a spurious error.
            None => slint::SharedString::new(),
        }
    });

    // Build the intersection of writable parameters across all connected units.
    {
        let swarm_state = swarm_state.clone();
        let ui_weak = ui.as_weak();
        ui.on_refresh_swarm_params(move || {
            let swarm_state = swarm_state.clone();
            let ui_weak = ui_weak.clone();
            tokio::spawn(async move {
                let cfs: Vec<Arc<crazyflie_connection_server::Crazyflie>> = {
                    let state = swarm_state.lock().await;
                    client_units(&state).into_iter().map(|(_, cf)| cf).collect()
                };
                let unit_count = cfs.len() as i32;

                let mut list: Vec<SwarmParamData> = Vec::new();
                if let Some((first, rest)) = cfs.split_first() {
                    // A parameter is offered only if every connected unit has it.
                    let mut common: std::collections::HashSet<String> =
                        first.param.names().into_iter().collect();
                    for cf in rest {
                        let names: std::collections::HashSet<String> =
                            cf.param.names().into_iter().collect();
                        common.retain(|n| names.contains(n));
                    }
                    for name in common {
                        // Only writable params can be set; skip read-only ones.
                        if first.param.is_writable(&name).unwrap_or(false) {
                            if let Ok(type_name) = first.param.get_type(&name) {
                                list.push(SwarmParamData {
                                    name: name.clone().into(),
                                    type_name: type_name.into(),
                                    writable: true,
                                });
                            }
                        }
                    }
                    list.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
                }

                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_swarm_params_all(slint::ModelRc::new(slint::VecModel::from(
                            list.clone(),
                        )));
                        ui.set_swarm_params(slint::ModelRc::new(slint::VecModel::from(list)));
                        ui.set_set_param_unit_count(unit_count);
                        ui.set_set_param_loading(false);
                    }
                })
                .ok();
            });
        });
    }

    // Filter the cached parameter list by the search string.
    {
        let ui_weak = ui.as_weak();
        ui.on_filter_swarm_params(move |search| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let needle = search.as_str().to_lowercase();
            let filtered: Vec<SwarmParamData> = ui
                .get_swarm_params_all()
                .iter()
                .filter(|p| needle.is_empty() || p.name.as_str().to_lowercase().contains(&needle))
                .collect();
            ui.set_swarm_params(slint::ModelRc::new(slint::VecModel::from(filtered)));
        });
    }

    // Set the chosen parameter on every connected unit, reporting progress.
    {
        let swarm_state = swarm_state.clone();
        let ui_weak = ui.as_weak();
        ui.on_set_swarm_param(move |name, value, persist| {
            let name = name.to_string();
            let value = value.to_string();
            let swarm_state = swarm_state.clone();
            let ui_weak = ui_weak.clone();

            if let Some(ui) = ui_weak.upgrade() {
                let title = if persist {
                    format!("Setting {} (persist)", name)
                } else {
                    format!("Setting {}", name)
                };
                ui.set_progress_dialog_title(title.into());
                ui.set_progress_dialog_progress(0.0);
                ui.set_progress_dialog_status("Starting…".into());
                ui.set_progress_dialog_visible(true);
            }

            tokio::spawn(async move {
                let units: Vec<(usize, Arc<crazyflie_connection_server::Crazyflie>)> = {
                    let state = swarm_state.lock().await;
                    client_units(&state)
                };
                let total = units.len();
                if total == 0 {
                    let ui_weak_inner = ui_weak.clone();
                    slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_weak_inner.upgrade() {
                            ui.set_progress_dialog_visible(false);
                        }
                    })
                    .ok();
                    return;
                }

                let completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let failed = Arc::new(std::sync::atomic::AtomicUsize::new(0));

                let mut join_set = tokio::task::JoinSet::new();
                for (idx, cf) in units {
                    let name = name.clone();
                    let value = value.clone();
                    let ui_weak = ui_weak.clone();
                    let completed = completed.clone();
                    let failed = failed.clone();
                    join_set.spawn(async move {
                        let result: Result<(), String> = async {
                            let type_name = cf
                                .param
                                .get_type(&name)
                                .map_err(|e| format!("type lookup failed: {:?}", e))?;
                            let vt = value_type_from_name(type_name)
                                .ok_or_else(|| format!("unknown type {}", type_name))?;
                            // Parsed as the param's type to check the value, sent
                            // as f64 like every value through the client API.
                            let parsed = parse_param_value(vt, &value)?;
                            cf.param
                                .set_lossy(name.as_str(), parsed.to_f64_lossy())
                                .await
                                .map_err(|e| format!("set failed: {:?}", e))?;
                            // Optionally store to flash so the value survives reboot.
                            // Only persistent params can be stored; skip the rest
                            // (the live value above is still applied).
                            if persist {
                                match cf.param.is_persistent(&name).await {
                                    Ok(true) => cf
                                        .param
                                        .persistent_store(name.as_str())
                                        .await
                                        .map_err(|e| format!("persist failed: {:?}", e))?,
                                    Ok(false) => {
                                        eprintln!(
                                            "Persist {} on unit {}: not a persistent param, skipped",
                                            name, idx
                                        );
                                    }
                                    Err(e) => {
                                        return Err(format!("persist check failed: {:?}", e))
                                    }
                                }
                            }
                            Ok(())
                        }
                        .await;

                        match &result {
                            Ok(()) => eprintln!("Set {} on unit {} OK", name, idx),
                            Err(e) => {
                                failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                eprintln!("Set {} on unit {} FAILED: {}", name, idx, e);
                            }
                        }

                        let done =
                            completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                        let progress = done as f32 / total as f32;
                        let nfailed = failed.load(std::sync::atomic::Ordering::Relaxed);
                        let status: slint::SharedString = if nfailed > 0 {
                            format!("{}/{} done, {} failed", done, total, nfailed).into()
                        } else {
                            format!("{}/{} done", done, total).into()
                        };
                        let ui_weak_inner = ui_weak.clone();
                        slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_weak_inner.upgrade() {
                                ui.set_progress_dialog_progress(progress);
                                ui.set_progress_dialog_status(status);
                            }
                        })
                        .ok();
                    });
                }

                while join_set.join_next().await.is_some() {}

                tokio::time::sleep(std::time::Duration::from_millis(600)).await;
                let ui_weak_inner = ui_weak.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak_inner.upgrade() {
                        ui.set_progress_dialog_visible(false);
                    }
                })
                .ok();
            });
        });
    }

    // Upload persistent parameters from a file to all units
    {
        let swarm_state = swarm_state.clone();
        let connection = connection.clone();
        let ui_weak = ui.as_weak();
        ui.on_upload_params(move || {
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

            let swarm_state = swarm_state.clone();
            let connection = connection.clone();
            let ui_weak = ui_weak.clone();

            tokio::spawn(async move {
                // Open file dialog
                let Some(handle) = rfd::AsyncFileDialog::new()
                    .add_filter("YAML", &["yaml", "yml"])
                    .pick_file()
                    .await
                else { return };
                let path = handle.path().to_path_buf();

                // Parse the persistent parameter file
                let params = match load_persistent_params(&path) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("Failed to load params file: {}", e);
                        return;
                    }
                };
                if params.is_empty() {
                    eprintln!("No stored parameters found in {:?}", path);
                    return;
                }
                eprintln!("Loaded {} stored params from {:?}", params.len(), path);
                let params = Arc::new(params);

                let total = unit_info.len();
                if total == 0 {
                    eprintln!("No units configured for parameter upload");
                    return;
                }

                // Show progress dialog
                let ui_weak_inner = ui_weak.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak_inner.upgrade() {
                        ui.set_progress_dialog_title("Uploading Parameters".into());
                        ui.set_progress_dialog_progress(0.0);
                        ui.set_progress_dialog_status("Starting...".into());
                        ui.set_progress_dialog_visible(true);
                    }
                }).ok();

                // Check if units are already connected
                let connected_units: HashMap<usize, Arc<crazyflie_connection_server::Crazyflie>> = {
                    let state = swarm_state.lock().await;
                    client_units(&state).into_iter().collect()
                };
                let client = connection.read().await.client().clone();
                let swarm_connected = !connected_units.is_empty();
                eprintln!("Param upload: {} units, swarm connected: {}", total, swarm_connected);

                let completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));

                let mut join_set = tokio::task::JoinSet::new();
                for (idx, (uri, name)) in unit_info.into_iter().enumerate() {
                    let ui_weak = ui_weak.clone();
                    let completed = completed.clone();
                    let existing_cf = connected_units.get(&idx).cloned();
                    let client = client.clone();
                    let params = params.clone();

                    join_set.spawn(async move {
                        // Use existing connection or connect temporarily
                        let (cf, temp_connection) = if let Some(cf) = existing_cf {
                            eprintln!("Param upload {}: using existing connection", name);
                            (cf, false)
                        } else {
                            eprintln!("Param upload {}: connecting to {}...", name, uri);
                            match tokio::time::timeout(
                                std::time::Duration::from_secs(30),
                                client.attach(&uri_with_link_timeout(&uri)),
                            ).await {
                                Ok(Ok(cf)) => (Arc::new(cf), true),
                                Ok(Err(e)) => {
                                    eprintln!("Param upload {}: connect FAILED: {:?}", name, e);
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
                                    eprintln!("Param upload {}: connect timed out", name);
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

                        for (param_name, value) in params.iter() {
                            // Apply the live value first; only persist to flash if
                            // that succeeded and the param actually supports it.
                            match cf.param.set_lossy(param_name, *value).await {
                                Ok(()) => eprintln!("  {} {} = {} OK", name, param_name, value),
                                Err(e) => {
                                    eprintln!("  {} {} set FAILED: {:?}", name, param_name, e);
                                    continue;
                                }
                            }
                            match cf.param.is_persistent(param_name).await {
                                Ok(true) => match cf.param.persistent_store(param_name).await {
                                    Ok(()) => eprintln!("  {} {} stored OK", name, param_name),
                                    Err(e) => eprintln!("  {} {} persistent_store FAILED: {:?}", name, param_name, e),
                                },
                                Ok(false) => eprintln!("  {} {} not persistent, skipped store", name, param_name),
                                Err(e) => eprintln!("  {} {} persist check FAILED: {:?}", name, param_name, e),
                            }
                        }

                        if temp_connection {
                            cf.detach();
                            eprintln!("Param upload {}: detached", name);
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

                eprintln!("All params uploaded, closing dialog");
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
}
