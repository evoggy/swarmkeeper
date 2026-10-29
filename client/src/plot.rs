//! Live plots of log variables from one or more units, in one log.
//!
//! The Plot screen (ui/plot-screen.slint) picks a log configuration and the
//! units; each unit streams the configuration's variables into the same log.
//! The dock under the Units table and the 3D view shows that log too, and its
//! Start logs the selected unit. Everything reads and writes the `PlotBridge`
//! global: the charts are Slint `Path`s from command strings built here in the
//! plot area's pixels. Pan, zoom, hover and the navigator are ported from
//! swarmfinity's Explore plot.
//!
//! Each Crazyflie stamps its samples with its own clock, counting from its
//! boot. To put several units on one time axis, a unit's timestamps are moved
//! onto this computer's clock by the smallest difference seen over its first
//! samples (the radio only ever adds delay). The CSV keeps both clocks.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::settings::AppSettings;
use crate::{AppWindow, AxisTick, ChartData, LegendEntry, LogVarRow, PlotBridge, PlotLine, PlotUnit, UnitState};
use crate::host;

/// Samples kept per unit: a backstop against a log left running for days
/// (~55 h at 100 Hz), not a view window.
const MAX_SAMPLES: usize = 20_000_000;

/// Vertical breathing room (px) so the data line never touches the top/bottom
/// edge. Applied to both the data path and the Y ticks so values stay aligned.
const Y_MARGIN_PX: f64 = 6.0;

/// The time windows of the toolbar (`PlotBridge.window`), the last one the
/// whole log.
const WINDOWS_MS: [Option<i64>; 4] = [Some(10_000), Some(30_000), Some(60_000), None];

/// A chart's title row and the space between charts, as in plot.slint.
const CHART_TITLE_PX: f64 = 20.0;
const CHART_GAP_PX: f64 = 12.0;

/// How many first samples of a unit settle its clock offset.
const OFFSET_SAMPLES: u32 = 20;

/// Colours in `Theme.series`.
const COLORS: usize = 8;

/// Line patterns in one chart, by variable: on and off lengths in pixels.
const DASHES: [&[f64]; 4] = [&[], &[7.0, 4.0], &[2.0, 3.0], &[9.0, 3.0, 2.0, 3.0]];

/// One sample of a unit: every variable it logs, at one time.
#[derive(Debug, Clone)]
pub struct Sample {
    /// On the log's time axis: ms since the log started, from the unit's clock
    pub t_ms: i64,
    /// This computer's clock when it arrived, ms since 1970
    pub system_ms: i64,
    /// The Crazyflie's own timestamp, ms since it booted
    pub platform_ms: u32,
    /// In the order of `UnitLog::variables`; NaN when missing
    pub values: Vec<f64>,
}

/// What one unit has logged.
#[derive(Debug, Clone)]
pub struct UnitLog {
    pub unit: String,
    pub variables: Vec<String>,
    pub samples: Vec<Sample>,
    /// Platform time to log time, settled over the first samples
    offset_ms: Option<i64>,
    offset_samples: u32,
    last_platform_ms: Option<u32>,
}

impl UnitLog {
    pub fn new(unit: &str, variables: Vec<String>) -> Self {
        UnitLog { unit: unit.to_string(), variables, samples: Vec::new(), offset_ms: None, offset_samples: 0, last_platform_ms: None }
    }

    /// Add a sample that arrived `now_ms` after the log started.
    pub fn push(&mut self, now_ms: i64, system_ms: i64, platform_ms: u32, values: Vec<f64>) {
        // A reboot starts the unit's clock again
        if self.last_platform_ms.is_some_and(|last| platform_ms < last) {
            self.offset_ms = None;
            self.offset_samples = 0;
        }
        self.last_platform_ms = Some(platform_ms);
        let offset = now_ms - platform_ms as i64;
        if self.offset_samples < OFFSET_SAMPLES {
            self.offset_ms = Some(self.offset_ms.map_or(offset, |o| o.min(offset)));
            self.offset_samples += 1;
        }
        let mut t_ms = platform_ms as i64 + self.offset_ms.unwrap_or(offset);
        // The offset only settles downwards: never back in time
        if let Some(last) = self.samples.last() {
            t_ms = t_ms.max(last.t_ms);
        }
        self.samples.push(Sample { t_ms, system_ms, platform_ms, values });
        if self.samples.len() > MAX_SAMPLES {
            let overflow = self.samples.len() - MAX_SAMPLES;
            self.samples.drain(0..overflow);
        }
    }

    /// The samples between `t_min` and `t_max`, and one on each side.
    fn around(&self, t_min: i64, t_max: i64) -> &[Sample] {
        let first = self.samples.partition_point(|s| s.t_ms < t_min).saturating_sub(1);
        let last = (self.samples.partition_point(|s| s.t_ms <= t_max) + 1).min(self.samples.len());
        &self.samples[first..last.max(first)]
    }

    /// Variable `k` at `t_ms`, between the samples on each side of it.
    fn value_at(&self, k: usize, t_ms: f64) -> Option<f64> {
        let s = &self.samples;
        let pos = s.partition_point(|p| (p.t_ms as f64) < t_ms);
        let v = if pos == 0 {
            s.first()?.values[k]
        } else if pos >= s.len() {
            s.last()?.values[k]
        } else {
            let (a, b) = (&s[pos - 1], &s[pos]);
            let f = ((t_ms - a.t_ms as f64) / (b.t_ms - a.t_ms).max(1) as f64).clamp(0.0, 1.0);
            a.values[k] + f * (b.values[k] - a.values[k])
        };
        v.is_finite().then_some(v)
    }
}

/// Plot view / zoom state. X in ms on the log's time axis; Y as a 0..1
/// fraction of the value range shown. When `user_view` is false the plot
/// follows the newest samples over the chosen time window on every rebuild;
/// once the user pans or zooms, the values stick.
#[derive(Debug, Clone)]
pub struct PlotView {
    pub x_min_ms: i64,
    pub x_max_ms: i64,
    pub y_min_frac: f32,
    pub y_max_frac: f32,
    pub pan_start: Option<(i64, i64, f32, f32)>,
    pub user_view: bool,
    pub data_t_min_ms: i64,
    pub data_t_max_ms: i64,
    pub nav_drag_start: Option<(i64, i64, f32)>,
}

impl Default for PlotView {
    fn default() -> Self {
        Self {
            x_min_ms: 0,
            x_max_ms: 0,
            y_min_frac: 0.0,
            y_max_frac: 1.0,
            pan_start: None,
            user_view: false,
            data_t_min_ms: 0,
            data_t_max_ms: 0,
            nav_drag_start: None,
        }
    }
}

pub struct PlotState {
    /// The configuration the log was taken with
    pub config: Option<LogConfig>,
    pub units: Vec<UnitLog>,
    /// The configuration picked for the next Start, by name
    pub picked: Option<String>,
    /// The units to log, by name, in the order chosen
    pub chosen: Vec<String>,
    /// The units logging now
    pub running: HashSet<String>,
    /// Logging: Start pressed and not stopped. Units join and leave meanwhile.
    pub on: bool,
    /// Units, and single series (unit, variable), drawn or not. Logging
    /// goes on either way.
    pub hidden_units: HashSet<String>,
    pub hidden: HashSet<(String, String)>,
    /// Each unit's colour, kept for the whole log
    colors: HashMap<String, usize>,
    /// When the log started: `Sample::t_ms` counts from here
    epoch: host::Instant,
    pub view: PlotView,
    /// Last polled size: the plot area's width and the charts' height
    pub px: (f32, f32),
    /// Set when a size-change rebuild is already queued for the next tick.
    pub rebuild_pending: bool,
    /// Set by the data feed; the redraw timer rebuilds and clears it.
    pub dirty: bool,
    /// The series behind the legend entries last drawn: (unit, variable)
    drawn: Vec<(usize, usize)>,
}

impl PlotState {
    pub fn new() -> Self {
        Self {
            config: None,
            units: Vec::new(),
            picked: None,
            chosen: Vec::new(),
            running: HashSet::new(),
            on: false,
            hidden_units: HashSet::new(),
            hidden: HashSet::new(),
            colors: HashMap::new(),
            epoch: host::Instant::now(),
            view: PlotView::default(),
            px: (0.0, 0.0),
            rebuild_pending: false,
            dirty: false,
            drawn: Vec::new(),
        }
    }

    /// The colour of a unit, given the first time it is asked for.
    pub fn color_of(&mut self, unit: &str) -> usize {
        let next = self.colors.len() % COLORS;
        *self.colors.entry(unit.to_string()).or_insert(next)
    }

    /// Start a new, empty log.
    pub fn clear_log(&mut self) {
        self.units.clear();
        self.config = None;
        self.hidden.clear();
        self.colors.clear();
        for unit in self.chosen.clone() {
            self.color_of(&unit);
        }
        self.epoch = host::Instant::now();
        self.view = PlotView::default();
        self.dirty = true;
    }

    fn now_ms(&self) -> i64 {
        self.epoch.elapsed().as_millis() as i64
    }

    fn series_shown(&self, u: usize, k: usize) -> bool {
        let log = &self.units[u];
        !self.hidden_units.contains(&log.unit) && !self.hidden.contains(&(log.unit.clone(), log.variables[k].clone()))
    }

    /// How long the log is, in ms.
    fn span_ms(&self) -> i64 {
        let first = self.units.iter().filter_map(|l| l.samples.first()).map(|s| s.t_ms).min();
        let last = self.units.iter().filter_map(|l| l.samples.last()).map(|s| s.t_ms).max();
        first.zip(last).map_or(0, |(a, b)| b - a)
    }
}

pub(crate) type Shared = Arc<Mutex<PlotState>>;

/// Wire up the view callbacks (pan, zoom, hover, ...) and start the redraw
/// timer. Returns the shared state for `wire_logging`.
pub fn setup(ui: &AppWindow) -> Shared {
    let state: Shared = Arc::new(Mutex::new(PlotState::new()));
    let bridge = ui.global::<PlotBridge>();

    macro_rules! handler {
        (|$ui:ident, $st:ident $(, $arg:ident)*| $body:block) => {{
            let st = state.clone();
            let weak = ui.as_weak();
            move |$($arg),*| {
                let st = st.clone();
                if let Some($ui) = weak.upgrade() {
                    let $st = &st;
                    $body
                }
            }
        }};
    }

    bridge.on_pan_start({
        let st = state.clone();
        move || plot_pan_start(&st)
    });
    bridge.on_pan_move(handler!(|ui, st, dx, dy| { plot_pan_move(&ui, st, dx, dy); }));
    bridge.on_zoom(handler!(|ui, st, x, y, d| { plot_zoom(&ui, st, x, y, d); }));
    bridge.on_reset(handler!(|ui, st| { plot_reset(&ui, st); }));
    bridge.on_hover(handler!(|ui, st, f| { plot_hover(&ui, st, f); }));
    bridge.on_size_changed(handler!(|ui, st, w, h| { plot_size_changed(&ui, st, w, h); }));
    bridge.on_nav_press(handler!(|ui, st, f| { plot_nav_press(&ui, st, f); }));
    bridge.on_nav_drag(handler!(|ui, st, f| { plot_nav_drag(&ui, st, f); }));
    bridge.on_toggle_series(handler!(|ui, st, idx| { plot_toggle_series(&ui, st, idx); }));

    // Redraw when the data feed marked the plot dirty; once a second, the
    // unit list and the log's summary.
    {
        let st = state.clone();
        let weak = ui.as_weak();
        let timer = slint::Timer::default();
        let mut tick = 0u32;
        timer.start(slint::TimerMode::Repeated, Duration::from_millis(150), move || {
            let Some(ui) = weak.upgrade() else { return };
            let dirty = std::mem::take(&mut st.lock().unwrap().dirty);
            if dirty {
                rebuild_plot(&ui, &st);
            }
            tick = tick.wrapping_add(1);
            if tick % 7 == 0 {
                refresh_units(&ui, &st);
            }
        });
        // Keep the timer alive for the lifetime of the app.
        Box::leak(Box::new(timer));
    }

    state
}

/// For screenshots (`SWARMKEEPER_PLOT`): 92 s of three units flying, a
/// circle, a figure eight and steps up, logged every 20 ms.
pub fn seed_screenshot(ui: &AppWindow, state: &Shared) {
    let config = LogConfig {
        name: "Position".into(),
        description: None,
        variables: vec!["stateEstimate.x".into(), "stateEstimate.y".into(), "stateEstimate.z".into()],
        period_ms: 20,
    };
    {
        let mut s = state.lock().unwrap();
        s.chosen = vec!["CF-03".into(), "CF-07".into(), "CF-12".into()];
        for unit in s.chosen.clone() {
            s.color_of(&unit);
        }
        for (u, unit) in s.chosen.clone().iter().enumerate() {
            let mut log = UnitLog::new(unit, config.variables.clone());
            for k in 0..4620i64 {
                let t = k as f64 * 0.02;
                let up = ((t - 2.0) / 3.0).clamp(0.0, 1.0);
                let m = (t - 5.0).max(0.0);
                let noise = 0.006 * (t * 41.0 + u as f64).sin();
                let (x, y, z) = match u {
                    0 => (-0.5 + 0.6 * (0.5 * m).cos(), 0.6 * (0.5 * m).sin(), up),
                    1 => (0.8 * (0.35 * m).sin(), 0.4 * (0.7 * m).sin(), 1.3 * up),
                    _ => {
                        let step = 0.6 + 0.4 / (1.0 + (-(t - 32.0) * 1.5).exp()) + 0.4 / (1.0 + (-(t - 62.0) * 1.5).exp());
                        (1.0 + 0.05 * (0.4 * t).sin(), -0.5, step * up)
                    }
                };
                // Each unit booted at another time, and the radio adds a few ms
                let platform = 31_000 + 9_000 * u as u32 + (k * 20) as u32;
                let arrived = k * 20 + 4 + (k % 3);
                log.push(arrived, 1_793_300_000_000 + arrived, platform, vec![x + noise, y + noise, z + noise * 0.5]);
            }
            s.units.push(log);
        }
        s.running = s.chosen.iter().cloned().collect();
        s.config = Some(config.clone());
        s.picked = Some(config.name.clone());
        s.on = true;
        s.dirty = true;
    }
    let b = ui.global::<PlotBridge>();
    b.set_config_names(ModelRc::new(VecModel::from(vec![SharedString::from("Attitude"), SharedString::from("Battery"), SharedString::from("Position")])));
    b.set_config_index(2);
    show_config(&b, Some(&config));
    b.set_logging_active(true);
    b.set_status_text("".into());
    b.set_dock_visible(true);
    refresh_units(ui, state);
}

fn clear_plot(b: &PlotBridge) {
    b.set_charts(ModelRc::new(VecModel::from(Vec::<ChartData>::new())));
    b.set_x_ticks(ModelRc::new(VecModel::from(Vec::<AxisTick>::new())));
    b.set_has_view(false);
    b.set_hover_active(false);
    b.set_nav_min_frac(0.0);
    b.set_nav_max_frac(1.0);
}

/// Build the charts: one per variable with a line per unit, or one with
/// every line. Holds the state lock throughout.
pub fn rebuild_plot(window: &AppWindow, state: &Shared) {
    let b = window.global::<PlotBridge>();
    let split = b.get_split();
    let window_ms = WINDOWS_MS.get(b.get_window() as usize).copied().flatten();
    let hover_refresh;
    {
        let mut st = state.lock().unwrap();

        // Every series with data: (unit, variable)
        let all: Vec<(usize, usize)> = st
            .units
            .iter()
            .enumerate()
            .filter(|(_, log)| log.samples.len() >= 2)
            .flat_map(|(u, log)| (0..log.variables.len()).map(move |k| (u, k)))
            .collect();
        if all.is_empty() {
            st.view = PlotView::default();
            st.drawn.clear();
            drop(st);
            clear_plot(&b);
            return;
        }
        let shown: Vec<(usize, usize)> = all.iter().copied().filter(|&(u, k)| st.series_shown(u, k)).collect();

        // The time axis: the drawn series, or all when everything is hidden
        let range_src = if shown.is_empty() { &all } else { &shown };
        let data_t_min = range_src.iter().map(|&(u, _)| st.units[u].samples[0].t_ms).min().unwrap();
        let data_t_max = range_src.iter().map(|&(u, _)| st.units[u].samples.last().unwrap().t_ms).max().unwrap();
        let mut view = st.view.clone();
        if !view.user_view || view.x_max_ms <= view.x_min_ms {
            view.x_max_ms = data_t_max;
            view.x_min_ms = window_ms.map_or(data_t_min, |w| data_t_min.max(data_t_max - w));
            view.y_min_frac = 0.0;
            view.y_max_frac = 1.0;
        }
        if view.x_max_ms - view.x_min_ms < 1 {
            view.x_max_ms = view.x_min_ms + 1;
        }
        if view.y_max_frac <= view.y_min_frac {
            view.y_min_frac = 0.0;
            view.y_max_frac = 1.0;
        }
        view.data_t_min_ms = data_t_min;
        view.data_t_max_ms = data_t_max;
        st.view = view.clone();

        // The charts and their series: by variable, in the configuration's
        // order, or all in one
        let mut names: Vec<String> = st.config.as_ref().map(|c| c.variables.clone()).unwrap_or_default();
        for &(u, k) in &all {
            if !names.contains(&st.units[u].variables[k]) {
                names.push(st.units[u].variables[k].clone());
            }
        }
        let groups: Vec<(String, Vec<(usize, usize)>)> = if split {
            names
                .iter()
                .map(|n| (n.clone(), all.iter().copied().filter(|&(u, k)| &st.units[u].variables[k] == n).collect::<Vec<_>>()))
                .filter(|(_, series)| !series.is_empty())
                .collect()
        } else {
            vec![(String::new(), all.clone())]
        };
        let units_logged = all.iter().map(|&(u, _)| u).collect::<HashSet<_>>().len();

        let (w_px, h_px) = st.px;
        let plot_w = (w_px as f64).max(1.0);
        let n = groups.len() as f64;
        let titles = if split { n } else { 0.0 };
        let chart_h = ((h_px as f64 - titles * CHART_TITLE_PX - (n - 1.0) * CHART_GAP_PX) / n).max(10.0);
        let y_margin_frac = (Y_MARGIN_PX / chart_h).min(0.4);
        let view_dt = (view.x_max_ms - view.x_min_ms).max(1) as f64;
        let y_view_min = view.y_min_frac as f64;
        let y_view_span = (view.y_max_frac - view.y_min_frac).max(1e-6) as f64;
        let own_scales = !split && b.get_normalized();

        let mut drawn = Vec::new();
        let mut charts = Vec::with_capacity(groups.len());
        for (title, series) in &groups {
            let visible: Vec<(usize, usize)> = series.iter().copied().filter(|&(u, k)| st.series_shown(u, k)).collect();
            let range_of = |set: &[(usize, usize)]| {
                set.iter()
                    .filter_map(|&(u, k)| visible_range(&st.units[u], k, view.x_min_ms, view.x_max_ms))
                    .reduce(|(a, b), (c, d)| (a.min(c), b.max(d)))
                    .map(padded)
            };
            let shared_range = if own_scales && visible.len() > 1 { None } else { range_of(&visible) };
            // Colour: the unit's, and a pattern per variable when one chart
            // shows several units; one unit in one chart: a colour per variable
            let style = |u: usize, k: usize| -> (i32, i32) {
                let color = st.colors.get(&st.units[u].unit).copied().unwrap_or(u % COLORS);
                if split {
                    (color as i32, 0)
                } else if units_logged == 1 {
                    ((k % COLORS) as i32, 0)
                } else {
                    (color as i32, (k % DASHES.len()) as i32)
                }
            };

            let mut lines = Vec::with_capacity(visible.len());
            for &(u, k) in &visible {
                let log = &st.units[u];
                let (lo, hi) = shared_range.or_else(|| range_of(&[(u, k)])).unwrap_or((0.0, 1.0));
                let dy = (hi - lo).max(1e-9);
                // The samples in the view and one on each side, so the line
                // runs to the edges; at most about two per pixel
                let shown_samples = log.around(view.x_min_ms, view.x_max_ms);
                let stride = (shown_samples.len() / (2.0 * plot_w) as usize).max(1);
                let mut points: Vec<Option<(f64, f64)>> = Vec::with_capacity(shown_samples.len() / stride + 2);
                let mut last_index = 0;
                for (i, s) in shown_samples.iter().enumerate().step_by(stride) {
                    points.push(point(s, k, view.x_min_ms, view_dt, lo, dy, y_view_min, y_view_span, y_margin_frac, plot_w, chart_h));
                    last_index = i;
                }
                if last_index + 1 < shown_samples.len() {
                    let s = shown_samples.last().unwrap();
                    points.push(point(s, k, view.x_min_ms, view_dt, lo, dy, y_view_min, y_view_span, y_margin_frac, plot_w, chart_h));
                }
                let (color, dash) = style(u, k);
                lines.push(PlotLine { commands: path_commands(&points, DASHES[dash as usize]).into(), color_index: color });
            }

            let mut y_ticks = match shared_range {
                Some((lo, hi)) => {
                    let dy = (hi - lo).max(1e-9);
                    // About one tick per 40 px, so short charts in the dock stay legible
                    let target = ((chart_h / 40.0) as usize).clamp(2, 5);
                    build_y_ticks_real(lo + (1.0 - view.y_max_frac as f64) * dy, lo + (1.0 - view.y_min_frac as f64) * dy, target)
                }
                None if visible.is_empty() => Vec::new(),
                None => build_y_ticks_normalized(view.y_min_frac, view.y_max_frac),
            };
            for t in y_ticks.iter_mut() {
                t.pos = y_margin_frac as f32 + t.pos * (1.0 - 2.0 * y_margin_frac as f32);
            }

            // The legend: every series with data, hidden ones too so they
            // can be shown again. Grouped by unit in one chart of several.
            let mut legend = Vec::new();
            let mut last_unit = None;
            for &(u, k) in series {
                let log = &st.units[u];
                if !split && units_logged > 1 && last_unit != Some(u) {
                    let (color, _) = style(u, 0);
                    legend.push(LegendEntry { head: true, label: log.unit.as_str().into(), color_index: color, slot_index: -1, enabled: !st.hidden_units.contains(&log.unit), ..Default::default() });
                    last_unit = Some(u);
                }
                let (color, dash) = style(u, k);
                let latest = log.samples.iter().rev().map(|s| s.values[k]).find(|v| v.is_finite());
                legend.push(LegendEntry {
                    head: false,
                    label: if split { log.unit.as_str().into() } else { log.variables[k].as_str().into() },
                    color_index: color,
                    dash,
                    latest: latest.map(format_value).unwrap_or_default().into(),
                    hover_value: SharedString::new(),
                    slot_index: drawn.len() as i32,
                    enabled: st.series_shown(u, k),
                });
                drawn.push((u, k));
            }

            charts.push(ChartData {
                title: title.as_str().into(),
                lines: ModelRc::new(VecModel::from(lines)),
                y_ticks: ModelRc::new(VecModel::from(y_ticks)),
                legend: ModelRc::new(VecModel::from(legend)),
                w_px: plot_w as f32,
                h_px: chart_h as f32,
            });
        }
        st.drawn = drawn;

        let x_ticks = build_x_ticks(view.x_min_ms, view.x_max_ms, data_t_min, 7);
        let data_span = (data_t_max - data_t_min).max(1) as f64;
        b.set_charts(ModelRc::new(VecModel::from(charts)));
        b.set_x_ticks(ModelRc::new(VecModel::from(x_ticks)));
        b.set_has_view(view.user_view);
        b.set_nav_min_frac(((view.x_min_ms - data_t_min) as f64 / data_span).clamp(0.0, 1.0) as f32);
        b.set_nav_max_frac(((view.x_max_ms - data_t_min) as f64 / data_span).clamp(0.0, 1.0) as f32);
        hover_refresh = b.get_hover_active();
    }

    if hover_refresh {
        plot_hover(window, state, b.get_hover_x());
    }
}

/// Where sample `s`'s variable `k` goes in a chart, in pixels; None when it
/// is missing or far outside, which lifts the pen.
#[allow(clippy::too_many_arguments)]
fn point(s: &Sample, k: usize, x_min: i64, view_dt: f64, lo: f64, dy: f64, y_view_min: f64, y_view_span: f64, margin: f64, w: f64, h: f64) -> Option<(f64, f64)> {
    let v = s.values[k];
    if !v.is_finite() {
        return None;
    }
    let y_norm = ((1.0 - (v - lo) / dy) - y_view_min) / y_view_span;
    if !(-0.5..=1.5).contains(&y_norm) {
        return None;
    }
    Some(((s.t_ms - x_min) as f64 / view_dt * w, (margin + y_norm * (1.0 - 2.0 * margin)) * h))
}

/// SVG path commands through `points` (None lifts the pen), drawn in the
/// on/off pattern `dash` (empty: a solid line).
fn path_commands(points: &[Option<(f64, f64)>], dash: &[f64]) -> String {
    let mut out = String::with_capacity(points.len() * 16);
    let mut prev: Option<(f64, f64)> = None;
    // Where in the pattern the pen is, and whether it draws there
    let (mut part, mut left) = (0, dash.first().copied().unwrap_or(f64::INFINITY));
    let mut pen_down = false;
    for p in points {
        let Some((x, y)) = *p else {
            prev = None;
            pen_down = false;
            continue;
        };
        let Some((px, py)) = prev else {
            if part % 2 == 0 {
                let _ = write!(out, "M {x:.2} {y:.2} ");
                pen_down = true;
            }
            prev = Some((x, y));
            continue;
        };
        let (dx, dy) = (x - px, y - py);
        let length = (dx * dx + dy * dy).sqrt();
        let mut done = 0.0;
        while length - done > left {
            done += left;
            let (cx, cy) = (px + dx * done / length, py + dy * done / length);
            if part % 2 == 0 {
                let _ = write!(out, "L {cx:.2} {cy:.2} ");
                pen_down = false;
            } else {
                let _ = write!(out, "M {cx:.2} {cy:.2} ");
                pen_down = true;
            }
            part = (part + 1) % dash.len().max(1);
            left = dash.get(part).copied().unwrap_or(f64::INFINITY);
        }
        left -= length - done;
        if pen_down {
            let _ = write!(out, "L {x:.2} {y:.2} ");
        }
        prev = Some((x, y));
    }
    out
}

/// The lowest and highest value of variable `k` between `t_min` and `t_max`.
fn visible_range(log: &UnitLog, k: usize, t_min: i64, t_max: i64) -> Option<(f64, f64)> {
    let first = log.samples.partition_point(|s| s.t_ms < t_min);
    let last = log.samples.partition_point(|s| s.t_ms <= t_max);
    let shown = &log.samples[first..last.max(first)];
    let values = shown.iter().map(|s| s.values[k]).filter(|v| v.is_finite());
    let (lo, hi) = values.fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| (lo.min(v), hi.max(v)));
    (lo.is_finite() && hi.is_finite()).then_some((lo, hi))
}

/// A value range with room above and below, and some height when the value
/// doesn't change, so a flat line sits in the middle.
fn padded((lo, hi): (f64, f64)) -> (f64, f64) {
    let span = hi - lo;
    if span > 1e-9 {
        (lo - span * 0.05, hi + span * 0.05)
    } else {
        let half = (lo.abs() * 0.05).max(0.5);
        (lo - half, hi + half)
    }
}

/// A value for the legend: about four significant digits.
fn format_value(v: f64) -> String {
    let a = v.abs();
    let dp = if a >= 1000.0 { 0 } else if a >= 100.0 { 1 } else if a >= 10.0 { 2 } else { 3 };
    format!("{v:.dp$}").replace('-', "\u{2212}")
}

/// Smallest "nice" step in {1,2,5}×10ⁿ giving at most `target_ticks` intervals.
fn nice_step(span: f64, target_ticks: usize) -> f64 {
    if !(span > 0.0) || target_ticks == 0 {
        return 1.0;
    }
    let raw = span / target_ticks as f64;
    let mag = 10f64.powf(raw.log10().floor());
    let n = raw / mag;
    let nice = if n < 1.5 {
        1.0
    } else if n < 3.0 {
        2.0
    } else if n < 7.0 {
        5.0
    } else {
        10.0
    };
    nice * mag
}

fn decimals_for_step(step: f64) -> usize {
    let step = step.abs();
    if step >= 1.0 {
        0
    } else if step >= 0.1 {
        1
    } else if step >= 0.01 {
        2
    } else if step >= 0.001 {
        3
    } else {
        4
    }
}

/// Seconds since the log started: "0.50s", "12s", "1:05".
fn format_time_offset(s: f64) -> String {
    let abs = s.abs();
    if abs >= 60.0 {
        let total = s.round() as i64;
        let m = total / 60;
        let sec = (total % 60).abs();
        format!("{m}:{sec:02}")
    } else if abs >= 10.0 {
        format!("{s:.0}s")
    } else if abs >= 1.0 {
        format!("{s:.1}s")
    } else {
        format!("{s:.2}s")
    }
}

fn build_x_ticks(view_min_ms: i64, view_max_ms: i64, ref_ms: i64, target: usize) -> Vec<AxisTick> {
    let span_s = (view_max_ms - view_min_ms) as f64 / 1000.0;
    if !(span_s > 0.0) {
        return Vec::new();
    }
    let step_s = nice_step(span_s, target);
    let view_min_s_rel = (view_min_ms - ref_ms) as f64 / 1000.0;
    let view_max_s_rel = (view_max_ms - ref_ms) as f64 / 1000.0;
    let first = (view_min_s_rel / step_s).ceil() * step_s;
    let mut ticks = Vec::new();
    let mut t = first;
    let dt = (view_max_ms - view_min_ms).max(1) as f64 / 1000.0;
    let mut guard = 0;
    while t <= view_max_s_rel + step_s * 1e-6 && guard < 64 {
        let pos = ((t - view_min_s_rel) / dt) as f32;
        if (0.0..=1.0).contains(&pos) {
            ticks.push(AxisTick { pos, label: SharedString::from(format_time_offset(t)) });
        }
        t += step_s;
        guard += 1;
    }
    ticks
}

fn build_y_ticks_real(y_min: f64, y_max: f64, target: usize) -> Vec<AxisTick> {
    let span = y_max - y_min;
    if !(span > 0.0) {
        return Vec::new();
    }
    let step = nice_step(span, target);
    let dp = decimals_for_step(step);
    let first = (y_min / step).ceil() * step;
    let mut ticks = Vec::new();
    let mut v = first;
    let mut guard = 0;
    while v <= y_max + step * 1e-6 && guard < 64 {
        let pos = (1.0 - (v - y_min) / span) as f32;
        if (0.0..=1.0).contains(&pos) {
            // No "−0" from a rounding error
            let v = if v.abs() < step * 1e-6 { 0.0 } else { v };
            let label = format!("{v:.dp$}").replace('-', "\u{2212}");
            ticks.push(AxisTick { pos, label: label.into() });
        }
        v += step;
        guard += 1;
    }
    ticks
}

fn build_y_ticks_normalized(y_view_min: f32, y_view_max: f32) -> Vec<AxisTick> {
    (0..=4)
        .map(|i| {
            let viewport_frac = i as f32 / 4.0;
            let pct = (1.0 - (y_view_min + viewport_frac * (y_view_max - y_view_min))) * 100.0;
            AxisTick { pos: viewport_frac, label: format!("{pct:.0} %").into() }
        })
        .collect()
}

fn plot_pan_start(state: &Shared) {
    let mut s = state.lock().unwrap();
    let v = &mut s.view;
    v.pan_start = Some((v.x_min_ms, v.x_max_ms, v.y_min_frac, v.y_max_frac));
}

fn plot_pan_move(window: &AppWindow, state: &Shared, dx_frac: f32, dy_frac: f32) {
    {
        let mut s = state.lock().unwrap();
        let v = &mut s.view;
        let Some((s_xmin, s_xmax, s_ymin, s_ymax)) = v.pan_start else {
            return;
        };
        let span_x = (s_xmax - s_xmin) as f64;
        let delta_ms = (-dx_frac as f64 * span_x) as i64;
        v.x_min_ms = s_xmin + delta_ms;
        v.x_max_ms = s_xmax + delta_ms;
        let span_y = (s_ymax - s_ymin) as f64;
        let delta_y = -dy_frac as f64 * span_y;
        let new_ymin = (s_ymin as f64 + delta_y).clamp(0.0, 1.0 - span_y);
        v.y_min_frac = new_ymin as f32;
        v.y_max_frac = (new_ymin + span_y).clamp(0.0, 1.0) as f32;
        v.user_view = true;
    }
    rebuild_plot(window, state);
}

fn plot_zoom(window: &AppWindow, state: &Shared, cursor_x_frac: f32, _cursor_y_frac: f32, delta_y_px: f32) {
    {
        let mut s = state.lock().unwrap();
        let v = &mut s.view;
        let zoom = (delta_y_px as f64 / 120.0).clamp(-5.0, 5.0);
        let factor = 1.20_f64.powf(zoom);
        let span_x = (v.x_max_ms - v.x_min_ms).max(1) as f64;
        let new_span_x = (span_x / factor).max(20.0).min(span_x * 10.0);
        let cursor_ms = v.x_min_ms as f64 + cursor_x_frac as f64 * span_x;
        v.x_min_ms = (cursor_ms - cursor_x_frac as f64 * new_span_x) as i64;
        v.x_max_ms = v.x_min_ms + new_span_x as i64;
        v.user_view = true;
        v.pan_start = None;
    }
    rebuild_plot(window, state);
}

/// Back to following the newest samples.
fn plot_reset(window: &AppWindow, state: &Shared) {
    state.lock().unwrap().view = PlotView::default();
    rebuild_plot(window, state);
}

/// Show or hide one series from the legend; it keeps logging.
fn plot_toggle_series(window: &AppWindow, state: &Shared, slot_index: i32) {
    {
        let mut s = state.lock().unwrap();
        let Some(&(u, k)) = s.drawn.get(slot_index as usize) else { return };
        let key = (s.units[u].unit.clone(), s.units[u].variables[k].clone());
        if !s.hidden.remove(&key) {
            s.hidden.insert(key);
        }
    }
    rebuild_plot(window, state);
}

fn plot_nav_press(window: &AppWindow, state: &Shared, frac: f32) {
    {
        let mut s = state.lock().unwrap();
        let v = &mut s.view;
        let data_span = (v.data_t_max_ms - v.data_t_min_ms).max(1) as f64;
        if data_span <= 1.0 {
            return;
        }
        let view_span = (v.x_max_ms - v.x_min_ms).max(1) as f64;
        let view_min_frac = (v.x_min_ms - v.data_t_min_ms) as f64 / data_span;
        let view_max_frac = (v.x_max_ms - v.data_t_min_ms) as f64 / data_span;
        let inside_thumb = (frac as f64) >= view_min_frac && (frac as f64) <= view_max_frac;
        if !inside_thumb {
            let target_center_ms = v.data_t_min_ms as f64 + frac as f64 * data_span;
            v.x_min_ms = (target_center_ms - view_span / 2.0) as i64;
            v.x_max_ms = v.x_min_ms + view_span as i64;
            v.user_view = true;
        }
        v.nav_drag_start = Some((v.x_min_ms, v.x_max_ms, frac));
    }
    rebuild_plot(window, state);
}

fn plot_nav_drag(window: &AppWindow, state: &Shared, frac: f32) {
    {
        let mut s = state.lock().unwrap();
        let v = &mut s.view;
        let Some((s_min, s_max, s_frac)) = v.nav_drag_start else {
            return;
        };
        let data_span = (v.data_t_max_ms - v.data_t_min_ms).max(1) as f64;
        let delta_ms = ((frac - s_frac) as f64 * data_span) as i64;
        v.x_min_ms = s_min + delta_ms;
        v.x_max_ms = s_max + delta_ms;
        v.user_view = true;
    }
    rebuild_plot(window, state);
}

/// Slint reports a new size: the plot area's width and the charts' height.
/// Cache it and rebuild on the next event-loop tick — not synchronously from
/// inside the callback that fired during property evaluation.
fn plot_size_changed(window: &AppWindow, state: &Shared, w: f32, h: f32) {
    let should_schedule = {
        let mut s = state.lock().unwrap();
        let old = s.px;
        if (old.0 - w).abs() <= 0.5 && (old.1 - h).abs() <= 0.5 {
            return;
        }
        s.px = (w, h);
        !std::mem::replace(&mut s.rebuild_pending, true)
    };
    if !should_schedule {
        return;
    }
    let weak = window.as_weak();
    let st = state.clone();
    slint::Timer::single_shot(Duration::ZERO, move || {
        st.lock().unwrap().rebuild_pending = false;
        if let Some(window) = weak.upgrade() {
            rebuild_plot(&window, &st);
        }
    });
}

/// The crosshair and each series' value under it, in the legends. `frac`
/// outside 0..1 clears them.
fn plot_hover(window: &AppWindow, state: &Shared, frac: f32) {
    let b = window.global::<PlotBridge>();
    let hovering = (0.0..=1.0).contains(&frac);
    let (time, values) = {
        let s = state.lock().unwrap();
        let v = &s.view;
        let cursor_ms = v.x_min_ms as f64 + frac as f64 * (v.x_max_ms - v.x_min_ms).max(1) as f64;
        let time = format_time_offset((cursor_ms - v.data_t_min_ms as f64) / 1000.0);
        let values: Vec<Option<f64>> = if hovering {
            s.drawn.iter().map(|&(u, k)| s.units[u].value_at(k, cursor_ms)).collect()
        } else {
            Vec::new()
        };
        (time, values)
    };
    let charts: Vec<ChartData> = b
        .get_charts()
        .iter()
        .map(|mut chart| {
            let legend: Vec<LegendEntry> = chart
                .legend
                .iter()
                .map(|mut e| {
                    e.hover_value = values
                        .get(e.slot_index.max(0) as usize)
                        .copied()
                        .flatten()
                        .filter(|_| e.slot_index >= 0)
                        .map(format_value)
                        .unwrap_or_default()
                        .into();
                    e
                })
                .collect();
            chart.legend = ModelRc::new(VecModel::from(legend));
            chart
        })
        .collect();
    b.set_charts(ModelRc::new(VecModel::from(charts)));
    b.set_hover_x(frac.clamp(0.0, 1.0));
    b.set_hover_time(time.into());
    b.set_hover_active(hovering);
}

// ===========================================================================
// Log configurations, kept in the settings, and logging the units.
// ===========================================================================

/// A named set of log variables to stream and plot, kept in the settings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LogConfig {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Firmware log variable names ("group.name").
    pub variables: Vec<String>,
    /// Sample period in milliseconds (clamped to 10..=2550 when started).
    #[serde(default = "default_period_ms")]
    pub period_ms: u64,
}

fn default_period_ms() -> u64 {
    100
}

/// Where log configurations were kept before the settings had them, relative
/// to the data folder like `swarms/` and `scenes/`.
fn old_configs_dir() -> PathBuf {
    PathBuf::from("log_configs")
}

/// The saved log configurations, by name.
pub fn list_log_configs() -> Vec<LogConfig> {
    let settings: AppSettings = host::store::load();
    let mut configs = settings.log_configs;
    configs.sort_by_key(|c| c.name.to_lowercase());
    configs
}

/// Change the saved log configurations.
fn store_configs(change: impl FnOnce(&mut Vec<LogConfig>)) -> Result<(), String> {
    let mut settings: AppSettings = host::store::load();
    change(&mut settings.log_configs);
    host::store::save(&settings).map_err(|e| e.to_string())
}

/// Copy the configurations kept as files in `log_configs/` into the
/// settings, once: while the settings have none.
fn import_config_files() {
    let settings: AppSettings = host::store::load();
    if !settings.log_configs.is_empty() {
        return;
    }
    let Ok(dir) = host::fs::read_dir(old_configs_dir()) else { return };
    let found: Vec<LogConfig> = dir
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|e| e == "yaml" || e == "yml"))
        .filter_map(|path| serde_yaml::from_str(&host::fs::read_to_string(&path).ok()?).ok())
        .collect();
    if !found.is_empty() {
        eprintln!("Log configurations: {} copied from {} into the settings", found.len(), old_configs_dir().display());
        if let Err(e) = store_configs(|configs| *configs = found) {
            eprintln!("Saving the log configurations: {e}");
        }
    }
}

/// Show a configuration's variables and period beside the picker.
fn show_config(b: &PlotBridge, config: Option<&LogConfig>) {
    let variables: Vec<SharedString> = config.map(|c| c.variables.iter().map(|v| v.as_str().into()).collect()).unwrap_or_default();
    b.set_config_variables(ModelRc::new(VecModel::from(variables)));
    b.set_config_period(config.map_or(0, |c| c.period_ms as i32));
}

/// The configuration names in the pickers, the picked one marked.
pub fn refresh_config_list(ui: &AppWindow, state: &Shared) {
    let configs = list_log_configs();
    let picked = state.lock().unwrap().picked.clone();
    let b = ui.global::<PlotBridge>();
    let names: Vec<SharedString> = configs.iter().map(|c| c.name.as_str().into()).collect();
    b.set_config_names(ModelRc::new(VecModel::from(names)));
    let index = picked.as_ref().and_then(|p| configs.iter().position(|c| &c.name == p));
    b.set_config_index(index.map_or(-1, |i| i as i32));
    show_config(&b, index.map(|i| &configs[i]));
}

/// The Plot screen's unit list, who is in the log, and how much it holds.
fn refresh_units(ui: &AppWindow, state: &Shared) {
    let units = ui.get_units();
    let status = crate::units::status_by_unit(ui);
    let b = ui.global::<PlotBridge>();
    let st = state.lock().unwrap();
    let in_log: HashSet<&str> = st.units.iter().filter(|l| !l.samples.is_empty()).map(|l| l.unit.as_str()).collect();
    let mut connected = 0;
    let mut chosen_connected = 0;
    let rows: Vec<PlotUnit> = units
        .iter()
        .enumerate()
        .map(|(i, u)| {
            let live = !matches!(u.state, UnitState::Disconnected | UnitState::Error);
            let chosen = st.chosen.iter().any(|c| c.as_str() == u.name.as_str());
            connected += live as i32;
            chosen_connected += (live && chosen) as i32;
            let (health, text) = status.get(i).cloned().unwrap_or((crate::Health::Off, String::new()));
            let logged = in_log.contains(u.name.as_str());
            PlotUnit {
                name: u.name.clone(),
                status: text.into(),
                health,
                connected: live,
                chosen,
                in_log: logged,
                logging: st.running.contains(u.name.as_str()),
                shown: !st.hidden_units.contains(u.name.as_str()),
                color_index: if chosen || logged { st.colors.get(u.name.as_str()).map_or(-1, |&c| c as i32) } else { -1 },
            }
        })
        .collect();
    let changed = b.get_unit_choices().row_count() != rows.len() || b.get_unit_choices().iter().zip(&rows).any(|(a, b)| &a != b);
    if changed {
        b.set_unit_choices(ModelRc::new(VecModel::from(rows)));
    }
    b.set_connected_count(connected);
    b.set_chosen_count(chosen_connected);

    // Who is in the log, and how much of it there is
    let names: Vec<&str> = st.units.iter().filter(|l| !l.samples.is_empty()).map(|l| l.unit.as_str()).collect();
    let who = match names.len() {
        0 => st.chosen.join(", "),
        1..=4 => names.join(", "),
        n => format!("{n} units"),
    };
    let samples: usize = st.units.iter().map(|l| l.samples.len()).sum();
    let span = st.span_ms();
    b.set_logging_unit(who.into());
    b.set_has_data(samples > 0);
    b.set_log_summary(if samples == 0 {
        String::new()
    } else {
        let units = if names.len() == 1 { "1 unit".to_string() } else { format!("{} units", names.len()) };
        format!("{} logged · {} samples · {}", format_time_offset(span as f64 / 1000.0), samples, units)
    }
    .into());
}

/// The CSV of the log: a row per unit and sample, this computer's time and
/// the Crazyflie's, then the variables.
pub fn log_csv(st: &PlotState) -> String {
    let mut variables: Vec<String> = st.config.as_ref().map(|c| c.variables.clone()).unwrap_or_default();
    for log in &st.units {
        for v in &log.variables {
            if !variables.contains(v) {
                variables.push(v.clone());
            }
        }
    }
    let mut out = String::from("system_time,platform_time_ms,unit");
    for v in &variables {
        out.push(',');
        out.push_str(v);
    }
    out.push('\n');
    // Each unit's columns in the log's order
    let columns: Vec<Vec<Option<usize>>> =
        st.units.iter().map(|log| variables.iter().map(|v| log.variables.iter().position(|x| x == v)).collect()).collect();
    let mut rows: Vec<(i64, usize, usize)> =
        st.units.iter().enumerate().flat_map(|(u, log)| log.samples.iter().enumerate().map(move |(i, s)| (s.system_ms, u, i))).collect();
    rows.sort();
    for (system_ms, u, i) in rows {
        let log = &st.units[u];
        let s = &log.samples[i];
        let time = chrono::DateTime::from_timestamp_millis(system_ms)
            .map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%dT%H:%M:%S%.3f%:z").to_string())
            .unwrap_or_default();
        let _ = write!(out, "{},{},{}", time, s.platform_ms, log.unit);
        for column in &columns[u] {
            out.push(',');
            if let Some(v) = column.map(|k| s.values[k]).filter(|v| v.is_finite()) {
                let _ = write!(out, "{v}");
            }
        }
        out.push('\n');
    }
    out
}

fn set_status(weak: &slint::Weak<AppWindow>, text: &str) {
    let weak = weak.clone();
    let text = text.to_string();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            ui.global::<PlotBridge>().set_status_text(text.into());
        }
    });
}

/// Stream one unit's variables into the log until stopped (the task is
/// aborted) or the link ends.
async fn run_unit_log(
    cf: Arc<crazyflie_connection_server::Crazyflie>,
    unit: String,
    config: LogConfig,
    state: Shared,
    tasks: LogTasks,
    weak: slint::Weak<AppWindow>,
) {
    let ended = |text: String| {
        state.lock().unwrap().running.remove(&unit);
        tasks.lock().unwrap().remove(&unit);
        set_status(&weak, &text);
    };
    let toc: HashSet<String> = cf.log.names().into_iter().collect();
    let names: Vec<String> = config.variables.iter().filter(|n| toc.contains(*n)).cloned().collect();
    if names.is_empty() {
        ended(format!("{} has none of {}'s variables in its log TOC.", unit, config.name));
        return;
    }
    let missing = config.variables.len() - names.len();
    {
        let mut st = state.lock().unwrap();
        st.color_of(&unit);
        if !st.units.iter().any(|l| l.unit == unit) {
            st.units.push(UnitLog::new(&unit, names.clone()));
        }
    }
    let period_ms = config.period_ms.clamp(10, 2550);
    let vars: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut stream = match cf.log.subscribe(&vars, Duration::from_millis(period_ms)).await {
        Ok(stream) => stream,
        Err(e) => {
            ended(format!("{}: starting the log failed: {}", unit, e));
            return;
        }
    };
    if missing > 0 {
        set_status(&weak, &format!("{}: {} of the variables are not in its log TOC.", unit, missing));
    }
    while let Ok(data) = stream.next().await {
        let system_ms = chrono::Utc::now().timestamp_millis();
        let mut st = state.lock().unwrap();
        let now_ms = st.now_ms();
        if let Some(log) = st.units.iter_mut().find(|l| l.unit == unit) {
            let values = log.variables.iter().map(|n| data.data.get(n).copied().unwrap_or(f64::NAN)).collect();
            log.push(now_ms, system_ms, data.timestamp, values);
        }
        st.dirty = true;
    }
    ended(format!("{}: the log stream ended.", unit));
}

/// The log task of each unit logging, by name.
pub type LogTasks = Arc<Mutex<HashMap<String, host::JoinHandle<()>>>>;
/// Shared "unit selected for logging" — the unsorted original index, updated by
/// the unit-selection callbacks.
pub type SelectedIndex = Arc<Mutex<Option<usize>>>;

/// What starting and stopping units needs.
#[derive(Clone)]
struct Logging {
    state: Shared,
    swarm_state: crate::swarm::SwarmState,
    tasks: LogTasks,
    weak: slint::Weak<AppWindow>,
}

impl Logging {
    /// Start logging `unit` with the log's configuration, when it is
    /// connected and not logging yet.
    fn start_unit(&self, ui: &AppWindow, unit: &str) {
        let Some(config) = self.state.lock().unwrap().config.clone() else { return };
        let Some(index) = ui.get_units().iter().position(|u| u.name.as_str() == unit) else { return };
        let mut tasks = self.tasks.lock().unwrap();
        if tasks.contains_key(unit) {
            return;
        }
        self.state.lock().unwrap().running.insert(unit.to_string());
        let (cx, name) = (self.clone(), unit.to_string());
        let handle = host::spawn(async move {
            let cf = cx.swarm_state.lock().await.get(&index).map(|cu| cu.cf.clone());
            match cf {
                Some(cf) => run_unit_log(cf, name, config, cx.state, cx.tasks, cx.weak).await,
                None => {
                    cx.state.lock().unwrap().running.remove(&name);
                    cx.tasks.lock().unwrap().remove(&name);
                    set_status(&cx.weak, &format!("{} is not connected.", name));
                }
            }
        });
        tasks.insert(unit.to_string(), handle);
    }

    fn stop_unit(&self, unit: &str) {
        if let Some(handle) = self.tasks.lock().unwrap().remove(unit) {
            handle.abort();
        }
        self.state.lock().unwrap().running.remove(unit);
    }

    /// Start the chosen units with the picked configuration. A configuration
    /// other than the log's starts a new log; the same one goes on.
    fn start(&self, ui: &AppWindow) {
        let b = ui.global::<PlotBridge>();
        let picked = self.state.lock().unwrap().picked.clone();
        let Some(config) = picked.and_then(|p| list_log_configs().into_iter().find(|c| c.name == p)) else {
            b.set_status_text("Choose a log configuration first.".into());
            return;
        };
        let chosen = {
            let mut st = self.state.lock().unwrap();
            if st.config.as_ref() != Some(&config) {
                st.clear_log();
                st.config = Some(config.clone());
            }
            st.on = true;
            st.chosen.clone()
        };
        for unit in &chosen {
            self.start_unit(ui, unit);
        }
        b.set_logging_active(true);
        b.set_status_text("".into());
        refresh_units(ui, &self.state);
    }

    fn stop(&self, ui: &AppWindow) {
        for (_, handle) in self.tasks.lock().unwrap().drain() {
            handle.abort();
        }
        {
            let mut st = self.state.lock().unwrap();
            st.running.clear();
            st.on = false;
        }
        let b = ui.global::<PlotBridge>();
        b.set_logging_active(false);
        b.set_status_text("Stopped.".into());
        refresh_units(ui, &self.state);
    }
}

/// Register the logging callbacks (start, stop, units, configurations, CSV)
/// and fill the pickers. Call after `swarm_state` exists.
pub fn wire_logging(ui: &AppWindow, state: &Shared, swarm_state: crate::swarm::SwarmState, selected: SelectedIndex) {
    import_config_files();
    refresh_config_list(ui, state);
    let bridge = ui.global::<PlotBridge>();
    let toc_picker: SharedToc = Arc::new(Mutex::new(TocPicker::default()));
    let cx = Logging { state: state.clone(), swarm_state: swarm_state.clone(), tasks: Arc::new(Mutex::new(HashMap::new())), weak: ui.as_weak() };

    // The Plot screen's Start: the chosen units
    {
        let cx = cx.clone();
        bridge.on_start_chosen(move || {
            if let Some(ui) = cx.weak.upgrade() {
                cx.start(&ui);
            }
        });
    }

    // The dock's Start: the selected unit only
    {
        let cx = cx.clone();
        let selected = selected.clone();
        bridge.on_start_logging(move || {
            let Some(ui) = cx.weak.upgrade() else { return };
            let unit = selected.lock().unwrap().and_then(|i| ui.get_units().row_data(i)).map(|u| u.name.to_string());
            let Some(unit) = unit else {
                ui.global::<PlotBridge>().set_status_text("Select a unit, then Start.".into());
                return;
            };
            let others: Vec<String> = {
                let mut st = cx.state.lock().unwrap();
                st.chosen = vec![unit.clone()];
                st.color_of(&unit);
                st.running.iter().filter(|u| **u != unit).cloned().collect()
            };
            for other in others {
                cx.stop_unit(&other);
            }
            cx.start(&ui);
        });
    }

    // Stop every unit
    {
        let cx = cx.clone();
        bridge.on_stop_logging(move || {
            if let Some(ui) = cx.weak.upgrade() {
                cx.stop(&ui);
            }
        });
    }

    // Tick a unit: it joins the log, or leaves it, also while logging
    {
        let cx = cx.clone();
        bridge.on_toggle_unit(move |unit| {
            let Some(ui) = cx.weak.upgrade() else { return };
            let (joins, on) = {
                let mut st = cx.state.lock().unwrap();
                let joins = !st.chosen.iter().any(|c| c.as_str() == unit.as_str());
                if joins {
                    st.chosen.push(unit.to_string());
                    st.color_of(&unit);
                } else {
                    st.chosen.retain(|c| c.as_str() != unit.as_str());
                }
                (joins, st.on)
            };
            if on && joins {
                cx.start_unit(&ui, &unit);
            } else if !joins {
                cx.stop_unit(&unit);
            }
            refresh_units(&ui, &cx.state);
        });
    }

    // All connected units, or none
    {
        let cx = cx.clone();
        bridge.on_choose_all(move || {
            let Some(ui) = cx.weak.upgrade() else { return };
            let connected: Vec<String> = ui
                .get_units()
                .iter()
                .filter(|u| !matches!(u.state, UnitState::Disconnected | UnitState::Error))
                .map(|u| u.name.to_string())
                .collect();
            let (joining, on) = {
                let mut st = cx.state.lock().unwrap();
                let joining: Vec<String> = connected.iter().filter(|u| !st.chosen.contains(u)).cloned().collect();
                for unit in &joining {
                    st.chosen.push(unit.clone());
                    st.color_of(unit);
                }
                (joining, st.on)
            };
            if on {
                for unit in &joining {
                    cx.start_unit(&ui, unit);
                }
            }
            refresh_units(&ui, &cx.state);
        });
    }
    {
        let cx = cx.clone();
        bridge.on_choose_none(move || {
            let Some(ui) = cx.weak.upgrade() else { return };
            let chosen = std::mem::take(&mut cx.state.lock().unwrap().chosen);
            for unit in &chosen {
                cx.stop_unit(unit);
            }
            refresh_units(&ui, &cx.state);
        });
    }

    // Show or hide a unit's lines; it keeps logging
    {
        let cx = cx.clone();
        bridge.on_toggle_unit_shown(move |unit| {
            let Some(ui) = cx.weak.upgrade() else { return };
            {
                let mut st = cx.state.lock().unwrap();
                if !st.hidden_units.remove(unit.as_str()) {
                    st.hidden_units.insert(unit.to_string());
                }
            }
            rebuild_plot(&ui, &cx.state);
            refresh_units(&ui, &cx.state);
        });
    }

    // Pick a configuration. Another one than the log's starts a new log, so
    // it can only change while stopped.
    {
        let state = state.clone();
        let weak = ui.as_weak();
        bridge.on_select_config(move |index| {
            let Some(ui) = weak.upgrade() else { return };
            let configs = list_log_configs();
            let config = configs.get(index as usize).cloned();
            {
                let mut st = state.lock().unwrap();
                st.picked = config.as_ref().map(|c| c.name.clone());
                if !st.on && st.config.is_some() && st.config != config {
                    st.clear_log();
                }
            }
            refresh_config_list(&ui, &state);
            refresh_units(&ui, &state);
        });
    }

    // Remove a configuration from the settings. A log taken with it stays.
    {
        let state = state.clone();
        let weak = ui.as_weak();
        bridge.on_delete_config(move |index| {
            let Some(ui) = weak.upgrade() else { return };
            let Some(config) = list_log_configs().into_iter().nth(index as usize) else { return };
            if let Err(e) = store_configs(|configs| configs.retain(|c| c.name != config.name)) {
                ui.global::<PlotBridge>().set_status_text(format!("Deleting {} failed: {}", config.name, e).into());
                return;
            }
            {
                let mut st = state.lock().unwrap();
                if st.picked.as_ref() == Some(&config.name) {
                    st.picked = None;
                }
            }
            refresh_config_list(&ui, &state);
            ui.global::<PlotBridge>().set_status_text(format!("Deleted {}.", config.name).into());
        });
    }

    // Save the log as a CSV file
    {
        let state = state.clone();
        let weak = ui.as_weak();
        bridge.on_save_csv(move || {
            let (csv, name) = {
                let st = state.lock().unwrap();
                let config = st.config.as_ref().map_or("log", |c| c.name.as_str()).to_string();
                (log_csv(&st), format!("{}-{}.csv", config, chrono::Local::now().format("%Y%m%d-%H%M%S")))
            };
            let weak = weak.clone();
            host::spawn(async move {
                let picked = host::dialog::FileDialog::new()
                    .set_title("Save the log")
                    .add_filter("CSV", &["csv"])
                    .set_file_name(name)
                    .save_file()
                    .await;
                let Some(file) = picked else { return };
                let text = match host::fs::write(file.path(), csv) {
                    Ok(()) => format!("Saved {}", file.path().display()),
                    Err(e) => format!("Saving {} failed: {}", file.path().display(), e),
                };
                set_status(&weak, &text);
            });
        });
    }

    // Clear the log, while stopped
    {
        let state = state.clone();
        let weak = ui.as_weak();
        bridge.on_clear_log(move || {
            let Some(ui) = weak.upgrade() else { return };
            {
                let mut st = state.lock().unwrap();
                if st.on {
                    return;
                }
                st.clear_log();
            }
            ui.global::<PlotBridge>().set_status_text("".into());
            rebuild_plot(&ui, &state);
            refresh_units(&ui, &state);
        });
    }

    // "New log configuration": the variables from a connected unit's log
    // TOC — the selected unit, else a chosen one, else any.
    {
        let swarm_state = swarm_state.clone();
        let selected = selected.clone();
        let state = state.clone();
        let picker = toc_picker.clone();
        let weak = ui.as_weak();
        bridge.on_prepare_new_config(move || {
            {
                let mut p = picker.lock().unwrap();
                p.all.clear();
                p.checked.clear();
                p.filter.clear();
            }
            let Some(ui) = weak.upgrade() else { return };
            let b = ui.global::<PlotBridge>();
            b.set_toc_rows(ModelRc::new(VecModel::from(Vec::<LogVarRow>::new())));
            b.set_toc_filter(SharedString::from(""));
            b.set_toc_unit(SharedString::from(""));
            b.set_toc_status(SharedString::from("Loading variables…"));
            let units = ui.get_units();
            let chosen = state.lock().unwrap().chosen.clone();
            let mut candidates: Vec<usize> = selected.lock().unwrap().iter().copied().collect();
            candidates.extend(chosen.iter().filter_map(|c| units.iter().position(|u| u.name.as_str() == c.as_str())));
            let names: Vec<String> = units.iter().map(|u| u.name.to_string()).collect();
            let (swarm_state, picker, weak) = (swarm_state.clone(), picker.clone(), weak.clone());
            host::spawn(async move {
                let found = {
                    let st = swarm_state.lock().await;
                    candidates.iter().find_map(|i| st.get(i).map(|cu| (*i, cu.cf.clone()))).or_else(|| {
                        st.iter().min_by_key(|(i, _)| **i).map(|(i, cu)| (*i, cu.cf.clone()))
                    })
                };
                let Some((index, cf)) = found else {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            ui.global::<PlotBridge>().set_toc_status(SharedString::from("Connect a unit first: the variables come from its log TOC."));
                        }
                    });
                    return;
                };
                let mut all = cf.log.names();
                all.sort();
                picker.lock().unwrap().all = all;
                let unit = names.get(index).cloned().unwrap_or_default();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.global::<PlotBridge>().set_toc_unit(unit.into());
                        push_toc_rows(&ui, &picker);
                    }
                });
            });
        });
    }

    // Tick a variable in the picker.
    {
        let picker = toc_picker.clone();
        let weak = ui.as_weak();
        bridge.on_toggle_toc_var(move |name| {
            {
                let mut p = picker.lock().unwrap();
                let name = name.to_string();
                if !p.checked.remove(&name) {
                    p.checked.insert(name);
                }
            }
            if let Some(ui) = weak.upgrade() {
                push_toc_rows(&ui, &picker);
            }
        });
    }

    // The picker's filter.
    {
        let picker = toc_picker.clone();
        let weak = ui.as_weak();
        bridge.on_toc_filter_changed(move |text| {
            picker.lock().unwrap().filter = text.to_string();
            if let Some(ui) = weak.upgrade() {
                push_toc_rows(&ui, &picker);
            }
        });
    }

    // Save the ticked variables as a configuration in the settings (one of
    // the same name is replaced), pick it and close the dialog. A problem
    // keeps the dialog open with a message.
    {
        let picker = toc_picker.clone();
        let state = state.clone();
        let weak = ui.as_weak();
        bridge.on_save_new_config(move |name, period| {
            let mut variables: Vec<String> = picker.lock().unwrap().checked.iter().cloned().collect();
            variables.sort();
            let Some(ui) = weak.upgrade() else { return };
            let b = ui.global::<PlotBridge>();
            let name = name.trim().to_string();
            if name.is_empty() {
                b.set_toc_status(SharedString::from("Enter a name."));
                return;
            }
            if variables.is_empty() {
                b.set_toc_status(SharedString::from("Tick at least one variable."));
                return;
            }
            let period_ms = if period.is_finite() && (10.0..=2550.0).contains(&period) { period as u64 } else { 100 };
            let config = LogConfig { name: name.clone(), description: None, variables, period_ms };
            if let Err(e) = store_configs(|configs| {
                configs.retain(|c| c.name != config.name);
                configs.push(config);
            }) {
                b.set_toc_status(SharedString::from(format!("Saving failed: {e}")));
                return;
            }
            {
                let mut st = state.lock().unwrap();
                if !st.on {
                    st.picked = Some(name);
                }
            }
            refresh_config_list(&ui, &state);
            b.set_show_new_config(false);
        });
    }
}

/// The "New log configuration" picker: the unit's whole log TOC, the
/// variables ticked, and the filter.
#[derive(Default)]
pub struct TocPicker {
    all: Vec<String>,
    checked: HashSet<String>,
    filter: String,
}

pub type SharedToc = Arc<Mutex<TocPicker>>;

/// Show the filtered TOC rows and how many are ticked (UI thread).
fn push_toc_rows(ui: &AppWindow, picker: &SharedToc) {
    let p = picker.lock().unwrap();
    let filter = p.filter.to_lowercase();
    let rows: Vec<LogVarRow> = p
        .all
        .iter()
        .filter(|n| filter.is_empty() || n.to_lowercase().contains(&filter))
        .map(|n| LogVarRow { name: n.as_str().into(), checked: p.checked.contains(n) })
        .collect();
    let status = if p.all.is_empty() {
        "No variables".to_string()
    } else {
        format!("{} shown · {} ticked", rows.len(), p.checked.len())
    };
    let b = ui.global::<PlotBridge>();
    b.set_toc_rows(ModelRc::new(VecModel::from(rows)));
    b.set_toc_status(SharedString::from(status));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log_of(values: &[(i64, f64)]) -> UnitLog {
        let mut log = UnitLog::new("CF-01", vec!["a".into()]);
        for &(t, v) in values {
            log.push(t, 0, t as u32, vec![v]);
        }
        log
    }

    #[test]
    fn range_of_the_samples_in_view_only() {
        let log = log_of(&(0..10).map(|i| (i * 100, if i == 2 { 50.0 } else { i as f64 })).collect::<Vec<_>>());
        // The spike at 200 ms is outside the view
        assert_eq!(visible_range(&log, 0, 500, 900), Some((5.0, 9.0)));
        assert_eq!(visible_range(&log, 0, 2000, 3000), None);
        // A flat line gets some height, centred on it
        let (lo, hi) = padded((4.0, 4.0));
        assert!(lo < 4.0 && hi > 4.0 && (4.0 - lo - (hi - 4.0)).abs() < 1e-9);
    }

    #[test]
    fn units_share_one_time_axis() {
        // Two units booted 9 s apart, logged at the same moments; their
        // samples arrive 3 to 8 ms late
        let mut a = UnitLog::new("CF-01", vec!["v".into()]);
        let mut b = UnitLog::new("CF-02", vec!["v".into()]);
        for k in 0..50i64 {
            a.push(1000 + k * 10 + 3 + k % 5, 0, (20_000 + k * 10) as u32, vec![0.0]);
            b.push(1000 + k * 10 + 4 + k % 4, 0, (29_000 + k * 10) as u32, vec![0.0]);
        }
        for (sa, sb) in a.samples.iter().zip(&b.samples).skip(OFFSET_SAMPLES as usize) {
            assert!((sa.t_ms - sb.t_ms).abs() <= 1, "{} vs {}", sa.t_ms, sb.t_ms);
        }
        assert!(a.samples.windows(2).all(|w| w[0].t_ms <= w[1].t_ms));
    }

    #[test]
    fn csv_has_both_clocks_and_a_row_per_unit_sample() {
        let mut st = PlotState::new();
        st.config = Some(LogConfig { name: "Position".into(), description: None, variables: vec!["x".into(), "z".into()], period_ms: 20 });
        let mut a = UnitLog::new("CF-01", vec!["x".into(), "z".into()]);
        a.push(0, 1_700_000_000_000, 5000, vec![1.5, 0.25]);
        // CF-02 lacks z
        let mut b = UnitLog::new("CF-02", vec!["x".into()]);
        b.push(0, 1_700_000_000_005, 7000, vec![-2.0]);
        st.units = vec![a, b];
        let csv = log_csv(&st);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines[0], "system_time,platform_time_ms,unit,x,z");
        assert_eq!(lines.len(), 3);
        assert!(lines[1].ends_with(",5000,CF-01,1.5,0.25"), "{}", lines[1]);
        assert!(lines[2].ends_with(",7000,CF-02,-2,"), "{}", lines[2]);
        assert!(lines[1].starts_with("2023-11-1"), "{}", lines[1]);
    }

    #[test]
    fn dashes_lift_the_pen() {
        let points = [Some((0.0, 0.0)), Some((22.0, 0.0))];
        assert_eq!(path_commands(&points, &[]).matches('M').count(), 1);
        // 7 on, 4 off: two dashes in 22 px
        assert_eq!(path_commands(&points, &[7.0, 4.0]).matches('M').count(), 2);
    }
}
