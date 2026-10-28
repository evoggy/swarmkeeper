//! Trajectory actions for the swarm and for one unit.

use std::collections::HashMap;
use std::sync::Arc;

use slint::{ComponentHandle, Model};

use crate::app::{AppContext, show_error_dialog};
use crate::swarm::client_units;
use crate::trajectory::{build_wave_compressed, generate_primitive, PRIMITIVE_SPEED, sample_trajectory, stack_trajectory_z, TRAJECTORY_TAKEOFF_RISE, trajectory_within_flight_areas, TrajectoryConfig, TrajectoryData, upload_compressed_trajectory_to_unit, upload_trajectory_config_to_unit, UploadedTrajectory};
use crate::units::sort_unit_indices;
use crate::{AppWindow, UnitState};

pub(crate) fn setup(ui: &AppWindow, cx: &AppContext) {
    let AppContext { swarm_state, viz_scene_state, trajectory_data, .. } = cx;
    // Upload trajectory to all connected units in parallel
    {
        let swarm_state = swarm_state.clone();
        let trajectory_data = trajectory_data.clone();
        let ui_weak = ui.as_weak();

        ui.on_upload_trajectory_swarm(move || {
            let swarm_state = swarm_state.clone();
            let trajectory_data = trajectory_data.clone();
            let ui_weak = ui_weak.clone();

            tokio::spawn(async move {
                // Open file dialog
                let Some(handle) = rfd::AsyncFileDialog::new()
                    .add_filter("YAML", &["yaml", "yml"])
                    .pick_file()
                    .await
                else { return };
                let path = handle.path().to_path_buf();

                // Parse trajectory
                let contents = match std::fs::read_to_string(&path) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("Failed to read trajectory file: {}", e);
                        return;
                    }
                };
                let traj_config: TrajectoryConfig = match serde_yaml::from_str(&contents) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("Failed to parse trajectory file: {}", e);
                        return;
                    }
                };

                let viz_points = sample_trajectory(&traj_config);
                let total_duration: f32 = traj_config.segments.iter().map(|s| s.duration).sum();
                let segment_count = traj_config.segments.len();
                let uploaded = UploadedTrajectory {
                    name: path.file_stem().and_then(|n| n.to_str()).unwrap_or("trajectory").to_string(),
                    config: traj_config.clone(),
                };

                // Convert to Poly4D segments
                use crazyflie_connection_server::memory::{Poly4D, Poly};
                let segments: Vec<Poly4D> = traj_config
                    .segments
                    .iter()
                    .map(|s| {
                        Poly4D::new(
                            s.duration,
                            Poly::from_slice(&s.x),
                            Poly::from_slice(&s.y),
                            Poly::from_slice(&s.z),
                            Poly::from_slice(&s.yaw),
                        )
                    })
                    .collect();
                let segments = Arc::new(segments);

                // Get all connected units
                let connected_units: Vec<(usize, Arc<crazyflie_connection_server::Crazyflie>)> = {
                    let state = swarm_state.lock().await;
                    client_units(&state)
                };

                let total = connected_units.len();
                if total == 0 {
                    eprintln!("No connected units for trajectory upload");
                    return;
                }

                // Show progress dialog
                {
                    let ui_weak_inner = ui_weak.clone();
                    slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_weak_inner.upgrade() {
                            ui.set_progress_dialog_title("Uploading Trajectory".into());
                            ui.set_progress_dialog_progress(0.0);
                            ui.set_progress_dialog_status("Starting...".into());
                            ui.set_progress_dialog_visible(true);
                        }
                    }).ok();
                }

                let completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));

                let mut join_set = tokio::task::JoinSet::new();
                for (unit_idx, cf) in &connected_units {
                    let unit_idx = *unit_idx;
                    let cf = cf.clone();
                    let ui_weak = ui_weak.clone();
                    let completed = completed.clone();
                    let segments = segments.clone();

                    join_set.spawn(async move {
                        use crazyflie_connection_server::memory::{MemoryType, TrajectoryMemory};

                        let mut success = true;
                        let traj_mems = cf.memory.get_memories(Some(MemoryType::Trajectory));
                        if let Some(mem) = traj_mems.first() {
                            if let Ok(traj_mem) = cf.memory.open_memory::<TrajectoryMemory>(mem).await {
                                match traj_mem.write_uncompressed(&segments, 0).await {
                                    Ok(bytes) => eprintln!("Unit {}: uploaded {} bytes", unit_idx, bytes),
                                    Err(e) => {
                                        eprintln!("Unit {}: failed to upload trajectory: {:?}", unit_idx, e);
                                        success = false;
                                    }
                                }
                            }
                        }

                        if success {
                            if let Err(e) = cf.high_level_commander
                                .define_trajectory(1, 0, segment_count as u8, None)
                                .await
                            {
                                eprintln!("Unit {}: failed to define trajectory: {:?}", unit_idx, e);
                            }
                        }

                        let done = completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                        let progress = done as f32 / total as f32;
                        let status: slint::SharedString = format!("Uploaded ({}/{})", done, total).into();
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

                // Store trajectory visualization data for each connected unit
                {
                    let mut td = trajectory_data.lock().await;
                    for (unit_idx, _) in &connected_units {
                        td.insert(*unit_idx, TrajectoryData {
                            points: viz_points.clone(),
                            duration: total_duration,
                            anchor: None,
                            saved_points: None,
                            uploaded: Some(uploaded.clone()),
                        });
                    }
                }

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

    // Fly trajectory on all connected units simultaneously
    {
        let swarm_state = swarm_state.clone();
        let trajectory_data = trajectory_data.clone();
        let viz_scene_state = viz_scene_state.clone();
        let ui_weak = ui.as_weak();
        ui.on_fly_trajectory_swarm(move || {
            let swarm_state = swarm_state.clone();
            let trajectory_data = trajectory_data.clone();
            let viz_scene_state = viz_scene_state.clone();
            let ui_weak = ui_weak.clone();

            tokio::spawn(async move {
                let connected_units: Vec<(usize, Arc<crazyflie_connection_server::Crazyflie>)> = {
                    let state = swarm_state.lock().await;
                    client_units(&state)
                };

                if connected_units.is_empty() {
                    eprintln!("No connected units");
                    return;
                }

                // Pre-flight geofence check: abort the whole flight if any unit's
                // trajectory would leave the flight area at its takeoff-height anchor.
                {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    let ui_weak_inner = ui_weak.clone();
                    let unit_indices: Vec<usize> = connected_units.iter().map(|(idx, _)| *idx).collect();
                    let _ = slint::invoke_from_event_loop(move || {
                        let mut positions = HashMap::new();
                        if let Some(ui) = ui_weak_inner.upgrade() {
                            let units = ui.get_units();
                            for idx in &unit_indices {
                                if let Some(u) = units.row_data(*idx) {
                                    positions.insert(*idx, [u.pos_x, u.pos_y, u.pos_z]);
                                }
                            }
                        }
                        let _ = tx.send(positions);
                    });
                    let positions = rx.await.unwrap_or_default();
                    let areas = match viz_scene_state.lock() {
                        Ok(s) => s.flight_areas.clone(),
                        Err(_) => Vec::new(),
                    };
                    if !areas.is_empty() {
                        let mut offenders: Vec<usize> = Vec::new();
                        {
                            let td = trajectory_data.lock().await;
                            for (idx, _) in &connected_units {
                                let Some(pos) = positions.get(idx) else { continue };
                                let Some(data) = td.get(idx) else { continue };
                                let anchor = [pos[0], pos[1], pos[2] + TRAJECTORY_TAKEOFF_RISE];
                                if trajectory_within_flight_areas(&data.points, anchor, &areas).is_err() {
                                    offenders.push(*idx);
                                }
                            }
                        }
                        if !offenders.is_empty() {
                            let list = offenders.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(", ");
                            eprintln!("Trajectory leaves flight area for unit(s) {} - aborting swarm flight", list);
                            show_error_dialog(
                                &ui_weak,
                                "Trajectory outside flight area".into(),
                                format!(
                                    "The trajectory would leave the flight area for unit(s) {}. \
                                     No units were flown - reduce the size or reposition.",
                                    list
                                ),
                            );
                            return;
                        }
                    }
                }

                // Arm all units in parallel
                eprintln!("Arming {} units...", connected_units.len());
                let mut join_set = tokio::task::JoinSet::new();
                for (idx, cf) in &connected_units {
                    let cf = cf.clone();
                    let idx = *idx;
                    join_set.spawn(async move {
                        if let Err(e) = cf.supervisor.send_arming_request(true).await {
                            eprintln!("Unit {}: arming failed: {:?}", idx, e);
                        }
                    });
                }
                while join_set.join_next().await.is_some() {}
                // Wait for propellers to spin up before taking off
                tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

                // Snapshot positions before takeoff and show takeoff line. The supervisor's
                // "is flying" bit comes along so we can tell a ground start from a unit that
                // is still hovering after a previous run.
                let launch_positions = {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    let ui_weak_inner = ui_weak.clone();
                    let unit_indices: Vec<usize> = connected_units.iter().map(|(idx, _)| *idx).collect();
                    let _ = slint::invoke_from_event_loop(move || {
                        let mut positions = HashMap::new();
                        if let Some(ui) = ui_weak_inner.upgrade() {
                            let units = ui.get_units();
                            for idx in &unit_indices {
                                if let Some(u) = units.row_data(*idx) {
                                    positions.insert(
                                        *idx,
                                        ([u.pos_x, u.pos_y, u.pos_z], u.state == UnitState::Flying),
                                    );
                                }
                            }
                        }
                        let _ = tx.send(positions);
                    });
                    let positions: HashMap<usize, ([f32; 3], bool)> = rx.await.unwrap_or_default();
                    {
                        let mut td = trajectory_data.lock().await;
                        for (idx, (pos, _)) in &positions {
                            if let Some(data) = td.get_mut(idx) {
                                // Save real trajectory points and show a takeoff line instead
                                data.saved_points = Some(std::mem::take(&mut data.points));
                                data.points = vec![[0.0, 0.0, 0.0], [0.0, 0.0, TRAJECTORY_TAKEOFF_RISE]];
                                data.anchor = Some(*pos);
                            }
                        }
                    }
                    // Record each launch surface for "Land", but only for units actually
                    // standing on it. A unit still hovering from a previous run would
                    // otherwise register its mid-air position as the ground, and Land would
                    // cut its motors at that height. See the single-unit path for detail.
                    {
                        let mut state = swarm_state.lock().await;
                        for (idx, (pos, flying)) in &positions {
                            if !flying {
                                if let Some(cu) = state.get_mut(idx) {
                                    cu.launch_z = Some(pos[2]);
                                }
                            }
                        }
                    }
                    positions
                };

                // Take off all units in parallel. take_off() wants an absolute world-frame
                // height, so each unit rises above its own launch surface - the origin is not
                // necessarily the floor, and rising above the *current* position would stack
                // another 0.5 m on top every time this is pressed while units still hover.
                let takeoff_heights: HashMap<usize, f32> = {
                    let state = swarm_state.lock().await;
                    connected_units
                        .iter()
                        .map(|(idx, _)| {
                            let base_z = state
                                .get(idx)
                                .and_then(|cu| cu.launch_z)
                                .or(launch_positions.get(idx).map(|(p, _)| p[2]))
                                .unwrap_or(0.0);
                            (*idx, base_z + TRAJECTORY_TAKEOFF_RISE)
                        })
                        .collect()
                };

                eprintln!("Taking off...");
                let mut join_set = tokio::task::JoinSet::new();
                for (idx, cf) in &connected_units {
                    let cf = cf.clone();
                    let idx = *idx;
                    let takeoff_z = takeoff_heights.get(&idx).copied().unwrap_or(TRAJECTORY_TAKEOFF_RISE);
                    join_set.spawn(async move {
                        if let Err(e) = cf.high_level_commander.take_off(takeoff_z, None, 2.0, None).await {
                            eprintln!("Unit {}: take-off failed: {:?}", idx, e);
                        }
                    });
                }
                while join_set.join_next().await.is_some() {}
                tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;

                // Restore real trajectory points and snapshot post-takeoff positions as anchors
                {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    let ui_weak_inner = ui_weak.clone();
                    let unit_indices: Vec<usize> = connected_units.iter().map(|(idx, _)| *idx).collect();
                    let _ = slint::invoke_from_event_loop(move || {
                        let mut positions = HashMap::new();
                        if let Some(ui) = ui_weak_inner.upgrade() {
                            let units = ui.get_units();
                            for idx in &unit_indices {
                                if let Some(u) = units.row_data(*idx) {
                                    positions.insert(*idx, [u.pos_x, u.pos_y, u.pos_z]);
                                }
                            }
                        }
                        let _ = tx.send(positions);
                    });
                    if let Ok(positions) = rx.await {
                        let mut td = trajectory_data.lock().await;
                        for (idx, pos) in positions {
                            if let Some(data) = td.get_mut(&idx) {
                                // Restore the real trajectory points
                                if let Some(saved) = data.saved_points.take() {
                                    data.points = saved;
                                }
                                data.anchor = Some(pos);
                            }
                        }
                    }
                }

                // Start trajectory on all units as close together as possible
                eprintln!("Starting trajectory on all units...");
                for (idx, cf) in &connected_units {
                    if let Err(e) = cf
                        .high_level_commander
                        .start_trajectory(1, 1.0, true, false, false, None)
                        .await
                    {
                        eprintln!("Unit {}: start trajectory failed: {:?}", idx, e);
                    }
                }
            });
        });
    }

    // Upload trajectory
    {
        let swarm_state = swarm_state.clone();
        let trajectory_data = trajectory_data.clone();
        let ui_weak = ui.as_weak();

        ui.on_upload_trajectory(move |row_index| {
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
            let trajectory_data = trajectory_data.clone();
            let ui_weak = ui_weak.clone();

            tokio::spawn(async move {
                // Open file dialog
                let Some(handle) = rfd::AsyncFileDialog::new()
                    .add_filter("YAML", &["yaml", "yml"])
                    .pick_file()
                    .await
                else { return };
                let path = handle.path().to_path_buf();

                // Parse trajectory
                let contents = match std::fs::read_to_string(&path) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("Failed to read trajectory file: {}", e);
                        return;
                    }
                };
                let traj_config: TrajectoryConfig = match serde_yaml::from_str(&contents) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("Failed to parse trajectory file: {}", e);
                        return;
                    }
                };

                let name = path
                    .file_stem()
                    .and_then(|n| n.to_str())
                    .unwrap_or("trajectory")
                    .to_string();

                if let Err(msg) =
                    upload_trajectory_config_to_unit(original_index, traj_config, &name, swarm_state, trajectory_data).await
                {
                    show_error_dialog(&ui_weak, "Trajectory upload failed".into(), msg);
                }
            });
        });
    }

    // Generate and upload a primitive trajectory (circle / square / cube) to the
    // selected unit, then visualize it. Press "Fly Trajectory" to execute.
    {
        let swarm_state = swarm_state.clone();
        let trajectory_data = trajectory_data.clone();
        let ui_weak = ui.as_weak();

        ui.on_generate_primitive(move |row_index, shape, size_str, size_y_str, z_offset_str, z_iters_str| {
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

            let size: f32 = size_str.trim().parse().unwrap_or(0.5);
            let size_y: f32 = size_y_str.trim().parse().unwrap_or(size);
            let z_offset: f32 = z_offset_str.trim().parse().unwrap_or(0.0);
            let z_iterations: u32 = z_iters_str.trim().parse().unwrap_or(1).max(1);
            let shape = shape.to_string();
            let swarm_state = swarm_state.clone();
            let trajectory_data = trajectory_data.clone();
            let ui_weak = ui_weak.clone();

            tokio::spawn(async move {
                let Some(traj_config) = generate_primitive(&shape, size, size_y) else {
                    eprintln!("Unknown primitive shape: {}", shape);
                    return;
                };
                let traj_config =
                    stack_trajectory_z(traj_config, z_offset, z_iterations, PRIMITIVE_SPEED);
                let size_text = if shape == "rectangle" {
                    format!("{:.2} x {:.2}m", size, size_y)
                } else {
                    format!("{:.2}m", size)
                };
                eprintln!(
                    "Generated {} trajectory (size {}, {} z-layer(s) {:.2}m apart, {} segments)",
                    shape,
                    size_text,
                    z_iterations,
                    z_offset,
                    traj_config.segments.len()
                );
                if let Err(msg) =
                    upload_trajectory_config_to_unit(original_index, traj_config, &shape, swarm_state, trajectory_data).await
                {
                    show_error_dialog(&ui_weak, "Trajectory upload failed".into(), msg);
                }
            });
        });
    }

    // Generate and upload a back-and-forth "wave" trajectory to the selected unit:
    // a square wave (X/Y moves alternate) or a zig-zag/sawtooth (X/Y move together).
    {
        let swarm_state = swarm_state.clone();
        let trajectory_data = trajectory_data.clone();
        let ui_weak = ui.as_weak();

        ui.on_generate_wave(move |row_index, x_str, y_str, reps_str, simultaneous, speed_str, z_offset_str, z_iters_str| {
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

            let x_step: f32 = x_str.trim().parse().unwrap_or(0.3);
            let y_amp: f32 = y_str.trim().parse().unwrap_or(0.3);
            let cycles: u32 = reps_str.trim().parse().unwrap_or(3);
            let speed: f32 = speed_str.trim().parse().unwrap_or(PRIMITIVE_SPEED);
            let z_offset: f32 = z_offset_str.trim().parse().unwrap_or(0.0);
            let z_iterations: u32 = z_iters_str.trim().parse().unwrap_or(1).max(1);
            let swarm_state = swarm_state.clone();
            let trajectory_data = trajectory_data.clone();
            let ui_weak = ui_weak.clone();

            tokio::spawn(async move {
                let (start, segments, viz_points, total_duration, poly_config) = build_wave_compressed(
                    x_step,
                    y_amp,
                    cycles,
                    simultaneous,
                    speed,
                    z_offset,
                    z_iterations,
                );
                eprintln!(
                    "Generated {} trajectory (x step {:.2}m, y amp {:.2}m, {} reps, {:.2} m/s, \
                     {} z-layer(s) {:.2}m apart)",
                    if simultaneous { "zig-zag" } else { "square-wave" },
                    x_step,
                    y_amp,
                    cycles,
                    speed,
                    z_iterations,
                    z_offset
                );
                let uploaded = UploadedTrajectory {
                    name: if simultaneous { "zigzag".into() } else { "square-wave".into() },
                    config: poly_config,
                };
                if let Err(msg) = upload_compressed_trajectory_to_unit(
                    original_index,
                    start,
                    segments,
                    viz_points,
                    total_duration,
                    Some(uploaded),
                    swarm_state,
                    trajectory_data,
                )
                .await
                {
                    show_error_dialog(&ui_weak, "Trajectory upload failed".into(), msg);
                }
            });
        });
    }

    // Save the trajectory currently loaded for a unit back out to a YAML file. Generated
    // shapes only ever live in memory (and in the unit's trajectory memory), so this is the
    // only way to keep one after generating and flying it.
    {
        let trajectory_data = trajectory_data.clone();
        let ui_weak = ui.as_weak();

        ui.on_download_trajectory(move |row_index| {
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

            let trajectory_data = trajectory_data.clone();
            let ui_weak = ui_weak.clone();

            tokio::spawn(async move {
                let uploaded = {
                    let td = trajectory_data.lock().await;
                    td.get(&original_index).and_then(|d| d.uploaded.clone())
                };
                let Some(uploaded) = uploaded else {
                    show_error_dialog(
                        &ui_weak,
                        "No trajectory to download".into(),
                        format!(
                            "Unit {} has no trajectory loaded. Upload one or generate a \
                             shape first, then download it.",
                            original_index
                        ),
                    );
                    return;
                };

                let yaml = match serde_yaml::to_string(&uploaded.config) {
                    Ok(y) => y,
                    Err(e) => {
                        show_error_dialog(
                            &ui_weak,
                            "Trajectory download failed".into(),
                            format!("Serializing the trajectory failed: {}", e),
                        );
                        return;
                    }
                };

                let Some(handle) = rfd::AsyncFileDialog::new()
                    .add_filter("YAML", &["yaml", "yml"])
                    .set_file_name(format!("{}.yaml", uploaded.name))
                    .set_directory(std::env::current_dir().unwrap_or_default())
                    .save_file()
                    .await
                else { return };
                let path = handle.path().to_path_buf();

                if let Err(e) = std::fs::write(&path, yaml) {
                    show_error_dialog(
                        &ui_weak,
                        "Trajectory download failed".into(),
                        format!("Writing {} failed: {}", path.display(), e),
                    );
                    return;
                }
                eprintln!(
                    "Saved trajectory for unit {} ({} segments) to {:?}",
                    original_index,
                    uploaded.config.segments.len(),
                    path
                );
            });
        });
    }

    // Fly trajectory
    {
        let swarm_state = swarm_state.clone();
        let trajectory_data = trajectory_data.clone();
        let viz_scene_state = viz_scene_state.clone();
        let ui_weak = ui.as_weak();

        ui.on_fly_trajectory(move |row_index| {
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
            let trajectory_data = trajectory_data.clone();
            let viz_scene_state = viz_scene_state.clone();
            let ui_weak = ui_weak.clone();

            tokio::spawn(async move {
                let (duration, points) = {
                    let td = trajectory_data.lock().await;
                    match td.get(&original_index) {
                        Some(d) => (d.duration, d.points.clone()),
                        None => {
                            eprintln!("No trajectory uploaded for unit {}", original_index);
                            return;
                        }
                    }
                };

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

                // Pre-flight geofence check: abort before arming if the trajectory
                // would leave the flight area at its takeoff-height anchor.
                {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    let ui_weak_inner = ui_weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        let pos = ui_weak_inner.upgrade().and_then(|ui| {
                            ui.get_units().row_data(original_index).map(|u| [u.pos_x, u.pos_y, u.pos_z])
                        });
                        let _ = tx.send(pos);
                    });
                    if let Ok(Some(pos)) = rx.await {
                        let anchor = [pos[0], pos[1], pos[2] + TRAJECTORY_TAKEOFF_RISE];
                        let areas = match viz_scene_state.lock() {
                            Ok(s) => s.flight_areas.clone(),
                            Err(_) => Vec::new(),
                        };
                        if let Err(bad) = trajectory_within_flight_areas(&points, anchor, &areas) {
                            eprintln!(
                                "Unit {}: trajectory leaves flight area at ({:.2}, {:.2}, {:.2}) - aborting",
                                original_index, bad[0], bad[1], bad[2]
                            );
                            show_error_dialog(
                                &ui_weak,
                                "Trajectory outside flight area".into(),
                                format!(
                                    "The trajectory would leave the flight area at \
                                     ({:.2}, {:.2}, {:.2}) m. Execution aborted - reduce the \
                                     size or reposition the unit.",
                                    bad[0], bad[1], bad[2]
                                ),
                            );
                            return;
                        }
                    }
                }

                eprintln!("Arming...");
                if let Err(e) = cf.supervisor.send_arming_request(true).await {
                    eprintln!("Arming failed: {:?}", e);
                    return;
                }
                // Wait for propellers to spin up before taking off
                tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

                // Snapshot position before takeoff and show takeoff line. The supervisor's
                // "is flying" bit comes along so we can tell a ground start from a unit that
                // is still hovering after a previous run.
                let launch_pos = {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    let ui_weak_inner = ui_weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        let snapshot = if let Some(ui) = ui_weak_inner.upgrade() {
                            let units = ui.get_units();
                            units.row_data(original_index).map(|u| {
                                ([u.pos_x, u.pos_y, u.pos_z], u.state == UnitState::Flying)
                            })
                        } else {
                            None
                        };
                        let _ = tx.send(snapshot);
                    });
                    let snapshot = rx.await.ok().flatten();
                    if let Some((pos, _)) = snapshot {
                        let mut td = trajectory_data.lock().await;
                        if let Some(data) = td.get_mut(&original_index) {
                            data.saved_points = Some(std::mem::take(&mut data.points));
                            data.points = vec![[0.0, 0.0, 0.0], [0.0, 0.0, TRAJECTORY_TAKEOFF_RISE]];
                            data.anchor = Some(pos);
                        }
                    }
                    snapshot
                };

                // Record the launch surface for "Land", but only while the unit is actually
                // standing on it. Flying a trajectory leaves the unit hovering at its final
                // setpoint, so on a second run this position is mid-air - taking that as the
                // launch surface would make Land "arrive" at the current height and cut the
                // motors, dropping the unit. While airborne, keep whatever surface we already
                // captured.
                if let Some((pos, flying)) = launch_pos {
                    if !flying {
                        let mut state = swarm_state.lock().await;
                        if let Some(cu) = state.get_mut(&original_index) {
                            cu.launch_z = Some(pos[2]);
                        }
                    }
                }

                // take_off() wants an absolute world-frame height. Rise above the *launch
                // surface* rather than above wherever the unit currently is, so pressing Fly
                // Trajectory again while it hovers returns it to the same height instead of
                // stacking another rise on top of the last one.
                let takeoff_z = {
                    let base_z = {
                        let state = swarm_state.lock().await;
                        state.get(&original_index).and_then(|cu| cu.launch_z)
                    }
                    .or(launch_pos.map(|(p, _)| p[2]))
                    .unwrap_or(0.0);
                    base_z + TRAJECTORY_TAKEOFF_RISE
                };
                eprintln!("Taking off to {:.2}m...", takeoff_z);
                if let Err(e) = cf.high_level_commander.take_off(takeoff_z, None, 2.0, None).await {
                    eprintln!("Take-off failed: {:?}", e);
                }
                tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;

                // Restore real trajectory points and snapshot post-takeoff position as anchor
                {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    let ui_weak_inner = ui_weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        let pos = if let Some(ui) = ui_weak_inner.upgrade() {
                            let units = ui.get_units();
                            units.row_data(original_index).map(|u| [u.pos_x, u.pos_y, u.pos_z])
                        } else {
                            None
                        };
                        let _ = tx.send(pos);
                    });
                    if let Ok(Some(pos)) = rx.await {
                        let mut td = trajectory_data.lock().await;
                        if let Some(data) = td.get_mut(&original_index) {
                            if let Some(saved) = data.saved_points.take() {
                                data.points = saved;
                            }
                            data.anchor = Some(pos);
                        }
                    }
                }

                eprintln!("Starting trajectory ({:.1}s)...", duration);
                if let Err(e) = cf
                    .high_level_commander
                    .start_trajectory(1, 1.0, true, false, false, None)
                    .await
                {
                    eprintln!("Start trajectory failed: {:?}", e);
                }
                // Let the trajectory play out, then leave the unit hovering at its final
                // setpoint (the HLC holds the last position) rather than landing. Use the
                // Land button to bring it down.
                tokio::time::sleep(tokio::time::Duration::from_secs_f32(duration + 0.5)).await;
                eprintln!("Trajectory complete - hovering");
            });
        });
    }

    // Clear trajectory
    {
        let trajectory_data = trajectory_data.clone();
        let ui_weak = ui.as_weak();

        ui.on_clear_trajectory(move |row_index| {
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

            let trajectory_data = trajectory_data.clone();
            tokio::spawn(async move {
                let mut td = trajectory_data.lock().await;
                td.remove(&original_index);
                eprintln!("Cleared trajectory for unit {}", original_index);
            });
        });
    }
}
