//! Spread a swarm over several radio channels, the same way as
//! `cfcli swarm rechannel`.
//!
//! Crazyflies on the same channel always share a Crazyradio (see
//! [`crate::radios::assign`]), so a swarm on one channel can't use more than
//! one. With one Crazyradio per channel, each radio serves its own part of the
//! swarm.
//!
//! The channel is stored in each Crazyflie's EEPROM config and the firmware
//! only applies it at boot, so a Crazyflie that moves is reprogrammed on its
//! current channel, rebooted and then looked for on its new channel. Its URI
//! in the swarm file only changes once it answers there. Running it again
//! therefore finishes a run that was interrupted: a Crazyflie that no longer
//! answers on its old channel but does on its new one only gets its URI
//! updated.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use crazyflie_lib::subsystems::memory::{EEPROMConfigMemory, MemoryType};
use crazyflie_lib::{Crazyflie, TocCache};
use crazyflie_link::LinkContext;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::radios::{RadioUri, CHANNEL_SPACING};

/// The lower channels are the more crowded ones (Wi-Fi), so a channel count
/// starts here and goes down.
const FIRST_CHANNEL: u8 = 80;
/// How long a rebooted Crazyflie gets to show up on its new channel.
const BOOT_TIMEOUT: Duration = Duration::from_secs(8);
/// Pause between looking for a rebooting Crazyflie.
const BOOT_POLL: Duration = Duration::from_millis(300);
/// Packets sent to a Crazyflie before it counts as not answering.
const ANSWER_ATTEMPTS: usize = 3;
/// Crazyflies worked on at the same time.
const PARALLEL: usize = 8;
/// Time to connect to a Crazyflie to reprogram it.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// The channels for `count` channels: 80, 78, 76, ...
pub fn channels_for_count(count: u8) -> Vec<u8> {
    (0..count.min(41)).map(|i| FIRST_CHANNEL - CHANNEL_SPACING * i).collect()
}

/// Read a channel list such as `80, 78 76`. Rejects empty lists and channels
/// given twice; returns warnings about channels that interfere.
pub fn parse_channels(text: &str) -> Result<(Vec<u8>, Vec<String>)> {
    let mut channels = Vec::new();
    for part in text.split(|c: char| c == ',' || c.is_whitespace()).filter(|p| !p.is_empty()) {
        let channel = part
            .parse::<u8>()
            .ok()
            .filter(|c| *c <= 125)
            .ok_or_else(|| anyhow!("'{}' is not a channel (0-125)", part))?;
        channels.push(channel);
    }
    if channels.is_empty() {
        bail!("No channels given");
    }
    let mut warnings = Vec::new();
    for (i, a) in channels.iter().enumerate() {
        for b in &channels[i + 1..] {
            if a == b {
                bail!("Channel {} is given twice", a);
            }
            if a.abs_diff(*b) < CHANNEL_SPACING {
                warnings.push(format!(
                    "Channels {} and {} are less than {} apart; they interfere at 2M and share a Crazyradio",
                    a, b, CHANNEL_SPACING
                ));
            }
        }
    }
    Ok((channels, warnings))
}

/// The new channel of each Crazyflie, given their current channels.
///
/// Each channel gets an equal share, the shares differ by at most one and the
/// first channels get the extra. Crazyflies already on one of the channels
/// stay there, up to that channel's share, in list order; the rest fill the
/// remaining places, in list order. That keeps the number of Crazyflies to
/// reprogram as low as possible, and planning again from the result changes
/// nothing.
pub fn plan(current: &[u8], channels: &[u8]) -> Vec<u8> {
    let (n, k) = (current.len(), channels.len());
    let share: Vec<usize> = (0..k).map(|j| n / k + usize::from(j < n % k)).collect();
    let mut filled = vec![0; k];
    let mut assigned: Vec<Option<usize>> = vec![None; n];

    for (i, channel) in current.iter().enumerate() {
        if let Some(j) = channels.iter().position(|c| c == channel) {
            if filled[j] < share[j] {
                filled[j] += 1;
                assigned[i] = Some(j);
            }
        }
    }
    for slot in assigned.iter_mut().filter(|slot| slot.is_none()) {
        let j = (0..k).find(|j| filled[*j] < share[*j]).expect("the shares add up to the Crazyflies");
        filled[j] += 1;
        *slot = Some(j);
    }
    assigned.into_iter().map(|j| channels[j.expect("every Crazyflie has a channel")]).collect()
}

/// One Crazyflie of the swarm, as written in the swarm file.
#[derive(Clone)]
pub struct Unit {
    pub name: String,
    pub uri: String,
}

/// Where one Crazyflie ends up.
#[derive(Debug)]
pub struct Placement {
    /// Its index in the swarm.
    pub unit: usize,
    pub name: String,
    pub uri: String,
    /// Its channel now, `None` when it isn't reached over the radio.
    pub from: Option<u8>,
    /// Its channel after the move, `None` when it isn't reached over the
    /// radio.
    pub to: Option<u8>,
    /// Its URI after the move.
    pub new_uri: String,
}

impl Placement {
    pub fn moves(&self) -> bool {
        self.from != self.to
    }
}

/// Plan the move of every Crazyflie in the swarm onto `channels`. Refuses a
/// plan that puts two Crazyflies on one channel with one address, since they
/// can't be told apart.
pub fn place(units: &[Unit], channels: &[u8]) -> Result<Vec<Placement>> {
    let parsed: Vec<Option<RadioUri>> = units
        .iter()
        .map(|unit| RadioUri::parse(&unit.uri))
        .collect::<Result<_>>()?;
    let current: Vec<u8> = parsed.iter().flatten().map(|uri| uri.channel).collect();
    if current.is_empty() {
        bail!("None of the Crazyflies is reached over the radio");
    }
    let mut new_channels = plan(&current, channels).into_iter();

    let mut placements = Vec::new();
    let mut seen: HashMap<(u8, String), &str> = HashMap::new();
    for (i, (unit, uri)) in units.iter().zip(&parsed).enumerate() {
        let Some(uri) = uri else {
            placements.push(Placement {
                unit: i,
                name: unit.name.clone(),
                uri: unit.uri.clone(),
                from: None,
                to: None,
                new_uri: unit.uri.clone(),
            });
            continue;
        };
        let to = new_channels.next().expect("one channel per radio Crazyflie");
        if let Some(other) = seen.insert((to, uri.address.clone()), &unit.name) {
            bail!(
                "{} and {} would both be on channel {} with address {}; give them different addresses first",
                other,
                unit.name,
                to,
                uri.address
            );
        }
        placements.push(Placement {
            unit: i,
            name: unit.name.clone(),
            uri: unit.uri.clone(),
            from: Some(uri.channel),
            to: Some(to),
            new_uri: if to == uri.channel { unit.uri.clone() } else { uri.with_channel(to) },
        });
    }
    Ok(placements)
}

/// How far along one Crazyflie is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Busy,
    Done,
    Failed,
}

pub enum Event {
    /// Status of one Crazyflie, by its index in the swarm.
    Unit { unit: usize, state: State, status: String },
    /// Status of the whole run.
    Overall(String),
}

pub type Report = Arc<dyn Fn(Event) + Send + Sync>;

/// Turns a swarm URI into one crazyflie-link can open (a real Crazyradio and
/// the link timeout).
pub type LinkUri = Arc<dyn Fn(String) -> futures::future::BoxFuture<'static, String> + Send + Sync>;

/// One Crazyflie that changes channel, with the URIs to reach it on.
#[derive(Clone)]
struct Move {
    unit: usize,
    from: u8,
    to: u8,
    /// Its URI on the old and on the new channel, ready to open.
    old_link: String,
    new_link: String,
}

/// Move the Crazyflies of `placements` that change channel. Returns the
/// indices (in the swarm) of those that answer on their new channel, whose
/// URI should change to [`Placement::new_uri`].
pub async fn run<C>(
    link_context: Arc<LinkContext>,
    toc_cache: C,
    link_uri: LinkUri,
    placements: &[Placement],
    report: Report,
) -> Vec<usize>
where
    C: TocCache + Clone + Send + Sync + 'static,
{
    let mut moves = Vec::new();
    for p in placements.iter().filter(|p| p.moves()) {
        let (Some(from), Some(to)) = (p.from, p.to) else { continue };
        moves.push(Move {
            unit: p.unit,
            from,
            to,
            old_link: link_uri(p.uri.clone()).await,
            new_link: link_uri(p.new_uri.clone()).await,
        });
    }
    let unit = |unit: usize, state: State, status: String| report(Event::Unit { unit, state, status });

    // 1. Where each one is now.
    report(Event::Overall(format!("Checking {}…", crazyflies(moves.len()))));
    for m in &moves {
        unit(m.unit, State::Busy, "Checking…".to_string());
    }
    let on_old = each(&moves, |m| answers(link_context.clone(), m.old_link.clone())).await;
    let on_new = each(&moves, |m| answers(link_context.clone(), m.new_link.clone())).await;

    // 2. Reprogram the ones on their old channel, and reboot them so that
    //    the firmware picks up the new channel.
    report(Event::Overall("Reprogramming…".to_string()));
    let mut rebooted = vec![false; moves.len()];
    let written = each(&moves, |m| {
        let (link_context, toc_cache, report, m) = (link_context.clone(), toc_cache.clone(), report.clone(), m.clone());
        let answers = on_old[moves.iter().position(|n| n.unit == m.unit).expect("a move")];
        async move {
            if !answers {
                return None;
            }
            report(Event::Unit { unit: m.unit, state: State::Busy, status: format!("Reprogramming to channel {}…", m.to) });
            let result = async {
                let cf = tokio::time::timeout(CONNECT_TIMEOUT, Crazyflie::connect_from_uri(&link_context, &m.old_link, toc_cache))
                    .await
                    .map_err(|_| anyhow!("connecting timed out"))?
                    .context("could not connect")?;
                let written = write_radio_channel(&cf, m.to).await;
                cf.disconnect().await;
                written.context("not reprogrammed")?;
                report(Event::Unit { unit: m.unit, state: State::Busy, status: "Rebooting…".to_string() });
                reboot(&link_context, &m.old_link).await.context("reprogrammed but not rebooted")
            }
            .await;
            Some(result)
        }
    })
    .await;
    let mut failed: HashMap<usize, String> = HashMap::new();
    for (k, result) in written.into_iter().enumerate() {
        match result {
            Some(Ok(())) => rebooted[k] = true,
            Some(Err(e)) => {
                failed.insert(k, format!("{:#}", e));
            }
            None => {}
        }
    }

    // 3. Look for the rebooted ones on their new channel, and for the ones
    //    that didn't come up there on their old one.
    report(Event::Overall("Looking for the Crazyflies on their new channels…".to_string()));
    for (k, m) in moves.iter().enumerate() {
        if rebooted[k] {
            unit(m.unit, State::Busy, format!("Looking on channel {}…", m.to));
        }
    }
    let found_new = each(&moves, |m| {
        let k = moves.iter().position(|n| n.unit == m.unit).expect("a move");
        let (link_context, link, rebooted) = (link_context.clone(), m.new_link.clone(), rebooted[k]);
        async move { rebooted && wait_for(link_context, link).await }
    })
    .await;
    let still_old = each(&moves, |m| {
        let k = moves.iter().position(|n| n.unit == m.unit).expect("a move");
        let (link_context, link, check) = (link_context.clone(), m.old_link.clone(), rebooted[k] && !found_new[k]);
        async move { check && answers(link_context, link).await }
    })
    .await;

    let mut moved = Vec::new();
    for (k, m) in moves.iter().enumerate() {
        let (state, status) = if let Some(error) = failed.get(&k) {
            (State::Failed, error.clone())
        } else if found_new[k] {
            moved.push(m.unit);
            (State::Done, format!("Moved to channel {}", m.to))
        } else if rebooted[k] && still_old[k] {
            (State::Failed, format!("Still on channel {} after the reboot, the new channel didn't take", m.from))
        } else if rebooted[k] {
            (State::Failed, format!("Doesn't answer on channel {} or {} after the reboot", m.to, m.from))
        } else if on_new[k] {
            // From a run that stopped after the reboot.
            moved.push(m.unit);
            (State::Done, format!("Already on channel {}", m.to))
        } else {
            (State::Failed, format!("Doesn't answer on channel {} or {}", m.from, m.to))
        };
        unit(m.unit, state, status);
    }

    let failed = moves.len() - moved.len();
    let mut summary = format!("{} moved", crazyflies(moved.len()));
    if failed > 0 {
        summary += &format!(", {} failed", failed);
    }
    report(Event::Overall(summary));
    moved
}

/// "1 Crazyflie", "3 Crazyflies"
pub fn crazyflies(n: usize) -> String {
    match n {
        1 => "1 Crazyflie".to_string(),
        n => format!("{} Crazyflies", n),
    }
}

/// Run `op` for every move, [`PARALLEL`] at a time, and return the results
/// in move order.
async fn each<F, R>(moves: &[Move], op: impl Fn(&Move) -> F) -> Vec<R>
where
    F: std::future::Future<Output = R> + Send + 'static,
    R: Send + 'static,
{
    let slots = Arc::new(Semaphore::new(PARALLEL));
    let mut set = JoinSet::new();
    for (k, m) in moves.iter().enumerate() {
        let (slots, work) = (slots.clone(), op(m));
        set.spawn(async move {
            let _slot = slots.acquire_owned().await.expect("the semaphore is never closed");
            (k, work.await)
        });
    }
    let mut results = set.join_all().await;
    results.sort_by_key(|(k, _)| *k);
    results.into_iter().map(|(_, result)| result).collect()
}

/// Whether the Crazyflie answers a packet, without connecting to it.
async fn answers(link_context: Arc<LinkContext>, link: String) -> bool {
    for _ in 0..ANSWER_ATTEMPTS {
        if matches!(link_context.scan_selected(vec![link.as_str()]).await, Ok(found) if !found.is_empty()) {
            return true;
        }
    }
    false
}

/// Wait for a rebooted Crazyflie to answer.
async fn wait_for(link_context: Arc<LinkContext>, link: String) -> bool {
    let deadline = tokio::time::Instant::now() + BOOT_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        if answers(link_context.clone(), link.clone()).await {
            return true;
        }
        tokio::time::sleep(BOOT_POLL).await;
    }
    false
}

/// Write a new radio channel to the EEPROM config and read it back. The
/// firmware only applies it at boot.
async fn write_radio_channel(cf: &Crazyflie, channel: u8) -> Result<()> {
    let mut eeprom = open_eeprom(cf).await?;
    eeprom.set_radio_channel(channel)?;
    eeprom.commit().await?;
    cf.memory.close_memory(eeprom).await?;

    // Opening it again reads it from the Crazyflie.
    let written = open_eeprom(cf).await?;
    let stored = written.get_radio_channel();
    cf.memory.close_memory(written).await?;
    if stored != channel {
        bail!("the EEPROM config reads back channel {} after writing {}", stored, channel);
    }
    Ok(())
}

async fn open_eeprom(cf: &Crazyflie) -> Result<EEPROMConfigMemory> {
    let memories = cf.memory.get_memories(Some(MemoryType::EEPROMConfig));
    if memories.len() != 1 {
        bail!("No EEPROMConfig memory found or more than one ({})", memories.len());
    }
    match cf.memory.open_memory::<EEPROMConfigMemory>(memories[0].clone()).await {
        Some(Ok(eeprom)) => Ok(eeprom),
        Some(Err(e)) => bail!("Could not read the EEPROM config: {}", e),
        None => bail!("No EEPROM config memory found"),
    }
}

/// Restart the Crazyflie into its firmware, through the nRF51.
async fn reboot(link_context: &LinkContext, link: &str) -> Result<()> {
    let connection = link_context.open_link(link).await?;
    for command in [vec![0xFF, 0xFE, 0xFF], vec![0xFF, 0xFE, 0xF0, 0x01]] {
        connection.send_packet(command.into()).await?;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    connection.close().await;
    Ok(())
}

/// The swarm file with these units (index, new URI) moved. Only the `uri:`
/// lines of the moved units change, so comments and formatting are kept; a
/// file laid out in a way that can't be edited line by line is written out
/// again instead (which drops its comments).
pub fn rewrite_swarm_file(text: &str, moved: &[(usize, String)]) -> Result<String> {
    let mut document: serde_yaml::Value = serde_yaml::from_str(text).context("the swarm file can't be read")?;
    let old_uris: Vec<String> = document["units"]
        .as_sequence()
        .ok_or_else(|| anyhow!("the swarm file has no units"))?
        .iter()
        .map(|unit| unit["uri"].as_str().unwrap_or_default().to_string())
        .collect();

    // The value of each `uri:` line, in file order.
    let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
    let uri_lines: Vec<(usize, String)> = lines
        .iter()
        .enumerate()
        .filter_map(|(i, line)| {
            let value = line.trim_start().trim_start_matches("- ").trim_start().strip_prefix("uri:")?;
            Some((i, value.trim().trim_matches(|c| c == '"' || c == '\'').to_string()))
        })
        .collect();
    let line_by_line = uri_lines.len() == old_uris.len() && uri_lines.iter().zip(&old_uris).all(|((_, v), u)| v == u);

    let rewritten = if line_by_line {
        for (unit, new_uri) in moved {
            let (line, old) = &uri_lines[*unit];
            lines[*line] = lines[*line].replacen(old.as_str(), new_uri, 1);
        }
        lines.join("\n")
    } else {
        let units = document["units"].as_sequence_mut().expect("checked above");
        for (unit, new_uri) in moved {
            units[*unit]["uri"] = serde_yaml::Value::String(new_uri.clone());
        }
        serde_yaml::to_string(&document)?
    };

    // Check the result before it replaces the file.
    let check: serde_yaml::Value = serde_yaml::from_str(&rewritten)?;
    let units = check["units"].as_sequence().ok_or_else(|| anyhow!("the rewritten swarm file has no units"))?;
    for (i, unit) in units.iter().enumerate() {
        let expected = moved.iter().find(|(m, _)| *m == i).map_or(old_uris[i].as_str(), |(_, uri)| uri.as_str());
        if unit["uri"].as_str() != Some(expected) {
            bail!("rewriting the swarm file went wrong at unit {}", i + 1);
        }
    }
    Ok(rewritten)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_starts_at_80_and_steps_down_by_2() {
        assert_eq!(channels_for_count(1), vec![80]);
        assert_eq!(channels_for_count(3), vec![80, 78, 76]);
        assert_eq!(*channels_for_count(41).last().unwrap(), 0);
    }

    #[test]
    fn parses_channel_lists() {
        assert_eq!(parse_channels("80, 78 76").unwrap().0, vec![80, 78, 76]);
        assert!(parse_channels("").is_err());
        assert!(parse_channels("80, 80").is_err());
        assert!(parse_channels("80, 126").is_err());
        assert_eq!(parse_channels("80, 79").unwrap().1.len(), 1);
    }

    #[test]
    fn splits_evenly_with_the_extra_on_the_first_channels() {
        let new = plan(&[90; 7], &[80, 78, 76]);
        assert_eq!(new, vec![80, 80, 80, 78, 78, 76, 76]);
    }

    #[test]
    fn crazyflies_already_on_a_channel_stay_up_to_its_share() {
        // Three on 78 already, but its share is 2: the first two stay.
        assert_eq!(plan(&[78, 90, 78, 78], &[80, 78]), vec![78, 80, 78, 80]);
    }

    #[test]
    fn moves_as_few_as_possible() {
        // 100 each on 72, 76 and 80 spread over 80, 78 and 76: only the ones
        // on 72 move, to 78.
        let mut current = vec![72; 100];
        current.extend([76; 100]);
        current.extend([80; 100]);
        let new = plan(&current, &channels_for_count(3));
        let moved = current.iter().zip(&new).filter(|(a, b)| a != b).count();
        assert_eq!(moved, 100);
        assert!(new[..100].iter().all(|c| *c == 78));
    }

    #[test]
    fn planning_again_changes_nothing() {
        let current = vec![90, 90, 80, 72, 72, 72, 76];
        let channels = channels_for_count(3);
        let new = plan(&current, &channels);
        assert_eq!(plan(&new, &channels), new);
    }

    #[test]
    fn more_channels_than_crazyflies() {
        assert_eq!(plan(&[90, 90], &[80, 78, 76]), vec![80, 78]);
    }

    fn units(uris: &[&str]) -> Vec<Unit> {
        uris.iter()
            .enumerate()
            .map(|(i, uri)| Unit { name: format!("CF-{:02}", i + 1), uri: uri.to_string() })
            .collect()
    }

    #[test]
    fn places_radio_crazyflies_and_skips_the_rest() {
        let placements = place(&units(&["radio:///90/2M/E7E7E7E701", "usb://0", "radio://0/80/2M/E7E7E7E702?timeout=2000"]), &[80, 78]).unwrap();
        assert_eq!(placements[0].new_uri, "radio:///78/2M/E7E7E7E701");
        assert!(placements[0].moves());
        assert!(!placements[1].moves());
        assert_eq!(placements[2].new_uri, "radio://0/80/2M/E7E7E7E702?timeout=2000");
        assert!(!placements[2].moves());
    }

    #[test]
    fn refuses_two_crazyflies_on_one_channel_with_one_address() {
        let swarm = units(&["radio:///80/2M/E7E7E7E7E7", "radio:///90/2M/E7E7E7E7E7"]);
        let err = place(&swarm, &[80]).unwrap_err();
        assert!(format!("{:#}", err).contains("CF-01 and CF-02 would both be on channel 80"), "{:#}", err);
        assert!(place(&swarm, &[80, 78]).is_ok());
    }

    #[test]
    fn rewrites_only_the_uri_lines_of_moved_units() {
        let text = "name: Lab # the bench\nunits:\n  - uri: radio://0/80/2M/E7E7E7E701\n    name: CF-01\n  - uri: \"radio:///80/2M/E7E7E7E702\"\n    name: CF-02\n";
        let out = rewrite_swarm_file(text, &[(1, "radio:///78/2M/E7E7E7E702".to_string())]).unwrap();
        assert_eq!(out, text.replace("radio:///80/2M/E7E7E7E702", "radio:///78/2M/E7E7E7E702"));
    }

    #[test]
    fn rewrites_flow_style_files_as_a_whole() {
        let text = "name: Lab\nunits: [{uri: radio://0/80/2M/E7E7E7E701, name: CF-01, color: red}]\n";
        let out = rewrite_swarm_file(text, &[(0, "radio://0/78/2M/E7E7E7E701".to_string())]).unwrap();
        let doc: serde_yaml::Value = serde_yaml::from_str(&out).unwrap();
        assert_eq!(doc["units"][0]["uri"].as_str(), Some("radio://0/78/2M/E7E7E7E701"));
        assert_eq!(doc["units"][0]["color"].as_str(), Some("red"));
    }
}
