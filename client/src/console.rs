//! Console output of the units: capture, storage and the assert snapshot.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use slint::{ComponentHandle, Model};
use tokio::sync::Mutex;

use crate::app::AppContext;
use crate::swarm_file::uri_with_link_timeout;
use crate::units::{rebuild_table_rows, sort_unit_indices, update_unit};
use crate::{AppWindow, ConsoleLineData};
use crate::host;

/// Firmware parameter that makes the Crazyflie dump its stored assert snapshot
/// as a single `DEBUG_PRINT` line on the console (`printAssertSnapshotData()`
/// in `cfassert.c`). Mirrors `cfcli debug assert`.
pub(crate) const ASSERT_INFO_PARAM: &str = "system.assertInfo";

/// Markers the firmware uses for the assert snapshot line, so a stray console
/// line racing with the dump doesn't get mistaken for the result.
pub(crate) const ASSERT_LINE_MARKERS: [&str; 4] = [
    "No assert information found",
    "Assert failed",
    "Hardfault.",
    "unknown type",
];

/// Strip a leading `WORD:` (or `WORD: `) subsystem prefix that the firmware's
/// `DEBUG_PRINT` prepends. Returns the original line when there is no prefix.
pub(crate) fn strip_console_prefix(line: &str) -> &str {
    match line.split_once(':') {
        Some((_, rest)) => rest.strip_prefix(' ').unwrap_or(rest),
        None => line,
    }
}

/// Trigger a firmware assert-info dump and return the resulting console line.
/// `printAssertSnapshotData()` emits exactly one line per invocation, so we
/// listen for new console lines (history excluded), set the parameter and
/// return the first line that looks like an assert snapshot. Falls back to the
/// first line seen if none matches, and to a local message on timeout.
pub(crate) async fn read_assert_info(
    cf: &crazyflie_connection_server::Crazyflie,
    wait_timeout: std::time::Duration,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    use futures::StreamExt;

    // Subscribe before triggering so the dump cannot be missed. No history, so
    // there is nothing to drain first.
    let mut stream = cf.console.line_stream_no_history().await?;

    cf.param
        .set_lossy(ASSERT_INFO_PARAM, 1.0)
        .await
        .map_err(|e| format!("could not set {}: {}", ASSERT_INFO_PARAM, e))?;

    let deadline = host::Instant::now() + wait_timeout;
    let mut first_line: Option<String> = None;

    loop {
        let remaining = deadline.saturating_duration_since(host::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match host::timeout(remaining, stream.next()).await {
            Ok(Some(line)) => {
                let text = strip_console_prefix(&line).trim_end().to_string();
                if ASSERT_LINE_MARKERS.iter().any(|m| text.contains(m)) {
                    return Ok(text);
                }
                first_line.get_or_insert(text);
            }
            // Stream closed (connection lost) or the timeout elapsed.
            _ => break,
        }
    }

    Ok(first_line.unwrap_or_else(|| "No assert info (no response from firmware)".to_string()))
}

/// One captured console line, stamped with the local wall-clock time it was
/// received. The stamp is applied on reception rather than coming from the
/// firmware, so the first lines of a connection (the history the firmware
/// replays on connect) all carry roughly the connection time.
#[derive(Serialize, Deserialize, Clone)]
#[serde(from = "ConsoleLineRepr", into = "ConsoleLineRepr")]
pub(crate) struct ConsoleLine {
    /// "HH:MM:SS.mmm", empty for lines captured before timestamps existed.
    pub(crate) time: String,
    pub(crate) text: String,
}

/// On-disk shape of a [`ConsoleLine`]. Console files written before timestamps
/// existed store a bare string per line, so those still load (with an empty
/// timestamp) instead of throwing away the whole file.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum ConsoleLineRepr {
    Stamped { time: String, text: String },
    Legacy(String),
}

impl From<ConsoleLineRepr> for ConsoleLine {
    fn from(repr: ConsoleLineRepr) -> Self {
        match repr {
            ConsoleLineRepr::Stamped { time, text } => ConsoleLine { time, text },
            ConsoleLineRepr::Legacy(text) => ConsoleLine {
                time: String::new(),
                text,
            },
        }
    }
}

impl From<ConsoleLine> for ConsoleLineRepr {
    fn from(line: ConsoleLine) -> Self {
        ConsoleLineRepr::Stamped {
            time: line.time,
            text: line.text,
        }
    }
}

/// Console history captured for every unit, keyed by CPU serial so it survives
/// reconnects and app restarts (persisted to disk, like the journal). Each unit
/// keeps at most [`CONSOLE_MAX_LINES`] lines; older lines are dropped.
pub(crate) type ConsoleStore = HashMap<String, Vec<ConsoleLine>>;
pub(crate) type SharedConsoleStore = Arc<Mutex<ConsoleStore>>;

/// Maximum number of console lines retained per unit. Console output is
/// high-rate, so this caps both memory use and the on-disk file size.
pub(crate) const CONSOLE_MAX_LINES: usize = 5000;

pub(crate) fn console_path() -> std::path::PathBuf {
    std::path::PathBuf::from("journals/console.yaml")
}

pub(crate) fn load_console() -> ConsoleStore {
    let path = console_path();
    if path.exists() {
        let contents = host::fs::read_to_string(&path).unwrap_or_default();
        serde_yaml::from_str(&contents).unwrap_or_default()
    } else {
        HashMap::new()
    }
}

pub(crate) fn save_console(store: &ConsoleStore) {
    let path = console_path();
    if let Some(parent) = path.parent() {
        host::fs::create_dir_all(parent).ok();
    }
    if let Ok(yaml) = serde_yaml::to_string(store) {
        host::fs::write(&path, yaml).ok();
    }
}

/// Spawn a background task that continuously captures the console output of a
/// connected unit into the persistent [`ConsoleStore`], keyed by `serial`. The
/// `line_stream` first replays the firmware's console history since connection
/// (so the boot banner is captured) and then yields live lines. The task ends
/// when the connection drops and the stream closes.
pub(crate) fn start_console_capture(
    serial: String,
    index: usize,
    cf: Arc<crazyflie_connection_server::Crazyflie>,
    ui_weak: slint::Weak<AppWindow>,
    console_store: SharedConsoleStore,
) {
    if serial.is_empty() {
        return;
    }
    host::spawn(async move {
        use futures::StreamExt;
        let mut stream = match cf.console.line_stream().await {
            Ok(stream) => stream,
            Err(e) => {
                eprintln!("Console of unit {}: {}", index, e);
                return;
            }
        };
        // Save to disk periodically rather than per line, since console output
        // can be bursty/high-rate.
        let mut lines_since_save = 0u32;

        while let Some(line) = stream.next().await {
            let time = chrono::Local::now().format("%H:%M:%S%.3f").to_string();
            let new_count = {
                let mut store = console_store.lock().await;
                let entry = store.entry(serial.clone()).or_default();
                entry.push(ConsoleLine { time, text: line });
                // Enforce the per-unit cap, dropping the oldest lines.
                if entry.len() > CONSOLE_MAX_LINES {
                    let overflow = entry.len() - CONSOLE_MAX_LINES;
                    entry.drain(0..overflow);
                }
                let count = entry.len() as i32;

                lines_since_save += 1;
                if lines_since_save >= 50 {
                    save_console(&store);
                    lines_since_save = 0;
                }
                count
            };

            update_unit(&ui_weak, index, move |u| {
                u.console_line_count = new_count;
            });
        }

        // Stream closed (connection lost) — flush any unsaved lines.
        if lines_since_save > 0 {
            let store = console_store.lock().await;
            save_console(&store);
        }
    });
}

#[cfg(test)]
mod console_store_tests {
    use super::*;

    /// Console files written before lines carried a timestamp store a bare
    /// string per line; those must still load (with an empty stamp) rather than
    /// failing to parse and dropping the whole history.
    #[test]
    fn console_store_reads_legacy_and_stamped_lines() {
        let yaml = "\
'0011':
- 'legacy line'
- time: '12:34:56.789'
  text: 'stamped line'
";
        let store: ConsoleStore = serde_yaml::from_str(yaml).expect("parse");
        let lines = store.get("0011").expect("unit present");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].time, "");
        assert_eq!(lines[0].text, "legacy line");
        assert_eq!(lines[1].time, "12:34:56.789");
        assert_eq!(lines[1].text, "stamped line");

        // Everything is written back in the stamped form.
        let round_tripped: ConsoleStore =
            serde_yaml::from_str(&serde_yaml::to_string(&store).expect("serialize")).expect("reparse");
        let lines = round_tripped.get("0011").expect("unit present");
        assert_eq!(lines[0].text, "legacy line");
        assert_eq!(lines[1].time, "12:34:56.789");
    }
}

pub(crate) fn setup(ui: &AppWindow, cx: &AppContext) {
    let AppContext { connection, swarm_state, console_store, .. } = cx;
    // Open console for selected unit (loads captured lines into the dialog)
    {
        let console_store = console_store.clone();
        let ui_weak = ui.as_weak();

        ui.on_open_console(move |row_index| {
            if row_index < 0 {
                return;
            }

            let serial = {
                let Some(ui) = ui_weak.upgrade() else { return };
                let units = ui.get_units();
                let col = ui.get_sort_column();
                let ascending = ui.get_sort_ascending();
                let indices = sort_unit_indices(&units, col, ascending);
                let row = row_index as usize;
                if row >= indices.len() {
                    return;
                }
                let original = indices[row];
                match units.row_data(original) {
                    Some(u) => u.serial.to_string(),
                    None => return,
                }
            };

            let console_store = console_store.clone();
            let ui_weak = ui_weak.clone();

            tokio::spawn(async move {
                let lines = {
                    let store = console_store.lock().await;
                    store.get(&serial).cloned().unwrap_or_default()
                };

                let slint_lines: Vec<ConsoleLineData> = lines
                    .iter()
                    .map(|l| ConsoleLineData {
                        time: l.time.as_str().into(),
                        text: l.text.as_str().into(),
                    })
                    .collect();

                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_console_lines(slint::ModelRc::new(slint::VecModel::from(slint_lines)));
                    }
                }).ok();
            });
        });
    }

    // Clear console for selected unit
    {
        let console_store = console_store.clone();
        let ui_weak = ui.as_weak();

        ui.on_clear_console(move |row_index| {
            if row_index < 0 {
                return;
            }

            let (serial, original_index) = {
                let Some(ui) = ui_weak.upgrade() else { return };
                let units = ui.get_units();
                let col = ui.get_sort_column();
                let ascending = ui.get_sort_ascending();
                let indices = sort_unit_indices(&units, col, ascending);
                let row = row_index as usize;
                if row >= indices.len() {
                    return;
                }
                let original = indices[row];
                match units.row_data(original) {
                    Some(u) => (u.serial.to_string(), original),
                    None => return,
                }
            };

            if serial.is_empty() {
                return;
            }

            let console_store = console_store.clone();
            let ui_weak = ui_weak.clone();

            tokio::spawn(async move {
                {
                    let mut store = console_store.lock().await;
                    store.remove(&serial);
                    save_console(&store);
                }

                let ui_weak_count = ui_weak.clone();
                update_unit(&ui_weak_count, original_index, move |u| {
                    u.console_line_count = 0;
                });

                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_console_lines(slint::ModelRc::new(slint::VecModel::from(Vec::<ConsoleLineData>::new())));
                    }
                }).ok();
            });
        });
    }

    // Read the firmware assert snapshot of the selected unit and show it in the
    // sidebar's Assert Info popup (same mechanism as `cfcli debug assert`).
    {
        let connection = connection.clone();
        let swarm_state = swarm_state.clone();
        let ui_weak = ui.as_weak();

        ui.on_read_assert(move |row_index| {
            if row_index < 0 {
                return;
            }

            let (original_index, uri, name) = {
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
                let Some(unit) = units.row_data(idx) else { return };
                (idx, unit.uri.to_string(), unit.name.to_string())
            };

            let connection = connection.clone();
            let swarm_state = swarm_state.clone();
            let ui_weak = ui_weak.clone();

            // Report the outcome (text + busy cleared) back to the popup.
            fn finish(ui_weak: &slint::Weak<AppWindow>, text: String) {
                let ui_weak = ui_weak.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_assert_text(text.into());
                        ui.set_assert_busy(false);
                    }
                }).ok();
            }

            if let Some(ui) = ui_weak.upgrade() {
                ui.set_assert_text("".into());
                ui.set_assert_busy(true);
            }

            tokio::spawn(async move {
                // Reuse the existing connection if the unit is connected, else
                // connect temporarily for the duration of the read.
                let existing_cf = {
                    let state = swarm_state.lock().await;
                    state.get(&original_index).map(|cu| cu.cf.clone())
                };

                let (cf, temp_connection) = if let Some(cf) = existing_cf {
                    (cf, false)
                } else {
                    eprintln!("Read assert {}: connecting to {}...", name, uri);
                    let client = connection.read().await.client().clone();
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(30),
                        client.attach(&uri_with_link_timeout(&uri)),
                    ).await {
                        Ok(Ok(cf)) => (Arc::new(cf), true),
                        Ok(Err(e)) => {
                            eprintln!("Read assert {}: connect FAILED: {:?}", name, e);
                            finish(&ui_weak, format!("Connect failed: {}", e));
                            return;
                        }
                        Err(_) => {
                            eprintln!("Read assert {}: connect timed out", name);
                            finish(&ui_weak, "Connection timed out".to_string());
                            return;
                        }
                    }
                };

                let result = read_assert_info(&cf, std::time::Duration::from_secs(3)).await;

                if temp_connection {
                    cf.detach();
                }

                match result {
                    Ok(text) => {
                        eprintln!("Read assert {}: {}", name, text);
                        finish(&ui_weak, text);
                    }
                    Err(e) => {
                        eprintln!("Read assert {}: FAILED: {}", name, e);
                        finish(&ui_weak, format!("Failed to read assert info: {}", e));
                    }
                }
            });
        });
    }

    // Clear console for the whole swarm
    {
        let console_store = console_store.clone();
        let ui_weak = ui.as_weak();

        ui.on_clear_all_console(move || {
            let console_store = console_store.clone();
            let ui_weak = ui_weak.clone();

            tokio::spawn(async move {
                {
                    let mut store = console_store.lock().await;
                    store.clear();
                    save_console(&store);
                }

                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        // Reset every unit's console line count.
                        let units = ui.get_units();
                        for i in 0..units.row_count() {
                            if let Some(mut u) = units.row_data(i) {
                                u.console_line_count = 0;
                                units.set_row_data(i, u);
                            }
                        }
                        ui.set_console_lines(slint::ModelRc::new(slint::VecModel::from(Vec::<ConsoleLineData>::new())));
                        rebuild_table_rows(&ui);
                    }
                }).ok();
            });
        });
    }
}
