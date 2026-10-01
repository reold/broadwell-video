//! Prove the export direction of the zero-copy encoder: grade into a VAAPI
//! surface on the GPU and have the encoder be able to read it.
//!
//! `encode_dmabuf_probe` proved that an encoder surface can be mapped to DRM
//! PRIME, and that the encoder accepts it back. This test proves the missing
//! half: that writing to those planes from wgpu actually lands in the surface,
//! which needs a fourth grafting patch so the import can be `COPY_DST`.
//!
//! Nothing here encodes: it takes a surface, writes a known pattern into its two
//! planes with `copy_buffer_to_texture`, downloads it with
//! `av_hwframe_transfer_data` and compares. If the bytes match, a graded picture
//! can reach the encoder without ever crossing the CPU.

use ffmpeg_sys_next::*;
use std::os::fd::FromRawFd;
use wgpu::util::DeviceExt as _;

const WIDTH: u32 = 640;
const HEIGHT: u32 = 360;
const ALIGN: u32 = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;

fn check(rc: i32, what: &str) {
    assert!(rc >= 0, "{what} failed: {rc}");
}

fn padded(bytes: u32) -> u32 {
    bytes.div_ceil(ALIGN) * ALIGN
}

/// A wgpu device with the dmabuf import machinery attached.
fn host() -> Option<grafting::HostWgpuContext> {
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
    desc.backends = wgpu::Backends::VULKAN;
    let instance = wgpu::Instance::new(desc);
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::LowPower,
        compatible_surface: None,
        force_fallback_adapter: false,
        apply_limit_buckets: false,
    }))
    .ok()?;
    let device_desc = wgpu::DeviceDescriptor {
        label: Some("encode-write-test"),
        required_features: wgpu::Features::empty(),
        required_limits: wgpu::Limits::default(),
        memory_hints: wgpu::MemoryHints::default(),
        experimental_features: wgpu::ExperimentalFeatures::disabled(),
        trace: wgpu::Trace::Off,
    };
    grafting::vulkan_dmabuf::create_dmabuf_host_context(&adapter, &device_desc).ok()
}

/// Fill a texture from `data` (one row per `row_bytes`, padded for the copy).
fn upload(
    host: &grafting::HostWgpuContext,
    texture: &wgpu::Texture,
    data: &[u8],
    row_bytes: u32,
    rows: u32,
    extent_width: u32,
) {
    let padded_row = padded(row_bytes);
    let mut padded_data = vec![0u8; (padded_row * rows) as usize];
    for row in 0..rows as usize {
        let src = row * row_bytes as usize;
        let dst = row * padded_row as usize;
        padded_data[dst..dst + row_bytes as usize]
            .copy_from_slice(&data[src..src + row_bytes as usize]);
    }
    let buffer = host
        .device
        .create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("upload"),
            contents: &padded_data,
            usage: wgpu::BufferUsages::COPY_SRC,
        });
    let mut enc = host
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("upload") });
    enc.copy_buffer_to_texture(
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_row),
                rows_per_image: Some(rows),
            },
        },
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::Extent3d {
            width: extent_width,
            height: rows,
            depth_or_array_layers: 1,
        },
    );
    host.queue.submit([enc.finish()]);
}

#[test]
fn a_vaapi_surface_can_be_written_from_the_gpu() {
    let host = host().expect("no Vulkan adapter; this test needs the GPU");

    unsafe {
        let device_name = std::ffi::CString::new("/dev/dri/renderD128").unwrap();
        let mut device: *mut AVBufferRef = std::ptr::null_mut();
        let rc = av_hwdevice_ctx_create(
            &mut device,
            AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
            device_name.as_ptr(),
            std::ptr::null_mut(),
            0,
        );
        assert!(rc >= 0 && !device.is_null(), "VAAPI device open failed: {rc}");

        // A surface pool, sized and formatted exactly as the encoder's would be.
        let mut frames_ref = av_hwframe_ctx_alloc(device);
        assert!(!frames_ref.is_null(), "av_hwframe_ctx_alloc");
        {
            let frames = (*frames_ref).data as *mut AVHWFramesContext;
            (*frames).format = AVPixelFormat::AV_PIX_FMT_VAAPI;
            (*frames).sw_format = AVPixelFormat::AV_PIX_FMT_NV12;
            (*frames).width = WIDTH as i32;
            (*frames).height = HEIGHT as i32;
            (*frames).initial_pool_size = 4;
        }
        check(av_hwframe_ctx_init(frames_ref), "av_hwframe_ctx_init");

        let frame = av_frame_alloc();
        check(
            av_hwframe_get_buffer(frames_ref, frame, 0),
            "av_hwframe_get_buffer",
        );

        // Map it the way the encoder will read it.
        let drm = av_frame_alloc();
        (*drm).format = AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
        check(
            av_hwframe_map(drm, frame, AV_HWFRAME_MAP_READ as i32),
            "av_hwframe_map",
        );
        let desc = (*drm).data[0] as *const AVDRMFrameDescriptor;
        assert!(!desc.is_null(), "no DRM descriptor");
        assert!((*desc).nb_layers >= 2, "expected a Y and a UV layer");

        let y_layer = &(*desc).layers[0];
        let uv_layer = &(*desc).layers[1];
        let y_plane = &y_layer.planes[0];
        let uv_plane = &uv_layer.planes[0];
        let y_obj = &(*desc).objects[y_plane.object_index as usize];
        let uv_obj = &(*desc).objects[uv_plane.object_index as usize];

        // This is what the fourth patch is for: the import has to be writable.
        let writable = wgpu::TextureUses::RESOURCE
            | wgpu::TextureUses::COPY_SRC
            | wgpu::TextureUses::COPY_DST;

        let mut imports = Vec::new();
        for (obj, plane, layer, size, format) in [
            (
                y_obj,
                y_plane,
                y_layer,
                dpi::PhysicalSize::new(WIDTH, HEIGHT),
                wgpu::TextureFormat::R8Unorm,
            ),
            (
                uv_obj,
                uv_plane,
                uv_layer,
                dpi::PhysicalSize::new(WIDTH / 2, HEIGHT / 2),
                wgpu::TextureFormat::Rg8Unorm,
            ),
        ] {
            let fd = std::os::fd::OwnedFd::from_raw_fd(libc::dup(obj.fd));
            let import = grafting::vulkan_dmabuf::VulkanDmaBufImport::new(
                size,
                format,
                layer.format,
                obj.format_modifier,
                vec![fd],
                vec![grafting::vulkan_dmabuf::VulkanDmaBufPlane {
                    buffer_index: 0,
                    offset: plane.offset as u64,
                    stride: plane.pitch as u64,
                }],
                grafting::vulkan_dmabuf::VulkanDmaBufQueueOwnership::Foreign,
            )
            .expect("import construction")
            .with_usage(writable);
            let texture =
                grafting::vulkan_dmabuf::import_dmabuf(import, &host).expect("import_dmabuf");
            imports.push(texture);
        }

        // A pattern that is different on every row, so a row mix-up cannot pass.
        let y_src: Vec<u8> = (0..HEIGHT)
            .flat_map(|row| std::iter::repeat_n(((row * 7 + 3) % 251) as u8, WIDTH as usize))
            .collect();
        let uv_src: Vec<u8> = (0..HEIGHT / 2)
            .flat_map(|row| {
                std::iter::repeat_n([200u8, (row % 200) as u8], (WIDTH / 2) as usize).flatten()
            })
            .collect();
        upload(&host, &imports[0], &y_src, WIDTH, HEIGHT, WIDTH);
        upload(
            &host,
            &imports[1],
            &uv_src,
            WIDTH,
            HEIGHT / 2,
            WIDTH / 2,
        );
        host.device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .ok();

        // Download the surface and check the bytes survived the round trip.
        let sw = av_frame_alloc();
        (*sw).format = AVPixelFormat::AV_PIX_FMT_NV12 as i32;
        (*sw).width = WIDTH as i32;
        (*sw).height = HEIGHT as i32;
        check(av_frame_get_buffer(sw, 0), "av_frame_get_buffer");
        check(
            av_hwframe_transfer_data(sw, frame, 0),
            "av_hwframe_transfer_data",
        );

        let y_linesize = (*sw).linesize[0] as usize;
        let mut mismatches = 0usize;
        for row in 0..HEIGHT as usize {
            let want = ((row * 7 + 3) % 251) as u8;
            let got = *(*sw).data[0].add(row * y_linesize);
            if got != want {
                if mismatches < 5 {
                    eprintln!("  y row {row}: want {want}, got {got}");
                }
                mismatches += 1;
            }
        }
        assert_eq!(
            mismatches, 0,
            "the GPU write did not reach the Y plane ({mismatches} rows wrong)"
        );

        let uv_linesize = (*sw).linesize[1] as usize;
        for row in 0..(HEIGHT / 2) as usize {
            // The interleaved plane is U then V, so the pair straddles the row.
            let u = *(*sw).data[1].add(row * uv_linesize);
            let v = *(*sw).data[1].add(row * uv_linesize + 1);
            assert_eq!(u, 200, "U sample of UV row {row}");
            assert_eq!(v, (row % 200) as u8, "V sample of UV row {row}");
        }
        println!("surface round trip verified: Y rows and UV rows all match");

        av_frame_free(&mut { sw } as *mut *mut AVFrame);
        av_frame_free(&mut { drm } as *mut *mut AVFrame);
        av_frame_free(&mut { frame } as *mut *mut AVFrame);
        av_buffer_unref(&mut frames_ref);
        av_buffer_unref(&mut device);
    }
}
