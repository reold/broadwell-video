@group(0) @binding(0) var y_tex: texture_2d<f32>;
@group(0) @binding(1) var uv_tex: texture_2d<f32>;
@group(0) @binding(2) var out_uv: texture_storage_2d<r32uint, write>;

struct GradeParams {
    exposure: f32,
    contrast: f32,
    saturation: f32,
    gamma: f32,
    scale: f32,
    offset_x: f32,
    offset_y: f32,
    pad: f32,
};

@group(0) @binding(3) var<uniform> params: GradeParams;

// The grade, in one place, textually identical in all three grade shaders.
//
// The preview draws through grade.wgsl into RGBA and the export through
// grade_y.wgsl and grade_uv.wgsl into packed NV12. If the arithmetic here ever
// differs between them the export stops matching what the preview showed, which
// is the one class of bug this pipeline was built to avoid. Edit all three.
// Where in the source this output pixel comes from, once the clip's transform
// has been applied. Outside the source is black, which is what shows through
// when a clip is made smaller than the frame.
fn source_coord(out_pos: vec2<f32>, dims: vec2<f32>) -> vec2<f32> {
    let centre = dims * 0.5;
    let offset = vec2<f32>(params.offset_x, params.offset_y) * dims;
    return (out_pos - centre - offset) / params.scale + centre;
}

fn graded(rgb_in: vec3<f32>) -> vec3<f32> {
    var c = rgb_in * exp2(params.exposure);
    c = (c - vec3<f32>(0.5)) * params.contrast + vec3<f32>(0.5);
    let luma = dot(c, vec3<f32>(0.2126, 0.7152, 0.0722));
    c = mix(vec3<f32>(luma), c, params.saturation);
    c = clamp(c, vec3<f32>(0.0), vec3<f32>(1.0));
    return pow(c, vec3<f32>(1.0 / params.gamma));
}

fn avg_y_at(uv_coord: vec2<i32>) -> f32 {
    let x0 = uv_coord.x * 2;
    let y0 = uv_coord.y * 2;
    let y00 = textureLoad(y_tex, vec2<i32>(x0, y0), 0).r;
    let y10 = textureLoad(y_tex, vec2<i32>(x0 + 1, y0), 0).r;
    let y01 = textureLoad(y_tex, vec2<i32>(x0, y0 + 1), 0).r;
    let y11 = textureLoad(y_tex, vec2<i32>(x0 + 1, y0 + 1), 0).r;
    return (y00 + y10 + y01 + y11) * 0.25;
}

fn graded_cbcr_at_uv(uv_coord: vec2<i32>) -> vec2<f32> {
    // The transform is applied in luma space so both planes agree about where
    // the picture is, then halved back into chroma space.
    let td = textureDimensions(y_tex);
    let dims = vec2<f32>(f32(td.x), f32(td.y));
    let source = source_coord(vec2<f32>(f32(uv_coord.x * 2), f32(uv_coord.y * 2)) + 0.5, dims);
    if (source.x < 0.0 || source.y < 0.0 || source.x >= dims.x || source.y >= dims.y) {
        return vec2<f32>(128.0 / 255.0, 128.0 / 255.0);
    }
    let uv_at = vec2<i32>(i32(source.x) / 2, i32(source.y) / 2);
    let y_avg = avg_y_at(uv_at);
    let uv_raw = textureLoad(uv_tex, uv_at, 0);

    let y = (y_avg - 16.0 / 255.0) * (255.0 / 219.0);
    let u = (uv_raw.r - 128.0 / 255.0) * (255.0 / 224.0);
    let v = (uv_raw.g - 128.0 / 255.0) * (255.0 / 224.0);

    let r = y + 1.5748 * v;
    let g = y - 0.1873 * u - 0.4681 * v;
    let b = y + 1.8556 * u;

    let rgb = clamp(vec3<f32>(r, g, b), vec3<f32>(0.0), vec3<f32>(1.0));
    let gd = graded(rgb);

    let cb = -0.1146 * gd.r - 0.3854 * gd.g + 0.5000 * gd.b;
    let cr = 0.5000 * gd.r - 0.4542 * gd.g - 0.0458 * gd.b;
    return vec2<f32>(
        clamp((128.0 + 224.0 * cb) / 255.0, 16.0 / 255.0, 240.0 / 255.0),
        clamp((128.0 + 224.0 * cr) / 255.0, 16.0 / 255.0, 240.0 / 255.0),
    );
}

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let y_dims = textureDimensions(y_tex);
    let uv_w = (y_dims.x + 1u) / 2u;
    let uv_h = (y_dims.y + 1u) / 2u;
    let out_w = uv_w / 2u;
    if (gid.x >= out_w || gid.y >= uv_h) {
        return;
    }

    let ux0 = i32(gid.x) * 2;
    let ux1 = ux0 + 1;
    let row = i32(gid.y);

    let c0 = graded_cbcr_at_uv(vec2<i32>(ux0, row));
    let c1 = graded_cbcr_at_uv(vec2<i32>(ux1, row));

    let cb0 = u32(clamp(c0.x, 0.0, 1.0) * 255.0 + 0.5);
    let cr0 = u32(clamp(c0.y, 0.0, 1.0) * 255.0 + 0.5);
    let cb1 = u32(clamp(c1.x, 0.0, 1.0) * 255.0 + 0.5);
    let cr1 = u32(clamp(c1.y, 0.0, 1.0) * 255.0 + 0.5);

    let packed = cb0 | (cr0 << 8u) | (cb1 << 16u) | (cr1 << 24u);
    textureStore(out_uv, vec2<i32>(i32(gid.x), row), vec4<u32>(packed, 0u, 0u, 0u));
}
