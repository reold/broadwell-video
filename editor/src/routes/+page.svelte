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

  type Clip = { in_ms: number; out_ms: number; effects: Effect[] };

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
      if (saved.bottom) bottomHeight = saved.bottom;
      if (saved.effects) effectsWidth = saved.effects;
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
    } else if (resizing === "effects") {
      const next = resizeStart.effects - (e.clientX - resizeStart.x);
      effectsWidth = Math.max(160, Math.min(window.innerWidth - 320, next));
    }
  }

  function endResize() {
    if (resizing !== "none") saveLayout();
    resizing = "none";
    window.removeEventListener("pointermove", onResizeMove);
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
    loadLayout();

    invoke<StateSnapshot>("get_state").then((s) => {
      positionMs = s.position_ms;
      durationMs = s.duration_ms || 60_000;
      playing = s.playing;
      if (s.fps > 0) fps = s.fps;
      clips = s.clips ?? [];
      undoDepth = s.undo_depth ?? 0;
      redoDepth = s.redo_depth ?? 0;
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
  <div class="preview-spacer"></div>

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
          ⏮
        </button>
        <button
          type="button"
          title="Back one second (Shift+Left)"
          onclick={() => stepSeconds(-1)}
        >
          ⏪
        </button>
        <button
          type="button"
          title="Back one frame (Left or comma)"
          onclick={() => stepFrames(-1)}
        >
          ◀
        </button>
        <button
          type="button"
          title={playing ? "Pause (Space)" : "Play (Space)"}
          onclick={togglePlay}
        >
          {playing ? "⏸" : "▶"}
        </button>
        <button
          type="button"
          title="Forward one frame (Right or period)"
          onclick={() => stepFrames(1)}
        >
          ▶
        </button>
        <button
          type="button"
          title="Forward one second (Shift+Right)"
          onclick={() => stepSeconds(1)}
        >
          ⏩
        </button>
        <button type="button" title="Go to end (End)" onclick={() => seekTo(durationMs)}>
          ⏭
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
          ↶
        </button>
        <button
          type="button"
          title="Redo (Ctrl+Shift+Z)"
          onclick={redo}
          disabled={redoDepth === 0}
        >
          ↷
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
                    style="left: {(entry.start / 1000) * pxPerSecond}px; width: {(entry.duration / 1000) * pxPerSecond}px"
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
        {#each clips[selectedClip]?.effects ?? [] as effect, i}
          {#if effect.Grade}
            <div class="effect">
              <div class="effect-head">
                <span>Grade</span>
                <button type="button" title="Remove this effect" onclick={() => removeEffect(i)}>
                  ×
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
                    oninput={(e) =>
                      setGradeParam(i, control.k, Number(e.currentTarget.value))}
                  />
                  <span class="param-value">{effect.Grade[control.k].toFixed(2)}</span>
                </label>
              {/each}
            </div>
          {/if}
        {/each}
        <button type="button" class="add-effect" onclick={addGrade} disabled={clips.length === 0}>
          Add grade
        </button>
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
    grid-template-columns: 64px 1fr 34px;
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
