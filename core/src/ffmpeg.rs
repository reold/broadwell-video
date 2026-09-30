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

/// All FFmpeg state for one open file. Owned by App.
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
            })
        }
    }

    pub fn frame_period(&self) -> Duration {
        Duration::from_secs_f64(1.0 / self.fps)
    }

    pub unsafe fn rewind(&mut self) {
        unsafe {
            av_seek_frame(self.fmt_ctx, self.video_stream, 0, AVSEEK_FLAG_BACKWARD);
            avcodec_flush_buffers(self.codec_ctx);
            av_frame_unref(self.decoded);
            av_packet_unref(self.packet);
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
