//! A made-up swarm of 49 units, for screenshots of the UI without
//! Crazyflies (see `host::screenshot`). The same swarm as in the design
//! mockups: a few units need attention, the rest are ready or charging.

use std::sync::Arc;

use slint::{ModelRc, VecModel};

use crate::host::rendering::SceneSources;
use crate::loco::PositioningData;
use crate::planning;
use crate::units::rebuild_table_rows;
use crate::view3d::VizSceneState;
use crate::{AppWindow, UnitData, UnitState};

/// Supervisor bits, as in `units::supervisor_text`.
const TUMBLED: i32 = 0x20;
const LOCKED: i32 = 0x40;

/// A small deterministic generator, so every screenshot shows the same swarm.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> f32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((self.0 >> 33) as f32) / (1u64 << 31) as f32
    }
}

pub(crate) fn units() -> Vec<UnitData> {
    let mut rng = Rng(20261028);
    (1..=49)
        .map(|i: i32| {
            let mut u = UnitData {
                name: format!("CF-{:02}", i).into(),
                uri: format!("radio://0/84/2M/D91F7001{:02X}?link_timeout=3000", i).into(),
                state: UnitState::Connected,
                pos_x: ((i - 1) % 7) as f32 * 0.5 - 1.5,
                pos_y: ((i - 1) / 7) as f32 * 0.5 - 1.5,
                pos_z: 0.0,
                battery_voltage: 3.85 + rng.next() * 0.3,
                deck_lighthouse: true,
                deck_led_top: i <= 8,
                serial: format!("{:08X}", 0x2B4A_0000u32 + i as u32 * 7919).into(),
                pm_state: "Battery".into(),
                supervisor_state: "Ready to fly".into(),
                platform_type: "Crazyflie 2.1+".into(),
                firmware_version: "2025.09".into(),
                link_quality: 0.9 + rng.next() * 0.1,
                uplink_rate: 95.0 + rng.next() * 10.0,
                downlink_rate: 95.0 + rng.next() * 10.0,
                radio_send_rate: 180.0 + rng.next() * 40.0,
                avg_retries: rng.next() * 0.3,
                selftest_passed: true,
                radio: (i - 1) % 4,
                ..Default::default()
            };
            if i >= 41 {
                // On a charger, in a row beside the grid
                u.pos_x = 2.5;
                u.pos_y = (i - 45) as f32 * 0.5;
            }
            u.pos_z = 0.01 + rng.next() * 0.02;
            match i {
                12 => {
                    u.battery_voltage = 3.52;
                    u.pm_state = "Low power".into();
                }
                17 => {
                    u.state = UnitState::Error;
                    u.error_message = "Link timed out".into();
                    u.link_quality = 0.0;
                }
                23 => {
                    u.selftest_passed = false;
                    u.supervisor_state = "Selftest failed".into();
                }
                29 => u.firmware_version = "2025.02".into(),
                31 => {
                    u.state = UnitState::Crashed;
                    u.supervisor_info = TUMBLED | LOCKED;
                    u.supervisor_state = "Tumbled".into();
                }
                35 => u.link_quality = 0.64,
                38 => {
                    u.state = UnitState::Disconnected;
                    u.link_quality = 0.0;
                    u.battery_voltage = 0.0;
                }
                41..=43 => {
                    u.state = UnitState::Charged;
                    u.battery_voltage = 4.18;
                    u.pm_state = "Charged".into();
                }
                44..=49 => {
                    u.state = UnitState::Charging;
                    u.battery_voltage = 3.7 + rng.next() * 0.4;
                    u.pm_state = "Charging".into();
                }
                _ => {}
            }
            u
        })
        .collect()
}

/// The room the demo swarm stands in, as in the design: a 4 × 4 m flight
/// area, a pad under every unit of the grid and four base stations.
pub(crate) fn scene() -> SceneSources {
    let mut positioning = PositioningData::default();
    positioning.lighthouse_bs = vec![
        (0, [-2.8, -2.8, 2.6]),
        (1, [2.8, -2.8, 2.6]),
        (2, [2.8, 2.8, 2.6]),
        (3, [-2.8, 2.8, 2.6]),
    ];
    positioning.lighthouse_active = 0b1111;
    let room = VizSceneState {
        flight_areas: vec![planning::BoxVolume::new([-2.0, -2.0, 0.0], [2.0, 2.0, 2.2])],
        takeoff_pads: (0..49)
            .map(|i| planning::TakeoffPad {
                x: (i % 7) as f32 * 0.5 - 1.5,
                y: (i / 7) as f32 * 0.5 - 1.5,
                radius: 0.14,
                label: String::new(),
            })
            .collect(),
        ..Default::default()
    };
    SceneSources {
        positioning_data: Arc::new(tokio::sync::Mutex::new(positioning)),
        trajectory_data: Default::default(),
        goto_targets: Default::default(),
        viz_scene_state: Arc::new(std::sync::Mutex::new(room)),
    }
}

/// Show the demo swarm, connected, in `ui`. When `flying`, the units that
/// are ready fly in three layers, as in the design.
pub(crate) fn show(ui: &AppWindow, flying: bool) {
    ui.set_swarm_name("49-unit swarm".into());
    ui.set_swarm_source("default.yaml".into());
    let mut units = units();
    if flying {
        for (i, u) in units.iter_mut().enumerate() {
            if u.state == UnitState::Connected && u.selftest_passed && u.supervisor_info == 0 && u.pm_state == "Battery" {
                u.state = UnitState::Flying;
                u.pos_z = 0.9 + (i / 7 % 3) as f32 * 0.3;
                u.battery_voltage -= 0.17;
            }
        }
    }
    ui.set_units(ModelRc::new(VecModel::from(units)));
    ui.set_swarm_connected(true);
    ui.set_drives_radios(true);
    ui.set_connection_mode("shared".into());
    ui.set_connection_label("This computer's Crazyradios, shared on port 7777".into());
    ui.set_signed_in(true);
    ui.set_needs_sign_in(false);
    ui.set_account_label("Test Pilot".into());
    ui.set_account_initials("TP".into());
    ui.set_account_text("Signed in as pilot@example.com".into());
    let servers: Vec<slint::SharedString> = vec!["lab-pc:7777".into(), "arena-connection-server".into()];
    ui.set_recent_servers(ModelRc::new(VecModel::from(servers)));
    // Swarms to open (the top bar's swarm switcher)
    let choice = |id: &str, name: &str, units, detail: &str, shared, current| crate::SwarmChoice {
        id: id.into(),
        name: name.into(),
        units,
        detail: detail.into(),
        shared,
        current,
    };
    ui.set_os_local(ModelRc::new(VecModel::from(vec![
        choice("default", "49-unit swarm", 49, "swarms/default.yaml", false, true),
        choice("fishtank", "Fishtank", 9, "swarms/fishtank.yaml", false, false),
        choice("arena", "Arena demo", 25, "swarms/arena.yaml", false, false),
    ])));
    ui.set_os_shared(ModelRc::new(VecModel::from(vec![
        choice("bitcraze/fishtank", "Fishtank", 9, "revision 12", true, false),
        choice("bitcraze/arena", "Arena demo", 25, "revision 4", true, false),
    ])));
    ui.set_os_shared_title("Shared on arc.bitcraze.io".into());
    ui.set_os_selected_id("fishtank".into());
    crate::status::seed_demo_traffic(4);
    rebuild_table_rows(ui);
    use crate::Health;
    for (text, tone) in [
        ("Connected 47 of 49 units in 2.4 s", Health::Ready),
        ("CF-17: timed out", Health::Fault),
        ("CF-31: tumbled", Health::Fault),
        ("Radio 2: retries up to 0.8 for 40 s", Health::Warn),
        ("cfcli on lab-pc attached to CF-05", Health::Ready),
    ] {
        crate::status::log(text, tone);
    }
    rebuild_table_rows(ui);
}
