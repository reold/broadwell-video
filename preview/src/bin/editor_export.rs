//! Drive the editor's export flow with no editor.
//!
//! Same renderer, same calls, same order as the render loop in
//! `editor/src-tauri/src/lib.rs`: `begin_export`, then `render_frame` until it
//! reports `Eof`, then `finish_export`, then count what is actually in the file.
//! The only thing missing is the webview, so a failed export can be reproduced
//! and iterated on without a human running the app and pasting logs.
//!
//! usage: editor_export <clip> <output>          export the whole clip
//!        editor_export <clip> play <frames>      present frames, no export
//!        editor_export <clip> scrub <seconds>    drag the playhead about, no export
//!        editor_export <clip> grade <steps>      drag a parameter slider, no export
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
    /// Present instead of exporting, for this many frames. A hidden window, so
    /// what it measures is the preview's own compute rather than the wait for a
    /// compositor that has nothing to show.
    play_frames: Option<u64>,
    /// Seconds of hard scrubbing, for reproducing what a drag can do to the
    /// decoder and the driver.
    scrub_seconds: Option<f64>,
    /// Slider steps to make while paused, to check the picture stays put.
    grade_steps: Option<u32>,
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

        // Optional: put a grade on the first clip, so the export can be shown to
        // depend on it. HWA_TEST_EXPOSURE=1.0 should brighten the file.
        if let Ok(spec) = std::env::var("HWA_TEST_EXPOSURE") {
            if let Ok(exposure) = spec.trim().parse::<f32>() {
                let mut s = state.lock().unwrap();
                s.clips = vec![hwa_core::state::Clip {
                    in_ms: 0,
                    out_ms: 3_600_000,
                    effects: vec![hwa_core::state::Effect::Grade(hwa_core::state::GradeParams {
                        exposure,
                        ..Default::default()
                    })],
                    transform: hwa_core::state::Transform::default(),
                }];
                s.refresh_duration();
                println!("grade: exposure {exposure:+.2} EV on clip 1");
            }
        }

        // Optional: cut the timeline up, so the clip walk is exercised. Source
        // ranges in milliseconds, comma separated:
        //   HWA_TEST_CLIPS="0-5000,10000-15000"
        if let Ok(spec) = std::env::var("HWA_TEST_CLIPS") {
            let clips: Vec<hwa_core::state::Clip> = spec
                .split(',')
                .filter_map(|part| {
                    let (a, b) = part.split_once('-')?;
                    Some(hwa_core::state::Clip::new(
                        a.trim().parse().ok()?,
                        b.trim().parse().ok()?,
                    ))
                })
                .collect();
            if !clips.is_empty() {
                let mut s = state.lock().unwrap();
                s.clips = clips;
                s.refresh_duration();
                println!(
                    "timeline: {} clips, {:.2}s",
                    s.clips.len(),
                    s.duration_ms as f64 / 1000.0
                );
            }
        }
        // Optional: exercise an edit and its undo, which is the path a delete
        // and Ctrl+Z take. HWA_TEST_UNDO=delete:1
        if let Ok(spec) = std::env::var("HWA_TEST_UNDO") {
            let (op, arg) = spec.split_once(':').unwrap_or((spec.as_str(), "0"));
            let index = arg.trim().parse::<usize>().unwrap_or(0);
            let mut s = state.lock().unwrap();
            if op.trim() == "delete" && index < s.clips.len() {
                let before = s.clips[index].clone();
                s.push_edit(hwa_core::state::Edit::DeleteClip { index, before });
                println!(
                    "undo test: deleted clip {index}, timeline {:.2}s in {} clips",
                    s.duration_ms as f64 / 1000.0,
                    s.clips.len()
                );
                s.undo();
                println!(
                    "undo test: undone, timeline {:.2}s in {} clips",
                    s.duration_ms as f64 / 1000.0,
                    s.clips.len()
                );
            }
        }

        let mut renderer = PreviewRenderer::new(host, &self.clip, format, state.clone())?;

        // Optional: shrink the picture, so the black around it can be measured.
        // After the renderer, because building it is what gives an untouched
        // project its one clip covering the whole file.
        if let Ok(spec) = std::env::var("HWA_TEST_SCALE") {
            if let Ok(scale) = spec.trim().parse::<f32>() {
                let mut s = state.lock().unwrap();
                for clip in s.clips.iter_mut() {
                    clip.transform.scale = scale;
                }
                s.look_version += 1;
                println!(
                    "transform: scale {scale} on {} clips",
                    s.clips.len()
                );
            }
        }

        if let Some(steps) = self.grade_steps {
            // Settle on a frame, then move a parameter the way a drag does. The
            // position must not change: a look change re-grades the frame that is
            // on screen, and an earlier attempt let the decoder carry on, so each
            // step walked the video forward.
            state.lock().unwrap().playing = false;
            let mid = if std::env::var("HWA_TEST_GRADE_AT").is_ok() {
                std::env::var("HWA_TEST_GRADE_AT")
                    .ok()
                    .and_then(|v| v.parse::<i64>().ok())
                    .unwrap_or(0)
            } else {
                state.lock().unwrap().duration_ms / 2
            };
            let mut s = state.lock().unwrap();
            s.position_ms = mid;
            s.pending_seek_ms = s.source_for_timeline(mid).map(|(_, src)| src);
            drop(s);
            for _ in 0..4 {
                let _ = renderer.render_frame(&surface, &config);
            }
            let start = state.lock().unwrap().position_ms;
            println!("grade: settled at {start} ms");
            let mut positions = Vec::new();
            for step in 0..steps {
                let mut s = state.lock().unwrap();
                let clip = 0usize;
                if s.clips[clip].effects.is_empty() {
                    s.clips[clip].effects = vec![hwa_core::state::Effect::Grade(
                        hwa_core::state::GradeParams::default(),
                    )];
                }
                let before = s.clips[clip].effects[0];
                let exposure = 0.05 * (step as f32 + 1.0);
                let after = hwa_core::state::Effect::Grade(hwa_core::state::GradeParams {
                    exposure,
                    ..Default::default()
                });
                s.push_edit(hwa_core::state::Edit::SetEffect {
                    clip,
                    at: 0,
                    before,
                    after,
                });
                drop(s);
                for _ in 0..3 {
                    let _ = renderer.render_frame(&surface, &config);
                }
                let now = state.lock().unwrap().position_ms;
                positions.push(now);
            }
            let moved = positions.windows(2).filter(|w| w[0] != w[1]).count();
            let drift = positions.last().copied().unwrap_or(0) - start;
            println!(
                "grade: {} steps, position {} -> {} ({} changes, drift {} ms)",
                steps,
                start,
                positions.last().copied().unwrap_or(0),
                moved,
                drift
            );
            if moved == 0 {
                println!("grade: the picture did not move");
            } else {
                println!("grade: FAILED, the picture moved while a parameter was changed");
            }
            return Ok(());
        }

        if let Some(seconds) = self.scrub_seconds {
            state.lock().unwrap().playing = false;
            let duration = {
                let s = state.lock().unwrap();
                s.duration_ms.max(1)
            };
            let deadline = Instant::now() + std::time::Duration::from_secs_f64(seconds);
            let (mut shown, mut skipped, mut errors) = (0u64, 0u64, 0u64);
            let mut seed = 0x2545_F491_4F6C_DD1Du64;
            let mut next_seek = Instant::now();
            while Instant::now() < deadline {
                if Instant::now() >= next_seek {
                    // A drag, roughly: a new target every 30 ms, anywhere.
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    let target = ((seed >> 33) % duration as u64) as i64;
                    let mut s = state.lock().unwrap();
                    s.position_ms = target;
                    s.pending_seek_ms = s.source_for_timeline(target).map(|(_, src)| src);
                    next_seek = Instant::now() + std::time::Duration::from_millis(30);
                }
                match renderer.render_frame(&surface, &config) {
                    Ok(hwa_core::renderer::FrameOutcome::Skipped) => skipped += 1,
                    // Anything that is not a skip or a pause put a frame through
                    // the pipeline. Counting every Ok as "presented" counted the
                    // pauses between seeks, which is most of a scrub.
                    Ok(hwa_core::renderer::FrameOutcome::Paused) => {}
                    Ok(_) => shown += 1,
                    Err(e) => {
                        errors += 1;
                        if errors <= 3 {
                            eprintln!("scrub error: {e:?}");
                        }
                    }
                }
            }
            println!(
                "scrub: {shown} presented, {skipped} skipped, {errors} errors, {} map failures",
                renderer.map_failures
            );
            return Ok(());
        }

        if let Some(frames) = self.play_frames {
            state.lock().unwrap().playing = true;
            let start = Instant::now();
            let mut rendered = 0u64;
            for _ in 0..frames {
                renderer.render_frame(&surface, &config)?;
                rendered += 1;
            }
            let wall = start.elapsed();
            let per = |d: std::time::Duration| d.as_secs_f64() * 1000.0 / rendered.max(1) as f64;
            println!(
                "play: {rendered} frames in {:.1}s = {:.1} fps",
                wall.as_secs_f64(),
                rendered as f64 / wall.as_secs_f64(),
            );
            println!(
                "  chase {:.2} ms  map {:.2} ms  import {:.2} ms  present {:.2} ms",
                per(renderer.chase_time),
                per(renderer.map_time),
                per(renderer.import_time),
                per(renderer.present_time),
            );
            return Ok(());
        }

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
        // Content: the grade has to reach the pixels, and the only way to know
        // is to look at them.
        let stats = std::process::Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-i",
                &self.output,
                "-vf",
                "signalstats,metadata=print:file=-",
                "-f",
                "null",
                "-",
            ])
            .output();
        if let Ok(stats) = stats {
            let text = String::from_utf8_lossy(&stats.stdout);
            // The keys arrive in whatever order ffmpeg feels like -- YAVG comes
            // before UAVG, which is not alphabetical -- so accumulate both and
            // flush on the frame header rather than assuming sequence.
            let (mut n, mut blank, mut luma_sum) = (0u64, 0u64, 0f64);
            let (mut u, mut y) = (None::<f64>, None::<f64>);
            for line in text.lines() {
                if line.starts_with("frame:") {
                    if let (Some(u), Some(y)) = (u.take(), y.take()) {
                        n += 1;
                        luma_sum += y;
                        if u < 20.0 {
                            blank += 1;
                        }
                    }
                    continue;
                }
                if let Some(rest) = line.split("UAVG=").nth(1) {
                    u = rest.trim().parse::<f64>().ok();
                }
                if let Some(rest) = line.split("YAVG=").nth(1) {
                    y = rest.trim().parse::<f64>().ok();
                }
            }
            if let (Some(u), Some(y)) = (u, y) {
                n += 1;
                luma_sum += y;
                if u < 20.0 {
                    blank += 1;
                }
            }
            if n > 0 {
                println!(
                    "  content: {n} frames, {blank} with no chroma, mean luma {:.1}",
                    luma_sum / n as f64
                );
            }
        }

        // An export that walked only part of the timeline used to verify clean,
        // because the check only asks whether what was written is readable. The
        // timeline knows how many frames it asked for.
        let (duration_ms, fps_now) = {
            let s = state.lock().unwrap();
            (s.duration_ms, s.fps)
        };
        let expected = (duration_ms as f64 * fps_now / 1000.0).round() as u64;
        // One frame per clip boundary lands on both sides of the cut, so the
        // tolerance is a frame per clip rather than a fixed two.
        let tolerance = 2 + {
            let s = state.lock().unwrap();
            s.clips.len() as u64
        };
        if expected > 0 && written.abs_diff(expected) > tolerance {
            eprintln!(
                "WARNING: the timeline asked for about {expected} frames and the export wrote {written}"
            );
        }

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
    let output = args.next().ok_or_else(|| anyhow!("usage: editor_export <clip> <output|play> [frames]"))?;
    let play_frames = if output == "play" {
        Some(args.next().unwrap_or_else(|| "300".to_string()).parse()?)
    } else {
        None
    };
    let scrub_seconds = if output == "scrub" {
        Some(args.next().unwrap_or_else(|| "20".to_string()).parse()?)
    } else {
        None
    };
    let grade_steps = if output == "grade" {
        Some(args.next().unwrap_or_else(|| "12".to_string()).parse()?)
    } else {
        None
    };
    let event_loop = EventLoop::new().map_err(|e| anyhow!("EventLoop: {e}"))?;
    event_loop
        .run_app(&mut Runner {
            clip,
            output,
            play_frames,
            scrub_seconds,
            grade_steps,
        })
        .map_err(|e| anyhow!("run_app: {e}"))
}
