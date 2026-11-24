struct Uniforms {
    rect: vec4f,
    uv: vec4f,
    bg_srgb: vec4f,
    params: vec4f,
};

@group(0) @binding(0) var image_tex: texture_2d<f32>;
@group(0) @binding(1) var image_sampler: sampler;
@group(0) @binding(2) var<uniform> uniforms: Uniforms;

struct VertexOut {
    @builtin(position) position: vec4f,
    @location(0) uv: vec2f,
};

fn linear_to_srgb_channel(c: f32) -> f32 {
    let clamped = clamp(c, 0.0, 1.0);
    if clamped <= 0.0031308 {
        return clamped * 12.92;
    }
    return 1.055 * pow(clamped, 1.0 / 2.4) - 0.055;
}

fn srgb_to_linear_channel(c: f32) -> f32 {
    let clamped = clamp(c, 0.0, 1.0);
    if clamped <= 0.04045 {
        return clamped / 12.92;
    }
    return pow((clamped + 0.055) / 1.055, 2.4);
}

fn linear_to_srgb(rgb: vec3f) -> vec3f {
    return vec3f(
        linear_to_srgb_channel(rgb.r),
        linear_to_srgb_channel(rgb.g),
        linear_to_srgb_channel(rgb.b),
    );
}

fn srgb_to_linear(rgb: vec3f) -> vec3f {
    return vec3f(
        srgb_to_linear_channel(rgb.r),
        srgb_to_linear_channel(rgb.g),
        srgb_to_linear_channel(rgb.b),
    );
}

@vertex
fn vertex(@builtin(vertex_index) index: u32) -> VertexOut {
    let corner = vec2f(f32(index & 1u), f32(index >> 1u));
    var out: VertexOut;
    out.position = vec4f(mix(uniforms.rect.xy, uniforms.rect.zw, corner), 0.0, 1.0);
    out.uv = mix(uniforms.uv.xy, uniforms.uv.zw, corner);
    return out;
}

@fragment
fn fragment(input: VertexOut) -> @location(0) vec4f {
    let sample = textureSample(image_tex, image_sampler, input.uv);
    // Blend in perceptual sRGB space so low opacities stay deep and natural
    // instead of washing out dark theme backgrounds via linear-light gamma lift.
    let img_srgb = linear_to_srgb(sample.rgb);
    let k = sample.a * uniforms.params.x;
    let blended_lin = srgb_to_linear(mix(uniforms.bg_srgb.rgb, img_srgb, k));
    return vec4f(blended_lin * uniforms.params.y, uniforms.params.z);
}
