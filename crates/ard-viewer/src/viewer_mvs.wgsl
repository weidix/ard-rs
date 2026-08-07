@group(0) @binding(0) var<storage, read> records: array<u32>;
@group(0) @binding(1) var<storage, read> payload: array<i32>;
@group(0) @binding(2) var<storage, read> quantization: array<u32>;
@group(0) @binding(3) var output_image: texture_storage_2d<rgba8unorm, write>;
// Inverse DCT workspace for the two-pass transform: three components of
// eight rows of eight column-pass results.
var<workgroup> idct_workspace: array<i32, 192>;

// One 1-D stage of Screen Sharing's jpeg_idct_islow. Every operation wraps
// at 32 bits exactly as it does on the native ARM64 decoder.
fn idct_pass(input: array<i32, 8>) -> array<i32, 8> {
    var z2 = input[2];
    var z3 = input[6];
    var z1 = (z2 + z3) * 4433;
    var tmp2 = z1 + z3 * (-15137);
    var tmp3 = z1 + z2 * 6270;

    z2 = input[0];
    z3 = input[4];
    var tmp0 = (z2 + z3) << 13u;
    var tmp1 = (z2 - z3) << 13u;
    var tmp10 = tmp0 + tmp3;
    var tmp13 = tmp0 - tmp3;
    var tmp11 = tmp1 + tmp2;
    var tmp12 = tmp1 - tmp2;

    var odd0 = input[7];
    var odd1 = input[5];
    var odd2 = input[3];
    var odd3 = input[1];
    z1 = odd0 + odd3;
    z2 = odd1 + odd2;
    z3 = odd0 + odd2;
    var z4 = odd1 + odd3;
    var z5 = (z3 + z4) * 9633;
    odd0 = odd0 * 2446;
    odd1 = odd1 * 16819;
    odd2 = odd2 * 25172;
    odd3 = odd3 * 12299;
    z1 = z1 * (-7373);
    z2 = z2 * (-20995);
    z3 = z3 * (-16069);
    z4 = z4 * (-3196);
    z3 = z3 + z5;
    z4 = z4 + z5;
    odd0 = odd0 + z1 + z3;
    odd1 = odd1 + z2 + z4;
    odd2 = odd2 + z2 + z3;
    odd3 = odd3 + z1 + z4;

    var output: array<i32, 8>;
    output[0] = tmp10 + odd3;
    output[1] = tmp11 + odd2;
    output[2] = tmp12 + odd1;
    output[3] = tmp13 + odd0;
    output[4] = tmp13 - odd0;
    output[5] = tmp12 - odd1;
    output[6] = tmp11 - odd2;
    output[7] = tmp10 - odd3;
    return output;
}

fn descale(value: i32, bits: u32) -> i32 {
    return (value + (1i << (bits - 1u))) >> bits;
}

// Column pass of jpeg_idct_islow for one component. Returns the eight
// workspace rows for this thread's column.
fn idct_column_pass(component: u32, column: u32, data_offset: u32) -> array<i32, 8> {
    var column_values: array<i32, 8>;
    for (var u = 0u; u < 8u; u++) {
        let coefficient_index = u * 8u + column;
        let quantization_offset = select(64u, 0u, component == 0u);
        let quant = i32(quantization[quantization_offset + coefficient_index]);
        column_values[u] = payload[data_offset + component * 64u + coefficient_index] * quant;
    }
    if (column_values[1] == 0 && column_values[2] == 0 && column_values[3] == 0
        && column_values[4] == 0 && column_values[5] == 0 && column_values[6] == 0
        && column_values[7] == 0) {
        var output: array<i32, 8>;
        let dc = column_values[0] << 2u;
        for (var row = 0u; row < 8u; row++) {
            output[row] = dc;
        }
        return output;
    }
    let transformed = idct_pass(column_values);
    var output: array<i32, 8>;
    for (var row = 0u; row < 8u; row++) {
        output[row] = descale(transformed[row], 11u);
    }
    return output;
}

// Row pass for one component; returns the sample for this thread's pixel
// (x = pixel_x, y = row).
fn idct_row_sample(component: u32, pixel_x: u32, row: u32) -> i32 {
    var row_values: array<i32, 8>;
    for (var v = 0u; v < 8u; v++) {
        row_values[v] = idct_workspace[component * 64u + row * 8u + v];
    }
    if (row_values[1] == 0 && row_values[2] == 0 && row_values[3] == 0
        && row_values[4] == 0 && row_values[5] == 0 && row_values[6] == 0
        && row_values[7] == 0) {
        return descale(row_values[0], 5u) + 128;
    }
    let transformed = idct_pass(row_values);
    return descale(transformed[pixel_x], 18u) + 128;
}

fn rice_chroma_sample(component: u32, data_offset: u32) -> i32 {
    let coefficient = payload[data_offset + component * 64u];
    let quant = i32(quantization[64u]);
    return clamp(((coefficient * quant + 4) >> 3) + 128, 0, 255);
}

fn unpack_ycbcr(packed: u32) -> vec3<i32> {
    return vec3<i32>(
        i32(packed & 0xffu),
        i32((packed >> 8u) & 0xffu),
        i32((packed >> 16u) & 0xffu)
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

fn ycbcr_to_rgb(sample: vec3<i32>) -> vec4<f32> {
    // Screen Sharing's integer lookup tables: red/blue use symmetrically
    // rounded 16.16 coefficients, green combines its terms with the 32768
    // half-unit bias.
    let y = sample.x;
    let cb = sample.y - 128;
    let cr = sample.z - 128;
    let red = clamp(y + ((91881 * cr + 32768) >> 16), 0, 255);
    let green = clamp(y + ((32768 - 22554 * cb - 46802 * cr) >> 16), 0, 255);
    let blue = clamp(y + ((116130 * cb + 32768) >> 16), 0, 255);
    return vec4<f32>(f32(red), f32(green), f32(blue), 255.0) / 255.0;
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

    let column_result = idct_column_pass(0u, local.x, data_offset);
    idct_workspace[pixel_index] = column_result[local.y];
    if (kind == 4u) {
        let cb_result = idct_column_pass(1u, local.x, data_offset);
        let cr_result = idct_column_pass(2u, local.x, data_offset);
        idct_workspace[64u + pixel_index] = cb_result[local.y];
        idct_workspace[128u + pixel_index] = cr_result[local.y];
    }
    workgroupBarrier();
    if (!invocation_is_active) {
        return;
    }
    let destination = vec2<u32>(records[record] + local.x, records[record + 1u] + local.y);
    var sample: vec3<i32>;
    if (kind == 4u) {
        sample = vec3<i32>(
            idct_row_sample(0u, local.x, local.y),
            idct_row_sample(1u, local.x, local.y),
            idct_row_sample(2u, local.x, local.y)
        );
    } else {
        sample = vec3<i32>(
            idct_row_sample(0u, local.x, local.y),
            rice_chroma_sample(1u, data_offset),
            rice_chroma_sample(2u, data_offset)
        );
    }
    let color = ycbcr_to_rgb(sample);
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
    return encoded_to_output(textureSample(decoded_image, image_sampler, input.uv));
}

@fragment
fn fs_sharp(input: VertexOutput) -> @location(0) vec4<f32> {
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
