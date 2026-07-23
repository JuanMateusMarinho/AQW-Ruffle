/// Present-copy shader with a CRT look: the fixed Catmull-Rom resample of
/// `copy_sharp.wgsl` followed by scanlines and an aperture-grille phosphor
/// mask, both purely positional.
///
/// Selected instead of `copy.wgsl`/`copy_sharp.wgsl` when the in-game
/// "CRT Filter" option is ON (see `aqw_crt_filter_enabled`). Keeping every
/// weight positional honours the copy_sharp lesson: content-adaptive
/// per-pixel weighting (CAS) was field-rejected for making AA'd vector art
/// crawl. Scanline/mask factors depend only on the output raster position,
/// never on the sampled colors, so the image stays stable in motion.
///
/// The barrel warp (WARP) is mirrored on the host side: the desktop's
/// `window_to_movie_position` applies the same forward warp to mouse
/// coordinates (shared constant `aqw_crt_warp_strength`), so clicks land
/// exactly where the warped pixels appear. Keep both in lockstep.

// NOTE: The `common.wgsl` source is prepended to this before compilation.

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@group(1) @binding(0) var<uniform> transforms: common__Transforms;
@group(2) @binding(0) var<uniform> textureTransforms: common__TextureTransforms;
@group(2) @binding(1) var texture: texture_2d<f32>;
@group(2) @binding(2) var texture_sampler: sampler;

// These three lines are rewritten at startup (see `shaders.rs`), keeping the
// pipeline uniform-free. SHARPNESS matches copy_sharp; SCANLINE/MASK are the
// CRT strengths (RUFFLE_AQW_CRT_SCANLINE / RUFFLE_AQW_CRT_MASK).
// Scanlines are the dominant structure (HORIZONTAL - firm field decision;
// continuous vertical stripes were rejected twice). The phosphor mask
// returned at the user's explicit request (2026-07-18) as a low-strength
// SLOT mask: RGB segments with staggered horizontal breaks, which reads as
// texture rather than vertical lines. MASK_TYPE 0 = aperture grille
// (continuous stripes, env-only), 1 = slot mask. ABERRATION is the
// horizontal R/B displacement in output pixels (subtle color fringing).
const SHARPNESS: f32 = 0.5;
const SCANLINE: f32 = 0.85;
const MASK: f32 = 0.3;
const MASK_TYPE: u32 = 1u;
const ABERRATION: f32 = 0.7;
// Phosphor glow: bright areas bleed a soft halo over the scanlines, like a
// real tube's light spill. Weighted by brightness squared so dark areas
// stay clean (no washed-out frame - that was field-rejected).
const GLOW: f32 = 0.45;
// Tube feel: slight overall softness (a tube is never LCD-sharp) and a
// mild corner vignette.
const SOFTNESS: f32 = 0.16;
const VIGNETTE: f32 = 0.2;
// Halation: light scattering in the tube's glass, spilling from bright
// content into neighboring dark areas (the "bleeding" over blacks).
// Linear (not squared) so it actually lifts blacks next to light.
const HALATION: f32 = 0.12;
// Overall brightness trim on the compensation. 1.0 for AQW (dark-toned
// art, field-approved); DragonFable's bright parchment UI clips with the
// same tuning, so it bakes a lower value (see `shaders.rs`).
const BRIGHT: f32 = 1.0;
// Barrel warp: gentle tube curvature. Pixels outside the warped image are
// black (the rounded dark corners of a tube). MUST match the host's mouse
// warp (see header).
const WARP: f32 = 0.04;

// 4:3 aspect (1u = on): squeeze the 16:9 content into a centred region whose
// displayed aspect is 4:3, dark tube surround on the sides. Rewritten at
// startup from `aqw_crt_aspect_43_enabled`; the host mirrors the same squeeze
// in `window_to_movie_position`, so keep the two in lockstep.
const ASPECT_43: u32 = 1u;

// Virtual line count cap: AQW's native stage is 960x540, so simulate (at
// most) a 540-line tube stretched over the output. Below 1080 output lines
// the count adapts so a scanline always spans >= 2 output pixels - visible
// at any window size instead of dissolving into moire. (NO_SCALE means the
// game really renders at window resolution - the scanlines are cosmetic,
// not reconstructive.)
const LINES: f32 = 540.0;
const TAU: f32 = 6.28318530718;

@vertex
fn main_vertex(in: common__VertexInput) -> VertexOutput {
    let matrix_ = textureTransforms.texture_matrix;
    let uv = (mat3x3<f32>(matrix_[0].xyz, matrix_[1].xyz, matrix_[2].xyz) * vec3<f32>(in.position, 1.0)).xy;
    let pos = common__globals.view_matrix * transforms.world_matrix * vec4<f32>(in.position.x, in.position.y, 0.0, 1.0);
    return VertexOutput(pos, uv);
}

@fragment
fn main_fragment(in: VertexOutput) -> @location(0) vec4<f32> {
    // --- 4:3 squeeze (CRT-only) ---
    // Map the output position into "tube space": for ASPECT_43 the tube is a
    // centred region whose displayed aspect is 4:3, and everything below
    // (warp, resample, scanlines, mask, vignette) then operates within it, so
    // the phosphor raster stays aligned to the squeezed content and the
    // vignette hugs the tube's corners. Outside the region is the dark
    // surround. content_frac = (4/3) / window_aspect: 0.75 on a 16:9 window,
    // 1.0 (no squeeze) once the window is 4:3 or narrower. The host mirrors
    // this in `window_to_movie_position` so clicks track the squeezed picture.
    var ruv = in.uv;
    if (ASPECT_43 == 1u) {
        let td = vec2<f32>(textureDimensions(texture));
        let content_frac = clamp((4.0 / 3.0) / (td.x / td.y), 0.0, 1.0);
        let margin = (1.0 - content_frac) * 0.5;
        ruv.x = (in.uv.x - margin) / content_frac;
        if (ruv.x < 0.0 || ruv.x > 1.0) {
            return vec4<f32>(0.0, 0.0, 0.0, 1.0);
        }
    }

    // --- Barrel warp ---
    // The screen position `ruv` shows the content at warp(ruv); outside
    // the warped image the tube face is dark. The host applies this same
    // function to mouse coordinates, keeping clicks aligned.
    var uv = ruv;
    if (WARP > 0.0) {
        var cc = uv * 2.0 - vec2<f32>(1.0, 1.0);
        cc = cc * (1.0 + WARP * dot(cc, cc));
        uv = (cc + vec2<f32>(1.0, 1.0)) * 0.5;
        if (uv.x < 0.0 || uv.x > 1.0 || uv.y < 0.0 || uv.y > 1.0) {
            return vec4<f32>(0.0, 0.0, 0.0, 1.0);
        }
    }

    let dims = vec2<f32>(textureDimensions(texture));
    // --- Catmull-Rom resample (identical to copy_sharp.wgsl) ---
    let sample_pos = uv * dims;
    let tex_pos1 = floor(sample_pos - 0.5) + 0.5;
    let f = sample_pos - tex_pos1;
    let f2 = f * f;
    let f3 = f2 * f;

    let c = mix(0.25, 0.75, SHARPNESS);
    let w0 = -c * f + 2.0 * c * f2 - c * f3;
    let w1 = vec2<f32>(1.0, 1.0) + (c - 3.0) * f2 + (2.0 - c) * f3;
    let w2 = c * f + (3.0 - 2.0 * c) * f2 + (c - 2.0) * f3;
    let w3 = -c * f2 + c * f3;

    let w12 = w1 + w2;
    let offset12 = w2 / w12;
    let tp0 = (tex_pos1 - 1.0) / dims;
    let tp3 = (tex_pos1 + 2.0) / dims;
    let tp12 = (tex_pos1 + offset12) / dims;

    var result = textureSampleLevel(texture, texture_sampler, vec2<f32>(tp0.x, tp0.y), 0.0) * w0.x * w0.y
        + textureSampleLevel(texture, texture_sampler, vec2<f32>(tp12.x, tp0.y), 0.0) * w12.x * w0.y
        + textureSampleLevel(texture, texture_sampler, vec2<f32>(tp3.x, tp0.y), 0.0) * w3.x * w0.y
        + textureSampleLevel(texture, texture_sampler, vec2<f32>(tp0.x, tp12.y), 0.0) * w0.x * w12.y
        + textureSampleLevel(texture, texture_sampler, vec2<f32>(tp12.x, tp12.y), 0.0) * w12.x * w12.y
        + textureSampleLevel(texture, texture_sampler, vec2<f32>(tp3.x, tp12.y), 0.0) * w3.x * w12.y
        + textureSampleLevel(texture, texture_sampler, vec2<f32>(tp0.x, tp3.y), 0.0) * w0.x * w3.y
        + textureSampleLevel(texture, texture_sampler, vec2<f32>(tp12.x, tp3.y), 0.0) * w12.x * w3.y
        + textureSampleLevel(texture, texture_sampler, vec2<f32>(tp3.x, tp3.y), 0.0) * w3.x * w3.y;
    result = clamp(result, vec4<f32>(0.0), vec4<f32>(1.0));

    // --- Chromatic aberration ---
    // Displace R right / B left by a fraction of an output pixel: the
    // subtle color fringing of a tube's imperfect convergence.
    var col = result.rgb;
    if (ABERRATION > 0.0) {
        let ca = ABERRATION * fwidth(ruv.x);
        col.r = textureSampleLevel(texture, texture_sampler, uv + vec2<f32>(ca, 0.0), 0.0).r;
        col.b = textureSampleLevel(texture, texture_sampler, uv - vec2<f32>(ca, 0.0), 0.0).b;
    }

    // --- Tube softness + phosphor glow (before the scanlines, so the
    // dark rows cut through the halo and stay crisp; glow over the lines
    // erased them, field-observed) ---
    let gx = 1.6 * fwidth(ruv.x);
    let gy = 1.6 * fwidth(ruv.y);
    let halo = (textureSampleLevel(texture, texture_sampler, uv + vec2<f32>(gx, 0.0), 0.0).rgb
        + textureSampleLevel(texture, texture_sampler, uv - vec2<f32>(gx, 0.0), 0.0).rgb
        + textureSampleLevel(texture, texture_sampler, uv + vec2<f32>(0.0, gy), 0.0).rgb
        + textureSampleLevel(texture, texture_sampler, uv - vec2<f32>(0.0, gy), 0.0).rgb)
        * 0.25;
    // A tube is never LCD-sharp: blend a touch of the blur into the image.
    col = mix(col, halo, SOFTNESS);
    col = min(col + GLOW * halo * halo, vec3<f32>(1.0));

    // --- Halation (bleeding over blacks) ---
    // A wider, linear spill: light scattered in the glass lifts the dark
    // areas around bright content. Linear so blacks actually receive it
    // (the squared glow above vanishes in the dark).
    if (HALATION > 0.0) {
        let hx = 4.0 * fwidth(ruv.x);
        let hy = 4.0 * fwidth(ruv.y);
        let spill = (textureSampleLevel(texture, texture_sampler, uv + vec2<f32>(hx, hy), 0.0).rgb
            + textureSampleLevel(texture, texture_sampler, uv + vec2<f32>(-hx, hy), 0.0).rgb
            + textureSampleLevel(texture, texture_sampler, uv + vec2<f32>(hx, -hy), 0.0).rgb
            + textureSampleLevel(texture, texture_sampler, uv + vec2<f32>(-hx, -hy), 0.0).rgb)
            * 0.25;
        col = min(col + HALATION * spill, vec3<f32>(1.0));
    }

    // --- Scanlines ---
    // Discrete dark rows by integer output pixel, one per virtual line at
    // a 3px pitch (2 lit + 1 dark). Field-tested: 2px pitch (1px dark
    // rows) is invisible on the user's display regardless of strength;
    // 3px is the finest pitch that reads as lines there. (A cosine
    // profile sampled at pixel centers cancels out at exactly 2px pitch -
    // also field-observed as "just darker, no lines".)
    //
    // Beam-width modulation, the signature tube behavior: where the image
    // is bright the beam blooms and partially fills the gap between lines
    // (shallower dark row); in dark areas the gap stays deep.
    // Row index from the WARPED coordinate so the lines curve with the
    // image, like they do on the glass of a real tube.
    let out_lines = 1.0 / max(fwidth(ruv.y), 1e-6);
    let lines = min(LINES, out_lines * 0.3333);
    let span_px = max(3u, u32(round(out_lines / lines)));
    if (u32(uv.y * out_lines) % span_px == span_px - 1u) {
        let luma = dot(col, vec3<f32>(0.299, 0.587, 0.114));
        let scan_strength = SCANLINE * (1.0 - 0.5 * luma);
        col = col * (1.0 - scan_strength);
    }

    // --- Vignette ---
    // Mild corner falloff, like a tube's edge.
    let v = 1.0 - VIGNETTE * smoothstep(0.6, 1.05, length(uv - vec2<f32>(0.5, 0.5)) * 1.4142);
    col = col * v;

    // --- Phosphor mask (RGB triads; grille or slot) ---
    // Stripe width in WHOLE output pixels (integer math): fractional
    // widths sample the triad unevenly across the frame and tint whole
    // regions (field-observed as a green cast). Width scales with the
    // scanline pitch so the triads stay chunky at any output size.
    let span = out_lines / lines;
    let subw = max(1u, u32(round(span * 0.75)));
    let out_cols = 1.0 / max(fwidth(ruv.x), 1e-6);
    let px = u32(uv.x * out_cols);
    let stripe = (px / subw) % 3u;
    var grille = vec3<f32>(1.0 - MASK);
    if (stripe == 0u) {
        grille.r = 1.0;
    } else if (stripe == 1u) {
        grille.g = 1.0;
    } else {
        grille.b = 1.0;
    }
    // Dark gap on the last pixel column of each stripe (only when stripes
    // are wide enough to spare a column).
    var gap = 1.0;
    if (subw >= 2u && px % subw == subw - 1u) {
        gap = 1.0 - 0.6 * MASK;
    }
    if (MASK_TYPE == 1u) {
        // Slot mask: horizontal dark breaks staggered between adjacent
        // triad columns (the brick pattern). Breaks the stripes up so the
        // mask reads as phosphor texture, not vertical lines.
        let triad = px / (subw * 3u);
        let slot_h = span_px * 2u;
        let py = u32(uv.y * out_lines) + (triad % 2u) * (slot_h / 2u);
        if (py % slot_h == slot_h - 1u) {
            gap = gap * (1.0 - 0.6 * MASK);
        }
    }
    col = col * grille * gap;

    // Gentle brightness compensation only (field-tuned): just enough to
    // offset the grid's darkening. The aggressive full compensation was
    // rejected for washing the frame out / clipping the highlights.
    let comp = (1.0 + 0.25 * SCANLINE + 0.3 * MASK) * BRIGHT;
    col = min(col * comp, vec3<f32>(1.0));

    return vec4<f32>(col, result.a);
}
