<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";
  import { onMount } from "svelte";

  type GradeParams = {
    exposure: number;
    contrast: number;
    saturation: number;
    gamma: number;
  };

  /// The Rust enum serialises as an externally tagged object, one key per
  /// variant. One variant today.
  type Effect = { Grade: GradeParams };

  type Transform = {
    scale: number;
    offset_x: number;
    offset_y: number;
    rotation: number;
  };

  type Clip = {
    in_ms: number;
    out_ms: number;
    effects: Effect[];
    transform: Transform;
  };

  /// Where a clip's picture sits in the frame. Scale 1 fills it; anything
  /// smaller leaves the rest black, which is what a layer underneath will
  /// eventually show through.
  const TRANSFORM_CONTROLS: {
    k: keyof Transform;
    label: string;
    min: number;
    max: number;
    step: number;
  }[] = [
    { k: "scale", label: "Scale", min: 0.05, max: 3, step: 0.01 },
    { k: "offset_x", label: "X", min: -1, max: 1, step: 0.01 },
    { k: "offset_y", label: "Y", min: -1, max: 1, step: 0.01 },
    { k: "rotation", label: "Rotate", min: -3.15, max: 3.15, step: 0.01 },
  ];

  const TRANSFORM_DEFAULTS: Transform = {
    scale: 1,
    offset_x: 0,
    offset_y: 0,
    rotation: 0,
  };

  /// Every icon is a path, never a character.
  ///
  /// Transport glyphs and arrows in a font are emoji on some systems and
  /// missing on others, and they cannot take the theme's colour reliably. A
  ///  16x16 path drawn in currentColor can.
  const ICON: Record<string, string> = {
    start: "M4 3h1.6v10H4zM13 3L6.8 8 13 13z",
    rewind: "M2.5 3L8 8l-5.5 5zM8.5 3L14 8l-5.5 5z",
    stepBack: "M12 3L6 8l6 5zM3.4 3H5v10H3.4z",
    play: "M5 3l8 5-8 5z",
    pause: "M5 3h2.2v10H5zM8.8 3H11v10H8.8z",
    stepForward: "M4 3l6 5-6 5zM11 3h1.6v10H11z",
    forward: "M7.5 3L2 8l5.5 5zM13.5 3L8 8l5.5 5z",
    end: "M12 3h1.6v10H12zM3 3l6.2 5L3 13z",
    undo: "M7 4.2V1.5L2.4 5.5 7 9.5V6.8c2.6.1 4.4 1.6 4.6 4.2.6-4.3-1.7-6.7-4.6-6.8z",
    redo: "M9 4.2V1.5l4.6 4L9 9.5V6.8c-2.6.1-4.4 1.6-4.6 4.2-.6-4.3 1.7-6.7 4.6-6.8z",
    close: "M11.6 5.4L8.9 8l2.7 2.6-1 1L8 9l-2.6 2.6-1-1L7.1 8 4.4 5.4l1-1L8 7l2.6-2.6z",
    reset: "M8 3.2V1.2L4.2 4 8 6.8V4.7a3.6 3.6 0 1 1-3.5 4.4H3a5 5 0 1 0 5-5.9z",
  };

  /// What a reset returns a parameter to. Kept beside the controls so the two
  /// cannot drift: these are the values the shaders shipped with.
  const GRADE_DEFAULTS: GradeParams = {
    exposure: 0,
    contrast: 1,
    saturation: 1.4,
    gamma: 1.1,
  };

  const GRADE_CONTROLS: {
    k: keyof GradeParams;
    label: string;
    min: number;
    max: number;
    step: number;
  }[] = [
    { k: "exposure", label: "Exposure", min: -2, max: 2, step: 0.05 },
    { k: "contrast", label: "Contrast", min: 0, max: 2, step: 0.02 },
    { k: "saturation", label: "Saturation", min: 0, max: 3, step: 0.05 },
    { k: "gamma", label: "Gamma", min: 0.2, max: 3, step: 0.05 },
  ];

  type StateSnapshot = {
    playing: boolean;
    position_ms: number;
    duration_ms: number;
    fps: number;
    clips: Clip[];
    undo_depth: number;
    redo_depth: number;
    video_width: number;
    video_height: number;
  };

  type ExportProgress = {
    stage: "idle" | "running" | "done" | "failed" | "cancelled";
    frames_done: number;
    frames_total: number;
    fps: number;
    output: string;
    error: string | null;
  };

  let exportPath = $state("");
  let exportState = $state<ExportProgress | null>(null);
  let exportError = $state("");

  let exportRunning = $derived(exportState?.stage === "running");
  let exportPercent = $derived(
    exportState && exportState.frames_total > 0
      ? Math.min(100, (exportState.frames_done / exportState.frames_total) * 100)
      : 0
  );

  let clips = $state<Clip[]>([]);
  let selectedClip = $state(0);
  let videoW = $state(0);
  let videoH = $state(0);

  // ---- Direct manipulation of the picture ----
  //
  // The video is drawn aspect-fit inside its pane, so the picture has two
  // coordinate spaces: the pane's pixels, and the frame's fractions. Handles
  // live in the first and the transform lives in the second; every gesture
  // below is a conversion between them.
  let paneEl: HTMLDivElement | undefined = $state();
  let paneSize = $state({ w: 0, h: 0 });

  /// Where the untouched video sits inside the pane.
  let fit = $derived.by(() => {
    const { w: pw, h: ph } = paneSize;
    if (!pw || !ph || !videoW || !videoH) return null;
    const scale = Math.min(pw / videoW, ph / videoH);
    const w = videoW * scale;
    const h = videoH * scale;
    return { left: (pw - w) / 2, top: (ph - h) / 2, w, h };
  });

  /// Where the picture sits once the clip's transform has been applied. Given
  /// the same numbers the shader uses, so the outline lands on the pixels.
  let picture = $derived.by(() => {
    if (!fit) return null;
    const t = clips[selectedClip]?.transform ?? TRANSFORM_DEFAULTS;
    const width = fit.w * t.scale;
    const height = fit.h * t.scale;
    const cx = fit.left + fit.w / 2 + t.offset_x * fit.w;
    const cy = fit.top + fit.h / 2 + t.offset_y * fit.h;
    return {
      left: cx - width / 2,
      top: cy - height / 2,
      width,
      height,
      cx,
      cy,
      rotation: t.rotation,
    };
  });

  let handleDrag = $state<null | {
    mode: "move" | "rotate" | "nw" | "ne" | "se" | "sw";
    x: number;
    y: number;
    start: Transform;
    radius: number;
    angle: number;
    cx: number;
    cy: number;
  }>(null);

  function beginHandleDrag(mode: "move" | "rotate" | "nw" | "ne" | "se" | "sw", e: PointerEvent) {
    const clip = clips[selectedClip];
    if (e.button !== 0 || !clip || !picture || !fit) return;
    e.preventDefault();
    e.stopPropagation();
    beginUndoGroup();
    const dx = e.clientX - picture.cx;
    const dy = e.clientY - picture.cy;
    handleDrag = {
      mode,
      x: e.clientX,
      y: e.clientY,
      start: clip.transform,
      radius: Math.max(1, Math.hypot(dx, dy)),
      angle: Math.atan2(dy, dx),
      cx: picture.cx,
      cy: picture.cy,
    };
    window.addEventListener("pointermove", onHandleMove);
    window.addEventListener("pointerup", endHandleDrag, { once: true });
  }

  function onHandleMove(e: PointerEvent) {
    const drag = handleDrag;
    if (!drag || !fit) return;
    const t = drag.start;

    if (drag.mode === "move") {
      // The offset is applied before the rotation in the shader, so a pointer
      // delta has to be turned back into the frame's axes before it means
      // anything.
      const dx = e.clientX - drag.x;
      const dy = e.clientY - drag.y;
      const c = Math.cos(-t.rotation);
      const sn = Math.sin(-t.rotation);
      const fx = (dx * c + dy * sn) / fit.w;
      const fy = (-dx * sn + dy * c) / fit.h;
      applyTransform({
        ...t,
        offset_x: clamp(t.offset_x + fx, -2, 2),
        offset_y: clamp(t.offset_y + fy, -2, 2),
      });
      return;
    }

    if (drag.mode === "rotate") {
      const a = Math.atan2(e.clientY - drag.cy, e.clientX - drag.cx);
      let rotation = t.rotation + (a - drag.angle);
      // Shift snaps to fifteen degrees, which is how you get a picture square
      // to the frame without fighting the pointer.
      if (e.shiftKey) {
        const step = Math.PI / 12;
        rotation = Math.round(rotation / step) * step;
      }
      applyTransform({ ...t, rotation });
      return;
    }

    // A corner scales from the centre, so the aspect ratio is never in question.
    const radius = Math.max(1, Math.hypot(e.clientX - drag.cx, e.clientY - drag.cy));
    const scale = clamp((t.scale * radius) / drag.radius, 0.05, 5);
    applyTransform({ ...t, scale });
  }

  function endHandleDrag() {
    handleDrag = null;
    window.removeEventListener("pointermove", onHandleMove);
  }

  /// Open one undo step for a gesture. Called when a pointer goes down on
  /// anything draggable, so a whole drag is one Ctrl+Z and two drags are two --
  /// which is the boundary real editors use, and the one thing folding by
  /// subject could not express.
  function beginUndoGroup() {
    invoke("begin_undo_group").catch(() => {});
  }

  function applyTransform(transform: Transform) {
    invoke("set_transform", { clip: selectedClip, transform }).catch(() => {});
  }

  function clamp(v: number, lo: number, hi: number) {
    return Math.max(lo, Math.min(hi, v));
  }

  function measurePane() {
    if (!paneEl) return;
    const r = paneEl.getBoundingClientRect();
    paneSize = { w: r.width, h: r.height };
  }

  /// A clip drag in progress.
  ///
  /// The drag draws itself from this and nothing reaches the document until the
  /// pointer comes up, so a drag is one edit and one undo step rather than a
  /// hundred. `deltaMs` is shown as a ghost; the commit is on release.
  let clipDrag = $state<null | {
    index: number;
    mode: "move" | "in" | "out";
    startX: number;
    deltaMs: number;
    targetIndex: number;
  }>(null);

  const EDGE_PX = 6;

  /// Typed so the handle gesture keeps the literal union rather than widening
  /// to `string`.
  const CORNERS = ["nw", "ne", "se", "sw"] as const;

  // Panel sizes, remembered between runs. Blender's arrangement: every boundary
  // is somewhere the user can put it, and it stays put.
  let bottomHeight = $state(300);
  let effectsWidth = $state(228);
  let resizing = $state<"none" | "bottom" | "effects">("none");
  let resizeStart = { x: 0, y: 0, bottom: 300, effects: 228 };

  function loadLayout() {
    try {
      const raw = localStorage.getItem("hwa.layout");
      if (!raw) return;
      const saved = JSON.parse(raw) as { bottom?: number; effects?: number };
      // Clamped on the way in as well as out: a layout saved on a bigger screen
      // pushed the timeline off the bottom of a smaller one, and a playhead that
      // is off-screen looks like a playhead that has gone.
      if (saved.bottom) {
        bottomHeight = Math.max(120, Math.min(window.innerHeight - 160, saved.bottom));
      }
      if (saved.effects) {
        effectsWidth = Math.max(160, Math.min(window.innerWidth - 320, saved.effects));
      }
    } catch {
      // A layout that will not parse is not worth failing a startup over.
    }
  }

  function saveLayout() {
    try {
      localStorage.setItem(
        "hwa.layout",
        JSON.stringify({ bottom: bottomHeight, effects: effectsWidth })
      );
    } catch {
      /* ignore */
    }
  }

  /// Which part of a clip the pointer went down on: an edge means trim, the
  /// middle means move.
  function beginClipDrag(index: number, e: PointerEvent) {
    if (e.button !== 0) return;
    const el = e.currentTarget as HTMLElement;
    const box = el.getBoundingClientRect();
    const offset = e.clientX - box.left;
    const mode =
      offset <= EDGE_PX ? "in" : offset >= box.width - EDGE_PX ? "out" : "move";
    e.preventDefault();
    e.stopPropagation();
    selectedClip = index;
    clipDrag = { index, mode, startX: e.clientX, deltaMs: 0, targetIndex: index };
    window.addEventListener("pointermove", onClipDragMove);
    window.addEventListener("pointerup", endClipDrag, { once: true });
  }

  /// Snap a delta to the frame grid, because a trim that lands between frames is
  /// a trim nobody can see.
  function snapMs(ms: number): number {
    const frame = 1000 / (fps || 30);
    return Math.round(ms / frame) * frame;
  }

  function onClipDragMove(e: PointerEvent) {
    if (!clipDrag) return;
    const deltaMs = ((e.clientX - clipDrag.startX) / pxPerSecond) * 1000;
    let targetIndex = clipDrag.targetIndex;
    if (clipDrag.mode === "move") {
      // Where the pointer's centre would land on the timeline.
      const entry = clipLayout[clipDrag.index];
      const centre = entry.start + entry.duration / 2 + deltaMs;
      let start = 0;
      for (let i = 0; i < clipLayout.length; i += 1) {
        const mid = start + clipLayout[i].duration / 2;
        if (centre < mid) {
          targetIndex = i;
          break;
        }
        targetIndex = i;
        start += clipLayout[i].duration;
      }
    }
    clipDrag = { ...clipDrag, deltaMs, targetIndex };
  }

  function endClipDrag() {
    const drag = clipDrag;
    clipDrag = null;
    window.removeEventListener("pointermove", onClipDragMove);
    if (!drag) return;
    const clip = clips[drag.index];
    if (!clip) return;

    if (drag.mode === "move") {
      if (drag.targetIndex !== drag.index) {
        invoke("move_clip", { from: drag.index, to: drag.targetIndex }).catch(() => {});
        selectedClip = drag.targetIndex;
      }
      return;
    }

    const delta = snapMs(drag.deltaMs);
    if (Math.abs(delta) < 1) return;
    if (drag.mode === "in") {
      // Never past the out point: a clip of zero length is a way to lose frames.
      const next = Math.min(clip.out_ms - 1, Math.max(0, clip.in_ms + delta));
      invoke("trim_clip", {
        index: drag.index,
        in_ms: Math.round(next),
        out_ms: clip.out_ms,
      }).catch(() => {});
    } else {
      const next = Math.max(clip.in_ms + 1, clip.out_ms + delta);
      invoke("trim_clip", {
        index: drag.index,
        in_ms: clip.in_ms,
        out_ms: Math.round(next),
      }).catch(() => {});
    }
  }

  /// How a clip is drawn while it is being dragged: trimmed at one end or moved
  /// along, without the document having been touched.
  function dragGeometry(i: number) {
    if (!clipDrag || clipDrag.index !== i) return null;
    const delta = clipDrag.mode === "move" ? clipDrag.deltaMs : snapMs(clipDrag.deltaMs);
    if (clipDrag.mode === "in") {
      return { left: delta, width: -delta };
    }
    if (clipDrag.mode === "out") {
      return { left: 0, width: delta };
    }
    return { left: delta, width: 0 };
  }

  function beginResize(which: "bottom" | "effects", e: PointerEvent) {
    if (e.button !== 0) return;
    e.preventDefault();
    e.stopPropagation();
    resizing = which;
    resizeStart = {
      x: e.clientX,
      y: e.clientY,
      bottom: bottomHeight,
      effects: effectsWidth,
    };
    window.addEventListener("pointermove", onResizeMove);
    window.addEventListener("pointerup", endResize, { once: true });
  }

  function onResizeMove(e: PointerEvent) {
    if (resizing === "bottom") {
      // The strip grows upward, so dragging up makes it taller.
      const next = resizeStart.bottom - (e.clientY - resizeStart.y);
      bottomHeight = Math.max(120, Math.min(window.innerHeight - 160, next));
      reportLayout();
    } else if (resizing === "effects") {
      const next = resizeStart.effects - (e.clientX - resizeStart.x);
      effectsWidth = Math.max(160, Math.min(window.innerWidth - 320, next));
    }
  }

  function endResize() {
    if (resizing !== "none") {
      saveLayout();
      reportLayout();
    }
    resizing = "none";
    window.removeEventListener("pointermove", onResizeMove);
  }

  /// The video pane is a native subsurface that the backend sizes as the window
  /// minus the bottom strip, so a splitter drag is invisible to it until it is
  /// told. This is the telling.
  function reportLayout() {
    invoke("set_ui_height", { height: Math.round(bottomHeight) }).catch(() => {});
  }
  let undoDepth = $state(0);
  let redoDepth = $state(0);
  let positionMs = $state(0);
  let durationMs = $state(60_000);
  let playing = $state(true);
  /// The clip's frame rate, from the core. The timecode used to assume 30. */
  let fps = $state(30);
  let pxPerSecond = $state(40);
  let dragging = $state(false);
  let lanesEl: HTMLDivElement;

  /// Where each clip sits on the timeline, in order. The core keeps the clips;
  /// this only lays them out to be drawn.
  let clipLayout = $derived.by(() => {
    let start = 0;
    return clips.map((clip) => {
      const duration = Math.max(0, clip.out_ms - clip.in_ms);
      const entry = { start, duration };
      start += duration;
      return entry;
    });
  });

  let playheadPx = $derived((positionMs / 1000) * pxPerSecond);
  let totalSeconds = $derived(durationMs / 1000);
  let formattedTime = $derived(formatTc(positionMs / 1000));
  let formattedDuration = $derived(formatTc(durationMs / 1000));

  let tickStep = $derived(
    totalSeconds <= 30 ? 1 :
    totalSeconds <= 120 ? 5 :
    totalSeconds <= 600 ? 30 :
    60
  );
  let ticks = $derived(
    Array.from(
      { length: Math.floor(totalSeconds / tickStep) + 1 },
      (_, i) => i * tickStep
    )
  );

  /// HH:MM:SS:FF at the clip's own rate. `withFrames` off gives the short form
  /// the ruler wants.
  function formatTc(sec: number, withFrames = true): string {
    const s = Math.max(0, sec);
    const hh = Math.floor(s / 3600);
    const mm = Math.floor((s % 3600) / 60);
    const ss = Math.floor(s % 60);
    const ff = Math.floor((s % 1) * (fps || 30));
    const p = (n: number) => n.toString().padStart(2, "0");
    return withFrames
      ? `${p(hh)}:${p(mm)}:${p(ss)}:${p(ff)}`
      : `${p(hh)}:${p(mm)}:${p(ss)}`;
  }

  onMount(() => {
    // The webview's own context menu offers Reload and Inspect Element. Useful
    // while developing, not what a right click on a clip should do.
    window.addEventListener("contextmenu", (e) => e.preventDefault());
    loadLayout();
    reportLayout();
    measurePane();
    // Held rather than returned here. A `return` in the middle of onMount skips
    // everything after it, and everything after this is the initial state, the
    // export path and the update listeners -- which is how the timeline came up
    // with no clips and the output file name blank: neither was ever asked for.
    let observer: ResizeObserver | null = null;
    if (paneEl) {
      observer = new ResizeObserver(measurePane);
      observer.observe(paneEl);
    }

    invoke<StateSnapshot>("get_state").then((s) => {
      positionMs = s.position_ms;
      durationMs = s.duration_ms || 60_000;
      playing = s.playing;
      if (s.fps > 0) fps = s.fps;
      clips = s.clips ?? [];
      undoDepth = s.undo_depth ?? 0;
      redoDepth = s.redo_depth ?? 0;
      videoW = s.video_width ?? 0;
      videoH = s.video_height ?? 0;
    });

    invoke<string>("default_export_path").then((p) => {
      if (!exportPath) exportPath = p;
    });

    const unlisten = listen<StateSnapshot>("playhead_update", (event) => {
      const s = event.payload;
      if (!dragging) {
        positionMs = s.position_ms;
      }
      durationMs = s.duration_ms || durationMs;
      playing = s.playing;
      if (s.fps > 0) fps = s.fps;
      clips = s.clips ?? [];
      undoDepth = s.undo_depth ?? 0;
      redoDepth = s.redo_depth ?? 0;
      videoW = s.video_width ?? 0;
      videoH = s.video_height ?? 0;
    });

    const unlistenExport = listen<ExportProgress>("export_progress", (event) => {
      exportState = event.payload;
      exportError = event.payload.error ?? "";
    });

    // Keyboard is most of the difference between a scrub bar and an editor.
    // Ignored while the export path field has focus, or space would type a
    // space and never reach the transport.
    function onKey(e: KeyboardEvent) {
      const target = e.target as HTMLElement | null;
      if (
        target &&
        (target.tagName === "INPUT" ||
          target.tagName === "TEXTAREA" ||
          target.isContentEditable)
      ) {
        return;
      }
      switch (e.key) {
        case " ":
          e.preventDefault();
          togglePlay();
          break;
        case "z":
        case "Z":
          if (e.ctrlKey || e.metaKey) {
            e.preventDefault();
            if (e.shiftKey) redo();
            else undo();
          }
          break;
        case "ArrowLeft":
          e.preventDefault();
          if (e.shiftKey) stepSeconds(-1);
          else stepFrames(-1);
          break;
        case "ArrowRight":
          e.preventDefault();
          if (e.shiftKey) stepSeconds(1);
          else stepFrames(1);
          break;
        // Blender's pair, and the ones a video editor expects.
        case ",":
          e.preventDefault();
          stepFrames(-1);
          break;
        case ".":
          e.preventDefault();
          stepFrames(1);
          break;
        case "Home":
          e.preventDefault();
          seekTo(0);
          break;
        case "End":
          e.preventDefault();
          seekTo(durationMs);
          break;
      }
    }
    window.addEventListener("keydown", onKey);

    return () => {
      observer?.disconnect();
      unlisten.then((f) => f());
      unlistenExport.then((f) => f());
      window.removeEventListener("keydown", onKey);
    };
  });

  function startExport() {
    exportError = "";
    // Errors come back as a rejected promise; a failure inside the export loop
    // arrives on the progress event instead.
    Promise.resolve(invoke("start_export", { output: exportPath })).catch((e) => {
      exportError = String(e);
    });
  }

  function cancelExport() {
    Promise.resolve(invoke("cancel_export")).catch(() => {});
  }

  function togglePlay() {
    invoke<boolean>("toggle_play").then((p) => (playing = p));
  }

  // ---- Transport ----
  //
  // There is no playback rate in the core, so a step is a seek: one frame, or
  // one second. Stepping pauses first, because stepping through a moving
  // playhead is a fight.

  function seekTo(ms: number) {
    const clamped = Math.max(0, Math.min(durationMs, Math.round(ms)));
    positionMs = clamped;
    invoke("seek_to", { ms: clamped }).catch(() => {});
  }

  function pauseForStep() {
    if (playing) {
      playing = false;
      invoke("set_paused", { paused: true }).catch(() => {});
    }
  }

  function stepFrames(n: number) {
    pauseForStep();
    seekTo(positionMs + n * (1000 / (fps || 30)));
  }

  function stepSeconds(n: number) {
    pauseForStep();
    seekTo(positionMs + n * 1000);
  }

  // ---- Editing ----
  //
  // The clips are the core's; these just ask it to change them. The snapshot
  // comes back twenty times a second, so the timeline redraws itself.

  function splitAtPlayhead() {
    invoke("split_at_playhead").catch(() => {});
  }

  function deleteSelected() {
    invoke("delete_clip", { index: selectedClip }).catch(() => {});
    if (selectedClip > 0) selectedClip -= 1;
  }

  function resetTimeline() {
    invoke("reset_timeline").catch(() => {});
    selectedClip = 0;
  }

  function undo() {
    invoke("undo").catch(() => {});
  }

  function redo() {
    invoke("redo").catch(() => {});
  }

  // ---- Effects ----
  //
  // The document is the source of truth: the sliders read the clip's effects
  // from the snapshot and never hold their own copy, so what is drawn is what
  // the renderer was told. Each move is an edit, and the spine folds a drag's
  // worth of them into one undo step.

  function addGrade() {
    invoke("add_grade", { clip: selectedClip }).catch(() => {});
  }

  function removeEffect(at: number) {
    invoke("remove_effect", { clip: selectedClip, at }).catch(() => {});
  }

  function setTransformParam(key: keyof Transform, value: number) {
    const clip = clips[selectedClip];
    if (!clip) return;
    const transform = { ...clip.transform, [key]: value };
    invoke("set_transform", { clip: selectedClip, transform }).catch(() => {});
  }

  function setGradeParam(at: number, key: keyof GradeParams, value: number) {
    const effect = clips[selectedClip]?.effects[at];
    if (!effect?.Grade) return;
    const params = { ...effect.Grade, [key]: value };
    invoke("set_effect", { clip: selectedClip, at, params }).catch(() => {});
  }

  // ---- Drag handling with global listeners ----

  function beginDrag(e: PointerEvent) {
    if (e.button !== 0) return;
    // Selecting the label's text is a drag too, and it is not a scrub.
    const target = e.target as HTMLElement | null;
    if (target?.closest(".clip-label")) return;
    dragging = true;
    invoke("log_msg", { msg: `beginDrag clientX=${e.clientX}` });
    seekFromPointer(e);

    window.addEventListener("pointermove", onWindowPointerMove);
    window.addEventListener("pointerup", onWindowPointerUp);
  }

  function onWindowPointerMove(e: PointerEvent) {
    if (!dragging) return;
    seekFromPointer(e);
  }

  function onWindowPointerUp(_e: PointerEvent) {
    if (!dragging) return;
    dragging = false;
    invoke("log_msg", { msg: `endDrag pos=${Math.round(positionMs)}` });
    window.removeEventListener("pointermove", onWindowPointerMove);
    window.removeEventListener("pointerup", onWindowPointerUp);
    invoke("seek_to", { ms: Math.round(positionMs) });
  }

  function seekFromPointer(e: PointerEvent) {
    if (!lanesEl) {
      invoke("log_msg", { msg: "lanesEl is null" });
      return;
    }
    const rect = lanesEl.getBoundingClientRect();
    const x = e.clientX - rect.left + lanesEl.scrollLeft;
    const px = Math.max(0, Math.min(x, totalSeconds * pxPerSecond));
    positionMs = (px / pxPerSecond) * 1000;

    // Deliberately unthrottled. The backend keeps only the newest target and
    // services it once per loop iteration, so every pointer move can be sent
    // without queueing work up; the old 120 ms throttle was what capped
    // scrubbing at single-digit updates per second.
    invoke("seek_to", { ms: Math.round(positionMs) });
  }
</script>

<div class="timeline-root">
  <div class="preview-spacer" bind:this={paneEl}>
    {#if picture && handleDrag === null}
      <div
        class="picture-frame"
        style="left: {picture.left}px; top: {picture.top}px; width: {picture.width}px;                height: {picture.height}px; transform: rotate({picture.rotation}rad)"
        onpointerdown={(e) => beginHandleDrag("move", e)}
        role="presentation"
      >
        {#each CORNERS as corner}
          <div
            class="handle {corner}"
            onpointerdown={(e) => beginHandleDrag(corner, e)}
            role="presentation"
          ></div>
        {/each}
        <div
          class="handle rotate"
          onpointerdown={(e) => beginHandleDrag("rotate", e)}
          role="presentation"
        ></div>
      </div>
    {/if}
  </div>

  <div
    class="splitter-h"
    role="separator"
    aria-label="Resize timeline"
    onpointerdown={(e) => beginResize("bottom", e)}
  ></div>
  <div class="ui-bottom" style="height: {bottomHeight}px">
    <div class="toolbar">
      <div class="transport">
        <button type="button" title="Go to start (Home)" onclick={() => seekTo(0)}>
          <svg class="icon" viewBox="0 0 16 16" aria-hidden="true">
            <path d={ICON.start} />
          </svg>
        </button>
        <button
          type="button"
          title="Back one second (Shift+Left)"
          onclick={() => stepSeconds(-1)}
        >
          <svg class="icon" viewBox="0 0 16 16" aria-hidden="true">
            <path d={ICON.rewind} />
          </svg>
        </button>
        <button
          type="button"
          title="Back one frame (Left or comma)"
          onclick={() => stepFrames(-1)}
        >
          <svg class="icon" viewBox="0 0 16 16" aria-hidden="true">
            <path d={ICON.stepBack} />
          </svg>
        </button>
        <button
          type="button"
          title={playing ? "Pause (Space)" : "Play (Space)"}
          onclick={togglePlay}
        >
          <svg class="icon" viewBox="0 0 16 16" aria-hidden="true">
            <path d={playing ? ICON.pause : ICON.play} />
          </svg>
        </button>
        <button
          type="button"
          title="Forward one frame (Right or period)"
          onclick={() => stepFrames(1)}
        >
          <svg class="icon" viewBox="0 0 16 16" aria-hidden="true">
            <path d={ICON.stepForward} />
          </svg>
        </button>
        <button
          type="button"
          title="Forward one second (Shift+Right)"
          onclick={() => stepSeconds(1)}
        >
          <svg class="icon" viewBox="0 0 16 16" aria-hidden="true">
            <path d={ICON.forward} />
          </svg>
        </button>
        <button type="button" title="Go to end (End)" onclick={() => seekTo(durationMs)}>
          <svg class="icon" viewBox="0 0 16 16" aria-hidden="true">
            <path d={ICON.end} />
          </svg>
        </button>
      </div>
      <div class="timecode selectable" title="position / duration">
        {formattedTime} <span class="dim">/ {formattedDuration}</span>
      </div>
      <div class="spacer"></div>
      <div class="export">
        <input
          class="path"
          type="text"
          bind:value={exportPath}
          disabled={exportRunning}
          placeholder="output file"
          aria-label="Export output path"
        />
        {#if exportRunning}
          <button type="button" onclick={cancelExport}>Cancel</button>
          <div class="bar" title="{exportPercent.toFixed(0)}%">
            <div class="fill" style="width: {exportPercent}%"></div>
          </div>
          <span class="progress">
            {exportState?.frames_done ?? 0}{exportState && exportState.frames_total > 0
              ? `/${exportState.frames_total}`
              : ""}
            · {(exportState?.fps ?? 0).toFixed(1)} fps
          </span>
        {:else}
          <button type="button" onclick={startExport} disabled={!exportPath}>Export</button>
          {#if exportState?.stage === "done"}
            <span class="ok" title={exportState.output}>saved {exportState.frames_done}</span>
          {:else if exportState?.stage === "cancelled"}
            <span class="warn">cancelled</span>
          {:else if exportError}
            <span class="err" title={exportError}>failed</span>
          {/if}
        {/if}
      </div>
      <div class="edit">
        <button
          type="button"
          title="Undo (Ctrl+Z)"
          onclick={undo}
          disabled={undoDepth === 0}
        >
          <svg class="icon" viewBox="0 0 16 16" aria-hidden="true">
            <path d={ICON.undo} />
          </svg>
        </button>
        <button
          type="button"
          title="Redo (Ctrl+Shift+Z)"
          onclick={redo}
          disabled={redoDepth === 0}
        >
          <svg class="icon" viewBox="0 0 16 16" aria-hidden="true">
            <path d={ICON.redo} />
          </svg>
        </button>
        <button
          type="button"
          title="Split the clip under the playhead"
          onclick={splitAtPlayhead}
        >
          Split
        </button>
        <button
          type="button"
          title="Delete the selected clip"
          onclick={deleteSelected}
          disabled={clips.length <= 1}
        >
          Delete
        </button>
        <button type="button" title="Put the whole file back" onclick={resetTimeline}>
          Reset
        </button>
      </div>
      <div class="zoom">
        <span>zoom</span>
        <input type="range" min="10" max="200" bind:value={pxPerSecond} />
      </div>
      <div class="ipc">{durationMs > 0 ? `${(durationMs / 1000).toFixed(1)}s` : ""}</div>
    </div>

    <div class="body">
      <div class="headers">
        <div class="ruler-corner"></div>
        {#each ["V2", "V1", "A1"] as name}
          <div class="track-header">{name}</div>
        {/each}
      </div>

      <div class="lanes-scroll" bind:this={lanesEl}>
        <!-- Dragging is captured at the whole timeline area, not just the ruler -->
        <div
          class="lanes-inner"
          style="width: {totalSeconds * pxPerSecond}px"
          onpointerdown={beginDrag}
          role="slider"
          tabindex="0"
          aria-label="Timeline"
          aria-valuenow={positionMs / 1000}
          aria-valuemin="0"
          aria-valuemax={totalSeconds}
        >
          <div class="ruler">
            {#each ticks as t}
              <div class="tick" style="left: {t * pxPerSecond}px">
                <div class="tick-mark"></div>
                <div class="tick-label">{formatTc(t, false)}</div>
              </div>
            {/each}
          </div>

          {#each [0, 1, 2] as lane}
            <div class="lane">
              {#if lane === 1}
                {#each clipLayout as entry, i}
                  <div
                    class="clip"
                    class:selected={i === selectedClip}
                    class:dragging={clipDrag?.index === i}
                    style="left: {((entry.start + (dragGeometry(i)?.left ?? 0)) / 1000) *
                      pxPerSecond}px; width: {((entry.duration +
                      (dragGeometry(i)?.width ?? 0)) /
                      1000) *
                      pxPerSecond}px"
                    onpointerdown={(e) => beginClipDrag(i, e)}
                    onclick={(e) => {
                      e.stopPropagation();
                      selectedClip = i;
                    }}
                    onkeydown={(e) => {
                      if (e.key === "Enter" || e.key === " ") {
                        e.preventDefault();
                        selectedClip = i;
                      }
                    }}
                    role="button"
                    tabindex="0"
                    aria-label="Clip {i + 1}"
                  >
                    <span class="clip-label selectable">
                      {formatTc(entry.duration / 1000, false)}
                    </span>
                  </div>
                {/each}
              {/if}
            </div>
          {/each}

          <div class="playhead" style="left: {playheadPx}px">
            <div class="playhead-head"></div>
          </div>
        </div>
      </div>

      <div
        class="splitter-v"
        role="separator"
        aria-label="Resize effects panel"
        onpointerdown={(e) => beginResize("effects", e)}
      ></div>
      <div class="effects" style="width: {effectsWidth}px">
        <div class="effects-head">
          <span>Effects</span>
          <span class="dim">clip {selectedClip + 1}</span>
        </div>
        {#if clips[selectedClip]}
          <div class="effect">
            <div class="effect-head">
              <span>Transform</span>
              <button
                type="button"
                title="Reset the transform"
                disabled={TRANSFORM_CONTROLS.every(
                  (control) =>
                    clips[selectedClip].transform[control.k] ===
                    TRANSFORM_DEFAULTS[control.k]
                )}
                onclick={() =>
                  invoke("set_transform", {
                    clip: selectedClip,
                    transform: TRANSFORM_DEFAULTS,
                  }).catch(() => {})}
              >
                <svg class="icon" viewBox="0 0 16 16" aria-hidden="true">
                  <path d={ICON.reset} />
                </svg>
              </button>
            </div>
            {#each TRANSFORM_CONTROLS as control}
              <label class="param">
                <span class="param-name">{control.label}</span>
                <input
                  type="range"
                  min={control.min}
                  max={control.max}
                  step={control.step}
                  value={clips[selectedClip].transform[control.k]}
                  onpointerdown={beginUndoGroup}
                  oninput={(e) =>
                    setTransformParam(control.k, Number(e.currentTarget.value))}
                />
                <span class="param-value">
                  {clips[selectedClip].transform[control.k].toFixed(2)}
                </span>
                <button
                  type="button"
                  class="param-reset"
                  title="Reset {control.label.toLowerCase()}"
                  disabled={clips[selectedClip].transform[control.k] ===
                    TRANSFORM_DEFAULTS[control.k]}
                  onclick={() =>
                    setTransformParam(control.k, TRANSFORM_DEFAULTS[control.k])}
                >
                  <svg class="icon" viewBox="0 0 16 16" aria-hidden="true">
                    <path d={ICON.reset} />
                  </svg>
                </button>
              </label>
            {/each}
          </div>
        {/if}
        {#each clips[selectedClip]?.effects ?? [] as effect, i}
          {#if effect.Grade}
            <div class="effect">
              <div class="effect-head">
                <span>Grade</span>
                <button type="button" title="Remove this effect" onclick={() => removeEffect(i)}>
                  <svg class="icon" viewBox="0 0 16 16" aria-hidden="true">
                    <path d={ICON.close} />
                  </svg>
                </button>
              </div>
              {#each GRADE_CONTROLS as control}
                <label class="param">
                  <span class="param-name">{control.label}</span>
                  <input
                    type="range"
                    min={control.min}
                    max={control.max}
                    step={control.step}
                    value={effect.Grade[control.k]}
                    onpointerdown={beginUndoGroup}
                    oninput={(e) =>
                      setGradeParam(i, control.k, Number(e.currentTarget.value))}
                  />
                  <span class="param-value">{effect.Grade[control.k].toFixed(2)}</span>
                  <button
                    type="button"
                    class="param-reset"
                    title="Reset {control.label.toLowerCase()} to its default"
                    disabled={effect.Grade[control.k] === GRADE_DEFAULTS[control.k]}
                    onclick={() => setGradeParam(i, control.k, GRADE_DEFAULTS[control.k])}
                  >
                    <svg class="icon" viewBox="0 0 16 16" aria-hidden="true">
                      <path d={ICON.reset} />
                    </svg>
                  </button>
                </label>
              {/each}
            </div>
          {/if}
        {/each}
        {#if !(clips[selectedClip]?.effects ?? []).some((effect) => effect.Grade)}
          <button
            type="button"
            class="add-effect"
            onclick={addGrade}
            disabled={clips.length === 0}
          >
            Add grade
          </button>
        {/if}
      </div>
    </div>
  </div>
</div>

<style>
  .timeline-root {
    width: 100%;
    height: 100vh;
    display: flex;
    flex-direction: column;
    user-select: none;
    overflow: hidden;
    background: transparent;
  }

  .preview-spacer {
    flex: 1;
    min-height: 0;
    background: transparent;
    position: relative;
    overflow: hidden;
  }

  /* The outline of the picture, drawn where the shader puts it. Dragging the
     middle moves it; a corner scales it; the grip above turns it. */
  .picture-frame {
    position: absolute;
    border: 1px solid rgba(255, 255, 255, 0.55);
    cursor: move;
  }
  .handle {
    position: absolute;
    width: 10px;
    height: 10px;
    margin: -5px 0 0 -5px;
    background: var(--text);
    border: 1px solid var(--bg-window);
    border-radius: 2px;
  }
  .handle.nw { left: 0; top: 0; cursor: nwse-resize; }
  .handle.ne { left: 100%; top: 0; cursor: nesw-resize; }
  .handle.se { left: 100%; top: 100%; cursor: nwse-resize; }
  .handle.sw { left: 0; top: 100%; cursor: nesw-resize; }
  .handle.rotate {
    left: 50%;
    top: -22px;
    margin-left: -5px;
    border-radius: 50%;
    background: var(--accent);
    cursor: grab;
  }
  .handle.rotate::after {
    content: "";
    position: absolute;
    left: 50%;
    top: 100%;
    width: 1px;
    height: 12px;
    background: rgba(255, 255, 255, 0.55);
  }

  .ui-bottom {
    height: var(--bottom-h, 300px);
    height: 300px;
    display: flex;
    flex-direction: column;
    background: var(--bg-panel);
    flex-shrink: 0;
  }

  .toolbar {
    height: var(--toolbar-h);
    background: var(--bg-header);
    border-bottom: 1px solid var(--border);
    display: flex;
    align-items: center;
    padding: 0 12px;
    gap: 16px;
    flex-shrink: 0;
  }
  .transport { display: flex; gap: 4px; }
  .transport button {
    background: var(--bg-widget);
    border: 1px solid var(--border);
    color: var(--text);
    min-width: 28px;
    height: 24px;
    border-radius: 3px;
    cursor: pointer;
    font-size: 11px;
    line-height: 1;
  }
  .transport button:hover { background: var(--bg-hover); }
  .transport button:active { background: var(--accent); }

  .timecode {
    font-family: var(--font-mono);
    font-size: 14px;
    color: var(--playhead);
    letter-spacing: 0.5px;
  }
  .timecode .dim { color: var(--text-muted); }

  .spacer { flex: 1; }

  .lanes-inner { min-width: 100%; }

  .splitter-h {
    height: 4px;
    cursor: row-resize;
    background: var(--border);
    flex: 0 0 auto;
  }
  .splitter-h:hover { background: var(--accent); }
  .splitter-v {
    width: 4px;
    cursor: col-resize;
    background: var(--border);
    flex: 0 0 auto;
  }
  .splitter-v:hover { background: var(--accent); }

  .effects {
    width: 228px;
    border-left: 1px solid var(--border);
    background: var(--bg-panel);
    padding: 6px 8px;
    overflow-y: auto;
    font-size: var(--font-size-sm);
  }
  .effects-head {
    display: flex;
    justify-content: space-between;
    color: var(--text-dim);
    text-transform: uppercase;
    font-size: var(--font-size-xs);
    letter-spacing: 0.5px;
    margin-bottom: 6px;
  }
  .effects-head .dim { color: var(--text-muted); text-transform: none; }
  .effect {
    background: var(--bg-window);
    border: 1px solid var(--border);
    border-left: 3px solid var(--clip-effect);
    border-radius: 3px;
    padding: 6px 8px;
    margin-bottom: 6px;
  }
  .effect-head {
    display: flex;
    justify-content: space-between;
    align-items: center;
    color: var(--text);
    margin-bottom: 4px;
  }
  .effect-head button {
    background: none;
    border: none;
    color: var(--text-dim);
    cursor: pointer;
    font-size: 14px;
    line-height: 1;
    padding: 0 2px;
  }
  .effect-head button:hover { color: var(--playhead); }
  .param {
    display: grid;
    grid-template-columns: 64px 1fr 34px 14px;
    align-items: center;
    gap: 6px;
    margin: 2px 0;
  }
  .param-name { color: var(--text-dim); }
  .param-value { font-family: var(--font-mono); color: var(--text); text-align: right; }
  .param input[type="range"] { width: 100%; }
  .add-effect {
    width: 100%;
    background: var(--bg-widget);
    border: 1px solid var(--border);
    color: var(--text);
    height: 24px;
    border-radius: 3px;
    cursor: pointer;
    font-size: var(--font-size-sm);
  }
  .add-effect:hover:not(:disabled) { background: var(--bg-hover); }
  .add-effect:disabled { color: var(--text-muted); cursor: default; }

  .icon {
    width: 12px;
    height: 12px;
    fill: currentColor;
    display: block;
    margin: 0 auto;
  }
  .param-reset {
    background: none;
    border: none;
    padding: 0;
    color: var(--text-muted);
    cursor: pointer;
    width: 14px;
  }
  .param-reset:hover:not(:disabled) { color: var(--text); }
  .param-reset:disabled { color: transparent; cursor: default; }

  .edit { display: flex; gap: 6px; align-items: center; margin-right: 12px; }
  .edit button {
    background: var(--bg-widget);
    border: 1px solid var(--border);
    color: var(--text);
    height: 24px;
    padding: 0 10px;
    border-radius: 3px;
    cursor: pointer;
    font-size: var(--font-size-sm);
  }
  .edit button:hover:not(:disabled) { background: var(--bg-hover); }
  .edit button:disabled { color: var(--text-muted); cursor: default; }

  .zoom { display: flex; align-items: center; gap: 8px; color: var(--text-dim); }
  .zoom input[type="range"] { width: 120px; accent-color: var(--accent); }

  .export { display: flex; align-items: center; gap: 6px; min-width: 0; }
  .export .path {
    background: var(--bg-window);
    border: 1px solid var(--border);
    color: var(--text);
    font-family: var(--font-mono);
    font-size: var(--font-size-sm);
    padding: 3px 6px;
    border-radius: 3px;
    width: 260px;
    min-width: 0;
  }
  .export .path:disabled { color: var(--text-dim); }
  .export button {
    background: var(--bg-widget);
    border: 1px solid var(--border);
    color: var(--text);
    height: 24px;
    padding: 0 10px;
    border-radius: 3px;
    cursor: pointer;
    font-size: var(--font-size-sm);
  }
  .export button:hover:not(:disabled) { background: var(--bg-hover); }
  .export button:disabled { color: var(--text-muted); cursor: default; }
  .export .bar {
    width: 90px;
    height: 8px;
    background: var(--bg-window);
    border: 1px solid var(--border);
    border-radius: 2px;
    overflow: hidden;
  }
  .export .fill { height: 100%; background: var(--accent); }
  .export .progress { color: var(--text-dim); font-size: var(--font-size-xs); font-family: var(--font-mono); }
  .export .ok { color: #7ac47a; font-size: var(--font-size-xs); }
  .export .warn { color: var(--playhead); font-size: var(--font-size-xs); }
  .export .err { color: #d97070; font-size: var(--font-size-xs); }

  .ipc { color: var(--text-dim); font-size: 12px; }

  .body {
    flex: 1;
    display: flex;
    overflow: hidden;
    min-height: 0;
  }

  .headers {
    width: var(--track-header-w);
    background: var(--bg-panel);
    border-right: 1px solid var(--border);
    flex-shrink: 0;
  }
  .ruler-corner {
    height: var(--ruler-h);
    background: var(--bg-header);
    border-bottom: 1px solid var(--border);
  }
  .track-header {
    height: var(--track-h);
    display: flex;
    align-items: center;
    justify-content: center;
    background: var(--bg-panel);
    border-bottom: 1px solid var(--border);
    color: var(--text-dim);
    font-weight: 500;
    font-size: 12px;
  }

  .lanes-scroll {
    flex: 1;
    overflow-x: auto;
    overflow-y: hidden;
    background: var(--bg-window);
    position: relative;
  }
  .lanes-inner {
    position: relative;
    min-height: 100%;
    cursor: ew-resize;
  }

  .ruler {
    height: var(--ruler-h);
    background: var(--bg-header);
    border-bottom: 1px solid var(--border);
    position: relative;
  }
  .tick { position: absolute; top: 0; height: var(--ruler-h); }
  .tick-mark {
    width: 1px;
    height: 6px;
    background: var(--text-dim);
    opacity: 0.5;
  }
  .tick-label {
    position: absolute;
    top: 8px;
    left: 3px;
    font-size: var(--font-size-xs);
    color: var(--text-dim);
  }

  .lane {
    height: var(--track-h);
    border-bottom: 1px solid var(--border);
    position: relative;
    background: var(--bg-window);
  }
  .lane:nth-child(even) { background: #212121; }

  /* The media span, drawn on its track. Uses the theme's clip colour, so when
     real clips arrive they already look like this. */
  .clip {
    position: absolute;
    top: 3px;
    bottom: 3px;
    background: var(--clip-video);
    border: 1px solid var(--accent);
    border-radius: 3px;
    box-sizing: border-box;
    overflow: hidden;
    display: flex;
    align-items: center;
  }
  .clip.selected { border-color: var(--playhead); background: #46698c; }
  .clip.dragging { opacity: 0.85; border-color: var(--accent); }
  .clip { cursor: grab; }
  .clip.dragging { cursor: grabbing; }

  .clip-label {
    padding: 0 6px;
    font-family: var(--font-mono);
    font-size: var(--font-size-xs);
    color: var(--text);
    white-space: nowrap;
  }

  .playhead {
    position: absolute;
    top: 0;
    bottom: 0;
    width: 2px;
    background: var(--playhead);
    pointer-events: none;
    z-index: 10;
  }
  .playhead-head {
    position: absolute;
    top: 0;
    left: -6px;
    width: 14px;
    height: 12px;
    background: var(--playhead);
    clip-path: polygon(0 0, 100% 0, 50% 100%);
  }
</style>
