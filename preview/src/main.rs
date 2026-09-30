mod app;

use anyhow::{Context, Result, anyhow};
use app::App;
use hwa_core::{ffmpeg, gpu};
use std::sync::Arc;
use std::time::Instant;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::{Window, WindowId};

struct Root {
    app: Option<App>,
    path: String,
    export_to: Option<String>,
    effect_passes: usize,
}

impl ApplicationHandler for Root {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.app.is_some() {
            return;
        }

        let hidden = self.export_to.is_some();
        match build_app(event_loop, &self.path, hidden, self.effect_passes) {
            Ok(a) => {
                self.app = Some(a);
                if let Some(output) = self.export_to.clone() {
                    let app = self.app.as_mut().unwrap();
                    match app.export(&output, usize::MAX) {
                        Ok(n) => println!("exported {n} frames → {output}"),
                        Err(e) => eprintln!("export failed: {e:?}"),
                    }
                    event_loop.exit();
                }
            }
            Err(e) => {
                eprintln!("initialization failed: {e:?}");
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(app) = self.app.as_mut() else { return };

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(new_size) => {
                if new_size.width == 0 || new_size.height == 0 {
                    return;
                }
                app.surface_config.width = new_size.width;
                app.surface_config.height = new_size.height;
                app.surface.configure(&app.host.device, &app.surface_config);
            }
            WindowEvent::RedrawRequested => {
                if let Err(e) = app.render_one_frame() {
                    eprintln!("render error: {e:?}");
                    event_loop.exit();
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(app) = self.app.as_ref() {
            app.window.request_redraw();
        }
    }
}

fn build_app(
    event_loop: &ActiveEventLoop,
    path: &str,
    hidden: bool,
    effect_passes: usize,
) -> Result<App> {
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

    let info = adapter.get_info();
    println!("Adapter: {} ({:?})", info.name, info.backend);

    let (vw, vh, fps) = unsafe { ffmpeg::peek_video_info(path)? };
    println!("Video: {vw}x{vh} @ {fps:.2} fps");
    println!("Effect passes (diagnostic): {effect_passes}");

    let mut attrs = Window::default_attributes()
        .with_title("HWA Video Preview")
        .with_inner_size(winit::dpi::PhysicalSize::new(vw, vh));
    if hidden {
        attrs = attrs.with_visible(false);
    }
    let window = Arc::new(
        event_loop
            .create_window(attrs)
            .map_err(|e| anyhow!("create_window: {e}"))?,
    );

    let surface = instance
        .create_surface(window.clone())
        .map_err(|e| anyhow!("create_surface: {e}"))?;
    let caps = surface.get_capabilities(&adapter);
    let surface_format = caps
        .formats
        .iter()
        .find(|f| !f.is_srgb())
        .copied()
        .unwrap_or(caps.formats[0]);
    println!("Surface format: {surface_format:?}");

    let surface_config = wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format: surface_format,
        width: vw,
        height: vh,
        present_mode: wgpu::PresentMode::Fifo,
        alpha_mode: caps.alpha_modes[0],
        view_formats: vec![],
        desired_maximum_frame_latency: 2,
        color_space: wgpu::SurfaceColorSpace::Srgb,
    };

    let device_desc = wgpu::DeviceDescriptor {
        label: Some("preview-device"),
        required_features: wgpu::Features::empty(),
        required_limits: wgpu::Limits::default(),
        memory_hints: wgpu::MemoryHints::default(),
        experimental_features: wgpu::ExperimentalFeatures::disabled(),
        trace: wgpu::Trace::Off,
    };
    let host = grafting::vulkan_dmabuf::create_dmabuf_host_context(&adapter, &device_desc)
        .map_err(|e| anyhow!("create_dmabuf_host_context: {e:?}"))?;
    surface.configure(&host.device, &surface_config);

    let ff = unsafe { ffmpeg::Handles::open(path, "/dev/dri/renderD128")? };
    let pipelines = gpu::build_pipelines(&host, surface_format, vw, vh);

    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let y_row_bytes = vw;
    let y_src_padded = y_row_bytes.div_ceil(align) * align;
    let y_buf_size = (y_src_padded * vh) as u64;

    let uv_row_bytes = (vw / 2) * 2;
    let uv_src_padded = uv_row_bytes.div_ceil(align) * align;
    let uv_buf_size = (uv_src_padded * (vh / 2)) as u64;

    let readback_y_buffer = host.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("nv12-y-readback"),
        size: y_buf_size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let readback_uv_buffer = host.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("nv12-uv-readback"),
        size: uv_buf_size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    Ok(App {
        window,
        surface,
        surface_config,
        host,
        pipelines,
        ff,
        texture_cache: std::collections::HashMap::new(),
        readback_y_buffer,
        readback_uv_buffer,
        frames: 0,
        last_log: Instant::now(),
        timing_decode: std::time::Duration::ZERO,
        timing_filter: std::time::Duration::ZERO,
        timing_import: std::time::Duration::ZERO,
        timing_gpu: std::time::Duration::ZERO,
        cache_hits: 0,
        cache_misses: 0,
        last_report_elapsed: 0.0,
        effect_passes,
    })
}

fn main() -> Result<()> {
    env_logger::init();

    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .unwrap_or_else(|| "/tmp/h264_test.mp4".to_string());

    let export_to = match args.next().as_deref() {
        Some("export") => {
            let out = args.next().context("export requires an output path")?;
            Some(out)
        }
        _ => None,
    };

    let effect_passes: usize = std::env::var("EFFECT_PASSES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let event_loop = EventLoop::new().map_err(|e| anyhow!("EventLoop::new: {e}"))?;
    let mut root = Root {
        app: None,
        path,
        export_to,
        effect_passes,
    };
    event_loop
        .run_app(&mut root)
        .map_err(|e| anyhow!("event loop: {e}"))?;
    Ok(())
}
