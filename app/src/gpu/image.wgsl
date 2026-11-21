struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) sample_rows: vec2<f32>,
};

@group(0) @binding(0) var pixels: texture_2d<f32>;
@group(0) @binding(1) var pixel_sampler: sampler;
@group(0) @binding(2) var indices: texture_2d<u32>;
@group(0) @binding(3) var palette: texture_2d<f32>;

fn quad_position(bounds: vec4<f32>, corner: vec2<f32>) -> vec4<f32> {
    return vec4<f32>(mix(bounds.xy, bounds.zw, corner), 0.0, 1.0);
}

@vertex
fn vertex_main(
    @builtin(vertex_index) vertex: u32,
    @location(0) bounds: vec4<f32>,
    @location(1) source: vec4<f32>,
    @location(2) sample_rows: vec2<f32>,
) -> VertexOutput {
    let corner = vec2<f32>(f32(vertex & 1u), f32(vertex >> 1u));
    var output: VertexOutput;
    output.position = quad_position(bounds, corner);
    output.uv = mix(source.xy, source.zw, corner);
    output.sample_rows = sample_rows;
    return output;
}

struct SolidQuad {
    @builtin(position) position: vec4<f32>,
    @location(0) @interpolate(flat) color: vec4<f32>,
};

@vertex
fn solid_vertex(
    @builtin(vertex_index) vertex: u32,
    @location(0) bounds: vec4<f32>,
    @location(1) color: vec4<f32>,
) -> SolidQuad {
    let corner = vec2<f32>(f32(vertex & 1u), f32(vertex >> 1u));
    return SolidQuad(quad_position(bounds, corner), color);
}

@fragment
fn solid_fragment(input: SolidQuad) -> @location(0) vec4<f32> {
    return input.color;
}

@fragment
fn fragment_main(input: VertexOutput) -> @location(0) vec4<f32> {
    let y = clamp(input.uv.y, input.sample_rows.x, input.sample_rows.y);
    return textureSampleLevel(pixels, pixel_sampler, vec2<f32>(input.uv.x, y), 0.0);
}

fn indexed_color(texel: vec2<i32>, has_alpha: bool) -> vec4<f32> {
    let index = textureLoad(indices, texel, 0).rg;
    let rgb = textureLoad(palette, vec2<i32>(i32(index.r), 0), 0).rgb;
    let alpha = select(1.0, f32(index.g) / 255.0, has_alpha);
    return vec4<f32>(rgb * alpha, alpha);
}

fn sample_indexed(input: VertexOutput, has_alpha: bool) -> vec4<f32> {
    let dimensions = textureDimensions(indices);
    let extent = vec2<f32>(dimensions);
    let step = abs(vec2<f32>(dpdx(input.uv.x), dpdy(input.uv.y))) * extent;
    let y = clamp(input.uv.y, input.sample_rows.x, input.sample_rows.y);
    let position = vec2<f32>(input.uv.x, y) * extent;
    let low = vec2<i32>(0);
    let high = vec2<i32>(dimensions) - 1;
    if all(abs(step - 1.0) * vec2<f32>(dimensions) < vec2<f32>(0.01))
        || all(dimensions == vec2<u32>(1u)) {
        return indexed_color(clamp(vec2<i32>(position), low, high), has_alpha);
    }

    let centered = position - 0.5;
    let base = vec2<i32>(floor(centered));
    let first = clamp(base, low, high);
    let last = clamp(base + 1, low, high);
    let weight = fract(centered);
    let top = mix(
        indexed_color(first, has_alpha),
        indexed_color(vec2<i32>(last.x, first.y), has_alpha),
        weight.x,
    );
    let bottom = mix(
        indexed_color(vec2<i32>(first.x, last.y), has_alpha),
        indexed_color(last, has_alpha),
        weight.x,
    );
    return mix(top, bottom, weight.y);
}

@fragment
fn indexed_main(input: VertexOutput) -> @location(0) vec4<f32> {
    return sample_indexed(input, false);
}

@fragment
fn indexed_alpha_main(input: VertexOutput) -> @location(0) vec4<f32> {
    return sample_indexed(input, true);
}
