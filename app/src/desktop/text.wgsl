@group(0) @binding(0) var atlas: texture_2d_array<f32>;

struct Vertex {
    @builtin(position) position: vec4f,
    @location(0) uv: vec2f,
    @location(1) color: vec4f,
    @location(2) @interpolate(flat) sprite: vec2u,
}

@vertex
fn vertex(
    @builtin(vertex_index) index: u32,
    @location(0) rect: vec4f,
    @location(1) uv: vec4f,
    @location(2) color: vec4f,
    @location(3) sprite: vec2u,
) -> Vertex {
    let corner = vec2f(f32(index & 1u), f32(index >> 1u));
    return Vertex(vec4f(rect.xy + corner * rect.zw, 0.0, 1.0),
                  uv.xy + corner * uv.zw, color, sprite);
}

@fragment
fn fragment(input: Vertex) -> @location(0) vec4f {
    if input.sprite.x == 0u {
        return input.color;
    }
    if input.sprite.x == 3u {
        let thickness = f32(max(input.sprite.y & 0xffu, 1u));
        let height = f32(max((input.sprite.y >> 8u) & 0xffu, 3u));
        let period = f32(max(input.sprite.y >> 16u, 4u));
        let amp = max((height - thickness) * 0.5, 0.5);
        let freq = 6.2831853 / period;
        let phase = input.uv.x * freq;
        let wave_y = amp * sin(phase);
        let slope = amp * freq * cos(phase);
        let dist = abs(input.uv.y - wave_y) * inverseSqrt(1.0 + slope * slope);
        let cov = clamp(thickness * 0.5 + 0.5 - dist, 0.0, 1.0);
        return vec4f(input.color.rgb, input.color.a * cov);
    }
    if input.sprite.x == 4u {
        let thickness = f32(max(input.sprite.y & 0xffu, 1u));
        let step = max(thickness * 2.0, 2.0);
        let local = fract(input.uv.x / step) * step;
        let cov = select(0.0, 1.0, local < thickness);
        return vec4f(input.color.rgb, input.color.a * cov);
    }
    if input.sprite.x == 5u {
        let thickness = f32(max(input.sprite.y & 0xffu, 1u));
        let dash = max(thickness * 4.0, 4.0);
        let step = dash * 2.0;
        let local = fract(input.uv.x / step) * step;
        let cov = select(0.0, 1.0, local < dash);
        return vec4f(input.color.rgb, input.color.a * cov);
    }
    if input.sprite.x == 6u {
        let radius = f32(input.sprite.y & 0x3fu);
        let thickness = f32((input.sprite.y >> 6u) & 0xfu);
        let size = vec2f(
            f32((input.sprite.y >> 10u) & 0x7ffu),
            f32((input.sprite.y >> 21u) & 0x7ffu),
        );
        let half_size = size * 0.5;
        let p = abs(input.uv) - (half_size - vec2f(radius));
        let sdf = length(max(p, vec2f(0.0))) + min(max(p.x, p.y), 0.0) - radius;
        var cov: f32;
        if thickness == 0.0 {
            cov = clamp(0.5 - sdf, 0.0, 1.0);
        } else {
            let edge = abs(sdf + thickness * 0.5) - thickness * 0.5;
            cov = clamp(0.5 - edge, 0.0, 1.0);
        }
        return vec4f(input.color.rgb, input.color.a * cov);
    }
    let glyph = textureLoad(atlas, vec2i(input.uv), i32(input.sprite.y), 0);
    let color = select(input.color.rgb, glyph.rgb, input.sprite.x == 2u);
    return vec4f(color, glyph.a * input.color.a);
}
