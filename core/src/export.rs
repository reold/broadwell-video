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
                "-g",
                "60",
                output,
            ])
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

    pub fn finish(mut self) -> Result<u64> {
        self.stdin.take();
        let status = self.child.wait().context("wait on ffmpeg failed")?;
        if !status.success() {
            bail!("ffmpeg exited with {status}");
        }
        Ok(self.frames)
    }
}
