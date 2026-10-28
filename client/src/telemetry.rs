//! Telemetry streamed from every connected unit.

use std::sync::Arc;

use tokio::sync::Mutex;

use crate::loco::SharedPositioningData;
use crate::trajectory::SharedGotoTargets;
use crate::units::{pm_state_text, supervisor_text, update_unit};
use crate::{AppWindow, UnitState};

/// Telemetry log period scaled to the size of the swarm, since all units share
/// the same radio bandwidth: up to 30 units 10 Hz, 31-70 units 5 Hz, above 1 Hz.
pub(crate) fn telemetry_period_ms(unit_count: usize) -> u64 {
    match unit_count {
        0..=30 => 100,
        31..=70 => 200,
        _ => 1000,
    }
}

pub(crate) async fn start_telemetry(
    index: usize,
    uri: String,
    cf: Arc<crazyflie_connection_server::Crazyflie>,
    ui_weak: slint::Weak<AppWindow>,
    positioning_data: SharedPositioningData,
    positioning_source: Arc<Mutex<Option<usize>>>,
    goto_targets: SharedGotoTargets,
    unit_count: usize,
) {
    // Supervisor and positioning status variables may not exist on all
    // firmware, and a subscription fails on a missing variable.
    let log_names: std::collections::HashSet<String> = cf.log.names().into_iter().collect();
    let has = |name: &str| log_names.contains(name);
    let has_supervisor_info = has("supervisor.info");
    let has_lh_active = has("lighthouse.bsActive");
    let has_ranging_state = has("ranging.state");
    let has_loco_mode = has("loco.mode");
    eprintln!("Telemetry {}: has_supervisor_info={}, has_lh_active={}, has_ranging_state={}", uri, has_supervisor_info, has_lh_active, has_ranging_state);
    let mut variables = vec!["stateEstimate.x", "stateEstimate.y", "stateEstimate.z", "pm.vbat", "pm.state"];
    for (name, present) in [
        ("supervisor.info", has_supervisor_info),
        ("lighthouse.bsActive", has_lh_active),
        ("ranging.state", has_ranging_state),
        ("loco.mode", has_loco_mode),
    ] {
        if present {
            variables.push(name);
        }
    }

    let period_ms = telemetry_period_ms(unit_count);
    eprintln!("Telemetry {}: logging at {} ms ({} units)", uri, period_ms, unit_count);
    let mut log_stream = match cf.log.subscribe(&variables, std::time::Duration::from_millis(period_ms)).await {
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

    // Through a connection server each statistics read is a request, so
    // they are read at most this often rather than with every sample.
    const STATS_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
    let mut stats = crazyflie_connection_server::LinkStatistics::default();
    let mut stats_read: Option<std::time::Instant> = None;

    loop {
        let data = match log_stream.next().await {
            Ok(d) => d,
            Err(_) => {
                eprintln!("Log stream error for {} (index {}): disconnected or error", uri, index);
                break
            },
        };
        let value = |name: &str| data.data.get(name).copied();

        let x = value("stateEstimate.x").unwrap_or(0.0) as f32;
        let y = value("stateEstimate.y").unwrap_or(0.0) as f32;
        let z = value("stateEstimate.z").unwrap_or(0.0) as f32;

        // Drop the goto marker once the unit has arrived at its target.
        {
            const GOTO_REACHED_TOLERANCE: f32 = 0.15; // metres
            let mut targets = goto_targets.lock().await;
            if let Some(t) = targets.get(&index) {
                let dx = x - t[0];
                let dy = y - t[1];
                let dz = z - t[2];
                if (dx * dx + dy * dy + dz * dz).sqrt() <= GOTO_REACHED_TOLERANCE {
                    targets.remove(&index);
                }
            }
        }

        let vbat = value("pm.vbat").unwrap_or(0.0) as f32;
        let pm_state = value("pm.state").unwrap_or(-1.0) as i8;
        let pm_state_str = pm_state_text(pm_state);

        let supervisor_info = value("supervisor.info").unwrap_or(0.0) as u16;

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
            let lh_active = value("lighthouse.bsActive").unwrap_or(0.0) as u16;
            let ranging_active = value("ranging.state").unwrap_or(0.0) as u16;
            if ranging_active != 0 {
                eprintln!("ranging.state = {}", ranging_active);
            }
            let loco_mode = value("loco.mode").unwrap_or(0.0) as u8;
            if let Ok(mut pd) = positioning_data.try_lock() {
                pd.lighthouse_active = lh_active;
                pd.loco_active = ranging_active;
                pd.loco_mode = loco_mode;
            }
        }

        let supervisor_state_str = supervisor_text(supervisor_info as i32);

        if stats_read.is_none_or(|read| read.elapsed() >= STATS_INTERVAL) {
            if let Ok(read) = cf.link_service.get_statistics().await {
                stats = read;
            }
            stats_read = Some(std::time::Instant::now());
        }
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
            u.armed = supervisor_info & 0x0002 != 0;
            u.state = unit_state;
        });
    }

    // Connection lost
    goto_targets.lock().await.remove(&index);
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
        u.armed = false;
        u.journal_entry_count = 0;
        u.platform_type = "".into();
        u.firmware_version = "".into();
    });
}
