mod commands;
pub mod target;

use crate::Transforms;
use crate::backend::RenderTargetMode;
use crate::blend::ComplexBlend;
use crate::buffer_builder::BufferBuilder;
use crate::buffer_pool::TexturePool;
use crate::dynamic_transforms::DynamicTransforms;
use crate::filters::FilterSource;
use crate::mesh::Mesh;
use crate::pixel_bender::{ShaderMode, run_pixelbender_shader_impl};
use crate::surface::commands::{Chunk, CommandRenderer, chunk_blends};
use crate::utils::supported_sample_count;
use crate::{Descriptors, MaskState, Pipelines};
use ruffle_render::commands::CommandList;
use ruffle_render::pixel_bender_support::{ImageInputTexture, PixelBenderShaderArgument};
use ruffle_render::quality::StageQuality;
use std::sync::{Arc, OnceLock};
use target::CommandTarget;
use tracing::instrument;

/// Kill-switch: `RUFFLE_AQW_NO_BLEND_SCISSOR` restores full-surface complex
/// blend passes, for field A/B without a rebuild.
fn blend_scissor_disabled() -> bool {
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED
        .get_or_init(|| ruffle_render::backend::aqw_env_flag("RUFFLE_AQW_NO_BLEND_SCISSOR", false))
}

use crate::utils::run_copy_pipeline;

pub use crate::surface::commands::LayerRef;

use self::commands::ChunkBlendMode;

#[derive(Debug)]
pub struct Surface {
    size: wgpu::Extent3d,
    quality: StageQuality,
    sample_count: u32,
    pipelines: Arc<Pipelines>,
    format: wgpu::TextureFormat,
    /// Where this surface sits inside the coordinate space its commands are
    /// expressed in. Non-zero for a blend target sized to its content instead
    /// of the whole screen, so that drawing subtracts it to land in range.
    origin: (u32, u32),
}

impl Surface {
    pub fn new(
        descriptors: &Descriptors,
        quality: StageQuality,
        width: u32,
        height: u32,
        frame_buffer_format: wgpu::TextureFormat,
    ) -> Self {
        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };

        let sample_count = supported_sample_count(
            &descriptors.adapter,
            quality.sample_count(),
            frame_buffer_format,
        );
        let pipelines = descriptors.pipelines(sample_count, frame_buffer_format);
        Self {
            size,
            quality,
            sample_count,
            pipelines,
            format: frame_buffer_format,
            origin: (0, 0),
        }
    }

    /// Places this surface at `origin` in its commands' coordinate space.
    pub fn with_origin(mut self, origin: (u32, u32)) -> Self {
        self.origin = origin;
        self
    }

    #[expect(clippy::too_many_arguments)]
    #[instrument(level = "debug", skip_all)]
    pub fn draw_commands_and_copy_to<'frame, 'global: 'frame>(
        &self,
        frame_view: &wgpu::TextureView,
        smooth: bool,
        sharp: bool,
        crt: bool,
        render_target_mode: RenderTargetMode,
        descriptors: &'global Descriptors,
        staging_belt: &'frame mut wgpu::util::StagingBelt,
        dynamic_transforms: &'global DynamicTransforms,
        draw_encoder: &'frame mut wgpu::CommandEncoder,
        meshes: &'global Vec<Mesh>,
        commands: CommandList,
        layer: LayerRef,
        texture_pool: &mut TexturePool,
    ) {
        let target = self.draw_commands(
            render_target_mode,
            descriptors,
            meshes,
            commands,
            staging_belt,
            dynamic_transforms,
            draw_encoder,
            layer,
            texture_pool,
        );

        run_copy_pipeline(
            descriptors,
            self.format,
            frame_view,
            target.color_view(),
            target.whole_frame_bind_group(descriptors),
            target.globals(),
            1,
            smooth,
            sharp,
            crt,
            target.copy_uv_scale(),
            draw_encoder,
        );
    }

    #[expect(clippy::too_many_arguments)]
    #[instrument(level = "debug", skip_all)]
    pub fn draw_commands<'frame, 'global: 'frame>(
        &self,
        render_target_mode: RenderTargetMode,
        descriptors: &'global Descriptors,
        meshes: &'global Vec<Mesh>,
        commands: CommandList,
        staging_belt: &'global mut wgpu::util::StagingBelt,
        dynamic_transforms: &'global DynamicTransforms,
        draw_encoder: &'frame mut wgpu::CommandEncoder,
        nearest_layer: LayerRef<'frame>,
        texture_pool: &mut TexturePool,
    ) -> CommandTarget {
        // Read before the mode is handed over, and carried down so a blend can
        // tell the scene apart from another blend's target. It is the whole
        // difference between a multiply that fixed-function state could do and
        // one that cannot.
        let dest_opaque = render_target_mode.clears_opaque();

        let target = CommandTarget::new(
            descriptors,
            texture_pool,
            self.size,
            self.format,
            self.sample_count,
            render_target_mode,
            draw_encoder,
            // Never pad command-list targets: blend and alpha-mask shaders
            // derive UVs from NDC position, which is only correct when every
            // texture in the pass has exactly the attachment's dimensions.
            false,
        );

        let mut num_masks = 0;
        let mut mask_state = MaskState::NoMask;
        let (chunks, content_bounds) = chunk_blends(
            commands,
            descriptors,
            staging_belt,
            dynamic_transforms,
            draw_encoder,
            meshes,
            self.quality,
            target.width(),
            target.height(),
            match nearest_layer {
                LayerRef::Current => LayerRef::Parent(&target),
                layer => layer,
            },
            texture_pool,
            self.origin,
            dest_opaque,
        );
        target.set_content_bounds(content_bounds);

        for chunk in chunks {
            match chunk {
                Chunk::Draw {
                    chunk,
                    needs_stencil,
                    transforms,
                } => {
                    transforms.copy_to(
                        staging_belt,
                        &descriptors.device,
                        draw_encoder,
                        &dynamic_transforms.buffer,
                    );
                    let mut render_pass =
                        draw_encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                            label: create_debug_label!(
                                "Chunked draw calls {}",
                                if needs_stencil {
                                    "(with stencil)"
                                } else {
                                    "(Stencilless)"
                                }
                            )
                            .as_deref(),
                            color_attachments: &[target.color_attachments()],
                            depth_stencil_attachment: if needs_stencil {
                                target.stencil_attachment(descriptors, texture_pool)
                            } else {
                                None
                            },
                            ..Default::default()
                        });
                    render_pass.set_bind_group(0, target.globals().bind_group(), &[]);
                    let mut renderer = CommandRenderer::new(
                        &self.pipelines,
                        descriptors,
                        dynamic_transforms,
                        render_pass,
                        num_masks,
                        mask_state,
                        needs_stencil,
                    );

                    for command in &chunk {
                        renderer.execute(command);
                    }

                    num_masks = renderer.num_masks();
                    mask_state = renderer.mask_state();
                }
                Chunk::Blend {
                    texture,
                    blend_mode: ChunkBlendMode::Shader(shader),
                    needs_stencil,
                    bounds: _,
                    rect: _,
                } => {
                    assert!(!needs_stencil, "Shader blend mode not implemented in masks");
                    // Not bounded: a PixelBender blend is arbitrary user code,
                    // with no guarantee it leaves transparent source alone the
                    // way the built-in blends do.
                    let parent_blend_buffer =
                        target.update_blend_buffer(descriptors, texture_pool, draw_encoder, None);
                    run_pixelbender_shader_impl(
                        descriptors,
                        shader,
                        ShaderMode::Filter,
                        &[
                            PixelBenderShaderArgument::ImageInput {
                                index: 0,
                                channels: 0xFF,
                                name: "background".to_string(),
                                texture: Some(ImageInputTexture::TextureRef(
                                    parent_blend_buffer.texture(),
                                )),
                            },
                            PixelBenderShaderArgument::ImageInput {
                                index: 1,
                                channels: 0xff,
                                name: "foreground".to_string(),
                                texture: Some(ImageInputTexture::TextureRef(texture.texture())),
                            },
                        ],
                        parent_blend_buffer.texture(),
                        draw_encoder,
                        target.color_attachments(),
                        target.sample_count(),
                        &FilterSource::for_entire_texture(texture.texture()),
                    )
                    .expect("Failed to run PixelBender blend mode");
                }
                Chunk::Blend {
                    texture,
                    blend_mode: ChunkBlendMode::Complex(blend_mode),
                    needs_stencil,
                    bounds,
                    rect,
                } => {
                    let parent = match blend_mode {
                        ComplexBlend::Alpha | ComplexBlend::Erase => {
                            match nearest_layer {
                                LayerRef::None => {
                                    // An Alpha or Erase with no Layer above it should be ignored
                                    continue;
                                }
                                LayerRef::Current => &target,
                                LayerRef::Parent(layer) => layer,
                            }
                        }
                        _ => &target,
                    };

                    // How much of the surface this pass is really about, logged
                    // whether or not it gets bounded so the two can be compared
                    // in the field.
                    crate::blend::note_blend_coverage(
                        bounds.clipped_area(target.width(), target.height()),
                        target.width() as u64 * target.height() as u64,
                    );

                    // Confine the pass to the blended object instead of the
                    // whole surface. Every complex blend shader discards where
                    // `src.a <= 0`, and the source target was cleared to
                    // transparent, so nothing outside `bounds` could ever have
                    // been written -- this drops the fill, not the result. A
                    // crowded room runs hundreds of these per frame, each
                    // otherwise costing a full screen of blending.
                    //
                    // A pixel of slack absorbs rounding in the NDC-to-UV round
                    // trip the blend shaders do.
                    //
                    // A content-sized target already covers only `rect`, so the
                    // scissor is then just a clamp; it still matters for a
                    // full-size one, and for narrowing the parent snapshot.
                    //
                    // `bounds` is in the commands' own coordinates, so it is
                    // shifted into this target's space before clamping.
                    let scissor = if blend_scissor_disabled() {
                        // Kill-switch: the whole target, as before.
                        Some((0, 0, target.width(), target.height()))
                    } else {
                        bounds
                            .translated(-(self.origin.0 as f32), -(self.origin.1 as f32))
                            .to_scissor(target.width(), target.height(), 1)
                    };
                    let Some(scissor) = scissor else {
                        // Nothing covered: every fragment would have discarded.
                        continue;
                    };

                    // Only the scissored region is read back, so only it needs
                    // snapshotting for the shader's `dst`.
                    let parent_blend_buffer = parent.update_blend_buffer(
                        descriptors,
                        texture_pool,
                        draw_encoder,
                        Some(scissor),
                    );

                    let blend_bind_group =
                        descriptors
                            .device
                            .create_bind_group(&wgpu::BindGroupDescriptor {
                                label: create_debug_label!(
                                    "Complex blend binds {:?} {}",
                                    blend_mode,
                                    if needs_stencil {
                                        "(with stencil)"
                                    } else {
                                        "(Stencilless)"
                                    }
                                )
                                .as_deref(),
                                layout: &descriptors.bind_layouts.blend,
                                entries: &[
                                    wgpu::BindGroupEntry {
                                        binding: 0,
                                        resource: wgpu::BindingResource::TextureView(
                                            parent_blend_buffer.view(),
                                        ),
                                    },
                                    wgpu::BindGroupEntry {
                                        binding: 1,
                                        resource: wgpu::BindingResource::TextureView(
                                            texture.view(),
                                        ),
                                    },
                                    wgpu::BindGroupEntry {
                                        binding: 2,
                                        resource: wgpu::BindingResource::Sampler(
                                            descriptors.bitmap_samplers.get_sampler(false, false),
                                        ),
                                    },
                                ],
                            });

                    // The quad covers exactly the source's footprint, which is
                    // what makes the unit-quad coordinate usable as its texture
                    // coordinate however the target was sized.
                    let mut blend_transforms = BufferBuilder::new_for_uniform(&descriptors.limits);
                    blend_transforms.set_buffer_limit(dynamic_transforms.buffer.size());
                    let blend_transform_offset = blend_transforms
                        .add(&[Transforms {
                            world_matrix: [
                                [rect.2 as f32, 0.0, 0.0, 0.0],
                                [0.0, rect.3 as f32, 0.0, 0.0],
                                [0.0, 0.0, 1.0, 0.0],
                                [rect.0 as f32, rect.1 as f32, 0.0, 1.0],
                            ],
                            mult_color: [1.0, 1.0, 1.0, 1.0],
                            add_color: [0.0, 0.0, 0.0, 0.0],
                        }])
                        .expect("A single transform always fits an empty buffer");
                    blend_transforms.copy_to(
                        staging_belt,
                        &descriptors.device,
                        draw_encoder,
                        &dynamic_transforms.buffer,
                    );

                    let mut render_pass =
                        draw_encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                            label: create_debug_label!(
                                "Complex blend {:?} {}",
                                blend_mode,
                                if needs_stencil {
                                    "(with stencil)"
                                } else {
                                    "(Stencilless)"
                                }
                            )
                            .as_deref(),
                            color_attachments: &[target.color_attachments()],
                            depth_stencil_attachment: if needs_stencil {
                                target.stencil_attachment(descriptors, texture_pool)
                            } else {
                                None
                            },
                            ..Default::default()
                        });
                    render_pass.set_bind_group(0, target.globals().bind_group(), &[]);
                    let (scissor_x, scissor_y, scissor_w, scissor_h) = scissor;
                    render_pass.set_scissor_rect(scissor_x, scissor_y, scissor_w, scissor_h);

                    if needs_stencil {
                        match mask_state {
                            MaskState::NoMask => {}
                            MaskState::DrawMaskStencil => {
                                render_pass.set_stencil_reference(num_masks - 1);
                            }
                            MaskState::DrawMaskedContent => {
                                render_pass.set_stencil_reference(num_masks);
                            }
                            MaskState::ClearMaskStencil => {
                                render_pass.set_stencil_reference(num_masks);
                            }
                        }
                        render_pass.set_pipeline(
                            self.pipelines.complex_blends[blend_mode].pipeline_for(mask_state),
                        );
                    } else {
                        render_pass.set_pipeline(
                            self.pipelines.complex_blends[blend_mode].stencilless_pipeline(),
                        );
                    }

                    render_pass.set_bind_group(
                        1,
                        &dynamic_transforms.bind_group,
                        &[blend_transform_offset.start as wgpu::DynamicOffset],
                    );
                    render_pass.set_bind_group(2, &blend_bind_group, &[]);

                    render_pass.set_vertex_buffer(0, descriptors.quad.vertices_pos.slice(..));
                    render_pass.set_index_buffer(
                        descriptors.quad.indices.slice(..),
                        wgpu::IndexFormat::Uint32,
                    );

                    render_pass.draw_indexed(0..6, 0, 0..1);
                }
            }
        }

        // If nothing happened, ensure it's cleared so we don't operate on garbage data
        target.ensure_cleared(draw_encoder);

        target
    }

    pub fn quality(&self) -> StageQuality {
        self.quality
    }

    pub fn sample_count(&self) -> u32 {
        self.sample_count
    }

    pub fn size(&self) -> wgpu::Extent3d {
        self.size
    }
}
