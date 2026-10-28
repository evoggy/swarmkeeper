//! System commands: reboot, power, sleep, identify, synchronised blink and TLV storage.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use slint::{ComponentHandle, Model};

use crate::app::AppContext;
use crate::radios;
use crate::swarm::{client_units, ConnectedUnit, link_uri};
use crate::swarm_file::uri_with_link_timeout;
use crate::units::{sort_unit_indices, update_unit};
use crate::{AppWindow, UnitState};

/// What a unit's nRF51 is asked to do
#[derive(Clone, Copy, Debug)]
pub(crate) enum PlatformAction {
    Reboot,
    PowerOff,
    Sleep,
    Wakeup,
}

/// Send a platform command to a unit through the client API. The server
/// first checks that it answers. A reboot and a wakeup return once its
/// firmware answers again; the server then gives the unit back (`available`)
/// and `follow_jobs` attaches it again. A unit put to sleep is `busy` until then
pub(crate) async fn send_platform(
    client: &crazyflie_connection_server::Client,
    uri: &str,
    action: PlatformAction,
) -> Result<(), String> {
    let sent = match action {
        PlatformAction::Reboot => client.reboot(uri).await,
        PlatformAction::PowerOff => client.power_off_all(uri).await,
        PlatformAction::Sleep => client.power_off_stm32_domain(uri).await,
        PlatformAction::Wakeup => client.power_on_stm32_domain(uri).await,
    };
    sent.map_err(|e| e.to_string())
}

/// Clear the persistent TLV/KVE storage on a connected Crazyflie by writing a
/// freshly-formatted empty KVE table (0x01, 0xFF, 0xFF) at the partition start
/// (EEPROM offset 1024 / 0x0400). The radio config block (0x0000-0x0014) is left
/// untouched, so no checksum recompute is needed. The firmware sees a valid,
/// empty store on the next boot. Mirrors `scripts/erase-tlv.sh` /
/// `cfcli mem write EEPROMConfig --offset 1024 --data 1,255,255`.
/// Power-cycle the unit afterwards so storage-backed subsystems re-read it.
pub(crate) async fn clear_tlv_storage(
    cf: &crazyflie_connection_server::Crazyflie,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use crazyflie_connection_server::memory::{MemoryType, RawMemory};
    const KVE_PARTITION_START: usize = 1024; // 0x0400, fixed offset in the EEPROM
    const EMPTY_KVE: [u8; 3] = [0x01, 0xFF, 0xFF];

    let memories = cf.memory.get_memories(Some(MemoryType::EEPROMConfig));
    if memories.len() != 1 {
        return Err(format!("expected 1 EEPROMConfig memory, found {}", memories.len()).into());
    }
    let raw = cf
        .memory
        .open_memory::<RawMemory>(memories[0])
        .await
        .map_err(|e| format!("could not open EEPROM as raw memory: {}", e))?;
    raw.write(KVE_PARTITION_START, &EMPTY_KVE).await.map_err(|e| format!("EEPROM write failed: {}", e).into())
}

pub(crate) fn setup(ui: &AppWindow, cx: &AppContext) {
    let AppContext { link_context, connection, swarm_state, radio_plan, positioning_source, .. } = cx;
    // Sync blink: broadcast synchronized blink command to all units via P2P radio broadcast
    {
        let link_context = link_context.clone();
        let radio_plan = radio_plan.clone();
        let ui_weak = ui.as_weak();
        ui.on_sync_blink(move || {
            let Some(ui_ref) = ui_weak.upgrade() else { return };
            let units = ui_ref.get_units();
            let uris: Vec<String> = (0..units.row_count())
                .filter_map(|i| units.row_data(i).map(|u| u.uri.to_string()))
                .collect();

            let link_context = link_context.clone();
            let radio_plan = radio_plan.clone();

            tokio::spawn(async move {
                // Collect unique (radio_nth, channel) pairs from all unit URIs
                let mut radio_channels: Vec<(usize, u8)> = Vec::new();
                for uri in &uris {
                    let link_uri = link_uri(&link_context, &radio_plan, uri).await;
                    if let Ok(Some(parsed)) = radios::RadioUri::parse(&link_uri) {
                        radio_channels.push((parsed.radio.unwrap_or(0), parsed.channel));
                    }
                }
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
        let radio_plan = radio_plan.clone();
        let ui_weak = ui.as_weak();
        ui.on_delayed_sync_blink(move || {
            let Some(ui_ref) = ui_weak.upgrade() else { return };
            let units = ui_ref.get_units();
            let uris: Vec<String> = (0..units.row_count())
                .filter_map(|i| units.row_data(i).map(|u| u.uri.to_string()))
                .collect();

            let link_context = link_context.clone();
            let radio_plan = radio_plan.clone();

            tokio::spawn(async move {
                let mut radio_channels: Vec<(usize, u8)> = Vec::new();
                for uri in &uris {
                    let link_uri = link_uri(&link_context, &radio_plan, uri).await;
                    if let Ok(Some(parsed)) = radios::RadioUri::parse(&link_uri) {
                        radio_channels.push((parsed.radio.unwrap_or(0), parsed.channel));
                    }
                }
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

    // Reboot swarm: the units come back connected (see follow_jobs)
    {
        let connection = connection.clone();
        let ui_weak = ui.as_weak();
        ui.on_reboot_swarm(move || {
            let Some(ui_ref) = ui_weak.upgrade() else { return };
            let units = ui_ref.get_units();
            let uris: Vec<String> = (0..units.row_count())
                .filter_map(|i| units.row_data(i).map(|u| u.uri.to_string()))
                .collect();

            let connection = connection.clone();

            tokio::spawn(async move {
                let client = connection.read().await.client().clone();
                let futs: Vec<_> = uris.iter().map(|uri| {
                    let client = client.clone();
                    let uri = uri.clone();
                    async move {
                        eprintln!("Rebooting {} ...", uri);
                        if let Err(e) = send_platform(&client, &uri, PlatformAction::Reboot).await {
                            eprintln!("Reboot failed for {}: {}", uri, e);
                        }
                    }
                }).collect();
                // The server connects the units it had again once they are
                // back, and follow_jobs attaches them again
                futures::future::join_all(futs).await;
            });
        });
    }

    // Power off swarm (send power-off to all units, then disconnect)
    {
        let connection = connection.clone();
        let swarm_state = swarm_state.clone();
        let positioning_source = positioning_source.clone();
        let ui_weak = ui.as_weak();
        ui.on_power_off_swarm(move || {
            let Some(ui_ref) = ui_weak.upgrade() else { return };
            let units = ui_ref.get_units();
            let uris: Vec<String> = (0..units.row_count())
                .filter_map(|i| units.row_data(i).map(|u| u.uri.to_string()))
                .collect();

            let connection = connection.clone();
            let swarm_state = swarm_state.clone();
            let positioning_source = positioning_source.clone();
            let ui_weak = ui_weak.clone();

            tokio::spawn(async move {
                let client = connection.read().await.client().clone();
                let futs: Vec<_> = uris.iter().map(|uri| {
                    let client = client.clone();
                    let uri = uri.clone();
                    async move {
                        eprintln!("Powering off {} ...", uri);
                        if let Err(e) = send_platform(&client, &uri, PlatformAction::PowerOff).await {
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
                    cu.disconnect().await;
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
        let connection = connection.clone();
        let ui_weak = ui.as_weak();
        ui.on_sysoff_swarm(move || {
            let Some(ui_ref) = ui_weak.upgrade() else { return };
            let units = ui_ref.get_units();
            let uris: Vec<String> = (0..units.row_count())
                .filter_map(|i| units.row_data(i).map(|u| u.uri.to_string()))
                .collect();

            let connection = connection.clone();

            tokio::spawn(async move {
                let client = connection.read().await.client().clone();
                let futs: Vec<_> = uris.iter().map(|uri| {
                    let client = client.clone();
                    let uri = uri.clone();
                    async move {
                        eprintln!("Sending sysoff (sleep) to {} ...", uri);
                        if let Err(e) = send_platform(&client, &uri, PlatformAction::Sleep).await {
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
        let connection = connection.clone();
        let ui_weak = ui.as_weak();
        ui.on_syson_swarm(move || {
            let Some(ui_ref) = ui_weak.upgrade() else { return };
            let units = ui_ref.get_units();
            let uris: Vec<String> = (0..units.row_count())
                .filter_map(|i| units.row_data(i).map(|u| u.uri.to_string()))
                .collect();

            let connection = connection.clone();

            tokio::spawn(async move {
                let client = connection.read().await.client().clone();
                let futs: Vec<_> = uris.iter().map(|uri| {
                    let client = client.clone();
                    let uri = uri.clone();
                    async move {
                        eprintln!("Sending syson (wake) to {} ...", uri);
                        if let Err(e) = send_platform(&client, &uri, PlatformAction::Wakeup).await {
                            eprintln!("SysOn failed for {}: {:?}", uri, e);
                        }
                    }
                }).collect();
                futures::future::join_all(futs).await;
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

                let _ = cf.param.set_lossy("motorPowerSet.enable", 2.0).await;

                loop {
                    if stop_flag.load(Ordering::Relaxed) { break; }

                    let color = colors[color_idx % colors.len()];
                    color_idx += 1;

                    if has_led_top { let _ = cf.param.set_lossy("colorLedTop.wrgb8888", f64::from(color)).await; }
                    if has_led_bottom { let _ = cf.param.set_lossy("colorLedBot.wrgb8888", f64::from(color)).await; }

                    let _ = cf.param.set_lossy("motorPowerSet.m1", 5000.0).await;
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                    let _ = cf.param.set_lossy("motorPowerSet.m1", 0.0).await;

                    if stop_flag.load(Ordering::Relaxed) { break; }
                    tokio::time::sleep(tokio::time::Duration::from_millis(400)).await;
                }

                // Reset LEDs and motors
                if has_led_top { let _ = cf.param.set_lossy("colorLedTop.wrgb8888", 0.0).await; }
                if has_led_bottom { let _ = cf.param.set_lossy("colorLedBot.wrgb8888", 0.0).await; }
                let _ = cf.param.set_lossy("motorPowerSet.m1", 0.0).await;
                let _ = cf.param.set_lossy("motorPowerSet.enable", 0.0).await;

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

    // Reboot single unit: it comes back connected (see follow_jobs)
    {
        let connection = connection.clone();
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

            let connection = connection.clone();

            tokio::spawn(async move {
                let client = connection.read().await.client().clone();
                eprintln!("Rebooting unit {} ({}) ...", original_index, uri);
                if let Err(e) = send_platform(&client, &uri, PlatformAction::Reboot).await {
                    eprintln!("Reboot failed for {}: {}", uri, e);
                }
            });
        });
    }

    // Clear TLV (persistent storage) on a single unit
    {
        let connection = connection.clone();
        let swarm_state = swarm_state.clone();
        let ui_weak = ui.as_weak();
        ui.on_clear_tlv_unit(move |row_index| {
            if row_index < 0 { return; }

            let (original_index, uri, name) = {
                let Some(ui) = ui_weak.upgrade() else { return };
                let units = ui.get_units();
                let col = ui.get_sort_column();
                let ascending = ui.get_sort_ascending();
                let indices = sort_unit_indices(&units, col, ascending);
                let row = row_index as usize;
                if row >= indices.len() { return; }
                let idx = indices[row];
                let Some(unit) = units.row_data(idx) else { return };
                (idx, unit.uri.to_string(), unit.name.to_string())
            };

            let connection = connection.clone();
            let swarm_state = swarm_state.clone();
            let ui_weak = ui_weak.clone();

            tokio::spawn(async move {
                let client = connection.read().await.client().clone();
                // Reuse the existing connection if the unit is connected, else
                // connect temporarily for the duration of the operation.
                let existing_cf = {
                    let state = swarm_state.lock().await;
                    state.get(&original_index).map(|cu| cu.cf.clone())
                };

                let (cf, temp_connection) = if let Some(cf) = existing_cf {
                    eprintln!("Clear TLV {}: using existing connection", name);
                    (cf, false)
                } else {
                    eprintln!("Clear TLV {}: connecting to {}...", name, uri);
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(30),
                        client.attach(&uri_with_link_timeout(&uri)),
                    ).await {
                        Ok(Ok(cf)) => (Arc::new(cf), true),
                        Ok(Err(e)) => {
                            eprintln!("Clear TLV {}: connect FAILED: {:?}", name, e);
                            let msg = format!("Clear TLV connect failed: {}", e);
                            update_unit(&ui_weak, original_index, move |u| {
                                u.state = UnitState::Error;
                                u.error_message = msg.into();
                            });
                            return;
                        }
                        Err(_) => {
                            eprintln!("Clear TLV {}: connect timed out", name);
                            update_unit(&ui_weak, original_index, move |u| {
                                u.state = UnitState::Error;
                                u.error_message = "Clear TLV connection timed out".into();
                            });
                            return;
                        }
                    }
                };

                match clear_tlv_storage(&cf).await {
                    Ok(()) => eprintln!("Clear TLV {}: OK (power-cycle to apply)", name),
                    Err(e) => {
                        eprintln!("Clear TLV {}: FAILED: {}", name, e);
                        let msg = format!("Clear TLV failed: {}", e);
                        update_unit(&ui_weak, original_index, move |u| {
                            u.error_message = msg.into();
                        });
                    }
                }

                if temp_connection {
                    cf.detach();
                    eprintln!("Clear TLV {}: detached", name);
                }
            });
        });
    }

    // Power off single unit
    {
        let connection = connection.clone();
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

            let connection = connection.clone();
            let swarm_state = swarm_state.clone();
            let ui_weak = ui_weak.clone();

            tokio::spawn(async move {
                let client = connection.read().await.client().clone();
                eprintln!("Powering off unit {} ({}) ...", original_index, uri);
                if let Err(e) = send_platform(&client, &uri, PlatformAction::PowerOff).await {
                    eprintln!("Power off failed for {}: {:?}", uri, e);
                }

                // Disconnect the unit
                let connected = {
                    let mut state = swarm_state.lock().await;
                    state.remove(&original_index)
                };
                if let Some(cu) = connected {
                    cu.disconnect().await;
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
        let connection = connection.clone();
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

            let connection = connection.clone();

            tokio::spawn(async move {
                let client = connection.read().await.client().clone();
                eprintln!("Sending sysoff (sleep) to unit {} ({}) ...", original_index, uri);
                if let Err(e) = send_platform(&client, &uri, PlatformAction::Sleep).await {
                    eprintln!("SysOff failed for {}: {:?}", uri, e);
                }
            });
        });
    }

    // SysOn single unit (wake, do NOT disconnect)
    {
        let connection = connection.clone();
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

            let connection = connection.clone();

            tokio::spawn(async move {
                let client = connection.read().await.client().clone();
                eprintln!("Sending syson (wake) to {} ...", uri);
                if let Err(e) = send_platform(&client, &uri, PlatformAction::Wakeup).await {
                    eprintln!("SysOn failed for {}: {:?}", uri, e);
                }
            });
        });
    }

    // Clear TLV (persistent storage) across the whole swarm
    {
        let swarm_state = swarm_state.clone();
        let connection = connection.clone();
        let ui_weak = ui.as_weak();
        ui.on_clear_tlv(move || {
            // Collect (index, uri, name) for every configured unit.
            let Some(ui_ref) = ui_weak.upgrade() else { return };
            let units = ui_ref.get_units();
            let unit_count = units.row_count();
            let mut unit_info: Vec<(usize, String, String)> = Vec::new();
            for i in 0..unit_count {
                if let Some(unit) = units.row_data(i) {
                    unit_info.push((i, unit.uri.to_string(), unit.name.to_string()));
                }
            }

            let swarm_state = swarm_state.clone();
            let connection = connection.clone();
            let ui_weak = ui_weak.clone();

            tokio::spawn(async move {
                let total = unit_info.len();
                if total == 0 {
                    eprintln!("No units configured for clear TLV");
                    return;
                }

                // Show progress dialog (shared with TOC/params)
                let ui_weak_inner = ui_weak.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak_inner.upgrade() {
                        ui.set_progress_dialog_title("Clearing TLV".into());
                        ui.set_progress_dialog_progress(0.0);
                        ui.set_progress_dialog_status("Starting...".into());
                        ui.set_progress_dialog_visible(true);
                    }
                }).ok();

                // Reuse existing connections where present, connect temporarily otherwise.
                let connected_units: HashMap<usize, Arc<crazyflie_connection_server::Crazyflie>> = {
                    let state = swarm_state.lock().await;
                    client_units(&state).into_iter().collect()
                };
                let client = connection.read().await.client().clone();
                let swarm_connected = !connected_units.is_empty();
                eprintln!("Clear TLV: {} units, swarm connected: {}", total, swarm_connected);

                let completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));

                let mut join_set = tokio::task::JoinSet::new();
                for (idx, uri, name) in unit_info.into_iter() {
                    let ui_weak = ui_weak.clone();
                    let completed = completed.clone();
                    let existing_cf = connected_units.get(&idx).cloned();
                    let client = client.clone();

                    join_set.spawn(async move {
                        // mem write uses the normal shared-radio connection (not
                        // cfloader), so concurrent access across the swarm is safe.
                        let (cf, temp_connection) = if let Some(cf) = existing_cf {
                            (cf, false)
                        } else {
                            match tokio::time::timeout(
                                std::time::Duration::from_secs(30),
                                client.attach(&uri_with_link_timeout(&uri)),
                            ).await {
                                Ok(Ok(cf)) => (Arc::new(cf), true),
                                Ok(Err(e)) => {
                                    eprintln!("Clear TLV {}: connect FAILED: {:?}", name, e);
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
                                    eprintln!("Clear TLV {}: connect timed out", name);
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

                        let ok = match clear_tlv_storage(&cf).await {
                            Ok(()) => { eprintln!("Clear TLV {}: OK", name); true }
                            Err(e) => { eprintln!("Clear TLV {}: FAILED: {}", name, e); false }
                        };

                        if temp_connection {
                            cf.detach();
                        }

                        let done = completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                        let progress = done as f32 / total as f32;
                        let status: slint::SharedString = if ok {
                            format!("Cleared {} ({}/{})", name, done, total).into()
                        } else {
                            format!("{} failed ({}/{})", name, done, total).into()
                        };
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

                eprintln!("Clear TLV complete");
                let ui_weak_inner = ui_weak.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak_inner.upgrade() {
                        ui.set_progress_dialog_status("Done — power-cycle units to apply".into());
                    }
                }).ok();
                tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

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
