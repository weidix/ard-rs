struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

struct YuvConversion {
    red: vec4<f32>,
    green: vec4<f32>,
    blue: vec4<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VertexOutput {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0)
    );
    var coordinates = array<vec2<f32>, 3>(
        vec2<f32>(0.0, 1.0), vec2<f32>(2.0, 1.0), vec2<f32>(0.0, -1.0)
    );
    var output: VertexOutput;
    output.position = vec4<f32>(positions[vertex_index], 0.0, 1.0);
    output.uv = coordinates[vertex_index];
    return output;
}

@group(1) @binding(0) var image_sampler: sampler;
@group(1) @binding(1) var luma_image: texture_2d<f32>;
@group(1) @binding(2) var chroma_image: texture_2d<f32>;
@group(1) @binding(3) var<uniform> conversion: YuvConversion;

fn srgb_to_linear_component(value: f32) -> f32 {
    if (value <= 0.04045) {
        return value / 12.92;
    }
    return pow((value + 0.055) / 1.055, 2.4);
}

fn encoded_to_output(encoded: vec3<f32>) -> vec4<f32> {
    let clamped = clamp(encoded, vec3<f32>(0.0), vec3<f32>(1.0));
    return vec4<f32>(
        srgb_to_linear_component(clamped.r),
        srgb_to_linear_component(clamped.g),
        srgb_to_linear_component(clamped.b),
        1.0
    );
}

fn convert_yuv(sample: vec3<f32>) -> vec3<f32> {
    let value = vec4<f32>(sample, 1.0);
    return vec3<f32>(
        dot(conversion.red, value),
        dot(conversion.green, value),
        dot(conversion.blue, value)
    );
}

fn cubic_weight(value: f32) -> f32 {
    let x = abs(value);
    if (x <= 1.0) {
        return ((1.5 * x - 2.5) * x) * x + 1.0;
    }
    if (x < 2.0) {
        return ((-0.5 * x + 2.5) * x - 4.0) * x + 2.0;
    }
    return 0.0;
}

fn sample_luma_sharp(uv: vec2<f32>) -> f32 {
    let dimensions_u = textureDimensions(luma_image);
    let dimensions = vec2<f32>(dimensions_u);
    let source = uv * dimensions - vec2<f32>(0.5);
    let base = vec2<i32>(floor(source));
    let fraction = fract(source);
    let maximum = vec2<i32>(dimensions_u) - vec2<i32>(1);
    var value = 0.0;
    var weight_sum = 0.0;
    for (var row = -1; row <= 2; row++) {
        let weight_y = cubic_weight(f32(row) - fraction.y);
        for (var column = -1; column <= 2; column++) {
            let weight = cubic_weight(f32(column) - fraction.x) * weight_y;
            let location = clamp(base + vec2<i32>(column, row), vec2<i32>(0), maximum);
            value += textureLoad(luma_image, location, 0).r * weight;
            weight_sum += weight;
        }
    }
    return value / weight_sum;
}

fn sample_chroma_sharp(uv: vec2<f32>) -> vec2<f32> {
    let dimensions_u = textureDimensions(chroma_image);
    let dimensions = vec2<f32>(dimensions_u);
    let source = uv * dimensions - vec2<f32>(0.5);
    let base = vec2<i32>(floor(source));
    let fraction = fract(source);
    let maximum = vec2<i32>(dimensions_u) - vec2<i32>(1);
    var value = vec2<f32>(0.0);
    var weight_sum = 0.0;
    for (var row = -1; row <= 2; row++) {
        let weight_y = cubic_weight(f32(row) - fraction.y);
        for (var column = -1; column <= 2; column++) {
            let weight = cubic_weight(f32(column) - fraction.x) * weight_y;
            let location = clamp(base + vec2<i32>(column, row), vec2<i32>(0), maximum);
            value += textureLoad(chroma_image, location, 0).rg * weight;
            weight_sum += weight;
        }
    }
    return value / weight_sum;
}

@fragment
fn fs_interpolated(input: VertexOutput) -> @location(0) vec4<f32> {
    let y = textureSample(luma_image, image_sampler, input.uv).r;
    let cbcr = textureSample(chroma_image, image_sampler, input.uv).rg;
    return encoded_to_output(convert_yuv(vec3<f32>(y, cbcr)));
}

@fragment
fn fs_sharp(input: VertexOutput) -> @location(0) vec4<f32> {
    let source_footprint = vec2<f32>(textureDimensions(luma_image)) * fwidth(input.uv);
    if (max(source_footprint.x, source_footprint.y) > 1.0) {
        let y = textureSample(luma_image, image_sampler, input.uv).r;
        let cbcr = textureSample(chroma_image, image_sampler, input.uv).rg;
        return encoded_to_output(convert_yuv(vec3<f32>(y, cbcr)));
    }
    return encoded_to_output(convert_yuv(vec3<f32>(
        sample_luma_sharp(input.uv),
        sample_chroma_sharp(input.uv)
    )));
}

@fragment
fn fs_nearest(input: VertexOutput) -> @location(0) vec4<f32> {
    let y_dimensions_u = textureDimensions(luma_image);
    let uv_dimensions_u = textureDimensions(chroma_image);
    let y_location = clamp(
        vec2<i32>(floor(input.uv * vec2<f32>(y_dimensions_u))),
        vec2<i32>(0),
        vec2<i32>(y_dimensions_u) - vec2<i32>(1)
    );
    let uv_location = clamp(
        vec2<i32>(floor(input.uv * vec2<f32>(uv_dimensions_u))),
        vec2<i32>(0),
        vec2<i32>(uv_dimensions_u) - vec2<i32>(1)
    );
    let y = textureLoad(luma_image, y_location, 0).r;
    let cbcr = textureLoad(chroma_image, uv_location, 0).rg;
    return encoded_to_output(convert_yuv(vec3<f32>(y, cbcr)));
}
