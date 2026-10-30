//! The connection server in this process, for `shared` and `direct` mode.

use std::sync::Arc;
use std::time::Duration;

use crazyflie_connection_server::{Client, InProcess, ServerConfig};

use crate::host::radios::LinkContext;
use crate::host::toc_cache::FileTocCache;

/// Open `mode` (see [`crate::connection`]). In `shared` and `direct` mode
/// the server runs in this process on `link_context`, the Crazyradios
/// Swarmkeeper's own features use too.
pub async fn open(
    mode: &str,
    name: &str,
    link_context: Arc<LinkContext>,
    toc_cache: FileTocCache,
) -> crazyflie_connection_server::Result<Client> {
    let in_process = InProcess {
        config: ServerConfig {
            name: name.to_string(),
            linger: Duration::from_secs(10),
            // Swarmkeeper's own features (sniffer, radio test, flashing)
            // open Crazyradios too: the server only keeps the ones it uses.
            hold_radios: false,
            toc_cache: None,
            link_context: Some(link_context),
        }
        .with_toc_cache(toc_cache),
        auto_shared: true,
    };
    Client::open_with(mode, in_process).await
}
