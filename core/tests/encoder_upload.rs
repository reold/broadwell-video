//! The control for the frame loss at pipeline depth two.
//!
//! The in-process encoder loses a picture mid-stream at depth two, and never at
//! depth one. The child process it replaced ran at depth two at least and was
//! complete, and the one clear difference is where the pixels come from: it
//! uploaded software frames with hwupload, while the in-process path grades
//! straight into the encoder's surfaces.
//!
//! So: the same sixty frames, at depth two, uploaded from system memory with
//! `av_hwframe_transfer_data`. If this is complete, the loss belongs to surfaces
//! written by the render engine and what is missing is the handover back to the
//! video engine. If it is not, the loss is in how this encoder is set up.

use ffmpeg_sys_next::*;
use hwa_core::encoder::VaapiEncoder;
use std::process::Command;

const WIDTH: u32 = 640;
const HEIGHT: u32 = 360;
const FRAMES: u64 = 60;
const LUMA: u8 = 160;
const CHROMA: u8 = 128;
const DEPTH: i32 = 2;

#[test]
fn uploaded_frames_survive_pipeline_depth_two() {
    let out = std::path::Path::new("target/encoder-uploaded.mp4");
    std::fs::create_dir_all("target").ok();
    std::fs::remove_file(out).ok();

    let mut encoder = VaapiEncoder::new(out.to_str().unwrap(), WIDTH, HEIGHT, 29.97, 22, DEPTH)
        .expect("open encoder");

    unsafe {
        let sw = av_frame_alloc();
        (*sw).format = AVPixelFormat::AV_PIX_FMT_NV12 as i32;
        (*sw).width = WIDTH as i32;
        (*sw).height = HEIGHT as i32;
        assert!(av_frame_get_buffer(sw, 0) >= 0, "av_frame_get_buffer");
        let y_stride = (*sw).linesize[0] as usize;
        for row in 0..HEIGHT as usize {
            std::ptr::write_bytes((*sw).data[0].add(row * y_stride), LUMA, WIDTH as usize);
        }
        let uv_stride = (*sw).linesize[1] as usize;
        for row in 0..(HEIGHT / 2) as usize {
            let row_ptr = (*sw).data[1].add(row * uv_stride);
            for i in 0..(WIDTH / 2) as usize {
                *row_ptr.add(i * 2) = CHROMA;
                *row_ptr.add(i * 2 + 1) = CHROMA;
            }
        }

        for _ in 0..FRAMES {
            encoder.begin_frame().expect("surface");
            encoder.upload_from(sw).expect("upload");
            encoder.write_frame().expect("write frame");
        }
        av_frame_free(&mut { sw } as *mut *mut AVFrame);
    }

    let written = encoder.finish().expect("finish");
    assert_eq!(written, FRAMES);

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
            out.to_str().unwrap(),
        ])
        .output()
        .expect("ffprobe");
    let count: u64 = String::from_utf8_lossy(&counted.stdout)
        .trim()
        .parse()
        .expect("frame count");
    println!("depth {DEPTH} with uploaded input: {count} of {FRAMES} decodable");
    assert_eq!(count, FRAMES, "depth two lost frames on uploaded input too");
}
