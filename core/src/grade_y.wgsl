@group(0) @binding(0) var y_tex: texture_2d<f32>;
@group(0) @binding(1) var uv_tex: texture_2d<f32>;
@group(0) @binding(2) var out_y: texture_storage_2d<r32uint, write>;

fn graded_y_at(coord: vec2<i32>) -> f32 {
    let y_raw = textureLoad(y_tex, coord, 0).r;
    let uv_raw = textureLoad(uv_tex, vec2<i32>(coord.x / 2, coord.y / 2), 0);

    let y = (y_raw - 16.0 / 255.0) * (255.0 / 219.0);
    let u = (uv_raw.r - 128.0 / 255.0) * (255.0 / 224.0);
    let v = (uv_raw.g - 128.0 / 255.0) * (255.0 / 224.0);

    let r = y + 1.5748 * v;
    let g = y - 0.1873 * u - 0.4681 * v;
    let b = y + 1.8556 * u;

    let rgb = clamp(vec3<f32>(r, g, b), vec3<f32>(0.0), vec3<f32>(1.0));

    let luma = dot(rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
    let saturated = mix(vec3<f32>(luma), rgb, 1.4);
    let gd = pow(
        clamp(saturated, vec3<f32>(0.0), vec3<f32>(1.0)),
        vec3<f32>(1.0 / 1.1),
    );

    let y_full = dot(gd, vec3<f32>(0.2126, 0.7152, 0.0722));
    return (16.0 + 219.0 * y_full) / 255.0;
}

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let y_dims = textureDimensions(y_tex);
    let out_w = y_dims.x / 4u;
    if (gid.x >= out_w || gid.y >= y_dims.y) {
        return;
    }

    let x_base = i32(gid.x) * 4;
    let row = i32(gid.y);

    let y0 = u32(clamp(graded_y_at(vec2<i32>(x_base + 0, row)), 0.0, 1.0) * 255.0 + 0.5);
    let y1 = u32(clamp(graded_y_at(vec2<i32>(x_base + 1, row)), 0.0, 1.0) * 255.0 + 0.5);
    let y2 = u32(clamp(graded_y_at(vec2<i32>(x_base + 2, row)), 0.0, 1.0) * 255.0 + 0.5);
    let y3 = u32(clamp(graded_y_at(vec2<i32>(x_base + 3, row)), 0.0, 1.0) * 255.0 + 0.5);

    let packed = y0 | (y1 << 8u) | (y2 << 16u) | (y3 << 24u);
    textureStore(out_y, vec2<i32>(i32(gid.x), row), vec4<u32>(packed, 0u, 0u, 0u));
}
