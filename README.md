# Swarmkeeper

A tool for creating, configuring, and maintaining Crazyflie swarms. For some specific functionality
there's also an accompanying application.

Still very much work in progress...

## Components

### Firmware

Out-of-tree Crazyflie firmware application providing swarm-specific functionality.

**Building:**

```bash
cd firmware/swarmkeeper-app
CRAZYFLIE_BASE=<path-to-crazyflie-firmare> make
```

### Client

Rust desktop application for managing and operating Crazyflie swarms. Built with Slint for the UI and OpenGL for 3D visualization.

**Features:**

- **Unit management** -- connect to a swarm of Crazyflies, monitor status (battery, position, state), upload trajectories, and control flight (takeoff, goto, land, emergency stop)
- **Radio test** -- measure link quality per channel for each Crazyflie in the swarm
- **3D visualization** -- real-time view of Crazyflie positions, Lighthouse base stations, and Loco anchors with labels
- **Lighthouse coverage** -- place base stations, configure room dimensions and offsets, and compute/visualize which areas have coverage from 0--4+ base stations. Includes receiver FOV and tilt compensation settings. Load base station geometry from Crazyflie config files
- **Lighthouse wizard** -- guided step-by-step calibration of Lighthouse base station geometry from measurement samples, with a built-in solver (WIP)
- **TDoA3 coverage** -- place Loco anchors and compute GDOP / positioning error metrics (GDOP, HDOP, VDOP, per-axis error, sensitivity) with color-mapped voxel visualization
- **Planning** -- combined scene editor for designing a positioning system. Place Lighthouse base stations, TDoA3 anchors, and opaque geometric obstacles (boxes, cylinders with per-object color). Computes combined LH + TDoA3 coverage with obstacle occlusion. Import scenes from the individual LH/TDoA3 tabs. Save/load planning scenes as YAML

**Install (Linux, x86_64 and arm64):**

```bash
curl -fsSL https://evoggy.github.io/packages/apt/evoggy.gpg.key \
  | sudo gpg --dearmor -o /usr/share/keyrings/evoggy-archive-keyring.gpg
echo "deb [arch=amd64,arm64 signed-by=/usr/share/keyrings/evoggy-archive-keyring.gpg] https://evoggy.github.io/packages/apt stable main" \
  | sudo tee /etc/apt/sources.list.d/evoggy.list
sudo apt update && sudo apt install swarmkeeper
```

The package adds Swarmkeeper to the application menu and installs the udev
rules for the Crazyradio. Started that way, swarms, scenes, plans, log configs,
journals and recordings are kept in `~/Documents/Swarmkeeper`; run from
`client/` in a checkout, they stay in the checkout as before.

**System Dependencies (Linux):**

```bash
# Ubuntu/Debian
sudo apt install libfontconfig1-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev libxkbcommon-dev

# Fedora
sudo dnf install fontconfig-devel libxcb-devel libxkbcommon-devel
```

**Building:**

```bash
cd client
cargo build
cargo run
```
