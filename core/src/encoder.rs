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
use std::collections::VecDeque;
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
    /// Packets actually written to the container.
    muxed: u64,
    discarded: u64,
    /// Waits that gave up because no packet came.
    starved: u64,
    /// Frames sent to the encoder that it may still be holding.
    ///
    /// The VAAPI encoder works from surface ids, so releasing the AVFrame once
    /// it has been sent can return its surface to the pool while a picture is
    /// still queued. Measured with `async_depth=4`: fifty-eight packets for
    /// sixty frames and twenty-nine decodable, against sixty and sixty at depth
    /// 1 — which did not fix that so much as hide it by encoding strictly one
    /// frame at a time, at 30.7 ms of send per frame. Holding a reference keeps
    /// each surface out of the pool until the queue has moved past it.
    handoff: VecDeque<*mut AVFrame>,
    /// When the previous frame was sent, for pacing.
    last_send: Option<std::time::Instant>,
    finished: bool,
}

/// How many sent frames to keep alive. Comfortably more than `async_depth`.
const HANDOFF_HOLD: usize = 8;

/// The shortest interval between frames handed to the encoder.
///
/// The driver discards pictures when it is fed faster than it can encode, and
/// the numbers say where that starts. Unpaced, the editor's flow runs at 124 fps
/// and the file comes back with 881 packets for 900 frames; the throughput
/// harness at 91 fps loses none. Pacing is cheaper than dropping: a cap of about
/// seventy frames a second costs a little speed and keeps every picture, while
/// the file that arrives short costs the whole export.
const MIN_FRAME_PERIOD: std::time::Duration = std::time::Duration::from_micros(14_000);

/// How long to wait for a packet before giving up and carrying on.
///
/// The wait exists to apply back-pressure, not to hang: if the driver has
/// discarded this picture there is no packet coming, and spinning for one
/// forever is worse than a short file.
const WAIT_FOR_PACKET: std::time::Duration = std::time::Duration::from_millis(500);

/// How many pictures may be in the encoder at once before the caller waits.
///
/// The child process this replaced was paced by its pipe: a write blocked when
/// ffmpeg's input buffer filled. In process there is nothing to block on, and
/// `avcodec_send_frame` keeps accepting long after the encoder has stopped
/// keeping up, so the driver drops pictures.
///
/// One, and deliberately not more. Four was tried and is not enough: the editor
/// at 128 frames a second still lost six pictures, which showed up as 894 packets
/// for 900 frames. Waits are on packet arrival, so a deeper window only lets the
/// driver queue work it will then discard. Serial costs nothing measurable --
/// 91.1 fps against 89.8 with a window of four, both with 900 of 900 decodable --
/// because the encoder's latency is not the limit; its queue is.
const IN_FLIGHT_WINDOW: u64 = 1;

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
    pub fn new(
        path: &str,
        width: u32,
        height: u32,
        fps: f64,
        qp: i32,
        async_depth: i32,
    ) -> Result<Self> {
        // The encoder falls back to the ambient driver if its caller has not
        // already chosen one, which on this machine means the archived i965.
        crate::select_vaapi_driver();
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
            // Global header, so the encoder produces its parameter sets up front
            // and `avformat_write_header` can put them in the container. Without
            // it the mp4 muxer coped and matroska refused the stream outright
            // with "Invalid data found when processing input".
            (*codec_ctx).flags |= AV_CODEC_FLAG_GLOBAL_HEADER as i32;

            // Constant QP, matching what the child process used to be told.
            if !(*codec_ctx).priv_data.is_null() {
                let mode = CString::new("CQP")?;
                let key = CString::new("rc_mode")?;
                av_opt_set((*codec_ctx).priv_data, key.as_ptr(), mode.as_ptr(), 0);
                let qp_key = CString::new("qp")?;
                av_opt_set_int((*codec_ctx).priv_data, qp_key.as_ptr(), qp as i64, 0);
                // One picture in flight for the editor, and that is a
                // correctness requirement rather than a preference: depth above
                // one loses pictures mid-stream, not at the flush, so the
                // keepalive in `finish` cannot absorb it. Callers pass the depth
                // so a test can demonstrate that.
                //
                // Depth 4 loses pictures: fifty-eight packets for sixty frames
                // and twenty-nine decodable, and still fifty-six of sixty with
                // the handoff hold below holding surfaces out of the pool. The
                // child process this replaced ran the same encoder at the same
                // default depth and was complete, and the difference is where
                // the pixels come from: it uploaded software frames with
                // hwupload, while these surfaces are written by the render
                // engine. Until that handover to the video engine is done
                // properly -- grafting acquires the image from the foreign queue
                // but nothing releases it back -- depth has to stay at one.
                //
                // The cost is real and measured: 30.7 ms of `send` per frame
                // against 0.19 ms for the grade and its copies, which caps an
                // export at roughly 32 fps. That is the next thing to fix.
                let depth_key = CString::new("async_depth")?;
                let rc = av_opt_set_int((*codec_ctx).priv_data, depth_key.as_ptr(), async_depth as i64, 0);
                if rc.is_negative() {
                    bail!("this ffmpeg's h264_vaapi has no async_depth option: {}", describe(rc));
                }
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
                muxed: 0,
                discarded: 0,
                starved: 0,
                handoff: VecDeque::new(),
                last_send: None,
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

    /// Fill the current surface from a software frame.
    ///
    /// Not how the editor works — it grades straight into the surface — but this
    /// is what `hwupload` does inside the ffmpeg CLI, which makes it the control
    /// when something about GPU-written surfaces is in question.
    pub fn upload_from(&mut self, software: *mut AVFrame) -> Result<()> {
        unsafe {
            check(
                av_hwframe_transfer_data(self.frame, software, 0),
                "upload a software frame into the encoder surface",
            )
        }
    }

    /// Send the surface filled by the GPU and mux whatever the encoder emits.
    pub fn write_frame(&mut self) -> Result<()> {
        // Pace to the fastest rate the driver has been measured to sustain.
        if let Some(last) = self.last_send {
            let elapsed = last.elapsed();
            if elapsed < MIN_FRAME_PERIOD {
                std::thread::sleep(MIN_FRAME_PERIOD - elapsed);
            }
        }
        self.last_send = Some(std::time::Instant::now());
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
            // Keep this frame's surface out of the pool while the encoder may
            // still be reading it. See the field comment.
            let held = av_frame_alloc();
            if held.is_null() {
                bail!("av_frame_alloc for the handoff failed");
            }
            check(av_frame_ref(held, self.frame), "reference the sent frame")?;
            self.handoff.push_back(held);
            while self.handoff.len() > HANDOFF_HOLD {
                if let Some(mut old) = self.handoff.pop_front() {
                    av_frame_free(&mut old);
                }
            }
            // Counted before draining so the mux cap in `drain` knows how many
            // pictures belong to real frames.
            self.frames_written += 1;
            self.drain(false)?;
            // Back-pressure: wait for the encoder rather than queueing pictures
            // it will have to drop. See IN_FLIGHT_WINDOW.
            while self.frames_written.saturating_sub(self.muxed) > IN_FLIGHT_WINDOW {
                if self.drain(true)? {
                    break;
                }
            }
            Ok(())
        }
    }

    /// Pull whatever the encoder has ready, muxing it.
    ///
    /// Returns true once the stream is finished. With `wait_for_one` it does not
    /// return until at least one packet has been muxed, which is how the caller
    /// applies back-pressure: there is nothing else to block on.
    fn drain(&mut self, wait_for_one: bool) -> Result<bool> {
        unsafe {
            let mut muxed_any = false;
            let waited = std::time::Instant::now();
            loop {
                let rc = avcodec_receive_packet(self.codec, self.packet);
                if rc == AVERROR_EOF {
                    return Ok(true);
                }
                if rc == AVERROR(libc::EAGAIN) {
                    if wait_for_one && !muxed_any {
                        // The encoder is still working. Nothing outside the
                        // driver can see its completion, so this is a short
                        // sleep rather than a fence -- but only for so long.
                        if waited.elapsed() > WAIT_FOR_PACKET {
                            self.starved += 1;
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_micros(200));
                        continue;
                    }
                    // The encoder wants more input before it can emit.
                    break;
                }
                // Anything else is a real failure and used to be swallowed by
                // treating every negative return as "nothing ready".
                check(rc, "receive an encoded packet")?;
                if (*self.packet).flags & AV_PKT_FLAG_DISCARD as i32 != 0 {
                    // Counted, and the flag is cleared before muxing.
                    //
                    // An earlier version skipped these, on the reading that a
                    // discard packet holds no picture. That is wrong: at depth
                    // above one the first packet of the file -- the IDR the whole
                    // first GOP references -- comes back flagged, and skipping it
                    // left a stream whose first decodable frame was thirty frames
                    // in. Then it muxed them with the flag still set, which is
                    // not the same thing: the mp4 muxer honours the flag and
                    // drops the packet, so an export could be told it had written
                    // packets the container never received. Clearing it is what
                    // makes "mux every packet" true.
                    self.discarded += 1;
                    (*self.packet).flags &= !(AV_PKT_FLAG_DISCARD as i32);
                }
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
                self.muxed += 1;
                muxed_any = true;
                if wait_for_one {
                    break;
                }
            }
            Ok(false)
        }
    }

    /// Flush, write the trailer and report how many frames were encoded.
    ///
    /// One throwaway picture is spent first, and at pipeline depth one it is
    /// still needed: without it the final picture comes back as a filler instead
    /// of a coded one and a sixty frame export decodes fifty-nine. The filler
    /// that the duplicate produces is muxed like everything else and decodes to
    /// nothing, which is cheaper than losing a real frame.
    pub fn finish(&mut self) -> Result<u64> {
        if self.finished {
            return Ok(self.frames_written);
        }
        unsafe {
            if self.frames_written > 0 {
                (*self.frame).pts = self.frames_written as i64;
                (*self.frame).duration = 1;
                check(
                    avcodec_send_frame(self.codec, self.frame),
                    "send the keepalive picture",
                )?;
            }
            check(avcodec_send_frame(self.codec, null_mut()), "flush the encoder")?;
            while !self.drain(true)? {}
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

    /// Waits that gave up because the driver produced no packet.
    pub fn starved(&self) -> u64 {
        self.starved
    }

    /// Packets written to the container.
    pub fn muxed(&self) -> u64 {
        self.muxed
    }

    /// Pictures the driver declined to encode, reported as filler packets.
    ///
    /// One is expected: the keepalive sent by `finish`. More than that means
    /// pictures were lost, which the frame-count check on the output catches.
    pub fn discarded(&self) -> u64 {
        self.discarded
    }
}

impl Drop for VaapiEncoder {
    fn drop(&mut self) {
        unsafe {
            for mut held in self.handoff.drain(..) {
                av_frame_free(&mut held);
            }
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
