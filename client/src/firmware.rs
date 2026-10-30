//! Crazyflie firmware releases: what a release .zip holds, as cfcli reads
//! it (`utils/firmware.rs`), and which release file fits a platform.
//!
//! A release zip has a `manifest.json` naming, for each file, the targets it
//! goes to (`stm32`, `nrf51`, `bcLighthouse4`, `deckctrl`, ...) and its type
//! (`fw`, `cfg`, `bootloader+softdevice`). The releases are published on
//! GitHub as `firmware-<platform>-<release>.zip` (host/releases.rs).

use std::collections::HashMap;
use std::io::Read;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Deserializer};

/// The GitHub repository the releases are published in.
pub const RELEASE_REPOSITORY: &str = "bitcraze/crazyflie-release";

/// Manifest `type` of the nRF51 bootloader and softdevice bundle. cfcli only
/// flashes it when the nRF51 firmware needs a softdevice the Crazyflie lacks;
/// Swarmkeeper leaves it out.
const NRF51_SOFTDEVICE_TYPE: &str = "bootloader+softdevice";

/// One file of a release, for one target.
#[derive(Clone)]
pub struct Part {
    /// The file in the zip, or the binary's file name
    pub file: String,
    /// `stm32`, `nrf51`, `bcLighthouse4`, `deckctrl`, ...
    pub target: String,
    /// `fw`, `cfg`, ...
    pub kind: String,
    pub data: Vec<u8>,
}

impl std::fmt::Debug for Part {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Part").field("file", &self.file).field("target", &self.key()).field("bytes", &self.data.len()).finish()
    }
}

impl Part {
    /// A binary for a target named as cfcli does, `stm32-fw` or `bcCam:qcc`.
    pub fn from_binary(file: &str, key: &str, data: Vec<u8>) -> Part {
        let (target, kind) = key.split_once('-').unwrap_or((key, ""));
        Part { file: file.to_string(), target: target.to_string(), kind: kind.to_string(), data }
    }

    /// The target and type, as cfcli and the deck flashing name them:
    /// `stm32-fw`, `bcLighthouse4-fw`, `deckctrl-cfg`.
    pub fn key(&self) -> String {
        if self.kind.is_empty() { self.target.clone() } else { format!("{}-{}", self.target, self.kind) }
    }

    /// The STM32 or the nRF51: flashed through the connection server, by
    /// broadcast where the bootloaders take it.
    pub fn is_firmware(&self) -> bool {
        self.server_target().is_some()
    }

    /// The connection server's name for a firmware part.
    pub fn server_target(&self) -> Option<&'static str> {
        match self.target.as_str() {
            "stm32" => Some("stm32"),
            "nrf51" => Some("nrf51"),
            _ => None,
        }
    }

    /// What to call it in the dialog.
    pub fn name(&self) -> String {
        target_name(&self.key())
    }
}

/// What to call a target (`stm32-fw`, `bcLighthouse4-fw`, ...).
pub fn target_name(key: &str) -> String {
    let name = match key {
        "stm32-fw" => "STM32",
        "nrf51-fw" => "nRF51",
        "bcLighthouse4-fw" => "Lighthouse deck",
        "bcAI:esp-fw" => "AI deck, ESP32",
        "bcAI:gap8-fw" => "AI deck, GAP8",
        "bcCam:qcc" | "bcCam:qcc-fw" => "Camera deck",
        "bcColorLedTop:col-fw" => "Color LED deck, top",
        "bcColorLedBot:col-fw" => "Color LED deck, bottom",
        "deckctrl-fw" => "DeckCtrl",
        "deckctrl-cfg" => "DeckCtrl configuration",
        other => return other.to_string(),
    };
    name.to_string()
}

/// The order parts are flashed in: the STM32 and nRF51 first (one job on the
/// connection server), then DeckCtrl, then the decks, as cfcli does.
fn order(part: &Part) -> (u8, String) {
    let rank = match part.target.as_str() {
        "stm32" => 0,
        "nrf51" => 1,
        "deckctrl" => 2,
        _ => 3,
    };
    (rank, part.key())
}

/// The release file name of a platform, from what a Crazyflie calls itself
/// (`cf.platform.device_type_name()`): the Crazyflie 2.0, 2.1 and 2.1+ run
/// the same `cf2` firmware.
pub fn platform_release_name(device_type: &str) -> Option<&'static str> {
    let device_type = device_type.trim();
    if device_type.starts_with("Crazyflie 2.1 Brushless") {
        Some("cf21bl")
    } else if device_type.starts_with("Crazyflie 2.") {
        Some("cf2")
    } else if device_type.starts_with("Flapper") {
        Some("flapper")
    } else if device_type.starts_with("Crazyflie Bolt") {
        Some("bolt")
    } else if device_type.starts_with("Roadrunner") {
        Some("tag")
    } else {
        None
    }
}

/// The release zip's name for a platform: `firmware-cf2-2025.09.zip`.
pub fn release_asset_name(platform: &str, release: &str) -> String {
    format!("firmware-{}-{}.zip", platform, release)
}

#[derive(Deserialize)]
struct FileInfo {
    #[serde(deserialize_with = "string_or_list")]
    target: Vec<String>,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct Manifest {
    version: u32,
    subversion: u32,
    fw_platform: String,
    release: String,
    files: HashMap<String, FileInfo>,
}

/// A manifest target is one name or a list of them.
fn string_or_list<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMore {
        One(String),
        More(Vec<String>),
    }
    Ok(match OneOrMore::deserialize(deserializer)? {
        OneOrMore::One(target) => vec![target],
        OneOrMore::More(targets) => targets,
    })
}

/// A release .zip, read.
#[derive(Debug)]
pub struct ReleaseZip {
    /// `cf2`, `cf21bl`, ...
    pub platform: String,
    /// `2025.09`
    pub release: String,
    /// In the order they are flashed
    pub parts: Vec<Part>,
}

impl ReleaseZip {
    pub fn read(data: &[u8]) -> Result<ReleaseZip> {
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(data)).context("not a zip file")?;
        let manifest: Manifest = {
            let mut file = archive.by_name("manifest.json").context("no manifest.json: not a firmware release")?;
            let mut text = Vec::new();
            file.read_to_end(&mut text)?;
            serde_json::from_slice(&text).context("reading manifest.json")?
        };
        if manifest.version != 2 || manifest.subversion != 1 {
            bail!("manifest version {}.{}: only 2.1 is known", manifest.version, manifest.subversion);
        }
        let mut parts = Vec::new();
        for (file, info) in &manifest.files {
            if info.kind == NRF51_SOFTDEVICE_TYPE {
                continue;
            }
            let mut data = Vec::new();
            archive.by_name(file).with_context(|| format!("{file}, in the manifest, isn't in the zip"))?.read_to_end(&mut data)?;
            for target in &info.target {
                parts.push(Part { file: file.clone(), target: target.clone(), kind: info.kind.clone(), data: data.clone() });
            }
        }
        parts.sort_by_key(order);
        Ok(ReleaseZip { platform: manifest.fw_platform, release: manifest.release, parts })
    }
}

/// A release on GitHub, from its API.
#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
    pub size: u64,
}

impl Release {
    /// "2025.09"
    pub fn label(&self) -> &str {
        self.name.as_deref().filter(|n| !n.is_empty()).unwrap_or(&self.tag_name)
    }

    /// "Sep 2025", from `2025-09-03T12:00:00Z`
    pub fn date(&self) -> String {
        self.published_at
            .as_deref()
            .and_then(|d| chrono::DateTime::parse_from_rfc3339(d).ok())
            .map(|d| d.format("%b %Y").to_string())
            .unwrap_or_default()
    }

    /// The zip for a platform, when the release has one.
    pub fn asset(&self, platform: &str) -> Option<&Asset> {
        let name = release_asset_name(platform, &self.tag_name);
        self.assets.iter().find(|a| a.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn release_zip(manifest: &str, files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(&mut out);
        let stored = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        zip.start_file("manifest.json", stored).unwrap();
        zip.write_all(manifest.as_bytes()).unwrap();
        for (name, data) in files {
            zip.start_file(*name, stored).unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap();
        out.into_inner()
    }

    #[test]
    fn a_release_in_flashing_order_without_the_softdevice() {
        let manifest = r#"{"version": 2, "subversion": 1, "fw_platform": "cf2", "release": "2025.09", "files": {
            "lighthouse.bin": {"platform": "deck", "target": "bcLighthouse4", "type": "fw", "release": "2025.09", "repository": "x"},
            "cf2_nrf-2025.09.bin": {"platform": "cf2", "target": "nrf51", "type": "fw", "release": "2025.09", "repository": "x"},
            "sd-s130.bin": {"platform": "cf2", "target": "nrf51", "type": "bootloader+softdevice", "release": "2025.09", "repository": "x"},
            "cf2-2025.09.bin": {"platform": "cf2", "target": ["stm32"], "type": "fw", "release": "2025.09", "repository": "x"}
        }}"#;
        let zip = release_zip(manifest, &[("lighthouse.bin", b"lh"), ("cf2_nrf-2025.09.bin", b"nrf"), ("sd-s130.bin", b"sd"), ("cf2-2025.09.bin", b"stm")]);
        let release = ReleaseZip::read(&zip).unwrap();
        assert_eq!((release.platform.as_str(), release.release.as_str()), ("cf2", "2025.09"));
        let keys: Vec<String> = release.parts.iter().map(Part::key).collect();
        assert_eq!(keys, ["stm32-fw", "nrf51-fw", "bcLighthouse4-fw"]);
        assert_eq!(release.parts[0].data, b"stm");
        assert!(release.parts[0].is_firmware() && !release.parts[2].is_firmware());
        assert_eq!(release.parts[2].name(), "Lighthouse deck");
    }

    #[test]
    fn platforms_by_what_a_crazyflie_calls_itself() {
        assert_eq!(platform_release_name("Crazyflie 2.1"), Some("cf2"));
        assert_eq!(platform_release_name("Crazyflie 2.1+"), Some("cf2"));
        assert_eq!(platform_release_name("Crazyflie 2.1 Brushless"), Some("cf21bl"));
        assert_eq!(platform_release_name("Crazyflie Bolt 1.1"), Some("bolt"));
        assert_eq!(platform_release_name("Flapper (Bolt 1.1)"), Some("flapper"));
        assert_eq!(platform_release_name("Roadrunner 1.0"), Some("tag"));
        assert_eq!(platform_release_name("Something else"), None);
        assert_eq!(release_asset_name("cf21bl", "2025.09"), "firmware-cf21bl-2025.09.zip");
    }

    #[test]
    fn a_binary_is_named_by_its_target() {
        let part = Part::from_binary("cf2.bin", "stm32-fw", vec![1]);
        assert_eq!((part.target.as_str(), part.kind.as_str(), part.key()), ("stm32", "fw", "stm32-fw".to_string()));
        let cam = Part::from_binary("qcc.bin", "bcCam:qcc", vec![1]);
        assert_eq!(cam.key(), "bcCam:qcc");
    }
}
