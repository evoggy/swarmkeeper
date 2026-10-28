//! The radio channel test.

use slint::{ComponentHandle, Model};

use crate::app::AppContext;
use crate::host::radios;
use crate::host::radios::link_uri;
use crate::swarm::SwarmState;
use crate::units::update_unit;
use crate::{AppWindow, ChannelResult, UnitState};
use crate::host;

pub(crate) async fn run_radio_channel_test(
    uri: String,
    swarm_state: SwarmState,
    _unit_index: usize,
    ui_weak: slint::Weak<AppWindow>,
) {
    let (radio_nth, original_channel, address) = match radios::RadioUri::parse(&uri) {
        Ok(Some(parsed)) => (parsed.radio.unwrap_or(0), parsed.channel, parsed.address_bytes()),
        _ => {
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
                connected.disconnect().await;
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
                    u.armed = false;
                    u.journal_entry_count = 0;
                    u.platform_type = "".into();
                    u.firmware_version = "".into();
                });
            }
        }
        // Brief delay to let all radio connections fully close
        if had_connections {
            host::sleep(std::time::Duration::from_millis(500)).await;
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

pub(crate) fn setup(ui: &AppWindow, cx: &AppContext) {
    let AppContext { link_context, swarm_state, radio_plan, .. } = cx;
    // Radio channel test
    {
        let link_context = link_context.clone();
        let radio_plan = radio_plan.clone();
        let swarm_state = swarm_state.clone();
        let ui_weak = ui.as_weak();

        ui.on_run_radio_test(move |unit_index| {
            let link_context = link_context.clone();
            let radio_plan = radio_plan.clone();
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

            host::spawn(async move {
                let uri = link_uri(&link_context, &radio_plan, &uri).await;
                run_radio_channel_test(uri, swarm_state, unit_index as usize, ui_weak).await;
            });
        });
    }
}
