//! Firmware flashing for a single Crazyflie.
//!
//! The targets are the same hardcoded list as cfcli's:
//! - `stm32-fw` / `nrf51-fw`: flashed over the radio bootloader by the
//!   connection server ([`crazyflie_connection_server::Client::flash`]), which
//!   shares the Crazyradio with the other units' links. See
//!   [`bootloader_target`].
//! - `deckctrl-fw` / `deckctrl-cfg`: flashed over the DeckCtrl DFU memory.
//! - everything else (`bcAI:esp-fw`, `bcLighthouse4-fw`, ...): flashed into the
//!   matching deck-memory section.
//!
//! The deck targets are a port of cfcli's pipeline (`src/modules/bootloader.rs`)
//! and use crazyflie-lib directly, so they only work when Swarmkeeper drives
//! the Crazyradios.

use anyhow::{anyhow, bail, Result};
use crazyflie_lib::subsystems::memory::{DeckMemory, MemoryType, RawMemory};
use crazyflie_lib::Crazyflie;
use crazyflie_link::{LinkContext, Packet};
use tokio::time::{sleep, Duration};

const TARGET_NRF51: u8 = 0xFE;

/// How long connecting to a Crazyflie for its decks may take. A connect that
/// reads a stale answer left by an earlier link (a version answer to a
/// connection server's probe) can otherwise wait forever.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

// DeckCtrl DFU memory layout (matches cfcli)
const DECK_CTRL_DFU_FLASH_OFFSET: usize = 0x10000;
const DECK_CTRL_DFU_CFG_DEFAULT_OFFSET: usize = 0x17800;
const DECK_CTRL_DFU_CMD_OFFSET: usize = 0x03;
const DECK_CTRL_DFU_CMD_ENTER_DFU: u8 = 0x01;
const DECK_CTRL_DFU_CMD_ENTER_FIRMWARE: u8 = 0x02;
const DECK_CTRL_DFU_RESET_DELAY_MS: u64 = 3000;
const DECK_CTRL_DFU_STATUS_IN_DFU_MODE: u8 = 1 << 0;
const DECK_CTRL_DFU_STATUS_CAN_ENABLE_DFU: u8 = 1 << 1;

/// The hardcoded list of flashable targets, in the same order as `cfcli`.
pub fn target_list() -> &'static [&'static str] {
    &[
        "stm32-fw",
        "nrf51-fw",
        "bcAI:esp-fw",
        "bcCam:qcc",
        "bcLighthouse4-fw",
        "bcColorLedTop:col-fw",
        "bcColorLedBot:col-fw",
        "deckctrl-fw",
        "deckctrl-cfg",
    ]
}

/// The connection server's name for a target flashed over the radio
/// bootloader (`stm32-fw` → `stm32`), `None` for the deck targets.
pub fn bootloader_target(target: &str) -> Option<&'static str> {
    match target {
        "stm32-fw" => Some("stm32"),
        "nrf51-fw" => Some("nrf51"),
        _ => None,
    }
}

/// The Crazyflie doesn't have the deck a target is for: nothing to flash.
#[derive(Debug)]
pub struct NoDeck(pub String);

impl std::fmt::Display for NoDeck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for NoDeck {}

/// Flash `data` to the deck target `target` on the Crazyflie at `uri`, over
/// a connection of its own: the unit must not be connected. Fails with
/// [`NoDeck`] when it doesn't have the deck.
///
/// `progress` is called with `(bytes_written, total_bytes)` as flashing
/// proceeds; `total_bytes` equals `data.len()`.
pub async fn flash_deck_target<T, F>(
    link_context: &LinkContext,
    uri: &str,
    toc_cache: T,
    target: &str,
    data: &[u8],
    progress: F,
) -> Result<()>
where
    T: crazyflie_lib::TocCache + Clone,
    F: FnMut(usize, usize) + Send,
{
    match target {
        "deckctrl-fw" => flash_deck_ctrl(link_context, uri, toc_cache, false, data, progress).await,
        "deckctrl-cfg" => flash_deck_ctrl(link_context, uri, toc_cache, true, data, progress).await,
        deck if bootloader_target(deck).is_none() => {
            flash_deck_section(link_context, uri, toc_cache, deck_section(deck), data, progress).await
        }
        other => bail!("{} is flashed through the connection server", other),
    }
}

/// The deck memory section a deck target is flashed into, as in cfcli:
/// `bcColorLedBot:col-fw` is the firmware (`fw`) of section
/// `bcColorLedBot:col`, and a target without a type (`bcCam:qcc`) is the
/// section itself.
fn deck_section(target: &str) -> &str {
    target.split('-').next().unwrap_or(target)
}

/// Connect for flashing a deck, trying once more when the first connect
/// fails or doesn't finish: the second starts on a link with nothing left
/// over from before.
async fn connect<T>(link_context: &LinkContext, uri: &str, toc_cache: T) -> Result<Crazyflie>
where
    T: crazyflie_lib::TocCache + Clone,
{
    let mut last = anyhow!("not tried");
    for attempt in 1..=2 {
        match tokio::time::timeout(CONNECT_TIMEOUT, Crazyflie::connect_from_uri(link_context, uri, toc_cache.clone())).await {
            Ok(Ok(cf)) => return Ok(cf),
            Ok(Err(e)) => last = anyhow!("connecting: {}", e),
            Err(_) => last = anyhow!("connecting: no answer within {} s", CONNECT_TIMEOUT.as_secs()),
        }
        if attempt == 1 {
            eprintln!("{}: {:#}; trying again", uri, last);
            sleep(Duration::from_secs(1)).await;
        }
    }
    Err(last)
}

/// Reboot the Crazyflie from bootloader/deck-bootloader back into firmware.
async fn reboot(link_context: &LinkContext, uri: &str) -> Result<()> {
    let link = link_context.open_link(uri).await?;
    // ResetInit
    let packet: Packet = vec![0xFF, TARGET_NRF51, 0xFF].into();
    link.send_packet(packet).await?;
    // Reset to firmware
    let packet: Packet = vec![0xFF, TARGET_NRF51, 0xF0, 0x01].into();
    link.send_packet(packet).await?;
    sleep(Duration::from_millis(500)).await;
    link.close().await;
    Ok(())
}

/// Flash a deck firmware into its deck-memory section (e.g. `bcAI:esp-fw`).
async fn flash_deck_section<T, F>(
    link_context: &LinkContext,
    uri: &str,
    toc_cache: T,
    section_name: &str,
    data: &[u8],
    progress: F,
) -> Result<()>
where
    T: crazyflie_lib::TocCache + Clone,
    F: FnMut(usize, usize) + Send,
{
    let cf = connect(link_context, uri, toc_cache).await?;

    let memories = cf.memory.get_memories(Some(MemoryType::DeckMemory));
    if memories.is_empty() {
        cf.disconnect().await;
        bail!(NoDeck("No deck memory found (is a deck attached?)".into()));
    }

    let deck_memory = match cf.memory.open_memory::<DeckMemory>(memories[0].clone()).await {
        Some(Ok(deck)) => deck,
        Some(Err(e)) => {
            cf.disconnect().await;
            bail!("Could not open deck memory: {:?}", e);
        }
        None => {
            cf.disconnect().await;
            bail!("Deck memory not found");
        }
    };

    let result = flash_deck_section_inner(&deck_memory, section_name, data, progress).await;

    cf.memory.close_memory(deck_memory).await.ok();
    cf.disconnect().await;

    result?;

    // Reboot so the deck exits its bootloader and runs the new firmware.
    reboot(link_context, uri).await?;
    Ok(())
}

async fn flash_deck_section_inner<F>(
    deck_memory: &DeckMemory,
    section_name: &str,
    data: &[u8],
    progress: F,
) -> Result<()>
where
    F: FnMut(usize, usize) + Send,
{
    let section = deck_memory
        .sections()
        .iter()
        .find(|s| s.name() == section_name)
        .ok_or_else(|| anyhow!(NoDeck(format!("Deck section '{}' not found (deck not attached?)", section_name))))?;

    if !section.bootloader_active().await? {
        section.reset_to_bootloader().await?;
        // The deck may take a couple of seconds to complete the ROM-bootloader
        // handshake before flipping the active bit, so poll up to 5 s.
        let mut active = false;
        for _ in 0..50 {
            sleep(Duration::from_millis(100)).await;
            if section.bootloader_active().await? {
                active = true;
                break;
            }
        }
        if !active {
            bail!("Failed to activate bootloader for deck section '{}'", section_name);
        }
    }

    section.flash_firmware_with_progress(data, progress).await?;
    Ok(())
}

/// Flash a DeckCtrl firmware or config over the DeckCtrl DFU memory.
async fn flash_deck_ctrl<T, F>(
    link_context: &LinkContext,
    uri: &str,
    toc_cache: T,
    is_cfg: bool,
    data: &[u8],
    progress: F,
) -> Result<()>
where
    T: crazyflie_lib::TocCache + Clone,
    F: FnMut(usize, usize) + Send,
{
    let cf = connect(link_context, uri, toc_cache.clone()).await?;
    let raw = open_deck_ctrl_dfu_raw(&cf).await?;
    let header = read_deck_ctrl_dfu_header(&raw).await?;

    if header.version != 1 {
        cf.disconnect().await;
        bail!("Unsupported DeckCtrlDFU version: {}", header.version);
    }

    let already_in_dfu = (header.status & DECK_CTRL_DFU_STATUS_IN_DFU_MODE) != 0;
    if !already_in_dfu {
        if header.deck_ctrl_count > 1 {
            cf.disconnect().await;
            bail!(
                "Cannot enter DFU: expected at most one DeckCtrl deck attached, found {}",
                header.deck_ctrl_count
            );
        }
        if (header.status & DECK_CTRL_DFU_STATUS_CAN_ENABLE_DFU) == 0 {
            cf.disconnect().await;
            bail!("Cannot enter DFU: STATUS_CAN_ENABLE_DFU is not set");
        }
        raw.write(DECK_CTRL_DFU_CMD_OFFSET, &[DECK_CTRL_DFU_CMD_ENTER_DFU])
            .await?;
        cf.disconnect().await;
        sleep(Duration::from_millis(DECK_CTRL_DFU_RESET_DELAY_MS)).await;
    } else {
        cf.disconnect().await;
    }

    // Reconnect — DFU entry power-cycles the Crazyflie and memory IDs may be
    // re-enumerated, so re-look up the memory after reconnecting.
    let cf = connect(link_context, uri, toc_cache).await?;
    let raw = open_deck_ctrl_dfu_raw(&cf).await?;
    let header = read_deck_ctrl_dfu_header(&raw).await?;
    if (header.status & DECK_CTRL_DFU_STATUS_IN_DFU_MODE) == 0 {
        cf.disconnect().await;
        bail!("DeckCtrl did not enter DFU mode");
    }

    let address = if is_cfg {
        DECK_CTRL_DFU_CFG_DEFAULT_OFFSET
    } else {
        DECK_CTRL_DFU_FLASH_OFFSET
    };

    let flash_result = raw.write_with_progress(address, data, progress).await;

    // Always try to leave DFU mode and reboot, even on a failed write.
    raw.write(DECK_CTRL_DFU_CMD_OFFSET, &[DECK_CTRL_DFU_CMD_ENTER_FIRMWARE])
        .await
        .ok();
    cf.disconnect().await;
    sleep(Duration::from_millis(DECK_CTRL_DFU_RESET_DELAY_MS)).await;

    flash_result?;
    Ok(())
}

async fn open_deck_ctrl_dfu_raw(cf: &Crazyflie) -> Result<RawMemory> {
    let memories = cf.memory.get_memories(Some(MemoryType::DeckCtrlDFU));
    if memories.is_empty() {
        bail!(NoDeck("DeckCtrlDFU memory not present, cannot flash DeckCtrl".into()));
    }
    if memories.len() > 1 {
        bail!(
            "Multiple DeckCtrlDFU memories found ({}), cannot flash DeckCtrl",
            memories.len()
        );
    }
    match cf.memory.open_memory::<RawMemory>(memories[0].clone()).await {
        Some(Ok(m)) => Ok(m),
        Some(Err(e)) => bail!("Could not access DeckCtrlDFU memory: {}", e),
        None => bail!("DeckCtrlDFU memory not found"),
    }
}

struct DeckCtrlDfuHeader {
    version: u8,
    deck_ctrl_count: u8,
    status: u8,
}

async fn read_deck_ctrl_dfu_header(raw: &RawMemory) -> Result<DeckCtrlDfuHeader> {
    let header = raw.read(0, 4).await?;
    Ok(DeckCtrlDfuHeader {
        version: header[0],
        deck_ctrl_count: header[1],
        status: header[2],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deck_targets_name_their_section() {
        assert_eq!(deck_section("bcColorLedBot:col-fw"), "bcColorLedBot:col");
        assert_eq!(deck_section("bcAI:esp-fw"), "bcAI:esp");
        assert_eq!(deck_section("bcLighthouse4-fw"), "bcLighthouse4");
        assert_eq!(deck_section("bcCam:qcc"), "bcCam:qcc");
    }
}
