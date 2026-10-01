// Fullscreen blit with aspect-preserving letterbox / pillarbox.

@group(0) @binding(0) var samp: sampler;
@group(0) @binding(1) var tex: texture_2d<f32>;

// x = video display aspect (width / height)
// y = target display aspect (width / height)
// z, w reserved for future pan/zoom.
struct BlitParams {
    aspects: vec4<f32>,
};

@group(0) @binding(2) var<uniform> params: BlitParams;

struct VOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) i: u32) -> VOut {
    var out: VOut;
    var p = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 3.0, -1.0),
        vec2<f32>(-1.0,  3.0),
    );
    out.pos = vec4<f32>(p[i], 0.0, 1.0);
    out.uv = vec2<f32>((p[i].x + 1.0) * 0.5, (1.0 - p[i].y) * 0.5);
    return out;
}

@fragment
fn fs(in: VOut) -> @location(0) vec4<f32> {
    let video_aspect = max(params.aspects.x, 0.000001);
    let target_aspect = max(params.aspects.y, 0.000001);

    // Fraction of the target covered by the video, centred on both axes.
    var coverage = vec2<f32>(1.0, 1.0);
    if (video_aspect > target_aspect) {
        // Video is wider than the target: bars top and bottom.
        coverage.y = target_aspect / video_aspect;
    } else {
        // Video is narrower than the target: bars left and right.
        coverage.x = video_aspect / target_aspect;
    }

    let min_uv = (vec2<f32>(1.0, 1.0) - coverage) * 0.5;
    let max_uv = min_uv + coverage;

    // Outside the video. The render pass clears to black, but the fullscreen
    // triangle covers the whole target, so the bars are written explicitly.
    // This also stops CLAMP_TO_EDGE from smearing edge pixels outwards.
    if (any(in.uv < min_uv) || any(in.uv > max_uv)) {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }

    let uv = (in.uv - min_uv) / coverage;
    // Explicit LOD 0: the graded source has a single mip level, so this is
    // exact, and it uses no implicit derivatives at all.
    return textureSampleLevel(tex, samp, uv, 0.0);
}
