//! The LPS sniffer tab.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use slint::{ComponentHandle, Model};

use crate::app::AppContext;
use crate::loco::AnchorPositionEntry;
use crate::{AppWindow, host::sniffer, SnifferAnchorRow, SnifferFeedRow, SnifferMatrixRow, SnifferSurveyRow};
use crate::host;

/// Build the Slint models from the current sniffer state and push them to the
/// UI thread. Safe to call from the serial reader thread.
/// Format one axis of a survey-vs-reference offset as signed millimetres, or an
/// em dash when the anchor had no reference position to compare against.
pub(crate) fn axis_delta_mm(delta: Option<[f32; 3]>, axis: usize) -> slint::SharedString {
    match delta {
        Some(d) => format!("{:+.0}", d[axis] * 1000.0).into(),
        None => "—".into(),
    }
}

pub(crate) fn update_sniffer_ui(
    ui_weak: &slint::Weak<AppWindow>,
    state: &Arc<std::sync::Mutex<sniffer::SnifferState>>,
    pending: Option<Arc<AtomicBool>>,
) {
    let build_start = host::Instant::now();
    let Ok(st) = state.lock() else {
        eprintln!("[sniffer] update_sniffer_ui: state lock poisoned/contended, skipping");
        if let Some(p) = pending {
            p.store(false, Ordering::Release);
        }
        return;
    };

    let connected = st.connected;
    let total_packets = st.total_packets as i32;
    let status = if let Some(ref e) = st.error {
        format!("Error: {e}")
    } else if st.connected {
        format!("Connected ({})", st.port_name)
    } else {
        "Disconnected".to_string()
    };

    // Per-anchor stats, sorted by source id.
    let mut ids: Vec<u8> = st.stats.keys().copied().collect();
    ids.sort_unstable();
    let mut total_rate = 0.0f32;
    let mut anchor_rows: Vec<SnifferAnchorRow> = Vec::with_capacity(ids.len());
    for id in &ids {
        let s = &st.stats[id];
        total_rate += s.rate_hz();
        let ago = (st.now - s.last_seen).max(0.0);
        anchor_rows.push(SnifferAnchorRow {
            id: *id as i32,
            kind: s.kind.label().into(),
            rate: s.rate_hz(),
            count: s.count as i32,
            lost: s.lost as i32,
            last_seen: format!("{ago:.1} s ago").into(),
            rssi: s.rx_power.unwrap_or(0.0),
            has_rssi: s.rx_power.is_some(),
            nlos: s.nlos().unwrap_or(0.0),
        });
    }

    // Aggregate loss split: where packets are going missing.
    let wire_dropped = st.wire_dropped as i32;
    let host_resyncs = st.host_resyncs as i32;
    let air_lost = st.air_lost() as i32;

    // Recording status (set by the reader thread).
    let recording = st.recording;
    let rec_count = st.rec_count as i32;
    let rec_path = st.rec_path.clone();

    // Distance matrix.
    let mids = st.matrix.ids();
    let matrix_ids: Vec<i32> = mids.iter().map(|&i| i as i32).collect();
    // Plain data only — `SnifferMatrixRow`'s inner model isn't `Send`, so the
    // rows are assembled inside the event-loop closure below.
    let matrix_rows_data: Vec<(i32, Vec<slint::SharedString>)> = mids
        .iter()
        .map(|&ri| {
            let cells = mids
                .iter()
                .map(|&ci| {
                    if ri == ci {
                        slint::SharedString::from("—")
                    } else if let Some(d) = st.matrix.distance(ri, ci) {
                        slint::SharedString::from(format!("{d:.2}"))
                    } else {
                        slint::SharedString::from("·")
                    }
                })
                .collect();
            (ri as i32, cells)
        })
        .collect();

    // Survey results, and what they are being compared against.
    let ref_status = st.reference_status();
    let survey_status = st.survey_status.clone();
    let survey_rows: Vec<SnifferSurveyRow> = st
        .survey
        .iter()
        .map(|s| SnifferSurveyRow {
            id: s.id as i32,
            x: s.pos[0],
            y: s.pos[1],
            z: s.pos[2],
            residual: s.residual,
            links: format!("{}/{}", s.links_used, s.links_total).into(),
            thin: s.links_used * 2 < s.links_total,
            has_ref: s.reference.is_some(),
            // Signed per-axis offsets in millimetres, so a systematic tilt or a
            // single mis-typed coordinate is obvious at a glance.
            dx: axis_delta_mm(s.delta(), 0),
            dy: axis_delta_mm(s.delta(), 1),
            dz: axis_delta_mm(s.delta(), 2),
            delta_text: s
                .delta_norm()
                .map_or_else(|| "—".into(), |d| format!("{:.0}", d * 1000.0).into()),
            delta: s.delta_norm().unwrap_or(0.0),
        })
        .collect();

    // Packet feed, newest first.
    let feed_rows: Vec<SnifferFeedRow> = st
        .feed
        .iter()
        .rev()
        .take(200)
        .map(|p| {
            let info = match p.kind {
                sniffer::PacketKind::Tdoa3 => {
                    let dists = p
                        .remote_anchors
                        .iter()
                        .filter(|r| r.distance_ticks.is_some())
                        .count();
                    format!(
                        "seq {}, {} remotes ({} ranged){}",
                        p.seq.map(|s| s as i32).unwrap_or(-1),
                        p.remote_anchors.len(),
                        dists,
                        if p.lpp_position.is_some() { ", +pos" } else { "" }
                    )
                }
                _ => match p.seq {
                    Some(s) => format!("seq {s}, {} B", p.payload_len),
                    None => format!("{} B", p.payload_len),
                },
            };
            SnifferFeedRow {
                time: format!("@{:010x}", p.rx_timestamp).into(),
                from: p.from as i32,
                to: p.to as i32,
                kind: p.kind.label().into(),
                info: info.into(),
            }
        })
        .collect();

    let total_packets_dbg = st.total_packets;
    drop(st);

    eprintln!(
        "[sniffer] build: total={} anchors={} matrix={}x{} survey={} feed={} build_took={:?}",
        total_packets_dbg,
        anchor_rows.len(),
        matrix_ids.len(),
        matrix_ids.len(),
        survey_rows.len(),
        feed_rows.len(),
        build_start.elapsed(),
    );

    let ui_weak = ui_weak.clone();
    let post_instant = host::Instant::now();
    slint::invoke_from_event_loop(move || {
        // Clear the in-flight flag as soon as this update runs, so the reader is
        // free to queue the next one — but only after the prior render finished.
        if let Some(p) = pending {
            p.store(false, Ordering::Release);
        }
        let apply_start = host::Instant::now();
        eprintln!(
            "[sniffer] applied on UI thread: queue_latency={:?}",
            post_instant.elapsed(),
        );
        let Some(ui) = ui_weak.upgrade() else {
            eprintln!("[sniffer] applied: ui weak upgrade failed");
            return;
        };
        ui.set_sniffer_connected(connected);
        ui.set_sniffer_status(status.into());
        ui.set_sniffer_total_packets(total_packets);
        ui.set_sniffer_total_rate(total_rate);
        ui.set_sniffer_wire_dropped(wire_dropped);
        ui.set_sniffer_host_resyncs(host_resyncs);
        ui.set_sniffer_air_lost(air_lost);
        ui.set_sniffer_recording_active(recording);
        ui.set_sniffer_rec_count(rec_count);
        ui.set_sniffer_rec_path(rec_path.into());
        ui.set_sniffer_anchor_rows(slint::ModelRc::new(slint::VecModel::from(anchor_rows)));
        ui.set_sniffer_matrix_ids(slint::ModelRc::new(slint::VecModel::from(matrix_ids)));
        let matrix_rows: Vec<SnifferMatrixRow> = matrix_rows_data
            .into_iter()
            .map(|(id, cells)| SnifferMatrixRow {
                id,
                cells: slint::ModelRc::new(slint::VecModel::from(cells)),
            })
            .collect();
        ui.set_sniffer_matrix_rows(slint::ModelRc::new(slint::VecModel::from(matrix_rows)));
        ui.set_sniffer_ref_status(ref_status.into());
        ui.set_sniffer_survey_status(survey_status.into());
        ui.set_sniffer_survey_rows(slint::ModelRc::new(slint::VecModel::from(survey_rows)));
        ui.set_sniffer_feed_rows(slint::ModelRc::new(slint::VecModel::from(feed_rows)));
        eprintln!(
            "[sniffer] applied: set_models_took={:?}",
            apply_start.elapsed(),
        );
    })
    .ok();
}


pub(crate) fn setup(ui: &AppWindow, _cx: &AppContext) {
    // ---- LPS sniffer tab ----
    {
        let sniffer_state: Arc<std::sync::Mutex<sniffer::SnifferState>> =
            Arc::new(std::sync::Mutex::new(sniffer::SnifferState::default()));
        let sniffer_stop = Arc::new(AtomicBool::new(false));
        // Opt-in capture: while true the reader thread writes every sniffed
        // packet to a JSONL recording file. Toggled by the UI "Record" checkbox.
        let sniffer_recording = Arc::new(AtomicBool::new(false));
        let sniffer_thread: Arc<std::sync::Mutex<Option<host::ThreadHandle>>> =
            Arc::new(std::sync::Mutex::new(None));
        // Back-pressure: true while a UI refresh is queued but not yet applied,
        // so the reader never floods the event loop with model rebuilds.
        let sniffer_ui_pending = Arc::new(AtomicBool::new(false));

        // Populate the port list on launch.
        {
            let ports: Vec<slint::SharedString> =
                sniffer::list_ports().into_iter().map(Into::into).collect();
            ui.set_sniffer_port_names(slint::ModelRc::new(slint::VecModel::from(ports)));
        }

        {
            let ui_weak = ui.as_weak();
            ui.on_sniffer_refresh_ports(move || {
                let Some(ui) = ui_weak.upgrade() else { return };
                let ports: Vec<slint::SharedString> =
                    sniffer::list_ports().into_iter().map(Into::into).collect();
                ui.set_sniffer_port_names(slint::ModelRc::new(slint::VecModel::from(ports)));
            });
        }

        {
            let ui_weak = ui.as_weak();
            let state = sniffer_state.clone();
            let stop = sniffer_stop.clone();
            let recording = sniffer_recording.clone();
            let thread_holder = sniffer_thread.clone();
            let ui_pending = sniffer_ui_pending.clone();
            ui.on_sniffer_connect(move || {
                eprintln!("[sniffer] on_sniffer_connect: callback entered (UI thread)");
                let Some(ui) = ui_weak.upgrade() else { return };
                // Ignore if a reader is already running.
                if thread_holder.lock().unwrap().is_some() {
                    eprintln!("[sniffer] on_sniffer_connect: already running, ignoring");
                    return;
                }
                let idx = ui.get_sniffer_port_index().max(0) as usize;
                let names = ui.get_sniffer_port_names();
                let Some(port) = names.row_data(idx) else {
                    ui.set_sniffer_status("No serial port selected".into());
                    return;
                };
                stop.store(false, Ordering::Relaxed);
                ui_pending.store(false, Ordering::Relaxed);
                let handle = sniffer::start(
                    port.to_string(),
                    state.clone(),
                    stop.clone(),
                    recording.clone(),
                    ui_weak.clone(),
                    ui_pending.clone(),
                );
                *thread_holder.lock().unwrap() = Some(handle);
                eprintln!("[sniffer] on_sniffer_connect: callback returning (UI thread free)");
            });
        }

        {
            let stop = sniffer_stop.clone();
            let thread_holder = sniffer_thread.clone();
            ui.on_sniffer_disconnect(move || {
                stop.store(true, Ordering::Relaxed);
                if let Some(h) = thread_holder.lock().unwrap().take() {
                    let _ = h.join();
                }
            });
        }

        {
            let ui_weak = ui.as_weak();
            let state = sniffer_state.clone();
            ui.on_sniffer_solve_survey(move || {
                if let Ok(mut st) = state.lock() {
                    st.solve_survey();
                }
                update_sniffer_ui(&ui_weak, &state, None);
            });
        }

        {
            let state = sniffer_state.clone();
            ui.on_sniffer_set_paused(move |p| {
                if let Ok(mut st) = state.lock() {
                    st.paused = p;
                }
            });
        }

        {
            // The reader thread opens/closes the recording file to match this
            // flag; toggling it before connecting simply starts recording once
            // the reader is up.
            let recording = sniffer_recording.clone();
            ui.on_sniffer_set_recording(move |on| {
                recording.store(on, Ordering::Relaxed);
            });
        }

        {
            let state = sniffer_state.clone();
            ui.on_sniffer_save_survey(move || {
                let survey = match state.lock() {
                    Ok(st) => st.survey.clone(),
                    Err(_) => return,
                };
                if survey.is_empty() {
                    return;
                }
                host::spawn(async move {
                    let Some(file) = host::dialog::FileDialog::new()
                        .add_filter("YAML", &["yaml", "yml"])
                        .set_file_name("sniffer-geometry.yaml")
                        .save_file()
                        .await
                    else {
                        return;
                    };
                    // Same shape the Loco tab loads: { id: { x, y, z } }.
                    let entries: std::collections::BTreeMap<u8, AnchorPositionEntry> = survey
                        .iter()
                        .map(|s| {
                            (
                                s.id,
                                AnchorPositionEntry {
                                    x: s.pos[0],
                                    y: s.pos[1],
                                    z: s.pos[2],
                                },
                            )
                        })
                        .collect();
                    match serde_yaml::to_string(&entries) {
                        Ok(text) => {
                            if let Err(e) = host::fs::write(file.path(), text) {
                                eprintln!("Failed to write survey geometry: {e}");
                            }
                        }
                        Err(e) => eprintln!("Failed to serialize survey geometry: {e}"),
                    }
                });
            });
        }

        // Stop the reader cleanly when the window closes.
        {
            let stop = sniffer_stop.clone();
            let thread_holder = sniffer_thread.clone();
            ui.window().on_close_requested(move || {
                stop.store(true, Ordering::Relaxed);
                if let Some(h) = thread_holder.lock().unwrap().take() {
                    let _ = h.join();
                }
                slint::quit_event_loop().ok();
                slint::CloseRequestResponse::HideWindow
            });
        }
    }
}
