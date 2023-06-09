//! Outputting those images

use anyhow::anyhow;
use glutin::{
    api::egl::{context::NotCurrentContext, display::Display, surface::Surface},
    config::{Api, ConfigTemplateBuilder, GlConfig},
    context::{AsRawContext, ContextApi, ContextAttributesBuilder, RawContext, Version},
    display::{AsRawDisplay, GlDisplay, RawDisplay},
    prelude::{NotCurrentGlContextSurfaceAccessor, PossiblyCurrentGlContext},
    surface::{GlSurface, SurfaceAttributesBuilder, WindowSurface},
};
use gstreamer::{
    prelude::Cast,
    traits::{ElementExt, GstBinExt},
    Context, Element, ElementFactory, FlowError, FlowSuccess, Message, MessageView, Pipeline,
};
use gstreamer_app::{AppSink, AppSinkCallbacks};
use gstreamer_gl::{
    gst_video::{VideoCapsBuilder, VideoFormat, VideoFrame, VideoInfo},
    prelude::{ContextGLExt, VideoFrameGLExt},
    traits::GLContextExt,
    GLBaseMemory, GLContext, GLPlatform, GLSyncMeta, CAPS_FEATURE_MEMORY_GL_MEMORY, GLAPI,
    GL_DISPLAY_CONTEXT_TYPE,
};
use gstreamer_gl_egl::GLDisplayEGL;
use std::{mem, num::NonZeroU32, sync::mpsc};
use tracing::{debug, info, warn};

/// Logic to create DRM & GBM devices to get a raw window handle
mod window {
    use anyhow::anyhow;
    use drm::control::{self, connector, crtc, Device, Event, Mode, ModeTypeFlags, PageFlipFlags};
    use gbm::{AsRaw, BufferObjectFlags};
    use raw_window_handle::{GbmDisplayHandle, GbmWindowHandle, RawDisplayHandle, RawWindowHandle};
    use std::{
        fs::{File, OpenOptions},
        io,
        os::fd::{AsFd, BorrowedFd},
    };
    use tracing::{debug, warn};

    #[derive(Debug)]
    /// A simple wrapper for a device node.
    pub struct Card(File);

    /// Implementing [`AsFd`] is a prerequisite to implementing the traits found
    /// in this crate. Here, we are just calling [`File::as_fd()`] on the inner
    /// [`File`].
    impl AsFd for Card {
        fn as_fd(&self) -> BorrowedFd<'_> {
            self.0.as_fd()
        }
    }

    /// With [`AsFd`] implemented, we can now implement [`drm::Device`].
    impl drm::Device for Card {}

    impl control::Device for Card {}

    impl Card {
        /// Simple helper method for opening a [`Card`].
        fn open() -> io::Result<Self> {
            let mut options = OpenOptions::new();
            options.read(true);
            options.write(true);

            // The normal location of the primary device node on Linux
            Ok(Card(options.open("/dev/dri/card0")?))
        }
    }

    pub struct GbmContext {
        dev: gbm::Device<Card>,
        conn: connector::Handle,
        crtc: crtc::Handle,
        pub mode: Mode,
        pub surface: gbm::Surface<()>,
    }

    impl GbmContext {
        pub fn create() -> Result<Self, anyhow::Error> {
            let card = Card::open()?;

            // select a connector based on connectedness and find the right mode
            let (conn_info, mode) = card
            .resource_handles()?
            .connectors
            .into_iter()
            .filter_map(|conn_handle| {
                let info = card
                    .get_connector(conn_handle, true)
                    .map_err(|err| {
                        warn!(?err, "failed to get crtc");
                        err
                    })
                    .ok()?;
                debug!(interface = ?info.interface(), size = ?info.size(), state = ?info.state(), "saw connector");
                Some(info).filter(|c| c.state() == connector::State::Connected)
            })
            .filter_map(|info| {
                let mode = *info.modes().iter()
                    .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED)).or_else(|| info.modes().iter().max_by(|a, b| a.size().cmp(&b.size())))?;
                Some((info, mode))
            }).next()
            .ok_or_else(|| anyhow!("no connected connector found"))?;
            {
                let name = mode.name().to_str()?;
                let size = mode.size();
                let mode_type = mode.mode_type();
                let flags = mode.flags();
                debug!(%name, ?size, ?mode_type, ?flags, "using mode");
            }

            // get handles
            let enc = conn_info
                .current_encoder()
                .ok_or_else(|| anyhow!("no current encoder"))?;
            let crtc = card
                .get_encoder(enc)?
                .crtc()
                .ok_or_else(|| anyhow!("no crtc on encoder"))?;

            // create GBM device
            let gbm = gbm::Device::new(card)?;
            let (width, height) = mode.size();
            let surface = gbm.create_surface::<()>(
                width as _,
                height as _,
                gbm::Format::Xrgb8888,
                BufferObjectFlags::SCANOUT.union(BufferObjectFlags::RENDERING),
            )?;

            debug!("created GBM surface");

            Ok(GbmContext {
                dev: gbm,
                conn: conn_info.handle(),
                crtc,
                mode,
                surface,
            })
        }

        pub fn raw_display(&self) -> RawDisplayHandle {
            let mut display_handle = GbmDisplayHandle::empty();
            display_handle.gbm_device = self.dev.as_raw() as _;
            display_handle.into()
        }

        pub fn raw_window(&self) -> RawWindowHandle {
            let mut window_handle = GbmWindowHandle::empty();
            window_handle.gbm_surface = self.surface.as_raw() as _;
            window_handle.into()
        }

        /// outputs the current buffer to the hardware
        /// does not use double buffering, might be glitchy
        pub fn scanout_hard(&self) -> Result<(), anyhow::Error> {
            let bo = unsafe { self.surface.lock_front_buffer() }?;
            let fb = self.dev.add_framebuffer(&bo, 24, 32)?;

            self.dev
                .set_crtc(self.crtc, Some(fb), (0, 0), &[self.conn], Some(self.mode))?;

            Ok(())
        }

        pub fn scanout_pageflip(&self) -> Result<(), anyhow::Error> {
            let bo = unsafe { self.surface.lock_front_buffer() }?;
            let fb = self.dev.add_framebuffer(&bo, 24, 32)?;

            self.dev
                .page_flip(self.crtc, fb, PageFlipFlags::EVENT, None)?;
            // wait for the flip to happen
            self.dev
                .receive_events()?
                .find(|ev| matches!(ev, Event::PageFlip(_)))
                .ok_or_else(|| anyhow!("no pageflip happened before stream ended"))?;
            Ok(())
        }
    }
}

pub struct DisplaySetup {
    context: NotCurrentContext,
    surface: Surface<WindowSurface>,
    gbm: window::GbmContext,

    gst_context: GLContext,
    gst_sink: Element,

    renderer: Renderer,
    frame_rx: mpsc::Receiver<(VideoInfo, gstreamer::Buffer)>,
}

struct Renderer {
    gl: glow::Context,
    program: glow::Program,
    attr_position: u32,
    attr_texture: u32,
    vao: glow::VertexArray,
    vertex_buffer: glow::Buffer,
    vbo_indices: glow::Buffer,
}

pub struct ContextProvider {
    context: GLContext,
    display: GLDisplayEGL,
}

impl DisplaySetup {
    pub fn create(pipeline: &Pipeline) -> Result<(Self, ContextProvider), anyhow::Error> {
        // create DRM / GBM stuff
        let gbm_ctx = window::GbmContext::create()?;
        let raw_display_handle = gbm_ctx.raw_display();

        let tpl = ConfigTemplateBuilder::default()
            .with_api(Api::GLES2)
            .build();
        let dpl = unsafe { Display::new(raw_display_handle) }?;
        debug!(display = ?dpl, "got EGL display");
        let cfgs = unsafe { dpl.find_configs(tpl) }?;
        let cfg = cfgs
            .inspect(|cfg| {
                let samples = cfg.num_samples();
                let hw_accel = cfg.hardware_accelerated();
                let color_buf_type = cfg.color_buffer_type();
                let surface_types = cfg.config_surface_types();
                let api = cfg.api();
                debug!(
                    ?samples,
                    ?hw_accel,
                    ?color_buf_type,
                    ?surface_types,
                    ?api,
                    "found display config"
                )
            })
            .max_by(|a, b| a.num_samples().cmp(&b.num_samples()))
            .ok_or_else(|| anyhow!("no display config found"))?;

        debug!(display = ?dpl, "got display");
        let display = dpl;

        debug!("creating window surface");
        let (width, height) = gbm_ctx.mode.size();
        let (width, height) = (
            NonZeroU32::new(width as _).unwrap(),
            NonZeroU32::new(height as _).unwrap(),
        );
        let surface_attrs = SurfaceAttributesBuilder::<WindowSurface>::new().build(
            gbm_ctx.raw_window(),
            width,
            height,
        );
        let surface = unsafe { display.create_window_surface(&cfg, &surface_attrs) }?;

        let context_attributes = ContextAttributesBuilder::new()
            .with_debug(true)
            .with_context_api(ContextApi::Gles(Some(Version::new(2, 0))))
            .build(None);
        let ctx = unsafe { display.create_context(&cfg, &context_attributes) }?;

        let ctx = ctx.make_current(&surface)?;

        // load OpenGL function pointers
        let gl = unsafe {
            glow::Context::from_loader_function_cstr(|sym| display.get_proc_address(sym))
        };

        let gst_display = unsafe {
            let RawDisplay::Egl(raw) = display.raw_display();
            GLDisplayEGL::with_egl_display(raw as _)
        }?;

        let RawContext::Egl(ctx_raw) = ctx.raw_context();
        let gst_gl_api = {
            let mut res = GLAPI::empty();
            let a = cfg.api();
            if a.contains(Api::OPENGL) {
                res.insert(GLAPI::OPENGL);
            }
            if a.contains(Api::GLES1) {
                res.insert(GLAPI::GLES1);
            }
            if a.contains(Api::GLES2) {
                res.insert(GLAPI::GLES2);
            }
            res
        };
        let gl_context = unsafe {
            GLContext::new_wrapped(&gst_display, ctx_raw as _, GLPlatform::EGL, gst_gl_api)
        }
        .ok_or_else(|| anyhow!("failed to wrap context"))?;
        gl_context.activate(true)?;
        gl_context.fill_info()?;

        let renderer = unsafe { Renderer::init_scene(gl) }
            .map_err(|e| anyhow!("failed to create OpenGL setup: {}", e))?;

        surface.swap_buffers(&ctx)?;
        gbm_ctx.scanout_hard()?;

        let ctx = ctx.make_not_current()?;

        let caps = VideoCapsBuilder::new()
            .features([CAPS_FEATURE_MEMORY_GL_MEMORY])
            .format(VideoFormat::Rgba)
            .field("texture-target", "2D")
            .build();

        let appsink = AppSink::builder()
            .enable_last_sample(true)
            .max_buffers(1)
            .caps(&caps)
            .build();

        let (frame_tx, frame_rx) = mpsc::channel();
        appsink.set_callbacks(
            AppSinkCallbacks::builder()
                .new_sample(move |appsink| {
                    let sample = appsink.pull_sample().map_err(|err| {
                        debug!(?err, "failed to pull sample");
                        FlowError::Eos
                    })?;

                    let info = sample
                        .caps()
                        .ok_or_else(|| anyhow!("failed to get caps"))
                        .and_then(|caps| VideoInfo::from_caps(caps).map_err(Into::into))
                        .map_err(|err| {
                            warn!(?err, "Failed to get video info from sample");
                            FlowError::NotNegotiated
                        })?;

                    let mut buffer = sample.buffer_owned().unwrap();
                    {
                        let context = (buffer.n_memory() > 0)
                            .then(|| buffer.peek_memory(0))
                            .and_then(|m| m.downcast_memory_ref::<GLBaseMemory>())
                            .map(|m| m.context())
                            .ok_or_else(|| {
                                warn!("Failed to get GL context from buffer");
                                FlowError::Error
                            })?
                            .clone(); // so `buffer.make_mut` below works

                        if let Some(meta) = buffer.meta::<GLSyncMeta>() {
                            meta.set_sync_point(&context);
                        } else {
                            let buffer = buffer.make_mut();
                            let meta = GLSyncMeta::add(buffer, &context);
                            meta.set_sync_point(&context);
                        }
                    }

                    frame_tx
                        .send((info, buffer))
                        .map(|_| FlowSuccess::Ok)
                        .map_err(|_| FlowError::Error)
                })
                .build(),
        );

        let sink = ElementFactory::make("glsinkbin")
            .property("sink", &appsink)
            .build()?;
        pipeline.add(&sink)?;

        Ok((
            DisplaySetup {
                context: ctx,
                surface,
                gbm: gbm_ctx,
                gst_sink: sink,
                gst_context: gl_context.clone(),
                renderer,
                frame_rx,
            },
            ContextProvider::new(gl_context, gst_display),
        ))
    }

    pub fn sink(&self) -> &Element {
        &self.gst_sink
    }

    pub fn size(&self) -> (u16, u16) {
        self.gbm.mode.size()
    }

    pub fn main_loop(mut self) -> Result<(), anyhow::Error> {
        loop {
            let (info, buffer) = self.frame_rx.recv()?;
            let frame = VideoFrame::from_buffer_readable_gl(buffer, &info)
                .map_err(|_| anyhow!("failed to read video frame from buffer"))?;
            let sync_meta = frame.buffer().meta::<GLSyncMeta>().unwrap();
            sync_meta.wait(&self.gst_context);
            self.context = {
                let ctx = self.context.make_current(&self.surface)?;
                if let Some(texture) = frame.texture_id(0) {
                    unsafe { self.renderer.draw_frame(texture) };
                }
                self.surface.swap_buffers(&ctx)?;
                self.gbm.scanout_pageflip()?;
                ctx.make_not_current()?
            };
        }
    }
}

impl ContextProvider {
    fn new(gst_context: GLContext, gst_display: GLDisplayEGL) -> Self {
        Self {
            context: gst_context,
            display: gst_display,
        }
    }

    pub fn bus_sync_handler(&self, msg: &Message) {
        if let MessageView::NeedContext(ctxt) = msg.view() {
            let context_type = ctxt.context_type();
            debug!(?context_type, src = ?msg.src(), "context request");
            if context_type == *GL_DISPLAY_CONTEXT_TYPE {
                if let Some(el) = msg.src().map(|s| s.downcast_ref::<Element>().unwrap()) {
                    debug!(?el, "handing glDisplay context to element");
                    let context = Context::new(context_type, true);
                    context.set_gl_display(&self.display);
                    el.set_context(&context);
                }
            }
            if context_type == "gst.gl.app_context" {
                if let Some(el) = msg.src().map(|s| s.downcast_ref::<Element>().unwrap()) {
                    debug!(?el, "handing app context to element");
                    let mut context = Context::new(context_type, true);
                    {
                        let context = context.get_mut().unwrap();
                        let s = context.structure_mut();
                        s.set("context", &self.context);
                    }
                    el.set_context(&context);
                }
            }
        }
    }
}

impl Renderer {
    #[rustfmt::skip]
    const VERTICES: &[f32] = &[
        1.0,  1.0, 0.0, 1.0, 0.0,
        -1.0,  1.0, 0.0, 0.0, 0.0,
        -1.0, -1.0, 0.0, 0.0, 1.0,
        1.0, -1.0, 0.0, 1.0, 1.0,
    ];

    const INDICES: &[u16] = &[0, 1, 2, 0, 2, 3];

    #[rustfmt::skip]
    pub const IDENTITY: &[f32] = &[
        1.0, 0.0, 0.0, 0.0,
        0.0, 1.0, 0.0, 0.0,
        0.0, 0.0, 1.0, 0.0,
        0.0, 0.0, 0.0, 1.0,
    ];

    pub const VS_SRC: &str = "
uniform mat4 u_transformation;
attribute vec4 a_position;
attribute vec2 a_texcoord;
varying vec2 v_texcoord;

void main() {
    gl_Position = u_transformation * a_position;
    v_texcoord = a_texcoord;
}
\0";

    pub const FS_SRC: &str = "
#ifdef GL_ES
precision mediump float;
#endif
varying vec2 v_texcoord;
uniform sampler2D tex;

void main() {
    gl_FragColor = texture2D(tex, v_texcoord);
}
\0";

    unsafe fn init_scene(gl: glow::Context) -> Result<Self, String> {
        use glow::*;

        let (renderer, version) = (
            gl.get_parameter_string(glow::RENDERER),
            gl.get_parameter_string(glow::VERSION),
        );
        info!(%renderer, %version, "OpenGL info");

        let vs = gl.create_shader(VERTEX_SHADER)?;
        gl.shader_source(vs, Self::VS_SRC);
        gl.compile_shader(vs);

        let fs = gl.create_shader(FRAGMENT_SHADER)?;
        gl.shader_source(fs, Self::FS_SRC);
        gl.compile_shader(fs);

        let program = gl.create_program()?;
        gl.attach_shader(program, vs);
        gl.attach_shader(program, fs);
        gl.link_program(program);

        gl.get_program_link_status(program)
            .then_some(())
            .ok_or_else(|| "failed to link program".to_string())?;

        let attr_position = gl.get_attrib_location(program, "a_position").unwrap();
        let attr_texture = gl.get_attrib_location(program, "a_texcoord").unwrap();

        let vao = gl.create_vertex_array()?;

        let vertex_buffer = gl.create_buffer()?;
        gl.bind_buffer(ARRAY_BUFFER, Some(vertex_buffer));
        gl.buffer_data_u8_slice(ARRAY_BUFFER, as_byte_slice(Self::VERTICES), STATIC_DRAW);

        let vbo_indices = gl.create_buffer()?;
        gl.bind_buffer(ELEMENT_ARRAY_BUFFER, Some(vbo_indices));
        gl.buffer_data_u8_slice(
            ELEMENT_ARRAY_BUFFER,
            as_byte_slice(Self::INDICES),
            STATIC_DRAW,
        );

        // todo: is this needed?
        gl.bind_buffer(ARRAY_BUFFER, Some(vertex_buffer));
        gl.bind_buffer(ELEMENT_ARRAY_BUFFER, Some(vbo_indices));

        // Load the vertex position
        gl.vertex_attrib_pointer_f32(
            attr_position,
            3,
            FLOAT,
            false,
            (5 * mem::size_of::<f32>()) as i32,
            0,
        );

        // Load the texture coordinate
        gl.vertex_attrib_pointer_f32(
            attr_texture,
            2,
            FLOAT,
            false,
            (5 * mem::size_of::<f32>()) as i32,
            (3 * mem::size_of::<f32>()) as i32,
        );

        gl.enable_vertex_attrib_array(attr_position as _);
        gl.enable_vertex_attrib_array(attr_texture as _);

        gl.bind_vertex_array(None);
        gl.bind_buffer(ELEMENT_ARRAY_BUFFER, None);
        gl.bind_buffer(ARRAY_BUFFER, None);

        Ok(Self {
            gl,
            program,
            attr_position,
            attr_texture,
            vao,
            vertex_buffer,
            vbo_indices,
        })
    }

    unsafe fn draw_frame(&self, texture_id: u32) {
        use glow::*;
        let gl = &self.gl;

        gl.clear_color(0.0, 0.0, 0.0, 1.0);
        gl.clear(COLOR_BUFFER_BIT);

        gl.blend_color(0.0, 0.0, 0.0, 1.0);
        gl.blend_func_separate(SRC_ALPHA, CONSTANT_COLOR, ONE, ONE_MINUS_SRC_ALPHA);

        gl.blend_equation(FUNC_ADD);
        gl.enable(BLEND);

        gl.use_program(Some(self.program));

        gl.bind_vertex_array(Some(self.vao));

        {
            gl.bind_buffer(ELEMENT_ARRAY_BUFFER, Some(self.vbo_indices));
            gl.bind_buffer(ARRAY_BUFFER, Some(self.vertex_buffer));

            // Load the vertex position
            gl.vertex_attrib_pointer_f32(
                self.attr_position,
                3,
                FLOAT,
                false,
                (5 * mem::size_of::<f32>()) as i32,
                0,
            );

            // Load the texture coordinate
            gl.vertex_attrib_pointer_f32(
                self.attr_texture,
                2,
                FLOAT,
                false,
                (5 * mem::size_of::<f32>()) as i32,
                (3 * mem::size_of::<f32>()) as i32,
            );

            gl.enable_vertex_attrib_array(self.attr_position);
            gl.enable_vertex_attrib_array(self.attr_texture);
        }

        gl.active_texture(TEXTURE0);
        let texture_id = NonZeroU32::new(texture_id).unwrap();
        gl.bind_texture(TEXTURE_2D, Some(glow::NativeTexture(texture_id)));

        let location = gl.get_uniform_location(self.program, "tex");
        gl.uniform_1_i32(location.as_ref(), 0);

        let location = gl.get_uniform_location(self.program, "u_transformation");
        gl.uniform_matrix_4_f32_slice(location.as_ref(), false, Self::IDENTITY);

        gl.draw_elements(TRIANGLES, 6, UNSIGNED_SHORT, 0);

        gl.bind_texture(TEXTURE_2D, None);
        gl.use_program(None);

        gl.bind_vertex_array(None);

        {
            gl.bind_buffer(ELEMENT_ARRAY_BUFFER, None);
            gl.bind_buffer(ARRAY_BUFFER, None);

            gl.disable_vertex_attrib_array(self.attr_position);
            gl.disable_vertex_attrib_array(self.attr_texture);
        }
    }
}

fn as_byte_slice<T>(slice: &[T]) -> &[u8] {
    unsafe {
        core::slice::from_raw_parts(
            slice.as_ptr() as *const u8,
            slice.len() * core::mem::size_of::<T>(),
        )
    }
}
