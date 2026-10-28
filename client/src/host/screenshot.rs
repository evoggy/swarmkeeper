//! Screenshots of the UI without a display, for checking a layout:
//!
//! ```sh
//! SWARMKEEPER_SCREENSHOT=units.png SWARMKEEPER_SCREEN=units SWARMKEEPER_THEME=dark cargo run
//! # also SWARMKEEPER_SIZE=1280x800, SWARMKEEPER_SELECT=<row>, SWARMKEEPER_FILTER=<chip>
//! ```
//!
//! Shows the demo swarm (`crate::demo`) and draws one frame with Slint's
//! software renderer, so the OpenGL views stay empty. Nothing connects.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use slint::platform::software_renderer::{MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType};
use slint::platform::{Platform, WindowAdapter};
use slint::{ComponentHandle, PhysicalSize, PlatformError};

use crate::{AppWindow, Theme};

struct Headless(Rc<MinimalSoftwareWindow>);

impl Platform for Headless {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.0.clone())
    }
}

/// Where to save a screenshot, when one is asked for.
pub fn requested() -> Option<PathBuf> {
    std::env::var_os("SWARMKEEPER_SCREENSHOT").map(PathBuf::from)
}

/// Draw the screen named by `SWARMKEEPER_SCREEN` (default `units`) in the
/// theme named by `SWARMKEEPER_THEME` (`light` or `dark`) and save it.
pub fn take(path: &Path) {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Headless(window.clone()))).expect("no other Slint platform yet");

    let ui = AppWindow::new().expect("Failed to create window");
    let size = std::env::var("SWARMKEEPER_SIZE").ok().and_then(|s| {
        let (w, h) = s.split_once('x')?;
        Some(PhysicalSize::new(w.parse().ok()?, h.parse().ok()?))
    });
    ui.window().set_size(size.unwrap_or(PhysicalSize::new(1440, 900)));
    ui.global::<Theme>().set_dark(std::env::var("SWARMKEEPER_THEME").is_ok_and(|t| t == "dark"));
    ui.set_screen(std::env::var("SWARMKEEPER_SCREEN").unwrap_or_else(|_| "units".into()).into());
    crate::demo::show(&ui);
    // A row to select (opens the unit panel) and a filter chip.
    if let Some(row) = std::env::var("SWARMKEEPER_SELECT").ok().and_then(|r| r.parse().ok()) {
        ui.global::<crate::ViewSettings>().set_selected_unit(row);
    }
    if let Some(filter) = std::env::var("SWARMKEEPER_FILTER").ok().and_then(|f| f.parse().ok()) {
        ui.set_unit_filter(filter);
        crate::units::rebuild_table_rows(&ui);
    }
    ui.show().expect("show");

    slint::platform::update_timers_and_animations();
    let size = window.size();
    let (width, height) = (size.width as usize, size.height as usize);
    let mut pixels = vec![PremultipliedRgbaColor::default(); width * height];
    window.request_redraw();
    window.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, width);
    });
    let rgb: Vec<u8> = pixels.iter().flat_map(|p| [p.red, p.green, p.blue]).collect();
    match write_png(path, width as u32, height as u32, &rgb) {
        Ok(()) => eprintln!("Saved {}", path.display()),
        Err(e) => eprintln!("Saving {}: {}", path.display(), e),
    }
}

/// An uncompressed RGB PNG: no dependency for a debugging aid.
fn write_png(path: &Path, width: u32, height: u32, rgb: &[u8]) -> std::io::Result<()> {
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let crc = crc32(&out[start..]);
        out.extend_from_slice(&crc.to_be_bytes());
    }
    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &b in bytes {
            crc ^= b as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
            }
        }
        !crc
    }

    // Scanlines, each with filter type 0.
    let stride = width as usize * 3;
    let mut raw = Vec::with_capacity((stride + 1) * height as usize);
    for row in rgb.chunks(stride) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    // zlib stream of stored deflate blocks.
    let mut zlib = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65_535).collect();
    for (i, block) in blocks.iter().enumerate() {
        zlib.push(if i + 1 == blocks.len() { 1 } else { 0 });
        let len = block.len() as u16;
        zlib.extend_from_slice(&len.to_le_bytes());
        zlib.extend_from_slice(&(!len).to_le_bytes());
        zlib.extend_from_slice(block);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in &raw {
        a = (a + byte as u32) % 65_521;
        b = (b + a) % 65_521;
    }
    zlib.extend_from_slice(&((b << 16) | a).to_be_bytes());

    let mut header = Vec::new();
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&[8, 2, 0, 0, 0]);

    let mut png = vec![0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1A, b'\n'];
    chunk(&mut png, b"IHDR", &header);
    chunk(&mut png, b"IDAT", &zlib);
    chunk(&mut png, b"IEND", &[]);
    std::fs::File::create(path)?.write_all(&png)
}
