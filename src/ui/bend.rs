//! Full-screen effects drawn after the UI while Settings selects Wave, Matrix,
//! or Retro. The frame is painted into an offscreen target and sampled again.
//! Wave refracts around the pointer. A saturated color stays itself; white
//! light is what fringes. Matrix rains small pieces down the screen, changing
//! brightness and tint of the pixels already on screen. Retro curves the frame
//! like a CRT: the glass meets the middle of each edge, and corners past the
//! tube are black. None skips this pass and keeps the saved frame rate.

use blade_graphics as bg;

const SHADER_SOURCE: &str = include_str!("bend.wgsl");

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    screen_pointer: [f32; 4],
    time: [f32; 4],
}

#[derive(blade_macros::ShaderData)]
struct Globals {
    r_uniforms: Uniforms,
    r_scene: bg::TextureView,
    r_sampler: bg::Sampler,
}

/// Pointer, clock, and which effect to run. Screen size comes from the window.
#[derive(Clone, Copy)]
pub(crate) struct Sample {
    pub pointer_px: [f32; 2],
    pub time: f32,
    pub effect: crate::store::PaintEffect,
}

/// Barrel strength shared by the retro shader and the pointer remap.
pub const RETRO_CURVATURE: f32 = 0.06;

/// UI position drawn at `pos` on the curved tube.
///
/// The shader shows `tube(screen_uv)` at each output pixel, so a click at
/// `pos` has to be reported as that same sample or it misses the widget.
/// The middle of each edge stays on the frame. A corner past the glass
/// lands outside `screen`. Matches `tube_centered` in `bend.wgsl`.
pub fn retro_content_pos(pos: egui::Pos2, screen: egui::Rect) -> egui::Pos2 {
    let width = screen.width();
    let height = screen.height();
    if width < 1.0 || height < 1.0 {
        return pos;
    }
    let u = (pos.x - screen.min.x) / width;
    let v = (pos.y - screen.min.y) / height;
    let x = u * 2.0 - 1.0;
    let y = v * 2.0 - 1.0;
    let bulge = x * x + y * y;
    let scale = (1.0 + RETRO_CURVATURE * bulge) / (1.0 + RETRO_CURVATURE);
    let nx = x * scale * 0.5 + 0.5;
    let ny = y * scale * 0.5 + 0.5;
    egui::pos2(screen.min.x + nx * width, screen.min.y + ny * height)
}

pub fn warp_retro_pointer(events: &mut [egui::Event], screen: egui::Rect) {
    for event in events {
        match event {
            egui::Event::PointerMoved(pos) | egui::Event::PointerButton { pos, .. } => {
                *pos = retro_content_pos(*pos, screen);
            }
            egui::Event::Touch { pos, .. } => *pos = retro_content_pos(*pos, screen),
            _ => {}
        }
    }
}

struct Scene {
    texture: bg::Texture,
    view: bg::TextureView,
    size: [u32; 2],
}

pub(crate) struct Renderer {
    pipeline: bg::RenderPipeline,
    sampler: bg::Sampler,
    format: bg::TextureFormat,
    scene: Option<Scene>,
}

impl Renderer {
    pub fn new(info: bg::SurfaceInfo, context: &bg::Context) -> Self {
        let shader = context.create_shader(bg::ShaderDesc {
            source: SHADER_SOURCE,
            naga_module: None,
        });
        shader.check_struct_size::<Uniforms>();
        let layout = <Globals as bg::ShaderData>::layout();
        let pipeline = context.create_render_pipeline(bg::RenderPipelineDesc {
            name: "light bend",
            data_layouts: &[&layout],
            vertex: shader.at("vs_main"),
            vertex_fetches: &[],
            primitive: bg::PrimitiveState {
                topology: bg::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            fragment: Some(shader.at("fs_main")),
            color_targets: &[bg::ColorTargetState {
                format: info.format,
                blend: None,
                write_mask: bg::ColorWrites::all(),
            }],
            multisample_state: Default::default(),
        });
        let sampler = context.create_sampler(bg::SamplerDesc {
            name: "light bend",
            address_modes: [bg::AddressMode::ClampToEdge; 3],
            mag_filter: bg::FilterMode::Linear,
            min_filter: bg::FilterMode::Linear,
            ..Default::default()
        });
        Self {
            pipeline,
            sampler,
            format: info.format,
            scene: None,
        }
    }

    pub fn destroy(&mut self, context: &bg::Context) {
        self.discard_scene(context);
        context.destroy_sampler(self.sampler);
        context.destroy_render_pipeline(&mut self.pipeline);
    }

    /// The color target the UI should paint into for this frame.
    pub fn scene(
        &mut self,
        encoder: &mut bg::CommandEncoder,
        context: &bg::Context,
        size: [u32; 2],
    ) -> bg::TextureView {
        if self.scene.as_ref().is_some_and(|scene| scene.size == size) {
            return self.scene.as_ref().expect("checked above").view;
        }
        self.discard_scene(context);
        let extent = bg::Extent {
            width: size[0].max(1),
            height: size[1].max(1),
            depth: 1,
        };
        let texture = context.create_texture(bg::TextureDesc {
            name: "light bend",
            format: self.format,
            size: extent,
            array_layer_count: 1,
            mip_level_count: 1,
            sample_count: 1,
            dimension: bg::TextureDimension::D2,
            usage: bg::TextureUsage::COPY | bg::TextureUsage::TARGET | bg::TextureUsage::RESOURCE,
            external: None,
        });
        encoder.init_texture(texture);
        let view = context.create_texture_view(
            texture,
            bg::TextureViewDesc {
                name: "light bend",
                format: self.format,
                dimension: bg::ViewDimension::D2,
                subresources: &bg::TextureSubresources::default(),
            },
        );
        self.scene = Some(Scene {
            texture,
            view,
            size,
        });
        view
    }

    pub fn composite(
        &self,
        pass: &mut bg::RenderCommandEncoder,
        sample: Sample,
        screen_px: [f32; 2],
    ) {
        let scene = self
            .scene
            .as_ref()
            .expect("scene target is allocated before the bend pass")
            .view;
        let mut encoder = pass.with(&self.pipeline);
        encoder.bind(
            0,
            &Globals {
                r_uniforms: Uniforms {
                    screen_pointer: [
                        screen_px[0],
                        screen_px[1],
                        sample.pointer_px[0],
                        sample.pointer_px[1],
                    ],
                    time: [
                        sample.time,
                        sample.effect.shader_mode(),
                        if sample.effect == crate::store::PaintEffect::Retro {
                            RETRO_CURVATURE
                        } else {
                            0.0
                        },
                        0.0,
                    ],
                },
                r_scene: scene,
                r_sampler: self.sampler,
            },
        );
        encoder.draw(0, 3, 0, 1);
    }

    #[cfg(test)]
    fn scene_texture(&self) -> bg::Texture {
        self.scene.as_ref().expect("scene").texture
    }

    fn discard_scene(&mut self, context: &bg::Context) {
        if let Some(scene) = self.scene.take() {
            context.destroy_texture_view(scene.view);
            context.destroy_texture(scene.texture);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipeline_builds_when_a_gpu_is_present() {
        // SAFETY: the context stays on this thread and its resources are
        // destroyed before the test returns. A machine without a GPU skips.
        let Ok(context) = (unsafe { bg::Context::init(bg::ContextDesc::default()) }) else {
            return;
        };
        let mut renderer = Renderer::new(
            bg::SurfaceInfo {
                format: bg::TextureFormat::Rgba8UnormSrgb,
                alpha: bg::AlphaMode::PreMultiplied,
            },
            &context,
        );
        renderer.destroy(&context);
    }

    #[test]
    fn a_white_frame_stays_bright_after_the_bend() {
        // SAFETY: resources stay on this thread and are destroyed before return.
        let Ok(context) = (unsafe { bg::Context::init(bg::ContextDesc::default()) }) else {
            return;
        };
        let format = bg::TextureFormat::Rgba8Unorm;
        let mut renderer = Renderer::new(
            bg::SurfaceInfo {
                format,
                alpha: bg::AlphaMode::Ignored,
            },
            &context,
        );
        let mut encoder = context.create_command_encoder(bg::CommandEncoderDesc {
            name: "light bend test",
            buffer_count: 1,
            manual_barriers: false,
        });
        let side = 64u32;
        encoder.start();
        let scene = renderer.scene(&mut encoder, &context, [side, side]);
        let pixels = vec![255u8; (side * side * 4) as usize];
        let upload = context.create_buffer(bg::BufferDesc {
            name: "light bend upload",
            size: pixels.len() as u64,
            memory: bg::Memory::Shared,
        });
        unsafe {
            std::ptr::copy_nonoverlapping(pixels.as_ptr(), upload.data(), pixels.len());
        }
        {
            let mut transfer = encoder.transfer("light bend upload");
            transfer.copy_buffer_to_texture(
                upload.into(),
                side * 4,
                bg::TexturePiece {
                    texture: renderer.scene_texture(),
                    mip_level: 0,
                    array_layer: 0,
                    origin: [0; 3],
                },
                bg::Extent {
                    width: side,
                    height: side,
                    depth: 1,
                },
            );
        }
        let _ = scene;
        let output = context.create_texture(bg::TextureDesc {
            name: "light bend output",
            format,
            size: bg::Extent {
                width: side,
                height: side,
                depth: 1,
            },
            array_layer_count: 1,
            mip_level_count: 1,
            sample_count: 1,
            dimension: bg::TextureDimension::D2,
            usage: bg::TextureUsage::TARGET | bg::TextureUsage::COPY,
            external: None,
        });
        encoder.init_texture(output);
        let output_view = context.create_texture_view(
            output,
            bg::TextureViewDesc {
                name: "light bend output",
                format,
                dimension: bg::ViewDimension::D2,
                subresources: &bg::TextureSubresources::default(),
            },
        );
        {
            let mut pass = encoder.render(
                "light bend",
                bg::RenderTargetSet {
                    colors: &[bg::RenderTarget {
                        view: output_view,
                        init_op: bg::InitOp::Clear(bg::TextureColor::TransparentBlack),
                        finish_op: bg::FinishOp::Store,
                    }],
                    depth_stencil: None,
                },
            );
            renderer.composite(
                &mut pass,
                Sample {
                    pointer_px: [side as f32 * 0.5, side as f32 * 0.5],
                    time: 0.4,
                    effect: crate::store::PaintEffect::Wave,
                },
                [side as f32, side as f32],
            );
        }
        let stride = (side * 4).div_ceil(256) * 256;
        let readback = context.create_buffer(bg::BufferDesc {
            name: "light bend readback",
            size: u64::from(stride) * u64::from(side),
            memory: bg::Memory::Shared,
        });
        {
            let mut transfer = encoder.transfer("light bend readback");
            transfer.copy_texture_to_buffer(
                output.into(),
                readback.into(),
                stride,
                bg::Extent {
                    width: side,
                    height: side,
                    depth: 1,
                },
            );
        }
        let sync = context.submit(&mut encoder);
        context.wait_for(&sync, !0).expect("bend pass");
        let mapped =
            unsafe { std::slice::from_raw_parts(readback.data(), (stride * side) as usize) };
        let pixel = &mapped[0..4];
        assert!(
            pixel[0] > 200 && pixel[1] > 200 && pixel[2] > 200,
            "a flat white frame should stay bright, got {pixel:?}"
        );
        context.destroy_buffer(readback);
        context.destroy_texture_view(output_view);
        context.destroy_texture(output);
        context.destroy_buffer(upload);
        renderer.destroy(&context);
        context.destroy_command_encoder(&mut encoder);
    }

    #[test]
    fn a_red_line_stays_red_and_a_white_line_fringes() {
        // SAFETY: resources stay on this thread and are destroyed before return.
        let Ok(context) = (unsafe { bg::Context::init(bg::ContextDesc::default()) }) else {
            return;
        };
        let side = 80u32;
        let red = paint_column(
            &context,
            side,
            [255, 0, 0, 255],
            crate::store::PaintEffect::Wave,
            false,
            0.4,
        );
        let mut saw_red = false;
        for pixel in red.as_chunks::<4>().0 {
            let (r, g, b) = (pixel[0], pixel[1], pixel[2]);
            if r > 40 || g > 20 || b > 20 {
                assert!(
                    g < 20 && b < 20 && r > g && r > b,
                    "a red line picked up other wavelengths: {pixel:?}"
                );
                saw_red = true;
            }
        }
        assert!(saw_red, "the red line disappeared");

        let green = paint_column(
            &context,
            side,
            [0, 255, 0, 255],
            crate::store::PaintEffect::Wave,
            false,
            0.4,
        );
        let mut saw_green = false;
        for pixel in green.as_chunks::<4>().0 {
            let (r, g, b) = (pixel[0], pixel[1], pixel[2]);
            if r > 20 || g > 40 || b > 20 {
                assert!(
                    r < 20 && b < 20 && g > r && g > b,
                    "a green line split into other wavelengths: {pixel:?}"
                );
                saw_green = true;
            }
        }
        assert!(saw_green, "the green line disappeared");

        let white = paint_column(
            &context,
            side,
            [255, 255, 255, 255],
            crate::store::PaintEffect::Wave,
            false,
            0.4,
        );
        let fringed = white.as_chunks::<4>().0.iter().any(|pixel| {
            let (r, b) = (pixel[0] as i16, pixel[2] as i16);
            pixel[0].max(pixel[2]) > 40 && (r - b).abs() > 30
        });
        assert!(fringed, "white light should separate into colored fringes");
    }

    #[test]
    fn retro_reaches_the_edges_and_blacks_out_the_corners() {
        // SAFETY: resources stay on this thread and are destroyed before return.
        let Ok(context) = (unsafe { bg::Context::init(bg::ContextDesc::default()) }) else {
            return;
        };
        let side = 64u32;
        let frame = paint_column(
            &context,
            side,
            [255, 255, 255, 255],
            crate::store::PaintEffect::Retro,
            true,
            0.4,
        );
        let at = |x: u32, y: u32| {
            let index = ((y * side + x) * 4) as usize;
            frame[index] as u16 + frame[index + 1] as u16 + frame[index + 2] as u16
        };
        let center = at(side / 2, side / 2);
        let edge = at(side - 1, side / 2);
        let corner = at(0, 0);
        assert!(
            center > 450,
            "the middle of the tube should stay bright, sum was {center}"
        );
        assert!(
            edge > 400,
            "the tube should meet the middle of each edge, sum was {edge}"
        );
        assert!(
            corner < 8,
            "past the glass the frame should be black, sum was {corner}"
        );
    }

    #[test]
    fn retro_pointer_follows_the_curve() {
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(200.0, 100.0));
        let center = retro_content_pos(egui::pos2(100.0, 50.0), screen);
        assert!((center.x - 100.0).abs() < 0.01);
        assert!((center.y - 50.0).abs() < 0.01);
        let edge = retro_content_pos(egui::pos2(200.0, 50.0), screen);
        assert!(
            (edge.x - 200.0).abs() < 0.01 && (edge.y - 50.0).abs() < 0.01,
            "the middle of an edge stays on the frame, got {edge:?}"
        );
        let corner = retro_content_pos(egui::pos2(200.0, 100.0), screen);
        assert!(
            corner.x > 200.0 && corner.y > 100.0,
            "a corner click is off the glass, got {corner:?}"
        );
        let seen = egui::pos2(180.0, 90.0);
        let content = retro_content_pos(seen, screen);
        assert!(
            content.x > seen.x + 0.5,
            "a click toward the corner samples further out, got {content:?}"
        );
    }

    #[test]
    fn matrix_only_changes_brightness_and_tint() {
        let Ok(context) = (unsafe { bg::Context::init(bg::ContextDesc::default()) }) else {
            return;
        };
        let side = 96u32;
        let white = paint_column(
            &context,
            side,
            [255, 255, 255, 255],
            crate::store::PaintEffect::Matrix,
            true,
            0.4,
        );
        let black = paint_column(
            &context,
            side,
            [0, 0, 0, 255],
            crate::store::PaintEffect::Matrix,
            true,
            0.4,
        );
        let mut min_g = 255u8;
        let mut max_g = 0u8;
        let mut saw_dim = false;
        let mut saw_bright = false;
        for pixel in white.as_chunks::<4>().0 {
            min_g = min_g.min(pixel[1]);
            max_g = max_g.max(pixel[1]);
            if pixel[1] < 160 && pixel[0].abs_diff(pixel[1]) <= 12 {
                saw_dim = true;
            }
            if pixel[1] > 200 && pixel[1] > pixel[0] + 20 && pixel[1] > pixel[2] + 20 {
                saw_bright = true;
            }
        }
        assert!(
            saw_dim,
            "content outside a trail should sit below the original"
        );
        assert!(
            saw_bright,
            "a trail should lift the green channel of the existing pixel"
        );
        assert!(
            max_g > min_g + 40,
            "the rain should move brightness across existing pixels, {min_g}..={max_g}"
        );
        for pixel in black.as_chunks::<4>().0 {
            assert!(
                pixel[0] < 8 && pixel[1] < 8 && pixel[2] < 8,
                "empty pixels stay empty: {pixel:?}"
            );
        }
        let dark = paint_column(
            &context,
            side,
            [6, 7, 8, 255],
            crate::store::PaintEffect::Matrix,
            true,
            0.4,
        );
        for pixel in dark.as_chunks::<4>().0 {
            assert!(
                pixel[0].abs_diff(6) <= 1 && pixel[1].abs_diff(7) <= 1 && pixel[2].abs_diff(8) <= 1,
                "a near-black background stays as drawn: {pixel:?}"
            );
        }
    }

    #[test]
    fn matrix_pieces_fall_downward() {
        let Ok(context) = (unsafe { bg::Context::init(bg::ContextDesc::default()) }) else {
            return;
        };
        let side = 128u32;
        let earlier = paint_column(
            &context,
            side,
            [255, 255, 255, 255],
            crate::store::PaintEffect::Matrix,
            true,
            1.0,
        );
        let later = paint_column(
            &context,
            side,
            [255, 255, 255, 255],
            crate::store::PaintEffect::Matrix,
            true,
            1.08,
        );
        let profile = |frame: &[u8]| -> Vec<f32> {
            (0..side as usize)
                .map(|y| {
                    (0..side as usize)
                        .map(|x| {
                            let index = (y * side as usize + x) * 4;
                            frame[index + 1] as f32 - frame[index] as f32
                        })
                        .sum()
                })
                .collect()
        };
        let before = profile(&earlier);
        let after = profile(&later);
        // Same rows for every candidate, so a short overlap cannot win on length.
        let span = 36i32;
        let mut best_shift = 0i32;
        let mut best_error = f32::MAX;
        for shift in -span..=span {
            let mut error = 0.0;
            for y in span..(side as i32 - span) {
                let delta = before[y as usize] - after[(y + shift) as usize];
                error += delta * delta;
            }
            if error < best_error {
                best_error = error;
                best_shift = shift;
            }
        }
        assert!(
            best_shift > 8,
            "pieces should travel down the screen, shift was {best_shift}"
        );
    }

    fn paint_column(
        context: &bg::Context,
        side: u32,
        line: [u8; 4],
        effect: crate::store::PaintEffect,
        solid: bool,
        time: f32,
    ) -> Vec<u8> {
        let format = bg::TextureFormat::Rgba8Unorm;
        let mut renderer = Renderer::new(
            bg::SurfaceInfo {
                format,
                alpha: bg::AlphaMode::Ignored,
            },
            context,
        );
        let mut pixels = vec![0u8; (side * side * 4) as usize];
        for pixel in pixels.as_chunks_mut::<4>().0 {
            if solid {
                pixel.copy_from_slice(&line);
            } else {
                pixel[3] = 255;
            }
        }
        if !solid {
            for y in 0..side {
                for x in side / 2 - 1..side / 2 + 1 {
                    let index = ((y * side + x) * 4) as usize;
                    pixels[index..index + 4].copy_from_slice(&line);
                }
            }
        }
        let mut encoder = context.create_command_encoder(bg::CommandEncoderDesc {
            name: "light bend column",
            buffer_count: 1,
            manual_barriers: false,
        });
        encoder.start();
        let _scene = renderer.scene(&mut encoder, context, [side, side]);
        let upload = context.create_buffer(bg::BufferDesc {
            name: "light bend column upload",
            size: pixels.len() as u64,
            memory: bg::Memory::Shared,
        });
        unsafe {
            std::ptr::copy_nonoverlapping(pixels.as_ptr(), upload.data(), pixels.len());
        }
        {
            let mut transfer = encoder.transfer("light bend column upload");
            transfer.copy_buffer_to_texture(
                upload.into(),
                side * 4,
                bg::TexturePiece {
                    texture: renderer.scene_texture(),
                    mip_level: 0,
                    array_layer: 0,
                    origin: [0; 3],
                },
                bg::Extent {
                    width: side,
                    height: side,
                    depth: 1,
                },
            );
        }
        let output = context.create_texture(bg::TextureDesc {
            name: "light bend column output",
            format,
            size: bg::Extent {
                width: side,
                height: side,
                depth: 1,
            },
            array_layer_count: 1,
            mip_level_count: 1,
            sample_count: 1,
            dimension: bg::TextureDimension::D2,
            usage: bg::TextureUsage::TARGET | bg::TextureUsage::COPY,
            external: None,
        });
        encoder.init_texture(output);
        let output_view = context.create_texture_view(
            output,
            bg::TextureViewDesc {
                name: "light bend column output",
                format,
                dimension: bg::ViewDimension::D2,
                subresources: &bg::TextureSubresources::default(),
            },
        );
        {
            let mut pass = encoder.render(
                "light bend",
                bg::RenderTargetSet {
                    colors: &[bg::RenderTarget {
                        view: output_view,
                        init_op: bg::InitOp::Clear(bg::TextureColor::TransparentBlack),
                        finish_op: bg::FinishOp::Store,
                    }],
                    depth_stencil: None,
                },
            );
            renderer.composite(
                &mut pass,
                Sample {
                    pointer_px: [side as f32 * 0.5, side as f32 * 0.5],
                    time,
                    effect,
                },
                [side as f32, side as f32],
            );
        }
        let stride = (side * 4).div_ceil(256) * 256;
        let readback = context.create_buffer(bg::BufferDesc {
            name: "light bend column readback",
            size: u64::from(stride) * u64::from(side),
            memory: bg::Memory::Shared,
        });
        {
            let mut transfer = encoder.transfer("light bend column readback");
            transfer.copy_texture_to_buffer(
                output.into(),
                readback.into(),
                stride,
                bg::Extent {
                    width: side,
                    height: side,
                    depth: 1,
                },
            );
        }
        let sync = context.submit(&mut encoder);
        context.wait_for(&sync, !0).expect("bend column");
        let mapped =
            unsafe { std::slice::from_raw_parts(readback.data(), (stride * side) as usize) };
        let mut tight = Vec::with_capacity((side * side * 4) as usize);
        for row in mapped.chunks_exact(stride as usize) {
            tight.extend_from_slice(&row[..(side * 4) as usize]);
        }
        context.destroy_buffer(readback);
        context.destroy_texture_view(output_view);
        context.destroy_texture(output);
        context.destroy_buffer(upload);
        renderer.destroy(context);
        context.destroy_command_encoder(&mut encoder);
        tight
    }
}
