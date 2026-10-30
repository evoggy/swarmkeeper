//! Lighthouse geometry and calibration upload.

use std::collections::HashMap;
use std::sync::Arc;

use serde::Deserialize;
use slint::{ComponentHandle, Model};

use crate::AppWindow;
use crate::app::AppContext;
use crate::swarm::client_units;
use crate::swarm_file::uri_with_link_timeout;
use crate::host;

// Lighthouse geometry YAML file format (compatible with crazyflie-lib-python)
#[derive(Deserialize)]
pub(crate) struct LighthouseConfigFile {
    #[serde(default)]
    pub(crate) geos: HashMap<u8, GeoEntry>,
    #[serde(default)]
    pub(crate) calibs: HashMap<u8, CalibEntry>,
}

#[derive(Deserialize)]
pub(crate) struct GeoEntry {
    pub(crate) origin: [f64; 3],
    pub(crate) rotation: [[f64; 3]; 3],
}

#[derive(Deserialize)]
pub(crate) struct CalibEntry {
    pub(crate) uid: u32,
    pub(crate) sweeps: [SweepEntry; 2],
}

#[derive(Deserialize)]
pub(crate) struct SweepEntry {
    pub(crate) phase: f64,
    pub(crate) tilt: f64,
    pub(crate) curve: f64,
    pub(crate) gibmag: f64,
    pub(crate) gibphase: f64,
    pub(crate) ogeemag: f64,
    pub(crate) ogeephase: f64,
}

pub(crate) fn setup(ui: &AppWindow, cx: &AppContext) {
    let AppContext { connection, swarm_state, .. } = cx;
    // Upload lighthouse geometry + calibration
    {
        let swarm_state = swarm_state.clone();
        let connection = connection.clone();
        let ui_weak = ui.as_weak();

        ui.on_upload_geometry(move || {
            let swarm_state = swarm_state.clone();
            let connection = connection.clone();
            let ui_weak = ui_weak.clone();

            host::spawn(async move {
                // Open file dialog
                let Some(handle) = host::dialog::FileDialog::new()
                    .add_filter("YAML", &["yaml", "yml"])
                    .pick_file()
                    .await
                else { return };
                let path = handle.path().to_path_buf();

                // Parse YAML
                let contents = match host::fs::read_to_string(&path) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("Failed to read geometry file: {:?}", e);
                        return;
                    }
                };
                let config: LighthouseConfigFile = match serde_yaml::from_str(&contents) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("Failed to parse geometry file: {:?}", e);
                        return;
                    }
                };

                // Convert to the client API's types
                use crazyflie_connection_server::memory::{LighthouseBsGeometry, LighthouseBsCalibration, LighthouseCalibrationSweep};

                let geometries: HashMap<u8, LighthouseBsGeometry> = config.geos.into_iter().map(|(id, g)| {
                    (id, LighthouseBsGeometry {
                        origin: [g.origin[0] as f32, g.origin[1] as f32, g.origin[2] as f32],
                        rotation_matrix: [
                            [g.rotation[0][0] as f32, g.rotation[0][1] as f32, g.rotation[0][2] as f32],
                            [g.rotation[1][0] as f32, g.rotation[1][1] as f32, g.rotation[1][2] as f32],
                            [g.rotation[2][0] as f32, g.rotation[2][1] as f32, g.rotation[2][2] as f32],
                        ],
                        valid: true,
                    })
                }).collect();

                let calibrations: HashMap<u8, LighthouseBsCalibration> = config.calibs.into_iter().map(|(id, c)| {
                    let sweeps = [
                        LighthouseCalibrationSweep {
                            phase: c.sweeps[0].phase as f32,
                            tilt: c.sweeps[0].tilt as f32,
                            curve: c.sweeps[0].curve as f32,
                            gibmag: c.sweeps[0].gibmag as f32,
                            gibphase: c.sweeps[0].gibphase as f32,
                            ogeemag: c.sweeps[0].ogeemag as f32,
                            ogeephase: c.sweeps[0].ogeephase as f32,
                        },
                        LighthouseCalibrationSweep {
                            phase: c.sweeps[1].phase as f32,
                            tilt: c.sweeps[1].tilt as f32,
                            curve: c.sweeps[1].curve as f32,
                            gibmag: c.sweeps[1].gibmag as f32,
                            gibphase: c.sweeps[1].gibphase as f32,
                            ogeemag: c.sweeps[1].ogeemag as f32,
                            ogeephase: c.sweeps[1].ogeephase as f32,
                        },
                    ];
                    (id, LighthouseBsCalibration { sweeps, uid: c.uid, valid: true })
                }).collect();

                eprintln!("Parsed {} geometries and {} calibrations", geometries.len(), calibrations.len());

                // Collect unit URIs
                let unit_info: Vec<(String, String)> = {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    let ui_weak_inner = ui_weak.clone();
                    slint::invoke_from_event_loop(move || {
                        let mut info = Vec::new();
                        if let Some(ui) = ui_weak_inner.upgrade() {
                            let units = ui.get_units();
                            for i in 0..units.row_count() {
                                if let Some(unit) = units.row_data(i) {
                                    info.push((unit.uri.to_string(), unit.name.to_string()));
                                }
                            }
                        }
                        tx.send(info).ok();
                    }).ok();
                    match rx.await {
                        Ok(i) => i,
                        Err(_) => return,
                    }
                };

                let total = unit_info.len();
                if total == 0 { return; }

                // Show progress dialog
                {
                    let ui_weak_inner = ui_weak.clone();
                    slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_weak_inner.upgrade() {
                            ui.set_progress_dialog_title("Uploading Geometry".into());
                            ui.set_progress_dialog_progress(0.0);
                            ui.set_progress_dialog_status("Starting...".into());
                            ui.set_progress_dialog_visible(true);
                        }
                    }).ok();
                }

                // Upload to all units in parallel
                let completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let geometries = Arc::new(geometries);
                let calibrations = Arc::new(calibrations);

                // Connected units are written through their connection, the
                // others are attached to for the upload
                let connected_units: HashMap<usize, Arc<crazyflie_connection_server::Crazyflie>> = {
                    let state = swarm_state.lock().await;
                    client_units(&state).into_iter().collect()
                };
                let client = connection.read().await.client().clone();

                let mut join_set = host::JoinSet::new();
                for (idx, (uri, name)) in unit_info.into_iter().enumerate() {
                    let existing_cf = connected_units.get(&idx).cloned();
                    let client = client.clone();
                    let ui_weak = ui_weak.clone();
                    let completed = completed.clone();
                    let geometries = geometries.clone();
                    let calibrations = calibrations.clone();

                    join_set.spawn(async move {
                        eprintln!("Uploading geometry to {} ...", uri);

                        let temp_connection = existing_cf.is_none();
                        let connect_result = match existing_cf {
                            Some(cf) => Ok(Ok(cf)),
                            None => host::timeout(
                                std::time::Duration::from_secs(30),
                                client.attach(&uri_with_link_timeout(&uri)),
                            )
                            .await
                            .map(|attached| attached.map(Arc::new)),
                        };
                        let cf = match connect_result {
                            Ok(Ok(cf)) => cf,
                            Ok(Err(e)) => {
                                eprintln!("Failed to connect to {} for geometry upload: {:?}", uri, e);
                                let done = completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                                let progress = done as f32 / total as f32;
                                let status: slint::SharedString = format!("Failed: {} - {} ({}/{})", name, e, done, total).into();
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
                                eprintln!("Connection to {} timed out for geometry upload", uri);
                                let done = completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                                let progress = done as f32 / total as f32;
                                let status: slint::SharedString = format!("Failed: {} - Connection timed out ({}/{})", name, done, total).into();
                                let ui_weak_inner = ui_weak.clone();
                                slint::invoke_from_event_loop(move || {
                                    if let Some(ui) = ui_weak_inner.upgrade() {
                                        ui.set_progress_dialog_progress(progress);
                                        ui.set_progress_dialog_status(status);
                                    }
                                }).ok();
                                return;
                            }
                        };

                        use crazyflie_connection_server::memory::{MemoryType, LighthouseMemory};

                        let lh_mems = cf.memory.get_memories(Some(MemoryType::Lighthouse));
                        if let Some(mem) = lh_mems.first() {
                            if let Ok(lh) = cf.memory.open_memory::<LighthouseMemory>(mem).await {
                                if let Err(e) = lh.write_geometries(&geometries).await {
                                    eprintln!("Failed to write geometries to {}: {:?}", uri, e);
                                }
                                if let Err(e) = lh.write_calibrations(&calibrations).await {
                                    eprintln!("Failed to write calibrations to {}: {:?}", uri, e);
                                }
                            }
                        }

                        // Persist to flash so data survives reboot
                        let geo_ids: Vec<u8> = geometries.keys().copied().collect();
                        let calib_ids: Vec<u8> = calibrations.keys().copied().collect();
                        match cf.localization.lighthouse.persist_lighthouse_data(&geo_ids, &calib_ids).await {
                            Ok(true) => eprintln!("Persisted lighthouse data on {}", uri),
                            Ok(false) => eprintln!("Failed to persist lighthouse data on {}: firmware reported failure", uri),
                            Err(e) => eprintln!("Failed to persist lighthouse data on {}: {:?}", uri, e),
                        }

                        if temp_connection {
                            cf.detach();
                        }
                        eprintln!("Geometry uploaded to {}", uri);

                        let done = completed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                        let progress = done as f32 / total as f32;
                        let status: slint::SharedString = format!("Uploaded to {} ({}/{})", name, done, total).into();
                        let ui_weak_inner = ui_weak.clone();
                        slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_weak_inner.upgrade() {
                                ui.set_progress_dialog_progress(progress);
                                ui.set_progress_dialog_status(status);
                            }
                        }).ok();
                    });
                }

                // Wait for all uploads to complete
                while join_set.join_next().await.is_some() {}

                host::sleep(std::time::Duration::from_millis(500)).await;

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
