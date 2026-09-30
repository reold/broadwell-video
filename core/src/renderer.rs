//! Shared preview pipeline. Owns the FFmpeg decoder, the wgpu pipelines,
//! and the DMA-BUF texture cache. Callers provide a surface + config and
//! drive the frame loop; this module handles decode → import → grade → blit.

use crate::{ffmpeg, gpu};
use anyhow::Result;
use ffmpeg_sys_next::*;
use std::collections::HashMap;
use std::os::fd::{FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

const MAX_CACHE_ENTRIES: usize = 64;

#[derive(Clone, Copy, Debug)]
pub enum FrameOutcome {
    Processed,
    Skipped,
    Eof,
}

pub struct PreviewRenderer {
    pub ff: ffmpeg::Handles,
    pub host: grafting::HostWgpuContext,
    pub pipelines: gpu::Pipelines,
    pub texture_cache: HashMap<gpu::CacheKey, gpu::CachedNv12>,
    pub frames: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub last_log: Instant,
}

impl PreviewRenderer {
    /// Build the full preview pipeline for `video_path`.
    ///
    /// `surface_format` is the wgpu format of the target surface (used to
    /// construct the blit pipeline). The host context is consumed.
    pub fn new(
        host: grafting::HostWgpuContext,
        video_path: &str,
        surface_format: wgpu::TextureFormat,
    ) -> Result<Self> {
        let ff = unsafe { ffmpeg::Handles::open(video_path, "/dev/dri/renderD128")? };
        let vw = ff.width;
        let vh = ff.height;
        println!("Video: {vw}x{vh} @ {:.2} fps", ff.fps);

        let pipelines = gpu::build_pipelines(&host, surface_format, vw, vh);

        Ok(Self {
            ff,
            host,
            pipelines,
            texture_cache: HashMap::new(),
            frames: 0,
            cache_hits: 0,
            cache_misses: 0,
            last_log: Instant::now(),
        })
    }

    /// Advance one frame and blit it into `surface`.
    ///
    /// The caller is responsible for calling `surface.present()` afterwards
    /// if the surface backend requires it. In wgpu 30 the present happens
    /// inside `queue.present()` which we call here.
    pub fn render_frame(
        &mut self,
        surface: &wgpu::Surface<'_>,
        config: &wgpu::SurfaceConfiguration,
    ) -> Result<FrameOutcome> {
        let vw = self.ff.width;
        let vh = self.ff.height;

        unsafe {
            // ---- decode ----
            let rc = av_read_frame(self.ff.fmt_ctx, self.ff.packet);
            if rc < 0 {
                self.ff.rewind();
                return Ok(FrameOutcome::Eof);
            }
            if (*self.ff.packet).stream_index != self.ff.video_stream {
                av_packet_unref(self.ff.packet);
                return Ok(FrameOutcome::Skipped);
            }
            if avcodec_send_packet(self.ff.codec_ctx, self.ff.packet) < 0 {
                av_packet_unref(self.ff.packet);
                return Ok(FrameOutcome::Skipped);
            }
            av_packet_unref(self.ff.packet);

            let rc = avcodec_receive_frame(self.ff.codec_ctx, self.ff.decoded);
            if rc == -libc::EAGAIN {
                return Ok(FrameOutcome::Skipped);
            }
            if rc != 0 {
                eprintln!("avcodec_receive_frame rc={rc}");
                return Ok(FrameOutcome::Eof);
            }

            // ---- map decoded NV12 to DRM_PRIME ----
            (*self.ff.drm_frame).format = AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
            ffmpeg::check(
                av_hwframe_map(
                    self.ff.drm_frame,
                    self.ff.decoded,
                    AV_HWFRAME_MAP_READ as i32,
                ),
                "av_hwframe_map",
            )?;
            av_frame_unref(self.ff.decoded);

            let desc = (*self.ff.drm_frame).data[0] as *const AVDRMFrameDescriptor;
            if desc.is_null() || (*desc).nb_layers < 2 {
                av_frame_unref(self.ff.drm_frame);
                return Ok(FrameOutcome::Skipped);
            }

            let y_layer = &(*desc).layers[0];
            let uv_layer = &(*desc).layers[1];
            let y_plane = &y_layer.planes[0];
            let uv_plane = &uv_layer.planes[0];
            let y_obj = &(*desc).objects[y_plane.object_index as usize];
            let uv_obj = &(*desc).objects[uv_plane.object_index as usize];

            let key = {
                let mut stat: libc::stat = std::mem::zeroed();
                if libc::fstat(y_obj.fd, &mut stat) != 0 {
                    av_frame_unref(self.ff.drm_frame);
                    return Ok(FrameOutcome::Skipped);
                }
                gpu::CacheKey {
                    dev: stat.st_dev as u64,
                    ino: stat.st_ino as u64,
                    offset: y_plane.offset as u64,
                    modifier: y_obj.format_modifier,
                }
            };

            let (y_view, uv_view) = if let Some(entry) = self.texture_cache.get(&key) {
                self.cache_hits += 1;
                (entry.y_view.clone(), entry.uv_view.clone())
            } else {
                self.cache_misses += 1;

                let y_fd: OwnedFd = {
                    let raw = libc::dup(y_obj.fd);
                    if raw < 0 {
                        av_frame_unref(self.ff.drm_frame);
                        return Ok(FrameOutcome::Skipped);
                    }
                    OwnedFd::from_raw_fd(raw)
                };
                let uv_fd: OwnedFd = {
                    let raw = libc::dup(uv_obj.fd);
                    if raw < 0 {
                        av_frame_unref(self.ff.drm_frame);
                        return Ok(FrameOutcome::Skipped);
                    }
                    OwnedFd::from_raw_fd(raw)
                };

                let y_import = match grafting::vulkan_dmabuf::VulkanDmaBufImport::new(
                    dpi::PhysicalSize::new(vw, vh),
                    wgpu::TextureFormat::R8Unorm,
                    y_layer.format,
                    y_obj.format_modifier,
                    vec![y_fd],
                    vec![grafting::vulkan_dmabuf::VulkanDmaBufPlane {
                        buffer_index: 0,
                        offset: y_plane.offset as u64,
                        stride: y_plane.pitch as u64,
                    }],
                    grafting::vulkan_dmabuf::VulkanDmaBufQueueOwnership::Foreign,
                ) {
                    Ok(i) => i,
                    Err(e) => {
                        eprintln!("Y import failed: {e:?}");
                        av_frame_unref(self.ff.drm_frame);
                        return Ok(FrameOutcome::Skipped);
                    }
                };
                let y_texture = match grafting::vulkan_dmabuf::import_dmabuf(y_import, &self.host) {
                    Ok(t) => t,
                    Err(e) => {
                        eprintln!("Y import_dmabuf failed: {e:?}");
                        av_frame_unref(self.ff.drm_frame);
                        return Ok(FrameOutcome::Skipped);
                    }
                };

                let uv_import = match grafting::vulkan_dmabuf::VulkanDmaBufImport::new(
                    dpi::PhysicalSize::new(vw / 2, vh / 2),
                    wgpu::TextureFormat::Rg8Unorm,
                    uv_layer.format,
                    uv_obj.format_modifier,
                    vec![uv_fd],
                    vec![grafting::vulkan_dmabuf::VulkanDmaBufPlane {
                        buffer_index: 0,
                        offset: uv_plane.offset as u64,
                        stride: uv_plane.pitch as u64,
                    }],
                    grafting::vulkan_dmabuf::VulkanDmaBufQueueOwnership::Foreign,
                ) {
                    Ok(i) => i,
                    Err(e) => {
                        eprintln!("UV import failed: {e:?}");
                        av_frame_unref(self.ff.drm_frame);
                        return Ok(FrameOutcome::Skipped);
                    }
                };
                let uv_texture = match grafting::vulkan_dmabuf::import_dmabuf(uv_import, &self.host)
                {
                    Ok(t) => t,
                    Err(e) => {
                        eprintln!("UV import_dmabuf failed: {e:?}");
                        av_frame_unref(self.ff.drm_frame);
                        return Ok(FrameOutcome::Skipped);
                    }
                };

                let y_view = y_texture.create_view(&wgpu::TextureViewDescriptor::default());
                let uv_view = uv_texture.create_view(&wgpu::TextureViewDescriptor::default());

                if self.texture_cache.len() < MAX_CACHE_ENTRIES {
                    self.texture_cache.insert(
                        key,
                        gpu::CachedNv12 {
                            y_texture,
                            uv_texture,
                            y_view: y_view.clone(),
                            uv_view: uv_view.clone(),
                        },
                    );
                }
                (y_view, uv_view)
            };

            av_frame_unref(self.ff.drm_frame);

            // ---- acquire surface texture ----
            let frame = match surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(f) => f,
                wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
                wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                    surface.configure(&self.host.device, config);
                    return Ok(FrameOutcome::Skipped);
                }
                other => {
                    eprintln!("surface acquire: {other:?}");
                    return Ok(FrameOutcome::Skipped);
                }
            };
            let surface_view = frame
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default());

            // ---- grade + blit in one encoder ----
            let grade_bg = self
                .host
                .device
                .create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("grade-bg"),
                    layout: &self.pipelines.grade_bgl,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(&y_view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(&uv_view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: wgpu::BindingResource::TextureView(&self.pipelines.out_view),
                        },
                    ],
                });

            let blit_bg = self
                .host
                .device
                .create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("blit-bg"),
                    layout: &self.pipelines.blit_bgl,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::Sampler(&self.pipelines.blit_sampler),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(&self.pipelines.out_view),
                        },
                    ],
                });

            let mut enc =
                self.host
                    .device
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                        label: Some("frame-encoder"),
                    });
            {
                let mut cp = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                    label: Some("grade-pass"),
                    timestamp_writes: None,
                });
                cp.set_pipeline(&self.pipelines.grade_pipeline);
                cp.set_bind_group(0, &grade_bg, &[]);
                cp.dispatch_workgroups(vw.div_ceil(8), vh.div_ceil(8), 1);
            }
            {
                let mut rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("blit-pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &surface_view,
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
                rp.set_pipeline(&self.pipelines.blit_pipeline);
                rp.set_bind_group(0, &blit_bg, &[]);
                rp.draw(0..3, 0..1);
            }
            self.host.queue.submit([enc.finish()]);
            self.host.queue.present(frame);

            self.host
                .device
                .poll(wgpu::PollType::Wait {
                    submission_index: None,
                    timeout: None,
                })
                .ok();

            drop(grade_bg);
            drop(blit_bg);
        }

        self.frames += 1;
        if self.last_log.elapsed() >= Duration::from_secs(2) {
            let secs = self.last_log.elapsed().as_secs_f64();
            println!(
                "preview {:.1} fps | cache h{} m{} sz{}",
                self.frames as f64 / secs,
                self.cache_hits,
                self.cache_misses,
                self.texture_cache.len(),
            );
            self.frames = 0;
            self.cache_hits = 0;
            self.cache_misses = 0;
            self.last_log = Instant::now();
        }

        Ok(FrameOutcome::Processed)
    }

    pub fn frame_period(&self) -> Duration {
        self.ff.frame_period()
    }
}
