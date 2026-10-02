@group(0) @binding(0) var y_tex: texture_2d<f32>;
@group(0) @binding(1) var uv_tex: texture_2d<f32>;
@group(0) @binding(2) var out_tex: texture_storage_2d<rgba8unorm, write>;

struct GradeParams {
    exposure: f32,
    contrast: f32,
    saturation: f32,
    gamma: f32,
};

@group(0) @binding(3) var<uniform> params: GradeParams;

// The grade, in one place, textually identical in all three grade shaders.
//
// The preview draws through grade.wgsl into RGBA and the export through
// grade_y.wgsl and grade_uv.wgsl into packed NV12. If the arithmetic here ever
// differs between them the export stops matching what the preview showed, which
// is the one class of bug this pipeline was built to avoid. Edit all three.
fn graded(rgb_in: vec3<f32>) -> vec3<f32> {
    var c = rgb_in * exp2(params.exposure);
    c = (c - vec3<f32>(0.5)) * params.contrast + vec3<f32>(0.5);
    let luma = dot(c, vec3<f32>(0.2126, 0.7152, 0.0722));
    c = mix(vec3<f32>(luma), c, params.saturation);
    c = clamp(c, vec3<f32>(0.0), vec3<f32>(1.0));
    return pow(c, vec3<f32>(1.0 / params.gamma));
}

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

    let out_rgb = graded(rgb);

    textureStore(out_tex, coord, vec4<f32>(out_rgb, 1.0));
}
