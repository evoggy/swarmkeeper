//! Flight commands for the swarm and for one unit.

use std::collections::HashMap;
use std::sync::Arc;

use slint::{ComponentHandle, Model};

use crate::app::AppContext;
use crate::units::sort_unit_indices;
use crate::{AppWindow, planning};
use crate::host;

pub(crate) fn setup(ui: &AppWindow, cx: &AppContext) {
    let AppContext { swarm_state, goto_targets, viz_scene_state, .. } = cx;
    // Land all connected units
    {
        let swarm_state = swarm_state.clone();
        let ui_weak = ui.as_weak();
        ui.on_land_swarm(move || {
            let swarm_state = swarm_state.clone();

            // Current Z per unit, needed to scale each descent. Read here on the event
            // loop rather than inside the task.
            let positions: HashMap<usize, f32> = match ui_weak.upgrade() {
                Some(ui) => {
                    let units = ui.get_units();
                    (0..units.row_count())
                        .filter_map(|i| units.row_data(i).map(|u| (i, u.pos_z)))
                        .collect()
                }
                None => HashMap::new(),
            };

            host::spawn(async move {
                // Each unit lands back on the surface it launched from. land() takes an
                // absolute world-frame height, and the origin is not the floor once it has
                // been moved - descending to a hard-coded 0.0 would cut the motors while
                // still airborne (or drive the unit into the floor). Mirrors the per-unit
                // Land button; falls back to 0.0 for units that never took off through
                // this app.
                let targets: Vec<(usize, Arc<crazyflie_connection_server::Crazyflie>, f32)> = {
                    let state = swarm_state.lock().await;
                    state
                        .iter()
                        .map(|(idx, cu)| (*idx, cu.cf.clone(), cu.launch_z.unwrap_or(0.0)))
                        .collect()
                };

                eprintln!("Landing {} units...", targets.len());
                for (idx, cf, target_z) in &targets {
                    let pos_z = positions.get(idx).copied().unwrap_or(*target_z);
                    // 2 seconds per metre of actual descent, minimum 1 second. The absolute
                    // distance keeps this right when pos_z or target_z is negative.
                    let duration: f32 = ((pos_z - target_z).abs() * 2.0).max(1.0);
                    eprintln!(
                        "Landing unit {} from {:.2}m to {:.2}m over {:.1}s...",
                        idx, pos_z, target_z, duration
                    );
                    if let Err(e) = cf.high_level_commander.land(*target_z, None, duration, None).await {
                        eprintln!("Unit {}: land failed: {:?}", idx, e);
                    }
                }
            });
        });
    }

    // Emergency stop: disarm all connected units
    {
        let swarm_state = swarm_state.clone();
        ui.on_emergency_stop(move || {
            let swarm_state = swarm_state.clone();

            host::spawn(async move {
                let connected_units: Vec<(usize, Arc<crazyflie_connection_server::Crazyflie>)> = {
                    let state = swarm_state.lock().await;
                    state.iter().map(|(idx, cu)| (*idx, cu.cf.clone())).collect()
                };

                eprintln!("EMERGENCY STOP: disarming {} units...", connected_units.len());
                for (idx, cf) in &connected_units {
                    if let Err(e) = cf.supervisor.send_arming_request(false).await {
                        eprintln!("Unit {}: disarm failed: {:?}", idx, e);
                    }
                }
            });
        });
    }

    // Arm all connected units (Swarm menu)
    {
        let swarm_state = swarm_state.clone();
        ui.on_arm_swarm(move || {
            let swarm_state = swarm_state.clone();

            host::spawn(async move {
                let connected_units: Vec<(usize, Arc<crazyflie_connection_server::Crazyflie>)> = {
                    let state = swarm_state.lock().await;
                    state.iter().map(|(idx, cu)| (*idx, cu.cf.clone())).collect()
                };

                eprintln!("Arming {} units...", connected_units.len());
                let mut join_set = host::JoinSet::new();
                for (idx, cf) in connected_units {
                    join_set.spawn(async move {
                        if let Err(e) = cf.supervisor.send_arming_request(true).await {
                            eprintln!("Unit {}: arm failed: {:?}", idx, e);
                        }
                    });
                }
                while join_set.join_next().await.is_some() {}
            });
        });
    }

    // Disarm all connected units (Swarm menu)
    {
        let swarm_state = swarm_state.clone();
        ui.on_disarm_swarm(move || {
            let swarm_state = swarm_state.clone();

            host::spawn(async move {
                let connected_units: Vec<(usize, Arc<crazyflie_connection_server::Crazyflie>)> = {
                    let state = swarm_state.lock().await;
                    state.iter().map(|(idx, cu)| (*idx, cu.cf.clone())).collect()
                };

                eprintln!("Disarming {} units...", connected_units.len());
                let mut join_set = host::JoinSet::new();
                for (idx, cf) in connected_units {
                    join_set.spawn(async move {
                        if let Err(e) = cf.supervisor.send_arming_request(false).await {
                            eprintln!("Unit {}: disarm failed: {:?}", idx, e);
                        }
                    });
                }
                while join_set.join_next().await.is_some() {}
            });
        });
    }

    // HLC Takeoff
    {
        let swarm_state = swarm_state.clone();
        let ui_weak = ui.as_weak();

        ui.on_hlc_takeoff(move |row_index, height_str, yaw_str, time_str, relative| {
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
            let ui_weak_inner = ui_weak.clone();

            host::spawn(async move {
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

                // Snapshot current x,y,z before takeoff. x,y become the home position.
                let home_pos = {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    let _ = slint::invoke_from_event_loop(move || {
                        let pos = if let Some(ui) = ui_weak_inner.upgrade() {
                            let units = ui.get_units();
                            units.row_data(original_index).map(|u| [u.pos_x, u.pos_y, u.pos_z])
                        } else {
                            None
                        };
                        let _ = tx.send(pos);
                    });
                    rx.await.ok().flatten()
                };

                // When relative, the entered height is added to the current Z so takeoff
                // is "go up by H" regardless of the world-frame origin (which may be negative).
                let target_height = match (relative, home_pos) {
                    (true, Some(pos)) => pos[2] + height,
                    _ => height,
                };

                if let Some(pos) = home_pos {
                    let home = [pos[0], pos[1], target_height];
                    let mut state = swarm_state.lock().await;
                    if let Some(cu) = state.get_mut(&original_index) {
                        cu.home_position = Some(home);
                        // Remember the launch surface so landing returns to it.
                        cu.launch_z = Some(pos[2]);
                        eprintln!("Unit {} home position saved: ({:.2}, {:.2}, {:.2}), launch z {:.2}", original_index, home[0], home[1], home[2], pos[2]);
                    }
                }

                eprintln!("Arming unit {}...", original_index);
                if let Err(e) = cf.supervisor.send_arming_request(true).await {
                    eprintln!("Arming failed: {:?}", e);
                    return;
                }
                // Wait for propellers to spin up before taking off
                host::sleep(std::time::Duration::from_secs(2)).await;

                eprintln!("Taking off unit {} to {:.2}m{}, yaw={:.1}deg, over {:.1}s...", original_index, target_height, if relative { " (relative)" } else { "" }, yaw_deg, duration);
                if let Err(e) = cf.high_level_commander.take_off(target_height, Some(yaw_rad), duration, None).await {
                    eprintln!("Takeoff failed: {:?}", e);
                }
            });
        });
    }

    // HLC Goto
    {
        let swarm_state = swarm_state.clone();
        let goto_targets = goto_targets.clone();
        let ui_weak = ui.as_weak();

        ui.on_hlc_goto(move |row_index, x_str, y_str, z_str, yaw_str, relative, speed_str| {
            if row_index < 0 {
                return;
            }

            let (original_index, current_pos) = {
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
                let pos = units.row_data(idx).map(|u| [u.pos_x, u.pos_y, u.pos_z]).unwrap_or([0.0, 0.0, 0.0]);
                (idx, pos)
            };

            let x: f32 = x_str.parse().unwrap_or(0.0);
            let y: f32 = y_str.parse().unwrap_or(0.0);
            let z: f32 = z_str.parse().unwrap_or(0.0);
            let yaw_deg: f32 = yaw_str.parse().unwrap_or(0.0);
            let yaw_rad = yaw_deg.to_radians();
            let speed: f32 = speed_str.parse().unwrap_or(1.0);

            // For a relative goto the target (x,y,z) is itself the displacement vector;
            // for an absolute goto the displacement is target minus current position.
            let distance = if relative {
                (x * x + y * y + z * z).sqrt()
            } else {
                let dx = x - current_pos[0];
                let dy = y - current_pos[1];
                let dz = z - current_pos[2];
                (dx * dx + dy * dy + dz * dz).sqrt()
            };

            // duration = distance / speed. The firmware uses a rest-to-rest min-snap profile
            // whose peak velocity is ~1.875x the average (= set speed) independent of distance,
            // so short moves need correspondingly short durations to actually hit the set speed -
            // no large floor. The tiny 0.1s floor only guards a near-zero-distance no-op move
            // (and a zero/negative speed) from producing an invalid duration.
            let duration: f32 = if speed > 0.0 {
                (distance / speed).max(0.1)
            } else {
                2.0
            };

            // Absolute world-space target for the visualization marker. A relative
            // goto displaces from the current position; an absolute goto is the target.
            let target = if relative {
                [current_pos[0] + x, current_pos[1] + y, current_pos[2] + z]
            } else {
                [x, y, z]
            };

            let swarm_state = swarm_state.clone();
            let goto_targets = goto_targets.clone();

            host::spawn(async move {
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

                goto_targets.lock().await.insert(original_index, target);

                eprintln!(
                    "Goto unit {} ({:.2}, {:.2}, {:.2}) yaw={:.1}deg relative={} dist={:.2}m at {:.2}m/s over {:.1}s...",
                    original_index, x, y, z, yaw_deg, relative, distance, speed, duration
                );
                if let Err(e) = cf.high_level_commander.go_to(x, y, z, yaw_rad, duration, relative, false, None).await {
                    eprintln!("Goto failed: {:?}", e);
                }
            });
        });
    }

    // HLC Land
    {
        let swarm_state = swarm_state.clone();
        let goto_targets = goto_targets.clone();
        let ui_weak = ui.as_weak();

        ui.on_hlc_land(move |row_index| {
            if row_index < 0 {
                return;
            }

            let (original_index, pos_z) = {
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
                let z = units.row_data(idx).map(|u| u.pos_z).unwrap_or(0.0);
                (idx, z)
            };

            let swarm_state = swarm_state.clone();
            let goto_targets = goto_targets.clone();

            host::spawn(async move {
                // Landing cancels any in-progress goto, so drop its marker.
                goto_targets.lock().await.remove(&original_index);

                let (cf, launch_z) = {
                    let state = swarm_state.lock().await;
                    match state.get(&original_index) {
                        Some(cu) => (cu.cf.clone(), cu.launch_z),
                        None => {
                            eprintln!("Unit {} is not connected", original_index);
                            return;
                        }
                    }
                };

                // Land back to the surface the unit launched from. The world origin
                // may be non-zero (even negative), so descend to the captured launch Z
                // rather than a hard-coded 0.0. Fall back to 0.0 if it never took off
                // through this app (e.g. connected mid-flight).
                let target_z = launch_z.unwrap_or(0.0);
                // Duration scales with the actual descent distance at 2 seconds per
                // meter, with a 1 second floor. Using the absolute distance keeps this
                // correct when pos_z or target_z is negative.
                let duration: f32 = ((pos_z - target_z).abs() * 2.0).max(1.0);

                eprintln!("Landing unit {} from {:.2}m to {:.2}m over {:.1}s...", original_index, pos_z, target_z, duration);
                if let Err(e) = cf.high_level_commander.land(target_z, None, duration, None).await {
                    eprintln!("Land failed: {:?}", e);
                }
            });
        });
    }

    // HLC Go Home
    {
        let swarm_state = swarm_state.clone();
        let ui_weak = ui.as_weak();

        ui.on_hlc_go_home(move |row_index| {
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

            host::spawn(async move {
                let (cf, home) = {
                    let state = swarm_state.lock().await;
                    match state.get(&original_index) {
                        Some(cu) => (cu.cf.clone(), cu.home_position),
                        None => {
                            eprintln!("Unit {} is not connected", original_index);
                            return;
                        }
                    }
                };

                let Some(home) = home else {
                    eprintln!("Unit {} has no saved home position (takeoff first)", original_index);
                    return;
                };

                eprintln!(
                    "Go home unit {} to ({:.2}, {:.2}, {:.2}) over 3.0s...",
                    original_index, home[0], home[1], home[2]
                );
                if let Err(e) = cf.high_level_commander.go_to(home[0], home[1], home[2], 0.0, 3.0, false, false, None).await {
                    eprintln!("Go home failed: {:?}", e);
                }
            });
        });
    }

    // Random goto inside a waypoint area: samples a point inside the waypoint
    // areas (avoiding obstacles), checks the straight path is clear, then flies.
    {
        let viz_scene_state = viz_scene_state.clone();
        let ui_weak = ui.as_weak();
        ui.on_random_goto(move |row_index| {
            let Some(ui) = ui_weak.upgrade() else { return };
            // Current position of the drone in this row (for path validation).
            let from = {
                let units = ui.get_units();
                let indices = sort_unit_indices(&units, ui.get_sort_column(), ui.get_sort_ascending());
                match indices.get(row_index.max(0) as usize).and_then(|&i| units.row_data(i)) {
                    Some(u) => [u.pos_x, u.pos_y, u.pos_z],
                    None => [0.0, 0.0, 0.0],
                }
            };
            let (waypoint_areas, obstacles) = {
                let s = viz_scene_state.lock().unwrap();
                (s.waypoint_areas.clone(), s.obstacles.clone())
            };
            if waypoint_areas.is_empty() {
                eprintln!("No waypoint area defined; load/save a scene with one first");
                return;
            }
            // Try a number of samples; require both the point and the path clear.
            let mut rng = planning::rng_seed();
            let mut target = None;
            for _ in 0..64 {
                if let Some(p) = planning::sample_waypoint(&mut rng, &waypoint_areas, &obstacles, 16) {
                    if !planning::segment_blocked(from, p, &obstacles) {
                        target = Some(p);
                        break;
                    }
                }
            }
            let Some(p) = target else {
                eprintln!("Could not find a reachable random waypoint (all blocked by obstacles)");
                return;
            };
            let x_str: slint::SharedString = format!("{:.2}", p[0]).into();
            let y_str: slint::SharedString = format!("{:.2}", p[1]).into();
            let z_str: slint::SharedString = format!("{:.2}", p[2]).into();
            ui.set_unit_goto_x_text(x_str.clone());
            ui.set_unit_goto_y_text(y_str.clone());
            ui.set_unit_goto_z_text(z_str.clone());
            // Trigger the same path as the Goto button (yaw=0, relative=false, 1.0 m/s)
            ui.invoke_hlc_goto(row_index, x_str, y_str, z_str, "0".into(), false, "1.0".into());
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

            host::spawn(async move {
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

                match cf.supervisor.send_arming_request(arm).await {
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

            host::spawn(async move {
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

                match cf.supervisor.send_crash_recovery_request().await {
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
}
