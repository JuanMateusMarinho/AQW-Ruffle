// NOTE: The `common.wgsl` source is prepended to this before compilation.

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    // Coordinate into `current_texture`. The quad covers exactly that
    // texture's footprint, so the unit-quad position is already its UV --
    // which is what lets the blend source be sized to its content instead
    // of the whole surface, while `uv` stays screen-space for the parent.
    @location(1) src_uv: vec2<f32>,
};

@group(1) @binding(0) var<uniform> transforms: common__Transforms;
@group(2) @binding(0) var parent_texture: texture_2d<f32>;
@group(2) @binding(1) var current_texture: texture_2d<f32>;
@group(2) @binding(2) var texture_sampler: sampler;

@vertex
fn main_vertex(in: common__VertexInput) -> VertexOutput {
    let pos = common__globals.view_matrix * transforms.world_matrix * vec4<f32>(in.position.x, in.position.y, 1.0, 1.0);
    let uv = vec2<f32>((pos.x + 1.0) / 2.0, -((pos.y - 1.0) / 2.0));
    return VertexOutput(pos, uv, in.position);
}

fn blend_func(src: vec3<f32>, dst: vec3<f32>) -> vec3<f32> {
    var out = src;
    if (dst.r <= 0.5) { out.r = (2.0 * src.r * dst.r); } else { out.r = (1.0 - 2.0 * (1.0 - dst.r) * (1.0 - src.r)); }
    if (dst.g <= 0.5) { out.g = (2.0 * src.g * dst.g); } else { out.g = (1.0 - 2.0 * (1.0 - dst.g) * (1.0 - src.g)); }
    if (dst.b <= 0.5) { out.b = (2.0 * src.b * dst.b); } else { out.b = (1.0 - 2.0 * (1.0 - dst.b) * (1.0 - src.b)); }
    return out;
}

@fragment
fn main_fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    // dst is the parent pixel we're blending onto
    var dst: vec4<f32> = textureSample(parent_texture, texture_sampler, in.uv);
    // src is the pixel that we want to apply
    var src: vec4<f32> = textureSample(current_texture, texture_sampler, in.src_uv);

    if (src.a > 0.0) {
        // With nothing underneath, the backdrop contributes nothing and the
        // formula below reduces to `src` -- but it gets there through
        // `dst.rgb / dst.a`, which is 0/0. Defensive only: `multiply.wgsl` has
        // always had this branch and the rest of the family had not, and 0/0 is
        // undefined rather than guaranteed to fold away. Every blend image test
        // is byte-identical with and without it, which also says the drivers
        // tested here already fold `0.0 * x` to zero.
        //
        // Do not read this as a fix for anything: it was written on 2026-08-06
        // chasing an AQW item whose colour is wrong while an ancestor is
        // cached, and it did not change that symptom at all.
        if (dst.a <= 0.0) {
            return src;
        }
        return vec4<f32>(src.rgb * (1.0 - dst.a) + dst.rgb * (1.0 - src.a) + src.a * dst.a * blend_func(src.rgb / src.a, dst.rgb / dst.a), src.a + dst.a * (1.0 - src.a));
    } else {
        if (true) {
            // This needs to be in a branch because... reasons. Bug in naga.
            // https://github.com/gfx-rs/naga/issues/2168
            discard;
        }
        return dst;
    }
}
