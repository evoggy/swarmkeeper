//! Manual control of one unit with a gamepad.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use slint::ComponentHandle;
use tokio::sync::Mutex;

use crate::AppWindow;
use crate::app::AppContext;
use crate::units::sort_unit_indices;

pub(crate) struct ManualControlState {
    pub(crate) running: Arc<AtomicBool>,
}

pub(crate) type SharedManualControl = Arc<Mutex<Option<ManualControlState>>>;
pub(crate) type SharedGamepadIds = Arc<std::sync::Mutex<Vec<gilrs::GamepadId>>>;

pub(crate) fn apply_deadzone(value: f32, deadzone: f32) -> f32 {
    if value.abs() < deadzone { 0.0 } else { value }
}

pub(crate) fn map_joystick_axes(lx: f32, ly: f32, rx: f32, ry: f32) -> (f32, f32, f32, u16) {
    let deadzone = 0.1;
    let roll = apply_deadzone(rx, deadzone) * 30.0;
    let pitch = apply_deadzone(ry, deadzone) * 30.0;
    let yawrate = apply_deadzone(lx, deadzone) * 200.0;
    let thrust_f = apply_deadzone(ly, deadzone).max(0.0) * 60000.0;
    let thrust = thrust_f as u16;
    (roll, pitch, yawrate, thrust)
}

pub(crate) async fn run_manual_control(
    cf: Arc<crazyflie_connection_server::Crazyflie>,
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
pub(crate) async fn stop_manual_control_loop(manual_control: &SharedManualControl) {
    let mut mc = manual_control.lock().await;
    if let Some(prev) = mc.take() {
        prev.running.store(false, Ordering::Relaxed);
        drop(mc);
        // Brief delay to let the loop exit and send cleanup packets
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }
}

pub(crate) fn setup(ui: &AppWindow, cx: &AppContext) {
    let AppContext { swarm_state, gilrs, gamepad_ids, manual_control, .. } = cx;
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
                        if let Err(e) = cf.supervisor.send_arming_request(false).await {
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
}
