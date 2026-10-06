//! Terminal cells painted as one batch of textured quads per pane.
//!
//! egui still owns the fonts: glyph quads, UVs and the atlas are egui's own, so
//! cells look like egui text. What goes away is a laid-out galley, a tessellated
//! mesh, and a draw call per run of cells. A pane hands its batch to egui as a
//! `PaintCallback`, which keeps egui's z-order: the window splits the
//! tessellated primitives at each batch and draws the batch in between.

use blade_egui as be;
use blade_graphics as bg;
use egui::emath::GuiRounding as _;
use std::{collections, sync};

const SHADER_SOURCE: &str = include_str!("grid.wgsl");

/// One textured quad in points. Solid fills sample the atlas white texel.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct Quad {
    min: [f32; 2],
    max: [f32; 2],
    /// Atlas texels, not normalized.
    uv_min: [f32; 2],
    uv_max: [f32; 2],
    /// Fragments outside this rect are discarded.
    clip_min: [f32; 2],
    clip_max: [f32; 2],
    /// egui's premultiplied sRGBA bytes.
    color: u32,
    /// Horizontal shift of the top edge, for italics.
    skew: f32,
    padding: [f32; 2],
}

/// What a pane painted this frame, carried through egui as a paint callback.
pub(crate) struct Batch {
    quads: Vec<Quad>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Style {
    pub italics: bool,
    pub underline: bool,
    pub strikethrough: bool,
}

#[derive(Clone, Copy, Debug)]
struct GlyphQuad {
    min: egui::Pos2,
    max: egui::Pos2,
    uv_min: egui::Pos2,
    uv_max: egui::Pos2,
    skew: f32,
}

/// A cluster as egui lays it out, relative to the galley origin.
#[derive(Clone, Debug, Default)]
struct Shaped {
    /// Galley height; the cell centers the galley vertically in its row.
    height: f32,
    quads: Vec<GlyphQuad>,
    /// Horizontal extent of the glyph advances, for underline and strike.
    span: (f32, f32),
    /// Logical bottom and middle of the glyph row.
    underline_y: f32,
    strike_y: f32,
}

impl Shaped {
    fn new(galley: &egui::Galley) -> Self {
        let mut shaped = Self {
            height: galley.size().y,
            ..Default::default()
        };
        let mut span = None::<(f32, f32)>;
        for row in &galley.rows {
            let offset = row.pos.to_vec2();
            let vertices = &row.visuals.mesh.vertices[row.visuals.glyph_vertex_range.clone()];
            // epaint emits each glyph as left-top, right-top, left-bottom,
            // right-bottom; italics move the top pair right.
            for &[top, _, left_bottom, bottom] in vertices.as_chunks::<4>().0 {
                shaped.quads.push(GlyphQuad {
                    min: egui::pos2(left_bottom.pos.x, top.pos.y) + offset,
                    max: bottom.pos + offset,
                    uv_min: top.uv,
                    uv_max: bottom.uv,
                    skew: top.pos.x - left_bottom.pos.x,
                });
            }
            for glyph in &row.glyphs {
                let (left, right) = (glyph.pos.x + offset.x, glyph.max_x() + offset.x);
                span = Some(span.map_or((left, right), |(l, r)| (l.min(left), r.max(right))));
                let logical = glyph.logical_rect().translate(offset);
                shaped.underline_y = logical.bottom();
                shaped.strike_y = logical.center().y;
            }
        }
        shaped.span = span.unwrap_or_default();
        shaped
    }
}

/// Most glyphs a pane keeps before starting over.
const MAX_GLYPHS: usize = 4096;

/// Glyph quads kept across frames. They are positions in egui's font atlas,
/// valid only while egui keeps the same fonts. egui returns the same galley
/// for the same job until it recreates its fonts, which is also when it
/// rebuilds the atlas, so one sentinel galley tells when to start over.
#[derive(Default)]
pub(crate) struct Glyphs {
    font: Option<(egui::FontId, f32)>,
    sentinel: Option<sync::Arc<egui::Galley>>,
    /// ASCII, indexed by `ascii_slot`: hashing every cell costs more than
    /// the rest of the lookup.
    ascii: Vec<Option<Shaped>>,
    /// Everything else, keyed by character, italics, and subpixel bin.
    map: collections::HashMap<(char, bool, u8), Shaped>,
}

/// Slot of an ASCII glyph in `Glyphs::ascii`.
fn ascii_slot(c: char, italics: bool, bin: u8) -> Option<usize> {
    c.is_ascii()
        .then(|| (c as usize) << 3 | usize::from(italics) << 2 | usize::from(bin))
}

impl Glyphs {
    fn validate(&mut self, ui: &egui::Ui, font: &egui::FontId, pixels_per_point: f32) {
        let format = egui::TextFormat::simple(font.clone(), egui::Color32::WHITE);
        let job = egui::text::LayoutJob::simple_format("M".to_owned(), format);
        let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
        let same_fonts = self
            .sentinel
            .as_ref()
            .is_some_and(|sentinel| sync::Arc::ptr_eq(sentinel, &galley));
        let key = (font.clone(), pixels_per_point);
        if !same_fonts || self.font.as_ref() != Some(&key) || self.map.len() > MAX_GLYPHS {
            self.ascii.clear();
            self.map.clear();
        }
        self.ascii.resize(128 << 3, None);
        self.font = Some(key);
        self.sentinel = Some(galley);
    }
}

/// Collects one pane's quads for a frame.
pub(crate) struct Builder<'a> {
    font: egui::FontId,
    pixels_per_point: f32,
    clip: egui::Rect,
    glyphs: &'a mut Glyphs,
    quads: Vec<Quad>,
}

impl<'a> Builder<'a> {
    pub fn new(
        ui: &egui::Ui,
        glyphs: &'a mut Glyphs,
        font: egui::FontId,
        clip: egui::Rect,
    ) -> Self {
        let pixels_per_point = ui.ctx().pixels_per_point();
        glyphs.validate(ui, &font, pixels_per_point);
        Self {
            font,
            pixels_per_point,
            clip,
            glyphs,
            quads: Vec::new(),
        }
    }

    fn push(
        &mut self,
        rect: egui::Rect,
        uv: egui::Rect,
        clip: egui::Rect,
        skew: f32,
        color: egui::Color32,
    ) {
        push(&mut self.quads, self.clip, rect, uv, clip, skew, color);
    }

    /// Same pixels as `Painter::rect_filled` with no rounding.
    pub fn fill(&mut self, rect: egui::Rect, color: egui::Color32) {
        let rect = rect.round_to_pixels(self.pixels_per_point);
        let white = egui::Rect::from_min_max(egui::epaint::WHITE_UV, egui::epaint::WHITE_UV);
        self.push(rect, white, self.clip, 0.0, color);
    }

    /// A horizontal or vertical line of the given width, centered like
    /// `Painter::line_segment` centers it.
    pub fn line(&mut self, from: egui::Pos2, to: egui::Pos2, stroke: egui::Stroke) {
        let half = stroke.width * 0.5;
        let rect = if from.y == to.y {
            let mut y = from.y;
            stroke.round_center_to_pixel(self.pixels_per_point, &mut y);
            egui::Rect::from_x_y_ranges(from.x.min(to.x)..=from.x.max(to.x), y - half..=y + half)
        } else {
            let mut x = from.x;
            stroke.round_center_to_pixel(self.pixels_per_point, &mut x);
            egui::Rect::from_x_y_ranges(x - half..=x + half, from.y.min(to.y)..=from.y.max(to.y))
        };
        let white = egui::Rect::from_min_max(egui::epaint::WHITE_UV, egui::epaint::WHITE_UV);
        self.push(rect, white, self.clip, 0.0, stroke.color);
    }

    /// Same pixels as `Painter::rect_stroke` with `StrokeKind::Inside`.
    pub fn outline(&mut self, rect: egui::Rect, stroke: egui::Stroke) {
        let w = stroke.width;
        let edges = [
            egui::Rect::from_min_max(rect.min, egui::pos2(rect.max.x, rect.min.y + w)),
            egui::Rect::from_min_max(egui::pos2(rect.min.x, rect.max.y - w), rect.max),
            egui::Rect::from_min_max(
                egui::pos2(rect.min.x, rect.min.y + w),
                egui::pos2(rect.min.x + w, rect.max.y - w),
            ),
            egui::Rect::from_min_max(
                egui::pos2(rect.max.x - w, rect.min.y + w),
                egui::pos2(rect.max.x, rect.max.y - w),
            ),
        ];
        for edge in edges {
            self.fill(edge, stroke.color);
        }
    }

    /// Paint a character cluster in a cell, the way a galley at
    /// `cell.left()`, centered in the row, would paint it. `clip` is the cell
    /// span the glyph may not spill out of.
    pub fn text(
        &mut self,
        ui: &egui::Ui,
        cluster: &str,
        cell: egui::Rect,
        clip: egui::Rect,
        style: Style,
        color: egui::Color32,
    ) {
        // egui rasterizes a glyph at one of four subpixel offsets. Bin the
        // cell's own position, so a column renders the same whatever is next
        // to it, and start the galley on the whole pixel.
        let (pixel, bin) = subpixel(cell.left() * self.pixels_per_point);
        let mut chars = cluster.chars();
        let uncached;
        let shaped = match (chars.next(), chars.next()) {
            (None, _) => return,
            // Most cells are one character: cache those.
            (Some(c), None) => {
                let (font, pixels_per_point) = (&self.font, self.pixels_per_point);
                let shape = || layout(ui, font, pixels_per_point, cluster, style.italics, bin);
                match ascii_slot(c, style.italics, bin) {
                    Some(slot) => self.glyphs.ascii[slot].get_or_insert_with(shape),
                    None => self
                        .glyphs
                        .map
                        .entry((c, style.italics, bin))
                        .or_insert_with(shape),
                }
            }
            _ => {
                uncached = layout(
                    ui,
                    &self.font,
                    self.pixels_per_point,
                    cluster,
                    style.italics,
                    bin,
                );
                &uncached
            }
        };
        let top = cell.top() + (cell.height() - shaped.height) * 0.5;
        let origin = egui::vec2(
            pixel / self.pixels_per_point,
            top.round_to_pixels(self.pixels_per_point),
        );
        for glyph in &shaped.quads {
            push(
                &mut self.quads,
                self.clip,
                egui::Rect::from_min_max(glyph.min + origin, glyph.max + origin),
                egui::Rect::from_min_max(glyph.uv_min, glyph.uv_max),
                clip,
                glyph.skew,
                color,
            );
        }
        let stroke = egui::Stroke::new(1.0_f32, color);
        let (left, right) = (shaped.span.0 + origin.x, shaped.span.1 + origin.x);
        let lines = [
            style.underline.then_some(shaped.underline_y),
            style.strikethrough.then_some(shaped.strike_y),
        ];
        for y in lines.into_iter().flatten() {
            let y = y + origin.y;
            let half = stroke.width * 0.5;
            let mut center = y;
            stroke.round_center_to_pixel(self.pixels_per_point, &mut center);
            let rect = egui::Rect::from_x_y_ranges(left..=right, center - half..=center + half);
            let white = egui::Rect::from_min_max(egui::epaint::WHITE_UV, egui::epaint::WHITE_UV);
            push(&mut self.quads, self.clip, rect, white, clip, 0.0, color);
        }
    }

    /// The paint callback carrying this batch, or nothing if it is empty.
    pub fn finish(self) -> Option<egui::Shape> {
        (!self.quads.is_empty()).then(|| {
            egui::Shape::Callback(egui::PaintCallback {
                rect: self.clip,
                callback: sync::Arc::new(Batch { quads: self.quads }),
            })
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn push(
    quads: &mut Vec<Quad>,
    pane: egui::Rect,
    rect: egui::Rect,
    uv: egui::Rect,
    clip: egui::Rect,
    skew: f32,
    color: egui::Color32,
) {
    let clip = clip.intersect(pane);
    if !clip.is_positive() || (!rect.intersects(clip) && skew == 0.0) {
        return;
    }
    quads.push(Quad {
        min: rect.min.into(),
        max: rect.max.into(),
        uv_min: uv.min.into(),
        uv_max: uv.max.into(),
        clip_min: clip.min.into(),
        clip_max: clip.max.into(),
        color: u32::from_le_bytes(color.to_array()),
        skew,
        padding: [0.0; 2],
    });
}

/// Lay out a cluster at a subpixel bin through egui, which rasterizes it into
/// its atlas if needed.
fn layout(
    ui: &egui::Ui,
    font: &egui::FontId,
    pixels_per_point: f32,
    cluster: &str,
    italics: bool,
    bin: u8,
) -> Shaped {
    let format = egui::TextFormat {
        font_id: font.clone(),
        color: egui::Color32::WHITE,
        italics,
        ..Default::default()
    };
    let mut job = egui::text::LayoutJob::default();
    job.append(cluster, f32::from(bin) * 0.25 / pixels_per_point, format);
    Shaped::new(&ui.fonts_mut(|fonts| fonts.layout_job(job)))
}

/// Whole pixel and quarter-pixel bin of a physical x, as epaint bins glyphs.
fn subpixel(x: f32) -> (f32, u8) {
    let whole = x.floor();
    match ((x - whole) * 4.0).round() as u8 {
        4 => (whole + 1.0, 0),
        bin => (whole, bin),
    }
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Zeroable, bytemuck::Pod)]
struct Uniforms {
    screen_size: [f32; 2],
    atlas_size: [f32; 2],
}

#[derive(blade_macros::ShaderData)]
struct Globals {
    r_uniforms: Uniforms,
}

#[derive(blade_macros::ShaderData)]
struct Locals {
    r_quads: bg::BufferPiece,
    r_texture: bg::TextureView,
    r_sampler: bg::Sampler,
}

/// A copy of egui's font atlas. blade-egui keeps its textures private, so the
/// grid mirrors the one it samples from the same texture deltas.
struct Atlas {
    texture: bg::Texture,
    view: bg::TextureView,
    sampler: bg::Sampler,
    size: [u32; 2],
}

impl Atlas {
    fn new(context: &bg::Context, size: [u32; 2], options: egui::TextureOptions) -> Self {
        let format = bg::TextureFormat::Rgba8Unorm;
        let extent = bg::Extent {
            width: size[0],
            height: size[1],
            depth: 1,
        };
        let texture = context.create_texture(bg::TextureDesc {
            name: "terminal glyphs",
            format,
            size: extent,
            array_layer_count: 1,
            mip_level_count: 1,
            dimension: bg::TextureDimension::D2,
            usage: bg::TextureUsage::COPY | bg::TextureUsage::RESOURCE,
            sample_count: 1,
            external: None,
        });
        let view = context.create_texture_view(
            texture,
            bg::TextureViewDesc {
                name: "terminal glyphs",
                format,
                dimension: bg::ViewDimension::D2,
                subresources: &bg::TextureSubresources::default(),
            },
        );
        let filter = |filter| match filter {
            egui::TextureFilter::Nearest => bg::FilterMode::Nearest,
            egui::TextureFilter::Linear => bg::FilterMode::Linear,
        };
        let sampler = context.create_sampler(bg::SamplerDesc {
            name: "terminal glyphs",
            address_modes: [bg::AddressMode::ClampToEdge; 3],
            mag_filter: filter(options.magnification),
            min_filter: filter(options.minification),
            ..Default::default()
        });
        Self {
            texture,
            view,
            sampler,
            size,
        }
    }

    fn destroy(self, context: &bg::Context) {
        context.destroy_texture_view(self.view);
        context.destroy_texture(self.texture);
        context.destroy_sampler(self.sampler);
    }
}

/// Draws pane batches between blade-egui's primitives.
pub(crate) struct Renderer {
    pipeline: bg::RenderPipeline,
    belt: blade_util::BufferBelt,
    atlas: Option<Atlas>,
    dropped: Vec<Atlas>,
    retired: Vec<(Atlas, bg::SyncPoint)>,
}

impl Renderer {
    pub fn new(info: bg::SurfaceInfo, context: &bg::Context) -> Self {
        let shader = context.create_shader(bg::ShaderDesc {
            source: SHADER_SOURCE,
            naga_module: None,
        });
        let globals_layout = <Globals as bg::ShaderData>::layout();
        let locals_layout = <Locals as bg::ShaderData>::layout();
        let pipeline = context.create_render_pipeline(bg::RenderPipelineDesc {
            name: "terminal grid",
            data_layouts: &[&globals_layout, &locals_layout],
            vertex: shader.at("vs_main"),
            vertex_fetches: &[],
            primitive: bg::PrimitiveState {
                topology: bg::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            fragment: Some(shader.at("fs_main")),
            // blade-egui's blending, so overlapping egui and grid pixels agree.
            color_targets: &[bg::ColorTargetState {
                format: info.format,
                blend: Some(bg::BlendState {
                    color: bg::BlendComponent {
                        src_factor: bg::BlendFactor::One,
                        dst_factor: bg::BlendFactor::OneMinusSrcAlpha,
                        operation: bg::BlendOperation::Add,
                    },
                    alpha: bg::BlendComponent {
                        src_factor: bg::BlendFactor::OneMinusDstAlpha,
                        dst_factor: bg::BlendFactor::One,
                        operation: bg::BlendOperation::Add,
                    },
                }),
                write_mask: bg::ColorWrites::all(),
            }],
            multisample_state: Default::default(),
        });
        Self {
            pipeline,
            belt: blade_util::BufferBelt::new(blade_util::BufferBeltDescriptor {
                memory: bg::Memory::Shared,
                min_chunk_size: 0x10000,
                alignment: bg::limits::STORAGE_BUFFER_ALIGNMENT,
            }),
            atlas: None,
            dropped: Vec::new(),
            retired: Vec::new(),
        }
    }

    pub fn destroy(&mut self, context: &bg::Context) {
        context.destroy_render_pipeline(&mut self.pipeline);
        self.belt.destroy(context);
        let retired = self.retired.drain(..).map(|(atlas, _)| atlas);
        for atlas in self
            .atlas
            .take()
            .into_iter()
            .chain(self.dropped.drain(..))
            .chain(retired)
        {
            atlas.destroy(context);
        }
    }

    /// Apply egui's font atlas changes. Call next to
    /// `GuiPainter::update_textures`, with the same delta.
    pub fn update_atlas(
        &mut self,
        encoder: &mut bg::CommandEncoder,
        delta: &egui::TexturesDelta,
        context: &bg::Context,
    ) {
        let done = self
            .retired
            .iter()
            .position(|retired| !context.wait_for(&retired.1, 0).unwrap_or(true))
            .unwrap_or(self.retired.len());
        for (atlas, _) in self.retired.drain(..done) {
            atlas.destroy(context);
        }
        let mut copies = Vec::new();
        for &(id, ref image) in &delta.set {
            if id != egui::TextureId::default() {
                continue;
            }
            let egui::ImageData::Color(ref pixels) = image.image;
            let [width, height] = image.image.size().map(|side| side as u32);
            let atlas = match image.pos {
                // A whole image replaces the atlas, possibly at a new size.
                None => {
                    let atlas = Atlas::new(context, [width, height], image.options);
                    encoder.init_texture(atlas.texture);
                    if let Some(old) = self.atlas.replace(atlas) {
                        self.dropped.push(old);
                    }
                    self.atlas.as_ref()
                }
                Some(_) => self.atlas.as_ref(),
            };
            let Some(atlas) = atlas else {
                log::warn!("egui patched a font atlas the grid never received");
                continue;
            };
            let origin = image.pos.map_or([0; 3], |[x, y]| [x as u32, y as u32, 0]);
            copies.push((
                self.belt.alloc_pod(pixels.pixels.as_slice(), context),
                bg::TexturePiece {
                    texture: atlas.texture,
                    mip_level: 0,
                    array_layer: 0,
                    origin,
                },
                bg::Extent {
                    width,
                    height,
                    depth: 1,
                },
            ));
        }
        if !copies.is_empty() {
            let mut transfer = encoder.transfer("terminal glyphs");
            for (source, destination, extent) in copies {
                transfer.copy_buffer_to_texture(source, 4 * extent.width, destination, extent);
            }
        }
    }

    /// Paint egui's primitives, drawing each grid batch at its place in them.
    pub fn paint(
        &mut self,
        pass: &mut bg::RenderCommandEncoder,
        painter: &mut be::GuiPainter,
        jobs: &[egui::ClippedPrimitive],
        screen: &be::ScreenDescriptor,
        context: &bg::Context,
    ) {
        let mut start = 0;
        for (index, job) in jobs.iter().enumerate() {
            let egui::epaint::Primitive::Callback(ref callback) = job.primitive else {
                continue;
            };
            let Some(batch) = callback.callback.downcast_ref::<Batch>() else {
                continue;
            };
            painter.paint(pass, &jobs[start..index], screen, context);
            self.draw(pass, batch, job.clip_rect, screen, context);
            start = index + 1;
        }
        painter.paint(pass, &jobs[start..], screen, context);
    }

    fn draw(
        &mut self,
        pass: &mut bg::RenderCommandEncoder,
        batch: &Batch,
        clip: egui::Rect,
        screen: &be::ScreenDescriptor,
        context: &bg::Context,
    ) {
        let Some(ref atlas) = self.atlas else {
            return;
        };
        let scale = screen.scale_factor;
        let (width, height) = screen.physical_size;
        let min_x = (scale * clip.min.x).clamp(0.0, width as f32).trunc() as i32;
        let min_y = (scale * clip.min.y).clamp(0.0, height as f32).trunc() as i32;
        let max_x = (scale * clip.max.x).clamp(0.0, width as f32).ceil() as i32;
        let max_y = (scale * clip.max.y).clamp(0.0, height as f32).ceil() as i32;
        if batch.quads.is_empty() || max_x <= min_x || max_y <= min_y {
            return;
        }
        let quads = self.belt.alloc_pod(&batch.quads, context);
        let mut encoder = pass.with(&self.pipeline);
        encoder.bind(
            0,
            &Globals {
                r_uniforms: Uniforms {
                    screen_size: [width as f32 / scale, height as f32 / scale],
                    atlas_size: atlas.size.map(|side| side as f32),
                },
            },
        );
        encoder.bind(
            1,
            &Locals {
                r_quads: quads,
                r_texture: atlas.view,
                r_sampler: atlas.sampler,
            },
        );
        encoder.set_scissor_rect(&bg::ScissorRect {
            x: min_x,
            y: min_y,
            w: (max_x - min_x) as u32,
            h: (max_y - min_y) as u32,
        });
        encoder.draw(0, 6 * batch.quads.len() as u32, 0, 1);
    }

    /// Call after submitting the frame at `sync_point`.
    pub fn after_submit(&mut self, sync_point: &bg::SyncPoint) {
        self.retired.extend(
            self.dropped
                .drain(..)
                .map(|atlas| (atlas, sync_point.clone())),
        );
        self.belt.flush(sync_point);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subpixel_bins_like_epaint() {
        assert_eq!(subpixel(10.0), (10.0, 0));
        assert_eq!(subpixel(10.1), (10.0, 0));
        assert_eq!(subpixel(10.3), (10.0, 1));
        assert_eq!(subpixel(10.5), (10.0, 2));
        assert_eq!(subpixel(10.7), (10.0, 3));
        assert_eq!(subpixel(10.9), (11.0, 0));
    }

    #[test]
    fn quad_matches_the_shader_layout() {
        // 16 scalars in grid.wgsl's `Quad`.
        assert_eq!(std::mem::size_of::<Quad>(), 64);
    }

    fn frame(ctx: &egui::Context, paint: impl FnMut(&mut egui::Ui)) {
        let mut paint = paint;
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| paint(ui));
    }

    #[test]
    fn quads_outside_the_pane_are_dropped() {
        let ctx = egui::Context::default();
        let mut glyphs = Glyphs::default();
        frame(&ctx, |ui| {
            let pane = egui::Rect::from_min_size(egui::pos2(10.0, 10.0), egui::vec2(100.0, 50.0));
            let mut builder = Builder::new(ui, &mut glyphs, egui::FontId::monospace(13.0), pane);
            let size = egui::vec2(8.0, 16.0);
            builder.fill(
                egui::Rect::from_min_size(egui::pos2(200.0, 10.0), size),
                egui::Color32::RED,
            );
            builder.fill(
                egui::Rect::from_min_size(egui::pos2(20.0, 10.0), size),
                egui::Color32::RED,
            );
            assert_eq!(builder.quads.len(), 1);
            assert_eq!(builder.quads[0].clip_min, [10.0, 10.0]);
        });
    }

    #[test]
    fn glyph_cache_lives_until_egui_recreates_its_fonts() {
        let ctx = egui::Context::default();
        let mut glyphs = Glyphs::default();
        let pane = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(200.0, 40.0));
        let cell = egui::Rect::from_min_size(egui::pos2(8.0, 4.0), egui::vec2(8.0, 18.0));
        let paint = |glyphs: &mut Glyphs| {
            frame(&ctx, |ui| {
                let mut builder = Builder::new(ui, glyphs, egui::FontId::monospace(13.0), pane);
                builder.text(ui, "M", cell, cell, Style::default(), egui::Color32::WHITE);
            });
        };
        paint(&mut glyphs);
        let first = glyphs.sentinel.clone().unwrap();
        paint(&mut glyphs);
        assert!(sync::Arc::ptr_eq(&first, glyphs.sentinel.as_ref().unwrap()));
        let slot = ascii_slot('M', false, 0).unwrap();
        assert!(glyphs.ascii[slot].is_some());
        // Different font definitions rebuild egui's fonts and atlas, even
        // when the monospace family itself is unchanged.
        let mut fonts = egui::FontDefinitions::default();
        if let Some(family) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
            family.reverse();
        }
        ctx.set_fonts(fonts);
        glyphs.map.insert(('é', false, 0), Shaped::default());
        let stale = ascii_slot('x', false, 0).unwrap();
        glyphs.ascii[stale] = Some(Shaped::default());
        paint(&mut glyphs);
        assert!(!sync::Arc::ptr_eq(
            &first,
            glyphs.sentinel.as_ref().unwrap()
        ));
        assert!(glyphs.map.is_empty(), "stale atlas positions dropped");
        assert!(
            glyphs.ascii[stale].is_none(),
            "stale atlas positions dropped"
        );
    }

    #[test]
    fn glyphs_come_from_egui_layout() {
        let ctx = egui::Context::default();
        let mut glyphs = Glyphs::default();
        let pane = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(200.0, 40.0));
        let mut quads = 0;
        frame(&ctx, |ui| {
            let mut builder = Builder::new(ui, &mut glyphs, egui::FontId::monospace(13.0), pane);
            let cell = egui::Rect::from_min_size(egui::pos2(8.0, 4.0), egui::vec2(8.0, 18.0));
            builder.text(ui, "M", cell, cell, Style::default(), egui::Color32::WHITE);
            builder.text(ui, " ", cell, cell, Style::default(), egui::Color32::WHITE);
            builder.text(
                ui,
                "_",
                cell,
                cell,
                Style {
                    underline: true,
                    ..Style::default()
                },
                egui::Color32::WHITE,
            );
            quads = builder.quads.len();
            let glyph = builder.quads[0];
            assert!(glyph.uv_max[0] > glyph.uv_min[0], "a real atlas region");
            assert!(
                glyph.min[0] >= 8.0 && glyph.max[1] <= 22.0,
                "inside its cell"
            );
        });
        // 'M', nothing for the space, '_' plus its underline.
        assert_eq!(quads, 3);
    }
}
