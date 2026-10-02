use anyhow::{Result, bail};
use ffmpeg_sys_next::*;
use hwa_core::export::{Exporter, PixFmt};
use hwa_core::ffmpeg;
use hwa_core::gpu::{CacheKey, CachedNv12, Pipelines};
use std::collections::HashMap;
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::Arc;
use std::time::{Duration, Instant};
use winit::window::Window;

const MAX_CACHE_ENTRIES: usize = 64;

#[derive(Clone, Copy)]
enum GradeKind {
    Rgba,
    Nv12,
}

enum FrameOutcome {
    Processed,
    Skipped,
    Eof,
}

pub struct App {
    pub window: Arc<Window>,
    pub surface: wgpu::Surface<'static>,
    pub surface_config: wgpu::SurfaceConfiguration,
    pub host: grafting::HostWgpuContext,
    pub pipelines: Pipelines,
    pub ff: ffmpeg::Handles,
    pub texture_cache: HashMap<CacheKey, CachedNv12>,
    pub readback_y_buffer: wgpu::Buffer,
    pub readback_uv_buffer: wgpu::Buffer,
    pub frames: u64,
    pub last_log: Instant,
    pub timing_decode: Duration,
    pub timing_filter: Duration,
    pub timing_import: Duration,
    pub timing_gpu: Duration,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub last_report_elapsed: f64,
    /// Diagnostic: number of no-op effect passes to chain in preview.
    pub effect_passes: usize,
}

impl App {
    fn process_frame(&mut self, kind: GradeKind) -> Result<FrameOutcome> {
        let vw = self.ff.width;
        let vh = self.ff.height;

        unsafe {
            // ---- decode ----
            let t = Instant::now();
            let rc = av_read_frame(self.ff.fmt_ctx, self.ff.packet);
            if rc < 0 {
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
                bail!("avcodec_receive_frame rc={rc}");
            }
            self.timing_decode += t.elapsed();

            // ---- map decoded NV12 directly to DRM_PRIME ----
            let t = Instant::now();
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
            if desc.is_null() {
                av_frame_unref(self.ff.drm_frame);
                bail!("null DRM descriptor");
            }
            if (*desc).nb_layers < 2 {
                let n = (*desc).nb_layers;
                av_frame_unref(self.ff.drm_frame);
                bail!("expected NV12 (2 layers), got {n}");
            }

            let y_layer = &(*desc).layers[0];
            let uv_layer = &(*desc).layers[1];
            let y_plane = &y_layer.planes[0];
            let uv_plane = &uv_layer.planes[0];
            let y_obj = &(*desc).objects[y_plane.object_index as usize];
            let uv_obj = &(*desc).objects[uv_plane.object_index as usize];

            let key: CacheKey = {
                let mut stat: libc::stat = std::mem::zeroed();
                if libc::fstat(y_obj.fd, &mut stat) != 0 {
                    av_frame_unref(self.ff.drm_frame);
                    bail!("fstat failed: {}", std::io::Error::last_os_error());
                }
                CacheKey {
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
                        bail!("dup(y) failed: {}", std::io::Error::last_os_error());
                    }
                    OwnedFd::from_raw_fd(raw)
                };
                let uv_fd: OwnedFd = {
                    let raw = libc::dup(uv_obj.fd);
                    if raw < 0 {
                        av_frame_unref(self.ff.drm_frame);
                        bail!("dup(uv) failed: {}", std::io::Error::last_os_error());
                    }
                    OwnedFd::from_raw_fd(raw)
                };

                let y_import = grafting::vulkan_dmabuf::VulkanDmaBufImport::new(
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
                )
                .map_err(|e| {
                    av_frame_unref(self.ff.drm_frame);
                    anyhow::anyhow!("VulkanDmaBufImport(Y): {e:?}")
                })?;

                let y_texture = grafting::vulkan_dmabuf::import_dmabuf(y_import, &self.host)
                    .map_err(|e| {
                        av_frame_unref(self.ff.drm_frame);
                        anyhow::anyhow!("import_dmabuf(Y): {e:?}")
                    })?;

                let uv_import = grafting::vulkan_dmabuf::VulkanDmaBufImport::new(
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
                )
                .map_err(|e| {
                    av_frame_unref(self.ff.drm_frame);
                    anyhow::anyhow!("VulkanDmaBufImport(UV): {e:?}")
                })?;

                let uv_texture = grafting::vulkan_dmabuf::import_dmabuf(uv_import, &self.host)
                    .map_err(|e| {
                        av_frame_unref(self.ff.drm_frame);
                        anyhow::anyhow!("import_dmabuf(UV): {e:?}")
                    })?;

                let y_view = y_texture.create_view(&wgpu::TextureViewDescriptor::default());
                let uv_view = uv_texture.create_view(&wgpu::TextureViewDescriptor::default());

                let y_view_clone = y_view.clone();
                let uv_view_clone = uv_view.clone();
                if self.texture_cache.len() < MAX_CACHE_ENTRIES {
                    self.texture_cache.insert(
                        key,
                        CachedNv12 {
                            y_texture,
                            uv_texture,
                            y_view: y_view_clone,
                            uv_view: uv_view_clone,
                        },
                    );
                }
                (y_view, uv_view)
            };

            av_frame_unref(self.ff.drm_frame);
            self.timing_import += t.elapsed();

            // ---- compute dispatch ----
            let t = Instant::now();
            let mut enc =
                self.host
                    .device
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                        label: Some("grade-encoder"),
                    });

            match kind {
                GradeKind::Rgba => {
                    let bg = self
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
                                    resource: wgpu::BindingResource::TextureView(
                                        &self.pipelines.out_view,
                                    ),
                                },
                                wgpu::BindGroupEntry {
                                    binding: 3,
                                    resource: self.pipelines.grade_uniform.as_entire_binding(),
                                },
                            ],
                        });
                    {
                        let mut cp = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                            label: Some("grade-pass"),
                            timestamp_writes: None,
                        });
                        cp.set_pipeline(&self.pipelines.grade_pipeline);
                        cp.set_bind_group(0, &bg, &[]);
                        cp.dispatch_workgroups(vw.div_ceil(8), vh.div_ceil(8), 1);
                    }
                }
                GradeKind::Nv12 => {
                    let uv_w = vw / 2;
                    let uv_h = vh / 2;
                    let bg_y = self
                        .host
                        .device
                        .create_bind_group(&wgpu::BindGroupDescriptor {
                            label: Some("grade-y-bg"),
                            layout: &self.pipelines.grade_y_bgl,
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
                                    resource: wgpu::BindingResource::TextureView(
                                        &self.pipelines.out_y_view,
                                    ),
                                },
                            ],
                        });
                    let bg_uv = self
                        .host
                        .device
                        .create_bind_group(&wgpu::BindGroupDescriptor {
                            label: Some("grade-uv-bg"),
                            layout: &self.pipelines.grade_uv_bgl,
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
                                    resource: wgpu::BindingResource::TextureView(
                                        &self.pipelines.out_uv_view,
                                    ),
                                },
                            ],
                        });
                    {
                        let mut cp = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                            label: Some("grade-y-pass"),
                            timestamp_writes: None,
                        });
                        cp.set_pipeline(&self.pipelines.grade_y_pipeline);
                        cp.set_bind_group(0, &bg_y, &[]);
                        cp.dispatch_workgroups(vw.div_ceil(8), vh.div_ceil(8), 1);
                    }
                    {
                        let mut cp = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                            label: Some("grade-uv-pass"),
                            timestamp_writes: None,
                        });
                        cp.set_pipeline(&self.pipelines.grade_uv_pipeline);
                        cp.set_bind_group(0, &bg_uv, &[]);
                        cp.dispatch_workgroups(uv_w.div_ceil(8), uv_h.div_ceil(8), 1);
                    }
                }
            }

            self.host.queue.submit([enc.finish()]);
            self.timing_gpu += t.elapsed();
        }

        Ok(FrameOutcome::Processed)
    }

    pub fn render_one_frame(&mut self) -> Result<()> {
        let frame_start = Instant::now();

        match self.process_frame(GradeKind::Rgba)? {
            FrameOutcome::Eof => {
                unsafe { self.ff.rewind() };
                self.frames = 0;
                return Ok(());
            }
            FrameOutcome::Skipped => return Ok(()),
            FrameOutcome::Processed => {}
        }

        let t = Instant::now();
        {
            let surface_tex = match self.surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(t)
                | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
                other => {
                    eprintln!("surface acquire: {other:?}, reconfiguring");
                    self.surface
                        .configure(&self.host.device, &self.surface_config);
                    return Ok(());
                }
            };
            let surface_view = surface_tex
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default());

            let mut enc =
                self.host
                    .device
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                        label: Some("blit-encoder"),
                    });

            // Diagnostic: chain N no-op effect passes through ping-pong textures.
            // When effect_passes == 0, blit directly from the graded output.
            let blit_source_view = if self.effect_passes == 0 {
                self.pipelines.out_view.clone()
            } else {
                let vw = self.ff.width;
                let vh = self.ff.height;
                let mut prev_view = self.pipelines.out_view.clone();
                let mut last_is_a = true;
                for i in 0..self.effect_passes {
                    let (dst_view, is_a) = if i % 2 == 0 {
                        (self.pipelines.ping_a_view.clone(), true)
                    } else {
                        (self.pipelines.ping_b_view.clone(), false)
                    };
                    let bg = self
                        .host
                        .device
                        .create_bind_group(&wgpu::BindGroupDescriptor {
                            label: Some("noop-bg"),
                            layout: &self.pipelines.noop_bgl,
                            entries: &[
                                wgpu::BindGroupEntry {
                                    binding: 0,
                                    resource: wgpu::BindingResource::TextureView(&prev_view),
                                },
                                wgpu::BindGroupEntry {
                                    binding: 1,
                                    resource: wgpu::BindingResource::TextureView(&dst_view),
                                },
                            ],
                        });
                    {
                        let mut cp = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                            label: Some("noop-pass"),
                            timestamp_writes: None,
                        });
                        cp.set_pipeline(&self.pipelines.noop_pipeline);
                        cp.set_bind_group(0, &bg, &[]);
                        cp.dispatch_workgroups(vw.div_ceil(8), vh.div_ceil(8), 1);
                    }
                    prev_view = dst_view;
                    last_is_a = is_a;
                }
                if last_is_a {
                    self.pipelines.ping_a_view.clone()
                } else {
                    self.pipelines.ping_b_view.clone()
                }
            };

            // Blit from whichever texture holds the final result.
            // Letterbox against this window's current size.
            self.pipelines.set_letterbox(
                &self.host.queue,
                self.ff.display_aspect(),
                self.surface_config.width,
                self.surface_config.height,
            );

            let blit_bg = self
                .pipelines
                .blit_bind_group(&self.host.device, &blit_source_view);

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
            self.host.queue.present(surface_tex);

            self.host
                .device
                .poll(wgpu::PollType::Wait {
                    submission_index: None,
                    timeout: None,
                })
                .ok();
        }
        self.timing_gpu += t.elapsed();

        self.frames += 1;
        if self.last_log.elapsed() >= Duration::from_secs(2) {
            self.print_stats();
        }

        let elapsed = frame_start.elapsed();
        let period = self.ff.frame_period();
        if elapsed < period {
            std::thread::sleep(period - elapsed);
        }

        Ok(())
    }

    pub fn export(&mut self, output: &str, max_frames: usize) -> Result<u64> {
        let mut exporter = Exporter::new(
            self.ff.width,
            self.ff.height,
            self.ff.fps,
            output,
            PixFmt::Nv12,
        )?;

        let mut written: u64 = 0;
        let export_start = Instant::now();
        let mut t_process = Duration::ZERO;
        let mut t_readback = Duration::ZERO;
        let mut t_pipe = Duration::ZERO;
        self.last_report_elapsed = 0.0;

        loop {
            if written as usize >= max_frames {
                break;
            }

            let t = Instant::now();
            match self.process_frame(GradeKind::Nv12)? {
                FrameOutcome::Eof => break,
                FrameOutcome::Skipped => continue,
                FrameOutcome::Processed => {}
            }
            t_process += t.elapsed();

            let t = Instant::now();
            let nv12 = self.readback_nv12()?;
            t_readback += t.elapsed();

            let t = Instant::now();
            exporter.write_frame(&nv12)?;
            t_pipe += t.elapsed();

            written += 1;

            if written % 30 == 0 {
                let elapsed = export_start.elapsed().as_secs_f64();
                let window_secs = elapsed - self.last_report_elapsed;
                let fps_window = 30.0 / window_secs;
                println!(
                    "  {} frames ({:.1} fps overall, {:.1} fps window) | proc {:.2} rb {:.2} pipe {:.2} ms | cache h{} m{} sz{}",
                    written,
                    written as f64 / elapsed,
                    fps_window,
                    t_process.as_secs_f64() * 1000.0 / 30.0,
                    t_readback.as_secs_f64() * 1000.0 / 30.0,
                    t_pipe.as_secs_f64() * 1000.0 / 30.0,
                    self.cache_hits,
                    self.cache_misses,
                    self.texture_cache.len(),
                );
                t_process = Duration::ZERO;
                t_readback = Duration::ZERO;
                t_pipe = Duration::ZERO;
                self.cache_hits = 0;
                self.cache_misses = 0;
                self.last_report_elapsed = elapsed;
            }
        }

        let (total, clean_exit) = exporter.finish()?;
        let elapsed = export_start.elapsed().as_secs_f64();
        println!(
            "  done: {} frames in {:.1}s ({:.1} fps){}",
            total,
            elapsed,
            total as f64 / elapsed,
            if clean_exit {
                ""
            } else {
                " (ffmpeg exited non-zero after writing the file)"
            }
        );
        Ok(total)
    }

    /// Read the Y and UV planes into a single tightly-packed NV12 frame.
    /// The export textures are R32Uint with 4 Y values per texel (or 2 UV
    /// pairs per texel), so the readback bytes are already in NV12 layout.
    fn readback_nv12(&mut self) -> Result<Vec<u8>> {
        let vw = self.ff.width;
        let vh = self.ff.height;
        let uv_w = vw / 2;
        let uv_h = vh / 2;

        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let y_row_bytes = vw;
        let y_src_padded = y_row_bytes.div_ceil(align) * align;
        let uv_row_bytes = uv_w * 2;
        let uv_src_padded = uv_row_bytes.div_ceil(align) * align;

        let mut enc = self
            .host
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("nv12-readback-encoder"),
            });
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.pipelines.out_y_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &self.readback_y_buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(y_src_padded),
                    rows_per_image: Some(vh),
                },
            },
            wgpu::Extent3d {
                width: vw / 4,
                height: vh,
                depth_or_array_layers: 1,
            },
        );
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.pipelines.out_uv_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &self.readback_uv_buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(uv_src_padded),
                    rows_per_image: Some(uv_h),
                },
            },
            wgpu::Extent3d {
                width: uv_w / 2,
                height: uv_h,
                depth_or_array_layers: 1,
            },
        );
        self.host.queue.submit([enc.finish()]);
        self.host
            .device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .ok();

        let y_slice = self.readback_y_buffer.slice(..);
        let uv_slice = self.readback_uv_buffer.slice(..);
        let (y_tx, y_rx) = std::sync::mpsc::channel();
        let (uv_tx, uv_rx) = std::sync::mpsc::channel();
        y_slice.map_async(wgpu::MapMode::Read, move |r| y_tx.send(r).unwrap());
        uv_slice.map_async(wgpu::MapMode::Read, move |r| uv_tx.send(r).unwrap());
        self.host
            .device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .ok();
        y_rx.recv().unwrap().expect("y map failed");
        uv_rx.recv().unwrap().expect("uv map failed");

        let mut out = Vec::with_capacity((vw * vh + uv_w * uv_h * 2) as usize);
        {
            let data = y_slice.get_mapped_range().expect("y range");
            for row in 0..vh {
                let start = (row * y_src_padded) as usize;
                out.extend_from_slice(&data[start..start + y_row_bytes as usize]);
            }
        }
        self.readback_y_buffer.unmap();
        {
            let data = uv_slice.get_mapped_range().expect("uv range");
            for row in 0..uv_h {
                let start = (row * uv_src_padded) as usize;
                out.extend_from_slice(&data[start..start + uv_row_bytes as usize]);
            }
        }
        self.readback_uv_buffer.unmap();

        Ok(out)
    }

    fn print_stats(&mut self) {
        let secs = self.last_log.elapsed().as_secs_f64();
        let n = self.frames as f64;
        let pipeline_ms =
            (self.timing_decode + self.timing_filter + self.timing_import + self.timing_gpu)
                .as_secs_f64()
                * 1000.0
                / n;
        println!(
            "fps {:.1} | dec {:.2} imp {:.2} gpu {:.2} | pipe {:.2}ms = {:.0}fps | passes {} | cache h{} m{} sz{}",
            n / secs,
            self.timing_decode.as_secs_f64() * 1000.0 / n,
            self.timing_import.as_secs_f64() * 1000.0 / n,
            self.timing_gpu.as_secs_f64() * 1000.0 / n,
            pipeline_ms,
            1000.0 / pipeline_ms,
            self.effect_passes,
            self.cache_hits,
            self.cache_misses,
            self.texture_cache.len(),
        );
        self.frames = 0;
        self.last_log = Instant::now();
        self.timing_decode = Duration::ZERO;
        self.timing_filter = Duration::ZERO;
        self.timing_import = Duration::ZERO;
        self.timing_gpu = Duration::ZERO;
        self.cache_hits = 0;
        self.cache_misses = 0;
    }
}
