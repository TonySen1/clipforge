//! Undo/redo history.
//!
//! Snapshot-based: every committed edit stores the whole `Project` before and
//! after. Cloning a project is cheap because the expensive parts (decoded
//! metadata, keyframe indices) live behind `Arc`, so a snapshot only copies the
//! clip/track vectors.
//!
//! The stack is bounded: at 200 entries a 10 000-clip project still costs a few
//! megabytes, and nobody undoes further than that.

use crate::domain::project::Project;
use std::collections::VecDeque;

pub const DEFAULT_LIMIT: usize = 200;

/// One committed change.
#[derive(Clone, Debug)]
struct Entry {
    /// i18n key describing the edit, e.g. `edit.split`.
    label: &'static str,
    before: Project,
    after: Project,
}

/// Undo/redo history for one project.
#[derive(Clone, Debug)]
pub struct History {
    done: VecDeque<Entry>,
    undone: Vec<Entry>,
    limit: usize,
}

impl Default for History {
    fn default() -> Self {
        Self::new(DEFAULT_LIMIT)
    }
}

impl History {
    pub fn new(limit: usize) -> Self {
        Self {
            done: VecDeque::new(),
            undone: Vec::new(),
            limit: limit.max(1),
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.done.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.undone.is_empty()
    }

    /// Label of the edit that undo would revert.
    pub fn undo_label(&self) -> Option<&'static str> {
        self.done.back().map(|entry| entry.label)
    }

    /// Label of the edit that redo would reapply.
    pub fn redo_label(&self) -> Option<&'static str> {
        self.undone.last().map(|entry| entry.label)
    }

    pub fn len(&self) -> usize {
        self.done.len()
    }

    pub fn is_empty(&self) -> bool {
        self.done.is_empty()
    }

    /// Runs `edit` and records it if it succeeds.
    ///
    /// The project is only mutated on success, so a refused edit leaves both
    /// the document and the history untouched.
    pub fn commit<E>(
        &mut self,
        project: &mut Project,
        label: &'static str,
        edit: impl FnOnce(&mut Project) -> Result<E, crate::domain::EditError>,
    ) -> Result<E, crate::domain::EditError> {
        let before = project.clone();
        let result = edit(project)?;

        // Any new edit invalidates the redo branch.
        self.undone.clear();
        self.done.push_back(Entry {
            label,
            before,
            after: project.clone(),
        });
        while self.done.len() > self.limit {
            self.done.pop_front();
        }

        Ok(result)
    }

    /// Records an already-applied change (used when the caller mutated the
    /// project itself, e.g. adding an imported source).
    pub fn push(&mut self, label: &'static str, before: Project, after: Project) {
        self.undone.clear();
        self.done.push_back(Entry {
            label,
            before,
            after,
        });
        while self.done.len() > self.limit {
            self.done.pop_front();
        }
    }

    /// Reverts the last edit. Returns its label so the UI can say what happened.
    pub fn undo(&mut self, project: &mut Project) -> Option<&'static str> {
        let entry = self.done.pop_back()?;
        *project = entry.before.clone();
        let label = entry.label;
        self.undone.push(entry);
        Some(label)
    }

    /// Reapplies the last undone edit.
    pub fn redo(&mut self, project: &mut Project) -> Option<&'static str> {
        let entry = self.undone.pop()?;
        *project = entry.after.clone();
        let label = entry.label;
        self.done.push_back(entry);
        Some(label)
    }

    /// Drops all history, e.g. when a new project is opened.
    pub fn clear(&mut self) {
        self.done.clear();
        self.undone.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::edit::EditError;
    use crate::domain::{ClipId, MediaSource, SourceId};
    use crate::media::{probe, KeyframeIndex};
    use crate::time::Time;

    fn source(id: u64, seconds: f64) -> MediaSource {
        let json = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/fixtures/ffprobe/sample_h264.json"),
        )
        .unwrap();
        let mut info = probe::parse(std::path::Path::new("a.mp4"), &json).unwrap();
        info.duration = Time::from_secs(seconds);
        MediaSource::new(
            SourceId(id),
            info,
            KeyframeIndex::new(vec![Time::ZERO, Time::from_secs(seconds)]),
        )
    }

    fn project() -> Project {
        let mut project = Project::empty();
        let id = project.alloc_source_id();
        let mut src = source(0, 10.0);
        src.id = id;
        project.add_source(src);
        project
    }

    fn first(project: &Project) -> ClipId {
        project.video_track().unwrap().clips[0].id
    }

    #[test]
    fn starts_empty() {
        let history = History::default();
        assert!(!history.can_undo());
        assert!(!history.can_redo());
        assert_eq!(history.len(), 0);
    }

    #[test]
    fn commit_records_and_undo_reverts() {
        let mut project = project();
        let mut history = History::default();
        let id = first(&project);

        history
            .commit(&mut project, "edit.split", |p| {
                p.split_clip(id, Time::from_secs(4.0), Default::default())
            })
            .unwrap();
        assert_eq!(project.video_track().unwrap().clips.len(), 2);
        assert!(history.can_undo());

        assert_eq!(history.undo(&mut project), Some("edit.split"));
        assert_eq!(project.video_track().unwrap().clips.len(), 1);
        assert!(history.can_redo());
    }

    #[test]
    fn redo_reapplies() {
        let mut project = project();
        let mut history = History::default();
        let id = first(&project);

        history
            .commit(&mut project, "edit.split", |p| {
                p.split_clip(id, Time::from_secs(4.0), Default::default())
            })
            .unwrap();
        history.undo(&mut project);
        history.redo(&mut project);

        assert_eq!(project.video_track().unwrap().clips.len(), 2);
        assert!(!history.can_redo());
    }

    #[test]
    fn failed_edit_changes_nothing() {
        let mut project = project();
        let mut history = History::default();
        let id = first(&project);

        let err = history
            .commit(&mut project, "edit.split", |p| {
                p.split_clip(id, Time::ZERO, Default::default())
            })
            .unwrap_err();

        assert!(matches!(err, EditError::SplitOutsideClip { .. }));
        assert!(!history.can_undo(), "a refused edit must not be recorded");
        assert_eq!(project.video_track().unwrap().clips.len(), 1);
    }

    #[test]
    fn new_edit_clears_the_redo_branch() {
        let mut project = project();
        let mut history = History::default();
        let id = first(&project);

        history
            .commit(&mut project, "edit.split", |p| {
                p.split_clip(id, Time::from_secs(4.0), Default::default())
            })
            .unwrap();
        history.undo(&mut project);
        assert!(history.can_redo());

        history
            .commit(&mut project, "edit.split", |p| {
                p.split_clip(id, Time::from_secs(6.0), Default::default())
            })
            .unwrap();
        assert!(!history.can_redo(), "redo branch must be discarded");
    }

    #[test]
    fn history_is_bounded() {
        let mut project = project();
        let mut history = History::new(3);
        let id = first(&project);

        for _ in 0..10 {
            // Always split the longest clip so no sliver forms.
            history
                .commit(&mut project, "edit.split", |p| {
                    let clip = p
                        .video_track()
                        .unwrap()
                        .clips
                        .iter()
                        .max_by_key(|c| c.duration().micros())
                        .unwrap()
                        .clone();
                    let mid = clip
                        .timeline_start
                        .saturating_add(Time(clip.duration().micros() / 2));
                    p.split_clip(clip.id, mid, Default::default())
                })
                .unwrap();
        }

        assert_eq!(history.len(), 3);
        let _ = id;
    }

    #[test]
    fn undo_and_redo_are_noops_at_the_ends() {
        let mut project = project();
        let mut history = History::default();

        assert_eq!(history.undo(&mut project), None);
        assert_eq!(history.redo(&mut project), None);
    }

    #[test]
    fn labels_are_reported() {
        let mut project = project();
        let mut history = History::default();
        let id = first(&project);

        history
            .commit(&mut project, "edit.split", |p| {
                p.split_clip(id, Time::from_secs(4.0), Default::default())
            })
            .unwrap();

        assert_eq!(history.undo_label(), Some("edit.split"));
        assert_eq!(history.redo_label(), None);

        history.undo(&mut project);
        assert_eq!(history.redo_label(), Some("edit.split"));
    }

    #[test]
    fn clear_drops_everything() {
        let mut project = project();
        let mut history = History::default();
        let id = first(&project);

        history
            .commit(&mut project, "edit.split", |p| {
                p.split_clip(id, Time::from_secs(4.0), Default::default())
            })
            .unwrap();
        history.undo(&mut project);

        history.clear();
        assert!(!history.can_undo());
        assert!(!history.can_redo());
    }

    #[test]
    fn multi_step_undo_walks_back_in_order() {
        let mut project = project();
        let mut history = History::default();
        let id = first(&project);

        // Split twice.
        history
            .commit(&mut project, "edit.split", |p| {
                p.split_clip(id, Time::from_secs(3.0), Default::default())
            })
            .unwrap();
        let second = project.video_track().unwrap().clips[1].id;
        history
            .commit(&mut project, "edit.trim", |p| {
                p.trim_clip(
                    second,
                    crate::domain::TrimEdge::End,
                    Time::from_secs(7.0),
                    Default::default(),
                )
            })
            .unwrap();

        assert_eq!(history.undo(&mut project), Some("edit.trim"));
        assert_eq!(
            project.video_track().unwrap().clips[1].timeline_end(),
            Time::from_secs(10.0)
        );

        assert_eq!(history.undo(&mut project), Some("edit.split"));
        assert_eq!(project.video_track().unwrap().clips.len(), 1);

        assert_eq!(history.undo(&mut project), None);
    }
}
