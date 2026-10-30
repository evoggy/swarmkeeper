//! The rechannel dialog: move the swarm onto several channels.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use slint::{ComponentHandle, Model, ModelRc, VecModel};

use crate::app::AppContext;
use crate::flashing::{connected, disconnect_for_flashing};
use crate::host;
use crate::host::radios::{self, link_uri, RadioUri};
use crate::rechannel::{self, Placement, State};
use crate::settings::AppSettings;
use crate::swarm::join_channels;
use crate::swarm_file::{load_swarm_config, uri_with_default_timeout, uri_with_link_timeout};
use crate::units::rebuild_table_rows;
use crate::{AppWindow, RcChip, RcLane, RcMove, RcRowState, RechannelBridge};

/// The swarm file as the dialog last read it, the channels and the plan, and
/// how the moves went.
#[derive(Default)]
pub(crate) struct RechannelState {
    path: Option<PathBuf>,
    units: Vec<rechannel::Unit>,
    /// Not connected when the dialog opened, by unit
    offline: Vec<bool>,
    radios: usize,
    channels: Vec<u8>,
    placements: Vec<Placement>,
    /// Why this swarm can't be moved at all
    refused: Option<String>,
    plan_error: String,
    warnings: Vec<String>,
    channel_error: String,
    /// How each move went, by unit
    progress: Vec<Option<(State, String)>>,
    running: bool,
    finished: bool,
    status: String,
}

pub(crate) type SharedRechannelState = Arc<Mutex<RechannelState>>;

impl RechannelState {
    /// The channels the Crazyflies are on now.
    fn current_channels(&self) -> Vec<u8> {
        self.units
            .iter()
            .filter_map(|unit| RadioUri::parse(&unit.uri).ok().flatten())
            .map(|uri| uri.channel)
            .collect()
    }

    /// Plan the move onto the channels.
    fn replan(&mut self) {
        self.channel_error.clear();
        let planned = rechannel::parse_channels(&join_channels(&self.channels))
            .and_then(|(channels, warnings)| Ok((rechannel::place(&self.units, &channels)?, warnings)));
        (self.placements, self.warnings, self.plan_error) = match planned {
            Ok((placements, warnings)) => (placements, warnings, String::new()),
            Err(e) => (Vec::new(), Vec::new(), format!("{:#}", e)),
        };
        self.progress = vec![None; self.units.len()];
    }

    fn movers(&self) -> impl Iterator<Item = &Placement> {
        self.placements.iter().filter(|p| p.moves())
    }

    fn offline(&self, unit: usize) -> bool {
        self.offline.get(unit).copied().unwrap_or(true)
    }

    fn row_state(&self, unit: usize) -> RcRowState {
        match self.progress.get(unit).and_then(|p| p.as_ref()) {
            Some((State::Busy, _)) => RcRowState::Busy,
            Some((State::Done, _)) => RcRowState::Done,
            Some((State::Failed, _)) => RcRowState::Failed,
            None => RcRowState::Idle,
        }
    }

    /// "36 of 49 move" and where they go.
    fn plan_text(&self) -> (String, String) {
        let on_radio = self.placements.iter().filter(|p| p.from.is_some()).count();
        let moving = self.movers().count();
        if moving == 0 {
            return ("Nothing moves".into(), "Every unit is on one of these channels already, in an equal share.".into());
        }
        let staying: Vec<&Placement> = self.placements.iter().filter(|p| p.from.is_some() && !p.moves()).collect();
        let mut stay_channels: Vec<u8> = staying.iter().filter_map(|p| p.to).collect();
        stay_channels.sort();
        stay_channels.dedup();
        let mut text = match (staying.len(), stay_channels.as_slice()) {
            (0, _) => "None stays where it is".to_string(),
            (n, [channel]) => format!("{} stay on ch {}", n, channel),
            (n, _) => format!("{} stay on their channels", n),
        };
        let arrivals: Vec<(usize, u8)> = self
            .channels
            .iter()
            .map(|channel| (self.movers().filter(|p| p.to == Some(*channel)).count(), *channel))
            .filter(|(n, _)| *n > 0)
            .collect();
        for (i, (n, channel)) in arrivals.iter().enumerate() {
            text += &match i {
                0 => format!("; {} go to ch {}", n, channel),
                _ => format!(", {} to ch {}", n, channel),
            };
        }
        text += ".";
        if self.channels.len() > on_radio {
            text += " More channels than Crazyflies: some stay empty.";
        }
        for warning in &self.warnings {
            text += &format!(" {}.", warning);
        }
        (format!("{} of {} move", moving, on_radio), text)
    }

    /// Where it stands after the run: "14 of 36 moved, 2 failed".
    fn moves_note(&self) -> String {
        if !self.running && !self.finished {
            return "Only the units that change channel".into();
        }
        let movers: Vec<usize> = self.movers().map(|p| p.unit).collect();
        let count = |state: RcRowState| movers.iter().filter(|unit| self.row_state(**unit) == state).count();
        let failed = count(RcRowState::Failed);
        let mut note = format!("{} of {} moved", count(RcRowState::Done), movers.len());
        if failed > 0 {
            note += &format!(", {} failed", failed);
        }
        note
    }
}

/// Show the state in the dialog.
fn show(ui: &AppWindow, state: &RechannelState) {
    let bridge = ui.global::<RechannelBridge>();
    let locked = state.running || state.finished;

    let lanes: Vec<RcLane> = state
        .channels
        .iter()
        .enumerate()
        .map(|(j, channel)| {
            let units: Vec<RcChip> = state
                .placements
                .iter()
                .filter(|p| p.to == Some(*channel))
                .map(|p| RcChip {
                    name: p.name.clone().into(),
                    moves: p.moves(),
                    state: state.row_state(p.unit),
                    offline: state.offline(p.unit),
                })
                .collect();
            RcLane {
                channel: *channel as i32,
                radio: if j < state.radios { format!("Radio {}", j).into() } else { "".into() },
                shares_radio: state.radios > 0 && j >= state.radios,
                units: ModelRc::new(VecModel::from(units)),
            }
        })
        .collect();

    // The connected ones first, as they are the ones likely to move, and once
    // it's over the ones that failed
    let mut movers: Vec<&Placement> = state.movers().collect();
    movers.sort_by_key(|p| state.offline(p.unit));
    if state.finished {
        movers.sort_by_key(|p| state.row_state(p.unit) != RcRowState::Failed);
    }
    let moves: Vec<RcMove> = movers
        .iter()
        .map(|p| RcMove {
            name: p.name.clone().into(),
            from: p.from.map(|c| format!("ch {}", c)).unwrap_or_default().into(),
            to: p.to.map(|c| format!("ch {}", c)).unwrap_or_default().into(),
            state: state.row_state(p.unit),
            status: match state.progress.get(p.unit).and_then(|p| p.as_ref()) {
                Some((_, status)) => status.clone(),
                None if state.offline(p.unit) && !locked => "Not connected".into(),
                None => "Waiting".into(),
            }
            .into(),
        })
        .collect();

    let offline = movers.iter().filter(|p| state.offline(p.unit)).count();
    let (plan_title, plan_text) = state.plan_text();
    bridge.set_radios(state.radios as i32);
    bridge.set_channels(ModelRc::new(VecModel::from(
        state.channels.iter().map(|c| *c as i32).collect::<Vec<_>>(),
    )));
    bridge.set_lane_units(lanes.iter().map(|lane| lane.units.row_count()).max().unwrap_or(0) as i32);
    bridge.set_lanes_share(lanes.iter().any(|lane| lane.shares_radio));
    bridge.set_lanes(ModelRc::new(VecModel::from(lanes)));
    bridge.set_moves(ModelRc::new(VecModel::from(moves)));
    bridge.set_plan_title(plan_title.into());
    bridge.set_plan_text(plan_text.into());
    bridge.set_plan_error(state.refused.clone().unwrap_or_else(|| state.plan_error.clone()).into());
    bridge.set_offline_text(
        match offline {
            _ if locked => String::new(),
            0 => String::new(),
            1 => "1 of them is not connected".into(),
            n => format!("{} of them are not connected", n),
        }
        .into(),
    );
    bridge.set_channel_error(state.channel_error.clone().into());
    bridge.set_moves_note(state.moves_note().into());
    bridge.set_status(state.status.clone().into());
    bridge.set_running(state.running);
    bridge.set_finished(state.finished);
    bridge.set_moving(state.movers().count() as i32);
}

/// Apply one rechannel event (UI thread).
fn apply_event(state: &mut RechannelState, event: rechannel::Event) {
    match event {
        rechannel::Event::Unit { unit, state: unit_state, status } => {
            if let Some(progress) = state.progress.get_mut(unit) {
                *progress = Some((unit_state, status));
            }
        }
        rechannel::Event::Overall(status) => state.status = status,
    }
}

/// Run an edit of the channels and show the new plan.
fn on_edit(ui: &AppWindow, state: &SharedRechannelState, edit: impl FnOnce(&mut RechannelState)) {
    let mut state = state.lock().unwrap();
    if state.running || state.finished {
        return;
    }
    edit(&mut state);
    show(ui, &state);
}

/// Read the swarm file and plan, with one channel per Crazyradio.
fn open(ui: &AppWindow, state: &mut RechannelState) {
    let settings: AppSettings = host::store::load();
    let shared = settings.last_shared_swarm.filter(|_| settings.last_swarm_config.is_none());
    let path = settings.last_swarm_config.map(PathBuf::from);
    let units: Vec<rechannel::Unit> = match path.as_deref().map(load_swarm_config) {
        Some(Ok(config)) => config
            .units
            .into_iter()
            .map(|u| rechannel::Unit { name: u.name, uri: u.uri })
            .collect(),
        Some(Err(e)) => {
            eprintln!("{}", e);
            Vec::new()
        }
        None => Vec::new(),
    };
    // The moves go by the unit list's order, so the file must still match
    // what was opened.
    let listed = ui.get_units();
    let matches_list = units.len() == listed.row_count()
        && units.iter().zip(listed.iter()).all(|(file, listed)| file.name == listed.name.as_str());

    *state = RechannelState {
        path,
        offline: listed.iter().map(|u| !connected(&u)).collect(),
        units: if matches_list { units } else { Vec::new() },
        radios: radios::attached_radios(),
        ..Default::default()
    };
    state.channels = rechannel::channels_keeping(&state.current_channels(), state.radios.max(1));
    state.refused = if let Some(id) = shared {
        Some(format!(
            "'{}' is shared on the server, and Swarmkeeper can't change shared swarms yet: \
             rechannel it with cfcli (cfcli swarm rechannel), then reload it here",
            id
        ))
    } else if !matches_list {
        Some("The swarm file changed since it was opened; open it again".into())
    } else {
        None
    };
    state.replan();
}

/// The edits of the channel list.
fn wire_edits(ui: &AppWindow, rc_state: &SharedRechannelState) {
    let bridge = ui.global::<RechannelBridge>();
    {
        let ui_weak = ui.as_weak();
        let rc_state = rc_state.clone();
        bridge.on_set_count(move |count| {
            let Some(ui) = ui_weak.upgrade() else { return };
            on_edit(&ui, &rc_state, |state| {
                let count = count.max(1) as usize;
                state.channels.truncate(count);
                while state.channels.len() < count {
                    let Some(channel) = rechannel::next_channel(&state.channels) else { break };
                    state.channels.push(channel);
                }
                state.replan();
            });
        });
    }

    {
        let ui_weak = ui.as_weak();
        let rc_state = rc_state.clone();
        bridge.on_remove_channel(move |channel| {
            let Some(ui) = ui_weak.upgrade() else { return };
            on_edit(&ui, &rc_state, |state| {
                if state.channels.len() > 1 {
                    state.channels.retain(|c| *c as i32 != channel);
                    state.replan();
                }
            });
        });
    }

    {
        let ui_weak = ui.as_weak();
        let rc_state = rc_state.clone();
        bridge.on_add_channel(move |text| {
            let Some(ui) = ui_weak.upgrade() else { return };
            on_edit(&ui, &rc_state, |state| {
                let text = text.trim();
                if text.is_empty() {
                    return;
                }
                match text.parse::<u8>().ok().filter(|c| *c <= 125) {
                    None => state.channel_error = format!("'{}' is not a channel: 0 to 125", text),
                    Some(channel) if state.channels.contains(&channel) => {
                        state.channel_error = format!("Channel {} is in the list already", channel)
                    }
                    Some(channel) => {
                        state.channels.push(channel);
                        state.replan();
                    }
                }
            });
        });
    }

    {
        let ui_weak = ui.as_weak();
        let rc_state = rc_state.clone();
        bridge.on_one_per_radio(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            on_edit(&ui, &rc_state, |state| {
                state.channels = rechannel::channels_keeping(&state.current_channels(), state.radios.max(1));
                state.replan();
            });
        });
    }
}

pub(crate) fn setup(ui: &AppWindow, cx: &AppContext) {
    let AppContext { link_context, connection, swarm_state, radio_plan, positioning_source, .. } = cx;
    let rc_state: SharedRechannelState = Arc::default();
    let bridge = ui.global::<RechannelBridge>();

    {
        let ui_weak = ui.as_weak();
        let rc_state = rc_state.clone();
        bridge.on_opened(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let mut state = rc_state.lock().unwrap();
            if !state.running {
                open(&ui, &mut state);
            }
            show(&ui, &state);
        });
    }

    wire_edits(ui, &rc_state);

    {
        let ui_weak = ui.as_weak();
        bridge.on_connect_swarm(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.invoke_connect_swarm();
            }
        });
    }

    let link_context = link_context.clone();
    let radio_plan = radio_plan.clone();
    let connection = connection.clone();
    let swarm_state = swarm_state.clone();
    let positioning_source = positioning_source.clone();
    let ui_weak = ui.as_weak();
    bridge.on_start(move || {
        let Some(ui) = ui_weak.upgrade() else { return };
        let (path, placements) = {
            let mut state = rc_state.lock().unwrap();
            if state.running || state.finished || state.refused.is_some() || state.movers().next().is_none() {
                return;
            }
            state.running = true;
            state.status = "Disconnecting the swarm…".into();
            show(&ui, &state);
            (state.path.clone(), state.placements.clone())
        };

        let link_context = link_context.clone();
        let radio_plan = radio_plan.clone();
        let connection = connection.clone();
        let swarm_state = swarm_state.clone();
        let positioning_source = positioning_source.clone();
        let rc_state = rc_state.clone();
        let ui_weak = ui_weak.clone();
        host::spawn(async move {
            disconnect_for_flashing(&swarm_state, &positioning_source, &ui_weak).await;
            let (client, drives_radios) = {
                let connection = connection.read().await;
                (connection.client().clone(), connection.drives_radios())
            };

            let report: rechannel::Report = {
                let ui_weak = ui_weak.clone();
                let rc_state = rc_state.clone();
                Arc::new(move |event| {
                    let ui_weak = ui_weak.clone();
                    let rc_state = rc_state.clone();
                    slint::invoke_from_event_loop(move || {
                        let Some(ui) = ui_weak.upgrade() else { return };
                        let mut state = rc_state.lock().unwrap();
                        apply_event(&mut state, event);
                        show(&ui, &state);
                    })
                    .ok();
                })
            };
            // Through a connection server, the server picks the Crazyradios
            let to_link: rechannel::LinkUri = {
                let (link_context, radio_plan) = (link_context.clone(), radio_plan.clone());
                Arc::new(move |uri: String| {
                    let (link_context, radio_plan) = (link_context.clone(), radio_plan.clone());
                    Box::pin(async move {
                        match drives_radios {
                            true => link_uri(&link_context, &radio_plan, &uri).await,
                            false => uri_with_link_timeout(&uri),
                        }
                    })
                })
            };
            let moved = rechannel::run(client, to_link, &placements, report).await;

            // The swarm file and the unit list follow the Crazyflies that
            // answer on their new channel.
            let new_uris: Vec<(usize, String)> = placements
                .iter()
                .filter(|p| moved.contains(&p.unit))
                .map(|p| (p.unit, p.new_uri.clone()))
                .collect();
            let saved = match (&path, new_uris.is_empty()) {
                (_, true) => Ok(false),
                (None, false) => Err(anyhow::anyhow!("no swarm file is open")),
                (Some(path), false) => host::fs::read_to_string(path)
                    .map_err(anyhow::Error::from)
                    .and_then(|text| rechannel::rewrite_swarm_file(&text, &new_uris))
                    .and_then(|text| host::fs::write(path, text).map_err(anyhow::Error::from))
                    .map(|()| true),
            };
            let mut uris: Vec<String> = placements.iter().map(|p| uri_with_default_timeout(&p.uri)).collect();
            for (unit, uri) in &new_uris {
                uris[*unit] = uri_with_default_timeout(uri);
            }
            radio_plan.lock().await.set_swarm(uris);

            let ui_weak = ui_weak.clone();
            slint::invoke_from_event_loop(move || {
                let Some(ui) = ui_weak.upgrade() else { return };
                let units = ui.get_units();
                for (unit, uri) in &new_uris {
                    if let Some(mut row) = units.row_data(*unit) {
                        row.uri = uri_with_default_timeout(uri).into();
                        row.radio = -1;
                        units.set_row_data(*unit, row);
                    }
                }
                rebuild_table_rows(&ui);

                let mut state = rc_state.lock().unwrap();
                for (unit, uri) in new_uris {
                    if let Some(u) = state.units.get_mut(unit) {
                        u.uri = uri;
                    }
                }
                match saved {
                    Ok(true) => state.status += "; swarm file updated",
                    Ok(false) => {}
                    Err(e) => state.status += &format!("; the swarm file could not be updated: {:#}", e),
                }
                // The results stay until the dialog is opened again.
                state.running = false;
                state.finished = true;
                show(&ui, &state);
            })
            .ok();
        });
    });
}

/// For screenshots (`SWARMKEEPER_RECHANNEL=plan|running|done`): the demo swarm
/// spread over four channels, before, during or after the moves.
pub fn seed_screenshot(ui: &AppWindow, phase: &str) {
    let listed = ui.get_units();
    let mut state = RechannelState {
        units: listed.iter().map(|u| rechannel::Unit { name: u.name.into(), uri: u.uri.into() }).collect(),
        offline: listed.iter().map(|u| !connected(&u)).collect(),
        radios: 4,
        ..Default::default()
    };
    state.channels = rechannel::channels_keeping(&state.current_channels(), 4);
    state.replan();
    if phase != "plan" {
        let mut movers: Vec<(usize, u8, u8)> = state.movers().map(|p| (p.unit, p.from.unwrap_or(0), p.to.unwrap_or(0))).collect();
        movers.sort_by_key(|(unit, _, _)| state.offline(*unit));
        for (k, (unit, from, to)) in movers.into_iter().enumerate() {
            state.progress[unit] = if state.offline(unit) {
                Some((State::Failed, format!("Doesn't answer on channel {} or {}", from, to)))
            } else if phase == "done" || k < 14 {
                Some((State::Done, format!("Moved to channel {}", to)))
            } else if k == 14 {
                Some((State::Busy, format!("Rebooting onto channel {}…", to)))
            } else if k == 15 {
                Some((State::Busy, format!("Reprogramming to channel {}…", to)))
            } else {
                None
            };
        }
        state.running = phase == "running";
        state.finished = phase == "done";
        state.status = match phase {
            "done" => "34 Crazyflies moved, 2 failed; swarm file updated".into(),
            _ => "Reprogramming…".into(),
        };
    }
    ui.global::<RechannelBridge>().set_visible(true);
    show(ui, &state);
    wire_edits(ui, &Arc::new(Mutex::new(state)));
}
