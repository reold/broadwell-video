//! Shared preview pipeline.

use crate::{ffmpeg, gpu, state::SharedState};
use anyhow::Result;
use ffmpeg_sys_next::*;
use std::collections::HashMap;
use std::os::fd::{FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

const MAX_CACHE_ENTRIES: usize = 64;

/// How long one `render_frame` call may spend seeking and decoding before it
/// presents whatever it has reached. The rest of the frame budget belongs to
/// the import, grade and blit.
const CHASE_BUDGET: Duration = Duration::from_millis(6);

/// The loop runs at display rate while a scrub is in flight.
const SCRUB_PERIOD: Duration = Duration::from_millis(16);

/// How long after the last seek the loop keeps running fast, so a brief pause
/// mid-drag does not drop back to frame-rate pacing.
const SCRUB_WINDOW: Duration = Duration::from_millis(400);

#[derive(Clone, Copy, Debug)]
pub enum FrameOutcome {
    Processed,
    Skipped,
    Paused,
    Eof,
}

enum DecodeStep {
    Frame,
    Eof,
}

pub struct PreviewRenderer {
    pub ff: ffmpeg::Handles,
    pub host: grafting::HostWgpuContext,
    pub pipelines: gpu::Pipelines,
    pub texture_cache: HashMap<gpu::CacheKey, gpu::CachedNv12>,
    pub state: SharedState,
    pub cache_hits: u64,
    pub cache_misses: u64,
    /// After a seek, decode frames until PTS >= this value before rendering.
    prune_to_ms: Option<i64>,
    /// PTS of the last frame pulled out of the decoder.
    last_pts: i64,
    /// When the last seek was serviced, for loop pacing.
    last_seek: Option<Instant>,
    /// Counters for the periodic log line.
    pub presents: u64,
    pub seeks: u64,
    pub lag_ms_total: i64,
    pub lag_samples: u64,
}

impl PreviewRenderer {
    pub fn new(
        host: grafting::HostWgpuContext,
        video_path: &str,
        surface_format: wgpu::TextureFormat,
        state: SharedState,
    ) -> Result<Self> {
        let ff = unsafe { ffmpeg::Handles::open(video_path, "/dev/dri/renderD128")? };
        let vw = ff.width;
        let vh = ff.height;
        println!(
            "Video: {vw}x{vh} @ {:.2} fps, duration {} ms",
            ff.fps, ff.duration_ms
        );
        if (ff.sar_num, ff.sar_den) != (1, 1) {
            println!(
                "Non-square pixels: SAR {}/{} -> display aspect {:.4}",
                ff.sar_num,
                ff.sar_den,
                ff.display_aspect()
            );
        }

        {
            let mut s = state.lock().unwrap();
            s.duration_ms = ff.duration_ms;
            s.fps = ff.fps;
        }

        let pipelines = gpu::build_pipelines(&host, surface_format, vw, vh);

        Ok(Self {
            ff,
            host,
            pipelines,
            texture_cache: HashMap::new(),
            state,
            cache_hits: 0,
            cache_misses: 0,
            prune_to_ms: None,
            last_pts: 0,
            last_seek: None,
            presents: 0,
            seeks: 0,
            lag_ms_total: 0,
            lag_samples: 0,
        })
    }

    /// Pull one frame out of the decoder. Reads new packets on demand.
    unsafe fn decode_step(&mut self) -> DecodeStep {
        unsafe {
            loop {
                let rc = avcodec_receive_frame(self.ff.codec_ctx, self.ff.decoded);
                if rc == 0 {
                    return DecodeStep::Frame;
                }
                if rc == -libc::EAGAIN {
                    // Decoder needs more input.
                    let rc = av_read_frame(self.ff.fmt_ctx, self.ff.packet);
                    if rc < 0 {
                        // End of stream: flush and drain.
                        avcodec_send_packet(self.ff.codec_ctx, std::ptr::null());
                        continue;
                    }
                    if (*self.ff.packet).stream_index != self.ff.video_stream {
                        av_packet_unref(self.ff.packet);
                        continue;
                    }
                    let _ = avcodec_send_packet(self.ff.codec_ctx, self.ff.packet);
                    av_packet_unref(self.ff.packet);
                    continue;
                }
                // Any other negative value is AVERROR_EOF or a decode error.
                return DecodeStep::Eof;
            }
        }
    }

    /// Consume the pending seek, if any, and set the prune target.
    fn take_pending_seek(&mut self) -> Option<i64> {
        let mut s = self.state.lock().unwrap();
        if let Some(ms) = s.pending_seek_ms.take() {
            s.position_ms = ms;
            Some(ms)
        } else {
            None
        }
    }

    pub fn render_frame(
        &mut self,
        surface: &wgpu::Surface<'_>,
        config: &wgpu::SurfaceConfiguration,
    ) -> Result<FrameOutcome> {
        let vw = self.ff.width;
        let vh = self.ff.height;

        // ---- Should we do anything this call? ----
        let (playing, had_seek) = {
            let s = self.state.lock().unwrap();
            (s.playing, s.pending_seek_ms.is_some())
        };
        if !playing && !had_seek && self.prune_to_ms.is_none() {
            return Ok(FrameOutcome::Paused);
        }

        // The seek and the decode that follows share one slice of the frame
        // budget; the import, grade and blit get the rest. Whatever the decoder
        // has reached when the budget runs out is what gets presented, so a drag
        // never blocks until a seek lands on its exact frame: the image trails
        // the pointer by a frame or two and catches up over the next iterations.
        // Pruning all the way to the target in a single call is what made each
        // scrub update cost 15-26 ms.
        let deadline = Instant::now() + CHASE_BUDGET;

        // ---- Decode, chasing the newest target within the budget ----
        unsafe {
            loop {
                if let Some(ms) = self.take_pending_seek() {
                    self.ff.seek_to_ms(ms)?;
                    self.prune_to_ms = Some(ms);
                    self.seeks += 1;
                    self.last_seek = Some(Instant::now());
                }

                match self.decode_step() {
                    DecodeStep::Frame => {}
                    DecodeStep::Eof => {
                        if self.prune_to_ms.take().is_some() {
                            // Dragged past the end of the file. Hold the last
                            // frame rather than rewinding under a cursor that is
                            // still moving.
                            self.state.lock().unwrap().position_ms = self.last_pts;
                            return Ok(FrameOutcome::Skipped);
                        }
                        self.ff.rewind();
                        return Ok(FrameOutcome::Eof);
                    }
                }

                let pts = self.ff.current_pts_ms();
                self.last_pts = pts;

                match self.prune_to_ms {
                    Some(target) if pts < target => {
                        if Instant::now() >= deadline {
                            // Short of the target and out of budget: present
                            // this frame and carry on from here next call.
                            // position_ms stays at the target, so the playhead
                            // shows the position that was asked for.
                            break;
                        }
                        av_frame_unref(self.ff.decoded);
                        continue;
                    }
                    Some(_) => self.prune_to_ms = None,
                    None => {}
                }

                // This is the frame we render.
                self.state.lock().unwrap().position_ms = pts;
                break;
            }

            // How far the frame being presented is from the position the user
            // asked for. Only sampled while a target is outstanding, so it
            // measures the scrub rather than diluting into playback.
            if let Some(target) = self.prune_to_ms {
                self.lag_ms_total += (target - self.last_pts).abs();
                self.lag_samples += 1;
            }

            // ---- Import the decoded NV12 frame ----
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

            // ---- grade + blit ----
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

            // ---- letterbox for the current surface size ----
            // display_aspect() folds in non-square pixels, so anamorphic
            // footage gets its bars in the right place.
            self.pipelines.set_letterbox(
                &self.host.queue,
                self.ff.display_aspect(),
                config.width,
                config.height,
            );

            let blit_bg = self
                .pipelines
                .blit_bind_group(&self.host.device, &self.pipelines.out_view);

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
            self.presents += 1;

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

        Ok(FrameOutcome::Processed)
    }

    pub fn frame_period(&self) -> Duration {
        self.ff.frame_period()
    }

    /// Loop period for the caller's render loop.
    pub fn loop_period(&self) -> Duration {
        let playing = self.state.lock().unwrap().playing;
        let since_seek = self.last_seek.map(|t| t.elapsed());
        loop_period_for(
            self.ff.frame_period(),
            wants_display_rate(self.prune_to_ms.is_some(), playing, since_seek),
        )
    }
}

/// Whether the loop should run at display rate instead of the video frame rate.
///
/// An outstanding chase always wants display rate. The recent-seek window only
/// applies while paused: during playback, pacing has to return to the video's
/// frame rate the moment a chase lands, or the rest of the window would
/// fast-forward the film at display rate.
fn wants_display_rate(
    target_outstanding: bool,
    playing: bool,
    since_seek: Option<Duration>,
) -> bool {
    if target_outstanding {
        return true;
    }
    if playing {
        return false;
    }
    since_seek.is_some_and(|elapsed| elapsed < SCRUB_WINDOW)
}

/// Frame-rate pacing is right for playback and wrong for scrubbing: sleeping to
/// the video's frame period capped scrub updates at the video frame rate, which
/// is what made a 29.97 fps clip feel like single-digit updates per second.
fn loop_period_for(frame_period: Duration, display_rate: bool) -> Duration {
    if display_rate {
        frame_period.min(SCRUB_PERIOD)
    } else {
        frame_period
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME_30: Duration = Duration::from_millis(33);

    #[test]
    fn scrub_pacing_does_not_inherit_the_video_frame_rate() {
        assert_eq!(loop_period_for(FRAME_30, true), SCRUB_PERIOD);
        // A faster-than-display source keeps its own period.
        assert_eq!(
            loop_period_for(Duration::from_millis(8), true),
            Duration::from_millis(8)
        );
    }

    #[test]
    fn playback_pacing_follows_the_video() {
        assert_eq!(loop_period_for(FRAME_30, false), FRAME_30);
    }

    #[test]
    fn an_outstanding_chase_always_runs_at_display_rate() {
        assert!(wants_display_rate(true, false, None));
        assert!(wants_display_rate(true, true, None));
    }

    #[test]
    fn a_recent_seek_keeps_a_paused_scrub_responsive() {
        assert!(wants_display_rate(
            false,
            false,
            Some(Duration::from_millis(50))
        ));
        // But not indefinitely.
        assert!(!wants_display_rate(
            false,
            false,
            Some(SCRUB_WINDOW + Duration::from_millis(1))
        ));
    }

    #[test]
    fn a_landed_chase_does_not_leave_playback_running_fast() {
        // Otherwise the loop would present 60 frames per second of a 30 fps
        // film for the rest of the window after every seek.
        assert!(!wants_display_rate(
            false,
            true,
            Some(Duration::from_millis(10))
        ));
    }
}
