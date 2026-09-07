//! FileMan-style winit lifecycle and Blade/egui rendering.

use blade_egui as be;
use blade_graphics as bg;
use std::{fs, io, path, sync, time};

use crate::{desktop, ui, workspace};
use anyhow::Context;

#[path = "wake_flag.rs"]
mod wake_flag;

const INITIAL_SIZE: (u32, u32) = (1280, 760);
#[cfg(target_os = "linux")]
type LocalWake = sync::Arc<dyn Fn() + Send + Sync>;

#[derive(Debug)]
enum Event {
    /// egui asked for a frame (hover, widgets, timers). Not fps-capped.
    Repaint(time::Instant),
    /// SSH/tmux worker has new data. Coalesced to the configured fps.
    Remote,
}

struct Runtime {
    surface: bg::Surface,
    config: bg::SurfaceConfig,
    surface_info: bg::SurfaceInfo,
    encoder: bg::CommandEncoder,
    last_sync: Option<bg::SyncPoint>,
    pending_view: Option<bg::TextureView>,
    painter: be::GuiPainter,
    input: egui_winit::State,
    size: winit::dpi::PhysicalSize<u32>,
    #[cfg(target_os = "linux")]
    wayland_drop: Option<crate::wayland_drop::WaylandDrop>,
    // These owners are dropped LAST, after every resource referring to them.
    // Native window and display handles must survive surface teardown.
    window: winit::window::Window,
    context: bg::Context,
}

impl Runtime {
    fn new(
        event_loop: &winit::event_loop::ActiveEventLoop,
        ctx: &egui::Context,
        #[cfg(target_os = "linux")] local_wake: LocalWake,
    ) -> anyhow::Result<Self> {
        #[allow(unused_mut)]
        let mut attributes = winit::window::Window::default_attributes()
            .with_title("Starcom")
            .with_inner_size(winit::dpi::LogicalSize::new(INITIAL_SIZE.0, INITIAL_SIZE.1))
            .with_min_inner_size(winit::dpi::LogicalSize::new(640, 400))
            .with_window_icon(app_icon());
        #[cfg(target_os = "linux")]
        {
            use winit::platform::{
                wayland::WindowAttributesExtWayland, x11::WindowAttributesExtX11,
            };
            attributes = WindowAttributesExtWayland::with_name(attributes, "starcom", "starcom");
            attributes = WindowAttributesExtX11::with_name(attributes, "starcom", "starcom");
        }
        #[cfg(windows)]
        {
            // winit 0.30 already implements OLE file drops. macOS registers
            // NSDraggingDestination. Wayland is the gap: see wayland_drop.
            use winit::platform::windows::WindowAttributesExtWindows;
            attributes = attributes.with_drag_and_drop(true);
        }
        let window = event_loop
            .create_window(attributes)
            .context("create Starcom window")?;
        #[cfg(target_os = "linux")]
        let wayland_drop = crate::wayland_drop::WaylandDrop::attach(&window, local_wake);
        // SAFETY: the returned graphics context and surface are confined to this
        // event-loop thread; the window outlives the explicitly destroyed surface.
        let context = unsafe {
            bg::Context::init(bg::ContextDesc {
                presentation: true,
                validation: cfg!(debug_assertions),
                ..Default::default()
            })
        }
        .map_err(|error| anyhow::anyhow!("GPU initialization failed: {error:?}"))?;
        let size = window.inner_size();
        let config = bg::SurfaceConfig {
            size: bg::Extent {
                width: size.width.max(1),
                height: size.height.max(1),
                depth: 1,
            },
            usage: bg::TextureUsage::TARGET,
            ..Default::default()
        };
        let surface = context
            .create_surface_configured(&window, config)
            .map_err(|error| anyhow::anyhow!("GPU surface creation failed: {error:?}"))?;
        let surface_info = surface.info();
        let painter = be::GuiPainter::new(surface_info, &context);
        let encoder = context.create_command_encoder(bg::CommandEncoderDesc {
            name: "starcom",
            buffer_count: 1,
        });
        let input = egui_winit::State::new(
            ctx.clone(),
            egui::ViewportId::ROOT,
            &window,
            Some(window.scale_factor() as f32),
            None,
            None,
        );
        Ok(Self {
            window,
            context,
            surface,
            config,
            surface_info,
            encoder,
            last_sync: None,
            pending_view: None,
            painter,
            input,
            size,
            #[cfg(target_os = "linux")]
            wayland_drop,
        })
    }

    fn resize(&mut self, size: winit::dpi::PhysicalSize<u32>) -> anyhow::Result<()> {
        self.size = size;
        if size.width == 0 || size.height == 0 {
            return Ok(());
        }
        self.finish_frame()?;
        self.config.size.width = size.width;
        self.config.size.height = size.height;
        self.context
            .reconfigure_surface(&mut self.surface, self.config);
        let info = self.surface.info();
        // The painter owns the font atlas. Replacing it silently would lose
        // textures unless egui reuploads them, so fail explicitly on this rare
        // platform change rather than displaying a corrupted window.
        anyhow::ensure!(
            info == self.surface_info,
            "surface format changed; reopen Starcom"
        );
        Ok(())
    }

    fn finish_frame(&mut self) -> anyhow::Result<()> {
        if let Some(ref sync) = self.last_sync {
            self.context
                .wait_for(sync, !0)
                .map_err(|error| anyhow::anyhow!("GPU frame wait failed: {error:?}"))?;
        }
        if let Some(view) = self.pending_view.take() {
            self.context.destroy_texture_view(view);
        }
        self.last_sync = None;
        Ok(())
    }

    fn paint(&mut self, ctx: &egui::Context, output: egui::FullOutput) -> anyhow::Result<()> {
        self.input
            .handle_platform_output(&self.window, output.platform_output);
        let jobs = ctx.tessellate(output.shapes, output.pixels_per_point);
        let screen = be::ScreenDescriptor {
            physical_size: (self.size.width, self.size.height),
            scale_factor: output.pixels_per_point,
        };
        self.finish_frame()?;
        self.encoder.start();
        self.painter
            .update_textures(&mut self.encoder, &output.textures_delta, &self.context);
        let frame = self.surface.acquire_frame();
        self.encoder.init_texture(frame.texture());
        let view = self.context.create_texture_view(
            frame.texture(),
            bg::TextureViewDesc {
                name: "starcom surface",
                format: self.surface_info.format,
                dimension: bg::ViewDimension::D2,
                subresources: &bg::TextureSubresources::default(),
            },
        );
        {
            let mut pass = self.encoder.render(
                "starcom",
                bg::RenderTargetSet {
                    colors: &[bg::RenderTarget {
                        view,
                        init_op: bg::InitOp::Clear(bg::TextureColor::OpaqueBlack),
                        finish_op: bg::FinishOp::Store,
                    }],
                    depth_stencil: None,
                },
            );
            self.painter.paint(&mut pass, &jobs, &screen, &self.context);
        }
        self.encoder.present(frame);
        let sync = self.context.submit(&mut self.encoder);
        self.painter.after_submit(&sync);
        self.last_sync = Some(sync);
        self.pending_view = Some(view);
        Ok(())
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        if let Err(error) = self.finish_frame() {
            log::error!("{error}");
        }
        // Swapchain teardown waits for presentation as well as rendering. Keep
        // both the native window and event loop alive until it is complete.
        self.context.destroy_surface(&mut self.surface);
        self.context.destroy_command_encoder(&mut self.encoder);
        self.painter.destroy(&self.context);
    }
}

#[derive(Clone, Copy)]
struct Wake {
    when: time::Instant,
    /// Local UI: paint as soon as `when` arrives. Remote output: still
    /// subject to the fps interval on the actual RedrawRequested.
    force: bool,
}

struct App {
    workspace: workspace::Workspace,
    ctx: egui::Context,
    runtime: Option<Runtime>,
    next_repaint: Option<Wake>,
    last_paint: Option<time::Instant>,
    input_redraw: bool,
    remote_wake: sync::Arc<wake_flag::WakeFlag>,
    error: Option<anyhow::Error>,
    #[cfg(target_os = "linux")]
    local_wake: LocalWake,
}

impl App {
    fn shutdown(&mut self) {
        self.ctx.set_request_repaint_callback(|_| {});
        self.workspace.shutdown();
        self.runtime.take();
        self.next_repaint = None;
    }

    fn schedule(&mut self, when: time::Instant, force: bool) {
        self.next_repaint = Some(match self.next_repaint {
            Some(previous) => Wake {
                when: previous.when.min(when),
                force: previous.force || force,
            },
            None => Wake { when, force },
        });
    }

    fn request_redraw(&mut self) {
        let Some(runtime) = self.runtime.as_ref() else {
            return;
        };
        self.input_redraw = true;
        runtime.window.request_redraw();
    }

    #[cfg(target_os = "linux")]
    fn pump_wayland_drop(&mut self) {
        let Some(runtime) = self.runtime.as_mut() else {
            return;
        };
        let Some(dropper) = runtime.wayland_drop.as_mut() else {
            return;
        };
        let pump = dropper.pump();
        let mut redraw = false;
        if let Some(error) = pump.error {
            self.workspace
                .set_notice(format!("Could not accept file drop: {error}"));
            redraw = true;
        }
        if pump.hovering {
            let files = &mut runtime.input.egui_input_mut().hovered_files;
            if files.is_empty() {
                files.push(egui::HoveredFile::default());
            }
            redraw = true;
        }
        if !pump.dropped.is_empty() {
            let input = runtime.input.egui_input_mut();
            input.hovered_files.clear();
            for path in pump.dropped {
                input.dropped_files.push(egui::DroppedFile {
                    path: Some(path),
                    ..Default::default()
                });
            }
            redraw = true;
        }
        if redraw {
            runtime.window.request_redraw();
            self.input_redraw = true;
        }
    }
}

impl winit::application::ApplicationHandler<Event> for App {
    fn resumed(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
        if self.runtime.is_some() {
            self.workspace.notice_suspend();
            return;
        }
        match Runtime::new(
            event_loop,
            &self.ctx,
            #[cfg(target_os = "linux")]
            sync::Arc::clone(&self.local_wake),
        ) {
            Ok(runtime) => {
                log::info!("Starcom desktop initialized");
                runtime.window.request_redraw();
                self.runtime = Some(runtime);
            }
            Err(error) => {
                self.error = Some(error);
                event_loop.exit();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &winit::event_loop::ActiveEventLoop,
        window_id: winit::window::WindowId,
        event: winit::event::WindowEvent,
    ) {
        if self
            .runtime
            .as_ref()
            .is_none_or(|runtime| runtime.window.id() != window_id)
        {
            return;
        }
        if !matches!(event, winit::event::WindowEvent::RedrawRequested) {
            let modifiers = self
                .runtime
                .as_ref()
                .expect("checked above")
                .input
                .egui_input()
                .modifiers;
            let control = self
                .workspace
                .terminal_focused(&self.ctx)
                .then(|| ui::input::terminal_control_key(&event, modifiers))
                .flatten();
            let redraw = if let Some(key) = control {
                let runtime = self.runtime.as_mut().expect("checked above");
                runtime.input.egui_input_mut().events.push(key);
                true
            } else {
                let runtime = self.runtime.as_mut().expect("checked above");
                runtime
                    .input
                    .on_window_event(&runtime.window, &event)
                    .repaint
            };
            if redraw {
                self.request_redraw();
            }
        }
        #[cfg(target_os = "linux")]
        self.pump_wayland_drop();
        match event {
            winit::event::WindowEvent::CloseRequested => {
                self.shutdown();
                event_loop.exit();
            }
            winit::event::WindowEvent::Resized(size) => {
                let result = self.runtime.as_mut().expect("checked above").resize(size);
                if let Err(error) = result {
                    self.error = Some(error);
                    event_loop.exit();
                } else {
                    self.request_redraw();
                }
            }
            winit::event::WindowEvent::RedrawRequested => {
                let interval = self.workspace.paint_interval();
                let force = self.input_redraw;
                self.input_redraw = false;
                // Only remote coalescing uses this path (force is false). Local
                // pointer/key events set input_redraw and paint immediately.
                if !force && self.last_paint.is_some_and(|at| at.elapsed() < interval) {
                    let at = self.last_paint.expect("checked above") + interval;
                    self.schedule(at, false);
                    return;
                }
                if self
                    .next_repaint
                    .is_some_and(|wake| wake.when <= time::Instant::now())
                {
                    self.next_repaint = None;
                }
                let runtime = self.runtime.as_mut().expect("checked above");
                if runtime.size.width == 0 || runtime.size.height == 0 {
                    return;
                }
                let input = runtime.input.take_egui_input(&runtime.window);
                let mut action = workspace::Action::None;
                let output = self.ctx.run_ui(input, |root| {
                    action = self.workspace.show(root);
                });
                let runtime = self.runtime.as_mut().expect("checked above");
                if let Err(error) = runtime.paint(&self.ctx, output) {
                    self.error = Some(error);
                    event_loop.exit();
                    return;
                }
                self.last_paint = Some(time::Instant::now());
                self.workspace.apply(action, || {
                    self.runtime
                        .as_mut()
                        .and_then(|runtime| runtime.input.clipboard_text())
                });
            }
            _ => {}
        }
    }

    fn exiting(&mut self, _event_loop: &winit::event_loop::ActiveEventLoop) {
        // run_app consumes EventLoop; waiting until it returns is too late on
        // backends whose window/surface cleanup needs the live display connection.
        self.shutdown();
    }

    fn user_event(&mut self, _event_loop: &winit::event_loop::ActiveEventLoop, event: Event) {
        match event {
            Event::Repaint(when) => self.schedule(when, true),
            Event::Remote => {
                self.remote_wake.acknowledge();
                if !self.workspace.remote_changed() {
                    return;
                }
                let now = time::Instant::now();
                let interval = self.workspace.paint_interval();
                let when = match self.last_paint {
                    Some(painted) => now.max(painted + interval),
                    None => now,
                };
                self.schedule(when, false);
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
        #[cfg(target_os = "linux")]
        self.pump_wayland_drop();
        self.workspace.notice_suspend();
        event_loop.set_control_flow(winit::event_loop::ControlFlow::Wait);
        if let Some(wake) = self.next_repaint {
            if wake.when <= time::Instant::now() {
                self.next_repaint = None;
                if wake.force {
                    self.input_redraw = true;
                }
                if let Some(ref runtime) = self.runtime {
                    runtime.window.request_redraw();
                }
            } else {
                event_loop.set_control_flow(winit::event_loop::ControlFlow::WaitUntil(wake.when));
            }
        }
    }
}

fn app_icon() -> Option<winit::window::Icon> {
    const PNG: &[u8] = include_bytes!("../etc/macos/icon.png");
    let mut reader = png::Decoder::new(io::Cursor::new(PNG)).read_info().ok()?;
    let mut pixels = vec![0_u8; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut pixels).ok()?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    pixels.truncate(info.buffer_size());
    winit::window::Icon::from_rgba(pixels, info.width, info.height).ok()
}

pub(crate) fn configure(ctx: &egui::Context) {
    ctx.set_visuals(egui::Visuals::dark());
    let mut style = (*ctx.global_style()).clone();
    style.animation_time = 0.0;
    style.spacing.item_spacing = egui::vec2(7.0, 6.0);
    style.visuals.panel_fill = egui::Color32::from_rgb(27, 30, 36);
    ctx.set_global_style(style);
}

pub fn run(startup: desktop::Startup) -> anyhow::Result<()> {
    let event_loop = winit::event_loop::EventLoop::<Event>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let ctx = egui::Context::default();
    configure(&ctx);
    ctx.set_request_repaint_callback({
        let proxy = proxy.clone();
        move |info| {
            if let Some(when) = time::Instant::now().checked_add(info.delay) {
                let _ = proxy.send_event(Event::Repaint(when));
            }
        }
    });
    let remote_wake = sync::Arc::new(wake_flag::WakeFlag::default());
    let Some(workspace) = workspace::Workspace::launch(
        {
            let proxy = proxy.clone();
            let wake = sync::Arc::clone(&remote_wake);
            sync::Arc::new(move || {
                wake.post(|| proxy.send_event(Event::Remote).is_ok());
            })
        },
        startup,
    )?
    else {
        return Ok(());
    };
    let mut app = App {
        workspace,
        ctx,
        runtime: None,
        next_repaint: None,
        last_paint: None,
        input_redraw: false,
        remote_wake,
        error: None,
        #[cfg(target_os = "linux")]
        local_wake: {
            let proxy = proxy.clone();
            sync::Arc::new(move || {
                let _ = proxy.send_event(Event::Repaint(time::Instant::now()));
            })
        },
    };
    let result = event_loop.run_app(&mut app);
    // Idempotent fallback for event-loop initialization failures; normally the
    // exiting callback has already released all native resources.
    app.shutdown();
    result?;
    if let Some(error) = app.error {
        return Err(error);
    }
    Ok(())
}

/// Render the actual desktop UI with deterministic demo data. No SSH, display
/// server, screen capture, or invented image is involved; this uses Blade itself.
pub fn save_snapshot(
    workspace: &mut workspace::Workspace,
    path: &path::Path,
) -> anyhow::Result<()> {
    // SAFETY: offscreen resources are used on this thread and destroyed only
    // after waiting for GPU completion before reading the shared buffer.
    let context = unsafe { bg::Context::init(bg::ContextDesc::default()) }
        .map_err(|error| anyhow::anyhow!("offscreen GPU initialization failed: {error:?}"))?;
    let size = bg::Extent {
        width: INITIAL_SIZE.0,
        height: INITIAL_SIZE.1,
        depth: 1,
    };
    // The painter emits linear light; an sRGB target encodes PNG bytes
    // correctly instead of exporting dark linear values as sRGB.
    let format = bg::TextureFormat::Rgba8UnormSrgb;
    let mut painter = be::GuiPainter::new(
        bg::SurfaceInfo {
            format,
            alpha: bg::AlphaMode::PreMultiplied,
        },
        &context,
    );
    let mut encoder = context.create_command_encoder(bg::CommandEncoderDesc {
        name: "desktop snapshot",
        buffer_count: 1,
    });
    let texture = context.create_texture(bg::TextureDesc {
        name: "desktop snapshot",
        format,
        size,
        array_layer_count: 1,
        mip_level_count: 1,
        sample_count: 1,
        dimension: bg::TextureDimension::D2,
        usage: bg::TextureUsage::TARGET | bg::TextureUsage::COPY,
        external: None,
    });
    let view = context.create_texture_view(
        texture,
        bg::TextureViewDesc {
            name: "desktop snapshot",
            format,
            dimension: bg::ViewDimension::D2,
            subresources: &bg::TextureSubresources::default(),
        },
    );
    let stride = (size.width * 4).div_ceil(256) * 256;
    let buffer = context.create_buffer(bg::BufferDesc {
        name: "desktop readback",
        size: u64::from(stride) * u64::from(size.height),
        memory: bg::Memory::Shared,
    });
    let result = (|| {
        let ctx = egui::Context::default();
        configure(&ctx);
        let mut textures = egui::TexturesDelta::default();
        let mut shapes = Vec::new();
        // Let panel sizes and sticky-bottom scroll positions settle. Preserve
        // all font texture deltas, not just those from the final layout pass.
        for pass in 0..3 {
            let screen = egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(size.width as f32, size.height as f32),
            );
            let input = egui::RawInput {
                screen_rect: Some(screen),
                time: Some(pass as f64 / 60.0),
                viewports: [(
                    egui::ViewportId::ROOT,
                    egui::ViewportInfo {
                        native_pixels_per_point: Some(1.0),
                        inner_rect: Some(screen),
                        ..Default::default()
                    },
                )]
                .into_iter()
                .collect(),
                ..Default::default()
            };
            let output = ctx.run_ui(input, |root| {
                workspace.show(root);
            });
            textures.append(output.textures_delta);
            shapes = output.shapes;
        }
        let jobs = ctx.tessellate(shapes, 1.0);
        encoder.start();
        encoder.init_texture(texture);
        painter.update_textures(&mut encoder, &textures, &context);
        {
            let mut pass = encoder.render(
                "desktop snapshot",
                bg::RenderTargetSet {
                    colors: &[bg::RenderTarget {
                        view,
                        init_op: bg::InitOp::Clear(bg::TextureColor::OpaqueBlack),
                        finish_op: bg::FinishOp::Store,
                    }],
                    depth_stencil: None,
                },
            );
            painter.paint(
                &mut pass,
                &jobs,
                &be::ScreenDescriptor {
                    physical_size: (size.width, size.height),
                    scale_factor: 1.0,
                },
                &context,
            );
        }
        {
            let mut transfer = encoder.transfer("desktop readback");
            transfer.copy_texture_to_buffer(
                bg::TexturePiece {
                    texture,
                    mip_level: 0,
                    array_layer: 0,
                    origin: [0, 0, 0],
                },
                buffer.into(),
                stride,
                size,
            );
        }
        let sync = context.submit(&mut encoder);
        painter.after_submit(&sync);
        context
            .wait_for(&sync, !0)
            .map_err(|error| anyhow::anyhow!("snapshot GPU wait failed: {error:?}"))?;
        // SAFETY: buffer is Shared, sized for every padded row, and the GPU
        // write has completed. Copy into owned memory before destroying it.
        let mapped = unsafe {
            std::slice::from_raw_parts(buffer.data(), stride as usize * size.height as usize)
        };
        let mut rgba = Vec::with_capacity(size.width as usize * size.height as usize * 4);
        for row in mapped.chunks_exact(stride as usize) {
            rgba.extend_from_slice(&row[..size.width as usize * 4]);
        }
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        let mut png = png::Encoder::new(
            io::BufWriter::new(fs::File::create(path)?),
            size.width,
            size.height,
        );
        png.set_color(png::ColorType::Rgba);
        png.set_depth(png::BitDepth::Eight);
        png.set_compression(png::Compression::High);
        let mut writer = png.write_header()?;
        writer.write_image_data(&rgba)?;
        writer.finish()?;
        Ok(())
    })();
    context.destroy_texture_view(view);
    context.destroy_texture(texture);
    context.destroy_buffer(buffer);
    painter.destroy(&context);
    context.destroy_command_encoder(&mut encoder);
    result
}

#[cfg(test)]
mod tests {
    #[test]
    fn app_icon_decodes_as_rgba() {
        assert!(super::app_icon().is_some());
    }
}
