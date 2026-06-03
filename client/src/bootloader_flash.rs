//! Over-the-air firmware flashing for a single Crazyflie.
//!
//! This is a focused port of the flashing pipeline in `cfcli`
//! (`src/modules/bootloader.rs`), reduced to flashing one `(target, binary)`
//! pair at a time and reporting byte-level progress through a callback so the
//! UI can show a per-Crazyflie progress bar.
//!
//! Three categories of target are supported, mirroring the hardcoded cfcli
//! target list:
//! - `stm32-fw` / `nrf51-fw`: flashed over the radio bootloader via `cfloader`.
//! - `deckctrl-fw` / `deckctrl-cfg`: flashed over the DeckCtrl DFU memory.
//! - everything else (`bcAI:esp-fw`, `bcLighthouse4-fw`, ...): flashed into the
//!   matching deck-memory section.

use anyhow::{anyhow, bail, Result};
use cfloader::{Bllink, CFLoader};
use crazyflie_lib::subsystems::memory::{DeckMemory, MemoryType, RawMemory};
use crazyflie_lib::Crazyflie;
use crazyflie_link::{Connection, LinkContext, Packet};
use tokio::time::{sleep, Duration};

const TARGET_NRF51: u8 = 0xFE;

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

enum TargetKind {
    Stm32,
    Nrf51,
    DeckCtrl { is_cfg: bool },
    Deck { section: String },
}

fn classify(target: &str) -> TargetKind {
    match target {
        "stm32-fw" => TargetKind::Stm32,
        "nrf51-fw" => TargetKind::Nrf51,
        "deckctrl-fw" => TargetKind::DeckCtrl { is_cfg: false },
        "deckctrl-cfg" => TargetKind::DeckCtrl { is_cfg: true },
        other => TargetKind::Deck {
            section: other.to_string(),
        },
    }
}

/// Flash `data` to `target` on the Crazyflie at `uri`.
///
/// `progress` is called with `(bytes_written, total_bytes)` as flashing
/// proceeds; `total_bytes` equals `data.len()`.
///
/// Note for `stm32-fw` / `nrf51-fw`: `cfloader` opens the Crazyradio itself (it
/// links its own crazyradio version), so the radio must be free — disconnect
/// every unit on that radio before calling, and flash one Crazyflie at a time.
pub async fn flash_target<T, F>(
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
    match classify(target) {
        TargetKind::Stm32 => {
            flash_bootloader_target(link_context, uri, true, data, progress).await
        }
        TargetKind::Nrf51 => {
            flash_bootloader_target(link_context, uri, false, data, progress).await
        }
        TargetKind::Deck { section } => {
            flash_deck_section(link_context, uri, toc_cache, &section, data, progress).await
        }
        TargetKind::DeckCtrl { is_cfg } => {
            flash_deck_ctrl(link_context, uri, toc_cache, is_cfg, data, progress).await
        }
    }
}

/// Send the nRF51 reset-to-bootloader sequence and return the new 5-byte radio
/// address the bootloader listens on.
async fn reset_and_get_bootloader_address(link: &Connection) -> Result<[u8; 5]> {
    // Disable safelink so we can send raw "bootloader" messages to the nRF51
    let packet: Packet = vec![0xFF, TARGET_NRF51, 0xFF, 0x05, 0x00].into();
    link.send_packet(packet).await?;

    let packet: Packet = vec![0xFF, TARGET_NRF51, 0xFF].into();
    link.send_packet(packet).await?;

    let mut new_address = Vec::new();
    loop {
        let packet = tokio::select! {
            result = link.recv_packet() => result?,
            _ = sleep(Duration::from_millis(100)) => {
                return Err(anyhow!("Timeout waiting for bootloader address"));
            }
        };
        let data = packet.get_data();
        if data.len() > 2 && data[0..2] == [TARGET_NRF51, 0xFF] {
            new_address.push(0xb1);
            for byte in data[2..6].iter().rev() {
                new_address.push(*byte);
            }
            break;
        }
    }

    for _ in 0..10 {
        let packet: Packet = vec![0xFF, TARGET_NRF51, 0xF0, 0x00].into();
        link.send_packet(packet).await?;
    }
    sleep(Duration::from_millis(500)).await;

    new_address
        .try_into()
        .map_err(|_| anyhow!("Bootloader address must be exactly 5 bytes"))
}

/// Flash the STM32 or nRF51 over the radio bootloader using `cfloader`.
async fn flash_bootloader_target<F>(
    link_context: &LinkContext,
    uri: &str,
    is_stm32: bool,
    data: &[u8],
    progress: F,
) -> Result<()>
where
    F: FnMut(usize, usize) + Send,
{
    let separator = if uri.contains('?') { "&" } else { "?" };
    let link = link_context
        .open_link(&format!("{}{}safelink=0", uri, separator))
        .await?;
    let address = reset_and_get_bootloader_address(&link).await?;
    link.close().await;
    sleep(Duration::from_millis(500)).await;

    // cfloader opens the Crazyradio itself (its own crazyradio version), so the
    // radio must be free at this point — the caller disconnects all units first.
    let bllink = Bllink::new(Some(&address)).await?;
    let mut cfloader = CFLoader::new(bllink).await?;

    if is_stm32 {
        let start_address = {
            let info = cfloader.stm32_info();
            info.flash_start() as u32 * info.page_size() as u32
        };
        cfloader
            .flash_stm32_with_progress(start_address, data, Some(progress))
            .await?;
    } else {
        let start_address = {
            let info = cfloader.nrf51_info();
            info.flash_start() as u32 * info.page_size() as u32
        };
        cfloader
            .flash_nrf51_with_progress(start_address, data, Some(progress))
            .await?;
    }

    cfloader.reset_to_firmware().await?;
    Ok(())
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
    let cf = Crazyflie::connect_from_uri(link_context, uri, toc_cache).await?;

    let memories = cf.memory.get_memories(Some(MemoryType::DeckMemory));
    if memories.is_empty() {
        cf.disconnect().await;
        bail!("No deck memory found (is a deck attached?)");
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
        .ok_or_else(|| {
            anyhow!(
                "Deck section '{}' not found (deck not attached?)",
                section_name
            )
        })?;

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
    let cf = Crazyflie::connect_from_uri(link_context, uri, toc_cache.clone()).await?;
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
    let cf = Crazyflie::connect_from_uri(link_context, uri, toc_cache).await?;
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
        bail!("DeckCtrlDFU memory not present, cannot flash DeckCtrl");
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
