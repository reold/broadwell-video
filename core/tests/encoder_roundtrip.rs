//! The whole in-process export path, in one test.
//!
//! Take a surface from the encoder, write a known picture into its two planes
//! from the GPU, hand it back with `avcodec_send_frame`, and let libavformat
//! mux it. Then check both the frame count and the *content*: a file that has
//! thirty frames of the wrong thing is not a working export, which is the
//! lesson this project already paid for once.

use ffmpeg_sys_next::*;
use hwa_core::encoder::VaapiEncoder;
use std::os::fd::FromRawFd;
use std::process::Command;
use wgpu::util::DeviceExt as _;

const WIDTH: u32 = 640;
const HEIGHT: u32 = 360;
/// Long enough to force the surface pool to be reused several times, which is
/// where the packet loss showed up first.
const FRAMES: u64 = 60;
/// A mid grey, chosen so that a channel mix-up or a blank surface is obvious.
const LUMA: u8 = 160;
const CHROMA: u8 = 128;
const ALIGN: u32 = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;

fn host() -> grafting::HostWgpuContext {
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
    desc.backends = wgpu::Backends::VULKAN;
    let instance = wgpu::Instance::new(desc);
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::LowPower,
        compatible_surface: None,
        force_fallback_adapter: false,
        apply_limit_buckets: false,
    }))
    .expect("no Vulkan adapter: this test needs the GPU");
    let device_desc = wgpu::DeviceDescriptor {
        label: Some("encoder-roundtrip"),
        required_features: wgpu::Features::empty(),
        required_limits: wgpu::Limits::default(),
        memory_hints: wgpu::MemoryHints::default(),
        experimental_features: wgpu::ExperimentalFeatures::disabled(),
        trace: wgpu::Trace::Off,
    };
    grafting::vulkan_dmabuf::create_dmabuf_host_context(&adapter, &device_desc)
        .expect("dmabuf host context")
}

/// Import one plane of an encoder surface, ready to be written.
fn write_plane(
    host: &grafting::HostWgpuContext,
    fd: std::os::fd::RawFd,
    surface: &hwa_core::encoder::Surface,
    plane: hwa_core::encoder::Plane,
    size: dpi::PhysicalSize<u32>,
    format: wgpu::TextureFormat,
    contents: &[u8],
    row_bytes: u32,
) {
    let import = grafting::vulkan_dmabuf::VulkanDmaBufImport::new(
        size,
        format,
        plane.fourcc,
        surface.modifier,
        vec![unsafe { std::os::fd::OwnedFd::from_raw_fd(libc::dup(fd)) }],
        vec![grafting::vulkan_dmabuf::VulkanDmaBufPlane {
            buffer_index: 0,
            offset: plane.offset,
            stride: plane.pitch,
        }],
        grafting::vulkan_dmabuf::VulkanDmaBufQueueOwnership::Foreign,
    )
    .expect("import construction")
    .with_usage(
        wgpu::TextureUses::RESOURCE
            | wgpu::TextureUses::COPY_SRC
            | wgpu::TextureUses::COPY_DST,
    );
    let texture = grafting::vulkan_dmabuf::import_dmabuf(import, host).expect("import_dmabuf");

    let padded_row = row_bytes.div_ceil(ALIGN) * ALIGN;
    let mut padded = vec![0u8; (padded_row * size.height) as usize];
    for row in 0..size.height as usize {
        let dst = row * padded_row as usize;
        let src = row * row_bytes as usize;
        padded[dst..dst + row_bytes as usize]
            .copy_from_slice(&contents[src..src + row_bytes as usize]);
    }
    let buffer = host
        .device
        .create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("plane"),
            contents: &padded,
            usage: wgpu::BufferUsages::COPY_SRC,
        });
    let mut enc = host
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("plane") });
    enc.copy_buffer_to_texture(
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_row),
                rows_per_image: Some(size.height),
            },
        },
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::Extent3d {
            width: size.width,
            height: size.height,
            depth_or_array_layers: 1,
        },
    );
    host.queue.submit([enc.finish()]);
}

// Sixty frames, so the surface pool is exercised more than once, and the
// content is checked as well as the count: a file with the right number of
// frames of the wrong picture is not a working export.
#[test]
fn an_in_process_export_encodes_what_the_gpu_wrote() {
    let host = host();
    let out = std::path::Path::new("target/encoder-roundtrip.mp4");
    std::fs::create_dir_all("target").ok();
    std::fs::remove_file(out).ok();

    let y_plane = vec![LUMA; (WIDTH * HEIGHT) as usize];
    let uv_plane: Vec<u8> = std::iter::repeat_n([CHROMA, CHROMA], (WIDTH * HEIGHT / 4) as usize)
        .flatten()
        .collect();

    let mut encoder =
        VaapiEncoder::new(out.to_str().unwrap(), WIDTH, HEIGHT, 29.97, 22).expect("open encoder");

    for _ in 0..FRAMES {
        let surface = encoder.begin_frame().expect("surface");
        write_plane(
            &host,
            surface.y_fd,
            &surface,
            surface.y,
            dpi::PhysicalSize::new(WIDTH, HEIGHT),
            wgpu::TextureFormat::R8Unorm,
            &y_plane,
            WIDTH,
        );
        write_plane(
            &host,
            surface.uv_fd,
            &surface,
            surface.uv,
            dpi::PhysicalSize::new(WIDTH / 2, HEIGHT / 2),
            wgpu::TextureFormat::Rg8Unorm,
            &uv_plane,
            WIDTH,
        );
        // The GPU writes have to be finished before the encoder reads the
        // surface. This is one sync of our own queue, not a readback.
        host.device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .ok();
        encoder.write_frame().expect("write frame");
    }

    let written = encoder.finish().expect("finish");
    assert_eq!(written, FRAMES, "frames handed to the encoder");

    // The file is the source of truth, so count what is in it.
    let counted = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-count_frames",
            "-select_streams",
            "v",
            "-show_entries",
            "stream=nb_read_frames",
            "-of",
            "default=nw=1:nk=1",
            out.to_str().unwrap(),
        ])
        .output()
        .expect("run ffprobe");
    let count: u64 = String::from_utf8_lossy(&counted.stdout)
        .trim()
        .parse()
        .expect("frame count");
    assert_eq!(count, FRAMES, "decodable frames in the muxed file");

    // And that it holds the picture we wrote, not whatever was in the surface.
    let decoded = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-i",
            out.to_str().unwrap(),
            "-frames:v",
            "1",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "gray",
            "-",
        ])
        .output()
        .expect("run ffmpeg");
    assert!(
        decoded.stdout.len() >= (WIDTH * HEIGHT) as usize,
        "decoded frame was {} bytes",
        decoded.stdout.len()
    );
    let mean: f64 = decoded.stdout[..(WIDTH * HEIGHT) as usize]
        .iter()
        .map(|b| *b as f64)
        .sum::<f64>()
        / (WIDTH * HEIGHT) as f64;
    println!("decoded luma mean {mean:.1} against {LUMA} written");
    assert!(
        (mean - LUMA as f64).abs() < 12.0,
        "decoded luma {mean:.1} is not the {LUMA} the GPU wrote"
    );

    unsafe {
        av_log_set_level(AV_LOG_QUIET);
    }
}
