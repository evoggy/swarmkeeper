//! The per-unit journal.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use slint::{ComponentHandle, Model};
use tokio::sync::Mutex;

use crate::app::AppContext;
use crate::units::{sort_unit_indices, update_unit};
use crate::{AppWindow, JournalEntryData};

#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct JournalEntry {
    pub(crate) timestamp: String,
    pub(crate) text: String,
}

pub(crate) type JournalStore = HashMap<String, Vec<JournalEntry>>;
pub(crate) type SharedJournalStore = Arc<Mutex<JournalStore>>;

pub(crate) fn journal_path() -> std::path::PathBuf {
    std::path::PathBuf::from("journals/journal.yaml")
}

pub(crate) fn load_journal() -> JournalStore {
    let path = journal_path();
    if path.exists() {
        let contents = std::fs::read_to_string(&path).unwrap_or_default();
        serde_yaml::from_str(&contents).unwrap_or_default()
    } else {
        HashMap::new()
    }
}

pub(crate) fn save_journal(store: &JournalStore) {
    let path = journal_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    if let Ok(yaml) = serde_yaml::to_string(store) {
        std::fs::write(&path, yaml).ok();
    }
}

pub(crate) fn setup(ui: &AppWindow, cx: &AppContext) {
    let AppContext { journal_store, .. } = cx;
    // Open journal for selected unit
    {
        let journal_store = journal_store.clone();
        let ui_weak = ui.as_weak();

        ui.on_open_journal(move |row_index| {
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

            let journal_store = journal_store.clone();
            let ui_weak = ui_weak.clone();

            tokio::spawn(async move {
                let entries = {
                    let store = journal_store.lock().await;
                    store.get(&serial).cloned().unwrap_or_default()
                };

                let slint_entries: Vec<JournalEntryData> = entries
                    .iter()
                    .rev()
                    .map(|e| JournalEntryData {
                        timestamp: e.timestamp.clone().into(),
                        text: e.text.clone().into(),
                    })
                    .collect();

                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_journal_entries(slint::ModelRc::new(slint::VecModel::from(slint_entries)));
                    }
                }).ok();
            });
        });
    }

    // Add journal entry
    {
        let journal_store = journal_store.clone();
        let ui_weak = ui.as_weak();

        ui.on_add_journal_entry(move |row_index, text| {
            if row_index < 0 || text.is_empty() {
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
                eprintln!("Cannot add journal entry: unit has no serial number");
                return;
            }

            let text = text.to_string();
            let journal_store = journal_store.clone();
            let ui_weak = ui_weak.clone();

            tokio::spawn(async move {
                let (entries, new_count) = {
                    let mut store = journal_store.lock().await;
                    let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
                    let entry = JournalEntry { timestamp, text };
                    store.entry(serial.clone()).or_default().push(entry);
                    save_journal(&store);
                    let entries = store.get(&serial).cloned().unwrap_or_default();
                    let count = entries.len() as i32;
                    (entries, count)
                };

                let slint_entries: Vec<JournalEntryData> = entries
                    .iter()
                    .rev()
                    .map(|e| JournalEntryData {
                        timestamp: e.timestamp.clone().into(),
                        text: e.text.clone().into(),
                    })
                    .collect();

                let ui_weak_count = ui_weak.clone();
                update_unit(&ui_weak_count, original_index, move |u| {
                    u.journal_entry_count = new_count;
                });

                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_journal_entries(slint::ModelRc::new(slint::VecModel::from(slint_entries)));
                    }
                }).ok();
            });
        });
    }

    // Delete journal entry
    {
        let journal_store = journal_store.clone();
        let ui_weak = ui.as_weak();

        ui.on_delete_journal_entry(move |row_index, entry_index| {
            if row_index < 0 || entry_index < 0 {
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

            let journal_store = journal_store.clone();
            let ui_weak = ui_weak.clone();
            let entry_index = entry_index as usize;

            tokio::spawn(async move {
                let (entries, new_count) = {
                    let mut store = journal_store.lock().await;
                    let Some(unit_entries) = store.get_mut(&serial) else {
                        return;
                    };
                    // UI displays entries in reverse order, so map index back
                    let store_index = unit_entries.len().saturating_sub(1 + entry_index);
                    if store_index < unit_entries.len() {
                        unit_entries.remove(store_index);
                    }
                    save_journal(&store);
                    let entries = store.get(&serial).cloned().unwrap_or_default();
                    let count = entries.len() as i32;
                    (entries, count)
                };

                let slint_entries: Vec<JournalEntryData> = entries
                    .iter()
                    .rev()
                    .map(|e| JournalEntryData {
                        timestamp: e.timestamp.clone().into(),
                        text: e.text.clone().into(),
                    })
                    .collect();

                let ui_weak_count = ui_weak.clone();
                update_unit(&ui_weak_count, original_index, move |u| {
                    u.journal_entry_count = new_count;
                });

                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_journal_entries(slint::ModelRc::new(slint::VecModel::from(slint_entries)));
                    }
                }).ok();
            });
        });
    }
}
