extern crate gdk_wayland_sys;

mod wayland_subsurface;

use hwa_core::state::{SharedState, new_shared};
use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
};
use std::{
    ffi::c_void,
    ptr::NonNull,
    time::{Duration, Instant},
};
use tauri::{Emitter, Manager, State};
use wayland_subsurface::WaylandSubsurface;

#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {name}! IPC works.")
}

#[tauri::command]
fn log_msg(msg: String) {
    println!("[frontend] {msg}");
}

#[tauri::command]
fn get_state(state: State<'_, SharedState>) -> hwa_core::state::StateSnapshot {
    state.lock().unwrap().snapshot()
}

#[tauri::command]
fn toggle_play(state: State<'_, SharedState>) -> bool {
    let mut s = state.lock().unwrap();
    s.playing = !s.playing;
    s.playing
}

#[tauri::command]
fn set_paused(state: State<'_, SharedState>, paused: bool) {
    state.lock().unwrap().playing = !paused;
}

#[tauri::command]
fn seek_to(state: State<'_, SharedState>, ms: i64) {
    // Deliberately not logged: an unthrottled drag calls this on every pointer
    // move. The periodic stats line reports the seek rate instead.
    let mut s = state.lock().unwrap();
    s.pending_seek_ms = Some(ms);
    s.position_ms = ms;
}

fn spawn_video(subsurface: WaylandSubsurface, window: tauri::WebviewWindow, shared: SharedState) {
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

        let win_size = window.inner_size().unwrap_or(tauri::PhysicalSize {
            width: 1440,
            height: 900,
        });
        let scale = window.scale_factor().unwrap_or(1.0);
        let ui_height_px = (300.0 * scale) as u32;
        let preview_width = win_size.width.max(1);
        let preview_height = win_size.height.saturating_sub(ui_height_px).max(1);

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

        let mut renderer =
            match hwa_core::renderer::PreviewRenderer::new(host, &video_path, format, shared) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("PreviewRenderer::new failed: {e:?}");
                    return;
                }
            };

        let mut frames: u64 = 0;
        let mut last_log = Instant::now();
        let mut t_total = Duration::ZERO;
        let mut t_render = Duration::ZERO;
        let mut last_emit = Instant::now();
        let mut last_size = (config.width, config.height);

        loop {
            let iter_start = Instant::now();

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

            let t = Instant::now();
            match renderer.render_frame(&surface, &config) {
                Ok(_) => {}
                Err(e) => {
                    eprintln!("render_frame error: {e:?}");
                    break;
                }
            }
            t_render += t.elapsed();

            if last_emit.elapsed() >= Duration::from_millis(16) {
                let snap = renderer.state.lock().unwrap().snapshot();
                let _ = window.emit("playhead_update", &snap);
                last_emit = Instant::now();
            }

            let period = renderer.loop_period();
            let elapsed = iter_start.elapsed();
            if elapsed < period {
                std::thread::sleep(period - elapsed);
            }

            t_total += iter_start.elapsed();
            frames += 1;

            if last_log.elapsed() >= Duration::from_secs(2) {
                let n = frames as f64;
                let secs = last_log.elapsed().as_secs_f64();
                let snap = renderer.state.lock().unwrap().snapshot();
                let chase = if renderer.lag_samples > 0 {
                    renderer.lag_ms_total as f64 / renderer.lag_samples as f64
                } else {
                    0.0
                };
                println!(
                    "loop {:.1}/s | video {:.2} fps | iter {:.2} ms | render {:.2} | presents {:.1}/s seeks {:.1}/s rewinds {} | chase {:.0} ms | pos {} / {} | playing {} | cache h{} m{} sz{}",
                    n / secs,
                    renderer.ff.fps,
                    t_total.as_secs_f64() * 1000.0 / n,
                    t_render.as_secs_f64() * 1000.0 / n,
                    renderer.presents as f64 / secs,
                    renderer.seeks as f64 / secs,
                    renderer.rewinds,
                    chase,
                    snap.position_ms,
                    snap.duration_ms,
                    snap.playing,
                    renderer.cache_hits,
                    renderer.cache_misses,
                    renderer.texture_cache.len(),
                );
                frames = 0;
                t_total = Duration::ZERO;
                t_render = Duration::ZERO;
                renderer.cache_hits = 0;
                renderer.cache_misses = 0;
                renderer.presents = 0;
                renderer.seeks = 0;
                renderer.rewinds = 0;
                renderer.lag_ms_total = 0;
                renderer.lag_samples = 0;
                last_log = Instant::now();
            }
        }
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let shared = new_shared();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(shared.clone())
        .invoke_handler(tauri::generate_handler![
            greet,
            log_msg,
            get_state,
            toggle_play,
            set_paused,
            seek_to,
        ])
        .setup(move |app| {
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

            spawn_video(subsurface, window, shared.clone());
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
