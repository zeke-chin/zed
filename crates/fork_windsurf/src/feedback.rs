//! What happens after a prediction is accepted or rejected (`fork/spike/结论.md`
//! 问题 7, 9, 21):
//!
//! - a `SUPERCOMPLETE_FEEDBACK` step goes into the trajectory;
//! - an entry goes into `supercomplete_feedback_context` of later requests,
//!   with the prediction as `diff_suggestion` (character and line diffs of the
//!   window);
//! - a `ProvideFeedback` call reports it, at most once per
//!   [`MIN_REPORT_INTERVAL`]; failures are only logged.
//!
//! Only intentional rejections count: Zed reports `Rejected` for Escape and
//! for picking a completion menu entry over the prediction, and `Ignored` for
//! everything else, which isn't reported.

use std::time::{Duration, SystemTime};

use similar::{ChangeTag, TextDiff};

use crate::{
    backend::DocumentContext,
    context::SelectionWindow,
    native::timestamp,
    proto::exa::{
        codeium_common_pb::{Document, DocumentPosition, ProviderSource},
        cortex_pb::{
            CortexStepStatus, CortexStepSupercompleteFeedback, CortexStepType,
            CortexTrajectoryStep, cortex_trajectory_step,
        },
        diff_action_pb::{
            CharacterDiff, CharacterDiffChange, DiffChangeType, UnifiedDiff, UnifiedDiffLineType,
            unified_diff,
        },
        language_server_pb::handle_streaming_tab_v2_response::Diff,
        prompt_pb::SupercompleteFeedback,
    },
    trajectory::step_metadata,
};

/// Entries kept in `supercomplete_feedback_context`. The official client kept
/// every entry of a 70 second session (two); this bounds a long session.
pub const MAX_FEEDBACK_ENTRIES: usize = 5;
pub const MIN_REPORT_INTERVAL: Duration = Duration::from_secs(5);

/// The parts of a request's document that feedback refers to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentInfo {
    pub uri: String,
    pub workspace_uri: String,
    pub editor_language: String,
}

impl DocumentInfo {
    pub fn of(document: &DocumentContext) -> Self {
        Self {
            uri: document.absolute_uri.clone(),
            workspace_uri: document.workspace_uri.clone(),
            editor_language: document.editor_language.clone(),
        }
    }
}

/// A prediction as shown to the user, kept for feedback.
#[derive(Clone, Debug, PartialEq)]
pub struct PredictionRecord {
    pub completion_id: String,
    /// The `prompt_id` of the `GetTab` request that produced it.
    pub prompt_id: String,
    pub uri: String,
    pub workspace_uri: String,
    pub editor_language: String,
    pub selection_start_line: u32,
    pub selection_end_line: u32,
    pub old_text: String,
    pub new_text: String,
    /// Row and column (characters) of the cursor after accepting.
    pub cursor_position: (u32, u32),
    /// Came from a predictive request.
    pub from_cache: bool,
}

impl PredictionRecord {
    pub fn new(
        prompt_id: String,
        document: &DocumentInfo,
        window: &SelectionWindow,
        old_text: String,
        new_text: String,
        cursor_position: (u32, u32),
        from_cache: bool,
    ) -> Self {
        Self {
            completion_id: uuid::Uuid::new_v4().to_string(),
            prompt_id,
            uri: document.uri.clone(),
            workspace_uri: document.workspace_uri.clone(),
            editor_language: document.editor_language.clone(),
            selection_start_line: window.start_line,
            selection_end_line: window.end_line,
            old_text,
            new_text,
            cursor_position,
            from_cache,
        }
    }

    pub fn diff_suggestion(&self) -> Diff {
        Diff {
            path: self.uri.clone(),
            selection_start_line: u64::from(self.selection_start_line),
            selection_end_line: u64::from(self.selection_end_line),
            character_diff: Some(character_diff(&self.old_text, &self.new_text)),
            cursor_position: Some(DocumentPosition {
                row: u64::from(self.cursor_position.0),
                col: u64::from(self.cursor_position.1),
            }),
            old_str: self.old_text.clone(),
            unified_diff: Some(line_diff(&self.old_text, &self.new_text)),
            render_info: None,
        }
    }

    /// The `supercomplete_feedback_context` entry.
    pub fn context_entry(&self, accepted: bool, timestamp_ms: i64) -> SupercompleteFeedback {
        SupercompleteFeedback {
            accepted,
            intentional_reject: !accepted,
            completion_id: self.completion_id.clone(),
            timestamp_ms,
            diff_suggestion: Some(self.diff_suggestion()),
            tabjump_suggestion: None,
        }
    }

    /// The `SUPERCOMPLETE_FEEDBACK` step. `document_text` is the buffer when
    /// the feedback happened.
    pub fn feedback_step(
        &self,
        accepted: bool,
        document_text: String,
        is_dirty: bool,
        feedback_delay_ms: i64,
        created_at: SystemTime,
    ) -> CortexTrajectoryStep {
        CortexTrajectoryStep {
            r#type: CortexStepType::SupercompleteFeedback as i32,
            status: CortexStepStatus::Done as i32,
            metadata: Some(step_metadata(created_at)),
            step: Some(cortex_trajectory_step::Step::SupercompleteFeedback(
                CortexStepSupercompleteFeedback {
                    completion_id: self.completion_id.clone(),
                    completion_text: String::new(),
                    feedback_type: if accepted { "accept" } else { "reject" }.to_string(),
                    feedback_reason: if accepted { "" } else { "intentional" }.to_string(),
                    document: Some(Document {
                        absolute_uri: self.uri.clone(),
                        workspace_uri: self.workspace_uri.clone(),
                        text: document_text,
                        editor_language: self.editor_language.clone(),
                        line_ending: "\n".to_string(),
                        timestamp: Some(timestamp(created_at)),
                        is_dirty,
                        ..Default::default()
                    }),
                    feedback_delay_ms,
                    provider_source: ProviderSource::Supercomplete as i32,
                },
            )),
        }
    }
}

/// Character-level changes from `old_text` to `new_text`, consecutive
/// changes of the same kind merged.
pub fn character_diff(old_text: &str, new_text: &str) -> CharacterDiff {
    let diff = TextDiff::from_chars(old_text, new_text);
    let mut changes: Vec<CharacterDiffChange> = Vec::new();
    for change in diff.iter_all_changes() {
        let change_type = match change.tag() {
            ChangeTag::Equal => DiffChangeType::Unchanged,
            ChangeTag::Insert => DiffChangeType::Insert,
            ChangeTag::Delete => DiffChangeType::Delete,
        } as i32;
        match changes.last_mut() {
            Some(last) if last.r#type == change_type => last.text.push_str(change.value()),
            _ => changes.push(CharacterDiffChange {
                text: change.value().to_string(),
                r#type: change_type,
            }),
        }
    }
    CharacterDiff { changes }
}

/// Line-level diff of the whole window. Within a changed block the inserted
/// lines come before the deleted ones, as in the official client's
/// `diff_suggestion`.
pub fn line_diff(old_text: &str, new_text: &str) -> UnifiedDiff {
    let old_lines: Vec<&str> = old_text.split('\n').collect();
    let new_lines: Vec<&str> = new_text.split('\n').collect();
    let operations =
        similar::capture_diff_slices(similar::Algorithm::Myers, &old_lines, &new_lines);
    let mut lines = Vec::new();
    let mut push = |line_type: UnifiedDiffLineType, text: &str| {
        lines.push(unified_diff::UnifiedDiffLine {
            text: text.to_string(),
            r#type: line_type as i32,
        });
    };
    for operation in &operations {
        if let similar::DiffOp::Equal { .. } = operation {
            for old_index in operation.old_range() {
                push(UnifiedDiffLineType::Unchanged, old_lines[old_index]);
            }
        } else {
            for new_index in operation.new_range() {
                push(UnifiedDiffLineType::Insert, new_lines[new_index]);
            }
            for old_index in operation.old_range() {
                push(UnifiedDiffLineType::Delete, old_lines[old_index]);
            }
        }
    }
    UnifiedDiff { lines }
}

/// Row and column, in characters, of `byte_offset` in `text`.
pub fn row_and_column(text: &str, byte_offset: usize) -> (u32, u32) {
    let before = text.get(..byte_offset).unwrap_or(text);
    let row = before.matches('\n').count() as u32;
    let line_start = before.rfind('\n').map(|index| index + 1).unwrap_or(0);
    let column = before[line_start..].chars().count() as u32;
    (row, column)
}

pub fn unix_millis(time: SystemTime) -> i64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn record() -> PredictionRecord {
        PredictionRecord {
            completion_id: "completion".to_string(),
            prompt_id: "prompt".to_string(),
            uri: "file:///work/vector.py".to_string(),
            workspace_uri: "file:///work".to_string(),
            editor_language: "python".to_string(),
            selection_start_line: 3,
            selection_end_line: 5,
            old_text: "    w = 1\n    u = v.sc".to_string(),
            new_text: "    w = 1\n    u = v.scale(2)\n    print(u.length())".to_string(),
            cursor_position: (5, 21),
            from_cache: false,
        }
    }

    fn texts(diff: &CharacterDiff) -> Vec<(i32, &str)> {
        diff.changes
            .iter()
            .map(|change| (change.r#type, change.text.as_str()))
            .collect()
    }

    #[test]
    fn test_character_diff_merges_runs() {
        let diff = character_diff("    u = v.sc", "    u = v.scale(2)");
        assert_eq!(
            texts(&diff),
            vec![
                (DiffChangeType::Unchanged as i32, "    u = v.sc"),
                (DiffChangeType::Insert as i32, "ale(2)"),
            ]
        );
        let diff = character_diff("factor", "k");
        assert!(
            diff.changes
                .iter()
                .all(|change| change.r#type != DiffChangeType::Unchanged as i32)
        );
    }

    #[test]
    fn test_line_diff_puts_insertions_first() {
        let diff = line_diff(&record().old_text, &record().new_text);
        let lines: Vec<(i32, &str)> = diff
            .lines
            .iter()
            .map(|line| (line.r#type, line.text.as_str()))
            .collect();
        assert_eq!(
            lines,
            vec![
                (UnifiedDiffLineType::Unchanged as i32, "    w = 1"),
                (UnifiedDiffLineType::Insert as i32, "    u = v.scale(2)"),
                (UnifiedDiffLineType::Insert as i32, "    print(u.length())"),
                (UnifiedDiffLineType::Delete as i32, "    u = v.sc"),
            ]
        );
    }

    #[test]
    fn test_feedback_entry_and_step() {
        let record = record();
        let accepted = record.context_entry(true, 42);
        assert!(accepted.accepted && !accepted.intentional_reject);
        let diff = accepted.diff_suggestion.expect("diff");
        assert_eq!(diff.path, "file:///work/vector.py");
        assert_eq!((diff.selection_start_line, diff.selection_end_line), (3, 5));
        assert_eq!(diff.old_str, record.old_text);
        assert_eq!(
            diff.cursor_position,
            Some(DocumentPosition { row: 5, col: 21 })
        );
        let rejected = record.context_entry(false, 43);
        assert!(!rejected.accepted && rejected.intentional_reject);

        let step =
            record.feedback_step(false, "text".to_string(), true, 800, SystemTime::UNIX_EPOCH);
        assert_eq!(step.r#type, CortexStepType::SupercompleteFeedback as i32);
        let Some(cortex_trajectory_step::Step::SupercompleteFeedback(feedback)) = step.step else {
            panic!("expected a feedback step");
        };
        assert_eq!(
            (
                feedback.feedback_type.as_str(),
                feedback.feedback_reason.as_str()
            ),
            ("reject", "intentional")
        );
        assert_eq!(feedback.feedback_delay_ms, 800);
        assert_eq!(
            feedback.provider_source,
            ProviderSource::Supercomplete as i32
        );
        assert_eq!(
            feedback.document.map(|document| document.text),
            Some("text".to_string())
        );
    }

    #[test]
    fn test_row_and_column_count_characters() {
        assert_eq!(row_and_column("ab\n名字x", "ab\n名字".len()), (1, 2));
        assert_eq!(row_and_column("abc", 3), (0, 3));
    }
}
