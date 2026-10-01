extern crate gdk_wayland_sys;

mod wayland_subsurface;

use hwa_core::state::{SharedState, new_shared};
use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
};
use std::{
    ffi::c_void,
    ptr::NonNull,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
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

/// Latest window size in physical pixels, published by the main thread.
///
/// The render loop used to call `window.inner_size()` every frame. That is a
/// synchronous round trip into the GTK main loop, and that loop is busy
/// compositing the webview, so it cost more than the frame itself did. Reading
/// two atomics costs nothing.
struct WindowSize {
    w: AtomicU32,
    h: AtomicU32,
}

fn spawn_video(
    subsurface: WaylandSubsurface,
    window: tauri::WebviewWindow,
    shared: SharedState,
    size: Arc<WindowSize>,
) {
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

        // One blocking read at startup is fine; the loop reads the atomics.
        let win_size = window.inner_size().unwrap_or(tauri::PhysicalSize {
            width: 1440,
            height: 900,
        });
        size.w.store(win_size.width, Ordering::Relaxed);
        size.h.store(win_size.height, Ordering::Relaxed);
        let mut scale = window.scale_factor().unwrap_or(1.0);
        let mut ui_height_px = (300.0 * scale) as u32;
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
        let mut active_frames: u64 = 0;
        let mut last_log = Instant::now();
        let mut t_total = Duration::ZERO;
        let mut t_render = Duration::ZERO;
        let mut last_emit = Instant::now();
        let mut last_emit_key = (true, 0i64, 0i64);
        let mut last_size = (config.width, config.height);
        let mut last_size_poll = Instant::now();

        loop {
            let iter_start = Instant::now();

            // Prefer the size the main thread publishes on resize events, and
            // poll slowly as a fallback in case this compositor sends none.
            if last_size_poll.elapsed() >= Duration::from_millis(500) {
                if let Ok(sz) = window.inner_size() {
                    if sz.width > 0 && sz.height > 0 {
                        size.w.store(sz.width, Ordering::Relaxed);
                        size.h.store(sz.height, Ordering::Relaxed);
                    }
                }
                scale = window.scale_factor().unwrap_or(scale);
                ui_height_px = (300.0 * scale) as u32;
                last_size_poll = Instant::now();
            }

            let win_w = size.w.load(Ordering::Relaxed);
            let win_h = size.h.load(Ordering::Relaxed);
            if win_w > 0 && win_h > 0 {
                let new_w = win_w;
                let new_h = win_h.saturating_sub(ui_height_px).max(1);
                if (new_w, new_h) != last_size {
                    config.width = new_w;
                    config.height = new_h;
                    surface.configure(&renderer.host.device, &config);
                    last_size = (new_w, new_h);
                }
            }

            let t = Instant::now();
            match renderer.render_frame(&surface, &config) {
                Ok(hwa_core::renderer::FrameOutcome::Paused) => {}
                Ok(_) => active_frames += 1,
                Err(e) => {
                    eprintln!("render_frame error: {e:?}");
                    break;
                }
            }
            t_render += t.elapsed();

            // Emitting crosses into the GTK main loop, so it blocks this thread
            // until the webview can take it; at 60 Hz that cost more than the
            // frame did. The UI does not need it mid-drag either, because the
            // playhead follows the pointer locally. So: only when something
            // actually changed, no faster than 20 Hz, and never while scrubbing.
            if last_emit.elapsed() >= Duration::from_millis(50) && !renderer.is_scrubbing() {
                let snap = renderer.state.lock().unwrap().snapshot();
                let key = (snap.playing, snap.position_ms, snap.duration_ms);
                if key != last_emit_key {
                    let _ = window.emit("playhead_update", &snap);
                    last_emit_key = key;
                }
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
                let active = active_frames.max(1) as f64;
                let secs = last_log.elapsed().as_secs_f64();
                let snap = renderer.state.lock().unwrap().snapshot();
                let lag = if renderer.lag_samples > 0 {
                    renderer.lag_ms_total as f64 / renderer.lag_samples as f64
                } else {
                    0.0
                };
                let map_seek = if renderer.map_seek_count > 0 {
                    renderer.map_seek_time.as_secs_f64() * 1000.0
                        / renderer.map_seek_count as f64
                } else {
                    0.0
                };
                let map_warm = if renderer.map_warm_count > 0 {
                    renderer.map_warm_time.as_secs_f64() * 1000.0
                        / renderer.map_warm_count as f64
                } else {
                    0.0
                };
                println!(
                    "loop {:.1}/s | iter {:.2} ms | render {:.2} = chase {:.1} + map {:.1} + rest {:.1} + present {:.1} | map after-seek {:.1} (n{}) vs warm {:.1} (n{}) | presents {:.1}/s (ring {:.1}/s) seeks {:.1}/s | lag {:.0} ms | pos {} / {} | playing {} | cache h{} m{} sz{}",
                    n / secs,
                    t_total.as_secs_f64() * 1000.0 / n,
                    t_render.as_secs_f64() * 1000.0 / n,
                    renderer.chase_time.as_secs_f64() * 1000.0 / active,
                    renderer.map_time.as_secs_f64() * 1000.0 / active,
                    renderer
                        .import_time
                        .saturating_sub(renderer.map_time)
                        .as_secs_f64()
                        * 1000.0
                        / active,
                    renderer.present_time.as_secs_f64() * 1000.0 / active,
                    map_seek,
                    renderer.map_seek_count,
                    map_warm,
                    renderer.map_warm_count,
                    renderer.presents as f64 / secs,
                    renderer.ring_presents as f64 / secs,
                    renderer.seeks as f64 / secs,
                    lag,
                    snap.position_ms,
                    snap.duration_ms,
                    snap.playing,
                    renderer.cache_hits,
                    renderer.cache_misses,
                    renderer.texture_cache.len(),
                );
                frames = 0;
                active_frames = 0;
                t_total = Duration::ZERO;
                t_render = Duration::ZERO;
                renderer.cache_hits = 0;
                renderer.cache_misses = 0;
                renderer.presents = 0;
                renderer.ring_presents = 0;
                renderer.seeks = 0;
                renderer.rewinds = 0;
                renderer.lag_ms_total = 0;
                renderer.lag_samples = 0;
                renderer.chase_time = Duration::ZERO;
                renderer.map_time = Duration::ZERO;
                renderer.map_seek_time = Duration::ZERO;
                renderer.map_seek_count = 0;
                renderer.map_warm_time = Duration::ZERO;
                renderer.map_warm_count = 0;
                renderer.import_time = Duration::ZERO;
                renderer.present_time = Duration::ZERO;
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

            // Publish the window size from here rather than letting the render
            // thread ask for it every frame.
            let size = Arc::new(WindowSize {
                w: AtomicU32::new(0),
                h: AtomicU32::new(0),
            });
            if let Ok(s) = window.inner_size() {
                size.w.store(s.width, Ordering::Relaxed);
                size.h.store(s.height, Ordering::Relaxed);
            }
            let size_for_events = size.clone();
            window.on_window_event(move |event| {
                if let tauri::WindowEvent::Resized(resized) = event {
                    size_for_events.w.store(resized.width, Ordering::Relaxed);
                    size_for_events.h.store(resized.height, Ordering::Relaxed);
                }
            });

            spawn_video(subsurface, window, shared.clone(), size);
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
