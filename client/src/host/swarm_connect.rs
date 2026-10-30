//! Connecting to many Crazyflies at once, the way cfcli's swarm runner does.
//!
//! All Crazyflies connect at the same time, also the ones that share a
//! Crazyradio. Connecting a few at a time is slower: the ones already
//! connected keep polling the radio while the next ones connect (25 on one
//! radio: 3.0 s at 8 at a time, 1.4 s all at once, 2026-10-19).
//!
//! Crazyflies running the same firmware have the same parameter and log
//! TOCs, so downloading them from more than one Crazyflie is wasted radio
//! time. [`connect_all`] connects using only the TOC cache at first, lets one
//! Crazyflie per TOC checksum download what is missing, and then connects the
//! rest from the cache.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crazyflie_lib::{Crazyflie, TocCache};
use crazyflie_link::LinkContext;
use tokio::task::JoinSet;

/// Time for a connection that only reads TOCs from the cache.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Time for a connection that downloads TOCs.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(90);

/// One Crazyflie to connect to.
pub struct Target {
    /// The caller's index for it, handed back with the result.
    pub index: usize,
    /// The URI to connect with: a real Crazyradio and the link timeout.
    pub link_uri: String,
}

/// Called with the caller's index and the result of each connection.
pub type Connected = Arc<dyn Fn(usize, Result<Crazyflie, String>) + Send + Sync>;

/// Called with the caller's index of each Crazyflie that waits for another
/// one to download a TOC it needs.
pub type Waiting = Arc<dyn Fn(usize) + Send + Sync>;

/// Connect to every target and hand each result to `connected` as soon as it
/// is there, in the order they finish. Each TOC missing from the cache is
/// downloaded from one Crazyflie only; the others with the same TOC wait for
/// it and then connect from the cache.
pub async fn connect_all<C>(
    link_context: Arc<LinkContext>,
    toc_cache: C,
    targets: Vec<Target>,
    connected: Connected,
    waiting: Waiting,
) where
    C: TocCache + Clone + Send + Sync + 'static,
{
    let targets = Arc::new(targets);

    let mut pending: Vec<usize> = (0..targets.len()).collect();
    while !pending.is_empty() {
        // Every pending Crazyflie whose TOCs are all in the cache.
        let mut probes = JoinSet::new();
        for i in pending {
            let (link_context, toc_cache, targets, connected) =
                (link_context.clone(), toc_cache.clone(), targets.clone(), connected.clone());
            probes.spawn(async move {
                let probe = ProbeCache::new(toc_cache);
                match connect(&link_context, &targets[i].link_uri, probe.clone(), CONNECT_TIMEOUT).await {
                    Ok(cf) => {
                        let missed = probe.missed();
                        if missed.is_empty() {
                            connected(targets[i].index, Ok(cf));
                            None
                        } else {
                            cf.disconnect().await;
                            Some((i, missed))
                        }
                    }
                    Err(e) => {
                        connected(targets[i].index, Err(e));
                        None
                    }
                }
            });
        }
        let mut missing: Vec<(usize, Vec<Vec<u8>>)> = probes.join_all().await.into_iter().flatten().collect();
        missing.sort_unstable();

        // One Crazyflie per missing TOC downloads it, the others with the
        // same TOC try again once it is in the cache. Every round finishes at
        // least one Crazyflie per missing TOC, so this ends.
        let mut seen: Vec<Vec<Vec<u8>>> = Vec::new();
        let mut downloads = JoinSet::new();
        pending = Vec::new();
        for (i, keys) in missing {
            if seen.contains(&keys) {
                waiting(targets[i].index);
                pending.push(i);
                continue;
            }
            seen.push(keys);
            let (link_context, toc_cache, targets, connected) =
                (link_context.clone(), toc_cache.clone(), targets.clone(), connected.clone());
            downloads.spawn(async move {
                let result = connect(&link_context, &targets[i].link_uri, toc_cache, DOWNLOAD_TIMEOUT).await;
                connected(targets[i].index, result);
            });
        }
        downloads.join_all().await;
    }
}

async fn connect<C: TocCache + Send + Sync + 'static>(
    link_context: &LinkContext,
    link_uri: &str,
    toc_cache: C,
    timeout: Duration,
) -> Result<Crazyflie, String> {
    match tokio::time::timeout(timeout, Crazyflie::connect_from_uri(link_context, link_uri, toc_cache)).await {
        Ok(Ok(cf)) => Ok(cf),
        Ok(Err(e)) => {
            eprintln!("Failed to connect to {}: {:?}", link_uri, e);
            Err(format!("{}", e))
        }
        Err(_) => {
            eprintln!("Connection to {} timed out", link_uri);
            Err("Connection timed out".to_string())
        }
    }
}

/// A TOC cache that never makes the Crazyflie download a TOC. A miss is
/// noted and answered with an empty TOC, so the connection finishes right
/// away (an empty parameter TOC also skips reading all the values); the
/// caller then disconnects and tries again once the TOC is in the cache.
#[derive(Clone)]
struct ProbeCache<C> {
    inner: C,
    missed: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl<C: TocCache> ProbeCache<C> {
    fn new(inner: C) -> Self {
        ProbeCache { inner, missed: Arc::default() }
    }

    /// The keys that weren't in the cache, sorted so that two Crazyflies
    /// missing the same TOCs compare equal.
    fn missed(&self) -> Vec<Vec<u8>> {
        let mut missed = self.missed.lock().unwrap().clone();
        missed.sort();
        missed
    }
}

impl<C: TocCache> TocCache for ProbeCache<C> {
    fn get_toc(&self, key: &[u8]) -> Option<String> {
        self.inner.get_toc(key).or_else(|| {
            self.missed.lock().unwrap().push(key.to_vec());
            Some("{}".to_string())
        })
    }

    fn store_toc(&self, key: &[u8], toc: &str) {
        self.inner.store_toc(key, toc);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[derive(Clone, Default)]
    struct MapCache(Arc<Mutex<HashMap<Vec<u8>, String>>>);

    impl TocCache for MapCache {
        fn get_toc(&self, key: &[u8]) -> Option<String> {
            self.0.lock().unwrap().get(key).cloned()
        }

        fn store_toc(&self, key: &[u8], toc: &str) {
            self.0.lock().unwrap().insert(key.to_vec(), toc.to_string());
        }
    }

    #[test]
    fn probe_cache_answers_a_miss_with_an_empty_toc() {
        let cache = MapCache::default();
        cache.store_toc(&[1, 0xaa, 0xbb, 0xcc, 0xdd], "{\"a\":[1,8]}");
        let probe = ProbeCache::new(cache);
        assert_eq!(probe.get_toc(&[1, 0xaa, 0xbb, 0xcc, 0xdd]).as_deref(), Some("{\"a\":[1,8]}"));
        assert!(probe.missed().is_empty());
        assert_eq!(probe.get_toc(&[1, 2, 3, 4, 5]).as_deref(), Some("{}"));
        assert_eq!(probe.get_toc(&[1, 0, 0, 0, 0]).as_deref(), Some("{}"));
        assert_eq!(probe.missed(), vec![vec![1, 0, 0, 0, 0], vec![1, 2, 3, 4, 5]]);
    }

    #[test]
    fn an_empty_toc_parses_as_a_toc() {
        // What crazyflie-lib does with a cached TOC.
        let toc: std::collections::BTreeMap<String, (u16, u8)> = serde_json::from_str("{}").unwrap();
        assert!(toc.is_empty());
    }
}

/// Serve a unit connected here to the client API, and to other programs in
/// shared mode. Its link is closed when that fails.
pub async fn adopt(
    client: &crazyflie_connection_server::Client,
    uri: &str,
    cf: crazyflie_lib::Crazyflie,
) -> crazyflie_connection_server::Result<crazyflie_connection_server::Crazyflie> {
    let lib = Arc::new(cf);
    let adopted = client.adopt(uri, lib.clone()).await;
    if adopted.is_err() {
        lib.disconnect().await;
    }
    adopted
}

/// Disconnect a unit, or through a connection server detach from it. Its
/// telemetry and console streams end either way.
pub async fn disconnect(cf: &crazyflie_connection_server::Crazyflie) {
    let lib = cf.lib();
    cf.detach();
    if let Some(lib) = lib {
        lib.disconnect().await;
    }
}
