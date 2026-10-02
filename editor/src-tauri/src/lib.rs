extern crate gdk_wayland_sys;

mod wayland_subsurface;

use hwa_core::state::{ExportJob, ExportStage, SharedState, new_shared};
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
    println!("[{}] [frontend] {msg}", hwa_core::wall_clock_stamp());
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

/// The frontend tells the renderer how tall the bottom strip is, in logical
/// pixels, whenever a splitter moves.
#[tauri::command]
fn set_ui_height(size: State<'_, Arc<WindowSize>>, window: tauri::WebviewWindow, height: u32) {
    let scale = window.scale_factor().unwrap_or(1.0);
    size.ui_h
        .store(((height as f64) * scale).round() as u32, Ordering::Relaxed);
}

#[tauri::command]
fn set_paused(state: State<'_, SharedState>, paused: bool) {
    state.lock().unwrap().playing = !paused;
}

#[tauri::command]
fn seek_to(state: State<'_, SharedState>, ms: i64) {
    // An export is reading the decoder in its own order. A seek from the UI
    // during one used to move the decoder under it, and the export wrote 1878
    // frames of a 900 frame timeline.
    if state
        .lock()
        .unwrap()
        .export
        .as_ref()
        .is_some_and(|j| j.is_running() || j.stage == ExportStage::Verifying)
    {
        return;
    }
    // Deliberately not logged: an unthrottled drag calls this on every pointer
    // move. The periodic stats line reports the seek rate instead.
    //
    // The UI speaks timeline milliseconds and the renderer's queue speaks source
    // milliseconds, so this is where the edit is applied to a seek.
    let mut s = state.lock().unwrap();
    let clamped = ms.clamp(0, s.duration_ms.max(0));
    s.position_ms = clamped;
    s.pending_seek_ms = s.source_for_timeline(clamped).map(|(_, source)| source);
}

/// Cut the clip under the playhead in two, at the playhead.
#[tauri::command]
fn split_at_playhead(state: State<'_, SharedState>) -> Result<(), String> {
    let mut s = state.lock().unwrap();
    let at = s.position_ms;
    let Some((index, source)) = s.source_for_timeline(at) else {
        return Err("nothing to split".into());
    };
    let before = s.clips[index].clone();
    // A cut at either end would make a zero length clip, which is only a way to
    // lose frames.
    if source <= before.in_ms || source >= before.out_ms {
        return Err("the playhead is at a clip edge".into());
    }
    s.push_edit(hwa_core::state::Edit::SplitClip {
        index,
        before,
        at_source_ms: source,
    });
    s.pending_seek_ms = Some(source);
    Ok(())
}

/// Remove a clip from the timeline.
#[tauri::command]
fn delete_clip(state: State<'_, SharedState>, index: usize) -> Result<(), String> {
    let mut s = state.lock().unwrap();
    if index >= s.clips.len() {
        return Err(format!("no clip {index}"));
    }
    let before = s.clips[index].clone();
    s.push_edit(hwa_core::state::Edit::DeleteClip { index, before });
    if let Some((_, source)) = s.source_for_timeline(s.position_ms) {
        s.pending_seek_ms = Some(source);
    }
    Ok(())
}

/// Put the whole file back on the timeline as one clip.
#[tauri::command]
fn reset_timeline(state: State<'_, SharedState>) -> Result<(), String> {
    let mut s = state.lock().unwrap();
    let whole = s.clips.iter().map(|c| c.out_ms).max().unwrap_or(0);
    if whole <= 0 {
        return Err("no media loaded".into());
    }
    // Deliberately not an edit: this is the state a project starts in, and
    // undoing your way back into a cut timeline is not what anyone means by it.
    s.reset_timeline(whole);
    s.pending_seek_ms = Some(0);
    Ok(())
}

#[tauri::command]
fn undo(state: State<'_, SharedState>) -> bool {
    state.lock().unwrap().undo()
}

#[tauri::command]
fn redo(state: State<'_, SharedState>) -> bool {
    state.lock().unwrap().redo()
}

/// Reorder: put the clip at `from` where `to` is.
#[tauri::command]
fn move_clip(state: State<'_, SharedState>, from: usize, to: usize) -> Result<(), String> {
    let mut s = state.lock().unwrap();
    if from >= s.clips.len() || to >= s.clips.len() {
        return Err(format!("no clip {from} or {to}"));
    }
    if from == to {
        return Ok(());
    }
    s.push_edit(hwa_core::state::Edit::MoveClip { from, to });
    Ok(())
}

/// Change where a clip starts and ends in the source.
#[tauri::command]
fn trim_clip(
    state: State<'_, SharedState>,
    index: usize,
    in_ms: i64,
    out_ms: i64,
) -> Result<(), String> {
    let mut s = state.lock().unwrap();
    let before = s
        .clips
        .get(index)
        .cloned()
        .ok_or_else(|| format!("no clip {index}"))?;
    if out_ms <= in_ms {
        return Err("a clip has to be at least one frame long".into());
    }
    let mut after = before.clone();
    // In point first, then out point, each clamped against the other: a drag
    // that runs off the end of the media otherwise produces a negative length
    // clip, which the export would walk backwards through.
    after.in_ms = in_ms.max(0).min(before.out_ms - 1);
    after.out_ms = out_ms.max(after.in_ms + 1);
    if after == before {
        return Ok(());
    }
    s.push_edit(hwa_core::state::Edit::TrimClip {
        index,
        before,
        after,
    });
    if let Some((_, source)) = s.source_for_timeline(s.position_ms) {
        s.pending_seek_ms = Some(source);
    }
    Ok(())
}

/// Put a grade on a clip, so the effects panel has something to move.
#[tauri::command]
fn add_grade(state: State<'_, SharedState>, clip: usize) -> Result<(), String> {
    let mut s = state.lock().unwrap();
    if clip >= s.clips.len() {
        return Err(format!("no clip {clip}"));
    }
    // One grade per clip. A chain allows several nodes in principle, but two
    // grades is never what someone meant, and the second one silently doing
    // nothing to the second half of the picture is worse than refusing it.
    if s.clips[clip].effects.iter().any(|e| matches!(e, hwa_core::state::Effect::Grade(_))) {
        return Ok(());
    }
    let at = s.clips[clip].effects.len();
    s.push_edit(hwa_core::state::Edit::InsertEffect {
        clip,
        at,
        effect: hwa_core::state::Effect::Grade(hwa_core::state::GradeParams::default()),
    });
    Ok(())
}

/// Replace one effect. A slider drag calls this per pointer move, and the edit
/// spine folds those into a single undo step.
#[tauri::command]
fn set_effect(
    state: State<'_, SharedState>,
    clip: usize,
    at: usize,
    params: hwa_core::state::GradeParams,
) -> Result<(), String> {
    let mut s = state.lock().unwrap();
    let before = *s
        .clips
        .get(clip)
        .and_then(|c| c.effects.get(at))
        .ok_or_else(|| format!("no effect {at} on clip {clip}"))?;
    s.push_edit(hwa_core::state::Edit::SetEffect {
        clip,
        at,
        before,
        after: hwa_core::state::Effect::Grade(params),
    });
    Ok(())
}

#[tauri::command]
fn remove_effect(state: State<'_, SharedState>, clip: usize, at: usize) -> Result<(), String> {
    let mut s = state.lock().unwrap();
    let effect = *s
        .clips
        .get(clip)
        .and_then(|c| c.effects.get(at))
        .ok_or_else(|| format!("no effect {at} on clip {clip}"))?;
    s.push_edit(hwa_core::state::Edit::RemoveEffect { clip, at, effect });
    Ok(())
}

/// Where an export would go if the user just hits the button: next to the clip
/// being edited, with `-export` appended so the original is never overwritten.
#[tauri::command]
fn default_export_path(state: State<'_, SharedState>) -> String {
    let s = state.lock().unwrap();
    let source = std::path::Path::new(&s.video_path);
    let stem = source
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "export".to_string());
    let dir = source
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    dir.join(format!("{stem}-export.mp4")).to_string_lossy().into_owned()
}

#[tauri::command]
fn start_export(state: State<'_, SharedState>, output: String) -> Result<(), String> {
    let mut s = state.lock().unwrap();
    if s.export.as_ref().is_some_and(|job| job.is_running()) {
        return Err("an export is already running".into());
    }
    if output.trim().is_empty() {
        return Err("choose an output file first".into());
    }
    // The real frame count is whatever the decoder produces; this is only so
    // the progress bar has a denominator.
    let frames_total = ((s.duration_ms.max(0) as f64 / 1000.0) * s.fps).round() as u64;
    s.export = Some(ExportJob {
        output: output.trim().to_string(),
        frames_done: 0,
        frames_total,
        fps: 0.0,
        stage: ExportStage::Running,
        error: None,
        cancel: false,
    });
    Ok(())
}

#[tauri::command]
fn cancel_export(state: State<'_, SharedState>) {
    if let Some(job) = state.lock().unwrap().export.as_mut() {
        job.cancel = true;
    }
}

/// Count what is actually in the exported file, and report it.
///
/// Runs on its own thread because counting means decoding: `ffprobe
/// -count_frames` is the only check that catches pictures the muxer wrote and
/// the decoder cannot reconstruct, and it costs a full pass over the file.
fn verify_export(
    state: SharedState,
    window: tauri::WebviewWindow,
    output: String,
    frames: u64,
    clean_exit: bool,
    stamp: String,
) {
    match hwa_core::export::probe_output_frames(&output) {
        hwa_core::export::OutputFrames::Counted(in_file) if in_file >= frames => {
            println!("[{stamp}] export verified: {in_file} frames in the file");
            if !clean_exit {
                println!(
                    "[{stamp}] note: ffmpeg exited non-zero after writing a complete file \
                     (known iHD teardown abort)"
                );
            }
            report_export(&state, ExportStage::Done, None);
        }
        hwa_core::export::OutputFrames::Counted(in_file) => {
            let missing = frames.saturating_sub(in_file);
            println!(
                "[{stamp}] WARNING: wrote {frames} frames but the file holds {in_file}; \
                 {missing} were lost, and every picture referencing one is undecodable too."
            );
            report_export(
                &state,
                ExportStage::Failed,
                Some(format!("{in_file} of {frames} frames survived")),
            );
        }
        hwa_core::export::OutputFrames::Unreadable => {
            // ffprobe ran and could not open it. That is a broken export, and it
            // used to be reported as a missing ffprobe and then as success.
            println!("[{stamp}] WARNING: {output} cannot be read back; the container is broken");
            report_export(
                &state,
                ExportStage::Failed,
                Some("the exported file cannot be read back".into()),
            );
        }
        hwa_core::export::OutputFrames::NoProbe => {
            println!(
                "[{stamp}] export not verified (no ffprobe); ffmpeg exit was {}",
                if clean_exit { "clean" } else { "abnormal" }
            );
            report_export(
                &state,
                if clean_exit {
                    ExportStage::Done
                } else {
                    ExportStage::Failed
                },
                if clean_exit {
                    None
                } else {
                    Some("ffmpeg exited abnormally and the output could not be verified".into())
                },
            );
        }
    }
    if let Some(p) = export_progress(&state) {
        let _ = window.emit("export_progress", &p);
    }
}

fn report_export(state: &SharedState, stage: ExportStage, error: Option<String>) {
    if let Some(job) = state.lock().unwrap().export.as_mut() {
        job.stage = stage;
        job.error = error;
    }
}

fn export_progress(state: &SharedState) -> Option<hwa_core::state::ExportProgress> {
    state.lock().unwrap().export.as_ref().map(|job| job.progress())
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
    /// Height of the bottom strip, in physical pixels, as laid out by the
    /// frontend. The video pane is the window minus this, so a splitter drag has
    /// to reach the renderer or the subsurface sits at a size nothing matches.
    ui_h: AtomicU32,
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
        size.ui_h.store(ui_height_px, Ordering::Relaxed);
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
    let mut render_failures = 0u32;
        let mut last_emit_key = (true, 0i64, 0i64);
        let mut last_size = (config.width, config.height);
        let mut last_size_poll = Instant::now();
        let mut export_started = Instant::now();
        let mut last_export_emit = Instant::now();

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
                // The frontend owns the layout now: it reports the strip height
                // in logical pixels and this is the only place the scale is
                // applied.
                last_size_poll = Instant::now();
            }

            // Read the strip height every frame: it changes while a splitter is
            // being dragged, and the poll below only runs twice a second.
            let reported = size.ui_h.load(Ordering::Relaxed);
            ui_height_px = if reported > 0 {
                reported
            } else {
                (300.0 * scale) as u32
            };

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

            // ---- Export takes over the loop while one is running ----
            //
            // One frame per iteration, fed straight to the encoder instead of
            // the surface, as fast as the GPU can grade and read it back.
            let mut exporting_now = false;
            {
                let (wanted, output, cancel) = {
                    let s = renderer.state.lock().unwrap();
                    match s.export.as_ref() {
                        Some(job) => (job.is_running(), job.output.clone(), job.cancel),
                        None => (false, String::new(), false),
                    }
                };

                if wanted {
                    exporting_now = true;
                    if !renderer.is_exporting() {
                        match renderer.begin_export(&output) {
                            Ok(()) => export_started = Instant::now(),
                            Err(e) => {
                                report_export(
                                    &renderer.state,
                                    ExportStage::Failed,
                                    Some(format!("{e:?}")),
                                );
                                if let Some(p) = export_progress(&renderer.state) {
                                    let _ = window.emit("export_progress", &p);
                                }
                                exporting_now = false;
                            }
                        }
                    }
                }

                if exporting_now {
                    if cancel {
                        renderer.cancel_export();
                        report_export(&renderer.state, ExportStage::Cancelled, None);
                        if let Some(p) = export_progress(&renderer.state) {
                            let _ = window.emit("export_progress", &p);
                        }
                    } else {
                        let t = Instant::now();
                        match renderer.render_frame(&surface, &config) {
                            Ok(hwa_core::renderer::FrameOutcome::Eof) => {
                                match renderer.finish_export() {
                                    Ok((frames, clean_exit)) => {
                                        let stamp = hwa_core::wall_clock_stamp();
                                        println!(
                                            "[{stamp}] export finished: {frames} frames -> {output}"
                                        );
                                        // Verification decodes the whole file, which
                                        // on a 900 frame export is seconds. It used to
                                        // run here, on the render thread, holding the
                                        // state lock: the UI froze for the duration and
                                        // an edit clicked meanwhile looked like it had
                                        // hung. It runs on its own thread now and
                                        // reports through the same event the progress
                                        // bar already listens to.
                                        let state_for_check = renderer.state.clone();
                                        let window_for_check = window.clone();
                                        let output_for_check = output.clone();
                                        std::thread::spawn(move || {
                                            verify_export(
                                                state_for_check,
                                                window_for_check,
                                                output_for_check,
                                                frames,
                                                clean_exit,
                                                stamp,
                                            );
                                        });
                                        // Written, not yet checked. Not Running:
                                        // that is what starts an export.
                                        report_export(&renderer.state, ExportStage::Verifying, None);
                                    }
                                    Err(e) => report_export(
                                        &renderer.state,
                                        ExportStage::Failed,
                                        Some(format!("{e:?}")),
                                    ),
                                }
                                if let Some(p) = export_progress(&renderer.state) {
                                    let _ = window.emit("export_progress", &p);
                                }
                            }
                            Ok(_) => {
                                active_frames += 1;
                                let frames = renderer.export_frames();
                                let elapsed = export_started.elapsed().as_secs_f64().max(0.001);
                                {
                                    let mut s = renderer.state.lock().unwrap();
                                    if let Some(job) = s.export.as_mut() {
                                        job.frames_done = frames;
                                        job.fps = frames as f64 / elapsed;
                                    }
                                }
                                if last_export_emit.elapsed() >= Duration::from_millis(200) {
                                    if let Some(p) = export_progress(&renderer.state) {
                                        let _ = window.emit("export_progress", &p);
                                    }
                                    last_export_emit = Instant::now();
                                }
                            }
                            Err(e) => {
                                eprintln!("export frame failed: {e:?}");
                                renderer.cancel_export();
                                report_export(
                                    &renderer.state,
                                    ExportStage::Failed,
                                    Some(format!("{e:?}")),
                                );
                                if let Some(p) = export_progress(&renderer.state) {
                                    let _ = window.emit("export_progress", &p);
                                }
                            }
                        }
                        t_render += t.elapsed();
                    }
                }
            }

            if !exporting_now {
                let t = Instant::now();
                match renderer.render_frame(&surface, &config) {
                    Ok(hwa_core::renderer::FrameOutcome::Paused) => {}
                    Ok(_) => {
                        active_frames += 1;
                        render_failures = 0;
                    }
                    Err(e) => {
                        // A frame that fails is not a render loop that ends. The
                        // decoder and the driver both have recoverable states --
                        // a VA-API surface that will not sync, a seek that lands
                        // somewhere odd -- and breaking here turned one of those
                        // into a dead picture while the window carried on
                        // accepting pointer events. Give up only if it keeps
                        // failing, which means something is actually wrong.
                        render_failures += 1;
                        if render_failures <= 3 {
                            eprintln!("render_frame error ({render_failures}): {e:?}");
                        }
                        if render_failures > 120 {
                            eprintln!(
                                "render_frame has failed {render_failures} times in a row; stopping the render loop"
                            );
                            break;
                        }
                    }
                }
                t_render += t.elapsed();

                // Emitting crosses into the GTK main loop, so it blocks this thread
                // until the webview can take it; at 60 Hz that cost more than the
                // frame did. The UI does not need it mid-drag either, because the
                // playhead follows the pointer locally. So: only when something
                // actually changed, no faster than 20 Hz, and never while scrubbing.
                if last_emit.elapsed() >= Duration::from_millis(50) && !renderer.is_scrubbing() {
                    let (snap, dirty) = {
                        let mut s = renderer.state.lock().unwrap();
                        let dirty = s.dirty;
                        s.dirty = false;
                        (s.snapshot(), dirty)
                    };
                    // The key only covers what moves the playhead. An edit moves
                    // neither of those, so a document change has to force the
                    // emit or the panel sits on stale values until something
                    // else happens to change.
                    let key = (snap.playing, snap.position_ms, snap.duration_ms);
                    if key != last_emit_key || dirty {
                        let _ = window.emit("playhead_update", &snap);
                        last_emit_key = key;
                    }
                    last_emit = Instant::now();
                }
            }

            let period = if exporting_now {
                // No pacing while exporting: the encoder is the consumer, and
                // back-pressure comes from its pipe.
                Duration::ZERO
            } else {
                renderer.loop_period()
            };
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
                    "[{}] loop {:.1}/s | iter {:.2} ms | render {:.2} = chase {:.1} + map {:.1} + rest {:.1} + present {:.1} | map after-seek {:.1} (n{}) vs warm {:.1} (n{}) | presents {:.1}/s (ring {:.1}/s) seeks {:.1}/s | lag {:.0} ms | export grade {:.1} handoff {:.1} send {:.1} | pos {} / {} | playing {} | cache h{} m{} sz{}",
                    hwa_core::wall_clock_stamp(),
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
                    renderer.export_grade_time.as_secs_f64() * 1000.0 / active,
                    renderer.export_handoff_time.as_secs_f64() * 1000.0 / active,
                    renderer.export_write_time.as_secs_f64() * 1000.0 / active,
                    hwa_core::timecode(snap.position_ms, renderer.ff.fps),
                    hwa_core::timecode(snap.duration_ms, renderer.ff.fps),
                    snap.playing,                    renderer.cache_hits,
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
                renderer.export_grade_time = Duration::ZERO;
                renderer.export_handoff_time = Duration::ZERO;
                renderer.export_write_time = Duration::ZERO;
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
            set_ui_height,
            move_clip,
            trim_clip,
            seek_to,
            default_export_path,
            start_export,
            split_at_playhead,
            delete_clip,
            reset_timeline,
            undo,
            redo,
            add_grade,
            set_effect,
            remove_effect,
            cancel_export,
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
                ui_h: AtomicU32::new(0),
            });
            // The frontend reports the strip height through a command, so the
            // same Arc has to be reachable from the IPC side.
            app.manage(size.clone());
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
