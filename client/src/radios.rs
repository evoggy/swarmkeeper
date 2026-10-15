//! Radio URIs that leave the Crazyradio open, and the choice of a real
//! Crazyradio for them. The same rules as cfcli (`src/utils/radio.rs`), so a
//! swarm file behaves the same in both.
//!
//! `radio:///80/2M/E7E7E7E7E7`, with the radio left empty, means channel 80,
//! address E7E7E7E7E7, on any Crazyradio. crazyflie-link only accepts a number
//! for the radio, so the empty radio is filled in with the index of an attached
//! Crazyradio before the URI is handed to it:
//!
//! * The Crazyflies of the swarm are spread over all Crazyradios that can be
//!   opened (see [`assign`]). Crazyflies on the same channel always share a
//!   radio, since two radios on one channel collide, and channels less than 2
//!   apart count as the same channel: a 2 Mbit/s channel is about 2 MHz wide.
//! * A URI on a channel the swarm doesn't use gets the first Crazyradio that
//!   can be opened, which skips radios held by another program.
//!
//! A number in the URI (`radio://1/…`) still means exactly that Crazyradio.
//!
//! [`RadioPlan`] keeps the choice for the loaded swarm, so that every link
//! to a Crazyflie goes through the same Crazyradio until the swarm is
//! connected again.

use std::collections::BTreeMap;

use anyhow::{bail, Result};
use crazyflie_link::LinkContext;

const CRAZYRADIO_VID: u16 = 0x1915;
const CRAZYRADIO_PID: u16 = 0x7777;

/// Channels closer than this share a Crazyradio.
pub const CHANNEL_SPACING: u8 = 2;

const URI_FORMAT: &str = "expected radio://<radio>/<channel>/<datarate>/<address>";

/// A parsed `radio://` URI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RadioUri {
    /// The radio index, `None` for an empty radio (any radio).
    pub radio: Option<usize>,
    pub channel: u8,
    /// The address as 10 upper-case hex digits, zero padded on the left the
    /// same way crazyflie-link reads a short address.
    pub address: String,
    /// Everything after the radio index, e.g. `80/2M/E7E7E7E7E7?safelink=0`.
    rest: String,
}

impl RadioUri {
    /// Parse a `radio://` URI. Returns `Ok(None)` for other URIs (`usb://`).
    pub fn parse(uri: &str) -> Result<Option<RadioUri>> {
        let Some(after_scheme) = uri.strip_prefix("radio://") else {
            return Ok(None);
        };
        let invalid = |why: &str| anyhow::anyhow!("URI '{}': {}", uri, why);

        let (radio, rest) = after_scheme.split_once('/').ok_or_else(|| invalid(URI_FORMAT))?;
        let radio = match radio {
            "" => None,
            n => Some(
                n.parse::<usize>()
                    .map_err(|_| invalid("the radio must be a number, or empty for any Crazyradio"))?,
            ),
        };

        let path = rest.split('?').next().unwrap_or_default();
        let parts: Vec<&str> = path.split('/').collect();
        if parts.len() != 3 {
            bail!(invalid(URI_FORMAT));
        }
        let channel = parts[0]
            .parse::<u8>()
            .ok()
            .filter(|c| *c <= 125)
            .ok_or_else(|| invalid("the channel must be 0-125"))?;
        if !["250K", "1M", "2M"].contains(&parts[1].to_uppercase().as_str()) {
            bail!(invalid("the datarate must be 250K, 1M or 2M"));
        }
        let address = parts[2];
        if address.is_empty() || address.len() > 10 || !address.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!(invalid("the address must be 1-10 hex digits"));
        }

        Ok(Some(RadioUri {
            radio,
            channel,
            address: format!("{:0>10}", address.to_uppercase()),
            rest: rest.to_string(),
        }))
    }

    /// The address as bytes, in the order crazyflie-link sends them.
    pub fn address_bytes(&self) -> [u8; 5] {
        let mut bytes = [0u8; 5];
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&self.address[2 * i..2 * i + 2], 16).expect("parsed as hex");
        }
        bytes
    }

    /// The same URI on another channel, with the radio as it was written.
    pub fn with_channel(&self, channel: u8) -> String {
        let (_, after_channel) = self.rest.split_once('/').expect("a parsed URI has a channel");
        let radio = self.radio.map(|r| r.to_string()).unwrap_or_default();
        format!("radio://{}/{}/{}", radio, channel, after_channel)
    }

    /// The same URI on the given Crazyradio.
    pub fn with_radio(&self, radio: usize) -> String {
        format!("radio://{}/{}", radio, self.rest)
    }
}

/// Number of attached Crazyradios. crazyflie-link numbers them in the same
/// order: the USB device list, filtered on the Crazyradio VID/PID.
pub fn attached_radios() -> usize {
    let Ok(devices) = rusb::devices() else {
        return 0;
    };
    devices
        .iter()
        .filter(|device| {
            device
                .device_descriptor()
                .map(|desc| desc.vendor_id() == CRAZYRADIO_VID && desc.product_id() == CRAZYRADIO_PID)
                .unwrap_or(false)
        })
        .count()
}

/// Indices of the attached Crazyradios that can be opened. A radio that
/// another program holds (cfcli, a second Swarmkeeper) fails to open and is
/// left out. Radios that no link uses are closed again before this returns.
pub async fn available_radios(link_context: &LinkContext) -> Vec<usize> {
    let mut available = Vec::new();
    for radio in 0..attached_radios() {
        if link_context.get_radio(radio).await.is_ok() {
            available.push(radio);
        }
    }
    available
}

/// What [`assign`] needs to know about one Crazyflie.
#[derive(Debug, Clone, Copy)]
pub struct Link {
    /// The radio from the URI, `None` for an empty radio.
    pub radio: Option<usize>,
    pub channel: u8,
}

/// The result of [`assign`].
#[derive(Debug)]
pub struct Assignment {
    /// The Crazyradio for each link, in the order they were given.
    pub radios: Vec<usize>,
    /// Problems with the pinned radios, for the user to see.
    pub warnings: Vec<String>,
}

/// Choose a Crazyradio for each link. `available` lists the radios that can
/// be used and must not be empty.
///
/// 1. A pinned link (`radio://1/…`) uses its radio, and its channel then
///    belongs to that radio.
/// 2. Channels less than 2 apart form one group, chained: 72, 73 and 74 are
///    a single group. Links without a radio on a group with a pinned link
///    follow it.
/// 3. The other groups go, largest first (equal ones in list order), to the
///    radio with the fewest links, the lowest index on a tie. The same input
///    always gives the same result.
pub fn assign(links: &[Link], available: &[usize]) -> Assignment {
    assert!(!available.is_empty(), "assign() needs at least one radio");
    let mut warnings = Vec::new();

    let mut channels: Vec<u8> = links.iter().map(|link| link.channel).collect();
    channels.sort_unstable();
    channels.dedup();
    let mut group_of_channel = BTreeMap::new();
    let mut groups = 0;
    for (i, channel) in channels.iter().enumerate() {
        if i == 0 || channel - channels[i - 1] >= CHANNEL_SPACING {
            groups += 1;
        }
        group_of_channel.insert(*channel, groups - 1);
    }
    let group = |link: &Link| group_of_channel[&link.channel];
    let describe = |group: usize| {
        let members: Vec<String> = group_of_channel
            .iter()
            .filter(|(_, g)| **g == group)
            .map(|(channel, _)| channel.to_string())
            .collect();
        match members.len() {
            1 => format!("channel {}", members[0]),
            _ => format!("channels {}", members.join(", ")),
        }
    };

    // Pinned links claim their group's radio, the first one in the list wins.
    let mut group_radio: Vec<Option<usize>> = vec![None; groups];
    for link in links {
        let Some(radio) = link.radio else { continue };
        let g = group(link);
        match group_radio[g] {
            None => group_radio[g] = Some(radio),
            Some(claimed) if claimed != radio => {
                let warning = format!(
                    "{} is pinned to more than one Crazyradio; the Crazyflies with radio:/// on it use radio {}",
                    describe(g),
                    claimed
                );
                if !warnings.contains(&warning) {
                    warnings.push(warning);
                }
            }
            Some(_) => {}
        }
    }
    let mut pinned: Vec<usize> = links.iter().filter_map(|link| link.radio).collect();
    pinned.sort_unstable();
    pinned.dedup();
    for radio in pinned.iter().filter(|radio| !available.contains(radio)) {
        warnings.push(format!("Crazyradio {} is pinned in a URI but can't be opened", radio));
    }

    // Links already placed count towards their radio's load.
    let mut load: BTreeMap<usize, usize> = available.iter().map(|radio| (*radio, 0)).collect();
    let mut size = vec![0; groups];
    for link in links {
        match link.radio.or(group_radio[group(link)]) {
            Some(radio) => {
                if let Some(n) = load.get_mut(&radio) {
                    *n += 1;
                }
            }
            None => size[group(link)] += 1,
        }
    }

    // Equal groups are taken in the order they first appear in the list.
    let mut first_seen = vec![usize::MAX; groups];
    for (i, link) in links.iter().enumerate() {
        first_seen[group(link)] = first_seen[group(link)].min(i);
    }
    let mut free: Vec<usize> = (0..groups).filter(|g| group_radio[*g].is_none()).collect();
    free.sort_by_key(|g| (std::cmp::Reverse(size[*g]), first_seen[*g]));
    for g in free {
        let radio = load
            .iter()
            .min_by_key(|(radio, n)| (**n, **radio))
            .map(|(radio, _)| *radio)
            .expect("available is not empty");
        *load.get_mut(&radio).expect("radio is in load") += size[g];
        group_radio[g] = Some(radio);
    }

    let radios = links
        .iter()
        .map(|link| link.radio.unwrap_or_else(|| group_radio[group(link)].expect("every group has a radio")))
        .collect();
    Assignment { radios, warnings }
}

/// Which Crazyradio the `radio:///` URIs of the loaded swarm use.
///
/// Planned the first time a link needs it, and again each time the whole
/// swarm is connected, so that Crazyradios plugged in since are used. In
/// between, every link to a Crazyflie goes through the same radio.
#[derive(Default)]
pub struct RadioPlan {
    /// The swarm's URIs.
    uris: Vec<String>,
    /// The Crazyradio of each channel with `radio:///` Crazyflies on it,
    /// once planned.
    by_channel: Option<BTreeMap<u8, usize>>,
}

pub type SharedRadioPlan = std::sync::Arc<tokio::sync::Mutex<RadioPlan>>;

impl RadioPlan {
    /// A swarm was loaded: forget the plan for the previous one.
    pub fn set_swarm(&mut self, uris: Vec<String>) {
        self.uris = uris;
        self.by_channel = None;
    }

    /// Spread the swarm over the Crazyradios that can be opened now. Returns
    /// the warnings about pinned radios. Fails when there is no Crazyradio,
    /// and then keeps the previous plan.
    pub async fn replan(&mut self, link_context: &LinkContext) -> Result<Vec<String>> {
        let parsed: Vec<RadioUri> = self
            .uris
            .iter()
            .filter_map(|uri| RadioUri::parse(uri).ok().flatten())
            .collect();
        if parsed.is_empty() {
            self.by_channel = Some(BTreeMap::new());
            return Ok(Vec::new());
        }
        let available = available_radios(link_context).await;
        if available.is_empty() {
            bail!("No Crazyradio could be opened");
        }
        let links: Vec<Link> = parsed.iter().map(|r| Link { radio: r.radio, channel: r.channel }).collect();
        let assignment = assign(&links, &available);
        let mut by_channel = BTreeMap::new();
        for (uri, radio) in parsed.iter().zip(&assignment.radios) {
            if uri.radio.is_none() {
                by_channel.insert(uri.channel, *radio);
            }
        }
        self.by_channel = Some(by_channel);
        Ok(assignment.warnings)
    }

    /// The Crazyradio a `radio:///` URI uses, planning first if needed.
    /// `None` for other URIs, and when no Crazyradio can be opened.
    pub async fn radio_for(&mut self, link_context: &LinkContext, uri: &str) -> Option<usize> {
        let parsed = RadioUri::parse(uri).ok().flatten()?;
        if parsed.radio.is_some() {
            return parsed.radio;
        }
        if self.by_channel.is_none() {
            if let Err(e) = self.replan(link_context).await {
                eprintln!("Planning the Crazyradios: {:#}", e);
            }
        }
        if let Some(radio) = self.by_channel.as_ref().and_then(|plan| plan.get(&parsed.channel)) {
            return Some(*radio);
        }
        // Not a channel of the swarm: the first Crazyradio that opens.
        available_radios(link_context).await.first().copied()
    }

    /// The URI to hand to crazyflie-link: a `radio:///` URI gets its
    /// Crazyradio filled in, other URIs are returned unchanged. When no
    /// Crazyradio can be opened the radio becomes 0, so that connecting fails
    /// with the usual error.
    pub async fn link_uri(&mut self, link_context: &LinkContext, uri: &str) -> String {
        match RadioUri::parse(uri) {
            Ok(Some(parsed)) if parsed.radio.is_none() => {
                let radio = self.radio_for(link_context, uri).await.unwrap_or(0);
                parsed.with_radio(radio)
            }
            _ => uri.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn any(channel: u8) -> Link {
        Link { radio: None, channel }
    }

    fn pinned(radio: usize, channel: u8) -> Link {
        Link { radio: Some(radio), channel }
    }

    #[test]
    fn parses_any_and_pinned_radios() {
        let uri = RadioUri::parse("radio:///80/2M/E7E7E7E7E7").unwrap().unwrap();
        assert_eq!(uri.radio, None);
        assert_eq!(uri.channel, 80);
        assert_eq!(uri.address, "E7E7E7E7E7");

        let uri = RadioUri::parse("radio://1/5/250K/e7").unwrap().unwrap();
        assert_eq!(uri.radio, Some(1));
        assert_eq!(uri.address, "00000000E7");
        assert_eq!(uri.address_bytes(), [0, 0, 0, 0, 0xE7]);
    }

    #[test]
    fn other_schemes_are_not_radio_uris() {
        assert_eq!(RadioUri::parse("usb://0").unwrap(), None);
    }

    #[test]
    fn rejects_malformed_radio_uris() {
        for uri in [
            "radio://x/80/2M/E7E7E7E7E7",
            "radio:///126/2M/E7E7E7E7E7",
            "radio:///80/3M/E7E7E7E7E7",
            "radio:///80/E7E7E7E7E7",
            "radio:///80/2M/E7E7E7E7E7E7",
            "radio:///80/2M/XYZ",
            "radio://*",
            "radio://*/80/2M/E7E7E7E7E7",
        ] {
            assert!(RadioUri::parse(uri).is_err(), "{} should be rejected", uri);
        }
    }

    #[test]
    fn with_radio_keeps_the_rest_of_the_uri() {
        let uri = RadioUri::parse("radio:///80/2M/E7E7E7E7E7?timeout=2000").unwrap().unwrap();
        assert_eq!(uri.with_radio(2), "radio://2/80/2M/E7E7E7E7E7?timeout=2000");
    }

    #[test]
    fn with_channel_keeps_the_radio_and_the_rest() {
        let uri = RadioUri::parse("radio:///80/2M/E7E7E7E701?safelink=0").unwrap().unwrap();
        assert_eq!(uri.with_channel(76), "radio:///76/2M/E7E7E7E701?safelink=0");
        let uri = RadioUri::parse("radio://1/80/250K/E7").unwrap().unwrap();
        assert_eq!(uri.with_channel(4), "radio://1/4/250K/E7");
    }

    #[test]
    fn one_radio_takes_everything() {
        let links = [any(80), any(76), any(72)];
        assert_eq!(assign(&links, &[0]).radios, vec![0, 0, 0]);
    }

    #[test]
    fn channels_are_spread_largest_group_first() {
        // 3 on 72, 2 on 76, 1 on 80 over two radios: 72 alone on radio 0,
        // 76 and 80 together on radio 1.
        let links = [any(72), any(76), any(72), any(80), any(76), any(72)];
        assert_eq!(assign(&links, &[0, 1]).radios, vec![0, 1, 0, 1, 1, 0]);
    }

    #[test]
    fn a_channel_never_spans_two_radios() {
        let links = [any(80), any(80), any(80), any(80)];
        assert_eq!(assign(&links, &[0, 1, 2]).radios, vec![0, 0, 0, 0]);
    }

    #[test]
    fn neighbouring_channels_share_a_radio() {
        // 72, 73 and 74 chain into one group; 80 is its own.
        let links = [any(72), any(74), any(80), any(73)];
        assert_eq!(assign(&links, &[0, 1]).radios, vec![0, 0, 1, 0]);
    }

    #[test]
    fn channels_two_apart_are_separate() {
        let links = [any(80), any(78), any(76)];
        assert_eq!(assign(&links, &[0, 1, 2]).radios, vec![0, 1, 2]);
    }

    #[test]
    fn pinned_links_claim_their_channel() {
        // Channel 80 is pinned to radio 1, so the other CF on 80 follows it
        // and channel 72 gets radio 0, the one with the fewest links.
        let links = [any(80), pinned(1, 80), any(72)];
        let assignment = assign(&links, &[0, 1]);
        assert_eq!(assignment.radios, vec![1, 1, 0]);
        assert!(assignment.warnings.is_empty());
    }

    #[test]
    fn conflicting_pins_are_kept_with_a_warning() {
        let links = [pinned(0, 80), pinned(1, 80), any(80)];
        let assignment = assign(&links, &[0, 1]);
        assert_eq!(assignment.radios, vec![0, 1, 0]);
        assert_eq!(assignment.warnings.len(), 1);
        assert!(assignment.warnings[0].contains("channel 80"), "{:?}", assignment.warnings);
    }

    #[test]
    fn a_pin_on_a_missing_radio_is_reported() {
        let links = [pinned(3, 80), any(72)];
        let assignment = assign(&links, &[0]);
        assert_eq!(assignment.radios, vec![3, 0]);
        assert!(assignment.warnings[0].contains("Crazyradio 3"), "{:?}", assignment.warnings);
    }

    #[test]
    fn skips_radios_that_are_not_available() {
        // Radio 0 is held by another program.
        let links = [any(80), any(76)];
        assert_eq!(assign(&links, &[1, 2]).radios, vec![1, 2]);
    }
}
