<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";

  let playheadPx = $state(180);
  let pxPerSecond = $state(40);
  let totalSeconds = $state(60);
  let dragging = $state(false);
  let ipcMsg = $state("");
  let lanesEl: HTMLDivElement;

  let playheadSeconds = $derived(playheadPx / pxPerSecond);
  let formattedTime = $derived(formatTc(playheadSeconds));

  const clips = [
    { lane: 0, start: 2,  dur: 8,  label: "intro.mp4",   kind: "video" },
    { lane: 0, start: 10, dur: 6,  label: "b_roll.mp4",  kind: "video" },
    { lane: 0, start: 16, dur: 11, label: "talking.mp4", kind: "video" },
    { lane: 1, start: 0,  dur: 12, label: "overlay.png", kind: "effect" },
    { lane: 1, start: 18, dur: 5,  label: "title.text",  kind: "effect" },
    { lane: 2, start: 0,  dur: 27, label: "music.mp3",   kind: "audio" },
  ];

  const laneNames = ["V2", "V1", "A1"];

  function formatTc(sec: number): string {
    const s = Math.max(0, sec);
    const hh = Math.floor(s / 3600);
    const mm = Math.floor((s % 3600) / 60);
    const ss = Math.floor(s % 60);
    const ff = Math.floor((s % 1) * 30);
    const p = (n: number) => n.toString().padStart(2, "0");
    return `${p(hh)}:${p(mm)}:${p(ss)}:${p(ff)}`;
  }

  function startDrag(e: PointerEvent) {
    dragging = true;
    (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
    movePlayhead(e);
  }

  function movePlayhead(e: PointerEvent) {
    if (!dragging || !lanesEl) return;
    const rect = lanesEl.getBoundingClientRect();
    const x = e.clientX - rect.left + lanesEl.scrollLeft;
    playheadPx = Math.max(0, Math.min(x, totalSeconds * pxPerSecond));
  }

  function endDrag(e: PointerEvent) {
    dragging = false;
    (e.currentTarget as HTMLElement).releasePointerCapture(e.pointerId);
    invoke("greet", { name: formattedTime }).then((r) => (ipcMsg = r as string));
  }

  function onRulerClick(e: MouseEvent) {
    const rect = lanesEl.getBoundingClientRect();
    playheadPx = Math.max(0, e.clientX - rect.left + lanesEl.scrollLeft);
  }

  function onRulerKey(e: KeyboardEvent) {
    if (e.key === "ArrowLeft") {
      playheadPx = Math.max(0, playheadPx - pxPerSecond / 2);
    } else if (e.key === "ArrowRight") {
      playheadPx = Math.min(totalSeconds * pxPerSecond, playheadPx + pxPerSecond / 2);
    }
  }

  let ticks = $derived(Array.from({ length: totalSeconds + 1 }, (_, i) => i));
</script>

<svelte:head>
  <style>
    html, body {
      background: transparent !important;
      margin: 0;
      padding: 0;
      overflow: hidden;
      height: 100%;
    }
  </style>
</svelte:head>

<div class="timeline-root">
  <!-- Reserved space for the video preview. Nothing is drawn here, so the
       wgpu subsurface underneath shows through. -->
  <div class="preview-spacer"></div>

  <!-- Bottom 300px: actual UI -->
  <div class="ui-bottom">
    <div class="toolbar">
      <div class="transport">
        <button type="button" title="Play">▶</button>
        <button type="button" title="Pause">⏸</button>
        <button type="button" title="Stop">⏹</button>
      </div>
      <div class="timecode">{formattedTime}</div>
      <div class="spacer"></div>
      <div class="zoom">
        <span>zoom</span>
        <input type="range" min="10" max="200" bind:value={pxPerSecond} />
      </div>
      <div class="ipc">{ipcMsg}</div>
    </div>

    <div class="body">
      <div class="headers">
        <div class="ruler-corner"></div>
        {#each laneNames as name}
          <div class="track-header">{name}</div>
        {/each}
      </div>

      <div class="lanes-scroll" bind:this={lanesEl}>
        <div class="lanes-inner" style="width: {totalSeconds * pxPerSecond}px">
          <!-- svelte-ignore a11y_click_events_have_key_events -->
          <!-- svelte-ignore a11y_no_static_element_interactions -->
          <div
            class="ruler"
            onclick={onRulerClick}
            onkeydown={onRulerKey}
            role="slider"
            tabindex="0"
            aria-label="Timeline ruler"
            aria-valuenow={playheadSeconds}
            aria-valuemin="0"
            aria-valuemax={totalSeconds}
          >
            {#each ticks as t}
              <div class="tick" style="left: {t * pxPerSecond}px">
                <div class="tick-mark"></div>
                <div class="tick-label">{t}s</div>
              </div>
            {/each}
          </div>

          {#each laneNames as _, laneIdx}
            <div class="lane">
              {#each clips.filter((c) => c.lane === laneIdx) as clip}
                <div
                  class="clip clip-{clip.kind}"
                  style="left: {clip.start * pxPerSecond}px;
                         width: {clip.dur * pxPerSecond}px"
                >
                  {clip.label}
                </div>
              {/each}
            </div>
          {/each}

          <div
            class="playhead"
            style="left: {playheadPx}px"
            onpointerdown={startDrag}
            onpointermove={movePlayhead}
            onpointerup={endDrag}
            role="slider"
            aria-valuenow={playheadSeconds}
            aria-valuemin="0"
            aria-valuemax={totalSeconds}
            tabindex="0"
          >
            <div class="playhead-head"></div>
          </div>
        </div>
      </div>
    </div>
  </div>
</div>

<style>
  :global(html),
  :global(body) {
    background: transparent;
    margin: 0;
    padding: 0;
    overflow: hidden;
  }

  .timeline-root {
    --bg-panel:    #2b2b2b;
    --bg-header:   #3d3d3d;
    --bg-widget:   #545454;
    --text:        #e5e5e5;
    --text-dim:    #a0a0a0;
    --accent:      #4772b3;
    --playhead:    #ff8c00;
    --border:      #131313;
    --bg-window:   #1d1d1d;
    --clip-video:  #3a5a7a;
    --clip-audio:  #2f5040;
    --clip-effect: #6a4a7a;

    font-family: -apple-system, "Inter", "Segoe UI", system-ui, sans-serif;
    font-size: 13px;
    color: var(--text);
    background: transparent;
    width: 100%;
    height: 100vh;
    display: flex;
    flex-direction: column;
    user-select: none;
    overflow: hidden;
  }

  /* Top 600px: nothing drawn, wgpu subsurface shows through */
  .preview-spacer {
    flex: 1;
    min-height: 0;
    background: transparent;
  }

  /* Bottom 300px: opaque UI */
  .ui-bottom {
    height: 300px;
    display: flex;
    flex-direction: column;
    background: var(--bg-panel);
    flex-shrink: 0;
  }

  .toolbar {
    height: 40px;
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
    width: 28px;
    height: 24px;
    border-radius: 3px;
    cursor: pointer;
    font-size: 11px;
    line-height: 1;
  }
  .transport button:hover { background: #656565; }
  .transport button:active { background: var(--accent); }

  .timecode {
    font-family: "JetBrains Mono", "Fira Code", monospace;
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
    width: 60px;
    background: var(--bg-panel);
    border-right: 1px solid var(--border);
    flex-shrink: 0;
  }
  .ruler-corner {
    height: 24px;
    background: var(--bg-header);
    border-bottom: 1px solid var(--border);
  }
  .track-header {
    height: 44px;
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
    height: 100%;
    min-height: 100%;
  }

  .ruler {
    height: 24px;
    background: var(--bg-header);
    border-bottom: 1px solid var(--border);
    position: relative;
    cursor: crosshair;
  }
  .tick { position: absolute; top: 0; height: 24px; }
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
    font-size: 10px;
    color: var(--text-dim);
  }

  .lane {
    height: 44px;
    border-bottom: 1px solid var(--border);
    position: relative;
    background: var(--bg-window);
  }
  .lane:nth-child(even) { background: #212121; }

  .clip {
    position: absolute;
    top: 4px;
    height: 36px;
    border-radius: 3px;
    padding: 0 8px;
    display: flex;
    align-items: center;
    font-size: 11px;
    color: var(--text);
    overflow: hidden;
    white-space: nowrap;
    text-overflow: ellipsis;
    cursor: grab;
    border: 1px solid rgba(0, 0, 0, 0.4);
  }
  .clip-video  { background: var(--clip-video); }
  .clip-audio  { background: var(--clip-audio); }
  .clip-effect { background: var(--clip-effect); }
  .clip:hover  { filter: brightness(1.15); }

  .playhead {
    position: absolute;
    top: 0;
    bottom: 0;
    width: 2px;
    background: var(--playhead);
    cursor: ew-resize;
    z-index: 10;
  }
  .playhead::before {
    content: "";
    position: absolute;
    top: 0;
    bottom: 0;
    left: -6px;
    right: -6px;
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
