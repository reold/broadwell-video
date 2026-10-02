@group(0) @binding(0) var y_tex: texture_2d<f32>;
@group(0) @binding(1) var uv_tex: texture_2d<f32>;
@group(0) @binding(2) var out_y: texture_storage_2d<r32uint, write>;

struct GradeParams {
    exposure: f32,
    contrast: f32,
    saturation: f32,
    gamma: f32,
    scale: f32,
    offset_x: f32,
    offset_y: f32,
    rotation: f32,
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
    let d = out_pos - centre - offset;
    // The inverse rotation: where in the source did this output pixel come
    // from, given the picture was turned by `rotation` on its way out.
    let c = cos(params.rotation);
    let s = sin(params.rotation);
    let turned = vec2<f32>(d.x * c + d.y * s, -d.x * s + d.y * c);
    return turned / params.scale + centre;
}

fn graded(rgb_in: vec3<f32>) -> vec3<f32> {
    var c = rgb_in * exp2(params.exposure);
    c = (c - vec3<f32>(0.5)) * params.contrast + vec3<f32>(0.5);
    let luma = dot(c, vec3<f32>(0.2126, 0.7152, 0.0722));
    c = mix(vec3<f32>(luma), c, params.saturation);
    c = clamp(c, vec3<f32>(0.0), vec3<f32>(1.0));
    return pow(c, vec3<f32>(1.0 / params.gamma));
}

fn graded_y_at(coord: vec2<i32>) -> f32 {
    let td = textureDimensions(y_tex);
    let dims = vec2<f32>(f32(td.x), f32(td.y));
    let source = source_coord(vec2<f32>(f32(coord.x), f32(coord.y)) + 0.5, dims);
    if (source.x < 0.0 || source.y < 0.0 || source.x >= dims.x || source.y >= dims.y) {
        // Black, in the limited range the encoder expects.
        return 16.0 / 255.0;
    }
    let s = vec2<i32>(i32(source.x), i32(source.y));
    let y_raw = textureLoad(y_tex, s, 0).r;
    let uv_raw = textureLoad(uv_tex, vec2<i32>(s.x / 2, s.y / 2), 0);

    let y = (y_raw - 16.0 / 255.0) * (255.0 / 219.0);
    let u = (uv_raw.r - 128.0 / 255.0) * (255.0 / 224.0);
    let v = (uv_raw.g - 128.0 / 255.0) * (255.0 / 224.0);

    let r = y + 1.5748 * v;
    let g = y - 0.1873 * u - 0.4681 * v;
    let b = y + 1.8556 * u;

    let rgb = clamp(vec3<f32>(r, g, b), vec3<f32>(0.0), vec3<f32>(1.0));

    let gd = graded(rgb);

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
