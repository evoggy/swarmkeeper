//! Flash firmware: the dialog (ui/flash.slint) and the job.
//!
//! What to flash is a release from GitHub (for the units' platform), a
//! release .zip or a binary for one target. The STM32 and nRF51 go to the
//! units as one job on the connection server: by broadcast where the
//! bootloaders take it, each unit then checked by CRC, the others one at a
//! time. The decks come after, one unit per Crazyradio at a time with the
//! Crazyradios side by side, so a swarm spread over several channels has
//! several decks flashed at once (as cfcli does).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::app::AppContext;
use crate::firmware::{self, Part, Release, ReleaseZip};
use crate::host::radios::{link_uri, RadioUri};
use crate::host::{self, bootloader_flash};
use crate::swarm::{ConnectedUnit, SwarmState};
use crate::units::update_unit;
use crate::{AppWindow, FlashBridge, FlashCard, FlashCell, FlashPart, FlashPhase, FlashState, FlashStep, FlashUnit, UnitData, UnitState};

/// How long the Crazyflies get to start their new firmware before their
/// decks are flashed. The nRF51 delays the start by 5 s when an AI deck is
/// attached (cfcli's `RESTART_DELAY`).
const RESTART_DELAY: Duration = Duration::from_secs(7);

/// A unit, connected or not, by name.
fn connected(u: &UnitData) -> bool {
    !matches!(u.state, UnitState::Disconnected | UnitState::Error)
}

/// The progress of one unit for one part.
#[derive(Clone, Debug)]
struct Cell {
    state: FlashState,
    text: String,
    progress: f32,
    bar: bool,
}

impl Cell {
    fn new(state: FlashState, text: impl Into<String>) -> Cell {
        Cell { state, text: text.into(), progress: 0.0, bar: false }
    }

    fn busy(text: impl Into<String>, progress: f32) -> Cell {
        Cell { state: FlashState::Busy, text: text.into(), progress, bar: true }
    }
}

/// A unit being flashed.
struct RunUnit {
    name: String,
    /// Its URI as the connection server knows it
    key: String,
    cells: Vec<Cell>,
}

/// How far the STM32 and nRF51 job is.
#[derive(Default)]
struct FirmwareJob {
    /// Units (run index) in their bootloaders
    in_bootloader: HashSet<usize>,
    armed: Vec<usize>,
    /// The image broadcast now, and how far: (`stm32`, done, total)
    broadcast: Option<(String, u64, u64)>,
    broadcast_done: Vec<String>,
    checking: Option<usize>,
    checked: HashSet<usize>,
    /// Units flashed one at a time, in order, and the one now: (unit, image, fraction)
    unicast: Vec<usize>,
    unicast_now: Option<(usize, String, f32)>,
}

/// A flash, running or done.
struct Run {
    parts: Vec<Part>,
    units: Vec<RunUnit>,
    /// The part being flashed (the STM32 and nRF51 are one step)
    step: usize,
    steps: Vec<FlashState>,
    firmware: FirmwareJob,
    /// A deck part: (done, of, units being flashed: (run index, radio, fraction))
    deck: Option<(usize, usize, Vec<(usize, usize, f32)>)>,
    finished: bool,
    status: String,
}

impl Run {
    fn unit_by_key(&self, key: &str) -> Option<usize> {
        self.units.iter().position(|u| u.key == key)
    }

    /// The columns of the firmware parts (STM32, nRF51).
    fn firmware_columns(&self) -> Vec<usize> {
        self.parts.iter().enumerate().filter(|(_, p)| p.is_firmware()).map(|(i, _)| i).collect()
    }

    fn set_firmware_cells(&mut self, unit: usize, cell: Cell) {
        for column in self.firmware_columns() {
            self.units[unit].cells[column] = cell.clone();
        }
    }

    fn column_of_image(&self, image: &str) -> Option<usize> {
        self.parts.iter().position(|p| p.server_target() == Some(image))
    }

    /// Show a progress report of the firmware job.
    fn firmware_progress(&mut self, p: crazyflie_connection_server::FlashProgress) {
        let fraction = if p.total > 0 { p.done as f32 / p.total as f32 } else { 0.0 };
        let Some(uri) = p.uri.as_deref() else {
            // A broadcast: "Broadcasting stm32 to 45 Crazyflies"
            let image = p.step.split_whitespace().nth(1).unwrap_or_default().to_string();
            if p.total > 0 && p.done >= p.total && !self.firmware.broadcast_done.contains(&image) {
                self.firmware.broadcast_done.push(image.clone());
            }
            self.firmware.broadcast = Some((image.clone(), p.done, p.total));
            if let Some(column) = self.column_of_image(&image) {
                let done = p.total > 0 && p.done >= p.total;
                for unit in self.firmware.armed.clone() {
                    self.units[unit].cells[column] = if done {
                        Cell::new(FlashState::Wait, "Broadcast, to check")
                    } else {
                        Cell::busy(format!("Broadcast {} %", (fraction * 100.0) as u32), fraction)
                    };
                }
            }
            return;
        };
        let Some(unit) = self.unit_by_key(uri) else { return };
        match p.step.as_str() {
            "Restarting into the bootloader" => self.set_firmware_cells(unit, Cell::new(FlashState::Busy, "Into the bootloader")),
            "Reading the bootloader" => {
                self.firmware.in_bootloader.insert(unit);
                self.set_firmware_cells(unit, Cell::new(FlashState::Wait, "In the bootloader"));
            }
            "Arming for broadcast" => {
                self.firmware.in_bootloader.insert(unit);
                if !self.firmware.armed.contains(&unit) {
                    self.firmware.armed.push(unit);
                }
                self.set_firmware_cells(unit, Cell::new(FlashState::Wait, "Armed for broadcast"));
            }
            "Checking" => {
                // The checks after a broadcast run one unit after another
                if let Some(previous) = self.firmware.checking.filter(|previous| *previous != unit) {
                    self.firmware.checked.insert(previous);
                    if !self.firmware.unicast.contains(&previous) {
                        self.set_firmware_cells(previous, Cell::new(FlashState::Done, "Checked, restarting"));
                    }
                }
                if !self.firmware.unicast.contains(&unit) {
                    self.firmware.checking = Some(unit);
                }
                self.set_firmware_cells(unit, Cell::new(FlashState::Busy, "Checking…"));
            }
            step if step.starts_with("Flashing ") => {
                let image = step.trim_start_matches("Flashing ").to_string();
                if !self.firmware.unicast.contains(&unit) {
                    self.firmware.unicast.push(unit);
                }
                self.firmware.unicast_now = Some((unit, image.clone(), fraction));
                if let Some(column) = self.column_of_image(&image) {
                    self.units[unit].cells[column] = Cell::busy(format!("One at a time {} %", (fraction * 100.0) as u32), fraction);
                }
            }
            "Checking that it left the bootloader" => self.set_firmware_cells(unit, Cell::new(FlashState::Busy, "Restarting")),
            other => self.set_firmware_cells(unit, Cell::new(FlashState::Busy, other)),
        }
    }

    /// The phases of the step running now.
    fn phases(&self) -> Vec<FlashPhase> {
        let Some(part) = self.parts.get(self.step_part()) else { return Vec::new() };
        let ratio = |n: usize, of: usize| if of == 0 { 0.0 } else { n as f32 / of as f32 };
        if part.is_firmware() {
            let job = &self.firmware;
            let total = self.units.len();
            let mut phases = vec![FlashPhase {
                name: "Into the bootloader".into(),
                text: format!("{} of {}", job.in_bootloader.len(), total).into(),
                done: ratio(job.in_bootloader.len(), total),
                busy: 0.0,
                fail: 0.0,
            }];
            if !job.armed.is_empty() {
                let images: Vec<&str> = self.parts.iter().filter_map(Part::server_target).collect();
                let now = job.broadcast.as_ref().filter(|(image, _, _)| !job.broadcast_done.contains(image));
                let done = job.broadcast_done.len() as f32;
                let busy = now.map_or(0.0, |(_, d, t)| ratio(*d as usize, *t as usize));
                phases.push(FlashPhase {
                    name: "Broadcast".into(),
                    text: match now {
                        Some((image, d, t)) => format!("{} to {} units · {} %", image, job.armed.len(), d * 100 / (*t).max(1)),
                        None if job.broadcast_done.len() >= images.len() => format!("{} units at once · done", job.armed.len()),
                        None => format!("{} units armed", job.armed.len()),
                    }
                    .into(),
                    done: done / images.len().max(1) as f32,
                    busy: busy / images.len().max(1) as f32,
                    fail: 0.0,
                });
                let checked = job.checked.len();
                phases.push(FlashPhase {
                    name: "Checking".into(),
                    text: format!("{} of {}", checked, job.armed.len()).into(),
                    done: ratio(checked, job.armed.len()),
                    busy: if job.checking.is_some() { ratio(1, job.armed.len()) / 2.0 } else { 0.0 },
                    fail: 0.0,
                });
            }
            if !job.unicast.is_empty() {
                let done = job.unicast.len() - job.unicast_now.iter().count();
                let (text, busy) = match &job.unicast_now {
                    Some((unit, image, f)) => (format!("{} · {} {} %", self.units[*unit].name, image, (f * 100.0) as u32), *f),
                    None => (format!("{} done", job.unicast.len()), 0.0),
                };
                phases.push(FlashPhase {
                    name: "One at a time".into(),
                    text: text.into(),
                    done: ratio(done, job.unicast.len()),
                    busy: ratio(1, job.unicast.len()) * busy,
                    fail: 0.0,
                });
            }
            phases
        } else if let Some((done, of, busy)) = &self.deck {
            vec![FlashPhase {
                name: part.name().into(),
                text: format!("{} of {} · {}", done, of, if busy.len() > 1 { format!("{} at a time, one per Crazyradio", busy.len()) } else { "one at a time".into() }).into(),
                done: ratio(*done, *of),
                busy: busy.iter().map(|(_, _, f)| f).sum::<f32>() / (*of).max(1) as f32,
                fail: 0.0,
            }]
        } else {
            Vec::new()
        }
    }

    /// The units flashed on their own now.
    fn cards(&self) -> Vec<FlashCard> {
        if self.parts.get(self.step_part()).is_some_and(Part::is_firmware) {
            self.firmware
                .unicast_now
                .iter()
                .map(|(unit, image, f)| FlashCard {
                    name: self.units[*unit].name.as_str().into(),
                    sub: format!("one at a time · {}", image).into(),
                    text: format!("{} %", (f * 100.0) as u32).into(),
                    progress: *f,
                    waiting: false,
                })
                .collect()
        } else {
            self.deck
                .iter()
                .flat_map(|(_, _, busy)| busy.iter())
                .map(|(unit, radio, f)| FlashCard {
                    name: self.units[*unit].name.as_str().into(),
                    sub: format!("Crazyradio {}", radio).into(),
                    text: format!("{} %", (f * 100.0) as u32).into(),
                    progress: *f,
                    waiting: false,
                })
                .collect()
        }
    }

    /// The first part of the step running now.
    fn step_part(&self) -> usize {
        let firmware = self.parts.iter().filter(|p| p.is_firmware()).count();
        match self.step {
            0 if firmware > 0 => 0,
            step if firmware > 0 => firmware + step - 1,
            step => step,
        }
    }

    /// The steps: the STM32 and nRF51 as one, then each deck part.
    fn step_names(&self) -> Vec<String> {
        let firmware: Vec<String> = self.parts.iter().filter(|p| p.is_firmware()).map(Part::name).collect();
        let mut names = Vec::new();
        if !firmware.is_empty() {
            names.push(firmware.join(" and "));
        }
        names.extend(self.parts.iter().filter(|p| !p.is_firmware()).map(Part::name));
        names
    }
}

/// The dialog.
#[derive(Default)]
struct Dialog {
    releases: Vec<Release>,
    releases_problem: Option<String>,
    /// The units' platform (`cf2`), or why there is none
    platform: Option<String>,
    platform_problem: Option<String>,
    release: Option<String>,
    release_zip: Option<ReleaseZip>,
    /// Bytes so far and of, while downloading
    download: Option<(u64, u64)>,
    download_problem: Option<String>,
    /// Bumped on every download, so that an older one that finishes late is ignored
    download_seq: u64,
    zip: Option<(String, ReleaseZip)>,
    zip_problem: Option<String>,
    binary: Option<(String, Vec<u8>)>,
    bin_target: Option<String>,
    /// Which decks each connected unit has (units model index → deck → has it),
    /// from its `deck.<name>` parameters
    decks: HashMap<usize, HashMap<String, bool>>,
    /// Parts left out, by key; units left out, by name
    unchecked: HashSet<String>,
    unticked: HashSet<String>,
    run: Option<Run>,
    /// Something changed: the redraw timer shows it
    dirty: bool,
}

type Shared = Arc<Mutex<Dialog>>;

impl Dialog {
    fn source_parts(&self, source: i32) -> Vec<Part> {
        match source {
            0 => self.release_zip.as_ref().map(|z| z.parts.clone()).unwrap_or_default(),
            1 => self.zip.as_ref().map(|(_, z)| z.parts.clone()).unwrap_or_default(),
            _ => match (&self.binary, &self.bin_target) {
                (Some((file, data)), Some(target)) => vec![Part::from_binary(file, target, data.clone())],
                _ => Vec::new(),
            },
        }
    }

    /// The parts offered: not the decks that none of the connected units
    /// has (all of them when none is connected).
    fn offered_parts(&self, source: i32) -> Vec<Part> {
        self.source_parts(source).into_iter().filter(|p| !self.no_unit_has(p)).collect()
    }

    /// The parts to flash: the ticked ones.
    fn chosen_parts(&self, source: i32) -> Vec<Part> {
        self.offered_parts(source).into_iter().filter(|p| !self.unchecked.contains(&p.key())).collect()
    }

    /// Whether the connected units were asked and none has the part's deck.
    fn no_unit_has(&self, part: &Part) -> bool {
        match deck_of(part) {
            Some(deck) => !self.decks.is_empty() && self.decks.values().all(|decks| decks.get(deck) == Some(&false)),
            None => false,
        }
    }
}

/// The firmware most of the connected units run.
fn usual_firmware(units: &[UnitData]) -> String {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for u in units.iter().filter(|u| connected(u) && !u.firmware_version.is_empty()) {
        *counts.entry(u.firmware_version.as_str()).or_default() += 1;
    }
    counts.into_iter().max_by_key(|(_, n)| *n).map(|(fw, _)| fw.to_string()).unwrap_or_default()
}

/// The decks a unit is asked about, by their `deck.<name>` parameter.
const DECKS: &[&str] = &["bcLighthouse4", "bcAI", "bcCam", "bcColorLedTop", "bcColorLedBot"];

/// The deck a part goes to (`bcAI` for `bcAI:esp`), for a deck part.
fn deck_of(part: &Part) -> Option<&str> {
    part.target.starts_with("bc").then(|| part.target.split(':').next().unwrap_or(&part.target))
}

impl Dialog {
    /// Whether the unit at `index` lacks the deck of a part, when that is
    /// known: it is connected and said so.
    fn lacks_deck(&self, index: usize, u: &UnitData, part: &Part) -> bool {
        let Some(deck) = deck_of(part) else { return false };
        match self.decks.get(&index).and_then(|decks| decks.get(deck)) {
            Some(has) => !has,
            None => {
                connected(u)
                    && match deck {
                        "bcLighthouse4" => !u.deck_lighthouse,
                        "bcColorLedTop" => !u.deck_led_top,
                        "bcColorLedBot" => !u.deck_led_bottom,
                        _ => false,
                    }
            }
        }
    }

    /// A release or zip came: every part it offers is ticked.
    fn tick_parts(&mut self) {
        self.unchecked.clear();
    }
}

fn size_text(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{} KB", bytes.div_ceil(1024))
    }
}

fn model<T: Clone + 'static>(items: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(items))
}

/// Show the dialog's state.
fn refresh(ui: &AppWindow, state: &Shared) {
    let b = ui.global::<FlashBridge>();
    let mut st = state.lock().unwrap();
    st.dirty = false;
    let source = b.get_source();
    let units: Vec<UnitData> = ui.get_units().iter().collect();
    let usual = usual_firmware(&units);

    // The source
    let labels: Vec<SharedString> = st.releases.iter().map(|r| r.label().into()).collect();
    let dates: Vec<SharedString> = st.releases.iter().map(|r| r.date().into()).collect();
    let index = st.release.as_ref().and_then(|tag| st.releases.iter().position(|r| &r.tag_name == tag)).map_or(-1, |i| i as i32);
    b.set_releases(model(labels));
    b.set_release_dates(model(dates));
    b.set_release_index(index);
    b.set_releases_status(st.releases_problem.clone().unwrap_or_else(|| "Looking on GitHub…".into()).into());
    let asset = match (&st.platform, &st.release) {
        (Some(platform), Some(release)) => firmware::release_asset_name(platform, release),
        _ => String::new(),
    };
    b.set_asset(asset.into());
    b.set_downloading(st.download.is_some());
    match st.download {
        Some((done, of)) => {
            b.set_download_progress(if of > 0 { done as f32 / of as f32 } else { 0.0 });
            b.set_download_text(format!("{} % · {:.1} of {:.1} MB", done * 100 / of.max(1), done as f64 / 1e6, of as f64 / 1e6).into());
        }
        None => b.set_download_text(if st.release_zip.is_some() { "· downloaded".into() } else { SharedString::new() }),
    }
    match &st.zip {
        Some((name, zip)) => {
            b.set_zip_name(name.as_str().into());
            b.set_zip_sub(format!("Release {} for {} · {} parts", zip.release, zip.platform, zip.parts.len()).into());
        }
        None => {
            b.set_zip_name("".into());
            b.set_zip_sub(st.zip_problem.clone().unwrap_or_else(|| "A release .zip from this computer".into()).into());
        }
    }
    match &st.binary {
        Some((name, data)) => {
            b.set_bin_name(name.as_str().into());
            b.set_bin_sub(size_text(data.len()).into());
        }
        None => {
            b.set_bin_name("".into());
            b.set_bin_sub("A .bin for one target".into());
        }
    }

    // The parts, or the binary's targets
    let parts = st.offered_parts(source);
    let part_rows: Vec<FlashPart> = parts
        .iter()
        .map(|p| FlashPart {
            key: p.key().into(),
            name: p.name().into(),
            file: p.file.as_str().into(),
            size: size_text(p.data.len()).into(),
            checked: !st.unchecked.contains(&p.key()),
            group: false,
        })
        .collect();
    b.set_parts(model(part_rows));
    let mut targets = Vec::new();
    for (i, key) in bootloader_flash::target_list().iter().enumerate() {
        if i == 2 {
            targets.push(FlashPart { name: "DECKS".into(), group: true, ..Default::default() });
        }
        targets.push(FlashPart {
            key: (*key).into(),
            name: firmware::target_name(key).into(),
            file: (*key).into(),
            checked: st.bin_target.as_deref() == Some(*key),
            ..Default::default()
        });
    }
    b.set_bin_targets(model(targets));
    let parts_status = match source {
        0 => st
            .platform_problem
            .clone()
            .or_else(|| st.download_problem.clone())
            .or_else(|| st.download.map(|_| "The parts show once the release is downloaded.".to_string()))
            .unwrap_or_default(),
        1 => match &st.zip {
            Some((_, zip)) if st.platform.as_ref().is_some_and(|p| *p != zip.platform) => {
                format!("This zip is for {}, the units are {}.", zip.platform, st.platform.clone().unwrap_or_default())
            }
            Some(_) => String::new(),
            None => st.zip_problem.clone().unwrap_or_else(|| "Choose a release .zip.".into()),
        },
        _ => String::new(),
    };
    b.set_parts_status(parts_status.into());

    // The units
    let chosen = st.chosen_parts(source);
    let columns: Vec<SharedString> = chosen.iter().map(|p| p.name().into()).collect();
    let pick = b.get_pick();
    let in_pick = |u: &UnitData| match pick {
        0 => connected(u),
        1 => true,
        _ => connected(u) && !u.firmware_version.is_empty() && u.firmware_version != usual,
    };
    let running = st.run.as_ref().is_some_and(|r| !r.finished);
    let counts = [
        units.iter().filter(|u| connected(u)).count() as i32,
        units.len() as i32,
        units.iter().filter(|u| connected(u) && !u.firmware_version.is_empty() && u.firmware_version != usual).count() as i32,
    ];
    let mut ticked = 0;
    let rows: Vec<FlashUnit> = units
        .iter()
        .enumerate()
        .map(|(index, u)| {
            let picked = in_pick(u);
            let is_ticked = picked && !st.unticked.contains(u.name.as_str());
            let in_run = st.run.as_ref().and_then(|r| r.units.iter().find(|ru| ru.name == u.name.as_str()));
            ticked += (st.run.is_none() && is_ticked) as i32;
            let cells: Vec<FlashCell> = match (&st.run, in_run) {
                (Some(_), Some(ru)) => ru.cells.iter().map(|c| FlashCell { state: c.state, text: c.text.as_str().into(), progress: c.progress, bar: c.bar }).collect(),
                (Some(r), None) => r.parts.iter().map(|_| FlashCell { state: FlashState::None, ..Default::default() }).collect(),
                (None, _) => chosen
                    .iter()
                    .map(|p| match (is_ticked, st.lacks_deck(index, u, p)) {
                        (false, _) => FlashCell { state: FlashState::None, ..Default::default() },
                        (true, true) => FlashCell { state: FlashState::None, text: "No deck".into(), ..Default::default() },
                        (true, false) => FlashCell { state: FlashState::Wait, text: "Will flash".into(), ..Default::default() },
                    })
                    .collect(),
            };
            let channel = RadioUri::parse(&u.uri).ok().flatten().map(|r| format!("ch {}", r.channel)).unwrap_or_default();
            FlashUnit {
                name: u.name.clone(),
                channel: channel.into(),
                firmware: if u.firmware_version.is_empty() { "–".into() } else { u.firmware_version.clone() },
                other: connected(u) && !u.firmware_version.is_empty() && !usual.is_empty() && u.firmware_version != usual,
                ticked: st.run.as_ref().map_or(is_ticked, |_| in_run.is_some()),
                locked: st.run.is_some() || !picked,
                dim: !picked,
                cells: model(cells),
            }
        })
        .collect();
    if let Some(run) = &st.run {
        ticked = run.units.len() as i32;
        b.set_columns(model(run.parts.iter().map(|p| p.name().into()).collect()));
    } else {
        b.set_columns(model(columns));
    }
    b.set_units(model(rows));
    b.set_pick_counts(model(counts.to_vec()));
    b.set_ticked_count(ticked);

    // The progress
    match &st.run {
        Some(run) => {
            let steps: Vec<FlashStep> =
                run.step_names().into_iter().zip(&run.steps).map(|(name, state)| FlashStep { name: name.into(), state: *state }).collect();
            b.set_steps(model(steps));
            b.set_phases(model(if run.finished { Vec::new() } else { run.phases() }));
            b.set_cards(model(if run.finished { Vec::new() } else { run.cards() }));
            b.set_status(run.status.as_str().into());
        }
        None => {
            b.set_steps(model(Vec::new()));
            b.set_phases(model(Vec::new()));
            b.set_cards(model(Vec::new()));
            let decks = chosen.iter().any(|p| !p.is_firmware());
            b.set_status(
                if decks { "Keep the units powered. The swarm disconnects before the decks, and connects again after." } else { "Keep the units powered until they are connected again." }
                    .into(),
            );
        }
    }
    b.set_running(running);
    b.set_finished(st.run.as_ref().is_some_and(|r| r.finished));
    let ready = st.run.is_none() && ticked > 0 && !chosen.is_empty() && !(source == 0 && st.download.is_some());
    b.set_can_start(ready);
    b.set_start_label(
        match (&st.run, chosen.len()) {
            (Some(r), _) if !r.finished => "Flashing…".to_string(),
            (Some(_), _) => "Flashed".to_string(),
            (None, 1) => format!("Flash {} to {} units", chosen[0].name(), ticked),
            (None, n) => format!("Flash {} parts to {} units", n, ticked),
        }
        .into(),
    );
}

/// Read the platform, firmware and decks of the connected units: the
/// release platform is the one most of them are, and the Firmware column
/// shows.
async fn query_units(swarm_state: &SwarmState, ui_weak: &slint::Weak<AppWindow>, state: &Shared) -> Result<String, String> {
    let units: Vec<(usize, Arc<crazyflie_connection_server::Crazyflie>)> =
        swarm_state.lock().await.iter().map(|(i, cu)| (*i, cu.cf.clone())).collect();
    let found: Vec<(usize, String, String)> = units
        .iter()
        .map(|(i, cf)| (*i, cf.platform.device_type_name().unwrap_or_default().to_string(), cf.platform.firmware_version().unwrap_or_default().to_string()))
        .collect();
    let mut decks = HashMap::new();
    for (i, cf) in &units {
        let mut has = HashMap::new();
        for deck in DECKS {
            if let Ok(value) = cf.param.get_lossy(&format!("deck.{}", deck)).await {
                has.insert(deck.to_string(), value != 0.0);
            }
        }
        decks.insert(*i, has);
    }
    state.lock().unwrap().decks = decks;
    for (index, device_type, version) in &found {
        let (device_type, version) = (device_type.clone(), version.clone());
        update_unit(ui_weak, *index, move |u| {
            u.platform_type = device_type.into();
            u.firmware_version = version.into();
        });
    }
    let mut counts: HashMap<&'static str, usize> = HashMap::new();
    for (_, device_type, _) in &found {
        if let Some(platform) = firmware::platform_release_name(device_type) {
            *counts.entry(platform).or_default() += 1;
        }
    }
    match counts.len() {
        0 if found.is_empty() => Err("Connect the swarm: the release is picked for the platform of its units.".into()),
        0 => Err(format!("No release platform for a {}.", found[0].1)),
        _ => Ok(counts.into_iter().max_by_key(|(_, n)| *n).map(|(p, _)| p.to_string()).unwrap()),
    }
}

/// Download the picked release for the platform and read it.
fn fetch_release(state: &Shared) {
    let (asset, seq) = {
        let mut st = state.lock().unwrap();
        st.release_zip = None;
        st.download_problem = None;
        st.dirty = true;
        let (Some(platform), Some(tag)) = (st.platform.clone(), st.release.clone()) else { return };
        let Some(release) = st.releases.iter().find(|r| r.tag_name == tag) else { return };
        let Some(asset) = release.asset(&platform).cloned() else {
            st.download_problem = Some(format!("Release {} has no {}.", tag, firmware::release_asset_name(&platform, &tag)));
            return;
        };
        st.download_seq += 1;
        st.download = Some((0, asset.size));
        (asset, st.download_seq)
    };
    let state = state.clone();
    host::spawn(async move {
        let progress_state = state.clone();
        let downloaded = host::releases::download(&asset, move |done, of| {
            let mut st = progress_state.lock().unwrap();
            if st.download_seq == seq {
                st.download = Some((done, of));
                st.dirty = true;
            }
        })
        .await;
        let mut st = state.lock().unwrap();
        if st.download_seq != seq {
            return;
        }
        st.download = None;
        st.dirty = true;
        match downloaded.and_then(|data| ReleaseZip::read(&data)) {
            Ok(zip) => {
                st.release_zip = Some(zip);
                st.tick_parts();
            }
            Err(e) => st.download_problem = Some(format!("{:#}", e)),
        }
    });
}

/// Flash the STM32 and nRF51 images to the run's units as one job on the
/// connection server. Returns which units it worked for (run index).
async fn flash_firmware(client: &crazyflie_connection_server::Client, uris: Vec<String>, images: Vec<crazyflie_connection_server::FlashImage>, state: &Shared) -> HashSet<usize> {
    let refs: Vec<&str> = uris.iter().map(String::as_str).collect();
    let results = {
        let state = state.clone();
        client
            .flash_swarm(&refs, images, move |p| {
                let mut st = state.lock().unwrap();
                if let Some(run) = st.run.as_mut() {
                    run.firmware_progress(p);
                }
                st.dirty = true;
            })
            .await
    };
    let results = match results {
        Ok(results) => results,
        Err(e) => uris.iter().map(|_| Err(e.to_string())).collect(),
    };
    let mut flashed = HashSet::new();
    let mut st = state.lock().unwrap();
    let Some(run) = st.run.as_mut() else { return flashed };
    run.firmware.unicast_now = None;
    for (unit, result) in results.into_iter().enumerate() {
        match result {
            Ok(how) => {
                flashed.insert(unit);
                run.set_firmware_cells(unit, Cell::new(FlashState::Done, format!("Done, {}", how)));
            }
            Err(reason) => {
                eprintln!("Flashing {} failed: {}", run.units[unit].name, reason);
                for (k, column) in run.firmware_columns().into_iter().enumerate() {
                    run.units[unit].cells[column] = Cell::new(FlashState::Fail, if k == 0 { reason.clone() } else { "–".into() });
                }
            }
        }
    }
    st.dirty = true;
    flashed
}

/// Flash one deck part to the units, one unit per Crazyradio at a time with
/// the Crazyradios side by side.
async fn flash_deck_part(cx: &FlashContext, column: usize, part: &Part, units: Vec<(usize, String)>) {
    // Each unit's Crazyradio, from the radio plan
    let mut by_radio: Vec<(usize, Vec<(usize, String)>)> = Vec::new();
    for (unit, uri) in units {
        let link = link_uri(&cx.link_context, &cx.radio_plan, &uri).await;
        let radio = RadioUri::parse(&link).ok().flatten().and_then(|r| r.radio).unwrap_or(0);
        match by_radio.iter_mut().find(|(r, _)| *r == radio) {
            Some((_, list)) => list.push((unit, link)),
            None => by_radio.push((radio, vec![(unit, link)])),
        }
    }
    let of = by_radio.iter().map(|(_, l)| l.len()).sum();
    {
        let mut st = cx.state.lock().unwrap();
        if let Some(run) = st.run.as_mut() {
            run.deck = Some((0, of, Vec::new()));
        }
        st.dirty = true;
    }
    let target = part.key();
    let workers = by_radio.into_iter().map(|(radio, units)| {
        let target = target.clone();
        async move {
            for (unit, link) in units {
                let progress_state = cx.state.clone();
                let progress = move |done: usize, total: usize| {
                    let f = if total > 0 { done as f32 / total as f32 } else { 0.0 };
                    let mut st = progress_state.lock().unwrap();
                    if let Some(run) = st.run.as_mut() {
                        if let Some((_, _, busy)) = run.deck.as_mut() {
                            match busy.iter_mut().find(|(u, _, _)| *u == unit) {
                                Some(entry) => entry.2 = f,
                                None => busy.push((unit, radio, f)),
                            }
                        }
                        run.units[unit].cells[column] = Cell::busy(format!("Flashing {} %", (f * 100.0) as u32), f);
                    }
                    st.dirty = true;
                };
                {
                    let mut st = cx.state.lock().unwrap();
                    if let Some(run) = st.run.as_mut() {
                        if let Some((_, _, busy)) = run.deck.as_mut() {
                            busy.push((unit, radio, 0.0));
                        }
                        run.units[unit].cells[column] = Cell::new(FlashState::Busy, "Connecting…");
                    }
                    st.dirty = true;
                }
                let result =
                    bootloader_flash::flash_deck_target(&cx.link_context, &link, cx.toc_cache.clone(), &target, &part.data, progress).await;
                {
                    let mut st = cx.state.lock().unwrap();
                    if let Some(run) = st.run.as_mut() {
                        run.units[unit].cells[column] = match &result {
                            Ok(()) => Cell::new(FlashState::Done, "Done"),
                            Err(e) if e.downcast_ref::<bootloader_flash::NoDeck>().is_some() => Cell::new(FlashState::None, "No deck"),
                            Err(e) => {
                                eprintln!("Flashing {} to {} failed: {:#}", target, run.units[unit].name, e);
                                Cell::new(FlashState::Fail, format!("{:#}", e))
                            }
                        };
                        if let Some((done, _, busy)) = run.deck.as_mut() {
                            *done += 1;
                            busy.retain(|(u, _, _)| *u != unit);
                        }
                    }
                    st.dirty = true;
                }
                // Give the deck time to restart before the next unit on this Crazyradio
                host::sleep(Duration::from_millis(500)).await;
            }
        }
    });
    futures::future::join_all(workers).await;
}

/// What the job needs from the app.
#[derive(Clone)]
struct FlashContext {
    state: Shared,
    link_context: Arc<host::radios::LinkContext>,
    toc_cache: host::toc_cache::FileTocCache,
    connection: crate::connection::SharedConnection,
    swarm_state: SwarmState,
    radio_plan: host::radios::SharedRadioPlan,
    positioning_source: Arc<tokio::sync::Mutex<Option<usize>>>,
    ui_weak: slint::Weak<AppWindow>,
}

/// Flash the parts to the units: the STM32 and nRF51 first, as one job,
/// then each deck part.
async fn run(cx: FlashContext, parts: Vec<Part>, units: Vec<(usize, String, String)>, reconnect: bool) {
    let (client, drives_radios) = {
        let connection = cx.connection.read().await;
        (connection.client().clone(), connection.drives_radios())
    };
    let firmware: Vec<&Part> = parts.iter().filter(|p| p.is_firmware()).collect();
    let decks: Vec<(usize, &Part)> = parts.iter().enumerate().filter(|(_, p)| !p.is_firmware()).collect();
    let set = |f: &dyn Fn(&mut Run)| {
        let mut st = cx.state.lock().unwrap();
        if let Some(run) = st.run.as_mut() {
            f(run);
        }
        st.dirty = true;
    };

    // The STM32 and nRF51, by broadcast where they can
    let mut ok: HashSet<usize> = (0..units.len()).collect();
    if !firmware.is_empty() {
        set(&|run| {
            run.steps[0] = FlashState::Busy;
            run.status = "Keep the units powered.".into();
        });
        let mut uris = Vec::new();
        for (_, uri, _) in &units {
            // With this computer's Crazyradios, each restarts into its
            // bootloader from the Crazyradio the radio plan gives it
            uris.push(if drives_radios { link_uri(&cx.link_context, &cx.radio_plan, uri).await } else { uri.clone() });
        }
        let images = firmware
            .iter()
            .map(|p| crazyflie_connection_server::FlashImage { target: p.server_target().unwrap().to_string(), start: None, data: p.data.clone() })
            .collect();
        ok = flash_firmware(&client, uris, images, &cx.state).await;
        let all = ok.len() == units.len();
        set(&|run| run.steps[0] = if all { FlashState::Done } else { FlashState::Fail });
    }

    // The decks: the swarm off its links, every unit flashed over a link of its own
    if !decks.is_empty() {
        let first_deck_step = if firmware.is_empty() { 0 } else { 1 };
        if !drives_radios {
            set(&|run| {
                for (column, _) in &decks {
                    for unit in run.units.iter_mut() {
                        unit.cells[*column] = Cell::new(FlashState::Fail, "Only with this computer's Crazyradios");
                    }
                }
                for step in first_deck_step..run.steps.len() {
                    run.steps[step] = FlashState::Fail;
                }
            });
        } else {
            crate::flashing::disconnect_for_flashing(&cx.swarm_state, &cx.positioning_source, &cx.ui_weak).await;
            if !firmware.is_empty() && !ok.is_empty() {
                set(&|run| run.status = "Waiting for the units to start their new firmware…".into());
                host::sleep(RESTART_DELAY).await;
            }
            for (k, (column, part)) in decks.iter().enumerate() {
                let step = first_deck_step + k;
                set(&|run| {
                    run.step = step;
                    run.steps[step] = FlashState::Busy;
                    run.status = "Keep the units powered.".into();
                    // Units whose firmware failed keep their decks
                    for (unit, ru) in run.units.iter_mut().enumerate() {
                        if !ok.contains(&unit) {
                            ru.cells[*column] = Cell::new(FlashState::None, "Skipped");
                        }
                    }
                });
                // Not the ones known to lack the deck
                let targets: Vec<(usize, String)> = {
                    let st = cx.state.lock().unwrap();
                    let lacking: HashSet<usize> = st
                        .run
                        .as_ref()
                        .map(|run| run.units.iter().enumerate().filter(|(_, u)| u.cells[*column].state == FlashState::None).map(|(i, _)| i).collect())
                        .unwrap_or_default();
                    units.iter().enumerate().filter(|(i, _)| ok.contains(i) && !lacking.contains(i)).map(|(i, (_, uri, _))| (i, uri.clone())).collect()
                };
                flash_deck_part(&cx, *column, part, targets).await;
                set(&|run| {
                    let failed = run.units.iter().any(|u| u.cells[*column].state == FlashState::Fail);
                    run.steps[step] = if failed { FlashState::Fail } else { FlashState::Done };
                    run.deck = None;
                });
            }
            if reconnect {
                // The last ones restart after their decks are flashed
                set(&|run| run.status = "Waiting for the units to restart…".into());
                host::sleep(RESTART_DELAY).await;
                let ui_weak = cx.ui_weak.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.invoke_connect_swarm();
                    }
                })
                .ok();
            }
        }
    }

    set(&|run| {
        run.finished = true;
        let failed = run.units.iter().filter(|u| u.cells.iter().any(|c| c.state == FlashState::Fail)).count();
        run.status = if failed == 0 {
            format!("All {} units flashed.", run.units.len())
        } else {
            format!("{} of {} units flashed; {} failed, see their rows.", run.units.len() - failed, run.units.len(), failed)
        };
    });
}

/// Disconnect the whole swarm before flashing deck firmware: each unit is
/// flashed over a connection of its own, and every unit is about to be
/// restarted anyway.
pub(crate) async fn disconnect_for_flashing(
    swarm_state: &SwarmState,
    positioning_source: &Arc<tokio::sync::Mutex<Option<usize>>>,
    ui_weak: &slint::Weak<AppWindow>,
) {
    *positioning_source.lock().await = None;
    let connected: Vec<(usize, ConnectedUnit)> = {
        let mut state = swarm_state.lock().await;
        state.drain().collect()
    };
    for (index, unit) in connected {
        unit.disconnect().await;
        update_unit(ui_weak, index, |u| {
            u.state = UnitState::Disconnected;
        });
    }
    {
        let ui_weak = ui_weak.clone();
        slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_swarm_connected(false);
            }
        })
        .ok();
    }
    // Give the radio a moment to settle after disconnecting.
    host::sleep(Duration::from_millis(300)).await;
}

pub(crate) fn setup(ui: &AppWindow, cx: &AppContext) {
    let state: Shared = Arc::new(Mutex::new(Dialog::default()));
    let b = ui.global::<FlashBridge>();
    let fcx = FlashContext {
        state: state.clone(),
        link_context: cx.link_context.clone(),
        toc_cache: cx.toc_cache.clone(),
        connection: cx.connection.clone(),
        swarm_state: cx.swarm_state.clone(),
        radio_plan: cx.radio_plan.clone(),
        positioning_source: cx.positioning_source.clone(),
        ui_weak: ui.as_weak(),
    };

    // Show what changed, a few times a second while the dialog is open
    {
        let state = state.clone();
        let ui_weak = ui.as_weak();
        let timer = slint::Timer::default();
        timer.start(slint::TimerMode::Repeated, Duration::from_millis(150), move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            if ui.global::<FlashBridge>().get_visible() && state.lock().unwrap().dirty {
                refresh(&ui, &state);
            }
        });
        Box::leak(Box::new(timer));
    }

    // Opened: the releases on GitHub, and the units' platform and firmware
    {
        let fcx = fcx.clone();
        b.on_opened(move || {
            let Some(ui) = fcx.ui_weak.upgrade() else { return };
            {
                let mut st = fcx.state.lock().unwrap();
                if st.run.as_ref().is_some_and(|r| r.finished) {
                    st.run = None;
                }
                st.unticked.clear();
            }
            refresh(&ui, &fcx.state);
            let fcx = fcx.clone();
            host::spawn(async move {
                let platform = query_units(&fcx.swarm_state, &fcx.ui_weak, &fcx.state).await;
                let releases = host::releases::list().await;
                let changed = {
                    let mut st = fcx.state.lock().unwrap();
                    let before = (st.platform.clone(), st.release.clone());
                    match platform {
                        Ok(p) => {
                            st.platform = Some(p);
                            st.platform_problem = None;
                        }
                        Err(why) => st.platform_problem = Some(why),
                    }
                    match releases {
                        Ok(list) => {
                            st.releases_problem = if list.is_empty() { Some("No releases on GitHub".into()) } else { None };
                            if st.release.as_ref().is_none_or(|tag| !list.iter().any(|r| &r.tag_name == tag)) {
                                st.release = list.first().map(|r| r.tag_name.clone());
                            }
                            st.releases = list;
                        }
                        Err(e) => st.releases_problem = Some(format!("{:#}", e)),
                    }
                    st.dirty = true;
                    before != (st.platform.clone(), st.release.clone()) || st.release_zip.is_none()
                };
                if changed {
                    fetch_release(&fcx.state);
                }
            });
        });
    }

    // The source tab, the release, the files
    {
        let state = state.clone();
        b.on_source_changed(move |_| state.lock().unwrap().dirty = true);
    }
    {
        let state = state.clone();
        b.on_release_picked(move |index| {
            {
                let mut st = state.lock().unwrap();
                let Some(release) = st.releases.get(index as usize) else { return };
                st.release = Some(release.tag_name.clone());
            }
            fetch_release(&state);
        });
    }
    {
        let state = state.clone();
        b.on_choose_zip(move || {
            let state = state.clone();
            host::spawn(async move {
                let Some(file) = host::dialog::FileDialog::new().set_title("Open a firmware release").add_filter("Release", &["zip"]).pick_file().await else { return };
                let name = file.path().file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                let read = host::fs::read(file.path()).map_err(anyhow::Error::from).and_then(|data| ReleaseZip::read(&data));
                let mut st = state.lock().unwrap();
                match read {
                    Ok(zip) => {
                        st.zip = Some((name, zip));
                        st.zip_problem = None;
                        st.tick_parts();
                    }
                    Err(e) => {
                        st.zip = None;
                        st.zip_problem = Some(format!("{}: {:#}", name, e));
                    }
                }
                st.dirty = true;
            });
        });
    }
    {
        let state = state.clone();
        b.on_choose_binary(move || {
            let state = state.clone();
            host::spawn(async move {
                let Some(file) = host::dialog::FileDialog::new()
                    .set_title("Open a firmware binary")
                    .add_filter("Firmware binary", &["bin"])
                    .add_filter("All files", &["*"])
                    .pick_file()
                    .await
                else {
                    return;
                };
                let name = file.path().file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                let mut st = state.lock().unwrap();
                match host::fs::read(file.path()) {
                    Ok(data) => {
                        st.binary = Some((name, data));
                        if st.bin_target.is_none() {
                            st.bin_target = Some("stm32-fw".into());
                        }
                    }
                    Err(e) => eprintln!("Reading {}: {}", name, e),
                }
                st.dirty = true;
            });
        });
    }

    // What and who
    {
        let state = state.clone();
        b.on_toggle_part(move |key| {
            let mut st = state.lock().unwrap();
            if !st.unchecked.remove(key.as_str()) {
                st.unchecked.insert(key.to_string());
            }
            st.dirty = true;
        });
    }
    {
        let state = state.clone();
        b.on_bin_target_picked(move |key| {
            let mut st = state.lock().unwrap();
            st.bin_target = Some(key.to_string());
            st.dirty = true;
        });
    }
    {
        let state = state.clone();
        b.on_pick_changed(move |_| {
            let mut st = state.lock().unwrap();
            st.unticked.clear();
            st.dirty = true;
        });
    }
    {
        let state = state.clone();
        b.on_toggle_unit(move |name| {
            let mut st = state.lock().unwrap();
            if !st.unticked.remove(name.as_str()) {
                st.unticked.insert(name.to_string());
            }
            st.dirty = true;
        });
    }

    // Flash
    {
        let fcx = fcx.clone();
        b.on_start(move || {
            let Some(ui) = fcx.ui_weak.upgrade() else { return };
            let b = ui.global::<FlashBridge>();
            let units: Vec<UnitData> = ui.get_units().iter().collect();
            let usual = usual_firmware(&units);
            let pick = b.get_pick();
            let (parts, chosen, run_units) = {
                let st = fcx.state.lock().unwrap();
                let parts = st.chosen_parts(b.get_source());
                let chosen: Vec<(usize, String, String)> = units
                    .iter()
                    .enumerate()
                    .filter(|(_, u)| match pick {
                        0 => connected(u),
                        1 => true,
                        _ => connected(u) && !u.firmware_version.is_empty() && u.firmware_version != usual,
                    })
                    .filter(|(_, u)| !st.unticked.contains(u.name.as_str()))
                    .map(|(i, u)| (i, u.uri.to_string(), u.name.to_string()))
                    .collect();
                let run_units: Vec<RunUnit> = chosen
                .iter()
                .map(|(index, uri, name)| RunUnit {
                    name: name.clone(),
                    key: crazyflie_connection_server::normalize_uri(uri).map(|u| u.key).unwrap_or_else(|_| uri.clone()),
                    cells: parts
                        .iter()
                        // Units known to lack a deck aren't flashed with it
                        .map(|p| if st.lacks_deck(*index, &units[*index], p) { Cell::new(FlashState::None, "No deck") } else { Cell::new(FlashState::Wait, "Waiting") })
                        .collect(),
                })
                .collect();
                (parts, chosen, run_units)
            };
            if parts.is_empty() || chosen.is_empty() {
                return;
            }
            let step_count = parts.iter().any(Part::is_firmware) as usize + parts.iter().filter(|p| !p.is_firmware()).count();
            let mut st = fcx.state.lock().unwrap();
            st.run = Some(Run {
                parts: parts.clone(),
                units: run_units,
                step: 0,
                steps: vec![FlashState::Wait; step_count],
                firmware: FirmwareJob::default(),
                deck: None,
                finished: false,
                status: "Starting…".into(),
            });
            st.dirty = true;
            drop(st);
            refresh(&ui, &fcx.state);
            let reconnect = ui.get_swarm_connected();
            host::spawn(run(fcx.clone(), parts, chosen, reconnect));
        });
    }
}

/// For screenshots (`SWARMKEEPER_FLASH=setup|running|decks`): the dialog with
/// a made-up release, before flashing or part way.
pub fn seed_screenshot(ui: &AppWindow, phase: &str) {
    let release = |tag: &str, date: &str| Release {
        tag_name: tag.into(),
        name: Some(tag.into()),
        published_at: Some(date.into()),
        prerelease: false,
        assets: Vec::new(),
    };
    let part = |file: &str, key: &str, kb: usize| Part::from_binary(file, key, vec![0; kb * 1024]);
    let parts = vec![part("cf2-2025.09.bin", "stm32-fw", 412), part("cf2_nrf-2025.09.bin", "nrf51-fw", 106), part("lighthouse.bin", "bcLighthouse4-fw", 104)];
    let mut dialog = Dialog {
        releases: vec![release("2025.09", "2025-09-03T10:00:00Z"), release("2025.02", "2025-02-11T10:00:00Z")],
        platform: Some("cf2".into()),
        release: Some("2025.09".into()),
        release_zip: Some(ReleaseZip { platform: "cf2".into(), release: "2025.09".into(), parts: parts.clone() }),
        ..Default::default()
    };
    let units: Vec<UnitData> = ui.get_units().iter().collect();
    if phase != "setup" {
        let chosen: Vec<(usize, &UnitData)> = units.iter().enumerate().filter(|(_, u)| connected(u)).collect();
        let mut run = Run {
            parts: parts.clone(),
            units: chosen.iter().map(|(_, u)| RunUnit { name: u.name.to_string(), key: u.name.to_string(), cells: vec![Cell::new(FlashState::Wait, "Waiting"); parts.len()] }).collect(),
            step: 0,
            steps: vec![FlashState::Busy, FlashState::Wait],
            firmware: FirmwareJob::default(),
            deck: None,
            finished: false,
            status: "Keep the units powered.".into(),
        };
        let n = run.units.len();
        let old = |name: &str| name == "CF-05" || name == "CF-22";
        if phase == "running" {
            for unit in 0..n {
                run.firmware.in_bootloader.insert(unit);
                if !old(&run.units[unit].name) {
                    run.firmware.armed.push(unit);
                }
            }
            run.firmware.broadcast = Some(("nrf51".into(), 106, 106));
            run.firmware.broadcast_done = vec!["stm32".into(), "nrf51".into()];
            let armed = run.firmware.armed.clone();
            for (k, unit) in armed.iter().enumerate() {
                let cell = if k < 30 { Cell::new(FlashState::Done, "Checked, restarting") } else if k == 30 { Cell::new(FlashState::Busy, "Checking…") } else { Cell::new(FlashState::Wait, "Broadcast, to check") };
                run.set_firmware_cells(*unit, cell);
                if k < 30 {
                    run.firmware.checked.insert(*unit);
                }
            }
            run.firmware.checking = armed.get(30).copied();
            if let Some(unit) = run.units.iter().position(|u| u.name == "CF-05") {
                run.firmware.unicast.push(unit);
                run.firmware.unicast_now = Some((unit, "stm32".into(), 0.64));
                run.units[unit].cells[0] = Cell::busy("One at a time 64 %", 0.64);
                run.units[unit].cells[1] = Cell::new(FlashState::Wait, "In the bootloader");
            }
            if let Some(unit) = run.units.iter().position(|u| u.name == "CF-22") {
                run.set_firmware_cells(unit, Cell::new(FlashState::Wait, "In the bootloader"));
            }
        } else {
            run.step = 1;
            run.steps = vec![FlashState::Done, FlashState::Busy];
            for unit in 0..n {
                run.set_firmware_cells(unit, Cell::new(FlashState::Done, "Done, broadcast"));
            }
            let busy: Vec<(usize, usize, f32)> = [(6, 0, 0.72), (18, 1, 0.40), (29, 2, 0.12), (39, 3, 0.90)].iter().filter(|(u, _, _)| *u < n).map(|&(u, r, f)| (u, r, f)).collect();
            for unit in 0..n {
                let on_radio = busy.iter().find(|(u, _, _)| *u == unit);
                run.units[unit].cells[2] = match on_radio {
                    Some((_, _, f)) => Cell::busy(format!("Flashing {} %", (f * 100.0) as u32), *f),
                    None if unit % 12 < 5 => Cell::new(FlashState::Done, "Done"),
                    None => Cell::new(FlashState::Wait, "Waiting"),
                };
            }
            run.deck = Some((n * 5 / 12, n, busy));
        }
        dialog.run = Some(run);
    }
    let state: Shared = Arc::new(Mutex::new(dialog));
    ui.global::<FlashBridge>().set_visible(true);
    refresh(ui, &state);
}
