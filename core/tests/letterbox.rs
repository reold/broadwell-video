//! Headless GPU test for the letterbox blit.
//!
//! `blit.wgsl` must fit the whole video inside the render target without
//! stretching it, and fill the remainder with black. This renders a flat grey
//! source through the real `Pipelines` + `blit_bind_group` path into an
//! offscreen target and checks the bar geometry, which is the only way to
//! catch an inverted or mis-signed aspect calculation without eyeballing a
//! window.
//!
//! If no Vulkan adapter is available these tests **fail**, unless
//! `HWA_ALLOW_NO_GPU=1` is set, so a silent no-op can never be mistaken for a
//! real pass.

use hwa_core::gpu;
use std::sync::mpsc;

/// Non-sRGB, so a written 128 reads back as exactly 128.
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

const GREY: u8 = 128;
/// Linear filtering can move an edge pixel by a hair; bars must be exact black.
const GREY_TOLERANCE: i32 = 2;

struct Fixture {
    host: grafting::HostWgpuContext,
    pipelines: gpu::Pipelines,
}

fn fixture() -> Option<Fixture> {
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
    desc.backends = wgpu::Backends::VULKAN;
    let instance = wgpu::Instance::new(desc);

    let adapter = match pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::LowPower,
        compatible_surface: None,
        force_fallback_adapter: false,
        apply_limit_buckets: false,
    })) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("request_adapter failed: {e:?}");
            return None;
        }
    };

    let device_desc = wgpu::DeviceDescriptor {
        label: Some("letterbox-test"),
        required_features: wgpu::Features::empty(),
        required_limits: wgpu::Limits::default(),
        memory_hints: wgpu::MemoryHints::default(),
        experimental_features: wgpu::ExperimentalFeatures::disabled(),
        trace: wgpu::Trace::Off,
    };
    let host = match grafting::vulkan_dmabuf::create_dmabuf_host_context(&adapter, &device_desc) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("create_dmabuf_host_context failed: {e:?}");
            return None;
        }
    };

    // The graded/export textures built here are unused by the blit; only the
    // blit pipeline, its layout and the letterbox uniform matter.
    let pipelines = gpu::build_pipelines(&host, FORMAT, 64, 36);
    Some(Fixture { host, pipelines })
}

/// No adapter on this machine. Skipping is allowed only when explicitly
/// requested, because these tests would otherwise pass without exercising
/// anything and look identical to a real pass.
macro_rules! fixture_or_skip {
    () => {
        match fixture() {
            Some(fx) => fx,
            None => {
                assert!(
                    std::env::var_os("HWA_ALLOW_NO_GPU").is_some(),
                    "no Vulkan adapter available, so this test would pass \
                     without exercising anything. Run with HWA_ALLOW_NO_GPU=1 \
                     to acknowledge a GPU-less environment."
                );
                eprintln!("no Vulkan adapter; skipping letterbox test (HWA_ALLOW_NO_GPU set)");
                return;
            }
        }
    };
}

/// Blit a flat grey `video`-sized source into a `target`-sized offscreen
/// texture and return tightly packed RGBA rows.
///
/// `video_aspect` is what the letterbox is driven by; `video` only sizes the
/// source texture, so it can differ from the aspect (an anamorphic frame, for
/// example, whose stored shape is not its display shape).
fn render(fx: &Fixture, video: (u32, u32), video_aspect: f32, target: (u32, u32)) -> Vec<u8> {
    let device = &fx.host.device;
    let queue = &fx.host.queue;

    let src = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("letterbox-src"),
        size: wgpu::Extent3d {
            width: video.0,
            height: video.1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let pixels = vec![GREY; (video.0 * video.1 * 4) as usize];
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &src,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &pixels,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(video.0 * 4),
            rows_per_image: Some(video.1),
        },
        wgpu::Extent3d {
            width: video.0,
            height: video.1,
            depth_or_array_layers: 1,
        },
    );
    let src_view = src.create_view(&wgpu::TextureViewDescriptor::default());

    let target_tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("letterbox-target"),
        size: wgpu::Extent3d {
            width: target.0,
            height: target.1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let target_view = target_tex.create_view(&wgpu::TextureViewDescriptor::default());

    let row_bytes = target.0 * 4;
    let bytes_per_row = row_bytes.div_ceil(256) * 256;
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("letterbox-readback"),
        size: (bytes_per_row * target.1) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    fx.pipelines
        .set_letterbox(queue, video_aspect, target.0, target.1);
    let bind_group = fx.pipelines.blit_bind_group(device, &src_view);

    let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("letterbox-encoder"),
    });
    {
        let mut rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("letterbox-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &target_view,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        rp.set_pipeline(&fx.pipelines.blit_pipeline);
        rp.set_bind_group(0, &bind_group, &[]);
        rp.draw(0..3, 0..1);
    }
    enc.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &target_tex,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: Some(target.1),
            },
        },
        wgpu::Extent3d {
            width: target.0,
            height: target.1,
            depth_or_array_layers: 1,
        },
    );
    queue.submit([enc.finish()]);
    device
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: None,
        })
        .ok();

    let slice = readback.slice(..);
    let (tx, rx) = mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    device
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: None,
        })
        .ok();
    rx.recv().expect("map callback dropped").expect("map failed");

    let mut out = vec![0u8; row_bytes as usize * target.1 as usize];
    {
        let data = slice.get_mapped_range().expect("mapped range");
        for row in 0..target.1 as usize {
            let start = row * bytes_per_row as usize;
            out[row * row_bytes as usize..(row + 1) * row_bytes as usize]
                .copy_from_slice(&data[start..start + row_bytes as usize]);
        }
    }
    readback.unmap();
    out
}

fn pixel(buf: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * width + x) * 4) as usize;
    [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
}

fn assert_black(buf: &[u8], width: u32, x: u32, y: u32, what: &str) {
    let p = pixel(buf, width, x, y);
    assert_eq!(
        [p[0], p[1], p[2]],
        [0, 0, 0],
        "{what} at ({x}, {y}) should be an exact black bar, got {p:?}"
    );
}

fn assert_grey(buf: &[u8], width: u32, x: u32, y: u32, what: &str) {
    let p = pixel(buf, width, x, y);
    let diff = (p[0] as i32 - GREY as i32).abs();
    assert!(
        diff <= GREY_TOLERANCE && p[0] == p[1] && p[1] == p[2],
        "{what} at ({x}, {y}) should be video grey ({GREY}), got {p:?}"
    );
}

/// 16:9 into 1:1 must letterbox: black bars top and bottom, no side bars.
#[test]
fn letterboxes_a_wide_video_into_a_square_target() {
    let fx = fixture_or_skip!();
    let frame = render(&fx, (64, 36), 16.0 / 9.0, (64, 64));

    assert_black(&frame, 64, 32, 0, "top bar");
    assert_black(&frame, 64, 32, 13, "top bar (last row)");
    assert_grey(&frame, 64, 32, 14, "video (first row)");
    assert_grey(&frame, 64, 32, 32, "video centre");
    assert_grey(&frame, 64, 32, 49, "video (last row)");
    assert_black(&frame, 64, 32, 50, "bottom bar (first row)");
    assert_black(&frame, 64, 32, 63, "bottom bar");

    // Vertically the video fills the target, so the edges are video.
    assert_grey(&frame, 64, 0, 32, "video left edge");
    assert_grey(&frame, 64, 63, 32, "video right edge");
}

/// 1:1 into 2:1 must pillarbox: black bars left and right, no top/bottom bars.
#[test]
fn pillarboxes_a_square_video_into_a_wide_target() {
    let fx = fixture_or_skip!();
    let frame = render(&fx, (64, 64), 1.0, (64, 32));

    assert_black(&frame, 64, 0, 16, "left bar");
    assert_black(&frame, 64, 15, 16, "left bar (last column)");
    assert_grey(&frame, 64, 16, 16, "video (first column)");
    assert_grey(&frame, 64, 32, 16, "video centre");
    assert_grey(&frame, 64, 47, 16, "video (last column)");
    assert_black(&frame, 64, 48, 16, "right bar (first column)");
    assert_black(&frame, 64, 63, 16, "right bar");

    // Horizontally the video fills the target, so the edges are video.
    assert_grey(&frame, 64, 32, 0, "video top edge");
    assert_grey(&frame, 64, 32, 31, "video bottom edge");
}

/// Matching aspects must not produce bars anywhere, corners included.
#[test]
fn matching_aspect_fills_the_target() {
    let fx = fixture_or_skip!();
    let frame = render(&fx, (64, 36), 64.0 / 36.0, (64, 36));

    assert_grey(&frame, 64, 0, 0, "top-left corner");
    assert_grey(&frame, 64, 63, 0, "top-right corner");
    assert_grey(&frame, 64, 0, 35, "bottom-left corner");
    assert_grey(&frame, 64, 63, 35, "bottom-right corner");
    assert_grey(&frame, 64, 32, 18, "centre");
}

/// The regression from the brief: 1080p video in the editor's 1440x600
/// preview pane. 16:9 (1.78) is narrower than 2.4:1, so it must pillarbox
/// rather than stretch.
#[test]
fn editor_preview_pane_pillarboxes_1080p() {
    let fx = fixture_or_skip!();
    let frame = render(&fx, (1920, 1080), 16.0 / 9.0, (1440, 600));

    assert_black(&frame, 1440, 0, 300, "left bar");
    assert_black(&frame, 1440, 186, 300, "left bar (last column)");
    assert_grey(&frame, 1440, 187, 300, "video (first column)");
    assert_grey(&frame, 1440, 720, 300, "video centre");
    assert_grey(&frame, 1440, 1252, 300, "video (last column)");
    assert_black(&frame, 1440, 1253, 300, "right bar (first column)");
    assert_black(&frame, 1440, 1439, 300, "right bar");

    // 1080p is 16:9 and the pane is 2.4:1, so the video fills the full height.
    assert_grey(&frame, 1440, 720, 0, "video top edge");
    assert_grey(&frame, 1440, 720, 599, "video bottom edge");
}

/// Anamorphic NTSC: 720x480 is stored 3:2 but tagged SAR 32:27, which displays
/// as 16:9. It must produce the same bars as 1920x1080 in the same pane, and
/// the assertions below are tight enough to fail if the SAR were ignored (a
/// 3:2 frame would put the bars at columns 0..269 and 1170..1439).
#[test]
fn anamorphic_ntsc_matches_1080p_geometry() {
    let fx = fixture_or_skip!();

    let aspect = hwa_core::ffmpeg::display_aspect(720, 480, 32, 27);
    assert!(
        (aspect - 16.0 / 9.0).abs() < 1e-4,
        "SAR 32:27 on 720x480 should display as 16:9, got {aspect}"
    );

    // 192x128 is a 3:2 stored frame, matching real 720x480 material.
    let frame = render(&fx, (192, 128), aspect, (1440, 600));

    assert_black(&frame, 1440, 0, 300, "left bar");
    assert_black(&frame, 1440, 186, 300, "left bar (last column)");
    assert_grey(&frame, 1440, 187, 300, "video (first column)");
    assert_grey(&frame, 1440, 720, 300, "video centre");
    assert_grey(&frame, 1440, 1252, 300, "video (last column)");
    assert_black(&frame, 1440, 1253, 300, "right bar (first column)");
    assert_black(&frame, 1440, 1439, 300, "right bar");

    // Sanity: the display-aspect video also fills the full height.
    assert_grey(&frame, 1440, 720, 0, "video top edge");
    assert_grey(&frame, 1440, 720, 599, "video bottom edge");
}
