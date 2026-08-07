@group(0) @binding(0) var<storage, read> records: array<u32>;
@group(0) @binding(1) var<storage, read> payload: array<i32>;
@group(0) @binding(2) var<storage, read> quantization: array<u32>;
@group(0) @binding(3) var output_image: texture_storage_2d<rgba8unorm, write>;
var<workgroup> idct_horizontal: array<f32, 192>;

const BASIS: array<f32, 64> = array<f32, 64>(
    11585.0, 11585.0, 11585.0, 11585.0, 11585.0, 11585.0, 11585.0, 11585.0,
    16069.0, 13623.0, 9102.0, 3196.0, -3196.0, -9102.0, -13623.0, -16069.0,
    15137.0, 6270.0, -6270.0, -15137.0, -15137.0, -6270.0, 6270.0, 15137.0,
    13623.0, -3196.0, -16069.0, -9102.0, 9102.0, 16069.0, 3196.0, -13623.0,
    11585.0, -11585.0, -11585.0, 11585.0, 11585.0, -11585.0, -11585.0, 11585.0,
    9102.0, -16069.0, 3196.0, 13623.0, -13623.0, -3196.0, 16069.0, -9102.0,
    6270.0, -15137.0, 15137.0, -6270.0, -6270.0, 15137.0, -15137.0, 6270.0,
    3196.0, -9102.0, 13623.0, -16069.0, 16069.0, -13623.0, 9102.0, -3196.0
);

fn unpack_ycbcr(packed: u32) -> vec3<f32> {
    return vec3<f32>(
        f32(packed & 0xffu),
        f32((packed >> 8u) & 0xffu),
        f32((packed >> 16u) & 0xffu)
    );
}

fn unpack_rgba(packed: u32) -> vec4<f32> {
    return vec4<f32>(
        f32(packed & 0xffu),
        f32((packed >> 8u) & 0xffu),
        f32((packed >> 16u) & 0xffu),
        f32((packed >> 24u) & 0xffu)
    ) / 255.0;
}

fn horizontal_idct_sample(component: u32, pixel_x: u32, row: u32, data_offset: u32) -> f32 {
    var sum = 0.0;
    for (var u = 0u; u < 8u; u++) {
        let coefficient_index = row * 8u + u;
        let coefficient = f32(payload[data_offset + component * 64u + coefficient_index]);
        let quantization_offset = select(64u, 0u, component == 0u);
        let quant = f32(quantization[quantization_offset + coefficient_index]);
        sum += coefficient * quant * (BASIS[u * 8u + pixel_x] / 16384.0);
    }
    return sum;
}

fn idct_sample(component: u32, pixel_x: u32, pixel_y: u32) -> f32 {
    var sum = 0.0;
    for (var v = 0u; v < 8u; v++) {
        sum += idct_horizontal[component * 64u + v * 8u + pixel_x]
            * (BASIS[v * 8u + pixel_y] / 16384.0);
    }
    return clamp(floor(sum * 0.25 + 0.5) + 128.0, 0.0, 255.0);
}

fn rice_chroma_sample(component: u32, data_offset: u32) -> f32 {
    let coefficient = payload[data_offset + component * 64u];
    let quant = i32(quantization[64u]);
    return clamp(f32((coefficient * quant + 4) >> 3) + 128.0, 0.0, 255.0);
}

fn ycbcr_to_rgb(sample: vec3<f32>) -> vec4<f32> {
    let y = sample.x / 255.0;
    let cb = (sample.y - 128.0) / 255.0;
    let cr = (sample.z - 128.0) / 255.0;
    let rgb = vec3<f32>(
        y + 1.402 * cr,
        y - 0.344136 * cb - 0.714136 * cr,
        y + 1.772 * cb
    );
    return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}

@compute @workgroup_size(8, 8, 1)
fn decode_tiles(
    @builtin(workgroup_id) group: vec3<u32>,
    @builtin(num_workgroups) workgroups: vec3<u32>,
    @builtin(local_invocation_id) local: vec3<u32>,
) {
    let tile = group.y * workgroups.x + group.x;
    if (tile >= records[0]) {
        return;
    }
    let record = 1u + tile * 8u;
    let width = records[record + 2u];
    let height = records[record + 3u];
    let invocation_is_active = local.x < width && local.y < height;
    let kind = records[record + 4u];
    let data_offset = records[record + 5u];
    let packed_color = records[record + 6u];
    let pixel_index = local.y * 8u + local.x;

    // Solid and literal-pixel tiles do not need the shared IDCT workspace or
    // a workgroup barrier. They are common in ARD's adaptive stream, so keep
    // their fast path entirely in registers.
    if (kind < 4u) {
        if (!invocation_is_active) {
            return;
        }
        var color: vec4<f32>;
        switch kind {
            case 0u: {
                color = ycbcr_to_rgb(unpack_ycbcr(packed_color));
            }
            case 1u: {
                color = unpack_rgba(packed_color);
            }
            case 2u: {
                color = ycbcr_to_rgb(unpack_ycbcr(u32(payload[data_offset + pixel_index])));
            }
            default: {
                color = unpack_rgba(u32(payload[data_offset + pixel_index]));
            }
        }
        textureStore(output_image, vec2<u32>(records[record] + local.x, records[record + 1u] + local.y), color);
        return;
    }

    idct_horizontal[pixel_index] = horizontal_idct_sample(0u, local.x, local.y, data_offset);
    if (kind == 4u) {
        idct_horizontal[64u + pixel_index] = horizontal_idct_sample(1u, local.x, local.y, data_offset);
        idct_horizontal[128u + pixel_index] = horizontal_idct_sample(2u, local.x, local.y, data_offset);
    }
    workgroupBarrier();
    if (!invocation_is_active) {
        return;
    }
    let destination = vec2<u32>(records[record] + local.x, records[record + 1u] + local.y);
    var color: vec4<f32>;
    switch kind {
        case 0u: {
            color = ycbcr_to_rgb(unpack_ycbcr(packed_color));
        }
        case 1u: {
            color = unpack_rgba(packed_color);
        }
        case 2u: {
            color = ycbcr_to_rgb(unpack_ycbcr(u32(payload[data_offset + pixel_index])));
        }
        case 3u: {
            color = unpack_rgba(u32(payload[data_offset + pixel_index]));
        }
        case 4u: {
            color = ycbcr_to_rgb(vec3<f32>(
                idct_sample(0u, local.x, local.y),
                idct_sample(1u, local.x, local.y),
                idct_sample(2u, local.x, local.y)
            ));
        }
        default: {
            color = ycbcr_to_rgb(vec3<f32>(
                idct_sample(0u, local.x, local.y),
                rice_chroma_sample(1u, data_offset),
                rice_chroma_sample(2u, data_offset)
            ));
        }
    }
    textureStore(output_image, destination, color);
}

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
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
@group(1) @binding(1) var decoded_image: texture_2d<f32>;

fn srgb_to_linear_component(value: f32) -> f32 {
    if (value <= 0.04045) {
        return value / 12.92;
    }
    return pow((value + 0.055) / 1.055, 2.4);
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

fn sample_sharp(uv: vec2<f32>) -> vec4<f32> {
    let dimensions_u = textureDimensions(decoded_image);
    let dimensions = vec2<f32>(dimensions_u);
    let source = uv * dimensions - vec2<f32>(0.5);
    let base = vec2<i32>(floor(source));
    let fraction = fract(source);
    let maximum = vec2<i32>(dimensions_u) - vec2<i32>(1);
    var color = vec4<f32>(0.0);
    var weight_sum = 0.0;
    for (var row = -1; row <= 2; row++) {
        let weight_y = cubic_weight(f32(row) - fraction.y);
        for (var column = -1; column <= 2; column++) {
            let weight = cubic_weight(f32(column) - fraction.x) * weight_y;
            let location = clamp(base + vec2<i32>(column, row), vec2<i32>(0), maximum);
            color += textureLoad(decoded_image, location, 0) * weight;
            weight_sum += weight;
        }
    }
    return clamp(color / weight_sum, vec4<f32>(0.0), vec4<f32>(1.0));
}

fn encoded_to_output(encoded: vec4<f32>) -> vec4<f32> {
    return vec4<f32>(
        srgb_to_linear_component(encoded.r),
        srgb_to_linear_component(encoded.g),
        srgb_to_linear_component(encoded.b),
        encoded.a
    );
}

@fragment
fn fs_interpolated(input: VertexOutput) -> @location(0) vec4<f32> {
    let source_footprint = vec2<f32>(textureDimensions(decoded_image)) * fwidth(input.uv);
    var encoded: vec4<f32>;
    if (max(source_footprint.x, source_footprint.y) > 1.0) {
        encoded = textureSample(decoded_image, image_sampler, input.uv);
    } else {
        encoded = sample_sharp(input.uv);
    }
    return encoded_to_output(encoded);
}

@fragment
fn fs_nearest(input: VertexOutput) -> @location(0) vec4<f32> {
    let dimensions_u = textureDimensions(decoded_image);
    let dimensions = vec2<f32>(dimensions_u);
    let maximum = vec2<i32>(dimensions_u) - vec2<i32>(1);
    let location = clamp(vec2<i32>(floor(input.uv * dimensions)), vec2<i32>(0), maximum);
    return encoded_to_output(textureLoad(decoded_image, location, 0));
}
