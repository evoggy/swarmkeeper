//! How Swarmkeeper reaches the Crazyflies, chosen in the connection menu at
//! the right of the menu bar (`CRAZYFLIE_CONNECTION` overrides it):
//!
//! - `shared`: Swarmkeeper drives this computer's Crazyradios, and other
//!   programs (cfcli, scripts) attach to the same Crazyflies through it on
//!   port 7777, for as long as Swarmkeeper runs.
//! - `direct`: the same, without letting other programs in.
//! - `local`: through a connection server that already runs on this
//!   computer (`cfcli server run`). It outlives Swarmkeeper.
//! - an address (`host`, `host:port`): through a connection server on
//!   another computer.
//! - `auto`, the default: `local` if a server runs on this computer, else
//!   `shared`.
//!
//! When Swarmkeeper drives the Crazyradios itself, it connects the swarm with
//! its own radio plan and TOC sharing ([`crate::host::swarm_connect`]) and hands
//! each connection to the server in this process
//! ([`crazyflie_connection_server::Client::adopt`]). Every unit is then used through the
//! client API. The features not ported to it yet use the crazyflie-lib
//! connection behind it ([`crazyflie_connection_server::Crazyflie::lib`]), so they only
//! work in `shared` and `direct` mode.

use std::sync::Arc;
use std::time::Duration;

use crazyflie_connection_server::{Client, CONNECTION_ENV};
use tokio::sync::RwLock;

use crate::host::radios::LinkContext;
use crate::host::{self, server, toc_cache::FileTocCache};
use crate::AppWindow;

/// The mode when none is chosen.
pub const DEFAULT_MODE: &str = "auto";

/// How long to wait for a connection server. A host that doesn't answer
/// at all would otherwise hold up the start for the kernel's ~2 minutes
/// of connect retries, with no window.
const OPEN_TIMEOUT: Duration = Duration::from_secs(5);

/// How many connection servers the settings remember.
const RECENT_SERVERS: usize = 8;

/// The connection, replaced when the user picks another mode.
pub type SharedConnection = Arc<RwLock<Connection>>;

pub struct Connection {
    /// The mode as chosen, e.g. `auto`.
    pub mode: String,
    client: Client,
    /// This client's name, as other clients see it.
    name: String,
    /// Why the chosen mode could not be used, when Swarmkeeper fell back to
    /// `direct`.
    pub problem: Option<String>,
}

/// The mode to use: `CRAZYFLIE_CONNECTION`, else the setting, else [`DEFAULT_MODE`].
pub fn mode(setting: Option<&str>) -> String {
    host::env::var(CONNECTION_ENV)
        .filter(|mode| !mode.is_empty())
        .or_else(|| setting.map(str::to_string))
        .unwrap_or_else(|| DEFAULT_MODE.to_string())
}

impl Connection {
    /// Open `mode`. When that fails (a server that doesn't answer within
    /// [`OPEN_TIMEOUT`], port 7777 taken), fall back to `direct` and keep
    /// the reason in `problem`, so that Swarmkeeper still starts.
    pub async fn open(mode: &str, link_context: Arc<LinkContext>, toc_cache: FileTocCache) -> Connection {
        let name = format!("Swarmkeeper on {}", host::env::hostname());
        let opened = host::task::timeout(OPEN_TIMEOUT, server::open(mode, &name, link_context.clone(), toc_cache.clone()))
            .await
            .unwrap_or_else(|_| Err(crazyflie_connection_server::Error::Transport(format!("no answer within {} s", OPEN_TIMEOUT.as_secs()))));
        let (client, problem) = match opened {
            Ok(client) => (client, None),
            Err(e) => {
                eprintln!("Connection mode '{}': {}. Using this computer's Crazyradios", mode, e);
                let client = server::open("direct", &name, link_context, toc_cache).await.expect("direct mode needs no server");
                (client, Some(e.to_string()))
            }
        };
        if let Err(e) = client.set_name(&name).await {
            eprintln!("Naming this client: {}", e);
        }
        eprintln!("Connection: {} ({})", client.mode(), mode);
        Connection { mode: mode.to_string(), client, name, problem }
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Whether Swarmkeeper drives the Crazyradios itself (`shared` or
    /// `direct`), rather than going through a connection server.
    pub fn drives_radios(&self) -> bool {
        self.client.is_in_process()
    }

    /// Disconnect what the server in this process connected and stop
    /// serving other programs, before switching mode or exiting.
    pub async fn close(&self) {
        self.client.close().await;
    }

    /// Short text for the menu bar.
    pub fn label(&self) -> String {
        match self.client.mode() {
            "shared" => "Radios: this computer, shared".to_string(),
            "direct" if self.problem.is_some() => "Radios: this computer (fallback)".to_string(),
            "direct" => "Radios: this computer".to_string(),
            url => format!("Server: {}", server_host(url)),
        }
    }

    /// What the connection menu says about the current mode.
    pub fn description(&self) -> String {
        let mut text = match self.client.mode() {
            "shared" => format!(
                "Swarmkeeper drives this computer's Crazyradios. Other programs (cfcli, scripts) \
                 can use the same Crazyflies through it on port {} while Swarmkeeper runs.",
                crazyflie_connection_server::DEFAULT_PORT
            ),
            "direct" => "Swarmkeeper drives this computer's Crazyradios, and no other program can use them.".to_string(),
            url => format!(
                "Through the connection server at {}. Features that need the Crazyradios directly \
                 (deck flashing, sniffer, radio test, trajectories, ...) don't work through a server yet.",
                server_host(url)
            ),
        };
        if let Some(problem) = &self.problem {
            text = format!("Could not use '{}': {}.\n{}", self.mode, problem, text);
        }
        text
    }
}

/// Whether `mode` is a connection server on another computer (`host`,
/// `host:port` or a URL) rather than one of the named modes.
pub fn is_remote(mode: &str) -> bool {
    !matches!(mode, "auto" | "shared" | "direct" | "local" | "")
}

/// Put `server` first in the connection servers used before.
pub fn remember_server(recent: &mut Vec<String>, server: &str) {
    recent.retain(|s| s != server);
    recent.insert(0, server.to_string());
    recent.truncate(RECENT_SERVERS);
}

/// Show the connection servers used before.
pub fn show_recent(ui: &AppWindow, recent: &[String]) {
    let servers: Vec<slint::SharedString> = recent.iter().map(|s| s.as_str().into()).collect();
    ui.set_recent_servers(slint::ModelRc::new(slint::VecModel::from(servers)));
}

/// Show the connection in the menu bar.
pub fn show(ui: &AppWindow, connection: &Connection, others: &str) {
    ui.set_connection_label(connection.label().into());
    let text = if others.is_empty() { connection.description() } else { format!("{}\n\n{}", connection.description(), others) };
    ui.set_connection_text(text.into());
    ui.set_connection_mode(connection.mode.clone().into());
    ui.set_drives_radios(connection.drives_radios());
    ui.set_connection_problem(connection.problem.is_some());
}

/// The other programs attached to the Crazyflies the server has, one line
/// per Crazyflie. Empty when there are none.
pub async fn others(connection: &Connection) -> String {
    let Ok(connected) = connection.client.list().await else {
        return String::new();
    };
    let lines: Vec<String> = connected
        .into_iter()
        .filter_map(|cf| {
            let others: Vec<String> = cf.clients.into_iter().filter(|client| *client != connection.name).collect();
            (!others.is_empty()).then(|| format!("{}: {}", cf.uri, others.join(", ")))
        })
        .collect();
    if lines.is_empty() {
        String::new()
    } else {
        format!("Also attached:\n{}", lines.join("\n"))
    }
}

/// `lab-pc:7777` from `ws://lab-pc:7777/`.
fn server_host(url: &str) -> &str {
    url.trim_start_matches("ws://").trim_start_matches("wss://").trim_end_matches('/')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_servers_last_first_without_repeats() {
        let mut recent = Vec::new();
        for server in ["a:7777", "b", "a:7777", "c"] {
            remember_server(&mut recent, server);
        }
        assert_eq!(recent, ["c", "a:7777", "b"]);
        for i in 0..20 {
            remember_server(&mut recent, &format!("host{}", i));
        }
        assert_eq!(recent.len(), RECENT_SERVERS);
        assert_eq!(recent[0], "host19");
        assert!(is_remote("lab-pc:7777") && !is_remote("local") && !is_remote("auto"));
    }
}
