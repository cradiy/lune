struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@group(0) @binding(0) var pixels: texture_2d<f32>;
@group(0) @binding(1) var pixel_sampler: sampler;
@group(0) @binding(2) var indices: texture_2d<u32>;
@group(0) @binding(3) var palette: texture_2d<f32>;

@vertex
fn vertex_main(
    @builtin(vertex_index) vertex: u32,
    @location(0) bounds: vec4<f32>,
    @location(1) source: vec4<f32>,
) -> VertexOutput {
    let corner = vec2<f32>(f32(vertex & 1u), f32(vertex >> 1u));
    var output: VertexOutput;
    output.position = vec4<f32>(mix(bounds.xy, bounds.zw, corner), 0.0, 1.0);
    output.uv = mix(source.xy, source.zw, corner);
    return output;
}

@fragment
fn fragment_main(input: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(pixels, pixel_sampler, input.uv);
}

fn indexed_pixel(uv: vec2<f32>) -> vec2<u32> {
    let size = textureDimensions(indices);
    let texel = clamp(vec2<i32>(uv * vec2<f32>(size)), vec2<i32>(0), vec2<i32>(size) - 1);
    return textureLoad(indices, texel, 0).rg;
}

fn palette_color(index: u32) -> vec3<f32> {
    return textureLoad(palette, vec2<i32>(i32(index), 0), 0).rgb;
}

@fragment
fn indexed_main(input: VertexOutput) -> @location(0) vec4<f32> {
    return vec4<f32>(palette_color(indexed_pixel(input.uv).r), 1.0);
}

@fragment
fn indexed_alpha_main(input: VertexOutput) -> @location(0) vec4<f32> {
    let pixel = indexed_pixel(input.uv);
    return vec4<f32>(palette_color(pixel.r), f32(pixel.g) / 255.0);
}
