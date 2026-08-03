@group(0) @binding(0) var<storage, read> records: array<u32>;
@group(0) @binding(1) var<storage, read> payload: array<i32>;
@group(0) @binding(2) var<storage, read> quantization: array<u32>;
@group(0) @binding(3) var output_image: texture_storage_2d<rgba8unorm, write>;

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

fn idct_sample(component: u32, pixel_x: u32, pixel_y: u32, data_offset: u32) -> f32 {
    var sum = 0.0;
    for (var v = 0u; v < 8u; v++) {
        for (var u = 0u; u < 8u; u++) {
            let coefficient_index = v * 8u + u;
            let coefficient = f32(payload[data_offset + component * 64u + coefficient_index]);
            let quantization_offset = select(64u, 0u, component == 0u);
            let quant = f32(quantization[quantization_offset + coefficient_index]);
            sum += coefficient * quant * BASIS[u * 8u + pixel_x] * BASIS[v * 8u + pixel_y];
        }
    }
    return clamp(floor((sum + 536870912.0) / 1073741824.0) + 128.0, 0.0, 255.0);
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
    @builtin(local_invocation_id) local: vec3<u32>,
) {
    let record = group.x * 8u;
    let width = records[record + 2u];
    let height = records[record + 3u];
    if (local.x >= width || local.y >= height) {
        return;
    }
    let destination = vec2<u32>(records[record] + local.x, records[record + 1u] + local.y);
    let kind = records[record + 4u];
    let data_offset = records[record + 5u];
    let packed_color = records[record + 6u];
    let pixel_index = local.y * 8u + local.x;
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
                idct_sample(0u, local.x, local.y, data_offset),
                idct_sample(1u, local.x, local.y, data_offset),
                idct_sample(2u, local.x, local.y, data_offset)
            ));
        }
        default: {
            color = ycbcr_to_rgb(vec3<f32>(
                idct_sample(0u, local.x, local.y, data_offset),
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

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    let encoded = textureSample(decoded_image, image_sampler, input.uv);
    return vec4<f32>(
        srgb_to_linear_component(encoded.r),
        srgb_to_linear_component(encoded.g),
        srgb_to_linear_component(encoded.b),
        encoded.a
    );
}
