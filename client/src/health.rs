//! The health test.

use std::collections::HashMap;
use std::sync::Arc;

use slint::{ComponentHandle, Model};

use crate::app::AppContext;
use crate::swarm::client_units;
use crate::swarm_file::uri_with_link_timeout;
use crate::{AppWindow, HealthTestResult};
use crate::host;

pub(crate) fn setup(ui: &AppWindow, cx: &AppContext) {
    let AppContext { connection, swarm_state, .. } = cx;
    // Health test handler
    {
        let connection = connection.clone();
        let swarm_state = swarm_state.clone();
        let ui_weak = ui.as_weak();
        ui.on_run_health_test(move || {
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
                eprintln!("No units configured for health test");
                return;
            }

            // Initialize results
            let initial_results: Vec<HealthTestResult> = unit_info
                .iter()
                .map(|(_, name)| HealthTestResult {
                    name: name.clone().into(),
                    status: "Waiting".into(),
                    m1_pass: false,
                    m2_pass: false,
                    m3_pass: false,
                    m4_pass: false,
                })
                .collect();
            let results_model = std::rc::Rc::new(slint::VecModel::from(initial_results));
            ui_ref.set_health_test_results(results_model.clone().into());
            ui_ref.set_health_test_progress(0.0);
            ui_ref.set_health_test_status("Starting...".into());
            ui_ref.set_health_test_running(true);

            let connection = connection.clone();
            let swarm_state = swarm_state.clone();
            let ui_weak = ui_weak.clone();

            host::spawn(async move {
                let completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                // Connected units are tested through their connection, the
                // others are attached to for the test
                let connected_units: HashMap<usize, Arc<crazyflie_connection_server::Crazyflie>> = {
                    let state = swarm_state.lock().await;
                    client_units(&state).into_iter().collect()
                };
                let client = connection.read().await.client().clone();

                let mut join_set = host::JoinSet::new();
                for (idx, (uri, name)) in unit_info.into_iter().enumerate() {
                    let ui_weak = ui_weak.clone();
                    let completed = completed.clone();
                    let existing_cf = connected_units.get(&idx).cloned();
                    let client = client.clone();

                    join_set.spawn(async move {
                        // Update status to "Connecting"
                        {
                            let ui_weak = ui_weak.clone();
                            slint::invoke_from_event_loop(move || {
                                if let Some(ui) = ui_weak.upgrade() {
                                    let results = ui.get_health_test_results();
                                    if let Some(mut r) = results.row_data(idx) {
                                        r.status = "Connecting".into();
                                        results.set_row_data(idx, r);
                                    }
                                }
                            }).ok();
                        }

                        eprintln!("Health test {}: connecting to {}...", name, uri);
                        let temp_connection = existing_cf.is_none();
                        let attached = match existing_cf {
                            Some(cf) => Ok(Ok(cf)),
                            None => host::timeout(
                                std::time::Duration::from_secs(30),
                                client.attach(&uri_with_link_timeout(&uri)),
                            )
                            .await
                            .map(|attached| attached.map(Arc::new)),
                        };
                        let cf = match attached {
                            Ok(Ok(cf)) => cf,
                            Ok(Err(e)) => {
                                eprintln!("Health test {}: connect FAILED: {:?}", name, e);
                                let done = completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                                let progress = done as f32 / total as f32;
                                let ui_weak = ui_weak.clone();
                                slint::invoke_from_event_loop(move || {
                                    if let Some(ui) = ui_weak.upgrade() {
                                        let results = ui.get_health_test_results();
                                        if let Some(mut r) = results.row_data(idx) {
                                            r.status = "Error".into();
                                            results.set_row_data(idx, r);
                                        }
                                        ui.set_health_test_progress(progress);
                                        ui.set_health_test_status(format!("{}/{} done", done, total).into());
                                    }
                                }).ok();
                                return;
                            }
                            Err(_) => {
                                eprintln!("Health test {}: connect timed out", name);
                                let done = completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                                let progress = done as f32 / total as f32;
                                let ui_weak = ui_weak.clone();
                                slint::invoke_from_event_loop(move || {
                                    if let Some(ui) = ui_weak.upgrade() {
                                        let results = ui.get_health_test_results();
                                        if let Some(mut r) = results.row_data(idx) {
                                            r.status = "Error".into();
                                            results.set_row_data(idx, r);
                                        }
                                        ui.set_health_test_progress(progress);
                                        ui.set_health_test_status(format!("{}/{} done", done, total).into());
                                    }
                                }).ok();
                                return;
                            }
                        };

                        // Update status to "Testing"
                        {
                            let ui_weak = ui_weak.clone();
                            slint::invoke_from_event_loop(move || {
                                if let Some(ui) = ui_weak.upgrade() {
                                    let results = ui.get_health_test_results();
                                    if let Some(mut r) = results.row_data(idx) {
                                        r.status = "Testing".into();
                                        results.set_row_data(idx, r);
                                    }
                                }
                            }).ok();
                        }

                        // Set the PWM ratio (required for brushless motors), then start the
                        // prop test. Create the log block for health.motorPass BEFORE starting
                        // so we don't miss the result.
                        let motor_pass_result = async {
                            cf.param.set_lossy("health.propTestPWMRatio", 10000.0).await?;
                            cf.param.set_lossy("health.propTestThreshold", 1.0).await?;

                            let mut log_stream = cf
                                .log
                                .subscribe(&["health.motorPass"], std::time::Duration::from_millis(100))
                                .await?;

                            // Now start the prop test
                            cf.param.set_lossy("health.startPropTest", 1.0).await?;
                            eprintln!("Health test {}: prop test started", name);

                            let result = host::timeout(
                                std::time::Duration::from_secs(15),
                                async {
                                    loop {
                                        let data = log_stream.next().await?;
                                        let motor_pass =
                                            data.data.get("health.motorPass").copied().unwrap_or(0.0) as u8;
                                        // Firmware sets the per-motor pass bits (0..=3)
                                        // incrementally as it tests each motor, and sets bit 7
                                        // (HEALTH_MOTOR_TEST_FINISHED_BIT) only when the whole
                                        // test is done. Wait for that finished bit so we read the
                                        // final result instead of a partial, mid-test value.
                                        if motor_pass & 0x80 != 0 {
                                            return Ok::<u8, Box<dyn std::error::Error + Send + Sync>>(motor_pass);
                                        }
                                    }
                                },
                            )
                            .await;

                            match result {
                                Ok(Ok(motor_pass)) => Ok(Some(motor_pass)),
                                Ok(Err(e)) => Err(e),
                                Err(_) => Ok(None), // timeout
                            }
                        }
                        .await;

                        if temp_connection {
                            cf.detach();
                            eprintln!("Health test {}: detached", name);
                        }

                        let done = completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                        let progress = done as f32 / total as f32;

                        let (status, m1, m2, m3, m4) = match motor_pass_result {
                            Ok(Some(motor_pass)) => {
                                let m1 = motor_pass & 0x01 != 0;
                                let m2 = motor_pass & 0x02 != 0;
                                let m3 = motor_pass & 0x04 != 0;
                                let m4 = motor_pass & 0x08 != 0;
                                let status = if motor_pass & 0x0F == 0x0F { "Pass" } else { "Fail" };
                                eprintln!("Health test {}: {} (motorPass=0x{:02X})", name, status, motor_pass);
                                (status, m1, m2, m3, m4)
                            }
                            Ok(None) => {
                                eprintln!("Health test {}: timeout (15s)", name);
                                ("Timeout", false, false, false, false)
                            }
                            Err(e) => {
                                eprintln!("Health test {}: log error: {:?}", name, e);
                                ("Error", false, false, false, false)
                            }
                        };

                        let ui_weak = ui_weak.clone();
                        let status: slint::SharedString = status.into();
                        slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_weak.upgrade() {
                                let results = ui.get_health_test_results();
                                if let Some(mut r) = results.row_data(idx) {
                                    r.status = status;
                                    r.m1_pass = m1;
                                    r.m2_pass = m2;
                                    r.m3_pass = m3;
                                    r.m4_pass = m4;
                                    results.set_row_data(idx, r);
                                }
                                ui.set_health_test_progress(progress);
                                ui.set_health_test_status(format!("{}/{} done", done, total).into());
                            }
                        }).ok();
                    });
                }

                while join_set.join_next().await.is_some() {}

                eprintln!("All health tests complete");
                let ui_weak_inner = ui_weak.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak_inner.upgrade() {
                        ui.set_health_test_running(false);
                        ui.set_health_test_status("Complete".into());
                    }
                }).ok();
            });
        });
    }
}
