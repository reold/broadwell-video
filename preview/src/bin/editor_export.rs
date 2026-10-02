//! Drive the editor's export flow with no editor.
//!
//! Same renderer, same calls, same order as the render loop in
//! `editor/src-tauri/src/lib.rs`: `begin_export`, then `render_frame` until it
//! reports `Eof`, then `finish_export`, then count what is actually in the file.
//! The only thing missing is the webview, so a failed export can be reproduced
//! and iterated on without a human running the app and pasting logs.
//!
//! usage: editor-export <clip> <output>
//!
//! `hwa-preview`'s own `export` mode is left alone: it drives the subprocess
//! path and stays the reference the in-process path is compared against.

use anyhow::{Result, anyhow};
use hwa_core::renderer::{FrameOutcome, PreviewRenderer};
use hwa_core::state;
use std::sync::Arc;
use std::time::Instant;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::{Window, WindowId};

struct Runner {
    clip: String,
    output: String,
}

impl Runner {
    fn run(&mut self, event_loop: &ActiveEventLoop) -> Result<()> {
        let mut instance_desc = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
        instance_desc.backends = wgpu::Backends::VULKAN;
        let instance = wgpu::Instance::new(instance_desc);
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: None,
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        }))
        .map_err(|e| anyhow!("no Vulkan adapter: {e}"))?;

        // A window like the editor's, hidden: the export branch of render_frame
        // never presents, so the surface only has to exist.
        let (vw, vh, _fps) = unsafe { hwa_core::ffmpeg::peek_video_info(&self.clip)? };
        let window = Arc::new(
            event_loop
                .create_window(
                    Window::default_attributes()
                        .with_title("editor-export")
                        .with_inner_size(winit::dpi::PhysicalSize::new(vw, vh))
                        .with_visible(false),
                )
                .map_err(|e| anyhow!("create_window: {e}"))?,
        );
        let surface = instance
            .create_surface(window.clone())
            .map_err(|e| anyhow!("create_surface: {e}"))?;
        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .find(|f| !f.is_srgb())
            .copied()
            .unwrap_or(caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: vw,
            height: vh,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
            color_space: wgpu::SurfaceColorSpace::Srgb,
        };
        let device_desc = wgpu::DeviceDescriptor {
            label: Some("editor-export-device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            memory_hints: wgpu::MemoryHints::default(),
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            trace: wgpu::Trace::Off,
        };
        let host = grafting::vulkan_dmabuf::create_dmabuf_host_context(&adapter, &device_desc)
            .map_err(|e| anyhow!("create_dmabuf_host_context: {e:?}"))?;
        surface.configure(&host.device, &config);

        let state = state::new_shared();
        state.lock().unwrap().video_path = self.clip.clone();
        let mut renderer = PreviewRenderer::new(host, &self.clip, format, state)?;

        // ---- exactly what the editor's loop does ----
        renderer.begin_export(&self.output)?;
        let start = Instant::now();
        loop {
            match renderer.render_frame(&surface, &config)? {
                FrameOutcome::Eof => break,
                _ => {}
            }
        }
        let (written, _clean) = renderer.finish_export()?;
        let wall = start.elapsed();

        let in_file = hwa_core::export::count_output_frames(&self.output);
        let packets = std::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-count_packets",
                "-select_streams",
                "v",
                "-show_entries",
                "stream=nb_read_packets",
                "-of",
                "default=nw=1:nk=1",
                &self.output,
            ])
            .output()
            .ok()
            .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<u64>().ok());
        let per = |d: std::time::Duration| d.as_secs_f64() * 1000.0 / written.max(1) as f64;
        println!(
            "editor flow: {written} frames in {:.1}s = {:.1} fps",
            wall.as_secs_f64(),
            written as f64 / wall.as_secs_f64(),
        );
        // Packets separate the two ways an export comes up short: fewer packets
        // than frames means the encoder never produced them, more means the
        // container dropped pictures it was given.
        println!(
            "  packets in file: {:?} (for {written} frames sent)",
            packets
        );
        println!(
            "  grade {:.2} ms  handoff {:.2} ms  send {:.2} ms",
            per(renderer.export_grade_time),
            per(renderer.export_handoff_time),
            per(renderer.export_write_time),
        );
        // eprintln, not println: iHD aborts somewhere in teardown on this
        // machine and buffered stdout is lost when it does.
        match in_file {
            Some(n) if n >= written => {
                eprintln!("  verified: wrote {written}, the file holds {n}")
            }
            Some(n) => eprintln!("  FAILED: wrote {written}, the file holds {n} ({} lost)", written - n),
            None => eprintln!("  wrote {written}; ffprobe could not read the file"),
        }
        Ok(())
    }
}

impl ApplicationHandler for Runner {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if let Err(e) = self.run(event_loop) {
            eprintln!("harness failed: {e:?}");
            std::process::exit(1);
        }
        event_loop.exit();
    }

    fn window_event(&mut self, _event_loop: &ActiveEventLoop, _id: WindowId, _event: WindowEvent) {}
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let clip = args.next().ok_or_else(|| anyhow!("usage: editor-export <clip> <output>"))?;
    let output = args.next().ok_or_else(|| anyhow!("usage: editor-export <clip> <output>"))?;
    let event_loop = EventLoop::new().map_err(|e| anyhow!("EventLoop: {e}"))?;
    event_loop
        .run_app(&mut Runner { clip, output })
        .map_err(|e| anyhow!("run_app: {e}"))
}
