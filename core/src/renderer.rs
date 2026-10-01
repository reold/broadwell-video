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

/// The loop runs at display rate while a scrub is in flight. Deliberately a
/// touch faster than the panel's 60 Hz refresh: presenting slightly ahead of
/// the display means a slow iteration drops into the next vblank rather than
/// missing one and holding the same frame for two.
const SCRUB_PERIOD: Duration = Duration::from_millis(16);

/// Decode forward to the target rather than seeking while it is within this
/// distance.
///
/// Seeking is far more expensive than the decode it saves: a seek flushes the
/// decoder, so the next `av_hwframe_map` has to re-sync a freshly restarted
/// VA-API pipeline, and the log shows import time tracking seeks/s (9 ms at 2
/// seeks/s, 35 ms at 19). A 500 ms threshold kept tripping on its own lag,
/// because a decoder 400 ms behind a target that moved 150 ms reads as a
/// 550 ms jump: seek, slower map, more lag, seek again. Decoding 1.5 s of video
/// costs about 14 ms, which is what a seek costs anyway.
const SEEK_AHEAD_MS: i64 = 1_500;

/// Recent frames kept graded on the GPU so a scrub can be served without asking
/// the decoder for anything.
///
/// Video cannot be decoded backwards, so every backward drag update used to
/// seek, and a map that follows a seek costs 42-66 ms against 5.5-6.9 ms for a
/// warm one. Caching what has already been decoded turns a backward drag into a
/// lookup, which is the only way presentation keeps up at display rate.
const RING_BUDGET_BYTES: u64 = 192 * 1024 * 1024;
const RING_MIN_SLOTS: usize = 8;
const RING_MAX_SLOTS: usize = 48;

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

/// One graded frame held ready to present.
struct RingSlot {
    #[allow(dead_code)]
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    pts_ms: i64,
    valid: bool,
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
    /// PTS of the frame currently on screen.
    last_presented_pts: Option<i64>,
    /// Target the last presented frame was chasing, so a legitimate backwards
    /// drag can be told apart from a picture that regressed against a forward
    /// one.
    last_target_ms: Option<i64>,
    /// Counters and per-phase timings for the periodic log line.
    pub presents: u64,
    pub seeks: u64,
    pub rewinds: u64,
    pub lag_ms_total: i64,
    pub lag_samples: u64,
    pub chase_time: Duration,
    pub map_time: Duration,
    /// Map cost split by whether this call seeked first. Decides whether the
    /// export is expensive because the decoder pipeline was just flushed, or
    /// because a chase filled it with a burst of frames.
    pub map_seek_time: Duration,
    pub map_seek_count: u64,
    pub map_warm_time: Duration,
    pub map_warm_count: u64,
    pub import_time: Duration,
    pub present_time: Duration,
    /// Recently decoded frames, graded and ready to present.
    ring: Vec<RingSlot>,
    ring_next: usize,
    /// Presents served from the ring, and frames decoded into it.
    pub ring_presents: u64,
    pub ring_decodes: u64,
    /// Playback state last seen, so the decoder can be re-anchored when
    /// scrubbing served from the ring has left it somewhere else.
    was_playing: bool,
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

        // Graded frames kept resident for scrubbing, sized by budget so a 1080p
        // clip does not hold half a gigabyte of them.
        let frame_bytes = (vw as u64).max(1) * (vh as u64).max(1) * 4;
        let ring = (0..ring_capacity(frame_bytes))
            .map(|_| {
                let texture = host.device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("ring-slot"),
                    size: wgpu::Extent3d {
                        width: vw.max(1),
                        height: vh.max(1),
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    usage: wgpu::TextureUsages::STORAGE_BINDING
                        | wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                });
                let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
                RingSlot {
                    texture,
                    view,
                    pts_ms: 0,
                    valid: false,
                }
            })
            .collect();

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
            last_presented_pts: None,
            last_target_ms: None,
            presents: 0,
            seeks: 0,
            rewinds: 0,
            lag_ms_total: 0,
            lag_samples: 0,
            chase_time: Duration::ZERO,
            map_time: Duration::ZERO,
            map_seek_time: Duration::ZERO,
            map_seek_count: 0,
            map_warm_time: Duration::ZERO,
            map_warm_count: 0,
            import_time: Duration::ZERO,
            present_time: Duration::ZERO,
            ring,
            ring_next: 0,
            ring_presents: 0,
            ring_decodes: 0,
            was_playing: true,
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

        // ---- A resident frame may already cover this target ----
        //
        // This is what makes a scrub track the pointer. Video cannot be
        // decoded backwards, so every backward drag update otherwise seeks, and
        // a map that follows a seek costs 42-66 ms against 5.5-6.9 ms warm.
        // Serving the update from the ring needs neither a seek nor a map.
        //
        // Only while paused: during playback a seek has to move the decoder
        // itself, not just the picture.
        let mut seeked_this_call = false;
        if !playing {
            let pending = self.state.lock().unwrap().pending_seek_ms;
            if let Some(target) = pending {
                if let Some(slot) = self.cached_slot_for(target) {
                    {
                        let mut s = self.state.lock().unwrap();
                        s.pending_seek_ms = None;
                        s.position_ms = target;
                    }
                    self.last_seek = Some(Instant::now());
                    let resident_pts = self.ring[slot].pts_ms;
                    self.lag_ms_total += (resident_pts - target).abs();
                    self.lag_samples += 1;
                    if self.last_presented_pts != Some(resident_pts) {
                        self.present_resident(slot, target, surface, config)?;
                    }
                    return Ok(FrameOutcome::Processed);
                }
            }
        }

        // Scrubbing served from the ring leaves the decoder wherever it was, so
        // re-anchor it when playback resumes.
        if playing && !self.was_playing {
            let resume_ms = self.state.lock().unwrap().position_ms;
            unsafe {
                self.ff.seek_to_ms(resume_ms)?;
            }
            // The seek lands on a keyframe, so chase forward to the playhead
            // rather than presenting the keyframe.
            self.prune_to_ms = Some(resume_ms);
            self.seeks += 1;
            seeked_this_call = true;
        }
        self.was_playing = playing;

        // The seek and the decode that follows share one slice of the frame
        // budget; the import, grade and blit get the rest. Whatever the decoder
        // has reached when the budget runs out is what gets presented, so a drag
        // never blocks until a seek lands on its exact frame: the image trails
        // the pointer by a frame or two and catches up over the next iterations.
        // Pruning all the way to the target in a single call is what made each
        // scrub update cost 15-26 ms.
        let deadline = Instant::now() + CHASE_BUDGET;
        let t_chase = Instant::now();

        // ---- Decode, chasing the newest target within the budget ----
        unsafe {
            loop {
                if let Some(ms) = self.take_pending_seek() {
                    self.last_seek = Some(Instant::now());
                    // A backward seek lands on the keyframe at or before the
                    // target and then decodes forward, so re-seeking on every
                    // drag update can present a frame *earlier* than the one
                    // already on screen: the picture jumps back by up to a
                    // whole GOP and then crawls forward again. That is the
                    // visible stutter. Only seek when the target is behind the
                    // decoder, or far enough ahead that decoding the gap would
                    // cost more than the seek and its keyframe jump.
                    if needs_seek(self.last_pts, ms) {
                        self.ff.seek_to_ms(ms)?;
                        self.seeks += 1;
                        seeked_this_call = true;
                    }
                    self.prune_to_ms = Some(ms);
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
            self.chase_time += t_chase.elapsed();

            // ---- Import the decoded NV12 frame ----
            let t_import = Instant::now();
            (*self.ff.drm_frame).format = AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
            let t_map = Instant::now();
            ffmpeg::check(
                av_hwframe_map(
                    self.ff.drm_frame,
                    self.ff.decoded,
                    AV_HWFRAME_MAP_READ as i32,
                ),
                "av_hwframe_map",
            )?;
            // Timed separately: this is the call that has to sync the video
            // engine, and it is the entire cost of the import phase.
            let map_elapsed = t_map.elapsed();
            self.map_time += map_elapsed;
            if seeked_this_call {
                self.map_seek_time += map_elapsed;
                self.map_seek_count += 1;
            } else {
                self.map_warm_time += map_elapsed;
                self.map_warm_count += 1;
            }
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
            self.import_time += t_import.elapsed();

            // ---- grade into a resident slot and present it ----
            // Grading into the ring, rather than a scratch texture, is what
            // lets this frame be shown again later without going back to the
            // decoder for it.
            let slot = self.next_ring_slot();
            let target = self.prune_to_ms.unwrap_or(self.last_pts);
            let slot_view = self.ring[slot].view.clone();
            self.present_view(
                &slot_view,
                Some((&y_view, &uv_view)),
                self.last_pts,
                target,
                surface,
                config,
            )?;
            self.ring[slot].pts_ms = self.last_pts;
            self.ring[slot].valid = true;
            self.ring_decodes += 1;
        }

        Ok(FrameOutcome::Processed)
    }

    /// Next ring slot to write, round-robin.
    fn next_ring_slot(&mut self) -> usize {
        let slot = self.ring_next;
        self.ring_next = (self.ring_next + 1) % self.ring.len().max(1);
        slot
    }

    /// Resident frame to show for `target`, if one is close enough that showing
    /// it beats going back to the decoder for the exact one.
    fn cached_slot_for(&self, target: i64) -> Option<usize> {
        let frame_ms = self.ff.frame_period().as_millis() as i64;
        let tolerance = cache_tolerance_ms(frame_ms, self.last_target_ms == Some(target));
        let resident: Vec<(i64, bool)> =
            self.ring.iter().map(|s| (s.pts_ms, s.valid)).collect();
        nearest_resident(&resident, target)
            .filter(|(_, delta)| delta.abs() <= tolerance)
            .map(|(slot, _)| slot)
    }

    /// Present a frame already resident in the ring, with no decoder work at
    /// all: no seek, no decode, no map.
    fn present_resident(
        &mut self,
        slot: usize,
        target_ms: i64,
        surface: &wgpu::Surface<'_>,
        config: &wgpu::SurfaceConfiguration,
    ) -> Result<()> {
        let pts_ms = self.ring[slot].pts_ms;
        let view = self.ring[slot].view.clone();
        self.ring_presents += 1;
        self.present_view(&view, None, pts_ms, target_ms, surface, config)
    }

    /// Grade `planes` into `source` when they are given, then blit `source` to
    /// the surface and present it.
    fn present_view(
        &mut self,
        source: &wgpu::TextureView,
        planes: Option<(&wgpu::TextureView, &wgpu::TextureView)>,
        pts_ms: i64,
        target_ms: i64,
        surface: &wgpu::Surface<'_>,
        config: &wgpu::SurfaceConfiguration,
    ) -> Result<()> {
        let t_present = Instant::now();
        let vw = self.ff.width;
        let vh = self.ff.height;

        let frame = match surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) => f,
            wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                surface.configure(&self.host.device, config);
                return Ok(());
            }
            other => {
                eprintln!("surface acquire: {other:?}");
                return Ok(());
            }
        };
        let surface_view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut enc =
            self.host
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("frame-encoder"),
                });

        if let Some((y_view, uv_view)) = planes {
            let grade_bg = self
                .host
                .device
                .create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("grade-bg"),
                    layout: &self.pipelines.grade_bgl,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(y_view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(uv_view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: wgpu::BindingResource::TextureView(source),
                        },
                    ],
                });
            let mut cp = enc.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("grade-pass"),
                timestamp_writes: None,
            });
            cp.set_pipeline(&self.pipelines.grade_pipeline);
            cp.set_bind_group(0, &grade_bg, &[]);
            cp.dispatch_workgroups(vw.div_ceil(8), vh.div_ceil(8), 1);
        }

        // display_aspect() folds in non-square pixels, so anamorphic footage
        // gets its bars in the right place.
        self.pipelines.set_letterbox(
            &self.host.queue,
            self.ff.display_aspect(),
            config.width,
            config.height,
        );

        let blit_bg = self.pipelines.blit_bind_group(&self.host.device, source);
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

        // A frame shown earlier than the one before it is only a defect when
        // the target itself moved forward; dragging backwards is supposed to
        // rewind. This counts regressions, not rewinds.
        if let (Some(prev_pts), Some(prev_target)) =
            (self.last_presented_pts, self.last_target_ms)
        {
            if pts_ms < prev_pts && target_ms > prev_target {
                self.rewinds += 1;
            }
        }
        self.last_presented_pts = Some(pts_ms);
        self.last_target_ms = Some(target_ms);

        self.host
            .device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .ok();
        // Includes the blocking poll, which is where a frame's latency actually
        // goes if the CPU cannot run ahead of the GPU.
        self.present_time += t_present.elapsed();

        Ok(())
    }

    pub fn frame_period(&self) -> Duration {
        self.ff.frame_period()
    }

    /// Loop period for the caller's render loop.
    pub fn loop_period(&self) -> Duration {
        loop_period_for(self.ff.frame_period(), self.is_scrubbing())
    }

    /// True while a target is outstanding or one arrived recently.
    ///
    /// The UI drives its own playhead during a drag, so the render loop uses
    /// this to skip event emission that would otherwise block on the webview.
    pub fn is_scrubbing(&self) -> bool {
        let playing = self.state.lock().unwrap().playing;
        let since_seek = self.last_seek.map(|t| t.elapsed());
        wants_display_rate(self.prune_to_ms.is_some(), playing, since_seek)
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

/// Whether chasing `target_ms` from `current_ms` needs a seek, or whether the
/// decoder can simply carry on toward it.
///
/// Decoding forward is both cheaper and smoother than seeking for short
/// distances: a seek resolves to the keyframe *before* the target and then
/// decodes forward again, so seeking on every drag update can present a frame
/// earlier than the one already on screen and make the picture jump backwards.
/// That jump is the stutter this avoids.
fn needs_seek(current_ms: i64, target_ms: i64) -> bool {
    let delta = target_ms - current_ms;
    delta < 0 || delta > SEEK_AHEAD_MS
}

/// How many graded frames to keep resident for a given frame size.
fn ring_capacity(frame_bytes: u64) -> usize {
    ((RING_BUDGET_BYTES / frame_bytes.max(1)) as usize).clamp(RING_MIN_SLOTS, RING_MAX_SLOTS)
}

/// Nearest resident frame to `target` as (index, signed delta), ignoring slots
/// that hold nothing.
fn nearest_resident(resident: &[(i64, bool)], target: i64) -> Option<(usize, i64)> {
    resident
        .iter()
        .enumerate()
        .filter(|(_, (_, valid))| *valid)
        .map(|(slot, (pts, _))| (slot, *pts - target))
        .min_by_key(|(_, delta)| delta.abs())
}

/// How far a resident frame may be from the target and still be the frame to
/// show, in milliseconds.
///
/// While the target is moving, a frame or two of slack is far better than
/// missing the ring and paying a 25-44 ms map: the ring only holds frames that
/// were stopped on, and a drag moves further between presents than half a frame
/// of video, so a tight window misses on almost every update. Once the target
/// settles the window tightens to half a frame, which forces a decode of the
/// exact frame, so the picture the user comes to rest on is correct.
fn cache_tolerance_ms(frame_ms: i64, settled: bool) -> i64 {
    let frame_ms = frame_ms.max(1);
    if settled {
        (frame_ms / 2).max(1)
    } else {
        frame_ms * 2
    }
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

    #[test]
    fn short_forward_targets_decode_instead_of_seeking() {
        // The anti-stutter rule: a drag that nudges forward must not re-seek,
        // because the seek resolves to an earlier keyframe and the picture
        // jumps backwards.
        assert!(!needs_seek(5_000, 5_100));
        assert!(!needs_seek(5_000, 5_000 + SEEK_AHEAD_MS));
    }

    #[test]
    fn far_or_backward_targets_still_seek() {
        // Backwards: there is no decoding backwards.
        assert!(needs_seek(5_000, 4_900));
        assert!(needs_seek(5_000, 0));
        // Forwards past the point where decoding the gap beats seeking.
        assert!(needs_seek(5_000, 5_000 + SEEK_AHEAD_MS + 1));
        assert!(needs_seek(0, 30_000));
    }

    #[test]
    fn the_ring_picks_the_nearest_resident_frame() {
        let resident = [(0, true), (33, true), (66, false), (99, true)];
        assert_eq!(nearest_resident(&resident, 30), Some((1, 3)));
        assert_eq!(nearest_resident(&resident, 96), Some((3, 3)));
        // Invalid slots are never chosen, even when they are closest.
        assert_eq!(nearest_resident(&resident, 66), Some((1, -33)));
        assert_eq!(nearest_resident(&[(0, false)], 0), None);
        assert_eq!(nearest_resident(&[], 0), None);
    }

    #[test]
    fn a_moving_target_tolerates_more_drift_than_a_settled_one() {
        // 30 fps: two frames of slack while dragging, half a frame at rest.
        assert_eq!(cache_tolerance_ms(33, false), 66);
        assert_eq!(cache_tolerance_ms(33, true), 16);
        // Degenerate frame periods still yield a usable window.
        assert_eq!(cache_tolerance_ms(0, true), 1);
        assert_eq!(cache_tolerance_ms(1, true), 1);
    }

    #[test]
    fn ring_capacity_stays_within_the_memory_budget() {
        // 1080p RGBA8 is 8.29 MB a frame: a 192 MB budget is 24 of them.
        let hd = ring_capacity(1920 * 1080 * 4);
        assert_eq!(hd, 24);
        assert!(hd as u64 * 1920 * 1080 * 4 <= RING_BUDGET_BYTES);
        // Small frames are capped, and tiny ones do not go below the floor.
        assert_eq!(ring_capacity(64 * 36 * 4), RING_MAX_SLOTS);
        assert_eq!(ring_capacity(u64::MAX), RING_MIN_SLOTS);
        assert_eq!(ring_capacity(0), RING_MAX_SLOTS);
    }
}
