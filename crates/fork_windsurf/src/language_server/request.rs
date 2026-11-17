//! What Zed sends to the language server and how its answers become the
//! same `TabStream` the native client produces. Field values follow
//! `extension.js` (`getDocumentInfo`, `refreshContextForIdeAction`,
//! `provideFeedback`) as replayed by `fork/spike/lib/ls_driver.py`.

use std::time::SystemTime;

use language::{PointUtf16, TextBufferSnapshot, ToOffset as _};
use serde_json::{Value, json};

use crate::{
    backend::{DocumentContext, OtherDocument, TabRequestInput},
    native::{get_tab::TabStream, timestamp},
    proto::exa::{
        codeium_common_pb::{
            Document, EditorOptions, Metadata, StopReason, SupercompleteTriggerCondition,
        },
        diff_action_pb::DiffChangeType,
        language_server_pb::{
            HandleStreamingTabV2Request, HandleStreamingTabV2Response, IdeAction,
            RefreshContextForIdeActionRequest, handle_streaming_tab_v2_response::Suggestion,
        },
    },
};

/// `Document` as the official extension fills it: UTF-8 byte cursor offset,
/// no `language` and no `cursor_position`.
pub fn document(document: &DocumentContext) -> Document {
    Document {
        absolute_uri: document.absolute_uri.clone(),
        workspace_uri: document.workspace_uri.clone(),
        text: document.text.clone(),
        editor_language: document.editor_language.clone(),
        cursor_offset: document.cursor_offset as u64,
        line_ending: "\n".to_string(),
        timestamp: Some(timestamp(SystemTime::now())),
        is_dirty: document.is_dirty,
        ..Default::default()
    }
}

pub fn other_document(document: &OtherDocument) -> Document {
    Document {
        absolute_uri: document.uri.clone(),
        workspace_uri: document.workspace_uri.clone(),
        text: document.text.clone(),
        editor_language: document.editor_language.clone(),
        line_ending: "\n".to_string(),
        timestamp: Some(timestamp(SystemTime::now())),
        is_dirty: document.is_dirty,
        ..Default::default()
    }
}

pub fn tab_request(metadata: Metadata, input: &TabRequestInput) -> HandleStreamingTabV2Request {
    let context = &input.language_server;
    HandleStreamingTabV2Request {
        metadata: Some(metadata),
        document: Some(document(&input.document)),
        editor_options: Some(EditorOptions {
            tab_size: u64::from(context.tab_size),
            insert_spaces: context.insert_spaces,
            ..Default::default()
        }),
        diagnostics: input.diagnostics.clone(),
        supercomplete_trigger_condition: context.trigger as i32,
        clipboard_entry: context.clipboard.clone(),
        other_documents: context.other_documents.iter().map(other_document).collect(),
        disable_tab_jump: context.disable_tab_jump,
    }
}

pub fn refresh_request(
    metadata: Metadata,
    action: IdeAction,
    active: Option<&DocumentContext>,
    others: &[OtherDocument],
    workspace_uris: Vec<String>,
) -> RefreshContextForIdeActionRequest {
    RefreshContextForIdeActionRequest {
        metadata: Some(metadata),
        active_document: active.map(document),
        other_documents: others.iter().map(other_document).collect(),
        workspace_uris,
        ide_action: action as i32,
        recently_opened_uris: active
            .map(|document| vec![document.absolute_uri.clone()])
            .unwrap_or_default(),
        ..Default::default()
    }
}

/// Maps the trigger Zed reports to the official one.
pub fn trigger_condition(
    trigger: edit_prediction_types::EditPredictionRequestTrigger,
) -> SupercompleteTriggerCondition {
    use edit_prediction_types::EditPredictionRequestTrigger as Trigger;
    match trigger {
        Trigger::Explicit => SupercompleteTriggerCondition::Forced,
        Trigger::PredictionAccepted | Trigger::PredictionPartiallyAccepted => {
            SupercompleteTriggerCondition::SupercompleteAccept
        }
        Trigger::LSPCompletionAccepted => SupercompleteTriggerCondition::AutocompleteAccept,
        Trigger::DiagnosticNavigation | Trigger::Other => {
            SupercompleteTriggerCondition::CursorLineNavigation
        }
        Trigger::BufferEdit
        | Trigger::EditorCreated
        | Trigger::ProviderChanged
        | Trigger::UserInfoChanged
        | Trigger::VimModeChanged
        | Trigger::SettingsChanged => SupercompleteTriggerCondition::Typing,
    }
}

/// The absolute path in a `file://` URI; other strings are returned as is.
pub fn uri_to_path(uri: &str) -> String {
    url::Url::parse(uri)
        .ok()
        .filter(|url| url.scheme() == "file")
        .and_then(|url| url.to_file_path().ok())
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| uri.to_string())
}

/// Turns the language server's answer into the shape the delegate consumes.
/// A `Diff` rewrites whole lines `[selection_start_line, selection_end_line)`
/// of the document that was sent: its text is the unchanged and deleted
/// parts of `character_diff` and the rewrite is the unchanged and inserted
/// parts (`computeFinalDocumentState` in `extension.js`).
pub fn tab_stream(response: HandleStreamingTabV2Response) -> TabStream {
    let mut stream = TabStream {
        prompt_id: response.prompt_id,
        completion_id: response.completion_id,
        request_id: response.request_uid,
        stop_reason: StopReason::try_from(response.stop_reason)
            .ok()
            .filter(|reason| *reason != StopReason::Unspecified)
            .map(|reason| reason.as_str_name().to_string()),
        ..Default::default()
    };
    if let Some(filter_reason) = response.filter_reason {
        stream.filter_reason = Some(filter_reason.reason);
        stream.noop = true;
        return stream;
    }
    match response.suggestion {
        Some(Suggestion::Diff(diff)) => {
            stream.path = uri_to_path(&diff.path);
            let changes = diff
                .character_diff
                .map(|character_diff| character_diff.changes)
                .unwrap_or_default();
            if changes.is_empty() {
                stream.old_text = diff.old_str.clone();
                stream.new_text = diff.old_str;
            }
            for change in changes {
                let change_type =
                    DiffChangeType::try_from(change.r#type).unwrap_or(DiffChangeType::Unspecified);
                match change_type {
                    DiffChangeType::Unchanged => {
                        stream.old_text.push_str(&change.text);
                        stream.new_text.push_str(&change.text);
                    }
                    DiffChangeType::Delete => stream.old_text.push_str(&change.text),
                    DiffChangeType::Insert => stream.new_text.push_str(&change.text),
                    DiffChangeType::Unspecified => {}
                }
                stream.deltas += 1;
            }
            stream.line_range = Some((
                diff.selection_start_line as u32,
                diff.selection_end_line as u32,
            ));
        }
        Some(Suggestion::TabJump(tab_jump)) => {
            stream.path = uri_to_path(&tab_jump.path);
            stream.tab_jump = true;
            let position = tab_jump.jump_position.unwrap_or_default();
            stream.jump_position = Some((position.row as u32, position.col as u32));
        }
        Some(Suggestion::Noop(_)) | None => stream.noop = true,
    }
    stream
}

/// The byte range of lines `[start_line, end_line)` in `text`, without the
/// line break after the last one. An empty range is the start of
/// `start_line`.
pub fn line_range_bytes(
    text: &str,
    start_line: u32,
    end_line: u32,
) -> Option<std::ops::Range<usize>> {
    let mut line_starts = vec![0];
    line_starts.extend(text.match_indices('\n').map(|(index, _)| index + 1));
    let start = *line_starts.get(start_line as usize)?;
    if end_line <= start_line {
        return Some(start..start);
    }
    let end = line_starts
        .get(end_line as usize)
        .map(|next_line_start| next_line_start - 1)
        .unwrap_or(text.len());
    Some(start..end.max(start))
}

/// The `didChange` content changes that turn `previous` into `current`, with
/// UTF-16 positions, applied in order (like `lsp_store`'s incremental sync).
pub fn content_changes(previous: &TextBufferSnapshot, current: &TextBufferSnapshot) -> Vec<Value> {
    current
        .edits_since::<PointUtf16>(previous.version())
        .map(|edit| {
            let start = edit.new.start;
            let end = start + (edit.old.end - edit.old.start);
            let new_start = edit.new.start.to_offset(current);
            let new_end = edit.new.end.to_offset(current);
            let text: String = current.text_for_range(new_start..new_end).collect();
            json!({
                "range": {
                    "start": { "line": start.row, "character": start.column },
                    "end": { "line": end.row, "character": end.column },
                },
                "text": text,
            })
        })
        .collect()
}

pub fn did_open_params(uri: &str, language_id: &str, version: i32, text: String) -> Value {
    json!({
        "textDocument": {
            "uri": uri,
            "languageId": language_id,
            "version": version,
            "text": text,
        }
    })
}

pub fn did_change_params(uri: &str, version: i32, changes: Vec<Value>) -> Value {
    json!({
        "textDocument": { "uri": uri, "version": version },
        "contentChanges": changes,
    })
}

pub fn did_close_params(uri: &str) -> Value {
    json!({ "textDocument": { "uri": uri } })
}

pub fn workspace_folder(uri: &str) -> Value {
    let name = uri_to_path(uri)
        .rsplit('/')
        .find(|segment| !segment.is_empty())
        .unwrap_or_default()
        .to_string();
    json!({ "uri": uri, "name": name })
}

pub fn initialize_params(workspace_uris: &[String], client_version: &str) -> Value {
    let root_uri = workspace_uris.first().cloned();
    json!({
        "processId": std::process::id(),
        "rootUri": root_uri,
        "rootPath": root_uri.as_deref().map(uri_to_path),
        "workspaceFolders": workspace_uris.iter().map(|uri| workspace_folder(uri)).collect::<Vec<_>>(),
        "capabilities": {
            "textDocument": { "synchronization": { "didSave": true, "dynamicRegistration": true } },
            "workspace": { "workspaceFolders": true, "configuration": true },
        },
        "clientInfo": { "name": "Devin", "version": client_version },
    })
}

/// The request as JSON for `windsurf: show last request`, with the key
/// replaced by its length and document texts by their sizes.
pub fn redacted_tab_request(request: &HandleStreamingTabV2Request) -> Value {
    let mut value = serde_json::to_value(request).unwrap_or_else(
        |error| json!({ "error": format!("couldn't serialize the request: {error}") }),
    );
    if let Some(api_key) = value.pointer_mut("/metadata/api_key") {
        let length = api_key.as_str().map(str::len).unwrap_or_default();
        *api_key = format!("<redacted, {length} chars>").into();
    }
    if let Some(documents) = value
        .get_mut("other_documents")
        .and_then(Value::as_array_mut)
    {
        for document in documents {
            if let Some(text) = document.get_mut("text") {
                let length = text.as_str().map(str::len).unwrap_or_default();
                *text = format!("<{length} bytes>").into();
            }
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::proto::exa::{
        codeium_common_pb::{DocumentPosition, SuperCompleteFilterReason},
        diff_action_pb::{CharacterDiff, CharacterDiffChange},
        language_server_pb::handle_streaming_tab_v2_response::{Diff, NoOp, TabJump},
    };

    fn change(change_type: DiffChangeType, text: &str) -> CharacterDiffChange {
        CharacterDiffChange {
            text: text.to_string(),
            r#type: change_type as i32,
        }
    }

    #[test]
    fn test_diff_becomes_old_and_new_text_of_the_lines() {
        let stream = tab_stream(HandleStreamingTabV2Response {
            completion_id: "completion".to_string(),
            prompt_id: "prompt".to_string(),
            stop_reason: StopReason::StopPattern as i32,
            suggestion: Some(Suggestion::Diff(Diff {
                path: "file:///work/vector.py".to_string(),
                selection_start_line: 2,
                selection_end_line: 4,
                character_diff: Some(CharacterDiff {
                    changes: vec![
                        change(DiffChangeType::Unchanged, "def scale(self, "),
                        change(DiffChangeType::Delete, "factor"),
                        change(DiffChangeType::Insert, "k"),
                        change(DiffChangeType::Unchanged, "):\n    pass"),
                    ],
                }),
                cursor_position: Some(DocumentPosition { row: 2, col: 17 }),
                old_str: "def scale(self, factor):\n    pass".to_string(),
                ..Default::default()
            })),
            ..Default::default()
        });
        assert_eq!(stream.path, "/work/vector.py");
        assert_eq!(stream.old_text, "def scale(self, factor):\n    pass");
        assert_eq!(stream.new_text, "def scale(self, k):\n    pass");
        assert_eq!(stream.line_range, Some((2, 4)));
        assert_eq!(stream.completion_id, "completion");
        assert_eq!(stream.prompt_id, "prompt");
        assert_eq!(
            stream.stop_reason.as_deref(),
            Some("STOP_REASON_STOP_PATTERN")
        );
        assert!(!stream.noop && !stream.tab_jump);
    }

    #[test]
    fn test_tab_jump_noop_and_filtered_responses() {
        let jump = tab_stream(HandleStreamingTabV2Response {
            suggestion: Some(Suggestion::TabJump(TabJump {
                path: "file:///work/vector.py".to_string(),
                jump_position: Some(DocumentPosition { row: 12, col: 55 }),
                is_import: false,
            })),
            ..Default::default()
        });
        assert!(jump.tab_jump);
        assert_eq!(jump.jump_position, Some((12, 55)));
        assert_eq!(jump.path, "/work/vector.py");

        let noop = tab_stream(HandleStreamingTabV2Response {
            suggestion: Some(Suggestion::Noop(NoOp {})),
            ..Default::default()
        });
        assert!(noop.noop);
        assert!(tab_stream(HandleStreamingTabV2Response::default()).noop);

        let filtered = tab_stream(HandleStreamingTabV2Response {
            filter_reason: Some(SuperCompleteFilterReason {
                reason: "revert".to_string(),
            }),
            suggestion: Some(Suggestion::Diff(Diff::default())),
            ..Default::default()
        });
        assert!(filtered.noop);
        assert_eq!(filtered.filter_reason.as_deref(), Some("revert"));
    }

    #[test]
    fn test_line_range_bytes() {
        let text = "zero\none\ntwo\nthree";
        assert_eq!(line_range_bytes(text, 0, 1), Some(0..4));
        assert_eq!(line_range_bytes(text, 1, 3), Some(5..12));
        assert_eq!(line_range_bytes(text, 2, 4), Some(9..18));
        assert_eq!(line_range_bytes(text, 3, 9), Some(13..18));
        assert_eq!(line_range_bytes(text, 1, 1), Some(5..5));
        assert_eq!(line_range_bytes(text, 4, 5), None);
        assert_eq!(line_range_bytes("a\n", 1, 2), Some(2..2));
    }

    #[test]
    fn test_content_changes_use_utf16_positions_in_order() {
        let mut buffer = language::TextBuffer::new(
            language::ReplicaId::LOCAL,
            language::BufferId::new(1).expect("buffer id"),
            "let 名前 = 1;\nlet b = 2;\n",
        );
        let before = buffer.snapshot().clone();
        buffer.edit([(10..10, "x"), (20..21, "")]);
        let after = buffer.snapshot().clone();
        assert_eq!(after.text(), "let 名前x = 1;\nlet  = 2;\n");
        assert_eq!(
            content_changes(&before, &after),
            vec![
                json!({
                    "range": {
                        "start": { "line": 0, "character": 6 },
                        "end": { "line": 0, "character": 6 },
                    },
                    "text": "x",
                }),
                json!({
                    "range": {
                        "start": { "line": 1, "character": 4 },
                        "end": { "line": 1, "character": 5 },
                    },
                    "text": "",
                }),
            ]
        );
    }

    #[test]
    fn test_trigger_conditions() {
        use edit_prediction_types::EditPredictionRequestTrigger as Trigger;
        assert_eq!(
            trigger_condition(Trigger::BufferEdit),
            SupercompleteTriggerCondition::Typing
        );
        assert_eq!(
            trigger_condition(Trigger::Explicit),
            SupercompleteTriggerCondition::Forced
        );
        assert_eq!(
            trigger_condition(Trigger::PredictionAccepted),
            SupercompleteTriggerCondition::SupercompleteAccept
        );
        assert_eq!(
            trigger_condition(Trigger::Other),
            SupercompleteTriggerCondition::CursorLineNavigation
        );
    }
}
