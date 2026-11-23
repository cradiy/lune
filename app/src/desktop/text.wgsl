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
    let glyph = textureLoad(atlas, vec2i(input.uv), i32(input.sprite.y), 0);
    let color = select(input.color.rgb, glyph.rgb, input.sprite.x == 2u);
    return vec4f(color, glyph.a * input.color.a);
}
