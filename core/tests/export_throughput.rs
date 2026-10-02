//! Throughput harness for the in-process export path: decode, grade, hand the
//! result to the encoder's own surface, and mux, with nothing crossing the CPU.
//!
//! Not a unit test. It needs `HWA_TEST_VIDEO` pointing at a real clip, and it
//! reports numbers rather than asserting them, because the point is to compare
//! against the subprocess path this replaced:
//!
//!   HWA_TEST_VIDEO=/path/clip.mkv cargo test --release -p hwa-core \
//!       --test export_throughput -- --ignored --nocapture
//!
//! The decode sequence is the renderer's, repeated here because this harness
//! drives the loop itself rather than a window. Everything downstream of it —
//! the grade pipelines, the packing, the staging copies, the encoder — is the
//! same code the editor runs.

use ffmpeg_sys_next::*;
use hwa_core::encoder::VaapiEncoder;
use hwa_core::export::Nv12Readback;
use hwa_core::{ffmpeg, gpu};
use std::os::fd::FromRawFd;
use std::collections::HashMap;
use std::process::Command;
use std::time::{Duration, Instant};

const DEVICE: &str = "/dev/dri/renderD128";

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
    .expect("no Vulkan adapter: this harness needs the GPU");
    let device_desc = wgpu::DeviceDescriptor {
        label: Some("export-throughput"),
        required_features: wgpu::Features::empty(),
        required_limits: wgpu::Limits::default(),
        memory_hints: wgpu::MemoryHints::default(),
        experimental_features: wgpu::ExperimentalFeatures::disabled(),
        trace: wgpu::Trace::Off,
    };
    grafting::vulkan_dmabuf::create_dmabuf_host_context(&adapter, &device_desc)
        .expect("dmabuf host context")
}

/// The two planes of a dma-buf backed frame as textures.
fn import_plane(
    host: &grafting::HostWgpuContext,
    fd: std::os::fd::RawFd,
    fourcc: u32,
    modifier: u64,
    offset: u64,
    stride: u64,
    size: dpi::PhysicalSize<u32>,
    format: wgpu::TextureFormat,
    writable: bool,
) -> wgpu::Texture {
    let raw = unsafe { libc::dup(fd) };
    assert!(raw >= 0, "dup failed");
    let mut import = grafting::vulkan_dmabuf::VulkanDmaBufImport::new(
        size,
        format,
        fourcc,
        modifier,
        vec![unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) }],
        vec![grafting::vulkan_dmabuf::VulkanDmaBufPlane {
            buffer_index: 0,
            offset,
            stride,
        }],
        grafting::vulkan_dmabuf::VulkanDmaBufQueueOwnership::Foreign,
    )
    .expect("import construction");
    if writable {
        import = import.with_usage(
            wgpu::TextureUses::RESOURCE
                | wgpu::TextureUses::COPY_SRC
                | wgpu::TextureUses::COPY_DST,
        );
    }
    grafting::vulkan_dmabuf::import_dmabuf(import, host).expect("import_dmabuf")
}

/// Decode frames one at a time, the way the renderer does for an export.
struct Decoder {
    ff: ffmpeg::Handles,
    drained: bool,
}

impl Decoder {
    unsafe fn next(&mut self) -> bool {
        loop {
            let mut got_packet = false;
            let rc = av_read_frame(self.ff.fmt_ctx, self.ff.packet);
            if rc < 0 {
                if !self.drained {
                    avcodec_send_packet(self.ff.codec_ctx, std::ptr::null());
                    self.drained = true;
                }
            } else {
                got_packet = true;
                if (*self.ff.packet).stream_index == self.ff.video_stream {
                    let _ = avcodec_send_packet(self.ff.codec_ctx, self.ff.packet);
                }
                av_packet_unref(self.ff.packet);
            }

            let rc2 = avcodec_receive_frame(self.ff.codec_ctx, self.ff.decoded);
            if rc2 >= 0 {
                return true;
            }
            if rc < 0 && !got_packet {
                return false;
            }
        }
    }
}

#[test]
#[ignore = "throughput harness; set HWA_TEST_VIDEO to run it"]
fn in_process_export_throughput() {
    let Ok(path) = std::env::var("HWA_TEST_VIDEO") else {
        println!("HWA_TEST_VIDEO is not set; nothing to measure");
        return;
    };
    let host = host();
    let ff = unsafe { ffmpeg::Handles::open(&path, DEVICE).expect("open the clip") };
    let vw = ff.width;
    let vh = ff.height;
    let fps = ff.fps;
    println!("clip {vw}x{vh} at {fps:.3} fps");

    let pipelines = gpu::build_pipelines(&host, wgpu::TextureFormat::Bgra8UnormSrgb, vw, vh);
    let geometry = Nv12Readback::new(vw, vh);
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

    let out = "target/export-throughput.mp4";
    std::fs::create_dir_all("target").ok();
    std::fs::remove_file(out).ok();
    let mut encoder =
        VaapiEncoder::new(out, vw, vh, fps, 22, 1).expect("open the in-process encoder");

    let mut decoder = Decoder {
        ff,
        drained: false,
    };

    // Encoder surface imports are keyed by surface, like the renderer's cache:
    // they must outlive the frame that made them, because destroying the VkImage
    // wrapping a dma-buf while the encoder still reads that surface loses
    // pictures. Decoder surfaces are left per frame, which is what the renderer's
    // bounded frame cache does in effect.
    // Keyed by VA-API surface id, not file descriptor. The descriptors belong to
    // the mapped frame and are closed each frame, so their numbers recycle and
    // an fd-keyed cache serves one surface's texture for another: this harness
    // reproduced exactly that, 600 of 900 frames encoded from a surface nothing
    // had written.
    let mut encoder_imports: HashMap<(usize, u64), wgpu::Texture> = HashMap::new();

    let mut frames = 0u64;
    let (mut t_import, mut t_grade, mut t_handoff, mut t_send) = (
        Duration::ZERO,
        Duration::ZERO,
        Duration::ZERO,
        Duration::ZERO,
    );
    let wall = Instant::now();

    loop {
        let t = Instant::now();
        if unsafe { !decoder.next() } {
            break;
        }
        let ff = &mut decoder.ff;

        // Map the decoded frame and import both planes.
        unsafe {
            (*ff.drm_frame).format = AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
            ffmpeg::check(
                av_hwframe_map(ff.drm_frame, ff.decoded, AV_HWFRAME_MAP_READ as i32),
                "av_hwframe_map",
            )
            .expect("map");
            av_frame_unref(ff.decoded);
        }
        let desc = unsafe { (*decoder.ff.drm_frame).data[0] as *const AVDRMFrameDescriptor };
        assert!(!desc.is_null() && unsafe { (*desc).nb_layers } >= 2, "descriptor");
        let (y_layer, uv_layer) = unsafe { (&(*desc).layers[0], &(*desc).layers[1]) };
        let (y_plane, uv_plane) = (&y_layer.planes[0], &uv_layer.planes[0]);
        let (y_obj, uv_obj) = unsafe {
            (
                &(*desc).objects[y_plane.object_index as usize],
                &(*desc).objects[uv_plane.object_index as usize],
            )
        };
        let source_y = import_plane(
            &host,
            y_obj.fd,
            y_layer.format,
            y_obj.format_modifier,
            y_plane.offset as u64,
            y_plane.pitch as u64,
            dpi::PhysicalSize::new(vw, vh),
            wgpu::TextureFormat::R8Unorm,
            false,
        );
        let source_uv = import_plane(
            &host,
            uv_obj.fd,
            uv_layer.format,
            uv_obj.format_modifier,
            uv_plane.offset as u64,
            uv_plane.pitch as u64,
            dpi::PhysicalSize::new(vw / 2, vh / 2),
            wgpu::TextureFormat::Rg8Unorm,
            false,
        );
        unsafe { av_frame_unref(decoder.ff.drm_frame) };
        let source_y_view = source_y.create_view(&wgpu::TextureViewDescriptor::default());
        let source_uv_view = source_uv.create_view(&wgpu::TextureViewDescriptor::default());
        t_import += t.elapsed();

        // The encoder lends the surface this frame is written into.
        let surface = encoder.begin_frame().expect("surface");
        let mut dest_plane = |fd: std::os::fd::RawFd,
                              info: hwa_core::encoder::Plane,
                              size: dpi::PhysicalSize<u32>,
                              format: wgpu::TextureFormat,
                              map: &mut HashMap<(usize, u64), wgpu::Texture>| {
            map.entry((surface.id, info.offset))
                .or_insert_with(|| {
                    import_plane(
                        &host,
                        fd,
                        info.fourcc,
                        surface.modifier,
                        info.offset,
                        info.pitch,
                        size,
                        format,
                        true,
                    )
                })
                .clone()
        };
        let dest_y = dest_plane(
            surface.y_fd,
            surface.y,
            dpi::PhysicalSize::new(vw, vh),
            wgpu::TextureFormat::R8Unorm,
            &mut encoder_imports,
        );
        let dest_uv = dest_plane(
            surface.uv_fd,
            surface.uv,
            dpi::PhysicalSize::new(vw / 2, vh / 2),
            wgpu::TextureFormat::Rg8Unorm,
            &mut encoder_imports,
        );

        let bg_y = host.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("grade-y"),
            layout: &pipelines.grade_y_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&source_y_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&source_uv_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&pipelines.out_y_view),
                },
            ],
        });
        let bg_uv = host.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("grade-uv"),
            layout: &pipelines.grade_uv_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&source_y_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&source_uv_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&pipelines.out_uv_view),
                },
            ],
        });


        let t = Instant::now();
        let mut enc = host
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("export"),
            });
        {
            let mut cp = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("grade-y"),
                timestamp_writes: None,
            });
            cp.set_pipeline(&pipelines.grade_y_pipeline);
            cp.set_bind_group(0, &bg_y, &[]);
            cp.dispatch_workgroups(vw.div_ceil(8), vh.div_ceil(8), 1);
        }
        {
            let mut cp = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("grade-uv"),
                timestamp_writes: None,
            });
            cp.set_pipeline(&pipelines.grade_uv_pipeline);
            cp.set_bind_group(0, &bg_uv, &[]);
            cp.dispatch_workgroups((vw / 2).div_ceil(8), (vh / 2).div_ceil(8), 1);
        }
        for (packed, buffer, row, rows, texels) in [
            (
                &pipelines.out_y_texture,
                &staging_y,
                geometry.y_padded_row_bytes,
                vh,
                vw / 4,
            ),
            (
                &pipelines.out_uv_texture,
                &staging_uv,
                geometry.uv_padded_row_bytes,
                vh / 2,
                vw / 4,
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
        for (buffer, plane, row, rows, width) in [
            (&staging_y, &dest_y, geometry.y_padded_row_bytes, vh, vw),
            (
                &staging_uv,
                &dest_uv,
                geometry.uv_padded_row_bytes,
                vh / 2,
                vw / 2,
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
        t_grade += t.elapsed();

        let t = Instant::now();
        host.device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .ok();
        t_handoff += t.elapsed();

        let t = Instant::now();
        encoder.write_frame().expect("write frame");
        t_send += t.elapsed();

        frames += 1;
    }

    let elapsed = wall.elapsed();
    let written = encoder.finish().expect("finish");
    let per = |d: Duration| d.as_secs_f64() * 1000.0 / frames.max(1) as f64;
    println!(
        "{frames} frames in {:.1}s = {:.1} fps | import {:.2} ms grade+copy {:.2} ms handoff {:.2} ms send {:.2} ms",
        elapsed.as_secs_f64(),
        frames as f64 / elapsed.as_secs_f64(),
        per(t_import),
        per(t_grade),
        per(t_handoff),
        per(t_send),
    );

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
            out,
        ])
        .output()
        .expect("ffprobe");
    let count: u64 = String::from_utf8_lossy(&counted.stdout)
        .trim()
        .parse()
        .expect("frame count");
    println!("encoder wrote {written} frames; the file holds {count}");

    // Content, not just count. A surface the GPU never wrote encodes as all
    // zeros: chroma of 0 rather than the 128 of a black frame, which decodes as
    // a green flash. Counting frames cannot see that -- a whole export was
    // pixel-wrong while every count was right -- so this asks signalstats for
    // per frame chroma and insists none of it is empty.
    let stats = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-i",
            out,
            "-vf",
            "signalstats,metadata=print:file=-",
            "-f",
            "null",
            "-",
        ])
        .output()
        .expect("ffmpeg signalstats");
    let text = String::from_utf8_lossy(&stats.stdout);
    let mut pending: Option<f64> = None;
    let (mut checked, mut blank) = (0u64, 0u64);
    let mut close = |u: Option<f64>, checked: &mut u64, blank: &mut u64| {
        if let Some(u) = u {
            *checked += 1;
            if u < 20.0 {
                *blank += 1;
            }
        }
    };
    for line in text.lines() {
        if line.starts_with("frame:") {
            close(pending.take(), &mut checked, &mut blank);
            continue;
        }
        if let Some(rest) = line.split("UAVG=").nth(1) {
            pending = rest.trim().parse().ok();
        }
    }
    close(pending, &mut checked, &mut blank);
    println!("content: {checked} frames measured, {blank} with no chroma");
    assert_eq!(checked, count, "signalstats saw a different number of frames");
    assert_eq!(blank, 0, "{blank} frames carry an empty chroma plane");

    unsafe {
        av_log_set_level(AV_LOG_QUIET);
    }
}
