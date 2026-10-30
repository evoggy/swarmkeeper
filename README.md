# Swarmkeeper

A desktop application for setting up, running and maintaining Crazyflie swarms. It runs on
Linux (x86_64 and arm64).

Still very much work in progress...

## Install

Swarmkeeper is installed from the evoggy APT repository, for Debian and Ubuntu on amd64 and arm64.
Add the repository's signing key and the repository once:

```bash
curl -fsSL https://evoggy.github.io/packages/apt/evoggy.gpg.key \
  | sudo gpg --dearmor -o /usr/share/keyrings/evoggy-archive-keyring.gpg
echo "deb [arch=amd64,arm64 signed-by=/usr/share/keyrings/evoggy-archive-keyring.gpg] https://evoggy.github.io/packages/apt stable main" \
  | sudo tee /etc/apt/sources.list.d/evoggy.list
```

Then install it:

```bash
sudo apt update
sudo apt install swarmkeeper
```

New releases come with the rest of the system's updates (`sudo apt update && sudo apt upgrade`).

The `.deb` attached to each [GitHub release](https://github.com/evoggy/swarmkeeper/releases) can
also be installed directly (`sudo apt install ./swarmkeeper_<version>_amd64.deb`). It adds the
repository above itself, so later releases arrive the same way.

The package adds Swarmkeeper to the application menu and installs udev rules that give the
logged-in user access to the Crazyradio and to a Crazyflie over USB; a Crazyradio already plugged
in picks them up at once. Swarm files and the rest of Swarmkeeper's data are kept in
`~/Documents/Swarmkeeper`.

## What it does

- **Status** -- the swarm's state at a glance: how many units are ready, on a charger, need
  attention or don't respond (hover a unit for why), a battery histogram, what needs attention
  with how to fix it, each Crazyradio's units, link quality and traffic, and the session's events
- **Units** -- every unit with its status, battery, link, position, decks, radio and firmware,
  with filters and search. A unit's panel has its details, take off, go to, go home and land,
  the console, and reboot, sleep, wake up and power off
- **Swarm actions** -- arm all, land all, a health test, and reboot, sleep, wake up or power off
  the whole swarm. Land all and Emergency stop are always in the top bar
- **3D view** -- the units' positions in their status colours, with the base stations and anchors
- **Plot** -- log the same variables from one or more units onto one time axis, as a chart per
  variable or one chart for all, and save it as CSV
- **Several Crazyradios** -- a URI with the radio left empty (`radio:///80/2M/E7E7E7E701`) means
  any Crazyradio. Connecting the swarm spreads those Crazyflies over all Crazyradios that can be
  opened, keeping Crazyflies on the same (or a neighbouring) channel on one radio; a number in
  the URI still pins that radio. The same rules as `cfcli`
- **Rechannel** -- spread the swarm over several channels, one per Crazyradio by default
  (Swarm actions > Rechannel swarm...). The dialog shows which units go to which channel, and
  each move as it happens. Works on a swarm file on this computer, which is updated with the new
  URIs
- **Shared TOC download** -- connecting the swarm downloads each parameter/log TOC that isn't
  cached yet from one Crazyflie only; the others with the same firmware connect from the cache
- **Connection server** -- other programs (for example `cfcli` or a script) can use the
  Crazyflies Swarmkeeper is connected to, through
  [crazyflie-connection-server](https://github.com/evoggy/crazyflie-connection-server), and
  Swarmkeeper can use a server on another computer. The connection menu in the top bar picks
  the mode; `CRAZYFLIE_CONNECTION` overrides it. Experimental
- **Shared swarms** -- sign in to a server that shares swarms (the account menu at the right of
  the top bar), and Open swarm lists the swarms shared in your organizations next to the swarm
  files, with the same `<org>/<swarm>` IDs as `cfcli`. A shared swarm opens at its latest
  revision, or from this computer's copy when the server can't be reached
- **Settings** -- a light or dark theme, the connection, the account and flight defaults

## Building from source

System dependencies:

```bash
# Ubuntu/Debian
sudo apt install pkg-config libudev-dev libfontconfig1-dev libxkbcommon-dev

# Fedora
sudo dnf install pkgconf systemd-devel fontconfig-devel libxcb-devel libxkbcommon-devel
```

Build and run:

```bash
cd client
cargo build
cargo run
```

Run from `client/` in a checkout, the swarms and the other data stay in the checkout instead of
`~/Documents/Swarmkeeper`.
