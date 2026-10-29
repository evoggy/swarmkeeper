//! Screenshots of the UI without a display, for checking a layout:
//!
//! ```sh
//! SWARMKEEPER_SCREENSHOT=units.png SWARMKEEPER_SCREEN=units SWARMKEEPER_THEME=dark cargo run
//! # also SWARMKEEPER_SIZE=1280x800, SWARMKEEPER_SELECT=<row>, SWARMKEEPER_FILTER=<chip>,
//! # SWARMKEEPER_HOVER=<row> (3D view), SWARMKEEPER_SCENE=flying,
//! # SWARMKEEPER_CLICK="x,y;x,y" (clicks first, to open a menu),
//! # SWARMKEEPER_PLAN=<scene.yaml> (Planning; default the arena scene in plans/)
//! ```
//!
//! Shows the demo swarm (`crate::demo`) and draws one frame with Slint's
//! software renderer. The main 3D view is drawn with OpenGL ES without a
//! window (EGL) and pasted in; the other OpenGL views stay empty. Nothing
//! connects.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use slint::platform::software_renderer::{MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType};
use slint::platform::{Platform, WindowAdapter};
use slint::{ComponentHandle, PhysicalSize, PlatformError};

use crate::planning_ui::PlanningState;
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
    let screen = std::env::var("SWARMKEEPER_SCREEN").unwrap_or_else(|_| "units".into());
    ui.set_screen(screen.as_str().into());
    crate::demo::show(&ui, std::env::var("SWARMKEEPER_SCENE").is_ok_and(|s| s == "flying"));
    // A row to select (opens the unit panel) and a filter chip.
    if let Some(row) = std::env::var("SWARMKEEPER_SELECT").ok().and_then(|r| r.parse().ok()) {
        ui.global::<crate::ViewSettings>().set_selected_unit(row);
    }
    // Fit and the view presets in the 3D view
    let ui_weak = ui.as_weak();
    ui.on_viz_fit(move || {
        if let Some(ui) = ui_weak.upgrade() {
            crate::view3d::fit(&ui);
        }
    });
    // Planning: a scene file, its coverage computed, and picking objects
    let plan = (screen == "planning").then(|| load_plan(&ui));
    let ui_weak = ui.as_weak();
    ui.on_planning_select(move |kind, index| {
        let Some(ui) = ui_weak.upgrade() else { return };
        let (object, area) = match kind {
            1..=3 => ((kind, index), (0, -1)),
            4 | 5 => ((0, -1), (kind - 3, index)),
            _ => ((0, -1), (0, -1)),
        };
        ui.set_planning_selected_type(object.0);
        ui.set_planning_selected_index(object.1);
        ui.set_planning_selected_area_kind(area.0);
        ui.set_planning_selected_area_index(area.1);
    });
    // The unit under the pointer in the 3D view
    if let Some(row) = std::env::var("SWARMKEEPER_HOVER").ok().and_then(|r| r.parse().ok()) {
        ui.set_viz_hover_unit(row);
    }
    if let Some(filter) = std::env::var("SWARMKEEPER_FILTER").ok().and_then(|f| f.parse().ok()) {
        ui.set_unit_filter(filter);
        crate::units::rebuild_table_rows(&ui);
    }
    ui.show().expect("show");

    slint::platform::update_timers_and_animations();
    // Clicks before the picture, to open a menu: "x,y;x,y"
    for click in std::env::var("SWARMKEEPER_CLICK").unwrap_or_default().split(';').filter(|c| !c.is_empty()) {
        use slint::platform::{PointerEventButton, WindowEvent};
        let Some((x, y)) = click.split_once(',').and_then(|(x, y)| Some((x.parse().ok()?, y.parse().ok()?))) else { continue };
        let position = slint::LogicalPosition::new(x, y);
        window.dispatch_event(WindowEvent::PointerMoved { position });
        window.dispatch_event(WindowEvent::PointerPressed { position, button: PointerEventButton::Left });
        window.dispatch_event(WindowEvent::PointerReleased { position, button: PointerEventButton::Left });
        slint::platform::update_timers_and_animations();
    }
    // A click may have changed Settings › Features
    crate::settings::apply_features(&ui);
    // Keep the context until the frame is drawn
    let _gl = draw_gl_view(&ui, &screen, plan);
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

type OffscreenGl = (glutin::api::egl::context::PossiblyCurrentContext, glutin::api::egl::display::Display);

/// An OpenGL ES 3 context without a window or a surface.
fn offscreen_gl() -> Result<(OffscreenGl, glow::Context), Box<dyn std::error::Error>> {
    use glutin::api::egl::{device::Device, display::Display};
    use glutin::config::{ConfigSurfaceTypes, ConfigTemplateBuilder};
    use glutin::context::{ContextApi, ContextAttributesBuilder, Version};
    use glutin::prelude::*;

    let device = Device::query_devices()?.next().ok_or("no EGL device")?;
    let display = unsafe { Display::with_device(&device, None)? };
    let template = ConfigTemplateBuilder::new().with_surface_type(ConfigSurfaceTypes::empty()).build();
    let config = unsafe { display.find_configs(template)? }.next().ok_or("no EGL config")?;
    let attributes = ContextAttributesBuilder::new().with_context_api(ContextApi::Gles(Some(Version::new(3, 0)))).build(None);
    let context = unsafe { display.create_context(&config, &attributes)? }.make_current_surfaceless()?;
    let gl = unsafe { glow::Context::from_loader_function_cstr(|name| display.get_proc_address(name)) };
    Ok(((context, display), gl))
}

/// Draw the OpenGL view of `screen` and show it as a picture: the main 3D
/// view around the demo swarm, or Planning.
fn draw_gl_view(ui: &AppWindow, screen: &str, plan: Option<PlanningState>) -> Option<OffscreenGl> {
    let (keep, gl) = offscreen_gl().map_err(|e| eprintln!("No OpenGL ES without a window ({}): the 3D view stays empty", e)).ok()?;
    let mut renderer = crate::host::renderer::Scene3DRenderer::new(gl);
    match plan {
        Some(mut ps) => {
            ps.selected_area_kind = ui.get_planning_selected_area_kind();
            ps.selected_area_index = ui.get_planning_selected_area_index();
            crate::host::rendering::draw_planning_view(ui, &mut renderer, &std::sync::Mutex::new(ps));
            ui.set_planning_texture(slint::Image::from_rgba8(renderer.read_pixels()));
        }
        None if screen == "view3d" => {
            crate::host::rendering::draw_main_view(ui, &mut renderer, &crate::demo::scene());
            ui.set_viz_texture(slint::Image::from_rgba8(renderer.read_pixels()));
        }
        None => {}
    }
    Some(keep)
}

/// Read the scene in `SWARMKEEPER_PLAN` (default: the arena scene in
/// `plans/`) into Planning and compute its coverage.
fn load_plan(ui: &AppWindow) -> PlanningState {
    use crate::planning_ui::{apply_coverage, apply_scene, coverage_job, run_coverage};
    let mut ps = PlanningState::default();
    let path = std::env::var("SWARMKEEPER_PLAN").unwrap_or_else(|_| "plans/bitcraze_arena_lh4_and_loco.yaml".into());
    match crate::planning::load_scene(Path::new(&path)) {
        Ok(scene) => {
            let name = Path::new(&path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            apply_scene(ui, &mut ps, &scene, &name);
            let out = run_coverage(coverage_job(ui, &ps));
            apply_coverage(ui, &mut ps, out);
        }
        Err(e) => eprintln!("Reading {}: {}", path, e),
    }
    ps
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
