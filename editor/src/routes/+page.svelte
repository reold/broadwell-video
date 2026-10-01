<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";
  import { onMount } from "svelte";

  type StateSnapshot = {
    playing: boolean;
    position_ms: number;
    duration_ms: number;
    fps: number;
  };

  let positionMs = $state(0);
  let durationMs = $state(60_000);
  let playing = $state(true);
  let pxPerSecond = $state(40);
  let dragging = $state(false);
  let lanesEl: HTMLDivElement;

  let playheadPx = $derived((positionMs / 1000) * pxPerSecond);
  let totalSeconds = $derived(durationMs / 1000);
  let formattedTime = $derived(formatTc(positionMs / 1000));

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

  function formatTc(sec: number): string {
    const s = Math.max(0, sec);
    const hh = Math.floor(s / 3600);
    const mm = Math.floor((s % 3600) / 60);
    const ss = Math.floor(s % 60);
    const ff = Math.floor((s % 1) * 30);
    const p = (n: number) => n.toString().padStart(2, "0");
    return `${p(hh)}:${p(mm)}:${p(ss)}:${p(ff)}`;
  }

  onMount(() => {
    invoke<StateSnapshot>("get_state").then((s) => {
      positionMs = s.position_ms;
      durationMs = s.duration_ms || 60_000;
      playing = s.playing;
    });

    const unlisten = listen<StateSnapshot>("playhead_update", (event) => {
      const s = event.payload;
      if (!dragging) {
        positionMs = s.position_ms;
      }
      durationMs = s.duration_ms || durationMs;
      playing = s.playing;
    });

    return () => {
      unlisten.then((f) => f());
    };
  });

  function togglePlay() {
    invoke<boolean>("toggle_play").then((p) => (playing = p));
  }

  // ---- Drag handling with global listeners ----

  function beginDrag(e: PointerEvent) {
    if (e.button !== 0) return;
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

  <div class="ui-bottom">
    <div class="toolbar">
      <div class="transport">
        <button
          type="button"
          title={playing ? "Pause" : "Play"}
          onclick={togglePlay}
        >
          {playing ? "⏸" : "▶"}
        </button>
        <button
          type="button"
          title="Stop"
          onclick={() => invoke("seek_to", { ms: 0 })}
        >
          ⏹
        </button>
      </div>
      <div class="timecode">{formattedTime}</div>
      <div class="spacer"></div>
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
                <div class="tick-label">{t}s</div>
              </div>
            {/each}
          </div>

          {#each [0, 1, 2] as _}
            <div class="lane"></div>
          {/each}

          <div class="playhead" style="left: {playheadPx}px">
            <div class="playhead-head"></div>
          </div>
        </div>
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

  .spacer { flex: 1; }

  .zoom { display: flex; align-items: center; gap: 8px; color: var(--text-dim); }
  .zoom input[type="range"] { width: 120px; accent-color: var(--accent); }

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
