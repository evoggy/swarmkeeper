//! Firmware flashing through the connection server.

use std::collections::HashMap;
use std::sync::Arc;

use slint::{ComponentHandle, Model};
use tokio::sync::Mutex;

use crate::app::AppContext;
use crate::host::radios::link_uri;
use crate::swarm::{ConnectedUnit, SwarmState};
use crate::units::update_unit;
use crate::{AppWindow, host::bootloader_flash, UnitState};
use crate::host;

/// Flash the STM32 or nRF51 (`target`, as the connection server names it) of
/// these units (index, URI) as one job on the connection server: by broadcast
/// to the units whose bootloaders take it, the others one after another.
/// Shows each unit's progress and result. Returns how many succeeded.
pub(crate) async fn flash_through_server(
    client: &crazyflie_connection_server::Client,
    units: Vec<(usize, String)>,
    target: &str,
    data: Vec<u8>,
    ui_weak: &slint::Weak<AppWindow>,
) -> usize {
    let keys: Vec<(usize, String)> = units
        .iter()
        .filter_map(|(idx, uri)| Some((*idx, crazyflie_connection_server::normalize_uri(uri).ok()?.key)))
        .collect();
    let index_of = |uri: &str| keys.iter().find(|(_, key)| key == uri).map(|(idx, _)| *idx);
    // The units armed for a broadcast, which then gets their progress.
    let mut armed: std::collections::HashSet<usize> = std::collections::HashSet::new();
    // The last percentage shown per unit and step, to show whole-percent steps only.
    let mut shown: HashMap<(Option<usize>, String), u64> = HashMap::new();
    let progress = |p: crazyflie_connection_server::FlashProgress| {
        let index = p.uri.as_deref().and_then(index_of);
        let pct = if p.total > 0 { p.done * 100 / p.total } else { 0 };
        if shown.insert((index, p.step.clone()), pct) == Some(pct) {
            return;
        }
        let frac = if p.total > 0 { p.done as f32 / p.total as f32 } else { 0.0 };
        match index {
            Some(index) => {
                if p.step == "Arming for broadcast" {
                    armed.insert(index);
                } else {
                    armed.remove(&index);
                }
                let flashing = p.step.starts_with("Flashing");
                let status: slint::SharedString =
                    if flashing { format!("Flashing… {}%", pct) } else { format!("{}…", p.step) }.into();
                update_unit(ui_weak, index, move |u| {
                    if flashing {
                        u.flash_progress = frac;
                    }
                    u.flash_status = status;
                });
            }
            // A broadcast: every armed unit gets the image at the same time.
            None => {
                for index in &armed {
                    let status: slint::SharedString = format!("{}… {}%", p.step, pct).into();
                    update_unit(ui_weak, *index, move |u| {
                        u.flash_progress = frac;
                        u.flash_status = status;
                    });
                }
            }
        }
    };

    let uris: Vec<&str> = units.iter().map(|(_, uri)| uri.as_str()).collect();
    let image = crazyflie_connection_server::FlashImage { target: target.to_string(), start: None, data };
    let results = match client.flash_swarm(&uris, vec![image], progress).await {
        Ok(results) => results,
        Err(e) => units.iter().map(|_| Err(e.to_string())).collect(),
    };
    let mut ok_count = 0;
    for ((idx, uri), result) in units.iter().zip(results) {
        match result {
            Ok(how) => {
                ok_count += 1;
                let status: slint::SharedString = format!("Done ({})", how).into();
                update_unit(ui_weak, *idx, move |u| {
                    u.flash_progress = 1.0;
                    u.flash_status = status;
                });
            }
            Err(reason) => {
                eprintln!("Flashing {} failed: {}", uri, reason);
                let status: slint::SharedString = format!("Error: {}", reason).into();
                update_unit(ui_weak, *idx, move |u| u.flash_status = status);
            }
        }
    }
    ok_count
}

/// Disconnect the whole swarm before flashing deck firmware: each unit is
/// flashed over a connection of its own, and every unit is about to be
/// restarted anyway.
pub(crate) async fn disconnect_for_flashing(
    swarm_state: &SwarmState,
    positioning_source: &Arc<Mutex<Option<usize>>>,
    ui_weak: &slint::Weak<AppWindow>,
) {
    *positioning_source.lock().await = None;
    let connected: Vec<(usize, ConnectedUnit)> = {
        let mut state = swarm_state.lock().await;
        state.drain().collect()
    };
    for (index, unit) in connected {
        unit.disconnect().await;
        update_unit(ui_weak, index, |u| {
            u.state = UnitState::Disconnected;
        });
    }
    {
        let ui_weak = ui_weak.clone();
        slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_swarm_connected(false);
            }
        }).ok();
    }
    // Give the radio a moment to settle after disconnecting.
    host::sleep(std::time::Duration::from_millis(300)).await;
}

pub(crate) fn setup(ui: &AppWindow, cx: &AppContext) {
    let AppContext { link_context, toc_cache, connection, swarm_state, radio_plan, flash_binary, positioning_source, .. } = cx;
    // Flash firmware (bootloader) dialog
    {
        // Populate the target dropdown with the same hardcoded list cfcli uses.
        let target_names: Vec<slint::SharedString> = bootloader_flash::target_list()
            .iter()
            .map(|t| (*t).into())
            .collect();
        ui.set_flash_target_names(slint::ModelRc::new(slint::VecModel::from(target_names)));
        ui.set_flash_selected_target(0);
    }

    // Select firmware binary via file picker
    {
        let flash_binary = flash_binary.clone();
        let ui_weak = ui.as_weak();
        ui.on_flash_select_binary(move || {
            let flash_binary = flash_binary.clone();
            let ui_weak = ui_weak.clone();
            host::spawn(async move {
                let Some(handle) = host::dialog::FileDialog::new()
                    .add_filter("Firmware binary", &["bin"])
                    .add_filter("All files", &["*"])
                    .set_title("Select firmware binary")
                    .pick_file()
                    .await
                else {
                    return;
                };

                let path = handle.path().to_path_buf();
                let name: slint::SharedString = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default()
                    .into();

                *flash_binary.lock().await = Some(path);

                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_flash_binary_name(name);
                        ui.set_flash_binary_selected(true);
                    }
                }).ok();
            });
        });
    }

    // Flash the selected binary to the whole swarm, one unit at a time
    {
        let link_context = link_context.clone();
        let radio_plan = radio_plan.clone();
        let toc_cache = toc_cache.clone();
        let swarm_state = swarm_state.clone();
        let flash_binary = flash_binary.clone();
        let positioning_source = positioning_source.clone();
        let connection = connection.clone();
        let ui_weak = ui.as_weak();

        ui.on_flash_start(move || {
            let Some(ui_ref) = ui_weak.upgrade() else { return };

            // Resolve the chosen target name from the dropdown index.
            let targets = bootloader_flash::target_list();
            let target_idx = ui_ref.get_flash_selected_target().max(0) as usize;
            let Some(target) = targets.get(target_idx).copied() else { return };
            let target = target.to_string();

            // Collect (model index, uri, name) for every unit up front.
            let units = ui_ref.get_units();
            let mut unit_info: Vec<(usize, String, String)> = Vec::new();
            for i in 0..units.row_count() {
                if let Some(unit) = units.row_data(i) {
                    unit_info.push((i, unit.uri.to_string(), unit.name.to_string()));
                }
            }

            ui_ref.set_flashing(true);
            ui_ref.set_flash_overall_status("Preparing…".into());

            let link_context = link_context.clone();
            let radio_plan = radio_plan.clone();
            let toc_cache = toc_cache.clone();
            let swarm_state = swarm_state.clone();
            let flash_binary = flash_binary.clone();
            let positioning_source = positioning_source.clone();
            let connection = connection.clone();
            let ui_weak = ui_weak.clone();

            host::spawn(async move {
                let finish = |ui_weak: &slint::Weak<AppWindow>, status: String| {
                    let ui_weak = ui_weak.clone();
                    slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_weak.upgrade() {
                            ui.set_flash_overall_status(status.into());
                            ui.set_flashing(false);
                        }
                    })
                    .ok();
                };

                // Load the firmware binary from disk.
                let path = flash_binary.lock().await.clone();
                let data = match path {
                    Some(p) => match host::fs::read(&p) {
                        Ok(d) => d,
                        Err(e) => return finish(&ui_weak, format!("Failed to read binary: {}", e)),
                    },
                    None => return finish(&ui_weak, "No binary selected".to_string()),
                };

                let (client, drives_radios) = {
                    let connection = connection.read().await;
                    (connection.client().clone(), connection.drives_radios())
                };
                // The STM32 and nRF51 are flashed by the connection server as
                // one job, by broadcast where the bootloaders take it, and each
                // unit is connected again after. Deck firmware is flashed over a
                // connection of its own, and every unit restarts anyway.
                let server_target = bootloader_flash::bootloader_target(&target);
                if server_target.is_none() {
                    if !drives_radios {
                        return finish(&ui_weak, "Deck firmware can't be flashed through a connection server yet".to_string());
                    }
                    disconnect_for_flashing(&swarm_state, &positioning_source, &ui_weak).await;
                }

                // Mark every unit as queued.
                for (idx, _, _) in &unit_info {
                    let idx = *idx;
                    update_unit(&ui_weak, idx, |u| {
                        u.flash_progress = 0.0;
                        u.flash_status = "Queued".into();
                    });
                }

                let total = unit_info.len();
                let ok_count = match server_target {
                    Some(server_target) => {
                        let mut units = Vec::new();
                        for (idx, uri, _) in &unit_info {
                            // With Swarmkeeper's own Crazyradios, each restarts
                            // into its bootloader from the one the radio plan gives it.
                            let uri = if drives_radios { link_uri(&link_context, &radio_plan, uri).await } else { uri.clone() };
                            units.push((*idx, uri));
                        }
                        let overall: slint::SharedString = format!("Flashing {} to {} units", target, total).into();
                        let ui_weak_inner = ui_weak.clone();
                        slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_weak_inner.upgrade() {
                                ui.set_flash_overall_status(overall);
                            }
                        })
                        .ok();
                        flash_through_server(&client, units, server_target, data, &ui_weak).await
                    }
                    None => {
                        let mut ok_count = 0usize;
                        for (n, (idx, uri, name)) in unit_info.iter().enumerate() {
                            let idx = *idx;
                            let overall: slint::SharedString =
                                format!("Flashing {} ({}/{}) — {}", name, n + 1, total, target).into();
                            {
                                let ui_weak_inner = ui_weak.clone();
                                slint::invoke_from_event_loop(move || {
                                    if let Some(ui) = ui_weak_inner.upgrade() {
                                        ui.set_flash_overall_status(overall);
                                    }
                                }).ok();
                            }
                            update_unit(&ui_weak, idx, |u| {
                                u.flash_progress = 0.0;
                                u.flash_status = "Flashing…".into();
                            });

                            // Per-chunk progress callback, throttled to whole-percent steps.
                            let ui_for_progress = ui_weak.clone();
                            let mut last_pct: i32 = -1;
                            let progress = move |written: usize, total_bytes: usize| {
                                let frac = if total_bytes > 0 {
                                    written as f32 / total_bytes as f32
                                } else {
                                    0.0
                                };
                                let pct = (frac * 100.0) as i32;
                                if pct != last_pct {
                                    last_pct = pct;
                                    let status: slint::SharedString = format!("Flashing… {}%", pct).into();
                                    update_unit(&ui_for_progress, idx, move |u| {
                                        u.flash_progress = frac;
                                        u.flash_status = status;
                                    });
                                }
                            };

                            let result = bootloader_flash::flash_deck_target(
                                link_context.as_ref(),
                                &link_uri(&link_context, &radio_plan, uri).await,
                                toc_cache.clone(),
                                &target,
                                &data,
                                progress,
                            )
                            .await;

                            match result {
                                Ok(()) => {
                                    ok_count += 1;
                                    update_unit(&ui_weak, idx, |u| {
                                        u.flash_progress = 1.0;
                                        u.flash_status = "Done".into();
                                    });
                                }
                                Err(e) => {
                                    let msg: slint::SharedString = format!("Error: {}", e).into();
                                    eprintln!("Flashing {} failed: {:?}", uri, e);
                                    update_unit(&ui_weak, idx, move |u| {
                                        u.flash_status = msg;
                                    });
                                }
                            }

                            // Give the deck time to restart before the next unit.
                            host::sleep(std::time::Duration::from_millis(500)).await;
                        }
                        ok_count
                    }
                };

                finish(&ui_weak, format!("Finished: {}/{} succeeded", ok_count, total));
            });
        });
    }
}
