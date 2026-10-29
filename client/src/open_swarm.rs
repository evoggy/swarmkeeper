//! Opening a swarm: a swarm file, from the `swarms` folder or anywhere, or a
//! swarm shared on the server Swarmkeeper is signed in to (`<org>/<swarm>`,
//! see [`crate::shared_swarms`]). Also the account shared swarms need: the
//! account menu and the sign-in dialog.
//!
//! A shared swarm opens as the server's latest revision, or as this
//! computer's copy when the server can't be reached. While it is open,
//! Swarmkeeper checks the server for a newer revision now and then, and the
//! info bar offers to reload it; reloading disconnects the swarm, so it
//! never happens by itself.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use slint::{ComponentHandle, ModelRc, VecModel, Weak};

use crate::host::account::{self, KeyRejected};
use crate::host::radios::SharedRadioPlan;
use crate::shared_swarms::{Gone, Listed, Shared, SharedId};
use crate::{swarm_file::apply_swarm_config, swarm_file::load_swarm_config, swarm_file::uri_with_default_timeout, settings::AppSettings, AppWindow, swarm::ConnectedUnit, loco::PositioningData, loco::SharedPositioningData, SwarmChoice, swarm_file::SwarmConfig, swarm::SwarmState};
use crate::host;

/// How often the open shared swarm is checked for a newer revision.
const WATCH_INTERVAL: Duration = Duration::from_secs(60);

/// Where the open swarm is from.
#[derive(Clone)]
enum Source {
    File(PathBuf),
    Shared { id: SharedId, host: String, revision: i32, offline: bool },
}

#[derive(Clone)]
struct Ctx {
    ui: Weak<AppWindow>,
    swarm_state: SwarmState,
    positioning_data: SharedPositioningData,
    radio_plan: SharedRadioPlan,
    /// The open swarm, when it came from a file or the server.
    current: Arc<std::sync::Mutex<Option<Source>>>,
    /// The sign-in waiting for the browser, and its link.
    sign_in: Arc<std::sync::Mutex<Option<host::JoinHandle<()>>>>,
    sign_in_link: Arc<std::sync::Mutex<String>>,
}

impl Ctx {
    /// Run `f` with the window on the UI thread.
    fn ui(&self, f: impl FnOnce(&AppWindow) + Send + 'static) {
        let ui = self.ui.clone();
        slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui.upgrade() {
                f(&ui);
            }
        })
        .ok();
    }

    fn current(&self) -> Option<Source> {
        self.current.lock().unwrap().clone()
    }

    fn current_shared(&self) -> Option<(SharedId, i32)> {
        match self.current() {
            Some(Source::Shared { id, revision, .. }) => Some((id, revision)),
            _ => None,
        }
    }
}

/// What the info bar says about a swarm file: its path, relative to the data
/// folder when it is in it.
pub fn file_source(path: &Path) -> String {
    let cwd = host::env::data_dir();
    let shown = path.strip_prefix(&cwd).unwrap_or(path);
    shown.display().to_string()
}

fn source_text(source: &Source) -> String {
    match source {
        Source::File(path) => file_source(path),
        Source::Shared { id, host, revision, offline: false } => format!("{id} · {host}, revision {revision}"),
        Source::Shared { id, host, revision, offline: true } => {
            format!("{id} · this computer's copy, revision {revision} ({host} can't be reached)")
        }
    }
}

fn parse_swarm(yaml: &str, id: &SharedId) -> Result<SwarmConfig, String> {
    serde_yaml::from_str(yaml).map_err(|e| format!("Failed to parse swarm '{}': {}", id, e))
}

/// A swarm file's name and number of Crazyflies, for lists.
fn summary(yaml: &str) -> Option<(String, usize)> {
    serde_yaml::from_str::<SwarmConfig>(yaml).ok().map(|config| (config.name, config.units.len()))
}

/// The URIs the radio plan spreads over the Crazyradios, as the unit list
/// has them.
fn radio_uris(config: &SwarmConfig) -> Vec<String> {
    config.units.iter().map(|u| uri_with_default_timeout(&u.uri)).collect()
}

/// Remember the open swarm for the next start.
fn remember(source: &Source) {
    let mut settings: AppSettings = host::store::load();
    match source {
        Source::File(path) => {
            settings.last_swarm_config = Some(path.to_string_lossy().into_owned());
            settings.last_shared_swarm = None;
        }
        Source::Shared { id, .. } => {
            settings.last_shared_swarm = Some(id.to_string());
            settings.last_swarm_config = None;
        }
    }
    if let Err(e) = host::store::save(&settings) {
        eprintln!("Failed to save settings: {}", e);
    }
}

/// Make `config` the open swarm: disconnect the units of the one open now,
/// forget their positions, show the new one and remember it.
async fn switch_swarm(ctx: &Ctx, config: SwarmConfig, source: Source) {
    remember(&source);
    *ctx.current.lock().unwrap() = Some(source.clone());

    // Disconnect all connected units
    let units: Vec<(usize, ConnectedUnit)> = {
        let mut state = ctx.swarm_state.lock().await;
        state.drain().collect()
    };
    for (index, connected) in units {
        eprintln!("Disconnecting unit {} ...", index);
        connected.disconnect().await;
    }

    // Clear positioning data
    {
        let mut pd = ctx.positioning_data.lock().await;
        *pd = PositioningData::default();
    }

    // The new swarm gets its own Crazyradios.
    ctx.radio_plan.lock().await.set_swarm(radio_uris(&config));

    let text = source_text(&source);
    ctx.ui(move |ui| {
        apply_swarm_config(ui, &config);
        ui.set_swarm_source(text.into());
        ui.set_swarm_update("".into());
    });
}

/// The user changed the ID of organizations on the server, (old, new): the
/// open swarm follows.
fn follow_renames(ctx: &Ctx, renamed: &[(String, String)]) {
    let mut current = ctx.current.lock().unwrap();
    let Some(Source::Shared { id, .. }) = current.as_mut() else {
        return;
    };
    let Some((_, new)) = renamed.iter().find(|(old, new)| *old == id.org && old != new) else {
        return;
    };
    id.org = new.clone();
    let source = current.clone().unwrap();
    drop(current);
    remember(&source);
    let text = source_text(&source);
    ctx.ui(move |ui| ui.set_swarm_source(text.into()));
}

/// The swarm files in the `swarms` folder of the data folder: ID (the file
/// name without extension) and path.
fn local_swarms() -> Vec<(String, PathBuf)> {
    let Ok(entries) = host::fs::read_dir("swarms") else {
        return Vec::new();
    };
    let mut swarms: Vec<(String, PathBuf)> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && path.extension().is_some_and(|ext| ext == "yaml" || ext == "yml"))
        .filter_map(|path| Some((path.file_stem()?.to_string_lossy().into_owned(), path)))
        .collect();
    swarms.sort();
    swarms
}

fn absolute(path: &Path) -> PathBuf {
    host::env::data_dir().join(path)
}

fn shared_rows(listed: &[Listed], current: Option<&SharedId>, offline: bool) -> ModelRc<SwarmChoice> {
    let rows: Vec<SwarmChoice> = listed
        .iter()
        .map(|swarm| SwarmChoice {
            id: swarm.id.to_string().into(),
            name: swarm.name.clone().into(),
            units: swarm.units as i32,
            detail: if offline {
                format!("copy, revision {}", swarm.revision)
            } else {
                format!("revision {}", swarm.revision)
            }
            .into(),
            shared: true,
            current: current == Some(&swarm.id),
        })
        .collect();
    ModelRc::new(VecModel::from(rows))
}

/// What to tell the user about an error from the server, and whether it
/// means signing in again.
fn explain(e: &anyhow::Error) -> (String, bool) {
    let rejected = e.downcast_ref::<KeyRejected>().is_some();
    let mut text = format!("{:#}", e);
    if let Some(first) = text.get(..1) {
        text = first.to_uppercase() + &text[1..];
    }
    (text, rejected)
}

// ---- The account ----

/// Show who Swarmkeeper is signed in as: what the credentials say at once,
/// then what the server says.
fn refresh_account(ctx: &Ctx) {
    let credentials = account::load().unwrap_or_else(|e| {
        eprintln!("Failed to read the sign-in: {:#}", e);
        None
    });
    let Some(credentials) = credentials else {
        ctx.ui(|ui| {
            ui.set_account_label("Sign in".into());
            ui.set_account_text("Not signed in. Sign in to open the swarms shared in your organizations.".into());
            ui.set_account_orgs("".into());
            ui.set_account_initials("".into());
            ui.set_account_picture(slint::Image::default());
            ui.set_signed_in(false);
            ui.set_needs_sign_in(true);
            ui.set_os_shared_title("Shared".into());
        });
        return;
    };
    let host = account::host(&credentials.server).to_string();
    {
        let host = host.clone();
        let server = credentials.server.clone();
        ctx.ui(move |ui| {
            ui.set_account_label(host.clone().into());
            ui.set_account_text(format!("Signed in to {}.", host).into());
            ui.set_signed_in(true);
            ui.set_needs_sign_in(false);
            ui.set_os_shared_title(format!("Shared on {}", host).into());
            ui.set_sign_in_server(server.into());
        });
    }
    let ctx = ctx.clone();
    host::spawn(async move {
        match account::whoami(&credentials).await {
            Ok(user) => {
                let org_names = user.orgs.iter().map(|org| org.name.as_str()).collect::<Vec<_>>().join(", ");
                let orgs = if user.orgs.is_empty() {
                    format!(
                        "You aren't in an organization yet: create one at {}, or ask an admin to invite you.",
                        credentials.server
                    )
                } else {
                    let orgs: Vec<String> =
                        user.orgs.iter().map(|org| format!("{} ({}, {})", org.name, org.slug, org.role)).collect();
                    format!("Swarms are shared in: {}.", orgs.join(", "))
                };
                let text = format!(
                    "Signed in to {} as {}.\n\n{}\n\nSwarmkeeper's key is '{}'; you can revoke it at {}/keys.",
                    host, user.name, orgs, credentials.key_name, credentials.server
                );
                let avatar_url = user.avatar_url.clone();
                ctx.ui(move |ui| {
                    ui.set_account_initials(initials(&user.name).into());
                    ui.set_account_label(user.name.into());
                    ui.set_account_text(text.into());
                    ui.set_account_orgs(org_names.into());
                });
                // The picture, when there is one; the initials until then
                if let Some(url) = avatar_url {
                    match account::avatar(&url).await {
                        Ok(pixels) => ctx.ui(move |ui| ui.set_account_picture(slint::Image::from_rgba8(pixels))),
                        Err(e) => eprintln!("No account picture: {:#}", e),
                    }
                }
            }
            Err(e) if e.downcast_ref::<KeyRejected>().is_some() => {
                let text = format!("{}.", explain(&e).0);
                ctx.ui(move |ui| {
                    ui.set_account_label("Sign in again".into());
                    ui.set_account_text(text.into());
                    ui.set_needs_sign_in(true);
                    ui.set_account_orgs("".into());
                    ui.set_account_picture(slint::Image::default());
                });
            }
            Err(e) => {
                eprintln!("Couldn't ask {} who Swarmkeeper is signed in as: {:#}", host, e);
                let text = format!("Signed in to {}, which can't be reached right now.", host);
                ctx.ui(move |ui| ui.set_account_text(text.into()));
            }
        }
    });
}

/// The initials of a name for the avatar: the first letters of its first and
/// last words: "AL" for "Ada Lovelace", "A" for "ada".
fn initials(name: &str) -> String {
    let words: Vec<&str> = name.split_whitespace().collect();
    let first = |word: &&str| word.chars().next();
    let letters = match words.as_slice() {
        [] => Vec::new(),
        [only] => vec![first(only)],
        [head, .., tail] => vec![first(head), first(tail)],
    };
    letters.into_iter().flatten().flat_map(char::to_uppercase).collect()
}

/// The server no longer takes the key: show it in the account menu.
fn key_rejected(ctx: &Ctx, text: String) {
    ctx.ui(move |ui| {
        ui.set_account_label("Sign in again".into());
        ui.set_account_text(text.into());
        ui.set_needs_sign_in(true);
        ui.set_account_orgs("".into());
    });
}

fn show_error(ctx: &Ctx, title: &str, message: String) {
    let title = title.to_string();
    ctx.ui(move |ui| {
        ui.set_error_dialog_title(title.into());
        ui.set_error_dialog_message(message.into());
        ui.set_error_dialog_visible(true);
    });
}

fn sign_in(ctx: &Ctx, server: String) {
    if let Some(task) = ctx.sign_in.lock().unwrap().take() {
        task.abort();
    }
    ctx.sign_in_link.lock().unwrap().clear();
    ctx.ui(|ui| {
        ui.set_sign_in_error("".into());
        ui.set_sign_in_link("".into());
        ui.set_sign_in_waiting(true);
    });
    let ctx2 = ctx.clone();
    let task = host::spawn(async move {
        let ctx = ctx2;
        let sign_in = match account::start(&server).await {
            Ok(sign_in) => sign_in,
            Err(e) => {
                let text = explain(&e).0;
                ctx.ui(move |ui| {
                    ui.set_sign_in_waiting(false);
                    ui.set_sign_in_error(text.into());
                });
                return;
            }
        };
        let link = sign_in.url.to_string();
        *ctx.sign_in_link.lock().unwrap() = link.clone();
        if let Err(e) = host::env::open_url(&link) {
            eprintln!("Couldn't open the browser: {}", e);
        }
        ctx.ui(move |ui| ui.set_sign_in_link(link.into()));

        match sign_in.finish().await {
            Ok(signed_in) => {
                ctx.ui(|ui| {
                    ui.set_sign_in_waiting(false);
                    ui.set_sign_in_dialog_visible(false);
                });
                if let Some(old) = signed_in.old_key_kept {
                    show_error(
                        &ctx,
                        "Signed In",
                        format!(
                            "Signed in, but the key of the previous sign-in, '{}', couldn't be revoked. Revoke it at {}/keys.",
                            old.key_name, old.server
                        ),
                    );
                }
                refresh_account(&ctx);
                refresh_shared(&ctx);
            }
            Err(e) => {
                let text = explain(&e).0;
                ctx.ui(move |ui| {
                    ui.set_sign_in_waiting(false);
                    ui.set_sign_in_error(text.into());
                });
            }
        }
    });
    *ctx.sign_in.lock().unwrap() = Some(task);
}

// ---- The open swarm dialog ----

/// Fill the dialog: the swarm files at once, and the shared swarms from the
/// copies until the server answers.
fn fill_open_dialog(ctx: &Ctx, ui: &AppWindow) {
    let current = ctx.current();
    let current_file = match &current {
        Some(Source::File(path)) => Some(absolute(path)),
        _ => None,
    };
    let local: Vec<SwarmChoice> = local_swarms()
        .into_iter()
        .map(|(id, path)| {
            let (name, units) = host::fs::read_to_string(&path)
                .ok()
                .and_then(|yaml| summary(&yaml))
                .unwrap_or_else(|| ("(can't read the file)".to_string(), 0));
            SwarmChoice {
                id: id.into(),
                name: name.into(),
                units: units as i32,
                detail: "".into(),
                shared: false,
                current: current_file.as_deref() == Some(absolute(&path).as_path()),
            }
        })
        .collect();
    ui.set_os_local(ModelRc::new(VecModel::from(local)));
    refresh_shared(ctx);
}

/// List the shared swarms in the dialog: the copies at once, then the
/// server's list.
fn refresh_shared(ctx: &Ctx) {
    let shared = match Shared::open() {
        Ok(Some(shared)) => shared,
        Ok(None) => {
            ctx.ui(|ui| {
                ui.set_os_shared(ModelRc::new(VecModel::from(Vec::<SwarmChoice>::new())));
                ui.set_os_shared_note("".into());
            });
            return;
        }
        Err(e) => {
            let text = explain(&e).0;
            ctx.ui(move |ui| ui.set_os_shared_note(text.into()));
            return;
        }
    };
    let current = ctx.current_shared().map(|(id, _)| id);
    let copies = shared.listed_copies(summary);
    let note = format!("Checking {}...", shared.host());
    {
        let current = current.clone();
        ctx.ui(move |ui| {
            ui.set_os_shared(shared_rows(&copies, current.as_ref(), true));
            ui.set_os_shared_note(note.into());
        });
    }
    let ctx = ctx.clone();
    host::spawn(async move {
        match shared.list(summary).await {
            Ok(listing) => {
                follow_renames(&ctx, &listing.renamed);
                let current = ctx.current_shared().map(|(id, _)| id);
                let note = if listing.offline {
                    format!("Can't reach {}: these are the copies on this computer", shared.host())
                } else {
                    String::new()
                };
                ctx.ui(move |ui| {
                    ui.set_os_shared(shared_rows(&listing.swarms, current.as_ref(), listing.offline));
                    ui.set_os_shared_note(note.into());
                });
            }
            Err(e) => {
                let (text, rejected) = explain(&e);
                if rejected {
                    key_rejected(&ctx, format!("{}.", text));
                }
                ctx.ui(move |ui| ui.set_os_shared_note(text.into()));
            }
        }
    });
}

/// Open a shared swarm: the server's latest revision, or the copy. Returns
/// what to tell the user when it can't be opened.
async fn open_shared(ctx: &Ctx, id: &SharedId) -> Result<(), String> {
    let shared = match Shared::open() {
        Ok(Some(shared)) => shared,
        Ok(None) => return Err(format!("'{}' is a shared swarm: sign in to open it.", id)),
        Err(e) => return Err(explain(&e).0),
    };
    match shared.load(id).await {
        Ok(opened) => {
            let config = parse_swarm(&opened.yaml, &opened.id)?;
            let source = Source::Shared {
                id: opened.id,
                host: shared.host().to_string(),
                revision: opened.revision,
                offline: opened.offline,
            };
            switch_swarm(ctx, config, source).await;
            Ok(())
        }
        Err(e) => {
            let (text, rejected) = explain(&e);
            if rejected {
                key_rejected(ctx, format!("{}.", text));
            }
            Err(format!("{}.", text))
        }
    }
}

fn open_chosen(ctx: &Ctx, id: String, is_shared: bool) {
    ctx.ui(|ui| {
        ui.set_os_busy(true);
        ui.set_os_error("".into());
    });
    let ctx = ctx.clone();
    host::spawn(async move {
        let result = if is_shared {
            match SharedId::parse(&id) {
                Some(id) => open_shared(&ctx, &id).await,
                None => Err(format!("'{}' isn't a shared swarm ID", id)),
            }
        } else {
            match local_swarms().into_iter().find(|(local, _)| *local == id) {
                Some((_, path)) => match load_swarm_config(&path) {
                    Ok(config) => {
                        switch_swarm(&ctx, config, Source::File(absolute(&path))).await;
                        Ok(())
                    }
                    Err(e) => Err(e),
                },
                None => Err(format!("The swarm file '{}' is gone from the swarms folder.", id)),
            }
        };
        // A swarm gone from the server goes from the list too.
        let refresh = is_shared && result.is_err();
        ctx.ui(move |ui| {
            ui.set_os_busy(false);
            match result {
                Ok(()) => ui.set_os_dialog_visible(false),
                Err(text) => ui.set_os_error(text.into()),
            }
        });
        if refresh {
            refresh_shared(&ctx);
        }
    });
}

// ---- Startup and the watcher ----

/// Open the shared swarm open last time: its copy at once, so the swarm shows
/// without waiting for the server, then the server's latest revision.
fn restore_shared(ctx: &Ctx, ui: &AppWindow, id: &str) {
    let Some(id) = SharedId::parse(id) else {
        return;
    };
    let shared = match Shared::open() {
        Ok(Some(shared)) => shared,
        Ok(None) => {
            eprintln!("The swarm open last, '{}', is a shared swarm: sign in to open it", id);
            return;
        }
        Err(e) => {
            eprintln!("{:#}", e);
            return;
        }
    };
    let id = shared.current_id(&id);
    let mut shown = None;
    let mut uris = None;
    if let Ok((yaml, revision)) = shared.cached(&id) {
        match parse_swarm(&yaml, &id) {
            Ok(config) => {
                uris = Some(radio_uris(&config));
                apply_swarm_config(ui, &config);
                let source = Source::Shared { id: id.clone(), host: shared.host().to_string(), revision, offline: false };
                ui.set_swarm_source(source_text(&source).into());
                *ctx.current.lock().unwrap() = Some(source);
                shown = Some(revision);
            }
            Err(e) => eprintln!("{}", e),
        }
    }
    let ctx = ctx.clone();
    host::spawn(async move {
        // main planned the Crazyradios for the swarm shown before this one.
        if let Some(uris) = uris {
            ctx.radio_plan.lock().await.set_swarm(uris);
        }
        let result = shared.load(&id).await;
        let connected = !ctx.swarm_state.lock().await.is_empty();
        // The user may have opened another swarm in the meantime.
        let still_open = match ctx.current() {
            None => shown.is_none(),
            Some(Source::Shared { id: open, .. }) => open == id,
            Some(Source::File(_)) => false,
        };
        if !still_open {
            return;
        }
        match result {
            // Keep the copy when it is the latest, or the server can't say,
            // or the user already connected to it: offer the new one then.
            Ok(opened) if shown.is_some() && (opened.offline || shown == Some(opened.revision) || connected) => {
                follow_renames(&ctx, &[(id.org.clone(), opened.id.org.clone())]);
                set_offline(&ctx, opened.offline);
                if !opened.offline && shown != Some(opened.revision) {
                    offer_update(&ctx, opened.revision, shared.host());
                }
            }
            Ok(opened) => match parse_swarm(&opened.yaml, &opened.id) {
                Ok(config) => {
                    let source = Source::Shared {
                        id: opened.id,
                        host: shared.host().to_string(),
                        revision: opened.revision,
                        offline: opened.offline,
                    };
                    switch_swarm(&ctx, config, source).await;
                }
                Err(e) => show_error(&ctx, "Open Swarm", e),
            },
            Err(e) => {
                let (text, rejected) = explain(&e);
                eprintln!("{}", text);
                if rejected {
                    key_rejected(&ctx, format!("{}.", text));
                } else if e.downcast_ref::<Gone>().is_some() {
                    forget_gone(&ctx, &text);
                } else if shown.is_none() {
                    let text = format!("{} · {}", id, text);
                    ctx.ui(move |ui| ui.set_swarm_source(text.into()));
                }
            }
        }
    });
}

/// Whether the server could be reached when the open shared swarm was last
/// checked.
fn set_offline(ctx: &Ctx, now: bool) {
    let mut current = ctx.current.lock().unwrap();
    let Some(Source::Shared { offline, .. }) = current.as_mut() else {
        return;
    };
    if *offline == now {
        return;
    }
    *offline = now;
    let text = source_text(current.as_ref().unwrap());
    drop(current);
    ctx.ui(move |ui| ui.set_swarm_source(text.into()));
}

fn offer_update(ctx: &Ctx, revision: i32, host: &str) {
    let text = format!("Revision {} on {}", revision, host);
    ctx.ui(move |ui| ui.set_swarm_update(text.into()));
}

/// The open shared swarm is gone from the server: say so, and don't open it
/// at the next start.
fn forget_gone(ctx: &Ctx, text: &str) {
    *ctx.current.lock().unwrap() = None;
    let mut settings: AppSettings = host::store::load();
    settings.last_shared_swarm = None;
    if let Err(e) = host::store::save(&settings) {
        eprintln!("Failed to save settings: {}", e);
    }
    let text = text.to_string();
    ctx.ui(move |ui| {
        ui.set_swarm_source(text.into());
        ui.set_swarm_update("".into());
    });
}

/// Check the open shared swarm for a newer revision now and then. Only
/// downloads it (into the copy); the user reloads.
async fn watch(ctx: Ctx) {
    let mut interval = host::interval(WATCH_INTERVAL);
    interval.tick().await;
    loop {
        interval.tick().await;
        let Some((id, revision)) = ctx.current_shared() else {
            continue;
        };
        let Ok(Some(shared)) = Shared::open() else {
            continue;
        };
        let result = shared.load(&id).await;
        // The user may have opened another swarm in the meantime.
        if ctx.current_shared().map(|(open, _)| open) != Some(id.clone()) {
            continue;
        }
        match result {
            Ok(opened) => {
                follow_renames(&ctx, &[(id.org.clone(), opened.id.org.clone())]);
                set_offline(&ctx, opened.offline);
                if !opened.offline && opened.revision != revision {
                    offer_update(&ctx, opened.revision, shared.host());
                }
            }
            Err(e) => {
                let (text, rejected) = explain(&e);
                if rejected {
                    key_rejected(&ctx, format!("{}.", text));
                } else if e.downcast_ref::<Gone>().is_some() {
                    forget_gone(&ctx, &text);
                } else {
                    eprintln!("Checking '{}' for a newer revision: {}", id, text);
                }
            }
        }
    }
}

// ---- Wiring ----

pub fn setup(
    ui: &AppWindow,
    swarm_state: SwarmState,
    positioning_data: SharedPositioningData,
    radio_plan: SharedRadioPlan,
    settings: &AppSettings,
) {
    let ctx = Ctx {
        ui: ui.as_weak(),
        swarm_state,
        positioning_data,
        radio_plan,
        current: Arc::new(std::sync::Mutex::new(None)),
        sign_in: Arc::new(std::sync::Mutex::new(None)),
        sign_in_link: Arc::new(std::sync::Mutex::new(String::new())),
    };

    ui.set_sign_in_server(account::DEFAULT_SERVER.into());
    refresh_account(&ctx);

    // The swarm open last: main shows a file itself.
    match (&settings.last_swarm_config, &settings.last_shared_swarm) {
        (Some(path), _) if !ui.get_swarm_source().is_empty() => {
            *ctx.current.lock().unwrap() = Some(Source::File(PathBuf::from(path)));
        }
        (None, Some(id)) => restore_shared(&ctx, ui, id),
        _ => {}
    }

    {
        let ctx = ctx.clone();
        ui.on_os_show(move || {
            if let Some(ui) = ctx.ui.upgrade() {
                fill_open_dialog(&ctx, &ui);
            }
        });
    }
    {
        let ctx = ctx.clone();
        ui.on_os_open(move |id, shared| open_chosen(&ctx, id.to_string(), shared));
    }

    // Browse for a swarm file
    {
        let ctx = ctx.clone();
        ui.on_open_swarm_requested(move || {
            let ctx = ctx.clone();
            host::spawn(async move {
                let Some(handle) = host::dialog::FileDialog::new()
                    .add_filter("YAML", &["yaml", "yml"])
                    .pick_file()
                    .await
                else {
                    return;
                };
                let path = handle.path().to_path_buf();
                match load_swarm_config(&path) {
                    Ok(config) => switch_swarm(&ctx, config, Source::File(path)).await,
                    Err(e) => show_error(&ctx, "Open Swarm", e),
                }
            });
        });
    }

    // Reload the open shared swarm
    {
        let ctx = ctx.clone();
        ui.on_reload_swarm(move || {
            let Some((id, _)) = ctx.current_shared() else {
                return;
            };
            let ctx = ctx.clone();
            host::spawn(async move {
                if let Err(text) = open_shared(&ctx, &id).await {
                    show_error(&ctx, "Reload Swarm", text);
                }
            });
        });
    }

    // The account
    {
        let ctx = ctx.clone();
        ui.on_sign_in(move |server| sign_in(&ctx, server.to_string()));
    }
    {
        let ctx = ctx.clone();
        ui.on_sign_in_open_link(move || {
            let link = ctx.sign_in_link.lock().unwrap().clone();
            if !link.is_empty() {
                if let Err(e) = host::env::open_url(&link) {
                    eprintln!("Couldn't open the browser: {}", e);
                }
            }
        });
    }
    {
        let ctx = ctx.clone();
        ui.on_sign_in_cancel(move || {
            if let Some(task) = ctx.sign_in.lock().unwrap().take() {
                task.abort();
            }
            ctx.ui(|ui| ui.set_sign_in_waiting(false));
        });
    }
    {
        let ctx = ctx.clone();
        ui.on_sign_out(move || {
            let ctx = ctx.clone();
            host::spawn(async move {
                let result = account::sign_out().await;
                refresh_account(&ctx);
                refresh_shared(&ctx);
                if let Err(e) = result {
                    show_error(&ctx, "Sign Out", explain(&e).0);
                }
            });
        });
    }

    host::spawn(watch(ctx));
}

#[cfg(test)]
mod tests {
    use super::initials;

    #[test]
    fn initials_of_first_and_last_word() {
        assert_eq!(initials("Ada Lovelace"), "AL");
        assert_eq!(initials("Ada King Lovelace"), "AL");
        assert_eq!(initials("ada"), "A");
        assert_eq!(initials("  "), "");
    }
}
