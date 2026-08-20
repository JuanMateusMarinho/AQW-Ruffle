use crate::blend::ComplexBlend;
use enum_map::{EnumMap, enum_map};
use ruffle_render::shader_source::SHADER_FILTER_COMMON;

#[derive(Debug)]
pub struct Shaders {
    pub color_shader: wgpu::ShaderModule,
    /// This has a pipeline-overridable `bool` constant, `late_saturate`,
    /// with a default of `false`. It switches to performing saturation
    /// after re-multiplying the alpha, rather than before. This is used
    /// for the Stage3D `bitmap_opaque` pipeline, which needs to be able to
    /// avoid changing initially-in-range rgb values (regadless of whether
    /// dividing by the alpha value would produce an out-of-range value).
    pub bitmap_shader: wgpu::ShaderModule,
    pub gradient_shader: wgpu::ShaderModule,
    pub copy_shader: wgpu::ShaderModule,
    pub copy_sharp_shader: wgpu::ShaderModule,
    pub copy_crt_shader: wgpu::ShaderModule,
    pub alpha_mask_shader: wgpu::ShaderModule,
    pub blend_shaders: EnumMap<ComplexBlend, wgpu::ShaderModule>,
    pub color_matrix_filter: wgpu::ShaderModule,
    pub blur_filter: wgpu::ShaderModule,
    pub glow_filter: wgpu::ShaderModule,
    pub bevel_filter: wgpu::ShaderModule,
    pub displacement_map_filter: wgpu::ShaderModule,
}

impl Shaders {
    pub fn new(device: &wgpu::Device) -> Self {
        let color_shader = make_shader(device, "color.wgsl", include_str!("../shaders/color.wgsl"));
        let bitmap_shader = make_shader(
            device,
            "bitmap.wgsl",
            include_str!("../shaders/bitmap.wgsl"),
        );
        let copy_shader = make_shader(device, "copy.wgsl", include_str!("../shaders/copy.wgsl"));
        let copy_sharp_shader = {
            let source = include_str!("../shaders/copy_sharp.wgsl").replace(
                "const SHARPNESS: f32 = 0.5;",
                &format!("const SHARPNESS: f32 = {:.4};", aqw_present_sharpness()),
            );
            make_shader(device, "copy_sharp.wgsl", &source)
        };
        let copy_crt_shader = {
            let source = include_str!("../shaders/copy_crt.wgsl")
                .replace(
                    "const SHARPNESS: f32 = 0.5;",
                    &format!("const SHARPNESS: f32 = {:.4};", aqw_present_sharpness()),
                )
                .replace(
                    "const SCANLINE: f32 = 0.85;",
                    &format!(
                        "const SCANLINE: f32 = {:.4};",
                        aqw_crt_env_strength("RUFFLE_AQW_CRT_SCANLINE", 0.85)
                    ),
                )
                .replace(
                    "const MASK: f32 = 0.3;",
                    &format!(
                        "const MASK: f32 = {:.4};",
                        aqw_crt_env_strength("RUFFLE_AQW_CRT_MASK", 0.3)
                    ),
                )
                .replace(
                    "const MASK_TYPE: u32 = 1u;",
                    &format!(
                        "const MASK_TYPE: u32 = {}u;",
                        match std::env::var("RUFFLE_AQW_CRT_MASK_TYPE").as_deref() {
                            Ok("grille") => 0,
                            _ => 1,
                        }
                    ),
                )
                .replace(
                    "const HALATION: f32 = 0.12;",
                    &format!(
                        "const HALATION: f32 = {:.4};",
                        aqw_crt_env_strength(
                            "RUFFLE_AQW_CRT_HALATION",
                            if artix_game_is_dragonfable() {
                                0.08
                            } else {
                                0.12
                            }
                        )
                    ),
                )
                .replace(
                    "const GLOW: f32 = 0.45;",
                    &format!(
                        "const GLOW: f32 = {:.4};",
                        aqw_crt_env_strength(
                            "RUFFLE_AQW_CRT_GLOW",
                            if artix_game_is_dragonfable() {
                                0.3
                            } else {
                                0.45
                            }
                        )
                    ),
                )
                .replace(
                    "const BRIGHT: f32 = 1.0;",
                    &format!(
                        "const BRIGHT: f32 = {:.4};",
                        std::env::var("RUFFLE_AQW_CRT_BRIGHTNESS")
                            .ok()
                            .and_then(|v| v.trim().parse::<f32>().ok())
                            .filter(|n| n.is_finite())
                            .map(|n| n.clamp(0.0, 1.5))
                            .unwrap_or(if artix_game_is_dragonfable() {
                                0.85
                            } else {
                                1.0
                            })
                    ),
                )
                .replace(
                    "const SOFTNESS: f32 = 0.16;",
                    &format!(
                        "const SOFTNESS: f32 = {:.4};",
                        aqw_crt_env_strength("RUFFLE_AQW_CRT_SOFTNESS", 0.16)
                    ),
                )
                .replace(
                    "const VIGNETTE: f32 = 0.2;",
                    &format!(
                        "const VIGNETTE: f32 = {:.4};",
                        aqw_crt_env_strength("RUFFLE_AQW_CRT_VIGNETTE", 0.2)
                    ),
                )
                .replace(
                    "const WARP: f32 = 0.04;",
                    &format!(
                        "const WARP: f32 = {:.4};",
                        ruffle_render::backend::aqw_crt_warp_strength()
                    ),
                )
                .replace(
                    "const ABERRATION: f32 = 0.7;",
                    &format!(
                        "const ABERRATION: f32 = {:.4};",
                        std::env::var("RUFFLE_AQW_CRT_ABERRATION")
                            .ok()
                            .and_then(|v| v.trim().parse::<f32>().ok())
                            .filter(|n| n.is_finite())
                            .map(|n| n.clamp(0.0, 4.0))
                            .unwrap_or(0.7)
                    ),
                )
                .replace(
                    "const ASPECT_43: u32 = 1u;",
                    &format!(
                        "const ASPECT_43: u32 = {}u;",
                        u32::from(ruffle_render::backend::aqw_crt_aspect_43_enabled())
                    ),
                );
            make_shader(device, "copy_crt.wgsl", &source)
        };
        let color_matrix_filter = make_filter_shader(
            device,
            "filter/color_matrix.wgsl",
            include_str!("../shaders/filter/color_matrix.wgsl"),
        );
        let blur_filter = make_filter_shader(
            device,
            "filter/blur.wgsl",
            include_str!("../shaders/filter/blur.wgsl"),
        );
        let glow_filter = make_filter_shader(
            device,
            "filter/glow.wgsl",
            include_str!("../shaders/filter/glow.wgsl"),
        );
        let bevel_filter = make_filter_shader(
            device,
            "filter/bevel.wgsl",
            include_str!("../shaders/filter/bevel.wgsl"),
        );
        let displacement_map_filter = make_filter_shader(
            device,
            "filter/displacement_map.wgsl",
            include_str!("../shaders/filter/displacement_map.wgsl"),
        );
        let gradient_shader = make_shader(
            device,
            "gradient.wgsl",
            include_str!("../shaders/gradient.wgsl"),
        );
        let alpha_mask_shader = make_shader(
            device,
            "alpha_mask.wgsl",
            include_str!("../shaders/alpha_mask.wgsl"),
        );

        let blend_shaders = enum_map! {
            ComplexBlend::Multiply => make_shader(device, "blend/multiply.wgsl", include_str!("../shaders/blend/multiply.wgsl")),
            ComplexBlend::Lighten => make_shader(device, "blend/lighten.wgsl", include_str!("../shaders/blend/lighten.wgsl")),
            ComplexBlend::Darken => make_shader(device, "blend/darken.wgsl", include_str!("../shaders/blend/darken.wgsl")),
            ComplexBlend::Difference => make_shader(device, "blend/difference.wgsl", include_str!("../shaders/blend/difference.wgsl")),
            ComplexBlend::Invert => make_shader(device, "blend/invert.wgsl", include_str!("../shaders/blend/invert.wgsl")),
            ComplexBlend::Alpha => make_shader(device, "blend/alpha.wgsl", include_str!("../shaders/blend/alpha.wgsl")),
            ComplexBlend::Erase => make_shader(device, "blend/erase.wgsl", include_str!("../shaders/blend/erase.wgsl")),
            ComplexBlend::Overlay => make_shader(device, "blend/overlay.wgsl", include_str!("../shaders/blend/overlay.wgsl")),
            ComplexBlend::HardLight => make_shader(device, "blend/hardlight.wgsl", include_str!("../shaders/blend/hardlight.wgsl")),
        };

        Self {
            color_shader,
            bitmap_shader,
            gradient_shader,
            copy_shader,
            copy_sharp_shader,
            copy_crt_shader,
            alpha_mask_shader,
            blend_shaders,
            color_matrix_filter,
            blur_filter,
            glow_filter,
            bevel_filter,
            displacement_map_filter,
        }
    }
}

fn aqw_present_sharpness() -> f32 {
    std::env::var("RUFFLE_AQW_SHARPNESS")
        .ok()
        .and_then(|v| v.trim().parse::<f32>().ok())
        .filter(|n| n.is_finite())
        .map(|n| n.clamp(0.0, 1.0))
        .unwrap_or(0.5)
}

fn artix_game_is_dragonfable() -> bool {
    std::env::var("ARTIX_RUFFLE_GAME").is_ok_and(|v| v == "df")
        || std::env::var("ARTIX_RUFFLE_GAME_ICON").is_ok_and(|v| v == "dragonfable")
}

fn aqw_crt_env_strength(var: &str, default: f32) -> f32 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.trim().parse::<f32>().ok())
        .filter(|n| n.is_finite())
        .map(|n| n.clamp(0.0, 1.0))
        .unwrap_or(default)
}

fn make_shader(device: &wgpu::Device, name: &str, source: &str) -> wgpu::ShaderModule {
    let common = include_str!("../shaders/common.wgsl");
    device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: create_debug_label!("Shader {name}").as_deref(),
        source: wgpu::ShaderSource::Wgsl(format!("{common}\n{source}").into()),
    })
}
fn make_filter_shader(device: &wgpu::Device, name: &str, source: &str) -> wgpu::ShaderModule {
    device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: create_debug_label!("Shader {name}").as_deref(),
        source: wgpu::ShaderSource::Wgsl(format!("{SHADER_FILTER_COMMON}\n{source}").into()),
    })
}
