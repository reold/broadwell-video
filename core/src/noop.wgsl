// Diagnostic no-op effect pass: reads a texture, writes it out unchanged.
// Each pass is a full-frame read + write, which is what a real effect costs.

@group(0) @binding(0) var input_tex: texture_2d<f32>;
@group(0) @binding(1) var output_tex: texture_storage_2d<rgba8unorm, write>;

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = textureDimensions(input_tex);
    if (gid.x >= dims.x || gid.y >= dims.y) {
        return;
    }
    let coord = vec2<i32>(gid.xy);
    let c = textureLoad(input_tex, coord, 0);
    textureStore(output_tex, coord, c);
}
