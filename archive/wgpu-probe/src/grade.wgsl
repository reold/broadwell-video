@group(0) @binding(0) var input_tex:  texture_2d<f32>;
@group(0) @binding(1) var output_tex: texture_storage_2d<rgba8unorm, write>;

struct Params {
    resolution: vec2<u32>,
    gamma:      f32,
    saturation: f32,
}
@group(0) @binding(2) var<uniform> params: Params;

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= params.resolution.x || gid.y >= params.resolution.y) {
        return;
    }
    let coord = vec2<i32>(gid.xy);
    let c = textureLoad(input_tex, coord, 0);

    let corrected = pow(c.rgb, vec3<f32>(1.0 / params.gamma));
    let luma      = dot(corrected, vec3<f32>(0.2126, 0.7152, 0.0722));
    let saturated = mix(vec3<f32>(luma), corrected, params.saturation);

    textureStore(output_tex, coord, vec4<f32>(saturated, c.a));
}
