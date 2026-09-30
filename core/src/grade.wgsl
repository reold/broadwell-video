@group(0) @binding(0) var y_tex: texture_2d<f32>;
@group(0) @binding(1) var uv_tex: texture_2d<f32>;
@group(0) @binding(2) var out_tex: texture_storage_2d<rgba8unorm, write>;

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = textureDimensions(y_tex);
    if (gid.x >= dims.x || gid.y >= dims.y) {
        return;
    }
    let coord = vec2<i32>(gid.xy);

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
    let graded = pow(
        clamp(saturated, vec3<f32>(0.0), vec3<f32>(1.0)),
        vec3<f32>(1.0 / 1.1),
    );

    textureStore(out_tex, coord, vec4<f32>(graded, 1.0));
}
