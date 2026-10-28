//! The LPS sniffer tab.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use slint::{ComponentHandle, Model};

use crate::app::AppContext;
use crate::loco::AnchorPositionEntry;
use crate::{AppWindow, sniffer, SnifferAnchorRow, SnifferFeedRow, SnifferMatrixRow, SnifferSurveyRow};

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
    let build_start = std::time::Instant::now();
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
    let post_instant = std::time::Instant::now();
    slint::invoke_from_event_loop(move || {
        // Clear the in-flight flag as soon as this update runs, so the reader is
        // free to queue the next one — but only after the prior render finished.
        if let Some(p) = pending {
            p.store(false, Ordering::Release);
        }
        let apply_start = std::time::Instant::now();
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

/// Open the serial port, switch the node to binary sniffer mode, and stream
/// decoded packets into `state` until `stop` is set. Runs on its own thread.
pub(crate) fn start_sniffer(
    port: String,
    state: Arc<std::sync::Mutex<sniffer::SnifferState>>,
    stop: Arc<AtomicBool>,
    recording: Arc<AtomicBool>,
    ui_weak: slint::Weak<AppWindow>,
    ui_pending: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        eprintln!("[sniffer] reader thread started, opening {port}");
        let opened = serialport::new(&port, 9600)
            .timeout(std::time::Duration::from_millis(50))
            .open();
        let mut sp = match opened {
            Ok(p) => {
                eprintln!("[sniffer] port {port} opened");
                p
            }
            Err(e) => {
                eprintln!("[sniffer] open failed: {e}");
                if let Ok(mut st) = state.lock() {
                    st.connected = false;
                    st.port_name = port.clone();
                    st.error = Some(e.to_string());
                }
                update_sniffer_ui(&ui_weak, &state, None);
                return;
            }
        };

        // The node only enables its USB output once the host asserts DTR — it
        // watches SET_CONTROL_LINE_STATE and drops everything until then.
        // pyserial asserts DTR on open; the serialport crate does not, so do it
        // explicitly or we never see a single byte.
        match sp.write_data_terminal_ready(true) {
            Ok(()) => eprintln!("[sniffer] write_data_terminal_ready(true) OK"),
            Err(e) => eprintln!("[sniffer] write_data_terminal_ready FAILED: {e}"),
        }

        // Switch the node from human-readable to binary output.
        match sp.write_all(b"b") {
            Ok(()) => eprintln!("[sniffer] wrote 'b' OK"),
            Err(e) => eprintln!("[sniffer] write 'b' FAILED: {e}"),
        }
        match sp.flush() {
            Ok(()) => eprintln!("[sniffer] flush OK — entering read loop"),
            Err(e) => eprintln!("[sniffer] flush FAILED: {e}"),
        }
        if let Ok(mut st) = state.lock() {
            st.connected = true;
            st.error = None;
            st.port_name = port.clone();
        }
        update_sniffer_ui(&ui_weak, &state, None);

        let start = std::time::Instant::now();
        let mut dec = sniffer::FrameDecoder::new();
        let mut rxbuf = [0u8; 2048];
        let mut packets = Vec::new();
        // Opt-in recorder, opened/closed in the loop to track the `recording`
        // flag. Owned here so all file I/O stays off the shared state mutex.
        let mut recorder: Option<sniffer::SnifferRecorder> = None;
        let mut last_ui = std::time::Instant::now();
        let mut bytes_total: u64 = 0;
        let mut packets_total: u64 = 0;
        let mut reads_total: u64 = 0;
        let mut timeouts: u64 = 0;
        let mut zero_reads: u64 = 0;

        while !stop.load(Ordering::Relaxed) {
            // Open/close the recording file to match the UI checkbox.
            match (recording.load(Ordering::Relaxed), recorder.is_some()) {
                (true, false) => {
                    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
                    match sniffer::SnifferRecorder::create(
                        std::path::Path::new("recordings/sniffer"),
                        &stamp,
                    ) {
                        Ok(r) => {
                            eprintln!("[sniffer] recording to {}", r.path.display());
                            recorder = Some(r);
                        }
                        Err(e) => {
                            eprintln!("[sniffer] cannot start recording: {e}");
                            // Clear the flag so we don't retry (and spam) every loop.
                            recording.store(false, Ordering::Relaxed);
                        }
                    }
                }
                (false, true) => {
                    if let Some(mut r) = recorder.take() {
                        r.flush();
                        eprintln!(
                            "[sniffer] recording stopped: {} packets -> {}",
                            r.count,
                            r.path.display()
                        );
                    }
                }
                _ => {}
            }

            match sp.read(&mut rxbuf) {
                Ok(0) => {
                    zero_reads += 1;
                }
                Ok(n) => {
                    reads_total += 1;
                    bytes_total += n as u64;
                    packets.clear();
                    dec.push(&rxbuf[..n], &mut packets);
                    packets_total += packets.len() as u64;
                    let now = start.elapsed().as_secs_f64();
                    // Record every decoded sample, outside the state lock, so a
                    // recording is full-fidelity (the live feed is capped).
                    if let Some(r) = recorder.as_mut() {
                        for p in &packets {
                            r.record(now, p);
                        }
                    }
                    if let Ok(mut st) = state.lock() {
                        st.now = now;
                        st.host_resyncs = dec.resyncs;
                        for p in packets.drain(..) {
                            st.ingest(p, now);
                        }
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {
                    timeouts += 1;
                    if let Ok(mut st) = state.lock() {
                        st.now = start.elapsed().as_secs_f64();
                    }
                }
                Err(ref e) => {
                    eprintln!("[sniffer] read error: {e} — stopping reader");
                    break;
                }
            }
            // Throttle to ~6 Hz. Reset the timer every tick so the debug line is
            // rate-limited even when an update is skipped, and watch ui_pending:
            // if it stays `true` the event loop isn't draining our closures.
            if last_ui.elapsed().as_millis() >= 150 {
                last_ui = std::time::Instant::now();
                // Flush buffered samples and mirror recording status to the UI.
                if let Some(r) = recorder.as_mut() {
                    r.flush();
                }
                if let Ok(mut st) = state.lock() {
                    st.recording = recorder.is_some();
                    st.rec_count = recorder.as_ref().map(|r| r.count).unwrap_or(0);
                    st.rec_path = recorder
                        .as_ref()
                        .map(|r| r.path.display().to_string())
                        .unwrap_or_default();
                }
                let pend = ui_pending.load(Ordering::Acquire);
                eprintln!(
                    "[sniffer] tick: okN={reads_total} bytes={bytes_total} packets={packets_total} timeouts={timeouts} zero={zero_reads} ui_pending={pend}"
                );
                if !ui_pending.swap(true, Ordering::AcqRel) {
                    update_sniffer_ui(&ui_weak, &state, Some(ui_pending.clone()));
                } else {
                    eprintln!("[sniffer] tick: skipped update (previous still in flight)");
                }
            }
        }

        eprintln!("[sniffer] reader loop exited (stop={})", stop.load(Ordering::Relaxed));
        // Finalise any in-progress recording before the thread ends.
        if let Some(mut r) = recorder.take() {
            r.flush();
            eprintln!(
                "[sniffer] recording closed: {} packets -> {}",
                r.count,
                r.path.display()
            );
        }
        if let Ok(mut st) = state.lock() {
            st.connected = false;
            st.recording = false;
            st.rec_count = 0;
            st.rec_path = String::new();
        }
        update_sniffer_ui(&ui_weak, &state, None);
    })
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
        let sniffer_thread: Arc<std::sync::Mutex<Option<std::thread::JoinHandle<()>>>> =
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
                let handle = start_sniffer(
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
                tokio::spawn(async move {
                    let Some(file) = rfd::AsyncFileDialog::new()
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
                            if let Err(e) = std::fs::write(file.path(), text) {
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
