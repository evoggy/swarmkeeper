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

#[derive(Clone, Copy)]
pub struct UnitPos {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub color: [f32; 3],
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
    displayed_texture: RenderTexture,
    next_texture: RenderTexture,
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

impl Scene3DRenderer {
    pub fn new(gl: glow::Context) -> Self {
        let gl = Rc::new(gl);
        unsafe {
            let program = gl.create_program().expect("Cannot create program");

            let shaders_src = [
                (glow::VERTEX_SHADER, VERTEX_SHADER),
                (glow::FRAGMENT_SHADER, FRAGMENT_SHADER),
            ];

            let mut shaders = Vec::new();
            for (shader_type, src) in &shaders_src {
                let shader = gl.create_shader(*shader_type).expect("Cannot create shader");
                gl.shader_source(shader, src);
                gl.compile_shader(shader);
                if !gl.get_shader_compile_status(shader) {
                    panic!("Shader compile error: {}", gl.get_shader_info_log(shader));
                }
                gl.attach_shader(program, shader);
                shaders.push(shader);
            }

            gl.link_program(program);
            if !gl.get_program_link_status(program) {
                panic!("Program link error: {}", gl.get_program_info_log(program));
            }
            for s in shaders {
                gl.detach_shader(program, s);
                gl.delete_shader(s);
            }

            let u_mvp = gl.get_uniform_location(program, "u_mvp").unwrap();
            let u_color = gl.get_uniform_location(program, "u_color").unwrap();
            let u_point_size = gl.get_uniform_location(program, "u_point_size").unwrap();
            let u_alpha = gl.get_uniform_location(program, "u_alpha").unwrap();

            let vbo = gl.create_buffer().expect("Cannot create buffer");
            let vao = gl.create_vertex_array().expect("Cannot create VAO");

            let pos_loc = gl.get_attrib_location(program, "position").unwrap();
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
                displayed_texture, next_texture,
            }
        }
    }

    pub fn render(
        &mut self,
        width: u32,
        height: u32,
        yaw: f32,
        pitch: f32,
        distance: f32,
        pan_x: f32,
        pan_y: f32,
        units: &[UnitPos],
        fixed_points: &[UnitPos],
        trajectory_lines: &[f32],
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

            // Draw ground grid
            let mut grid_verts = Vec::new();
            let grid_range = 5;
            for i in -grid_range..=grid_range {
                let v = i as f32;
                // Line parallel to Y axis
                grid_verts.extend_from_slice(&[v, -(grid_range as f32), 0.0, v, grid_range as f32, 0.0]);
                // Line parallel to X axis
                grid_verts.extend_from_slice(&[-(grid_range as f32), v, 0.0, grid_range as f32, v, 0.0]);
            }
            upload_and_draw(gl, self.vbo, &grid_verts, glow::LINES);
            gl.uniform_3_f32(Some(&self.u_color), 0.3, 0.3, 0.35);
            gl.draw_arrays(glow::LINES, 0, grid_verts.len() as i32 / 3);

            // Draw axes (2m each)
            // X axis - red
            gl.uniform_3_f32(Some(&self.u_color), 0.94, 0.27, 0.27);
            let x_axis = [0.0, 0.0, 0.0, 2.0, 0.0, 0.0];
            upload_and_draw(gl, self.vbo, &x_axis, glow::LINES);
            gl.draw_arrays(glow::LINES, 0, 2);

            // Y axis - green
            gl.uniform_3_f32(Some(&self.u_color), 0.29, 0.85, 0.50);
            let y_axis = [0.0, 0.0, 0.0, 0.0, 2.0, 0.0];
            upload_and_draw(gl, self.vbo, &y_axis, glow::LINES);
            gl.draw_arrays(glow::LINES, 0, 2);

            // Z axis - blue
            gl.uniform_3_f32(Some(&self.u_color), 0.38, 0.65, 0.98);
            let z_axis = [0.0, 0.0, 0.0, 0.0, 0.0, 2.0];
            upload_and_draw(gl, self.vbo, &z_axis, glow::LINES);
            gl.draw_arrays(glow::LINES, 0, 2);

            // Draw units as points
            gl.uniform_1_f32(Some(&self.u_point_size), 10.0);
            for unit in units {
                gl.uniform_3_f32(Some(&self.u_color), unit.color[0], unit.color[1], unit.color[2]);
                let pos = [unit.x, unit.y, unit.z];
                upload_and_draw(gl, self.vbo, &pos, glow::POINTS);
                gl.draw_arrays(glow::POINTS, 0, 1);

                // Vertical line from unit to ground
                gl.uniform_3_f32(
                    Some(&self.u_color),
                    unit.color[0] * 0.4,
                    unit.color[1] * 0.4,
                    unit.color[2] * 0.4,
                );
                gl.uniform_1_f32(Some(&self.u_point_size), 1.0);
                let drop_line = [unit.x, unit.y, unit.z, unit.x, unit.y, 0.0];
                upload_and_draw(gl, self.vbo, &drop_line, glow::LINES);
                gl.draw_arrays(glow::LINES, 0, 2);
                gl.uniform_1_f32(Some(&self.u_point_size), 10.0);
            }

            // Draw fixed points (base stations / anchors)
            gl.uniform_1_f32(Some(&self.u_point_size), 8.0);
            for pt in fixed_points {
                gl.uniform_3_f32(Some(&self.u_color), pt.color[0], pt.color[1], pt.color[2]);
                let pos = [pt.x, pt.y, pt.z];
                upload_and_draw(gl, self.vbo, &pos, glow::POINTS);
                gl.draw_arrays(glow::POINTS, 0, 1);

                // Drop line to ground
                gl.uniform_3_f32(
                    Some(&self.u_color),
                    pt.color[0] * 0.4,
                    pt.color[1] * 0.4,
                    pt.color[2] * 0.4,
                );
                gl.uniform_1_f32(Some(&self.u_point_size), 1.0);
                let drop_line = [pt.x, pt.y, pt.z, pt.x, pt.y, 0.0];
                upload_and_draw(gl, self.vbo, &drop_line, glow::LINES);
                gl.draw_arrays(glow::LINES, 0, 2);
                gl.uniform_1_f32(Some(&self.u_point_size), 8.0);
            }

            // Draw trajectory path
            if trajectory_lines.len() >= 6 {
                gl.uniform_3_f32(Some(&self.u_color), 0.0, 0.9, 0.9); // cyan
                gl.uniform_1_f32(Some(&self.u_point_size), 1.0);
                upload_and_draw(gl, self.vbo, trajectory_lines, glow::LINE_STRIP);
                gl.draw_arrays(glow::LINE_STRIP, 0, (trajectory_lines.len() / 3) as i32);
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
}

/// Map a normalised GDOP ratio `t` ∈ [0, 1] to an RGB colour.
/// 0 = green (good), 0.5 = yellow, 1 = red (bad).
fn gdop_color(t: f32) -> (f32, f32, f32) {
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

fn build_mvp(yaw: f32, pitch: f32, distance: f32, pan_x: f32, pan_y: f32, aspect: f32) -> [f32; 16] {
    // Camera position from spherical coordinates
    let cam_x = distance * pitch.cos() * yaw.cos();
    let cam_y = distance * pitch.cos() * yaw.sin();
    let cam_z = distance * pitch.sin();

    // Pan target: offset the look-at center
    let target = [pan_x, pan_y, 0.5];
    let eye = [cam_x + pan_x, cam_y + pan_y, cam_z];

    let view = look_at(eye, target, [0.0, 0.0, 1.0]);
    let proj = perspective(45.0_f32.to_radians(), aspect, 0.1, 100.0);

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
