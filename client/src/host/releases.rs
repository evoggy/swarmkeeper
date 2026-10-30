//! Crazyflie firmware releases from GitHub: the list, and the release zips,
//! downloaded into `firmware/` in the data folder and kept for next time.
//! A browser build would fetch the same URLs.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::firmware::{Asset, Release, RELEASE_REPOSITORY};
use crate::host;

/// Releases listed, the newest first.
const LISTED: usize = 10;

fn http() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!("Swarmkeeper/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .build()?)
}

/// The firmware releases on GitHub, the newest first, without pre-releases.
pub async fn list() -> Result<Vec<Release>> {
    let url = format!("https://api.github.com/repos/{RELEASE_REPOSITORY}/releases?per_page={LISTED}");
    let response = http()?
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .context("can't reach GitHub")?;
    if !response.status().is_success() {
        bail!("GitHub answered {}", response.status());
    }
    let releases: Vec<Release> = response.json().await.context("reading the releases from GitHub")?;
    Ok(releases.into_iter().filter(|r| !r.prerelease).collect())
}

/// Where a release zip is kept.
pub fn cached_path(asset: &Asset) -> PathBuf {
    host::env::data_dir().join("firmware").join(&asset.name)
}

/// The release zip, if it was downloaded whole before.
pub fn cached(asset: &Asset) -> Option<Vec<u8>> {
    host::fs::read(cached_path(asset)).ok().filter(|data| data.len() as u64 == asset.size)
}

/// Download a release zip, reporting `(bytes so far, of)`, and keep it.
pub async fn download(asset: &Asset, mut progress: impl FnMut(u64, u64)) -> Result<Vec<u8>> {
    if let Some(data) = cached(asset) {
        progress(asset.size, asset.size);
        return Ok(data);
    }
    let mut response = http()?
        .get(&asset.browser_download_url)
        .send()
        .await
        .with_context(|| format!("can't download {}", asset.name))?;
    if !response.status().is_success() {
        bail!("downloading {}: {}", asset.name, response.status());
    }
    let total = response.content_length().unwrap_or(asset.size);
    let mut data = Vec::with_capacity(total as usize);
    progress(0, total);
    while let Some(chunk) = response.chunk().await.with_context(|| format!("downloading {}", asset.name))? {
        data.extend_from_slice(&chunk);
        progress(data.len() as u64, total);
    }
    let path = cached_path(asset);
    if let Some(dir) = path.parent() {
        host::fs::create_dir_all(dir).ok();
    }
    if let Err(e) = host::fs::write(&path, &data) {
        eprintln!("Keeping {}: {}", path.display(), e);
    }
    Ok(data)
}
