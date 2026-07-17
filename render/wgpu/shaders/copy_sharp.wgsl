/// Present-copy shader with fixed cubic (Catmull-Rom) resampling.
///
/// Used in place of `copy.wgsl` for scaled AQW presents. It restores lineart
/// definition when the pixel-area fallback is upscaled and keeps the validated
/// crisp look when an SSAA surface is downsampled.
///
/// A contrast-adaptive sharpener (CAS-style) was tried here first and
/// field-rejected: its per-pixel adaptive weight makes anti-aliased vector
/// art look gritty and crawl in motion. Keep this filter FIXED (weights
/// depend only on the sample phase, never on the pixel values).

// NOTE: The `common.wgsl` source is prepended to this before compilation.

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@group(1) @binding(0) var<uniform> transforms: common__Transforms;
@group(2) @binding(0) var<uniform> textureTransforms: common__TextureTransforms;
@group(2) @binding(1) var texture: texture_2d<f32>;
@group(2) @binding(2) var texture_sampler: sampler;

// 0 = softest, 1 = hardest. This line is rewritten at startup from
// `RUFFLE_AQW_SHARPNESS` (see `shaders.rs`), keeping the pipeline
// uniform-free. Maps onto the cubic's C parameter below; the 0.5 default is
// exact Catmull-Rom.
const SHARPNESS: f32 = 0.5;

@vertex
fn main_vertex(in: common__VertexInput) -> VertexOutput {
    let matrix_ = textureTransforms.texture_matrix;
    let uv = (mat3x3<f32>(matrix_[0].xyz, matrix_[1].xyz, matrix_[2].xyz) * vec3<f32>(in.position, 1.0)).xy;
    let pos = common__globals.view_matrix * transforms.world_matrix * vec4<f32>(in.position.x, in.position.y, 0.0, 1.0);
    return VertexOutput(pos, uv);
}

@fragment
fn main_fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    let dims = vec2<f32>(textureDimensions(texture));
    // Texel-space position; texel i's center sits at i + 0.5.
    let sample_pos = in.uv * dims;
    let tex_pos1 = floor(sample_pos - 0.5) + 0.5;
    let f = sample_pos - tex_pos1;
    let f2 = f * f;
    let f3 = f2 * f;

    // B = 0 cubic filter family; C = 0.5 is Catmull-Rom. Higher C sharpens
    // (more negative lobe, more ringing risk), lower softens.
    let c = mix(0.25, 0.75, SHARPNESS);
    let w0 = -c * f + 2.0 * c * f2 - c * f3;
    let w1 = vec2<f32>(1.0, 1.0) + (c - 3.0) * f2 + (2.0 - c) * f3;
    let w2 = c * f + (3.0 - 2.0 * c) * f2 + (c - 2.0) * f3;
    let w3 = -c * f2 + c * f3;

    // The two positive center taps collapse into one bilinear fetch placed at
    // their weighted centroid, so the 4×4 kernel needs only 3×3 samples (the
    // sampler is bound clamp-to-edge + linear on this path).
    let w12 = w1 + w2;
    let offset12 = w2 / w12;
    let tp0 = (tex_pos1 - 1.0) / dims;
    let tp3 = (tex_pos1 + 2.0) / dims;
    let tp12 = (tex_pos1 + offset12) / dims;

    let result = textureSampleLevel(texture, texture_sampler, vec2<f32>(tp0.x, tp0.y), 0.0) * w0.x * w0.y
        + textureSampleLevel(texture, texture_sampler, vec2<f32>(tp12.x, tp0.y), 0.0) * w12.x * w0.y
        + textureSampleLevel(texture, texture_sampler, vec2<f32>(tp3.x, tp0.y), 0.0) * w3.x * w0.y
        + textureSampleLevel(texture, texture_sampler, vec2<f32>(tp0.x, tp12.y), 0.0) * w0.x * w12.y
        + textureSampleLevel(texture, texture_sampler, vec2<f32>(tp12.x, tp12.y), 0.0) * w12.x * w12.y
        + textureSampleLevel(texture, texture_sampler, vec2<f32>(tp3.x, tp12.y), 0.0) * w3.x * w12.y
        + textureSampleLevel(texture, texture_sampler, vec2<f32>(tp0.x, tp3.y), 0.0) * w0.x * w3.y
        + textureSampleLevel(texture, texture_sampler, vec2<f32>(tp12.x, tp3.y), 0.0) * w12.x * w3.y
        + textureSampleLevel(texture, texture_sampler, vec2<f32>(tp3.x, tp3.y), 0.0) * w3.x * w3.y;

    // The negative lobes can slightly over/undershoot at hard edges.
    return clamp(result, vec4<f32>(0.0), vec4<f32>(1.0));
}
