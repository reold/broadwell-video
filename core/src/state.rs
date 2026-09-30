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
}

impl EditorState {
    pub fn new() -> Self {
        Self {
            playing: true,
            position_ms: 0,
            duration_ms: 0,
            fps: 30.0,
            pending_seek_ms: None,
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
