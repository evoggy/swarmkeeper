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

Rust desktop application with UI for swarm management.

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
