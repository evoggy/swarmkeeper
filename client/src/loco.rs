//! Loco positioning: anchor positions read from and written to the units.

use std::collections::HashMap;
use std::sync::Arc;

use slint::ComponentHandle;
use tokio::sync::Mutex;

use crate::app::AppContext;
use crate::{AppWindow, LocoAnchorStatus};
use crate::host;

/// Human-readable name for a `loco.mode` log value.
pub(crate) fn loco_mode_text(mode: u8) -> &'static str {
    match mode {
        0 => "Auto",
        1 => "TWR",
        2 => "TDoA2",
        3 => "TDoA3",
        _ => "Unknown",
    }
}

/// Build the Loco anchor status list from the shared positioning data and push
/// it to the UI (the "Loco" debug tab). Shows every anchor that has either a
/// stored position or is currently being received, marking which are active.
pub(crate) fn update_loco_anchors(ui_weak: &slint::Weak<AppWindow>, pd: &PositioningData) {
    use std::collections::BTreeSet;

    let active: std::collections::HashSet<u8> = pd.loco_active_ids.iter().copied().collect();
    // Union of anchors we have a position for, anchors currently heard, and
    // anchors we have a loaded target for (so pending uploads are visible even
    // before the anchor has been heard).
    let mut ids: BTreeSet<u8> = pd.loco_seen.keys().copied().collect();
    ids.extend(active.iter().copied());
    ids.extend(pd.loco_targets.keys().copied());

    let mut confirmed_count = 0usize;
    let anchors: Vec<LocoAnchorStatus> = ids
        .iter()
        .map(|id| {
            let pos = pd.loco_seen.get(id);
            let target = pd.loco_targets.get(id);
            let confirmed = match (pos, target) {
                (Some(p), Some(t)) => anchor_pos_matches(p, t),
                _ => false,
            };
            if confirmed {
                confirmed_count += 1;
            }
            LocoAnchorStatus {
                id: *id as i32,
                x: pos.map(|p| p[0]).unwrap_or(0.0),
                y: pos.map(|p| p[1]).unwrap_or(0.0),
                z: pos.map(|p| p[2]).unwrap_or(0.0),
                has_position: pos.is_some(),
                receiving: active.contains(id),
                has_target: target.is_some(),
                target_x: target.map(|t| t[0]).unwrap_or(0.0),
                target_y: target.map(|t| t[1]).unwrap_or(0.0),
                target_z: target.map(|t| t[2]).unwrap_or(0.0),
                confirmed,
            }
        })
        .collect();

    let total = anchors.len() as i32;
    let active_count = active.len() as i32;
    let mode_text = loco_mode_text(pd.loco_mode).to_string();

    let target_count = pd.loco_targets.len();
    let upload_active = pd.loco_upload_active;
    let upload_status = if target_count == 0 {
        String::new()
    } else if upload_active {
        format!("Uploading anchor positions: {} / {} confirmed", confirmed_count, target_count)
    } else {
        format!("All {} anchor positions confirmed", target_count)
    };

    let ui_weak = ui_weak.clone();
    slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak.upgrade() {
            ui.set_loco_anchors(slint::ModelRc::new(slint::VecModel::from(anchors)));
            ui.set_loco_mode_text(mode_text.into());
            ui.set_loco_active_count(active_count);
            ui.set_loco_total_count(total);
            ui.set_loco_upload_status(upload_status.into());
            ui.set_loco_upload_active(upload_active);
        }
    })
    .ok();
}

/// Read the anchor positions a Crazyflie has stored in its Loco2 memory, plus
/// the ids it is currently hearing. TDoA3 anchor IDs can be any byte value
/// (0-255); at most 16 of each list are read, as the firmware keeps.
pub(crate) async fn read_loco_anchor_positions(
    cf: &crazyflie_connection_server::Crazyflie,
) -> (Vec<(u8, [f32; 3])>, Vec<u8>) {
    use crazyflie_connection_server::memory::{LocoMemory2, MemoryType};

    const MAX_ANCHORS: usize = 16;

    let mut positions = Vec::new();
    let mems = cf.memory.get_memories(Some(MemoryType::Loco2));
    let Some(mem) = mems.first() else {
        return (positions, Vec::new());
    };
    let Ok(loco) = cf.memory.open_memory::<LocoMemory2>(mem).await else {
        return (positions, Vec::new());
    };

    let all_ids = match loco.read_id_list().await {
        Ok(ids) => ids.into_iter().take(MAX_ANCHORS).collect(),
        Err(e) => {
            eprintln!("Failed to read loco id list: {:?}", e);
            Vec::new()
        }
    };
    let active_ids = match loco.read_active_id_list().await {
        Ok(ids) => ids.into_iter().take(MAX_ANCHORS).collect(),
        Err(e) => {
            eprintln!("Failed to read loco active list: {:?}", e);
            Vec::new()
        }
    };

    for id in all_ids {
        match loco.read_anchor_data(id).await {
            Ok(anchor) if anchor.is_valid => positions.push((id, anchor.position)),
            Ok(_) => {}
            Err(e) => eprintln!("Failed to read loco anchor {}: {:?}", id, e),
        }
    }

    (positions, active_ids)
}

#[derive(Default, Clone)]
pub(crate) struct PositioningData {
    pub(crate) lighthouse_bs: Vec<(u8, [f32; 3])>,  // (base station ID, position)
    pub(crate) loco_anchors: Vec<(u8, [f32; 3])>,   // (anchor ID, position)
    pub(crate) loco_seen: HashMap<u8, [f32; 3]>,    // all loco anchors ever seen (ID -> position)
    pub(crate) loco_active_ids: Vec<u8>,            // anchor IDs currently being received (firmware active list)
    pub(crate) loco_mode: u8,                       // 0=unknown/auto, 1=TWR, 2=TDoA2, 3=TDoA3 (loco.mode log)
    pub(crate) lighthouse_active: u16,
    pub(crate) loco_active: u16,
    // Anchor positions loaded from file to upload to the anchors over UWB.
    // The telemetry loop keeps (re)sending these until the read-back position
    // matches (the LPP anchor-position protocol is best-effort).
    pub(crate) loco_targets: HashMap<u8, [f32; 3]>, // desired anchor positions (ID -> position)
    pub(crate) loco_upload_active: bool,            // true while still pushing targets to the anchors
}

/// Tolerance (in meters, per axis) for considering a read-back anchor position
/// to match its loaded target — i.e. the upload "stuck".
pub(crate) const ANCHOR_POS_TOLERANCE: f32 = 0.02;

/// LPP short-packet type for setting an anchor's position (firmware
/// `LPP_SHORT_ANCHORPOS`). Payload is this byte followed by 3x little-endian f32.
pub(crate) const LPP_TYPE_ANCHOR_POSITION: u8 = 0x01;

/// One anchor entry in an anchor-positions YAML file (`{ id: {x, y, z} }`).
#[derive(serde::Deserialize, serde::Serialize)]
pub(crate) struct AnchorPositionEntry {
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) z: f32,
}

pub(crate) fn anchor_pos_matches(a: &[f32; 3], b: &[f32; 3]) -> bool {
    (a[0] - b[0]).abs() <= ANCHOR_POS_TOLERANCE
        && (a[1] - b[1]).abs() <= ANCHOR_POS_TOLERANCE
        && (a[2] - b[2]).abs() <= ANCHOR_POS_TOLERANCE
}

pub(crate) type SharedPositioningData = Arc<Mutex<PositioningData>>;

pub(crate) fn setup(ui: &AppWindow, cx: &AppContext) {
    let AppContext { swarm_state, positioning_source, positioning_data, .. } = cx;
    // Handle positioning source dropdown change
    {
        let positioning_source = positioning_source.clone();
        ui.on_positioning_source_changed(move |index| {
            let positioning_source = positioning_source.clone();
            host::spawn(async move {
                let mut ps = positioning_source.lock().await;
                *ps = Some(index as usize);
                eprintln!("Positioning source changed to unit {}", index);
            });
        });
    }

    // Continuous positioning data reader (every 2 seconds)
    {
        let swarm_state = swarm_state.clone();
        let positioning_data = positioning_data.clone();
        let positioning_source = positioning_source.clone();
        let ui_weak = ui.as_weak();

        host::spawn(async move {
            loop {
                host::sleep(std::time::Duration::from_secs(2)).await;

                let source_index = {
                    let ps = positioning_source.lock().await;
                    match *ps {
                        Some(idx) => idx,
                        None => continue,
                    }
                };

                let cf = {
                    let state = swarm_state.lock().await;
                    match state.get(&source_index) {
                        Some(cu) => cu.cf.clone(),
                        None => continue, // Not connected, skip
                    }
                };

                use crazyflie_connection_server::memory::{LighthouseMemory, MemoryType};

                let mut lighthouse_positions = Vec::new();

                // Read Lighthouse base station geometries
                let lh_mems = cf.memory.get_memories(Some(MemoryType::Lighthouse));
                if let Some(mem) = lh_mems.first() {
                    if let Ok(lh) = cf.memory.open_memory::<LighthouseMemory>(mem).await {
                        match lh.read_all_geometries().await {
                            Ok(geos) => {
                                for (id, geo) in &geos {
                                    if geo.valid {
                                        lighthouse_positions.push((*id, geo.origin));
                                    }
                                }
                            }
                            Err(e) => eprintln!("Failed to read lighthouse geometries: {:?}", e),
                        }
                    }
                }

                // Read Loco anchor positions and the list of currently-received anchors.
                let (loco_positions, loco_active_ids) = read_loco_anchor_positions(&cf).await;

                // Store positioning data and work out which anchor targets
                // still need (re)sending. We compute the send list while holding
                // the lock but send the (best-effort) LPP packets after dropping
                // it, so we never hold the mutex across an await.
                let mut anchors_to_send: Vec<(u8, [f32; 3])> = Vec::new();
                {
                    let mut pd = positioning_data.lock().await;
                    pd.lighthouse_bs = lighthouse_positions;
                    // Update the seen-cache with current positions
                    for &(id, pos) in &loco_positions {
                        pd.loco_seen.insert(id, pos);
                    }
                    pd.loco_anchors = loco_positions;
                    pd.loco_active_ids = loco_active_ids;

                    if pd.loco_upload_active && !pd.loco_targets.is_empty() {
                        let mut all_confirmed = true;
                        for (&id, target) in &pd.loco_targets {
                            let matched = pd
                                .loco_seen
                                .get(&id)
                                .map_or(false, |p| anchor_pos_matches(p, target));
                            if !matched {
                                all_confirmed = false;
                                anchors_to_send.push((id, *target));
                            }
                        }
                        // Once every anchor's read-back matches, stop resending.
                        if all_confirmed {
                            pd.loco_upload_active = false;
                        }
                    }

                    update_loco_anchors(&ui_weak, &pd);
                }

                // (Re)send anchor positions over UWB for anchors not yet confirmed.
                // LPP anchor-position packets are best-effort, hence the retry loop.
                for (id, pos) in anchors_to_send {
                    let mut data = Vec::with_capacity(13);
                    data.push(LPP_TYPE_ANCHOR_POSITION);
                    data.extend_from_slice(&pos[0].to_le_bytes());
                    data.extend_from_slice(&pos[1].to_le_bytes());
                    data.extend_from_slice(&pos[2].to_le_bytes());
                    if let Err(e) = cf
                        .localization
                        .loco_positioning
                        .send_short_lpp_packet(id, &data)
                        .await
                    {
                        eprintln!("Failed to send anchor {} position: {:?}", id, e);
                    }
                }
            }
        });
    }

    // Load anchor positions from file and start uploading them to the anchors.
    {
        let positioning_data = positioning_data.clone();
        let ui_weak = ui.as_weak();

        ui.on_load_anchor_positions(move || {
            let positioning_data = positioning_data.clone();
            let ui_weak = ui_weak.clone();
            host::spawn(async move {
                let Some(handle) = host::dialog::FileDialog::new()
                    .add_filter("YAML", &["yaml", "yml"])
                    .pick_file()
                    .await
                else { return };
                let path = handle.path().to_path_buf();

                let contents = match host::fs::read_to_string(&path) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("Failed to read anchor positions file: {}", e);
                        return;
                    }
                };

                // File format: a map of anchor id -> { x, y, z }.
                let entries: std::collections::BTreeMap<u8, AnchorPositionEntry> =
                    match serde_yaml::from_str(&contents) {
                        Ok(c) => c,
                        Err(e) => {
                            eprintln!("Failed to parse anchor positions file: {}", e);
                            return;
                        }
                    };

                if entries.is_empty() {
                    eprintln!("Anchor positions file contained no anchors");
                    return;
                }

                let targets: HashMap<u8, [f32; 3]> = entries
                    .into_iter()
                    .map(|(id, e)| (id, [e.x, e.y, e.z]))
                    .collect();
                eprintln!("Loaded {} anchor positions from {}", targets.len(), path.display());

                {
                    let mut pd = positioning_data.lock().await;
                    pd.loco_targets = targets;
                    pd.loco_upload_active = true;
                    // Push the targets to the UI right away; the telemetry loop
                    // will keep sending and refresh the confirmed state.
                    update_loco_anchors(&ui_weak, &pd);
                }
            });
        });
    }

    // Stop the anchor-position upload (leave the loaded targets visible).
    {
        let positioning_data = positioning_data.clone();
        let ui_weak = ui.as_weak();

        ui.on_stop_anchor_upload(move || {
            let positioning_data = positioning_data.clone();
            let ui_weak = ui_weak.clone();
            host::spawn(async move {
                let mut pd = positioning_data.lock().await;
                pd.loco_upload_active = false;
                update_loco_anchors(&ui_weak, &pd);
            });
        });
    }
}
