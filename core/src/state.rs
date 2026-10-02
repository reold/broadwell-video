use std::sync::{Arc, Mutex};

#[derive(Clone, serde::Serialize)]
pub struct StateSnapshot {
    pub playing: bool,
    pub position_ms: i64,
    pub duration_ms: i64,
    pub fps: f64,
    /// The timeline, in order, as source ranges of the loaded file.
    pub clips: Vec<Clip>,
}

/// One segment of the loaded file, in source milliseconds.
///
/// The timeline is a list of these, so trimming and reordering the media is a
/// matter of changing the list. Clips from *different* files would need a
/// decoder per file and are not modelled here: the source is one file, and this
/// says which parts of it play, in what order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Clip {
    pub in_ms: i64,
    pub out_ms: i64,
}

impl Clip {
    pub fn duration_ms(&self) -> i64 {
        (self.out_ms - self.in_ms).max(0)
    }

    pub fn contains(&self, source_ms: i64) -> bool {
        source_ms >= self.in_ms && source_ms < self.out_ms
    }
}

pub struct EditorState {
    pub playing: bool,
    /// Position on the *timeline*, not in the source file. The renderer works in
    /// source milliseconds and translates on the way in and out, so everything
    /// outside it can think in terms of the edit.
    pub position_ms: i64,
    /// Total length of the timeline.
    pub duration_ms: i64,
    pub fps: f64,
    /// Where a seek should land, in *source* milliseconds. Internal to the
    /// renderer's loop; `seek_to` translates before setting it.
    pub pending_seek_ms: Option<i64>,
    /// Path of the file being edited, so an export can suggest a name.
    pub video_path: String,
    /// The edit itself. Empty means the whole file, which is what a freshly
    /// opened clip gets.
    pub clips: Vec<Clip>,
    /// The export in flight or the last one to finish.
    pub export: Option<ExportJob>,
}

/// Where an export has got to.
#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ExportStage {
    Idle,
    Running,
    Done,
    Failed,
    Cancelled,
}

/// Export progress as reported to the UI.
#[derive(Clone, serde::Serialize)]
pub struct ExportProgress {
    pub stage: ExportStage,
    pub frames_done: u64,
    pub frames_total: u64,
    pub fps: f64,
    pub output: String,
    pub error: Option<String>,
}

pub struct ExportJob {
    pub output: String,
    pub frames_done: u64,
    /// Estimated from the duration, so the bar has somewhere to go. The real
    /// count is whatever the decoder produces.
    pub frames_total: u64,
    pub fps: f64,
    pub stage: ExportStage,
    pub error: Option<String>,
    /// Set by the UI to ask the render loop to stop early.
    pub cancel: bool,
}

impl ExportJob {
    pub fn progress(&self) -> ExportProgress {
        ExportProgress {
            stage: self.stage,
            frames_done: self.frames_done,
            frames_total: self.frames_total,
            fps: self.fps,
            output: self.output.clone(),
            error: self.error.clone(),
        }
    }

    pub fn is_running(&self) -> bool {
        self.stage == ExportStage::Running
    }
}

impl EditorState {
    pub fn new() -> Self {
        Self {
            playing: true,
            position_ms: 0,
            duration_ms: 0,
            fps: 30.0,
            pending_seek_ms: None,
            video_path: String::new(),
            clips: Vec::new(),
            export: None,
        }
    }

    /// One clip covering everything, for a file nobody has cut yet.
    pub fn reset_timeline(&mut self, source_duration_ms: i64) {
        self.clips = vec![Clip {
            in_ms: 0,
            out_ms: source_duration_ms.max(0),
        }];
        self.refresh_duration();
        self.position_ms = 0;
    }

    /// Recompute the timeline length from the clips.
    pub fn refresh_duration(&mut self) {
        self.duration_ms = self.clips.iter().map(|c| c.duration_ms()).sum();
        if self.position_ms > self.duration_ms {
            self.position_ms = self.duration_ms;
        }
    }

    /// Where a timeline position lands in the source, and which clip it is in.
    pub fn source_for_timeline(&self, timeline_ms: i64) -> Option<(usize, i64)> {
        let mut start = 0;
        for (i, clip) in self.clips.iter().enumerate() {
            let end = start + clip.duration_ms();
            if timeline_ms < end {
                return Some((i, clip.in_ms + (timeline_ms - start).max(0)));
            }
            start = end;
        }
        // Past the end: the last frame of the last clip.
        self.clips
            .last()
            .map(|c| (self.clips.len() - 1, (c.out_ms - 1).max(c.in_ms)))
    }

    /// Where a source position sits on the timeline.
    pub fn timeline_for_source(&self, source_ms: i64) -> i64 {
        let mut start = 0;
        for clip in &self.clips {
            if clip.contains(source_ms) {
                return start + (source_ms - clip.in_ms);
            }
            start += clip.duration_ms();
        }
        // Before the first clip, or past the last: clamp to the nearest end.
        if let Some(first) = self.clips.first() {
            if source_ms < first.in_ms {
                return 0;
            }
        }
        0.max(start - 1)
    }

    /// The clip an export should be reading at this source position, or the one
    /// that comes after it when the position has run past the end.
    pub fn next_clip_in(&self, source_ms: i64) -> Option<i64> {
        for (i, clip) in self.clips.iter().enumerate() {
            if clip.contains(source_ms) {
                return None;
            }
            if source_ms < clip.in_ms {
                return Some(clip.in_ms);
            }
            let _ = i;
        }
        None
    }

    pub fn snapshot(&self) -> StateSnapshot {
        StateSnapshot {
            playing: self.playing,
            position_ms: self.position_ms,
            duration_ms: self.duration_ms,
            fps: self.fps,
            clips: self.clips.clone(),
        }
    }
}

pub type SharedState = Arc<Mutex<EditorState>>;

pub fn new_shared() -> SharedState {
    Arc::new(Mutex::new(EditorState::new()))
}
