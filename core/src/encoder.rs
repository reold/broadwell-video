//! In-process VA-API H.264 encoding.
//!
//! The export path used to feed raw NV12 to a child ffmpeg over a pipe, which
//! cost a CPU readback of every frame: two GPU syncs, a 3 MB memcpy and a pipe
//! write, measured at 16-17 ms of a 23 ms frame. None of that is required. The
//! encoder's own surface pool exports as DRM PRIME in the same R8/GR88 layout
//! the decode path already imports, so the grade is written straight into the
//! surface the encoder reads. libavformat muxes here as well, which is what
//! removes the child process and its pipe.
//!
//! The caller's half of the bargain: `begin_frame` hands out a surface, the GPU
//! writes both planes, then `write_frame` sends it. The picture never crosses
//! the CPU.

use anyhow::{Result, bail};
use ffmpeg_sys_next::*;
use std::ffi::CString;
use std::os::fd::RawFd;
use std::ptr::null_mut;

/// One plane of an encoder surface, as the importer needs to see it.
#[derive(Clone, Copy, Debug)]
pub struct Plane {
    /// Linux DRM fourcc, which is what the importer switches on.
    pub fourcc: u32,
    pub offset: u64,
    /// Bytes per row in the dmabuf. Not always width times bytes per texel: the
    /// Y plane here is padded to 384 rows for a 1080p surface.
    pub pitch: u64,
}

/// An encoder surface handed out for grading.
///
/// The file descriptors belong to the encoder and stay valid until the next
/// `begin_frame`. Anything that takes ownership — a dma-buf import does — must
/// `dup` first.
#[derive(Clone, Copy, Debug)]
pub struct Surface {
    pub y_fd: RawFd,
    pub uv_fd: RawFd,
    pub modifier: u64,
    pub y: Plane,
    pub uv: Plane,
    pub width: u32,
    pub height: u32,
}

/// A VA-API H.264 encoder writing to `path`, muxed by libavformat.
pub struct VaapiEncoder {
    codec: *mut AVCodecContext,
    frames: *mut AVBufferRef,
    packet: *mut AVPacket,
    frame: *mut AVFrame,
    /// The DRM PRIME view of `frame`, kept alive while the GPU writes it.
    mapped: *mut AVFrame,
    mux: *mut AVFormatContext,
    stream: *mut AVStream,
    stream_index: i32,
    frames_written: u64,
    finished: bool,
}

fn describe(rc: i32) -> String {
    let mut buffer = [0i8; 256];
    unsafe {
        if av_strerror(rc, buffer.as_mut_ptr(), buffer.len()).is_negative() {
            return format!("error {rc}");
        }
        let text = std::ffi::CStr::from_ptr(buffer.as_ptr());
        format!("{} ({rc})", text.to_string_lossy())
    }
}

fn check(rc: i32, what: &str) -> Result<()> {
    if rc.is_negative() {
        bail!("{what}: {}", describe(rc));
    }
    Ok(())
}

impl VaapiEncoder {
    /// Open the encoder and the output file.
    ///
    /// `fps` is the *source* rate; it becomes the encoder's time base and the
    /// presentation timestamps, so a 29.97 clip keeps its duration rather than
    /// being stretched to 30.
    pub fn new(path: &str, width: u32, height: u32, fps: f64, qp: i32) -> Result<Self> {
        unsafe {
            let device_name = CString::new("/dev/dri/renderD128")?;
            let mut device: *mut AVBufferRef = null_mut();
            let rc = av_hwdevice_ctx_create(
                &mut device,
                AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                device_name.as_ptr(),
                null_mut(),
                0,
            );
            check(rc, "open the VAAPI device")?;

            let codec_name = CString::new("h264_vaapi")?;
            let codec = avcodec_find_encoder_by_name(codec_name.as_ptr());
            if codec.is_null() {
                bail!("no h264_vaapi encoder in this ffmpeg build");
            }

            let codec_ctx = avcodec_alloc_context3(codec);
            if codec_ctx.is_null() {
                bail!("avcodec_alloc_context3 returned null");
            }
            let rate = av_d2q(fps, 100_000);
            (*codec_ctx).width = width as i32;
            (*codec_ctx).height = height as i32;
            (*codec_ctx).framerate = rate;
            (*codec_ctx).time_base = AVRational {
                num: rate.den,
                den: rate.num,
            };
            (*codec_ctx).pix_fmt = AVPixelFormat::AV_PIX_FMT_VAAPI;
            (*codec_ctx).gop_size = 30;
            (*codec_ctx).max_b_frames = 0;
            (*codec_ctx).bit_rate = 20_000_000;

            // Constant QP, matching what the child process used to be told.
            if !(*codec_ctx).priv_data.is_null() {
                let mode = CString::new("CQP")?;
                let key = CString::new("rc_mode")?;
                av_opt_set((*codec_ctx).priv_data, key.as_ptr(), mode.as_ptr(), 0);
                let qp_key = CString::new("qp")?;
                av_opt_set_int((*codec_ctx).priv_data, qp_key.as_ptr(), qp as i64, 0);
            }

            // The surface pool, which has to exist before the codec opens: the
            // VAAPI encoder refuses to open without a frames reference.
            let frames = av_hwframe_ctx_alloc(device);
            if frames.is_null() {
                bail!("av_hwframe_ctx_alloc returned null");
            }
            {
                let ctx = (*frames).data as *mut AVHWFramesContext;
                (*ctx).format = AVPixelFormat::AV_PIX_FMT_VAAPI;
                (*ctx).sw_format = AVPixelFormat::AV_PIX_FMT_NV12;
                (*ctx).width = width as i32;
                (*ctx).height = height as i32;
                // The encoder may hold a couple of surfaces in flight; the pool
                // has to cover that plus the one being graded.
                //
                // 64 rather than 16 because reuse is not yet safe here. At 16
                // the packets themselves went missing (28-29 of 30 muxed); at 64
                // all 30 arrive with correct timestamps. Handing an encoder
                // surfaces out of the caller's pool is not the same as letting
                // it own them, and the real fix is to hold a reference to each
                // sent frame until the encoder is finished with it rather than
                // unref'ing it at the start of the next `begin_frame`. That
                // also brings this back down to a sensible size for 1080p,
                // where 64 surfaces is about 190 MB.
                (*ctx).initial_pool_size = 64;
            }
            check(av_hwframe_ctx_init(frames), "initialise the surface pool")?;

            (*codec_ctx).hw_device_ctx = av_buffer_ref(device);
            (*codec_ctx).hw_frames_ctx = av_buffer_ref(frames);
            check(avcodec_open2(codec_ctx, codec, null_mut()), "open the encoder")?;

            // Muxing.
            let c_path = CString::new(path)?;
            let mut mux: *mut AVFormatContext = null_mut();
            check(
                avformat_alloc_output_context2(&mut mux, null_mut(), null_mut(), c_path.as_ptr()),
                "allocate the output context",
            )?;
            let stream = avformat_new_stream(mux, null_mut());
            if stream.is_null() {
                bail!("avformat_new_stream returned null");
            }
            (*stream).time_base = (*codec_ctx).time_base;
            check(
                avcodec_parameters_from_context((*stream).codecpar, codec_ctx),
                "copy the codec parameters",
            )?;
            check(
                avio_open(&mut (*mux).pb, c_path.as_ptr(), AVIO_FLAG_WRITE as i32),
                "open the output file",
            )?;
            check(avformat_write_header(mux, null_mut()), "write the header")?;

            av_buffer_unref(&mut { device });

            Ok(Self {
                codec: codec_ctx,
                frames,
                packet: av_packet_alloc(),
                frame: av_frame_alloc(),
                mapped: av_frame_alloc(),
                mux,
                stream,
                stream_index: (*stream).index,
                frames_written: 0,
                finished: false,
            })
        }
    }

    /// Take the next surface and describe its two planes.
    pub fn begin_frame(&mut self) -> Result<Surface> {
        unsafe {
            av_frame_unref(self.frame);
            check(
                av_hwframe_get_buffer(self.frames, self.frame, 0),
                "get an encoder surface",
            )?;

            av_frame_unref(self.mapped);
            (*self.mapped).format = AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
            check(
                av_hwframe_map(self.mapped, self.frame, AV_HWFRAME_MAP_READ as i32),
                "map the encoder surface as DRM PRIME",
            )?;

            let desc = (*self.mapped).data[0] as *const AVDRMFrameDescriptor;
            if desc.is_null() || (*desc).nb_layers < 2 {
                bail!("encoder surface did not describe two DRM layers");
            }
            let y_layer = &(*desc).layers[0];
            let uv_layer = &(*desc).layers[1];
            let y_plane = &y_layer.planes[0];
            let uv_plane = &uv_layer.planes[0];
            let y_object = &(*desc).objects[y_plane.object_index as usize];
            let uv_object = &(*desc).objects[uv_plane.object_index as usize];

            Ok(Surface {
                y_fd: y_object.fd,
                uv_fd: uv_object.fd,
                modifier: y_object.format_modifier,
                y: Plane {
                    fourcc: y_layer.format,
                    offset: y_plane.offset as u64,
                    pitch: y_plane.pitch as u64,
                },
                uv: Plane {
                    fourcc: uv_layer.format,
                    offset: uv_plane.offset as u64,
                    pitch: uv_plane.pitch as u64,
                },
                width: (*self.frame).width as u32,
                height: (*self.frame).height as u32,
            })
        }
    }

    /// Send the surface filled by the GPU and mux whatever the encoder emits.
    pub fn write_frame(&mut self) -> Result<()> {
        unsafe {
            (*self.frame).pts = self.frames_written as i64;
            // Duration in the encoder's time base, so the muxer knows how long
            // each picture lasts. Without it ffmpeg infers from the next PTS:
            // the packet after a gap came out stretched to two frames and the
            // final one was flagged discard.
            (*self.frame).duration = 1;
            check(
                avcodec_send_frame(self.codec, self.frame),
                "send a graded frame to the encoder",
            )?;
            self.drain()?;
            self.frames_written += 1;
            Ok(())
        }
    }

    fn drain(&mut self) -> Result<()> {
        unsafe {
            loop {
                let rc = avcodec_receive_packet(self.codec, self.packet);
                if rc == AVERROR_EOF {
                    break;
                }
                if rc == AVERROR(libc::EAGAIN) {
                    // The encoder wants more input before it can emit.
                    break;
                }
                // Anything else is a real failure and used to be swallowed by
                // treating every negative return as "nothing ready".
                check(rc, "receive an encoded packet")?;
                (*self.packet).stream_index = self.stream_index;
                av_packet_rescale_ts(
                    self.packet,
                    (*self.codec).time_base,
                    (*self.stream).time_base,
                );
                check(
                    av_interleaved_write_frame(self.mux, self.packet),
                    "mux an encoded packet",
                )?;
                av_packet_unref(self.packet);
            }
            Ok(())
        }
    }

    /// Flush, write the trailer and report how many frames were encoded.
    pub fn finish(&mut self) -> Result<u64> {
        if self.finished {
            return Ok(self.frames_written);
        }
        unsafe {
            check(avcodec_send_frame(self.codec, null_mut()), "flush the encoder")?;
            self.drain()?;
            check(av_write_trailer(self.mux), "write the trailer")?;
            if !(*self.mux).pb.is_null() {
                avio_closep(&mut (*self.mux).pb);
            }
        }
        self.finished = true;
        Ok(self.frames_written)
    }

    /// Frames handed to the encoder so far.
    pub fn frames_written(&self) -> u64 {
        self.frames_written
    }
}

impl Drop for VaapiEncoder {
    fn drop(&mut self) {
        unsafe {
            if !self.finished && !self.mux.is_null() {
                // Best effort: a half-written file is better closed than left.
                av_write_trailer(self.mux);
                if !(*self.mux).pb.is_null() {
                    avio_closep(&mut (*self.mux).pb);
                }
            }
            if !self.mux.is_null() {
                avformat_free_context(self.mux);
            }
            if !self.packet.is_null() {
                av_packet_free(&mut self.packet);
            }
            if !self.frame.is_null() {
                av_frame_free(&mut self.frame);
            }
            if !self.mapped.is_null() {
                av_frame_free(&mut self.mapped);
            }
            if !self.frames.is_null() {
                av_buffer_unref(&mut self.frames);
            }
            if !self.codec.is_null() {
                avcodec_free_context(&mut self.codec);
            }
        }
    }
}
