use std::sync::{Arc, Mutex};

/// The grade's parameters, in the order they are applied.
///
/// Defaults reproduce the look this project shipped before any of it was
/// adjustable: saturation 1.4 and a gamma of 1.1. An unedited project therefore
/// renders exactly as it used to, which is the property that makes it safe to
/// route every frame through this.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GradeParams {
    /// Stops, applied as a power of two. 0 is unchanged.
    pub exposure: f32,
    /// Around a mid grey pivot. 1 is unchanged.
    pub contrast: f32,
    /// Toward luma. 1 is unchanged.
    pub saturation: f32,
    /// Applied as `pow(c, 1/gamma)`. 1 is unchanged.
    pub gamma: f32,
}

impl GradeParams {
    /// The four floats the shaders read, in declaration order.
    pub fn to_array(&self) -> [f32; 4] {
        [self.exposure, self.contrast, self.saturation, self.gamma]
    }
}

impl Default for GradeParams {
    fn default() -> Self {
        Self {
            exposure: 0.0,
            contrast: 1.0,
            saturation: 1.4,
            gamma: 1.1,
        }
    }
}

/// Where a clip's picture sits in the frame.
///
/// This is the beginning of compositing: a clip is a picture that can be made
/// smaller and moved, and whatever it does not cover is black until there is a
/// layer underneath to show instead.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Transform {
    /// 1.0 fills the frame; 0.5 is half as wide and half as tall.
    pub scale: f32,
    /// Offset from centre, as a fraction of the frame.
    pub offset_x: f32,
    pub offset_y: f32,
}

impl Default for Transform {
    fn default() -> Self {
        Self {
            scale: 1.0,
            offset_x: 0.0,
            offset_y: 0.0,
        }
    }
}

impl Transform {
    /// What the third uniform vector carries.
    pub fn to_array(&self) -> [f32; 4] {
        [self.scale, self.offset_x, self.offset_y, 0.0]
    }
}

/// One node of a clip's effect chain.
///
/// One variant today. The chain is an ordered list rather than a single set of
/// fields because the order is the thing that stops being obvious the moment a
/// second kind of node exists.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Effect {
    Grade(GradeParams),
}

impl Effect {
    pub fn name(&self) -> &'static str {
        match self {
            Effect::Grade(_) => "Grade",
        }
    }
}

/// An edit, and its own inverse.
///
/// Every change to the document goes through one of these so it can be undone,
/// and `invert` never reads the document: each variant carries what its inverse
/// needs. That keeps undo correct when other edits have landed since.
#[derive(Clone, Debug, PartialEq)]
pub enum Edit {
    /// Cut a clip in two. `before` is the clip as it was, so the inverse can
    /// name the tail it is about to remove.
    SplitClip {
        index: usize,
        before: Clip,
        at_source_ms: i64,
    },
    /// Put a split back together: restore the head and drop the tail.
    ///
    /// A split is not undone by removing the tail alone -- the head's out point
    /// was moved by the split and has to move back, which is what `before`
    /// carries.
    MergeClip {
        index: usize,
        before: Clip,
        at_source_ms: i64,
    },
    InsertClip {
        index: usize,
        clip: Clip,
    },
    DeleteClip {
        index: usize,
        before: Clip,
    },
    /// Reorder: take the clip at `from` and put it where `to` is.
    MoveClip {
        from: usize,
        to: usize,
    },
    /// Change a clip's source range. Carries both ends so the inverse is just
    /// the swap, which is why a trim does not need to know which edge moved.
    TrimClip {
        index: usize,
        before: Clip,
        after: Clip,
    },
    InsertEffect {
        clip: usize,
        at: usize,
        effect: Effect,
    },
    RemoveEffect {
        clip: usize,
        at: usize,
        effect: Effect,
    },
    SetEffect {
        clip: usize,
        at: usize,
        before: Effect,
        after: Effect,
    },
    /// Move or resize a clip's picture within the frame.
    SetTransform {
        clip: usize,
        before: Transform,
        after: Transform,
    },
}

impl Edit {
    /// The tail a split produces.
    fn split_tail(before: &Clip, at_source_ms: i64) -> Clip {
        Clip {
            in_ms: at_source_ms,
            out_ms: before.out_ms,
            effects: before.effects.clone(),
            transform: before.transform,
        }
    }

    pub fn apply(&self, s: &mut EditorState) {
        match self.clone() {
            Edit::SplitClip {
                index,
                before,
                at_source_ms,
            } => {
                if s.clips.get(index).is_some()
                    && at_source_ms > before.in_ms
                    && at_source_ms < before.out_ms
                {
                    let mut head = before.clone();
                    head.out_ms = at_source_ms;
                    s.clips[index] = head;
                    s.clips.insert(index + 1, Edit::split_tail(&before, at_source_ms));
                }
            }
            Edit::MergeClip { index, before, .. } => {
                if index + 1 < s.clips.len() {
                    s.clips.remove(index + 1);
                }
                if index < s.clips.len() {
                    s.clips[index] = before.clone();
                }
            }
            Edit::InsertClip { index, clip } => {
                let at = index.min(s.clips.len());
                s.clips.insert(at, clip);
            }
            Edit::DeleteClip { index, .. } => {
                if index < s.clips.len() {
                    s.clips.remove(index);
                }
            }
            Edit::MoveClip { from, to } => {
                if from < s.clips.len() && to < s.clips.len() {
                    let clip = s.clips.remove(from);
                    s.clips.insert(to, clip);
                }
            }
            Edit::TrimClip { index, after, .. } => {
                if let Some(slot) = s.clips.get_mut(index) {
                    *slot = after;
                }
            }
            Edit::InsertEffect { clip, at, effect } => {
                if let Some(c) = s.clips.get_mut(clip) {
                    let at = at.min(c.effects.len());
                    c.effects.insert(at, effect);
                }
            }
            Edit::RemoveEffect { clip, at, .. } => {
                if let Some(c) = s.clips.get_mut(clip) {
                    if at < c.effects.len() {
                        c.effects.remove(at);
                    }
                }
            }
            Edit::SetEffect {
                clip, at, after, ..
            } => {
                if let Some(c) = s.clips.get_mut(clip) {
                    if let Some(slot) = c.effects.get_mut(at) {
                        *slot = after;
                    }
                }
            }
            Edit::SetTransform { clip, after, .. } => {
                if let Some(c) = s.clips.get_mut(clip) {
                    c.transform = after;
                }
            }
        }
        s.refresh_duration();
        if self.changes_look() {
            s.look_version += 1;
        }
        s.dirty = true;
    }

    /// Whether applying this edit changes what a frame looks like, as opposed to
    /// where the frames come from.
    pub fn changes_look(&self) -> bool {
        matches!(
            self,
            Edit::InsertEffect { .. }
                | Edit::RemoveEffect { .. }
                | Edit::SetEffect { .. }
                | Edit::SetTransform { .. }
        )
    }

    pub fn invert(&self) -> Edit {
        match self.clone() {
            Edit::SplitClip {
                index,
                before,
                at_source_ms,
            } => Edit::MergeClip {
                index,
                before,
                at_source_ms,
            },
            Edit::MergeClip {
                index,
                before,
                at_source_ms,
            } => Edit::SplitClip {
                index,
                before,
                at_source_ms,
            },
            Edit::InsertClip { index, clip } => Edit::DeleteClip {
                index,
                before: clip,
            },
            Edit::DeleteClip { index, before } => Edit::InsertClip { index, clip: before },
            Edit::MoveClip { from, to } => Edit::MoveClip {
                from: to,
                to: from,
            },
            Edit::TrimClip {
                index,
                before,
                after,
            } => Edit::TrimClip {
                index,
                before: after,
                after: before,
            },
            Edit::InsertEffect { clip, at, effect } => Edit::RemoveEffect { clip, at, effect },
            Edit::RemoveEffect { clip, at, effect } => Edit::InsertEffect { clip, at, effect },
            Edit::SetEffect {
                clip,
                at,
                before,
                after,
            } => Edit::SetEffect {
                clip,
                at,
                before: after,
                after: before,
            },
            Edit::SetTransform {
                clip,
                before,
                after,
            } => Edit::SetTransform {
                clip,
                before: after,
                after: before,
            },
        }
    }

    /// Whether a new edit should fold into the one before it.
    ///
    /// Dragging a slider produces an edit per pointer move. Without this the undo
    /// stack fills with the drag's intermediate states and a single undo goes
    /// back one pixel.
    pub fn coalesces_with(&self, other: &Edit) -> bool {
        match (self, other) {
            (
                Edit::SetEffect { clip: a, at: ai, .. },
                Edit::SetEffect { clip: b, at: bi, .. },
            ) => a == b && ai == bi,
            (
                Edit::SetTransform { clip: a, .. },
                Edit::SetTransform { clip: b, .. },
            ) => a == b,
            _ => false,
        }
    }
}

#[derive(Clone, serde::Serialize)]
pub struct StateSnapshot {
    pub playing: bool,
    pub position_ms: i64,
    pub duration_ms: i64,
    pub fps: f64,
    /// The timeline, in order, as source ranges of the loaded file.
    pub clips: Vec<Clip>,
    /// How many edits can be undone and redone, so the UI can grey its buttons.
    pub undo_depth: usize,
    pub redo_depth: usize,
}

/// One segment of the loaded file, in source milliseconds.
///
/// The timeline is a list of these, so trimming and reordering the media is a
/// matter of changing the list. Clips from *different* files would need a
/// decoder per file and are not modelled here: the source is one file, and this
/// says which parts of it play, in what order.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Clip {
    pub in_ms: i64,
    pub out_ms: i64,
    /// This clip's own effect chain, applied in order.
    #[serde(default)]
    pub effects: Vec<Effect>,
    /// Where this clip's picture sits in the frame.
    #[serde(default)]
    pub transform: Transform,
}

impl Clip {
    pub fn new(in_ms: i64, out_ms: i64) -> Self {
        Self {
            in_ms,
            out_ms,
            effects: Vec::new(),
            transform: Transform::default(),
        }
    }

    /// The grade for this clip, or the default look if it has none.
    pub fn grade(&self) -> GradeParams {
        self.effects
            .iter()
            .find_map(|e| match e {
                Effect::Grade(p) => Some(*p),
            })
            .unwrap_or_default()
    }
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
    /// Length of that file. Not the same as `duration_ms`, which is the length
    /// of the *timeline* once clips have been cut, moved or removed.
    pub source_duration_ms: i64,
    /// The edit itself. Empty means the whole file, which is what a freshly
    /// opened clip gets.
    pub clips: Vec<Clip>,
    /// The export in flight or the last one to finish.
    pub export: Option<ExportJob>,
    /// Edits that can be undone, oldest first, and edits that can be redone.
    undo: Vec<Edit>,
    redo: Vec<Edit>,
    /// Bumped whenever an edit changes what a frame should look like.
    ///
    /// Graded frames live in the renderer's ring. An effect change makes every
    /// one of them stale, and without this the ring goes on serving the old look
    /// until each frame happens to be decoded again -- which is why a grade
    /// appeared to apply itself only after scrubbing back and forth a few times.
    pub look_version: u64,
    /// Set when something changed that the UI has not been told about yet.
    pub dirty: bool,
}

/// Where an export has got to.
#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ExportStage {
    Idle,
    Running,
    /// Written, not yet checked.
    ///
    /// Distinct from Running on purpose: the render loop starts an export when
    /// the stage is Running, so reporting a finished-but-unverified export as
    /// Running started a second one, which overwrote the file the first had just
    /// written.
    Verifying,
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
            source_duration_ms: 0,
            clips: Vec::new(),
            export: None,
            undo: Vec::new(),
            redo: Vec::new(),
            look_version: 0,
            dirty: false,
        }
    }

    /// Apply an edit, and remember how to undo it.
    ///
    /// A repeated `SetEffect` on the same node folds into the one before it, so a
    /// slider drag is one undo step rather than a hundred.
    pub fn push_edit(&mut self, edit: Edit) {
        let folded = match (self.undo.last_mut(), &edit) {
            (Some(prev), Edit::SetEffect { after, .. }) if prev.coalesces_with(&edit) => {
                if let Edit::SetEffect { after: slot, .. } = prev {
                    *slot = *after;
                }
                true
            }
            _ => false,
        };
        if !folded {
            self.undo.push(edit.clone());
        }
        edit.apply(self);
        self.redo.clear();
    }

    pub fn undo(&mut self) -> bool {
        if let Some(edit) = self.undo.pop() {
            let inverse = edit.invert();
            inverse.apply(self);
            self.redo.push(edit);
            true
        } else {
            false
        }
    }

    pub fn redo(&mut self) -> bool {
        if let Some(edit) = self.redo.pop() {
            edit.apply(self);
            self.undo.push(edit);
            true
        } else {
            false
        }
    }

    pub fn undo_depth(&self) -> usize {
        self.undo.len()
    }

    pub fn redo_depth(&self) -> usize {
        self.redo.len()
    }

    /// The clip playing at a source position, if any.
    pub fn clip_index_for_source(&self, source_ms: i64) -> Option<usize> {
        self.clips.iter().position(|c| c.contains(source_ms))
    }

    /// One clip covering everything, for a file nobody has cut yet.
    pub fn reset_timeline(&mut self, source_duration_ms: i64) {
        self.clips = vec![Clip::new(0, source_duration_ms.max(0))];
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
            undo_depth: self.undo.len(),
            redo_depth: self.redo.len(),
        }
    }
}

pub type SharedState = Arc<Mutex<EditorState>>;

pub fn new_shared() -> SharedState {
    Arc::new(Mutex::new(EditorState::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn three_clips() -> EditorState {
        let mut s = EditorState::new();
        s.clips = vec![Clip::new(0, 1000), Clip::new(2000, 3000), Clip::new(5000, 6000)];
        s.refresh_duration();
        s
    }

    #[test]
    fn splitting_and_undoing_leaves_the_timeline_as_it_was() {
        let mut s = three_clips();
        let before = s.clips.clone();
        let clip = s.clips[0].clone();
        s.push_edit(Edit::SplitClip {
            index: 0,
            before: clip,
            at_source_ms: 400,
        });
        assert_eq!(s.clips.len(), 4);
        assert_eq!(s.clips[0].out_ms, 400);
        assert_eq!(s.clips[1].in_ms, 400);
        assert_eq!(s.duration_ms, 3000);

        assert!(s.undo());
        assert_eq!(s.clips, before);
        assert!(s.redo());
        assert_eq!(s.clips.len(), 4);
    }

    #[test]
    fn deleting_a_clip_and_undoing_puts_it_back_in_place() {
        let mut s = three_clips();
        let before = s.clips.clone();
        let removed = s.clips[1].clone();
        s.push_edit(Edit::DeleteClip {
            index: 1,
            before: removed,
        });
        assert_eq!(s.clips.len(), 2);
        assert_eq!(s.duration_ms, 2000);
        assert!(s.undo());
        assert_eq!(s.clips, before);
    }

    #[test]
    fn a_slider_drag_is_one_undo_step() {
        let mut s = three_clips();
        s.clips[0].effects = vec![Effect::Grade(GradeParams::default())];
        for exposure in [0.25, 0.5, 0.9] {
            let before = s.clips[0].effects[0];
            s.push_edit(Edit::SetEffect {
                clip: 0,
                at: 0,
                before,
                after: Effect::Grade(GradeParams {
                    exposure,
                    ..Default::default()
                }),
            });
        }
        assert_eq!(s.clips[0].grade().exposure, 0.9);
        // Three moves of one slider, and one thing to undo.
        assert_eq!(s.undo_depth(), 1);
        assert!(s.undo());
        assert_eq!(s.clips[0].grade().exposure, 0.0);
    }

    #[test]
    fn timeline_and_source_positions_map_both_ways() {
        let s = three_clips();
        // 1500 ms into the timeline is 500 ms into the second clip's source.
        assert_eq!(s.source_for_timeline(1500), Some((1, 2500)));
        // The round trip is the property that matters: any timeline position
        // maps to a source position that maps back to it.
        for timeline in [0, 500, 999, 1000, 1500, 2999] {
            let (_, source) = s.source_for_timeline(timeline).expect("in range");
            assert_eq!(s.timeline_for_source(source), timeline, "at {timeline}");
        }
        assert_eq!(s.timeline_for_source(2500), 1500);
        // A gap in the source between clips is not on the timeline at all.
        assert_eq!(s.clip_index_for_source(1500), None);
    }

    #[test]
    fn the_default_grade_is_the_look_that_was_always_there() {
        let g = GradeParams::default();
        assert_eq!(g.to_array(), [0.0, 1.0, 1.4, 1.1]);
        // A clip with no effects grades with the default.
        assert_eq!(Clip::new(0, 100).grade(), g);
    }
}

#[cfg(test)]
mod edit_tests {
    use super::*;

    fn three() -> EditorState {
        let mut s = EditorState::new();
        s.clips = vec![Clip::new(0, 1000), Clip::new(2000, 3000), Clip::new(5000, 6000)];
        s.refresh_duration();
        s
    }

    #[test]
    fn moving_a_clip_and_undoing_puts_the_order_back() {
        let mut s = three();
        let before = s.clips.clone();
        s.push_edit(Edit::MoveClip { from: 0, to: 2 });
        assert_eq!(s.clips[2].in_ms, 0);
        assert_eq!(s.clips[0].in_ms, 2000);
        assert_eq!(s.duration_ms, 3000);
        assert!(s.undo());
        assert_eq!(s.clips, before);
    }

    #[test]
    fn trimming_and_undoing_restores_both_ends() {
        let mut s = three();
        let before = s.clips.clone();
        let original = s.clips[0].clone();
        let mut trimmed = original.clone();
        trimmed.in_ms = 250;
        trimmed.out_ms = 800;
        s.push_edit(Edit::TrimClip {
            index: 0,
            before: original,
            after: trimmed,
        });
        assert_eq!(s.clips[0].duration_ms(), 550);
        assert_eq!(s.duration_ms, 2550);
        assert!(s.undo());
        assert_eq!(s.clips, before);
    }

    #[test]
    fn a_clip_carries_its_effects_through_a_move_and_a_trim() {
        let mut s = three();
        s.clips[0].effects = vec![Effect::Grade(GradeParams {
            exposure: 1.0,
            ..Default::default()
        })];
        let moving = s.clips[0].clone();
        s.push_edit(Edit::MoveClip { from: 0, to: 1 });
        assert_eq!(s.clips[1].effects, moving.effects);
        let mut trimmed = s.clips[1].clone();
        trimmed.out_ms -= 100;
        let original = s.clips[1].clone();
        s.push_edit(Edit::TrimClip {
            index: 1,
            before: original,
            after: trimmed,
        });
        assert_eq!(s.clips[1].grade().exposure, 1.0);
    }
}

#[cfg(test)]
mod transform_tests {
    use super::*;

    #[test]
    fn a_transform_undoes_to_where_it_was() {
        let mut s = EditorState::new();
        s.clips = vec![Clip::new(0, 1000)];
        s.refresh_duration();
        s.push_edit(Edit::SetTransform {
            clip: 0,
            before: Transform::default(),
            after: Transform {
                scale: 0.5,
                offset_x: 0.25,
                offset_y: -0.25,
            },
        });
        assert_eq!(s.clips[0].transform.scale, 0.5);
        assert_eq!(s.look_version, 1);
        assert!(s.undo());
        assert_eq!(s.clips[0].transform, Transform::default());
        // A transform changes what a frame looks like, like any other grading
        // edit, so the ring has to be told.
        assert_eq!(s.look_version, 2);
    }

    #[test]
    fn a_transform_survives_a_split() {
        let mut s = EditorState::new();
        s.clips = vec![Clip::new(0, 1000)];
        s.clips[0].transform.scale = 0.5;
        s.refresh_duration();
        let before = s.clips[0].clone();
        s.push_edit(Edit::SplitClip {
            index: 0,
            before,
            at_source_ms: 400,
        });
        assert_eq!(s.clips[0].transform.scale, 0.5);
        assert_eq!(s.clips[1].transform.scale, 0.5);
    }
}
