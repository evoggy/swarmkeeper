//! The Lighthouse setup wizard.

use slint::ComponentHandle;

use crate::app::AppContext;
use crate::{AppWindow, lh_wizard, WizardBsData, WizardSampleData};
use crate::host;

pub(crate) fn setup(ui: &AppWindow, cx: &AppContext) {
    let AppContext { swarm_state, wizard_state, .. } = cx;
    // LH Wizard callbacks
    {
        let ui_weak = ui.as_weak();
        let ws = wizard_state.clone();
        let uw = ui_weak.clone();
        ui.on_lh_wizard_step_next(move || {
            let mut ws = ws.lock().unwrap();
            let next = (ws.current_step as i32 + 1).min(4);
            ws.current_step = lh_wizard::WizardStep::from_index(next);
            if let Some(ui) = uw.upgrade() {
                ui.set_lh_wizard_current_step(next);
                ui.set_lh_wizard_step_instructions(ws.current_step.instructions().into());
                ui.set_lh_wizard_measure_button_text(ws.current_step.button_text().into());
            }
        });

        let ws = wizard_state.clone();
        let uw = ui_weak.clone();
        ui.on_lh_wizard_step_prev(move || {
            let mut ws = ws.lock().unwrap();
            let prev = (ws.current_step as i32 - 1).max(0);
            ws.current_step = lh_wizard::WizardStep::from_index(prev);
            if let Some(ui) = uw.upgrade() {
                ui.set_lh_wizard_current_step(prev);
                ui.set_lh_wizard_step_instructions(ws.current_step.instructions().into());
                ui.set_lh_wizard_measure_button_text(ws.current_step.button_text().into());
            }
        });

        let ws = wizard_state.clone();
        let uw = ui_weak.clone();
        ui.on_lh_wizard_clear(move || {
            let mut ws = ws.lock().unwrap();
            ws.container.clear_all_samples();
            ws.latest_solution = None;
            ws.current_step = lh_wizard::WizardStep::Origin;
            if let Some(ui) = uw.upgrade() {
                ui.set_lh_wizard_current_step(0);
                ui.set_lh_wizard_step_instructions(lh_wizard::WizardStep::Origin.instructions().into());
                ui.set_lh_wizard_measure_button_text(lh_wizard::WizardStep::Origin.button_text().into());
                ui.set_lh_wizard_origin_ok(false);
                ui.set_lh_wizard_x_axis_ok(false);
                ui.set_lh_wizard_xy_plane_ok(false);
                ui.set_lh_wizard_has_converged(false);
                ui.set_lh_wizard_error_mean("".into());
                ui.set_lh_wizard_error_max("".into());
                ui.set_lh_wizard_progress_info("".into());
                ui.set_lh_wizard_notification_text("Samples cleared".into());
                ui.set_lh_wizard_notification_color(slint::Color::from_argb_u8(255, 255, 235, 59));
                ui.set_lh_wizard_xy_plane_count(0);
                ui.set_lh_wizard_xyz_space_count(0);
                ui.set_lh_wizard_verification_count(0);
                ui.set_lh_wizard_sample_details(Default::default());
                ui.set_lh_wizard_bs_details(Default::default());
            }
        });

        let ws = wizard_state.clone();
        let uw = ui_weak.clone();
        let ss = swarm_state.clone();
        ui.on_lh_wizard_measure(move || {
            let ws = ws.clone();
            let uw = uw.clone();
            let ss = ss.clone();

            // Get selected CF index and current step
            let (selected_cf, current_step) = {
                let ws_lock = ws.lock().unwrap();
                (
                    if let Some(ui) = uw.upgrade() { ui.get_lh_wizard_selected_cf() } else { -1 },
                    ws_lock.current_step,
                )
            };

            if selected_cf < 0 {
                if let Some(ui) = uw.upgrade() {
                    ui.set_lh_wizard_notification_text("No Crazyflie selected".into());
                    ui.set_lh_wizard_notification_color(slint::Color::from_argb_u8(255, 255, 183, 77));
                }
                return;
            }

            // Set measuring state
            if let Some(ui) = uw.upgrade() {
                ui.set_lh_wizard_measuring(true);
                ui.set_lh_wizard_notification_text("Collecting angles...".into());
                ui.set_lh_wizard_notification_color(slint::Color::from_argb_u8(255, 255, 235, 59));
            }

            let unit_index = selected_cf as usize;

            host::spawn(async move {
                // Get CF from swarm state
                let cf = {
                    let state = ss.lock().await;
                    state.get(&unit_index).map(|u| u.cf.clone())
                };

                let Some(cf) = cf else {
                    slint::invoke_from_event_loop(move || {
                        if let Some(ui) = uw.upgrade() {
                            ui.set_lh_wizard_measuring(false);
                            ui.set_lh_wizard_notification_text("Crazyflie not connected".into());
                            ui.set_lh_wizard_notification_color(slint::Color::from_argb_u8(255, 244, 67, 54));
                        }
                    }).ok();
                    return;
                };

                // Enable angle stream
                if let Err(e) = cf.param.set_lossy("locSrv.enLhAngleStream", 1.0).await {
                    eprintln!("Failed to enable angle stream: {}", e);
                    slint::invoke_from_event_loop(move || {
                        if let Some(ui) = uw.upgrade() {
                            ui.set_lh_wizard_measuring(false);
                            ui.set_lh_wizard_notification_text("Failed to enable angle stream".into());
                            ui.set_lh_wizard_notification_color(slint::Color::from_argb_u8(255, 244, 67, 54));
                        }
                    }).ok();
                    return;
                };

                // Collect angle samples (average 50 readings per BS)
                use futures::StreamExt;
                let mut angle_stream = match cf.localization.lighthouse.angle_stream().await {
                    Ok(stream) => stream,
                    Err(e) => {
                        eprintln!("Failed to open the angle stream: {}", e);
                        let _ = cf.param.set_lossy("locSrv.enLhAngleStream", 0.0).await;
                        slint::invoke_from_event_loop(move || {
                            if let Some(ui) = uw.upgrade() {
                                ui.set_lh_wizard_measuring(false);
                                ui.set_lh_wizard_notification_text("Failed to open the angle stream".into());
                                ui.set_lh_wizard_notification_color(slint::Color::from_argb_u8(255, 244, 67, 54));
                            }
                        }).ok();
                        return;
                    }
                };
                let mut bs_samples: std::collections::HashMap<u8, Vec<([f32; 4], [f32; 4])>> = std::collections::HashMap::new();
                let samples_needed = 50;
                let timeout = std::time::Duration::from_secs(3);

                let result = host::timeout(timeout, async {
                    loop {
                        let all_have_enough = !bs_samples.is_empty() &&
                            bs_samples.values().all(|v| v.len() >= samples_needed);
                        if all_have_enough {
                            break;
                        }

                        if let Some(data) = angle_stream.next().await {
                            bs_samples.entry(data.base_station)
                                .or_default()
                                .push((data.angles.x, data.angles.y));
                        }
                    }
                }).await;

                // Disable angle stream
                let _ = cf.param.set_lossy("locSrv.enLhAngleStream", 0.0).await;

                // Debug: print collection results
                eprintln!("[LH Wizard] Angle collection done. Timeout: {}, BS count: {}", result.is_err(), bs_samples.len());
                for (bs_id, samples) in &bs_samples {
                    eprintln!("[LH Wizard]   BS {}: {} samples", bs_id, samples.len());
                    if let Some(first) = samples.first() {
                        eprintln!("[LH Wizard]     first x: {:?}", first.0);
                        eprintln!("[LH Wizard]     first y: {:?}", first.1);
                    }
                }

                if result.is_err() && bs_samples.is_empty() {
                    slint::invoke_from_event_loop(move || {
                        if let Some(ui) = uw.upgrade() {
                            ui.set_lh_wizard_measuring(false);
                            ui.set_lh_wizard_notification_text("Timeout - no angle data received".into());
                            ui.set_lh_wizard_notification_color(slint::Color::from_argb_u8(255, 244, 67, 54));
                        }
                    }).ok();
                    return;
                }

                // Average the collected angles and create a sample
                let mut averaged_angles = std::collections::HashMap::new();
                let bs_count = bs_samples.len();
                for (bs_id, samples) in &bs_samples {
                    let n = samples.len() as f64;
                    let mut avg_x = [0.0f64; 4];
                    let mut avg_y = [0.0f64; 4];
                    for (x, y) in samples {
                        for s in 0..4 {
                            avg_x[s] += x[s] as f64;
                            avg_y[s] += y[s] as f64;
                        }
                    }
                    for s in 0..4 {
                        avg_x[s] /= n;
                        avg_y[s] /= n;
                    }

                    // Convert to LighthouseBsVectors (V2 angles → V1 via from_lh2)
                    use crate::lh_geo::bs_vector::LighthouseBsVector;
                    let vectors = [
                        LighthouseBsVector::from_lh2(avg_x[0], avg_y[0]),
                        LighthouseBsVector::from_lh2(avg_x[1], avg_y[1]),
                        LighthouseBsVector::from_lh2(avg_x[2], avg_y[2]),
                        LighthouseBsVector::from_lh2(avg_x[3], avg_y[3]),
                    ];
                    averaged_angles.insert(*bs_id, vectors);
                }

                let sample = crate::lh_geo::sample::LhCfPoseSample::new(averaged_angles);
                eprintln!("[LH Wizard] Created sample with {} BS, uid={}", sample.angles_calibrated.len(), sample.uid());

                // Add sample to container based on current step
                {
                    let ws_lock = ws.lock().unwrap();
                    eprintln!("[LH Wizard] Adding sample as {:?}, container version before: {}", current_step, ws_lock.container.get_data_version());
                    match current_step {
                        lh_wizard::WizardStep::Origin => ws_lock.container.set_origin_sample(sample),
                        lh_wizard::WizardStep::XAxis => ws_lock.container.set_x_axis_sample(sample),
                        lh_wizard::WizardStep::XyPlane => ws_lock.container.append_xy_plane_sample(sample),
                        lh_wizard::WizardStep::XyzSpace => ws_lock.container.append_xyz_space_samples(vec![sample]),
                        lh_wizard::WizardStep::Verification => ws_lock.container.append_verification_samples(vec![sample]),
                    }
                }

                // Run solver in background
                let solution = {
                    let ws_lock = ws.lock().unwrap();
                    lh_wizard::run_solver(&ws_lock.container)
                };

                eprintln!("[LH Wizard] Solver done. converged={}, bs_poses={}, progress_ok={}, progress_info={}",
                    solution.has_converged, solution.bs_poses.len(), solution.progress_is_ok, solution.progress_info);

                let has_converged = solution.has_converged;
                let sample_details = lh_wizard::get_sample_details(&solution);
                let bs_details = lh_wizard::get_bs_details(&solution);
                let error_stats = solution.error_stats.clone();
                let origin_ok = solution.is_origin_sample_valid;
                let origin_info = solution.origin_sample_info.clone();
                let x_axis_ok = solution.is_x_axis_samples_valid;
                let x_axis_info = solution.x_axis_samples_info.clone();
                let xy_plane_ok = solution.is_xy_plane_samples_valid;
                let xy_plane_info = solution.xy_plane_samples_info.clone();
                let progress_info = solution.progress_info.clone();

                // Count samples by type
                let mut xy_count = 0i32;
                let mut xyz_count = 0i32;
                let mut verif_count = 0i32;
                for s in &solution.samples {
                    match s.sample_type {
                        crate::lh_geo::sample::LhCfPoseSampleType::XyPlane => xy_count += 1,
                        crate::lh_geo::sample::LhCfPoseSampleType::XyzSpace => xyz_count += 1,
                        crate::lh_geo::sample::LhCfPoseSampleType::Verification => verif_count += 1,
                        _ => {}
                    }
                }

                // Store solution
                ws.lock().unwrap().latest_solution = Some(solution);

                // Update UI
                let step_name = format!("{:?}", current_step);
                slint::invoke_from_event_loop(move || {
                    let Some(ui) = uw.upgrade() else { return };
                    ui.set_lh_wizard_measuring(false);
                    ui.set_lh_wizard_notification_text(
                        format!("{} measured ({} BS, {} samples avg)", step_name, bs_count,
                            samples_needed.min(bs_samples.values().map(|v| v.len()).min().unwrap_or(0))).into()
                    );
                    ui.set_lh_wizard_notification_color(slint::Color::from_argb_u8(255, 129, 199, 132));

                    // Update solution status
                    ui.set_lh_wizard_origin_ok(origin_ok);
                    ui.set_lh_wizard_origin_status(origin_info.into());
                    ui.set_lh_wizard_x_axis_ok(x_axis_ok);
                    ui.set_lh_wizard_x_axis_status(x_axis_info.into());
                    ui.set_lh_wizard_xy_plane_ok(xy_plane_ok);
                    ui.set_lh_wizard_xy_plane_status(xy_plane_info.into());
                    ui.set_lh_wizard_has_converged(has_converged);
                    ui.set_lh_wizard_progress_info(progress_info.into());
                    ui.set_lh_wizard_xy_plane_count(xy_count);
                    ui.set_lh_wizard_xyz_space_count(xyz_count);
                    ui.set_lh_wizard_verification_count(verif_count);

                    if let Some(stats) = &error_stats {
                        ui.set_lh_wizard_error_mean(format!("{:.4}m", stats.mean).into());
                        ui.set_lh_wizard_error_max(format!("{:.4}m", stats.max).into());
                    }

                    // Update sample details
                    let sd: Vec<WizardSampleData> = sample_details.iter().map(|s| WizardSampleData {
                        sample_type: s.sample_type.clone().into(),
                        x: s.x.clone().into(),
                        y: s.y.clone().into(),
                        z: s.z.clone().into(),
                        error: s.error.clone().into(),
                        is_verification: s.is_verification,
                        is_invalid: s.is_invalid,
                        is_large_error: s.is_large_error,
                    }).collect();
                    ui.set_lh_wizard_sample_details(slint::ModelRc::new(slint::VecModel::from(sd)));

                    // Update BS details
                    let bd: Vec<WizardBsData> = bs_details.iter().map(|b| WizardBsData {
                        id: b.id,
                        x: b.x.clone().into(),
                        y: b.y.clone().into(),
                        z: b.z.clone().into(),
                        samples: b.samples,
                        links: b.links,
                        low_links: b.low_links,
                    }).collect();
                    ui.set_lh_wizard_bs_details(slint::ModelRc::new(slint::VecModel::from(bd)));
                }).ok();
            });
        });

        // Import/Export session
        let ws = wizard_state.clone();
        let uw = ui_weak.clone();
        ui.on_lh_wizard_import(move || {
            let ws = ws.clone();
            let uw = uw.clone();
            slint::spawn_local(async move {
                let Some(handle) = host::dialog::FileDialog::new()
                    .add_filter("YAML", &["yaml", "yml"])
                    .pick_file().await
                else { return };
                let path = handle.path().to_path_buf();

                match host::fs::read_to_string(&path) {
                    Ok(yaml_str) => {
                        let ws_lock = ws.lock().unwrap();
                        match ws_lock.container.load_from_yaml(&yaml_str) {
                            Ok(()) => {
                                if let Some(ui) = uw.upgrade() {
                                    ui.set_lh_wizard_notification_text("Session imported".into());
                                    ui.set_lh_wizard_notification_color(slint::Color::from_argb_u8(255, 129, 199, 132));
                                }
                            }
                            Err(e) => {
                                eprintln!("Failed to import session: {}", e);
                            }
                        }
                    }
                    Err(e) => eprintln!("Failed to read file: {}", e),
                }
            }).unwrap();
        });

        let ws = wizard_state.clone();
        ui.on_lh_wizard_export(move || {
            let ws = ws.clone();
            slint::spawn_local(async move {
                let Some(handle) = host::dialog::FileDialog::new()
                    .add_filter("YAML", &["yaml", "yml"])
                    .set_file_name("lh_geo_session.yaml")
                    .save_file().await
                else { return };
                let path = handle.path().to_path_buf();

                let ws_lock = ws.lock().unwrap();
                match ws_lock.container.save_to_yaml() {
                    Ok(yaml_str) => {
                        if let Err(e) = host::fs::write(&path, yaml_str) {
                            eprintln!("Failed to write file: {}", e);
                        }
                    }
                    Err(e) => eprintln!("Failed to serialize: {}", e),
                }
            }).unwrap();
        });

        // Upload geometry - placeholder
        ui.on_lh_wizard_upload(move || {
            // TODO: Upload geometry to connected CFs
        });

        // CF selection - placeholder
        ui.on_lh_wizard_select_cf(move |_idx| {
            // TODO: Select which CF to use for measurements
        });

        // View interaction callbacks (camera rotation via mouse drag)
        let _uw = ui_weak.clone();
        ui.on_lh_wizard_view_mouse_pressed(move |_x, _y| {
            // Start camera rotation
        });

        let _uw = ui_weak.clone();
        ui.on_lh_wizard_view_mouse_moved(move |_x, _y| {
            // Camera rotation while dragging
            // The Slint touch area handles basic yaw/pitch updates
        });

        ui.on_lh_wizard_view_mouse_released(move || {});

        let uw = ui_weak.clone();
        ui.on_lh_wizard_view_pan(move |dx_px, dy_px| {
            let Some(ui) = uw.upgrade() else { return };
            let yaw = ui.get_lh_wizard_cam_yaw();
            let sy = yaw.sin();
            let cy = yaw.cos();
            let dx = dx_px * 0.01;
            let dy = dy_px * 0.01;
            ui.set_lh_wizard_cam_pan_x(
                ui.get_lh_wizard_cam_pan_x() + dx * sy + dy * cy,
            );
            ui.set_lh_wizard_cam_pan_y(
                ui.get_lh_wizard_cam_pan_y() + dx * (-cy) + dy * sy,
            );
        });
    }
}
