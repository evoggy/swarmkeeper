//! Swarms shared through the server Swarmkeeper is signed in to (see
//! [`crate::host::account`]). Ported from cfcli's `documents` module, and laid out
//! on disk the same way.
//!
//! A shared swarm's ID is `<org>/<swarm>`. Swarmkeeper keeps a copy of each
//! shared swarm it opens in `synced/<server>/swarms/<org>/<swarm>.yaml` in
//! the data folder, outside `swarms/`, so the copies never mix with the swarm
//! files there. `synced/<server>/state.json` says which revision each copy
//! is.
//!
//! Opening a shared swarm asks the server for the latest revision first,
//! which costs a 304 when the copy is current. When the server can't be
//! reached, the copy is used, so working with the Crazyflies never waits for
//! the internet.
//!
//! The `<org>` in an ID is the user's own ID for the organization, which they
//! can change on the server. `synced/<server>/orgs.json` remembers the ID of
//! each organization (the server lists their UUIDs, which never change), so
//! when one changes, the copies move to the new ID, and the old ID says what
//! the new one is.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use reqwest::{header, RequestBuilder, Response, StatusCode};
use serde::{Deserialize, Serialize};

use crate::host::account::{self, Credentials, KeyRejected};
use crate::host;

/// The folder of the copies, the key in the state file and the server's API
/// path.
const FOLDER: &str = "swarms";

/// A shared swarm's ID, `<org>/<swarm>`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SharedId {
    pub org: String,
    pub name: String,
}

impl SharedId {
    /// `<org>/<swarm>`, or None for anything else.
    pub fn parse(id: &str) -> Option<Self> {
        let (org, name) = id.split_once('/')?;
        let org_ok = !org.is_empty() && org.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        (org_ok && is_id(name)).then(|| SharedId { org: org.to_string(), name: name.to_string() })
    }
}

impl fmt::Display for SharedId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.org, self.name)
    }
}

/// IDs are file names: letters, digits, '-', '_' and '.', not first.
fn is_id(id: &str) -> bool {
    !id.is_empty() && !id.starts_with('.') && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// The swarm isn't on the server: it was deleted, or the user left its
/// organization. Its copy is gone too.
#[derive(Debug)]
pub struct Gone {
    pub id: SharedId,
    pub host: String,
}

impl fmt::Display for Gone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "'{}' isn't on {} any more: it was deleted, or you are no longer in its organization",
            self.id, self.host
        )
    }
}

impl std::error::Error for Gone {}

/// What Swarmkeeper knows about its copy of a shared swarm.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
struct CopyState {
    /// The server's revision the copy is.
    revision: i32,
}

/// `state.json`: the copies of each kind, by `<org>/<id>`. Only swarms
/// here, but cfcli keeps lighthouse configurations the same way.
type State = BTreeMap<String, BTreeMap<String, CopyState>>;

/// `orgs.json`: the user's IDs for their organizations.
#[derive(Debug, Default, Serialize, Deserialize)]
struct OrgIds {
    /// The ID of each organization, by UUID, as the server last listed it.
    #[serde(default)]
    ids: BTreeMap<String, String>,
    /// Old IDs and what they became.
    #[serde(default)]
    renamed: BTreeMap<String, String>,
}

/// A shared swarm in a list.
#[derive(Debug, Clone)]
pub struct Listed {
    pub id: SharedId,
    pub name: String,
    pub units: usize,
    pub revision: i32,
}

/// The shared swarms.
pub struct Listing {
    pub swarms: Vec<Listed>,
    /// The server couldn't be reached: these are the copies.
    pub offline: bool,
    /// Organizations whose ID the user changed, (old, new).
    pub renamed: Vec<(String, String)>,
}

/// A shared swarm, opened.
pub struct Opened {
    /// The ID now: another one than asked for when the user changed the ID
    /// of its organization.
    pub id: SharedId,
    pub yaml: String,
    pub revision: i32,
    /// The server couldn't be reached: this is the copy.
    pub offline: bool,
}

/// What asking the server for a swarm came to.
enum Fetch {
    /// The copy is the server's latest now.
    Current,
    Offline,
    /// The server doesn't have it (for the user).
    Missing,
}

/// The shared swarms on the server Swarmkeeper is signed in to. Made for
/// one task: once the server doesn't answer, it isn't asked again.
pub struct Shared {
    credentials: Credentials,
    /// `synced/<server>` in the data folder.
    dir: PathBuf,
    client: reqwest::Client,
    offline: AtomicBool,
}

impl Shared {
    /// None when Swarmkeeper isn't signed in. Doesn't contact the server.
    pub fn open() -> Result<Option<Self>> {
        let Some(credentials) = account::load()? else {
            return Ok(None);
        };
        // "localhost:3000" would be an invalid folder name on Windows.
        let host = account::host(&credentials.server).replace([':', '/', '\\'], "_");
        // The data folder is the working directory (see `enter_data_dir`).
        let dir = Path::new("synced").join(host);
        let client = account::http(Duration::from_secs(15))?;
        Ok(Some(Shared { credentials, dir, client, offline: AtomicBool::new(false) }))
    }

    pub fn host(&self) -> &str {
        account::host(&self.credentials.server)
    }

    // ---- The copies and their state ----

    fn copy_path(&self, id: &SharedId) -> PathBuf {
        self.dir.join(FOLDER).join(&id.org).join(format!("{}.yaml", id.name))
    }

    fn state_path(&self) -> PathBuf {
        self.dir.join("state.json")
    }

    fn state(&self) -> Result<State> {
        read_json(&self.state_path())
    }

    fn save_state(&self, state: &State) -> Result<()> {
        write_atomic(&self.state_path(), serde_json::to_string_pretty(state)?.as_bytes())
    }

    /// The copies, by `<org>/<swarm>`.
    fn copy_states(&self) -> Result<BTreeMap<String, CopyState>> {
        Ok(self.state()?.remove(FOLDER).unwrap_or_default())
    }

    fn copy_state(&self, id: &SharedId) -> Result<Option<CopyState>> {
        Ok(self.copy_states()?.get(&id.to_string()).copied())
    }

    fn set_copy_state(&self, id: &SharedId, copy: Option<CopyState>) -> Result<()> {
        let mut state = self.state()?;
        let copies = state.entry(FOLDER.to_string()).or_default();
        match copy {
            Some(copy) => copies.insert(id.to_string(), copy),
            None => copies.remove(&id.to_string()),
        };
        self.save_state(&state)
    }

    fn has_copy(&self, id: &SharedId) -> bool {
        self.copy_path(id).is_file()
    }

    /// Keep `yaml`, exactly as the server sent it, as the copy of `id`.
    fn keep(&self, id: &SharedId, yaml: &str, revision: i32) -> Result<()> {
        write_atomic(&self.copy_path(id), yaml.as_bytes())?;
        self.set_copy_state(id, Some(CopyState { revision }))
    }

    fn forget(&self, id: &SharedId) -> Result<()> {
        let path = self.copy_path(id);
        match host::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(e).with_context(|| format!("removing {}", path.display()));
            }
            _ => {}
        }
        self.set_copy_state(id, None)
    }

    /// The copy of `id` and its revision, without asking the server.
    pub fn cached(&self, id: &SharedId) -> Result<(String, i32)> {
        let path = self.copy_path(id);
        let yaml = host::fs::read_to_string(&path)
            .with_context(|| format!("this computer has no copy of '{}' from {}", id, self.host()))?;
        Ok((yaml, self.copy_state(id)?.unwrap_or_default().revision))
    }

    /// The copies, without asking the server. `summary` gives a copy's name
    /// and number of Crazyflies.
    pub fn listed_copies(&self, summary: impl Fn(&str) -> Option<(String, usize)>) -> Vec<Listed> {
        let Ok(copies) = self.copy_states() else {
            return Vec::new();
        };
        copies
            .into_iter()
            .filter_map(|(key, copy)| {
                let id = SharedId::parse(&key)?;
                let yaml = host::fs::read_to_string(self.copy_path(&id)).ok()?;
                let (name, units) = summary(&yaml).unwrap_or_else(|| ("(can't read the copy)".to_string(), 0));
                Some(Listed { id, name, units, revision: copy.revision })
            })
            .collect()
    }

    // ---- Organization IDs ----

    fn org_ids(&self) -> Result<OrgIds> {
        read_json(&self.dir.join("orgs.json"))
    }

    /// What `id` is called now, if the user changed the ID of its
    /// organization.
    fn renamed(&self, id: &SharedId) -> Result<Option<SharedId>> {
        let org = self.org_ids()?.renamed.get(&id.org).cloned();
        Ok(org.map(|org| SharedId { org, name: id.name.clone() }))
    }

    /// Follow the organizations whose ID the user changed since the last
    /// list: the copies move to the new ID. Returns the changes, (old, new).
    fn follow_renames(&self, listed: &[(Listed, Option<String>)]) -> Result<Vec<(String, String)>> {
        let mut orgs = self.org_ids()?;
        let before = serde_json::to_string(&orgs)?;
        let mut renames: Vec<(String, String)> = Vec::new();
        for (item, uuid) in listed {
            let Some(uuid) = uuid else { continue };
            if let Some(old) = orgs.ids.insert(uuid.clone(), item.id.org.clone()) {
                if old != item.id.org && !renames.contains(&(old.clone(), item.id.org.clone())) {
                    renames.push((old, item.id.org.clone()));
                }
            }
        }
        for (old, new) in &renames {
            for to in orgs.renamed.values_mut().filter(|to| *to == old) {
                *to = new.clone();
            }
            orgs.renamed.insert(old.clone(), new.clone());
        }
        // An ID in use is no longer an old one.
        for (item, _) in listed {
            orgs.renamed.remove(&item.id.org);
        }
        if serde_json::to_string(&orgs)? != before {
            write_atomic(&self.dir.join("orgs.json"), serde_json::to_string_pretty(&orgs)?.as_bytes())?;
        }
        if renames.is_empty() {
            return Ok(renames);
        }

        // Take all the moving copies out first, so that two organizations
        // can swap IDs. Copies of every kind move, as cfcli does, though
        // Swarmkeeper only keeps swarms. (A swarm's link to a lighthouse
        // configuration keeps the old ID until the next download, which
        // brings the new one; Swarmkeeper doesn't follow the links yet.)
        let mut state = self.state()?;
        let mut moving = Vec::new();
        for (kind, copies) in state.iter_mut() {
            for (old, new) in &renames {
                let ids: Vec<SharedId> = copies
                    .keys()
                    .filter_map(|key| SharedId::parse(key))
                    .filter(|id| id.org == *old)
                    .collect();
                for id in ids {
                    let copy = copies.remove(&id.to_string()).unwrap_or_default();
                    let path = self.dir.join(kind).join(old).join(format!("{}.yaml", id.name));
                    let yaml = match host::fs::read(&path) {
                        Ok(yaml) => {
                            host::fs::remove_file(&path).with_context(|| format!("moving {}", path.display()))?;
                            Some(yaml)
                        }
                        Err(_) => None,
                    };
                    moving.push((kind.clone(), SharedId { org: new.clone(), name: id.name }, copy, yaml));
                }
            }
        }
        for (kind, id, copy, yaml) in moving {
            if let Some(yaml) = yaml {
                write_atomic(&self.dir.join(&kind).join(&id.org).join(format!("{}.yaml", id.name)), &yaml)?;
            }
            state.entry(kind).or_default().insert(id.to_string(), copy);
        }
        self.save_state(&state)?;
        for kind in state.keys() {
            for (old, _) in &renames {
                // Only goes when empty.
                let _ = host::fs::remove_dir(self.dir.join(kind).join(old));
            }
        }
        Ok(renames)
    }

    // ---- Talking to the server ----

    fn url(&self, id: &SharedId) -> Result<reqwest::Url> {
        account::endpoint(&self.credentials.server, &format!("/api/v1/{}/{}/{}", FOLDER, id.org, id.name))
    }

    /// Send a request with the key. None when the server can't be reached.
    async fn send(&self, request: RequestBuilder) -> Result<Option<Response>> {
        if self.offline.load(Ordering::Relaxed) {
            return Ok(None);
        }
        match request.bearer_auth(&self.credentials.key).send().await {
            Ok(response) if response.status() == StatusCode::UNAUTHORIZED => {
                bail!(KeyRejected { host: self.host().to_string(), key_name: self.credentials.key_name.clone() })
            }
            Ok(response) => Ok(Some(response)),
            Err(e) if e.is_connect() || e.is_timeout() => {
                self.offline.store(true, Ordering::Relaxed);
                Ok(None)
            }
            Err(e) => Err(e).with_context(|| format!("talking to {}", self.host())),
        }
    }

    /// The swarms on the server, in all the user's organizations, and the
    /// organizations whose ID changed. None when the server can't be reached.
    async fn server_list(&self) -> Result<Option<(Vec<Listed>, Vec<(String, String)>)>> {
        let url = account::endpoint(&self.credentials.server, &format!("/api/v1/{}", FOLDER))?;
        let Some(response) = self.send(self.client.get(url)).await? else {
            return Ok(None);
        };
        let items: Vec<serde_json::Map<String, serde_json::Value>> =
            account::answer(&self.credentials.server, response).await?;
        let mut listed = Vec::new();
        for item in items {
            let text = |key: &str| item.get(key).and_then(|v| v.as_str()).unwrap_or_default().to_string();
            let Some(id) = SharedId::parse(&text("id")) else {
                continue;
            };
            let number = |key: &str| item.get(key).and_then(|v| v.as_u64()).unwrap_or_default();
            // Servers before per-user organization IDs don't send it.
            let uuid = Some(text("org_uuid")).filter(|uuid| !uuid.is_empty());
            let swarm = Listed { id, name: text("name"), units: number("units") as usize, revision: number("revision") as i32 };
            listed.push((swarm, uuid));
        }
        let renamed = self.follow_renames(&listed)?;
        Ok(Some((listed.into_iter().map(|(swarm, _)| swarm).collect(), renamed)))
    }

    /// Download `id` into its copy unless the copy is current.
    async fn fetch(&self, id: &SharedId) -> Result<Fetch> {
        let copy = self.copy_state(id)?;
        let mut request = self.client.get(self.url(id)?);
        if let Some(copy) = copy.filter(|c| c.revision > 0 && self.has_copy(id)) {
            request = request.header(header::IF_NONE_MATCH, format!("\"{}\"", copy.revision));
        }
        let Some(response) = self.send(request).await? else {
            return Ok(Fetch::Offline);
        };
        match response.status() {
            StatusCode::NOT_MODIFIED => Ok(Fetch::Current),
            StatusCode::NOT_FOUND => Ok(Fetch::Missing),
            status if status.is_success() => {
                let revision = etag_revision(&response)?;
                let yaml = response.text().await.with_context(|| format!("reading '{}' from {}", id, self.host()))?;
                self.keep(id, &yaml, revision)?;
                Ok(Fetch::Current)
            }
            _ => account::answer::<serde_json::Value>(&self.credentials.server, response).await.map(|_| Fetch::Current),
        }
    }

    // ---- What Swarmkeeper does ----

    /// The shared swarms: from the server, or the copies when it can't be
    /// reached. Copies of swarms gone from the server go too.
    pub async fn list(&self, summary: impl Fn(&str) -> Option<(String, usize)>) -> Result<Listing> {
        let Some((mut swarms, renamed)) = self.server_list().await? else {
            return Ok(Listing { swarms: self.listed_copies(summary), offline: true, renamed: Vec::new() });
        };
        for key in self.copy_states()?.keys() {
            if let Some(id) = SharedId::parse(key) {
                if !swarms.iter().any(|s| s.id == id) {
                    self.forget(&id)?;
                }
            }
        }
        swarms.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(Listing { swarms, offline: false, renamed })
    }

    /// The latest revision of a shared swarm, kept as its copy; the copy when
    /// the server can't be reached. Follows the organization to its new ID
    /// when the user changed it. Fails with [`Gone`] when the server doesn't
    /// have it.
    pub async fn load(&self, id: &SharedId) -> Result<Opened> {
        let mut id = self.renamed(id)?.unwrap_or_else(|| id.clone());
        let mut listed = false;
        // Learn the organization's UUID before keeping a copy in it: without
        // it, a later change of the organization's ID looks like the swarm
        // was deleted.
        if !self.org_ids()?.ids.values().any(|org| *org == id.org) {
            listed = self.server_list().await?.is_some();
        }
        loop {
            match self.fetch(&id).await? {
                Fetch::Current => {
                    let (yaml, revision) = self.cached(&id)?;
                    return Ok(Opened { id, yaml, revision, offline: false });
                }
                Fetch::Offline => {
                    if !self.has_copy(&id) {
                        bail!("can't reach {}, and this computer has no copy of '{}'", self.host(), id);
                    }
                    let (yaml, revision) = self.cached(&id)?;
                    return Ok(Opened { id, yaml, revision, offline: true });
                }
                Fetch::Missing if !listed => {
                    // Perhaps the user changed the organization's ID: the
                    // list says, and moves the copies.
                    listed = true;
                    self.server_list().await?;
                    if let Some(now) = self.renamed(&id)? {
                        id = now;
                        continue;
                    }
                    self.forget(&id)?;
                    bail!(Gone { id, host: self.host().to_string() });
                }
                Fetch::Missing => {
                    self.forget(&id)?;
                    bail!(Gone { id, host: self.host().to_string() });
                }
            }
        }
    }

    /// What the server's ID for `id` is now, without asking the server: the
    /// last list's.
    pub fn current_id(&self, id: &SharedId) -> SharedId {
        self.renamed(id).ok().flatten().unwrap_or_else(|| id.clone())
    }
}

fn read_json<T: Default + serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    match host::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).with_context(|| format!("reading {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Write a file, replacing any old one in one step.
fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    host::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = dir.join(format!(".{}.tmp", name));
    host::fs::write(&tmp, contents).with_context(|| format!("writing {}", tmp.display()))?;
    host::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

/// The revision in a response's ETag, `"N"`.
fn etag_revision(response: &Response) -> Result<i32> {
    response
        .headers()
        .get(header::ETAG)
        .and_then(|etag| etag.to_str().ok())
        .and_then(|etag| etag.trim_start_matches("W/").trim_matches('"').parse().ok())
        .context("the server sent a swarm without its revision")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_shared_ids() {
        assert_eq!(
            SharedId::parse("bitcraze-lab/cage"),
            Some(SharedId { org: "bitcraze-lab".into(), name: "cage".into() })
        );
        assert_eq!(SharedId::parse("cage"), None);
        assert_eq!(SharedId::parse("Bitcraze/cage"), None);
        assert_eq!(SharedId::parse("lab/.hidden"), None);
        assert_eq!(SharedId::parse("lab/a/b"), None);
    }

    fn shared_in(dir: &Path) -> Shared {
        Shared {
            credentials: Credentials { server: "http://127.0.0.1:9".into(), key: "k".into(), key_name: "test".into() },
            dir: dir.to_path_buf(),
            client: reqwest::Client::new(),
            offline: AtomicBool::new(false),
        }
    }

    fn listed(org: &str, name: &str, uuid: &str) -> (Listed, Option<String>) {
        let id = SharedId { org: org.into(), name: name.into() };
        (Listed { id, name: name.into(), units: 1, revision: 1 }, Some(uuid.into()))
    }

    #[test]
    fn copies_follow_a_changed_organization_id() {
        let dir = std::env::temp_dir().join(format!("swarmkeeper-shared-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let shared = shared_in(&dir);
        let cage = SharedId::parse("lab/cage").unwrap();
        shared.keep(&cage, "name: Cage\nunits: []\n", 3).unwrap();
        assert!(shared.follow_renames(&[listed("lab", "cage", "u1")]).unwrap().is_empty());

        let renames = shared.follow_renames(&[listed("bench", "cage", "u1")]).unwrap();
        assert_eq!(renames, vec![("lab".to_string(), "bench".to_string())]);
        let moved = SharedId::parse("bench/cage").unwrap();
        assert_eq!(shared.cached(&moved).unwrap(), ("name: Cage\nunits: []\n".to_string(), 3));
        assert!(!shared.has_copy(&cage));
        assert_eq!(shared.current_id(&cage), moved);

        // The old ID in use again is no longer an old one.
        shared.follow_renames(&[listed("lab", "other", "u2"), listed("bench", "cage", "u1")]).unwrap();
        assert_eq!(shared.current_id(&cage), cage);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn uses_the_copy_when_the_server_cant_be_reached() {
        let dir = std::env::temp_dir().join(format!("swarmkeeper-offline-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let shared = shared_in(&dir);
        let cage = SharedId::parse("lab/cage").unwrap();
        assert!(shared.load(&cage).await.is_err());
        shared.keep(&cage, "name: Cage\nunits: []\n", 2).unwrap();
        let opened = shared.load(&cage).await.unwrap();
        assert!(opened.offline);
        assert_eq!(opened.revision, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
