//! CPU-side WGSL validation for every shader in `core/src`.
//!
//! This needs no GPU, so it runs in environments with no `/dev/dri`. The
//! on-GPU geometry test for the letterbox lives in `tests/letterbox.rs` and
//! is skipped when no Vulkan adapter is available.
//!
//! `naga` is the same front end `wgpu` uses, pinned to the same major version,
//! so a shader that passes here compiles on the GPU.

fn validate(name: &str, source: &str) -> Result<(), String> {
    let module = naga::front::wgsl::parse_str(source)
        .map_err(|e| format!("{name}: WGSL parse error:\n{}", e.emit_to_string(source)))?;
    naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(&module)
    .map(|_| ())
    .map_err(|e| format!("{name}: WGSL validation error: {e:?}"))
}

#[test]
fn all_shaders_parse_and_validate() {
    for (name, source) in [
        ("blit.wgsl", include_str!("../src/blit.wgsl")),
        ("grade.wgsl", include_str!("../src/grade.wgsl")),
        ("grade_y.wgsl", include_str!("../src/grade_y.wgsl")),
        ("grade_uv.wgsl", include_str!("../src/grade_uv.wgsl")),
        ("noop.wgsl", include_str!("../src/noop.wgsl")),
    ] {
        if let Err(e) = validate(name, source) {
            panic!("{e}");
        }
    }
}

/// Note: naga's validator does *not* enforce WGSL's uniform-control-flow
/// rules for `textureSample` (it accepts a sample inside a UV-dependent
/// branch), so a passing validation here is a syntax/type check and not a
/// uniformity proof. `blit.wgsl` uses `textureSampleLevel` with an explicit
/// LOD precisely so that no derivative-based sampling is involved at all.
#[test]
fn texture_sampling_uses_explicit_lod() {
    let source = include_str!("../src/blit.wgsl");
    assert!(
        source.contains("textureSampleLevel("),
        "blit.wgsl should sample with an explicit LOD"
    );
    assert!(
        !source.contains("textureSample("),
        "blit.wgsl should not rely on implicit derivatives"
    );
}
