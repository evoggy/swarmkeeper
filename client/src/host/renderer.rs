use std::num::NonZeroU32;
use std::rc::Rc;

use glow::HasContext;

macro_rules! define_scoped_binding {
    (struct $binding_ty_name:ident => $obj_name:path, $param_name:path, $binding_fn:ident, $target_name:path) => {
        struct $binding_ty_name {
            saved_value: Option<$obj_name>,
            gl: Rc<glow::Context>,
        }

        impl $binding_ty_name {
            unsafe fn new(gl: &Rc<glow::Context>, new_binding: Option<$obj_name>) -> Self {
                unsafe {
                    let saved_value =
                        NonZeroU32::new(gl.get_parameter_i32($param_name) as u32).map($obj_name);
                    gl.$binding_fn($target_name, new_binding);
                    Self { saved_value, gl: gl.clone() }
                }
            }
        }

        impl Drop for $binding_ty_name {
            fn drop(&mut self) {
                unsafe {
                    self.gl.$binding_fn($target_name, self.saved_value);
                }
            }
        }
    };
    (struct $binding_ty_name:ident => $obj_name:path, $param_name:path, $binding_fn:ident) => {
        struct $binding_ty_name {
            saved_value: Option<$obj_name>,
            gl: Rc<glow::Context>,
        }

        impl $binding_ty_name {
            unsafe fn new(gl: &Rc<glow::Context>, new_binding: Option<$obj_name>) -> Self {
                unsafe {
                    let saved_value =
                        NonZeroU32::new(gl.get_parameter_i32($param_name) as u32).map($obj_name);
                    gl.$binding_fn(new_binding);
                    Self { saved_value, gl: gl.clone() }
                }
            }
        }

        impl Drop for $binding_ty_name {
            fn drop(&mut self) {
                unsafe {
                    self.gl.$binding_fn(self.saved_value);
                }
            }
        }
    };
}

define_scoped_binding!(struct ScopedTextureBinding => glow::NativeTexture, glow::TEXTURE_BINDING_2D, bind_texture, glow::TEXTURE_2D);
define_scoped_binding!(struct ScopedFrameBufferBinding => glow::NativeFramebuffer, glow::DRAW_FRAMEBUFFER_BINDING, bind_framebuffer, glow::DRAW_FRAMEBUFFER);
define_scoped_binding!(struct ScopedVBOBinding => glow::NativeBuffer, glow::ARRAY_BUFFER_BINDING, bind_buffer, glow::ARRAY_BUFFER);
define_scoped_binding!(struct ScopedVAOBinding => glow::NativeVertexArray, glow::VERTEX_ARRAY_BINDING, bind_vertex_array);

struct RenderTexture {
    texture: glow::Texture,
    width: u32,
    height: u32,
    fbo: glow::Framebuffer,
    gl: Rc<glow::Context>,
}

impl RenderTexture {
    unsafe fn new(gl: &Rc<glow::Context>, width: u32, height: u32) -> Self {
        unsafe {
            let fbo = gl.create_framebuffer().expect("Unable to create framebuffer");
            let texture = gl.create_texture().expect("Unable to allocate texture");

            let _saved_texture = ScopedTextureBinding::new(gl, Some(texture));

            let old_unpack_alignment = gl.get_parameter_i32(glow::UNPACK_ALIGNMENT);
            let old_unpack_row_length = gl.get_parameter_i32(glow::UNPACK_ROW_LENGTH);
            let old_unpack_skip_pixels = gl.get_parameter_i32(glow::UNPACK_SKIP_PIXELS);
            let old_unpack_skip_rows = gl.get_parameter_i32(glow::UNPACK_SKIP_ROWS);

            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
            gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, width as i32);
            gl.pixel_store_i32(glow::UNPACK_SKIP_PIXELS, 0);
            gl.pixel_store_i32(glow::UNPACK_SKIP_ROWS, 0);

            gl.tex_image_2d(
                glow::TEXTURE_2D, 0, glow::RGBA as _, width as _, height as _, 0,
                glow::RGBA as _, glow::UNSIGNED_BYTE as _, glow::PixelUnpackData::Slice(None),
            );

            let _saved_fbo = ScopedFrameBufferBinding::new(gl, Some(fbo));
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(texture), 0,
            );
            debug_assert_eq!(gl.check_framebuffer_status(glow::FRAMEBUFFER), glow::FRAMEBUFFER_COMPLETE);

            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, old_unpack_alignment);
            gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, old_unpack_row_length);
            gl.pixel_store_i32(glow::UNPACK_SKIP_PIXELS, old_unpack_skip_pixels);
            gl.pixel_store_i32(glow::UNPACK_SKIP_ROWS, old_unpack_skip_rows);

            Self { texture, width, height, fbo, gl: gl.clone() }
        }
    }
}

impl Drop for RenderTexture {
    fn drop(&mut self) {
        unsafe {
            self.gl.delete_framebuffer(self.fbo);
            self.gl.delete_texture(self.texture);
        }
    }
}

/// A Crazyflie in the main 3D view.
#[derive(Clone, Copy, Default)]
pub struct UnitPos {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub color: [f32; 3],
    pub selected: bool,
    pub hovered: bool,
    /// Drawn as a ring: on a pad, charging or charged.
    pub hollow: bool,
}

/// A Lighthouse base station or a Loco anchor.
#[derive(Clone, Copy)]
pub struct FixedPoint {
    pub pos: [f32; 3],
    /// Seen by the units right now. The others are drawn faded.
    pub active: bool,
}

/// The colours of the main 3D view, from `Theme` (theme.slint), and the
/// size of a logical pixel.
#[derive(Clone, Copy)]
pub struct SceneStyle {
    pub scene: [f32; 3],
    pub grid: [f32; 3],
    pub grid_major: [f32; 3],
    pub area: [f32; 3],
    pub waypoint_area: [f32; 3],
    pub pad: [f32; 3],
    pub drop: [f32; 3],
    pub base_station: [f32; 3],
    pub surface: [f32; 3],
    pub axes: [[f32; 3]; 3],
    pub accent: [f32; 3],
    pub accent_tint: [f32; 3],
    /// Rings: the hover ring and the ring of a unit on a pad
    pub muted: [f32; 3],
    /// The walls of a planned room
    pub room: [f32; 3],
    /// Coverage: seen by 0, 1, 2, 3, 4 or more base stations
    pub coverage: [[f32; 3]; 5],
    /// Physical pixels per logical pixel
    pub scale: f32,
}

/// Where the main 3D view looks from (the `cam-*` properties).
#[derive(Clone, Copy)]
pub struct Camera {
    pub yaw: f32,
    pub pitch: f32,
    pub distance: f32,
    pub pan_x: f32,
    pub pan_y: f32,
}

/// Everything the main 3D view draws.
pub struct MainScene<'a> {
    pub units: &'a [UnitPos],
    pub base_stations: &'a [FixedPoint],
    pub anchors: &'a [FixedPoint],
    pub trajectory_lines: &'a [f32],
    pub flight_areas: &'a [FlightAreaBox],
    pub waypoint_areas: &'a [FlightAreaBox],
    pub pads: &'a [PadCircle],
    pub obstacle_triangles: &'a [Vec<f32>],
    pub obstacle_wireframes: &'a [Vec<f32>],
    pub obstacle_colors: &'a [[f32; 3]],
    pub goto_points: &'a [[f32; 3]],
    pub goto_lines: &'a [f32],
    /// Half the side of the floor grid, in metres
    pub grid_half: f32,
}

/// Everything the Planning view draws.
pub struct PlanScene<'a> {
    pub room: [f32; 3],
    pub room_offset: [f32; 3],
    /// Position and rotation
    pub base_stations: &'a [([f32; 3], [[f32; 3]; 3])],
    pub anchors: &'a [[f32; 3]],
    pub obstacle_triangles: &'a [Vec<f32>],
    pub obstacle_wireframes: &'a [Vec<f32>],
    pub obstacle_colors: &'a [[f32; 3]],
    pub flight_areas: &'a [FlightAreaBox],
    pub waypoint_areas: &'a [FlightAreaBox],
    pub pads: &'a [PadCircle],
    pub trajectories: &'a [Vec<[f32; 3]>],
    /// The coverage slice: cells (x, y, colour) at `slice_z`, `cell` wide
    pub slice: &'a [(f32, f32, [f32; 3])],
    pub slice_z: f32,
    pub cell: f32,
    /// 0 none, 1 base station, 2 anchor, 3 obstacle
    pub selected_type: i32,
    pub selected_index: i32,
    pub active_handle: i32,
    /// 0 none, 1 flight area, 2 waypoint area
    pub selected_area_kind: i32,
    pub selected_area_index: i32,
}

/// A unit's diameter on screen, in logical pixels, at `w` (clip-space w,
/// the distance along the view direction): 7.5 cm across, kept between
/// 7 and 16 px as in the design.
pub fn unit_diameter(w: f32, height: f32) -> f32 {
    let focal = (height / 2.0) / (FOV_DEG.to_radians() / 2.0).tan();
    (focal * UNIT_SIZE / w).clamp(UNIT_MIN_PX, UNIT_MAX_PX)
}

/// The vertical field of view
pub const FOV_DEG: f32 = 45.0;
const UNIT_SIZE: f32 = 0.075;
const UNIT_MIN_PX: f32 = 7.0;
const UNIT_MAX_PX: f32 = 16.0;

#[derive(Clone, Copy)]
pub struct PadCircle {
    pub x: f32,
    pub y: f32,
    pub radius: f32,
}

#[derive(Clone, Copy)]
pub struct FlightAreaBox {
    pub min: [f32; 3],
    pub max: [f32; 3],
}

/// Which classes of object the main visualization draws.
///
/// The 3D view's context menu flips these so a crowded scene can be thinned
/// down to whatever the user is actually looking at. Lighthouse base stations
/// and Loco anchors share the `fixed_points` slice and are filtered out by the
/// caller instead of by a flag here.
#[derive(Clone, Copy)]
pub struct VizVisibility {
    pub grid: bool,
    pub axes: bool,
    pub units: bool,
    /// Vertical lines from units, anchors and goto targets down to the floor.
    pub drop_lines: bool,
    pub trajectories: bool,
    pub goto_targets: bool,
    pub areas: bool,
    pub pads: bool,
    pub obstacles: bool,
}

impl Default for VizVisibility {
    fn default() -> Self {
        Self {
            grid: true,
            axes: true,
            units: true,
            drop_lines: true,
            trajectories: true,
            goto_targets: true,
            areas: true,
            pads: true,
            obstacles: true,
        }
    }
}


pub struct Scene3DRenderer {
    gl: Rc<glow::Context>,
    program: glow::Program,
    vbo: glow::Buffer,
    vao: glow::VertexArray,
    u_mvp: glow::UniformLocation,
    u_color: glow::UniformLocation,
    u_point_size: glow::UniformLocation,
    u_alpha: glow::UniformLocation,
    marker: MarkerProgram,
    /// Samples for antialiasing, 0 without (OpenGL ES 2)
    samples: i32,
    msaa: Option<MsaaTarget>,
    displayed_texture: RenderTexture,
    next_texture: RenderTexture,
}

/// Round units and square base stations, drawn as point sprites: a fill,
/// an inset ring and up to three rings around it (MARKER_FRAGMENT_SHADER).
struct MarkerProgram {
    program: glow::Program,
    u_mvp: glow::UniformLocation,
    u_size_k: glow::UniformLocation,
    u_size_min: glow::UniformLocation,
    u_size_max: glow::UniformLocation,
    u_pad: glow::UniformLocation,
    u_fill: glow::UniformLocation,
    u_inner: glow::UniformLocation,
    u_inner_w: glow::UniformLocation,
    u_rings: [(glow::UniformLocation, glow::UniformLocation); 3],
    u_shape: glow::UniformLocation,
    u_corner: glow::UniformLocation,
    u_dot: glow::UniformLocation,
    u_alpha: glow::UniformLocation,
}

/// One marker: what MARKER_FRAGMENT_SHADER draws, sizes in physical pixels.
struct Marker {
    pos: [f32; 3],
    /// Fixed diameter, or None to size it by distance like a unit
    size: Option<f32>,
    fill: [f32; 3],
    inner: ([f32; 3], f32),
    rings: [([f32; 3], f32); 3],
    square: bool,
    alpha: f32,
}

/// A multisampled colour and depth buffer, resolved into the texture.
struct MsaaTarget {
    fbo: glow::Framebuffer,
    color: glow::Renderbuffer,
    depth: glow::Renderbuffer,
    width: u32,
    height: u32,
    gl: Rc<glow::Context>,
}

impl MsaaTarget {
    unsafe fn new(gl: &Rc<glow::Context>, width: u32, height: u32, samples: i32) -> Self {
        unsafe {
            let fbo = gl.create_framebuffer().expect("Unable to create framebuffer");
            let color = gl.create_renderbuffer().expect("Unable to create renderbuffer");
            let depth = gl.create_renderbuffer().expect("Unable to create renderbuffer");
            let saved_rb = NonZeroU32::new(gl.get_parameter_i32(glow::RENDERBUFFER_BINDING) as u32).map(glow::NativeRenderbuffer);
            gl.bind_renderbuffer(glow::RENDERBUFFER, Some(color));
            gl.renderbuffer_storage_multisample(glow::RENDERBUFFER, samples, glow::RGBA8, width as i32, height as i32);
            gl.bind_renderbuffer(glow::RENDERBUFFER, Some(depth));
            gl.renderbuffer_storage_multisample(glow::RENDERBUFFER, samples, glow::DEPTH_COMPONENT24, width as i32, height as i32);
            gl.bind_renderbuffer(glow::RENDERBUFFER, saved_rb);
            let _saved_fbo = ScopedFrameBufferBinding::new(gl, Some(fbo));
            gl.framebuffer_renderbuffer(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::RENDERBUFFER, Some(color));
            gl.framebuffer_renderbuffer(glow::FRAMEBUFFER, glow::DEPTH_ATTACHMENT, glow::RENDERBUFFER, Some(depth));
            debug_assert_eq!(gl.check_framebuffer_status(glow::FRAMEBUFFER), glow::FRAMEBUFFER_COMPLETE);
            Self { fbo, color, depth, width, height, gl: gl.clone() }
        }
    }
}

impl Drop for MsaaTarget {
    fn drop(&mut self) {
        unsafe {
            self.gl.delete_framebuffer(self.fbo);
            self.gl.delete_renderbuffer(self.color);
            self.gl.delete_renderbuffer(self.depth);
        }
    }
}

const VERTEX_SHADER: &str = r#"#version 100
attribute vec3 position;
uniform mat4 u_mvp;
uniform float u_point_size;
void main() {
    gl_Position = u_mvp * vec4(position, 1.0);
    gl_PointSize = u_point_size;
}
"#;

const FRAGMENT_SHADER: &str = r#"#version 100
precision mediump float;
uniform vec3 u_color;
uniform float u_alpha;
void main() {
    gl_FragColor = vec4(u_color, u_alpha);
}
"#;

const MARKER_VERTEX_SHADER: &str = r#"#version 100
attribute vec3 position;
uniform mat4 u_mvp;
uniform float u_size_k;
uniform float u_size_min;
uniform float u_size_max;
// The fragment shader reads it too, at its default precision
uniform mediump float u_pad;
varying mediump float v_r;
void main() {
    gl_Position = u_mvp * vec4(position, 1.0);
    float d = clamp(u_size_k / gl_Position.w, u_size_min, u_size_max);
    v_r = d * 0.5;
    gl_PointSize = d + 2.0 * u_pad;
}
"#;

// From the outside in: three rings (halo, selection, the gap to the scene),
// then the marker with an inset ring, and for a base station a dot.
const MARKER_FRAGMENT_SHADER: &str = r#"#version 100
precision mediump float;
uniform vec3 u_fill;
uniform vec3 u_inner;
uniform float u_inner_w;
uniform vec3 u_ring1;
uniform float u_ring1_w;
uniform vec3 u_ring2;
uniform float u_ring2_w;
uniform vec3 u_ring3;
uniform float u_ring3_w;
uniform float u_shape;
uniform float u_corner;
uniform float u_dot;
uniform float u_pad;
uniform float u_alpha;
varying mediump float v_r;
float inside(float d, float edge) {
    return 1.0 - smoothstep(edge - 0.5, edge + 0.5, d);
}
void main() {
    float size = 2.0 * (v_r + u_pad);
    vec2 p = (gl_PointCoord - 0.5) * size;
    float d;
    if (u_shape < 0.5) {
        d = length(p) - v_r;
    } else {
        vec2 q = abs(p) - vec2(v_r - u_corner);
        d = length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - u_corner;
    }
    float e1 = u_ring1_w;
    float e2 = e1 + u_ring2_w;
    float e3 = e2 + u_ring3_w;
    float a = inside(d, e3);
    if (a <= 0.0) discard;
    vec3 c = u_ring3;
    c = mix(c, u_ring2, inside(d, e2));
    c = mix(c, u_ring1, inside(d, e1));
    c = mix(c, u_inner, inside(d, 0.0));
    c = mix(c, u_fill, inside(d, -u_inner_w));
    if (u_shape > 0.5) {
        vec2 q = abs(p);
        c = mix(c, u_inner, inside(max(q.x, q.y), u_dot));
    }
    gl_FragColor = vec4(c, a * u_alpha);
}
"#;

unsafe fn compile_program(gl: &glow::Context, vertex: &str, fragment: &str) -> glow::Program {
    unsafe {
        let program = gl.create_program().expect("Cannot create program");
        let mut shaders = Vec::new();
        for (shader_type, src) in [(glow::VERTEX_SHADER, vertex), (glow::FRAGMENT_SHADER, fragment)] {
            let shader = gl.create_shader(shader_type).expect("Cannot create shader");
            gl.shader_source(shader, src);
            gl.compile_shader(shader);
            if !gl.get_shader_compile_status(shader) {
                panic!("Shader compile error: {}", gl.get_shader_info_log(shader));
            }
            gl.attach_shader(program, shader);
            shaders.push(shader);
        }
        // Both programs read the vertices from the same VAO.
        gl.bind_attrib_location(program, 0, "position");
        gl.link_program(program);
        if !gl.get_program_link_status(program) {
            panic!("Program link error: {}", gl.get_program_info_log(program));
        }
        for s in shaders {
            gl.detach_shader(program, s);
            gl.delete_shader(s);
        }
        program
    }
}

impl MarkerProgram {
    unsafe fn new(gl: &glow::Context) -> Self {
        unsafe {
            let program = compile_program(gl, MARKER_VERTEX_SHADER, MARKER_FRAGMENT_SHADER);
            let u = |name: &str| gl.get_uniform_location(program, name).unwrap_or_else(|| panic!("no uniform {}", name));
            Self {
                program,
                u_mvp: u("u_mvp"),
                u_size_k: u("u_size_k"),
                u_size_min: u("u_size_min"),
                u_size_max: u("u_size_max"),
                u_pad: u("u_pad"),
                u_fill: u("u_fill"),
                u_inner: u("u_inner"),
                u_inner_w: u("u_inner_w"),
                u_rings: [(u("u_ring1"), u("u_ring1_w")), (u("u_ring2"), u("u_ring2_w")), (u("u_ring3"), u("u_ring3_w"))],
                u_shape: u("u_shape"),
                u_corner: u("u_corner"),
                u_dot: u("u_dot"),
                u_alpha: u("u_alpha"),
            }
        }
    }
}

impl Scene3DRenderer {
    pub fn new(gl: glow::Context) -> Self {
        let gl = Rc::new(gl);
        unsafe {
            let program = compile_program(&gl, VERTEX_SHADER, FRAGMENT_SHADER);
            let marker = MarkerProgram::new(&gl);
            // Antialiasing needs OpenGL ES 3
            let samples = if gl.version().major >= 3 { gl.get_parameter_i32(glow::MAX_SAMPLES).min(4) } else { 0 };

            let u_mvp = gl.get_uniform_location(program, "u_mvp").unwrap();
            let u_color = gl.get_uniform_location(program, "u_color").unwrap();
            let u_point_size = gl.get_uniform_location(program, "u_point_size").unwrap();
            let u_alpha = gl.get_uniform_location(program, "u_alpha").unwrap();

            let vbo = gl.create_buffer().expect("Cannot create buffer");
            let vao = gl.create_vertex_array().expect("Cannot create VAO");

            let pos_loc = 0;
            gl.bind_vertex_array(Some(vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            gl.enable_vertex_attrib_array(pos_loc);
            gl.vertex_attrib_pointer_f32(pos_loc, 3, glow::FLOAT, false, 12, 0);
            gl.bind_buffer(glow::ARRAY_BUFFER, None);
            gl.bind_vertex_array(None);

            let displayed_texture = RenderTexture::new(&gl, 800, 600);
            let next_texture = RenderTexture::new(&gl, 800, 600);

            Self {
                gl, program, vbo, vao, u_mvp, u_color, u_point_size, u_alpha,
                marker, samples, msaa: None,
                displayed_texture, next_texture,
            }
        }
    }

    /// Draw the main 3D view at `width` × `height` physical pixels.
    pub fn render(
        &mut self,
        width: u32,
        height: u32,
        camera: Camera,
        scene: &MainScene,
        vis: VizVisibility,
        style: &SceneStyle,
    ) -> slint::Image {
        let width = width.max(1);
        let height = height.max(1);
        let px = style.scale;

        unsafe {
            let gl = self.gl.clone();
            let gl = &gl;

            if self.next_texture.width != width || self.next_texture.height != height {
                let mut new_tex = RenderTexture::new(gl, width, height);
                std::mem::swap(&mut self.next_texture, &mut new_tex);
            }
            if self.samples > 0 && self.msaa.as_ref().is_none_or(|m| m.width != width || m.height != height) {
                self.msaa = Some(MsaaTarget::new(gl, width, height, self.samples));
            }
            let target = self.msaa.as_ref().map_or(self.next_texture.fbo, |m| m.fbo);

            let saved_read_fbo = NonZeroU32::new(gl.get_parameter_i32(glow::READ_FRAMEBUFFER_BINDING) as u32).map(glow::NativeFramebuffer);
            let _saved_fbo = ScopedFrameBufferBinding::new(gl, Some(target));
            let mut saved_viewport = [0i32; 4];
            gl.get_parameter_i32_slice(glow::VIEWPORT, &mut saved_viewport);
            gl.viewport(0, 0, width as _, height as _);

            let [r, g, b] = style.scene;
            gl.clear_color(r, g, b, 1.0);
            gl.depth_mask(true);
            gl.clear(glow::COLOR_BUFFER_BIT | glow::DEPTH_BUFFER_BIT);
            // Like the design: the floor, the lines and then the markers on
            // top, in that order. Only obstacles hide what is behind them.
            gl.disable(glow::DEPTH_TEST);
            gl.enable(glow::BLEND);
            gl.blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);

            gl.use_program(Some(self.program));
            gl.uniform_1_f32(Some(&self.u_alpha), 1.0);
            gl.uniform_1_f32(Some(&self.u_point_size), 1.0);

            let _saved_vao = ScopedVAOBinding::new(gl, Some(self.vao));
            let _saved_vbo = ScopedVBOBinding::new(gl, Some(self.vbo));

            let aspect = width as f32 / height as f32;
            let mvp = build_mvp(camera.yaw, camera.pitch, camera.distance, camera.pan_x, camera.pan_y, aspect);
            gl.uniform_matrix_4_f32_slice(Some(&self.u_mvp), false, &mvp);

            let lines = |verts: &[f32], color: [f32; 3], line_width: f32, alpha: f32| {
                if verts.is_empty() {
                    return;
                }
                gl.uniform_3_f32(Some(&self.u_color), color[0], color[1], color[2]);
                gl.uniform_1_f32(Some(&self.u_alpha), alpha);
                gl.line_width(line_width * px);
                upload_and_draw(gl, self.vbo, verts, glow::LINES);
                gl.draw_arrays(glow::LINES, 0, verts.len() as i32 / 3);
            };

            // Floor grid: 0.5 m, every metre stronger
            if vis.grid {
                let half = scene.grid_half.ceil();
                let steps = (half * 2.0) as i32;
                let (mut minor, mut major) = (Vec::new(), Vec::new());
                for k in -steps..=steps {
                    let v = k as f32 * 0.5;
                    let out = if k % 2 == 0 { &mut major } else { &mut minor };
                    out.extend_from_slice(&[v, -half, 0.0, v, half, 0.0, -half, v, 0.0, half, v, 0.0]);
                }
                lines(&minor, style.grid, 1.0, 1.0);
                lines(&major, style.grid_major, 1.0, 1.0);
            }

            if vis.pads {
                let mut verts = Vec::new();
                for pad in scene.pads {
                    circle_segments(&mut verts, [pad.x, pad.y, 0.0], pad.radius, 32);
                }
                lines(&verts, style.pad, 1.25, 1.0);
            }

            if vis.areas {
                let mut verts = Vec::new();
                for fa in scene.flight_areas {
                    box_edges(&mut verts, fa.min, fa.max, Some((0.10, 0.07)));
                }
                lines(&verts, style.area, 1.5, 1.0);
                verts.clear();
                for wa in scene.waypoint_areas {
                    box_edges(&mut verts, wa.min, wa.max, Some((0.10, 0.07)));
                }
                lines(&verts, style.waypoint_area, 1.5, 1.0);
            }

            if vis.drop_lines {
                let mut verts = Vec::new();
                for fixed in scene.base_stations.iter().chain(scene.anchors) {
                    let [x, y, z] = fixed.pos;
                    dashed(&mut verts, [x, y, z], [x, y, 0.0], 0.04, 0.08);
                }
                lines(&verts, style.drop, 1.0, 1.0);
            }

            // Units off the floor: a line down and a small shadow under them
            if vis.units && vis.drop_lines {
                let airborne: Vec<&UnitPos> = scene.units.iter().filter(|u| u.z > 0.15).collect();
                let mut shadows = Vec::new();
                for u in &airborne {
                    disc_triangles(&mut shadows, [u.x, u.y, 0.0], 0.05, 12);
                }
                if !shadows.is_empty() {
                    gl.uniform_3_f32(Some(&self.u_color), style.drop[0], style.drop[1], style.drop[2]);
                    gl.uniform_1_f32(Some(&self.u_alpha), 0.35);
                    upload_and_draw(gl, self.vbo, &shadows, glow::TRIANGLES);
                    gl.draw_arrays(glow::TRIANGLES, 0, shadows.len() as i32 / 3);
                }
                let drops: Vec<f32> = airborne.iter().flat_map(|u| [u.x, u.y, u.z, u.x, u.y, 0.0]).collect();
                lines(&drops, style.drop, 1.0, 1.0);
            }

            if vis.axes {
                for (axis, color) in style.axes.iter().enumerate() {
                    let mut end = [0.0; 3];
                    end[axis] = 1.0;
                    lines(&[0.0, 0.0, 0.0, end[0], end[1], end[2]], *color, 2.0, 1.0);
                }
            }

            // Obstacles hide what is behind them, so they need the depth test
            if vis.obstacles && !scene.obstacle_triangles.is_empty() {
                gl.enable(glow::DEPTH_TEST);
                for (i, tris) in scene.obstacle_triangles.iter().enumerate() {
                    if !tris.is_empty() {
                        let c = scene.obstacle_colors.get(i).copied().unwrap_or([0.45, 0.45, 0.50]);
                        gl.uniform_3_f32(Some(&self.u_color), c[0], c[1], c[2]);
                        gl.uniform_1_f32(Some(&self.u_alpha), 0.75);
                        upload_and_draw(gl, self.vbo, tris, glow::TRIANGLES);
                        gl.draw_arrays(glow::TRIANGLES, 0, tris.len() as i32 / 3);
                    }
                }
                // LEQUAL so front coplanar edges show, edges behind stay hidden.
                gl.depth_func(glow::LEQUAL);
                for (i, wire) in scene.obstacle_wireframes.iter().enumerate() {
                    let c = scene.obstacle_colors.get(i).copied().unwrap_or([0.45, 0.45, 0.50]);
                    lines(wire, [(c[0] + 0.2).min(1.0), (c[1] + 0.2).min(1.0), (c[2] + 0.2).min(1.0)], 1.0, 1.0);
                }
                gl.depth_func(glow::LESS);
                gl.disable(glow::DEPTH_TEST);
            }

            if vis.trajectories && scene.trajectory_lines.len() >= 6 {
                let t = scene.trajectory_lines;
                gl.uniform_3_f32(Some(&self.u_color), style.accent[0], style.accent[1], style.accent[2]);
                gl.uniform_1_f32(Some(&self.u_alpha), 1.0);
                gl.line_width(2.0 * px);
                upload_and_draw(gl, self.vbo, t, glow::LINE_STRIP);
                gl.draw_arrays(glow::LINE_STRIP, 0, (t.len() / 3) as i32);
            }

            // Go-to targets: a dashed line from the unit, a cross at the target
            if vis.goto_targets {
                let mut verts = Vec::new();
                for seg in scene.goto_lines.chunks_exact(6) {
                    dashed(&mut verts, [seg[0], seg[1], seg[2]], [seg[3], seg[4], seg[5]], 0.06, 0.05);
                }
                for t in scene.goto_points {
                    let a = 0.12;
                    verts.extend_from_slice(&[
                        t[0] - a, t[1], t[2], t[0] + a, t[1], t[2],
                        t[0], t[1] - a, t[2], t[0], t[1] + a, t[2],
                        t[0], t[1], t[2] - a, t[0], t[1], t[2] + a,
                    ]);
                    if vis.drop_lines {
                        dashed(&mut verts, *t, [t[0], t[1], 0.0], 0.04, 0.08);
                    }
                }
                lines(&verts, style.accent, 1.5, 1.0);
            }

            // Markers, far ones first
            let no_ring = ([0.0; 3], 0.0);
            let mut markers: Vec<Marker> = Vec::new();
            let fixed = |p: &FixedPoint| Marker {
                pos: p.pos,
                size: Some(16.0 * px),
                fill: style.surface,
                inner: (style.base_station, 2.0 * px),
                rings: [no_ring; 3],
                square: true,
                alpha: if p.active { 1.0 } else { 0.5 },
            };
            markers.extend(scene.base_stations.iter().chain(scene.anchors).map(fixed));
            let mut units: Vec<Marker> = Vec::new();
            if vis.units {
                for u in scene.units {
                    let (fill, inner) = if u.hollow { (style.scene, (style.muted, 2.0 * px)) } else { (u.color, no_ring) };
                    let rings = if u.selected {
                        [(style.scene, 2.0 * px), (style.accent, 2.5 * px), (style.accent_tint, 4.5 * px)]
                    } else if u.hovered {
                        [(style.scene, 2.0 * px), (style.muted, 2.0 * px), no_ring]
                    } else {
                        [(style.scene, 2.0 * px), no_ring, no_ring]
                    };
                    units.push(Marker { pos: [u.x, u.y, u.z], size: None, fill, inner, rings, square: false, alpha: 1.0 });
                }
            }
            let depth = |m: &Marker| clip_w(m.pos, &mvp);
            markers.sort_by(|a, b| depth(b).total_cmp(&depth(a)));
            units.sort_by(|a, b| depth(b).total_cmp(&depth(a)));
            markers.extend(units);
            self.draw_markers(gl, &markers, &mvp, height as f32, px);

            gl.use_program(None);
            gl.line_width(1.0);
            gl.disable(glow::BLEND);

            // Resolve the samples into the texture
            if let Some(msaa) = &self.msaa {
                gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(msaa.fbo));
                gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(self.next_texture.fbo));
                gl.blit_framebuffer(0, 0, width as i32, height as i32, 0, 0, width as i32, height as i32, glow::COLOR_BUFFER_BIT, glow::NEAREST);
            }
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, saved_read_fbo);
            gl.viewport(saved_viewport[0], saved_viewport[1], saved_viewport[2], saved_viewport[3]);
        }

        let result = unsafe {
            slint::BorrowedOpenGLTextureBuilder::new_gl_2d_rgba_texture(
                self.next_texture.texture.0,
                (self.next_texture.width, self.next_texture.height).into(),
            )
            .build()
        };

        std::mem::swap(&mut self.next_texture, &mut self.displayed_texture);
        result
    }

    unsafe fn draw_markers(&self, gl: &glow::Context, markers: &[Marker], mvp: &[f32; 16], height: f32, px: f32) {
        let m = &self.marker;
        unsafe {
            gl.use_program(Some(m.program));
            gl.uniform_matrix_4_f32_slice(Some(&m.u_mvp), false, mvp);
            let focal = (height / 2.0) / (FOV_DEG.to_radians() / 2.0).tan();
            gl.uniform_1_f32(Some(&m.u_corner), 4.0 * px);
            gl.uniform_1_f32(Some(&m.u_dot), 2.0 * px);
            for marker in markers {
                let (k, min, max) = match marker.size {
                    Some(size) => (0.0, size, size),
                    None => (focal * UNIT_SIZE, UNIT_MIN_PX * px, UNIT_MAX_PX * px),
                };
                gl.uniform_1_f32(Some(&m.u_size_k), k);
                gl.uniform_1_f32(Some(&m.u_size_min), min);
                gl.uniform_1_f32(Some(&m.u_size_max), max);
                let pad: f32 = marker.rings.iter().map(|r| r.1).sum::<f32>() + 1.0;
                gl.uniform_1_f32(Some(&m.u_pad), pad);
                let [r, g, b] = marker.fill;
                gl.uniform_3_f32(Some(&m.u_fill), r, g, b);
                let ([r, g, b], w) = marker.inner;
                gl.uniform_3_f32(Some(&m.u_inner), r, g, b);
                gl.uniform_1_f32(Some(&m.u_inner_w), w);
                for ((u_color, u_width), ([r, g, b], w)) in m.u_rings.iter().zip(marker.rings) {
                    gl.uniform_3_f32(Some(u_color), r, g, b);
                    gl.uniform_1_f32(Some(u_width), w);
                }
                gl.uniform_1_f32(Some(&m.u_shape), if marker.square { 1.0 } else { 0.0 });
                gl.uniform_1_f32(Some(&m.u_alpha), marker.alpha);
                upload_and_draw(gl, self.vbo, &marker.pos, glow::POINTS);
                gl.draw_arrays(glow::POINTS, 0, 1);
            }
        }
    }

    /// The last frame `render` drew, as pixels: for screenshots.
    pub fn read_pixels(&self) -> slint::SharedPixelBuffer<slint::Rgba8Pixel> {
        let tex = &self.displayed_texture;
        let mut buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(tex.width, tex.height);
        unsafe {
            let gl = &self.gl;
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(tex.fbo));
            gl.read_pixels(
                0, 0, tex.width as i32, tex.height as i32, glow::RGBA, glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(buffer.make_mut_bytes())),
            );
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
        }
        buffer
    }

    /// Render the lighthouse coverage scene.
    ///
    /// `room` is (x, y, z) in metres.
    /// `base_stations` contains (pos, rotation_matrix) for each BS.
    /// `voxels` contains (x, y, z, coverage_count) for every voxel.
    /// `horiz_deg` / `vert_deg` are the full sweep angles.
    pub fn render_coverage(
        &mut self,
        width: u32,
        height: u32,
        yaw: f32,
        pitch: f32,
        distance: f32,
        pan_x: f32,
        pan_y: f32,
        room: [f32; 3],
        room_offset: [f32; 3],
        base_stations: &[([f32; 3], [[f32; 3]; 3])],
        voxels: &[(f32, f32, f32, u8)],
        horiz_deg: f32,
        vert_deg: f32,
        show_coverage: &[bool; 5],
        selected_bs: i32,
        active_handle: i32,
        trajectories: &[Vec<[f32; 3]>],
    ) -> slint::Image {
        let width = width.max(1);
        let height = height.max(1);

        unsafe {
            let gl = &self.gl;

            if self.next_texture.width != width || self.next_texture.height != height {
                let mut new_tex = RenderTexture::new(gl, width, height);
                std::mem::swap(&mut self.next_texture, &mut new_tex);
            }

            let _saved_fbo = ScopedFrameBufferBinding::new(gl, Some(self.next_texture.fbo));
            let mut saved_viewport = [0i32; 4];
            gl.get_parameter_i32_slice(glow::VIEWPORT, &mut saved_viewport);
            gl.viewport(0, 0, width as _, height as _);

            gl.clear_color(0.12, 0.12, 0.15, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT | glow::DEPTH_BUFFER_BIT);
            gl.enable(glow::DEPTH_TEST);

            gl.use_program(Some(self.program));
            gl.uniform_1_f32(Some(&self.u_alpha), 1.0);

            let _saved_vao = ScopedVAOBinding::new(gl, Some(self.vao));
            let _saved_vbo = ScopedVBOBinding::new(gl, Some(self.vbo));

            let aspect = width as f32 / height as f32;
            let mvp = build_mvp(yaw, pitch, distance, pan_x, pan_y, aspect);
            gl.uniform_matrix_4_f32_slice(Some(&self.u_mvp), false, &mvp);
            gl.uniform_1_f32(Some(&self.u_point_size), 1.0);

            // --- Ground grid ---
            let ox = room_offset[0];
            let oy = room_offset[1];
            let oz = room_offset[2];
            let mut grid_verts = Vec::new();
            let gx = room[0].ceil() as i32;
            let gy = room[1].ceil() as i32;
            for i in 0..=gx {
                let v = i as f32 + ox;
                grid_verts.extend_from_slice(&[v, oy, oz, v, room[1] + oy, oz]);
            }
            for i in 0..=gy {
                let v = i as f32 + oy;
                grid_verts.extend_from_slice(&[ox, v, oz, room[0] + ox, v, oz]);
            }
            gl.uniform_3_f32(Some(&self.u_color), 0.3, 0.3, 0.35);
            upload_and_draw(gl, self.vbo, &grid_verts, glow::LINES);
            gl.draw_arrays(glow::LINES, 0, grid_verts.len() as i32 / 3);

            // --- Room box outline ---
            let (x0, y0, z0) = (ox, oy, oz);
            let (x1, y1, z1) = (room[0] + ox, room[1] + oy, room[2] + oz);
            #[rustfmt::skip]
            let box_verts = [
                // bottom
                x0, y0, z0,  x1, y0, z0,
                x1, y0, z0,  x1, y1, z0,
                x1, y1, z0,  x0, y1, z0,
                x0, y1, z0,  x0, y0, z0,
                // top
                x0, y0, z1,  x1, y0, z1,
                x1, y0, z1,  x1, y1, z1,
                x1, y1, z1,  x0, y1, z1,
                x0, y1, z1,  x0, y0, z1,
                // verticals
                x0, y0, z0,  x0, y0, z1,
                x1, y0, z0,  x1, y0, z1,
                x1, y1, z0,  x1, y1, z1,
                x0, y1, z0,  x0, y1, z1,
            ];
            gl.uniform_3_f32(Some(&self.u_color), 0.5, 0.5, 0.55);
            upload_and_draw(gl, self.vbo, &box_verts, glow::LINES);
            gl.draw_arrays(glow::LINES, 0, box_verts.len() as i32 / 3);

            // --- Base station axes and FOV frustum ---
            let horiz_half = (horiz_deg / 2.0_f32).to_radians();
            let vert_half = (vert_deg / 2.0_f32).to_radians();
            let fov_len = 1.5_f32; // length of FOV frustum lines

            for (pos, rot) in base_stations {
                // Helper to transform local point to world
                let to_world = |lx: f32, ly: f32, lz: f32| -> [f32; 3] {
                    [
                        pos[0] + rot[0][0] * lx + rot[0][1] * ly + rot[0][2] * lz,
                        pos[1] + rot[1][0] * lx + rot[1][1] * ly + rot[1][2] * lz,
                        pos[2] + rot[2][0] * lx + rot[2][1] * ly + rot[2][2] * lz,
                    ]
                };

                // Draw BS local axes (0.5m each)
                let axis_len = 0.5;
                let origin = to_world(0.0, 0.0, 0.0);
                let x_end = to_world(axis_len, 0.0, 0.0);
                let y_end = to_world(0.0, axis_len, 0.0);
                let z_end = to_world(0.0, 0.0, axis_len);

                // X axis - red
                gl.uniform_3_f32(Some(&self.u_color), 0.94, 0.27, 0.27);
                let line = [origin[0], origin[1], origin[2], x_end[0], x_end[1], x_end[2]];
                upload_and_draw(gl, self.vbo, &line, glow::LINES);
                gl.draw_arrays(glow::LINES, 0, 2);

                // Y axis - green
                gl.uniform_3_f32(Some(&self.u_color), 0.29, 0.85, 0.50);
                let line = [origin[0], origin[1], origin[2], y_end[0], y_end[1], y_end[2]];
                upload_and_draw(gl, self.vbo, &line, glow::LINES);
                gl.draw_arrays(glow::LINES, 0, 2);

                // Z axis - blue
                gl.uniform_3_f32(Some(&self.u_color), 0.38, 0.65, 0.98);
                let line = [origin[0], origin[1], origin[2], z_end[0], z_end[1], z_end[2]];
                upload_and_draw(gl, self.vbo, &line, glow::LINES);
                gl.draw_arrays(glow::LINES, 0, 2);

                // FOV frustum: four corner rays
                let tan_h = horiz_half.tan();
                let tan_v = vert_half.tan();
                let corners = [
                    (1.0, tan_h, tan_v),
                    (1.0, -tan_h, tan_v),
                    (1.0, -tan_h, -tan_v),
                    (1.0, tan_h, -tan_v),
                ];

                gl.uniform_3_f32(Some(&self.u_color), 0.7, 0.4, 0.2);
                let mut frustum_verts = Vec::new();
                let mut far_points = Vec::new();
                for (cx, cy, cz) in &corners {
                    let len = (cx * cx + cy * cy + cz * cz).sqrt();
                    let scale = fov_len / len;
                    let far = to_world(cx * scale, cy * scale, cz * scale);
                    frustum_verts.extend_from_slice(&[origin[0], origin[1], origin[2], far[0], far[1], far[2]]);
                    far_points.push(far);
                }
                // Connect far rectangle
                for i in 0..4 {
                    let j = (i + 1) % 4;
                    frustum_verts.extend_from_slice(&[
                        far_points[i][0], far_points[i][1], far_points[i][2],
                        far_points[j][0], far_points[j][1], far_points[j][2],
                    ]);
                }
                upload_and_draw(gl, self.vbo, &frustum_verts, glow::LINES);
                gl.draw_arrays(glow::LINES, 0, frustum_verts.len() as i32 / 3);

                // BS point
                gl.uniform_3_f32(Some(&self.u_color), 0.94, 0.27, 0.27);
                gl.uniform_1_f32(Some(&self.u_point_size), 8.0);
                upload_and_draw(gl, self.vbo, &origin, glow::POINTS);
                gl.draw_arrays(glow::POINTS, 0, 1);
                gl.uniform_1_f32(Some(&self.u_point_size), 1.0);
            }

            // --- Coverage voxels ---
            // Bucket voxels by exact coverage count (0..4)
            let mut buckets: [Vec<f32>; 5] = Default::default();
            for &(x, y, z, count) in voxels {
                let idx = (count as usize).min(4);
                buckets[idx].push(x);
                buckets[idx].push(y);
                buckets[idx].push(z);
            }

            // Only show level-0 when there is actual coverage data
            let has_coverage = (1..5).any(|i| !buckets[i].is_empty());

            // --- Draw voxels as transparent cloud ---
            gl.depth_mask(false);
            gl.enable(glow::BLEND);
            gl.blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);
            gl.uniform_1_f32(Some(&self.u_point_size), 12.0);

            // Level 0: red, level 1: yellow, levels 2-4: green
            let colors: [[f32; 3]; 5] = [
                [0.9, 0.15, 0.15],
                [0.95, 0.8, 0.1],
                [0.2, 0.9, 0.35],
                [0.2, 0.9, 0.35],
                [0.2, 0.9, 0.35],
            ];
            let alphas: [f32; 5] = [0.06, 0.08, 0.08, 0.08, 0.08];

            for (i, bucket) in buckets.iter().enumerate() {
                if bucket.is_empty() || !show_coverage[i] {
                    continue;
                }
                // Skip uncovered when there's no coverage data at all
                if i == 0 && !has_coverage {
                    continue;
                }
                gl.uniform_3_f32(
                    Some(&self.u_color),
                    colors[i][0], colors[i][1], colors[i][2],
                );
                gl.uniform_1_f32(Some(&self.u_alpha), alphas[i]);
                upload_and_draw(gl, self.vbo, bucket, glow::POINTS);
                gl.draw_arrays(glow::POINTS, 0, bucket.len() as i32 / 3);
            }

            // Restore state
            gl.uniform_1_f32(Some(&self.u_alpha), 1.0);
            gl.disable(glow::BLEND);
            gl.depth_mask(true);

            // --- Trajectories ---
            if !trajectories.is_empty() {
                gl.uniform_1_f32(Some(&self.u_point_size), 1.0);
                for (i, traj) in trajectories.iter().enumerate() {
                    if traj.len() < 2 {
                        continue;
                    }
                    // Assign a distinct colour per CF using a hue rotation
                    let hue = (i as f32 * 0.618033988) % 1.0; // golden ratio
                    let (r, g, b) = hue_to_rgb(hue);
                    gl.uniform_3_f32(Some(&self.u_color), r, g, b);

                    let verts: Vec<f32> = traj.iter().flat_map(|p| p.iter().copied()).collect();
                    upload_and_draw(gl, self.vbo, &verts, glow::LINE_STRIP);
                    gl.draw_arrays(glow::LINE_STRIP, 0, traj.len() as i32);
                }

                // Draw start positions as larger dots
                gl.uniform_1_f32(Some(&self.u_point_size), 6.0);
                for (i, traj) in trajectories.iter().enumerate() {
                    if let Some(start) = traj.first() {
                        let hue = (i as f32 * 0.618033988) % 1.0;
                        let (r, g, b) = hue_to_rgb(hue);
                        gl.uniform_3_f32(Some(&self.u_color), r, g, b);
                        upload_and_draw(gl, self.vbo, start, glow::POINTS);
                        gl.draw_arrays(glow::POINTS, 0, 1);
                    }
                }
                gl.uniform_1_f32(Some(&self.u_point_size), 1.0);
            }

            // --- Gizmo for selected base station ---
            if selected_bs >= 0 && (selected_bs as usize) < base_stations.len() {
                gl.disable(glow::DEPTH_TEST); // gizmo always on top

                let (pos, rot) = &base_stations[selected_bs as usize];

                // Derive azimuth/elevation from rotation matrix
                let azimuth = rot[1][0].atan2(rot[0][0]);
                let elevation = (-rot[2][0]).asin();

                // -- Translation handles (world-axis arrows) --
                let axes: [([f32; 3], [f32; 3], i32); 3] = [
                    ([GIZMO_TRANSLATE_LEN, 0.0, 0.0], [0.94, 0.27, 0.27], HANDLE_TRANSLATE_X),
                    ([0.0, GIZMO_TRANSLATE_LEN, 0.0], [0.29, 0.85, 0.50], HANDLE_TRANSLATE_Y),
                    ([0.0, 0.0, GIZMO_TRANSLATE_LEN], [0.38, 0.65, 0.98], HANDLE_TRANSLATE_Z),
                ];

                for (dir, color, handle_id) in &axes {
                    let active = *handle_id == active_handle;
                    let bright = if active { 1.0 } else { 0.7 };
                    gl.uniform_3_f32(
                        Some(&self.u_color),
                        (color[0] * bright).min(1.0),
                        (color[1] * bright).min(1.0),
                        (color[2] * bright).min(1.0),
                    );

                    let end = [pos[0] + dir[0], pos[1] + dir[1], pos[2] + dir[2]];
                    let line = [pos[0], pos[1], pos[2], end[0], end[1], end[2]];
                    upload_and_draw(gl, self.vbo, &line, glow::LINES);
                    gl.draw_arrays(glow::LINES, 0, 2);

                    // Arrow tip point
                    gl.uniform_1_f32(
                        Some(&self.u_point_size),
                        if active { 14.0 } else { 10.0 },
                    );
                    upload_and_draw(gl, self.vbo, &end, glow::POINTS);
                    gl.draw_arrays(glow::POINTS, 0, 1);
                }

                // -- Azimuth rotation arc (XY plane at BS height) --
                let az_active = active_handle == HANDLE_ROTATE_AZ;
                let az_color = if az_active {
                    [1.0, 0.5, 1.0]
                } else {
                    [0.8, 0.2, 0.8]
                };
                gl.uniform_3_f32(
                    Some(&self.u_color),
                    az_color[0],
                    az_color[1],
                    az_color[2],
                );
                let mut arc_verts = Vec::with_capacity((GIZMO_ARC_STEPS + 1) * 3);
                for step in 0..=GIZMO_ARC_STEPS {
                    let a = azimuth - GIZMO_ARC_SPAN
                        + 2.0 * GIZMO_ARC_SPAN * step as f32 / GIZMO_ARC_STEPS as f32;
                    arc_verts.extend_from_slice(&[
                        pos[0] + GIZMO_ROTATE_RADIUS * a.cos(),
                        pos[1] + GIZMO_ROTATE_RADIUS * a.sin(),
                        pos[2],
                    ]);
                }
                upload_and_draw(gl, self.vbo, &arc_verts, glow::LINE_STRIP);
                gl.draw_arrays(glow::LINE_STRIP, 0, arc_verts.len() as i32 / 3);

                // Arc endpoint markers
                gl.uniform_1_f32(
                    Some(&self.u_point_size),
                    if az_active { 10.0 } else { 7.0 },
                );
                upload_and_draw(gl, self.vbo, &arc_verts[..3], glow::POINTS);
                gl.draw_arrays(glow::POINTS, 0, 1);
                let last = arc_verts.len() - 3;
                upload_and_draw(gl, self.vbo, &arc_verts[last..], glow::POINTS);
                gl.draw_arrays(glow::POINTS, 0, 1);

                // -- Elevation rotation arc (vertical plane of current azimuth) --
                let el_active = active_handle == HANDLE_ROTATE_EL;
                let el_color = if el_active {
                    [1.0, 0.8, 0.3]
                } else {
                    [0.9, 0.6, 0.1]
                };
                gl.uniform_3_f32(
                    Some(&self.u_color),
                    el_color[0],
                    el_color[1],
                    el_color[2],
                );
                let mut arc_verts = Vec::with_capacity((GIZMO_ARC_STEPS + 1) * 3);
                for step in 0..=GIZMO_ARC_STEPS {
                    let e = elevation - GIZMO_ARC_SPAN
                        + 2.0 * GIZMO_ARC_SPAN * step as f32 / GIZMO_ARC_STEPS as f32;
                    arc_verts.extend_from_slice(&[
                        pos[0] + GIZMO_ROTATE_RADIUS * e.cos() * azimuth.cos(),
                        pos[1] + GIZMO_ROTATE_RADIUS * e.cos() * azimuth.sin(),
                        pos[2] - GIZMO_ROTATE_RADIUS * e.sin(),
                    ]);
                }
                upload_and_draw(gl, self.vbo, &arc_verts, glow::LINE_STRIP);
                gl.draw_arrays(glow::LINE_STRIP, 0, arc_verts.len() as i32 / 3);

                gl.uniform_1_f32(
                    Some(&self.u_point_size),
                    if el_active { 10.0 } else { 7.0 },
                );
                upload_and_draw(gl, self.vbo, &arc_verts[..3], glow::POINTS);
                gl.draw_arrays(glow::POINTS, 0, 1);
                let last = arc_verts.len() - 3;
                upload_and_draw(gl, self.vbo, &arc_verts[last..], glow::POINTS);
                gl.draw_arrays(glow::POINTS, 0, 1);

                // White line showing current look direction
                let look_len = 0.8_f32;
                let look_end = [
                    pos[0] + rot[0][0] * look_len,
                    pos[1] + rot[1][0] * look_len,
                    pos[2] + rot[2][0] * look_len,
                ];
                gl.uniform_3_f32(Some(&self.u_color), 1.0, 1.0, 1.0);
                let line = [pos[0], pos[1], pos[2], look_end[0], look_end[1], look_end[2]];
                upload_and_draw(gl, self.vbo, &line, glow::LINES);
                gl.draw_arrays(glow::LINES, 0, 2);

                gl.enable(glow::DEPTH_TEST);
            }

            gl.use_program(None);
            gl.disable(glow::DEPTH_TEST);
            gl.viewport(saved_viewport[0], saved_viewport[1], saved_viewport[2], saved_viewport[3]);
        }

        let result = unsafe {
            slint::BorrowedOpenGLTextureBuilder::new_gl_2d_rgba_texture(
                self.next_texture.texture.0,
                (self.next_texture.width, self.next_texture.height).into(),
            )
            .build()
        };

        std::mem::swap(&mut self.next_texture, &mut self.displayed_texture);
        result
    }

    /// Render a TDoA3 GDOP visualization.
    ///
    /// Voxels carry a continuous GDOP value (f32) instead of a discrete count.
    /// The colour is mapped green (low GDOP / good) → yellow → red (high GDOP / bad).
    pub fn render_tdoa3(
        &mut self,
        width: u32,
        height: u32,
        yaw: f32,
        pitch: f32,
        distance: f32,
        pan_x: f32,
        pan_y: f32,
        room: [f32; 3],
        room_offset: [f32; 3],
        anchors: &[[f32; 3]],
        voxels: &[(f32, f32, f32, f32)], // x, y, z, value
        min_scale: f32,
        max_scale: f32,
        show_uncovered: bool,
        invert_gradient: bool,
        selected_anchor: i32,
        active_handle: i32,
    ) -> slint::Image {
        let width = width.max(1);
        let height = height.max(1);

        unsafe {
            let gl = &self.gl;

            if self.next_texture.width != width || self.next_texture.height != height {
                let mut new_tex = RenderTexture::new(gl, width, height);
                std::mem::swap(&mut self.next_texture, &mut new_tex);
            }

            let _saved_fbo = ScopedFrameBufferBinding::new(gl, Some(self.next_texture.fbo));
            let mut saved_viewport = [0i32; 4];
            gl.get_parameter_i32_slice(glow::VIEWPORT, &mut saved_viewport);
            gl.viewport(0, 0, width as _, height as _);

            gl.clear_color(0.12, 0.12, 0.15, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT | glow::DEPTH_BUFFER_BIT);
            gl.enable(glow::DEPTH_TEST);

            gl.use_program(Some(self.program));
            gl.uniform_1_f32(Some(&self.u_alpha), 1.0);

            let _saved_vao = ScopedVAOBinding::new(gl, Some(self.vao));
            let _saved_vbo = ScopedVBOBinding::new(gl, Some(self.vbo));

            let aspect = width as f32 / height as f32;
            let mvp = build_mvp(yaw, pitch, distance, pan_x, pan_y, aspect);
            gl.uniform_matrix_4_f32_slice(Some(&self.u_mvp), false, &mvp);
            gl.uniform_1_f32(Some(&self.u_point_size), 1.0);

            // --- Ground grid ---
            let ox = room_offset[0];
            let oy = room_offset[1];
            let oz = room_offset[2];
            let mut grid_verts = Vec::new();
            let gx = room[0].ceil() as i32;
            let gy = room[1].ceil() as i32;
            for i in 0..=gx {
                let v = i as f32 + ox;
                grid_verts.extend_from_slice(&[v, oy, oz, v, room[1] + oy, oz]);
            }
            for i in 0..=gy {
                let v = i as f32 + oy;
                grid_verts.extend_from_slice(&[ox, v, oz, room[0] + ox, v, oz]);
            }
            gl.uniform_3_f32(Some(&self.u_color), 0.3, 0.3, 0.35);
            upload_and_draw(gl, self.vbo, &grid_verts, glow::LINES);
            gl.draw_arrays(glow::LINES, 0, grid_verts.len() as i32 / 3);

            // --- Room box outline ---
            let (x0, y0, z0) = (ox, oy, oz);
            let (x1, y1, z1) = (room[0] + ox, room[1] + oy, room[2] + oz);
            #[rustfmt::skip]
            let box_verts = [
                x0, y0, z0,  x1, y0, z0,
                x1, y0, z0,  x1, y1, z0,
                x1, y1, z0,  x0, y1, z0,
                x0, y1, z0,  x0, y0, z0,
                x0, y0, z1,  x1, y0, z1,
                x1, y0, z1,  x1, y1, z1,
                x1, y1, z1,  x0, y1, z1,
                x0, y1, z1,  x0, y0, z1,
                x0, y0, z0,  x0, y0, z1,
                x1, y0, z0,  x1, y0, z1,
                x1, y1, z0,  x1, y1, z1,
                x0, y1, z0,  x0, y1, z1,
            ];
            gl.uniform_3_f32(Some(&self.u_color), 0.5, 0.5, 0.55);
            upload_and_draw(gl, self.vbo, &box_verts, glow::LINES);
            gl.draw_arrays(glow::LINES, 0, box_verts.len() as i32 / 3);

            // --- Anchors ---
            for pos in anchors {
                gl.uniform_3_f32(Some(&self.u_color), 1.0, 0.85, 0.0);
                gl.uniform_1_f32(Some(&self.u_point_size), 8.0);
                upload_and_draw(gl, self.vbo, pos, glow::POINTS);
                gl.draw_arrays(glow::POINTS, 0, 1);
            }
            gl.uniform_1_f32(Some(&self.u_point_size), 1.0);

            // --- GDOP voxels as gradient-coloured cloud ---
            // Bucket voxels into N_GDOP_BUCKETS levels for the uniform-colour shader,
            // plus one bucket for uncovered (GDOP = infinity).
            const N_GDOP_BUCKETS: usize = 16;
            let mut buckets: Vec<Vec<f32>> = vec![Vec::new(); N_GDOP_BUCKETS];
            let mut uncovered: Vec<f32> = Vec::new();

            let range = (max_scale - min_scale).max(0.001);
            for &(x, y, z, gdop) in voxels {
                if !gdop.is_finite() {
                    uncovered.push(x);
                    uncovered.push(y);
                    uncovered.push(z);
                } else if gdop < min_scale || gdop > max_scale {
                    // Outside the visible range — skip
                } else {
                    let t = ((gdop - min_scale) / range).clamp(0.0, 1.0);
                    let idx = ((t * N_GDOP_BUCKETS as f32) as usize).min(N_GDOP_BUCKETS - 1);
                    buckets[idx].push(x);
                    buckets[idx].push(y);
                    buckets[idx].push(z);
                }
            }

            let has_data = buckets.iter().any(|b| !b.is_empty());

            gl.depth_mask(false);
            gl.enable(glow::BLEND);
            gl.blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);
            gl.uniform_1_f32(Some(&self.u_point_size), 12.0);

            // Draw uncovered voxels (dark red)
            if show_uncovered && has_data && !uncovered.is_empty() {
                gl.uniform_3_f32(Some(&self.u_color), 0.5, 0.1, 0.1);
                gl.uniform_1_f32(Some(&self.u_alpha), 0.06);
                upload_and_draw(gl, self.vbo, &uncovered, glow::POINTS);
                gl.draw_arrays(glow::POINTS, 0, uncovered.len() as i32 / 3);
            }

            // Draw metric buckets: green (good) → yellow → red (bad)
            for (i, bucket) in buckets.iter().enumerate() {
                if bucket.is_empty() {
                    continue;
                }
                let mut t = (i as f32 + 0.5) / N_GDOP_BUCKETS as f32;
                if invert_gradient {
                    t = 1.0 - t;
                }
                let (r, g, b) = gdop_color(t);
                gl.uniform_3_f32(Some(&self.u_color), r, g, b);
                gl.uniform_1_f32(Some(&self.u_alpha), 0.08);
                upload_and_draw(gl, self.vbo, bucket, glow::POINTS);
                gl.draw_arrays(glow::POINTS, 0, bucket.len() as i32 / 3);
            }

            gl.uniform_1_f32(Some(&self.u_alpha), 1.0);
            gl.disable(glow::BLEND);
            gl.depth_mask(true);

            // --- Translation gizmo for selected anchor ---
            if selected_anchor >= 0 && (selected_anchor as usize) < anchors.len() {
                gl.disable(glow::DEPTH_TEST);

                let pos = &anchors[selected_anchor as usize];

                let axes: [([f32; 3], [f32; 3], i32); 3] = [
                    ([GIZMO_TRANSLATE_LEN, 0.0, 0.0], [0.94, 0.27, 0.27], HANDLE_TRANSLATE_X),
                    ([0.0, GIZMO_TRANSLATE_LEN, 0.0], [0.29, 0.85, 0.50], HANDLE_TRANSLATE_Y),
                    ([0.0, 0.0, GIZMO_TRANSLATE_LEN], [0.38, 0.65, 0.98], HANDLE_TRANSLATE_Z),
                ];

                for (dir, color, handle_id) in &axes {
                    let active = *handle_id == active_handle;
                    let bright = if active { 1.0 } else { 0.7 };
                    gl.uniform_3_f32(
                        Some(&self.u_color),
                        (color[0] * bright).min(1.0),
                        (color[1] * bright).min(1.0),
                        (color[2] * bright).min(1.0),
                    );

                    let end = [pos[0] + dir[0], pos[1] + dir[1], pos[2] + dir[2]];
                    let line = [pos[0], pos[1], pos[2], end[0], end[1], end[2]];
                    upload_and_draw(gl, self.vbo, &line, glow::LINES);
                    gl.draw_arrays(glow::LINES, 0, 2);

                    gl.uniform_1_f32(
                        Some(&self.u_point_size),
                        if active { 14.0 } else { 10.0 },
                    );
                    upload_and_draw(gl, self.vbo, &end, glow::POINTS);
                    gl.draw_arrays(glow::POINTS, 0, 1);
                }

                gl.enable(glow::DEPTH_TEST);
            }

            gl.use_program(None);
            gl.disable(glow::DEPTH_TEST);
            gl.viewport(saved_viewport[0], saved_viewport[1], saved_viewport[2], saved_viewport[3]);
        }

        let result = unsafe {
            slint::BorrowedOpenGLTextureBuilder::new_gl_2d_rgba_texture(
                self.next_texture.texture.0,
                (self.next_texture.width, self.next_texture.height).into(),
            )
            .build()
        };

        std::mem::swap(&mut self.next_texture, &mut self.displayed_texture);
        result
    }

    /// Render a combined view showing both LH coverage and TDoA3 GDOP in the same scene.
    #[allow(dead_code)]
    #[allow(clippy::too_many_arguments)]
    pub fn render_combined(
        &mut self,
        width: u32,
        height: u32,
        yaw: f32,
        pitch: f32,
        distance: f32,
        pan_x: f32,
        pan_y: f32,
        room: [f32; 3],
        room_offset: [f32; 3],
        // LH data
        base_stations: &[([f32; 3], [[f32; 3]; 3])],
        lh_voxels: &[(f32, f32, f32, u8)],
        horiz_deg: f32,
        vert_deg: f32,
        show_lh_coverage: &[bool; 5],
        show_lh_voxels: bool,
        // TDoA3 data
        anchors: &[[f32; 3]],
        tdoa3_voxels: &[(f32, f32, f32, f32)],
        min_scale: f32,
        max_scale: f32,
        show_tdoa3_voxels: bool,
        show_uncovered: bool,
    ) -> slint::Image {
        let width = width.max(1);
        let height = height.max(1);

        unsafe {
            let gl = &self.gl;

            if self.next_texture.width != width || self.next_texture.height != height {
                let mut new_tex = RenderTexture::new(gl, width, height);
                std::mem::swap(&mut self.next_texture, &mut new_tex);
            }

            let _saved_fbo = ScopedFrameBufferBinding::new(gl, Some(self.next_texture.fbo));
            let mut saved_viewport = [0i32; 4];
            gl.get_parameter_i32_slice(glow::VIEWPORT, &mut saved_viewport);
            gl.viewport(0, 0, width as _, height as _);

            gl.clear_color(0.12, 0.12, 0.15, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT | glow::DEPTH_BUFFER_BIT);
            gl.enable(glow::DEPTH_TEST);

            gl.use_program(Some(self.program));
            gl.uniform_1_f32(Some(&self.u_alpha), 1.0);

            let _saved_vao = ScopedVAOBinding::new(gl, Some(self.vao));
            let _saved_vbo = ScopedVBOBinding::new(gl, Some(self.vbo));

            let aspect = width as f32 / height as f32;
            let mvp = build_mvp(yaw, pitch, distance, pan_x, pan_y, aspect);
            gl.uniform_matrix_4_f32_slice(Some(&self.u_mvp), false, &mvp);
            gl.uniform_1_f32(Some(&self.u_point_size), 1.0);

            // --- Ground grid ---
            let ox = room_offset[0];
            let oy = room_offset[1];
            let oz = room_offset[2];
            let mut grid_verts = Vec::new();
            let gx = room[0].ceil() as i32;
            let gy = room[1].ceil() as i32;
            for i in 0..=gx {
                let v = i as f32 + ox;
                grid_verts.extend_from_slice(&[v, oy, oz, v, room[1] + oy, oz]);
            }
            for i in 0..=gy {
                let v = i as f32 + oy;
                grid_verts.extend_from_slice(&[ox, v, oz, room[0] + ox, v, oz]);
            }
            gl.uniform_3_f32(Some(&self.u_color), 0.3, 0.3, 0.35);
            upload_and_draw(gl, self.vbo, &grid_verts, glow::LINES);
            gl.draw_arrays(glow::LINES, 0, grid_verts.len() as i32 / 3);

            // --- Room box outline ---
            let (x0, y0, z0) = (ox, oy, oz);
            let (x1, y1, z1) = (room[0] + ox, room[1] + oy, room[2] + oz);
            #[rustfmt::skip]
            let box_verts = [
                x0, y0, z0,  x1, y0, z0,
                x1, y0, z0,  x1, y1, z0,
                x1, y1, z0,  x0, y1, z0,
                x0, y1, z0,  x0, y0, z0,
                x0, y0, z1,  x1, y0, z1,
                x1, y0, z1,  x1, y1, z1,
                x1, y1, z1,  x0, y1, z1,
                x0, y1, z1,  x0, y0, z1,
                x0, y0, z0,  x0, y0, z1,
                x1, y0, z0,  x1, y0, z1,
                x1, y1, z0,  x1, y1, z1,
                x0, y1, z0,  x0, y1, z1,
            ];
            gl.uniform_3_f32(Some(&self.u_color), 0.5, 0.5, 0.55);
            upload_and_draw(gl, self.vbo, &box_verts, glow::LINES);
            gl.draw_arrays(glow::LINES, 0, box_verts.len() as i32 / 3);

            // --- Base stations (axes + FOV frustum) ---
            let horiz_half = (horiz_deg / 2.0_f32).to_radians();
            let vert_half = (vert_deg / 2.0_f32).to_radians();
            let fov_len = 1.5_f32;

            for (pos, rot) in base_stations {
                let to_world = |lx: f32, ly: f32, lz: f32| -> [f32; 3] {
                    [
                        pos[0] + rot[0][0] * lx + rot[0][1] * ly + rot[0][2] * lz,
                        pos[1] + rot[1][0] * lx + rot[1][1] * ly + rot[1][2] * lz,
                        pos[2] + rot[2][0] * lx + rot[2][1] * ly + rot[2][2] * lz,
                    ]
                };

                let axis_len = 0.5;
                let origin = to_world(0.0, 0.0, 0.0);
                let x_end = to_world(axis_len, 0.0, 0.0);
                let y_end = to_world(0.0, axis_len, 0.0);
                let z_end = to_world(0.0, 0.0, axis_len);

                gl.uniform_3_f32(Some(&self.u_color), 0.94, 0.27, 0.27);
                let line = [origin[0], origin[1], origin[2], x_end[0], x_end[1], x_end[2]];
                upload_and_draw(gl, self.vbo, &line, glow::LINES);
                gl.draw_arrays(glow::LINES, 0, 2);

                gl.uniform_3_f32(Some(&self.u_color), 0.29, 0.85, 0.50);
                let line = [origin[0], origin[1], origin[2], y_end[0], y_end[1], y_end[2]];
                upload_and_draw(gl, self.vbo, &line, glow::LINES);
                gl.draw_arrays(glow::LINES, 0, 2);

                gl.uniform_3_f32(Some(&self.u_color), 0.38, 0.65, 0.98);
                let line = [origin[0], origin[1], origin[2], z_end[0], z_end[1], z_end[2]];
                upload_and_draw(gl, self.vbo, &line, glow::LINES);
                gl.draw_arrays(glow::LINES, 0, 2);

                // FOV frustum
                let tan_h = horiz_half.tan();
                let tan_v = vert_half.tan();
                let corners = [
                    (1.0, tan_h, tan_v),
                    (1.0, -tan_h, tan_v),
                    (1.0, -tan_h, -tan_v),
                    (1.0, tan_h, -tan_v),
                ];

                gl.uniform_3_f32(Some(&self.u_color), 0.7, 0.4, 0.2);
                let mut frustum_verts = Vec::new();
                let mut far_points = Vec::new();
                for (cx, cy, cz) in &corners {
                    let len = (cx * cx + cy * cy + cz * cz).sqrt();
                    let scale = fov_len / len;
                    let far = to_world(cx * scale, cy * scale, cz * scale);
                    frustum_verts.extend_from_slice(&[origin[0], origin[1], origin[2], far[0], far[1], far[2]]);
                    far_points.push(far);
                }
                for i in 0..4 {
                    let j = (i + 1) % 4;
                    frustum_verts.extend_from_slice(&[
                        far_points[i][0], far_points[i][1], far_points[i][2],
                        far_points[j][0], far_points[j][1], far_points[j][2],
                    ]);
                }
                upload_and_draw(gl, self.vbo, &frustum_verts, glow::LINES);
                gl.draw_arrays(glow::LINES, 0, frustum_verts.len() as i32 / 3);

                // BS point
                gl.uniform_3_f32(Some(&self.u_color), 0.94, 0.27, 0.27);
                gl.uniform_1_f32(Some(&self.u_point_size), 8.0);
                upload_and_draw(gl, self.vbo, &origin, glow::POINTS);
                gl.draw_arrays(glow::POINTS, 0, 1);
                gl.uniform_1_f32(Some(&self.u_point_size), 1.0);
            }

            // --- TDoA3 Anchors ---
            for pos in anchors {
                gl.uniform_3_f32(Some(&self.u_color), 1.0, 0.85, 0.0);
                gl.uniform_1_f32(Some(&self.u_point_size), 8.0);
                upload_and_draw(gl, self.vbo, pos, glow::POINTS);
                gl.draw_arrays(glow::POINTS, 0, 1);
            }
            gl.uniform_1_f32(Some(&self.u_point_size), 1.0);

            // --- Voxel clouds (transparent) ---
            gl.depth_mask(false);
            gl.enable(glow::BLEND);
            gl.blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);
            gl.uniform_1_f32(Some(&self.u_point_size), 12.0);

            // LH coverage voxels
            if show_lh_voxels && !lh_voxels.is_empty() {
                let mut buckets: [Vec<f32>; 5] = Default::default();
                for &(x, y, z, count) in lh_voxels {
                    let idx = (count as usize).min(4);
                    buckets[idx].push(x);
                    buckets[idx].push(y);
                    buckets[idx].push(z);
                }

                let has_coverage = (1..5).any(|i| !buckets[i].is_empty());

                let colors: [[f32; 3]; 5] = [
                    [0.9, 0.15, 0.15],
                    [0.95, 0.8, 0.1],
                    [0.2, 0.9, 0.35],
                    [0.2, 0.9, 0.35],
                    [0.2, 0.9, 0.35],
                ];
                let alphas: [f32; 5] = [0.06, 0.08, 0.08, 0.08, 0.08];

                for (i, bucket) in buckets.iter().enumerate() {
                    if bucket.is_empty() || !show_lh_coverage[i] {
                        continue;
                    }
                    if i == 0 && !has_coverage {
                        continue;
                    }
                    gl.uniform_3_f32(Some(&self.u_color), colors[i][0], colors[i][1], colors[i][2]);
                    gl.uniform_1_f32(Some(&self.u_alpha), alphas[i]);
                    upload_and_draw(gl, self.vbo, bucket, glow::POINTS);
                    gl.draw_arrays(glow::POINTS, 0, bucket.len() as i32 / 3);
                }
            }

            // TDoA3 GDOP voxels
            if show_tdoa3_voxels && !tdoa3_voxels.is_empty() {
                const N_GDOP_BUCKETS: usize = 16;
                let mut buckets: Vec<Vec<f32>> = vec![Vec::new(); N_GDOP_BUCKETS];
                let mut uncovered: Vec<f32> = Vec::new();

                let range = (max_scale - min_scale).max(0.001);
                for &(x, y, z, gdop) in tdoa3_voxels {
                    if !gdop.is_finite() {
                        uncovered.push(x);
                        uncovered.push(y);
                        uncovered.push(z);
                    } else if gdop >= min_scale && gdop <= max_scale {
                        let t = ((gdop - min_scale) / range).clamp(0.0, 1.0);
                        let idx = ((t * N_GDOP_BUCKETS as f32) as usize).min(N_GDOP_BUCKETS - 1);
                        buckets[idx].push(x);
                        buckets[idx].push(y);
                        buckets[idx].push(z);
                    }
                }

                let has_data = buckets.iter().any(|b| !b.is_empty());

                if show_uncovered && has_data && !uncovered.is_empty() {
                    gl.uniform_3_f32(Some(&self.u_color), 0.5, 0.1, 0.1);
                    gl.uniform_1_f32(Some(&self.u_alpha), 0.06);
                    upload_and_draw(gl, self.vbo, &uncovered, glow::POINTS);
                    gl.draw_arrays(glow::POINTS, 0, uncovered.len() as i32 / 3);
                }

                for (i, bucket) in buckets.iter().enumerate() {
                    if bucket.is_empty() {
                        continue;
                    }
                    let t = (i as f32 + 0.5) / N_GDOP_BUCKETS as f32;
                    let (r, g, b) = gdop_color(t);
                    gl.uniform_3_f32(Some(&self.u_color), r, g, b);
                    gl.uniform_1_f32(Some(&self.u_alpha), 0.08);
                    upload_and_draw(gl, self.vbo, bucket, glow::POINTS);
                    gl.draw_arrays(glow::POINTS, 0, bucket.len() as i32 / 3);
                }
            }

            gl.uniform_1_f32(Some(&self.u_alpha), 1.0);
            gl.disable(glow::BLEND);
            gl.depth_mask(true);

            gl.use_program(None);
            gl.disable(glow::DEPTH_TEST);
            gl.viewport(saved_viewport[0], saved_viewport[1], saved_viewport[2], saved_viewport[3]);
        }

        let result = unsafe {
            slint::BorrowedOpenGLTextureBuilder::new_gl_2d_rgba_texture(
                self.next_texture.texture.0,
                (self.next_texture.width, self.next_texture.height).into(),
            )
            .build()
        };

        std::mem::swap(&mut self.next_texture, &mut self.displayed_texture);
        result
    }

    /// Draw the Planning view at `width` × `height` physical pixels.
    pub fn render_plan(&mut self, width: u32, height: u32, camera: Camera, scene: &PlanScene, style: &SceneStyle) -> slint::Image {
        let width = width.max(1);
        let height = height.max(1);
        let px = style.scale;

        unsafe {
            let gl = self.gl.clone();
            let gl = &gl;

            if self.next_texture.width != width || self.next_texture.height != height {
                let mut new_tex = RenderTexture::new(gl, width, height);
                std::mem::swap(&mut self.next_texture, &mut new_tex);
            }
            if self.samples > 0 && self.msaa.as_ref().is_none_or(|m| m.width != width || m.height != height) {
                self.msaa = Some(MsaaTarget::new(gl, width, height, self.samples));
            }
            let target = self.msaa.as_ref().map_or(self.next_texture.fbo, |m| m.fbo);

            let saved_read_fbo = NonZeroU32::new(gl.get_parameter_i32(glow::READ_FRAMEBUFFER_BINDING) as u32).map(glow::NativeFramebuffer);
            let _saved_fbo = ScopedFrameBufferBinding::new(gl, Some(target));
            let mut saved_viewport = [0i32; 4];
            gl.get_parameter_i32_slice(glow::VIEWPORT, &mut saved_viewport);
            gl.viewport(0, 0, width as _, height as _);

            let [r, g, b] = style.scene;
            gl.clear_color(r, g, b, 1.0);
            gl.depth_mask(true);
            gl.clear(glow::COLOR_BUFFER_BIT | glow::DEPTH_BUFFER_BIT);
            gl.disable(glow::DEPTH_TEST);
            gl.enable(glow::BLEND);
            gl.blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA);

            gl.use_program(Some(self.program));
            gl.uniform_1_f32(Some(&self.u_point_size), 1.0);
            let _saved_vao = ScopedVAOBinding::new(gl, Some(self.vao));
            let _saved_vbo = ScopedVBOBinding::new(gl, Some(self.vbo));

            let aspect = width as f32 / height as f32;
            let mvp = build_mvp(camera.yaw, camera.pitch, camera.distance, camera.pan_x, camera.pan_y, aspect);
            gl.uniform_matrix_4_f32_slice(Some(&self.u_mvp), false, &mvp);

            let draw = |verts: &[f32], mode: u32, color: [f32; 3], line_width: f32, alpha: f32| {
                if verts.is_empty() {
                    return;
                }
                gl.uniform_3_f32(Some(&self.u_color), color[0], color[1], color[2]);
                gl.uniform_1_f32(Some(&self.u_alpha), alpha);
                gl.line_width(line_width * px);
                upload_and_draw(gl, self.vbo, verts, mode);
                gl.draw_arrays(mode, 0, verts.len() as i32 / 3);
            };
            let lines = |verts: &[f32], color: [f32; 3], line_width: f32| draw(verts, glow::LINES, color, line_width, 1.0);

            let [ox, oy, oz] = scene.room_offset;
            let [rw, rd, rh] = scene.room;

            // Floor grid inside the room: 0.5 m, every metre stronger
            let (mut minor, mut major) = (Vec::new(), Vec::new());
            let mut k = 0;
            while k as f32 * 0.5 <= rw + 1e-3 {
                let x = ox + k as f32 * 0.5;
                let out = if k % 2 == 0 { &mut major } else { &mut minor };
                out.extend_from_slice(&[x, oy, oz, x, oy + rd, oz]);
                k += 1;
            }
            k = 0;
            while k as f32 * 0.5 <= rd + 1e-3 {
                let y = oy + k as f32 * 0.5;
                let out = if k % 2 == 0 { &mut major } else { &mut minor };
                out.extend_from_slice(&[ox, y, oz, ox + rw, y, oz]);
                k += 1;
            }
            lines(&minor, style.grid, 1.0);
            lines(&major, style.grid_major, 1.0);

            // The coverage slice: a cell per point, coloured by its count
            if !scene.slice.is_empty() {
                let h = scene.cell / 2.0;
                let z = scene.slice_z;
                let mut by_color: Vec<([f32; 3], Vec<f32>)> = Vec::new();
                for &(x, y, color) in scene.slice {
                    let verts = match by_color.iter_mut().find(|(c, _)| *c == color) {
                        Some((_, v)) => v,
                        None => {
                            by_color.push((color, Vec::new()));
                            &mut by_color.last_mut().unwrap().1
                        }
                    };
                    verts.extend_from_slice(&[
                        x - h, y - h, z, x + h, y - h, z, x + h, y + h, z,
                        x - h, y - h, z, x + h, y + h, z, x - h, y + h, z,
                    ]);
                }
                for (color, verts) in &by_color {
                    draw(verts, glow::TRIANGLES, *color, 1.0, 0.62);
                }
            }

            let mut pads = Vec::new();
            for pad in scene.pads {
                circle_segments(&mut pads, [pad.x, pad.y, oz], pad.radius, 32);
            }
            lines(&pads, style.pad, 1.2);

            let mut room = Vec::new();
            box_edges(&mut room, [ox, oy, oz], [ox + rw, oy + rd, oz + rh], None);
            lines(&room, style.room, 1.0);

            // Obstacles: see-through, hiding each other
            if !scene.obstacle_triangles.is_empty() {
                gl.enable(glow::DEPTH_TEST);
                for (i, tris) in scene.obstacle_triangles.iter().enumerate() {
                    let c = scene.obstacle_colors.get(i).copied().unwrap_or([0.45, 0.45, 0.50]);
                    draw(tris, glow::TRIANGLES, c, 1.0, 0.55);
                }
                gl.disable(glow::DEPTH_TEST);
                for (i, wire) in scene.obstacle_wireframes.iter().enumerate() {
                    let selected = scene.selected_type == 3 && scene.selected_index == i as i32;
                    if selected {
                        lines(wire, style.accent, 2.2);
                    } else {
                        lines(wire, style.muted, 1.0);
                    }
                }
            }

            // Flight areas dashed long, waypoint areas dashed short; the
            // selected one solid
            for (kind, areas, color, dash) in [
                (1, scene.flight_areas, style.area, (0.10, 0.06)),
                (2, scene.waypoint_areas, style.waypoint_area, (0.03, 0.06)),
            ] {
                let mut verts = Vec::new();
                for (i, a) in areas.iter().enumerate() {
                    if scene.selected_area_kind == kind && scene.selected_area_index == i as i32 {
                        let mut sel = Vec::new();
                        box_edges(&mut sel, a.min, a.max, None);
                        lines(&sel, style.accent, 2.2);
                    } else {
                        box_edges(&mut verts, a.min, a.max, Some(dash));
                    }
                }
                lines(&verts, color, 1.5);
            }

            for (i, traj) in scene.trajectories.iter().enumerate() {
                if traj.len() < 2 {
                    continue;
                }
                let (r, g, b) = hue_to_rgb((i as f32 * 0.618033988) % 1.0);
                let verts: Vec<f32> = traj.iter().flat_map(|p| p.iter().copied()).collect();
                draw(&verts, glow::LINE_STRIP, [r, g, b], 1.5, 1.0);
            }

            // Base stations: where they aim, and the field of view of the
            // selected one
            let horiz_half = (160.0_f32 / 2.0).to_radians();
            let vert_half = (115.0_f32 / 2.0).to_radians();
            let mut aims = Vec::new();
            for (i, (pos, rot)) in scene.base_stations.iter().enumerate() {
                let to_world = |l: [f32; 3]| -> [f32; 3] {
                    [
                        pos[0] + rot[0][0] * l[0] + rot[0][1] * l[1] + rot[0][2] * l[2],
                        pos[1] + rot[1][0] * l[0] + rot[1][1] * l[1] + rot[1][2] * l[2],
                        pos[2] + rot[2][0] * l[0] + rot[2][1] * l[1] + rot[2][2] * l[2],
                    ]
                };
                aims.extend_from_slice(pos);
                aims.extend_from_slice(&to_world([0.6, 0.0, 0.0]));
                if scene.selected_type == 1 && scene.selected_index == i as i32 {
                    let len = 1.5_f32;
                    let corners: Vec<[f32; 3]> = [(1.0, 1.0), (-1.0, 1.0), (-1.0, -1.0), (1.0, -1.0)]
                        .iter()
                        .map(|&(sh, sv)| {
                            let d = [1.0, sh * horiz_half.tan(), sv * vert_half.tan()];
                            let n = dot(d, d).sqrt();
                            to_world([d[0] * len / n, d[1] * len / n, d[2] * len / n])
                        })
                        .collect();
                    let mut faces = Vec::new();
                    let mut edges = Vec::new();
                    for c in 0..4 {
                        let (a, b) = (corners[c], corners[(c + 1) % 4]);
                        faces.extend_from_slice(pos);
                        faces.extend_from_slice(&a);
                        faces.extend_from_slice(&b);
                        edges.extend_from_slice(pos);
                        edges.extend_from_slice(&a);
                        edges.extend_from_slice(&a);
                        edges.extend_from_slice(&b);
                    }
                    draw(&faces, glow::TRIANGLES, style.accent, 1.0, 0.07);
                    lines(&edges, style.accent, 1.25);
                }
            }
            lines(&aims, style.base_station, 1.5);

            // The gizmo of the selected object: arrows along the axes, and
            // the arcs that turn it
            let sel_pos = match scene.selected_type {
                1 => scene.base_stations.get(scene.selected_index as usize).map(|b| b.0),
                2 => scene.anchors.get(scene.selected_index as usize).copied(),
                3 => scene.obstacle_triangles.get(scene.selected_index as usize).and_then(|tris| {
                    let n = tris.len() / 3;
                    (n > 0).then(|| {
                        let mut c = [0.0f32; 3];
                        for v in tris.chunks_exact(3) {
                            c = [c[0] + v[0], c[1] + v[1], c[2] + v[2]];
                        }
                        [c[0] / n as f32, c[1] / n as f32, c[2] / n as f32]
                    })
                }),
                _ => None,
            };
            let mut gizmo_points: Vec<([f32; 3], [f32; 3], bool)> = Vec::new();
            if let Some(pos) = sel_pos {
                for (axis, handle) in [HANDLE_TRANSLATE_X, HANDLE_TRANSLATE_Y, HANDLE_TRANSLATE_Z].into_iter().enumerate() {
                    let mut end = pos;
                    end[axis] += GIZMO_TRANSLATE_LEN;
                    let active = scene.active_handle == handle;
                    lines(&[pos[0], pos[1], pos[2], end[0], end[1], end[2]], style.axes[axis], if active { 3.0 } else { 2.0 });
                    gizmo_points.push((end, style.axes[axis], active));
                }
                let arc = |center: f32, point: &dyn Fn(f32) -> [f32; 3]| -> Vec<f32> {
                    (0..=GIZMO_ARC_STEPS)
                        .flat_map(|step| point(center - GIZMO_ARC_SPAN + 2.0 * GIZMO_ARC_SPAN * step as f32 / GIZMO_ARC_STEPS as f32))
                        .collect()
                };
                if scene.selected_type == 1 {
                    let (_, rot) = &scene.base_stations[scene.selected_index as usize];
                    let azimuth = rot[1][0].atan2(rot[0][0]);
                    let elevation = (-rot[2][0]).asin();
                    let az = arc(azimuth, &|a| [pos[0] + GIZMO_ROTATE_RADIUS * a.cos(), pos[1] + GIZMO_ROTATE_RADIUS * a.sin(), pos[2]]);
                    let el = arc(elevation, &|e| [
                        pos[0] + GIZMO_ROTATE_RADIUS * e.cos() * azimuth.cos(),
                        pos[1] + GIZMO_ROTATE_RADIUS * e.cos() * azimuth.sin(),
                        pos[2] - GIZMO_ROTATE_RADIUS * e.sin(),
                    ]);
                    for (verts, color, handle) in [(az, style.accent, HANDLE_ROTATE_AZ), (el, style.area, HANDLE_ROTATE_EL)] {
                        let active = scene.active_handle == handle;
                        draw(&verts, glow::LINE_STRIP, color, if active { 3.0 } else { 2.0 }, 1.0);
                        let n = verts.len();
                        gizmo_points.push(([verts[0], verts[1], verts[2]], color, active));
                        gizmo_points.push(([verts[n - 3], verts[n - 2], verts[n - 1]], color, active));
                    }
                }
                if scene.selected_type == 3 {
                    let ring: Vec<f32> = (0..=GIZMO_ARC_STEPS * 2)
                        .flat_map(|step| {
                            let a = std::f32::consts::TAU * step as f32 / (GIZMO_ARC_STEPS * 2) as f32;
                            [pos[0] + GIZMO_ROTATE_RADIUS * a.cos(), pos[1] + GIZMO_ROTATE_RADIUS * a.sin(), pos[2]]
                        })
                        .collect();
                    let active = scene.active_handle == HANDLE_ROTATE_YAW;
                    draw(&ring, glow::LINE_STRIP, style.accent, if active { 3.0 } else { 2.0 }, 1.0);
                }
            }

            // Markers on top, far ones first
            let no_ring = ([0.0; 3], 0.0);
            let mut markers: Vec<Marker> = Vec::new();
            for (i, (pos, _)) in scene.base_stations.iter().enumerate() {
                let selected = scene.selected_type == 1 && scene.selected_index == i as i32;
                markers.push(Marker {
                    pos: *pos,
                    size: Some(16.0 * px),
                    fill: style.surface,
                    inner: (if selected { style.accent } else { style.base_station }, 2.0 * px),
                    rings: if selected { [(style.scene, 2.0 * px), (style.accent_tint, 3.0 * px), no_ring] } else { [no_ring; 3] },
                    square: true,
                    alpha: 1.0,
                });
            }
            for (i, pos) in scene.anchors.iter().enumerate() {
                let selected = scene.selected_type == 2 && scene.selected_index == i as i32;
                markers.push(Marker {
                    pos: *pos,
                    size: Some(14.0 * px),
                    fill: style.surface,
                    inner: (if selected { style.accent } else { style.axes[2] }, 2.5 * px),
                    rings: if selected { [(style.scene, 2.0 * px), (style.accent_tint, 3.0 * px), no_ring] } else { [(style.scene, 1.5 * px), no_ring, no_ring] },
                    square: false,
                    alpha: 1.0,
                });
            }
            // Handles on the sides of the selected area
            let selected_area = match scene.selected_area_kind {
                1 => scene.flight_areas.get(scene.selected_area_index as usize),
                2 => scene.waypoint_areas.get(scene.selected_area_index as usize),
                _ => None,
            };
            if let Some(area) = selected_area {
                for h in area_handle_positions(area.min, area.max) {
                    markers.push(Marker {
                        pos: h,
                        size: Some(12.0 * px),
                        fill: style.surface,
                        inner: (style.accent, 2.0 * px),
                        rings: [(style.scene, 1.5 * px), no_ring, no_ring],
                        square: false,
                        alpha: 1.0,
                    });
                }
            }
            for (pos, color, active) in gizmo_points {
                markers.push(Marker {
                    pos,
                    size: Some(if active { 12.0 } else { 9.0 } * px),
                    fill: color,
                    inner: no_ring,
                    rings: [(style.scene, 1.5 * px), no_ring, no_ring],
                    square: false,
                    alpha: 1.0,
                });
            }
            let depth = |m: &Marker| clip_w(m.pos, &mvp);
            markers.sort_by(|a, b| depth(b).total_cmp(&depth(a)));
            self.draw_markers(gl, &markers, &mvp, height as f32, px);

            gl.use_program(None);
            gl.line_width(1.0);
            gl.disable(glow::BLEND);

            if let Some(msaa) = &self.msaa {
                gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(msaa.fbo));
                gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(self.next_texture.fbo));
                gl.blit_framebuffer(0, 0, width as i32, height as i32, 0, 0, width as i32, height as i32, glow::COLOR_BUFFER_BIT, glow::NEAREST);
            }
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, saved_read_fbo);
            gl.viewport(saved_viewport[0], saved_viewport[1], saved_viewport[2], saved_viewport[3]);
        }

        let result = unsafe {
            slint::BorrowedOpenGLTextureBuilder::new_gl_2d_rgba_texture(
                self.next_texture.texture.0,
                (self.next_texture.width, self.next_texture.height).into(),
            )
            .build()
        };
        std::mem::swap(&mut self.next_texture, &mut self.displayed_texture);
        result
    }
}

/// Map a normalised GDOP ratio `t` ∈ [0, 1] to an RGB colour.
/// 0 = green (good), 0.5 = yellow, 1 = red (bad).
pub fn gdop_color(t: f32) -> (f32, f32, f32) {
    let t = t.clamp(0.0, 1.0);
    let r = (2.0 * t).min(1.0);
    let g = (2.0 * (1.0 - t)).min(1.0);
    (r, g, 0.1)
}

/// Hit-test TDoA3 anchors. Returns index of closest anchor within threshold, or -1.
pub fn hit_test_anchor(
    screen_x: f32,
    screen_y: f32,
    anchors: &[[f32; 3]],
    mvp: &[f32; 16],
    width: u32,
    height: u32,
    threshold: f32,
) -> i32 {
    let mut best_idx = -1i32;
    let mut best_dist = threshold;
    for (i, pos) in anchors.iter().enumerate() {
        if let Some((sx, sy)) = project_to_screen(*pos, mvp, width, height) {
            let dist = ((screen_x - sx).powi(2) + (screen_y - sy).powi(2)).sqrt();
            if dist < best_dist {
                best_dist = dist;
                best_idx = i as i32;
            }
        }
    }
    best_idx
}

/// Hit-test translation-only gizmo handles for a selected anchor. Returns handle ID or HANDLE_NONE.
pub fn hit_test_anchor_gizmo(
    screen_x: f32,
    screen_y: f32,
    anchor_pos: [f32; 3],
    mvp: &[f32; 16],
    width: u32,
    height: u32,
) -> i32 {
    let Some((sx0, sy0)) = project_to_screen(anchor_pos, mvp, width, height) else {
        return HANDLE_NONE;
    };

    let mut best_handle = HANDLE_NONE;
    let mut best_dist = GIZMO_HIT_THRESHOLD;

    let axes: [[f32; 3]; 3] = [
        [GIZMO_TRANSLATE_LEN, 0.0, 0.0],
        [0.0, GIZMO_TRANSLATE_LEN, 0.0],
        [0.0, 0.0, GIZMO_TRANSLATE_LEN],
    ];
    for (i, axis) in axes.iter().enumerate() {
        let end = [
            anchor_pos[0] + axis[0],
            anchor_pos[1] + axis[1],
            anchor_pos[2] + axis[2],
        ];
        if let Some((sx1, sy1)) = project_to_screen(end, mvp, width, height) {
            let dist = point_to_segment_dist_2d(screen_x, screen_y, sx0, sy0, sx1, sy1);
            if dist < best_dist {
                best_dist = dist;
                best_handle = (i + 1) as i32;
            }
        }
    }

    best_handle
}

impl Drop for Scene3DRenderer {
    fn drop(&mut self) {
        unsafe {
            self.gl.delete_program(self.program);
            self.gl.delete_program(self.marker.program);
            self.gl.delete_vertex_array(self.vao);
            self.gl.delete_buffer(self.vbo);
        }
    }
}

/// Convert a hue (0.0–1.0) to an RGB triple with full saturation and brightness.
fn hue_to_rgb(h: f32) -> (f32, f32, f32) {
    let h = h.fract();
    let h6 = h * 6.0;
    let i = h6 as i32;
    let f = h6 - i as f32;
    match i % 6 {
        0 => (1.0, f, 0.0),
        1 => (1.0 - f, 1.0, 0.0),
        2 => (0.0, 1.0, f),
        3 => (0.0, 1.0 - f, 1.0),
        4 => (f, 0.0, 1.0),
        _ => (1.0, 0.0, 1.0 - f),
    }
}

/// Clip-space w of `p`: its distance along the view direction.
pub fn clip_w(p: [f32; 3], mvp: &[f32; 16]) -> f32 {
    mvp[3] * p[0] + mvp[7] * p[1] + mvp[11] * p[2] + mvp[15]
}

/// Line segments around a horizontal circle.
fn circle_segments(out: &mut Vec<f32>, center: [f32; 3], radius: f32, segments: usize) {
    let point = |i: usize| {
        let a = i as f32 / segments as f32 * std::f32::consts::TAU;
        [center[0] + radius * a.cos(), center[1] + radius * a.sin(), center[2]]
    };
    for i in 0..segments {
        out.extend_from_slice(&point(i));
        out.extend_from_slice(&point(i + 1));
    }
}

/// Triangles filling a horizontal disc.
fn disc_triangles(out: &mut Vec<f32>, center: [f32; 3], radius: f32, segments: usize) {
    for i in 0..segments {
        let a0 = i as f32 / segments as f32 * std::f32::consts::TAU;
        let a1 = (i + 1) as f32 / segments as f32 * std::f32::consts::TAU;
        out.extend_from_slice(&center);
        out.extend_from_slice(&[center[0] + radius * a0.cos(), center[1] + radius * a0.sin(), center[2]]);
        out.extend_from_slice(&[center[0] + radius * a1.cos(), center[1] + radius * a1.sin(), center[2]]);
    }
}

/// A line from `a` to `b` as dashes `on` metres long with `off` between.
fn dashed(out: &mut Vec<f32>, a: [f32; 3], b: [f32; 3], on: f32, off: f32) {
    let d = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
    let len = dot(d, d).sqrt();
    if len < 1e-6 {
        return;
    }
    let at = |t: f32| [a[0] + d[0] * t / len, a[1] + d[1] * t / len, a[2] + d[2] * t / len];
    let mut t = 0.0;
    while t < len {
        out.extend_from_slice(&at(t));
        out.extend_from_slice(&at((t + on).min(len)));
        t += on + off;
    }
}

/// The 12 edges of a box, dashed when `dash` gives (on, off) lengths.
fn box_edges(out: &mut Vec<f32>, min: [f32; 3], max: [f32; 3], dash: Option<(f32, f32)>) {
    let [x0, y0, z0] = min;
    let [x1, y1, z1] = max;
    let corners = [[x0, y0], [x1, y0], [x1, y1], [x0, y1]];
    for k in 0..4 {
        let [ax, ay] = corners[k];
        let [bx, by] = corners[(k + 1) % 4];
        for (a, b) in [([ax, ay, z0], [bx, by, z0]), ([ax, ay, z1], [bx, by, z1]), ([ax, ay, z0], [ax, ay, z1])] {
            match dash {
                Some((on, off)) => dashed(out, a, b, on, off),
                None => {
                    out.extend_from_slice(&a);
                    out.extend_from_slice(&b);
                }
            }
        }
    }
}

unsafe fn upload_and_draw(gl: &glow::Context, vbo: glow::Buffer, data: &[f32], _mode: u32) {
    unsafe {
        gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
        gl.buffer_data_u8_slice(
            glow::ARRAY_BUFFER,
            data.align_to().1,
            glow::DYNAMIC_DRAW,
        );
    }
}

/// Project a 3D world point to 2D screen coordinates.
/// Returns (screen_x, screen_y) in pixels, or None if the point is behind the camera.
pub fn project_to_screen(
    point: [f32; 3],
    mvp: &[f32; 16],
    width: u32,
    height: u32,
) -> Option<(f32, f32)> {
    // Multiply by MVP: clip = mvp * [x, y, z, 1]
    let x = mvp[0] * point[0] + mvp[4] * point[1] + mvp[8]  * point[2] + mvp[12];
    let y = mvp[1] * point[0] + mvp[5] * point[1] + mvp[9]  * point[2] + mvp[13];
    let w = mvp[3] * point[0] + mvp[7] * point[1] + mvp[11] * point[2] + mvp[15];

    if w <= 0.0 {
        return None; // Behind camera
    }

    // Perspective divide -> NDC
    let ndc_x = x / w;
    let ndc_y = y / w;

    // NDC to screen (NDC is -1..1, Y is already flipped by the perspective matrix)
    let screen_x = (ndc_x + 1.0) * 0.5 * width as f32;
    let screen_y = (ndc_y + 1.0) * 0.5 * height as f32;

    Some((screen_x, screen_y))
}

pub fn compute_mvp(yaw: f32, pitch: f32, distance: f32, pan_x: f32, pan_y: f32, aspect: f32) -> [f32; 16] {
    build_mvp(yaw, pitch, distance, pan_x, pan_y, aspect)
}

/// Order of face-center drag handles for an area box: XMin, XMax, YMin, YMax, ZMin, ZMax.
pub fn area_handle_positions(min: [f32; 3], max: [f32; 3]) -> [[f32; 3]; 6] {
    let cx = (min[0] + max[0]) * 0.5;
    let cy = (min[1] + max[1]) * 0.5;
    let cz = (min[2] + max[2]) * 0.5;
    [
        [min[0], cy, cz], // XMin
        [max[0], cy, cz], // XMax
        [cx, min[1], cz], // YMin
        [cx, max[1], cz], // YMax
        [cx, cy, min[2]], // ZMin
        [cx, cy, max[2]], // ZMax
    ]
}

pub fn build_mvp(yaw: f32, pitch: f32, distance: f32, pan_x: f32, pan_y: f32, aspect: f32) -> [f32; 16] {
    // Camera position from spherical coordinates
    let cam_x = distance * pitch.cos() * yaw.cos();
    let cam_y = distance * pitch.cos() * yaw.sin();
    let cam_z = distance * pitch.sin();

    // Pan target: offset the look-at center
    let target = [pan_x, pan_y, 0.5];
    let eye = [cam_x + pan_x, cam_y + pan_y, cam_z];

    let view = look_at(eye, target, [0.0, 0.0, 1.0]);
    // near=0.3 keeps good depth precision even with the far range needed for
    // zooming far out, so coplanar wireframe edges don't z-fight with faces.
    let proj = perspective(FOV_DEG.to_radians(), aspect, 0.3, 300.0);

    mat4_mul(&proj, &view)
}

fn look_at(eye: [f32; 3], center: [f32; 3], up: [f32; 3]) -> [f32; 16] {
    let f = normalize(sub(center, eye));
    let s = normalize(cross(f, up));
    let u = cross(s, f);

    [
        s[0], u[0], -f[0], 0.0,
        s[1], u[1], -f[1], 0.0,
        s[2], u[2], -f[2], 0.0,
        -dot(s, eye), -dot(u, eye), dot(f, eye), 1.0,
    ]
}

fn perspective(fov: f32, aspect: f32, near: f32, far: f32) -> [f32; 16] {
    let f = 1.0 / (fov / 2.0).tan();
    let nf = 1.0 / (near - far);
    [
        f / aspect, 0.0, 0.0, 0.0,
        0.0, -f, 0.0, 0.0,
        0.0, 0.0, (far + near) * nf, -1.0,
        0.0, 0.0, 2.0 * far * near * nf, 0.0,
    ]
}

fn mat4_mul(a: &[f32; 16], b: &[f32; 16]) -> [f32; 16] {
    let mut out = [0.0f32; 16];
    for i in 0..4 {
        for j in 0..4 {
            for k in 0..4 {
                out[i * 4 + j] += a[k * 4 + j] * b[i * 4 + k];
            }
        }
    }
    out
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] { [a[0]-b[0], a[1]-b[1], a[2]-b[2]] }
fn dot(a: [f32; 3], b: [f32; 3]) -> f32 { a[0]*b[0] + a[1]*b[1] + a[2]*b[2] }
fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[1]*b[2]-a[2]*b[1], a[2]*b[0]-a[0]*b[2], a[0]*b[1]-a[1]*b[0]]
}
fn normalize(v: [f32; 3]) -> [f32; 3] {
    let len = dot(v, v).sqrt();
    if len < 1e-10 { return [0.0; 3]; }
    [v[0]/len, v[1]/len, v[2]/len]
}

// --- Gizmo constants and hit testing ---

pub const HANDLE_NONE: i32 = 0;
pub const HANDLE_TRANSLATE_X: i32 = 1;
pub const HANDLE_TRANSLATE_Y: i32 = 2;
pub const HANDLE_TRANSLATE_Z: i32 = 3;
pub const HANDLE_ROTATE_AZ: i32 = 4;
pub const HANDLE_ROTATE_EL: i32 = 5;
pub const HANDLE_ROTATE_YAW: i32 = 6;

pub const GIZMO_TRANSLATE_LEN: f32 = 1.0;
const GIZMO_ROTATE_RADIUS: f32 = 0.7;
const GIZMO_HIT_THRESHOLD: f32 = 15.0;
const GIZMO_ARC_STEPS: usize = 24;
const GIZMO_ARC_SPAN: f32 = std::f32::consts::PI / 3.0; // ±60°

fn point_to_segment_dist_2d(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let dx = bx - ax;
    let dy = by - ay;
    let len_sq = dx * dx + dy * dy;
    if len_sq < 1e-10 {
        return ((px - ax).powi(2) + (py - ay).powi(2)).sqrt();
    }
    let t = (((px - ax) * dx + (py - ay) * dy) / len_sq).clamp(0.0, 1.0);
    let cx = ax + t * dx;
    let cy = ay + t * dy;
    ((px - cx).powi(2) + (py - cy).powi(2)).sqrt()
}

/// Hit-test base stations. Returns index of closest BS within threshold, or -1.
pub fn hit_test_base_station(
    screen_x: f32,
    screen_y: f32,
    base_stations: &[([f32; 3], [[f32; 3]; 3])],
    mvp: &[f32; 16],
    width: u32,
    height: u32,
    threshold: f32,
) -> i32 {
    let mut best_idx = -1i32;
    let mut best_dist = threshold;
    for (i, (pos, _)) in base_stations.iter().enumerate() {
        if let Some((sx, sy)) = project_to_screen(*pos, mvp, width, height) {
            let dist = ((screen_x - sx).powi(2) + (screen_y - sy).powi(2)).sqrt();
            if dist < best_dist {
                best_dist = dist;
                best_idx = i as i32;
            }
        }
    }
    best_idx
}

/// Hit-test gizmo handles for a selected BS. Returns handle ID or HANDLE_NONE.
pub fn hit_test_gizmo(
    screen_x: f32,
    screen_y: f32,
    bs_pos: [f32; 3],
    bs_rot: &[[f32; 3]; 3],
    mvp: &[f32; 16],
    width: u32,
    height: u32,
) -> i32 {
    let Some((sx0, sy0)) = project_to_screen(bs_pos, mvp, width, height) else {
        return HANDLE_NONE;
    };

    let mut best_handle = HANDLE_NONE;
    let mut best_dist = GIZMO_HIT_THRESHOLD;

    // Translation handles along world axes
    let axes: [[f32; 3]; 3] = [
        [GIZMO_TRANSLATE_LEN, 0.0, 0.0],
        [0.0, GIZMO_TRANSLATE_LEN, 0.0],
        [0.0, 0.0, GIZMO_TRANSLATE_LEN],
    ];
    for (i, axis) in axes.iter().enumerate() {
        let end = [bs_pos[0] + axis[0], bs_pos[1] + axis[1], bs_pos[2] + axis[2]];
        if let Some((sx1, sy1)) = project_to_screen(end, mvp, width, height) {
            let dist = point_to_segment_dist_2d(screen_x, screen_y, sx0, sy0, sx1, sy1);
            if dist < best_dist {
                best_dist = dist;
                best_handle = (i + 1) as i32; // 1=X, 2=Y, 3=Z
            }
        }
    }

    // Derive azimuth/elevation from rotation matrix
    let azimuth = bs_rot[1][0].atan2(bs_rot[0][0]);
    let elevation = (-bs_rot[2][0]).asin();

    // Azimuth arc (XY plane at BS height)
    for step in 0..GIZMO_ARC_STEPS {
        let a0 = azimuth - GIZMO_ARC_SPAN
            + 2.0 * GIZMO_ARC_SPAN * step as f32 / GIZMO_ARC_STEPS as f32;
        let a1 = azimuth - GIZMO_ARC_SPAN
            + 2.0 * GIZMO_ARC_SPAN * (step + 1) as f32 / GIZMO_ARC_STEPS as f32;
        let p0 = [
            bs_pos[0] + GIZMO_ROTATE_RADIUS * a0.cos(),
            bs_pos[1] + GIZMO_ROTATE_RADIUS * a0.sin(),
            bs_pos[2],
        ];
        let p1 = [
            bs_pos[0] + GIZMO_ROTATE_RADIUS * a1.cos(),
            bs_pos[1] + GIZMO_ROTATE_RADIUS * a1.sin(),
            bs_pos[2],
        ];
        if let (Some((s0x, s0y)), Some((s1x, s1y))) = (
            project_to_screen(p0, mvp, width, height),
            project_to_screen(p1, mvp, width, height),
        ) {
            let dist = point_to_segment_dist_2d(screen_x, screen_y, s0x, s0y, s1x, s1y);
            if dist < best_dist {
                best_dist = dist;
                best_handle = HANDLE_ROTATE_AZ;
            }
        }
    }

    // Elevation arc (vertical plane of current azimuth)
    for step in 0..GIZMO_ARC_STEPS {
        let e0 = elevation - GIZMO_ARC_SPAN
            + 2.0 * GIZMO_ARC_SPAN * step as f32 / GIZMO_ARC_STEPS as f32;
        let e1 = elevation - GIZMO_ARC_SPAN
            + 2.0 * GIZMO_ARC_SPAN * (step + 1) as f32 / GIZMO_ARC_STEPS as f32;
        let p0 = [
            bs_pos[0] + GIZMO_ROTATE_RADIUS * e0.cos() * azimuth.cos(),
            bs_pos[1] + GIZMO_ROTATE_RADIUS * e0.cos() * azimuth.sin(),
            bs_pos[2] - GIZMO_ROTATE_RADIUS * e0.sin(),
        ];
        let p1 = [
            bs_pos[0] + GIZMO_ROTATE_RADIUS * e1.cos() * azimuth.cos(),
            bs_pos[1] + GIZMO_ROTATE_RADIUS * e1.cos() * azimuth.sin(),
            bs_pos[2] - GIZMO_ROTATE_RADIUS * e1.sin(),
        ];
        if let (Some((s0x, s0y)), Some((s1x, s1y))) = (
            project_to_screen(p0, mvp, width, height),
            project_to_screen(p1, mvp, width, height),
        ) {
            let dist = point_to_segment_dist_2d(screen_x, screen_y, s0x, s0y, s1x, s1y);
            if dist < best_dist {
                best_dist = dist;
                best_handle = HANDLE_ROTATE_EL;
            }
        }
    }

    best_handle
}
