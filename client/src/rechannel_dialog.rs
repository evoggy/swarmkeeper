//! The rechannel dialog: move the swarm onto several channels.

use std::sync::Arc;

use slint::{ComponentHandle, Model};

use crate::app::AppContext;
use crate::host::radios::link_uri;
use crate::flashing::disconnect_for_flashing;
use crate::settings::AppSettings;
use crate::swarm::{join_channels};
use crate::swarm_file::{load_swarm_config, uri_with_default_timeout, uri_with_link_timeout};
use crate::units::rebuild_table_rows;
use crate::{AppWindow, host::radios, RcRowState, rechannel, RechannelRow};
use crate::host;

/// The swarm file as the Rechannel dialog last read it, and the plan for it.
#[derive(Default)]
pub(crate) struct RechannelState {
    pub(crate) path: Option<std::path::PathBuf>,
    pub(crate) units: Vec<rechannel::Unit>,
    pub(crate) placements: Vec<rechannel::Placement>,
}

pub(crate) type SharedRechannelState = Arc<std::sync::Mutex<RechannelState>>;

/// Plan the move onto the channels in the dialog and show it.
pub(crate) fn rc_replan(ui: &AppWindow, state: &SharedRechannelState) {
    let mut state = state.lock().unwrap();
    let planned = rechannel::parse_channels(&ui.get_rc_channels())
        .and_then(|(channels, warnings)| Ok((rechannel::place(&state.units, &channels)?, channels, warnings)));
    let (placements, error) = match planned {
        Ok((placements, channels, warnings)) => {
            let moving = placements.iter().filter(|p| p.moves()).count();
            let on_radio = placements.iter().filter(|p| p.from.is_some()).count();
            let mut summary = format!("{} of {} move", moving, rechannel::crazyflies(on_radio));
            if channels.len() > on_radio {
                summary += "; more channels than Crazyflies, some stay empty";
            }
            for warning in warnings {
                summary += &format!("; {}", warning);
            }
            ui.set_rc_summary(summary.into());
            (placements, String::new())
        }
        Err(e) => (Vec::new(), format!("{:#}", e)),
    };

    // Without a plan the rows still list the swarm.
    let rows: Vec<RechannelRow> = if placements.is_empty() {
        state
            .units
            .iter()
            .map(|unit| {
                let from = radios::RadioUri::parse(&unit.uri).ok().flatten().map(|uri| uri.channel.to_string());
                RechannelRow {
                    name: unit.name.clone().into(),
                    uri: unit.uri.clone().into(),
                    from: from.clone().unwrap_or_default().into(),
                    to: from.unwrap_or_default().into(),
                    moves: false,
                    state: RcRowState::Idle,
                    status: "".into(),
                }
            })
            .collect()
    } else {
        placements
            .iter()
            .map(|p| RechannelRow {
                name: p.name.clone().into(),
                uri: p.uri.clone().into(),
                from: p.from.map(|c| c.to_string()).unwrap_or_default().into(),
                to: p.to.map(|c| c.to_string()).unwrap_or_default().into(),
                moves: p.moves(),
                state: RcRowState::Idle,
                status: "".into(),
            })
            .collect()
    };
    ui.set_rc_moving(placements.iter().filter(|p| p.moves()).count() as i32);
    ui.set_rc_rows(slint::ModelRc::new(slint::VecModel::from(rows)));
    ui.set_rc_plan_error(error.into());
    state.placements = placements;
}

/// Apply one rechannel event to the dialog (UI thread).
pub(crate) fn apply_rc_event(ui: &AppWindow, event: rechannel::Event) {
    match event {
        rechannel::Event::Unit { unit, state, status } => {
            let rows = ui.get_rc_rows();
            let Some(mut row) = rows.row_data(unit) else { return };
            row.state = match state {
                rechannel::State::Busy => RcRowState::Busy,
                rechannel::State::Done => RcRowState::Done,
                rechannel::State::Failed => RcRowState::Failed,
            };
            row.status = status.into();
            rows.set_row_data(unit, row);
        }
        rechannel::Event::Overall(status) => ui.set_rc_status(status.into()),
    }
}

pub(crate) fn setup(ui: &AppWindow, cx: &AppContext) {
    let AppContext { settings: _, link_context, connection, swarm_state, radio_plan, positioning_source, .. } = cx;
    // Rechannel swarm dialog: move the Crazyflies onto several channels so
    // that the swarm can use one Crazyradio per channel.
    {
        let rc_state: SharedRechannelState = Arc::default();

        // Read the swarm file and plan with one channel per Crazyradio.
        {
            let ui_weak = ui.as_weak();
            let rc_state = rc_state.clone();
            ui.on_rc_open(move || {
                let Some(ui) = ui_weak.upgrade() else { return };
                if ui.get_rc_running() {
                    return;
                }
                let settings: AppSettings = host::store::load();
                let shared = settings.last_shared_swarm.filter(|_| settings.last_swarm_config.is_none());
                let path = settings.last_swarm_config.map(std::path::PathBuf::from);
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
                // The rows double as the unit list's order, so the file must
                // still match what was opened.
                let matches_list = units.len() == ui.get_units().row_count()
                    && units.iter().zip(ui.get_units().iter()).all(|(file, listed)| file.name == listed.name.as_str());
                {
                    let mut state = rc_state.lock().unwrap();
                    state.path = path;
                    state.units = if matches_list { units } else { Vec::new() };
                }

                let radios = radios::attached_radios();
                let count = radios.max(1) as i32;
                ui.set_rc_radios(radios as i32);
                ui.set_rc_count(count);
                ui.set_rc_channels(join_channels(&rechannel::channels_for_count(count as u8)).into());
                ui.set_rc_status("".into());
                ui.set_rc_summary("".into());
                rc_replan(&ui, &rc_state);
                if let Some(id) = shared {
                    ui.set_rc_plan_error(
                        format!(
                            "'{}' is shared on the server, and Swarmkeeper can't change shared swarms yet: \
                             rechannel it with cfcli (cfcli swarm rechannel), then reload it here",
                            id
                        )
                        .into(),
                    );
                } else if !matches_list {
                    ui.set_rc_plan_error("The swarm file changed since it was opened; open it again".into());
                }
            });
        }

        {
            let ui_weak = ui.as_weak();
            let rc_state = rc_state.clone();
            ui.on_rc_count_edited(move |count| {
                let Some(ui) = ui_weak.upgrade() else { return };
                ui.set_rc_channels(join_channels(&rechannel::channels_for_count(count.clamp(1, 41) as u8)).into());
                rc_replan(&ui, &rc_state);
            });
        }

        {
            let ui_weak = ui.as_weak();
            let rc_state = rc_state.clone();
            ui.on_rc_channels_edited(move |_| {
                let Some(ui) = ui_weak.upgrade() else { return };
                rc_replan(&ui, &rc_state);
            });
        }

        {
            let link_context = link_context.clone();
            let radio_plan = radio_plan.clone();
            let connection = connection.clone();
            let swarm_state = swarm_state.clone();
            let positioning_source = positioning_source.clone();
            let rc_state = rc_state.clone();
            let ui_weak = ui.as_weak();
            ui.on_rc_start(move || {
                let Some(ui) = ui_weak.upgrade() else { return };
                if ui.get_rc_running() {
                    return;
                }
                ui.set_rc_running(true);
                ui.set_rc_status("Disconnecting the swarm…".into());

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

                    let (path, placements) = {
                        let mut state = rc_state.lock().unwrap();
                        (state.path.clone(), std::mem::take(&mut state.placements))
                    };
                    let report: rechannel::Report = {
                        let ui_weak = ui_weak.clone();
                        Arc::new(move |event| {
                            let ui_weak = ui_weak.clone();
                            slint::invoke_from_event_loop(move || {
                                if let Some(ui) = ui_weak.upgrade() {
                                    apply_rc_event(&ui, event);
                                }
                            }).ok();
                        })
                    };
                    // Through a connection server, the server picks the
                    // Crazyradios
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

                    // The swarm file and the unit list follow the Crazyflies
                    // that answer on their new channel.
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
                    radio_plan.lock().await.set_swarm(uris.clone());
                    if let Some(path) = &path {
                        let mut state = rc_state.lock().unwrap();
                        state.units = placements
                            .iter()
                            .map(|p| rechannel::Unit {
                                name: p.name.clone(),
                                uri: if moved.contains(&p.unit) { p.new_uri.clone() } else { p.uri.clone() },
                            })
                            .collect();
                        state.path = Some(path.clone());
                    }

                    let ui_weak = ui_weak.clone();
                    slint::invoke_from_event_loop(move || {
                        let Some(ui) = ui_weak.upgrade() else { return };
                        let units = ui.get_units();
                        for (unit, uri) in new_uris {
                            if let Some(mut row) = units.row_data(unit) {
                                row.uri = uri_with_default_timeout(&uri).into();
                                row.radio = -1;
                                units.set_row_data(unit, row);
                            }
                        }
                        rebuild_table_rows(&ui);
                        let status = ui.get_rc_status();
                        match saved {
                            Ok(true) => ui.set_rc_status(format!("{}; swarm file updated", status).into()),
                            Ok(false) => {}
                            Err(e) => ui.set_rc_status(
                                format!("{}; the swarm file could not be updated: {:#}", status, e).into(),
                            ),
                        }
                        ui.set_rc_running(false);
                        // Keep the results on screen; the plan follows on the
                        // next edit or when the dialog is opened again.
                        ui.set_rc_moving(0);
                    }).ok();
                });
            });
        }
    }
}
