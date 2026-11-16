//! The user's activity, sent as trajectory steps: `CODE_ACTION` for edits,
//! `SUPERCOMPLETE_FEEDBACK` after a prediction is accepted or rejected and
//! `CLIPBOARD` after a copy (`fork/spike/结论.md` 问题 9).
//!
//! Edits made to a buffer are merged into one step until something splits
//! them: a request (a trigger), or an IDE action such as accepting or rejecting
//! a prediction or copying text. Committed steps never change afterwards, so
//! the server's hashes for them stay valid.
//!
//! Retention: at most [`MAX_STEPS`] steps and nothing older than [`MAX_AGE`].
//! Dropping steps from the front invalidates every cached hash, so steps are
//! dropped in batches (down to [`TRIMMED_STEPS`], or everything older than
//! [`TRIMMED_AGE`]) to keep full resends rare.

use std::time::{Duration, Instant, SystemTime};

use collections::{HashMap, HashSet};
use gpui::{App, Entity, EntityId, WeakEntity};
use language::{Buffer, BufferSnapshot};
use similar::{Algorithm, DiffOp};

use crate::{
    context::buffer_location,
    native::timestamp,
    proto::exa::{
        cortex_pb::{
            ActionResult, ActionResultEdit, ActionSpec, ActionSpecCommand, CortexStepCodeAction,
            CortexStepMetadata, CortexStepSource, CortexStepStatus, CortexStepType,
            CortexTrajectoryStep, LineRangeTarget, action_result, action_spec, action_spec_command,
            cortex_trajectory_step,
        },
        diff_action_pb::{DiffBlock, UnifiedDiff, UnifiedDiffLineType, unified_diff},
    },
};

/// Unchanged lines kept around an edit in its diff.
const CONTEXT_LINES: usize = 10;
pub const MAX_STEPS: usize = 50;
pub const TRIMMED_STEPS: usize = 40;
pub const MAX_AGE: Duration = Duration::from_secs(10 * 60);
pub const TRIMMED_AGE: Duration = Duration::from_secs(8 * 60);

struct TrackedBuffer {
    buffer: WeakEntity<Buffer>,
    baseline: BufferSnapshot,
}

struct Entry {
    step: CortexTrajectoryStep,
    recorded_at: Instant,
}

pub struct Trajectory {
    mainline_id: String,
    entries: Vec<Entry>,
    buffers: HashMap<EntityId, TrackedBuffer>,
    uris_with_original_content: HashSet<String>,
}

impl Default for Trajectory {
    fn default() -> Self {
        Self {
            mainline_id: uuid::Uuid::new_v4().to_string(),
            entries: Vec::new(),
            buffers: HashMap::default(),
            uris_with_original_content: HashSet::default(),
        }
    }
}

impl Trajectory {
    pub fn mainline_id(&self) -> &str {
        &self.mainline_id
    }

    pub fn steps(&self) -> Vec<CortexTrajectoryStep> {
        self.entries
            .iter()
            .map(|entry| entry.step.clone())
            .collect()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Starts recording edits to `buffer`, using its current text as the
    /// baseline. Does nothing for buffers that are already tracked.
    pub fn track(&mut self, buffer: &Entity<Buffer>, cx: &App) {
        self.buffers
            .entry(buffer.entity_id())
            .or_insert_with(|| TrackedBuffer {
                buffer: buffer.downgrade(),
                baseline: buffer.read(cx).snapshot(),
            });
    }

    pub fn is_tracked(&self, buffer: &Entity<Buffer>) -> bool {
        self.buffers.contains_key(&buffer.entity_id())
    }

    /// The tracked buffers that are still alive.
    pub fn tracked_buffers(&self) -> impl Iterator<Item = Entity<Buffer>> + '_ {
        self.buffers
            .values()
            .filter_map(|tracked| tracked.buffer.upgrade())
    }

    /// Turns the edits made to each tracked buffer since the last call into
    /// one step per buffer.
    pub fn commit(&mut self, cx: &App) {
        let mut released = Vec::new();
        let mut pending = Vec::new();
        for (entity_id, tracked) in &mut self.buffers {
            let Some(buffer) = tracked.buffer.upgrade() else {
                released.push(*entity_id);
                continue;
            };
            let buffer = buffer.read(cx);
            if !buffer.version().changed_since(tracked.baseline.version()) {
                continue;
            }
            let current = buffer.snapshot();
            let baseline = std::mem::replace(&mut tracked.baseline, current.clone());
            if let Some(location) = buffer_location(buffer, cx) {
                pending.push((location.uri(), baseline, current));
            }
        }
        for entity_id in released {
            self.buffers.remove(&entity_id);
        }
        for (uri, baseline, current) in pending {
            self.push_edit(&uri, &baseline.text(), &current.text(), cx);
        }
        self.prune(cx);
    }

    /// Commits the edits made to `buffer` up to `snapshot`, an earlier state
    /// of the buffer. Used to split the user's typing from an accepted
    /// prediction that was applied after `snapshot`.
    pub fn commit_buffer_at(
        &mut self,
        buffer: &Entity<Buffer>,
        snapshot: &BufferSnapshot,
        cx: &App,
    ) {
        let Some(tracked) = self.buffers.get_mut(&buffer.entity_id()) else {
            return;
        };
        if !snapshot.version().changed_since(tracked.baseline.version()) {
            return;
        }
        let baseline = std::mem::replace(&mut tracked.baseline, snapshot.clone());
        if let Some(location) = buffer_location(buffer.read(cx), cx) {
            self.push_edit(&location.uri(), &baseline.text(), &snapshot.text(), cx);
        }
        self.prune(cx);
    }

    /// The step that accepting a prediction would add: the edits not yet
    /// committed plus the prediction, i.e. the buffer's baseline →
    /// `assumed_text`. Doesn't change the trajectory.
    pub fn assumed_step(
        &self,
        buffer: &Entity<Buffer>,
        assumed_text: &str,
        cx: &App,
    ) -> Option<CortexTrajectoryStep> {
        let tracked = self.buffers.get(&buffer.entity_id())?;
        let location = buffer_location(buffer.read(cx), cx)?;
        let uri = location.uri();
        let old_text = tracked.baseline.text();
        let original_content =
            (!self.uris_with_original_content.contains(&uri)).then(|| old_text.clone());
        edit_step(
            &uri,
            &old_text,
            assumed_text,
            original_content,
            SystemTime::now(),
        )
    }

    /// Appends a step that isn't an edit (feedback, clipboard).
    pub fn push(&mut self, step: CortexTrajectoryStep, cx: &App) {
        self.insert(self.entries.len(), step, cx);
    }

    /// Inserts a step at `index`, clamped to the end.
    pub fn insert(&mut self, index: usize, step: CortexTrajectoryStep, cx: &App) {
        let index = index.min(self.entries.len());
        self.entries.insert(
            index,
            Entry {
                step,
                recorded_at: cx.background_executor().now(),
            },
        );
        self.prune(cx);
    }

    fn push_edit(&mut self, uri: &str, old_text: &str, new_text: &str, cx: &App) {
        let original_content =
            (!self.uris_with_original_content.contains(uri)).then(|| old_text.to_string());
        if let Some(step) = edit_step(uri, old_text, new_text, original_content, SystemTime::now())
        {
            self.uris_with_original_content.insert(uri.to_string());
            self.entries.push(Entry {
                step,
                recorded_at: cx.background_executor().now(),
            });
        }
    }

    fn prune(&mut self, cx: &App) {
        let now = cx.background_executor().now();
        prune_entries(&mut self.entries, now, |entry| entry.recorded_at);
    }
}

/// Applies the retention policy described in the module documentation.
fn prune_entries<T>(entries: &mut Vec<T>, now: Instant, recorded_at: impl Fn(&T) -> Instant) {
    let expired =
        |entry: &T, age: Duration| now.saturating_duration_since(recorded_at(entry)) > age;
    if entries.first().is_some_and(|entry| expired(entry, MAX_AGE)) {
        let keep_from = entries
            .iter()
            .position(|entry| !expired(entry, TRIMMED_AGE))
            .unwrap_or(entries.len());
        entries.drain(..keep_from);
    }
    if entries.len() > MAX_STEPS {
        entries.drain(..entries.len() - TRIMMED_STEPS);
    }
}

pub fn step_metadata(created_at: SystemTime) -> CortexStepMetadata {
    CortexStepMetadata {
        created_at: Some(timestamp(created_at)),
        source: CortexStepSource::UserExplicit as i32,
        ..Default::default()
    }
}

/// One edit as a `CODE_ACTION` step: a line-level unified diff of `old_text`
/// → `new_text` with up to ten unchanged lines around the change. Returns
/// `None` when the texts are equal.
pub fn edit_step(
    uri: &str,
    old_text: &str,
    new_text: &str,
    original_content: Option<String>,
    created_at: SystemTime,
) -> Option<CortexTrajectoryStep> {
    let old_lines: Vec<&str> = old_text.split('\n').collect();
    let new_lines: Vec<&str> = new_text.split('\n').collect();
    let operations = similar::capture_diff_slices(Algorithm::Myers, &old_lines, &new_lines);
    let changes: Vec<_> = operations
        .iter()
        .filter(|operation| !matches!(operation, DiffOp::Equal { .. }))
        .collect();
    let first_change = changes.first()?;
    let last_change = changes.last()?;

    let first_new = first_change.new_range().start;
    let first_old = first_change.old_range().start;
    let last_new = last_change.new_range().end;
    let start_line = first_new.saturating_sub(CONTEXT_LINES);
    let end_line = (last_new.max(first_new) + CONTEXT_LINES).min(new_lines.len() - 1);
    let old_start = first_old - (first_new - start_line);

    let mut lines = Vec::new();
    let mut push = |line_type: UnifiedDiffLineType, text: &str| {
        lines.push(unified_diff::UnifiedDiffLine {
            text: text.to_string(),
            r#type: line_type as i32,
        });
    };
    for operation in &operations {
        let old_range = operation.old_range();
        let new_range = operation.new_range();
        if let DiffOp::Equal { .. } = operation {
            for (old_index, new_index) in old_range.zip(new_range) {
                if (start_line..=end_line).contains(&new_index) {
                    push(UnifiedDiffLineType::Unchanged, old_lines[old_index]);
                }
            }
        } else {
            for old_index in old_range.filter(|old_index| *old_index >= old_start) {
                push(UnifiedDiffLineType::Delete, old_lines[old_index]);
            }
            for new_index in new_range {
                push(UnifiedDiffLineType::Insert, new_lines[new_index]);
            }
        }
    }

    Some(CortexTrajectoryStep {
        r#type: CortexStepType::CodeAction as i32,
        status: CortexStepStatus::Done as i32,
        metadata: Some(step_metadata(created_at)),
        step: Some(cortex_trajectory_step::Step::CodeAction(
            CortexStepCodeAction {
                action_spec: Some(ActionSpec {
                    spec: Some(action_spec::Spec::Command(ActionSpecCommand {
                        target: Some(action_spec_command::Target::LineRange(LineRangeTarget {
                            absolute_uri: uri.to_string(),
                            start_line: start_line as u32,
                            end_line: end_line as u32,
                        })),
                        ..Default::default()
                    })),
                    ..Default::default()
                }),
                action_result: Some(ActionResult {
                    result: Some(action_result::Result::Edit(ActionResultEdit {
                        absolute_uri: uri.to_string(),
                        diff: Some(DiffBlock {
                            start_line: start_line as i32,
                            end_line: end_line as i32,
                            unified_diff: Some(UnifiedDiff { lines }),
                            ..Default::default()
                        }),
                        original_content: original_content.unwrap_or_default(),
                        ..Default::default()
                    })),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )),
    })
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn diff_lines(step: &CortexTrajectoryStep) -> (i32, i32, Vec<String>, String) {
        let Some(cortex_trajectory_step::Step::CodeAction(action)) = &step.step else {
            panic!("expected a code action step");
        };
        let Some(action_result::Result::Edit(edit)) = action
            .action_result
            .as_ref()
            .and_then(|result| result.result.as_ref())
        else {
            panic!("expected an edit result");
        };
        let diff = edit.diff.as_ref().expect("diff");
        let lines = diff
            .unified_diff
            .as_ref()
            .expect("unified diff")
            .lines
            .iter()
            .map(|line| {
                let marker = match UnifiedDiffLineType::try_from(line.r#type) {
                    Ok(UnifiedDiffLineType::Insert) => '+',
                    Ok(UnifiedDiffLineType::Delete) => '-',
                    _ => ' ',
                };
                format!("{marker}{}", line.text)
            })
            .collect();
        (
            diff.start_line,
            diff.end_line,
            lines,
            edit.original_content.clone(),
        )
    }

    #[test]
    fn test_edit_step_matches_reference_layout() {
        let old_text: String = (0..30).map(|row| format!("l{row}\n")).collect();
        let new_text = old_text.replace("l15\n", "l15 changed\n");
        let step = edit_step(
            "file:///a.py",
            &old_text,
            &new_text,
            Some(old_text.clone()),
            SystemTime::UNIX_EPOCH,
        )
        .expect("step");
        assert_eq!(step.r#type, CortexStepType::CodeAction as i32);
        assert_eq!(step.status, CortexStepStatus::Done as i32);
        let (start, end, lines, original) = diff_lines(&step);
        assert_eq!((start, end), (5, 26));
        let mut expected: Vec<String> = (5..15).map(|row| format!(" l{row}")).collect();
        expected.push("-l15".to_string());
        expected.push("+l15 changed".to_string());
        expected.extend((16..=26).map(|row| format!(" l{row}")));
        assert_eq!(lines, expected);
        assert_eq!(original, old_text);
    }

    #[test]
    fn test_edit_step_at_file_boundaries() {
        let step = edit_step(
            "file:///a.py",
            "a\nb",
            "x\na\nb\ny",
            None,
            SystemTime::now(),
        )
        .expect("step");
        let (start, end, lines, original) = diff_lines(&step);
        assert_eq!((start, end), (0, 3));
        assert_eq!(lines, vec!["+x", " a", " b", "+y"]);
        assert_eq!(original, "");
        assert!(edit_step("file:///a.py", "same", "same", None, SystemTime::now()).is_none());
    }

    #[test]
    fn test_retention_drops_steps_in_batches() {
        let start = Instant::now();
        let mut entries: Vec<Instant> = (0..MAX_STEPS).map(|_| start).collect();
        prune_entries(&mut entries, start, |entry| *entry);
        assert_eq!(entries.len(), MAX_STEPS);
        entries.push(start);
        prune_entries(&mut entries, start, |entry| *entry);
        assert_eq!(entries.len(), TRIMMED_STEPS);

        // Ages: 11, 9, 7 and 1 minutes. Once the oldest is past ten minutes,
        // everything older than eight minutes goes.
        let now = start + Duration::from_secs(20 * 60);
        let minutes_ago = |minutes: u64| now - Duration::from_secs(minutes * 60);
        let mut entries = vec![
            minutes_ago(11),
            minutes_ago(9),
            minutes_ago(7),
            minutes_ago(1),
        ];
        prune_entries(&mut entries, now, |entry| *entry);
        assert_eq!(entries, vec![minutes_ago(7), minutes_ago(1)]);
        // Nothing is past ten minutes: nothing goes, even past eight.
        let mut entries = vec![minutes_ago(9), minutes_ago(1)];
        prune_entries(&mut entries, now, |entry| *entry);
        assert_eq!(entries.len(), 2);
    }
}
