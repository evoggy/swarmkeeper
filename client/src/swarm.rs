//! Connecting, disconnecting and following the units of the swarm.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use slint::{ComponentHandle, Model};
use tokio::sync::Mutex;

use crate::app::AppContext;
use crate::console::{SharedConsoleStore, start_console_capture};
use crate::journal::SharedJournalStore;
use crate::loco::SharedPositioningData;
use crate::settings::AppSettings;
use crate::swarm_file::uri_with_link_timeout;
use crate::telemetry::start_telemetry;
use crate::host::toc_cache::FileTocCache;
use crate::trajectory::SharedGotoTargets;
use crate::units::{sort_unit_indices, update_unit};
use crate::host::radios::link_uri;
use crate::{AppWindow, connection, host::radios, host::swarm_connect, UnitState};
use crate::host;

pub(crate) struct ConnectedUnit {
    /// The unit through the client API: through a connection server, or
    /// through the one in Swarmkeeper. `cf.lib()` is the crazyflie-lib
    /// connection behind it when Swarmkeeper drives the Crazyradios itself.
    pub(crate) cf: Arc<crazyflie_connection_server::Crazyflie>,
    pub(crate) identify_stop: Option<Arc<AtomicBool>>,
    pub(crate) home_position: Option<[f32; 3]>,
    // World-frame Z of the surface the unit launched from, captured at takeoff.
    // Landing descends back to this height; the world origin may be non-zero
    // (even negative), so a hard-coded 0.0 land target is unsafe.
    pub(crate) launch_z: Option<f32>,
}

impl ConnectedUnit {
    /// Disconnect, or through a connection server detach: the server keeps
    /// the Crazyflie connected for its other clients.
    pub(crate) async fn disconnect(self) {
        disconnect_cf(&self.cf).await;
    }
}

/// Disconnect a unit, or through a connection server detach from it. Its
/// telemetry and console streams end either way.
pub(crate) async fn disconnect_cf(cf: &crazyflie_connection_server::Crazyflie) {
    swarm_connect::disconnect(cf).await;
}

pub(crate) type SwarmState = Arc<Mutex<HashMap<usize, ConnectedUnit>>>;

/// The connected units, through the client API.
pub(crate) fn client_units(state: &HashMap<usize, ConnectedUnit>) -> Vec<(usize, Arc<crazyflie_connection_server::Crazyflie>)> {
    state.iter().map(|(idx, cu)| (*idx, cu.cf.clone())).collect()
}

/// A channel list as the Rechannel dialog shows it: "80, 78, 76".
pub(crate) fn join_channels(channels: &[u8]) -> String {
    channels.iter().map(|c| c.to_string()).collect::<Vec<_>>().join(", ")
}

/// The URIs of the units in the swarm, in unit order.
pub(crate) fn unit_uris(ui: &AppWindow) -> Vec<String> {
    ui.get_units().iter().map(|unit| unit.uri.to_string()).collect()
}


/// What setting up a freshly connected unit needs.
#[derive(Clone)]
pub(crate) struct UnitSetup {
    pub(crate) swarm_state: SwarmState,
    pub(crate) ui_weak: slint::Weak<AppWindow>,
    pub(crate) positioning_source: Arc<Mutex<Option<usize>>>,
    pub(crate) positioning_data: SharedPositioningData,
    pub(crate) goto_targets: SharedGotoTargets,
    pub(crate) journal_store: SharedJournalStore,
    pub(crate) console_store: SharedConsoleStore,
    pub(crate) unit_count: usize,
    pub(crate) connection: connection::SharedConnection,
}

/// Connect to these units (index, URI) and set each one up as soon as it is
/// connected. When Swarmkeeper drives the Crazyradios, they are spread over
/// them by the radio plan, a few at a time on each, and each TOC that isn't in
/// the cache yet is downloaded from one Crazyflie only. Through a connection
/// server, the server does that part.
pub(crate) async fn connect_units(
    units: Vec<(usize, String)>,
    link_context: Arc<radios::LinkContext>,
    radio_plan: radios::SharedRadioPlan,
    toc_cache: FileTocCache,
    setup: UnitSetup,
) {
    let (drives_radios, client) = {
        let connection = setup.connection.read().await;
        (connection.drives_radios(), connection.client().clone())
    };
    if !drives_radios {
        return attach_units(units, client, setup).await;
    }

    // A Crazyflie the server already has (another program attached to it, or
    // it lingers after a one-off job) is attached to, not connected again: a
    // second crazyflie-lib connection would reset its log blocks.
    let served: std::collections::HashSet<String> =
        client.list().await.map(|cfs| cfs.into_iter().map(|cf| cf.uri).collect()).unwrap_or_default();
    let (served_units, units): (Vec<_>, Vec<_>) = units.into_iter().partition(|(_, uri)| {
        crazyflie_connection_server::normalize_uri(uri).is_ok_and(|uri| served.contains(&uri.key))
    });
    if !served_units.is_empty() {
        host::spawn(attach_units(served_units, client.clone(), setup.clone()));
    }

    let mut targets = Vec::new();
    for (index, uri) in &units {
        let link_uri = link_uri(&link_context, &radio_plan, uri).await;
        let radio = radios::RadioUri::parse(&link_uri).ok().flatten().and_then(|r| r.radio);
        update_unit(&setup.ui_weak, *index, move |u| {
            u.radio = radio.map_or(-1, |r| r as i32);
        });
        eprintln!("Connecting to {} ...", link_uri);
        targets.push(swarm_connect::Target { index: *index, link_uri });
    }
    let uris: Arc<HashMap<usize, String>> = Arc::new(units.into_iter().collect());

    let connected: swarm_connect::Connected = {
        let uris = uris.clone();
        Arc::new(move |index, result| match result {
            Ok(cf) => {
                eprintln!("Connected to {}", uris[&index]);
                let (uri, client, setup) = (uris[&index].clone(), client.clone(), setup.clone());
                host::spawn(async move {
                    match swarm_connect::adopt(&client, &uri, cf).await {
                        Ok(cf) => setup_connected_unit(index, uri, Arc::new(cf), setup).await,
                        Err(e) => {
                            let error_msg = e.to_string();
                            update_unit(&setup.ui_weak, index, move |u| {
                                u.state = UnitState::Error;
                                u.error_message = error_msg.into();
                            });
                        }
                    }
                });
            }
            Err(error_msg) => update_unit(&setup.ui_weak, index, move |u| {
                u.state = UnitState::Error;
                u.error_message = error_msg.into();
            }),
        })
    };
    let waiting: swarm_connect::Waiting = Arc::new(move |index| {
        eprintln!("{}: waiting for another Crazyflie to download the TOC", uris[&index])
    });
    swarm_connect::connect_all(link_context, toc_cache, targets, connected, waiting).await;
}

/// Attach to these units through a connection server, all at once, and set
/// each one up as soon as it is attached.
pub(crate) async fn attach_units(units: Vec<(usize, String)>, client: crazyflie_connection_server::Client, setup: UnitSetup) {
    let mut join_set = host::JoinSet::new();
    for (index, uri) in units {
        eprintln!("Attaching to {} through {} ...", uri, client.mode());
        let (client, setup) = (client.clone(), setup.clone());
        join_set.spawn(async move {
            match client.attach(&uri_with_link_timeout(&uri)).await {
                Ok(cf) => setup_connected_unit(index, uri, Arc::new(cf), setup).await,
                Err(e) => {
                    let error_msg = e.to_string();
                    update_unit(&setup.ui_weak, index, move |u| {
                        u.state = UnitState::Error;
                        u.error_message = error_msg.into();
                    });
                }
            }
        });
    }
    while join_set.join_next().await.is_some() {}
}

/// Follow the jobs on the connection server: a unit that a job (flashing,
/// started by Swarmkeeper or by another program) takes over is attached again
/// when the job gives it back. Ends when the connection closes.
pub(crate) async fn follow_jobs(
    client: crazyflie_connection_server::Client,
    link_context: Arc<radios::LinkContext>,
    radio_plan: radios::SharedRadioPlan,
    toc_cache: FileTocCache,
    setup: UnitSetup,
) {
    use crazyflie_connection_server::ServerEvent;
    use futures::StreamExt;
    let mut events = match client.watch().await {
        Ok(events) => events,
        Err(e) => {
            eprintln!("Not following the connection server's jobs: {}", e);
            return;
        }
    };
    let radios = host::spawn(follow_radios(client.clone(), setup.clone()));
    // The units each running job took over, by URI.
    let mut taken: HashMap<String, Vec<usize>> = HashMap::new();
    while let Some(event) = events.next().await {
        match event {
            ServerEvent::Busy { uri, reason } => {
                let mut state = setup.swarm_state.lock().await;
                let indices: Vec<usize> =
                    state.iter().filter(|(_, unit)| unit.cf.uri() == uri).map(|(index, _)| *index).collect();
                if indices.is_empty() {
                    continue;
                }
                eprintln!("{}", reason);
                // The server disconnects it, which ends its telemetry.
                for index in &indices {
                    state.remove(index);
                }
                taken.insert(uri, indices);
            }
            ServerEvent::Available { uri, connected, moved_to } => {
                let Some(indices) = taken.remove(&uri) else { continue };
                // Rebooted onto another channel or address: whoever moved it
                // (e.g. a rechannel) changes the swarm and connects it there
                if let Some(moved_to) = moved_to {
                    eprintln!("{} moved to {}", uri, moved_to);
                    continue;
                }
                // Not when the swarm was disconnected meanwhile, nor for a unit
                // list that changed.
                let (tx, rx) = tokio::sync::oneshot::channel();
                let ui_weak = setup.ui_weak.clone();
                slint::invoke_from_event_loop(move || {
                    let units = ui_weak.upgrade().map(|ui| (ui.get_swarm_connected(), unit_uris(&ui)));
                    tx.send(units).ok();
                })
                .ok();
                let Ok(Some((true, uris))) = rx.await else { continue };
                let units: Vec<(usize, String)> = indices
                    .into_iter()
                    .filter_map(|index| Some((index, uris.get(index)?.clone())))
                    .filter(|(_, unit_uri)| {
                        crazyflie_connection_server::normalize_uri(unit_uri).is_ok_and(|unit_uri| unit_uri.key == uri)
                    })
                    .collect();
                eprintln!("{} is available again (connected: {}), attaching", uri, connected);
                let setup = UnitSetup { unit_count: uris.len(), ..setup.clone() };
                host::spawn(connect_units(units, link_context.clone(), radio_plan.clone(), toc_cache.clone(), setup));
            }
        }
    }
    radios.abort();
}

/// How often to ask the connection server which Crazyradio each unit is on.
const RADIOS_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// Through a connection server, show which of its Crazyradios each unit is
/// on, for the Radios panel on the Status screen. When Swarmkeeper drives the
/// Crazyradios, the radio plan says instead (see `connect_units`).
async fn follow_radios(client: crazyflie_connection_server::Client, setup: UnitSetup) {
    if setup.connection.read().await.drives_radios() {
        return;
    }
    let mut interval = host::interval(RADIOS_INTERVAL);
    loop {
        interval.tick().await;
        let Ok(cfs) = client.list().await else { continue };
        let radios: HashMap<String, usize> = cfs.into_iter().filter_map(|cf| Some((cf.uri, cf.radio?))).collect();
        let units: Vec<(usize, i32)> = setup
            .swarm_state
            .lock()
            .await
            .iter()
            .filter_map(|(index, unit)| Some((*index, *radios.get(unit.cf.uri())? as i32)))
            .collect();
        for (index, radio) in units {
            update_unit(&setup.ui_weak, index, move |u| u.radio = radio);
        }
    }
}

/// Read what the unit list shows about a freshly connected unit, start
/// capturing its console and start its telemetry.
pub(crate) async fn setup_connected_unit(i: usize, uri: String, cf: Arc<crazyflie_connection_server::Crazyflie>, setup: UnitSetup) {
    let UnitSetup {
        swarm_state,
        ui_weak,
        positioning_source,
        positioning_data,
        goto_targets,
        journal_store,
        console_store,
        unit_count,
        connection: _,
    } = setup;

    // Store connected Crazyflie
    {
        let mut state = swarm_state.lock().await;
        state.insert(i, ConnectedUnit { cf: cf.clone(), identify_stop: None, home_position: None, launch_z: None });
    }

    // Read installed decks. The client API carries values as f64, exact for
    // every type up to 32 bits.
    let param = async |name: &str, default: f64| cf.param.get_lossy(name).await.unwrap_or(default);
    let deck_lighthouse = param("deck.bcLighthouse4", 0.0).await;
    let deck_loco = param("deck.bcLoco", 0.0).await;
    let deck_led_top = param("deck.bcColorLedTop", 0.0).await;
    let deck_led_bottom = param("deck.bcColorLedBot", 0.0).await;

    // Read selftest result
    let selftest_passed = param("system.selftestPassed", 1.0).await;

    // Read CPU serial number
    let id0 = param("cpu.id0", 0.0).await as u32;
    let id1 = param("cpu.id1", 0.0).await as u32;
    let id2 = param("cpu.id2", 0.0).await as u32;
    let serial = format!("{:08X}{:08X}{:08X}", id0, id1, id2);

    let journal_count = {
        let store = journal_store.lock().await;
        store.get(&serial).map_or(0, |entries| entries.len()) as i32
    };

    let console_count = {
        let store = console_store.lock().await;
        store.get(&serial).map_or(0, |lines| lines.len()) as i32
    };

    start_console_capture(
        serial.clone(),
        i,
        cf.clone(),
        ui_weak.clone(),
        console_store.clone(),
    );

    update_unit(&ui_weak, i, move |u| {
        u.state = UnitState::Connected;
        // An error from an earlier attempt (e.g. a job took the unit over
        // during its setup) no longer applies
        u.error_message = "".into();
        u.deck_lighthouse = deck_lighthouse != 0.0;
        u.deck_loco = deck_loco != 0.0;
        u.deck_led_top = deck_led_top != 0.0;
        u.deck_led_bottom = deck_led_bottom != 0.0;
        u.serial = serial.into();
        u.selftest_passed = selftest_passed != 0.0;
        u.journal_entry_count = journal_count;
        u.console_line_count = console_count;
    });

    // Update swarm-connected flag
    let ui_weak_inner = ui_weak.clone();
    slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak_inner.upgrade() {
            ui.set_swarm_connected(true);
        }
    }).ok();

    // Auto-select as positioning source if none selected (initial connect)
    {
        let mut ps: tokio::sync::MutexGuard<'_, Option<usize>> = positioning_source.lock().await;
        if ps.is_none() {
            *ps = Some(i);
            let ui_weak_inner = ui_weak.clone();
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak_inner.upgrade() {
                    ui.set_positioning_source_index(i as i32);
                }
            }).ok();
            eprintln!("Auto-selected unit {} as positioning source", i);
        }
    }

    start_telemetry(i, uri, cf, ui_weak, positioning_data, positioning_source, goto_targets, unit_count).await;
}

pub(crate) async fn setup(ui: &AppWindow, cx: &AppContext) {
    let AppContext { settings: _, link_context, toc_cache, connection, swarm_state, radio_plan, positioning_source, positioning_data, goto_targets, journal_store, console_store, .. } = cx;
    // Connect swarm
    {
        let link_context = link_context.clone();
        let radio_plan = radio_plan.clone();
        let toc_cache = toc_cache.clone();
        let ui_weak = ui.as_weak();
        let setup = UnitSetup {
            swarm_state: swarm_state.clone(),
            ui_weak: ui.as_weak(),
            positioning_source: positioning_source.clone(),
            positioning_data: positioning_data.clone(),
            goto_targets: goto_targets.clone(),
            journal_store: journal_store.clone(),
            console_store: console_store.clone(),
            unit_count: 0,
            connection: connection.clone(),
        };
        ui.on_connect_swarm(move || {
            let Some(ui_ref) = ui_weak.upgrade() else { return };
            let units = unit_uris(&ui_ref).into_iter().enumerate().collect();
            let setup = UnitSetup { unit_count: ui_ref.get_units().row_count(), ..setup.clone() };
            let link_context = link_context.clone();
            let radio_plan = radio_plan.clone();
            let toc_cache = toc_cache.clone();
            host::spawn(async move {
                // Spread the swarm over the Crazyradios attached now. Through
                // a connection server, the server picks the radios.
                if setup.connection.read().await.drives_radios() {
                    match radio_plan.lock().await.replan(&link_context).await {
                        Ok(warnings) => {
                            for warning in warnings {
                                eprintln!("Warning: {}", warning);
                            }
                        }
                        Err(e) => eprintln!("Planning the Crazyradios: {:#}", e),
                    }
                }
                connect_units(units, link_context, radio_plan, toc_cache, setup).await;
            });
        });
    }

    // SWARMKEEPER_CONNECT=1 connects the swarm as soon as it is open, without
    // a click, e.g. to try a connection server from a terminal
    if host::env::var("SWARMKEEPER_CONNECT").is_some() {
        let ui_weak = ui.as_weak();
        host::spawn(async move {
            loop {
                host::sleep(std::time::Duration::from_millis(500)).await;
                let (tx, rx) = tokio::sync::oneshot::channel();
                let ui_weak = ui_weak.clone();
                slint::invoke_from_event_loop(move || {
                    let open = ui_weak.upgrade().filter(|ui| ui.get_units().row_count() > 0);
                    if let Some(ui) = &open {
                        ui.invoke_connect_swarm();
                    }
                    tx.send(open.is_some()).ok();
                })
                .ok();
                if rx.await.unwrap_or(true) {
                    break;
                }
            }
        });
    }

    // The connection menu at the right of the menu bar
    {
        let ui_weak = ui.as_weak();
        let shown = connection.read().await;
        connection::show(&ui, &shown, "");
        drop(shown);

        // List the other programs when the menu opens.
        let opened = connection.clone();
        ui.on_connection_opened(move || {
            let (connection, ui_weak) = (opened.clone(), ui_weak.clone());
            host::spawn(async move {
                let connection = connection.read().await;
                let others = connection::others(&connection).await;
                let (label, text, mode) = (connection.label(), connection.description(), connection.mode.clone());
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_connection_label(label.into());
                        let text = if others.is_empty() { text } else { format!("{}\n\n{}", text, others) };
                        ui.set_connection_text(text.into());
                        ui.set_connection_mode(mode.into());
                    }
                })
                .ok();
            });
        });

        // Switch mode: disconnect, open the new mode, and connect the units
        // that were connected again through it.
        let setup = UnitSetup {
            swarm_state: swarm_state.clone(),
            ui_weak: ui.as_weak(),
            positioning_source: positioning_source.clone(),
            positioning_data: positioning_data.clone(),
            goto_targets: goto_targets.clone(),
            journal_store: journal_store.clone(),
            console_store: console_store.clone(),
            unit_count: 0,
            connection: connection.clone(),
        };
        let link_context = link_context.clone();
        let radio_plan = radio_plan.clone();
        let toc_cache = toc_cache.clone();
        let ui_weak = ui.as_weak();
        ui.on_connection_chosen(move |mode| {
            let mode = mode.trim().to_string();
            let Some(ui) = ui_weak.upgrade() else { return };
            if mode.is_empty() {
                return;
            }
            let uris = unit_uris(&ui);
            let setup = UnitSetup { unit_count: uris.len(), ..setup.clone() };
            let (link_context, radio_plan, toc_cache, ui_weak) =
                (link_context.clone(), radio_plan.clone(), toc_cache.clone(), ui_weak.clone());
            host::spawn(async move {
                *setup.positioning_source.lock().await = None;
                let units: Vec<(usize, ConnectedUnit)> = setup.swarm_state.lock().await.drain().collect();
                let mut reconnect = Vec::new();
                for (index, unit) in units {
                    unit.disconnect().await;
                    update_unit(&ui_weak, index, |u| u.state = UnitState::Disconnected);
                    if let Some(uri) = uris.get(index) {
                        reconnect.push((index, uri.clone()));
                    }
                }
                {
                    let mut connection = setup.connection.write().await;
                    connection.close().await;
                    *connection = connection::Connection::open(&mode, link_context.clone(), toc_cache.clone()).await;
                    host::spawn(follow_jobs(
                        connection.client().clone(),
                        link_context.clone(),
                        radio_plan.clone(),
                        toc_cache.clone(),
                        setup.clone(),
                    ));
                    let mut settings: AppSettings = host::store::load();
                    settings.connection = (mode != connection::DEFAULT_MODE).then(|| mode.clone());
                    if let Err(e) = host::store::save(&settings) {
                        eprintln!("Failed to save settings: {}", e);
                    }
                    let (label, text, mode) = (connection.label(), connection.description(), connection.mode.clone());
                    let drives_radios = connection.drives_radios();
                    let ui_weak = ui_weak.clone();
                    slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_weak.upgrade() {
                            ui.set_connection_label(label.into());
                            ui.set_connection_text(text.into());
                            ui.set_connection_mode(mode.into());
                            ui.set_drives_radios(drives_radios);
                            ui.set_swarm_connected(false);
                        }
                    })
                    .ok();
                }
                if !reconnect.is_empty() {
                    connect_units(reconnect, link_context, radio_plan, toc_cache, setup).await;
                }
            });
        });
    }

    // Disconnect swarm
    {
        let swarm_state = swarm_state.clone();
        let positioning_source = positioning_source.clone();
        let ui_weak = ui.as_weak();

        ui.on_disconnect_swarm(move || {
            let swarm_state = swarm_state.clone();
            let positioning_source = positioning_source.clone();
            let ui_weak = ui_weak.clone();

            host::spawn(async move {
                // Reset positioning source
                {
                    let mut ps = positioning_source.lock().await;
                    *ps = None;
                }
                let units: Vec<(usize, ConnectedUnit)> = {
                    let mut state = swarm_state.lock().await;
                    state.drain().collect()
                };

                for (index, connected) in units {
                    eprintln!("Disconnecting unit {} ...", index);
                    connected.disconnect().await;
                    update_unit(&ui_weak, index, |u| {
                        u.state = UnitState::Disconnected;
                        u.pos_x = 0.0;
                        u.pos_y = 0.0;
                        u.pos_z = 0.0;
                        u.battery_voltage = 0.0;
                        u.link_quality = 0.0;
                        u.deck_lighthouse = false;
                        u.deck_loco = false;
                        u.deck_led_top = false;
                        u.deck_led_bottom = false;
                        u.serial = "".into();
                        u.pm_state = "".into();
                        u.journal_entry_count = 0;
                        u.platform_type = "".into();
                        u.firmware_version = "".into();
                    });
                }

                let ui_weak_inner = ui_weak.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak_inner.upgrade() {
                        ui.set_swarm_connected(false);
                    }
                }).ok();
            });
        });
    }

    // Reconnect disconnected units
    {
        let link_context = link_context.clone();
        let radio_plan = radio_plan.clone();
        let toc_cache = toc_cache.clone();
        let ui_weak = ui.as_weak();
        let setup = UnitSetup {
            swarm_state: swarm_state.clone(),
            ui_weak: ui.as_weak(),
            positioning_source: positioning_source.clone(),
            positioning_data: positioning_data.clone(),
            goto_targets: goto_targets.clone(),
            journal_store: journal_store.clone(),
            console_store: console_store.clone(),
            unit_count: 0,
            connection: connection.clone(),
        };
        ui.on_reconnect_swarm(move || {
            let Some(ui_ref) = ui_weak.upgrade() else { return };
            let units = ui_ref.get_units();
            let disconnected: Vec<(usize, String)> = (0..units.row_count())
                .filter_map(|i| units.row_data(i).map(|unit| (i, unit)))
                .filter(|(_, unit)| matches!(unit.state, UnitState::Disconnected | UnitState::Error))
                .map(|(i, unit)| (i, unit.uri.to_string()))
                .collect();
            let setup = UnitSetup { unit_count: units.row_count(), ..setup.clone() };
            // The units still connected keep their Crazyradio, so the plan
            // stays as it is.
            host::spawn(connect_units(disconnected, link_context.clone(), radio_plan.clone(), toc_cache.clone(), setup));
        });
    }

    // Download TOC: connect to every unit and disconnect again, to fill the
    // cache. Each TOC is downloaded from one Crazyflie only.
    {
        let link_context = link_context.clone();
        let radio_plan = radio_plan.clone();
        let toc_cache = toc_cache.clone();
        let ui_weak = ui.as_weak();

        ui.on_download_toc(move || {
            let Some(ui_ref) = ui_weak.upgrade() else { return };
            let uris = unit_uris(&ui_ref);
            let names: Arc<Vec<String>> = Arc::new(ui_ref.get_units().iter().map(|u| u.name.to_string()).collect());

            ui_ref.set_progress_dialog_visible(true);
            ui_ref.set_progress_dialog_progress(0.0);
            ui_ref.set_progress_dialog_status("Starting...".into());
            ui_ref.set_progress_dialog_title("Downloading TOC".into());

            let link_context = link_context.clone();
            let radio_plan = radio_plan.clone();
            let toc_cache = toc_cache.clone();
            let ui_weak = ui_weak.clone();

            host::spawn(async move {
                let total = uris.len();
                let mut targets = Vec::new();
                for (index, uri) in uris.iter().enumerate() {
                    let link_uri = link_uri(&link_context, &radio_plan, uri).await;
                    targets.push(swarm_connect::Target { index, link_uri });
                }

                let done = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let connected: swarm_connect::Connected = {
                    let ui_weak = ui_weak.clone();
                    let names = names.clone();
                    Arc::new(move |index, result| {
                        let outcome = match result {
                            Ok(cf) => {
                                host::spawn(async move { cf.disconnect().await });
                                "done"
                            }
                            Err(error_msg) => {
                                update_unit(&ui_weak, index, move |u| {
                                    u.state = UnitState::Error;
                                    u.error_message = error_msg.into();
                                });
                                "failed"
                            }
                        };
                        let finished = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                        let progress = finished as f32 / total.max(1) as f32;
                        let status: slint::SharedString =
                            format!("{} {} ({}/{})", names[index], outcome, finished, total).into();
                        let ui_weak = ui_weak.clone();
                        slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_weak.upgrade() {
                                ui.set_progress_dialog_progress(progress);
                                ui.set_progress_dialog_status(status);
                            }
                        }).ok();
                    })
                };
                let waiting: swarm_connect::Waiting = Arc::new(move |index| {
                    eprintln!("{}: waiting for another Crazyflie to download the TOC", names[index])
                });
                swarm_connect::connect_all(link_context, toc_cache, targets, connected, waiting).await;

                // Brief pause so the user sees 100% before closing
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

    // Connect single unit
    {
        let link_context = link_context.clone();
        let radio_plan = radio_plan.clone();
        let toc_cache = toc_cache.clone();
        let swarm_state = swarm_state.clone();
        let ui_weak = ui.as_weak();
        let positioning_source = positioning_source.clone();
        let positioning_data = positioning_data.clone();
        let goto_targets = goto_targets.clone();
        let journal_store = journal_store.clone();
        let console_store = console_store.clone();

        let connection = connection.clone();
        ui.on_connect_unit(move |row_index| {
            if row_index < 0 {
                return;
            }

            let (original_index, uri, unit_count) = {
                let Some(ui) = ui_weak.upgrade() else { return };
                let units = ui.get_units();
                let unit_count = units.row_count();
                let col = ui.get_sort_column();
                let ascending = ui.get_sort_ascending();
                let indices = sort_unit_indices(&units, col, ascending);
                let row = row_index as usize;
                if row >= indices.len() {
                    return;
                }
                let idx = indices[row];
                let sorted = ui.get_sorted_units();
                let unit = sorted.row_data(row).unwrap();
                if !matches!(unit.state, UnitState::Disconnected | UnitState::Error) {
                    eprintln!("Unit {} is already connected", idx);
                    return;
                }
                (idx, unit.uri.to_string(), unit_count)
            };

            let setup = UnitSetup {
                swarm_state: swarm_state.clone(),
                ui_weak: ui_weak.clone(),
                positioning_source: positioning_source.clone(),
                positioning_data: positioning_data.clone(),
                goto_targets: goto_targets.clone(),
                journal_store: journal_store.clone(),
                console_store: console_store.clone(),
                unit_count,
                connection: connection.clone(),
            };
            host::spawn(connect_units(
                vec![(original_index, uri)],
                link_context.clone(),
                radio_plan.clone(),
                toc_cache.clone(),
                setup,
            ));
        });
    }

    // Disconnect single unit
    {
        let swarm_state = swarm_state.clone();
        let positioning_source = positioning_source.clone();
        let ui_weak = ui.as_weak();

        ui.on_disconnect_unit(move |row_index| {
            if row_index < 0 {
                return;
            }

            let original_index = {
                let Some(ui) = ui_weak.upgrade() else { return };
                let units = ui.get_units();
                let col = ui.get_sort_column();
                let ascending = ui.get_sort_ascending();
                let indices = sort_unit_indices(&units, col, ascending);
                let row = row_index as usize;
                if row >= indices.len() {
                    return;
                }
                indices[row]
            };

            let swarm_state = swarm_state.clone();
            let positioning_source = positioning_source.clone();
            let ui_weak = ui_weak.clone();

            host::spawn(async move {
                let connected = {
                    let mut state = swarm_state.lock().await;
                    state.remove(&original_index)
                };

                let Some(connected) = connected else {
                    eprintln!("Unit {} is not connected", original_index);
                    return;
                };

                eprintln!("Disconnecting unit {} ...", original_index);
                connected.disconnect().await;

                update_unit(&ui_weak, original_index, |u| {
                    u.state = UnitState::Disconnected;
                    u.pos_x = 0.0;
                    u.pos_y = 0.0;
                    u.pos_z = 0.0;
                    u.battery_voltage = 0.0;
                    u.link_quality = 0.0;
                    u.deck_lighthouse = false;
                    u.deck_loco = false;
                    u.deck_led_top = false;
                    u.deck_led_bottom = false;
                    u.serial = "".into();
                    u.pm_state = "".into();
                    u.journal_entry_count = 0;
                    u.platform_type = "".into();
                    u.firmware_version = "".into();
                });

                // Reset positioning source if this was it
                {
                    let mut ps = positioning_source.lock().await;
                    if *ps == Some(original_index) {
                        *ps = None;
                    }
                }

                // Check if any units are still connected
                let any_connected = {
                    let state = swarm_state.lock().await;
                    !state.is_empty()
                };
                if !any_connected {
                    let ui_weak_inner = ui_weak.clone();
                    slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_weak_inner.upgrade() {
                            ui.set_swarm_connected(false);
                        }
                    }).ok();
                }

                eprintln!("Disconnected unit {}", original_index);
            });
        });
    }
}
