//! The export handoff geometry: packed grade -> staging buffer -> the encoder's
//! own surface, all on the GPU.
//!
//! This is the shape the renderer uses, without the decoder or the compute
//! grade, because the strides and padding are where a zero-copy handoff breaks.
//! The packed textures hold four Y samples and two UV pairs per `R32Uint` texel
//! (the packing the readback used to want), the staging buffers need 256-byte
//! aligned rows for the copies, and the destination planes are R8 and Rg8 at the
//! encoder surface's own pitch. Get any of those wrong and the picture is torn
//! rather than absent, so the test checks the decoded luma as well as the count.

use ffmpeg_sys_next::*;
use hwa_core::encoder::VaapiEncoder;
use hwa_core::export::Nv12Readback;
use std::os::fd::FromRawFd;
use std::process::Command;

const WIDTH: u32 = 640;
const HEIGHT: u32 = 360;
const FRAMES: u64 = 30;
/// Mid grey, so a plane swap or a stride error shows up as a wrong number.
const LUMA: u8 = 160;

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
        label: Some("export-handoff"),
        required_features: wgpu::Features::empty(),
        required_limits: wgpu::Limits::default(),
        memory_hints: wgpu::MemoryHints::default(),
        experimental_features: wgpu::ExperimentalFeatures::disabled(),
        trace: wgpu::Trace::Off,
    };
    grafting::vulkan_dmabuf::create_dmabuf_host_context(&adapter, &device_desc)
        .expect("dmabuf host context")
}

fn packed_texture(
    host: &grafting::HostWgpuContext,
    width: u32,
    height: u32,
    texel: u32,
) -> wgpu::Texture {
    let texture = host.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("packed"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R32Uint,
        usage: wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    // Four identical bytes per texel: 4 Y samples, or 2 UV pairs.
    let bytes = texel.to_le_bytes();
    let row: Vec<u8> = bytes.iter().copied().cycle().take((width * 4) as usize).collect();
    let mut data = Vec::with_capacity((width * 4 * height) as usize);
    for _ in 0..height {
        data.extend_from_slice(&row);
    }
    host.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &data,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(width * 4),
            rows_per_image: Some(height),
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    texture
}

#[test]
fn the_packed_grade_reaches_the_encoder_surface() {
    let host = host();
    let geometry = Nv12Readback::new(WIDTH, HEIGHT);
    let out = std::path::Path::new("target/export-handoff.mp4");
    std::fs::create_dir_all("target").ok();
    std::fs::remove_file(out).ok();

    // The packed grade, exactly the shape the export shaders produce.
    let packed_y = packed_texture(&host, WIDTH / 4, HEIGHT, 0xA0A0_A0A0);
    let packed_uv = packed_texture(&host, WIDTH / 4, HEIGHT / 2, 0x8080_8080);

    let staging = |size: u64, label: &str| {
        host.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        })
    };
    let staging_y = staging(geometry.y_size, "staging-y");
    let staging_uv = staging(geometry.uv_size, "staging-uv");

    let mut encoder =
        VaapiEncoder::new(out.to_str().unwrap(), WIDTH, HEIGHT, 29.97, 22).expect("open encoder");

    for _ in 0..FRAMES {
        let surface = encoder.begin_frame().expect("surface");

        // The two destination planes, imported writable: the fourth grafting
        // patch, without which these copies fail validation.
        let plane = |fd: std::os::fd::RawFd,
                     info: hwa_core::encoder::Plane,
                     size: dpi::PhysicalSize<u32>,
                     format: wgpu::TextureFormat| {
            let raw = unsafe { libc::dup(fd) };
            assert!(raw >= 0, "dup failed");
            let import = grafting::vulkan_dmabuf::VulkanDmaBufImport::new(
                size,
                format,
                info.fourcc,
                surface.modifier,
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
                wgpu::TextureUses::RESOURCE
                    | wgpu::TextureUses::COPY_SRC
                    | wgpu::TextureUses::COPY_DST,
            );
            grafting::vulkan_dmabuf::import_dmabuf(import, &host).expect("import_dmabuf")
        };
        let y_plane = plane(
            surface.y_fd,
            surface.y,
            dpi::PhysicalSize::new(WIDTH, HEIGHT),
            wgpu::TextureFormat::R8Unorm,
        );
        let uv_plane = plane(
            surface.uv_fd,
            surface.uv,
            dpi::PhysicalSize::new(WIDTH / 2, HEIGHT / 2),
            wgpu::TextureFormat::Rg8Unorm,
        );

        let mut enc = host
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("handoff"),
            });
        // Packed texture -> staging, padded rows.
        for (packed, buffer, row, rows, texels) in [
            (
                &packed_y,
                &staging_y,
                geometry.y_padded_row_bytes,
                HEIGHT,
                WIDTH / 4,
            ),
            (
                &packed_uv,
                &staging_uv,
                geometry.uv_padded_row_bytes,
                HEIGHT / 2,
                WIDTH / 4,
            ),
        ] {
            enc.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture: packed,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(row),
                        rows_per_image: Some(rows),
                    },
                },
                wgpu::Extent3d {
                    width: texels,
                    height: rows,
                    depth_or_array_layers: 1,
                },
            );
        }
        // Staging -> the encoder's planes, at the surface's own pitch.
        for (buffer, plane, row, rows, width) in [
            (
                &staging_y,
                &y_plane,
                geometry.y_padded_row_bytes,
                HEIGHT,
                WIDTH,
            ),
            (
                &staging_uv,
                &uv_plane,
                geometry.uv_padded_row_bytes,
                HEIGHT / 2,
                WIDTH / 2,
            ),
        ] {
            enc.copy_buffer_to_texture(
                wgpu::TexelCopyBufferInfo {
                    buffer,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(row),
                        rows_per_image: Some(rows),
                    },
                },
                wgpu::TexelCopyTextureInfo {
                    texture: plane,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::Extent3d {
                    width,
                    height: rows,
                    depth_or_array_layers: 1,
                },
            );
        }
        host.queue.submit([enc.finish()]);
        host.device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .ok();
        encoder.write_frame().expect("write frame");
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
    assert_eq!(count, FRAMES, "decodable frames after the handoff");

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
        .expect("ffmpeg");
    let pixels = (WIDTH * HEIGHT) as usize;
    assert!(decoded.stdout.len() >= pixels, "short decode");
    let mean: f64 = decoded.stdout[..pixels].iter().map(|b| *b as f64).sum::<f64>() / pixels as f64;
    println!("handoff decoded luma mean {mean:.1} against {LUMA} packed");
    assert!(
        (mean - LUMA as f64).abs() < 12.0,
        "decoded luma {mean:.1} is not the {LUMA} that went through the staging buffers"
    );

    unsafe {
        av_log_set_level(AV_LOG_QUIET);
    }
}
