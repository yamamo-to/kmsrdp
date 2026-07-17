//! GBM/EGL readback for tiled (vendor-modifier) DRM/KMS framebuffers.
//!
//! Mirrors `reframe-server/rf-converter.c` upstream: the plane's dma-buf is
//! imported as an `EGLImage` via `EGL_EXT_image_dma_buf_import` (the fourcc +
//! modifier tell the driver how to detile it), bound as a
//! `GL_TEXTURE_EXTERNAL_OES` (required even for the Linear modifier on
//! NVIDIA - a plain `GL_TEXTURE_2D` sample silently returns garbage there),
//! drawn into an offscreen FBO, and read back with `glReadPixels`. No window
//! system or compositor cooperation involved - a `gbm_device` is only used
//! as an opaque handle so EGL can bind to the same GPU our DRM card fd
//! points at, exactly as Wayland compositors do.
//!
//! EGL/GLES/GBM are all dlopen'd at runtime (no link-time dependency, no
//! `-dev` package needed to build) since this is the same "read-only,
//! ask-for-nothing-we-don't-need" posture as the rest of `capture.rs`.

use std::ffi::{c_int, c_void};
use std::fs;
use std::io;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::Mutex;
use std::sync::OnceLock;

use drm_fourcc::{DrmFourcc, DrmModifier};
use khronos_egl as egl;
use libloading::{Library, Symbol};

// EGL_EXT_image_dma_buf_import / EGL_KHR_platform_gbm constants - stable
// values from the Khronos EGL registry, absent from khronos-egl's typed API
// because they're EXT/KHR extensions rather than core EGL.
const EGL_PLATFORM_GBM_KHR: egl::Enum = 0x31D7;
const EGL_LINUX_DMA_BUF_EXT: egl::Enum = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: egl::Attrib = 0x3271;
const EGL_DMA_BUF_PLANE0_FD_EXT: egl::Attrib = 0x3272;
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: egl::Attrib = 0x3273;
const EGL_DMA_BUF_PLANE0_PITCH_EXT: egl::Attrib = 0x3274;
const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: egl::Attrib = 0x3443;
const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: egl::Attrib = 0x3444;

// GLES2/3 core constants (from GLES2/gl2.h - stable since the ES2 spec).
const GL_FALSE: u32 = 0;
const GL_COLOR_BUFFER_BIT: u32 = 0x0000_4000;
const GL_TRIANGLE_STRIP: u32 = 0x0005;
const GL_FLOAT: u32 = 0x1406;
const GL_UNSIGNED_BYTE: u32 = 0x1401;
const GL_RGBA: u32 = 0x1908;
const GL_TEXTURE_2D: u32 = 0x0DE1;
const GL_TEXTURE0: u32 = 0x84C0;
const GL_TEXTURE_MIN_FILTER: u32 = 0x2801;
const GL_TEXTURE_MAG_FILTER: u32 = 0x2800;
const GL_TEXTURE_WRAP_S: u32 = 0x2802;
const GL_TEXTURE_WRAP_T: u32 = 0x2803;
const GL_NEAREST: i32 = 0x2600;
const GL_CLAMP_TO_EDGE: i32 = 0x812F;
const GL_ARRAY_BUFFER: u32 = 0x8892;
const GL_STATIC_DRAW: u32 = 0x88E4;
const GL_VERTEX_SHADER: u32 = 0x8B31;
const GL_FRAGMENT_SHADER: u32 = 0x8B30;
const GL_COMPILE_STATUS: u32 = 0x8B81;
const GL_LINK_STATUS: u32 = 0x8B82;
const GL_FRAMEBUFFER: u32 = 0x8D40;
const GL_COLOR_ATTACHMENT0: u32 = 0x8CE0;
const GL_FRAMEBUFFER_COMPLETE: u32 = 0x8CD5;
const GL_TEXTURE_EXTERNAL_OES: u32 = 0x8D65;

macro_rules! gl_fns {
    ($($field:ident : $name:literal => fn($($arg:ty),*) $(-> $ret:ty)?;)+) => {
        struct GlFns {
            $($field: unsafe extern "C" fn($($arg),*) $(-> $ret)?,)+
        }

        impl GlFns {
            unsafe fn load(lib: &Library) -> io::Result<Self> {
                $(
                    let $field = {
                        let sym: Symbol<unsafe extern "C" fn($($arg),*) $(-> $ret)?> =
                            unsafe { lib.get(concat!($name, "\0").as_bytes()) }
                                .map_err(|e| io::Error::other(format!("dlsym {}: {e}", $name)))?;
                        *sym
                    };
                )+
                Ok(GlFns { $($field,)+ })
            }
        }
    };
}

gl_fns! {
    get_error: "glGetError" => fn() -> u32;
    gen_textures: "glGenTextures" => fn(i32, *mut u32);
    delete_textures: "glDeleteTextures" => fn(i32, *const u32);
    bind_texture: "glBindTexture" => fn(u32, u32);
    tex_parameteri: "glTexParameteri" => fn(u32, u32, i32);
    tex_image_2d: "glTexImage2D" => fn(u32, i32, i32, i32, i32, i32, u32, u32, *const c_void);
    active_texture: "glActiveTexture" => fn(u32);
    gen_framebuffers: "glGenFramebuffers" => fn(i32, *mut u32);
    bind_framebuffer: "glBindFramebuffer" => fn(u32, u32);
    framebuffer_texture_2d: "glFramebufferTexture2D" => fn(u32, u32, u32, u32, i32);
    check_framebuffer_status: "glCheckFramebufferStatus" => fn(u32) -> u32;
    viewport: "glViewport" => fn(i32, i32, i32, i32);
    clear_color: "glClearColor" => fn(f32, f32, f32, f32);
    clear: "glClear" => fn(u32);
    create_shader: "glCreateShader" => fn(u32) -> u32;
    shader_source: "glShaderSource" => fn(u32, i32, *const *const i8, *const i32);
    compile_shader: "glCompileShader" => fn(u32);
    get_shaderiv: "glGetShaderiv" => fn(u32, u32, *mut i32);
    get_shader_info_log: "glGetShaderInfoLog" => fn(u32, i32, *mut i32, *mut i8);
    delete_shader: "glDeleteShader" => fn(u32);
    create_program: "glCreateProgram" => fn() -> u32;
    attach_shader: "glAttachShader" => fn(u32, u32);
    link_program: "glLinkProgram" => fn(u32);
    get_programiv: "glGetProgramiv" => fn(u32, u32, *mut i32);
    get_program_info_log: "glGetProgramInfoLog" => fn(u32, i32, *mut i32, *mut i8);
    delete_program: "glDeleteProgram" => fn(u32);
    use_program: "glUseProgram" => fn(u32);
    gen_buffers: "glGenBuffers" => fn(i32, *mut u32);
    bind_buffer: "glBindBuffer" => fn(u32, u32);
    buffer_data: "glBufferData" => fn(u32, isize, *const c_void, u32);
    vertex_attrib_pointer: "glVertexAttribPointer" => fn(u32, i32, u32, u8, i32, *const c_void);
    enable_vertex_attrib_array: "glEnableVertexAttribArray" => fn(u32);
    get_attrib_location: "glGetAttribLocation" => fn(u32, *const i8) -> i32;
    draw_arrays: "glDrawArrays" => fn(u32, i32, i32);
    read_pixels: "glReadPixels" => fn(i32, i32, i32, i32, u32, u32, *mut c_void);
    finish: "glFinish" => fn();
}

type EglInstance = egl::DynamicInstance<egl::EGL1_5>;
type EglImageTargetTexture2dOes = unsafe extern "system" fn(u32, *mut c_void);

struct GpuDetiler {
    _gles_lib: Library,
    _gbm_lib: Library,
    _gbm_device: *mut c_void,
    // gbm_create_device() does not dup its fd - it must stay open for the
    // device's whole lifetime, so it lives here rather than being dropped
    // once the device is created.
    _render_fd: fs::File,
    egl: EglInstance,
    display: egl::Display,
    context: egl::Context,
    gl: GlFns,
    image_target_texture_2d_oes: EglImageTargetTexture2dOes,
    program: u32,
    quad_vbo: u32,
    fbo: u32,
    color_tex: u32,
    width: u32,
    height: u32,
}

// Safety: only ever touched through the `Mutex` in `DETILER`, from whichever
// thread happens to be running the blocking capture task at the time - EGL
// contexts are fine to migrate between threads as long as at most one thread
// uses them at once, which the mutex guarantees.
unsafe impl Send for GpuDetiler {}

const VERTEX_SHADER: &str = "#version 300 es\n\
    in vec2 in_position;\n\
    in vec2 in_texcoord;\n\
    out vec2 pass_texcoord;\n\
    void main() {\n\
    \tgl_Position = vec4(in_position, 0.0, 1.0);\n\
    \tpass_texcoord = in_texcoord;\n\
    }\n";

// Swaps R/B on the way out so `glReadPixels(GL_RGBA)` hands back BGRX8888
// memory order directly - matches `PixelFormat::BgrX32` with no CPU-side
// per-pixel pass needed.
const FRAGMENT_SHADER: &str = "#version 300 es\n\
    #extension GL_OES_EGL_image_external_essl3 : require\n\
    precision highp float;\n\
    uniform samplerExternalOES image;\n\
    in vec2 pass_texcoord;\n\
    out vec4 out_color;\n\
    void main() {\n\
    \tvec4 c = texture(image, pass_texcoord);\n\
    \tout_color = vec4(c.b, c.g, c.r, c.a);\n\
    }\n";

fn render_node_for_card(card_path: &str) -> io::Result<String> {
    let card_name = card_path
        .rsplit('/')
        .next()
        .filter(|n| n.starts_with("card"))
        .ok_or_else(|| io::Error::other(format!("not a DRM card path: {card_path}")))?;

    let drm_dir = format!("/sys/class/drm/{card_name}/device/drm");
    for entry in fs::read_dir(&drm_dir)? {
        let name = entry?.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("renderD") {
            return Ok(format!("/dev/dri/{name}"));
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("no render node found for {card_path} (looked in {drm_dir})"),
    ))
}

unsafe fn gbm_create_device(lib: &Library, fd: RawFd) -> io::Result<*mut c_void> {
    let f: Symbol<unsafe extern "C" fn(c_int) -> *mut c_void> =
        unsafe { lib.get(b"gbm_create_device\0") }
            .map_err(|e| io::Error::other(format!("dlsym gbm_create_device: {e}")))?;
    let device = unsafe { f(fd) };
    if device.is_null() {
        return Err(io::Error::other("gbm_create_device failed"));
    }
    Ok(device)
}

fn compile_shader(gl: &GlFns, kind: u32, source: &str) -> io::Result<u32> {
    let shader = unsafe { (gl.create_shader)(kind) };
    if shader == 0 {
        return Err(io::Error::other("glCreateShader failed"));
    }
    let src_ptr = source.as_ptr() as *const i8;
    let src_len = source.len() as i32;
    unsafe { (gl.shader_source)(shader, 1, &src_ptr, &src_len) };
    unsafe { (gl.compile_shader)(shader) };

    let mut compiled = 0i32;
    unsafe { (gl.get_shaderiv)(shader, GL_COMPILE_STATUS, &mut compiled) };
    if compiled == GL_FALSE as i32 {
        let mut log = [0u8; 1024];
        let mut len = 0i32;
        unsafe {
            (gl.get_shader_info_log)(
                shader,
                log.len() as i32,
                &mut len,
                log.as_mut_ptr() as *mut i8,
            )
        };
        unsafe { (gl.delete_shader)(shader) };
        return Err(io::Error::other(format!(
            "shader compile failed: {}",
            String::from_utf8_lossy(&log[..len.max(0) as usize])
        )));
    }
    Ok(shader)
}

fn link_program(gl: &GlFns, vs: u32, fs: u32) -> io::Result<u32> {
    let program = unsafe { (gl.create_program)() };
    if program == 0 {
        return Err(io::Error::other("glCreateProgram failed"));
    }
    unsafe {
        (gl.attach_shader)(program, vs);
        (gl.attach_shader)(program, fs);
        (gl.link_program)(program);
    }
    let mut linked = 0i32;
    unsafe { (gl.get_programiv)(program, GL_LINK_STATUS, &mut linked) };
    if linked == GL_FALSE as i32 {
        let mut log = [0u8; 1024];
        let mut len = 0i32;
        unsafe {
            (gl.get_program_info_log)(
                program,
                log.len() as i32,
                &mut len,
                log.as_mut_ptr() as *mut i8,
            )
        };
        unsafe { (gl.delete_program)(program) };
        return Err(io::Error::other(format!(
            "program link failed: {}",
            String::from_utf8_lossy(&log[..len.max(0) as usize])
        )));
    }
    Ok(program)
}

impl GpuDetiler {
    fn new(card_path: &str) -> io::Result<Self> {
        let render_node = render_node_for_card(card_path)?;
        let render_fd = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&render_node)?;

        let gbm_lib = unsafe { Library::new("libgbm.so.1") }
            .or_else(|_| unsafe { Library::new("libgbm.so") })
            .map_err(|e| io::Error::other(format!("failed to load libgbm: {e}")))?;
        let gbm_device = unsafe { gbm_create_device(&gbm_lib, render_fd.as_raw_fd()) }?;

        let egl: EglInstance = unsafe { egl::DynamicInstance::<egl::EGL1_5>::load_required() }
            .map_err(|e| io::Error::other(format!("failed to load libEGL: {e}")))?;

        let display = unsafe {
            egl.get_platform_display(EGL_PLATFORM_GBM_KHR, gbm_device, &[egl::ATTRIB_NONE])
        }
        .map_err(|e| io::Error::other(format!("eglGetPlatformDisplay failed: {e:?}")))?;
        egl.initialize(display)
            .map_err(|e| io::Error::other(format!("eglInitialize failed: {e:?}")))?;
        egl.bind_api(egl::OPENGL_ES_API)
            .map_err(|e| io::Error::other(format!("eglBindAPI failed: {e:?}")))?;

        let config_attribs = [
            egl::SURFACE_TYPE,
            egl::PBUFFER_BIT,
            egl::RENDERABLE_TYPE,
            egl::OPENGL_ES3_BIT,
            egl::RED_SIZE,
            8,
            egl::GREEN_SIZE,
            8,
            egl::BLUE_SIZE,
            8,
            egl::ALPHA_SIZE,
            8,
            egl::NONE,
        ];
        let config = egl
            .choose_first_config(display, &config_attribs)
            .map_err(|e| io::Error::other(format!("eglChooseConfig failed: {e:?}")))?
            .ok_or_else(|| io::Error::other("no matching EGL config"))?;

        let context_attribs = [egl::CONTEXT_MAJOR_VERSION, 3, egl::NONE];
        let context = egl
            .create_context(display, config, None, &context_attribs)
            .map_err(|e| io::Error::other(format!("eglCreateContext failed: {e:?}")))?;
        egl.make_current(display, None, None, Some(context))
            .map_err(|e| io::Error::other(format!("eglMakeCurrent failed: {e:?}")))?;

        let gles_lib = unsafe { Library::new("libGLESv2.so.2") }
            .or_else(|_| unsafe { Library::new("libGLESv2.so") })
            .map_err(|e| io::Error::other(format!("failed to load libGLESv2: {e}")))?;
        let gl = unsafe { GlFns::load(&gles_lib)? };

        let image_target_texture_2d_oes = egl
            .get_proc_address("glEGLImageTargetTexture2DOES")
            .ok_or_else(|| io::Error::other("glEGLImageTargetTexture2DOES not available"))?;
        // Safety: looked up by name via eglGetProcAddress, matches the
        // known GL_OES_EGL_image_external signature.
        let image_target_texture_2d_oes: EglImageTargetTexture2dOes =
            unsafe { std::mem::transmute(image_target_texture_2d_oes) };

        let vs = compile_shader(&gl, GL_VERTEX_SHADER, VERTEX_SHADER)?;
        let fs = compile_shader(&gl, GL_FRAGMENT_SHADER, FRAGMENT_SHADER)?;
        let program = link_program(&gl, vs, fs)?;
        unsafe {
            (gl.delete_shader)(vs);
            (gl.delete_shader)(fs);
        }

        // Full-screen triangle strip covering NDC, texture V flipped since
        // the external OES image's (0,0) is the top-left texel (matching
        // the DRM framebuffer's memory layout) while our NDC Y grows
        // upward - without the flip the captured frame comes out upside
        // down.
        #[rustfmt::skip]
        let quad: [f32; 16] = [
            -1.0, -1.0,  0.0, 1.0,
             1.0, -1.0,  1.0, 1.0,
            -1.0,  1.0,  0.0, 0.0,
             1.0,  1.0,  1.0, 0.0,
        ];
        let mut quad_vbo = 0u32;
        unsafe {
            (gl.gen_buffers)(1, &mut quad_vbo);
            (gl.bind_buffer)(GL_ARRAY_BUFFER, quad_vbo);
            (gl.buffer_data)(
                GL_ARRAY_BUFFER,
                std::mem::size_of_val(&quad) as isize,
                quad.as_ptr() as *const c_void,
                GL_STATIC_DRAW,
            );
        }

        let mut fbo = 0u32;
        unsafe { (gl.gen_framebuffers)(1, &mut fbo) };

        Ok(GpuDetiler {
            _gles_lib: gles_lib,
            _gbm_lib: gbm_lib,
            _gbm_device: gbm_device,
            _render_fd: render_fd,
            egl,
            display,
            context,
            gl,
            image_target_texture_2d_oes,
            program,
            quad_vbo,
            fbo,
            color_tex: 0,
            width: 0,
            height: 0,
        })
    }

    fn ensure_current(&self) -> io::Result<()> {
        self.egl
            .make_current(self.display, None, None, Some(self.context))
            .map_err(|e| io::Error::other(format!("eglMakeCurrent failed: {e:?}")))
    }

    fn resize(&mut self, width: u32, height: u32) {
        if self.width == width && self.height == height && self.color_tex != 0 {
            return;
        }
        let gl = &self.gl;
        if self.color_tex != 0 {
            unsafe { (gl.delete_textures)(1, &self.color_tex) };
        }
        let mut color_tex = 0u32;
        unsafe {
            (gl.gen_textures)(1, &mut color_tex);
            (gl.bind_texture)(GL_TEXTURE_2D, color_tex);
            (gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
            (gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
            (gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
            (gl.tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
            (gl.tex_image_2d)(
                GL_TEXTURE_2D,
                0,
                GL_RGBA as i32,
                width as i32,
                height as i32,
                0,
                GL_RGBA,
                GL_UNSIGNED_BYTE,
                std::ptr::null(),
            );
            (gl.bind_texture)(GL_TEXTURE_2D, 0);
        }
        self.color_tex = color_tex;
        self.width = width;
        self.height = height;
    }

    /// Import `fd` (a plane's dma-buf, single-plane RGB only) as an
    /// `EGLImage`, draw it full-frame into the offscreen FBO, and read the
    /// result back as tightly-packed BGRX8888.
    #[allow(clippy::too_many_arguments)] // Parameters describe one DRM framebuffer plane.
    fn detile(
        &mut self,
        fd: RawFd,
        fourcc: DrmFourcc,
        modifier: DrmModifier,
        width: u32,
        height: u32,
        offset: u32,
        pitch: u32,
    ) -> io::Result<Vec<u8>> {
        self.ensure_current()?;
        self.resize(width, height);

        let modifier: u64 = modifier.into();
        let image_attribs: [egl::Attrib; 17] = [
            egl::WIDTH as egl::Attrib,
            width as egl::Attrib,
            egl::HEIGHT as egl::Attrib,
            height as egl::Attrib,
            EGL_LINUX_DRM_FOURCC_EXT,
            fourcc as egl::Attrib,
            EGL_DMA_BUF_PLANE0_FD_EXT,
            fd as egl::Attrib,
            EGL_DMA_BUF_PLANE0_OFFSET_EXT,
            offset as egl::Attrib,
            EGL_DMA_BUF_PLANE0_PITCH_EXT,
            pitch as egl::Attrib,
            EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,
            (modifier & 0xFFFF_FFFF) as egl::Attrib,
            EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,
            ((modifier >> 32) & 0xFFFF_FFFF) as egl::Attrib,
            egl::ATTRIB_NONE,
        ];

        let no_context = unsafe { egl::Context::from_ptr(egl::NO_CONTEXT) };
        let no_client_buffer = unsafe { egl::ClientBuffer::from_ptr(std::ptr::null_mut()) };
        let image = self
            .egl
            .create_image(
                self.display,
                no_context,
                EGL_LINUX_DMA_BUF_EXT,
                no_client_buffer,
                &image_attribs,
            )
            .map_err(|e| io::Error::other(format!("eglCreateImage failed: {e:?}")))?;

        let result = self.draw_and_read(image);

        let _ = self.egl.destroy_image(self.display, image);
        result
    }

    fn draw_and_read(&self, image: egl::Image) -> io::Result<Vec<u8>> {
        let gl = &self.gl;
        let mut texture = 0u32;
        unsafe {
            (gl.gen_textures)(1, &mut texture);
            (gl.active_texture)(GL_TEXTURE0);
            (gl.bind_texture)(GL_TEXTURE_EXTERNAL_OES, texture);
            (gl.tex_parameteri)(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
            (gl.tex_parameteri)(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
            (self.image_target_texture_2d_oes)(GL_TEXTURE_EXTERNAL_OES, image.as_ptr());

            (gl.bind_framebuffer)(GL_FRAMEBUFFER, self.fbo);
            (gl.framebuffer_texture_2d)(
                GL_FRAMEBUFFER,
                GL_COLOR_ATTACHMENT0,
                GL_TEXTURE_2D,
                self.color_tex,
                0,
            );
            let status = (gl.check_framebuffer_status)(GL_FRAMEBUFFER);
            if status != GL_FRAMEBUFFER_COMPLETE {
                (gl.delete_textures)(1, &texture);
                return Err(io::Error::other(format!("FBO incomplete: {status:#x}")));
            }

            (gl.viewport)(0, 0, self.width as i32, self.height as i32);
            (gl.clear_color)(0.0, 0.0, 0.0, 1.0);
            (gl.clear)(GL_COLOR_BUFFER_BIT);

            (gl.use_program)(self.program);
            (gl.bind_buffer)(GL_ARRAY_BUFFER, self.quad_vbo);
            let pos_loc = (gl.get_attrib_location)(self.program, c"in_position".as_ptr());
            let tex_loc = (gl.get_attrib_location)(self.program, c"in_texcoord".as_ptr());
            let stride = 4 * std::mem::size_of::<f32>() as i32;
            (gl.vertex_attrib_pointer)(
                pos_loc as u32,
                2,
                GL_FLOAT,
                GL_FALSE as u8,
                stride,
                std::ptr::null(),
            );
            (gl.enable_vertex_attrib_array)(pos_loc as u32);
            (gl.vertex_attrib_pointer)(
                tex_loc as u32,
                2,
                GL_FLOAT,
                GL_FALSE as u8,
                stride,
                (2 * std::mem::size_of::<f32>()) as *const c_void,
            );
            (gl.enable_vertex_attrib_array)(tex_loc as u32);

            (gl.draw_arrays)(GL_TRIANGLE_STRIP, 0, 4);

            let mut out = vec![0u8; self.width as usize * self.height as usize * 4];
            (gl.read_pixels)(
                0,
                0,
                self.width as i32,
                self.height as i32,
                GL_RGBA,
                GL_UNSIGNED_BYTE,
                out.as_mut_ptr() as *mut c_void,
            );
            (gl.finish)();

            let err = (gl.get_error)();
            (gl.bind_framebuffer)(GL_FRAMEBUFFER, 0);
            (gl.bind_texture)(GL_TEXTURE_EXTERNAL_OES, 0);
            (gl.delete_textures)(1, &texture);

            if err != 0 {
                return Err(io::Error::other(format!("GL error {err:#x} during detile")));
            }
            Ok(out)
        }
    }
}

static DETILER: OnceLock<Mutex<io::Result<GpuDetiler>>> = OnceLock::new();

/// Detile a single-plane RGB(A) dma-buf via GBM/EGL and return tightly
/// packed BGRX8888 bytes (stride == `width * 4`).
///
/// Only single-plane formats are supported (`XRGB8888`/`ARGB8888` with a
/// vendor modifier) - multi-plane YUV framebuffers aren't handled since
/// nothing upstream of this in `capture.rs` produces or expects them.
#[allow(clippy::too_many_arguments)] // Public boundary accepts the DRM plane metadata directly.
pub fn detile_to_bgrx(
    card_path: &str,
    fd: RawFd,
    fourcc: DrmFourcc,
    modifier: DrmModifier,
    width: u32,
    height: u32,
    offset: u32,
    pitch: u32,
) -> io::Result<Vec<u8>> {
    let cell = DETILER.get_or_init(|| Mutex::new(GpuDetiler::new(card_path)));
    let mut guard = cell.lock().unwrap_or_else(|e| e.into_inner());
    let detiler = guard
        .as_mut()
        .map_err(|e| io::Error::other(format!("GPU detiler init failed: {e}")))?;
    detiler.detile(fd, fourcc, modifier, width, height, offset, pitch)
}
