// EGL/GLES2 renderer (SW path, Phase 9).
//
// Uploads decoded BGRA frames as GL_RGBA textures and blits them via a
// fullscreen two-triangle quad.  The fragment shader swizzles B↔R so
// BGRA bytes uploaded as RGBA channels appear with the correct colors.
//
// Requires wayland-client with features = ["system"] (raw wl_display* /
// wl_surface* pointers needed for EGL surface creation).  libEGL.so,
// libGLESv2.so, and libwayland-egl.so are linked directly via #[link].

use anyhow::{Context, Result};
use std::ffi::c_void;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::sync::mpsc;

use crate::decode::sw::DecodedFrame;

// ── EGL constants ────────────────────────────────────────────────────────────

const EGL_FALSE: u32 = 0;
const EGL_NO_DISPLAY: *mut c_void = std::ptr::null_mut();
const EGL_NO_CONTEXT: *mut c_void = std::ptr::null_mut();
const EGL_NO_SURFACE: *mut c_void = std::ptr::null_mut();
const EGL_NONE: i32 = 0x3038;
const EGL_RED_SIZE: i32 = 0x3024;
const EGL_GREEN_SIZE: i32 = 0x3023;
const EGL_BLUE_SIZE: i32 = 0x3022;
const EGL_ALPHA_SIZE: i32 = 0x3021;
const EGL_SURFACE_TYPE: i32 = 0x3033;
const EGL_WINDOW_BIT: i32 = 0x0004;
const EGL_RENDERABLE_TYPE: i32 = 0x3040;
const EGL_OPENGL_ES2_BIT: i32 = 0x0004;
const EGL_CONTEXT_CLIENT_VERSION: i32 = 0x3098;
const EGL_OPENGL_ES_API: u32 = 0x30A0;

// ── GLES2 constants ───────────────────────────────────────────────────────────

const GL_TRIANGLES: u32 = 0x0004;
const GL_RGBA: u32 = 0x1908;
const GL_UNSIGNED_BYTE: u32 = 0x1401;
const GL_FLOAT: u32 = 0x1406;
const GL_FALSE_U8: u8 = 0;
const GL_ARRAY_BUFFER: u32 = 0x8892;
const GL_STATIC_DRAW: u32 = 0x88B4;
const GL_TEXTURE_2D: u32 = 0x0DE1;
const GL_TEXTURE0: u32 = 0x84C0;
const GL_TEXTURE_WRAP_S: u32 = 0x2802;
const GL_TEXTURE_WRAP_T: u32 = 0x2803;
const GL_TEXTURE_MIN_FILTER: u32 = 0x2801;
const GL_TEXTURE_MAG_FILTER: u32 = 0x2800;
const GL_CLAMP_TO_EDGE: i32 = 0x812F;
const GL_LINEAR: i32 = 0x2601;
const GL_VERTEX_SHADER: u32 = 0x8B31;
const GL_FRAGMENT_SHADER: u32 = 0x8B30;
const GL_LINK_STATUS: u32 = 0x8B82;
const GL_COMPILE_STATUS: u32 = 0x8B81;

// ── Raw FFI ───────────────────────────────────────────────────────────────────

#[allow(non_snake_case)]
#[link(name = "EGL")]
extern "C" {
    fn eglGetDisplay(display: *mut c_void) -> *mut c_void;
    fn eglInitialize(display: *mut c_void, major: *mut i32, minor: *mut i32) -> u32;
    fn eglChooseConfig(
        display: *mut c_void,
        attrib_list: *const i32,
        configs: *mut *mut c_void,
        config_size: i32,
        num_config: *mut i32,
    ) -> u32;
    fn eglBindAPI(api: u32) -> u32;
    fn eglCreateContext(
        display: *mut c_void,
        config: *mut c_void,
        share: *mut c_void,
        attrib_list: *const i32,
    ) -> *mut c_void;
    fn eglCreateWindowSurface(
        display: *mut c_void,
        config: *mut c_void,
        window: *mut c_void,
        attrib_list: *const i32,
    ) -> *mut c_void;
    fn eglMakeCurrent(
        display: *mut c_void,
        draw: *mut c_void,
        read: *mut c_void,
        ctx: *mut c_void,
    ) -> u32;
    fn eglSwapBuffers(display: *mut c_void, surface: *mut c_void) -> u32;
    fn eglDestroyContext(display: *mut c_void, ctx: *mut c_void) -> u32;
    fn eglDestroySurface(display: *mut c_void, surface: *mut c_void) -> u32;
    fn eglTerminate(display: *mut c_void) -> u32;
}

#[allow(non_snake_case)]
#[link(name = "GLESv2")]
extern "C" {
    fn glViewport(x: i32, y: i32, width: i32, height: i32);
    fn glGenTextures(n: i32, textures: *mut u32);
    fn glDeleteTextures(n: i32, textures: *const u32);
    fn glBindTexture(target: u32, texture: u32);
    fn glTexParameteri(target: u32, pname: u32, param: i32);
    fn glTexImage2D(
        target: u32,
        level: i32,
        internal_format: i32,
        width: i32,
        height: i32,
        border: i32,
        format: u32,
        type_: u32,
        data: *const c_void,
    );
    fn glCreateShader(type_: u32) -> u32;
    fn glShaderSource(
        shader: u32,
        count: i32,
        string: *const *const u8,
        length: *const i32,
    );
    fn glCompileShader(shader: u32);
    fn glGetShaderiv(shader: u32, pname: u32, params: *mut i32);
    fn glGetShaderInfoLog(
        shader: u32,
        buf_size: i32,
        length: *mut i32,
        info_log: *mut u8,
    );
    fn glCreateProgram() -> u32;
    fn glAttachShader(program: u32, shader: u32);
    fn glLinkProgram(program: u32);
    fn glGetProgramiv(program: u32, pname: u32, params: *mut i32);
    fn glGetProgramInfoLog(
        program: u32,
        buf_size: i32,
        length: *mut i32,
        info_log: *mut u8,
    );
    fn glUseProgram(program: u32);
    fn glDeleteShader(shader: u32);
    fn glDeleteProgram(program: u32);
    fn glGetAttribLocation(program: u32, name: *const u8) -> i32;
    fn glGetUniformLocation(program: u32, name: *const u8) -> i32;
    fn glUniform1i(location: i32, v0: i32);
    fn glEnableVertexAttribArray(index: u32);
    fn glDisableVertexAttribArray(index: u32);
    fn glVertexAttribPointer(
        index: u32,
        size: i32,
        type_: u32,
        normalized: u8,
        stride: i32,
        pointer: *const c_void,
    );
    fn glGenBuffers(n: i32, buffers: *mut u32);
    fn glDeleteBuffers(n: i32, buffers: *const u32);
    fn glBindBuffer(target: u32, buffer: u32);
    fn glBufferData(target: u32, size: isize, data: *const c_void, usage: u32);
    fn glDrawArrays(mode: u32, first: i32, count: i32);
    fn glActiveTexture(texture: u32);
}

#[allow(non_snake_case)]
#[link(name = "wayland-egl")]
extern "C" {
    fn wl_egl_window_create(surface: *mut c_void, width: i32, height: i32) -> *mut c_void;
    fn wl_egl_window_destroy(window: *mut c_void);
    fn wl_egl_window_resize(window: *mut c_void, width: i32, height: i32, dx: i32, dy: i32);
}

// ── Shaders ───────────────────────────────────────────────────────────────────

// GLSL 1.00 (GLES 2.0).
const VERT_SRC: &str = "
attribute vec2 a_pos;
attribute vec2 a_tex;
varying vec2 v_tex;
void main() {
    gl_Position = vec4(a_pos, 0.0, 1.0);
    v_tex = a_tex;
}
";

// Frame data is BGRA; uploaded as GL_RGBA so the channels arrive as
//   texture.r = B,  texture.g = G,  texture.b = R,  texture.a = A
// Swizzle c.bgra maps: out.r = tex.b = R, out.g = tex.g = G, out.b = tex.r = B.
// Also: GL textures are bottom-row-first, frame data is top-row-first, so
// the QUAD below inverts v to produce an upright image.
const FRAG_SRC: &str = "
precision mediump float;
uniform sampler2D u_tex;
varying vec2 v_tex;
void main() {
    vec4 c = texture2D(u_tex, v_tex);
    gl_FragColor = c.bgra;
}
";

// Fullscreen quad (two CCW triangles, 4 floats per vertex: pos.xy + tex.uv).
// Texcoord v is 1 at the bottom and 0 at the top so that the image appears
// upright despite GL's bottom-up texture convention.
#[rustfmt::skip]
static QUAD: [f32; 24] = [
    //  pos.x  pos.y   tex.u  tex.v
    -1.0,  -1.0,    0.0,   1.0,   // screen BL  →  tex (0,1)
     1.0,  -1.0,    1.0,   1.0,   // screen BR  →  tex (1,1)
     1.0,   1.0,    1.0,   0.0,   // screen TR  →  tex (1,0)
    -1.0,  -1.0,    0.0,   1.0,
     1.0,   1.0,    1.0,   0.0,
    -1.0,   1.0,    0.0,   0.0,   // screen TL  →  tex (0,0)
];

// ── EglRenderer ──────────────────────────────────────────────────────────────

pub struct EglRenderer {
    // libwayland-egl window (wraps wl_surface for EGL)
    wl_egl_window: *mut c_void,
    // EGL opaque handles
    egl_display: *mut c_void,
    egl_surface: *mut c_void,
    egl_context: *mut c_void,
    // GL object IDs
    program: u32,
    vbo: u32,
    texture: u32,
    // Cached attribute / uniform locations (set once at init)
    a_pos: u32,
    a_tex: u32,
    u_tex: i32,
    // Current window size; updated on resize
    width: u32,
    height: u32,
    // Shared render counter (mirrors ShmRenderer's counter for observability)
    render_count: Arc<AtomicU64>,
}

// SAFETY: EGL/GL state lives on the display OS thread exclusively.
// The raw pointers are valid for the lifetime of EglRenderer.
unsafe impl Send for EglRenderer {}

impl EglRenderer {
    /// Initialise EGL + GLES2 and bind to the given Wayland objects.
    ///
    /// Both raw pointers must come from a `wayland-client` connection
    /// built with `features = ["system"]`:
    /// * `wl_display_ptr` — `conn.backend().display_ptr() as *mut c_void`
    /// * `wl_surface_ptr` — `surface.id().as_ptr() as *mut c_void`
    pub fn new(
        wl_display_ptr: *mut c_void,
        wl_surface_ptr: *mut c_void,
        width: u32,
        height: u32,
        render_count: Arc<AtomicU64>,
    ) -> Result<Self> {
        // ── EGL display ────────────────────────────────────────────────
        let egl_display = unsafe { eglGetDisplay(wl_display_ptr) };
        if egl_display == EGL_NO_DISPLAY {
            anyhow::bail!("eglGetDisplay returned EGL_NO_DISPLAY");
        }
        let ok = unsafe {
            eglInitialize(egl_display, std::ptr::null_mut(), std::ptr::null_mut())
        };
        if ok == EGL_FALSE {
            anyhow::bail!("eglInitialize failed");
        }

        // ── EGL config (RGBA8888, window surface, GLES2) ───────────────
        #[rustfmt::skip]
        let config_attribs: [i32; 13] = [
            EGL_RED_SIZE,       8,
            EGL_GREEN_SIZE,     8,
            EGL_BLUE_SIZE,      8,
            EGL_ALPHA_SIZE,     8,
            EGL_SURFACE_TYPE,   EGL_WINDOW_BIT,
            EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
            EGL_NONE,
        ];
        let mut config: *mut c_void = std::ptr::null_mut();
        let mut num_configs = 0i32;
        let ok = unsafe {
            eglChooseConfig(
                egl_display,
                config_attribs.as_ptr(),
                &mut config,
                1,
                &mut num_configs,
            )
        };
        if ok == EGL_FALSE || num_configs == 0 {
            anyhow::bail!("eglChooseConfig: no suitable RGBA8888 ES2 config");
        }

        let ok = unsafe { eglBindAPI(EGL_OPENGL_ES_API) };
        if ok == EGL_FALSE {
            anyhow::bail!("eglBindAPI(OPENGL_ES_API) failed");
        }

        // ── EGL context (GLES 2.0) ─────────────────────────────────────
        let ctx_attribs: [i32; 3] = [EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE];
        let egl_context = unsafe {
            eglCreateContext(egl_display, config, EGL_NO_CONTEXT, ctx_attribs.as_ptr())
        };
        if egl_context == EGL_NO_CONTEXT {
            anyhow::bail!("eglCreateContext failed");
        }

        // ── wl_egl_window + EGL surface ────────────────────────────────
        let wl_egl_window = unsafe {
            wl_egl_window_create(wl_surface_ptr, width as i32, height as i32)
        };
        if wl_egl_window.is_null() {
            anyhow::bail!("wl_egl_window_create failed");
        }

        let egl_surface = unsafe {
            eglCreateWindowSurface(egl_display, config, wl_egl_window, std::ptr::null())
        };
        if egl_surface == EGL_NO_SURFACE {
            unsafe { wl_egl_window_destroy(wl_egl_window) };
            anyhow::bail!("eglCreateWindowSurface failed");
        }

        let ok = unsafe {
            eglMakeCurrent(egl_display, egl_surface, egl_surface, egl_context)
        };
        if ok == EGL_FALSE {
            anyhow::bail!("eglMakeCurrent failed");
        }

        // ── GL resources ───────────────────────────────────────────────
        let program = compile_program().context("compile GL program")?;

        let mut texture = 0u32;
        let mut vbo = 0u32;
        unsafe {
            glGenTextures(1, &mut texture);
            glBindTexture(GL_TEXTURE_2D, texture);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);

            glGenBuffers(1, &mut vbo);
            glBindBuffer(GL_ARRAY_BUFFER, vbo);
            glBufferData(
                GL_ARRAY_BUFFER,
                (QUAD.len() * std::mem::size_of::<f32>()) as isize,
                QUAD.as_ptr() as *const c_void,
                GL_STATIC_DRAW,
            );
        }

        let a_pos =
            unsafe { glGetAttribLocation(program, b"a_pos\0".as_ptr()) as u32 };
        let a_tex =
            unsafe { glGetAttribLocation(program, b"a_tex\0".as_ptr()) as u32 };
        let u_tex =
            unsafe { glGetUniformLocation(program, b"u_tex\0".as_ptr()) };

        Ok(Self {
            wl_egl_window,
            egl_display,
            egl_surface,
            egl_context,
            program,
            vbo,
            texture,
            a_pos,
            a_tex,
            u_tex,
            width,
            height,
            render_count,
        })
    }

    /// Drain all pending decoded frames from `frame_rx`, rendering the
    /// latest one (earlier ones are dropped — same policy as ShmRenderer).
    pub fn drain_frames(
        &mut self,
        frame_rx: &mpsc::Receiver<DecodedFrame>,
    ) -> Result<usize> {
        let mut latest: Option<DecodedFrame> = None;
        let mut count = 0usize;
        loop {
            match frame_rx.try_recv() {
                Ok(frame) => {
                    count += 1;
                    latest = Some(frame);
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return Ok(count),
            }
        }
        if let Some(frame) = latest {
            self.render_frame(&frame).context("EGL render")?;
        }
        Ok(count)
    }

    /// Resize the EGL window to match a new compositor-assigned size.
    pub fn resize(&mut self, w: u32, h: u32) {
        if (w, h) == (self.width, self.height) {
            return;
        }
        self.width = w;
        self.height = h;
        unsafe { wl_egl_window_resize(self.wl_egl_window, w as i32, h as i32, 0, 0) };
    }

    fn render_frame(&mut self, frame: &DecodedFrame) -> Result<()> {
        unsafe {
            glViewport(0, 0, self.width as i32, self.height as i32);

            // Upload BGRA frame data as GL_RGBA (swizzle in fragment shader).
            glActiveTexture(GL_TEXTURE0);
            glBindTexture(GL_TEXTURE_2D, self.texture);
            glTexImage2D(
                GL_TEXTURE_2D,
                0,
                GL_RGBA as i32,
                frame.width as i32,
                frame.height as i32,
                0,
                GL_RGBA,
                GL_UNSIGNED_BYTE,
                frame.pixels.as_ptr() as *const c_void,
            );

            glUseProgram(self.program);
            glUniform1i(self.u_tex, 0);

            glBindBuffer(GL_ARRAY_BUFFER, self.vbo);
            let stride = (4 * std::mem::size_of::<f32>()) as i32;
            let tex_offset = (2 * std::mem::size_of::<f32>()) as *const c_void;

            glEnableVertexAttribArray(self.a_pos);
            glEnableVertexAttribArray(self.a_tex);
            glVertexAttribPointer(
                self.a_pos, 2, GL_FLOAT, GL_FALSE_U8, stride, std::ptr::null(),
            );
            glVertexAttribPointer(
                self.a_tex, 2, GL_FLOAT, GL_FALSE_U8, stride, tex_offset,
            );

            glDrawArrays(GL_TRIANGLES, 0, 6);

            glDisableVertexAttribArray(self.a_pos);
            glDisableVertexAttribArray(self.a_tex);
        }

        let ok = unsafe { eglSwapBuffers(self.egl_display, self.egl_surface) };
        if ok == EGL_FALSE {
            anyhow::bail!("eglSwapBuffers failed");
        }
        self.render_count.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

impl Drop for EglRenderer {
    fn drop(&mut self) {
        unsafe {
            glDeleteTextures(1, &self.texture);
            glDeleteBuffers(1, &self.vbo);
            glDeleteProgram(self.program);
            eglMakeCurrent(
                self.egl_display,
                EGL_NO_SURFACE,
                EGL_NO_SURFACE,
                EGL_NO_CONTEXT,
            );
            eglDestroySurface(self.egl_display, self.egl_surface);
            eglDestroyContext(self.egl_display, self.egl_context);
            eglTerminate(self.egl_display);
            wl_egl_window_destroy(self.wl_egl_window);
        }
    }
}

// ── Shader helpers ────────────────────────────────────────────────────────────

fn compile_program() -> Result<u32> {
    let vert = compile_shader(GL_VERTEX_SHADER, VERT_SRC).context("vertex shader")?;
    let frag =
        compile_shader(GL_FRAGMENT_SHADER, FRAG_SRC).context("fragment shader")?;
    let prog = unsafe { glCreateProgram() };
    unsafe {
        glAttachShader(prog, vert);
        glAttachShader(prog, frag);
        glLinkProgram(prog);
        glDeleteShader(vert);
        glDeleteShader(frag);
    }
    let mut status = 0i32;
    unsafe { glGetProgramiv(prog, GL_LINK_STATUS, &mut status) };
    if status == 0 {
        let mut len = 0i32;
        let mut buf = vec![0u8; 512];
        unsafe { glGetProgramInfoLog(prog, 512, &mut len, buf.as_mut_ptr()) };
        let msg = String::from_utf8_lossy(&buf[..len as usize]).into_owned();
        anyhow::bail!("GL program link failed: {msg}");
    }
    Ok(prog)
}

fn compile_shader(kind: u32, src: &str) -> Result<u32> {
    let shader = unsafe { glCreateShader(kind) };
    let src_ptr = src.as_ptr();
    let src_len = src.len() as i32;
    unsafe { glShaderSource(shader, 1, &src_ptr, &src_len) };
    unsafe { glCompileShader(shader) };
    let mut status = 0i32;
    unsafe { glGetShaderiv(shader, GL_COMPILE_STATUS, &mut status) };
    if status == 0 {
        let mut len = 0i32;
        let mut buf = vec![0u8; 512];
        unsafe { glGetShaderInfoLog(shader, 512, &mut len, buf.as_mut_ptr()) };
        let msg = String::from_utf8_lossy(&buf[..len as usize]).into_owned();
        anyhow::bail!("GL shader compile failed: {msg}");
    }
    Ok(shader)
}
