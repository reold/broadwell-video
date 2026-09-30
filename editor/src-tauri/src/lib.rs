extern crate gdk_wayland_sys;

mod wayland_subsurface;

use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
};
use std::{ffi::c_void, ptr::NonNull, time::Duration};
use tauri::Manager;
use wayland_subsurface::WaylandSubsurface;

#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {name}! IPC works.")
}

fn spawn_renderer(subsurface: WaylandSubsurface, window: tauri::WebviewWindow) {
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

        let (device, queue) =
            match pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("probe-device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                memory_hints: wgpu::MemoryHints::default(),
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
                trace: wgpu::Trace::Off,
            })) {
                Ok(pair) => pair,
                Err(e) => {
                    eprintln!("request_device failed: {e:?}");
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
        surface.configure(&device, &config);

        println!(
            "wgpu on subsurface: {}x{} {:?} @ {:?}",
            config.width,
            config.height,
            format,
            adapter.get_info().backend
        );

        let mut t: f32 = 0.0;
        let mut frame_count: u64 = 0;

        loop {
            if let Ok(sz) = window.inner_size() {
                if sz.width > 0 && sz.height > 0 {
                    let new_w = sz.width;
                    let new_h = sz.height.saturating_sub(ui_height_px).max(1);
                    if new_w != config.width || new_h != config.height {
                        config.width = new_w;
                        config.height = new_h;
                        surface.configure(&device, &config);
                    }
                }
            }

            // Solid red base with a subtle brightness pulse so the frame
            // updates are visibly happening. Replaced by the video blit later.
            t += 0.016;
            let pulse = (t.sin() * 0.1 + 0.9) as f64;
            let clear = wgpu::Color {
                r: pulse,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            };

            let frame = match surface.get_current_texture() {
                wgpu::CurrentSurfaceTexture::Success(f) => f,
                wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
                wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                    surface.configure(&device, &config);
                    continue;
                }
                other => {
                    eprintln!("surface acquire: {other:?}");
                    std::thread::sleep(Duration::from_millis(16));
                    continue;
                }
            };

            let view = frame
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default());
            let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("clear-encoder"),
            });
            {
                let _rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("clear-pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        depth_slice: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(clear),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
            }
            queue.submit([enc.finish()]);
            queue.present(frame);

            frame_count += 1;
            if frame_count % 120 == 0 {
                println!("wgpu frame {frame_count}");
            }

            std::thread::sleep(Duration::from_millis(16));
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

            spawn_renderer(subsurface, window);
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
