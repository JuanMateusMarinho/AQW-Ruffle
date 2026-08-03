use crate::backend::RenderTargetMode;
use crate::blend::TrivialBlend;
use crate::blend::{BlendType, ComplexBlend};
use crate::buffer_builder::BufferBuilder;
use crate::buffer_pool::{TexturePool, quantize_pool_dimension};
use crate::content_bounds::ContentBounds;
use crate::dynamic_transforms::DynamicTransforms;
use crate::mesh::{DrawType, Mesh, as_mesh};
use crate::surface::Surface;
use crate::surface::target::CommandTarget;
use crate::{Descriptors, MaskState, Pipelines, Transforms, as_texture};
use ruffle_render::backend::ShapeHandle;
use ruffle_render::bitmap::{BitmapHandle, PixelSnapping};
use ruffle_render::commands::{Command, CommandHandler, CommandList, RenderBlendMode};
use ruffle_render::lines::{emulate_line, emulate_line_rect};
use ruffle_render::matrix::Matrix;
use ruffle_render::pixel_bender::PixelBenderShaderHandle;
use ruffle_render::quality::StageQuality;
use ruffle_render::transform::Transform;
use std::mem;
use std::sync::OnceLock;
use swf::{BlendMode, Color, ColorTransform, Twips};
use wgpu::Backend;

/// Kill-switch: `RUFFLE_AQW_NO_BLEND_TARGET_SHRINK` restores full-surface
/// complex blend targets, for field A/B without a rebuild.
fn blend_target_shrink_disabled() -> bool {
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED.get_or_init(|| {
        ruffle_render::backend::aqw_env_flag("RUFFLE_AQW_NO_BLEND_TARGET_SHRINK", false)
    })
}

/// MEASUREMENT ONLY -- `RUFFLE_AQW_BLEND_AS_NORMAL` demotes every complex blend
/// to a plain one, which is visually wrong but keeps the art on screen.
///
/// A complex blend costs a render pass of its own to composite; a trivial one
/// folds into the batched draw chunk. Toggling this therefore prices the
/// compositing passes alone, with the per-blend target and its sub-render
/// unchanged, which is what says whether collapsing those passes is worth
/// building. Never a fix: it is the wrong blend maths.
fn blend_measured_as_normal() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED
        .get_or_init(|| ruffle_render::backend::aqw_env_flag("RUFFLE_AQW_BLEND_AS_NORMAL", false))
}

/// MEASUREMENT ONLY -- `RUFFLE_AQW_SKIP_COMPLEX_BLEND` drops complex blends
/// entirely: no target, no sub-render, no composite. The blended art simply
/// does not appear.
///
/// This is the floor. Whatever frame time remains belongs to the rest of the
/// scene, so it says how much of the budget attacking blends could ever
/// recover -- and therefore whether any amount of that work reaches 24fps.
fn blend_measured_skipped() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        ruffle_render::backend::aqw_env_flag("RUFFLE_AQW_SKIP_COMPLEX_BLEND", false)
    })
}

use super::target::PoolOrArcTexture;

pub struct CommandRenderer<'pass, 'frame: 'pass, 'global: 'frame> {
    pipelines: &'frame Pipelines,
    descriptors: &'global Descriptors,
    num_masks: u32,
    mask_state: MaskState,
    render_pass: wgpu::RenderPass<'pass>,
    needs_stencil: bool,
    dynamic_transforms: &'global DynamicTransforms,
}

impl<'pass, 'frame: 'pass, 'global: 'frame> CommandRenderer<'pass, 'frame, 'global> {
    pub fn new(
        pipelines: &'frame Pipelines,
        descriptors: &'global Descriptors,
        dynamic_transforms: &'global DynamicTransforms,
        render_pass: wgpu::RenderPass<'pass>,
        num_masks: u32,
        mask_state: MaskState,
        needs_stencil: bool,
    ) -> Self {
        Self {
            pipelines,
            num_masks,
            mask_state,
            render_pass,
            descriptors,
            needs_stencil,
            dynamic_transforms,
        }
    }

    pub fn execute(&mut self, command: &'frame DrawCommand) {
        if self.needs_stencil {
            match self.mask_state {
                MaskState::NoMask => {}
                MaskState::DrawMaskStencil => {
                    self.render_pass.set_stencil_reference(self.num_masks - 1);
                }
                MaskState::DrawMaskedContent => {
                    self.render_pass.set_stencil_reference(self.num_masks);
                }
                MaskState::ClearMaskStencil => {
                    self.render_pass.set_stencil_reference(self.num_masks);
                }
            }
        }

        match command {
            DrawCommand::RenderBitmap {
                bitmap,
                transform_buffer,
                smoothing,
                blend_mode,
                render_stage3d,
            } => self.render_bitmap(
                bitmap,
                *transform_buffer,
                *smoothing,
                *blend_mode,
                *render_stage3d,
            ),
            DrawCommand::RenderTexture {
                _texture,
                binds,
                transform_buffer,
                blend_mode,
            } => self.render_texture(*transform_buffer, binds, *blend_mode),
            DrawCommand::RenderShape {
                shape,
                transform_buffer,
            } => self.render_shape(shape, *transform_buffer),
            DrawCommand::DrawRect { transform_buffer } => self.draw_rect(*transform_buffer),
            DrawCommand::DrawLine { transform_buffer } => {
                self.draw_lines::<false>(*transform_buffer)
            }
            DrawCommand::DrawLineRect { transform_buffer } => {
                self.draw_lines::<true>(*transform_buffer)
            }
            DrawCommand::PushMask => self.push_mask(),
            DrawCommand::ActivateMask => self.activate_mask(),
            DrawCommand::DeactivateMask => self.deactivate_mask(),
            DrawCommand::PopMask => self.pop_mask(),
            DrawCommand::RenderAlphaMask {
                maskee,
                mask,
                binds,
                transform_buffer,
            } => self.render_alpha_mask(maskee, mask, binds, *transform_buffer),
        }
    }

    pub fn prep_color(&mut self) {
        if self.needs_stencil {
            self.render_pass
                .set_pipeline(self.pipelines.color.pipeline_for(self.mask_state));
        } else {
            self.render_pass
                .set_pipeline(self.pipelines.color.stencilless_pipeline());
        }
    }

    pub fn prep_lines(&mut self) {
        if self.needs_stencil {
            self.render_pass
                .set_pipeline(self.pipelines.lines.pipeline_for(self.mask_state));
        } else {
            self.render_pass
                .set_pipeline(self.pipelines.lines.stencilless_pipeline());
        }
    }

    pub fn prep_gradient(&mut self, bind_group: &'pass wgpu::BindGroup) {
        if self.needs_stencil {
            self.render_pass
                .set_pipeline(self.pipelines.gradients.pipeline_for(self.mask_state));
        } else {
            self.render_pass
                .set_pipeline(self.pipelines.gradients.stencilless_pipeline());
        }

        self.render_pass.set_bind_group(2, bind_group, &[]);
    }

    pub fn prep_bitmap(
        &mut self,
        bind_group: &'pass wgpu::BindGroup,
        blend_mode: TrivialBlend,
        render_stage3d: bool,
    ) {
        match (self.needs_stencil, render_stage3d) {
            (true, true) => {
                self.render_pass
                    .set_pipeline(&self.pipelines.bitmap_opaque_dummy_stencil);
            }
            (true, false) => {
                self.render_pass
                    .set_pipeline(self.pipelines.bitmap[blend_mode].pipeline_for(self.mask_state));
            }
            (false, true) => {
                self.render_pass.set_pipeline(&self.pipelines.bitmap_opaque);
            }
            (false, false) => {
                self.render_pass
                    .set_pipeline(self.pipelines.bitmap[blend_mode].stencilless_pipeline());
            }
        }

        self.render_pass.set_bind_group(2, bind_group, &[]);
    }

    pub fn prep_alpha_mask(&mut self, bind_group: &'pass wgpu::BindGroup) {
        if self.needs_stencil {
            self.render_pass
                .set_pipeline(self.pipelines.alpha_mask.pipeline_for(self.mask_state));
        } else {
            self.render_pass
                .set_pipeline(self.pipelines.alpha_mask.stencilless_pipeline());
        }

        self.render_pass.set_bind_group(2, bind_group, &[]);
    }

    pub fn draw(
        &mut self,
        vertices: wgpu::BufferSlice<'pass>,
        indices: wgpu::BufferSlice<'pass>,
        num_indices: u32,
    ) {
        self.render_pass.set_vertex_buffer(0, vertices);
        self.render_pass
            .set_index_buffer(indices, wgpu::IndexFormat::Uint32);

        self.render_pass.draw_indexed(0..num_indices, 0, 0..1);
    }

    pub fn render_bitmap(
        &mut self,
        bitmap: &'frame BitmapHandle,
        transform_buffer: wgpu::DynamicOffset,
        smoothing: bool,
        blend_mode: TrivialBlend,
        render_stage3d: bool,
    ) {
        if cfg!(feature = "render_debug_labels") {
            self.render_pass
                .push_debug_group(&format!("render_bitmap {:?}", bitmap.0));
        }
        let texture = as_texture(bitmap);

        let descriptors = self.descriptors;
        let bind = texture.bind_group(
            smoothing,
            &descriptors.device,
            &descriptors.bind_layouts.bitmap,
            &descriptors.quad,
            bitmap.clone(),
            &descriptors.bitmap_samplers,
        );
        self.prep_bitmap(&bind.bind_group, blend_mode, render_stage3d);
        self.render_pass.set_bind_group(
            1,
            &self.dynamic_transforms.bind_group,
            &[transform_buffer],
        );

        self.draw(
            self.descriptors.quad.vertices_pos.slice(..),
            self.descriptors.quad.indices.slice(..),
            6,
        );
        if cfg!(feature = "render_debug_labels") {
            self.render_pass.pop_debug_group();
        }
    }

    pub fn render_texture(
        &mut self,
        transform_buffer: wgpu::DynamicOffset,
        bind_group: &'frame wgpu::BindGroup,
        blend_mode: TrivialBlend,
    ) {
        if cfg!(feature = "render_debug_labels") {
            self.render_pass.push_debug_group("render_texture");
        }
        self.prep_bitmap(bind_group, blend_mode, false);

        self.render_pass.set_bind_group(
            1,
            &self.dynamic_transforms.bind_group,
            &[transform_buffer],
        );

        self.draw(
            self.descriptors.quad.vertices_pos.slice(..),
            self.descriptors.quad.indices.slice(..),
            6,
        );
        if cfg!(feature = "render_debug_labels") {
            self.render_pass.pop_debug_group();
        }
    }

    pub fn render_shape(
        &mut self,
        shape: &'frame ShapeHandle,
        transform_buffer: wgpu::DynamicOffset,
    ) {
        if cfg!(feature = "render_debug_labels") {
            self.render_pass.push_debug_group("render_shape");
        }

        let mesh = as_mesh(shape);
        for draw in &mesh.draws {
            let num_indices = if self.mask_state != MaskState::DrawMaskStencil
                && self.mask_state != MaskState::ClearMaskStencil
            {
                draw.num_indices
            } else {
                // Omit strokes when drawing a mask stencil.
                draw.num_mask_indices
            };
            if num_indices == 0 {
                continue;
            }

            match &draw.draw_type {
                DrawType::Color => {
                    self.prep_color();
                }
                DrawType::Gradient { bind_group, .. } => {
                    self.prep_gradient(bind_group);
                }
                DrawType::Bitmap { binds, .. } => {
                    self.prep_bitmap(&binds.bind_group, TrivialBlend::Normal, false);
                }
            }
            self.render_pass.set_bind_group(
                1,
                &self.dynamic_transforms.bind_group,
                &[transform_buffer],
            );

            self.draw(
                mesh.vertex_buffer.slice(draw.vertices.clone()),
                mesh.index_buffer.slice(draw.indices.clone()),
                num_indices,
            );
        }
        if cfg!(feature = "render_debug_labels") {
            self.render_pass.pop_debug_group();
        }
    }

    pub fn render_alpha_mask(
        &mut self,
        _maskee: &PoolOrArcTexture,
        _mask: &PoolOrArcTexture,
        bind_group: &'frame wgpu::BindGroup,
        transform_buffer: wgpu::DynamicOffset,
    ) {
        if cfg!(feature = "render_debug_labels") {
            self.render_pass.push_debug_group("render_alpha_mask");
        }

        self.prep_alpha_mask(bind_group);

        self.render_pass.set_bind_group(
            1,
            &self.dynamic_transforms.bind_group,
            &[transform_buffer],
        );

        self.draw(
            self.descriptors.quad.vertices_pos.slice(..),
            self.descriptors.quad.indices.slice(..),
            6,
        );

        if cfg!(feature = "render_debug_labels") {
            self.render_pass.pop_debug_group();
        }
    }

    pub fn draw_rect(&mut self, transform_buffer: wgpu::DynamicOffset) {
        if cfg!(feature = "render_debug_labels") {
            self.render_pass.push_debug_group("draw_rect");
        }
        self.prep_color();

        self.render_pass.set_bind_group(
            1,
            &self.dynamic_transforms.bind_group,
            &[transform_buffer],
        );

        self.draw(
            self.descriptors.quad.vertices_pos_color.slice(..),
            self.descriptors.quad.indices.slice(..),
            6,
        );
        if cfg!(feature = "render_debug_labels") {
            self.render_pass.pop_debug_group();
        }
    }

    pub fn draw_lines<const RECT: bool>(&mut self, transform_buffer: wgpu::DynamicOffset) {
        if cfg!(feature = "render_debug_labels") {
            self.render_pass.push_debug_group("draw_lines");
        }
        self.prep_lines();

        self.render_pass.set_bind_group(
            1,
            &self.dynamic_transforms.bind_group,
            &[transform_buffer],
        );

        self.draw(
            self.descriptors.quad.vertices_pos_color.slice(..),
            if RECT {
                self.descriptors.quad.indices_line_rect.slice(..)
            } else {
                self.descriptors.quad.indices_line.slice(..)
            },
            if RECT { 5 } else { 2 },
        );
        if cfg!(feature = "render_debug_labels") {
            self.render_pass.pop_debug_group();
        }
    }

    pub fn push_mask(&mut self) {
        debug_assert!(
            self.mask_state == MaskState::NoMask || self.mask_state == MaskState::DrawMaskedContent
        );
        self.num_masks += 1;
        self.mask_state = MaskState::DrawMaskStencil;
        self.render_pass.set_stencil_reference(self.num_masks - 1);
    }

    pub fn activate_mask(&mut self) {
        debug_assert!(self.num_masks > 0 && self.mask_state == MaskState::DrawMaskStencil);
        self.mask_state = MaskState::DrawMaskedContent;
        self.render_pass.set_stencil_reference(self.num_masks);
    }

    pub fn deactivate_mask(&mut self) {
        debug_assert!(self.num_masks > 0 && self.mask_state == MaskState::DrawMaskedContent);
        self.mask_state = MaskState::ClearMaskStencil;
        self.render_pass.set_stencil_reference(self.num_masks);
    }

    pub fn pop_mask(&mut self) {
        debug_assert!(self.num_masks > 0 && self.mask_state == MaskState::ClearMaskStencil);
        self.num_masks -= 1;
        self.render_pass.set_stencil_reference(self.num_masks);
        if self.num_masks == 0 {
            self.mask_state = MaskState::NoMask;
        } else {
            self.mask_state = MaskState::DrawMaskedContent;
        };
    }

    pub fn num_masks(&self) -> u32 {
        self.num_masks
    }

    pub fn mask_state(&self) -> MaskState {
        self.mask_state
    }
}

pub enum Chunk {
    Draw {
        chunk: Vec<DrawCommand>,
        needs_stencil: bool,
        transforms: BufferBuilder,
    },
    Blend {
        texture: PoolOrArcTexture,
        blend_mode: ChunkBlendMode,
        needs_stencil: bool,
        /// Extent of the content drawn into `texture`, in surface pixels.
        ///
        /// The rest of `texture` is the clear colour, which is transparent for
        /// every blend mode, and every complex blend shader discards on
        /// `src.a <= 0`. So the pass can be scissored to this without changing
        /// a pixel — see `Surface::draw_commands`.
        bounds: ContentBounds,
        /// Footprint of `texture` in the target being composited into, as
        /// `(x, y, width, height)`.
        ///
        /// The whole target unless the blend was sized to its content, in which
        /// case the pass draws only this rect and `texture` covers exactly it —
        /// so the unit quad doubles as the source's texture coordinate.
        rect: (u32, u32, u32, u32),
    },
}

#[derive(Debug)]
pub enum ChunkBlendMode {
    Complex(ComplexBlend),
    Shader(PixelBenderShaderHandle),
}

#[derive(Debug)]
pub enum DrawCommand {
    RenderBitmap {
        bitmap: BitmapHandle,
        transform_buffer: wgpu::DynamicOffset,
        smoothing: bool,
        blend_mode: TrivialBlend,
        render_stage3d: bool,
    },
    RenderTexture {
        _texture: PoolOrArcTexture,
        binds: wgpu::BindGroup,
        transform_buffer: wgpu::DynamicOffset,
        blend_mode: TrivialBlend,
    },
    RenderAlphaMask {
        maskee: PoolOrArcTexture,
        mask: PoolOrArcTexture,
        binds: wgpu::BindGroup,
        transform_buffer: wgpu::DynamicOffset,
    },
    RenderShape {
        shape: ShapeHandle,
        transform_buffer: wgpu::DynamicOffset,
    },
    DrawRect {
        transform_buffer: wgpu::DynamicOffset,
    },
    DrawLine {
        transform_buffer: wgpu::DynamicOffset,
    },
    DrawLineRect {
        transform_buffer: wgpu::DynamicOffset,
    },
    PushMask,
    ActivateMask,
    DeactivateMask,
    PopMask,
}

#[derive(Copy, Clone)]
pub enum LayerRef<'a> {
    None,
    Current,
    Parent(&'a CommandTarget),
}

/// Replaces every blend with a RenderBitmap, with the subcommands rendered out to a temporary texture
/// Every complex blend will be its own item, but every other draw will be chunked together
///
/// Also returns the extent of everything drawn, in target pixels, which is what
/// lets a caller bound its own blend pass when these commands are themselves the
/// contents of a blend.
#[expect(clippy::too_many_arguments)]
pub fn chunk_blends<'a>(
    commands: CommandList,
    descriptors: &'a Descriptors,
    staging_belt: &'a mut wgpu::util::StagingBelt,
    dynamic_transforms: &'a DynamicTransforms,
    draw_encoder: &mut wgpu::CommandEncoder,
    meshes: &'a Vec<Mesh>,
    quality: StageQuality,
    width: u32,
    height: u32,
    nearest_layer: LayerRef,
    texture_pool: &mut TexturePool,
    origin: (u32, u32),
) -> (Vec<Chunk>, ContentBounds) {
    WgpuCommandHandler::new(
        descriptors,
        staging_belt,
        dynamic_transforms,
        draw_encoder,
        meshes,
        quality,
        width,
        height,
        nearest_layer,
        texture_pool,
        origin,
    )
    .chunk_blends(commands)
}

/// Extent a command list will cover, without rendering any of it.
///
/// A blend target has to be sized before its contents are drawn, so the bounds
/// the chunker accumulates as a side effect come too late. This walks the same
/// geometry ahead of time: matrices are already in surface pixels, quad-shaped
/// draws cover the unit square, and shapes carry their tessellated extent.
///
/// Conservative on purpose -- masks bound by their masker rather than the
/// intersection, and anything unbounded poisons the result to the full surface,
/// which just means a pass keeps its old full-size target.
pub fn command_list_bounds(commands: &CommandList) -> ContentBounds {
    fn visit(commands: &CommandList, out: &mut ContentBounds, depth: u32) {
        // Matches the recursion guard in `CommandList::execute`; a list too
        // deep to render should not be measured either.
        if depth > 64 {
            *out = ContentBounds::UNBOUNDED;
            return;
        }

        for command in &commands.commands {
            match command {
                Command::RenderBitmap {
                    bitmap, transform, ..
                }
                | Command::RenderStage3D { bitmap, transform } => {
                    let texture = as_texture(bitmap);
                    let mut matrix = transform.matrix;
                    matrix *= Matrix::scale(
                        texture.texture.width() as f32,
                        texture.texture.height() as f32,
                    );
                    out.union_transformed(&matrix, ContentBounds::UNIT);
                }
                Command::RenderShape { shape, transform } => {
                    out.union_transformed(&transform.matrix, as_mesh(shape).bounds);
                }
                Command::DrawRect { matrix, .. }
                | Command::DrawLine { matrix, .. }
                | Command::DrawLineRect { matrix, .. } => {
                    out.union_transformed(matrix, ContentBounds::UNIT);
                }
                Command::Blend(inner, _) => visit(inner, out, depth + 1),
                Command::RenderAlphaMask {
                    maskee_commands, ..
                } => {
                    // Output is `maskee.rgb * mask.a`, so the maskee bounds it.
                    visit(maskee_commands, out, depth + 1)
                }
                Command::PushMask
                | Command::ActivateMask
                | Command::DeactivateMask
                | Command::PopMask => {}
            }
        }
    }

    let mut bounds = ContentBounds::EMPTY;
    visit(commands, &mut bounds, 0);
    bounds
}

struct WgpuCommandHandler<'a> {
    descriptors: &'a Descriptors,
    quality: StageQuality,
    width: u32,
    height: u32,
    nearest_layer: LayerRef<'a>,
    meshes: &'a Vec<Mesh>,
    staging_belt: &'a mut wgpu::util::StagingBelt,
    dynamic_transforms: &'a DynamicTransforms,
    draw_encoder: &'a mut wgpu::CommandEncoder,
    texture_pool: &'a mut TexturePool,
    emulate_lines: bool,

    result: Vec<Chunk>,
    current: Vec<DrawCommand>,
    transforms: BufferBuilder,
    needs_stencil: bool,
    num_masks: i32,
    /// Extent of everything drawn so far, in the commands' own (surface)
    /// coordinates -- taken before `origin` is subtracted, so it stays
    /// meaningful to whoever composites this target.
    content_bounds: ContentBounds,
    /// Subtracted from every draw's translation, placing surface coordinates
    /// inside a target that covers only part of the surface.
    origin: (f32, f32),
}

impl<'a> WgpuCommandHandler<'a> {
    #[expect(clippy::too_many_arguments)]
    fn new(
        descriptors: &'a Descriptors,
        staging_belt: &'a mut wgpu::util::StagingBelt,
        dynamic_transforms: &'a DynamicTransforms,
        draw_encoder: &'a mut wgpu::CommandEncoder,
        meshes: &'a Vec<Mesh>,
        quality: StageQuality,
        width: u32,
        height: u32,
        nearest_layer: LayerRef<'a>,
        texture_pool: &'a mut TexturePool,
        origin: (u32, u32),
    ) -> Self {
        let transforms = Self::new_transforms(descriptors, dynamic_transforms);

        // DirectX does support drawing lines, but it's very inconsistent.
        // With MSAA, lines have 1.4px thickness, which makes them too thick.
        // Without MSAA, lines have 1px thickness, but their placement is sometimes off.
        let emulate_lines = descriptors.backend == Backend::Dx12;

        Self {
            descriptors,
            quality,
            width,
            height,
            nearest_layer,
            meshes,
            staging_belt,
            dynamic_transforms,
            draw_encoder,
            texture_pool,
            emulate_lines,

            result: vec![],
            current: vec![],
            transforms,
            needs_stencil: false,
            num_masks: 0,
            content_bounds: ContentBounds::EMPTY,
            origin: (origin.0 as f32, origin.1 as f32),
        }
    }

    fn new_transforms(
        descriptors: &'a Descriptors,
        dynamic_transforms: &'a DynamicTransforms,
    ) -> BufferBuilder {
        let mut transforms = BufferBuilder::new_for_uniform(&descriptors.limits);
        transforms.set_buffer_limit(dynamic_transforms.buffer.size());
        transforms
    }

    /// Replaces every blend with a RenderBitmap, with the subcommands rendered out to a temporary texture
    /// Every complex blend will be its own item, but every other draw will be chunked together
    fn chunk_blends(&mut self, commands: CommandList) -> (Vec<Chunk>, ContentBounds) {
        commands.execute(self);

        let current = mem::take(&mut self.current);
        let mut result = mem::take(&mut self.result);
        let needs_stencil = mem::take(&mut self.needs_stencil);
        let content_bounds = mem::take(&mut self.content_bounds);
        let transforms = mem::replace(
            &mut self.transforms,
            Self::new_transforms(self.descriptors, self.dynamic_transforms),
        );

        if !current.is_empty() {
            result.push(Chunk::Draw {
                chunk: current,
                needs_stencil,
                transforms,
            });
        }

        (result, content_bounds)
    }

    /// Grows the content extent by a draw of `local_bounds` placed by `matrix`.
    ///
    /// `local_bounds` is the geometry in the draw's own space: the unit square
    /// for everything quad-shaped, the mesh extent for shapes.
    fn note_bounds(&mut self, matrix: &Matrix, local_bounds: ContentBounds) {
        self.content_bounds.union_transformed(matrix, local_bounds);
    }

    fn add_to_current(
        &mut self,
        matrix: Matrix,
        color_transform: ColorTransform,
        local_bounds: ContentBounds,
        command_builder: impl FnOnce(wgpu::DynamicOffset) -> DrawCommand,
    ) {
        self.note_bounds(&matrix, local_bounds);
        // Bounds are recorded in surface coordinates above; only what the GPU
        // draws is shifted into the target.
        let transform = Transforms {
            world_matrix: [
                [matrix.a, matrix.b, 0.0, 0.0],
                [matrix.c, matrix.d, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
                [
                    matrix.tx.to_pixels() as f32 - self.origin.0,
                    matrix.ty.to_pixels() as f32 - self.origin.1,
                    0.0,
                    1.0,
                ],
            ],
            mult_color: color_transform.mult_rgba_normalized(),
            add_color: color_transform.add_rgba_normalized(),
        };
        if let Ok(transform_range) = self.transforms.add(&[transform]) {
            self.current.push(command_builder(
                transform_range.start as wgpu::DynamicOffset,
            ));
        } else {
            self.result.push(Chunk::Draw {
                chunk: mem::take(&mut self.current),
                needs_stencil: self.needs_stencil,
                transforms: mem::replace(
                    &mut self.transforms,
                    BufferBuilder::new_for_uniform(&self.descriptors.limits),
                ),
            });
            self.transforms
                .set_buffer_limit(self.dynamic_transforms.buffer.size());
            let transform_range = self
                .transforms
                .add(&[transform])
                .expect("Buffer must be able to fit a new thing, it was just emptied");
            self.current.push(command_builder(
                transform_range.start as wgpu::DynamicOffset,
            ));
        }
    }
}

impl CommandHandler for WgpuCommandHandler<'_> {
    fn blend(&mut self, commands: CommandList, blend_mode: RenderBlendMode) {
        let target_layer = if let RenderBlendMode::Builtin(BlendMode::Layer) = &blend_mode {
            LayerRef::Current
        } else {
            self.nearest_layer
        };
        let blend_type = BlendType::from(blend_mode);

        // We currently do not support shader blends in masks. In order not to
        // break other parts of the scene, we just fall back to a normal blend.
        //
        // TODO Add support for shader blends in masks.
        let is_shader_blend_in_mask =
            self.num_masks > 0 && matches!(blend_type, BlendType::Shader(_));
        // Whether this blend composites through a pass of its own, decided
        // before any measurement demotion below so that toggling the
        // measurement changes one thing and not two.
        let is_complex = matches!(blend_type, BlendType::Complex(_));

        if is_complex && blend_measured_skipped() {
            return;
        }

        let blend_type = if is_shader_blend_in_mask || (blend_measured_as_normal() && is_complex) {
            BlendType::Trivial(TrivialBlend::Normal)
        } else {
            blend_type
        };

        // A complex blend composites through its own render target. Sized to
        // the whole surface that is by far the biggest thing this renderer
        // holds -- a crowded room keeps hundreds alive at once, measured at
        // 843 x 1600x841 = 4.3GB, which is what pushes VRAM past the OS grant
        // and starts the paging that collapses the frame rate. Sizing it to
        // the content instead is the same picture in a fraction of the memory,
        // since everything outside the content is transparent either way.
        //
        // Measured ahead of drawing, because the target has to exist first.
        // Trivial blends feed a plain bitmap draw and shader blends run
        // arbitrary code, so both keep the full-size target.
        let bounded = is_complex && !blend_target_shrink_disabled();
        let rect = bounded
            .then(|| {
                command_list_bounds(&commands)
                    .translated(-self.origin.0, -self.origin.1)
                    .to_snapped_rect(self.width, self.height, quantize_pool_dimension)
            })
            .flatten();

        let (surface_origin, surface_width, surface_height) = match rect {
            Some((x, y, width, height)) => (
                (self.origin.0 as u32 + x, self.origin.1 as u32 + y),
                width,
                height,
            ),
            None => (
                (self.origin.0 as u32, self.origin.1 as u32),
                self.width,
                self.height,
            ),
        };

        if is_complex {
            crate::blend::note_blend_alloc(
                surface_width as u64 * surface_height as u64,
                self.width as u64 * self.height as u64,
            );
        }

        let surface = Surface::new(
            self.descriptors,
            self.quality,
            surface_width,
            surface_height,
            wgpu::TextureFormat::Rgba8Unorm,
        )
        .with_origin(surface_origin);

        let clear_color = blend_type.default_color();
        let target = surface.draw_commands(
            RenderTargetMode::FreshWithColor(clear_color),
            self.descriptors,
            self.meshes,
            commands,
            self.staging_belt,
            self.dynamic_transforms,
            self.draw_encoder,
            target_layer,
            self.texture_pool,
        );
        target.ensure_cleared(self.draw_encoder);
        // Recorded in surface coordinates, so it carries over untransformed
        // however the target was sized.
        let blend_bounds = target.content_bounds();

        match blend_type {
            BlendType::Trivial(blend_mode) => {
                let transform = Transform {
                    matrix: Matrix::scale(target.width() as f32, target.height() as f32),
                    color_transform: Default::default(),
                    perspective_projection: None,
                };
                let texture = target.take_color_texture();
                let bind_group =
                    self.descriptors
                        .device
                        .create_bind_group(&wgpu::BindGroupDescriptor {
                            layout: &self.descriptors.bind_layouts.bitmap,
                            entries: &[
                                wgpu::BindGroupEntry {
                                    binding: 0,
                                    resource: self
                                        .descriptors
                                        .quad
                                        .texture_transforms
                                        .as_entire_binding(),
                                },
                                wgpu::BindGroupEntry {
                                    binding: 1,
                                    resource: wgpu::BindingResource::TextureView(texture.view()),
                                },
                                wgpu::BindGroupEntry {
                                    binding: 2,
                                    resource: wgpu::BindingResource::Sampler(
                                        self.descriptors.bitmap_samplers.get_sampler(false, false),
                                    ),
                                },
                            ],
                            label: None,
                        });
                // The matrix covers the whole surface, but only `blend_bounds`
                // of it was actually drawn into; carry that up rather than
                // letting a nested blend widen an enclosing one to full screen.
                self.add_to_current(
                    transform.matrix,
                    transform.color_transform,
                    ContentBounds::EMPTY,
                    |transform_buffer| DrawCommand::RenderTexture {
                        _texture: texture,
                        binds: bind_group,
                        transform_buffer,
                        blend_mode,
                    },
                );
                self.content_bounds.union_pixels(blend_bounds);
            }
            blend_type => {
                if !self.current.is_empty() {
                    self.result.push(Chunk::Draw {
                        chunk: mem::take(&mut self.current),
                        needs_stencil: self.needs_stencil,
                        transforms: mem::replace(
                            &mut self.transforms,
                            BufferBuilder::new_for_uniform(&self.descriptors.limits),
                        ),
                    });
                }
                self.transforms
                    .set_buffer_limit(self.dynamic_transforms.buffer.size());
                let chunk_blend_mode = match blend_type {
                    BlendType::Complex(complex) => {
                        crate::blend::note_complex_blend(complex);
                        ChunkBlendMode::Complex(complex)
                    }
                    BlendType::Shader(shader) => {
                        crate::blend::note_shader_blend();
                        ChunkBlendMode::Shader(shader)
                    }
                    _ => unreachable!(),
                };
                self.result.push(Chunk::Blend {
                    texture: target.take_color_texture(),
                    blend_mode: chunk_blend_mode,
                    needs_stencil: self.num_masks > 0,
                    bounds: blend_bounds,
                    // Where this target sits in the target being composited
                    // into. The pass draws exactly this rect, so the unit quad
                    // doubles as the source texture's coordinate.
                    rect: rect.unwrap_or((0, 0, self.width, self.height)),
                });
                self.needs_stencil = self.num_masks > 0;
                // The blend writes wherever its source is opaque, so this chunk
                // contributes the same extent to any enclosing blend.
                self.content_bounds.union_pixels(blend_bounds);
            }
        }
    }

    fn render_bitmap(
        &mut self,
        bitmap: BitmapHandle,
        transform: Transform,
        smoothing: bool,
        pixel_snapping: PixelSnapping,
    ) {
        let mut matrix = transform.matrix;
        {
            let texture = as_texture(&bitmap);
            pixel_snapping.apply(&mut matrix);
            matrix *= Matrix::scale(
                texture.texture.width() as f32,
                texture.texture.height() as f32,
            );
        }
        // The texture size is already folded into `matrix`, so the drawn
        // geometry is the unit quad.
        self.add_to_current(
            matrix,
            transform.color_transform,
            ContentBounds::UNIT,
            |transform_buffer| DrawCommand::RenderBitmap {
                bitmap,
                transform_buffer,
                smoothing,
                blend_mode: TrivialBlend::Normal,
                render_stage3d: false,
            },
        );
    }
    fn render_stage3d(&mut self, bitmap: BitmapHandle, transform: Transform) {
        let mut matrix = transform.matrix;
        {
            let texture = as_texture(&bitmap);
            matrix *= Matrix::scale(
                texture.texture.width() as f32,
                texture.texture.height() as f32,
            );
        }
        self.add_to_current(
            matrix,
            transform.color_transform,
            ContentBounds::UNIT,
            |transform_buffer| DrawCommand::RenderBitmap {
                bitmap,
                transform_buffer,
                smoothing: false,
                blend_mode: TrivialBlend::Normal,
                render_stage3d: true,
            },
        );
    }

    fn render_shape(&mut self, shape: ShapeHandle, transform: Transform) {
        // Tessellated vertices are in pixels, same as the matrix, so the mesh
        // extent is the local geometry.
        let mesh_bounds = as_mesh(&shape).bounds;
        self.add_to_current(
            transform.matrix,
            transform.color_transform,
            mesh_bounds,
            |transform_buffer| DrawCommand::RenderShape {
                shape,
                transform_buffer,
            },
        );
    }

    fn draw_rect(&mut self, color: Color, matrix: Matrix) {
        self.add_to_current(
            matrix,
            ColorTransform::multiply_from(color),
            ContentBounds::UNIT,
            |transform_buffer| DrawCommand::DrawRect { transform_buffer },
        );
    }

    fn draw_line(&mut self, color: Color, mut matrix: Matrix) {
        if self.emulate_lines {
            let mut cl = CommandList::new();
            emulate_line(&mut cl, color, matrix);
            cl.execute(self);
        } else {
            matrix.tx += Twips::HALF_PX;
            matrix.ty += Twips::HALF_PX;
            self.add_to_current(
                matrix,
                ColorTransform::multiply_from(color),
                ContentBounds::UNIT,
                |transform_buffer| DrawCommand::DrawLine { transform_buffer },
            );
        }
    }

    fn draw_line_rect(&mut self, color: Color, mut matrix: Matrix) {
        if self.emulate_lines {
            let mut cl = CommandList::new();
            emulate_line_rect(&mut cl, color, matrix);
            cl.execute(self);
        } else {
            matrix.tx += Twips::HALF_PX;
            matrix.ty += Twips::HALF_PX;
            self.add_to_current(
                matrix,
                ColorTransform::multiply_from(color),
                ContentBounds::UNIT,
                |transform_buffer| DrawCommand::DrawLineRect { transform_buffer },
            );
        }
    }

    fn push_mask(&mut self) {
        self.needs_stencil = true;
        self.num_masks += 1;
        self.current.push(DrawCommand::PushMask);
    }

    fn activate_mask(&mut self) {
        self.needs_stencil = true;
        self.current.push(DrawCommand::ActivateMask);
    }

    fn deactivate_mask(&mut self) {
        self.needs_stencil = true;
        self.current.push(DrawCommand::DeactivateMask);
    }

    fn pop_mask(&mut self) {
        self.needs_stencil = true;
        self.num_masks -= 1;
        self.current.push(DrawCommand::PopMask);
    }

    fn render_alpha_mask(&mut self, maskee_commands: CommandList, mask_commands: CommandList) {
        let surface = Surface::new(
            self.descriptors,
            self.quality,
            self.width,
            self.height,
            wgpu::TextureFormat::Rgba8Unorm,
        );

        let maskee = surface.draw_commands(
            RenderTargetMode::FreshWithColor(wgpu::Color::TRANSPARENT),
            self.descriptors,
            self.meshes,
            maskee_commands,
            self.staging_belt,
            self.dynamic_transforms,
            self.draw_encoder,
            LayerRef::None,
            self.texture_pool,
        );
        maskee.ensure_cleared(self.draw_encoder);
        let matrix = Matrix::scale(maskee.width() as f32, maskee.height() as f32);
        // `alpha_mask.wgsl` outputs `maskee.rgb * mask.a`, so nothing can show
        // outside the maskee's own extent. Same coordinate system, no transform.
        let maskee_bounds = maskee.content_bounds();
        let maskee = maskee.take_color_texture();

        let mask = surface.draw_commands(
            RenderTargetMode::FreshWithColor(wgpu::Color::TRANSPARENT),
            self.descriptors,
            self.meshes,
            mask_commands,
            self.staging_belt,
            self.dynamic_transforms,
            self.draw_encoder,
            LayerRef::None,
            self.texture_pool,
        );
        mask.ensure_cleared(self.draw_encoder);
        let mask = mask.take_color_texture();

        let binds = self
            .descriptors
            .device
            .create_bind_group(&wgpu::BindGroupDescriptor {
                layout: &self.descriptors.bind_layouts.alpha_mask,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(maskee.view()),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(mask.view()),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(
                            self.descriptors.bitmap_samplers.get_sampler(false, false),
                        ),
                    },
                ],
                label: None,
            });

        self.add_to_current(
            matrix,
            Default::default(),
            ContentBounds::EMPTY,
            |transform_buffer| DrawCommand::RenderAlphaMask {
                maskee,
                mask,
                binds,
                transform_buffer,
            },
        );
        self.content_bounds.union_pixels(maskee_bounds);
    }
}
