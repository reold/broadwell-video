//! Depth two with GPU-written surfaces, holding the imports open.
//!
//! The frame loss at pipeline depth above one needs two things at once: surfaces
//! written by the render engine, and depth above one. Uploaded input at the same
//! depth is clean, so it is not the encoder's pipeline in general.
//!
//! One asymmetry never tested: the export imports the encoder's plane as a fresh
//! `wgpu::Texture` every frame and drops it at the end of that frame, which
//! destroys the VkImage wrapping the dma-buf, while the encoder may still hold
//! the surface. This test imports the same way but keeps every import alive for
//! the whole run. If it passes where the ordinary path loses frames, the
//! per-frame destruction is what the encoder is tripping over.

use hwa_core::encoder::VaapiEncoder;
use std::os::fd::FromRawFd;
use std::process::Command;

const WIDTH: u32 = 640;
const HEIGHT: u32 = 360;
const FRAMES: u64 = 60;
const LUMA: u8 = 160;
const DEPTH: i32 = 2;

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
    .expect("no Vulkan adapter");
    let device_desc = wgpu::DeviceDescriptor {
        label: Some("encoder-depth-gpu"),
        required_features: wgpu::Features::empty(),
        required_limits: wgpu::Limits::default(),
        memory_hints: wgpu::MemoryHints::default(),
        experimental_features: wgpu::ExperimentalFeatures::disabled(),
        trace: wgpu::Trace::Off,
    };
    grafting::vulkan_dmabuf::create_dmabuf_host_context(&adapter, &device_desc).expect("host")
}

/// Import one plane plane of an encoder surface, writable.
fn import(
    host: &grafting::HostWgpuContext,
    fd: std::os::fd::RawFd,
    info: hwa_core::encoder::Plane,
    modifier: u64,
    size: dpi::PhysicalSize<u32>,
    format: wgpu::TextureFormat,
) -> wgpu::Texture {
    let raw = unsafe { libc::dup(fd) };
    assert!(raw >= 0, "dup failed");
    let import = grafting::vulkan_dmabuf::VulkanDmaBufImport::new(
        size,
        format,
        info.fourcc,
        modifier,
        vec![unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) }],
        vec![grafting::vulkan_dmabuf::VulkanDmaBufPlane {
            buffer_index: 0,
            offset: info.offset,
            stride: info.pitch,
        }],
        grafting::vulkan_dmabuf::VulkanDmaBufQueueOwnership::Foreign,
    )
    .expect("import construction")
    .with_usage(
        wgpu::TextureUses::RESOURCE | wgpu::TextureUses::COPY_SRC | wgpu::TextureUses::COPY_DST,
    );
    grafting::vulkan_dmabuf::import_dmabuf(import, host).expect("import_dmabuf")
}

/// Write a flat grey into one plane with a buffer copy.
fn fill(
    host: &grafting::HostWgpuContext,
    texture: &wgpu::Texture,
    value: u8,
    row_bytes: u32,
    size: dpi::PhysicalSize<u32>,
) {
    use wgpu::util::DeviceExt as _;
    let padded_row = row_bytes.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
        * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let data = vec![value; (padded_row * size.height) as usize];
    let buffer = host
        .device
        .create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("fill"),
            contents: &data,
            usage: wgpu::BufferUsages::COPY_SRC,
        });
    let mut enc = host
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("fill") });
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
            texture,
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

#[test]
fn depth_two_survives_when_the_imports_are_held_open() {
    let host = host();
    let out = std::path::Path::new("target/encoder-depth-gpu.mp4");
    std::fs::create_dir_all("target").ok();
    std::fs::remove_file(out).ok();

    let mut encoder = VaapiEncoder::new(out.to_str().unwrap(), WIDTH, HEIGHT, 29.97, 22, DEPTH)
        .expect("open encoder");

    // Every import stays alive until the end of the run.
    let mut held: Vec<wgpu::Texture> = Vec::new();

    for _ in 0..FRAMES {
        let surface = encoder.begin_frame().expect("surface");
        let y = import(
            &host,
            surface.y_fd,
            surface.y,
            surface.modifier,
            dpi::PhysicalSize::new(WIDTH, HEIGHT),
            wgpu::TextureFormat::R8Unorm,
        );
        let uv = import(
            &host,
            surface.uv_fd,
            surface.uv,
            surface.modifier,
            dpi::PhysicalSize::new(WIDTH / 2, HEIGHT / 2),
            wgpu::TextureFormat::Rg8Unorm,
        );
        fill(&host, &y, LUMA, WIDTH, dpi::PhysicalSize::new(WIDTH, HEIGHT));
        fill(
            &host,
            &uv,
            128,
            WIDTH,
            dpi::PhysicalSize::new(WIDTH / 2, HEIGHT / 2),
        );
        host.device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .ok();
        encoder.write_frame().expect("write frame");
        held.push(y);
        held.push(uv);
    }

    let written = encoder.finish().expect("finish");
    assert_eq!(written, FRAMES);

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
        .expect("ffprobe");
    let count: u64 = String::from_utf8_lossy(&counted.stdout)
        .trim()
        .parse()
        .expect("frame count");
    println!(
        "depth {DEPTH}, GPU-written, {} imports held: {count} of {FRAMES} decodable",
        held.len()
    );
    assert_eq!(
        count, FRAMES,
        "holding the imports did not stop the loss, so it is not the destruction"
    );
}
