use anyhow::{Result, bail};
use ffmpeg_sys_next::*;
use std::ffi::CString;
use std::ptr;
use std::time::Duration;

pub fn check(rc: i32, what: &str) -> Result<()> {
    if rc < 0 {
        bail!("{} failed: {}", what, rc);
    }
    Ok(())
}

pub struct Handles {
    pub fmt_ctx: *mut AVFormatContext,
    pub codec_ctx: *mut AVCodecContext,
    pub hw_device_ctx: *mut AVBufferRef,
    pub packet: *mut AVPacket,
    pub decoded: *mut AVFrame,
    pub drm_frame: *mut AVFrame,
    pub video_stream: i32,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub duration_ms: i64,
    pub time_base_num: i32,
    pub time_base_den: i32,
    /// Sample aspect ratio (pixel shape) from the bitstream or the container.
    pub sar_num: i32,
    pub sar_den: i32,
}

impl Handles {
    pub unsafe fn open(path: &str, device: &str) -> Result<Self> {
        let cpath = CString::new(path.to_string())?;
        let device_name = CString::new(device)?;

        unsafe {
            let mut fmt_ctx: *mut AVFormatContext = ptr::null_mut();
            check(
                avformat_open_input(
                    &mut fmt_ctx,
                    cpath.as_ptr(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                ),
                "avformat_open_input",
            )?;
            check(
                avformat_find_stream_info(fmt_ctx, ptr::null_mut()),
                "avformat_find_stream_info",
            )?;

            let video_stream = av_find_best_stream(
                fmt_ctx,
                AVMediaType::AVMEDIA_TYPE_VIDEO,
                -1,
                -1,
                ptr::null_mut(),
                0,
            );
            if video_stream < 0 {
                bail!("no video stream");
            }
            let stream = *(*fmt_ctx).streams.add(video_stream as usize);
            let codecpar = (*stream).codecpar;

            let width = (*codecpar).width as u32;
            let height = (*codecpar).height as u32;
            let fps_val = av_q2d((*stream).r_frame_rate);
            let fps = if fps_val > 0.0 { fps_val } else { 30.0 };

            let duration_ms = if (*fmt_ctx).duration > 0 {
                (*fmt_ctx).duration / (AV_TIME_BASE as i64 / 1000)
            } else {
                0
            };

            let time_base_num = (*stream).time_base.num;
            let time_base_den = (*stream).time_base.den;

            let codec = avcodec_find_decoder((*codecpar).codec_id);
            if codec.is_null() {
                bail!("no decoder");
            }

            let codec_ctx = avcodec_alloc_context3(codec);
            check(
                avcodec_parameters_to_context(codec_ctx, codecpar),
                "avcodec_parameters_to_context",
            )?;

            let mut hw_device_ctx: *mut AVBufferRef = ptr::null_mut();
            check(
                av_hwdevice_ctx_create(
                    &mut hw_device_ctx,
                    AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                    device_name.as_ptr(),
                    ptr::null_mut(),
                    0,
                ),
                "av_hwdevice_ctx_create",
            )?;
            (*codec_ctx).hw_device_ctx = av_buffer_ref(hw_device_ctx);

            check(
                avcodec_open2(codec_ctx, codec, ptr::null_mut()),
                "avcodec_open2",
            )?;

            // Sample aspect ratio (pixel shape). The decoder fills this in from
            // the H.264 VUI, the container may also carry it (MP4 pasp, MKV
            // display dimensions). Prefer the most specific source, and fall
            // back to square pixels when nobody declares one.
            let mut sar = (*codec_ctx).sample_aspect_ratio;
            if sar.num <= 0 || sar.den <= 0 {
                sar = (*codecpar).sample_aspect_ratio;
            }
            if sar.num <= 0 || sar.den <= 0 {
                sar = (*stream).sample_aspect_ratio;
            }
            let (sar_num, sar_den) = if sar.num > 0 && sar.den > 0 {
                (sar.num, sar.den)
            } else {
                (1, 1)
            };

            let packet = av_packet_alloc();
            let decoded = av_frame_alloc();
            let drm_frame = av_frame_alloc();

            Ok(Handles {
                fmt_ctx,
                codec_ctx,
                hw_device_ctx,
                packet,
                decoded,
                drm_frame,
                video_stream,
                width,
                height,
                fps,
                duration_ms,
                time_base_num,
                time_base_den,
                sar_num,
                sar_den,
            })
        }
    }

    pub fn frame_period(&self) -> Duration {
        Duration::from_secs_f64(1.0 / self.fps)
    }

    /// Aspect ratio to letterbox against, including non-square pixels.
    pub fn display_aspect(&self) -> f32 {
        display_aspect(self.width, self.height, self.sar_num, self.sar_den)
    }

    pub unsafe fn rewind(&mut self) {
        unsafe {
            av_seek_frame(self.fmt_ctx, self.video_stream, 0, AVSEEK_FLAG_BACKWARD);
            avcodec_flush_buffers(self.codec_ctx);
            av_frame_unref(self.decoded);
            av_packet_unref(self.packet);
        }
    }

    pub unsafe fn seek_to_ms(&mut self, ms: i64) -> Result<()> {
        unsafe {
            let ts = av_rescale_q(
                ms,
                AVRational { num: 1, den: 1000 },
                AVRational {
                    num: self.time_base_num,
                    den: self.time_base_den,
                },
            );
            check(
                av_seek_frame(self.fmt_ctx, self.video_stream, ts, AVSEEK_FLAG_BACKWARD),
                "av_seek_frame",
            )?;
            avcodec_flush_buffers(self.codec_ctx);
            av_frame_unref(self.decoded);
            av_packet_unref(self.packet);
            Ok(())
        }
    }

    pub unsafe fn current_pts_ms(&self) -> i64 {
        unsafe {
            if self.decoded.is_null() {
                return 0;
            }
            let pts = (*self.decoded).best_effort_timestamp;
            if pts < 0 {
                return 0;
            }
            av_rescale_q(
                pts,
                AVRational {
                    num: self.time_base_num,
                    den: self.time_base_den,
                },
                AVRational { num: 1, den: 1000 },
            )
        }
    }
}

impl Drop for Handles {
    fn drop(&mut self) {
        unsafe {
            if !self.drm_frame.is_null() {
                av_frame_free(&mut self.drm_frame);
            }
            if !self.decoded.is_null() {
                av_frame_free(&mut self.decoded);
            }
            if !self.packet.is_null() {
                av_packet_free(&mut self.packet);
            }
            if !self.codec_ctx.is_null() {
                avcodec_free_context(&mut self.codec_ctx);
            }
            if !self.hw_device_ctx.is_null() {
                av_buffer_unref(&mut self.hw_device_ctx);
            }
            if !self.fmt_ctx.is_null() {
                avformat_close_input(&mut self.fmt_ctx);
            }
        }
    }
}

/// Display aspect ratio (width / height), honouring non-square pixels.
///
/// A non-positive `sar` means the stream declared none, so square pixels are
/// assumed. Video is not always stored with square pixels: 720x480 NTSC is
/// 16:9 for display when tagged SAR 32:27, and letterboxing it as 3:2 would
/// put the bars in the wrong place.
pub fn display_aspect(width: u32, height: u32, sar_num: i32, sar_den: i32) -> f32 {
    let sar = if sar_num > 0 && sar_den > 0 {
        sar_num as f32 / sar_den as f32
    } else {
        1.0
    };
    (width.max(1) as f32 / height.max(1) as f32) * sar
}

pub unsafe fn peek_video_info(path: &str) -> Result<(u32, u32, f64)> {
    let cpath = CString::new(path.to_string())?;
    unsafe {
        let mut fmt_ctx: *mut AVFormatContext = ptr::null_mut();
        check(
            avformat_open_input(
                &mut fmt_ctx,
                cpath.as_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
            ),
            "peek: avformat_open_input",
        )?;
        check(
            avformat_find_stream_info(fmt_ctx, ptr::null_mut()),
            "peek: avformat_find_stream_info",
        )?;
        let vs = av_find_best_stream(
            fmt_ctx,
            AVMediaType::AVMEDIA_TYPE_VIDEO,
            -1,
            -1,
            ptr::null_mut(),
            0,
        );
        if vs < 0 {
            avformat_close_input(&mut fmt_ctx);
            bail!("peek: no video stream");
        }
        let stream = *(*fmt_ctx).streams.add(vs as usize);
        let w = (*(*stream).codecpar).width as u32;
        let h = (*(*stream).codecpar).height as u32;
        let fps_val = av_q2d((*stream).r_frame_rate);
        let fps = if fps_val > 0.0 { fps_val } else { 30.0 };
        avformat_close_input(&mut fmt_ctx);
        Ok((w, h, fps))
    }
}
