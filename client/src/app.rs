//! State shared by the parts of the app, and small UI helpers.

use std::sync::Arc;

use tokio::sync::Mutex;

use crate::console::SharedConsoleStore;
use crate::journal::SharedJournalStore;
use crate::lh_coverage_ui::{CoverageState, GizmoState};
use crate::loco::SharedPositioningData;
use crate::manual_control::{SharedGamepadIds, SharedManualControl};
use crate::planning_ui::{PlanningGizmoState, PlanningState};
use crate::settings::AppSettings;
use crate::swarm::SwarmState;
use crate::tdoa3_ui::{Tdoa3GizmoState, Tdoa3State};
use crate::host::toc_cache::FileTocCache;
use crate::trajectory::{SharedGotoTargets, SharedTrajectoryData};
use crate::view3d::VizSceneState;
use crate::{AppWindow, connection, lh_wizard, plot, host::radios};
use crate::host;

/// Everything the parts of the app share, built once in `main`.
pub(crate) struct AppContext {
    pub(crate) settings: AppSettings,
    pub(crate) link_context: Arc<host::radios::LinkContext>,
    pub(crate) toc_cache: FileTocCache,
    pub(crate) connection: connection::SharedConnection,
    pub(crate) swarm_state: SwarmState,
    pub(crate) radio_plan: radios::SharedRadioPlan,
    pub(crate) plot_selected_index: plot::SelectedIndex,
    pub(crate) positioning_source: Arc<Mutex<Option<usize>>>,
    pub(crate) positioning_data: SharedPositioningData,
    pub(crate) goto_targets: SharedGotoTargets,
    pub(crate) journal_store: SharedJournalStore,
    pub(crate) console_store: SharedConsoleStore,
    pub(crate) viz_scene_state: Arc<std::sync::Mutex<VizSceneState>>,
    pub(crate) gilrs: Arc<std::sync::Mutex<host::gamepad::Gilrs>>,
    pub(crate) gamepad_ids: SharedGamepadIds,
    pub(crate) manual_control: SharedManualControl,
    pub(crate) trajectory_data: SharedTrajectoryData,
    pub(crate) coverage_state: Arc<std::sync::Mutex<CoverageState>>,
    pub(crate) gizmo_state: Arc<std::sync::Mutex<GizmoState>>,
    pub(crate) tdoa3_state: Arc<std::sync::Mutex<Tdoa3State>>,
    pub(crate) tdoa3_gizmo_state: Arc<std::sync::Mutex<Tdoa3GizmoState>>,
    pub(crate) planning_state: Arc<std::sync::Mutex<PlanningState>>,
    pub(crate) planning_gizmo_state: Arc<std::sync::Mutex<PlanningGizmoState>>,
    pub(crate) wizard_state: Arc<std::sync::Mutex<lh_wizard::LhWizardState>>,
}

/// Show a modal error/alert dialog with the given title and message. Safe to call
/// from async tasks; the work is marshalled onto the Slint event loop.
pub(crate) fn show_error_dialog(ui_weak: &slint::Weak<AppWindow>, title: String, message: String) {
    let ui_weak = ui_weak.clone();
    slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak.upgrade() {
            ui.set_error_dialog_title(title.into());
            ui.set_error_dialog_message(message.into());
            ui.set_error_dialog_visible(true);
        }
    })
    .ok();
}
