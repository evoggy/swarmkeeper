//! Live data plot for the unit / visualization dock.
//!
//! The rendering, axis-tick, legend and pan/zoom/hover logic is ported from
//! swarmfinity's Explore plot (which renders historical SQLite series). Here the
//! series are live ring buffers fed from a Crazyflie log stream and a redraw
//! `slint::Timer` regenerates the Slint `Path` commands as samples arrive. The
//! matching UI lives in `ui/plot.slint`; all of it reads/writes the `PlotBridge`
//! global, so the two `PlotPanel` instances (Units tab + Visualization tab)
//! behave as one panel with a single Rust target.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{stream, StreamExt};
use serde::{Deserialize, Serialize};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::{AppWindow, AxisTick, LegendEntry, LogVarRow, PlotBridge, PlotLine};

/// One time/value sample. `ts_ms` is monotonic milliseconds.
#[derive(Debug, Clone, Copy)]
pub struct SeriesPoint {
    pub ts_ms: i64,
    pub value: f64,
}

/// Per-series safety backstop on retained samples. The full session is kept so
/// the user can zoom out to the whole history; this only guards against an
/// unbounded multi-day run (~55 h at 100 ms, ~32 MB/series) — not a view window.
const MAX_POINTS: usize = 2_000_000;

/// Vertical breathing room (px) so the data line never touches the top/bottom
/// edge. Applied to both the data path and the Y ticks so values stay aligned.
const Y_MARGIN_PX: f64 = 6.0;

#[derive(Debug, Clone)]
pub struct PlotSlot {
    /// Firmware log variable name ("group.name"), or a synthetic label.
    pub name: String,
    /// Group shown in the picker (used in a later phase); ignored for rendering.
    #[allow(dead_code)]
    pub group: String,
    pub color: [u8; 3],
    pub active: bool,
    pub series: Vec<SeriesPoint>,
}

impl PlotSlot {
    pub fn push(&mut self, ts_ms: i64, value: f64) {
        self.series.push(SeriesPoint { ts_ms, value });
        if self.series.len() > MAX_POINTS {
            let overflow = self.series.len() - MAX_POINTS;
            self.series.drain(0..overflow);
        }
    }
}

/// Plot view / zoom state. X in absolute `ts_ms`; Y as a 0..1 fraction of each
/// series' full-range-normalized space. When `user_view` is false the plot
/// autoscales to the data on every rebuild (live auto-follow); once the user
/// pans or zooms, the values stick.
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
    pub slots: Vec<PlotSlot>,
    pub view: PlotView,
    /// Last polled plot-area pixel size.
    pub px: (f32, f32),
    /// Set when a size-change rebuild is already queued for the next tick.
    pub rebuild_pending: bool,
    /// Set by the data feed; the redraw timer rebuilds and clears it.
    pub dirty: bool,
    /// Name of the config whose slots are currently loaded. Data is cleared only
    /// when a *different* block is engaged — Stop/restart of the same block keeps
    /// accumulating so the full session stays zoomable.
    pub loaded_config: Option<String>,
}

impl PlotState {
    pub fn new() -> Self {
        Self {
            slots: Vec::new(),
            view: PlotView::default(),
            px: (0.0, 0.0),
            rebuild_pending: false,
            dirty: false,
            loaded_config: None,
        }
    }
}

/// Color palette for multi-series overlay. Cycled by slot index so each series
/// keeps a stable color.
const PLOT_COLORS: [[u8; 3]; 10] = [
    [0x42, 0x9b, 0xf5], // blue
    [0x66, 0xbb, 0x6a], // green
    [0xef, 0x53, 0x50], // red
    [0xff, 0xa7, 0x26], // orange
    [0xab, 0x47, 0xbc], // purple
    [0x26, 0xc6, 0xda], // teal
    [0xff, 0xca, 0x28], // amber
    [0x8d, 0x6e, 0x63], // brown
    [0x9f, 0xa8, 0xda], // muted violet
    [0xec, 0x40, 0x7a], // crimson
];

pub fn color_for(index: usize) -> [u8; 3] {
    PLOT_COLORS[index % PLOT_COLORS.len()]
}

type Shared = Arc<Mutex<PlotState>>;

/// Wire up all PlotBridge callbacks and start the redraw timer. Returns the
/// shared state so the caller (main) can hand it to the streaming code later.
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

    // Redraw timer: rebuild only when the data feed marked the plot dirty.
    {
        let st = state.clone();
        let weak = ui.as_weak();
        let timer = slint::Timer::default();
        timer.start(slint::TimerMode::Repeated, Duration::from_millis(150), move || {
            let dirty = {
                let mut s = st.lock().unwrap();
                let d = s.dirty;
                s.dirty = false;
                d
            };
            if dirty {
                if let Some(ui) = weak.upgrade() {
                    rebuild_plot(&ui, &st);
                }
            }
        });
        // Keep the timer alive for the lifetime of the app.
        Box::leak(Box::new(timer));
    }

    state
}

/// Demo: seed a few synthetic series and animate them so the dock, rendering
/// and interaction can be exercised without hardware. Not called in normal runs
/// (live streaming replaces it); kept as a manual test hook.
#[allow(dead_code)]
pub fn seed_demo(ui: &AppWindow, state: &Shared) {
    {
        let mut s = state.lock().unwrap();
        s.slots = vec![
            PlotSlot { name: "stateEstimate.z".into(), group: "demo".into(), color: color_for(0), active: true, series: Vec::new() },
            PlotSlot { name: "pm.vbat".into(), group: "demo".into(), color: color_for(2), active: true, series: Vec::new() },
            PlotSlot { name: "stateEstimate.vx".into(), group: "demo".into(), color: color_for(5), active: true, series: Vec::new() },
        ];
    }
    let bridge = ui.global::<PlotBridge>();
    bridge.set_status_text("Demo data".into());

    let st = state.clone();
    let weak = ui.as_weak();
    let mut tick: i64 = 0;
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(100), move || {
        let ts = tick * 100;
        let t = tick as f64 * 0.1;
        {
            let mut s = st.lock().unwrap();
            if s.slots.len() >= 3 {
                s.slots[0].push(ts, 0.5 + 0.4 * (t).sin());
                s.slots[1].push(ts, 4.10 - 0.0008 * tick as f64 + 0.01 * (t * 3.0).sin());
                s.slots[2].push(ts, 0.6 * (t * 0.7).cos());
            }
            s.dirty = true;
        }
        tick += 1;
        let _ = weak; // upgrade happens in the redraw timer
    });
    Box::leak(Box::new(timer));
}

fn clear_plot(b: &PlotBridge) {
    b.set_plot_lines(ModelRc::new(VecModel::from(Vec::<PlotLine>::new())));
    b.set_x_ticks(ModelRc::new(VecModel::from(Vec::<AxisTick>::new())));
    b.set_y_ticks(ModelRc::new(VecModel::from(Vec::<AxisTick>::new())));
    b.set_legend(ModelRc::new(VecModel::from(Vec::<LegendEntry>::new())));
    b.set_summary(SharedString::from(""));
    b.set_x_label(SharedString::from("—"));
    b.set_y_axis_label(SharedString::from("(normalized)"));
    b.set_has_view(false);
    b.set_hover_active(false);
    b.set_hover_text(SharedString::from(""));
    b.set_nav_min_frac(0.0);
    b.set_nav_max_frac(1.0);
}

/// Build the plot lines, axis ticks and legend for the current set of active
/// slots. Holds the state lock throughout (no series cloning); drops it before
/// the optional hover refresh, which re-locks.
pub fn rebuild_plot(window: &AppWindow, state: &Shared) {
    let b = window.global::<PlotBridge>();
    let hover_refresh;
    {
        let mut st = state.lock().unwrap();

        // Every series that has data — these all get a legend entry, so a
        // disabled series stays listed and clickable to re-enable.
        let with_data: Vec<usize> = st
            .slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.series.len() >= 2)
            .map(|(i, _)| i)
            .collect();

        if with_data.is_empty() {
            st.view = PlotView::default();
            drop(st);
            clear_plot(&b);
            return;
        }

        // Only enabled series are drawn and drive the view/axes.
        let active: Vec<usize> = with_data
            .iter()
            .copied()
            .filter(|&i| st.slots[i].active)
            .collect();

        // Range source: the drawn series, or — when everything is toggled off —
        // all series with data, so the axes stay sensible and the legend (and
        // thus the re-enable affordance) remains on screen.
        let range_src: &[usize] = if active.is_empty() { &with_data } else { &active };
        let data_t_min = range_src
            .iter()
            .map(|&i| st.slots[i].series.first().unwrap().ts_ms)
            .min()
            .unwrap();
        let data_t_max = range_src
            .iter()
            .map(|&i| st.slots[i].series.last().unwrap().ts_ms)
            .max()
            .unwrap();

        // Resolve the view: autoscale unless the user has pinned one.
        let mut view = st.view.clone();
        if !view.user_view || view.x_max_ms <= view.x_min_ms {
            view.x_min_ms = data_t_min;
            view.x_max_ms = data_t_max;
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

        let (plot_w_px, plot_h_px) = st.px;
        let plot_w = if plot_w_px > 0.5 { plot_w_px as f64 } else { 1.0 };
        let plot_h = if plot_h_px > 0.5 { plot_h_px as f64 } else { 1.0 };
        let y_margin_frac = (Y_MARGIN_PX / plot_h).min(0.4);

        let view_dt = (view.x_max_ms - view.x_min_ms).max(1) as f64;
        let view_span_s = view_dt / 1000.0;
        let total_span_s = (data_t_max - data_t_min) as f64 / 1000.0;

        let y_view_min = view.y_min_frac as f64;
        let y_view_span = (view.y_max_frac - view.y_min_frac).max(1e-6) as f64;

        let mut lines: Vec<PlotLine> = Vec::with_capacity(active.len());
        let mut total_pts: usize = 0;
        let mut single_series_full_range: Option<(f64, f64)> = None;

        // Lines — only enabled series.
        for &i in &active {
            let slot = &st.slots[i];
            let series = &slot.series;

            let (full_y_min, full_y_max) = series_y_range(series);
            let dy = (full_y_max - full_y_min).abs().max(1e-9);
            if active.len() == 1 {
                single_series_full_range = Some((full_y_min, full_y_max));
            }

            // Decimate to <=500 points across the FULL series so the shape
            // survives panning; always include the last sample.
            let stride = ((series.len() + 499) / 500).max(1);
            let mut decimated: Vec<&SeriesPoint> = series.iter().step_by(stride).collect();
            if let (Some(last), Some(tail)) = (series.last(), decimated.last()) {
                if !std::ptr::eq(*tail, last) {
                    decimated.push(last);
                }
            }
            total_pts += decimated.len();

            let mut commands = String::with_capacity(decimated.len() * 16);
            let mut pen_down = false;
            for p in &decimated {
                let x_norm = (p.ts_ms - view.x_min_ms) as f64 / view_dt;
                if !(-0.1..=1.1).contains(&x_norm) {
                    pen_down = false;
                    continue;
                }
                let y_full = 1.0 - (p.value - full_y_min) / dy;
                let y_norm = (y_full - y_view_min) / y_view_span;
                if !(-0.1..=1.1).contains(&y_norm) {
                    pen_down = false;
                    continue;
                }
                let x_px = x_norm * plot_w;
                let y_px = (y_margin_frac + y_norm * (1.0 - 2.0 * y_margin_frac)) * plot_h;
                let cmd = if pen_down { 'L' } else { 'M' };
                let _ = write!(commands, "{cmd} {x_px:.2} {y_px:.2} ");
                pen_down = true;
            }

            let [r, g, bl] = slot.color;
            let color = slint::Color::from_rgb_u8(r, g, bl);
            lines.push(PlotLine { commands: SharedString::from(commands), color });
        }

        // Legend — every series with data, so disabled ones stay clickable. The
        // `enabled` flag lets the UI mute the off ones.
        let mut legend: Vec<LegendEntry> = Vec::with_capacity(with_data.len());
        for &i in &with_data {
            let slot = &st.slots[i];
            let (full_y_min, full_y_max) = series_y_range(&slot.series);
            let decimals = decimals_for_step((full_y_max - full_y_min) / 5.0);
            let [r, g, bl] = slot.color;
            legend.push(LegendEntry {
                label: SharedString::from(slot.name.as_str()),
                color: slint::Color::from_rgb_u8(r, g, bl),
                range: SharedString::from(format!(
                    "{:.dp$} .. {:.dp$}",
                    full_y_min, full_y_max, dp = decimals
                )),
                hover_value: SharedString::from(""),
                slot_index: i as i32,
                enabled: slot.active,
            });
        }

        let x_ticks = build_x_ticks(view.x_min_ms, view.x_max_ms, data_t_min, 6);

        let (mut y_ticks, y_axis_label) = if let Some((full_y_min, full_y_max)) =
            single_series_full_range
        {
            let dy = (full_y_max - full_y_min).max(1e-9);
            let visible_top = full_y_min + (1.0 - view.y_min_frac as f64) * dy;
            let visible_bot = full_y_min + (1.0 - view.y_max_frac as f64) * dy;
            (
                build_y_ticks_real(visible_bot, visible_top, 5),
                st.slots[active[0]].name.clone(),
            )
        } else {
            (
                build_y_ticks_normalized(view.y_min_frac, view.y_max_frac),
                "(normalized)".to_string(),
            )
        };
        let ym = y_margin_frac as f32;
        for t in y_ticks.iter_mut() {
            t.pos = ym + t.pos * (1.0 - 2.0 * ym);
        }

        let summary = format!(
            "{} series · {} pts · view {:.1}s of {:.1}s",
            active.len(),
            total_pts,
            view_span_s,
            total_span_s,
        );
        let x_label = format!(
            "{} → {}",
            format_time_offset((view.x_min_ms - data_t_min) as f64 / 1000.0),
            format_time_offset((view.x_max_ms - data_t_min) as f64 / 1000.0),
        );

        let data_span = (data_t_max - data_t_min).max(1) as f64;
        let nav_min = ((view.x_min_ms - data_t_min) as f64 / data_span).clamp(0.0, 1.0) as f32;
        let nav_max = ((view.x_max_ms - data_t_min) as f64 / data_span).clamp(0.0, 1.0) as f32;

        b.set_plot_lines(ModelRc::new(VecModel::from(lines)));
        b.set_x_ticks(ModelRc::new(VecModel::from(x_ticks)));
        b.set_y_ticks(ModelRc::new(VecModel::from(y_ticks)));
        b.set_legend(ModelRc::new(VecModel::from(legend)));
        b.set_summary(SharedString::from(summary));
        b.set_x_label(SharedString::from(x_label));
        b.set_y_axis_label(SharedString::from(y_axis_label));
        b.set_has_view(view.user_view);
        b.set_nav_min_frac(nav_min);
        b.set_nav_max_frac(nav_max);

        hover_refresh = b.get_hover_active();
    }

    if hover_refresh {
        let frac = b.get_hover_x();
        plot_hover(window, state, frac);
    }
}

fn series_y_range(series: &[SeriesPoint]) -> (f64, f64) {
    let mut y_min = f64::INFINITY;
    let mut y_max = f64::NEG_INFINITY;
    for p in series {
        if p.value < y_min {
            y_min = p.value;
        }
        if p.value > y_max {
            y_max = p.value;
        }
    }
    if !y_min.is_finite() || !y_max.is_finite() {
        (0.0, 1.0)
    } else {
        (y_min, y_max)
    }
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
            ticks.push(AxisTick {
                pos,
                label: SharedString::from(format_time_offset(t)),
            });
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
            ticks.push(AxisTick {
                pos,
                label: SharedString::from(format!("{v:.dp$}")),
            });
        }
        v += step;
        guard += 1;
    }
    ticks
}

fn build_y_ticks_normalized(y_view_min: f32, y_view_max: f32) -> Vec<AxisTick> {
    let mut ticks = Vec::with_capacity(5);
    for i in 0..=4 {
        let viewport_frac = i as f32 / 4.0;
        let series_frac = y_view_min + viewport_frac * (y_view_max - y_view_min);
        let pct = (1.0 - series_frac) * 100.0;
        let label = if pct.fract().abs() < 0.05 {
            format!("{pct:.0}%")
        } else {
            format!("{pct:.1}%")
        };
        ticks.push(AxisTick {
            pos: viewport_frac,
            label: SharedString::from(label),
        });
    }
    ticks
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

fn plot_reset(window: &AppWindow, state: &Shared) {
    {
        let mut s = state.lock().unwrap();
        s.view = PlotView::default();
    }
    rebuild_plot(window, state);
}

fn plot_toggle_series(window: &AppWindow, state: &Shared, slot_index: i32) {
    {
        let mut s = state.lock().unwrap();
        if let Some(slot) = s.slots.get_mut(slot_index as usize) {
            slot.active = !slot.active;
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

/// Slint reports a new plot-area pixel size. Cache it and schedule a rebuild on
/// the next event-loop tick — we cannot rebuild synchronously from inside the
/// callback that fired during property evaluation.
fn plot_size_changed(window: &AppWindow, state: &Shared, w: f32, h: f32) {
    let should_schedule = {
        let mut s = state.lock().unwrap();
        let old = s.px;
        if (old.0 - w).abs() <= 0.5 && (old.1 - h).abs() <= 0.5 {
            return;
        }
        s.px = (w, h);
        if s.rebuild_pending {
            false
        } else {
            s.rebuild_pending = true;
            true
        }
    };
    if !should_schedule {
        return;
    }
    let weak = window.as_weak();
    let st = state.clone();
    slint::Timer::single_shot(Duration::ZERO, move || {
        {
            let mut s = st.lock().unwrap();
            s.rebuild_pending = false;
        }
        if let Some(window) = weak.upgrade() {
            rebuild_plot(&window, &st);
        }
    });
}

/// Update the hover crosshair position + per-series readout. `frac` outside
/// 0..1 clears the hover state.
fn plot_hover(window: &AppWindow, state: &Shared, frac: f32) {
    let b = window.global::<PlotBridge>();
    if !(0.0..=1.0).contains(&frac) {
        b.set_hover_active(false);
        b.set_hover_text(SharedString::from(""));
        let mut legend: Vec<LegendEntry> = b.get_legend().iter().collect();
        let mut changed = false;
        for entry in legend.iter_mut() {
            if !entry.hover_value.is_empty() {
                entry.hover_value = SharedString::from("");
                changed = true;
            }
        }
        if changed {
            b.set_legend(ModelRc::new(VecModel::from(legend)));
        }
        return;
    }

    let (header, legend_updates) = {
        let s = state.lock().unwrap();
        let v = &s.view;
        let span_ms = (v.x_max_ms - v.x_min_ms).max(1) as f64;
        let cursor_ms = v.x_min_ms as f64 + frac as f64 * span_ms;
        let data_t_min = s
            .slots
            .iter()
            .filter(|sl| sl.active)
            .filter_map(|sl| sl.series.first().map(|p| p.ts_ms))
            .min()
            .unwrap_or(0);

        let header = format_time_offset((cursor_ms - data_t_min as f64) / 1000.0);
        let mut updates: Vec<(i32, String)> = Vec::new();
        for (idx, slot) in s.slots.iter().enumerate() {
            if !slot.active {
                continue;
            }
            let series = &slot.series;
            if series.len() < 2 {
                continue;
            }
            let target_ms = cursor_ms.round() as i64;
            let pos = series.partition_point(|p| p.ts_ms < target_ms);
            let val = if pos == 0 {
                series[0].value
            } else if pos >= series.len() {
                series[series.len() - 1].value
            } else {
                let a = &series[pos - 1];
                let bb = &series[pos];
                let d = (bb.ts_ms - a.ts_ms).max(1) as f64;
                let tt = ((cursor_ms - a.ts_ms as f64) / d).clamp(0.0, 1.0);
                a.value + tt * (bb.value - a.value)
            };
            let (y_min, y_max) = series_y_range(series);
            let dp = decimals_for_step((y_max - y_min) / 5.0).max(2);
            updates.push((idx as i32, format!("{val:.dp$}")));
        }
        (header, updates)
    };

    let mut legend: Vec<LegendEntry> = b.get_legend().iter().collect();
    let mut tooltip = String::with_capacity(64);
    tooltip.push_str("t = ");
    tooltip.push_str(&header);
    for entry in legend.iter_mut() {
        let new_val = legend_updates
            .iter()
            .find(|(idx, _)| *idx == entry.slot_index)
            .map(|(_, s)| s.clone())
            .unwrap_or_default();
        if !new_val.is_empty() {
            tooltip.push('\n');
            tooltip.push_str(&entry.label);
            tooltip.push_str(": ");
            tooltip.push_str(&new_val);
        }
        entry.hover_value = SharedString::from(new_val);
    }
    b.set_legend(ModelRc::new(VecModel::from(legend)));
    b.set_hover_x(frac);
    b.set_hover_text(SharedString::from(tooltip));
    b.set_hover_active(true);
}

// ===========================================================================
// Phase 2/3: log configurations, persistence and live Crazyflie log streaming.
// ===========================================================================

/// A named, persistable set of log variables to stream and plot. Mirrors the
/// `PlanningScene` config pattern (serde + free save/load functions).
#[derive(Clone, Debug, Serialize, Deserialize)]
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

/// Directory holding the saved log configs, relative to the client working dir
/// (consistent with `swarms/`, `scenes/`, `plans/`).
pub fn log_configs_dir() -> PathBuf {
    PathBuf::from("log_configs")
}

pub fn load_log_config(path: &Path) -> Result<LogConfig, String> {
    let s = std::fs::read_to_string(path).map_err(|e| format!("read {path:?}: {e}"))?;
    serde_yaml::from_str(&s).map_err(|e| format!("parse {path:?}: {e}"))
}

pub fn save_log_config(cfg: &LogConfig) -> Result<PathBuf, String> {
    let dir = log_configs_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {dir:?}: {e}"))?;
    let path = dir.join(format!("{}.yaml", sanitize_file_name(&cfg.name)));
    let s = serde_yaml::to_string(cfg).map_err(|e| format!("serialize: {e}"))?;
    std::fs::write(&path, s).map_err(|e| format!("write {path:?}: {e}"))?;
    Ok(path)
}

fn sanitize_file_name(name: &str) -> String {
    let s: String = name
        .trim()
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    if s.is_empty() { "log".to_string() } else { s }
}

/// Scan `log_configs/` and return all parseable configs, sorted by name.
pub fn list_log_configs() -> Vec<LogConfig> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(log_configs_dir()) {
        for entry in rd.flatten() {
            let path = entry.path();
            let is_yaml = path
                .extension()
                .and_then(|e| e.to_str())
                .map_or(false, |e| e == "yaml" || e == "yml");
            if is_yaml {
                if let Ok(cfg) = load_log_config(&path) {
                    out.push(cfg);
                }
            }
        }
    }
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out
}

fn value_to_f64(v: crazyflie_lib::Value) -> f64 {
    use crazyflie_lib::Value::*;
    match v {
        U8(x) => x as f64,
        U16(x) => x as f64,
        U32(x) => x as f64,
        U64(x) => x as f64,
        I8(x) => x as f64,
        I16(x) => x as f64,
        I32(x) => x as f64,
        I64(x) => x as f64,
        F16(x) => f32::from(x) as f64,
        F32(x) => x as f64,
        F64(x) => x,
    }
}

/// Split `names` into log blocks that each fit `budget` bytes of payload.
/// Mirrors tdoa_doctor's chunker — the firmware caps a log packet's payload.
fn chunk_by_bytes(cf: &crazyflie_lib::Crazyflie, names: &[String], budget: usize) -> Vec<Vec<String>> {
    let mut chunks = Vec::new();
    let mut cur: Vec<String> = Vec::new();
    let mut used = 0usize;
    for n in names {
        let sz = cf.log.get_type(n).map(|t| t.byte_length()).unwrap_or(4);
        if !cur.is_empty() && used + sz > budget {
            chunks.push(std::mem::take(&mut cur));
            used = 0;
        }
        cur.push(n.clone());
        used += sz;
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    chunks
}

/// Conservative per-block payload budget (bytes), matching tdoa_doctor.
const BLOCK_BUDGET_BYTES: usize = 24;

fn set_status(weak: &slint::Weak<AppWindow>, text: &str) {
    let weak = weak.clone();
    let text = text.to_string();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            ui.global::<PlotBridge>().set_status_text(text.into());
        }
    });
}

fn set_logging_active(weak: &slint::Weak<AppWindow>, active: bool) {
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            ui.global::<PlotBridge>().set_logging_active(active);
        }
    });
}

/// Stream a config's variables from one Crazyflie into the plot's ring buffers
/// until aborted (config switch / stop) or the link drops.
///
/// Aborting this task drops every `LogStream`, which deletes the firmware log
/// blocks — the intended teardown when switching configs.
pub async fn run_plot_log(
    cf: Arc<crazyflie_lib::Crazyflie>,
    state: Shared,
    weak: slint::Weak<AppWindow>,
    cfg: LogConfig,
) {
    let toc: std::collections::HashSet<String> = cf.log.names().into_iter().collect();
    let names: Vec<String> = cfg.variables.iter().filter(|n| toc.contains(*n)).cloned().collect();
    let missing = cfg.variables.len() - names.len();

    if names.is_empty() {
        set_status(&weak, "None of the config's variables exist on this unit's TOC.");
        set_logging_active(&weak, false);
        return;
    }

    // Set up slots. Clear the graph ONLY when this is a different block than the
    // one currently loaded; restarting the same block (e.g. after Stop) keeps the
    // accumulated data so the whole session stays zoomable.
    {
        let mut s = state.lock().unwrap();
        let same_block = s.loaded_config.as_deref() == Some(cfg.name.as_str());
        if !same_block {
            s.slots = names
                .iter()
                .enumerate()
                .map(|(i, n)| PlotSlot {
                    name: n.clone(),
                    group: cfg.name.clone(),
                    color: color_for(i),
                    active: true,
                    series: Vec::new(),
                })
                .collect();
            s.view = PlotView::default();
            s.loaded_config = Some(cfg.name.clone());
        } else {
            // Same block resumed: keep existing series, but make sure every
            // streamed variable still has a slot to land in.
            for n in &names {
                if !s.slots.iter().any(|sl| &sl.name == n) {
                    let color = color_for(s.slots.len());
                    s.slots.push(PlotSlot {
                        name: n.clone(),
                        group: cfg.name.clone(),
                        color,
                        active: true,
                        series: Vec::new(),
                    });
                }
            }
        }
        s.dirty = true;
    }

    let period_ms = cfg.period_ms.clamp(10, 2550);
    let chunks = chunk_by_bytes(&cf, &names, BLOCK_BUDGET_BYTES);

    let mut streams = Vec::new();
    for chunk in &chunks {
        // LogPeriod is consumed by start(), so build one per block.
        let period = match crazyflie_lib::subsystems::log::LogPeriod::from_millis(period_ms) {
            Ok(p) => p,
            Err(_) => {
                set_status(&weak, "Invalid log period (10..=2550 ms).");
                set_logging_active(&weak, false);
                return;
            }
        };
        let mut block = match cf.log.create_block().await {
            Ok(b) => b,
            Err(e) => {
                set_status(&weak, &format!("create_block failed: {e}"));
                set_logging_active(&weak, false);
                return;
            }
        };
        for n in chunk {
            if let Err(e) = block.add_variable(n).await {
                eprintln!("plot-log: add_variable {n} failed: {e:?}");
            }
        }
        match block.start(period).await {
            Ok(s) => streams.push(s),
            Err(e) => eprintln!("plot-log: block.start failed: {e:?}"),
        }
    }

    if streams.is_empty() {
        set_status(&weak, "Failed to start any log blocks.");
        set_logging_active(&weak, false);
        return;
    }

    set_logging_active(&weak, true);
    let status = if missing == 0 {
        format!("Logging {} var(s) @ {} ms", names.len(), period_ms)
    } else {
        format!("Logging {} var(s) @ {} ms ({} not on TOC)", names.len(), period_ms, missing)
    };
    set_status(&weak, &status);

    // Merge every block's stream; each `unfold` owns its LogStream so dropping
    // `merged` (on abort) deletes the firmware blocks.
    let mut merged = stream::select_all(streams.into_iter().map(|s| {
        stream::unfold(s, |s| async move {
            match s.next().await {
                Ok(d) => Some((d, s)),
                Err(_) => None,
            }
        })
        .boxed()
    }));

    while let Some(data) = merged.next().await {
        let ts = data.timestamp as i64;
        let mut s = state.lock().unwrap();
        for (name, value) in &data.data {
            let v = value_to_f64(*value);
            if let Some(slot) = s.slots.iter_mut().find(|sl| &sl.name == name) {
                slot.push(ts, v);
            }
        }
        s.dirty = true;
    }

    // Natural end (link dropped). On abort this code is not reached; the
    // stop/switch handler updates the flag instead.
    set_logging_active(&weak, false);
    set_status(&weak, "Log stream ended.");
}

/// State for the "New config" variable picker: the connected unit's full log
/// TOC, the currently-checked variables, and the search filter.
#[derive(Default)]
pub struct TocPicker {
    all: Vec<String>,
    checked: std::collections::HashSet<String>,
    filter: String,
}

pub type SharedToc = Arc<Mutex<TocPicker>>;

/// Push the filtered TOC rows + status into the picker dialog (UI thread).
fn push_toc_rows(ui: &AppWindow, picker: &SharedToc) {
    let p = picker.lock().unwrap();
    let filt = p.filter.to_lowercase();
    let rows: Vec<LogVarRow> = p
        .all
        .iter()
        .filter(|n| filt.is_empty() || n.to_lowercase().contains(&filt))
        .map(|n| LogVarRow {
            name: SharedString::from(n.as_str()),
            checked: p.checked.contains(n),
        })
        .collect();
    let status = if p.all.is_empty() {
        "No variables".to_string()
    } else {
        format!("{} shown · {} selected", rows.len(), p.checked.len())
    };
    let b = ui.global::<PlotBridge>();
    b.set_toc_rows(ModelRc::new(VecModel::from(rows)));
    b.set_toc_status(SharedString::from(status));
}

/// Type of the shared abort handle for the active plot-log task.
pub type LogTask = Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>;
/// Shared "unit selected for logging" — the unsorted original index, updated by
/// the unit-selection callbacks.
pub type SelectedIndex = Arc<Mutex<Option<usize>>>;

/// Push the saved-config list into the dropdown.
pub fn refresh_config_list(ui: &AppWindow) {
    let names: Vec<SharedString> =
        list_log_configs().into_iter().map(|c| SharedString::from(c.name)).collect();
    ui.global::<PlotBridge>()
        .set_config_names(ModelRc::new(VecModel::from(names)));
}

/// Resolve the selected config + unit on the UI thread, then spawn the stream.
fn resolve_and_start(
    ui: &AppWindow,
    state: &Shared,
    swarm_state: &crate::SwarmState,
    selected: &SelectedIndex,
    log_task: &LogTask,
) {
    let b = ui.global::<PlotBridge>();
    let idx = b.get_config_index();
    let configs = list_log_configs();
    let Some(cfg) = (if idx >= 0 { configs.get(idx as usize).cloned() } else { None }) else {
        b.set_status_text("Pick or create a log config first.".into());
        b.set_logging_active(false);
        return;
    };
    let Some(orig) = *selected.lock().unwrap() else {
        b.set_status_text("Select a unit, then Start.".into());
        b.set_logging_active(false);
        return;
    };

    let weak = ui.as_weak();
    let state = state.clone();
    let swarm_state = swarm_state.clone();
    let log_task = log_task.clone();
    tokio::spawn(async move {
        // Abort any previous task first (drops its log blocks).
        if let Some(h) = log_task.lock().await.take() {
            h.abort();
        }
        let cf = {
            let st = swarm_state.lock().await;
            st.get(&orig).map(|cu| cu.cf.clone())
        };
        let Some(cf) = cf else {
            set_status(&weak, "Selected unit is not connected.");
            set_logging_active(&weak, false);
            return;
        };
        let handle = tokio::spawn(run_plot_log(cf, state, weak.clone(), cfg));
        *log_task.lock().await = Some(handle);
    });
}

/// Register the streaming-lifecycle callbacks (start / stop / switch / save) and
/// populate the dropdown. Call after `swarm_state` exists.
pub fn wire_logging(
    ui: &AppWindow,
    state: &Shared,
    swarm_state: crate::SwarmState,
    selected: SelectedIndex,
    log_task: LogTask,
) {
    refresh_config_list(ui);
    let bridge = ui.global::<PlotBridge>();
    let toc_picker: SharedToc = Arc::new(Mutex::new(TocPicker::default()));

    // Start.
    {
        let state = state.clone();
        let swarm_state = swarm_state.clone();
        let selected = selected.clone();
        let log_task = log_task.clone();
        let weak = ui.as_weak();
        bridge.on_start_logging(move || {
            if let Some(ui) = weak.upgrade() {
                resolve_and_start(&ui, &state, &swarm_state, &selected, &log_task);
            }
        });
    }

    // Stop — abort the task (drops the firmware blocks).
    {
        let log_task = log_task.clone();
        let weak = ui.as_weak();
        bridge.on_stop_logging(move || {
            let log_task = log_task.clone();
            let weak = weak.clone();
            tokio::spawn(async move {
                if let Some(h) = log_task.lock().await.take() {
                    h.abort();
                }
                set_logging_active(&weak, false);
                set_status(&weak, "Stopped.");
            });
        });
    }

    // Select a config from the dropdown. A *different* block clears the graph
    // (new variables); the same block leaves the accumulated data alone. If
    // logging, the new block is started immediately (old one torn down).
    {
        let state = state.clone();
        let swarm_state = swarm_state.clone();
        let selected = selected.clone();
        let log_task = log_task.clone();
        let weak = ui.as_weak();
        bridge.on_select_config(move |_idx| {
            let Some(ui) = weak.upgrade() else { return };
            let b = ui.global::<PlotBridge>();
            let idx = b.get_config_index();
            let cfg_name = if idx >= 0 {
                list_log_configs().get(idx as usize).map(|c| c.name.clone())
            } else {
                None
            };
            let is_new_block = state.lock().unwrap().loaded_config != cfg_name;
            if !is_new_block {
                return;
            }
            // New block selected — clear the graph now.
            {
                let mut s = state.lock().unwrap();
                s.slots.clear();
                s.loaded_config = None;
                s.view = PlotView::default();
                s.dirty = true;
            }
            if b.get_logging_active() {
                resolve_and_start(&ui, &state, &swarm_state, &selected, &log_task);
            }
        });
    }

    // "New…" dialog opened — fetch the selected unit's log TOC into the picker.
    {
        let swarm_state = swarm_state.clone();
        let selected = selected.clone();
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
            b.set_toc_status(SharedString::from("Loading variables…"));

            let Some(orig) = *selected.lock().unwrap() else {
                b.set_toc_status(SharedString::from("Select a connected unit first."));
                return;
            };
            let swarm_state = swarm_state.clone();
            let picker = picker.clone();
            let weak = weak.clone();
            tokio::spawn(async move {
                let cf = {
                    let st = swarm_state.lock().await;
                    st.get(&orig).map(|cu| cu.cf.clone())
                };
                let Some(cf) = cf else {
                    let weak2 = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak2.upgrade() {
                            ui.global::<PlotBridge>()
                                .set_toc_status(SharedString::from("Selected unit is not connected."));
                        }
                    });
                    return;
                };
                let mut names = cf.log.names();
                names.sort();
                picker.lock().unwrap().all = names;
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        push_toc_rows(&ui, &picker);
                    }
                });
            });
        });
    }

    // Toggle a variable's checkbox in the picker.
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

    // Search filter changed in the picker.
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

    // Save the picked variables as a new config, then select it and close the
    // dialog. Validation failures keep the dialog open with a status message.
    {
        let picker = toc_picker.clone();
        let weak = ui.as_weak();
        bridge.on_save_new_config(move |name, period| {
            let mut variables: Vec<String> =
                picker.lock().unwrap().checked.iter().cloned().collect();
            variables.sort();
            let Some(ui) = weak.upgrade() else { return };
            let b = ui.global::<PlotBridge>();
            if name.trim().is_empty() {
                b.set_toc_status(SharedString::from("Enter a name."));
                return;
            }
            if variables.is_empty() {
                b.set_toc_status(SharedString::from("Select at least one variable."));
                return;
            }
            let period_ms = if period.is_finite() && (10.0..=2550.0).contains(&period) {
                period as u64
            } else {
                100
            };
            let cfg = LogConfig {
                name: name.trim().to_string(),
                description: None,
                variables,
                period_ms,
            };
            if let Err(e) = save_log_config(&cfg) {
                eprintln!("plot-log: save config failed: {e}");
                b.set_toc_status(SharedString::from(format!("Save failed: {e}")));
                return;
            }
            refresh_config_list(&ui);
            if let Some(idx) = list_log_configs().iter().position(|c| c.name == cfg.name) {
                b.set_config_index(idx as i32);
            }
            b.set_show_new_config(false);
        });
    }
}
