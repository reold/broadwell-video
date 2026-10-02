use anyhow::{Context, Result, bail};
use std::io::Write;
use std::process::{Child, ChildStdin, Command, Stdio};

#[derive(Clone, Copy)]
#[allow(dead_code)]
pub enum PixFmt {
    Rgba,
    Nv12,
}

impl PixFmt {
    fn bytes_per_frame(self, width: u32, height: u32) -> usize {
        match self {
            PixFmt::Rgba => (width * height * 4) as usize,
            PixFmt::Nv12 => (width * height * 3 / 2) as usize,
        }
    }
    fn ffmpeg_name(self) -> &'static str {
        match self {
            PixFmt::Rgba => "rgba",
            PixFmt::Nv12 => "nv12",
        }
    }
}

/// Row alignment wgpu requires when copying a texture into a buffer.
pub const COPY_ALIGN: u32 = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;

/// Buffer geometry for reading back a packed NV12 frame.
///
/// The export grade writes 4 Y samples into each `R32Uint` texel and 2 UV pairs
/// into each UV texel, so the bytes already come back in NV12 order and only
/// the row padding has to be stripped.
pub struct Nv12Readback {
    pub y_row_bytes: u32,
    pub y_padded_row_bytes: u32,
    pub y_size: u64,
    pub uv_row_bytes: u32,
    pub uv_padded_row_bytes: u32,
    pub uv_height: u32,
    pub uv_size: u64,
}

impl Nv12Readback {
    pub fn new(width: u32, height: u32) -> Self {
        let pad = |bytes: u32| bytes.div_ceil(COPY_ALIGN) * COPY_ALIGN;
        let y_row_bytes = width;
        let y_padded_row_bytes = pad(y_row_bytes);
        // Half-height, and the same byte width as Y: (width/2) pairs of 2 bytes.
        let uv_row_bytes = width;
        let uv_padded_row_bytes = pad(uv_row_bytes);
        let uv_height = height / 2;
        Self {
            y_row_bytes,
            y_padded_row_bytes,
            y_size: y_padded_row_bytes as u64 * height.max(1) as u64,
            uv_row_bytes,
            uv_padded_row_bytes,
            uv_height,
            uv_size: uv_padded_row_bytes as u64 * uv_height.max(1) as u64,
        }
    }
}

/// Copy the packed Y and UV textures back and strip the row padding into one
/// tightly packed NV12 frame, ready for `Exporter::write_frame`.
pub fn readback_nv12(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    y_texture: &wgpu::Texture,
    uv_texture: &wgpu::Texture,
    y_buffer: &wgpu::Buffer,
    uv_buffer: &wgpu::Buffer,
    width: u32,
    height: u32,
    layout: &Nv12Readback,
) -> Result<Vec<u8>> {
    let uv_width = width / 2;

    let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("nv12-readback"),
    });
    enc.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: y_texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: y_buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(layout.y_padded_row_bytes),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width: width.div_ceil(4),
            height,
            depth_or_array_layers: 1,
        },
    );
    enc.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: uv_texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: uv_buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(layout.uv_padded_row_bytes),
                rows_per_image: Some(layout.uv_height),
            },
        },
        wgpu::Extent3d {
            width: uv_width.div_ceil(2),
            height: layout.uv_height,
            depth_or_array_layers: 1,
        },
    );
    queue.submit([enc.finish()]);
    // This poll is load-bearing, and that is measured rather than assumed:
    // removing it and letting the map_async below carry the wait made the
    // readback 16.9 ms -> 38 ms and the export 45.1 fps -> 24.1 fps. With the
    // device already idle, map_async completes on the next poll; with work
    // still in flight, wgpu pays for it twice.
    device
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: None,
        })
        .ok();

    let y_slice = y_buffer.slice(..);
    let uv_slice = uv_buffer.slice(..);
    let (y_tx, y_rx) = std::sync::mpsc::channel();
    let (uv_tx, uv_rx) = std::sync::mpsc::channel();
    y_slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = y_tx.send(r);
    });
    uv_slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = uv_tx.send(r);
    });
    device
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: None,
        })
        .ok();
    y_rx.recv().context("Y map callback")?.context("Y map")?;
    uv_rx
        .recv()
        .context("UV map callback")?
        .context("UV map")?;

    let mut out = Vec::with_capacity((width * height + uv_width * layout.uv_height * 2) as usize);
    {
        let data = y_slice.get_mapped_range().context("Y mapped range")?;
        for row in 0..height {
            let start = (row * layout.y_padded_row_bytes) as usize;
            out.extend_from_slice(&data[start..start + layout.y_row_bytes as usize]);
        }
    }
    y_buffer.unmap();
    {
        let data = uv_slice.get_mapped_range().context("UV mapped range")?;
        for row in 0..layout.uv_height {
            let start = (row * layout.uv_padded_row_bytes) as usize;
            out.extend_from_slice(&data[start..start + layout.uv_row_bytes as usize]);
        }
    }
    uv_buffer.unmap();

    Ok(out)
}

pub struct Exporter {
    child: Child,
    stdin: Option<ChildStdin>,
    width: u32,
    height: u32,
    pix_fmt: PixFmt,
    pub frames: u64,
}

impl Exporter {
    pub fn new(width: u32, height: u32, fps: f64, output: &str, pix_fmt: PixFmt) -> Result<Self> {
        let size = format!("{width}x{height}");
        let rate = format!("{fps:.6}");

        // `HWA_EXPORT_ENCODER=x264` swaps the hardware encoder for libx264.
        //
        // Not a preference: i965 on this GPU drops the occasional input frame,
        // and with no B-frames the muxer absorbs the loss as a double-length
        // packet while every picture that referenced the missing one becomes
        // undecodable, so a whole GOP is lost each time it happens. x264 is
        // slower but encodes every frame it is given, which makes it the
        // control in that experiment and a usable fallback if the hardware
        // encoder cannot be tamed.
        // Hardware encoding is back on, via the driver that is still maintained.
        //
        // The failure this project spent a session diagnosing was never the
        // silicon: it was the *archived* i965 driver. Measured on this machine
        // with 899 synthetic black frames piped straight into ffmpeg, i965 lost
        // input frames (840 decodable) and once aborted on an
        // i965_MapBuffer2 assertion, leaving an mp4 with no moov atom. The same
        // test through iHD is 899/899, three runs out of three, and end to end
        // on real 1080p30 it runs at 42.4 fps with 899 frames written and 899
        // decodable -- against i965's 26.8 fps and 817.
        //
        // Intel's own platform table says BDW is `D/Es`: decode plus PAK+shader
        // encoding, never `E` (VDENC/HuC), so the EUs are still shared with
        // compute and there is no low-power block. It also says BDW encode only
        // exists in the Full-Feature build, which is what Arch ships as
        // `intel-media-driver`; the Free-Kernel build would expose decode only.
        let encoder = std::env::var("HWA_EXPORT_ENCODER").unwrap_or_else(|_| "vaapi".to_string());
        // Set on the child only, so the decode path keeps whatever driver the
        // system is configured for.
        let driver = std::env::var("HWA_EXPORT_VAAPI_DRIVER").unwrap_or_else(|_| "iHD".to_string());
        // ultrafast because this is a two-core 15 W CPU and the export should
        // finish: measured end to end on Jellyfish, 52.0 fps against veryfast's
        // 21.9, both with every frame present. The cost is a larger file, which
        // is what CRF is for; medium manages 6.9 fps.
        let preset = std::env::var("HWA_EXPORT_X264_PRESET")
            .unwrap_or_else(|_| "ultrafast".to_string());

        // `HWA_EXPORT_GOP` trades bits for blast radius.
        //
        // The i965 hardware encoder on this machine drops an input frame now and
        // then -- measured with nothing but black frames piped into ffmpeg, so
        // it is the encoder and not this pipeline. With `-bf 0` there are no
        // B-frames to reorder, so a dropped frame leaves every later picture in
        // its GOP referencing a frame that never arrived, and the decoder emits
        // nothing for them: one drop at `-g 60` cost 58 pictures in one run.
        // Halving the GOP halves that, at the cost of more I-frames.
        let gop = std::env::var("HWA_EXPORT_GOP").unwrap_or_else(|_| "30".to_string());
        let mut tail: Vec<String> = if encoder == "vaapi" {
            [
                "-vaapi_device",
                "/dev/dri/renderD128",
                "-vf",
                "format=nv12,hwupload",
                "-c:v",
                "h264_vaapi",
                "-rc_mode",
                "CQP",
                "-qp",
                "22",
                "-bf",
                "0",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect()
        } else {
            [
                "-c:v", "libx264", "-preset", &preset, "-crf", "20", "-bf", "0",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect()
        };
        tail.push("-g".to_string());
        tail.push(gop);

        let mut child = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "rawvideo",
                "-pix_fmt",
                pix_fmt.ffmpeg_name(),
                "-s",
                &size,
                "-r",
                &rate,
                "-i",
                "-",
            ])
            .args(&tail)
            .arg(output)
            .env("LIBVA_DRIVER_NAME", &driver)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .context("failed to spawn ffmpeg — is it on PATH?")?;

        let stdin = child.stdin.take().context("ffmpeg stdin unavailable")?;

        Ok(Self {
            child,
            stdin: Some(stdin),
            width,
            height,
            pix_fmt,
            frames: 0,
        })
    }

    pub fn write_frame(&mut self, bytes: &[u8]) -> Result<()> {
        let expected = self.pix_fmt.bytes_per_frame(self.width, self.height);
        if bytes.len() != expected {
            bail!(
                "frame size mismatch: got {} bytes, expected {}",
                bytes.len(),
                expected
            );
        }
        let stdin = self.stdin.as_mut().context("exporter already finished")?;
        stdin.write_all(bytes).context("write to ffmpeg failed")?;
        self.frames += 1;
        Ok(())
    }

    /// Close the pipe and wait for ffmpeg.
    ///
    /// Returns the frames written and whether ffmpeg exited cleanly. Those are
    /// separate questions because iHD aborts during teardown (`free(): invalid
    /// pointer`) *after* the muxer has finished, so a complete and correct file
    /// can arrive with a non-zero exit. The file is the source of truth, so
    /// callers verify the output rather than the status.
    pub fn finish(mut self) -> Result<(u64, bool)> {
        self.stdin.take();
        let status = self.child.wait().context("wait on ffmpeg failed")?;
        Ok((self.frames, status.success()))
    }

    /// Abandon the export, discarding whatever was written.
    pub fn kill(mut self) {
        self.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Count the video frames actually present in a finished file.
///
/// "The export ran" and "the export worked" are different claims, and on this
/// machine they came apart: the hardware encoder can lose input frames without
/// failing, and each loss takes the rest of its GOP with it. Counting what
/// ended up in the container is the only honest check, and it costs one process
/// spawn at the end of an export.
/// What a frame count attempt found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFrames {
    Counted(u64),
    /// ffprobe ran and could not read the file: no moov atom, truncated, or not
    /// a container at all. This is a failure, not an absence of information.
    Unreadable,
    /// No ffprobe on this machine.
    NoProbe,
}

/// Count the video frames in a finished file.
pub fn probe_output_frames(path: &str) -> OutputFrames {
    let out = match Command::new("ffprobe")
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
            path,
        ])
        .output()
    {
        Ok(o) => o,
        Err(_) => return OutputFrames::NoProbe,
    };
    match String::from_utf8_lossy(&out.stdout).trim().parse() {
        Ok(n) => OutputFrames::Counted(n),
        Err(_) => OutputFrames::Unreadable,
    }
}

pub fn count_output_frames(path: &str) -> Option<u64> {
    let out = Command::new("ffprobe")
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
            path,
        ])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exporter validates every frame against `width * height * 3 / 2`, so
    /// a mistake in the packed row maths shows up here rather than as a failed
    /// export at the end of a long run.
    fn assert_nv12_frame_size(width: u32, height: u32) {
        let layout = Nv12Readback::new(width, height);
        let bytes = layout.y_row_bytes as u64 * height as u64
            + layout.uv_row_bytes as u64 * layout.uv_height as u64;
        assert_eq!(
            bytes,
            width as u64 * height as u64 * 3 / 2,
            "packed NV12 size for {width}x{height}"
        );
    }

    #[test]
    fn a_1080p_frame_pads_its_rows_for_the_copy() {
        // 1920 is 7.5 rows of 256, so the copy buffer pad to 2048 while the
        // frame handed to ffmpeg stays 1920 wide.
        let layout = Nv12Readback::new(1920, 1080);
        assert_eq!(layout.y_row_bytes, 1920);
        assert_eq!(layout.y_padded_row_bytes, 2048);
        assert_eq!(layout.y_size, 2048 * 1080);
        assert_eq!(layout.uv_row_bytes, 1920);
        assert_eq!(layout.uv_padded_row_bytes, 2048);
        assert_eq!(layout.uv_height, 540);
        assert_eq!(layout.uv_size, 2048 * 540);
        assert_nv12_frame_size(1920, 1080);
    }

    #[test]
    fn unaligned_widths_pad_the_buffer_but_not_the_frame() {
        // 1366 does not divide by 256, so the copy is padded while the frame
        // that goes to ffmpeg stays exactly 1366 wide.
        let layout = Nv12Readback::new(1366, 768);
        assert_eq!(layout.y_row_bytes, 1366);
        assert_eq!(layout.y_padded_row_bytes, 1536);
        assert_eq!(layout.y_size, 1536 * 768);
        assert_eq!(layout.uv_height, 384);
        assert_nv12_frame_size(1366, 768);
    }

    #[test]
    fn odd_heights_truncate_the_uv_plane() {
        // NV12 assumes even dimensions and real video has them; truncating keeps
        // the plane sizes consistent with the textures rather than reading past
        // them.
        let layout = Nv12Readback::new(64, 37);
        assert_eq!(layout.uv_height, 18);
        assert!(layout.uv_size <= layout.y_size);
    }
}
