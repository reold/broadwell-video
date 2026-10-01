//! Spike: can a VAAPI *encoder* surface be exported as DRM PRIME, so the GPU
//! can write the graded picture straight into the frame the encoder reads?
//!
//! The decode direction is proven (it is what the whole preview pipeline does).
//! The export direction is the one unproven link in the in-process zero-copy
//! encoder, so this probe answers three questions before any of it is designed:
//!
//!   1. Does `av_hwframe_get_buffer` on an encoder's frames context give us a
//!      surface we can map to DRM PRIME at all?
//!   2. What layout does the driver describe — how many layers, which fourccs,
//!      which modifier? That decides how many textures to import.
//!   3. Does the encoder accept that same surface back through
//!      `avcodec_send_frame` and produce packets?
//!
//! It writes nothing into the surface, so the encoded output is garbage. That is
//! fine: this is about whether the plumbing exists, not about pixels.

use ffmpeg_sys_next::*;
use std::ffi::CString;

const WIDTH: i32 = 640;
const HEIGHT: i32 = 360;

unsafe fn check(rc: i32, what: &str) {
    assert!(rc >= 0, "{what} failed: {rc}");
}

#[test]
fn encoder_surface_exports_as_drm_prime() {
    unsafe {
        let name = CString::new("h264_vaapi").unwrap();
        let codec = avcodec_find_encoder_by_name(name.as_ptr());
        assert!(!codec.is_null(), "no h264_vaapi encoder in this ffmpeg");

        let device_name = CString::new("/dev/dri/renderD128").unwrap();
        let mut device: *mut AVBufferRef = std::ptr::null_mut();
        let rc = av_hwdevice_ctx_create(
            &mut device,
            AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
            device_name.as_ptr(),
            std::ptr::null_mut(),
            0,
        );
        assert!(rc >= 0 && !device.is_null(), "VAAPI device open failed: {rc}");

        let ctx = avcodec_alloc_context3(codec);
        assert!(!ctx.is_null(), "avcodec_alloc_context3");
        (*ctx).width = WIDTH;
        (*ctx).height = HEIGHT;
        (*ctx).time_base = AVRational { num: 1, den: 30 };
        (*ctx).framerate = AVRational { num: 30, den: 1 };
        (*ctx).pix_fmt = AVPixelFormat::AV_PIX_FMT_VAAPI;
        (*ctx).bit_rate = 4_000_000;
        (*ctx).gop_size = 30;
        (*ctx).max_b_frames = 0;

        // The surface pool the encoder encodes from. If we can fill these
        // ourselves, the readback disappears. It has to exist *before* the codec
        // opens: the VAAPI encoder refuses to open without a frames reference
        // ("A hardware frames reference is required to associate the encoding
        // device"), unlike the decoder, which only needs the device.
        let mut frames_ref = av_hwframe_ctx_alloc(device);
        assert!(!frames_ref.is_null(), "av_hwframe_ctx_alloc");
        {
            let frames = (*frames_ref).data as *mut AVHWFramesContext;
            (*frames).format = AVPixelFormat::AV_PIX_FMT_VAAPI;
            (*frames).sw_format = AVPixelFormat::AV_PIX_FMT_NV12;
            (*frames).width = WIDTH;
            (*frames).height = HEIGHT;
            (*frames).initial_pool_size = 4;
        }
        check(av_hwframe_ctx_init(frames_ref), "av_hwframe_ctx_init");

        (*ctx).hw_device_ctx = av_buffer_ref(device);
        (*ctx).hw_frames_ctx = av_buffer_ref(frames_ref);
        check(
            avcodec_open2(ctx, codec, std::ptr::null_mut()),
            "avcodec_open2",
        );

        let frame = av_frame_alloc();
        check(av_hwframe_get_buffer(frames_ref, frame, 0), "av_hwframe_get_buffer");
        println!(
            "got a surface: format {:?} {}x{}",
            (*frame).format,
            (*frame).width,
            (*frame).height
        );

        // Question 1 and 2: export it the same way the decode path does.
        let drm = av_frame_alloc();
        (*drm).format = AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
        let rc = av_hwframe_map(drm, frame, AV_HWFRAME_MAP_READ as i32);
        assert!(
            rc >= 0,
            "av_hwframe_map of an ENCODER surface failed: {rc} \
             (this is the link the whole design rests on)"
        );

        let desc = (*drm).data[0] as *const AVDRMFrameDescriptor;
        assert!(!desc.is_null(), "no DRM descriptor");
        println!(
            "DRM PRIME: {} layer(s), {} object(s)",
            (*desc).nb_layers,
            (*desc).nb_objects
        );
        for i in 0..(*desc).nb_objects as isize {
            let obj = &(*desc).objects[i as usize];
            println!(
                "  object {i}: fd {} size {} modifier {:#x}",
                obj.fd, obj.size, obj.format_modifier
            );
        }
        for i in 0..(*desc).nb_layers as isize {
            let layer = &(*desc).layers[i as usize];
            println!(
                "  layer {i}: fourcc {:#010x} planes {}",
                layer.format, layer.nb_planes
            );
            for p in 0..layer.nb_planes as isize {
                let plane = &layer.planes[p as usize];
                println!(
                    "    plane {p}: object {} offset {} pitch {}",
                    plane.object_index, plane.offset, plane.pitch
                );
            }
        }

        // Question 3: does the encoder take the surface back and emit packets?
        // No av_frame_make_writable: that is for software frames and fails with
        // ENOSYS on a hardware one. A VAAPI surface is written by the GPU, and
        // we are only proving the encoder accepts it.
        check(avcodec_send_frame(ctx, frame), "avcodec_send_frame");
        check(avcodec_send_frame(ctx, std::ptr::null()), "flush");

        let packet = av_packet_alloc();
        let mut packets = 0;
        let mut bytes = 0usize;
        loop {
            let rc = avcodec_receive_packet(ctx, packet);
            if rc < 0 {
                break;
            }
            packets += 1;
            bytes += (*packet).size as usize;
            av_packet_unref(packet);
        }
        println!("encoder produced {packets} packet(s), {bytes} bytes");
        assert!(packets > 0, "encoder produced nothing from its own surface");

        av_packet_free(&mut { packet } as *mut *mut AVPacket);
        av_frame_free(&mut { drm } as *mut *mut AVFrame);
        av_frame_free(&mut { frame } as *mut *mut AVFrame);
        av_buffer_unref(&mut frames_ref);
        avcodec_free_context(&mut { ctx } as *mut *mut AVCodecContext);
        av_buffer_unref(&mut device);
    }
}
