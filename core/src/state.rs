use std::sync::{Arc, Mutex};

#[derive(Clone, serde::Serialize)]
pub struct StateSnapshot {
    pub playing: bool,
    pub position_ms: i64,
    pub duration_ms: i64,
    pub fps: f64,
}

pub struct EditorState {
    pub playing: bool,
    pub position_ms: i64,
    pub duration_ms: i64,
    pub fps: f64,
    pub pending_seek_ms: Option<i64>,
    /// Path of the file being edited, so an export can suggest a name.
    pub video_path: String,
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
            export: None,
        }
    }

    pub fn snapshot(&self) -> StateSnapshot {
        StateSnapshot {
            playing: self.playing,
            position_ms: self.position_ms,
            duration_ms: self.duration_ms,
            fps: self.fps,
        }
    }
}

pub type SharedState = Arc<Mutex<EditorState>>;

pub fn new_shared() -> SharedState {
    Arc::new(Mutex::new(EditorState::new()))
}
