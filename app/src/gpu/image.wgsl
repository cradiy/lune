struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) sample_bounds: vec4<f32>,
};

@group(0) @binding(0) var pixels: texture_2d<f32>;
@group(0) @binding(1) var pixel_sampler: sampler;
@group(0) @binding(2) var indices: texture_2d<u32>;
@group(0) @binding(3) var palette: texture_2d<f32>;
@group(0) @binding(4) var<uniform> gather_first: u32;

fn quad_position(bounds: vec4<f32>, corner: vec2<f32>) -> vec4<f32> {
    return vec4<f32>(mix(bounds.xy, bounds.zw, corner), 0.0, 1.0);
}

@vertex
fn vertex_main(
    @builtin(vertex_index) vertex: u32,
    @location(0) bounds: vec4<f32>,
    @location(1) source: vec4<f32>,
    @location(2) sample_bounds: vec4<f32>,
) -> VertexOutput {
    let corner = vec2<f32>(f32(vertex & 1u), f32(vertex >> 1u));
    var output: VertexOutput;
    output.position = quad_position(bounds, corner);
    output.uv = mix(source.xy, source.zw, corner);
    output.sample_bounds = sample_bounds;
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
    let uv = clamp(input.uv, input.sample_bounds.xy, input.sample_bounds.zw);
    var sample: vec4<f32>;
    if gather_first == 0u {
        sample = textureSampleLevel(pixels, pixel_sampler, uv, 0.0);
        if sample.a == 1.0 {
            return sample;
        }
        if sample.a == 0.0 {
            return vec4<f32>(0.0);
        }
    }
    let position = uv * vec2<f32>(textureDimensions(pixels));
    if all(abs(fract(position) - 0.5) < vec2<f32>(0.0001)) {
        if gather_first != 0u {
            sample = textureSampleLevel(pixels, pixel_sampler, uv, 0.0);
        }
        return premultiply(sample);
    }
    let alpha = textureGather(3, pixels, pixel_sampler, uv);
    if all(alpha == vec4<f32>(alpha.x)) {
        if alpha.x == 0.0 {
            return vec4<f32>(0.0);
        }
        if gather_first != 0u {
            sample = textureSampleLevel(pixels, pixel_sampler, uv, 0.0);
        }
        return premultiply(sample);
    }
    let weight = fract(position - 0.5);
    let inverse = 1.0 - weight;
    // Gather order is bottom left, bottom right, top right, top left.
    let weights = vec4<f32>(
        inverse.x * weight.y,
        weight.x * weight.y,
        weight.x * inverse.y,
        inverse.x * inverse.y,
    );
    let weighted_alpha = weights * alpha;
    return vec4<f32>(
        dot(textureGather(0, pixels, pixel_sampler, uv), weighted_alpha),
        dot(textureGather(1, pixels, pixel_sampler, uv), weighted_alpha),
        dot(textureGather(2, pixels, pixel_sampler, uv), weighted_alpha),
        dot(alpha, weights),
    );
}

fn premultiply(color: vec4<f32>) -> vec4<f32> {
    return vec4<f32>(color.rgb * color.a, color.a);
}

struct BilinearFilter {
    first: vec2<i32>,
    last: vec2<i32>,
    weight: vec2<f32>,
};

fn bilinear_filter(position: vec2<f32>, dimensions: vec2<u32>) -> BilinearFilter {
    let centered = position - 0.5;
    let base = vec2<i32>(floor(centered));
    let low = vec2<i32>(0);
    let high = vec2<i32>(dimensions) - 1;
    return BilinearFilter(clamp(base, low, high), clamp(base + 1, low, high), fract(centered));
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
    let position = clamp(input.uv, input.sample_bounds.xy, input.sample_bounds.zw) * extent;
    let low = vec2<i32>(0);
    let high = vec2<i32>(dimensions) - 1;
    if all(abs(step - 1.0) * vec2<f32>(dimensions) < vec2<f32>(0.01))
        || all(dimensions == vec2<u32>(1u)) {
        return indexed_color(clamp(vec2<i32>(position), low, high), has_alpha);
    }

    let taps = bilinear_filter(position, dimensions);
    let top = mix(
        indexed_color(taps.first, has_alpha),
        indexed_color(vec2<i32>(taps.last.x, taps.first.y), has_alpha),
        taps.weight.x,
    );
    let bottom = mix(
        indexed_color(vec2<i32>(taps.first.x, taps.last.y), has_alpha),
        indexed_color(taps.last, has_alpha),
        taps.weight.x,
    );
    return mix(top, bottom, taps.weight.y);
}

@fragment
fn indexed_main(input: VertexOutput) -> @location(0) vec4<f32> {
    return sample_indexed(input, false);
}

@fragment
fn indexed_alpha_main(input: VertexOutput) -> @location(0) vec4<f32> {
    return sample_indexed(input, true);
}
