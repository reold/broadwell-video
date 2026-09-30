extern crate gdk_wayland_sys;

mod wayland_subsurface;

use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
};
use std::{ffi::c_void, ptr::NonNull};
use tauri::Manager;
use wayland_subsurface::WaylandSubsurface;

#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {name}! IPC works.")
}

fn spawn_video(subsurface: WaylandSubsurface, window: tauri::WebviewWindow) {
    let display_ptr = subsurface.display_ptr();
    let surface_ptr = subsurface.surface_ptr();

    std::thread::spawn(move || {
        let _subsurface = subsurface;

        let raw_display = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
            NonNull::new(display_ptr as *mut c_void).expect("non-null display"),
        ));
        let raw_window = RawWindowHandle::Wayland(WaylandWindowHandle::new(
            NonNull::new(surface_ptr as *mut c_void).expect("non-null surface"),
        ));

        let mut instance_desc = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
        instance_desc.backends = wgpu::Backends::VULKAN;
        let instance = wgpu::Instance::new(instance_desc);

        let surface = unsafe {
            instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: Some(raw_display),
                raw_window_handle: raw_window,
            })
        };
        let surface = match surface {
            Ok(s) => s,
            Err(e) => {
                eprintln!("create_surface_unsafe failed: {e:?}");
                return;
            }
        };

        let adapter =
            match pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::LowPower,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
                apply_limit_buckets: false,
            })) {
                Ok(a) => a,
                Err(e) => {
                    eprintln!("request_adapter failed: {e:?}");
                    return;
                }
            };

        // Reserve the bottom 300 logical px for the timeline UI.
        let win_size = window.inner_size().unwrap_or(tauri::PhysicalSize {
            width: 1440,
            height: 900,
        });
        let scale = window.scale_factor().unwrap_or(1.0);
        let ui_height_px = (300.0 * scale) as u32;
        let preview_width = win_size.width.max(1);
        let preview_height = win_size.height.saturating_sub(ui_height_px).max(1);

        // ---- Host context via grafting ----
        let device_desc = wgpu::DeviceDescriptor {
            label: Some("preview-device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            memory_hints: wgpu::MemoryHints::default(),
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            trace: wgpu::Trace::Off,
        };
        let host = match grafting::vulkan_dmabuf::create_dmabuf_host_context(&adapter, &device_desc)
        {
            Ok(h) => h,
            Err(e) => {
                eprintln!("create_dmabuf_host_context failed: {e:?}");
                return;
            }
        };

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .find(|f| !f.is_srgb())
            .copied()
            .unwrap_or(caps.formats[0]);

        let mut config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: preview_width,
            height: preview_height,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
            color_space: wgpu::SurfaceColorSpace::Srgb,
        };
        surface.configure(&host.device, &config);

        println!(
            "wgpu on subsurface: {}x{} {:?} @ {:?}",
            config.width,
            config.height,
            format,
            adapter.get_info().backend
        );

        let video_path = std::env::var("HWA_VIDEO")
            .unwrap_or_else(|_| "/home/reold/Downloads/jellyfish-15-mbps-hd-h264.mkv".into());

        // ---- Shared preview renderer ----
        let mut renderer = match hwa_core::renderer::PreviewRenderer::new(host, &video_path, format)
        {
            Ok(r) => r,
            Err(e) => {
                eprintln!("PreviewRenderer::new failed: {e:?}");
                return;
            }
        };

        // ---- Main loop ----
        let mut last_size = (config.width, config.height);
        loop {
            // Handle window resize
            if let Ok(sz) = window.inner_size() {
                if sz.width > 0 && sz.height > 0 {
                    let new_w = sz.width;
                    let new_h = sz.height.saturating_sub(ui_height_px).max(1);
                    if (new_w, new_h) != last_size {
                        config.width = new_w;
                        config.height = new_h;
                        surface.configure(&renderer.host.device, &config);
                        last_size = (new_w, new_h);
                    }
                }
            }

            match renderer.render_frame(&surface, &config) {
                Ok(_) => {}
                Err(e) => {
                    eprintln!("render_frame error: {e:?}");
                    break;
                }
            }

            // Pace to the source framerate.
            std::thread::sleep(renderer.frame_period());
        }
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![greet])
        .setup(|app| {
            let window = app.get_webview_window("main").unwrap();

            std::thread::sleep(std::time::Duration::from_millis(300));

            let gtk_window = window.gtk_window().expect("no GTK window");

            let subsurface = match WaylandSubsurface::new(&gtk_window) {
                Some(s) => s,
                None => {
                    eprintln!(
                        "Could not create Wayland subsurface. This build requires \
                         a Wayland session with wl_subcompositor support."
                    );
                    return Ok(());
                }
            };

            spawn_video(subsurface, window);
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
