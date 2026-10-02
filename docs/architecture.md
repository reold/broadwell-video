# Architecture: what the well-built editors do, and what this one should do

This is a research note, not a description of the code as it stands. It exists
because this project has twice paid for an architectural shortcut — once for
trusting a driver nobody chose, once for a cache keyed on a recycled file
descriptor — and because the two features on the roadmap (clips, then an effect
chain) both want a spine that has not been built yet.

Sources are primary where possible and cited inline. Where a page returned only
navigation chrome, that is said rather than papered over.

## 1. The flow, as Apple builds it

Apple's own account of the media stack
([WWDC20, *Decode ProRes with AVFoundation and VideoToolbox*](https://developer.apple.com/videos/play/wwdc2020/10090/))
lays out the layers and the intent behind each:

- AVKit, AVFoundation, VideoToolbox, CoreMedia, CoreVideo, top to bottom.
  AVFoundation is the "powerful and flexible interface"; VideoToolbox is the
  "low-level interface for working with video decoders and encoders"; CoreMedia
  and CoreVideo are the building blocks.
- Video frames are `CVPixelBuffer`s: "wrappers around blocks of uncompressed
  raster image data", carrying pixel format, height, width, row bytes — and
  attachments like colour tags.
- `CMSampleBuffer` is the currency of the pipeline, and it wraps **either** a
  `CMBlockBuffer` of compressed data **or** a `CVPixelBuffer`. Compressed and
  uncompressed frames travel through the same pipe in the same type.
- Buffers come from a `CVPixelBufferPool`, and recycling is explicit: when a
  `CVPixelBuffer` is released its `IOSurface` returns to the pool to be handed
  out again.

The single most important sentence in that talk for this project:

> "the Video Toolbox runs decoders **out-of-process in a sandboxed server** …
> This both provides security benefits … but it also **adds application
> stability. If there is a crash in the video decoder, the result is a decode
> error rather than crashing your entire application.**"

Apple — with every incentive to go fast — deliberately puts a process boundary
between the application and the codec. Not for modularity. For the guarantee
that malformed or merely buggy codec code cannot take the editor down.

That is exactly the guarantee this project gave up. We moved encoding in-process
to get zero-copy, and within a day we had two consequences: the archived i965
driver's assertion abort (harmless when it killed a child `ffmpeg`, fatal when
it killed us) and an iHD double-free that ended the process inside
`finish_export` after every export. Both were survived only by workarounds — a
deliberate leak of the VA-API objects — that exist purely because there is no
process boundary.

## 2. The flow, as the editorial industry builds it

[OpenTimelineIO](https://github.com/AcademySoftwareFoundation/OpenTimelineIO),
the Academy Software Foundation's interchange format, is the closest thing the
industry has to an agreed editorial model, and its
[architecture document](https://raw.githubusercontent.com/AcademySoftwareFoundation/OpenTimelineIO/36764718900361680fc084ed2948cc41ea91828d/docs/tutorials/architecture.md)
is explicit:

- The canonical structure is `Timeline` → `tracks` (a `Stack`) → `Track`, whose
  children are `Clip`, `Gap`, `Stack`, `Track` or `Transition`. **Gaps and
  transitions are first-class items**, and containers nest.
- Time is `RationalTime`: "a point in time at `rt.value*(1/rt.rate)` seconds",
  rescalable between rates. Ranges are `TimeRange`: start plus duration, from
  which an inclusive and an exclusive end can both be derived.
- A clip's timing comes from two ranges: `media_reference.available_range` (what
  media exists) and `source_range` (what is cut in). `trimmed_range()` combines
  them, and `trimmed_range_in_parent()` places it in the parent's space.

Compare our own model: `Clip { in_ms, out_ms }` in source milliseconds, a flat
`Vec<Clip>`, no gaps, no transitions, no nesting, no media reference, and
integer milliseconds rather than rational time. It is the right *idea* — a clip
is a source range, which is OTIO's `source_range` — expressed in about a tenth
of the vocabulary and with a time representation that cannot express 29.97 fps
exactly.

## 3. What the GPU layer does

- Zero-copy from decoder to GPU is universal: Apple's `IOSurface` reaching Metal
  through a texture cache is the same shape as our DRM PRIME dma-buf reaching
  Vulkan through the patched grafting. We are not behind here.
- Presentation is where the well-built apps are careful. Vulkan's own
  documentation for
  [`VK_EXT_present_mode_fifo_latest_ready`](https://docs.vulkan.org/features/latest/features/proposals/VK_EXT_present_mode_fifo_latest_ready.html)
  states the problem we measured: "When an application manages to render
  multiple frames per display refresh cycle, `VK_PRESENT_MODE_FIFO_KHR` … can
  introduce some undesired latency, because only the oldest present request in
  the present queue is processed during each vertical blanking period. This also
  effectively caps an application's frame rate to the monitor refresh rate."
  `MAILBOX` fixes the latency but "precludes any useful interaction with
  time-based present APIs". The new mode dequeues several requests per vblank
  and shows the last ready one.
  Our preview measured 58.4 fps against a 60 Hz panel with `present` at 14.34 ms
  varying between 7 and 14 — a wait, not work. That is `FIFO` doing exactly what
  the spec says it does.
- Explicit synchronisation is the direction of travel: timeline semaphores
  ([Vulkanised 2026, *Solving All Synchronisation Problems with Timeline
  Semaphores*](https://vulkan.org/user/pages/09.events/vulkanised-2026/1115-Lucas-Silva-Devsh-TimelineSemaphores.pptx.pdf)),
  and Vulkan 1.4's `VK_KHR_internally_synchronized_queues`. Our path — implicit
  dma-buf fences plus a `poll(Wait)` per frame — is the older model.
- Decode and encode *inside* Vulkan exist and are listed in the feature
  registry: [`VK_KHR_video_decode_h264`](https://docs.vulkan.org/features/latest/features/index.html),
  `VK_KHR_video_encode_h264`, `VK_KHR_video_queue`. Broadwell's HasVK will not
  implement them, but they are where the API is going, and they would remove
  libavcodec from the frame path entirely.

Adobe's Mercury Playback Engine page was fetched and returned navigation only,
so nothing from it is claimed here.

## 4. Where this project already agrees with the good ones

Not everything needs changing. These are deliberate and match:

| Our design | The principle it matches |
|---|---|
| dma-buf → Vulkan import, no CPU copy | `IOSurface` → Metal texture cache |
| Preview and export share the grading code | one render engine for both (the reason the preview/export mismatch trap is real) |
| Bounded in-flight window on the encoder | a queue with back-pressure, not an unbounded one |
| Frame-count **and** per-frame chroma verification | not trusting the codec; the analogue of Apple's malformed-media stance |
| A ring of graded frames for scrubbing | a frame cache, however small |
| Clips as source ranges | OTIO's `source_range` |

## 5. Where it diverges, in priority order

**a. No codec isolation.** The one Apple guaranteed and we gave up. It is also
the cheap one to restore without losing zero-copy, because dma-bufs cross
process boundaries by design: a `hwa-codec` process owns libavcodec and VA-API,
hands the renderer plane descriptors over a Unix socket with `SCM_RIGHTS`, and a
crash there is a decode error rather than a dead editor. It would also let the
deliberate leak in `core/src/encoder.rs` become a process that simply dies.

**b. No document.** State is a bag of fields — `position_ms`, `clips`, `export`
— with no project file, no tracks, no gaps, no transitions, no media reference.
Every future feature (multi-file clips, effects, transitions, titles, undo)
wants the OTIO-shaped thing.

**c. Milliseconds, not rational time.** `29.97` is not 30, and 900 frames at
29.97 is 30.03 seconds, not 30. This already bit us once in the frontend, where
the timecode formatted at a hardcoded 30 fps. `RationalTime` is not academic.

**d. No command/undo spine.** Edits are ad-hoc Tauri commands:
`split_at_playhead` mutates the clip list in place. Undo needs each edit to be an
object that can produce its inverse, which is a spine, not a feature.

**e. Push where the pros pull.** We decode forward and render whatever comes;
the export *walks* the clip list. The alternative is a frame source asked for
the frame at time *T*, with preview, scrub and export as three consumers of it.
That is the shape that makes "export renders exactly what the preview showed"
structural rather than a thing to remember.

**f. No clock, and no audio.** Pacing is a sleep on the loop period, and there is
no audio at all. Professional playback drives presentation from a clock and
locks video to audio, because audio is the thing humans notice drifting.

**g. Implicit GPU synchronisation.** `poll(Wait)` on our own queue is a
correct-but-blunt answer to cross-engine ordering. The release barrier we
discussed and never needed is the same question in a smaller form.

## 6. A spine worth building, in order

1. **Rational time and the document.** `Time { frames, rate }`, `TimeRange`,
   `Track`, `Clip { media, source_range }`, `Gap`, and a JSON project file
   shaped like OTIO so it can interoperate later instead of being converted.
   Everything below hangs off this, and it is the change most likely to be
   regretted if deferred.
2. **Commands with inverses.** One enum of edits, an undo stack, and Tauri
   commands that are thin bookings over it. `split_at_playhead` becomes a
   constructor for `Edit::Split` rather than a mutation.
3. **Codec isolation.** The `hwa-codec` process described above. This is the
   robustness item, and the one place where our architecture is measurably worse
   than Apple's rather than merely different.
4. **A pull-based frame source**, with preview, scrub and export as consumers,
   and the frame cache keyed by (clip, source time) rather than by whatever
   recycling identity a driver happens to hand us.
5. **A clock**, then audio as its master. Presentation scheduled from a clock;
   `FIFO_LATEST_READY` where the driver offers it, `FIFO` with a shorter queue
   where it does not.
6. **Explicit synchronisation** between our queue and the video engine, if and
   when the implicit path proves insufficient — with the note that it has not
   yet, and that the last two "principled fixes" in this repo were both
   diagnosed from the wrong driver.

The effect chain, which is the next feature either way, does not need any of
this: it needs a parameters uniform and three shaders — `grade.wgsl` for the
preview, `grade_y.wgsl` and `grade_uv.wgsl` for the export — kept in step. It
will, however, be the first feature that *wants* the document and the undo
spine, and the first that would benefit from the frame cache being keyed on
time rather than identity.

---

## Addendum: the crate survey, and what to do next

A survey of the Rust ecosystem was done against the real constraint, which is
**wgpu 30.0.1**: wgpu types are not interchangeable across majors, so anything
pinned to wgpu 29 cannot share a device with this project. That single filter
decides most of the field. Findings, with the verdicts that matter:

**Shapes — adopt nothing.** Rects, rounded rects, ellipses and convex polygons
are analytic signed-distance fields evaluated in the compute pass this project
already dispatches, with anti-aliasing derived from the SDF gradient (`fwidth`)
rather than a fixed threshold. This is what Zed's GPUI does for its entire
editor chrome, and it is what Fret's ADR 0030 codifies. Shapes written this way
inherit the grade, produce the same packed NV12, and cost no extra pass.
`lyon_tessellation` 1.0.22 is the right tool only when strokes with joins,
concave polygons, holes or Béziers arrive — it is pure CPU tessellation with no
GPU coupling, so it fits wgpu 30 by construction.

**Text — `glyphon` 0.12.0.** The only crate that is simultaneously maintained,
wgpu-30-native, and able to shape complex scripts (it is cosmic-text plus
harfrust, and ships an Arabic sample). Two constraints found by reading its
source, not its README: its glyph atlas is `pub(crate)` and cannot be bound to
another pipeline, and its pipeline is built for exactly one colour-target
format. So the path is: render glyphon into an offscreen `Rgba8UnormSrgb`
texture inside a render pass we own, then sample that texture from the compute
pass. Roughly sixty lines. The upgrade path, if that format constraint bites, is
`cosmic-text` + `swash` + `etagere` and our own atlas — `SwashCache::get_image`
hands back `SwashContent::{Mask, SubpixelMask, Color}` and is exactly how
glyphon is built.

**Timeline — keep ours.** There is no maintained, published Rust
OpenTimelineIO. The crates.io entries are 2020 placeholders; the real
implementation (alchemist-editor/otio-rust, Apache-2.0) is five weeks old and
unpublished, so adopting it means a git dependency. `avio` 0.18.4 is the closest
architectural match but owns decode and encode, which would replace the VA-API +
dma-buf path. **`mediatime` 0.4.0 is the one adoption worth making:** `no_std`,
zero dependencies, `AVRational`-exact, with `Timebase`, `Timestamp`, `TimeRange`
and `Rate` as distinct types. It deletes the `f64`-seconds bug class this
project has now hit twice.

**Compositor — write it.** No crate composites a layer stack in wgpu. Every
project surveyed wrote its own: one dispatch per layer into a float accumulation
target, premultiplied alpha, ping-pong for blend modes. Roughly 300–600 lines,
and the absence is not a gap to wait on. `vello` is the trap — the published
crate pins wgpu 29 and the crate named `vello` is now an experimental renderer
under `research/`; `vello_gpu` on crates.io is a name reservation, not code.

**Prior art worth reading, not copying:** OpenTake (GPL-3.0, but the closest
twin — Rust core, Tauri 2, FFmpeg, wgpu, command-based undo, a Text panel) and
tooscut (714 stars, Rust compositor with keyframe animation).

### The next move, exactly

1. **Tracks.** `Project { tracks: Vec<Track> }` with `Track { clips }`, clips
   drawn bottom-up, and the transform already being the per-layer property. This
   is what "drag it to the upper layer" and "whatever layer is underneath"
   require, and the clip walk in `renderer.rs` is the thing that has to learn
   about it: today it walks one flat list by index.
2. **Shapes**, as an SDF `kind` on a generator layer, riding the same dispatch.
3. **Text**, per the glyphon note above.
4. **On-canvas handles**, which need the letterbox geometry in the snapshot so
   the frontend can map a pointer position into frame coordinates. That mapping
   is the whole reason the handles are not a pure-UI change.

Guard rail for all four: `editor_export` in `preview/src/bin/` is the only thing
that has caught the last four export bugs, because it can measure. Every feature
above needs a way to be measured there — frame counts, mean luma, or a new
probe — and not merely looked at.
