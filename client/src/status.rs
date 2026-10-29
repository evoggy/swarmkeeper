//! The Status screen: how the swarm, the radios and the servers are doing,
//! worked out from the same unit rows as the Units screen.

use std::cell::RefCell;
use std::collections::HashMap;

use slint::{Model, ModelRc, SharedString, VecModel};

use crate::{
    ActivityRow, AppWindow, AttentionRow, Fact, GridCell, Health, HealthTile, HistBar, RadioRow, ServerRow,
    StatusData, SwarmSummary, TrafficSeries, UnitData, UnitRow,
};

/// Traffic samples kept per radio: ten minutes, one a second. The chart draws
/// their means over five seconds, as one a second is jagged.
const TRAFFIC_SAMPLES: usize = 600;
const TRAFFIC_INTERVAL_S: i64 = 1;
const TRAFFIC_MEAN: u64 = 5;

#[derive(Default)]
struct History {
    /// Health of each unit (by name) the last time, to log the changes.
    health: HashMap<String, Health>,
    activity: Vec<ActivityRow>,
    /// Packets per second per radio, oldest first.
    traffic: Vec<Vec<f32>>,
    last_sample: i64,
    /// Samples taken so far, so each mean on the chart keeps its seconds.
    taken: u64,
    /// The traffic chart as drawn after the last sample.
    chart: Vec<TrafficSeries>,
}

thread_local! {
    static HISTORY: RefCell<History> = RefCell::new(History::default());
}

fn now() -> chrono::DateTime<chrono::Local> {
    chrono::Local::now()
}

/// Add a line to the activity log on the Status screen.
pub(crate) fn log(text: impl Into<SharedString>, tone: Health) {
    HISTORY.with(|h| {
        let mut h = h.borrow_mut();
        h.activity.insert(0, ActivityRow { time: now().format("%H:%M:%S").to_string().into(), text: text.into(), tone });
        h.activity.truncate(50);
    });
}

fn changed_text(name: &str, row: &UnitRow) -> Option<String> {
    Some(match row.health {
        Health::Off => format!("{} is not connected", name),
        Health::Fault | Health::Warn => format!("{}: {}", name, row.status.to_lowercase()),
        Health::Ready => format!("{} is ready to fly", name),
        Health::Flying => format!("{} took off", name),
        Health::Charging => format!("{} is charging", name),
        Health::Charged => format!("{} is charged", name),
    })
}

/// Rebuild the Status screen. `units` and `rows` are the sorted units and
/// their rows from `units::rebuild_table_rows`.
pub(crate) fn rebuild(ui: &AppWindow, units: &[UnitData], rows: &[UnitRow], summary: &SwarmSummary) {
    // Log what changed since last time.
    HISTORY.with(|h| {
        let mut h = h.borrow_mut();
        let mut changes = Vec::new();
        for (u, row) in units.iter().zip(rows) {
            let name = u.name.to_string();
            match h.health.insert(name.clone(), row.health) {
                Some(before) if before != row.health => changes.push((changed_text(&name, row), row.health)),
                _ => {}
            }
        }
        drop(h);
        // Many units changing at once (connecting the swarm) is one line.
        if changes.len() > 5 {
            let tone = if changes.iter().all(|(_, t)| *t == Health::Off) { Health::Off } else { Health::Ready };
            log(format!("{} units changed state", changes.len()), tone);
        } else {
            for (text, tone) in changes.into_iter().filter_map(|(t, tone)| t.map(|t| (t, tone))) {
                log(text, tone);
            }
        }
    });

    let connected = summary.connected > 0;
    let mut data = StatusData::default();

    // The overall band
    data.issues = summary.attention;
    if !connected {
        data.tone = Health::Off;
        data.headline = if summary.total == 0 { "No swarm open".into() } else { "Not connected".into() };
        data.subline = if summary.total == 0 {
            "Open a swarm to start".into()
        } else {
            format!("{} units in the swarm. Connect to see how they are doing.", summary.total).into()
        };
    } else if summary.flying > 0 {
        data.tone = Health::Flying;
        data.headline = format!("{} of {} units flying", summary.flying, summary.total).into();
        data.subline = attention_line(summary).into();
    } else {
        data.tone = if summary.attention > 0 { Health::Warn } else { Health::Ready };
        data.headline = format!("{} of {} units ready to fly", summary.ready, summary.total).into();
        data.subline = attention_line(summary).into();
    }

    // The state bar: ready, on a charger, warning, fault (connected) and not responding
    let warn = rows.iter().filter(|r| r.health == Health::Warn).count() as i32;
    let fault = rows.iter().filter(|r| r.health == Health::Fault && !r.link.is_empty()).count() as i32;
    data.state_counts = ModelRc::new(VecModel::from(vec![
        summary.ready + summary.flying,
        summary.charging + summary.charged,
        warn,
        fault,
        summary.disconnected,
    ]));

    // The unit grid
    data.grid = ModelRc::new(VecModel::from(
        units
            .iter()
            .zip(rows)
            .enumerate()
            .map(|(i, (u, r))| GridCell { name: u.name.clone(), health: r.health, row: i as i32 })
            .collect::<Vec<_>>(),
    ));

    // Battery histogram, 3.5..4.2 V in 0.1 V bins, units on battery or charging
    let mut bins = [0i32; 7];
    for (u, r) in units.iter().zip(rows) {
        if !r.battery.is_empty() {
            let bin = ((u.battery_voltage - 3.5) / 0.1).floor().clamp(0.0, 6.0) as usize;
            bins[bin] += 1;
        }
    }
    let most = (*bins.iter().max().unwrap_or(&0)).max(1) as f32;
    data.histogram = ModelRc::new(VecModel::from(
        bins.iter()
            .enumerate()
            .map(|(i, &n)| HistBar {
                label: format!("{:.1}", 3.5 + i as f32 * 0.1).into(),
                count: n,
                fraction: n as f32 / most,
                low: i == 0,
            })
            .collect::<Vec<_>>(),
    ));

    // Facts
    let lh = units.iter().filter(|u| u.deck_lighthouse).count();
    let loco = units.iter().filter(|u| u.deck_loco).count();
    let selftest_failed = rows.iter().filter(|r| r.status.as_str() == "Selftest failed").count();
    data.facts = ModelRc::new(VecModel::from(vec![
        Fact {
            label: "Firmware".into(),
            value: if summary.firmware.is_empty() {
                "–".into()
            } else if summary.firmware_differs > 0 {
                format!("{} · {} other", summary.firmware, summary.firmware_differs).into()
            } else {
                summary.firmware.clone()
            },
            tone: if summary.firmware_differs > 0 { Health::Warn } else { Health::Ready },
        },
        Fact {
            label: "Decks".into(),
            value: match (lh, loco) {
                (0, 0) => "No positioning deck".into(),
                (lh, 0) => format!("Lighthouse on {}", lh).into(),
                (0, loco) => format!("Loco on {}", loco).into(),
                (lh, loco) => format!("Lighthouse {} · Loco {}", lh, loco).into(),
            },
            tone: Health::Ready,
        },
        Fact {
            label: "Selftest".into(),
            value: if !connected {
                "–".into()
            } else if selftest_failed > 0 {
                format!("{} failed", selftest_failed).into()
            } else {
                "All passed".into()
            },
            tone: if selftest_failed > 0 { Health::Fault } else { Health::Ready },
        },
        Fact {
            label: "Lowest battery".into(),
            value: if summary.lowest_battery.is_empty() {
                "–".into()
            } else {
                format!("{} V · {}", summary.lowest_battery, summary.lowest_battery_unit).into()
            },
            tone: if summary.lowest_battery_level < 0.25 && !summary.lowest_battery.is_empty() { Health::Warn } else { Health::Ready },
        },
    ]));

    // Needs attention
    data.attention = ModelRc::new(VecModel::from(
        units
            .iter()
            .zip(rows)
            .enumerate()
            .filter(|(_, (_, r))| matches!(r.health, Health::Warn | Health::Fault))
            .map(|(i, (u, r))| AttentionRow {
                name: u.name.clone(),
                status: r.status.clone(),
                detail: r.detail.clone(),
                health: r.health,
                row: i as i32,
                action: match r.status.as_str() {
                    "Timed out" | "Connection failed" => "Connect",
                    "Tumbled" | "Locked" => "Recover",
                    "Other firmware" => "Flash",
                    _ => "Open",
                }
                .into(),
            })
            .collect::<Vec<_>>(),
    ));

    // Radios: the units on each Crazyradio
    let radio_count = units.iter().map(|u| u.radio).max().unwrap_or(-1) + 1;
    let mut radios = Vec::new();
    let mut rates = Vec::new();
    for radio in 0..radio_count.max(0) {
        let on: Vec<&UnitData> =
            units.iter().zip(rows).filter(|(u, r)| u.radio == radio && !r.link.is_empty()).map(|(u, _)| u).collect();
        let all = units.iter().filter(|u| u.radio == radio).count();
        let link = if on.is_empty() { 0.0 } else { on.iter().map(|u| u.link_quality).sum::<f32>() / on.len() as f32 };
        let rate: f32 = on.iter().map(|u| u.uplink_rate + u.downlink_rate).sum();
        let retries = if on.is_empty() { 0.0 } else { on.iter().map(|u| u.avg_retries).sum::<f32>() / on.len() as f32 };
        rates.push(rate);
        radios.push(RadioRow {
            name: format!("Radio {}", radio).into(),
            units: format!("{} of {}", on.len(), all).into(),
            link: if on.is_empty() { "–".into() } else { format!("{:.0} %", link * 100.0).into() },
            rate: format!("{:.0}", rate).into(),
            retries: format!("{:.2}", retries).into(),
            tone: if on.is_empty() { Health::Off } else if link < 0.7 { Health::Warn } else { Health::Ready },
            index: radio,
        });
    }
    data.radios = ModelRc::new(VecModel::from(radios));
    data.traffic = traffic(&rates);

    // Health tiles
    let warn_radios = (0..data.radios.row_count()).filter(|&i| data.radios.row_data(i).is_some_and(|r| r.tone == Health::Warn)).count();
    data.tiles = ModelRc::new(VecModel::from(vec![
        HealthTile {
            title: "Swarm".into(),
            state: if !connected { "Idle".into() } else if summary.attention > 0 { "Degraded".into() } else { "Healthy".into() },
            tone: if !connected { Health::Off } else if summary.attention > 0 { Health::Warn } else { Health::Ready },
            detail: format!("{} of {} connected", summary.connected, summary.total).into(),
            target: "units".into(),
        },
        HealthTile {
            title: "Radios".into(),
            state: if radio_count <= 0 { "Idle".into() } else if warn_radios > 0 { "Degraded".into() } else { "Healthy".into() },
            tone: if radio_count <= 0 { Health::Off } else if warn_radios > 0 { Health::Warn } else { Health::Ready },
            detail: if radio_count <= 0 { "No radio in use".into() } else { format!("{} Crazyradios in use", radio_count).into() },
            target: "radio".into(),
        },
        HealthTile {
            title: "Connection".into(),
            state: if ui.get_connection_problem() { "Fallback".into() } else { "Healthy".into() },
            tone: if ui.get_connection_problem() { Health::Warn } else { Health::Ready },
            detail: ui.get_connection_label(),
            target: "settings".into(),
        },
        HealthTile {
            title: "Positioning".into(),
            state: if !connected { "Idle".into() } else if lh + loco > 0 { "Healthy".into() } else { "None".into() },
            tone: if !connected || lh + loco == 0 { Health::Off } else { Health::Ready },
            detail: if lh > 0 { format!("Lighthouse on {} units", lh).into() } else if loco > 0 { format!("Loco on {} units", loco).into() } else { "No positioning deck".into() },
            target: "view3d".into(),
        },
        HealthTile {
            title: "Account".into(),
            state: if ui.get_signed_in() && !ui.get_needs_sign_in() { "Signed in".into() } else if ui.get_signed_in() { "Sign in again".into() } else { "Signed out".into() },
            tone: if ui.get_signed_in() && !ui.get_needs_sign_in() { Health::Ready } else if ui.get_signed_in() { Health::Warn } else { Health::Off },
            detail: "arc.bitcraze.io".into(),
            target: "settings".into(),
        },
    ]));

    // Servers
    let drives = ui.get_drives_radios();
    // The organizations, else the first line of the account text.
    let account_text = ui.get_account_text();
    let account_orgs = ui.get_account_orgs();
    let account_detail = if account_orgs.is_empty() { account_text.split("\n\n").next().unwrap_or_default() } else { account_orgs.as_str() };
    data.servers = ModelRc::new(VecModel::from(vec![
        ServerRow {
            name: if drives { "This computer".into() } else { "Connection server".into() },
            kind: if drives { "Swarmkeeper's own connection server".into() } else { "Crazyflies through".into() },
            state: if ui.get_connection_problem() { "Fallback".into() } else { "Connected".into() },
            detail: ui.get_connection_label(),
            tone: if ui.get_connection_problem() { Health::Warn } else { Health::Ready },
        },
        ServerRow {
            name: "arc.bitcraze.io".into(),
            kind: "Shared swarms".into(),
            state: if ui.get_signed_in() && !ui.get_needs_sign_in() { "Signed in".into() } else if ui.get_signed_in() { "Sign in again".into() } else { "Signed out".into() },
            detail: account_detail.into(),
            tone: if ui.get_signed_in() && !ui.get_needs_sign_in() { Health::Ready } else if ui.get_signed_in() { Health::Warn } else { Health::Off },
        },
    ]));

    data.activity = HISTORY.with(|h| ModelRc::new(VecModel::from(h.borrow().activity.clone())));
    ui.set_status(data);
}

/// Ten minutes of made-up traffic on `radios` radios, for the demo swarm.
pub(crate) fn seed_demo_traffic(radios: usize) {
    HISTORY.with(|h| {
        let mut h = h.borrow_mut();
        h.traffic = (0..radios)
            .map(|r| {
                (0..TRAFFIC_SAMPLES)
                    .map(|i| {
                        let t = i as f32 / 40.0 + r as f32;
                        let spike = if r == 2 && (350..390).contains(&i) { 900.0 } else { 0.0 };
                        2400.0 + 300.0 * (t.sin() + 0.5 * (2.3 * t).cos()) + spike
                    })
                    .collect()
            })
            .collect();
        h.last_sample = now().timestamp();
        h.taken = TRAFFIC_SAMPLES as u64;
        h.chart = draw_traffic(&h.traffic, h.taken);
    });
}

fn attention_line(summary: &SwarmSummary) -> String {
    match summary.attention {
        0 => "Nothing needs attention".to_string(),
        1 => format!("1 unit needs attention: {}", summary.attention_text),
        n => format!("{} units need attention: {}", n, summary.attention_text),
    }
}

/// Keep a sample per radio every second and draw the last ten minutes. The
/// chart is drawn again only when a sample is added.
fn traffic(rates: &[f32]) -> ModelRc<TrafficSeries> {
    HISTORY.with(|h| {
        let mut h = h.borrow_mut();
        let t = now().timestamp();
        let radios_changed = h.traffic.len() != rates.len();
        if radios_changed {
            h.traffic = vec![Vec::new(); rates.len()];
        }
        let sampled = t - h.last_sample >= TRAFFIC_INTERVAL_S;
        if sampled {
            h.last_sample = t;
            h.taken += 1;
            for (series, &rate) in h.traffic.iter_mut().zip(rates) {
                series.push(rate);
                if series.len() > TRAFFIC_SAMPLES {
                    series.remove(0);
                }
            }
        }
        if sampled || radios_changed {
            h.chart = draw_traffic(&h.traffic, h.taken);
        }
        ModelRc::new(VecModel::from(h.chart.clone()))
    })
}

/// The traffic of each radio as stacked areas, in a 600 × 100 box (the chart
/// scales it). `taken` is the number of samples taken so far.
fn draw_traffic(traffic: &[Vec<f32>], taken: u64) -> Vec<TrafficSeries> {
    let samples = traffic.first().map_or(0, |s| s.len());
    // The means over five seconds, each drawn at its last second: the last
    // one over the seconds so far, so the right edge moves every second.
    let first = taken.saturating_sub(samples as u64);
    let seconds: Vec<usize> = (0..samples).collect();
    let points: Vec<(usize, Vec<f32>)> = seconds
        .chunk_by(|&a, &b| (first + a as u64) / TRAFFIC_MEAN == (first + b as u64) / TRAFFIC_MEAN)
        .map(|mean| {
            let (from, to) = (mean[0], mean[mean.len() - 1]);
            (to, traffic.iter().map(|s| s[from..=to].iter().sum::<f32>() / mean.len() as f32).collect())
        })
        .collect();
    if points.len() < 2 {
        return Vec::new();
    }
    let top = points.iter().map(|(_, rates)| rates.iter().sum()).fold(1.0f32, f32::max) * 1.1;
    let x = |i: usize| 600.0 * (TRAFFIC_SAMPLES - samples + i) as f32 / (TRAFFIC_SAMPLES - 1) as f32;
    let y = |v: f32| 100.0 - 100.0 * v / top;
    let mut below = vec![0.0f32; points.len()];
    let mut series = Vec::new();
    for index in 0..traffic.len() {
        let above: Vec<f32> = below.iter().zip(&points).map(|(b, (_, rates))| b + rates[index]).collect();
        let mut path = format!("M {:.1} {:.1}", x(points[0].0), y(above[0]));
        for (k, (i, _)) in points.iter().enumerate().skip(1) {
            path += &format!(" L {:.1} {:.1}", x(*i), y(above[k]));
        }
        for (k, (i, _)) in points.iter().enumerate().rev() {
            path += &format!(" L {:.1} {:.1}", x(*i), y(below[k]));
        }
        path += " Z";
        let last = points.len() - 1;
        series.push(TrafficSeries {
            index: index as i32,
            commands: path.into(),
            label: format!("Radio {}", index).into(),
            label_y: (y(above[last]) + y(below[last])) / 2.0,
            value: format!("{:.0}", points[last].1[index]).into(),
        });
        below = above;
    }
    series
}
